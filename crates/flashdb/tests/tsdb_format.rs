//! On-flash format, mount/format and append semantics of the TSDB.
//!
//! These tests pin the byte layout produced by the Rust implementation against
//! the offsets documented in [`flashdb::low`] (which mirror the C structs
//! `sector_hdr_data` / `log_idx_data` for `FDB_WRITE_GRAN = 8`,
//! `fdb_time_t = int32_t`).

use flashdb::{Config, Error, Flash, Tsdb, TslStatus};
use flashdb_sim::RamFlash;

const SEC_SIZE: u32 = 4096;
const MAX_SIZE: u32 = 16384;
const MAX_LEN: u32 = 256;

type Nor = RamFlash<16384, 8>;

fn payload(i: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|j| ((0x5Au32.wrapping_add(i as u32 * 13).wrapping_add(j as u32 * 5)) & 0xFF) as u8)
        .collect()
}

fn mount(flash: Nor) -> Tsdb<Nor> {
    Tsdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE), MAX_LEN).expect("mount")
}

/// Sector header offsets for `FDB_WRITE_GRAN = 8`, `fdb_time_t = int32_t`
/// (see the layout table in `low::ts_layout_32`).
const SEC_HDR_SIZE: usize = 44;
const SEC_STATUS_OFF: usize = 0;
const SEC_MAGIC_OFF: usize = 3;
const SEC_START_TIME_OFF: usize = 7;
const SEC_END0_TIME_OFF: usize = 11;
const SEC_END0_IDX_OFF: usize = 15;
const SEC_END0_STAT_OFF: usize = 19;
const SEC_END1_TIME_OFF: usize = 24;
const SEC_END1_IDX_OFF: usize = 28;
const SEC_END1_STAT_OFF: usize = 32;

/// `log_idx_data` offsets for the same configuration.
const IDX_SIZE: usize = 20;
const IDX_STAT_OFF: usize = 0;
const IDX_TIME_OFF: usize = 8;
const IDX_LOG_LEN_OFF: usize = 12;
const IDX_LOG_ADDR_OFF: usize = 16;

fn get_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
}

fn get_i32(data: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(data[off..off + 4].try_into().unwrap())
}

#[test]
fn mount_on_erased_flash_formats_every_sector() {
    let db = mount(Nor::new(SEC_SIZE));
    assert_eq!(db.last_time(), 0);
    assert!(db.rollover(), "flashdb defaults rollover to true");
    assert_eq!(db.max_len(), MAX_LEN);

    let flash = db.into_flash();
    let data = flash.data();
    for sector in 0..(MAX_SIZE / SEC_SIZE) as usize {
        let base = sector * SEC_SIZE as usize;
        // status table: 3 bytes, EMPTY (1) clears the first write granule
        assert_eq!(data[base + SEC_STATUS_OFF], 0x00, "sector {sector} status");
        assert_eq!(&data[base + SEC_STATUS_OFF + 1..base + SEC_MAGIC_OFF], &[0xFF, 0xFF]);
        // magic word `TSL0` little endian, padded to 4 bytes with 0xFF
        assert_eq!(
            &data[base + SEC_MAGIC_OFF..base + SEC_START_TIME_OFF],
            &[0x54, 0x53, 0x4C, 0x30],
            "sector {sector} magic"
        );
        // start time and end info are still erased
        assert!(
            data[base + SEC_START_TIME_OFF..base + SEC_HDR_SIZE]
                .iter()
                .all(|b| *b == 0xFF),
            "sector {sector} rest of header must be erased"
        );
        // nothing was written past the header
        assert!(data[base + SEC_HDR_SIZE..base + SEC_SIZE as usize]
            .iter()
            .all(|b| *b == 0xFF));
    }
    // format_sector() writes the EMPTY status and the magic word per sector
    assert_eq!(flash.stats().erase_ops, 4);
    assert_eq!(flash.stats().write_ops, 8);
}

#[test]
fn append_writes_c_header_layout() {
    let mut db = mount(Nor::new(SEC_SIZE));
    let data = payload(0, 32);
    db.append(1000, &data).unwrap();
    let flash = db.into_flash();
    let image = flash.data();
    let base = 0usize;

    // status: USING (2) -> two erased chunks written
    assert_eq!(image[base + SEC_STATUS_OFF], 0x00);
    assert_eq!(image[base + SEC_STATUS_OFF + 1], 0x00);
    // start_time
    assert_eq!(get_i32(image, base + SEC_START_TIME_OFF), 1000);
    // index record at the top of the sector
    let idx = base + SEC_HDR_SIZE;
    // status PRE_WRITE (chunk 0) then WRITE (chunk 1)
    assert_eq!(&image[idx + IDX_STAT_OFF..idx + IDX_STAT_OFF + 5], &[0x00, 0x00, 0xFF, 0xFF, 0xFF]);
    assert_eq!(get_i32(image, idx + IDX_TIME_OFF), 1000);
    assert_eq!(get_u32(image, idx + IDX_LOG_LEN_OFF), 32);
    let log_addr = get_u32(image, idx + IDX_LOG_ADDR_OFF);
    assert_eq!(log_addr, SEC_SIZE - 32);
    // payload at the bottom of the sector
    assert_eq!(&image[log_addr as usize..log_addr as usize + 32], &data[..]);

    // Remount and check the recovered RAM state.
    let mut db = mount(flash);
    assert_eq!(db.last_time(), 1000);
    let entries: Vec<_> = db.iter().map(|e| e.unwrap()).collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, TslStatus::Write);
    assert_eq!(entries[0].time, 1000);
    assert_eq!(entries[0].log_len, 32);
    assert_eq!(entries[0].log_addr, SEC_SIZE - 32);
}

#[test]
fn append_rejects_bad_input() {
    let mut db = mount(Nor::new(SEC_SIZE));
    // payload larger than max_len
    assert!(matches!(
        db.append(1, &[0u8; MAX_LEN as usize + 1]),
        Err(Error::InvalidConfig)
    ));
    // first append must be > last_time (0)
    assert!(matches!(db.append(0, b"x"), Err(Error::InvalidConfig)));
    assert!(matches!(db.append(-5, b"x"), Err(Error::InvalidConfig)));
    db.append(1, b"x").unwrap();
    // strictly increasing timestamps
    assert!(matches!(db.append(1, b"x"), Err(Error::InvalidConfig)));
    assert!(matches!(db.append(0, b"x"), Err(Error::InvalidConfig)));
    db.append(2, b"x").unwrap();
    assert_eq!(db.last_time(), 2);
}

#[test]
fn mount_rejects_max_len_not_below_sector_size() {
    let err = Tsdb::mount(Nor::new(SEC_SIZE), Config::new(SEC_SIZE, MAX_SIZE), SEC_SIZE);
    assert!(matches!(err, Err(Error::InvalidConfig)));
}

#[test]
fn mount_rejects_bad_geometry() {
    // max_size must be a multiple of sec_size and cover at least 2 sectors
    assert!(matches!(
        Tsdb::mount(Nor::new(SEC_SIZE), Config::new(SEC_SIZE, 3 * 1024), MAX_LEN),
        Err(Error::InvalidConfig)
    ));
    assert!(matches!(
        Tsdb::mount(Nor::new(SEC_SIZE), Config::new(SEC_SIZE, SEC_SIZE), MAX_LEN),
        Err(Error::InvalidConfig)
    ));
}

#[test]
fn fills_until_saved_full_with_rollover_disabled() {
    let mut db = mount(Nor::new(SEC_SIZE));
    db.set_rollover(false);
    assert!(!db.rollover());

    // max_blob_count() assumes every log has the maximum payload size:
    // (4096 - 44) / (20 + 256) = 14 per sector, 4 sectors
    assert_eq!(db.max_blob_count(), 4 * ((SEC_SIZE as usize - 44) / (20 + 256)));
    // With 32-byte payloads a sector holds (4096 - 44) / (20 + 32) = 77 logs.
    let expected_entries = 4 * 77;

    let mut i = 0usize;
    loop {
        let data = payload(i, 32);
        match db.append(1000 + i as i32, &data) {
            Ok(()) => i += 1,
            Err(Error::SavedFull) => break,
            Err(e) => panic!("unexpected append error: {e:?}"),
        }
    }
    assert_eq!(i, expected_entries, "capacity for 32-byte logs");
    assert_eq!(db.last_time(), 1000 + expected_entries as i32 - 1);

    // Already full: every further append is rejected.
    assert!(matches!(db.append(9999, b"x"), Err(Error::SavedFull)));
    assert_eq!(db.last_time(), 1000 + expected_entries as i32 - 1);

    // Forward iteration sees every entry, in order, with the right payload.
    let mut it = db.iter();
    let mut n = 0usize;
    let mut last = 1000 - 1;
    let mut buf = [0u8; 64];
    while let Some(entry) = it.next() {
        let entry = entry.unwrap();
        assert_eq!(entry.status, TslStatus::Write);
        assert_eq!(entry.time, 1000 + n as i32);
        assert_eq!(entry.log_len, 32);
        assert_eq!(entry.time, last + 1);
        last = entry.time;
        let len = it.read_blob(&entry, &mut buf).unwrap();
        assert_eq!(len, 32);
        assert_eq!(&buf[..len], &payload(n, 32)[..]);
        n += 1;
    }
    assert_eq!(n, expected_entries);
    drop(it);

    // Reverse iteration is the exact mirror image.
    let forward: Vec<_> = db.iter().map(|e| e.unwrap()).collect();
    let mut reverse: Vec<_> = db.iter_reverse().map(|e| e.unwrap()).collect();
    reverse.reverse();
    assert_eq!(forward, reverse);

    // Remount of the fully populated flash: still full, same entries.
    let flash = db.into_flash();
    let mut db = mount(flash);
    assert_eq!(db.iter().count(), expected_entries);
    assert_eq!(
        db.last_time(),
        1000 + expected_entries as i32 - 1,
        "last_time is recovered from the sealed end info"
    );
    assert!(matches!(db.append(9999, b"x"), Err(Error::SavedFull)));
}

#[test]
fn remount_keeps_cur_sector_and_continues_appending() {
    let mut db = mount(Nor::new(SEC_SIZE));
    // Fill most of the first sector, then remount and keep going.
    for i in 0..70 {
        db.append(100 + i, &payload(i as usize, 32)).unwrap();
    }
    let flash = db.into_flash();
    let mut db = mount(flash);
    assert_eq!(db.last_time(), 169);
    for i in 70..80 {
        db.append(100 + i, &payload(i as usize, 32)).unwrap();
    }
    assert_eq!(db.iter().count(), 80);
    let times: Vec<_> = db.iter().map(|e| e.unwrap().time).collect();
    assert_eq!(times, (100..180).collect::<Vec<i32>>());
}

#[test]
fn end_info_is_written_when_a_sector_is_sealed() {
    let mut db = mount(Nor::new(SEC_SIZE));
    // Fill the first sector (77 entries of 32 bytes) and start the next one.
    for i in 0..78 {
        db.append(1 + i, &payload(i as usize, 32)).unwrap();
    }
    let flash = db.into_flash();
    let image = flash.data();
    let base = 0usize;
    // sector store status now FULL (3): three granules cleared
    assert_eq!(&image[base + SEC_STATUS_OFF..base + SEC_STATUS_OFF + 3], &[0, 0, 0]);
    // end_info[0]: time = last timestamp of the sealed sector, index = last index
    assert_eq!(get_i32(image, base + SEC_END0_TIME_OFF), 77);
    assert_eq!(
        get_u32(image, base + SEC_END0_IDX_OFF),
        (SEC_HDR_SIZE + 76 * IDX_SIZE) as u32
    );
    // end_info status: PRE_WRITE granule then WRITE granule
    assert_eq!(&image[base + SEC_END0_STAT_OFF..base + SEC_END0_STAT_OFF + 2], &[0, 0]);
    // end_info[1] is untouched
    assert!(image[base + SEC_END1_TIME_OFF..base + SEC_END1_STAT_OFF + 5]
        .iter()
        .all(|b| *b == 0xFF));
    // sector 1 is in use and starts at timestamp 78
    let base1 = SEC_SIZE as usize;
    assert_eq!(&image[base1 + SEC_STATUS_OFF..base1 + SEC_STATUS_OFF + 2], &[0, 0]);
    assert_eq!(get_i32(image, base1 + SEC_START_TIME_OFF), 78);
}

#[test]
fn clean_erases_all_sectors_and_resets_state() {
    let mut db = mount(Nor::new(SEC_SIZE));
    for i in 0..100 {
        db.append(1 + i, &payload(i as usize, 32)).unwrap();
    }
    db.clean().unwrap();
    assert_eq!(db.last_time(), 0);
    assert_eq!(db.iter().count(), 0);
    assert_eq!(db.iter_reverse().count(), 0);

    let flash = db.into_flash();
    let image = flash.data();
    for sector in 0..(MAX_SIZE / SEC_SIZE) as usize {
        let base = sector * SEC_SIZE as usize;
        assert_eq!(image[base], 0x00);
        assert_eq!(
            &image[base + SEC_MAGIC_OFF..base + SEC_START_TIME_OFF],
            &[0x54, 0x53, 0x4C, 0x30]
        );
        assert!(image[base + SEC_HDR_SIZE..base + SEC_SIZE as usize]
            .iter()
            .all(|b| *b == 0xFF));
    }

    // The database is usable again and timestamps restart from scratch.
    let mut db = mount(flash);
    assert_eq!(db.last_time(), 0);
    db.append(1, b"fresh").unwrap();
    assert_eq!(db.iter().count(), 1);
}

#[test]
fn set_status_moves_the_record_status() {
    let mut db = mount(Nor::new(SEC_SIZE));
    for i in 0..10 {
        db.append(1 + i, &payload(i as usize, 8)).unwrap();
    }
    let entries: Vec<_> = db.iter().map(|e| e.unwrap()).collect();
    for entry in &entries[..5] {
        db.set_status(entry, TslStatus::UserStatus1).unwrap();
    }
    for entry in &entries[5..] {
        db.set_status(entry, TslStatus::Deleted).unwrap();
    }
    let statuses: Vec<_> = db.iter().map(|e| e.unwrap().status).collect();
    assert!(statuses[..5].iter().all(|s| *s == TslStatus::UserStatus1));
    assert!(statuses[5..].iter().all(|s| *s == TslStatus::Deleted));
    // query_count filters by status
    assert_eq!(
        db.query_count(0, i32::MAX, TslStatus::UserStatus1).unwrap(),
        5
    );
    assert_eq!(db.query_count(0, i32::MAX, TslStatus::Deleted).unwrap(), 5);
    assert_eq!(db.query_count(0, i32::MAX, TslStatus::Write).unwrap(), 0);
    // The status survives a remount (it is stored in flash).
    let flash = db.into_flash();
    let mut db = mount(flash);
    assert_eq!(
        db.query_count(0, i32::MAX, TslStatus::UserStatus1).unwrap(),
        5
    );
}

#[test]
fn remount_after_clean_does_not_keep_old_data() {
    let mut db = mount(Nor::new(SEC_SIZE));
    for i in 0..10 {
        db.append(1 + i, b"x").unwrap();
    }
    let flash = db.into_flash();
    let mut db = mount(flash);
    db.clean().unwrap();
    let flash = db.into_flash();
    let mut db = mount(flash);
    assert_eq!(db.last_time(), 0);
    assert_eq!(db.iter().count(), 0);
}

/// Write granularity 1 (bit packed status tables) must use the same algorithm.
#[test]
fn works_with_write_granularity_one() {
    type Nor1 = RamFlash<16384, 1>;
    let mut db: Tsdb<Nor1> =
        Tsdb::mount(Nor1::new(SEC_SIZE), Config::new(SEC_SIZE, MAX_SIZE), MAX_LEN).unwrap();
    for i in 0..120 {
        db.append(1 + i, &payload(i as usize, 32)).unwrap();
    }
    assert_eq!(db.iter().count(), 120);
    let flash = db.into_flash();
    let mut db: Tsdb<Nor1> =
        Tsdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE), MAX_LEN).unwrap();
    assert_eq!(db.last_time(), 120);
    assert_eq!(db.iter().count(), 120);
    let mut it = db.iter();
    let first = it.next().unwrap().unwrap();
    let mut buf = [0u8; 32];
    assert_eq!(it.read_blob(&first, &mut buf).unwrap(), 32);
    assert_eq!(&buf[..], &payload(0, 32)[..]);
}

/// Write granularity 32 (word writes, `FDB_WRITE_GRAN=32`) smoke test.
#[test]
fn works_with_write_granularity_32() {
    type Nor32 = RamFlash<16384, 32>;
    let mut db: Tsdb<Nor32> =
        Tsdb::mount(Nor32::new(SEC_SIZE), Config::new(SEC_SIZE, MAX_SIZE), MAX_LEN).unwrap();
    for i in 0..100 {
        db.append(1 + i, &payload(i as usize, 32)).unwrap();
    }
    assert_eq!(db.iter().count(), 100);
    let flash = db.into_flash();
    let mut db: Tsdb<Nor32> =
        Tsdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE), MAX_LEN).unwrap();
    assert_eq!(db.last_time(), 100);
    assert_eq!(db.iter().count(), 100);
}
