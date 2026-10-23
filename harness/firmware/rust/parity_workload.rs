//! Deterministic workload shared by the real-flash parity firmware
//! (`flash_parity.rs`, on STM32U5 internal flash) and the host model
//! (`crates/flashdb-bench/src/bin/parity_model.rs`, on `flashdb-sim`).
//!
//! This file is compiled **verbatim into both programs** (the firmware declares
//! `mod parity_workload;`, the host model includes it with `#[path = ...]`), so
//! the workload, its data generation and its checksums cannot drift between the
//! board and the host-side reference that produces the expected values.
//!
//! # Geometry (must match the firmware's scratch partition)
//!
//! * one 4 x 8 KiB KVDB partition and one 4 x 8 KiB TSDB partition
//!   (`WRITE_GRAN_BITS = 128`, the STM32U5 quad-word programming granularity);
//! * KV: 40 keys `k00..k39`, 32-byte values; TS: 120 entries, 32-byte payloads,
//!   rollover disabled, timestamps 1000..1119.
//!
//! # Workload (exact, identical on both sides)
//!
//! KV (partition starts erased):
//!   1. for i in 0..40:          set(key(i), value(0x55, i, 32))
//!   2. for i in 0..20:          set(key(i), value(0x56, i, 32))
//!   3. for i in 0..10:          remove(key(i))
//!   4. for i in 0..10:          set(key(i), value(0x57, i, 32))
//!   5. for k in 0..180:         set("churn", value(0x5C, k, 32))  (forces GC)
//!   Final live set: the 40 keys plus the "churn" key = 41 entries; expected
//!   value seed(i) = 0x57 for i < 10, 0x56 for 10 <= i < 20, 0x55 otherwise.
//!
//! TS (partition starts erased, rollover disabled):
//!   for i in 0..120:            append(t = 1000 + i, payload(i)), 32 bytes,
//!                               `payload[j] = (0x5A + i*13 + j*5) & 0xFF`
//!
//! # Verification and fingerprints
//!
//! Both sides remount both databases from flash, read every value / payload
//! back and compare against the generator above, and iterate both databases.
//! Then they report:
//!
//! * `kv_checksum` / `ts_checksum`: FNV-1a-32 over, respectively, the 40 live
//!   (key, value) pairs in key-index order plus the final "churn" key/value,
//!   and the 120 payloads in timestamp order;
//! * `kv_image_fnv` / `ts_image_fnv`: FNV-1a-32 over the raw 32768 bytes of
//!   each partition. These are the fields that prove the real STM32U5 flash
//!   image is byte-identical to the host simulator's image.
//!
//! FNV-1a-32 parameters: offset basis 0x811C_9DC5, prime 0x0100_0193.

use flashdb::{Config, Error, Flash, Kvdb, Tsdb};

/// Erase block ("sector") size of the database partitions: one STM32U5 page.
pub const SEC_SIZE: u32 = 8 * 1024;
/// Sector count per partition (4 x 8 KiB).
pub const SECTORS: u32 = 4;
/// Size of one database partition.
pub const PART_SIZE: u32 = SEC_SIZE * SECTORS;

/// Number of KV keys and of TS entries.
pub const KV_KEYS: u32 = 40;
pub const KV_UPDATES: u32 = 20;
pub const KV_DELETES: u32 = 10;
pub const KV_CHURN: u32 = 180;
/// KV value length (byte) and TS payload length (byte) used by the workload.
pub const KV_VALUE_LEN: usize = 32;
pub const TS_PAYLOAD_LEN: usize = 32;
/// Maximum TSDB payload length accepted by the database (>= payload length).
pub const TS_MAX_LEN: u32 = 64;
/// First TS timestamp; entry `i` gets `TS_START_TIME + i`.
pub const TS_START_TIME: i32 = 1000;
pub const TS_APPENDS: u32 = 120;

/// FNV-1a-32 parameters.
pub const FNV_OFFSET: u32 = 0x811C_9DC5;
pub const FNV_PRIME: u32 = 0x0100_0193;

/// Workload stages, reported as the `stage=` reason when something fails.
/// Keep in sync with [`stage_tag`].
pub const STAGE_KV_MOUNT: u8 = 1;
pub const STAGE_KV_FORMAT: u8 = 2;
pub const STAGE_KV_SET: u8 = 3;
pub const STAGE_KV_REMOVE: u8 = 4;
pub const STAGE_KV_REMOUNT: u8 = 5;
pub const STAGE_KV_CHECK: u8 = 6;
pub const STAGE_KV_GET: u8 = 7;
pub const STAGE_KV_ITER: u8 = 8;
pub const STAGE_KV_IMAGE: u8 = 9;
pub const STAGE_TS_MOUNT: u8 = 10;
pub const STAGE_TS_CLEAN: u8 = 11;
pub const STAGE_TS_APPEND: u8 = 12;
pub const STAGE_TS_REMOUNT: u8 = 13;
pub const STAGE_TS_ITER: u8 = 14;
pub const STAGE_TS_BLOB: u8 = 15;
pub const STAGE_TS_QUERY: u8 = 16;
pub const STAGE_TS_IMAGE: u8 = 17;

/// NUL-terminated stage tag for the firmware's `fail(stage, code)` output.
pub fn stage_tag(stage: u8) -> &'static [u8] {
    match stage {
        STAGE_KV_MOUNT => b"kv_mount\0",
        STAGE_KV_FORMAT => b"kv_format\0",
        STAGE_KV_SET => b"kv_set\0",
        STAGE_KV_REMOVE => b"kv_remove\0",
        STAGE_KV_REMOUNT => b"kv_remount\0",
        STAGE_KV_CHECK => b"kv_check\0",
        STAGE_KV_GET => b"kv_get\0",
        STAGE_KV_ITER => b"kv_iter\0",
        STAGE_KV_IMAGE => b"kv_image\0",
        STAGE_TS_MOUNT => b"ts_mount\0",
        STAGE_TS_CLEAN => b"ts_clean\0",
        STAGE_TS_APPEND => b"ts_append\0",
        STAGE_TS_REMOUNT => b"ts_remount\0",
        STAGE_TS_ITER => b"ts_iter\0",
        STAGE_TS_BLOB => b"ts_blob\0",
        STAGE_TS_QUERY => b"ts_query\0",
        STAGE_TS_IMAGE => b"ts_image\0",
        _ => b"parity\0",
    }
}

/// Failure reason: a database operation failure (carrying the driver error) or
/// a verification mismatch at `index` (key index / TS entry index / count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParityError<E> {
    /// A flashdb operation failed at `stage`.
    Db { stage: u8, err: Error<E> },
    /// The verified data did not match the deterministic expectation.
    Verify { stage: u8, index: u32 },
}

impl<E> ParityError<E> {
    /// Stage constant ([`STAGE_KV_MOUNT`] ... [`STAGE_TS_IMAGE`]).
    pub fn stage(&self) -> u8 {
        match self {
            ParityError::Db { stage, .. } | ParityError::Verify { stage, .. } => *stage,
        }
    }
}

/// Result of the whole workload: the values the host model prints as expected
/// and the firmware prints as actual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Live KV entries found by iteration after the remount.
    pub kv_entries: u32,
    /// FNV-1a-32 over the live (key, value) pairs in key-index order.
    pub kv_checksum: u32,
    /// TS entries found by forward iteration after the remount.
    pub ts_entries: u32,
    /// FNV-1a-32 over the payloads in timestamp order.
    pub ts_checksum: u32,
    /// FNV-1a-32 over the raw 32768-byte KV partition image.
    pub kv_image_fnv: u32,
    /// FNV-1a-32 over the raw 32768-byte TS partition image.
    pub ts_image_fnv: u32,
}

/// FNV-1a-32 of `data`, chained onto `h` (pass [`FNV_OFFSET`] for a new hash).
pub fn fnv1a(mut h: u32, data: &[u8]) -> u32 {
    let mut i = 0;
    while i < data.len() {
        h ^= data[i] as u32;
        h = h.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    h
}

/// 3-byte KV key `k00..k39` (same encoding as `flash_longrun.rs`).
pub fn kv_key(i: u32) -> [u8; 3] {
    [b'k', b'0' + ((i / 10) % 10) as u8, b'0' + (i % 10) as u8]
}

/// Deterministic `N`-byte value for `(seed, i)`:
/// `v[j] = (seed + i*31 + j*7) & 0xFF`.
pub fn kv_value<const N: usize>(seed: u8, i: u32) -> [u8; N] {
    let mut v = [0u8; N];
    let mut j = 0;
    while j < N {
        v[j] = (seed as u32)
            .wrapping_add(i.wrapping_mul(31))
            .wrapping_add((j as u32).wrapping_mul(7)) as u8;
        j += 1;
    }
    v
}

/// Expected live value of key `i` after the whole KV workload.
pub fn expected_kv_value(i: u32) -> [u8; KV_VALUE_LEN] {
    if i < KV_DELETES {
        kv_value(0x57, i)
    } else if i < KV_UPDATES {
        kv_value(0x56, i)
    } else {
        kv_value(0x55, i)
    }
}

/// Deterministic 32-byte TS payload for entry `i`:
/// `p[j] = (0x5A + i*13 + j*5) & 0xFF`.
pub fn ts_payload(i: u32) -> [u8; TS_PAYLOAD_LEN] {
    let mut p = [0u8; TS_PAYLOAD_LEN];
    let mut j = 0;
    while j < TS_PAYLOAD_LEN {
        p[j] = (0x5Au32
            .wrapping_add(i.wrapping_mul(13))
            .wrapping_add((j as u32).wrapping_mul(5))
            & 0xFF) as u8;
        j += 1;
    }
    p
}

/// FNV-1a-32 over a whole database partition (raw on-flash image).
fn image_fnv<F: Flash>(flash: &mut F) -> core::result::Result<u32, Error<F::Error>> {
    let mut h = FNV_OFFSET;
    let mut buf = [0u8; 256];
    let mut off = 0u32;
    while off < PART_SIZE {
        let mut n = (PART_SIZE - off) as usize;
        if n > buf.len() {
            n = buf.len();
        }
        flash.read(off, &mut buf[..n]).map_err(Error::FlashRead)?;
        h = fnv1a(h, &buf[..n]);
        off += n as u32;
    }
    Ok(h)
}

fn db_err<E>(stage: u8, err: Error<E>) -> ParityError<E> {
    ParityError::Db { stage, err }
}

fn verify<E>(stage: u8, index: u32) -> ParityError<E> {
    ParityError::Verify { stage, index }
}

/// Run the deterministic workload on two database partitions.
///
/// Both partitions must start in the factory-erased state (the firmware erases
/// the scratch pages first; the host model's simulator is born erased). The
/// function formats/cleans both databases itself, so the resulting flash image
/// is independent of what the partitions contained before.
pub fn run<F: Flash>(
    kv_flash: F,
    ts_flash: F,
) -> core::result::Result<Outcome, ParityError<F::Error>> {
    let cfg = Config::new(SEC_SIZE, PART_SIZE);

    // ---- KV phase ---------------------------------------------------------
    let mut kv = Kvdb::mount(kv_flash, cfg).map_err(|e| db_err(STAGE_KV_MOUNT, e))?;
    kv.format().map_err(|e| db_err(STAGE_KV_FORMAT, e))?;

    let mut i = 0u32;
    while i < KV_KEYS {
        kv.set(&kv_key(i), &kv_value::<KV_VALUE_LEN>(0x55, i))
            .map_err(|e| db_err(STAGE_KV_SET, e))?;
        i += 1;
    }
    i = 0;
    while i < KV_UPDATES {
        kv.set(&kv_key(i), &kv_value::<KV_VALUE_LEN>(0x56, i))
            .map_err(|e| db_err(STAGE_KV_SET, e))?;
        i += 1;
    }
    i = 0;
    while i < KV_DELETES {
        kv.remove(&kv_key(i))
            .map_err(|e| db_err(STAGE_KV_REMOVE, e))?;
        i += 1;
    }
    i = 0;
    while i < KV_DELETES {
        kv.set(&kv_key(i), &kv_value::<KV_VALUE_LEN>(0x57, i))
            .map_err(|e| db_err(STAGE_KV_SET, e))?;
        i += 1;
    }
    let mut k = 0u32;
    while k < KV_CHURN {
        kv.set(b"churn", &kv_value::<KV_VALUE_LEN>(0x5C, k))
            .map_err(|e| db_err(STAGE_KV_SET, e))?;
        k += 1;
    }

    // Remount from flash and verify every live key, then iterate the database.
    let mut kv = Kvdb::mount(kv.into_flash(), cfg).map_err(|e| db_err(STAGE_KV_REMOUNT, e))?;
    kv.check().map_err(|e| db_err(STAGE_KV_CHECK, e))?;
    let mut kv_checksum = FNV_OFFSET;
    let mut value = [0u8; KV_VALUE_LEN];
    i = 0;
    while i < KV_KEYS {
        let expected = expected_kv_value(i);
        match kv.get(&kv_key(i), &mut value) {
            Ok(Some(n)) if n == KV_VALUE_LEN && value == expected => {}
            _ => return Err(verify(STAGE_KV_GET, i)),
        }
        kv_checksum = fnv1a(kv_checksum, &kv_key(i));
        kv_checksum = fnv1a(kv_checksum, &value);
        i += 1;
    }
    let mut kv_entries = 0u32;
    let mut it = kv.iter();
    while let Some(entry) = it.next() {
        entry.map_err(|e| db_err(STAGE_KV_ITER, e))?;
        kv_entries += 1;
    }
    // 40 workload keys plus the "churn" key that forced GC.
    if kv_entries != KV_KEYS + 1 {
        return Err(verify(STAGE_KV_ITER, kv_entries));
    }
    let churn = kv_value::<KV_VALUE_LEN>(0x5C, KV_CHURN - 1);
    match kv.get(b"churn", &mut value) {
        Ok(Some(n)) if n == KV_VALUE_LEN && value == churn => {}
        _ => return Err(verify(STAGE_KV_GET, KV_KEYS)),
    }
    kv_checksum = fnv1a(kv_checksum, b"churn");
    kv_checksum = fnv1a(kv_checksum, &churn);
    let mut kv_flash = kv.into_flash();
    let kv_image_fnv = image_fnv(&mut kv_flash).map_err(|e| db_err(STAGE_KV_IMAGE, e))?;

    // ---- TS phase ---------------------------------------------------------
    let mut ts = Tsdb::mount(ts_flash, cfg, TS_MAX_LEN).map_err(|e| db_err(STAGE_TS_MOUNT, e))?;
    ts.clean().map_err(|e| db_err(STAGE_TS_CLEAN, e))?;
    ts.set_rollover(false);
    i = 0;
    while i < TS_APPENDS {
        ts.append(TS_START_TIME + i as i32, &ts_payload(i))
            .map_err(|e| db_err(STAGE_TS_APPEND, e))?;
        i += 1;
    }

    // Remount, verify the whole live window and checksum the payloads in order.
    let mut ts =
        Tsdb::mount(ts.into_flash(), cfg, TS_MAX_LEN).map_err(|e| db_err(STAGE_TS_REMOUNT, e))?;
    let mut ts_checksum = FNV_OFFSET;
    let mut ts_entries = 0u32;
    let mut it = ts.iter();
    while let Some(entry) = it.next() {
        let entry = entry.map_err(|e| db_err(STAGE_TS_ITER, e))?;
        let expected_time = TS_START_TIME + ts_entries as i32;
        if entry.time != expected_time || entry.log_len as usize != TS_PAYLOAD_LEN {
            return Err(verify(STAGE_TS_ITER, ts_entries));
        }
        let n = it
            .read_blob(&entry, &mut value[..TS_PAYLOAD_LEN])
            .map_err(|e| db_err(STAGE_TS_BLOB, e))?;
        if n != TS_PAYLOAD_LEN || value[..TS_PAYLOAD_LEN] != ts_payload(ts_entries) {
            return Err(verify(STAGE_TS_BLOB, ts_entries));
        }
        ts_checksum = fnv1a(ts_checksum, &value[..TS_PAYLOAD_LEN]);
        ts_entries += 1;
    }
    if ts_entries != TS_APPENDS {
        return Err(verify(STAGE_TS_ITER, ts_entries));
    }
    let count = ts
        .query_count(
            TS_START_TIME,
            TS_START_TIME + TS_APPENDS as i32 - 1,
            flashdb::TslStatus::Write,
        )
        .map_err(|e| db_err(STAGE_TS_QUERY, e))?;
    if count as u32 != TS_APPENDS {
        return Err(verify(STAGE_TS_QUERY, count as u32));
    }
    let mut ts_flash = ts.into_flash();
    let ts_image_fnv = image_fnv(&mut ts_flash).map_err(|e| db_err(STAGE_TS_IMAGE, e))?;

    Ok(Outcome {
        kv_entries,
        kv_checksum,
        ts_entries,
        ts_checksum,
        kv_image_fnv,
        ts_image_fnv,
    })
}
