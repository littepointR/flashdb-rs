//! Power-cut robustness of `Kvdb::check()`.
//!
//! A reset during a record write leaves a torn header: the record never reached
//! the committed `Write` status, but its status granule is programmed and its
//! body is incomplete. Mount recovery already skips such a hole; `check()` must
//! do the same instead of reporting the whole database as corrupt.
//!
//! The fault position is swept over every flash operation of one KV update, so
//! both the "torn header" window and all normal interruption points are hit.

use flashdb::{Config, Kvdb};
use flashdb_sim::RamFlash;

const SEC: u32 = 8192;
const MAX: u32 = 32768;

fn cfg() -> Config {
    Config::new(SEC, MAX)
}

fn key(i: u32) -> [u8; 3] {
    [b'k', b'0' + (i / 10) as u8, b'0' + (i % 10) as u8]
}

fn value(i: u32) -> [u8; 24] {
    let mut v = [0u8; 24];
    for (j, b) in v.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31).wrapping_add(j as u8 * 7);
    }
    v
}

/// Fill a database with eight keys and return the raw flash image.
fn prefilled<const WG: u32>() -> Vec<u8> {
    let mut db = Kvdb::mount(RamFlash::<32768, WG>::new(SEC), cfg()).unwrap();
    for i in 0..8 {
        db.set(&key(i), &value(i)).unwrap();
    }
    db.into_flash().data().to_vec()
}

/// Interrupt the update of key 0 at every flash-op position, remount and require
/// a usable database whose `check()` is clean.
fn sweep<const WG: u32>() {
    let image = prefilled::<WG>();
    let mut interruptions = 0u32;
    for fail in 0..=400u64 {
        let mut flash = RamFlash::<32768, WG>::new(SEC);
        flash.load(&image).unwrap();
        flash.fail_after_writes(fail);
        let mut db = Kvdb::mount(flash, cfg()).unwrap();
        // This update opens/commits a new record and deletes the old one, so
        // interruption can tear the new header, the deletion or the commit.
        let _ = db.set(&key(0), &value(100));
        let mut flash = db.into_flash();
        flash.disarm_fault();
        interruptions += 1;

        // Remount: recovery must leave a usable, consistent database.
        let mut db = Kvdb::mount(flash, cfg())
            .unwrap_or_else(|e| panic!("WG={WG} fail={fail}: remount failed: {e:?}"));
        db.check().unwrap_or_else(|e| {
            panic!("WG={WG} fail={fail}: check() rejected a recovered DB: {e:?}")
        });
        // The key must still be readable (old or new value, never a CRC error).
        let mut buf = [0u8; 24];
        let n = db
            .get(&key(0), &mut buf)
            .unwrap_or_else(|e| panic!("WG={WG} fail={fail}: get failed: {e:?}"));
        assert!(n.is_some());
        // And the database must accept new writes after recovery.
        db.set(&key(0), &value(200))
            .unwrap_or_else(|e| panic!("WG={WG} fail={fail}: post-recovery set failed: {e:?}"));
        db.check()
            .unwrap_or_else(|e| panic!("WG={WG} fail={fail}: check after new write: {e:?}"));
    }
    assert_eq!(interruptions, 401);
}

#[test]
fn power_cut_torn_header_is_not_corruption_wg8() {
    sweep::<8>();
}

#[test]
fn power_cut_torn_header_is_not_corruption_wg128() {
    sweep::<128>();
}
