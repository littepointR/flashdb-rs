//! KVDB format/layout coverage across write granularities and edge cases.

use flashdb::{Config, Kvdb, KvdbSmallCache};
use flashdb_sim::RamFlash;

const SEC: u32 = 4096;
const MAX: u32 = 16384;

fn cfg() -> Config {
    Config::new(SEC, MAX)
}

fn exercise<const WG: u32>() {
    let mut flash = RamFlash::<16384, WG>::new(SEC);
    let mut db = Kvdb::mount(&mut flash, cfg()).unwrap_or_else(|e| panic!("mount WG={WG}: {e:?}"));

    // basic CRUD
    db.set(b"alpha", b"one").unwrap();
    db.set(b"beta", &[0x5A; 40]).unwrap();
    let mut buf = [0u8; 64];
    assert_eq!(db.get(b"alpha", &mut buf).unwrap(), Some(3));
    assert_eq!(&buf[..3], b"one");
    assert_eq!(db.get(b"beta", &mut buf).unwrap(), Some(40));
    assert_eq!(&buf[..40], &[0x5A; 40]);

    // empty value and maximum key length
    db.set(b"empty", b"").unwrap();
    assert_eq!(db.get(b"empty", &mut buf).unwrap(), Some(0));
    let name64 = [b'k'; 64];
    db.set(&name64, b"maxkey").unwrap();
    assert_eq!(db.get(&name64, &mut buf).unwrap(), Some(6));

    // update, delete
    db.set(b"alpha", &[1u8; 100]).unwrap();
    assert_eq!(db.get(b"alpha", &mut buf).unwrap(), Some(64));
    assert!(db.remove(b"alpha").unwrap());
    assert_eq!(db.get(b"alpha", &mut buf).unwrap(), None);

    // iteration and stats
    let stats = db.stats().unwrap();
    assert!(stats.key_count >= 3);

    // churn enough to force GC and sector rotation
    let mut value = [0u8; 200];
    for i in 0..300u32 {
        value[..4].copy_from_slice(&i.to_le_bytes());
        db.set(b"churn", &value).unwrap();
    }
    db.check().unwrap();
    assert_eq!(db.get(b"churn", &mut buf[..64]).unwrap(), Some(64));

    // remount and verify
    let mut flash = db.into_flash();
    flash.reset_stats();
    let mut db = Kvdb::mount(&mut flash, cfg()).unwrap();
    db.check().unwrap();
    assert_eq!(db.get(b"beta", &mut buf).unwrap(), Some(40));
    assert_eq!(db.get(&name64, &mut buf).unwrap(), Some(6));
    assert_eq!(db.stats().unwrap().key_count >= 3, true);

    // defaults + explicit format
    db.format_with_defaults(&[("fw", b"1.0".as_slice())])
        .unwrap();
    let mut db2 = Kvdb::mount(db.into_flash(), cfg()).unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(db2.get(b"fw", &mut buf).unwrap(), Some(3));
    assert_eq!(&buf[..3], b"1.0");
    assert_eq!(db2.stats().unwrap().key_count, 1);
}

#[test]
fn write_gran_1() {
    exercise::<1>();
}

#[test]
fn write_gran_8() {
    exercise::<8>();
}

#[test]
fn write_gran_32() {
    exercise::<32>();
}

#[test]
fn write_gran_64() {
    exercise::<64>();
}

#[test]
fn write_gran_128() {
    exercise::<128>();
}

#[test]
fn write_gran_256() {
    exercise::<256>();
}

#[test]
fn small_cache_variant() {
    let mut flash = RamFlash::<16384, 8>::new(SEC);
    let mut db = KvdbSmallCache::mount(&mut flash, cfg()).unwrap();
    for i in 0..8u32 {
        let mut key = [0u8; 8];
        key[0] = b'k';
        key[1] = b'0' + i as u8;
        let mut buf = [0u8; 16];
        db.set(&key[..2], &buf[..8]).unwrap();
        // re-read to fill the cache
        assert!(db.get(&key[..2], &mut buf).unwrap().is_some());
    }
}
