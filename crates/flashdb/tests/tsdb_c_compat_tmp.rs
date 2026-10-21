//! TEMPORARY scratch test used to compare against C-generated images in /tmp.
//! Deleted before completion; the permanent differential tests belong to the
//! harness workstream.

use flashdb::{Config, Tsdb, TslStatus};
use flashdb_sim::RamFlash;

const SEC: u32 = 4096;
const MAX: u32 = 16384;
const MAX_LEN: u32 = 256;

fn ts_payload(i: usize) -> [u8; 32] {
    let mut b = [0u8; 32];
    for (j, v) in b.iter_mut().enumerate() {
        *v = ((0x5Au32 + i as u32 * 13 + j as u32 * 5) & 0xFF) as u8;
    }
    b
}

fn build_rust<const WG: u32>(rollover: bool, limit: usize) -> (Vec<u8>, usize, i32) {
    let mut db: Tsdb<RamFlash<16384, WG>> =
        Tsdb::mount(RamFlash::new(SEC), Config::new(SEC, MAX), MAX_LEN).unwrap();
    db.set_rollover(rollover);
    let mut appended = 0usize;
    let mut last = 0i32;
    for i in 0..limit {
        match db.append(1000 + i as i32, &ts_payload(i)) {
            Ok(()) => {
                appended += 1;
                last = db.last_time();
            }
            Err(flashdb::Error::SavedFull) => break,
            Err(e) => panic!("unexpected append error: {e:?}"),
        }
    }
    (db.into_flash().data().to_vec(), appended, last)
}

fn check<const WG: u32>(path: &str, rollover: bool, limit: usize, want_appended: usize, want_last: i32) {
    let c = std::fs::read(path).unwrap();
    assert_eq!(c.len(), 16384);
    let (rust, appended, last) = build_rust::<WG>(rollover, limit);
    assert_eq!((appended, last), (want_appended, want_last), "{path} workload");
    std::fs::write(path.replace("/c_", "/rust_"), &rust).unwrap();
    if c != rust {
        let first = c
            .iter()
            .zip(rust.iter())
            .position(|(a, b)| a != b)
            .unwrap();
        panic!(
            "{path}: image differs at 0x{first:04X} (C={:02X} rust={:02X})",
            c[first], rust[first]
        );
    }

    // C image -> Rust mount and read.
    let mut flash = RamFlash::<16384, WG>::new(SEC);
    flash.load(&c).unwrap();
    let mut db: Tsdb<RamFlash<16384, WG>> =
        Tsdb::mount(flash, Config::new(SEC, MAX), MAX_LEN).unwrap();
    let mut times = Vec::new();
    let mut it = db.iter();
    let mut buf = [0u8; 32];
    while let Some(e) = it.next() {
        let e = e.unwrap();
        times.push(e.time);
        assert_eq!(e.status, TslStatus::Write);
        let n = it.read_blob(&e, &mut buf).unwrap();
        assert_eq!(n, 32);
        assert_eq!(buf, ts_payload((e.time - 1000) as usize));
    }
    assert!(!times.is_empty());
    assert_eq!(*times.last().unwrap(), want_last);
    assert_eq!(db.last_time(), want_last);
    // Contiguous increasing window.
    for w in times.windows(2) {
        assert_eq!(w[1], w[0] + 1);
    }
    let mut rev: Vec<i32> = db.iter_reverse().map(|e| e.unwrap().time).collect();
    rev.reverse();
    assert_eq!(rev, times);
}

#[test]
fn matches_c_reference_image_full_wg8() {
    check::<8>("/tmp/tsdb_c_gen/c_full_wg8.bin", false, 400, 308, 1307);
}

#[test]
fn matches_c_reference_image_roll_wg8() {
    check::<8>("/tmp/tsdb_c_gen/c_roll_wg8.bin", true, 400, 400, 1399);
}

#[test]
fn matches_c_reference_image_full_wg1() {
    check::<1>("/tmp/tsdb_c_gen/c_full_wg1.bin", false, 400, 336, 1335);
}

#[test]
fn matches_c_reference_image_roll_wg1() {
    check::<1>("/tmp/tsdb_c_gen/c_roll_wg1.bin", true, 400, 400, 1399);
}
