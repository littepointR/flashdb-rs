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
use crate::low::{
    align_up, crc32, crc32_u32, decode_status, encode_status, get_u32, put_u32, wg_align, KvLayout,
    KvStatus, SectorDirtyStatus, SectorStoreStatus, FAILED_ADDR, KVDB_SECTOR_MAGIC, KV_MAGIC,
    KV_NAME_MAX, SECTOR_COMBINED, SECTOR_NOT_COMBINED,
};
use crate::low;
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

#[inline]
fn read_raw_sector_header<F: Flash>(
    storage: &mut Storage<F>,
    addr: u32,
    lay: &KvLayout,
) -> Result<RawSectorHeader, F::Error> {
    let wg = F::WRITE_GRAN_BITS;
    let raw = lay.sec_raw_size as usize;
    if wg <= 8 {
        let mut b = [0u8; 20];
        storage.read(addr, &mut b[..raw])?;
        Ok(decode_sector_header(&b[..raw], lay, wg))
    } else if wg <= 32 {
        let mut b = [0u8; 36];
        storage.read(addr, &mut b[..raw])?;
        Ok(decode_sector_header(&b[..raw], lay, wg))
    } else if wg <= 64 {
        let mut b = [0u8; 64];
        storage.read(addr, &mut b[..raw])?;
        Ok(decode_sector_header(&b[..raw], lay, wg))
    } else if wg <= 128 {
        let mut b = [0u8; 112];
        storage.read(addr, &mut b[..raw])?;
        Ok(decode_sector_header(&b[..raw], lay, wg))
    } else {
        let mut b = [0u8; 224];
        storage.read(addr, &mut b[..raw])?;
        Ok(decode_sector_header(&b[..raw], lay, wg))
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
        storage.write_status(addr + lay.sec_store_off, SEC_STATUS_NUM, store_status)?;
        storage.write_status(addr + lay.sec_dirty_off, SEC_STATUS_NUM, dirty_status)?;
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
    let raw = lay.kv_raw_size as usize;
    if wg <= 8 {
        let mut b = [0u8; 28];
        storage.read(addr, &mut b[..raw])?;
        Ok(decode_kv_header_raw(&b[..raw], lay, wg))
    } else if wg <= 32 {
        let mut b = [0u8; 40];
        storage.read(addr, &mut b[..raw])?;
        Ok(decode_kv_header_raw(&b[..raw], lay, wg))
    } else if wg <= 64 {
        let mut b = [0u8; 64];
        storage.read(addr, &mut b[..raw])?;
        Ok(decode_kv_header_raw(&b[..raw], lay, wg))
    } else if wg <= 128 {
        let mut b = [0u8; 112];
        storage.read(addr, &mut b[..raw])?;
        Ok(decode_kv_header_raw(&b[..raw], lay, wg))
    } else {
        let mut b = [0u8; 196];
        storage.read(addr, &mut b[..raw])?;
        Ok(decode_kv_header_raw(&b[..raw], lay, wg))
    }
}

#[inline]
fn fill_kv_header_block(
    buf: &mut [u8],
    lay: &KvLayout,
    magic: u32,
    len: u32,
    crc: u32,
    name_len: u8,
    value_len: u32,
) {
    put_u32(buf, lay.kv_magic_off as usize, magic);
    put_u32(buf, lay.kv_len_off as usize, len);
    put_u32(buf, lay.kv_crc_off as usize, crc);
    buf[lay.kv_name_len_off as usize] = name_len;
    put_u32(buf, lay.kv_value_len_off as usize, value_len);
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
    storage.write_status(addr, KV_STATUS_NUM, status as usize)?;
    let wg = F::WRITE_GRAN_BITS;
    let raw = lay.kv_raw_size as usize;
    let mo = lay.kv_magic_off as usize;
    if wg <= 8 {
        let mut b = [0xFFu8; 28];
        fill_kv_header_block(&mut b[..raw], lay, magic, len, crc, name_len, value_len);
        storage.write(addr + mo as u32, &b[mo..raw])
    } else if wg <= 32 {
        let mut b = [0xFFu8; 40];
        fill_kv_header_block(&mut b[..raw], lay, magic, len, crc, name_len, value_len);
        storage.write(addr + mo as u32, &b[mo..raw])
    } else if wg <= 64 {
        let mut b = [0xFFu8; 64];
        fill_kv_header_block(&mut b[..raw], lay, magic, len, crc, name_len, value_len);
        storage.write(addr + mo as u32, &b[mo..raw])
    } else if wg <= 128 {
        let mut b = [0xFFu8; 112];
        fill_kv_header_block(&mut b[..raw], lay, magic, len, crc, name_len, value_len);
        storage.write(addr + mo as u32, &b[mo..raw])
    } else {
        let mut b = [0xFFu8; 196];
        fill_kv_header_block(&mut b[..raw], lay, magic, len, crc, name_len, value_len);
        storage.write(addr + mo as u32, &b[mo..raw])
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

#[inline]
fn hash_key(key: &[u8]) -> u32 {
    // FNV-1a: cheap and good enough for a direct-mapped name cache.
    let mut hash = 0x811C_9DC5u32;
    for &b in key {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
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
            core::ptr::addr_of_mut!((*p).cache).write([CacheSlot::EMPTY; CACHE]);
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
                self.cache_lru = (self.cache_lru & !(1 << set_index))
                    | (new_mru << set_index);
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

    /// Decode a KV header at `addr`, flagging whether it is self-consistent.
    fn read_kv_header(&mut self, addr: u32) -> Result<KvHeader, F::Error> {
        let lay = self.lay();
        let raw = read_raw_kv_header(&mut self.storage, addr, &lay)?;
        let wg = self.wg();
        // Guard the length computation before trusting header fields: an
        // erased header (end of the record area) has value_len == 0xFFFF_FFFF.
        let sane = raw.name_len as usize <= KV_NAME_MAX
            && raw.value_len <= self.storage.max_size()
            && raw.len != FAILED_ADDR
            && raw.len >= lay.kv_hdr_size
            && raw.len <= self.storage.max_size();
        let expected_len = if sane {
            lay.kv_hdr_size as u64
                + wg_align(raw.name_len as usize, wg) as u64
                + wg_align(raw.value_len as usize, wg) as u64
        } else {
            u64::MAX
        };
        let valid = sane && raw.magic == KV_MAGIC && raw.len as u64 == expected_len;
        Ok(KvHeader {
            addr,
            status: raw.status,
            crc: raw.crc,
            len: if valid { raw.len } else { lay.kv_hdr_size },
            value_len: if valid { raw.value_len } else { 0 },
            name_len: if valid { raw.name_len } else { 0 },
            valid,
            erased: raw.erased,
        })
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

    /// Read the key name of a record (including write-granule padding).
    fn read_name(&mut self, hdr: &KvHeader) -> Result<[u8; KV_NAME_MAX], F::Error> {
        let mut name = [0u8; KV_NAME_MAX];
        self.read_name_into(hdr, &mut name)?;
        Ok(name)
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
    fn find_next_kv_addr(&mut self, start: u32, end: u32) -> Result<u32, F::Error> {
        let lay = self.lay();
        let mut buf = [0u8; 32];
        let mut addr = start;
        while addr < end && addr.saturating_add(32) < end {
            self.storage.read(addr, &mut buf)?;
            for i in 0..(32 - 4) {
                if addr + i as u32 >= end {
                    break;
                }
                if get_u32(&buf, i) == KV_MAGIC {
                    let found = addr + i as u32 - lay.kv_magic_off;
                    if found >= start {
                        return Ok(found);
                    }
                }
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
        while addr < end {
            let mut buf = [0u8; 32];
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
    fn advance_kv(&mut self, sec_end: u32, hdr: &KvHeader) -> Result<u32, F::Error> {
        let lay = self.lay();
        if hdr.addr > sec_end || hdr.len == 0 {
            return Ok(FAILED_ADDR);
        }
        if hdr.valid {
            // Committed records are appended contiguously, so the next record
            // starts exactly after this one. FlashDB-C re-scans the following
            // bytes for the magic word on every step; we only do that when the
            // header is damaged (see below), which removes one flash read per
            // record from every traversal.
            let next = hdr.addr + hdr.len;
            return Ok(if next < sec_end { next } else { FAILED_ADDR });
        }
        if hdr.erased {
            // Trailing erased area: no more records.
            return Ok(FAILED_ADDR);
        }
        // Damaged record: fall back to scanning for the next magic word.
        let start = hdr.addr + wg_align(1, self.wg()) as u32;
        let end = sec_end - lay.sec_hdr_size;
        let addr = self.find_next_kv_addr(start, end)?;
        if addr == FAILED_ADDR || addr > sec_end {
            Ok(FAILED_ADDR)
        } else {
            Ok(addr)
        }
    }

    fn read_sector_info(&mut self, addr: u32, traversal: bool) -> Result<SecInfo, F::Error> {
        debug_assert!(self.storage.is_sector_aligned(addr));
        let lay = self.lay();
        let raw = read_raw_sector_header(&mut self.storage, addr, &lay)?;
        let mut sec = SecInfo {
            addr,
            check_ok: raw.magic == KVDB_SECTOR_MAGIC
                && (raw.combined == SECTOR_NOT_COMBINED || raw.combined == SECTOR_COMBINED),
            store: raw.store,
            dirty: raw.dirty,
            combined: raw.combined,
            remain: 0,
            empty_kv: FAILED_ADDR,
        };
        if !sec.check_ok {
            sec.store = SectorStoreStatus::Unused;
            sec.dirty = SectorDirtyStatus::Unused;
            sec.combined = SECTOR_NOT_COMBINED;
            return Ok(sec);
        }
        if !traversal {
            return Ok(sec);
        }
        let sec_size = self.storage.sec_size();
        let hdr_size = lay.sec_hdr_size;
        sec.remain = sec_size - hdr_size;
        sec.empty_kv = addr + hdr_size;
        if sec.store == SectorStoreStatus::Empty {
            return Ok(sec);
        }
        if sec.store == SectorStoreStatus::Using {
            let mut kv_addr = addr + hdr_size;
            loop {
                let hdr = self.read_kv_header(kv_addr)?;
                if !hdr.valid && hdr.status != KvStatus::PreWrite && hdr.status != KvStatus::ErrHdr {
                    if hdr.erased {
                        // Trailing erased area: C's sector cache keeps this
                        // sector's empty_kv/remain up to date, i.e. the free
                        // space after the last valid record. Reporting 0 here
                        // would make alloc/GC stop decisions disagree with C.
                        sec.remain = sec_size - (sec.empty_kv - addr);
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
            let ff = self.continue_ff_addr(sec.empty_kv, addr + sec_size)?;
            if sec.empty_kv != ff {
                sec.empty_kv = ff;
                sec.remain = sec_size - (ff - addr);
            }
        }
        Ok(sec)
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

    /// One mount-time scan that computes the oldest sector address (C's
    /// `check_oldest_addr_cb`) and whether a garbage collection was
    /// interrupted (a sector left with dirty status GC).
    fn inspect_sectors(&mut self) -> Result<(u32, bool), F::Error> {
        let sec_size = self.storage.sec_size();
        let mut oldest = 0u32;
        let mut prev = SectorStoreStatus::Unused;
        let mut gc_pending = false;
        let mut addr = 0u32;
        let mut traversed = 0u32;
        loop {
            traversed += sec_size;
            let sec = self.read_sector_info(addr, false)?;
            if prev == SectorStoreStatus::Empty
                && (sec.store == SectorStoreStatus::Full || sec.store == SectorStoreStatus::Using)
            {
                oldest = sec.addr;
            }
            if sec.check_ok && sec.dirty == SectorDirtyStatus::Gc {
                gc_pending = true;
            }
            prev = sec.store;
            addr = self.next_sector_addr(&sec, traversed);
            if addr == FAILED_ADDR {
                break;
            }
        }
        Ok((oldest, gc_pending))
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
        let lay = self.lay();
        let threshold = lay.sec_remain_threshold;
        // Fast path: the sector we are already appending to.
        if self.cur_sec.check_ok
            && self.cur_sec.store == SectorStoreStatus::Using
            && self.cur_sec.remain > kv_size + threshold
            && (self.cur_sec.dirty == SectorDirtyStatus::False
                || (self.cur_sec.dirty == SectorDirtyStatus::True && !self.gc_request))
        {
            return Ok(self.cur_sec.empty_kv);
        }
        let (empty, using, _) = self.sector_statistics()?;
        if using > 0 {
            if let Some(sec) = self.find_alloc_candidate(SectorStoreStatus::Using, kv_size)? {
                self.cur_sec = sec;
                return Ok(sec.empty_kv);
            }
        }
        if empty > 0 {
            if empty > GC_EMPTY_SEC_THRESHOLD || self.gc_request {
                if let Some(sec) = self.find_alloc_candidate(SectorStoreStatus::Empty, kv_size)? {
                    self.cur_sec = sec;
                    return Ok(sec.empty_kv);
                }
            } else {
                self.gc_request = true;
            }
        }
        Ok(FAILED_ADDR)
    }

    fn find_alloc_candidate(
        &mut self,
        status: SectorStoreStatus,
        kv_size: u32,
    ) -> Result<Option<SecInfo>, F::Error> {
        let lay = self.lay();
        let threshold = lay.sec_remain_threshold;
        let sec_size = self.storage.sec_size();
        let mut addr = self.storage.oldest_addr;
        let mut traversed = 0u32;
        loop {
            traversed += sec_size;
            let sec = self.read_sector_info(addr, false)?;
            if sec.check_ok && sec.store == status {
                let sec = self.read_sector_info(addr, true)?;
                if sec.remain > kv_size + threshold
                    && (sec.dirty == SectorDirtyStatus::False
                        || (sec.dirty == SectorDirtyStatus::True && !self.gc_request))
                {
                    return Ok(Some(sec));
                }
            }
            addr = self.next_sector_addr(&sec, traversed);
            if addr == FAILED_ADDR {
                break;
            }
        }
        Ok(None)
    }

    /// Count EMPTY/USING sectors. Also returns the address of the last EMPTY
    /// sector in scan order, which GC needs for C's stop-check heuristic.
    fn sector_statistics(&mut self) -> Result<(u32, u32, u32), F::Error> {
        let sec_size = self.storage.sec_size();
        let mut empty = 0u32;
        let mut using = 0u32;
        let mut last_empty = 0u32;
        let mut addr = self.storage.oldest_addr;
        let mut traversed = 0u32;
        loop {
            traversed += sec_size;
            let sec = self.read_sector_info(addr, false)?;
            if sec.check_ok {
                match sec.store {
                    SectorStoreStatus::Empty => {
                        empty += 1;
                        last_empty = sec.addr;
                    }
                    SectorStoreStatus::Using => using += 1,
                    _ => {}
                }
            }
            addr = self.next_sector_addr(&sec, traversed);
            if addr == FAILED_ADDR {
                break;
            }
        }
        Ok((empty, using, last_empty))
    }

    /// Move a sector's store status forward; returns whether the sector became
    /// full because of this record (`update_sec_status()` in C).
    fn update_sec_status(&mut self, sec: &mut SecInfo, new_kv_len: u32) -> Result<bool, F::Error> {
        let lay = self.lay();
        let mut is_full = false;
        match sec.store {
            SectorStoreStatus::Empty => {
                self.storage.write_status(
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
                    self.storage.write_status(
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
    fn create_kv_blob(&mut self, key: &[u8], value: &[u8]) -> Result<(), F::Error> {
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
        let mut sec = self.cur_sec;
        let is_full = self.update_sec_status(&mut sec, len)?;
        self.cur_sec = sec;

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
        self.storage
            .write_status(addr, KV_STATUS_NUM, KvStatus::Write as usize)?;

        self.cur_sec.empty_kv = addr + len;
        self.cur_sec.remain = self.cur_sec.remain.saturating_sub(len);
        if is_full {
            self.gc_request = true;
        }
        self.cache_insert(hash_key(key), addr);
        Ok(())
    }

    /// Delete (or pre-delete) a record and mark the sector dirty.
    fn del_kv_addr(
        &mut self,
        addr: u32,
        key: Option<&[u8]>,
        complete: bool,
    ) -> Result<(), F::Error> {
        let lay = self.lay();
        let status = if complete {
            KvStatus::Deleted
        } else {
            KvStatus::PreDelete
        };
        self.storage
            .write_status(addr, KV_STATUS_NUM, status as usize)?;
        if complete {
            if !self.last_is_complete_del {
                if let Some(key) = key {
                    self.cache_invalidate(hash_key(key));
                }
            }
            self.last_is_complete_del = false;
        } else {
            self.last_is_complete_del = true;
        }
        // Mark the sector dirty (only the first deletion costs a write).
        let sec_size = self.storage.sec_size();
        let sec_addr = addr - addr % sec_size;
        // Fast path: we already know the currently active sector is dirty.
        let dirty = if self.cur_sec.addr == sec_addr
            && self.cur_sec.check_ok
            && self.cur_sec.dirty == SectorDirtyStatus::True
        {
            SectorDirtyStatus::True as usize
        } else {
            self.storage
                .read_status(sec_addr + lay.sec_dirty_off, SEC_STATUS_NUM)?
        };
        if dirty == SectorDirtyStatus::False as usize {
            self.storage.write_status(
                sec_addr + lay.sec_dirty_off,
                SEC_STATUS_NUM,
                SectorDirtyStatus::True as usize,
            )?;
            if self.cur_sec.addr == sec_addr {
                self.cur_sec.dirty = SectorDirtyStatus::True;
            }
        }
        Ok(())
    }

    // -- lookup -------------------------------------------------------------

    /// Look up a record by key, without verifying the value CRC.
    ///
    /// This reads only the record header and key name. Use
    /// [`KvdbImpl::read_value`] to read the value (which verifies its CRC32).
    pub fn find(&mut self, key: &[u8]) -> Result<Option<KvEntry>, F::Error> {
        if key.len() > KV_NAME_MAX {
            return Ok(None);
        }
        let hash = hash_key(key);
        if let Some(addr) = self.cache_lookup(hash) {
            let hdr = self.read_kv_header(addr)?;
            if hdr.valid && hdr.status == KvStatus::Write && hdr.name_len as usize == key.len() {
                let mut entry = self.entry_from_header(&hdr);
                self.read_name_into(&hdr, &mut entry.name)?;
                if &entry.name[..key.len()] == key {
                    return Ok(Some(entry));
                }
            }
            // Stale cache entry: drop it and scan.
            self.cache_invalidate(hash);
        }
        let found = self.find_scan(key)?;
        if let Some(entry) = found.as_ref() {
            self.cache_insert(hash, entry.addr);
        }
        Ok(found)
    }

    fn find_scan(&mut self, key: &[u8]) -> Result<Option<KvEntry>, F::Error> {
        let lay = self.lay();
        let sec_size = self.storage.sec_size();
        let mut addr = self.storage.oldest_addr;
        let mut traversed = 0u32;
        loop {
            traversed += sec_size;
            let sec = self.read_sector_info(addr, false)?;
            if sec.check_ok
                && (sec.store == SectorStoreStatus::Using || sec.store == SectorStoreStatus::Full)
            {
                let mut kv_addr = sec.addr + lay.sec_hdr_size;
                loop {
                    let hdr = self.read_kv_header(kv_addr)?;
                    if hdr.valid
                        && hdr.status == KvStatus::Write
                        && hdr.name_len as usize == key.len()
                    {
                        let mut entry = self.entry_from_header(&hdr);
                        self.read_name_into(&hdr, &mut entry.name)?;
                        if &entry.name[..key.len()] == key {
                            return Ok(Some(entry));
                        }
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
        Ok(None)
    }

    /// Verify a record's CRC and copy up to `buf.len()` value bytes.
    fn read_value_verified(
        &mut self,
        entry: &KvEntry,
        expected_key: Option<&[u8]>,
        buf: &mut [u8],
    ) -> Result<usize, F::Error> {
        let lay = self.lay();
        let wg = self.wg();
        let name_len = entry.name_len as usize;
        let value_len = entry.value_len as usize;
        let aligned_name = wg_align(name_len, wg);
        let aligned_value = wg_align(value_len, wg);

        let mut crc = crc_name_len(0, entry.name_len);
        crc = crc32_u32(crc, entry.value_len);

        let mut name_buf = [0u8; KV_NAME_MAX];
        self.storage
            .read(entry.addr + lay.kv_hdr_size, &mut name_buf[..aligned_name])?;
        crc = crc32(crc, &name_buf[..aligned_name]);
        if let Some(key) = expected_key {
            if &name_buf[..name_len] != key {
                return Err(Error::ReadCorrupted);
            }
        }

        let value_addr = entry.value_addr;
        let copy_len = core::cmp::min(buf.len(), value_len);
        if buf.len() >= value_len {
            // Single flash read of the whole value, CRC computed in place.
            if value_len > 0 {
                self.storage.read(value_addr, &mut buf[..value_len])?;
                crc = crc32(crc, &buf[..value_len]);
            }
        } else {
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
        }
        if aligned_value > value_len {
            let padding = aligned_value - value_len;
            let mut pad = [0u8; 32];
            self.storage
                .read(value_addr + value_len as u32, &mut pad[..padding])?;
            crc = crc32(crc, &pad[..padding]);
        }
        if crc != entry.crc {
            return Err(Error::ReadCorrupted);
        }
        Ok(copy_len)
    }

    /// Get a value into `buf`; returns the number of bytes copied.
    ///
    /// `Ok(None)` means the key does not exist. If `buf` is smaller than the
    /// value, the value is truncated (use [`KvdbImpl::find`] for its length).
    /// The value's CRC32 is always verified before any byte is returned.
    pub fn get(&mut self, key: &[u8], buf: &mut [u8]) -> Result<Option<usize>, F::Error> {
        let Some(entry) = self.find(key)? else {
            return Ok(None);
        };
        let n = self.read_value_verified(&entry, Some(key), buf)?;
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
        self.read_value_verified(entry, None, buf)
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

        // 2. Find the old record (if any) and mark it pre-deleted.
        let old = self.find(key)?;
        if let Some(old) = old.as_ref() {
            self.del_kv_addr(old.addr, Some(key), false)?;
        }

        // 3. Write the new record.
        let result = self.create_kv_blob(key, value);

        // 4. Finalise deletion of the old record once the new one is committed.
        if let Some(old) = old.as_ref() {
            if result.is_ok() {
                self.del_kv_addr(old.addr, Some(key), true)?;
            }
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
        let Some(entry) = self.find(key)? else {
            return Ok(false);
        };
        self.del_kv_addr(entry.addr, Some(key), true)?;
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
        self.storage.write_status(
            sec.addr + lay.sec_dirty_off,
            SEC_STATUS_NUM,
            SectorDirtyStatus::Gc as usize,
        )?;
        let mut kv_addr = sec.addr + lay.sec_hdr_size;
        loop {
            let hdr = self.read_kv_header(kv_addr)?;
            if hdr.valid && (hdr.status == KvStatus::Write || hdr.status == KvStatus::PreDelete) {
                let name = self.read_name(&hdr)?;
                self.move_kv(&hdr, &name[..hdr.name_len as usize])?;
            }
            let next = self.advance_kv(sec.addr + self.storage.sec_size(), &hdr)?;
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
                let last = self.read_sector_info(previous, true)?;
                if last.check_ok {
                    last.remain
                } else {
                    0
                }
            }
        };
        // A freshly formatted sector always has (sec_size - header) free bytes.
        *last_gc_remain = Some(self.storage.sec_size() - lay.sec_hdr_size);
        Ok(previous_remain > free_size)
    }

    /// Move one live record to a new location (used by GC and recovery).
    fn move_kv(&mut self, hdr: &KvHeader, name: &[u8]) -> Result<(), F::Error> {
        let lay = self.lay();
        if hdr.status == KvStatus::Write {
            self.del_kv_addr(hdr.addr, Some(name), false)?;
        }
        let new_addr = self.alloc_kv(hdr.len)?;
        if new_addr == FAILED_ADDR {
            return Err(Error::SavedFull);
        }
        // During recovery a PRE_DELETE record may already have been recreated.
        if self.in_recovery && hdr.status == KvStatus::PreDelete {
            if let Some(existing) = self.find_scan(name)? {
                if existing.addr != hdr.addr {
                    self.del_kv_addr(hdr.addr, Some(name), true)?;
                    return Ok(());
                }
            }
        }
        let mut sec = self.cur_sec;
        self.update_sec_status(&mut sec, hdr.len)?;
        self.cur_sec = sec;
        self.storage
            .write_status(new_addr, KV_STATUS_NUM, KvStatus::PreWrite as usize)?;
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
        self.storage
            .write_status(new_addr, KV_STATUS_NUM, KvStatus::Write as usize)?;
        self.cur_sec.empty_kv = new_addr + hdr.len;
        self.cur_sec.remain = self.cur_sec.remain.saturating_sub(hdr.len);
        self.cache_insert(hash_key(name), new_addr);
        self.del_kv_addr(hdr.addr, Some(name), true)?;
        Ok(())
    }

    fn gc_collect_by_free_size(&mut self, free_size: u32) -> Result<(), F::Error> {
        let (empty, _, last_empty) = self.sector_statistics()?;
        if empty <= GC_EMPTY_SEC_THRESHOLD {
            let sec_size = self.storage.sec_size();
            // C's `gc_check_cb` records the last EMPTY sector address in scan
            // order and passes it as `gc->last_gc_sec_addr`. The first do_gc()
            // stop-check reads *that* sector's remain.
            let mut last_gc_addr = last_empty;
            let mut last_gc_remain: Option<u32> = None;
            let mut addr = self.storage.oldest_addr;
            let mut traversed = 0u32;
            loop {
                traversed += sec_size;
                let sec = self.read_sector_info(addr, false)?;
                if sec.check_ok
                    && (sec.dirty == SectorDirtyStatus::True
                        || sec.dirty == SectorDirtyStatus::Gc)
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
        let mut addr = self.storage.oldest_addr;
        let mut traversed = 0u32;
        loop {
            traversed += sec_size;
            let sec = self.read_sector_info(addr, false)?;
            if sec.check_ok
                && (sec.store == SectorStoreStatus::Using || sec.store == SectorStoreStatus::Full)
            {
                let mut kv_addr = sec.addr + lay.sec_hdr_size;
                loop {
                    let hdr = self.read_kv_header(kv_addr)?;
                    match hdr.status {
                        KvStatus::PreDelete if hdr.valid => {
                            let name = self.read_name(&hdr)?;
                            let name_len = hdr.name_len as usize;
                            self.move_kv(&hdr, &name[..name_len])?;
                        }
                        KvStatus::PreWrite => {
                            self.storage.write_status(
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

        // Pass 1: count damaged sector headers.
        let sec_size = self.storage.sec_size();
        let mut failed = 0u32;
        let mut addr = 0u32;
        let mut traversed = 0u32;
        loop {
            traversed += sec_size;
            let sec = self.read_sector_info(addr, false)?;
            if !sec.check_ok {
                failed += 1;
            }
            addr = self.next_sector_addr(&sec, traversed);
            if addr == FAILED_ADDR {
                break;
            }
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
            let mut traversed = 0u32;
            loop {
                traversed += sec_size;
                let sec = self.read_sector_info(addr, false)?;
                if !sec.check_ok {
                    self.format_sector(addr, SECTOR_NOT_COMBINED)?;
                }
                addr = self.next_sector_addr(&sec, traversed);
                if addr == FAILED_ADDR {
                    break;
                }
            }
        }

        let (oldest, gc_pending) = self.inspect_sectors()?;
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
        let mut addr = self.storage.oldest_addr;
        let mut traversed = 0u32;
        loop {
            traversed += sec_size;
            let sec = self.read_sector_info(addr, false)?;
            if sec.check_ok
                && (sec.store == SectorStoreStatus::Using || sec.store == SectorStoreStatus::Full)
            {
                let mut kv_addr = sec.addr + lay.sec_hdr_size;
                loop {
                    let hdr = self.read_kv_header(kv_addr)?;
                    if !hdr.valid {
                        // The record area ends at the first fully erased header.
                        if hdr.erased {
                            break;
                        }
                        return Err(Error::ReadCorrupted);
                    }
                    if hdr.status == KvStatus::Write || hdr.status == KvStatus::PreDelete {
                        let name = self.read_name(&hdr)?;
                        let mut entry = self.entry_from_header(&hdr);
                        entry.name = name;
                        let mut scratch = [0u8; 8];
                        self.read_value_verified(&entry, None, &mut scratch)?;
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
        self.db.read_value_verified(entry, None, buf)
    }
}

impl<'a, F: Flash, const CACHE: usize> Iterator for KvIterator<'a, F, CACHE> {
    type Item = Result<KvEntry, F::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let lay = self.db.lay();
        let sec_size = self.db.storage.sec_size();
        loop {
            if !self.in_sector {
                if self.sector_addr == FAILED_ADDR {
                    self.done = true;
                    return None;
                }
                self.traversed_len += sec_size;
                let sec = match self.db.read_sector_info(self.sector_addr, false) {
                    Ok(sec) => sec,
                    Err(e) => {
                        self.done = true;
                        return Some(Err(e));
                    }
                };
                let next_sec = self.db.next_sector_addr(&sec, self.traversed_len);
                self.sector_addr = next_sec;
                if sec.check_ok
                    && (sec.store == SectorStoreStatus::Using
                        || sec.store == SectorStoreStatus::Full)
                {
                    self.sec_end = sec.addr + sec_size;
                    self.next_addr = sec.addr + lay.sec_hdr_size;
                    self.in_sector = true;
                } else {
                    continue;
                }
            }
            if self.next_addr == FAILED_ADDR {
                self.in_sector = false;
                continue;
            }
            let hdr = match self.db.read_kv_header(self.next_addr) {
                Ok(hdr) => hdr,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };
            let next = match self.db.advance_kv(self.sec_end, &hdr) {
                Ok(next) => next,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };
            self.next_addr = next;
            if hdr.valid && hdr.status == KvStatus::Write {
                let name = match self.db.read_name(&hdr) {
                    Ok(name) => name,
                    Err(e) => {
                        self.done = true;
                        return Some(Err(e));
                    }
                };
                let mut entry = self.db.entry_from_header(&hdr);
                entry.name = name;
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
