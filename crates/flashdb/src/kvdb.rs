//! Key-value database (KVDB).
//!
//! KVDB stores named blobs in an append-only log spread over a ring of flash
//! sectors. Updating a key appends a new record and marks the old one deleted;
//! sectors are reclaimed by garbage collection once they hold enough garbage.
//! The on-flash format is byte-compatible with FlashDB-C.
//!
//! ## API shape
//!
//! Keys and values are byte slices; `&str` conveniences are provided. Reads
//! copy into a caller-provided buffer (no allocation, no hidden static buffer
//! like the C `fdb_kv_get()`), and iteration returns lightweight [`KvEntry`]
//! values:
//!
//! ```no_run
//! # use flashdb::{Config, Flash, Kvdb};
//! # fn demo<F: Flash>(flash: F) -> Result<(), flashdb::Error<F::Error>> {
//! let mut db = Kvdb::mount_with_defaults(flash, Config::new(4096, 16384), &[("version", b"1.0")])?;
//! db.set(b"boot_count", &42u32.to_le_bytes())?;
//!
//! let mut buf = [0u8; 16];
//! if let Some(n) = db.get(b"boot_count", &mut buf)? {
//!     let _count = u32::from_le_bytes(buf[..n].try_into().unwrap());
//! }
//!
//! let mut it = db.iter();
//! while let Some(entry) = it.next() {
//!     let entry = entry?;
//!     let _key = entry.name();
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Design notes vs. the C implementation
//!
//! * **O(1) name cache.** The C build keeps a 64-entry table (8 bytes each)
//!   that is scanned linearly on every lookup, and reads 64 bytes of key name
//!   from flash on every hit. This crate keeps the same 8 bytes per entry but
//!   hashes to a single slot: one comparison instead of 64, and no key-name
//!   read on a miss-path. Cache entries store a 32-bit hash (instead of a
//!   16-bit CRC) and the name is verified from flash before the entry is used.
//! * **No sector cache.** The C build reserves a second table of up to
//!   `8 * sizeof(kvdb_sec_info)` bytes (≈320 B) to remember sector free space.
//!   Here the currently active sector is tracked in the DB struct, so normal
//!   appends never re-traverse a sector; the table is not needed.
//! * **No gratuitous value reads.** A lookup only reads the record header and
//!   the key name; CRC verification happens when the value is actually read,
//!   and `get` computes the CRC *while* copying into the caller buffer instead
//!   of reading the value twice (C reads the whole value in `read_kv()` and
//!   again in `get_kv()`).
//! * **`Result` everywhere.** No `FDB_ASSERT` hangs, no error codes that need
//!   out-parameters, and the flash driver's own error type is preserved.

use crate::error::{Error, Result};
use crate::flash::Flash;
use crate::low;
use crate::low::{
    align_up, crc32, crc32_u32, decode_status, encode_status, get_u32, put_u32, wg_align, KvLayout,
    KvStatus, SectorDirtyStatus, SectorStoreStatus, FAILED_ADDR, KVDB_SECTOR_MAGIC, KV_MAGIC,
    KV_NAME_MAX, SECTOR_COMBINED, SECTOR_NOT_COMBINED,
};
use crate::storage::{Config, Storage};

/// Number of empty sectors that must remain before GC starts collecting.
const GC_EMPTY_SEC_THRESHOLD: u32 = 1;

/// Number of status entries in a KV node status table.
const KV_STATUS_NUM: usize = 6;
/// Number of status entries in a sector status table.
const SEC_STATUS_NUM: usize = 4;

// ---------------------------------------------------------------------------
// Low-level header encode/decode
// ---------------------------------------------------------------------------

/// Raw sector header fields.
struct RawSectorHeader {
    store: SectorStoreStatus,
    dirty: SectorDirtyStatus,
    magic: u32,
    combined: u32,
}

/// Raw KV header fields.
struct RawKvHeader {
    status: KvStatus,
    magic: u32,
    len: u32,
    crc: u32,
    name_len: u8,
    value_len: u32,
    /// True when the whole header is erased flash (0xFF), i.e. the end of the
    /// committed records in this sector.
    erased: bool,
}

#[inline]
fn decode_sector_header(buf: &[u8], lay: &KvLayout, wg: u32) -> RawSectorHeader {
    let so = lay.sec_store_off as usize;
    let ss = lay.store_status_size as usize;
    let doff = lay.sec_dirty_off as usize;
    let ds = lay.dirty_status_size as usize;
    RawSectorHeader {
        store: SectorStoreStatus::from_index(decode_status(wg, SEC_STATUS_NUM, &buf[so..so + ss])),
        dirty: SectorDirtyStatus::from_index(decode_status(
            wg,
            SEC_STATUS_NUM,
            &buf[doff..doff + ds],
        )),
        magic: get_u32(buf, lay.sec_magic_off as usize),
        combined: get_u32(buf, lay.sec_magic_off as usize + 4),
    }
}

/// Out-of-line wrapper around [`Storage::write_status`].
///
/// `write_status` is small enough that the optimiser inlines it into every
/// caller in this module (the status encoding plus the flash write is then
/// duplicated ~11 times); keeping a single copy behind a call site is
/// measurably smaller on the size profile, and status writes are not a
/// per-byte hot loop. `write_status` is common to the KV and TS paths, so the
/// wrapper lives here rather than in the storage layer.
#[inline(never)]
fn write_status<F: Flash>(
    storage: &mut Storage<F>,
    addr: u32,
    status_num: usize,
    status: usize,
) -> Result<(), F::Error> {
    storage.write_status(addr, status_num, status)
}

#[inline]
fn read_raw_sector_header<F: Flash>(
    storage: &mut Storage<F>,
    addr: u32,
    lay: &KvLayout,
) -> Result<RawSectorHeader, F::Error> {
    let wg = F::WRITE_GRAN_BITS;
    // Every branch reads exactly `sec_raw_size` bytes into an array of that
    // size, so no bounds-checked sub-slice of the buffer is needed.
    if wg == 1 {
        let mut b = [0u8; 16];
        storage.read(addr, &mut b)?;
        Ok(decode_sector_header(&b, lay, wg))
    } else if wg <= 8 {
        let mut b = [0u8; 20];
        storage.read(addr, &mut b)?;
        Ok(decode_sector_header(&b, lay, wg))
    } else if wg <= 32 {
        let mut b = [0u8; 36];
        storage.read(addr, &mut b)?;
        Ok(decode_sector_header(&b, lay, wg))
    } else if wg <= 64 {
        let mut b = [0u8; 64];
        storage.read(addr, &mut b)?;
        Ok(decode_sector_header(&b, lay, wg))
    } else if wg <= 128 {
        let mut b = [0u8; 112];
        storage.read(addr, &mut b)?;
        Ok(decode_sector_header(&b, lay, wg))
    } else {
        let mut b = [0u8; 224];
        storage.read(addr, &mut b)?;
        Ok(decode_sector_header(&b, lay, wg))
    }
}

#[inline]
fn fill_sector_header(
    buf: &mut [u8],
    lay: &KvLayout,
    wg: u32,
    combined: u32,
    scratch: &mut [u8; 32],
) {
    let (soff, sbytes) = encode_status(wg, SectorStoreStatus::Empty as usize, scratch);
    let slen = sbytes.len();
    let store_base = lay.sec_store_off as usize + soff;
    buf[store_base..store_base + slen].copy_from_slice(&sbytes[..slen]);
    let (doff, dbytes) = encode_status(wg, SectorDirtyStatus::False as usize, scratch);
    let dlen = dbytes.len();
    let dirty_base = lay.sec_dirty_off as usize + doff;
    buf[dirty_base..dirty_base + dlen].copy_from_slice(&dbytes[..dlen]);
    let mo = lay.sec_magic_off as usize;
    put_u32(buf, mo, KVDB_SECTOR_MAGIC);
    put_u32(buf, mo + 4, combined);
    put_u32(buf, mo + 8, FAILED_ADDR);
}

/// Write the fields of a formatted sector header (magic, combined, reserved),
/// starting at `sec_magic_off`. Returns the number of bytes written.
#[inline]
fn write_sector_header_tail<F: Flash>(
    storage: &mut Storage<F>,
    addr: u32,
    lay: &KvLayout,
    combined: u32,
) -> Result<(), F::Error> {
    let mo = lay.sec_magic_off as usize;
    let block = lay.sec_raw_size as usize - mo;
    if block <= 32 {
        let mut b = [0xFFu8; 32];
        put_u32(&mut b, 0, KVDB_SECTOR_MAGIC);
        put_u32(&mut b, 4, combined);
        put_u32(&mut b, 8, FAILED_ADDR);
        storage.write(addr + mo as u32, &b[..block])
    } else if block <= 64 {
        let mut b = [0xFFu8; 64];
        put_u32(&mut b, 0, KVDB_SECTOR_MAGIC);
        put_u32(&mut b, 4, combined);
        put_u32(&mut b, 8, FAILED_ADDR);
        storage.write(addr + mo as u32, &b[..block])
    } else {
        let mut b = [0xFFu8; 128];
        put_u32(&mut b, 0, KVDB_SECTOR_MAGIC);
        put_u32(&mut b, 4, combined);
        put_u32(&mut b, 8, FAILED_ADDR);
        storage.write(addr + mo as u32, &b[..block])
    }
}

/// Erase a sector and write a fresh sector header (EMPTY / clean).
///
/// For write granularity 1 this is the single full-struct write FlashDB-C does
/// (`FDB_WRITE_GRAN == 1` branch of `format_sector`). For larger granularities
/// C writes three separate pieces -- store status, dirty status, then the
/// magic/combined/reserved block -- to avoid re-programming chunks containing
/// already-written status bits; replicating that also keeps the flash byte
/// counters identical.
#[inline]
fn write_formatted_sector_header<F: Flash>(
    storage: &mut Storage<F>,
    addr: u32,
    lay: &KvLayout,
    combined: u32,
) -> Result<(), F::Error> {
    let wg = F::WRITE_GRAN_BITS;
    let store_status = SectorStoreStatus::Empty as usize;
    let dirty_status = SectorDirtyStatus::False as usize;
    if wg == 1 {
        let mut scratch = [0u8; 32];
        let mut b = [0xFFu8; 36];
        let raw = lay.sec_raw_size as usize;
        fill_sector_header(&mut b[..raw], lay, wg, combined, &mut scratch);
        storage.write(addr, &b[..raw])
    } else {
        write_status(
            storage,
            addr + lay.sec_store_off,
            SEC_STATUS_NUM,
            store_status,
        )?;
        write_status(
            storage,
            addr + lay.sec_dirty_off,
            SEC_STATUS_NUM,
            dirty_status,
        )?;
        write_sector_header_tail(storage, addr, lay, combined)
    }
}

#[inline]
fn decode_kv_header_raw(buf: &[u8], lay: &KvLayout, wg: u32) -> RawKvHeader {
    let ss = lay.kv_status_size as usize;
    RawKvHeader {
        status: KvStatus::from_index(decode_status(wg, KV_STATUS_NUM, &buf[..ss])),
        magic: get_u32(buf, lay.kv_magic_off as usize),
        len: get_u32(buf, lay.kv_len_off as usize),
        crc: get_u32(buf, lay.kv_crc_off as usize),
        name_len: buf[lay.kv_name_len_off as usize],
        value_len: get_u32(buf, lay.kv_value_len_off as usize),
        erased: buf.iter().all(|&b| b == 0xFF),
    }
}

#[inline]
fn read_raw_kv_header<F: Flash>(
    storage: &mut Storage<F>,
    addr: u32,
    lay: &KvLayout,
) -> Result<RawKvHeader, F::Error> {
    let wg = F::WRITE_GRAN_BITS;
    // As in `read_raw_sector_header`: read exactly `kv_raw_size` bytes into a
    // buffer of that size so no bounds-checked sub-slice is needed.
    if wg == 1 {
        let mut b = [0u8; 24];
        storage.read(addr, &mut b)?;
        Ok(decode_kv_header_raw(&b, lay, wg))
    } else if wg <= 8 {
        let mut b = [0u8; 28];
        storage.read(addr, &mut b)?;
        Ok(decode_kv_header_raw(&b, lay, wg))
    } else if wg <= 32 {
        let mut b = [0u8; 40];
        storage.read(addr, &mut b)?;
        Ok(decode_kv_header_raw(&b, lay, wg))
    } else if wg <= 64 {
        let mut b = [0u8; 64];
        storage.read(addr, &mut b)?;
        Ok(decode_kv_header_raw(&b, lay, wg))
    } else if wg <= 128 {
        let mut b = [0u8; 112];
        storage.read(addr, &mut b)?;
        Ok(decode_kv_header_raw(&b, lay, wg))
    } else {
        let mut b = [0u8; 196];
        storage.read(addr, &mut b)?;
        Ok(decode_kv_header_raw(&b, lay, wg))
    }
}

/// Fill the KV header fields that follow the status table.
///
/// All supported layouts place `magic`, `len`, `crc`, `name_len` and
/// `value_len` at the same offsets from the magic word; everything after the
/// value length stays erased (`0xFF`), matching C's
/// `memset(&kv_hdr, FDB_BYTE_ERASED, ...)`. `N` is the number of bytes between
/// the magic word and the end of the raw header struct.
#[inline]
fn fill_kv_header_tail<const N: usize>(
    b: &mut [u8; N],
    magic: u32,
    len: u32,
    crc: u32,
    name_len: u8,
    value_len: u32,
) {
    let m = magic.to_le_bytes();
    let l = len.to_le_bytes();
    let c = crc.to_le_bytes();
    let v = value_len.to_le_bytes();
    b[0] = m[0];
    b[1] = m[1];
    b[2] = m[2];
    b[3] = m[3];
    b[4] = l[0];
    b[5] = l[1];
    b[6] = l[2];
    b[7] = l[3];
    b[8] = c[0];
    b[9] = c[1];
    b[10] = c[2];
    b[11] = c[3];
    b[12] = name_len;
    b[16] = v[0];
    b[17] = v[1];
    b[18] = v[2];
    b[19] = v[3];
}

/// Write a KV node header: status marker, then fields from `magic` onwards,
/// exactly like `write_kv_hdr()` in C.
#[inline]
fn write_raw_kv_header<F: Flash>(
    storage: &mut Storage<F>,
    addr: u32,
    lay: &KvLayout,
    status: KvStatus,
    magic: u32,
    len: u32,
    crc: u32,
    name_len: u8,
    value_len: u32,
) -> Result<(), F::Error> {
    write_status(storage, addr, KV_STATUS_NUM, status as usize)?;
    let wg = F::WRITE_GRAN_BITS;
    let mo = lay.kv_magic_off as usize;
    if wg <= 32 {
        let mut b = [0xFFu8; 20];
        fill_kv_header_tail(&mut b, magic, len, crc, name_len, value_len);
        storage.write(addr + mo as u32, &b)
    } else if wg <= 64 {
        let mut b = [0xFFu8; 24];
        fill_kv_header_tail(&mut b, magic, len, crc, name_len, value_len);
        storage.write(addr + mo as u32, &b)
    } else if wg <= 128 {
        let mut b = [0xFFu8; 32];
        fill_kv_header_tail(&mut b, magic, len, crc, name_len, value_len);
        storage.write(addr + mo as u32, &b)
    } else {
        let mut b = [0xFFu8; 36];
        fill_kv_header_tail(&mut b, magic, len, crc, name_len, value_len);
        storage.write(addr + mo as u32, &b)
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Statistics about a KV database's live contents.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct KvStats {
    /// Number of live (committed, readable) keys.
    pub key_count: u32,
    /// Sum of on-flash record sizes, including headers and alignment padding.
    pub used_bytes: u32,
    /// Sum of value lengths.
    pub value_bytes: u32,
}

/// One KV record, as yielded by [`KvIterator`] or returned by [`Kvdb::find`].
///
/// The key bytes are copied into the struct; the value is read on demand with
/// [`Kvdb::read_value`] / [`KvIterator::read_value`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvEntry {
    /// Record status.
    pub status: KvStatus,
    /// Address of the record header.
    pub addr: u32,
    /// Total on-flash record length (header + aligned key + aligned value).
    pub len: u32,
    /// Value length in bytes.
    pub value_len: u32,
    /// Key length in bytes.
    pub name_len: u8,
    /// Address of the value bytes.
    pub value_addr: u32,
    /// Record CRC32 (covers key, value and their write-granule padding).
    pub crc: u32,
    name: [u8; KV_NAME_MAX],
}

impl KvEntry {
    /// Key bytes (not NUL terminated).
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }

    /// Key as UTF-8, if it is valid UTF-8.
    pub fn name_str(&self) -> Option<&str> {
        core::str::from_utf8(self.name()).ok()
    }

    /// Internal view of the record as a decoded header (value-read path).
    fn header(&self) -> KvHeader {
        KvHeader {
            addr: self.addr,
            status: self.status,
            crc: self.crc,
            len: self.len,
            value_len: self.value_len,
            name_len: self.name_len,
            valid: true,
            erased: false,
        }
    }
}

/// Iterator over the live entries of a [`Kvdb`].
///
/// Created by [`Kvdb::iter`]. Use [`KvIterator::read_value`] to copy the value
/// of the entry that was just yielded.
pub struct KvIterator<'a, F: Flash, const CACHE: usize> {
    db: &'a mut KvdbImpl<F, CACHE>,
    sector_addr: u32,
    traversed_len: u32,
    next_addr: u32,
    /// Exclusive end address of the sector currently being iterated.
    sec_end: u32,
    in_sector: bool,
    done: bool,
}

/// Per-sector bookkeeping (FlashDB `kvdb_sec_info`).
#[derive(Debug, Clone, Copy)]
struct SecInfo {
    addr: u32,
    check_ok: bool,
    store: SectorStoreStatus,
    dirty: SectorDirtyStatus,
    combined: u32,
    remain: u32,
    empty_kv: u32,
}

impl SecInfo {
    const UNKNOWN: Self = Self {
        addr: FAILED_ADDR,
        check_ok: false,
        store: SectorStoreStatus::Unused,
        dirty: SectorDirtyStatus::Unused,
        combined: SECTOR_NOT_COMBINED,
        remain: 0,
        empty_kv: FAILED_ADDR,
    };
}

/// KV node header in a convenient decoded form.
#[derive(Debug, Clone, Copy)]
struct KvHeader {
    addr: u32,
    status: KvStatus,
    crc: u32,
    len: u32,
    value_len: u32,
    name_len: u8,
    valid: bool,
    erased: bool,
}

impl KvHeader {
    /// Placeholder for caller-provided out-params (`read_kv_header_into`).
    const EMPTY: Self = Self {
        addr: FAILED_ADDR,
        status: KvStatus::Unused,
        crc: 0,
        len: 0,
        value_len: 0,
        name_len: 0,
        valid: false,
        erased: false,
    };
}

#[derive(Debug, Clone, Copy)]
struct CacheSlot {
    hash: u32,
    addr: u32,
}

impl CacheSlot {
    const EMPTY: Self = Self {
        hash: 0,
        addr: FAILED_ADDR,
    };
}

// ---------------------------------------------------------------------------
// Database implementation
// ---------------------------------------------------------------------------

/// A key-value database over a flash partition.
///
/// The `CACHE` parameter is the number of direct-mapped KV name-cache slots;
/// 0 disables the cache (minimum RAM, slower lookups), 64 matches FlashDB's
/// default cache table size with half the per-entry footprint and O(1) probing.
///
/// Most users should use the [`Kvdb`] alias (64-entry cache) or
/// [`KvdbNoCache`] for the leanest RAM footprint.
pub struct KvdbImpl<F: Flash, const CACHE: usize> {
    storage: Storage<F>,
    cur_sec: SecInfo,
    cache: [CacheSlot; CACHE],
    /// One MRU bit per cache set (bit = which of the two ways is most
    /// recently used). Sets above 32 fall back to replacing way 0.
    cache_lru: u32,
    gc_request: bool,
    in_recovery: bool,
    /// Set while a record is pre-deleted as part of an update; suppresses the
    /// cache invalidation on the completing delete so the freshly inserted
    /// cache entry for the new record survives (FlashDB's
    /// `last_is_complete_del`).
    last_is_complete_del: bool,
}

/// A KV database with FlashDB's default 64-entry name cache
/// (512 bytes of RAM, the same as FlashDB's cache table).
pub type Kvdb<F> = KvdbImpl<F, 64>;

/// A KV database without a name cache: minimum RAM (no cache table), every
/// lookup scans flash headers.
pub type KvdbNoCache<F> = KvdbImpl<F, 0>;

/// A KV database with a small 16-entry name cache (128 bytes of RAM), useful
/// for tiny MCUs with a handful of hot keys.
pub type KvdbSmallCache<F> = KvdbImpl<F, 16>;

/// FNV-1a offset basis, shared by [`hash_key`] and the streamed name hasher.
const FNV_OFFSET: u32 = 0x811C_9DC5;

#[inline]
fn hash_bytes_into(mut hash: u32, bytes: &[u8]) -> u32 {
    // FNV-1a: cheap and good enough for a direct-mapped name cache.
    for &b in bytes {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[inline]
fn hash_key(key: &[u8]) -> u32 {
    hash_bytes_into(FNV_OFFSET, key)
}

#[inline]
fn kv_len(lay: &KvLayout, wg: u32, name_len: usize, value_len: usize) -> u32 {
    lay.kv_hdr_size + wg_align(name_len, wg) as u32 + wg_align(value_len, wg) as u32
}

/// CRC prefix for `name_len`.
///
/// C computes `fdb_calc_crc32(0, &kv_hdr.name_len, sizeof(uint32_t))`: that
/// hashes the `u8` plus the three following struct-padding bytes, which the
/// `memset(&kv_hdr, FDB_BYTE_ERASED, ...)` left at 0xFF. Hashing `name_len` as
/// a `u32` (zero padding) makes every KV CRC disagree with C.
#[inline]
fn crc_name_len(crc: u32, name_len: u8) -> u32 {
    crc32(crc, &[name_len, 0xFF, 0xFF, 0xFF])
}

#[inline]
fn crc_pad(mut crc: u32, pad: usize) -> u32 {
    const PAD: [u8; 32] = [0xFF; 32];
    if pad > 0 {
        crc = crc32(crc, &PAD[..pad]);
    }
    crc
}

impl<F: Flash, const CACHE: usize> KvdbImpl<F, CACHE> {
    // -- construction -------------------------------------------------------

    /// Mount (open) a KV database, checking and recovering the on-flash state.
    ///
    /// If the whole partition is unformatted (or all sector headers are
    /// damaged), the database is formatted and mounted empty.
    ///
    /// For a database stored in a `static` (the usual embedded pattern on
    /// RAM-constrained MCUs), use [`KvdbImpl::mount_into`] instead: it avoids
    /// the stack copy of the database struct.
    pub fn mount(flash: F, config: Config) -> Result<Self, F::Error> {
        Self::mount_with_defaults(flash, config, &[])
    }

    /// Mount (open) a KV database and write `defaults` when the database has to
    /// be formatted (matching FlashDB's `fdb_kvdb_init` + `default_kv`).
    pub fn mount_with_defaults(
        flash: F,
        config: Config,
        defaults: &[(&str, &[u8])],
    ) -> Result<Self, F::Error> {
        let storage = Storage::new(flash, config)?;
        let mut db = Self {
            storage,
            cur_sec: SecInfo::UNKNOWN,
            cache: [CacheSlot::EMPTY; CACHE],
            cache_lru: 0,
            gc_request: false,
            in_recovery: false,
            last_is_complete_del: false,
        };
        db.load(defaults)?;
        Ok(db)
    }

    /// Initialize a database **in place** in caller-provided storage.
    ///
    /// This is the embedded-friendly form of [`KvdbImpl::mount`]: instead of
    /// constructing the (larger, cache-carrying) database on the stack and
    /// returning it by value, the fields are written directly into the slot.
    /// A static database therefore costs no stack copy:
    ///
    /// ```no_run
    /// use flashdb::{Config, Flash, Kvdb};
    /// use core::mem::MaybeUninit;
    ///
    /// struct MyFlash;
    /// impl Flash for MyFlash {
    ///     type Error = core::convert::Infallible;
    ///     fn read(&mut self, _: u32, _: &mut [u8]) -> Result<(), Self::Error> { Ok(()) }
    ///     fn write(&mut self, _: u32, _: &[u8]) -> Result<(), Self::Error> { Ok(()) }
    ///     fn erase(&mut self, _: u32, _: u32) -> Result<(), Self::Error> { Ok(()) }
    /// }
    ///
    /// static mut DB: MaybeUninit<Kvdb<MyFlash>> = MaybeUninit::uninit();
    ///
    /// fn boot(flash: MyFlash) -> Result<(), flashdb::Error<core::convert::Infallible>> {
    ///     // SAFETY: single-threaded initialization before the database is used.
    ///     unsafe {
    ///         Kvdb::mount_into(
    ///             &mut *core::ptr::addr_of_mut!(DB),
    ///             flash,
    ///             Config::new(4096, 16384),
    ///         )
    ///     }
    /// }
    /// ```
    ///
    /// `defaults` are written when the partition has to be formatted, exactly
    /// like [`KvdbImpl::mount_with_defaults`].
    #[allow(unsafe_code)]
    pub fn mount_with_defaults_into(
        slot: &mut core::mem::MaybeUninit<Self>,
        flash: F,
        config: Config,
        defaults: &[(&str, &[u8])],
    ) -> Result<(), F::Error> {
        let storage = Storage::new(flash, config)?;
        // SAFETY: all fields of `Self` are initialized exactly once below.
        // `MaybeUninit` guarantees the memory is valid for `Self` and has the
        // right alignment; `addr_of_mut!` does not create a reference to the
        // (not yet initialized) value, so no invalid reference is formed.
        unsafe {
            let p = slot.as_mut_ptr();
            core::ptr::addr_of_mut!((*p).storage).write(storage);
            core::ptr::addr_of_mut!((*p).cur_sec).write(SecInfo::UNKNOWN);
            // Initialize the cache in place.  `addr_of_mut!(...).write([EMPTY; CACHE])`
            // would materialize the whole (512 B at CACHE = 64) array in a stack
            // temporary first and then copy it into place; a slot-at-a-time store
            // keeps the mount frame flat, which matters on a RAM-constrained MCU.
            let cache = core::ptr::addr_of_mut!((*p).cache).cast::<CacheSlot>();
            let mut i = 0;
            while i < CACHE {
                cache.add(i).write(CacheSlot::EMPTY);
                i += 1;
            }
            core::ptr::addr_of_mut!((*p).cache_lru).write(0);
            core::ptr::addr_of_mut!((*p).gc_request).write(false);
            core::ptr::addr_of_mut!((*p).in_recovery).write(false);
            core::ptr::addr_of_mut!((*p).last_is_complete_del).write(false);
        }
        // SAFETY: every field was initialized above.
        let db = unsafe { slot.assume_init_mut() };
        db.load(defaults)
    }

    /// Placement form of [`KvdbImpl::mount`] (no defaults).
    pub fn mount_into(
        slot: &mut core::mem::MaybeUninit<Self>,
        flash: F,
        config: Config,
    ) -> Result<(), F::Error> {
        Self::mount_with_defaults_into(slot, flash, config, &[])
    }

    /// Consume the database and return the flash driver.
    pub fn into_flash(self) -> F {
        self.storage.into_flash()
    }

    // -- small helpers ------------------------------------------------------

    #[inline(always)]
    fn lay(&self) -> KvLayout {
        match low::kv_layout(F::WRITE_GRAN_BITS) {
            Some(l) => l,
            None => unreachable!(),
        }
    }

    #[inline(always)]
    fn wg(&self) -> u32 {
        F::WRITE_GRAN_BITS
    }

    /// Number of cache sets (2-way set associative; 1 direct-mapped entry for
    /// tiny cache sizes).
    #[inline(always)]
    fn cache_sets() -> usize {
        if CACHE >= 2 {
            CACHE / 2
        } else {
            CACHE
        }
    }

    #[inline(always)]
    fn cache_ways() -> usize {
        if CACHE >= 2 {
            2
        } else if CACHE == 1 {
            1
        } else {
            0
        }
    }

    #[inline]
    fn cache_lookup(&mut self, hash: u32) -> Option<u32> {
        let sets = Self::cache_sets();
        if sets == 0 {
            return None;
        }
        let set_index = hash as usize % sets;
        let base = set_index * Self::cache_ways();
        for way in 0..Self::cache_ways() {
            let slot = self.cache[base + way];
            if slot.addr != FAILED_ADDR && slot.hash == hash {
                if sets <= 32 && set_index < 32 {
                    // Mark the hit way as most recently used.
                    self.cache_lru =
                        (self.cache_lru & !(1 << set_index)) | ((way as u32) << set_index);
                }
                return Some(slot.addr);
            }
        }
        None
    }

    #[inline]
    fn cache_insert(&mut self, hash: u32, addr: u32) {
        let sets = Self::cache_sets();
        if sets == 0 {
            return;
        }
        let set_index = hash as usize % sets;
        let base = set_index * Self::cache_ways();
        let ways = Self::cache_ways();
        let mut slot_index = base;
        // If this exact hash is already cached, just move the address (the
        // common update path).
        for way in 0..ways {
            if self.cache[base + way].hash == hash && self.cache[base + way].addr != FAILED_ADDR {
                self.cache[base + way].addr = addr;
                if sets <= 32 && set_index < 32 {
                    self.cache_lru =
                        (self.cache_lru & !(1 << set_index)) | ((way as u32) << set_index);
                }
                return;
            }
        }
        if ways >= 2 {
            let mru = if set_index < 32 {
                (self.cache_lru >> set_index) & 1
            } else {
                1
            };
            // Prefer an empty way (updating the LRU bit), otherwise replace the
            // least recently used way.
            let way1 = base + 1;
            if self.cache[base].addr == FAILED_ADDR {
                slot_index = base;
            } else if self.cache[way1].addr == FAILED_ADDR {
                slot_index = way1;
            } else {
                slot_index = if mru == 0 { way1 } else { base };
            }
            if set_index < 32 {
                let new_mru = (slot_index - base) as u32;
                self.cache_lru = (self.cache_lru & !(1 << set_index)) | (new_mru << set_index);
            }
        }
        self.cache[slot_index] = CacheSlot { hash, addr };
    }

    #[inline]
    fn cache_invalidate(&mut self, hash: u32) {
        let sets = Self::cache_sets();
        if sets == 0 {
            return;
        }
        let base = (hash as usize % sets) * Self::cache_ways();
        for way in 0..Self::cache_ways() {
            if self.cache[base + way].hash == hash {
                self.cache[base + way] = CacheSlot::EMPTY;
            }
        }
    }

    fn cache_invalidate_sector(&mut self, sector_addr: u32) {
        let end = sector_addr + self.storage.sec_size();
        for slot in self.cache.iter_mut() {
            if slot.addr != FAILED_ADDR && slot.addr >= sector_addr && slot.addr < end {
                *slot = CacheSlot::EMPTY;
            }
        }
    }

    /// Decode a KV header at `addr` into `hdr`, flagging whether it is
    /// self-consistent.
    ///
    /// An out-param keeps the (small) header in the caller's frame instead of
    /// materialising a `Result<KvHeader, _>` pair at every call site.
    fn read_kv_header_into(&mut self, addr: u32, hdr: &mut KvHeader) -> Result<(), F::Error> {
        let lay = self.lay();
        let wg = self.wg();
        let raw = read_raw_kv_header(&mut self.storage, addr, &lay)?;
        // Guard the length computation before trusting header fields: an
        // erased header (end of the record area) has value_len == 0xFFFF_FFFF.
        // The arithmetic stays in u32: `sane` bounds every field, so the
        // 64-bit sum C uses is not needed.
        let sane = raw.name_len as usize <= KV_NAME_MAX
            && raw.value_len <= self.storage.max_size()
            && raw.len != FAILED_ADDR
            && raw.len >= lay.kv_hdr_size
            && raw.len <= self.storage.max_size();
        let name_part = wg_align(raw.name_len as usize, wg) as u32;
        let value_part = wg_align(raw.value_len as usize, wg) as u32;
        let valid = sane
            && raw.magic == KV_MAGIC
            && match (raw.len - lay.kv_hdr_size).checked_sub(name_part) {
                Some(rest) => rest == value_part,
                None => false,
            };
        hdr.addr = addr;
        hdr.status = raw.status;
        hdr.crc = raw.crc;
        hdr.valid = valid;
        hdr.erased = raw.erased;
        if valid {
            hdr.len = raw.len;
            hdr.value_len = raw.value_len;
            hdr.name_len = raw.name_len;
        } else {
            hdr.len = lay.kv_hdr_size;
            hdr.value_len = 0;
            hdr.name_len = 0;
        }
        Ok(())
    }

    /// Read the key name of a record (including write-granule padding) into
    /// the caller's buffer, avoiding a temporary copy.
    fn read_name_into(
        &mut self,
        hdr: &KvHeader,
        name: &mut [u8; KV_NAME_MAX],
    ) -> Result<(), F::Error> {
        let lay = self.lay();
        let aligned = wg_align(hdr.name_len as usize, self.wg());
        if aligned > 0 {
            self.storage
                .read(hdr.addr + lay.kv_hdr_size, &mut name[..aligned])?;
        }
        Ok(())
    }

    fn entry_from_header(&self, hdr: &KvHeader) -> KvEntry {
        let lay = self.lay();
        KvEntry {
            status: hdr.status,
            addr: hdr.addr,
            len: hdr.len,
            value_len: hdr.value_len,
            name_len: hdr.name_len,
            value_addr: hdr.addr
                + lay.kv_hdr_size
                + wg_align(hdr.name_len as usize, self.wg()) as u32,
            crc: hdr.crc,
            name: [0u8; KV_NAME_MAX],
        }
    }

    // -- flash scanning primitives -----------------------------------------

    /// Find the next address that contains a KV magic word, like
    /// `find_next_kv_addr()` in C.
    ///
    /// Only reached for damaged records; kept out of line and free of
    /// helper calls (the magic-word decode is written out as byte loads).
    #[cold]
    #[inline(never)]
    fn find_next_kv_addr(&mut self, start: u32, end: u32) -> Result<u32, F::Error> {
        let lay = self.lay();
        let mut buf = [0u8; 32];
        let mut addr = start;
        while addr < end && addr.saturating_add(32) < end {
            self.storage.read(addr, &mut buf)?;
            let mut i = 0usize;
            while i < 32 - 4 {
                if addr + i as u32 >= end {
                    break;
                }
                let word = (buf[i] as u32)
                    | ((buf[i + 1] as u32) << 8)
                    | ((buf[i + 2] as u32) << 16)
                    | ((buf[i + 3] as u32) << 24);
                if word == KV_MAGIC {
                    let found = addr + i as u32 - lay.kv_magic_off;
                    if found >= start {
                        return Ok(found);
                    }
                }
                i += 1;
            }
            addr += 32 - 4;
        }
        Ok(FAILED_ADDR)
    }

    /// Find the start of the trailing erased area, like `_fdb_continue_ff_addr`.
    ///
    /// Returns the aligned address just after the last programmed byte, or
    /// `end` when the region does not end in erased flash.
    fn continue_ff_addr(&mut self, start: u32, end: u32) -> Result<u32, F::Error> {
        const ERASED: u8 = 0xFF;
        let mut addr = start;
        let mut last_programmed: Option<u32> = None;
        // One buffer for the whole scan: re-initialising it per 32-byte chunk
        // would memset the same stack slot once per chunk.
        let mut buf = [0u8; 32];
        while addr < end {
            let n = core::cmp::min(32, (end - addr) as usize);
            self.storage.read(addr, &mut buf[..n])?;
            for (i, b) in buf[..n].iter().enumerate() {
                if *b != ERASED {
                    last_programmed = Some(addr + i as u32);
                }
            }
            addr += 32;
        }
        match last_programmed {
            Some(pos) => {
                let aligned = align_up(pos as usize + 1, low::wg_bytes(F::WRITE_GRAN_BITS)) as u32;
                Ok(core::cmp::min(aligned, end))
            }
            None => Ok(align_up(start as usize, low::wg_bytes(F::WRITE_GRAN_BITS)) as u32),
        }
    }

    /// Given the end address of the sector containing `hdr`, compute the next
    /// record address, like `get_next_kv_addr()` in C.
    ///
    /// `sec_end` is exclusive (`sector_addr + sector_size`). Callers only walk
    /// sectors whose store status is USING or FULL, so the empty-sector check
    /// C performs in `get_next_kv_addr()` is not needed here.
    ///
    /// Committed records are appended contiguously, so the next record starts
    /// exactly after this one. FlashDB-C re-scans the following bytes for the
    /// magic word on every step; we only do that when the header is damaged,
    /// which keeps the damaged-flash scanner out of the hot walk.
    #[inline]
    fn advance_kv(&mut self, sec_end: u32, hdr: &KvHeader) -> Result<u32, F::Error> {
        if hdr.addr > sec_end || hdr.len == 0 {
            return Ok(FAILED_ADDR);
        }
        if hdr.valid {
            let next = hdr.addr + hdr.len;
            return Ok(if next < sec_end { next } else { FAILED_ADDR });
        }
        if hdr.erased {
            // Trailing erased area: no more records.
            return Ok(FAILED_ADDR);
        }
        self.advance_kv_damaged(sec_end, hdr)
    }

    /// Damaged-record fallback: scan forward for the next KV magic word.
    #[cold]
    #[inline(never)]
    fn advance_kv_damaged(&mut self, sec_end: u32, hdr: &KvHeader) -> Result<u32, F::Error> {
        let lay = self.lay();
        let start = hdr.addr + wg_align(1, self.wg()) as u32;
        let end = sec_end - lay.sec_hdr_size;
        let addr = self.find_next_kv_addr(start, end)?;
        if addr == FAILED_ADDR || addr > sec_end {
            Ok(FAILED_ADDR)
        } else {
            Ok(addr)
        }
    }

    /// Read and decode a sector header into `sec` (no record traversal).
    ///
    /// The traversal half lives in [`KvdbImpl::read_sector_traversal`]: it is
    /// only needed when the caller has to know the sector's free space, but it
    /// is by far the larger piece, so keeping it out of this function keeps the
    /// frame of every plain sector scan small.
    fn read_sector_info_into(&mut self, addr: u32, sec: &mut SecInfo) -> Result<(), F::Error> {
        debug_assert!(self.storage.is_sector_aligned(addr));
        let lay = self.lay();
        let raw = read_raw_sector_header(&mut self.storage, addr, &lay)?;
        sec.addr = addr;
        sec.check_ok = raw.magic == KVDB_SECTOR_MAGIC
            && (raw.combined == SECTOR_NOT_COMBINED || raw.combined == SECTOR_COMBINED);
        if sec.check_ok {
            sec.store = raw.store;
            sec.dirty = raw.dirty;
            sec.combined = raw.combined;
        } else {
            sec.store = SectorStoreStatus::Unused;
            sec.dirty = SectorDirtyStatus::Unused;
            sec.combined = SECTOR_NOT_COMBINED;
        }
        sec.remain = 0;
        sec.empty_kv = FAILED_ADDR;
        Ok(())
    }

    /// Fill in the free space of a sector whose header `sec` was decoded by
    /// [`KvdbImpl::read_sector_info_into`] (FlashDB's `traversal` mode).
    fn read_sector_traversal(&mut self, sec: &mut SecInfo) -> Result<(), F::Error> {
        let lay = self.lay();
        let sec_size = self.storage.sec_size();
        let hdr_size = lay.sec_hdr_size;
        sec.remain = sec_size - hdr_size;
        sec.empty_kv = sec.addr + hdr_size;
        if sec.store == SectorStoreStatus::Empty {
            return Ok(());
        }
        if sec.store == SectorStoreStatus::Using {
            let mut kv_addr = sec.addr + hdr_size;
            let mut hdr = KvHeader::EMPTY;
            loop {
                self.read_kv_header_into(kv_addr, &mut hdr)?;
                if !hdr.valid && hdr.status != KvStatus::PreWrite && hdr.status != KvStatus::ErrHdr
                {
                    if hdr.erased {
                        // Trailing erased area: C's sector cache keeps this
                        // sector's empty_kv/remain up to date, i.e. the free
                        // space after the last valid record. Reporting 0 here
                        // would make alloc/GC stop decisions disagree with C.
                        sec.remain = sec_size - (sec.empty_kv - sec.addr);
                    } else {
                        sec.remain = 0;
                    }
                    break;
                }
                sec.empty_kv += hdr.len;
                sec.remain = sec.remain.saturating_sub(hdr.len);
                let next = self.advance_kv(sec.addr + sec_size, &hdr)?;
                if next == FAILED_ADDR {
                    break;
                }
                kv_addr = next;
            }
            let ff = self.continue_ff_addr(sec.empty_kv, sec.addr + sec_size)?;
            if sec.empty_kv != ff {
                sec.empty_kv = ff;
                sec.remain = sec_size - (ff - sec.addr);
            }
        }
        Ok(())
    }

    /// Next sector address in ring order, honouring the `combined` field.
    fn next_sector_addr(&self, pre: &SecInfo, traversed_len: u32) -> u32 {
        let sec_size = self.storage.sec_size();
        let block = if pre.combined == SECTOR_NOT_COMBINED || pre.combined == 0 {
            sec_size
        } else {
            match pre.combined.checked_mul(sec_size) {
                Some(v) if v > 0 => v,
                _ => sec_size,
            }
        };
        if traversed_len + block <= self.storage.max_size() {
            if pre.addr + block < self.storage.max_size() {
                pre.addr + block
            } else {
                0
            }
        } else {
            FAILED_ADDR
        }
    }

    // -- sector formatting --------------------------------------------------

    fn format_sector(&mut self, addr: u32, combined: u32) -> Result<(), F::Error> {
        debug_assert!(self.storage.is_sector_aligned(addr));
        let lay = self.lay();
        self.storage.erase_sector(addr)?;
        write_formatted_sector_header(&mut self.storage, addr, &lay, combined)?;
        self.cache_invalidate_sector(addr);
        if self.cur_sec.addr == addr {
            self.cur_sec = SecInfo::UNKNOWN;
        }
        Ok(())
    }

    fn format_all(&mut self) -> Result<(), F::Error> {
        let sec_size = self.storage.sec_size();
        let mut addr = 0u32;
        while addr < self.storage.max_size() {
            self.format_sector(addr, SECTOR_NOT_COMBINED)?;
            addr += sec_size;
        }
        self.storage.oldest_addr = 0;
        self.cur_sec = SecInfo::UNKNOWN;
        for slot in self.cache.iter_mut() {
            *slot = CacheSlot::EMPTY;
        }
        Ok(())
    }

    // -- write path ---------------------------------------------------------

    /// Allocate `kv_size` bytes of space; returns the record address and updates
    /// `cur_sec`. Returns `FAILED_ADDR` when there is no space (and may set the
    /// GC request flag).
    fn alloc_kv(&mut self, kv_size: u32) -> Result<u32, F::Error> {
        let threshold = self.lay().sec_remain_threshold;
        // Fast path: the sector we are already appending to.
        if self.cur_sec.check_ok
            && self.cur_sec.store == SectorStoreStatus::Using
            && self.cur_sec.remain > kv_size + threshold
            && (self.cur_sec.dirty == SectorDirtyStatus::False
                || (self.cur_sec.dirty == SectorDirtyStatus::True && !self.gc_request))
        {
            return Ok(self.cur_sec.empty_kv);
        }
        let mut sec = SecInfo::UNKNOWN;
        if self.alloc_scan(kv_size, &mut sec)? {
            self.cur_sec = sec;
            Ok(sec.empty_kv)
        } else {
            Ok(FAILED_ADDR)
        }
    }

    /// One pass over the sector ring that locates the first allocation
    /// candidate (preferring USING sectors) and writes it to `out`, so
    /// `alloc_kv` does not need a separate statistics scan.
    ///
    /// Returns whether a candidate was found. The pass stops as soon as one is
    /// found: the per-sector walk has no side effects, so the remaining
    /// sectors cannot change the result.
    fn alloc_scan(&mut self, kv_size: u32, out: &mut SecInfo) -> Result<bool, F::Error> {
        let threshold = self.lay().sec_remain_threshold;
        let sec_size = self.storage.sec_size();
        let mut empty = 0u32;
        let mut first_empty = FAILED_ADDR;
        let mut found = false;
        let mut sec = SecInfo::UNKNOWN;
        let mut addr = self.storage.oldest_addr;
        let mut traversed = 0u32;
        loop {
            traversed += sec_size;
            self.read_sector_info_into(addr, &mut sec)?;
            if sec.check_ok {
                match sec.store {
                    SectorStoreStatus::Empty => {
                        empty += 1;
                        if first_empty == FAILED_ADDR {
                            first_empty = sec.addr;
                        }
                    }
                    SectorStoreStatus::Using => {
                        *out = sec;
                        self.read_sector_traversal(out)?;
                        found = out.remain > kv_size + threshold
                            && (out.dirty == SectorDirtyStatus::False
                                || (out.dirty == SectorDirtyStatus::True && !self.gc_request));
                        if found {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            addr = self.next_sector_addr(&sec, traversed);
            if addr == FAILED_ADDR {
                break;
            }
        }
        if !found && first_empty != FAILED_ADDR {
            if empty > GC_EMPTY_SEC_THRESHOLD || self.gc_request {
                self.read_sector_info_into(first_empty, out)?;
                self.read_sector_traversal(out)?;
                found = out.remain > kv_size + threshold;
            } else {
                self.gc_request = true;
            }
        }
        Ok(found)
    }

    /// Count the EMPTY sectors and remember the last one in scan order (C's
    /// `gc_check_cb`), using one ring pass.
    fn sector_statistics(&mut self) -> Result<(u32, u32), F::Error> {
        let sec_size = self.storage.sec_size();
        let mut empty = 0u32;
        let mut last_empty = 0u32;
        let mut sec = SecInfo::UNKNOWN;
        let mut addr = self.storage.oldest_addr;
        let mut traversed = 0u32;
        loop {
            traversed += sec_size;
            self.read_sector_info_into(addr, &mut sec)?;
            if sec.check_ok && sec.store == SectorStoreStatus::Empty {
                empty += 1;
                last_empty = sec.addr;
            }
            addr = self.next_sector_addr(&sec, traversed);
            if addr == FAILED_ADDR {
                break;
            }
        }
        Ok((empty, last_empty))
    }

    /// Move the current sector's store status forward; returns whether the
    /// sector became full because of this record (`update_sec_status()` in C).
    ///
    /// It works on `self.cur_sec` directly (split borrow) instead of taking a
    /// `&mut SecInfo` copy of it.
    fn update_sec_status(&mut self, new_kv_len: u32) -> Result<bool, F::Error> {
        let lay = self.lay();
        let storage = &mut self.storage;
        let sec = &mut self.cur_sec;
        let mut is_full = false;
        match sec.store {
            SectorStoreStatus::Empty => {
                write_status(
                    storage,
                    sec.addr + lay.sec_store_off,
                    SEC_STATUS_NUM,
                    SectorStoreStatus::Using as usize,
                )?;
                sec.store = SectorStoreStatus::Using;
            }
            SectorStoreStatus::Using => {
                if sec.remain < lay.sec_remain_threshold
                    || sec.remain < new_kv_len + lay.sec_remain_threshold
                {
                    write_status(
                        storage,
                        sec.addr + lay.sec_store_off,
                        SEC_STATUS_NUM,
                        SectorStoreStatus::Full as usize,
                    )?;
                    sec.store = SectorStoreStatus::Full;
                    is_full = true;
                }
            }
            _ => {}
        }
        Ok(is_full)
    }

    /// Write a record at `cur_sec.empty_kv` (`create_kv_blob()` in C).
    fn create_kv_blob(&mut self, key: &[u8], value: &[u8], hash: u32) -> Result<(), F::Error> {
        let lay = self.lay();
        let wg = self.wg();
        let addr = self.cur_sec.empty_kv;
        if addr == FAILED_ADDR {
            return Err(Error::SavedFull);
        }
        let len = kv_len(&lay, wg, key.len(), value.len());
        let name_len = key.len() as u8;
        let value_len = value.len() as u32;

        // Mark the sector as using/full before the record is written.
        let is_full = self.update_sec_status(len)?;

        // CRC32 over name_len (+ struct padding), value_len, name (+padding),
        // value (+padding).
        let mut crc = crc_name_len(0, name_len);
        crc = crc32_u32(crc, value_len);
        crc = crc32(crc, key);
        crc = crc_pad(crc, wg_align(key.len(), wg) - key.len());
        crc = crc32(crc, value);
        crc = crc_pad(crc, wg_align(value.len(), wg) - value.len());

        write_raw_kv_header(
            &mut self.storage,
            addr,
            &lay,
            KvStatus::PreWrite,
            KV_MAGIC,
            len,
            crc,
            name_len,
            value_len,
        )?;
        self.storage.write_align(addr + lay.kv_hdr_size, key)?;
        self.storage.write_align(
            addr + lay.kv_hdr_size + wg_align(key.len(), wg) as u32,
            value,
        )?;
        write_status(
            &mut self.storage,
            addr,
            KV_STATUS_NUM,
            KvStatus::Write as usize,
        )?;

        self.cur_sec.empty_kv = addr + len;
        self.cur_sec.remain = self.cur_sec.remain.saturating_sub(len);
        if is_full {
            self.gc_request = true;
        }
        self.cache_insert(hash, addr);
        Ok(())
    }

    /// Delete (or pre-delete) a record and mark the sector dirty.
    ///
    /// `hash` is the name-cache hash of the record's key (when known), used to
    /// drop the cache entry; passing it keeps the caller from having to keep
    /// the key bytes alive.
    fn del_kv_addr(
        &mut self,
        addr: u32,
        hash: Option<u32>,
        complete: bool,
    ) -> Result<(), F::Error> {
        let status = if complete {
            KvStatus::Deleted
        } else {
            KvStatus::PreDelete
        };
        write_status(&mut self.storage, addr, KV_STATUS_NUM, status as usize)?;
        if complete {
            if !self.last_is_complete_del {
                if let Some(hash) = hash {
                    self.cache_invalidate(hash);
                }
            }
            self.last_is_complete_del = false;
        } else {
            self.last_is_complete_del = true;
        }
        // Mark the sector dirty (only the first deletion in a sector costs a
        // write, so the whole check is a cold path).
        let sec_size = self.storage.sec_size();
        let sec_addr = addr - addr % sec_size;
        if self.cur_sec.addr == sec_addr
            && self.cur_sec.check_ok
            && self.cur_sec.dirty == SectorDirtyStatus::True
        {
            return Ok(());
        }
        self.mark_sector_dirty(sec_addr)
    }

    /// First deletion in a sector: flag it dirty (`mark_sector_dirty` in C).
    #[cold]
    #[inline(never)]
    fn mark_sector_dirty(&mut self, sec_addr: u32) -> Result<(), F::Error> {
        let lay = self.lay();
        let dirty = self
            .storage
            .read_status(sec_addr + lay.sec_dirty_off, SEC_STATUS_NUM)?;
        if dirty != SectorDirtyStatus::False as usize {
            return Ok(());
        }
        write_status(
            &mut self.storage,
            sec_addr + lay.sec_dirty_off,
            SEC_STATUS_NUM,
            SectorDirtyStatus::True as usize,
        )?;
        if self.cur_sec.addr == sec_addr {
            self.cur_sec.dirty = SectorDirtyStatus::True;
        }
        Ok(())
    }

    // -- lookup -------------------------------------------------------------

    /// Look up a record by key, without verifying the value CRC.
    ///
    /// This reads only the record header and key name. Use
    /// [`KvdbImpl::read_value`] to read the value (which verifies its CRC32).
    pub fn find(&mut self, key: &[u8]) -> Result<Option<KvEntry>, F::Error> {
        let mut hdr = KvHeader::EMPTY;
        if !self.find_header_into(key, &mut hdr)? {
            return Ok(None);
        }
        let mut entry = self.entry_from_header(&hdr);
        self.read_name_into(&hdr, &mut entry.name)?;
        Ok(Some(entry))
    }

    /// Look up a record header by key (cache first, then a full scan).
    ///
    /// On a hit `out` holds the decoded header; the value CRC is *not*
    /// verified. This is the internal form used by `get`/`set`/`remove`: they
    /// only need the small header, not the 64-byte name copy embedded in
    /// [`KvEntry`].
    fn find_header_into(&mut self, key: &[u8], out: &mut KvHeader) -> Result<bool, F::Error> {
        if key.len() > KV_NAME_MAX {
            return Ok(false);
        }
        let hash = hash_key(key);
        if let Some(addr) = self.cache_lookup(hash) {
            self.read_kv_header_into(addr, out)?;
            if out.valid
                && out.status == KvStatus::Write
                && out.name_len as usize == key.len()
                && self.name_matches(out, key)?
            {
                return Ok(true);
            }
            // Stale cache entry: drop it and scan.
            self.cache_invalidate(hash);
        }
        if self.find_scan_header_into(key, out)? {
            self.cache_insert(hash, out.addr);
            return Ok(true);
        }
        Ok(false)
    }

    /// Find the address of a committed record, or `FAILED_ADDR` when absent.
    fn find_addr(&mut self, key: &[u8]) -> Result<u32, F::Error> {
        let mut hdr = KvHeader::EMPTY;
        Ok(if self.find_header_into(key, &mut hdr)? {
            hdr.addr
        } else {
            FAILED_ADDR
        })
    }

    /// Compare the key name stored at `hdr` with `key`, in small chunks so no
    /// 64-byte name buffer is needed on the lookup path.
    fn name_matches(&mut self, hdr: &KvHeader, key: &[u8]) -> Result<bool, F::Error> {
        let lay = self.lay();
        let mut buf = [0u8; 32];
        let mut off = 0usize;
        while off < key.len() {
            let n = core::cmp::min(32, key.len() - off);
            self.storage
                .read(hdr.addr + lay.kv_hdr_size + off as u32, &mut buf[..n])?;
            if buf[..n] != key[off..off + n] {
                return Ok(false);
            }
            off += n;
        }
        Ok(true)
    }

    fn find_scan_header_into(&mut self, key: &[u8], out: &mut KvHeader) -> Result<bool, F::Error> {
        let lay = self.lay();
        let sec_size = self.storage.sec_size();
        let mut sec = SecInfo::UNKNOWN;
        let mut addr = self.storage.oldest_addr;
        let mut traversed = 0u32;
        loop {
            traversed += sec_size;
            self.read_sector_info_into(addr, &mut sec)?;
            if sec.check_ok
                && (sec.store == SectorStoreStatus::Using || sec.store == SectorStoreStatus::Full)
            {
                let mut kv_addr = sec.addr + lay.sec_hdr_size;
                loop {
                    self.read_kv_header_into(kv_addr, out)?;
                    if out.valid
                        && out.status == KvStatus::Write
                        && out.name_len as usize == key.len()
                        && self.name_matches(out, key)?
                    {
                        return Ok(true);
                    }
                    let next = self.advance_kv(sec.addr + sec_size, out)?;
                    if next == FAILED_ADDR {
                        break;
                    }
                    kv_addr = next;
                }
            }
            addr = self.next_sector_addr(&sec, traversed);
            if addr == FAILED_ADDR {
                break;
            }
        }
        Ok(false)
    }

    /// Verify a record's CRC and copy up to `buf.len()` value bytes.
    fn read_value_verified(
        &mut self,
        hdr: &KvHeader,
        expected_key: Option<&[u8]>,
        buf: &mut [u8],
    ) -> Result<usize, F::Error> {
        let lay = self.lay();
        let wg = self.wg();
        let name_len = hdr.name_len as usize;
        let value_len = hdr.value_len as usize;
        let aligned_name = wg_align(name_len, wg);
        let aligned_value = wg_align(value_len, wg);
        let name_addr = hdr.addr + lay.kv_hdr_size;
        let value_addr = name_addr + aligned_name as u32;

        let mut crc = crc_name_len(0, hdr.name_len);
        crc = crc32_u32(crc, hdr.value_len);

        // One scratch buffer: the name first, then (re-used) the value
        // padding. A 64-byte single-read variant measured 92 B smaller than a
        // chunked 32-byte one, so ROM wins over the extra stack slot here.
        let mut scratch = [0u8; KV_NAME_MAX];
        self.storage.read(name_addr, &mut scratch[..aligned_name])?;
        crc = crc32(crc, &scratch[..aligned_name]);
        if let Some(key) = expected_key {
            if &scratch[..name_len] != key {
                return Err(Error::ReadCorrupted);
            }
        }

        let copy_len = core::cmp::min(buf.len(), value_len);
        if buf.len() >= value_len {
            // Single flash read of the whole value, CRC computed in place.
            if value_len > 0 {
                self.storage.read(value_addr, &mut buf[..value_len])?;
                crc = crc32(crc, &buf[..value_len]);
            }
        } else {
            crc = self.read_value_truncated(value_addr, value_len, copy_len, buf, crc)?;
        }
        if aligned_value > value_len {
            // Re-use the name scratch for the value padding (always smaller
            // than one write granule, so this loop normally runs once).
            let mut pad_off = 0usize;
            let padding = aligned_value - value_len;
            while pad_off < padding {
                let n = core::cmp::min(KV_NAME_MAX, padding - pad_off);
                self.storage.read(
                    value_addr + value_len as u32 + pad_off as u32,
                    &mut scratch[..n],
                )?;
                crc = crc32(crc, &scratch[..n]);
                pad_off += n;
            }
        }
        if crc != hdr.crc {
            return Err(Error::ReadCorrupted);
        }
        Ok(copy_len)
    }

    /// Cold half of [`KvdbImpl::read_value_verified`]: the caller's buffer is
    /// smaller than the value, so read in chunks and copy the prefix.
    #[cold]
    #[inline(never)]
    fn read_value_truncated(
        &mut self,
        value_addr: u32,
        value_len: usize,
        copy_len: usize,
        buf: &mut [u8],
        mut crc: u32,
    ) -> Result<u32, F::Error> {
        let mut tmp = [0u8; 32];
        let mut off = 0usize;
        while off < value_len {
            let chunk = core::cmp::min(32, value_len - off);
            self.storage
                .read(value_addr + off as u32, &mut tmp[..chunk])?;
            crc = crc32(crc, &tmp[..chunk]);
            if off < copy_len {
                let n = core::cmp::min(copy_len - off, chunk);
                buf[off..off + n].copy_from_slice(&tmp[..n]);
            }
            off += chunk;
        }
        Ok(crc)
    }

    /// Get a value into `buf`; returns the number of bytes copied.
    ///
    /// `Ok(None)` means the key does not exist. If `buf` is smaller than the
    /// value, the value is truncated (use [`KvdbImpl::find`] for its length).
    /// The value's CRC32 is always verified before any byte is returned.
    pub fn get(&mut self, key: &[u8], buf: &mut [u8]) -> Result<Option<usize>, F::Error> {
        let mut hdr = KvHeader::EMPTY;
        if !self.find_header_into(key, &mut hdr)? {
            return Ok(None);
        }
        let n = self.read_value_verified(&hdr, Some(key), buf)?;
        Ok(Some(n))
    }

    /// Get a value as `&str`; returns `Ok(None)` when the key is missing or the
    /// value is not valid UTF-8.
    pub fn get_str<'b>(
        &mut self,
        key: &[u8],
        buf: &'b mut [u8],
    ) -> Result<Option<&'b str>, F::Error> {
        match self.get(key, buf)? {
            None => Ok(None),
            Some(n) => Ok(core::str::from_utf8(&buf[..n]).ok()),
        }
    }

    /// Copy the value of a record obtained from [`KvdbImpl::find`].
    pub fn read_value(&mut self, entry: &KvEntry, buf: &mut [u8]) -> Result<usize, F::Error> {
        self.read_value_verified(&entry.header(), None, buf)
    }

    // -- mutation -----------------------------------------------------------

    /// Set a value (create or update).
    pub fn set(&mut self, key: &[u8], value: &[u8]) -> Result<(), F::Error> {
        if key.len() > KV_NAME_MAX {
            return Err(Error::InvalidName);
        }
        let lay = self.lay();
        let wg = self.wg();
        let len = kv_len(&lay, wg, key.len(), value.len());
        if len > self.storage.sec_size() - lay.sec_hdr_size {
            return Err(Error::SavedFull);
        }
        let hash = hash_key(key);

        // 1. Make sure the new record fits before touching the old one.
        let mut addr = self.alloc_kv(len)?;
        if addr == FAILED_ADDR && self.gc_request {
            self.gc_collect_by_free_size(len)?;
            addr = self.alloc_kv(len)?;
        }
        if addr == FAILED_ADDR {
            self.gc_request = false;
            return Err(Error::SavedFull);
        }

        // 2. Find the old record (if any) and mark it pre-deleted. Only its
        //    address is needed, so the lookup must not build a full `KvEntry`.
        let old_addr = self.find_addr(key)?;
        if old_addr != FAILED_ADDR {
            self.del_kv_addr(old_addr, Some(hash), false)?;
        }

        // 3. Write the new record.
        let result = self.create_kv_blob(key, value, hash);

        // 4. Finalise deletion of the old record once the new one is committed.
        if old_addr != FAILED_ADDR && result.is_ok() {
            self.del_kv_addr(old_addr, Some(hash), true)?;
        }
        result?;

        if self.gc_request {
            self.gc_collect_by_free_size(len)?;
        }
        Ok(())
    }

    /// Set a string value.
    pub fn set_str(&mut self, key: &str, value: &str) -> Result<(), F::Error> {
        self.set(key.as_bytes(), value.as_bytes())
    }

    /// Delete a key; returns whether the key existed.
    pub fn remove(&mut self, key: &[u8]) -> Result<bool, F::Error> {
        let addr = self.find_addr(key)?;
        if addr == FAILED_ADDR {
            return Ok(false);
        }
        self.del_kv_addr(addr, Some(hash_key(key)), true)?;
        Ok(true)
    }

    // -- garbage collection -------------------------------------------------

    /// Move still-live records out of a dirty sector, then format it.
    /// Returns `true` when GC has freed enough space and can stop.
    fn do_gc(
        &mut self,
        sec: &SecInfo,
        free_size: u32,
        last_gc_addr: &mut u32,
        last_gc_remain: &mut Option<u32>,
    ) -> Result<bool, F::Error> {
        let lay = self.lay();
        let sec_size = self.storage.sec_size();
        write_status(
            &mut self.storage,
            sec.addr + lay.sec_dirty_off,
            SEC_STATUS_NUM,
            SectorDirtyStatus::Gc as usize,
        )?;
        let mut kv_addr = sec.addr + lay.sec_hdr_size;
        let mut hdr = KvHeader::EMPTY;
        loop {
            self.read_kv_header_into(kv_addr, &mut hdr)?;
            if hdr.valid && (hdr.status == KvStatus::Write || hdr.status == KvStatus::PreDelete) {
                self.move_kv(&hdr)?;
            }
            let next = self.advance_kv(sec.addr + sec_size, &hdr)?;
            if next == FAILED_ADDR {
                break;
            }
            kv_addr = next;
        }
        self.format_sector(sec.addr, SECTOR_NOT_COMBINED)?;
        let previous = *last_gc_addr;
        *last_gc_addr = sec.addr;
        self.storage.oldest_addr = self.next_sector_addr(sec, 0);
        let previous_remain = match *last_gc_remain {
            Some(remain) => remain,
            None => {
                // First collected sector in this GC pass: C reads the sector at
                // `last_gc_addr` (initially the last EMPTY sector) to decide
                // whether to stop.
                let mut last = SecInfo::UNKNOWN;
                self.read_sector_info_into(previous, &mut last)?;
                if last.check_ok {
                    self.read_sector_traversal(&mut last)?;
                    last.remain
                } else {
                    0
                }
            }
        };
        // A freshly formatted sector always has (sec_size - header) free bytes.
        *last_gc_remain = Some(sec_size - lay.sec_hdr_size);
        Ok(previous_remain > free_size)
    }

    /// FNV-1a hash of a record's key name, streamed from flash so the GC path
    /// never materialises a 64-byte name buffer.
    fn hash_name(&mut self, hdr: &KvHeader) -> Result<u32, F::Error> {
        let lay = self.lay();
        let name_len = hdr.name_len as usize;
        let mut hash = FNV_OFFSET;
        let mut buf = [0u8; 16];
        let mut off = 0usize;
        while off < name_len {
            let n = core::cmp::min(16, name_len - off);
            self.storage
                .read(hdr.addr + lay.kv_hdr_size + off as u32, &mut buf[..n])?;
            hash = hash_bytes_into(hash, &buf[..n]);
            off += n;
        }
        Ok(hash)
    }

    /// Move one live record to a new location (used by GC and recovery).
    fn move_kv(&mut self, hdr: &KvHeader) -> Result<(), F::Error> {
        let lay = self.lay();
        let hash = self.hash_name(hdr)?;
        if hdr.status == KvStatus::Write {
            self.del_kv_addr(hdr.addr, Some(hash), false)?;
        }
        let new_addr = self.alloc_kv(hdr.len)?;
        if new_addr == FAILED_ADDR {
            return Err(Error::SavedFull);
        }
        // During recovery a PRE_DELETE record may already have been recreated
        // (recovery-only, hence the cold out-of-line helper).
        if self.in_recovery && hdr.status == KvStatus::PreDelete && self.recovery_dup_check(hdr)? {
            return Ok(());
        }
        self.update_sec_status(hdr.len)?;
        write_status(
            &mut self.storage,
            new_addr,
            KV_STATUS_NUM,
            KvStatus::PreWrite as usize,
        )?;
        let total = hdr.len - lay.kv_magic_off;
        let mut off = 0u32;
        let mut buf = [0u8; 32];
        while off < total {
            let chunk = core::cmp::min(32, total - off);
            let src = hdr.addr + lay.kv_magic_off + off;
            let dst = new_addr + lay.kv_magic_off + off;
            self.storage.read(src, &mut buf[..chunk as usize])?;
            self.storage.write(dst, &buf[..chunk as usize])?;
            off += chunk;
        }
        write_status(
            &mut self.storage,
            new_addr,
            KV_STATUS_NUM,
            KvStatus::Write as usize,
        )?;
        self.cur_sec.empty_kv = new_addr + hdr.len;
        self.cur_sec.remain = self.cur_sec.remain.saturating_sub(hdr.len);
        self.cache_insert(hash, new_addr);
        self.del_kv_addr(hdr.addr, Some(hash), true)?;
        Ok(())
    }

    /// Recovery-only check for a PRE_DELETE record that was already recreated
    /// at another address: delete the stale copy and report `true`.
    #[cold]
    #[inline(never)]
    fn recovery_dup_check(&mut self, hdr: &KvHeader) -> Result<bool, F::Error> {
        let mut name = [0u8; KV_NAME_MAX];
        self.read_name_into(hdr, &mut name)?;
        let name = &name[..hdr.name_len as usize];
        let mut existing = KvHeader::EMPTY;
        if self.find_scan_header_into(name, &mut existing)? && existing.addr != hdr.addr {
            self.del_kv_addr(hdr.addr, Some(hash_key(name)), true)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn gc_collect_by_free_size(&mut self, free_size: u32) -> Result<(), F::Error> {
        let (empty, last_empty) = self.sector_statistics()?;
        if empty <= GC_EMPTY_SEC_THRESHOLD {
            let sec_size = self.storage.sec_size();
            // C's `gc_check_cb` records the last EMPTY sector address in scan
            // order and passes it as `gc->last_gc_sec_addr`. The first do_gc()
            // stop-check reads *that* sector's remain.
            let mut last_gc_addr = last_empty;
            let mut last_gc_remain: Option<u32> = None;
            let mut sec = SecInfo::UNKNOWN;
            let mut addr = self.storage.oldest_addr;
            let mut traversed = 0u32;
            loop {
                traversed += sec_size;
                self.read_sector_info_into(addr, &mut sec)?;
                if sec.check_ok
                    && (sec.dirty == SectorDirtyStatus::True || sec.dirty == SectorDirtyStatus::Gc)
                {
                    if self.do_gc(&sec, free_size, &mut last_gc_addr, &mut last_gc_remain)? {
                        break;
                    }
                }
                addr = self.next_sector_addr(&sec, traversed);
                if addr == FAILED_ADDR {
                    break;
                }
            }
        }
        self.gc_request = false;
        Ok(())
    }

    fn gc_collect(&mut self) -> Result<(), F::Error> {
        let max = self.storage.max_size();
        self.gc_collect_by_free_size(max)
    }

    // -- mount/recovery -----------------------------------------------------

    /// One recovery pass over all live records (FlashDB `_fdb_kv_load`).
    fn recovery_scan(&mut self) -> Result<(), F::Error> {
        let lay = self.lay();
        let sec_size = self.storage.sec_size();
        let mut sec = SecInfo::UNKNOWN;
        let mut hdr = KvHeader::EMPTY;
        let mut addr = self.storage.oldest_addr;
        let mut traversed = 0u32;
        loop {
            traversed += sec_size;
            self.read_sector_info_into(addr, &mut sec)?;
            if sec.check_ok
                && (sec.store == SectorStoreStatus::Using || sec.store == SectorStoreStatus::Full)
            {
                let mut kv_addr = sec.addr + lay.sec_hdr_size;
                loop {
                    self.read_kv_header_into(kv_addr, &mut hdr)?;
                    match hdr.status {
                        KvStatus::PreDelete if hdr.valid => {
                            self.move_kv(&hdr)?;
                        }
                        KvStatus::PreWrite => {
                            write_status(
                                &mut self.storage,
                                hdr.addr,
                                KV_STATUS_NUM,
                                KvStatus::ErrHdr as usize,
                            )?;
                        }
                        _ => {}
                    }
                    let next = self.advance_kv(sec.addr + sec_size, &hdr)?;
                    if next == FAILED_ADDR {
                        break;
                    }
                    kv_addr = next;
                }
            }
            addr = self.next_sector_addr(&sec, traversed);
            if addr == FAILED_ADDR {
                break;
            }
        }
        Ok(())
    }

    /// Write caller-provided defaults (only when the partition had to be
    /// formatted). Kept out of line so mounting a populated database does not
    /// pull the whole write path's stack frame into `mount`.
    #[inline(never)]
    fn write_defaults(&mut self, defaults: &[(&str, &[u8])]) -> Result<(), F::Error> {
        for (key, value) in defaults {
            self.set(key.as_bytes(), value)?;
        }
        Ok(())
    }

    fn load(&mut self, defaults: &[(&str, &[u8])]) -> Result<(), F::Error> {
        self.in_recovery = true;
        let total = self.storage.sector_count();

        // Pass 1: count damaged sector headers and find the oldest sector with
        // the same pre-format statuses FlashDB-C uses (`check_oldest_addr_cb`):
        // the oldest sector is the first FULL/USING sector that follows an
        // EMPTY one; damaged headers are UNUSED and break that chain. Mounts
        // visit every sector, so an ascending walk is equivalent to the ring
        // iterator here.
        let sec_size = self.storage.sec_size();
        let max_size = self.storage.max_size();
        let mut failed = 0u32;
        let mut oldest = 0u32;
        let mut gc_pending = false;
        let mut prev_store = SectorStoreStatus::Unused;
        let mut sec = SecInfo::UNKNOWN;
        let mut addr = 0u32;
        while addr < max_size {
            self.read_sector_info_into(addr, &mut sec)?;
            if !sec.check_ok {
                failed += 1;
            } else if sec.dirty == SectorDirtyStatus::Gc {
                // Interrupted GC, like `check_and_recovery_gc_cb`. Formatting a
                // damaged header never changes a valid one, so detecting this
                // on the pre-format scan is equivalent to C's post-format pass.
                gc_pending = true;
            }
            if prev_store == SectorStoreStatus::Empty
                && (sec.store == SectorStoreStatus::Full || sec.store == SectorStoreStatus::Using)
            {
                oldest = sec.addr;
            }
            prev_store = sec.store;
            addr += sec_size;
        }

        if failed == total {
            // Unformatted partition (or all headers destroyed): format once.
            self.format_all()?;
            self.write_defaults(defaults)?;
            // A freshly formatted database is empty: no oldest-sector scan,
            // no interrupted GC and nothing to recover. This also saves the
            // header reads those passes would perform on first boot.
            self.in_recovery = false;
            return Ok(());
        } else if failed > 0 {
            // Format only the damaged sectors.
            let mut addr = 0u32;
            while addr < max_size {
                self.read_sector_info_into(addr, &mut sec)?;
                if !sec.check_ok {
                    self.format_sector(addr, SECTOR_NOT_COMBINED)?;
                }
                addr += sec_size;
            }
        }

        self.storage.oldest_addr = oldest;

        // Resume an interrupted garbage collection, then recover records.
        if gc_pending {
            self.gc_request = true;
            self.gc_collect()?;
        }
        loop {
            self.recovery_scan()?;
            if self.gc_request {
                self.gc_request = false;
                self.gc_collect()?;
            } else {
                break;
            }
        }
        self.in_recovery = false;
        Ok(())
    }

    // -- public maintenance -------------------------------------------------

    /// Erase every sector and reset the database to empty.
    pub fn format(&mut self) -> Result<(), F::Error> {
        self.format_all()
    }

    /// Erase every sector and write `defaults`.
    pub fn format_with_defaults(&mut self, defaults: &[(&str, &[u8])]) -> Result<(), F::Error> {
        self.format_all()?;
        self.write_defaults(defaults)
    }

    /// Verify the CRC32 of every committed record.
    pub fn check(&mut self) -> Result<(), F::Error> {
        let sec_size = self.storage.sec_size();
        let lay = self.lay();
        let mut sec = SecInfo::UNKNOWN;
        let mut hdr = KvHeader::EMPTY;
        let mut scratch = [0u8; 8];
        let mut addr = self.storage.oldest_addr;
        let mut traversed = 0u32;
        loop {
            traversed += sec_size;
            self.read_sector_info_into(addr, &mut sec)?;
            if sec.check_ok
                && (sec.store == SectorStoreStatus::Using || sec.store == SectorStoreStatus::Full)
            {
                let mut kv_addr = sec.addr + lay.sec_hdr_size;
                loop {
                    self.read_kv_header_into(kv_addr, &mut hdr)?;
                    if !hdr.valid {
                        // The record area ends at the first fully erased header.
                        if hdr.erased {
                            break;
                        }
                        return Err(Error::ReadCorrupted);
                    }
                    if hdr.status == KvStatus::Write || hdr.status == KvStatus::PreDelete {
                        self.read_value_verified(&hdr, None, &mut scratch)?;
                    }
                    let next = self.advance_kv(sec.addr + sec_size, &hdr)?;
                    if next == FAILED_ADDR {
                        break;
                    }
                    kv_addr = next;
                }
            }
            addr = self.next_sector_addr(&sec, traversed);
            if addr == FAILED_ADDR {
                break;
            }
        }
        Ok(())
    }

    /// Compute live statistics by iterating the database.
    pub fn stats(&mut self) -> Result<KvStats, F::Error> {
        let mut stats = KvStats::default();
        let mut it = self.iter();
        while let Some(entry) = it.next() {
            let entry = entry?;
            stats.key_count += 1;
            stats.used_bytes += entry.len;
            stats.value_bytes += entry.value_len;
        }
        Ok(stats)
    }

    /// Iterate all live records from the oldest sector.
    pub fn iter(&mut self) -> KvIterator<'_, F, CACHE> {
        KvIterator {
            sector_addr: self.storage.oldest_addr,
            traversed_len: 0,
            next_addr: FAILED_ADDR,
            sec_end: 0,
            in_sector: false,
            done: false,
            db: self,
        }
    }
}

// ---------------------------------------------------------------------------
// Iterator
// ---------------------------------------------------------------------------

impl<'a, F: Flash, const CACHE: usize> KvIterator<'a, F, CACHE> {
    /// Copy the value of `entry` into `buf`; returns the copied length.
    pub fn read_value(&mut self, entry: &KvEntry, buf: &mut [u8]) -> Result<usize, F::Error> {
        self.db.read_value_verified(&entry.header(), None, buf)
    }
}

impl<'a, F: Flash, const CACHE: usize> Iterator for KvIterator<'a, F, CACHE> {
    type Item = Result<KvEntry, F::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let sec_size = self.db.storage.sec_size();
        loop {
            if !self.in_sector {
                if self.sector_addr == FAILED_ADDR {
                    self.done = true;
                    return None;
                }
                self.traversed_len += sec_size;
                // Scoped so the 24-byte `SecInfo` slot can be reused by the
                // record/entry locals below (smaller iterator frame).
                let mut sec = SecInfo::UNKNOWN;
                if let Err(e) = self.db.read_sector_info_into(self.sector_addr, &mut sec) {
                    self.done = true;
                    return Some(Err(e));
                }
                let next_sec = self.db.next_sector_addr(&sec, self.traversed_len);
                self.sector_addr = next_sec;
                if sec.check_ok
                    && (sec.store == SectorStoreStatus::Using
                        || sec.store == SectorStoreStatus::Full)
                {
                    self.sec_end = sec.addr + sec_size;
                    self.next_addr = sec.addr + self.db.lay().sec_hdr_size;
                    self.in_sector = true;
                } else {
                    continue;
                }
            }
            if self.next_addr == FAILED_ADDR {
                self.in_sector = false;
                continue;
            }
            let mut hdr = KvHeader::EMPTY;
            if let Err(e) = self.db.read_kv_header_into(self.next_addr, &mut hdr) {
                self.done = true;
                return Some(Err(e));
            }
            match self.db.advance_kv(self.sec_end, &hdr) {
                Ok(next) => self.next_addr = next,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
            if hdr.valid && hdr.status == KvStatus::Write {
                let mut entry = self.db.entry_from_header(&hdr);
                if let Err(e) = self.db.read_name_into(&hdr, &mut entry.name) {
                    self.done = true;
                    return Some(Err(e));
                }
                return Some(Ok(entry));
            }
            // Invalid or non-committed record: keep going.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestFlash {
        data: [u8; 16384],
    }

    impl TestFlash {
        fn new() -> Self {
            Self {
                data: [0xFF; 16384],
            }
        }
    }

    impl Flash for TestFlash {
        type Error = core::convert::Infallible;
        const WRITE_GRAN_BITS: u32 = 8;

        fn read(&mut self, addr: u32, buf: &mut [u8]) -> core::result::Result<(), Self::Error> {
            let start = addr as usize;
            buf.copy_from_slice(&self.data[start..start + buf.len()]);
            Ok(())
        }
        fn write(&mut self, addr: u32, buf: &[u8]) -> core::result::Result<(), Self::Error> {
            let start = addr as usize;
            for (i, b) in buf.iter().enumerate() {
                self.data[start + i] &= *b;
            }
            Ok(())
        }
        fn erase(&mut self, addr: u32, len: u32) -> core::result::Result<(), Self::Error> {
            let start = addr as usize;
            self.data[start..start + len as usize].fill(0xFF);
            Ok(())
        }
    }

    #[test]
    fn cache_holds_all_small_key_sets() {
        let mut db = Kvdb::mount(TestFlash::new(), Config::new(4096, 16384)).unwrap();
        let mut key = [0u8; 8];
        let mut value = [0u8; 8];
        for k in 0..32u32 {
            key[..2].copy_from_slice(b"kv");
            key[2] = b'0' + (k % 8) as u8;
            for j in 0..8 {
                value[j] = (0x11u32 + k * 31 + j as u32 * 7) as u8;
            }
            db.set(&key[..3], &value).unwrap();
            // Every key of the last round must be a cache hit.
            if k >= 16 {
                let hash = hash_key(&key[..3]);
                assert!(
                    db.cache_lookup(hash).is_some(),
                    "cache miss for {:?} at k={k}",
                    core::str::from_utf8(&key[..3]).unwrap()
                );
            }
        }
        let occupied = db.cache.iter().filter(|s| s.addr != FAILED_ADDR).count();
        // Eight distinct keys that map to eight distinct sets (one slot each).
        assert_eq!(occupied, 8);
    }
}
