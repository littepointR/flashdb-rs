//! Error paths, fault injection and recovery corners.
//!
//! The other test files check the happy path plus power-cut recovery for the
//! normal API flow. This file targets the branches that only a hostile flash
//! driver or a damaged image can reach: driver errors on every operation,
//! configuration rejections, corrupted headers/values, interrupted
//! garbage-collection, and every iterator error exit. It also sweeps a fault
//! through the *whole* operation sequence of a workload, so each `?` on a
//! flash call is taken at least once instead of only the first one.
//!
//! Two techniques are used:
//!
//! * **Operation sweeps** — replay a fixed flash image with the simulator's
//!   fault armed at flash operation `n`, for every `n`. Each call site in the
//!   workload fails exactly once, and the error must always be a typed
//!   `Error::FlashRead/FlashWrite/FlashErase`.
//! * **Crafted images** — take a valid image, flip status bytes / magic words /
//!   length fields into the state a power cut can leave behind, then remount and
//!   check that recovery and the damaged-image scanners behave.
//!
//! Tests use the write-granularity-8 / 32-bit-timestamp on-flash layout
//! (offsets documented in `tsdb_format.rs` and `low.rs`) so the status-byte
//! manipulation is explicit.

use core::mem::MaybeUninit;

use flashdb::{Config, Error, Kvdb, KvdbImpl, KvdbNoCache, Tsdb, Tsdb64, TslStatus, KV_NAME_MAX};
use flashdb_sim::{RamFlash, SimError};

const SEC: u32 = 4096;
const MAX: u32 = 16384;
const MAX_LEN: u32 = 256;

type Nor = RamFlash<16384, 8>;

/// Write-granularity-8 KVDB sector/record offsets (`low::kv_layout(8)`).
const KV_SEC_STORE_OFF: u32 = 0;
const KV_SEC_DIRTY_OFF: u32 = 3;
const KV_SEC_MAGIC_OFF: u32 = 8;
const KV_HDR: u32 = 28;
const KV_MAGIC_OFF: u32 = 8;
const KV_LEN_OFF: u32 = 12;
const KV_NAME_LEN_OFF: u32 = 20;
const KV_STATUS_SIZE: u32 = 5;
const KV_SEC_STATUS_SIZE: u32 = 3;

/// Write-granularity-8, 32-bit timestamp TSDB layout (`low::ts_layout_32(8)`).
const TS_SEC_STORE_OFF: u32 = 0;
const TS_SEC_MAGIC_OFF: u32 = 3;
const TS_SEC_START_TIME_OFF: u32 = 7;
const TS_SEC_END0_TIME_OFF: u32 = 11;
const TS_SEC_END0_IDX_OFF: u32 = 15;
const TS_SEC_END0_STAT_OFF: u32 = 19;
const TS_SEC_STATUS_SIZE: u32 = 3;
const TS_SEC_HDR: u32 = 44;
const TS_IDX_STAT_SIZE: u32 = 5;
const TS_MAGIC: [u8; 4] = [0x54, 0x53, 0x4C, 0x30]; // "TSL0"

fn cfg() -> Config {
    Config::new(SEC, MAX)
}

fn ts_cfg() -> Config {
    Config::new(SEC, MAX)
}

fn loaded(image: &[u8]) -> Nor {
    let mut flash = Nor::new(SEC);
    flash
        .load(image)
        .expect("flash image size matches the partition");
    flash
}

// ---------------------------------------------------------------------------
// Sweep helpers
// ---------------------------------------------------------------------------

/// Replay `image` with a fault armed at flash operation `n`, for every `n`,
/// until the workload completes. Every failure returned by `body` must be a
/// typed flash error; any other error is a test failure.
///
/// The simulator arms an *absolute operation index* (not "the n-th write"), so
/// sweeping `n` over the whole workload hits every flash call site — reads,
/// writes and erases — exactly once.
///
/// Returns the number of injected failures.
fn sweep_ops<B>(image: &[u8], body: B) -> usize
where
    B: FnMut(Nor) -> Result<(), Error<SimError>>,
{
    sweep_ramflash::<16384, 8, B>(SEC, image, body)
}

/// [`sweep_ops`] for an arbitrary simulator geometry (size in bytes, write
/// granularity in bits, erase block size).
fn sweep_ramflash<const SIZE: usize, const WG: u32, B>(
    erase_size: u32,
    image: &[u8],
    mut body: B,
) -> usize
where
    B: FnMut(RamFlash<SIZE, WG>) -> Result<(), Error<SimError>>,
{
    // A workload that needs more flash operations than this is kept out of
    // this file: the sweep is quadratic in the operation count.
    const MAX_OPS: u64 = 2500;
    let mut injected = 0usize;
    let mut completed = false;
    for n in 0..MAX_OPS {
        let mut flash = RamFlash::<SIZE, WG>::new(erase_size);
        flash
            .load(image)
            .expect("flash image size matches the partition");
        flash.fail_after_ops(n);
        match body(flash) {
            Ok(()) => {
                completed = true;
                break;
            }
            Err(e) => {
                assert!(
                    matches!(
                        e,
                        Error::FlashRead(_) | Error::FlashWrite(_) | Error::FlashErase(_)
                    ),
                    "workload returned a non-flash error at fault index {n}: {e:?}"
                );
                injected += 1;
            }
        }
    }
    assert!(
        completed,
        "fault sweep never completed: workload exceeds {MAX_OPS} flash operations"
    );
    assert!(
        injected > 0,
        "fault sweep injected no failure: the fault index never landed inside the workload"
    );
    injected
}

/// FNV-1a over a key, matching the KVDB name-cache hash.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash = 0x811C_9DC5u32;
    for &b in bytes {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

// ---------------------------------------------------------------------------
// Configuration and flash-driver contract
// ---------------------------------------------------------------------------

#[test]
fn config_geometry_validation() {
    // `Config::sector_count` is public API and has its own zero/division guard.
    assert_eq!(Config::new(0, MAX).sector_count(), 0);
    assert_eq!(Config::new(SEC, MAX).sector_count(), 4);
    assert_eq!(Config::new(SEC, SEC).sector_count(), 1);

    // `sec_size == 0` and non-power-of-two sizes are rejected.
    for bad in [Config::new(0, MAX), Config::new(3000, 12000)] {
        let err = Kvdb::mount(Nor::new(SEC), bad);
        assert!(
            matches!(err, Err(Error::InvalidConfig)),
            "KV geometry {bad:?} must be rejected"
        );
        let err = Tsdb::mount(Nor::new(SEC), bad, MAX_LEN);
        assert!(
            matches!(err, Err(Error::InvalidConfig)),
            "TS geometry {bad:?} must be rejected"
        );
    }
    // `max_size` below two sectors and `max_size` not a multiple of `sec_size`.
    for bad in [Config::new(SEC, SEC), Config::new(SEC, MAX + 1)] {
        let err = Kvdb::mount(Nor::new(SEC), bad);
        assert!(
            matches!(err, Err(Error::InvalidConfig)),
            "geometry {bad:?} must be rejected"
        );
    }
}

/// A driver whose write granularity is not one of the supported values.
struct Wg16Flash;

impl flashdb::Flash for Wg16Flash {
    type Error = core::convert::Infallible;

    const WRITE_GRAN_BITS: u32 = 16;

    fn read(&mut self, _: u32, _: &mut [u8]) -> Result<(), Self::Error> {
        Ok(())
    }
    fn write(&mut self, _: u32, _: &[u8]) -> Result<(), Self::Error> {
        Ok(())
    }
    fn erase(&mut self, _: u32, _: u32) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// A driver that models a different erased value: the on-flash format is
/// defined in terms of `0xFF`, so this must be rejected instead of silently
/// producing an incompatible image.
struct BadErasedFlash;

impl flashdb::Flash for BadErasedFlash {
    type Error = core::convert::Infallible;

    const WRITE_GRAN_BITS: u32 = 8;
    const ERASED_BYTE: u8 = 0x00;

    fn read(&mut self, _: u32, _: &mut [u8]) -> Result<(), Self::Error> {
        Ok(())
    }
    fn write(&mut self, _: u32, _: &[u8]) -> Result<(), Self::Error> {
        Ok(())
    }
    fn erase(&mut self, _: u32, _: u32) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[test]
fn unsupported_driver_contract_is_rejected() {
    let err = Kvdb::mount(Wg16Flash, cfg());
    assert!(
        matches!(err, Err(Error::UnsupportedWriteGran)),
        "unsupported write granularity must be rejected"
    );
    let err = Tsdb::mount(Wg16Flash, ts_cfg(), MAX_LEN);
    assert!(matches!(err, Err(Error::UnsupportedWriteGran)));

    let err = Kvdb::mount(BadErasedFlash, cfg());
    assert!(
        matches!(err, Err(Error::InvalidConfig)),
        "a non-0xFF erased value must be rejected"
    );
}

// ---------------------------------------------------------------------------
// KVDB placement API and entry accessors
// ---------------------------------------------------------------------------

#[test]
fn kv_mount_into_placement_api() {
    // Plain `mount_into` on a fresh partition: formats, then serves data.
    let mut slot = MaybeUninit::<Kvdb<Nor>>::uninit();
    Kvdb::mount_into(&mut slot, Nor::new(SEC), cfg()).unwrap();
    // SAFETY: `mount_into` succeeded, the slot is initialized.
    let mut db = unsafe { slot.assume_init() };
    db.set(b"into", b"one").unwrap();
    let flash = db.into_flash();

    // Re-mount through the placement API; existing data is preserved.
    let mut slot = MaybeUninit::<Kvdb<Nor>>::uninit();
    Kvdb::mount_into(&mut slot, flash, cfg()).unwrap();
    // SAFETY: `mount_into` succeeded.
    let mut db = unsafe { slot.assume_init() };
    let mut buf = [0u8; 8];
    assert_eq!(db.get(b"into", &mut buf).unwrap(), Some(3));
    assert_eq!(&buf[..3], b"one");

    // `mount_with_defaults_into` on a *fresh* partition writes the defaults.
    let mut slot = MaybeUninit::<Kvdb<Nor>>::uninit();
    Kvdb::mount_with_defaults_into(
        &mut slot,
        Nor::new(SEC),
        cfg(),
        &[("version", b"2.0"), ("boot", b"\x01")],
    )
    .unwrap();
    // SAFETY: `mount_with_defaults_into` succeeded.
    let mut db = unsafe { slot.assume_init() };
    assert_eq!(db.get(b"version", &mut buf).unwrap(), Some(3));
    assert_eq!(&buf[..3], b"2.0");
    assert_eq!(db.get(b"boot", &mut buf).unwrap(), Some(1));

    // Errors are reported before the slot is initialized.
    let mut slot = MaybeUninit::<Kvdb<Nor>>::uninit();
    let err = Kvdb::mount_into(&mut slot, Nor::new(SEC), Config::new(SEC, SEC));
    assert!(matches!(err, Err(Error::InvalidConfig)));
}

#[test]
fn kv_entry_accessors_and_truncated_reads() {
    let mut db = Kvdb::mount(Nor::new(SEC), cfg()).unwrap();
    db.set(b"text", b"hello").unwrap();
    // A non-UTF-8 key still works as bytes; `name_str` reports the failure.
    db.set(&[0xF0, 0x28], b"raw").unwrap();
    // An empty key is legal; `read_name_into` then skips the name read.
    db.set(b"", b"").unwrap();
    db.set(b"bin", &[0xFF, 0xFE]).unwrap();

    let entry = db.find(b"text").unwrap().expect("key exists");
    assert_eq!(entry.name(), b"text");
    assert_eq!(entry.name_str(), Some("text"));
    let mut buf = [0u8; 8];
    assert_eq!(db.read_value(&entry, &mut buf).unwrap(), 5);
    assert_eq!(&buf[..5], b"hello");
    // Truncated destination: the CRC is still verified, only the prefix copied.
    let mut small = [0u8; 2];
    assert_eq!(db.read_value(&entry, &mut small).unwrap(), 2);
    assert_eq!(&small, b"he");

    let raw = db.find(&[0xF0, 0x28]).unwrap().expect("raw key exists");
    assert_eq!(raw.name_str(), None);
    // Empty key round-trip exercises the zero-length name path.
    assert!(db.find(b"").unwrap().is_some());
    assert_eq!(db.get(b"", &mut buf).unwrap(), Some(0));

    // `get_str` returns None for non-UTF-8 values and for missing keys.
    assert!(db.get_str(b"bin", &mut buf).unwrap().is_none());
    assert!(db.get_str(b"missing", &mut buf).unwrap().is_none());
    assert!(db.find(b"missing").unwrap().is_none());
    let mut iter = db.iter();
    let first = iter.next().unwrap().unwrap();
    assert!(iter.read_value(&first, &mut buf).unwrap() > 0);

    // An oversized lookup key is "not found", not an error: `set` is the only
    // entry point that validates names.
    let long = [0xAAu8; KV_NAME_MAX + 1];
    assert!(db.find(&long).unwrap().is_none());
    assert_eq!(db.get(&long, &mut buf).unwrap(), None);
}

#[test]
fn kv_invalid_name_and_oversized_value() {
    let mut db = Kvdb::mount(Nor::new(SEC), cfg()).unwrap();
    let long = [b'k'; KV_NAME_MAX + 1];
    assert_eq!(
        db.set(&long, b"v").unwrap_err(),
        Error::InvalidName,
        "oversized key must be rejected"
    );
    // A value larger than one whole sector can never fit, even after GC.
    let huge = [0u8; SEC as usize];
    assert_eq!(
        db.set(b"huge", &huge).unwrap_err(),
        Error::SavedFull,
        "a value larger than a sector must never be accepted"
    );
    db.set_str("str", "value").unwrap();
}

#[test]
fn kv_fills_until_saved_full_and_stays_consistent() {
    // 4 x 4 KiB sectors, 200-byte values: the database fills up after a few
    // dozen records, forcing GC (sector-full status writes, GC moves) before
    // the allocation finally fails.
    let mut db = Kvdb::mount(Nor::new(SEC), cfg()).unwrap();
    let value = [0x5Au8; 200];
    let mut written = 0u32;
    while written < 10_000 {
        let mut key = [0u8; 8];
        key[..4].copy_from_slice(b"fill");
        key[4..8].copy_from_slice(&written.to_le_bytes());
        match db.set(&key, &value) {
            Ok(()) => written += 1,
            Err(Error::SavedFull) => break,
            Err(e) => panic!("unexpected error while filling: {e:?}"),
        }
    }
    assert!(written > 0, "at least one record must fit before SavedFull");

    // The database is full but readable.
    let mut buf = [0u8; 200];
    let mut key = [0u8; 8];
    key[..4].copy_from_slice(b"fill");
    key[4..8].copy_from_slice(&(written - 1).to_le_bytes());
    assert_eq!(db.get(&key, &mut buf).unwrap(), Some(200));
    assert_eq!(db.stats().unwrap().key_count, written);
    db.check().unwrap();

    // Freeing one record must make room again.
    assert!(db.remove(&key).unwrap());
    let mut key2 = [0u8; 8];
    key2[..4].copy_from_slice(b"fill");
    key2[4..8].copy_from_slice(&(written + 1).to_le_bytes());
    db.set(&key2, &value).unwrap();
    let mut flash = db.into_flash();
    flash.reset_stats();
    let mut db = Kvdb::mount(flash, cfg()).unwrap();
    assert_eq!(db.stats().unwrap().key_count, written);
    db.check().unwrap();
}

// ---------------------------------------------------------------------------
// KVDB corrupted / interrupted images
// ---------------------------------------------------------------------------

fn kv_image(records: &[(&str, &[u8])]) -> Vec<u8> {
    let mut db = Kvdb::mount(Nor::new(SEC), cfg()).unwrap();
    for (key, value) in records {
        db.set(key.as_bytes(), value).unwrap();
    }
    db.into_flash().data().to_vec()
}

/// All KV records of an image: `(addr, name)` found by scanning for the magic
/// word, also for records the iterator would not yield (e.g. DELETED ones).
fn kv_records(image: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let mut out = Vec::new();
    for off in 0..image.len() {
        // The magic word lives at `addr + KV_MAGIC_OFF`.
        if off + 4 > image.len() || image[off..off + 4] != *b"KV00" {
            continue;
        }
        if (off as u32) < KV_MAGIC_OFF {
            continue;
        }
        let addr = off as u32 - KV_MAGIC_OFF;
        let nl_off = (addr + KV_NAME_LEN_OFF) as usize;
        if nl_off >= image.len() {
            continue;
        }
        let name_len = image[nl_off] as usize;
        let name_off = (addr + KV_HDR) as usize;
        if name_len > KV_NAME_MAX || name_off + name_len > image.len() {
            continue;
        }
        out.push((addr, image[name_off..name_off + name_len].to_vec()));
    }
    out
}

fn kv_addr_of(image: &[u8], key: &[u8]) -> u32 {
    kv_records(image)
        .into_iter()
        .find(|(_, name)| name == key)
        .unwrap_or_else(|| panic!("no record for key {key:?}"))
        .0
}

/// Force a record's status table to `status` (values 1..=5).
fn set_kv_record_status(image: &mut [u8], addr: u32, status: u8) {
    let base = addr as usize;
    for i in 0..KV_STATUS_SIZE {
        image[base + i as usize] = if (i as u8) < status { 0x00 } else { 0xFF };
    }
}

/// Force a sector's store status and/or dirty status (values 1..=3).
fn set_kv_sector_status(image: &mut [u8], sec_addr: u32, store: Option<u8>, dirty: Option<u8>) {
    let base = sec_addr as usize;
    if let Some(store) = store {
        for i in 0..KV_SEC_STATUS_SIZE {
            image[base + KV_SEC_STORE_OFF as usize + i as usize] =
                if (i as u8) < store { 0x00 } else { 0xFF };
        }
    }
    if let Some(dirty) = dirty {
        for i in 0..KV_SEC_STATUS_SIZE {
            image[base + KV_SEC_DIRTY_OFF as usize + i as usize] =
                if (i as u8) < dirty { 0x00 } else { 0xFF };
        }
    }
}

fn kv_damage_record_magic(image: &mut [u8], addr: u32) {
    let base = addr as usize + KV_MAGIC_OFF as usize;
    image[base..base + 4].copy_from_slice(&[0x00, 0x00, 0x00, 0x00]);
}

#[test]
fn kv_corrupted_value_is_read_corrupted() {
    let mut image = kv_image(&[("alpha", b"hello"), ("beta", b"world")]);
    // Flip one value byte, keeping the stored CRC: the integrity check must
    // reject the value instead of returning it.
    let pos = image.windows(5).position(|w| w == b"hello").unwrap();
    image[pos] ^= 0x01;
    let mut db = Kvdb::mount(loaded(&image), cfg()).unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(
        db.get(b"alpha", &mut buf).unwrap_err(),
        Error::ReadCorrupted
    );
    assert_eq!(db.check().unwrap_err(), Error::ReadCorrupted);
}

#[test]
fn kv_damaged_record_scanner_continues_at_next_magic() {
    let mut image = kv_image(&[("aaa", b"1"), ("bbb", b"2"), ("ccc", b"3")]);
    let addr = kv_addr_of(&image, b"bbb");
    kv_damage_record_magic(&mut image, addr);

    let mut db = Kvdb::mount(loaded(&image), cfg()).unwrap();
    // The damaged record is skipped and the scan continues at the next magic
    // word, so the records after it stay reachable.
    assert!(db.find(b"aaa").unwrap().is_some());
    assert!(db.find(b"ccc").unwrap().is_some());
    assert!(db.find(b"bbb").unwrap().is_none());
    assert_eq!(db.stats().unwrap().key_count, 2);
    // `check` reports the non-erased invalid header instead of silently
    // skipping it.
    assert_eq!(db.check().unwrap_err(), Error::ReadCorrupted);

    // Damaging the *last* record makes the damaged-record scan run out of
    // candidates: the record area simply ends.
    let mut image = kv_image(&[("aaa", b"1"), ("bbb", b"2")]);
    let addr = kv_addr_of(&image, b"bbb");
    kv_damage_record_magic(&mut image, addr);
    let mut db = Kvdb::mount(loaded(&image), cfg()).unwrap();
    assert!(db.find(b"aaa").unwrap().is_some());
    assert!(db.find(b"bbb").unwrap().is_none());
    assert_eq!(db.stats().unwrap().key_count, 1);
}

#[test]
fn kv_inconsistent_record_length_is_rejected() {
    // A header whose magic is intact but whose length is shorter than the name
    // it claims must fail the length-consistency check (the `checked_sub`
    // miss), not wrap around.
    let mut image = kv_image(&[("aaa", b"1"), ("bbb", b"2")]);
    let addr = kv_addr_of(&image, b"aaa");
    let base = addr as usize + KV_LEN_OFF as usize;
    image[base..base + 4].copy_from_slice(&KV_HDR.to_le_bytes());
    let mut db = Kvdb::mount(loaded(&image), cfg()).unwrap();
    assert!(db.find(b"aaa").unwrap().is_none());
    assert!(db.find(b"bbb").unwrap().is_some());
    assert_eq!(db.stats().unwrap().key_count, 1);
}

#[test]
fn kv_recovery_quarantines_prewrite_and_keeps_predelete_data() {
    // A PRE_WRITE record is the leftover of a write that never committed: it
    // must be quarantined (ERR_HDR) and must not be served.
    let mut image = kv_image(&[("keep", b"k"), ("torn", b"t"), ("tail", b"x")]);
    let addr = kv_addr_of(&image, b"torn");
    set_kv_record_status(&mut image, addr, 1); // PRE_WRITE
    let mut db = Kvdb::mount(loaded(&image), cfg()).unwrap();
    assert!(db.find(b"torn").unwrap().is_none());
    assert!(db.find(b"keep").unwrap().is_some());
    assert!(db.find(b"tail").unwrap().is_some());
    db.check().unwrap();
    // Remounting decodes the quarantined ERR_HDR status.
    let image = db.into_flash().data().to_vec();
    let mut db = Kvdb::mount(loaded(&image), cfg()).unwrap();
    assert!(db.find(b"torn").unwrap().is_none());
    db.check().unwrap();

    // A PRE_DELETE record is the *old* generation of an update: recovery must
    // keep its data alive (move it) rather than lose it.
    let mut image = kv_image(&[("keep", b"k"), ("moved", b"payload")]);
    let addr = kv_addr_of(&image, b"moved");
    set_kv_record_status(&mut image, addr, 3); // PRE_DELETE
    let mut db = Kvdb::mount(loaded(&image), cfg()).unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(db.get(b"moved", &mut buf).unwrap(), Some(7));
    assert_eq!(&buf[..7], b"payload");
    db.check().unwrap();
}

#[test]
fn kv_recovery_drops_duplicate_predelete_generation() {
    // Two generations of the same key: force the old (DELETED) generation back
    // to PRE_DELETE. Recovery sees the PRE_DELETE record first, tries to move
    // it, finds the newer committed generation and drops the stale copy.
    let mut image = {
        let mut db = Kvdb::mount(Nor::new(SEC), cfg()).unwrap();
        db.set(b"dup", b"old").unwrap();
        db.set(b"dup", b"new").unwrap();
        db.into_flash().data().to_vec()
    };
    let mut oldest = None;
    for (addr, name) in kv_records(&image) {
        if name == b"dup" && oldest.is_none() {
            oldest = Some(addr);
        }
    }
    let addr = oldest.expect("old generation record");
    set_kv_record_status(&mut image, addr, 3); // PRE_DELETE

    let mut db = Kvdb::mount(loaded(&image), cfg()).unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(db.get(b"dup", &mut buf).unwrap(), Some(3));
    assert_eq!(&buf[..3], b"new");
    assert_eq!(db.stats().unwrap().key_count, 1);
    db.check().unwrap();
}

/// An image with deleted records in dirty sectors, sector 0 marked as
/// "GC in progress" (what a power cut in the middle of GC leaves behind).
fn kv_interrupted_gc_image() -> Vec<u8> {
    let mut db = Kvdb::mount(Nor::new(SEC), cfg()).unwrap();
    let value = [0x33u8; 200];
    let mut i = 0u32;
    while i < 200 {
        let mut key = [0u8; 8];
        key[..2].copy_from_slice(b"gc");
        key[4..8].copy_from_slice(&i.to_le_bytes());
        match db.set(&key, &value) {
            Ok(()) => i += 1,
            Err(Error::SavedFull) => break,
            Err(e) => panic!("unexpected error while filling: {e:?}"),
        }
    }
    assert!(i > 8);
    for j in (0..i).step_by(2) {
        let mut key = [0u8; 8];
        key[..2].copy_from_slice(b"gc");
        key[4..8].copy_from_slice(&j.to_le_bytes());
        db.remove(&key).unwrap();
    }
    let mut image = db.into_flash().data().to_vec();
    set_kv_sector_status(&mut image, 0, Some(2), Some(3)); // USING + GC
    image
}

#[test]
fn kv_interrupted_gc_is_finished_on_mount() {
    let image = kv_interrupted_gc_image();
    let mut db = Kvdb::mount(loaded(&image), cfg()).unwrap();
    // Surviving records are still served after the resumed GC.
    let mut found = 0u32;
    for i in 0..200u32 {
        let mut key = [0u8; 8];
        key[..2].copy_from_slice(b"gc");
        key[4..8].copy_from_slice(&i.to_le_bytes());
        let mut buf = [0u8; 200];
        if db.get(&key, &mut buf).unwrap().is_some() {
            found += 1;
        }
    }
    assert!(found > 0, "no record survived the resumed GC");
    assert_eq!(found, db.stats().unwrap().key_count);
    db.check().unwrap();
}

#[test]
fn kv_partially_damaged_headers_are_reformatted() {
    // Destroy one sector header (not all of them): mounting formats only the
    // damaged sector and keeps the records of the healthy ones.
    let mut db = Kvdb::mount(Nor::new(SEC), cfg()).unwrap();
    for i in 0..40u32 {
        let mut key = [0u8; 4];
        key.copy_from_slice(&i.to_le_bytes());
        db.set(&key, &[0x11u8; 200]).unwrap();
    }
    let mut image = db.into_flash().data().to_vec();
    let base = KV_SEC_MAGIC_OFF as usize;
    image[base..base + 4].copy_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    let mut db = Kvdb::mount(loaded(&image), cfg()).unwrap();
    assert!(db.stats().unwrap().key_count > 0);
    db.check().unwrap();
}

// ---------------------------------------------------------------------------
// KVDB fault sweeps: every `?` on a flash operation
// ---------------------------------------------------------------------------

#[test]
fn kv_mount_format_fault_sweep() {
    // A fresh partition is formatted during mount: sweep read, write and erase
    // failures through that sequence.
    let erased = vec![0xFFu8; MAX as usize];
    let injected = sweep_ops(&erased, |flash| {
        let _ = Kvdb::mount(flash, cfg())?;
        Ok(())
    });
    assert!(injected >= 4);
}

#[test]
fn kv_mount_recovery_fault_sweep() {
    let image = kv_image(&[("aaa", b"1"), ("bbb", b"2"), ("ccc", b"3")]);
    let injected = sweep_ops(&image, |flash| {
        let _ = Kvdb::mount(flash, cfg())?;
        Ok(())
    });
    assert!(injected >= 4);

    // Same, but recovery has to write: PRE_WRITE quarantine and PRE_DELETE
    // move both hit the flash during mount.
    let mut torn = kv_image(&[("aaa", b"1"), ("bbb", b"2")]);
    let addr = kv_addr_of(&torn, b"aaa");
    set_kv_record_status(&mut torn, addr, 1);
    let addr = kv_addr_of(&torn, b"bbb");
    set_kv_record_status(&mut torn, addr, 3);
    let injected = sweep_ops(&torn, |flash| {
        let _ = Kvdb::mount(flash, cfg())?;
        Ok(())
    });
    assert!(injected >= 4);
}

#[test]
fn kv_crud_fault_sweep() {
    let image = kv_image(&[("aaa", b"1"), ("bbb", b"2"), ("ccc", b"3")]);
    let body = |flash: Nor| -> Result<(), Error<SimError>> {
        let mut db = Kvdb::mount(flash, cfg())?;
        db.set(b"ddd", b"4")?;
        let mut buf = [0u8; 32];
        let _ = db.get(b"aaa", &mut buf)?;
        let _ = db.find(b"bbb")?;
        let _ = db.remove(b"ccc")?;
        db.stats()?;
        db.check()?;
        let mut iter = db.iter();
        while let Some(entry) = iter.next() {
            let entry = entry?;
            let _ = iter.read_value(&entry, &mut buf)?;
        }
        Ok(())
    };
    assert!(sweep_ops(&image, body) >= 4);
}

#[test]
fn kv_gc_fault_sweep() {
    // A nearly full image keeps the per-iteration work small while the update
    // still goes through the whole GC path (alloc scan, move, format).
    let image = {
        let mut db = Kvdb::mount(Nor::new(SEC), cfg()).unwrap();
        let value = [0x77u8; 200];
        let mut i = 0u32;
        while i < 64 {
            let mut key = [0u8; 8];
            key[..4].copy_from_slice(b"full");
            key[4..8].copy_from_slice(&i.to_le_bytes());
            match db.set(&key, &value) {
                Ok(()) => i += 1,
                Err(Error::SavedFull) => break,
                Err(e) => panic!("unexpected error while filling: {e:?}"),
            }
        }
        // Deleting makes sectors dirty, so the next update runs GC.
        for j in (0..i).step_by(3) {
            let mut key = [0u8; 8];
            key[..4].copy_from_slice(b"full");
            key[4..8].copy_from_slice(&j.to_le_bytes());
            db.remove(&key).unwrap();
        }
        db.into_flash().data().to_vec()
    };
    let body = |flash: Nor| -> Result<(), Error<SimError>> {
        let mut db = Kvdb::mount(flash, cfg())?;
        match db.set(b"full-new", &[0x22u8; 200]) {
            Ok(()) | Err(Error::SavedFull) => Ok(()),
            Err(e) => Err(e),
        }
    };
    assert!(sweep_ops(&image, body) >= 4);
}

#[test]
fn kv_iterator_fault_sweep() {
    let image = kv_image(&[("aaa", b"1"), ("bbb", b"2"), ("ccc", b"3")]);
    let body = |flash: Nor| -> Result<(), Error<SimError>> {
        let mut db = Kvdb::mount(flash, cfg())?;
        let mut iter = db.iter();
        let mut buf = [0u8; 8];
        while let Some(entry) = iter.next() {
            let entry = entry?;
            let _ = iter.read_value(&entry, &mut buf)?;
        }
        // Exhausted iterators short-circuit on the next call.
        assert!(iter.next().is_none());
        Ok(())
    };
    assert!(sweep_ops(&image, body) >= 4);

    // The same, on an image whose damaged record forces the iterator through
    // the damaged-record scanner (which performs extra flash reads).
    let mut damaged = kv_image(&[("aaa", b"1"), ("bbb", b"2"), ("ccc", b"3")]);
    let addr = kv_addr_of(&damaged, b"bbb");
    kv_damage_record_magic(&mut damaged, addr);
    let body = |flash: Nor| -> Result<(), Error<SimError>> {
        let mut db = Kvdb::mount(flash, cfg())?;
        let mut iter = db.iter();
        let mut buf = [0u8; 8];
        while let Some(entry) = iter.next() {
            let entry = entry?;
            let _ = iter.read_value(&entry, &mut buf)?;
        }
        Ok(())
    };
    assert!(sweep_ops(&damaged, body) >= 4);
}

#[test]
fn kv_full_sector_is_skipped_by_the_allocator() {
    // After a remount the RAM current sector is unknown, so the first set runs
    // the allocator's ring scan. With a FULL sector in front of a USING one,
    // the scan must skip the full sector and allocate in the using sector.
    // (A sector only reaches FULL through a power-cut-visible status change, so
    // that state is written into the crafted image.)
    let config = Config::new(SEC, MAX);
    let mut db = Kvdb::mount(Nor::new(SEC), config).unwrap();
    let value = [0x44u8; 200];
    for i in 0..20u32 {
        let mut key = [0u8; 4];
        key.copy_from_slice(&i.to_le_bytes());
        db.set(&key, &value).unwrap();
    }
    let mut image = db.into_flash().data().to_vec();
    set_kv_sector_status(&mut image, 0, Some(3), None); // FULL
    let mut db = Kvdb::mount(loaded(&image), config).unwrap();
    db.set(b"after", b"remount")
        .expect("allocation must reuse the using sector");
    let mut buf = [0u8; 16];
    assert_eq!(db.get(b"after", &mut buf).unwrap(), Some(7));
    db.check().unwrap();
}

// ---------------------------------------------------------------------------
// KVDB cache variants and cold lookup paths
// ---------------------------------------------------------------------------

#[test]
fn kv_cache_variants() {
    // CACHE = 1: a single direct-mapped slot.
    let mut db = KvdbImpl::<Nor, 1>::mount(Nor::new(SEC), cfg()).unwrap();
    db.set(b"one", b"1").unwrap();
    let mut buf = [0u8; 4];
    assert_eq!(db.get(b"one", &mut buf).unwrap(), Some(1));
    assert!(db.remove(b"one").unwrap());
    db.set(b"two", b"22").unwrap();

    // No cache at all: every lookup scans flash, invalidation is a no-op.
    let mut db = KvdbNoCache::mount(Nor::new(SEC), cfg()).unwrap();
    db.set(b"x", b"y").unwrap();
    assert!(db.remove(b"x").unwrap());
    assert!(!db.remove(b"x").unwrap());

    // CACHE = 128 (64 sets, 2 ways): exercise the set-index >= 32 LRU encoding
    // and the replacement path of a full cache set. Keys are chosen so that
    // three of them hash into the same high set.
    const CACHE: usize = 128;
    const SETS: u32 = (CACHE / 2) as u32;
    let mut chosen: Vec<[u8; 5]> = Vec::new();
    let mut target_set = None;
    for i in 0..200_000u32 {
        let mut key = [b's', 0, 0, 0, 0];
        key[1..5].copy_from_slice(&i.to_le_bytes());
        let set = fnv1a(&key) % SETS;
        if set < 32 {
            continue;
        }
        match target_set {
            None => {
                target_set = Some(set);
                chosen.push(key);
            }
            Some(s) if s == set => {
                chosen.push(key);
                if chosen.len() == 3 {
                    break;
                }
            }
            _ => {}
        }
    }
    assert_eq!(chosen.len(), 3, "failed to find colliding keys");
    let mut db = KvdbImpl::<Nor, CACHE>::mount(Nor::new(SEC), cfg()).unwrap();
    for (i, key) in chosen.iter().enumerate() {
        db.set(key, &[i as u8; 16]).unwrap();
    }
    for key in &chosen {
        let mut buf = [0u8; 16];
        assert_eq!(db.get(key, &mut buf).unwrap(), Some(16));
    }
    assert!(db.remove(&chosen[1]).unwrap());
    // Re-insert after the removal to take the replacement branch again.
    db.set(&chosen[1], &[0xEE; 16]).unwrap();
}

#[test]
fn kv_cache_name_probe_rejects_hash_collision() {
    // The name cache is keyed by the 32-bit key hash only. Find two distinct
    // keys with the same hash and prove that a cache hit on the *other* key's
    // record falls back to the full scan instead of returning the wrong value.
    let mut seen: std::collections::HashMap<u32, [u8; 4]> = std::collections::HashMap::new();
    let mut pair = None;
    for i in 0..400_000u32 {
        // Spread the counter over all 32 bits so the sampled key space is not a
        // small structured subset of the 32-bit hash space.
        let key = i.wrapping_mul(0x9E37_79B9).to_le_bytes();
        if let Some(other) = seen.insert(fnv1a(&key), key) {
            pair = Some((other, key));
            break;
        }
    }
    let (key_a, key_b) = pair.expect("no FNV-1a collision found");
    assert_ne!(key_a, key_b);
    assert_eq!(fnv1a(&key_a), fnv1a(&key_b));

    let mut db = Kvdb::mount(Nor::new(SEC), cfg()).unwrap();
    db.set(&key_a, b"first").unwrap();
    db.set(&key_b, b"second").unwrap();
    // The cache now points the shared hash at key_b; looking up key_a must not
    // serve key_b's value.
    let mut buf = [0u8; 8];
    assert_eq!(db.get(&key_a, &mut buf).unwrap(), Some(5));
    assert_eq!(&buf[..5], b"first");
    assert_eq!(db.get(&key_b, &mut buf).unwrap(), Some(6));
    assert_eq!(&buf[..6], b"second");
    assert!(db.find(&key_a).unwrap().is_some());
    assert!(db.find(&key_b).unwrap().is_some());
}

#[test]
fn kv_damaged_record_found_by_erased_area_scan() {
    // A damaged header in the middle of a USING sector: when an allocation
    // traverses the sector, the bookkeeping stops at the damage and the
    // trailing programmed bytes are found by the erased-area scan.
    let mut image = kv_image(&[("aaa", b"1"), ("bbb", b"2"), ("ccc", b"3")]);
    let addr = kv_addr_of(&image, b"bbb");
    kv_damage_record_magic(&mut image, addr);
    let mut db = Kvdb::mount(loaded(&image), cfg()).unwrap();
    db.set(b"ddd", b"4").unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(db.get(b"ddd", &mut buf).unwrap(), Some(1));
    assert!(db.find(b"ccc").unwrap().is_some());
    db.check().unwrap_err(); // the damage is still reported by check
}

#[test]
fn kv_write_granule_remainder_reads() {
    // A 32-bit write granularity pads the 5-byte value to 8 bytes on flash;
    // `get` reads the padding back to verify the CRC. Sweeping a fault through
    // that read path covers the padding read's error branch.
    const SEC32: u32 = 2048;
    const MAX32: u32 = 8192;
    let config = Config::new(SEC32, MAX32);
    type Nor32 = RamFlash<8192, 32>;
    let mut db = Kvdb::mount(Nor32::new(SEC32), config).unwrap();
    db.set(b"pad", &[1, 2, 3, 4, 5]).unwrap();
    db.set(b"aligned", &[9; 8]).unwrap();
    let image = db.into_flash().data().to_vec();
    let body = |flash: Nor32| -> Result<(), Error<SimError>> {
        let mut db = Kvdb::mount(flash, config)?;
        let mut buf = [0u8; 8];
        assert_eq!(db.get(b"pad", &mut buf)?.unwrap(), 5);
        assert_eq!(&buf[..5], &[1, 2, 3, 4, 5]);
        assert_eq!(db.get(b"aligned", &mut buf)?.unwrap(), 8);
        Ok(())
    };
    assert!(sweep_ramflash::<8192, 32, _>(SEC32, &image, body) >= 4);
}

// ---------------------------------------------------------------------------
// TSDB helpers
// ---------------------------------------------------------------------------

fn ts_payload(i: i32) -> [u8; 32] {
    let mut data = [0u8; 32];
    for (j, b) in data.iter_mut().enumerate() {
        *b = ((i as u32 * 13).wrapping_add(j as u32 * 5) & 0xFF) as u8;
    }
    data
}

fn ts_image(records: i32) -> Vec<u8> {
    let mut db = Tsdb::mount(Nor::new(SEC), ts_cfg(), MAX_LEN).unwrap();
    for i in 1..=records {
        db.append(1000 + i, &ts_payload(i)).unwrap();
    }
    db.into_flash().data().to_vec()
}

fn ts_set_sector_status(image: &mut [u8], sec_addr: u32, status: u8) {
    let base = sec_addr as usize + TS_SEC_STORE_OFF as usize;
    for i in 0..TS_SEC_STATUS_SIZE {
        image[base + i as usize] = if (i as u8) < status { 0x00 } else { 0xFF };
    }
}

// ---------------------------------------------------------------------------
// TSDB: configuration, big write granularities, 64-bit timestamps
// ---------------------------------------------------------------------------

#[test]
fn ts_append_rejects_bad_input() {
    let mut db = Tsdb::mount(Nor::new(SEC), ts_cfg(), 32).unwrap();
    let oversized = [0u8; 33];
    assert_eq!(
        db.append(1, &oversized).unwrap_err(),
        Error::InvalidConfig,
        "payload above max_len must be rejected"
    );
    // Non-increasing timestamps are rejected, including the initial 0.
    assert_eq!(db.append(0, b"x").unwrap_err(), Error::InvalidConfig);
    db.append(1, b"x").unwrap();
    assert_eq!(db.append(1, b"y").unwrap_err(), Error::InvalidConfig);
    assert_eq!(db.append(0, b"z").unwrap_err(), Error::InvalidConfig);
    // `append_now` derives the timestamp from last_time + 1.
    db.append_now(b"now").unwrap();
    assert_eq!(db.last_time(), 2);
    // An empty payload is legal and has no readable blob.
    db.append_now(b"").unwrap();
    let mut iter = db.iter();
    while let Some(entry) = iter.next() {
        let entry = entry.unwrap();
        if entry.log_len == 0 {
            assert_eq!(iter.read_blob(&entry, &mut [0u8; 4]).unwrap(), 0);
        }
    }
}

#[test]
fn ts_64bit_append_now() {
    let mut db: Tsdb64<Nor> = Tsdb64::mount(Nor::new(SEC), ts_cfg(), MAX_LEN).unwrap();
    db.append_now(&ts_payload(1)).unwrap();
    db.append_now(&ts_payload(2)).unwrap();
    assert_eq!(db.last_time(), 2);
    assert_eq!(db.iter().count(), 2);

    // 64-bit timestamps above the 32-bit range round-trip at a larger write
    // granularity too (the index/header buffers take the wg <= 64 branch).
    let mut db: Tsdb64<RamFlash<16384, 64>> =
        Tsdb64::mount(RamFlash::new(SEC), ts_cfg(), MAX_LEN).unwrap();
    let base = 0x1_0000_0000i64;
    for i in 0..40i64 {
        db.append(base + i, &ts_payload(i as i32)).unwrap();
    }
    let times: Vec<i64> = db.iter().map(|e| e.unwrap().time).collect();
    assert_eq!(times.first(), Some(&base));
    assert_eq!(times.len(), 40);
    let flash = db.into_flash();
    let mut db: Tsdb64<RamFlash<16384, 64>> = Tsdb64::mount(flash, ts_cfg(), MAX_LEN).unwrap();
    assert_eq!(db.last_time(), base + 39);
    assert_eq!(db.iter().count(), 40);
}

#[test]
fn ts_write_granularities_64_128_256() {
    // The large write granularities use the same layout tables but different
    // status/padding paths (including the padded magic block and index tail).
    fn check<const WG: u32>() {
        let mut db: Tsdb<RamFlash<16384, WG>> =
            Tsdb::mount(RamFlash::new(SEC), ts_cfg(), MAX_LEN).unwrap();
        for i in 1..=24i32 {
            db.append(1000 + i, &ts_payload(i)).unwrap();
        }
        let mut seen = 0;
        let mut buf = [0u8; 64];
        let mut iter = db.iter();
        while let Some(entry) = iter.next() {
            let entry = entry.unwrap();
            assert_eq!(iter.read_blob(&entry, &mut buf).unwrap(), 32);
            seen += 1;
        }
        assert_eq!(seen, 24);
        let flash = db.into_flash();
        let mut db: Tsdb<RamFlash<16384, WG>> = Tsdb::mount(flash, ts_cfg(), MAX_LEN).unwrap();
        assert_eq!(db.last_time(), 1024);
        assert_eq!(db.iter().count(), 24);
        let reverse: Vec<i32> = db.iter_reverse().map(|e| e.unwrap().time).collect();
        assert_eq!(reverse.len(), 24);
        assert_eq!(reverse[0], 1024);
    }
    check::<64>();
    check::<128>();
    check::<256>();
}

#[test]
fn ts_iterator_boundaries_and_done_states() {
    let mut db = Tsdb::mount(Nor::new(SEC), ts_cfg(), MAX_LEN).unwrap();
    for i in 1..=5i32 {
        db.append(1000 + i, &ts_payload(i)).unwrap();
    }
    // Exhausting an iterator and calling it again hits the `done` fast paths.
    let mut forward = db.iter();
    while forward.next().is_some() {}
    assert!(forward.next().is_none());

    let mut reverse = db.iter_reverse();
    while reverse.next().is_some() {}
    assert!(reverse.next().is_none());

    let mut by_time = db.iter_by_time(1001, 1003);
    while by_time.next().is_some() {}
    assert!(by_time.next().is_none());

    // Empty range, out-of-range range and reverse range (from > to) are legal.
    assert_eq!(db.iter_by_time(2000, 3000).count(), 0);
    assert_eq!(db.iter_by_time(0, 500).count(), 0);
    assert_eq!(db.iter_by_time(1003, 1001).count(), 3);
    assert_eq!(db.query_count(1000, 1100, TslStatus::Write).unwrap(), 5);
    assert_eq!(db.query_count(1001, 1002, TslStatus::Write).unwrap(), 2);
    assert!(db.max_blob_count() > 0);

    // Positional status changes are visible to the next scan. Status encodings
    // are monotonic (NOR can only clear bits), so move forward only.
    let entry = db.iter().next().unwrap().unwrap();
    db.set_status(&entry, TslStatus::UserStatus1).unwrap();
    assert_eq!(
        db.query_count(1000, 1100, TslStatus::UserStatus1).unwrap(),
        1
    );
    db.set_status(&entry, TslStatus::Deleted).unwrap();
    assert_eq!(db.query_count(1000, 1100, TslStatus::Deleted).unwrap(), 1);
    db.set_status(&entry, TslStatus::UserStatus2).unwrap();
    assert_eq!(
        db.query_count(1000, 1100, TslStatus::UserStatus2).unwrap(),
        1
    );
}

// ---------------------------------------------------------------------------
// TSDB corrupted headers and forged sector states
// ---------------------------------------------------------------------------

#[test]
fn ts_duplicate_using_sectors_are_reformatted() {
    let mut image = ts_image(3);
    // Two sectors claim to be USING: that is a damaged database, so mounting
    // must reformat it (and stay usable).
    ts_set_sector_status(&mut image, 0, 2); // USING
    ts_set_sector_status(&mut image, SEC, 2); // USING as well
    let mut db = Tsdb::mount(loaded(&image), ts_cfg(), MAX_LEN).unwrap();
    assert_eq!(db.last_time(), 0);
    assert_eq!(db.iter().count(), 0);
    db.append(1, b"after-format").unwrap();
    assert_eq!(db.iter().count(), 1);
}

#[test]
fn ts_forged_unused_sector_states_mount() {
    // All sector headers have a valid magic but an erased (UNUSED) store
    // status: no current sector can be chosen and the fallback last sector is
    // used. The mount must succeed (defensive path, not a format).
    let mut unused = vec![0xFFu8; MAX as usize];
    for sec in 0..(MAX / SEC) {
        let base = (sec * SEC) as usize + TS_SEC_MAGIC_OFF as usize;
        unused[base..base + 4].copy_from_slice(&TS_MAGIC);
    }
    let db = Tsdb::mount(loaded(&unused), ts_cfg(), MAX_LEN).unwrap();
    assert_eq!(db.last_time(), 0);
    assert!(db.rollover());
}

#[test]
fn ts_forged_full_sectors_mount() {
    // Every sector FULL with zeroed end timestamp: the "all full, no usable
    // end info" fallback picks the last physical sector, and appending reports
    // SavedFull instead of wrapping.
    let mut full = vec![0xFFu8; MAX as usize];
    for sec in 0..(MAX / SEC) {
        let base = (sec * SEC) as usize;
        for i in 0..TS_SEC_STATUS_SIZE as usize {
            full[base + i] = 0x00; // store status = FULL
        }
        full[base + TS_SEC_MAGIC_OFF as usize..base + TS_SEC_MAGIC_OFF as usize + 4]
            .copy_from_slice(&TS_MAGIC);
        full[base + TS_SEC_START_TIME_OFF as usize..base + TS_SEC_START_TIME_OFF as usize + 4]
            .copy_from_slice(&0u32.to_le_bytes());
        // end_info[0] = WRITE, timestamp 0, index at the first slot
        for i in 0..TS_IDX_STAT_SIZE as usize {
            full[base + TS_SEC_END0_STAT_OFF as usize + i] = if i < 2 { 0x00 } else { 0xFF };
        }
        full[base + TS_SEC_END0_TIME_OFF as usize..base + TS_SEC_END0_TIME_OFF as usize + 4]
            .copy_from_slice(&0u32.to_le_bytes());
        full[base + TS_SEC_END0_IDX_OFF as usize..base + TS_SEC_END0_IDX_OFF as usize + 4]
            .copy_from_slice(&TS_SEC_HDR.to_le_bytes());
    }
    let mut db = Tsdb::mount(loaded(&full), ts_cfg(), MAX_LEN).unwrap();
    assert_eq!(db.last_time(), 0);
    db.set_rollover(false);
    assert_eq!(db.append(1, b"x").unwrap_err(), Error::SavedFull);
    // Cleaning recovers the database.
    db.clean().unwrap();
    db.append(1, b"x").unwrap();
    assert_eq!(db.iter().count(), 1);
}

#[test]
fn ts_damaged_sector_header_is_reformatted() {
    let mut image = ts_image(3);
    // Sector 1 never received data; damage sector 0's magic word and check the
    // database is reformatted and usable.
    let base = TS_SEC_MAGIC_OFF as usize;
    image[base..base + 4].copy_from_slice(&[0x00; 4]);
    let mut db = Tsdb::mount(loaded(&image), ts_cfg(), MAX_LEN).unwrap();
    db.append(1, b"recovered").unwrap();
    let mut iter = db.iter();
    let entry = iter.next().unwrap().unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(iter.read_blob(&entry, &mut buf).unwrap(), 9);
    assert_eq!(&buf[..9], b"recovered");
}

#[test]
fn ts_all_sectors_full_without_rollover() {
    // Fill every sector with rollover disabled: the append that does not fit
    // must return SavedFull rather than corrupting the ring.
    let mut db = Tsdb::mount(Nor::new(SEC), Config::new(SEC, 8192), 128).unwrap();
    db.set_rollover(false);
    let mut appended = 0i32;
    while appended < 10_000 {
        match db.append(1 + appended, &[0x42u8; 128]) {
            Ok(()) => appended += 1,
            Err(Error::SavedFull) => break,
            Err(e) => panic!("unexpected append error: {e:?}"),
        }
    }
    assert!(appended > 0, "TSDB never reported SavedFull");
    assert_eq!(db.iter().count(), appended as usize);
    assert_eq!(db.last_time(), appended);
}

// ---------------------------------------------------------------------------
// TSDB fault sweeps
// ---------------------------------------------------------------------------

#[test]
fn ts_mount_format_fault_sweep() {
    let erased = vec![0xFFu8; MAX as usize];
    let injected = sweep_ops(&erased, |flash| {
        let _ = Tsdb::mount(flash, ts_cfg(), MAX_LEN)?;
        Ok(())
    });
    assert!(injected >= 4);
}

#[test]
fn ts_mount_recovery_fault_sweep() {
    let image = ts_image(20);
    let injected = sweep_ops(&image, |flash| {
        let _ = Tsdb::mount(flash, ts_cfg(), MAX_LEN)?;
        Ok(())
    });
    assert!(injected >= 4);
}

#[test]
fn ts_append_and_iterate_fault_sweep() {
    let image = ts_image(12);
    let body = |flash: Nor| -> Result<(), Error<SimError>> {
        let mut db = Tsdb::mount(flash, ts_cfg(), MAX_LEN)?;
        db.append(2000, &ts_payload(99))?;
        let mut buf = [0u8; 64];
        let mut forward = db.iter();
        while let Some(entry) = forward.next() {
            let entry = entry?;
            let _ = forward.read_blob(&entry, &mut buf)?;
        }
        let mut reverse = db.iter_reverse();
        while let Some(entry) = reverse.next() {
            let entry = entry?;
            let _ = reverse.read_blob(&entry, &mut buf)?;
        }
        let mut by_time = db.iter_by_time(1005, 1010);
        while let Some(entry) = by_time.next() {
            let entry = entry?;
            let _ = by_time.read_blob(&entry, &mut buf)?;
        }
        let _ = db.query_count(1000, 2010, TslStatus::Write)?;
        if let Some(Ok(entry)) = db.iter().next() {
            db.set_status(&entry, TslStatus::Deleted)?;
        }
        Ok(())
    };
    assert!(sweep_ops(&image, body) >= 4);
}

#[test]
fn ts_sector_seal_fault_sweep() {
    // An image whose current sector is almost full: one append seals it (end
    // info write, status transition, next-sector read/format), so the sweep
    // covers every flash call of `prepare_sector`.
    let image = {
        let mut db = Tsdb::mount(Nor::new(SEC), ts_cfg(), MAX_LEN).unwrap();
        for i in 1..=14i32 {
            db.append(1000 + i, &[0x55u8; MAX_LEN as usize]).unwrap();
        }
        db.into_flash().data().to_vec()
    };
    let body = |flash: Nor| -> Result<(), Error<SimError>> {
        let mut db = Tsdb::mount(flash, ts_cfg(), MAX_LEN)?;
        let next = db.last_time() + 1;
        match db.append(next, &[0x66u8; MAX_LEN as usize]) {
            Ok(()) | Err(Error::SavedFull) => Ok(()),
            Err(e) => Err(e),
        }
    };
    assert!(sweep_ops(&image, body) >= 4);
}

#[test]
fn ts_clean_fault_sweep() {
    let image = ts_image(6);
    let body = |flash: Nor| -> Result<(), Error<SimError>> {
        let mut db = Tsdb::mount(flash, ts_cfg(), MAX_LEN)?;
        db.clean()?;
        db.append(1, b"post-clean")?;
        Ok(())
    };
    assert!(sweep_ops(&image, body) >= 4);
}

#[test]
fn ts_full_sector_without_end_info_is_scanned() {
    // Pack a sector completely with empty payloads (only the 20-byte index
    // records), seal it, then erase its end info as if the end-info write had
    // been interrupted. Mounting must re-scan the FULL sector, and the scan
    // stops when the index cursor reaches the payload cursor.
    let config = Config::new(SEC, 8192);
    let mut db: Tsdb<Nor> = Tsdb::mount(Nor::new(SEC), config, 0).unwrap();
    for i in 1..=203i32 {
        db.append(i, b"").unwrap();
    }
    let mut image = db.into_flash().data().to_vec();
    for b in image
        [TS_SEC_END0_STAT_OFF as usize..TS_SEC_END0_STAT_OFF as usize + TS_IDX_STAT_SIZE as usize]
        .iter_mut()
    {
        *b = 0xFF;
    }
    let mut db: Tsdb<Nor> = Tsdb::mount(loaded(&image), config, 0).unwrap();
    assert_eq!(db.last_time(), 203);
    let written = db
        .iter()
        .filter(|e| matches!(e, Ok(entry) if entry.status == TslStatus::Write))
        .count();
    assert_eq!(written, 203);
}

#[test]
fn ts_out_of_range_index_slots_are_skipped() {
    // Forge a FULL sector whose end index points past the last index slot of
    // the sector: the iterators must skip the out-of-range slots (their bounds
    // check) instead of reading outside the index area.
    let mut image = ts_image(3);
    ts_set_sector_status(&mut image, 0, 3); // FULL
    for i in 0..TS_IDX_STAT_SIZE as usize {
        image[TS_SEC_END0_STAT_OFF as usize + i] = if i < 2 { 0x00 } else { 0xFF };
    }
    image[TS_SEC_END0_TIME_OFF as usize..TS_SEC_END0_TIME_OFF as usize + 4]
        .copy_from_slice(&1003i32.to_le_bytes());
    image[TS_SEC_END0_IDX_OFF as usize..TS_SEC_END0_IDX_OFF as usize + 4]
        .copy_from_slice(&5000u32.to_le_bytes());
    let mut db = Tsdb::mount(loaded(&image), ts_cfg(), MAX_LEN).unwrap();
    // Drain past the out-of-range slots in every iterator direction/mode.
    assert!(db.iter().take(400).count() > 0);
    let mut reverse = db.iter_reverse();
    let _ = reverse.next();
    drop(reverse);
    let mut by_time = db.iter_by_time(1001, 1002);
    let _ = by_time.next();
    drop(by_time);
    // The forged end index also drives the by-time binary search out of range;
    // the scan must terminate cleanly rather than read outside the sector.
    let _ = db.query_count(1000, 1100, TslStatus::Write).unwrap();
}
