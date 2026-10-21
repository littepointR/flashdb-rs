//! Iteration semantics: forward, reverse and by-time scans, including the
//! reverse range form (`from > to`) and ranges crossing sector boundaries.

use flashdb::{Config, Tsdb, TslStatus};
use flashdb_sim::RamFlash;

const SEC_SIZE: u32 = 4096;
const MAX_SIZE: u32 = 16384;
const MAX_LEN: u32 = 256;

type Nor = RamFlash<16384, 8>;

/// Timestamps used by the tests: strictly increasing, not contiguous.
fn test_times(count: i32) -> Vec<i32> {
    (1..=count).map(|i| i * 2).collect()
}

fn payload_for(time: i32) -> [u8; 16] {
    let mut data = [0u8; 16];
    for (j, b) in data.iter_mut().enumerate() {
        *b = ((time as u32 * 13).wrapping_add(j as u32 * 7) & 0xFF) as u8;
    }
    data
}

fn populated(count: i32) -> Tsdb<Nor> {
    let mut db = Tsdb::mount(Nor::new(SEC_SIZE), Config::new(SEC_SIZE, MAX_SIZE), MAX_LEN).unwrap();
    db.set_rollover(false);
    for time in test_times(count) {
        db.append(time, &payload_for(time)).unwrap();
    }
    // Re-open from flash so the tests exercise mount recovery as well.
    let flash = db.into_flash();
    Tsdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE), MAX_LEN).unwrap()
}

fn collect_forward(db: &mut Tsdb<Nor>) -> Vec<(i32, u32, TslStatus)> {
    let mut out = Vec::new();
    let mut it = db.iter();
    while let Some(entry) = it.next() {
        let entry = entry.unwrap();
        let mut buf = [0u8; 32];
        let len = it.read_blob(&entry, &mut buf).unwrap() as u32;
        assert_eq!(len, entry.log_len, "read_blob must return the log length");
        out.push((entry.time, entry.log_len, entry.status));
    }
    out
}

fn collect_reverse(db: &mut Tsdb<Nor>) -> Vec<(i32, u32, TslStatus)> {
    let mut out = Vec::new();
    let mut it = db.iter_reverse();
    while let Some(entry) = it.next() {
        let entry = entry.unwrap();
        let mut buf = [0u8; 32];
        let len = it.read_blob(&entry, &mut buf).unwrap() as u32;
        assert_eq!(len, entry.log_len);
        out.push((entry.time, entry.log_len, entry.status));
    }
    out
}

fn collect_by_time(db: &mut Tsdb<Nor>, from: i32, to: i32) -> Vec<i32> {
    let mut times = Vec::new();
    let mut it = db.iter_by_time(from, to);
    while let Some(entry) = it.next() {
        let entry = entry.unwrap();
        let mut buf = [0u8; 32];
        let len = it.read_blob(&entry, &mut buf).unwrap() as u32;
        assert_eq!(len, core::cmp::min(entry.log_len, 32));
        times.push(entry.time);
    }
    times
}

fn expected(times: &[i32], from: i32, to: i32) -> Vec<i32> {
    if from <= to {
        times
            .iter()
            .copied()
            .filter(|t| *t >= from && *t <= to)
            .collect()
    } else {
        times
            .iter()
            .rev()
            .copied()
            .filter(|t| *t >= to && *t <= from)
            .collect()
    }
}

fn check_range(db: &mut Tsdb<Nor>, times: &[i32], from: i32, to: i32) {
    let actual = collect_by_time(db, from, to);
    let want = expected(times, from, to);
    assert_eq!(
        actual, want,
        "iter_by_time({from}, {to}) mismatch (forward={})",
        from <= to
    );
    let writes = db
        .query_count(from, to, TslStatus::Write)
        .unwrap_or_else(|e| panic!("query_count({from}, {to}) failed: {e:?}"));
    assert_eq!(writes, want.len(), "query_count({from}, {to})");
}

#[test]
fn forward_and_reverse_agree() {
    let times = test_times(250);
    let mut db = populated(250);

    let forward = collect_forward(&mut db);
    assert_eq!(forward.len(), times.len());
    let fwd_times: Vec<i32> = forward.iter().map(|(t, _, _)| *t).collect();
    assert_eq!(fwd_times, times);
    assert!(forward
        .iter()
        .all(|(_, len, status)| *len == 16 && *status == TslStatus::Write));

    // Verify the payload bytes through `read_blob`.
    let mut it = db.iter();
    while let Some(entry) = it.next() {
        let entry = entry.unwrap();
        let mut buf = [0u8; 32];
        let len = it.read_blob(&entry, &mut buf).unwrap();
        assert_eq!(len, 16);
        assert_eq!(&buf[..16], &payload_for(entry.time));
    }
    drop(it);

    let mut reverse = collect_reverse(&mut db);
    reverse.reverse();
    assert_eq!(forward, reverse);

    // A second pass must be exactly repeatable (no state left in the DB).
    assert_eq!(collect_forward(&mut db), forward);
}

#[test]
fn empty_database_iterators_yield_nothing() {
    let mut db = populated(0);
    assert_eq!(collect_forward(&mut db), []);
    assert_eq!(collect_reverse(&mut db), []);
    assert_eq!(collect_by_time(&mut db, 0, i32::MAX), []);
    assert_eq!(collect_by_time(&mut db, i32::MAX, 0), []);
    assert_eq!(db.query_count(0, i32::MAX, TslStatus::Write).unwrap(), 0);
}

#[test]
fn by_time_covers_exact_and_out_of_bounds_ranges() {
    let times = test_times(250);
    let mut db = populated(250);
    let min = times[0];
    let max = *times.last().unwrap();

    let mut ranges = vec![
        (min, max),
        (0, i32::MAX),
        (min - 1, max + 1),
        (min - 1, min),
        (min, min),
        (min + 1, min - 1), // reverse single
        (max, max),
        (max + 1, max - 1),
        (max + 1, i32::MAX),
        (i32::MIN, min - 1),
        (min - 2, min - 1),
        (min - 1, min - 2),
        (max + 2, max + 1),
        (max + 1, max + 2),
        (i32::MAX, i32::MIN),
        (i32::MIN, i32::MAX),
        (max, min), // full reverse
    ];
    // Every sector boundary plus its neighbours.
    for sector in 0..4usize {
        let first = min + (sector as i32) * 77 * 2;
        let last = first + 76 * 2;
        ranges.extend([
            (first - 1, last + 1),
            (first, last),
            (first - 1, last),
            (first, last + 1),
            (first + 1, last - 1),
            (last + 1, first - 1),
            (last, first),
            (last - 1, first + 1),
            (first - 2, first - 1),
            (first + 1, first - 1),
            (last + 1, last - 1),
        ]);
    }
    for (from, to) in ranges {
        check_range(&mut db, &times, from, to);
    }
}

#[test]
fn by_time_random_ranges() {
    let times = test_times(250);
    let mut db = populated(250);
    let min = times[0];
    let max = *times.last().unwrap();
    let span = (max - min + 40) as u32;
    // Deterministic LCG, like the benchmark protocol.
    let mut seed = 12345u32;
    for _ in 0..1000 {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let from = min - 20 + ((seed >> 8) % span) as i32;
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let to = min - 20 + ((seed >> 8) % span) as i32;
        check_range(&mut db, &times, from, to);
    }
}

#[test]
fn by_time_matches_a_filtered_full_scan() {
    let mut db = populated(250);
    let forward: Vec<i32> = collect_forward(&mut db).into_iter().map(|(t, _, _)| t).collect();
    let mut seed = 0xDEAD_BEEFu32;
    for _ in 0..200 {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let a = (seed >> 12) as i32 % 600 - 20;
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let b = (seed >> 12) as i32 % 600 - 20;
        let (from, to) = if a <= b { (a, b) } else { (b, a) };
        let want: Vec<i32> = forward
            .iter()
            .copied()
            .filter(|t| *t >= from && *t <= to)
            .collect();
        assert_eq!(collect_by_time(&mut db, from, to), want);
        // The same range reversed is the mirror image.
        let mut back = collect_by_time(&mut db, to, from);
        back.reverse();
        assert_eq!(back, want);
    }
}

#[test]
fn by_time_with_uneven_log_lengths() {
    // Variable payload sizes make sectors hold different entry counts and
    // exercise the by-time search with non-uniform index spacing.
    let mut db = Tsdb::mount(Nor::new(SEC_SIZE), Config::new(SEC_SIZE, MAX_SIZE), MAX_LEN).unwrap();
    db.set_rollover(false);
    let mut times = Vec::new();
    let mut total = 0;
    for i in 0..120i32 {
        let len = match i % 5 {
            0 => 1,
            1 => 17,
            2 => 64,
            3 => 129,
            _ => 255,
        };
        let mut data = [0u8; 255];
        for (j, b) in data[..len].iter_mut().enumerate() {
            *b = ((i as usize * 31 + j * 3) & 0xFF) as u8;
        }
        db.append(10 + i, &data[..len]).unwrap();
        times.push(10 + i);
        total += 1;
    }
    assert_eq!(total, 120);
    let flash = db.into_flash();
    let mut db = Tsdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE), MAX_LEN).unwrap();
    let mut it = db.iter();
    for (i, want_time) in times.iter().enumerate() {
        let entry = it.next().unwrap().unwrap();
        assert_eq!(entry.time, *want_time);
        let len = match i % 5 {
            0 => 1,
            1 => 17,
            2 => 64,
            3 => 129,
            _ => 255,
        };
        assert_eq!(entry.log_len, len);
        let mut buf = [0u8; 255];
        let n = it.read_blob(&entry, &mut buf).unwrap();
        assert_eq!(n, len as usize);
        for (j, b) in buf[..n].iter().enumerate() {
            assert_eq!(*b, ((i * 31 + j * 3) & 0xFF) as u8);
        }
    }
    assert!(it.next().is_none());
    for from in [0, 9, 10, 11, 50, 100, 129, 130, 140, i32::MAX] {
        for to in [0, 9, 10, 20, 100, 129, 130, 140, i32::MAX] {
            check_range(&mut db, &times, from, to);
        }
    }
}

#[test]
fn by_time_after_set_status() {
    let times = test_times(200);
    let mut db = populated(200);
    // Mark half of the entries deleted; the by-time scan must still yield them
    // (C's callback sees every non-UNUSED record).
    let entries: Vec<_> = db.iter().map(|e| e.unwrap()).collect();
    for (i, entry) in entries.iter().enumerate() {
        if i % 2 == 0 {
            db.set_status(entry, TslStatus::Deleted).unwrap();
        }
    }
    assert_eq!(db.query_count(0, i32::MAX, TslStatus::Deleted).unwrap(), 100);
    assert_eq!(db.query_count(0, i32::MAX, TslStatus::Write).unwrap(), 100);
    assert_eq!(collect_by_time(&mut db, 0, i32::MAX), times);
    assert_eq!(
        db.query_count(0, i32::MAX, TslStatus::UserStatus1).unwrap(),
        0
    );
}

#[test]
fn by_time_on_single_entry_database() {
    let mut db = Tsdb::mount(Nor::new(SEC_SIZE), Config::new(SEC_SIZE, MAX_SIZE), MAX_LEN).unwrap();
    db.append(7, &payload_for(7)).unwrap();
    let flash = db.into_flash();
    let mut db = Tsdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE), MAX_LEN).unwrap();
    for (from, to) in [
        (0, 100),
        (7, 7),
        (8, 6),
        (6, 8),
        (i32::MAX, i32::MIN),
        (0, 6),
        (8, 100),
    ] {
        check_range(&mut db, &[7], from, to);
    }
}
