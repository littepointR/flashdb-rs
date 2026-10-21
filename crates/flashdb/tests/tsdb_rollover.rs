//! Ring-buffer (rollover) behaviour of the TSDB.

use flashdb::{Config, Error, Tsdb, TslStatus};
use flashdb_sim::RamFlash;

const SEC_SIZE: u32 = 4096;
const MAX_SIZE: u32 = 16384;
const MAX_LEN: u32 = 256;
const PER_SECTOR: i32 = 77; // (4096 - 44) / (20 + 32)

type Nor = RamFlash<16384, 8>;

fn payload_for(i: i32) -> [u8; 32] {
    let mut data = [0u8; 32];
    for (j, b) in data.iter_mut().enumerate() {
        *b = ((i as u32 * 13).wrapping_add(0x5A).wrapping_add(j as u32 * 5) & 0xFF) as u8;
    }
    data
}

fn mount(flash: Nor) -> Tsdb<Nor> {
    Tsdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE), MAX_LEN).unwrap()
}

fn times(db: &mut Tsdb<Nor>) -> Vec<i32> {
    db.iter().map(|e| e.unwrap().time).collect()
}

#[test]
fn wraps_around_and_keeps_the_live_window() {
    let mut db = mount(Nor::new(SEC_SIZE));
    assert!(db.rollover());
    let appends: i32 = 3 * 4 * PER_SECTOR + 17; // 941 appends, 2.3 times around
    for i in 1..=appends {
        db.append(i, &payload_for(i)).unwrap();
    }
    assert_eq!(db.last_time(), appends);

    // The surviving window is the last three full sectors plus the current one:
    // sector 1: 694..=770, sector 2: 771..=847, sector 3: 848..=924,
    // sector 0 (using): 925..=941.
    let expected: Vec<i32> = (appends - 247..=appends).collect();
    assert_eq!(expected.len(), 248);
    assert_eq!(times(&mut db), expected);

    // Remount from flash: the ring state must be recovered identically.
    let flash = db.into_flash();
    let mut db = mount(flash);
    assert_eq!(db.last_time(), appends);
    assert_eq!(times(&mut db), expected);

    // Iterators and by-time scans agree.
    let mut reverse: Vec<i32> = db
        .iter_reverse()
        .map(|e| e.unwrap().time)
        .collect();
    reverse.reverse();
    assert_eq!(reverse, expected);
    let all: Vec<i32> = db
        .iter_by_time(0, i32::MAX)
        .map(|e| e.unwrap().time)
        .collect();
    assert_eq!(all, expected);
    assert_eq!(
        db.query_count(0, i32::MAX, TslStatus::Write).unwrap(),
        expected.len()
    );
    assert_eq!(
        db.query_count(600, 700, TslStatus::Write).unwrap(),
        7, // 694..=700
    );
    assert_eq!(
        db.query_count(600, i32::MAX, TslStatus::Write).unwrap(),
        expected.len(),
    );

    // Payloads are still correct after the wrap.
    let mut it = db.iter();
    while let Some(entry) = it.next() {
        let entry = entry.unwrap();
        let mut buf = [0u8; 32];
        assert_eq!(it.read_blob(&entry, &mut buf).unwrap(), 32);
        assert_eq!(&buf, &payload_for(entry.time));
    }
}

#[test]
fn sector_end_info_tracks_the_ring() {
    let mut db = mount(Nor::new(SEC_SIZE));
    for i in 1..=(3 * 4 * PER_SECTOR + 17) {
        db.append(i, &payload_for(i)).unwrap();
    }
    let flash = db.into_flash();
    let image = flash.data();
    // Sealed sectors in the live window: 1 (ends at 770), 2 (847), 3 (924).
    for (sector, end_time) in [(1usize, 770i32), (2, 847), (3, 924)] {
        let base = sector * SEC_SIZE as usize;
        let time = i32::from_le_bytes(image[base + 11..base + 15].try_into().unwrap());
        assert_eq!(time, end_time, "sector {sector} end_info[0].time");
        let status = &image[base + 19..base + 21];
        assert_eq!(status, &[0x00, 0x00], "sector {sector} end_info[0].status");
    }
    // Sector 0 is currently in use and starts at 925.
    assert_eq!(
        i32::from_le_bytes(image[7..11].try_into().unwrap()),
        925
    );
}

#[test]
fn full_database_with_rollover_disabled_stays_full() {
    let mut db = mount(Nor::new(SEC_SIZE));
    db.set_rollover(false);
    let capacity = 4 * PER_SECTOR;
    for i in 1..=capacity {
        db.append(i, &payload_for(i)).unwrap();
    }
    // A 32-byte log no longer fits (48 bytes are left in the last sector).
    assert!(matches!(
        db.append(capacity + 1, &payload_for(capacity + 1)),
        Err(Error::SavedFull)
    ));
    // Turning rollover back on does not resurrect the database, exactly like C:
    // the failed append already sealed the current sector and append refuses.
    db.set_rollover(true);
    assert!(matches!(
        db.append(capacity + 1, &payload_for(capacity + 1)),
        Err(Error::SavedFull)
    ));
    assert_eq!(db.last_time(), capacity);
    assert_eq!(times(&mut db), (1..=capacity).collect::<Vec<i32>>());

    // Remounting the full image reports every entry and stays full.
    let flash = db.into_flash();
    let mut db = mount(flash);
    assert_eq!(times(&mut db), (1..=capacity).collect::<Vec<i32>>());
    assert_eq!(db.last_time(), capacity);
    assert!(matches!(db.append(capacity + 1, b"x"), Err(Error::SavedFull)));
}

#[test]
fn many_wraps_stay_consistent() {
    let mut db = mount(Nor::new(SEC_SIZE));
    // 10 full rings.
    let appends = 10 * 4 * PER_SECTOR;
    for i in 1..=appends {
        db.append(i, &payload_for(i)).unwrap();
    }
    // At an exact ring boundary the current sector still holds 77 entries.
    let expected: Vec<i32> = (appends - 4 * PER_SECTOR + 1..=appends).collect();
    assert_eq!(expected.len(), (4 * PER_SECTOR) as usize);
    assert_eq!(times(&mut db), expected);
    let flash = db.into_flash();
    let mut db = mount(flash);
    assert_eq!(times(&mut db), expected);
    assert_eq!(db.last_time(), appends);
    // Timestamps keep increasing after the mount.
    db.append(appends + 1, &payload_for(appends + 1)).unwrap();
    let after: Vec<i32> = db.iter().map(|e| e.unwrap().time).collect();
    let expected_after: Vec<i32> = (appends - 3 * PER_SECTOR + 1..=appends + 1).collect();
    assert_eq!(after, expected_after);
}

#[test]
fn by_time_ranges_across_the_ring_wrap() {
    let mut db = mount(Nor::new(SEC_SIZE));
    let appends: i32 = 3 * 4 * PER_SECTOR + 17;
    for i in 1..=appends {
        db.append(i, &payload_for(i)).unwrap();
    }
    let first = *times(&mut db).first().unwrap();
    let last = appends;
    let mut ranges = vec![
        (first, last),
        (first - 1, last + 1),
        (first - 1, first),
        (first, first),
        (last, last),
        (last, last + 10),
        (first - 1, first - 2),
        (last + 1, last + 2),
        (0, first - 1),
        (last + 1, i32::MAX),
        (last, first),
        (i32::MAX, first),
    ];
    // Ranges that straddle each physical sector boundary in the live window.
    for t in [770, 847, 924] {
        ranges.extend([(t - 1, t + 1), (t, t), (t + 1, t - 1), (t - 2, t + 2)]);
    }
    for (from, to) in ranges {
        let expect: Vec<i32> = if from <= to {
            (first..=last).filter(|t| *t >= from && *t <= to).collect()
        } else {
            (first..=last)
                .rev()
                .filter(|t| *t >= to && *t <= from)
                .collect()
        };
        let actual: Vec<i32> = db
            .iter_by_time(from, to)
            .map(|e| e.unwrap().time)
            .collect();
        assert_eq!(actual, expect, "iter_by_time({from}, {to})");
    }
}
