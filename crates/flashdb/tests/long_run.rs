//! Long-running soak and large-data stress tests.
//!
//! These tests are **`#[ignore]`d by default**: they run tens to hundreds of
//! thousands of flash operations and use partitions up to 1 MiB, which is too
//! heavy for the default `cargo test` run (the normal suite finishes in a few
//! seconds). They are the long-run complement to the correctness tests:
//! power-cut recovery, C parity and rollover edge cases stay in the fast suite.
//!
//! Run them explicitly (release mode is recommended):
//!
//! ```text
//! cargo test -p flashdb --release --test long_run -- --ignored --nocapture
//! ```
//!
//! Covered:
//!
//! * `kv_long_run_gc_soak` — 512 KiB KVDB, 2 000 keys, 100 000 updates with
//!   continuous GC/erase churn; verifies the live key set and a deterministic
//!   value checksum before and after a remount, and that the simulator really
//!   erased sectors.
//! * `ts_many_ring_wraps_soak` — 500 full ring wraps (154 000 appends) on a
//!   16 KiB TSDB; verifies the contiguous live window, payload checksums,
//!   erase count, and the same window after a remount.
//! * `large_partition_and_values` — 256 KiB KVDB with 4 KiB values and a 1 MiB
//!   TSDB with 4 KiB payloads (write / GC / wraparound); verifies every value
//!   before and after a remount.

use flashdb::{Config, Kvdb, Tsdb, TslStatus};
use flashdb_sim::RamFlash;

/// Run a stack-heavy test body on a thread with a large stack: a `RamFlash`
/// with a 1 MiB region lives inline in the type.
fn with_big_stack<F: FnOnce() + Send + 'static>(body: F) {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(body)
        .expect("spawn long-run test thread")
        .join()
        .expect("long-run test thread panicked");
}

fn key(i: u32) -> Vec<u8> {
    format!("key.{i:05}").into_bytes()
}

fn fill(buf: &mut [u8], seed: u32) {
    for (j, b) in buf.iter_mut().enumerate() {
        *b = seed
            .wrapping_mul(31)
            .wrapping_add(j as u32 * 17)
            .wrapping_add(0x5A) as u8;
    }
}

// ---------------------------------------------------------------------------
// KVDB: sustained update/GC churn on a 512 KiB partition
// ---------------------------------------------------------------------------

type KvSoakNor = RamFlash<524288, 8>;

const KV_SOAK_SEC: u32 = 4096;
const KV_SOAK_MAX: u32 = 512 * 1024;
const KV_SOAK_KEYS: u32 = 2_000;
const KV_SOAK_UPDATES: u32 = 100_000;
const KV_SOAK_VALUE_LEN: usize = 64;

/// Deterministic-order checksum over every key's stored value.
fn kv_checksum(db: &mut Kvdb<KvSoakNor>) -> u32 {
    let mut h = 0x811C_9DC5u32;
    let mut buf = [0u8; KV_SOAK_VALUE_LEN];
    for i in 0..KV_SOAK_KEYS {
        let k = key(i);
        let n = db
            .get(&k, &mut buf)
            .expect("get during checksum")
            .expect("key missing during checksum");
        assert_eq!(n, KV_SOAK_VALUE_LEN, "value length changed for {}", i);
        for &b in &buf[..n] {
            h ^= b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    h
}

#[test]
#[ignore = "long-running soak: 100k KV updates / GC churn on a 512 KiB partition"]
fn kv_long_run_gc_soak() {
    with_big_stack(|| {
        let flash = KvSoakNor::new(KV_SOAK_SEC);
        let mut db = Kvdb::mount(flash, Config::new(KV_SOAK_SEC, KV_SOAK_MAX)).unwrap();

        // Fill the database.
        let mut value = [0u8; KV_SOAK_VALUE_LEN];
        for i in 0..KV_SOAK_KEYS {
            fill(&mut value, i);
            db.set(&key(i), &value)
                .unwrap_or_else(|e| panic!("set key {i} failed: {e:?}"));
        }
        assert_eq!(db.stats().unwrap().key_count, KV_SOAK_KEYS);

        // Sustained updates: every write is a new record, so the database
        // continuously fills sectors and has to erase/GC them.
        for n in 0..KV_SOAK_UPDATES {
            let i = n % KV_SOAK_KEYS;
            fill(&mut value, n.wrapping_add(i));
            db.set(&key(i), &value)
                .unwrap_or_else(|e| panic!("set key {i} failed: {e:?}"));
        }

        db.check().expect("database inconsistent after soak");
        let stats = db.stats().unwrap();
        assert_eq!(stats.key_count, KV_SOAK_KEYS);
        assert!(stats.used_bytes > 0);

        let before = kv_checksum(&mut db);
        let mut iterated = 0u32;
        {
            let mut it = db.iter();
            while let Some(entry) = it.next() {
                let _ = entry.expect("iteration error after soak").name();
                iterated += 1;
            }
        }
        assert_eq!(iterated, KV_SOAK_KEYS, "iterator lost keys");

        // A soak that never GCs/erases proves nothing: assert real churn.
        let flash = db.into_flash();
        let erases = flash.stats().erase_ops;
        assert!(
            erases > 500,
            "expected sustained GC erase churn, saw {erases} erases"
        );

        // Remount from the same flash image and verify the data is identical.
        let mut db = Kvdb::mount(flash, Config::new(KV_SOAK_SEC, KV_SOAK_MAX)).unwrap();
        assert_eq!(db.stats().unwrap().key_count, KV_SOAK_KEYS);
        db.check().expect("database inconsistent after remount");
        assert_eq!(
            kv_checksum(&mut db),
            before,
            "checksum changed after remount"
        );
    });
}

// ---------------------------------------------------------------------------
// TSDB: 500 ring wraps
// ---------------------------------------------------------------------------

type TsSoakNor = RamFlash<16384, 8>;

const TS_SOAK_SEC: u32 = 4096;
const TS_SOAK_MAX: u32 = 16 * 1024;
const TS_SOAK_MAX_LEN: u32 = 256;
const TS_SOAK_PAYLOAD: usize = 32;
/// (4096 - sector header 44) / (index 20 + payload 32), the same layout
/// constant the existing rollover tests use.
const TS_PER_SECTOR: u32 = 77;
const TS_RING: u32 = 4 * TS_PER_SECTOR;
const TS_WRAPS: u32 = 500;

fn ts_payload(i: u32) -> [u8; TS_SOAK_PAYLOAD] {
    let mut p = [0u8; TS_SOAK_PAYLOAD];
    fill(&mut p, i);
    p
}

/// Live-window checksum over (timestamp, payload) in iteration order.
fn ts_window_checksum(db: &mut Tsdb<TsSoakNor>) -> (u32, u32, i32, i32) {
    let mut h = 0x811C_9DC5u32;
    let mut count = 0u32;
    let mut first = i32::MAX;
    let mut last = i32::MIN;
    let mut buf = [0u8; TS_SOAK_PAYLOAD];
    let mut it = db.iter();
    while let Some(entry) = it.next() {
        let entry = entry.expect("iteration error");
        let n = it.read_blob(&entry, &mut buf).expect("blob read error");
        assert_eq!(n, TS_SOAK_PAYLOAD, "payload length changed");
        if count == 0 {
            first = entry.time;
        }
        last = entry.time;
        for &b in &buf[..n] {
            h ^= b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
        h = h.wrapping_mul(0x0100_0193) ^ entry.time as u32;
        count += 1;
    }
    (h, count, first, last)
}

#[test]
#[ignore = "long-running soak: 500 full TSDB ring wraps (154k appends)"]
fn ts_many_ring_wraps_soak() {
    let appends = TS_WRAPS * TS_RING;
    let mut db = Tsdb::mount(
        TsSoakNor::new(TS_SOAK_SEC),
        Config::new(TS_SOAK_SEC, TS_SOAK_MAX),
        TS_SOAK_MAX_LEN,
    )
    .unwrap();
    assert!(db.rollover(), "TSDB should default to rollover on");

    for i in 0..appends {
        // Timestamps start at 1000 and are strictly increasing.
        db.append(1000 + i as i32, &ts_payload(i)).unwrap();
    }

    assert_eq!(
        db.query_count(0, i32::MAX, TslStatus::Write).unwrap(),
        TS_RING as usize,
        "live window must hold exactly one ring"
    );
    let (checksum, count, first, last) = ts_window_checksum(&mut db);
    assert_eq!(count, TS_RING);
    assert_eq!(first, 1000 + (appends - TS_RING) as i32);
    assert_eq!(last, 1000 + appends as i32 - 1);
    assert_eq!(db.last_time(), 1000 + appends as i32 - 1);

    // Every wrap erases the four sectors of the ring at least once.
    let flash = db.into_flash();
    let erases = flash.stats().erase_ops;
    assert!(
        erases >= TS_WRAPS as u64 * 4,
        "expected >= {} erases for {TS_WRAPS} wraps, saw {erases}",
        TS_WRAPS as u64 * 4
    );

    // Remount: the live window must come back identically.
    let mut db = Tsdb::mount(
        flash,
        Config::new(TS_SOAK_SEC, TS_SOAK_MAX),
        TS_SOAK_MAX_LEN,
    )
    .unwrap();
    assert_eq!(
        db.query_count(0, i32::MAX, TslStatus::Write).unwrap(),
        TS_RING as usize
    );
    let after = ts_window_checksum(&mut db);
    assert_eq!(
        after,
        (checksum, count, first, last),
        "window changed after remount"
    );
}

// ---------------------------------------------------------------------------
// Large partitions and large values
// ---------------------------------------------------------------------------

type KvBigNor = RamFlash<262144, 8>; // 256 KiB = 64 sectors
type TsBigNor = RamFlash<1048576, 8>; // 1 MiB = 64 x 16 KiB sectors

const KV_BIG_SEC: u32 = 4096;
const KV_BIG_MAX: u32 = 256 * 1024;
const KV_BIG_KEYS: u32 = 40;
// Record = header + aligned key + value; a 4 KiB sector keeps one record, and
// the sector remain-threshold must still be satisfied (see `Kvdb::set`).
const KV_BIG_VALUE_LEN: usize = 3800;
const KV_BIG_ROUNDS: u32 = 8;

const TS_BIG_SEC: u32 = 16 * 1024;
const TS_BIG_MAX: u32 = 1024 * 1024;
const TS_BIG_MAX_LEN: u32 = 4096;
const TS_BIG_VALUE_LEN: usize = 4000;
/// (16384 - 44) / (20 + 4000) = 4 entries per sector, 64 sectors.
const TS_BIG_ENTRIES: u32 = 4 * 64;
const TS_BIG_APPENDS: u32 = 400; // > one full ring, forces a wrap

fn kv_big_checksum(db: &mut Kvdb<KvBigNor>) -> u32 {
    let mut h = 0x811C_9DC5u32;
    let mut buf = vec![0u8; KV_BIG_VALUE_LEN];
    for i in 0..KV_BIG_KEYS {
        let n = db
            .get(&key(i), &mut buf)
            .expect("get during checksum")
            .expect("key missing during checksum");
        assert_eq!(n, KV_BIG_VALUE_LEN);
        for &b in &buf[..n] {
            h ^= b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    h
}

#[test]
#[ignore = "large-data: 256 KiB KVDB with 4 KiB values + 1 MiB TSDB with 4 KiB payloads"]
fn large_partition_and_values() {
    with_big_stack(|| {
        // -- KVDB: 256 KiB partition, 4 KiB values, several GC rounds ------
        let mut db = Kvdb::mount(
            KvBigNor::new(KV_BIG_SEC),
            Config::new(KV_BIG_SEC, KV_BIG_MAX),
        )
        .unwrap();
        let mut value = vec![0u8; KV_BIG_VALUE_LEN];
        for i in 0..KV_BIG_KEYS {
            fill(&mut value, i);
            db.set(&key(i), &value)
                .unwrap_or_else(|e| panic!("set key {i} failed: {e:?}"));
        }
        // Each full rewrite of the value set touches ~200 KiB, so several
        // rounds force sector format/GC on the 256 KiB partition.
        for round in 0..KV_BIG_ROUNDS {
            for i in 0..KV_BIG_KEYS {
                fill(&mut value, round.wrapping_mul(KV_BIG_KEYS).wrapping_add(i));
                db.set(&key(i), &value)
                    .unwrap_or_else(|e| panic!("set key {i} failed: {e:?}"));
            }
            db.check().expect("large-value database inconsistent");
        }
        let before = kv_big_checksum(&mut db);
        let flash = db.into_flash();
        let erases = flash.stats().erase_ops;
        assert!(erases > 64, "expected GC erases on 256 KiB, saw {erases}");
        let mut db = Kvdb::mount(flash, Config::new(KV_BIG_SEC, KV_BIG_MAX)).unwrap();
        assert_eq!(db.stats().unwrap().key_count, KV_BIG_KEYS);
        assert_eq!(
            kv_big_checksum(&mut db),
            before,
            "KV checksum changed after remount"
        );
        drop(db);

        // -- TSDB: 1 MiB partition, 4 KiB payloads, wrap-around ------------
        let mut db = Tsdb::mount(
            TsBigNor::new(TS_BIG_SEC),
            Config::new(TS_BIG_SEC, TS_BIG_MAX),
            TS_BIG_MAX_LEN,
        )
        .unwrap();
        let mut payload = vec![0u8; TS_BIG_VALUE_LEN];
        for i in 0..TS_BIG_APPENDS {
            fill(&mut payload, i);
            db.append(1000 + i as i32, &payload)
                .unwrap_or_else(|e| panic!("append {i} failed: {e:?}"));
        }
        // 400 appends > 256-entry ring: the oldest 144 entries are gone.
        assert_eq!(
            db.query_count(0, i32::MAX, TslStatus::Write).unwrap(),
            TS_BIG_ENTRIES as usize
        );
        let expected_first = 1000 + (TS_BIG_APPENDS - TS_BIG_ENTRIES) as i32;

        let mut checksum = 0x811C_9DC5u32;
        let mut seen = 0u32;
        let mut buf = vec![0u8; TS_BIG_VALUE_LEN];
        {
            let mut it = db.iter();
            while let Some(entry) = it.next() {
                let entry = entry.expect("large-payload iteration error");
                if seen == 0 {
                    assert_eq!(entry.time, expected_first);
                }
                let n = it
                    .read_blob(&entry, &mut buf)
                    .expect("large blob read error");
                assert_eq!(n, TS_BIG_VALUE_LEN);
                let mut expected = vec![0u8; TS_BIG_VALUE_LEN];
                fill(&mut expected, (entry.time - 1000) as u32);
                assert_eq!(buf, expected, "payload mismatch at t={}", entry.time);
                for &b in &buf[..n] {
                    checksum ^= b as u32;
                    checksum = checksum.wrapping_mul(0x0100_0193);
                }
                seen += 1;
            }
        }
        assert_eq!(seen, TS_BIG_ENTRIES);

        let flash = db.into_flash();
        let mut db =
            Tsdb::mount(flash, Config::new(TS_BIG_SEC, TS_BIG_MAX), TS_BIG_MAX_LEN).unwrap();
        assert_eq!(
            db.query_count(0, i32::MAX, TslStatus::Write).unwrap(),
            TS_BIG_ENTRIES as usize
        );
        let mut seen_after = 0u32;
        let mut it = db.iter();
        while let Some(entry) = it.next() {
            let entry = entry.expect("iteration error after remount");
            it.read_blob(&entry, &mut buf)
                .expect("blob read error after remount");
            seen_after += 1;
        }
        assert_eq!(seen_after, TS_BIG_ENTRIES);
    });
}
