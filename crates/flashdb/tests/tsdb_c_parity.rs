//! Port of the upstream FlashDB TSDB test suite
//! (`reference/FlashDB/tests/fdb_tsdb_tc.c`) to the Rust API.
//!
//! The original test runs under the RT-Thread utest framework on a POSIX file
//! backend with a 16-sector database. The behaviour it checks — append, forward
//! iteration, iteration by time range (including reversed ranges), query
//! counts, setting TSL statuses, cleaning, and range queries across sector
//! boundaries — is portable, so every test case below mirrors one upstream
//! case on `flashdb_sim::RamFlash<65536, WG>` with
//! `Config::new(4096, 65536)` (16 x 4096-byte sectors, like the C test).
//!
//! Intentional API mapping:
//!
//! | upstream C                                  | Rust equivalent                 |
//! |---------------------------------------------|---------------------------------|
//! | `get_time()` callback, `fdb_tsl_append()`    | `Tsdb::append(time, data)` with  |
//! | timestamps emitted as 2, 4, 6, ...           | the same explicit timestamps     |
//! | `fdb_tsl_iter(db, cb, arg)` callbacks        | `Iterators` + explicit loops     |
//! | `fdb_tsl_set_status(db, tsl, status)` inside | collect entries first, then     |
//! | the iteration callback                       | `Tsdb::set_status()` afterwards  |
//! | `fdb_tsl_clean()`                            | `Tsdb::clean()`                  |
//! | `fdb_tsdb_deinit()` + `fdb_tsdb_init()`      | `into_flash()` + `mount()`       |
//!
//! `TEST_TS_COUNT` and `TEST_ITER1_COUNT` are recomputed with the same macro
//! arithmetic as the upstream file (which is aware of `FDB_WRITE_GRAN`) so the
//! amount of data and the sector-boundary coverage match the C reference.

use flashdb::{Config, Timestamp, Tsdb, TslEntry, TslIterator, TslStatus};
use flashdb_sim::RamFlash;

const SECTOR_SIZE: u32 = 4096;
const SECTOR_NUM: u32 = 16;
const MAX_LEN: u32 = 128;
const TEST_TIME_STEP: i32 = 2;
const LOG_BUF_LEN: usize = 10;

// ---------------------------------------------------------------------------
// upstream macro arithmetic (`fdb_tsdb_tc.c` lines 34-58)
// ---------------------------------------------------------------------------

fn wg_bytes(wg: u32) -> u32 {
    (wg + 7) / 8
}

fn align_up(value: u32, align: u32) -> u32 {
    if align <= 1 {
        value
    } else {
        (value + align - 1) & !(align - 1)
    }
}

fn status_table_size(wg: u32, status_num: u32) -> u32 {
    if wg == 1 {
        (status_num * wg + 7) / 8
    } else {
        ((status_num - 1) * wg + 7) / 8
    }
}

/// `_TSIL_PER_SECTOR`: how many int-sized TSLs a sector holds for this WG.
fn per_sector_int(wg: u32) -> u32 {
    let w = wg_bytes(wg);
    let tsl_status = status_table_size(wg, 6);
    // _TSIL_IDX_BASE_SZ = status + fdb_time_t(4) + 2 * uint32
    let idx_data = align_up(tsl_status + 4 + 4 + 4, w);
    let u32_align = align_up(4, w);
    let time_align = align_up(4, w);
    // _TSIL_SEC_HDR_RAW_SZ = store_status + u32_align + time_align
    //                        + 2*(time_align + u32_align + tsl_status) + uint32
    let sec_hdr_raw = status_table_size(wg, 4)
        + u32_align
        + time_align
        + 2 * (time_align + u32_align + tsl_status)
        + 4;
    let sec_hdr = align_up(sec_hdr_raw, w);
    (SECTOR_SIZE - sec_hdr) / (idx_data + align_up(4, w))
}

/// `_TSIL_PER_SECTOR_STR` (TSL payload is the 10-byte log buffer).
fn per_sector_str(wg: u32) -> u32 {
    let w = wg_bytes(wg);
    let tsl_status = status_table_size(wg, 6);
    let idx_data = align_up(tsl_status + 4 + 4 + 4, w);
    let u32_align = align_up(4, w);
    let time_align = align_up(4, w);
    let sec_hdr_raw = status_table_size(wg, 4)
        + u32_align
        + time_align
        + 2 * (time_align + u32_align + tsl_status)
        + 4;
    let sec_hdr = align_up(sec_hdr_raw, w);
    (SECTOR_SIZE - sec_hdr) / (idx_data + align_up(LOG_BUF_LEN as u32, w))
}

/// `TEST_TS_COUNT` = min(per_sector_str * 14, 256).
fn test_ts_count(wg: u32) -> u32 {
    (per_sector_str(wg) * 14).min(256)
}

/// `TEST_ITER1_COUNT` = 5 sectors worth of int TSLs.
fn test_iter1_count(wg: u32) -> u32 {
    5 * per_sector_int(wg)
}

// For the two granularities used by the C harness (and by us) these are the
// exact values the upstream macro arithmetic produces.
#[test]
fn upstream_counts_match_c_macros() {
    assert_eq!(test_ts_count(8), 256);
    assert_eq!(test_ts_count(1), 256);
    assert_eq!(per_sector_int(8), 193);
    assert_eq!(per_sector_int(1), 239);
    // TEST_ITER1_COUNT = TEST_ITER1_SECTORS(5) * _TSIL_PER_SECTOR
    assert_eq!(test_iter1_count(8), 965);
    assert_eq!(test_iter1_count(1), 1195);
}

// ---------------------------------------------------------------------------
// RT_ALIGN / RT_ALIGN_DOWN for the positive timestamps used by the test
// ---------------------------------------------------------------------------

fn rt_align(value: i32, align: i32) -> i32 {
    let sum = value + align - 1;
    sum - sum % align
}

fn rt_align_down(value: i32, align: i32) -> i32 {
    (value / align) * align
}

macro_rules! tsdb_parity_for_wg {
    ($modname:ident, $wg:literal) => {
        mod $modname {
            use super::*;

            type Nor = RamFlash<65536, $wg>;
            const WG: u32 = $wg;

            fn cfg() -> Config {
                Config::new(SECTOR_SIZE, SECTOR_SIZE * SECTOR_NUM)
            }

            fn fresh() -> Tsdb<Nor> {
                Tsdb::mount(Nor::new(SECTOR_SIZE), cfg(), MAX_LEN).unwrap()
            }

            fn reboot(db: Tsdb<Nor>) -> Tsdb<Nor> {
                Tsdb::mount(db.into_flash(), cfg(), MAX_LEN).unwrap()
            }

            fn count() -> u32 {
                test_ts_count(WG)
            }

            fn iter1_count() -> u32 {
                test_iter1_count(WG)
            }

            /// Read the payload of an entry as a decimal timestamp string, like
            /// the upstream `test_fdb_tsl_iter_cb` (`atoi(data)`).
            fn read_decimal(
                it: &mut TslIterator<'_, Nor>,
                entry: &TslEntry<i32>,
            ) -> i32 {
                let mut buf = [0u8; LOG_BUF_LEN];
                let n = it.read_blob(entry, &mut buf).unwrap();
                let text = core::str::from_utf8(&buf[..n]).unwrap();
                text.parse::<i32>().unwrap()
            }

            fn read_i32(it: &mut TslIterator<'_, Nor>, entry: &TslEntry<i32>) -> i32 {
                let mut buf = [0u8; 4];
                let n = it.read_blob(entry, &mut buf).unwrap();
                assert_eq!(n, 4);
                i32::from_le_bytes(buf)
            }

            /// `test_fdb_tsl_append`: append `count()` TSLs; the payload is the
            /// decimal representation of the timestamp (`get_time()` emits
            /// 2, 4, 6, ... in the C test).
            fn append_decimal(db: &mut Tsdb<Nor>, count: u32) {
                for i in 1..=count {
                    let time = TEST_TIME_STEP * i as i32;
                    db.append(time, time.to_string().as_bytes()).unwrap();
                }
            }

            fn append_count(db: &mut Tsdb<Nor>, count: u32) {
                db.clean().unwrap();
                for i in 0..count as i32 {
                    let time = TEST_TIME_STEP * (i + 1);
                    db.append(time, &i.to_le_bytes()).unwrap();
                }
            }

            // -- test_fdb_tsl_append / test_fdb_tsl_iter --------------------

            #[test]
            fn append_and_forward_iter() {
                let mut db = fresh();
                db.clean().unwrap();
                append_decimal(&mut db, count());

                let mut db = reboot(db);
                let mut seen = 0u32;
                let mut it = db.iter();
                while let Some(entry) = it.next() {
                    let entry = entry.unwrap();
                    assert_eq!(entry.status, TslStatus::Write);
                    let data = read_decimal(&mut it, &entry);
                    assert_eq!(entry.time, data, "time must round-trip through payload");
                    seen += 1;
                }
                assert_eq!(seen, count());
            }

            // -- test_fdb_tsl_iter_by_time ---------------------------------

            #[test]
            fn iter_by_time_single_timestamps() {
                let mut db = fresh();
                db.clean().unwrap();
                append_decimal(&mut db, count());

                let mut db = reboot(db);
                let to = count() as i32 * TEST_TIME_STEP;
                let mut cur = 0i32;
                while cur <= to {
                    let mut n = 0;
                    let mut it = db.iter_by_time(cur, cur);
                    while let Some(entry) = it.next() {
                        let entry = entry.unwrap();
                        assert_eq!(entry.time, cur);
                        let data = read_decimal(&mut it, &entry);
                        assert_eq!(entry.time, data);
                        n += 1;
                    }
                    // Timestamps start at 2, so the query [0, 0] legitimately
                    // returns nothing (the C callback simply never runs for
                    // empty ranges); every stored timestamp must be returned
                    // exactly once.
                    let expected = if cur >= TEST_TIME_STEP && cur % TEST_TIME_STEP == 0 {
                        1
                    } else {
                        0
                    };
                    assert_eq!(
                        n, expected,
                        "iter_by_time({cur}, {cur}) must yield {expected} TSL(s)"
                    );
                    cur += TEST_TIME_STEP;
                }

                // Whole range, forward: times must be 2, 4, ..., 2*count.
                let mut times = Vec::new();
                let mut it = db.iter_by_time(0, to);
                while let Some(entry) = it.next() {
                    let entry = entry.unwrap();
                    let data = read_decimal(&mut it, &entry);
                    assert_eq!(entry.time, data);
                    times.push(entry.time);
                }
                let expected: Vec<i32> = (1..=count() as i32)
                    .map(|i| TEST_TIME_STEP * i)
                    .collect();
                assert_eq!(times, expected);
            }

            // -- test_fdb_tsl_query_count ----------------------------------

            #[test]
            fn query_count_write_status() {
                let mut db = fresh();
                db.clean().unwrap();
                append_decimal(&mut db, count());

                let mut db = reboot(db);
                let to = count() as i32 * TEST_TIME_STEP;
                assert_eq!(
                    db.query_count(0, to, TslStatus::Write).unwrap(),
                    count() as usize
                );
            }

            // -- test_fdb_tsl_set_status -----------------------------------

            #[test]
            fn set_status_user_and_deleted() {
                let mut db = fresh();
                db.clean().unwrap();
                append_decimal(&mut db, count());

                let mut db = reboot(db);
                let user_status_count =
                    (count() / 2) as usize; // TEST_TS_USER_STATUS1_COUNT
                let deleted_count = (count() - count() / 2) as usize;
                let to = count() as i32 * TEST_TIME_STEP;

                // Collect first: the Rust iterator borrows the database, so the
                // statuses cannot be changed from inside the iteration loop.
                let mut entries = Vec::new();
                let mut it = db.iter_by_time(0, to);
                while let Some(entry) = it.next() {
                    entries.push(entry.unwrap());
                }
                for entry in &entries {
                    let status = if entry.time >= 0
                        && entry.time <= (user_status_count as i32) * TEST_TIME_STEP
                    {
                        TslStatus::UserStatus1
                    } else {
                        TslStatus::Deleted
                    };
                    db.set_status(entry, status).unwrap();
                }

                assert_eq!(
                    db.query_count(0, to, TslStatus::UserStatus1).unwrap(),
                    user_status_count
                );
                assert_eq!(
                    db.query_count(0, to, TslStatus::Deleted).unwrap(),
                    deleted_count
                );
            }

            // -- test_fdb_tsl_clean ---------------------------------------

            #[test]
            fn clean_removes_all_entries() {
                let mut db = fresh();
                db.clean().unwrap();
                append_decimal(&mut db, count());

                let mut db = reboot(db);
                db.clean().unwrap();
                let mut n = 0;
                let mut it = db.iter();
                while it.next().is_some() {
                    n += 1;
                }
                assert_eq!(n, 0);

                let mut db = reboot(db);
                let mut n = 0;
                let mut it = db.iter();
                while it.next().is_some() {
                    n += 1;
                }
                assert_eq!(n, 0);
            }

            // -- test_fdb_tsl_iter_by_time_1 ------------------------------

            #[derive(Clone, Copy)]
            struct SectorInfo {
                addr: u32,
                start_time: i32,
                end_time: i32,
            }

            /// `test_tsdb_data_by_time`: query `[from, to]`, compute the number
            /// of even timestamps the reference should return, and verify both
            /// the count and the timestamps (ascending or descending range).
            fn data_by_time(
                db: &mut Tsdb<Nor>,
                from: i32,
                to: i32,
                db_start: i32,
                db_end: i32,
            ) {
                let mut cur_time = from;
                let mut valid_to = to;
                if from <= to {
                    if from < db_start {
                        cur_time = db_start;
                    }
                    if to > db_end {
                        valid_to = db_end;
                    }
                } else {
                    if from > db_end {
                        cur_time = db_end;
                    }
                    if to < db_start {
                        valid_to = db_start;
                    }
                }

                let mut got = Vec::new();
                let mut it = db.iter_by_time(from, to);
                while let Some(entry) = it.next() {
                    got.push(entry.unwrap().time);
                }

                // expected number of TSLs (every stored timestamp is a
                // multiple of TEST_TIME_STEP)
                let mut expected = 0i64;
                if from <= to {
                    let mut i = cur_time as i64;
                    while i <= valid_to as i64 {
                        if i % TEST_TIME_STEP as i64 == 0 {
                            expected += 1;
                        }
                        i += 1;
                    }
                } else {
                    let mut i = cur_time as i64;
                    while i >= valid_to as i64 {
                        if i % TEST_TIME_STEP as i64 == 0 {
                            expected += 1;
                        }
                        i -= 1;
                    }
                }
                assert_eq!(
                    got.len() as i64,
                    expected,
                    "range [{from}, {to}] (db {db_start}..{db_end}): count mismatch"
                );

                let mut cur = cur_time;
                for time in &got {
                    if from <= to {
                        assert_eq!(*time, rt_align(cur, TEST_TIME_STEP));
                        cur += TEST_TIME_STEP;
                    } else {
                        assert_eq!(*time, rt_align_down(cur, TEST_TIME_STEP));
                        cur -= TEST_TIME_STEP;
                    }
                }
                if let Some(last) = got.last() {
                    if from <= to {
                        assert_eq!(*last, rt_align_down(valid_to, TEST_TIME_STEP));
                    } else {
                        assert_eq!(*last, rt_align(valid_to, TEST_TIME_STEP));
                    }
                }
            }

            /// `test_fdb_tsl_sector_bound_test`: 18 queries around the
            /// boundaries of two sectors, covering forward, reversed and empty
            /// ranges.
            fn sector_bound_test(
                db: &mut Tsdb<Nor>,
                secs: &[SectorInfo],
                start: usize,
                end: usize,
                db_start: i32,
                db_end: i32,
            ) {
                let s = &secs[start];
                let e = &secs[end];
                for from in [s.start_time - 1, s.start_time, s.start_time + 1] {
                    for to in [e.end_time - 1, e.end_time, e.end_time + 1] {
                        data_by_time(db, from, to, db_start, db_end);
                    }
                }
                for from in [s.end_time - 1, s.end_time, s.end_time + 1] {
                    for to in [e.start_time - 1, e.start_time, e.start_time + 1] {
                        data_by_time(db, from, to, db_start, db_end);
                    }
                }
            }

            #[test]
            fn iter_by_time_across_sectors() {
                let mut db = fresh();
                db.clean().unwrap();
                append_count(&mut db, iter1_count());

                let mut db = reboot(db);

                let mut secs: Vec<SectorInfo> = (0..SECTOR_NUM as usize)
                    .map(|i| SectorInfo {
                        addr: SECTOR_SIZE * i as u32,
                        start_time: i32::MAX,
                        end_time: 0,
                    })
                    .collect();
                let mut db_start = i32::MAX;
                let mut db_end = i32::MIN;

                // `get_sector_info_cb`: group by the payload sector address.
                let mut it = db.iter_by_time(0, i32::MAX);
                while let Some(entry) = it.next() {
                    let entry = entry.unwrap();
                    // The int payload stores the append index; timestamp is 2*(i+1).
                    let data = read_i32(&mut it, &entry);
                    assert_eq!(data, entry.time / TEST_TIME_STEP - 1);
                    let i = (entry.log_addr / SECTOR_SIZE) as usize;
                    if i < secs.len() {
                        let sec = &mut secs[i];
                        sec.addr = entry.log_addr / SECTOR_SIZE * SECTOR_SIZE;
                        if sec.start_time > entry.time {
                            sec.start_time = entry.time;
                        }
                        if sec.end_time < entry.time {
                            sec.end_time = entry.time;
                        }
                        if db_start > entry.time {
                            db_start = entry.time;
                        }
                        if db_end < entry.time {
                            db_end = entry.time;
                        }
                    }
                }

                // "must found more than 2 sectors"
                assert_ne!(
                    secs[2].start_time,
                    i32::MAX,
                    "the test data must span at least 3 sectors"
                );

                // database bound checks
                data_by_time(&mut db, db_start - 1, db_end + 1, db_start, db_end);
                data_by_time(&mut db, db_start - 2, db_start - 1, db_start, db_end);
                data_by_time(&mut db, db_start - 1, db_start - 2, db_start, db_end);
                data_by_time(&mut db, db_end + 1, db_end + 2, db_start, db_end);
                data_by_time(&mut db, db_end + 2, db_end + 1, db_start, db_end);

                // first sector
                data_by_time(&mut db, secs[0].start_time - 1, secs[0].end_time, db_start, db_end);
                data_by_time(&mut db, secs[0].start_time, secs[0].end_time, db_start, db_end);
                data_by_time(&mut db, secs[0].start_time, secs[0].end_time + 1, db_start, db_end);
                data_by_time(&mut db, secs[0].end_time + 1, secs[0].start_time, db_start, db_end);
                data_by_time(&mut db, secs[0].end_time, secs[0].start_time, db_start, db_end);
                data_by_time(&mut db, secs[0].end_time, secs[0].start_time - 1, db_start, db_end);

                // last used sector: the first sector with end_time == 0
                let last_idx = secs.iter().position(|s| s.end_time == 0).unwrap();
                assert!(last_idx >= 3, "last sector index {last_idx} must be >= 3");
                let last = secs[last_idx];
                data_by_time(&mut db, last.start_time - 1, last.end_time, db_start, db_end);
                data_by_time(&mut db, last.start_time, last.end_time, db_start, db_end);
                data_by_time(&mut db, last.start_time, last.end_time + 1, db_start, db_end);
                data_by_time(&mut db, last.end_time + 1, last.start_time, db_start, db_end);
                data_by_time(&mut db, last.end_time, last.start_time, db_start, db_end);
                data_by_time(&mut db, last.end_time, last.start_time - 1, db_start, db_end);

                // less than one sector
                data_by_time(
                    &mut db,
                    secs[0].start_time + 1,
                    secs[0].end_time - 1,
                    db_start,
                    db_end,
                );
                data_by_time(
                    &mut db,
                    secs[0].end_time - 1,
                    secs[0].start_time + 1,
                    db_start,
                    db_end,
                );
                // equal to one sector
                data_by_time(&mut db, secs[0].start_time, secs[0].end_time, db_start, db_end);
                data_by_time(&mut db, secs[0].end_time, secs[0].start_time, db_start, db_end);

                // one to two sectors
                for (a, b) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                    sector_bound_test(&mut db, &secs, a, b, db_start, db_end);
                }
                // more than two sectors
                for (a, b) in [(0, 2), (2, 0), (2, 2)] {
                    sector_bound_test(&mut db, &secs, a, b, db_start, db_end);
                }
            }

            // -- test_fdb_github_issue_249 ---------------------------------

            /// Upstream regression: with a large sector size (16 KiB) and
            /// payloads close to `max_len` (10 KiB), all three records must be
            /// visible after a reboot.
            #[test]
            fn github_issue_249_large_payloads() {
                const BIG_SEC: u32 = 16 * 1024;
                const BIG_DB: u32 = 512 * 1024;
                const BIG_MAX_LEN: u32 = 10 * 1024;
                type BigNor = RamFlash<524288, $wg>;

                // A 512 KiB flash lives inline in `RamFlash`, which is larger
                // than the default test-thread stack; run it on a dedicated
                // thread (the C test runs the same geometry on the RT-Thread
                // main stack / heap).
                std::thread::Builder::new()
                    .stack_size(8 * 1024 * 1024)
                    .spawn(|| {
                        let mut db = Tsdb::<BigNor>::mount(
                            BigNor::new(BIG_SEC),
                            Config::new(BIG_SEC, BIG_DB),
                            BIG_MAX_LEN,
                        )
                        .unwrap();
                        db.clean().unwrap();

                        // get_time() yields 2, 4, 6 for the three appends.
                        db.append(2, &vec![0xCD; 7 * 1024]).unwrap();
                        db.append(4, &vec![0xCD; 8 * 1024]).unwrap();
                        db.append(6, &vec![0xCD; 9 * 1024]).unwrap();

                        // reboot: deinit + init (same geometry / max_len)
                        let mut db = Tsdb::<BigNor>::mount(
                            db.into_flash(),
                            Config::new(BIG_SEC, BIG_DB),
                            BIG_MAX_LEN,
                        )
                        .unwrap();

                        assert_eq!(db.query_count(2, 6, TslStatus::Write).unwrap(), 3);
                        assert_eq!(
                            db.query_count(0, i32::MAX, TslStatus::Write).unwrap(),
                            3
                        );
                    })
                    .unwrap()
                    .join()
                    .expect("large-payload test thread panicked");
            }

            // Silence "unused" for Timestamp import when a WG module only uses
            // explicit timestamps.
            const _: fn() = || {
                fn assert_timestamp<T: Timestamp>() {}
                assert_timestamp::<i32>();
            };
        }
    };
}

tsdb_parity_for_wg!(wg8, 8);
tsdb_parity_for_wg!(wg1, 1);
