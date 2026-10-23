//! Power-failure recovery: inject a fault at every write operation of a
//! workload, then remount a fresh database on the resulting flash image and
//! verify that the invariants hold (see `harness/SCENARIOS.md`).
//!
//! The simulator can only inject "the n-th flash operation fails" faults, so
//! the replay loop arms the fault at every operation position and relies on
//! `fail_after_writes` ignoring the injected fault when that position is a
//! read. That covers every write/erase operation of the workload.

use flashdb::{Config, Tsdb, TslStatus};
use flashdb_sim::RamFlash;

const SEC_SIZE: u32 = 4096;
const MAX_SIZE: u32 = 16384;
const MAX_LEN: u32 = 256;

/// `sector_hdr_data.end_info[0].status` offset for write granularity 8 and
/// 32-bit timestamps (see `low::ts_layout_32(8)`).
const SEC_END0_STAT_OFF: u32 = 19;
/// `sector_hdr_data.end_info[1].status` offset for the same layout.
const SEC_END1_STAT_OFF: u32 = 32;

type Nor = RamFlash<16384, 8>;

fn config() -> Config {
    Config::new(SEC_SIZE, MAX_SIZE)
}

fn payload_for(i: i32) -> [u8; 32] {
    let mut data = [0u8; 32];
    for (j, b) in data.iter_mut().enumerate() {
        *b = ((i as u32 * 13)
            .wrapping_add(0x5A)
            .wrapping_add(j as u32 * 5)
            & 0xFF) as u8;
    }
    data
}

/// Mount a fresh RAM flash once so that all sectors are formatted.
fn formatted_flash() -> Nor {
    let db = Tsdb::mount(Nor::new(SEC_SIZE), config(), MAX_LEN).unwrap();
    db.into_flash()
}

fn stat_ops(stats: flashdb_sim::Stats) -> u64 {
    stats.read_ops + stats.write_ops + stats.erase_ops
}

/// Number of flash operations performed after `formatted_flash()` returns
/// (i.e. the second mount plus the whole workload).
fn workload_ops(appends: i32) -> u64 {
    let base = {
        let flash = formatted_flash();
        stat_ops(flash.stats())
    };
    let flash = formatted_flash();
    let mut db = Tsdb::mount(flash, config(), MAX_LEN).unwrap();
    for i in 1..=appends {
        db.append(i, &payload_for(i)).unwrap();
    }
    let flash = db.into_flash();
    stat_ops(flash.stats()) - base
}

/// Replay `appends` appends with a fault armed before the `fail_ops`-th flash
/// operation after formatting. Returns the flash image and the number of
/// appends that completed before the failure.
fn replay(appends: i32, fail_ops: u64) -> (Vec<u8>, i32) {
    let mut flash = formatted_flash();
    flash.fail_after_writes(fail_ops);
    let mut db = Tsdb::mount(flash, config(), MAX_LEN).expect("mount after arming");
    let mut done = 0i32;
    for i in 1..=appends {
        if db.append(i, &payload_for(i)).is_err() {
            break;
        }
        done += 1;
    }
    let flash = db.into_flash();
    (flash.data().to_vec(), done)
}

/// Number of flash operations performed by mounting `image`.
fn mount_ops(image: &[u8]) -> u64 {
    let mut flash = Nor::new(SEC_SIZE);
    flash.load(image).unwrap();
    let db = Tsdb::mount(flash, config(), MAX_LEN).unwrap();
    stat_ops(db.into_flash().stats())
}

/// Mount `image` and perform one append (timestamp 78), failing the `w`-th
/// flash operation after the mount. Returns the resulting image and whether the
/// append failed. This is how a fault is placed deterministically inside the
/// seal sequence, where the raw simulator op index counts mount reads too.
fn fault_append(image: &[u8], w: u64) -> (Vec<u8>, bool) {
    let ops = mount_ops(image);
    let mut flash = Nor::new(SEC_SIZE);
    flash.load(image).unwrap();
    flash.fail_after_ops(ops + w - 1);
    let mut db = Tsdb::mount(flash, config(), MAX_LEN).unwrap();
    let failed = db.append(78, &payload_for(78)).is_err();
    (db.into_flash().data().to_vec(), failed)
}

struct Recovered {
    committed: Vec<i32>,
    last_time: i32,
}

/// Remount an image and verify every invariant; returns what was recovered.
fn recover(image: &[u8], done: i32) -> Recovered {
    let mut flash = Nor::new(SEC_SIZE);
    flash.load(image).expect("image size");
    let mut db = Tsdb::mount(flash, config(), MAX_LEN).expect("remount must succeed");

    let mut committed = Vec::new();
    let mut prewrite = 0;
    let mut forward = Vec::new();
    {
        let mut it = db.iter();
        let mut buf = [0u8; 64];
        while let Some(entry) = it.next() {
            let entry = entry.expect("forward iteration must not fail");
            match entry.status {
                TslStatus::PreWrite => {
                    prewrite += 1;
                    assert_eq!(entry.time, 0, "PRE_WRITE entries expose time 0");
                    assert_eq!(entry.log_len, MAX_LEN, "PRE_WRITE exposes max_len");
                    assert_eq!(entry.log_addr, u32::MAX, "PRE_WRITE has no payload");
                    assert_eq!(
                        it.read_blob(&entry, &mut buf).unwrap(),
                        0,
                        "PRE_WRITE payload is not readable"
                    );
                }
                TslStatus::Unused => panic!("forward iteration yielded an UNUSED entry"),
                _ => {
                    let n = it.read_blob(&entry, &mut buf).expect("payload read");
                    assert_eq!(n, 32);
                    assert_eq!(
                        &buf[..32],
                        &payload_for(entry.time)[..],
                        "payload of committed entry {}",
                        entry.time
                    );
                    assert_eq!(entry.log_len, 32);
                    if let Some(prev) = committed.last() {
                        assert_eq!(entry.time, *prev + 1, "committed entries are contiguous");
                    }
                    committed.push(entry.time);
                }
            }
            forward.push(entry);
        }
    }
    assert!(
        prewrite <= 1,
        "a single power failure can leave at most one PRE_WRITE record"
    );
    if prewrite == 1 {
        // A torn append is always the last record of the database.
        let last = forward.last().unwrap();
        assert_eq!(last.status, TslStatus::PreWrite);
    }

    // Reverse iteration is the mirror image of forward iteration.
    let mut reverse: Vec<_> = db
        .iter_reverse()
        .map(|e| e.expect("reverse iteration"))
        .collect();
    reverse.reverse();
    assert_eq!(forward, reverse, "reverse iteration must mirror forward");

    // The surviving committed window can only be a suffix of the workload.
    let last_committed = committed.last().copied().unwrap_or(0);
    assert!(
        last_committed <= done,
        "recovered timestamp {last_committed} after {done} committed appends"
    );
    if let Some(first) = committed.first() {
        assert!(*first >= 1 && *first <= done);
        assert_eq!(
            committed.len() as i32,
            last_committed - first + 1,
            "surviving entries are contiguous"
        );
    }

    // query_count agrees with the scan as long as there is no torn record.
    // (A PRE_WRITE record reports time 0, which breaks the "timestamps are
    // ordered" assumption of the by-time binary search; C has the same
    // limitation, so by-time scans after a torn append are not asserted here.)
    if prewrite == 0 {
        assert_eq!(
            db.query_count(0, i32::MAX, TslStatus::Write).unwrap(),
            committed.len()
        );
    }

    // last_time is always the newest committed timestamp; when a damaged header
    // forced a re-format the database is empty and the timestamp is 0.
    assert_eq!(
        db.last_time(),
        last_committed,
        "last_time must be the newest committed timestamp"
    );
    let reformatted = committed.is_empty() && done > 0;

    // If the crash happened in the tiny window where every sector has been
    // sealed FULL and the next sector was not yet formatted, the database
    // reports `SavedFull` on the next append (same as C). In every other state
    // the database is still writable.
    let next = db.last_time().saturating_add(1);
    match db.append(next, &payload_for(next)) {
        Ok(()) => {
            assert_eq!(db.last_time(), next);
            let flash = db.into_flash();
            let mut db = Tsdb::mount(flash, config(), MAX_LEN).unwrap();
            let committed_after: Vec<i32> = db
                .iter()
                .filter_map(|e| {
                    let e = e.unwrap();
                    (e.status != TslStatus::PreWrite && e.status != TslStatus::Unused)
                        .then_some(e.time)
                })
                .collect();
            let mut want = committed.clone();
            want.push(next);
            if reformatted {
                want = vec![next];
            }
            // The append may have rolled over, evicting the oldest sector:
            // what remains must be a suffix of the expected window.
            assert!(committed_after.len() <= want.len());
            let tail = &want[want.len() - committed_after.len()..];
            assert_eq!(committed_after, tail, "the appended record must be visible");
            assert_eq!(db.last_time(), next);
        }
        Err(flashdb::Error::SavedFull) => {
            assert_eq!(committed.len(), 4 * 77);
        }
        Err(e) => panic!("append after recovery failed with {e:?}"),
    }

    Recovered {
        committed,
        last_time: last_committed,
    }
}

fn exercise_workload(appends: i32) {
    let ops = workload_ops(appends);
    let mut crashes = 0;
    for fail in 0..ops {
        let (image, done) = replay(appends, fail);
        if done < appends {
            crashes += 1;
        }
        let recovered = recover(&image, done);
        assert_eq!(
            recovered.last_time,
            recovered.committed.last().copied().unwrap_or(0)
        );
    }
    assert!(
        crashes > appends as u64 / 4,
        "too few injected crashes: {crashes}"
    );
}

#[test]
fn recovers_from_a_fault_at_every_write_without_rollover() {
    // 120 appends: fills sector 0 (77 entries) and part of sector 1, so the
    // workload exercises sector sealing without wrapping.
    exercise_workload(120);
}

#[test]
fn recovers_from_a_fault_at_every_write_across_a_wrap() {
    // 400 appends: the ring wraps once, so crashes also hit the wrap paths
    // (format the oldest sector, all-sectors-full states, ...).
    exercise_workload(400);
}

/// Two power failures in a row during the end-info writes: C hits `assert(0)`
/// when a sector has neither a valid end_info[0] nor end_info[1]; the Rust
/// implementation must scan the sector instead and stay usable.
#[test]
fn recovers_from_interrupted_end_info_twice() {
    let mut db = Tsdb::mount(Nor::new(SEC_SIZE), config(), MAX_LEN).unwrap();
    for i in 1..=77 {
        db.append(i, &payload_for(i)).unwrap();
    }
    let image = db.into_flash().data().to_vec();

    // The next append seals sector 0: fail on the 4th write (end_info[0] never
    // becomes WRITE), leaving a PRE_WRITE status in the header.
    let (crash1, failed) = fault_append(&image, 4);
    assert!(failed);
    // end_info[0].status has the PRE_WRITE granule cleared, the WRITE one not.
    assert_eq!(crash1[SEC_END0_STAT_OFF as usize], 0x00);
    assert_eq!(crash1[SEC_END0_STAT_OFF as usize + 1], 0xFF);

    // Remount: the sector is still USING with an interrupted end_info[0].
    let mut flash = Nor::new(SEC_SIZE);
    flash.load(&crash1).unwrap();
    let mut db = Tsdb::mount(flash, config(), MAX_LEN).unwrap();
    assert_eq!(db.last_time(), 77);
    assert_eq!(db.iter().count(), 77);
    // A second power failure during the retried seal: end_info[1].status becomes
    // PRE_WRITE (1st write) and the timestamp write (2nd write) is torn off.
    let image1 = db.into_flash().data().to_vec();
    let (crash2, failed) = fault_append(&image1, 2);
    assert!(failed);
    // Both end info slots are now PRE_WRITE: no valid end info pair is left.
    assert_eq!(crash2[SEC_END0_STAT_OFF as usize], 0x00);
    assert_eq!(crash2[SEC_END0_STAT_OFF as usize + 1], 0xFF);
    assert_eq!(crash2[SEC_END1_STAT_OFF as usize], 0x00);
    assert_eq!(crash2[SEC_END1_STAT_OFF as usize + 1], 0xFF);

    let mut flash = Nor::new(SEC_SIZE);
    flash.load(&crash2).unwrap();

    // The end info pair is unusable now; recovery must still expose all data.
    let mut db = Tsdb::mount(flash, config(), MAX_LEN).unwrap();
    assert_eq!(db.last_time(), 77);
    let times: Vec<i32> = db.iter().map(|e| e.unwrap().time).collect();
    assert_eq!(times, (1..=77).collect::<Vec<i32>>());
    // It is also still writable: the sector is sealed with the unusable end
    // info pair and the record goes to the next sector.
    db.append(78, &payload_for(78)).unwrap();
    assert_eq!(db.last_time(), 78);
    let times: Vec<i32> = db.iter().map(|e| e.unwrap().time).collect();
    assert_eq!(times, (1..=78).collect::<Vec<i32>>());

    // And the state survives another remount (the FULL sector is scanned).
    let flash = db.into_flash();
    let mut db = Tsdb::mount(flash, config(), MAX_LEN).unwrap();
    assert_eq!(db.last_time(), 78);
    let times: Vec<i32> = db.iter().map(|e| e.unwrap().time).collect();
    assert_eq!(times, (1..=78).collect::<Vec<i32>>());
}

fn exercise_torn_ts_for_wg<const WG: u32>() {
    use flashdb::Tsdb64;
    const SMALL_SEC: u32 = 1024;
    const SMALL_MAX: u32 = 4096;
    let cfg = Config::new(SMALL_SEC, SMALL_MAX);

    let mut db = Tsdb64::mount(RamFlash::<4096, WG>::new(SMALL_SEC), cfg, 64).unwrap();
    for t in 1..=40i64 {
        db.append(t * 10, &payload_for(t as i32)).unwrap();
    }
    let base_image = db.into_flash().data().to_vec();

    for tear_bytes in [1usize, 4, 12, 128, 512] {
        for fail in 0..=60u64 {
            let mut flash = RamFlash::<4096, WG>::new(SMALL_SEC);
            flash.load(&base_image).unwrap();
            flash.fail_torn_after_writes(fail, tear_bytes);
            let mut db = Tsdb64::mount(flash, cfg, 64).unwrap();
            for t in 41..=55i64 {
                if db.append(t * 10, &payload_for(t as i32)).is_err() {
                    break;
                }
            }
            let mut flash = db.into_flash();
            flash.disarm_fault();

            let mut db = Tsdb64::mount(flash, cfg, 64).unwrap_or_else(|e| {
                panic!("WG={WG} tear={tear_bytes} fail={fail}: Tsdb64 remount failed: {e:?}")
            });
            let mut prev = 0i64;
            let mut iter = db.iter();
            while let Some(entry) = iter.next() {
                let entry = entry.unwrap();
                if entry.status == TslStatus::Write {
                    assert!(
                        entry.time > prev,
                        "timestamps must stay strictly increasing"
                    );
                    prev = entry.time;
                    let mut buf = [0u8; 64];
                    let n = iter.read_blob(&entry, &mut buf).unwrap();
                    assert_eq!(&buf[..n], &payload_for((entry.time / 10) as i32)[..]);
                }
            }
            assert_eq!(db.last_time(), prev);
            let next_t = prev + 10;
            match db.append(next_t, &payload_for((next_t / 10) as i32)) {
                Ok(()) => assert_eq!(db.last_time(), next_t),
                Err(flashdb::Error::SavedFull) => {
                    db.clean().unwrap();
                    db.append(10, &payload_for(1)).unwrap();
                    assert_eq!(db.last_time(), 10);
                }
                Err(e) => {
                    panic!("WG={WG} tear={tear_bytes} fail={fail}: post-recovery append: {e:?}")
                }
            }
        }
    }
}

#[test]
fn recovers_from_torn_writes_and_erases_across_granularities_tsdb64() {
    exercise_torn_ts_for_wg::<1>();
    exercise_torn_ts_for_wg::<8>();
    exercise_torn_ts_for_wg::<32>();
    exercise_torn_ts_for_wg::<64>();
    exercise_torn_ts_for_wg::<128>();
}
