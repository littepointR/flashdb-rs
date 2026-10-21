//! KVDB smoke tests: basic CRUD, persistence, GC pressure, recovery.

use flashdb::{Config, Kvdb, KvdbNoCache, KvStatus};
use flashdb_sim::RamFlash;

const SEC: u32 = 4096;
const MAX: u32 = 16384;

fn cfg() -> Config {
    Config::new(SEC, MAX)
}

fn flash8() -> RamFlash<16384, 8> {
    RamFlash::<16384, 8>::new(SEC)
}

#[test]
fn crud_and_persistence() {
    let mut db = Kvdb::mount(flash8(), cfg()).unwrap();
    assert!(db.get(b"a", &mut [0u8; 8]).unwrap().is_none());

    db.set(b"a", b"1").unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(db.get(b"a", &mut buf).unwrap(), Some(1));
    assert_eq!(&buf[..1], b"1");

    // update
    db.set(b"a", b"hello").unwrap();
    assert_eq!(db.get(b"a", &mut buf).unwrap(), Some(5));
    assert_eq!(&buf[..5], b"hello");

    // second key
    db.set(b"b", &[0xAA; 32]).unwrap();
    assert_eq!(db.stats().unwrap().key_count, 2);

    // delete
    assert!(db.remove(b"a").unwrap());
    assert!(db.get(b"a", &mut buf).unwrap().is_none());
    assert!(!db.remove(b"a").unwrap());
    assert_eq!(db.stats().unwrap().key_count, 1);

    // remount
    let mut flash = db.into_flash();
    flash.reset_stats();
    let mut db = Kvdb::mount(flash, cfg()).unwrap();
    let mut buf32 = [0u8; 32];
    assert_eq!(db.get(b"b", &mut buf32).unwrap(), Some(32));
    assert_eq!(buf32, [0xAA; 32]);
    assert_eq!(db.stats().unwrap().key_count, 1);
    db.check().unwrap();
}

#[test]
fn defaults_on_fresh_flash() {
    let flash = flash8();
    let mut db = Kvdb::mount_with_defaults(
        flash,
        cfg(),
        &[("version", b"1.2.3"), ("boot", b"\x00")],
    )
    .unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(db.get(b"version", &mut buf).unwrap(), Some(5));
    assert_eq!(&buf[..5], b"1.2.3");
    assert_eq!(db.get(b"boot", &mut buf).unwrap(), Some(1));

    // Re-mount must NOT rewrite defaults (existing values preserved).
    db.set(b"version", b"9.9").unwrap();
    let flash = db.into_flash();
    let mut db = Kvdb::mount_with_defaults(flash, cfg(), &[("version", b"1.2.3")]).unwrap();
    assert_eq!(db.get(b"version", &mut buf).unwrap(), Some(3));
    assert_eq!(&buf[..3], b"9.9");
}

#[test]
fn update_stress_triggers_gc_and_survives_remount() {
    let mut db = Kvdb::mount(flash8(), cfg()).unwrap();
    let mut value = [0u8; 32];
    for i in 0..300u32 {
        value[..4].copy_from_slice(&i.to_le_bytes());
        db.set(b"counter", &value).unwrap();
        let mut buf = [0u8; 32];
        assert_eq!(db.get(b"counter", &mut buf).unwrap(), Some(32));
        assert_eq!(&buf[..4], &i.to_le_bytes());
    }
    db.check().unwrap();
    let mut flash = db.into_flash();
    flash.reset_stats();
    let mut db = Kvdb::mount(flash, cfg()).unwrap();
    let mut buf = [0u8; 32];
    assert_eq!(db.get(b"counter", &mut buf).unwrap(), Some(32));
    assert_eq!(&buf[..4], &299u32.to_le_bytes());
    db.check().unwrap();
}

#[test]
fn many_keys_iterate_and_update() {
    let mut db = Kvdb::mount(flash8(), cfg()).unwrap();
    for i in 0..24u32 {
        let key = [b'k', (i / 10) as u8 + b'0', (i % 10) as u8 + b'0'];
        let value = [i as u8; 48];
        db.set(&key, &value).unwrap();
    }
    assert_eq!(db.stats().unwrap().key_count, 24);

    let mut count = 0;
    let mut it = db.iter();
    while let Some(entry) = it.next() {
        let entry = entry.unwrap();
        assert_eq!(entry.status, KvStatus::Write);
        let mut buf = [0u8; 64];
        let n = it.read_value(&entry, &mut buf).unwrap();
        assert_eq!(n, 48);
        count += 1;
    }
    assert_eq!(count, 24);

    // update all keys twice
    for round in 0..2u8 {
        for i in 0..24u32 {
            let key = [b'k', (i / 10) as u8 + b'0', (i % 10) as u8 + b'0'];
            let value = [i as u8 + round; 48];
            db.set(&key, &value).unwrap();
        }
    }
    assert_eq!(db.stats().unwrap().key_count, 24);
    db.check().unwrap();
}

#[test]
fn no_cache_variant() {
    let mut db = KvdbNoCache::mount(flash8(), cfg()).unwrap();
    db.set(b"x", b"y").unwrap();
    let mut buf = [0u8; 4];
    assert_eq!(db.get(b"x", &mut buf).unwrap(), Some(1));
    assert_eq!(&buf[..1], b"y");
}

#[test]
fn write_gran_1_bit_status() {
    let flash = RamFlash::<16384, 1>::new(SEC);
    let mut db = Kvdb::mount(flash, cfg()).unwrap();
    for i in 0..10u32 {
        let key = [b'n', i as u8];
        db.set(&key, &[i as u8; 16]).unwrap();
    }
    let mut buf = [0u8; 16];
    assert_eq!(db.get(&[b'n', 7], &mut buf).unwrap(), Some(16));
    assert_eq!(buf[0], 7);
    db.check().unwrap();
    // remount
    let flash = db.into_flash();
    let mut db = Kvdb::mount(flash, cfg()).unwrap();
    assert_eq!(db.get(&[b'n', 7], &mut buf).unwrap(), Some(16));
    assert_eq!(db.stats().unwrap().key_count, 10);
}

#[test]
fn power_cut_recovery() {
    // Capture the flash image after a sequence of successful operations, then
    // replay the same sequence with a fault injected at every flash op index,
    // remount and verify the database is still consistent and usable.
    let mut db = Kvdb::mount(flash8(), cfg()).unwrap();
    db.set(b"k0", b"value-0").unwrap();
    db.set(b"k1", b"value-1").unwrap();
    db.set(b"k0", b"value-0-updated").unwrap();
    let good = db.into_flash();
    let baseline = good.data().to_vec();

    // Determine how many flash ops the failed run takes.
    for cut in 0..40u64 {
        let mut flash = flash8();
        flash.load(&baseline).unwrap();
        flash.fail_after_ops(cut);
        // The failed run may fail anywhere, including during mount.
        if let Ok(mut db) = Kvdb::mount(&mut flash, cfg()) {
            let _ = db.set(b"k2", b"value-2");
            let _ = db.set(b"k0", b"again");
        }
        flash.disarm_fault();

        // Remount must succeed and be consistent.
        let mut db = Kvdb::mount(&mut flash, cfg()).unwrap();
        db.check().unwrap();
        for key in [b"k0".as_slice(), b"k1", b"k2"] {
            let mut buf = [0u8; 32];
            if let Some(n) = db.get(key, &mut buf).unwrap() {
                assert!(n > 0);
            }
        }
        // Still writable.
        db.set(b"post", b"ok").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(db.get(b"post", &mut buf).unwrap(), Some(2));
    }
}
