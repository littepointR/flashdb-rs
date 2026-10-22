//! Time-series log database (TSDB).
//!
//! A TSDB stores timestamped blobs ("TSL" in FlashDB terminology) in a
//! circular set of flash sectors. Each sector is written top-down with index
//! records and bottom-up with payloads, until they meet; then the sector is
//! sealed, two "end info" records are written into its header for fast range
//! queries, and writing continues in the next sector (wrapping around when
//! rollover is enabled).
//!
//! The on-flash format is byte-compatible with FlashDB-C (see [`crate::low`]).
//!
//! ## API shape
//!
//! Unlike the C API's `void fdb_tsl_iter(db, cb, arg)` callbacks, iteration is
//! expressed as an `Iterator` over lightweight [`TslEntry`] values, with the
//! iterator itself able to read the blob payload:
//!
//! ```no_run
//! # use flashdb::{Config, Flash, Tsdb, TslStatus};
//! # fn demo<F: Flash>(flash: F) -> Result<(), flashdb::Error<F::Error>> {
//! let mut db: Tsdb<F> = Tsdb::mount(flash, Config::new(4096, 16384), 256)?;
//! db.append(1000, b"hello")?;
//!
//! let mut payload = [0u8; 64];
//! let mut it = db.iter();
//! while let Some(entry) = it.next() {
//!     let entry = entry?;
//!     if entry.status == TslStatus::Write {
//!         let n = it.read_blob(&entry, &mut payload)?;
//!         let _ = &payload[..n];
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Timestamp width
//!
//! FlashDB has a compile-time switch `FDB_USING_TIMESTAMP_64BIT` that changes
//! the on-flash layout. Here the width is a type parameter of [`TsdbImpl`]:
//! [`Tsdb`] is the 32-bit default (`i32`, FlashDB's default format) and
//! [`Tsdb64`] is the 64-bit format (`i64`, `FDB_USING_TIMESTAMP_64BIT`). A
//! database must always be mounted with the same timestamp type it was written
//! with.
//!
//! ## Recovery
//!
//! Mounting after a power failure is the delicate part. The module follows the
//! C implementation: partially written index records are visible as
//! [`TslStatus::PreWrite`] entries (reported with `log_len == max_len` and no
//! payload address, exactly like C's `read_tsl`), a sector whose end info was
//! interrupted is scanned during mount so its bookkeeping is rebuilt, and a
//! damaged sector header triggers a full re-format (all data is lost, as in C).
//! Unlike C, mount never asserts or hangs on an inconsistent end-info pair: it
//! falls back to scanning the sector instead.

use crate::error::{Error, Result};
use crate::flash::Flash;
use crate::low::{
    decode_status, get_u32, ts_layout_32, ts_layout_64, wg_align, SectorStoreStatus, TsLayout,
    TslStatus, FAILED_ADDR, TSDB_SECTOR_MAGIC,
};
use crate::storage::{Config, Storage};

/// `FDB_SECTOR_STORE_STATUS_NUM`.
const SECTOR_STORE_STATUS_NUM: usize = 4;
/// `FDB_TSL_STATUS_NUM`.
const TSL_STATUS_NUM: usize = 6;

/// Size of the largest write-granule aligned field block (256 bits).
const MAX_ALIGN: usize = 32;

/// Timestamp type stored in the TSDB.
///
/// FlashDB has a compile-time switch `FDB_USING_TIMESTAMP_64BIT`; here the
/// timestamp width is a type parameter instead: use [`i32`] for the default
/// FlashDB format (compatible with C builds without the 64-bit option) or
/// [`i64`] for 64-bit timestamps (`FDB_USING_TIMESTAMP_64BIT`).
///
/// The type parameter changes the on-flash layout, so a database must always be
/// mounted with the same timestamp type it was written with.
pub trait Timestamp: Copy + Ord + core::fmt::Debug + Default {
    /// Width in bits: 32 or 64.
    const WIDTH: u32;

    /// Convert to `i64` for range comparisons.
    fn to_i64(self) -> i64;

    /// Construct from `i64`, saturating at the type's bounds.
    fn from_i64(value: i64) -> Self;

    /// Write this timestamp little endian at `off`.
    fn write_le(self, buf: &mut [u8], off: usize);

    /// Read a little-endian timestamp at `off`.
    fn read_le(buf: &[u8], off: usize) -> Self;
}

impl Timestamp for i32 {
    const WIDTH: u32 = 32;

    #[inline(always)]
    fn to_i64(self) -> i64 {
        self as i64
    }

    #[inline(always)]
    fn from_i64(value: i64) -> Self {
        value.clamp(i32::MIN as i64, i32::MAX as i64) as i32
    }

    #[inline(always)]
    fn write_le(self, buf: &mut [u8], off: usize) {
        buf[off..off + 4].copy_from_slice(&self.to_le_bytes());
    }

    #[inline(always)]
    fn read_le(buf: &[u8], off: usize) -> Self {
        i32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
    }
}

impl Timestamp for i64 {
    const WIDTH: u32 = 64;

    #[inline(always)]
    fn to_i64(self) -> i64 {
        self
    }

    #[inline(always)]
    fn from_i64(value: i64) -> Self {
        value
    }

    #[inline(always)]
    fn write_le(self, buf: &mut [u8], off: usize) {
        buf[off..off + 8].copy_from_slice(&self.to_le_bytes());
    }

    #[inline(always)]
    fn read_le(buf: &[u8], off: usize) -> Self {
        let mut b = [0u8; 8];
        b.copy_from_slice(&buf[off..off + 8]);
        i64::from_le_bytes(b)
    }
}

/// One time-series log entry, as yielded by [`TslIterator`].
///
/// The value is a plain `Copy` struct: the payload itself is not read until
/// [`TslIterator::read_blob`] is called, so iterating over millions of entries
/// does not copy data that the caller may not need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TslEntry<T: Timestamp = i32> {
    /// Record status.
    pub status: TslStatus,
    /// Record timestamp.
    pub time: T,
    /// Payload length in bytes.
    pub log_len: u32,
    /// Address of the index record (used to change the status).
    pub index_addr: u32,
    /// Address of the payload (or [`FAILED_ADDR`] when not available).
    pub log_addr: u32,
}

/// Per-sector bookkeeping while traversing/using a sector.
#[derive(Debug, Clone, Copy)]
struct SecInfo<T: Timestamp> {
    addr: u32,
    start_time: T,
    end_time: T,
    end_idx: u32,
    remain: u32,
    empty_idx: u32,
    empty_data: u32,
    check_ok: bool,
    status: SectorStoreStatus,
    end_info_stat: [TslStatus; 2],
}

impl<T: Timestamp> Default for SecInfo<T> {
    fn default() -> Self {
        Self {
            addr: FAILED_ADDR,
            check_ok: false,
            status: SectorStoreStatus::Unused,
            start_time: T::default(),
            end_time: T::default(),
            end_idx: FAILED_ADDR,
            end_info_stat: [TslStatus::Unused; 2],
            remain: 0,
            empty_idx: 0,
            empty_data: 0,
        }
    }
}

/// A time-series database over a flash partition.
///
/// `T` selects the timestamp width of the on-flash format; use the [`Tsdb`]
/// alias for the 32-bit FlashDB default or [`Tsdb64`] for the
/// `FDB_USING_TIMESTAMP_64BIT` format.
///
/// See the [module documentation](self) for usage.
pub struct TsdbImpl<F: Flash, T: Timestamp> {
    storage: Storage<F>,
    cur_sec: SecInfo<T>,
    last_time: T,
    max_len: u32,
    rollover: bool,
}

/// Time-series database with the default 32-bit FlashDB timestamp format.
pub type Tsdb<F> = TsdbImpl<F, i32>;
/// Time-series database with 64-bit timestamps (`FDB_USING_TIMESTAMP_64BIT`).
pub type Tsdb64<F> = TsdbImpl<F, i64>;

/// Direction/range of a [`TslIterator`].
#[derive(Debug, Clone, Copy)]
enum IterMode<T: Timestamp> {
    /// Oldest to newest.
    Forward,
    /// Newest to oldest.
    Reverse,
    /// Only entries in `[from, to]`; when `from > to` iterate backwards from
    /// `from` down to `to`.
    ByTime { from: T, to: T },
}

/// Per-mode stepping function used by [`TslIterator::next`].
#[allow(type_alias_bounds)] // the bound is needed to name `F::Error`
type NextFn<'a, F: Flash, T> =
    for<'b> fn(&'b mut TslIterator<'a, F, T>) -> Option<Result<TslEntry<T>, F::Error>>;

/// Iterator over the entries of a [`Tsdb`].
///
/// Created by [`TsdbImpl::iter`], [`TsdbImpl::iter_reverse`] or
/// [`TsdbImpl::iter_by_time`]. Call [`TslIterator::read_blob`] to read the
/// payload of the entry that was just yielded.
pub struct TslIterator<'a, F: Flash, T: Timestamp = i32> {
    db: &'a mut TsdbImpl<F, T>,
    mode: IterMode<T>,
    /// Stepping function for this iterator's mode. Storing the function
    /// pointer (instead of a runtime `match` in `Iterator::next`) lets a
    /// `no_std` build drop the modes it never constructs.
    next: NextFn<'a, F, T>,
    /// Sector to read next (or [`FAILED_ADDR`] when there is none).
    sector_addr: u32,
    /// Sectors traversed so far, used to stop after a full round.
    traversed_len: u32,
    /// Next index address to read (forward) or the current one (reverse).
    next_index: u32,
    /// Whether the current sector still has entries to yield.
    in_sector: bool,
    /// By-time only: the start sector has been found.
    found_start: bool,
    /// Iteration finished (end of DB, out-of-range entry or I/O error).
    done: bool,
    /// First sector of the traversal, used by the by-time start condition.
    start_sector: u32,
    /// Address of the sector currently being iterated.
    sec_addr: u32,
    /// Index address of the last record in the current sector.
    sec_end_idx: u32,
    /// First/last index address that can hold a record in `sec_addr`, cached
    /// per sector so the per-entry read does not recompute them.
    sec_first: u32,
    sec_last: u32,
}

// ---------------------------------------------------------------------------
// Layout and small helpers
// ---------------------------------------------------------------------------

/// Resolve the on-flash layout for the flash's write granularity and the
/// database's timestamp width.
#[inline]
const fn ts_layout<F: Flash, T: Timestamp>() -> Option<TsLayout> {
    if T::WIDTH == 64 {
        ts_layout_64(F::WRITE_GRAN_BITS)
    } else {
        ts_layout_32(F::WRITE_GRAN_BITS)
    }
}

/// Store a `u32` little endian at `off`.
///
/// This is `low::put_u32` with a forced inline: the offsets are constants after
/// monomorphisation, so the calls in the append hot path fold into plain stores
/// (the out-of-line `low::put_u32` is also used by the KVDB, so it stays).
#[inline(always)]
fn put_u32_at(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

/// Placeholder entry used as the out-parameter slot of the decode path.
#[inline]
fn empty_entry<T: Timestamp>() -> TslEntry<T> {
    TslEntry {
        status: TslStatus::Unused,
        time: T::default(),
        log_len: 0,
        index_addr: 0,
        log_addr: FAILED_ADDR,
    }
}

/// Status table index of a sector store status.
#[inline]
const fn sector_store_status_index(status: SectorStoreStatus) -> usize {
    match status {
        SectorStoreStatus::Unused => 0,
        SectorStoreStatus::Empty => 1,
        SectorStoreStatus::Using => 2,
        SectorStoreStatus::Full => 3,
    }
}

/// Status table index of a TSL status.
#[inline]
const fn tsl_status_index(status: TslStatus) -> usize {
    match status {
        TslStatus::Unused => 0,
        TslStatus::PreWrite => 1,
        TslStatus::Write => 2,
        TslStatus::UserStatus1 => 3,
        TslStatus::Deleted => 4,
        TslStatus::UserStatus2 => 5,
    }
}

/// `FDB_ALIGN(size, align)`; `align` is not required to be a power of two
/// (FlashDB uses this with `LOG_IDX_DATA_SIZE`, which is 20 bytes for
/// write-granularity 8).
#[inline]
const fn align_to(size: u32, align: u32) -> u32 {
    if align == 0 {
        size
    } else {
        ((size + align - 1) / align) * align
    }
}

/// `get_next_sector_addr()`: the next sector in write order, `None` when a
/// full round over the partition has been traversed.
#[inline]
fn next_sector_addr(
    sec_addr: u32,
    traversed_len: u32,
    sec_size: u32,
    max_size: u32,
) -> Option<u32> {
    if traversed_len + sec_size <= max_size {
        if sec_addr + sec_size < max_size {
            Some(sec_addr + sec_size)
        } else {
            /* the next sector is on the top of the database */
            Some(0)
        }
    } else {
        None
    }
}

/// `get_last_sector_addr()`: the previous sector in write order, `None` when a
/// full round over the partition has been traversed.
#[inline]
fn prev_sector_addr(
    sec_addr: u32,
    traversed_len: u32,
    sec_size: u32,
    max_size: u32,
) -> Option<u32> {
    if traversed_len + sec_size <= max_size {
        if sec_addr >= sec_size {
            Some(sec_addr - sec_size)
        } else {
            Some(max_size - sec_size)
        }
    } else {
        None
    }
}

/// Address of the sector following `addr`, wrapping to the database start.
///
/// The three "next sector after this one" computations (current sector
/// advance, oldest sector after a seal, oldest sector at mount) share it.
#[inline]
const fn wrap_next(addr: u32, sec_size: u32, max_size: u32) -> u32 {
    if addr + sec_size < max_size {
        addr + sec_size
    } else {
        0
    }
}

/// `get_next_tsl_addr()`: next index record inside a sector that has entries.
#[inline]
fn next_tsl_addr(pre_index: u32, end_idx: u32, log_idx_size: u32) -> Option<u32> {
    if pre_index + log_idx_size <= end_idx {
        Some(pre_index + log_idx_size)
    } else {
        None
    }
}

/// `get_last_tsl_addr()`: previous index record inside a sector that has entries.
#[inline]
fn prev_tsl_addr(
    pre_index: u32,
    sec_addr: u32,
    log_idx_size: u32,
    sec_hdr_size: u32,
) -> Option<u32> {
    if pre_index >= sec_addr + sec_hdr_size + log_idx_size {
        Some(pre_index - log_idx_size)
    } else {
        None
    }
}

/// Whether the sector is known to contain at least one index record.
#[inline]
fn sector_has_entries<T: Timestamp>(sec: &SecInfo<T>, sec_hdr_size: u32) -> bool {
    sec.end_idx != FAILED_ADDR && sec.end_idx >= sec.addr + sec_hdr_size
}

/// Decode a TSDB sector header from its raw bytes into `out`
/// (`read_sector_info`).
///
/// Decoding into a caller-provided slot instead of returning `SecInfo<T>` by
/// value keeps the ~32 B result out of the return-value/temporary sequence.
#[inline]
fn decode_sector_header<T: Timestamp>(
    addr: u32,
    raw: &[u8],
    layout: &TsLayout,
    wg: u32,
    out: &mut SecInfo<T>,
) {
    *out = SecInfo {
        addr,
        ..Default::default()
    };
    if get_u32(raw, layout.sec_magic_off as usize) != TSDB_SECTOR_MAGIC {
        /* invalid magic: check_ok stays false */
        return;
    }
    out.check_ok = true;
    out.status = SectorStoreStatus::from_index(decode_status(
        wg,
        SECTOR_STORE_STATUS_NUM,
        &raw[layout.sec_store_off as usize..],
    ));
    out.start_time = T::read_le(raw, layout.sec_start_time_off as usize);
    out.end_info_stat[0] = TslStatus::from_index(decode_status(
        wg,
        TSL_STATUS_NUM,
        &raw[layout.sec_end0_stat_off as usize..],
    ));
    if out.end_info_stat[0] == TslStatus::Write {
        out.end_time = T::read_le(raw, layout.sec_end0_time_off as usize);
        out.end_idx = get_u32(raw, layout.sec_end0_idx_off as usize);
        /* end_info[1] is only the recovery fallback slot for an interrupted
         * end_info[0] write; a sector is always formatted before being reused,
         * so both slots can never hold a valid end node at the same time. */
        out.end_info_stat[1] = TslStatus::Unused;
    } else {
        out.end_info_stat[1] = TslStatus::from_index(decode_status(
            wg,
            TSL_STATUS_NUM,
            &raw[layout.sec_end1_stat_off as usize..],
        ));
        if out.end_info_stat[1] == TslStatus::Write {
            out.end_time = T::read_le(raw, layout.sec_end1_time_off as usize);
            out.end_idx = get_u32(raw, layout.sec_end1_idx_off as usize);
        }
    }
}

/// Decode one index record from its raw bytes into `out` (`read_tsl`).
#[inline]
fn decode_tsl<T: Timestamp>(
    index_addr: u32,
    raw: &[u8],
    layout: &TsLayout,
    wg: u32,
    max_len: u32,
    out: &mut TslEntry<T>,
) {
    let status = TslStatus::from_index(decode_status(wg, TSL_STATUS_NUM, raw));
    out.status = status;
    out.index_addr = index_addr;
    if status == TslStatus::PreWrite || status == TslStatus::Unused {
        /* An index record without valid metadata: report the worst case payload
         * size and no payload address, exactly like C. */
        out.log_len = max_len;
        out.log_addr = FAILED_ADDR;
        out.time = T::default();
    } else {
        out.log_len = get_u32(raw, layout.log_idx_loglen_off as usize);
        out.log_addr = get_u32(raw, layout.log_idx_logaddr_off as usize);
        out.time = T::read_le(raw, layout.log_idx_ts_off as usize);
    }
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

impl<F: Flash, T: Timestamp> TsdbImpl<F, T> {
    /// Mount (open) a TSDB, recovering interrupted writes if needed.
    ///
    /// * `max_len` is the maximum payload size of one log entry; larger
    ///   appends are rejected. It must be smaller than the sector size.
    ///
    /// If no valid sector header is found the database is formatted and
    /// mounted empty (unless a previous instance marked it non-formattable,
    /// which this API does not expose yet).
    pub fn mount(flash: F, config: Config, max_len: u32) -> Result<Self, F::Error> {
        let storage = Storage::new(flash, config)?;
        let mut db = Self {
            storage,
            cur_sec: SecInfo::default(),
            last_time: T::default(),
            max_len,
            rollover: true,
        };
        db.mount_inner()?;
        Ok(db)
    }

    /// Placement form of [`TsdbImpl::mount`]: initialize `slot` in place.
    ///
    /// This lets a database live in a `static` without ever building a whole
    /// `TsdbImpl` on the stack (FlashDB-C's `fdb_tsdb_init` takes a pointer in
    /// the same way). On success `slot` is fully initialized; on error the
    /// slot must not be used (mirrors `KvdbImpl::mount_with_defaults_into`).
    ///
    /// ```
    /// # use flashdb::{Config, Flash, Tsdb};
    /// # fn demo<F: Flash>(flash: F) -> Result<(), flashdb::Error<F::Error>> {
    /// let mut slot = core::mem::MaybeUninit::<Tsdb<F>>::uninit();
    /// Tsdb::mount_into(&mut slot, flash, Config::new(4096, 16384), 256)?;
    /// // SAFETY: mount_into succeeded, so `slot` is initialized.
    /// let db = unsafe { slot.assume_init_mut() };
    /// db.append(1000, b"hello")?;
    /// # Ok(())
    /// # }
    /// ```
    #[allow(unsafe_code)]
    pub fn mount_into(
        slot: &mut core::mem::MaybeUninit<Self>,
        flash: F,
        config: Config,
        max_len: u32,
    ) -> Result<(), F::Error> {
        let storage = Storage::new(flash, config)?;
        // SAFETY: `slot` is valid, aligned, writable memory for `Self` and no
        // reference to the (not yet initialized) value is created: every field
        // is initialized exactly once through `addr_of_mut!` and `.write()`.
        // `assume_init_mut` is only called after all fields are initialized.
        let db = unsafe {
            let p = slot.as_mut_ptr();
            core::ptr::addr_of_mut!((*p).storage).write(storage);
            core::ptr::addr_of_mut!((*p).cur_sec).write(SecInfo::default());
            core::ptr::addr_of_mut!((*p).last_time).write(T::default());
            core::ptr::addr_of_mut!((*p).max_len).write(max_len);
            core::ptr::addr_of_mut!((*p).rollover).write(true);
            slot.assume_init_mut()
        };
        db.mount_inner()
    }

    /// On-flash layout for this database, resolved from the compile-time flash
    /// write granularity and timestamp width (`FDB_WRITE_GRAN`,
    /// `FDB_USING_TIMESTAMP_64BIT`) so that all field offsets fold to constants.
    ///
    /// Associated function: the layout only depends on the type parameters, so
    /// the read/decode helpers can use it without a `&self` borrow (which would
    /// block the split borrows the hot paths need).
    #[inline(always)]
    fn layout() -> TsLayout {
        match ts_layout::<F, T>() {
            Some(layout) => layout,
            None => unreachable!(),
        }
    }

    /// Shared mount/recovery logic used by [`TsdbImpl::mount`] and
    /// [`TsdbImpl::mount_into`] (`fdb_tsdb_init`).
    fn mount_inner(&mut self) -> Result<(), F::Error> {
        if ts_layout::<F, T>().is_none() {
            return Err(Error::UnsupportedWriteGran);
        }
        let sec_size = self.storage.sec_size();
        let max_size = self.storage.max_size();
        let max_len = self.max_len;
        if max_len >= sec_size {
            /* FlashDB asserts max_len < sec_size. */
            return Err(Error::InvalidConfig);
        }

        /* Check all sector headers (fdb_tsdb_init + check_sec_hdr_cb). */
        let mut check_failed = false;
        /* Address of the USING/first-EMPTY sector chosen as current, or
         * FAILED_ADDR when there is none yet. */
        let mut cur_addr = FAILED_ADDR;
        let mut empty_num = 0usize;
        let mut empty_addr = 0u32;
        /* Newest FULL sector (largest end timestamp). Only used when a
         * database without rollover is mounted: C then guesses the last
         * physical sector, which is wrong after an interrupted wrap. */
        let mut latest_full_addr = FAILED_ADDR;
        let mut latest_full_time = T::default();
        /* Address whose decoded header is currently held in `cur_sec` because
         * the check loop visited it last (see below); FAILED_ADDR when unset. */
        let mut cached_cur_addr = FAILED_ADDR;
        let mut addr = 0u32;
        while addr < max_size {
            /* sector_iterator() reads the header once, with traversal, when the
             * status filter matches; reading it without traversal first (as the
             * C callback pair effectively does) has no observable effect, so it
             * is not repeated here. */
            let mut sec = SecInfo::<T>::default();
            Self::read_sec_into(&mut self.storage, max_len, addr, true, &mut sec)?;
            if !sec.check_ok {
                check_failed = true;
                break;
            } else if sec.status == SectorStoreStatus::Using {
                if cur_addr == FAILED_ADDR {
                    cur_addr = sec.addr;
                } else {
                    /* multiple sectors are in use: the DB is damaged */
                    check_failed = true;
                    break;
                }
            } else if sec.status == SectorStoreStatus::Empty {
                empty_num += 1;
                empty_addr = sec.addr;
                if empty_num == 1 && cur_addr == FAILED_ADDR {
                    cur_addr = sec.addr;
                }
            } else if sec.status == SectorStoreStatus::Full && sec.end_time > latest_full_time {
                latest_full_time = sec.end_time;
                latest_full_addr = sec.addr;
            }
            if sec.addr == max_size - sec_size && cur_addr == FAILED_ADDR {
                /* The last sector read by the check loop is always the last
                 * physical sector. Keep its decoded header (in cur_sec, which is
                 * still unset here) so that the final read below can reuse it:
                 * the common fully populated, rollover-disabled case saves one
                 * flash read. No write happens in between, so both reads would
                 * be identical. */
                self.cur_sec = sec;
                cached_cur_addr = sec.addr;
                /* not the current sector: only its header is cached */
            }
            addr += sec_size;
        }

        if check_failed {
            /* Headers are damaged: re-format the whole database. */
            self.format_all()?;
            /* fdb_tsdb_init reads the current sector once more after formatting. */
            Self::read_sec_into(&mut self.storage, max_len, 0, true, &mut self.cur_sec)?;
        } else {
            let mut cur_addr = cur_addr;
            let latest_addr = if empty_num > 0 {
                empty_addr
            } else if cur_addr == FAILED_ADDR {
                /* Every sector is full (rollover disabled, or a power failure
                 * right at the wrap). C reads an invalid current sector here.
                 * Use the sector with the newest end info, falling back to the
                 * last physical sector. */
                if latest_full_addr != FAILED_ADDR {
                    latest_full_addr
                } else {
                    max_size - sec_size
                }
            } else if self.rollover {
                cur_addr
            } else {
                /* There is no empty sector. */
                cur_addr = max_size - sec_size;
                cur_addr
            };
            if cur_addr == FAILED_ADDR {
                cur_addr = latest_addr;
            }
            /* db->cur_sec is the latest sector, the next one is the oldest. */
            self.storage.oldest_addr = wrap_next(latest_addr, sec_size, max_size);

            /* Reuse the header already decoded by the check loop (see above). */
            if cached_cur_addr != cur_addr {
                Self::read_sec_into(
                    &mut self.storage,
                    max_len,
                    cur_addr,
                    true,
                    &mut self.cur_sec,
                )?;
            }
            /* Recover the last saved timestamp (fdb_tsdb_init). */
            match self.cur_sec.status {
                SectorStoreStatus::Using => {
                    self.last_time = self.cur_sec.end_time;
                    if self.last_time == T::default() {
                        /* Power loss right after the sector was opened: there
                         * is no committed record in it, so take the end
                         * timestamp of the previous (sealed) sector. C reads an
                         * uninitialized end_time in this situation. */
                        self.last_time = self.read_previous_sector_end(cur_addr)?;
                    }
                }
                SectorStoreStatus::Full => {
                    /* Every sector is full (rollover disabled). C leaves
                     * last_time at 0 here; the end info is available, so use
                     * it instead of reporting a meaningless timestamp. */
                    self.last_time = self.cur_sec.end_time;
                }
                SectorStoreStatus::Empty => {
                    if self.storage.oldest_addr != self.cur_sec.addr {
                        self.last_time = self.read_previous_sector_end(self.cur_sec.addr)?;
                    }
                }
                SectorStoreStatus::Unused => {}
            }
        }

        Ok(())
    }

    /// Append a log entry with an explicit timestamp.
    ///
    /// The timestamp must be strictly greater than the previous one and the
    /// payload must not exceed [`TsdbImpl::max_len`]; otherwise the record is
    /// rejected with [`Error::InvalidConfig`] (FlashDB returns
    /// `FDB_WRITE_ERR`). [`Error::SavedFull`] means the database is full.
    pub fn append(&mut self, time: T, data: &[u8]) -> Result<(), F::Error> {
        /* check the append length, MUST be less than db->max_len */
        if data.len() > self.max_len as usize {
            return Err(Error::InvalidConfig);
        }
        /* check the current timestamp, MUST be more than the last saved one */
        if time <= self.last_time {
            return Err(Error::InvalidConfig);
        }

        let layout = Self::layout();
        let aligned = wg_align(data.len(), F::WRITE_GRAN_BITS) as u32;
        let need = layout.log_idx_size + aligned;

        /* Fast path: the current sector is USING and has room for the record.
         * Sealing/formatting a sector is the exception, so it lives in a cold
         * helper and keeps its sector copies out of this frame. */
        if self.cur_sec.status != SectorStoreStatus::Using || self.cur_sec.remain < need {
            self.prepare_sector(need, time)?;
        }

        let idx_addr = self.cur_sec.empty_idx;
        let log_addr = self.cur_sec.empty_data - aligned;
        self.write_tsl_at(idx_addr, log_addr, time, data)?;

        /* recalculate the current using sector info (in place: no sector copy) */
        let cur = &mut self.cur_sec;
        cur.end_idx = idx_addr;
        cur.end_time = time;
        cur.empty_idx = idx_addr.saturating_add(layout.log_idx_size);
        cur.empty_data = cur.empty_data.saturating_sub(aligned);
        cur.remain = cur.remain.saturating_sub(need);
        self.last_time = time;

        Ok(())
    }

    /// Append a log entry using the database's last timestamp + 1.
    ///
    /// This is a convenience for tests and for devices without an RTC; prefer
    /// [`TsdbImpl::append`] with a real timestamp in production.
    pub fn append_now(&mut self, data: &[u8]) -> Result<(), F::Error> {
        let next = T::from_i64(self.last_time.to_i64().saturating_add(1));
        self.append(next, data)
    }

    /// Iterate from the oldest entry to the newest.
    pub fn iter(&mut self) -> TslIterator<'_, F, T> {
        let start = self.storage.oldest_addr;
        TslIterator {
            db: self,
            mode: IterMode::Forward,
            next: TslIterator::next_forward,
            sector_addr: start,
            traversed_len: 0,
            next_index: 0,
            in_sector: false,
            found_start: false,
            done: false,
            start_sector: start,
            sec_addr: 0,
            sec_end_idx: 0,
            sec_first: 0,
            sec_last: 0,
        }
    }

    /// Iterate from the newest entry to the oldest.
    pub fn iter_reverse(&mut self) -> TslIterator<'_, F, T> {
        let start = self.cur_sec.addr;
        TslIterator {
            db: self,
            mode: IterMode::Reverse,
            next: TslIterator::next_reverse,
            sector_addr: start,
            traversed_len: 0,
            next_index: 0,
            in_sector: false,
            found_start: false,
            done: false,
            start_sector: start,
            sec_addr: 0,
            sec_end_idx: 0,
            sec_first: 0,
            sec_last: 0,
        }
    }

    /// Iterate entries with timestamps in `[from, to]`, or in `[to, from]` when
    /// `from > to` (reverse range iteration).
    pub fn iter_by_time(&mut self, from: T, to: T) -> TslIterator<'_, F, T> {
        let start = if from <= to {
            self.storage.oldest_addr
        } else {
            self.cur_sec.addr
        };
        TslIterator {
            db: self,
            mode: IterMode::ByTime { from, to },
            next: TslIterator::next_by_time_entry,
            sector_addr: start,
            traversed_len: 0,
            next_index: 0,
            in_sector: false,
            found_start: false,
            done: false,
            start_sector: start,
            sec_addr: 0,
            sec_end_idx: 0,
            sec_first: 0,
            sec_last: 0,
        }
    }

    /// Count entries with a given status inside a timestamp range.
    pub fn query_count(&mut self, from: T, to: T, status: TslStatus) -> Result<usize, F::Error> {
        let mut count = 0usize;
        let mut it = self.iter_by_time(from, to);
        while let Some(entry) = it.next() {
            if entry?.status == status {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Maximum number of `max_len` payloads the database can hold.
    pub fn max_blob_count(&self) -> usize {
        let sec_size = self.storage.sec_size() as usize;
        let layout = Self::layout();
        let sec_hdr_size = layout.sec_hdr_size as usize;
        let blob_size =
            layout.log_idx_size as usize + wg_align(self.max_len as usize, F::WRITE_GRAN_BITS);
        let n_sec = self.storage.sector_count() as usize;
        n_sec * (sec_size.saturating_sub(sec_hdr_size) / blob_size)
    }

    /// Change the status of an entry (as yielded by an iterator).
    pub fn set_status(&mut self, entry: &TslEntry<T>, status: TslStatus) -> Result<(), F::Error> {
        self.storage
            .write_status(entry.index_addr, TSL_STATUS_NUM, tsl_status_index(status))
    }

    /// Erase every sector and reset the database (dangerous, irreversible).
    pub fn clean(&mut self) -> Result<(), F::Error> {
        self.format_all()
    }

    /// Timestamp of the last appended (or recovered) entry.
    pub fn last_time(&self) -> T {
        self.last_time
    }

    /// Whether the database wraps around to the oldest sector when full.
    pub fn rollover(&self) -> bool {
        self.rollover
    }

    /// Enable/disable rollover.
    pub fn set_rollover(&mut self, rollover: bool) {
        self.rollover = rollover;
    }

    /// Maximum payload length accepted by [`TsdbImpl::append`].
    pub fn max_len(&self) -> u32 {
        self.max_len
    }

    /// Consume the database and return the flash driver.
    pub fn into_flash(self) -> F {
        self.storage.into_flash()
    }

    // -- private implementation -------------------------------------------

    /// End timestamp of the sector before `addr` (wrapping around), used when
    /// the current sector has no end info of its own.
    fn read_previous_sector_end(&mut self, addr: u32) -> Result<T, F::Error> {
        let sec_size = self.storage.sec_size();
        let max_size = self.storage.max_size();
        let max_len = self.max_len;
        let prev_addr = if addr >= sec_size {
            addr - sec_size
        } else {
            max_size - sec_size
        };
        let mut prev = SecInfo::<T>::default();
        Self::read_sec_into(&mut self.storage, max_len, prev_addr, false, &mut prev)?;
        Ok(prev.end_time)
    }

    /// Read one index record into `entry` (`read_tsl()`).
    ///
    /// Decoding into a caller-provided slot (instead of returning
    /// `Result<TslEntry<T>, _>` by value) keeps the ~20 B entry and the
    /// `Result` packing out of every iterator step and sector scan.
    ///
    /// The stack buffer is sized per write granularity/timestamp width instead
    /// of always using the largest layout, which keeps this per-entry hot path
    /// cheap on the common 1/8/32-bit configurations.
    fn read_tsl_into(
        storage: &mut Storage<F>,
        max_len: u32,
        index_addr: u32,
        entry: &mut TslEntry<T>,
    ) -> Result<(), F::Error> {
        let layout = Self::layout();
        let wg = F::WRITE_GRAN_BITS;
        let size = layout.log_idx_raw_size as usize;
        if wg <= 8 {
            /* at most 24 B: 24 B for 64-bit timestamps at WG=1/8 */
            let mut raw = [F::ERASED_BYTE; 24];
            storage.read(index_addr, &mut raw[..size])?;
            decode_tsl(index_addr, &raw[..size], &layout, wg, max_len, entry);
        } else if wg <= 32 {
            /* at most 40 B (64-bit timestamps at WG=32) */
            let mut raw = [F::ERASED_BYTE; 40];
            storage.read(index_addr, &mut raw[..size])?;
            decode_tsl(index_addr, &raw[..size], &layout, wg, max_len, entry);
        } else if wg <= 64 {
            /* at most 56 B */
            let mut raw = [F::ERASED_BYTE; 56];
            storage.read(index_addr, &mut raw[..size])?;
            decode_tsl(index_addr, &raw[..size], &layout, wg, max_len, entry);
        } else if wg <= 128 {
            /* at most 96 B */
            let mut raw = [F::ERASED_BYTE; 96];
            storage.read(index_addr, &mut raw[..size])?;
            decode_tsl(index_addr, &raw[..size], &layout, wg, max_len, entry);
        } else {
            /* at most 192 B (WG=256) */
            let mut raw = [F::ERASED_BYTE; 192];
            storage.read(index_addr, &mut raw[..size])?;
            decode_tsl(index_addr, &raw[..size], &layout, wg, max_len, entry);
        }
        Ok(())
    }

    /// Read and decode one sector header into `out` (`read_sector_info()`),
    /// including the `empty_idx` / `empty_data` / `remain` bookkeeping for a
    /// USING sector.
    ///
    /// `out` is written in place (no `SecInfo<T>` by-value return), so callers
    /// can decode straight into `cur_sec` or a scratch slot with split borrows.
    fn read_sec_into(
        storage: &mut Storage<F>,
        max_len: u32,
        addr: u32,
        traversal: bool,
        out: &mut SecInfo<T>,
    ) -> Result<(), F::Error> {
        let layout = Self::layout();
        let wg = F::WRITE_GRAN_BITS;
        let sec_size = storage.sec_size();
        let size = layout.sec_raw_size as usize;
        /* The stack buffer is sized per write granularity and timestamp width
         * (44 B in use for the common 32-bit timestamp / 8-bit write
         * granularity case) instead of zero-filling the largest 640 B layout. */
        if wg <= 8 {
            /* at most 56 B (64-bit timestamps at WG=8) */
            let mut raw = [F::ERASED_BYTE; 56];
            storage.read(addr, &mut raw[..size])?;
            decode_sector_header(addr, &raw[..size], &layout, wg, out);
        } else if wg <= 32 {
            /* at most 92 B (64-bit timestamps at WG=32) */
            let mut raw = [F::ERASED_BYTE; 92];
            storage.read(addr, &mut raw[..size])?;
            decode_sector_header(addr, &raw[..size], &layout, wg, out);
        } else if wg <= 64 {
            /* at most 160 B */
            let mut raw = [F::ERASED_BYTE; 160];
            storage.read(addr, &mut raw[..size])?;
            decode_sector_header(addr, &raw[..size], &layout, wg, out);
        } else if wg <= 128 {
            /* at most 320 B */
            let mut raw = [F::ERASED_BYTE; 320];
            storage.read(addr, &mut raw[..size])?;
            decode_sector_header(addr, &raw[..size], &layout, wg, out);
        } else {
            /* at most 640 B (WG=256) */
            let mut raw = [F::ERASED_BYTE; 640];
            storage.read(addr, &mut raw[..size])?;
            decode_sector_header(addr, &raw[..size], &layout, wg, out);
        }
        if !out.check_ok {
            return Ok(());
        }
        let end_info_valid =
            out.end_info_stat[0] == TslStatus::Write || out.end_info_stat[1] == TslStatus::Write;

        /* The TSL payloads are saved from the sector bottom, the indexes from
         * the sector top. */
        out.empty_idx = addr + layout.sec_hdr_size;
        out.empty_data = addr + sec_size;
        out.remain = out.empty_data - out.empty_idx;

        /* Traverse the sector to rebuild the bookkeeping. C only does this for
         * USING sectors; a FULL sector without a usable end info pair can only
         * result from a power failure during the end-info write, where C would
         * hit assert(0). Scan it as well so the entries stay iterable. */
        let scan = out.status == SectorStoreStatus::Using
            || (out.status == SectorStoreStatus::Full && !end_info_valid);
        if scan && (traversal || out.status == SectorStoreStatus::Full) {
            Self::scan_sector(storage, max_len, addr, sec_size, out)?;
        }
        Ok(())
    }

    /// Rebuild a sector's `end_idx` / `end_time` / `empty_*` / `remain`
    /// bookkeeping by walking its index records (the traversal part of
    /// `read_sector_info()`).
    ///
    /// Cold: only mounting/recovery and sector transitions hit it, so its frame
    /// and its record copy stay out of the per-entry read path.
    #[cold]
    #[inline(never)]
    fn scan_sector(
        storage: &mut Storage<F>,
        max_len: u32,
        addr: u32,
        sec_size: u32,
        sec: &mut SecInfo<T>,
    ) -> Result<(), F::Error> {
        let layout = Self::layout();
        let wg = F::WRITE_GRAN_BITS;
        let sector_end = addr + sec_size;
        let mut index = sec.empty_idx;
        let mut tsl = empty_entry::<T>();
        loop {
            /* A real record always fits between the index cursor and the
             * payload cursor; anything else is erased free space or a
             * damaged sector, stop before reading into the payload area. */
            if index.saturating_add(layout.log_idx_raw_size) > sec.empty_data
                || index.saturating_add(layout.log_idx_raw_size) > sector_end
            {
                break;
            }
            Self::read_tsl_into(storage, max_len, index, &mut tsl)?;
            if tsl.status == TslStatus::Unused {
                break;
            }
            if tsl.status != TslStatus::PreWrite {
                sec.end_time = tsl.time;
            }
            sec.end_idx = index;
            sec.empty_idx += layout.log_idx_size;
            let aligned = wg_align(tsl.log_len as usize, wg) as u32;
            sec.empty_data = sec.empty_data.saturating_sub(aligned);
            index += layout.log_idx_size;
            let used = layout.log_idx_size + aligned;
            if sec.remain > used {
                sec.remain -= used;
            } else {
                /* Same as C: the accounting is clamped, but this is not a
                 * flash error (the entry itself was valid). */
                sec.remain = 0;
                break;
            }
        }
        if sec.end_idx == FAILED_ADDR {
            /* No entry at all: put end_idx just below the first index. */
            sec.end_idx = sec.empty_idx.wrapping_sub(layout.log_idx_size);
        }
        Ok(())
    }

    /// Erase one sector and write the EMPTY status + magic word
    /// (`format_sector()`).
    fn format_sector(&mut self, addr: u32) -> Result<(), F::Error> {
        let layout = Self::layout();
        self.storage.erase_sector(addr)?;
        self.storage.write_status(
            addr + layout.sec_store_off,
            SECTOR_STORE_STATUS_NUM,
            sector_store_status_index(SectorStoreStatus::Empty),
        )?;
        /* the magic block is padded with erased bytes to uint32 alignment */
        if layout.uint32_align == 4 {
            /* the magic word is exactly 4 bytes at WG<=32 */
            self.storage.write(
                addr + layout.sec_magic_off,
                &TSDB_SECTOR_MAGIC.to_le_bytes(),
            )
        } else {
            let mut magic = [F::ERASED_BYTE; MAX_ALIGN];
            put_u32_at(&mut magic, 0, TSDB_SECTOR_MAGIC);
            self.storage.write(
                addr + layout.sec_magic_off,
                &magic[..layout.uint32_align as usize],
            )
        }
    }

    /// Format every sector and reset the RAM bookkeeping (`tsl_format_all()`).
    fn format_all(&mut self) -> Result<(), F::Error> {
        let sec_size = self.storage.sec_size();
        let max_size = self.storage.max_size();
        let max_len = self.max_len;
        let mut addr = 0u32;
        while addr < max_size {
            self.format_sector(addr)?;
            addr += sec_size;
        }
        self.storage.oldest_addr = 0;
        self.last_time = T::default();
        Self::read_sec_into(&mut self.storage, max_len, 0, false, &mut self.cur_sec)?;
        Ok(())
    }

    /// Write one of the two end-info records of a sector that is being sealed
    /// (the status is moved PRE_WRITE -> Write around the data writes).
    fn write_end_info(
        &mut self,
        sec_addr: u32,
        end_idx: u32,
        end_time: T,
        slot: usize,
    ) -> Result<(), F::Error> {
        let layout = Self::layout();
        /* end_info[1] is end_info[0] shifted by one end-info record. */
        let off = if slot == 0 {
            0
        } else {
            layout.sec_end1_time_off - layout.sec_end0_time_off
        };
        let stat_off = layout.sec_end0_stat_off + off;
        let time_off = layout.sec_end0_time_off + off;
        let idx_off = layout.sec_end0_idx_off + off;
        /* TSL_TIME_ALIGN_SIZE */
        let time_len = (layout.sec_end0_time_off - layout.sec_start_time_off) as usize;
        let idx_len = layout.uint32_align as usize;

        self.storage.write_status(
            sec_addr + stat_off,
            TSL_STATUS_NUM,
            tsl_status_index(TslStatus::PreWrite),
        )?;
        /* One erased-filled scratch buffer carries both the timestamp and the
         * index word; the write lengths are layout constants and never exceed
         * MAX_ALIGN. This is a cold (sector sealing) path, so the few extra
         * scratch bytes are cheaper than the old per-field buffer branches. */
        let mut buf = [F::ERASED_BYTE; MAX_ALIGN];
        end_time.write_le(&mut buf, 0);
        self.storage.write(sec_addr + time_off, &buf[..time_len])?;
        put_u32_at(&mut buf, 0, end_idx);
        self.storage.write(sec_addr + idx_off, &buf[..idx_len])?;
        self.storage.write_status(
            sec_addr + stat_off,
            TSL_STATUS_NUM,
            tsl_status_index(TslStatus::Write),
        )
    }

    /// Seal the current sector if the new record does not fit, advance to the
    /// next sector (formatting it when it still holds old data) and make an
    /// EMPTY sector USING (`update_sec_status()`).
    ///
    /// Cold: the per-append fast path in [`TsdbImpl::append`] already rejects
    /// the common "USING with room" case, so this only runs on a sector
    /// boundary. Keeping it out of line also keeps its sector copies,
    /// end-info buffers and next-sector decode out of the append frame.
    #[cold]
    #[inline(never)]
    fn prepare_sector(&mut self, need: u32, cur_time: T) -> Result<(), F::Error> {
        let layout = Self::layout();
        let sec_size = self.storage.sec_size();
        let max_size = self.storage.max_size();
        let max_len = self.max_len;

        /* Work on `self.cur_sec` in place: it is the sector the next record
         * goes into, so no copy in and no copy back is needed. */
        if self.cur_sec.status == SectorStoreStatus::Using && self.cur_sec.remain < need {
            /* save the end node index and timestamp of the previous record */
            let addr = self.cur_sec.addr;
            let end_idx = self.cur_sec.empty_idx - layout.log_idx_size;
            let end_time = self.last_time;
            if self.cur_sec.end_info_stat[0] == TslStatus::Unused {
                self.write_end_info(addr, end_idx, end_time, 0)?;
                self.cur_sec.end_info_stat[0] = TslStatus::Write;
            } else if self.cur_sec.end_info_stat[1] == TslStatus::Unused {
                self.write_end_info(addr, end_idx, end_time, 1)?;
                self.cur_sec.end_info_stat[1] = TslStatus::Write;
            }
            /* change current sector to full */
            self.storage.write_status(
                addr + layout.sec_store_off,
                SECTOR_STORE_STATUS_NUM,
                sector_store_status_index(SectorStoreStatus::Full),
            )?;
            self.cur_sec.status = SectorStoreStatus::Full;

            /* calculate next sector address */
            let new_sec_addr = if addr + sec_size < max_size {
                addr + sec_size
            } else if self.rollover {
                0
            } else {
                /* not rollover */
                return Err(Error::SavedFull);
            };

            Self::read_sec_into(
                &mut self.storage,
                max_len,
                new_sec_addr,
                false,
                &mut self.cur_sec,
            )?;
            if self.cur_sec.status != SectorStoreStatus::Empty {
                /* the next sector still contains old data: it becomes the new
                 * current sector after being formatted, and the sector after
                 * it becomes the oldest one */
                self.storage.oldest_addr = wrap_next(new_sec_addr, sec_size, max_size);
                self.format_sector(new_sec_addr)?;
                Self::read_sec_into(
                    &mut self.storage,
                    max_len,
                    new_sec_addr,
                    false,
                    &mut self.cur_sec,
                )?;
            }
        } else if self.cur_sec.status == SectorStoreStatus::Full {
            /* database full */
            return Err(Error::SavedFull);
        }

        if self.cur_sec.status == SectorStoreStatus::Empty {
            /* change the sector to using */
            self.cur_sec.status = SectorStoreStatus::Using;
            self.cur_sec.start_time = cur_time;
            let addr = self.cur_sec.addr;
            self.storage.write_status(
                addr + layout.sec_store_off,
                SECTOR_STORE_STATUS_NUM,
                sector_store_status_index(SectorStoreStatus::Using),
            )?;
            /* save the start timestamp (one erased-filled scratch buffer, the
             * length is a layout constant) */
            let mut time = [F::ERASED_BYTE; MAX_ALIGN];
            cur_time.write_le(&mut time, 0);
            let time_len = (layout.sec_end0_time_off - layout.sec_start_time_off) as usize;
            self.storage
                .write(addr + layout.sec_start_time_off, &time[..time_len])?;
        }

        Ok(())
    }

    /// Write one TSL at `idx_addr` / `log_addr`: PRE_WRITE status, index record,
    /// aligned payload, Write status (`write_tsl()`).
    fn write_tsl_at(
        &mut self,
        idx_addr: u32,
        log_addr: u32,
        time: T,
        data: &[u8],
    ) -> Result<(), F::Error> {
        let layout = Self::layout();
        let wg = F::WRITE_GRAN_BITS;

        /* write the status by write granularity */
        self.storage.write_status(
            idx_addr,
            TSL_STATUS_NUM,
            tsl_status_index(TslStatus::PreWrite),
        )?;
        /* write the other index info: time + log_len + log_addr (+ padding).
         * C writes the tail of the index struct starting at the timestamp; its
         * length is at most 32 bytes for every supported layout, so a small
         * buffer with relative offsets is enough. */
        let ts_off = layout.log_idx_ts_off;
        let len_off = (layout.log_idx_loglen_off - ts_off) as usize;
        let addr_off = (layout.log_idx_logaddr_off - ts_off) as usize;
        let block_len = (layout.log_idx_raw_size - ts_off) as usize;
        if wg <= 32 {
            /* the written index tail is at most 16 B at WG<=32 */
            let mut block = [F::ERASED_BYTE; 16];
            time.write_le(&mut block, 0);
            put_u32_at(&mut block, len_off, data.len() as u32);
            put_u32_at(&mut block, addr_off, log_addr);
            self.storage.write(idx_addr + ts_off, &block[..block_len])?;
        } else {
            let mut block = [F::ERASED_BYTE; MAX_ALIGN];
            time.write_le(&mut block, 0);
            put_u32_at(&mut block, len_off, data.len() as u32);
            put_u32_at(&mut block, addr_off, log_addr);
            self.storage.write(idx_addr + ts_off, &block[..block_len])?;
        }
        /* write the blob data */
        self.storage.write_align(log_addr, data)?;
        /* write the status by write granularity */
        self.storage
            .write_status(idx_addr, TSL_STATUS_NUM, tsl_status_index(TslStatus::Write))
    }

    /// Find the index address to start a by-time scan in one sector
    /// (`search_start_tsl_addr()`).
    fn search_start_tsl_addr(
        &mut self,
        first: u32,
        sector_end: u32,
        end: u32,
        from: T,
        to: T,
    ) -> Result<u32, F::Error> {
        let layout = Self::layout();
        let log_idx_size = layout.log_idx_size;
        let raw_size = layout.log_idx_raw_size;
        let last = sector_end.saturating_sub(raw_size);
        let mut start = first;
        let mut end = end;
        if end < start {
            return Ok(start);
        }
        end = end.min(last);
        if end < start {
            return Ok(start);
        }
        let mut tsl = empty_entry::<T>();
        loop {
            let mid = start + align_to((end - start) / 2, log_idx_size);
            Self::read_tsl_into(&mut self.storage, self.max_len, mid, &mut tsl)?;
            if tsl.time < from {
                start = mid + log_idx_size;
            } else if tsl.time > from {
                end = mid.wrapping_sub(log_idx_size);
            } else {
                return Ok(mid);
            }
            if start > end {
                if from > to && start <= last {
                    Self::read_tsl_into(&mut self.storage, self.max_len, start, &mut tsl)?;
                    if tsl.time > from && start > first {
                        start -= log_idx_size;
                    }
                }
                break;
            }
        }
        Ok(start)
    }
}

// ---------------------------------------------------------------------------
// Iterator
// ---------------------------------------------------------------------------

impl<'a, F: Flash, T: Timestamp> TslIterator<'a, F, T> {
    /// Read the payload of `entry` into `buf`.
    ///
    /// Returns the number of bytes read, which is the smaller of `buf.len()`
    /// and the payload length.
    pub fn read_blob(&mut self, entry: &TslEntry<T>, buf: &mut [u8]) -> Result<usize, F::Error> {
        if entry.log_addr == FAILED_ADDR || entry.log_len == 0 {
            return Ok(0);
        }
        let len = core::cmp::min(buf.len() as u32, entry.log_len) as usize;
        self.db.storage.read(entry.log_addr, &mut buf[..len])?;
        Ok(len)
    }

    /// Read the entry at `index` if it is inside the current sector's index
    /// area; yields the `Result` otherwise.
    fn read_entry(&mut self, index: u32) -> Option<Result<TslEntry<T>, F::Error>> {
        if index < self.sec_first || index > self.sec_last {
            return None;
        }
        let mut entry = empty_entry::<T>();
        match TsdbImpl::<F, T>::read_tsl_into(
            &mut self.db.storage,
            self.db.max_len,
            index,
            &mut entry,
        ) {
            Ok(()) => Some(Ok(entry)),
            Err(e) => {
                self.done = true;
                self.in_sector = false;
                Some(Err(e))
            }
        }
    }

    /// Advance the forward iterator to the next sector that has entries.
    /// Returns `false` when the whole ring has been traversed.
    #[inline(never)]
    fn advance_forward(&mut self) -> Result<bool, F::Error> {
        if self.sector_addr == FAILED_ADDR {
            self.done = true;
            return Ok(false);
        }
        let sec_addr = self.sector_addr;
        let sec_size = self.db.storage.sec_size();
        let max_size = self.db.storage.max_size();
        self.traversed_len = self.traversed_len.saturating_add(sec_size);
        let mut sec = SecInfo::<T>::default();
        TsdbImpl::<F, T>::read_sec_into(
            &mut self.db.storage,
            self.db.max_len,
            sec_addr,
            false,
            &mut sec,
        )?;
        self.sector_addr = next_sector_addr(sec.addr, self.traversed_len, sec_size, max_size)
            .unwrap_or(FAILED_ADDR);
        if sec.status == SectorStoreStatus::Using || sec.status == SectorStoreStatus::Full {
            /* A USING sector's bookkeeping lives in RAM (`cur_sec`), a FULL
             * sector's in its header. Read the two fields both cases need
             * instead of copying the whole `SecInfo` first. */
            let (addr, end_idx) = if sec.status == SectorStoreStatus::Using {
                (self.db.cur_sec.addr, self.db.cur_sec.end_idx)
            } else {
                (sec.addr, sec.end_idx)
            };
            let layout = TsdbImpl::<F, T>::layout();
            let first = addr + layout.sec_hdr_size;
            if end_idx != FAILED_ADDR && end_idx >= first {
                self.sec_first = first;
                self.sec_last = addr + sec_size - layout.log_idx_raw_size;
                self.next_index = first;
                self.sec_addr = addr;
                self.sec_end_idx = end_idx;
                self.in_sector = true;
            }
        }
        Ok(true)
    }

    /// Forward iteration over every entry (`fdb_tsl_iter`).
    fn next_forward(&mut self) -> Option<Result<TslEntry<T>, F::Error>> {
        let log_idx_size = TsdbImpl::<F, T>::layout().log_idx_size;
        loop {
            if self.in_sector {
                let index = self.next_index;
                match next_tsl_addr(index, self.sec_end_idx, log_idx_size) {
                    Some(next) => self.next_index = next,
                    None => self.in_sector = false,
                }
                match self.read_entry(index) {
                    None => continue,
                    Some(Err(e)) => return Some(Err(e)),
                    Some(Ok(entry)) => return Some(Ok(entry)),
                }
            }
            match self.advance_forward() {
                Ok(true) => {}
                Ok(false) => return None,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }

    /// Reverse iteration over every entry (`fdb_tsl_iter_reverse`).
    fn next_reverse(&mut self) -> Option<Result<TslEntry<T>, F::Error>> {
        loop {
            if self.done {
                return None;
            }
            if self.in_sector {
                let index = self.next_index;
                let layout = TsdbImpl::<F, T>::layout();
                let next = prev_tsl_addr(
                    index,
                    self.sec_addr,
                    layout.log_idx_size,
                    layout.sec_hdr_size,
                );
                match next {
                    Some(next) => self.next_index = next,
                    None => self.in_sector = false,
                }
                match self.read_entry(index) {
                    None => continue,
                    Some(Err(e)) => return Some(Err(e)),
                    Some(Ok(entry)) => return Some(Ok(entry)),
                }
            }

            if self.sector_addr == FAILED_ADDR {
                self.done = true;
                return None;
            }
            let sec_addr = self.sector_addr;
            let sec_size = self.db.storage.sec_size();
            let max_size = self.db.storage.max_size();
            self.traversed_len = self.traversed_len.saturating_add(sec_size);
            let mut sec = SecInfo::<T>::default();
            if let Err(e) = TsdbImpl::<F, T>::read_sec_into(
                &mut self.db.storage,
                self.db.max_len,
                sec_addr,
                false,
                &mut sec,
            ) {
                self.done = true;
                return Some(Err(e));
            }
            self.sector_addr = prev_sector_addr(sec.addr, self.traversed_len, sec_size, max_size)
                .unwrap_or(FAILED_ADDR);

            if sec.status == SectorStoreStatus::Using || sec.status == SectorStoreStatus::Full {
                let sec = if sec.status == SectorStoreStatus::Using {
                    /* copy the current using sector's RAM status */
                    self.db.cur_sec
                } else {
                    sec
                };
                let layout = TsdbImpl::<F, T>::layout();
                if sector_has_entries(&sec, layout.sec_hdr_size) {
                    self.sec_first = sec.addr + layout.sec_hdr_size;
                    self.sec_last = sec.addr + sec_size - layout.log_idx_raw_size;
                    self.next_index = sec.end_idx;
                    self.sec_addr = sec.addr;
                    self.sec_end_idx = sec.end_idx;
                    self.in_sector = true;
                }
            }
            /* Sectors without entries (EMPTY, or damaged headers) are skipped
             * in both directions. C's reverse iterator stops at the first
             * sector without data, which misses older data when a power
             * failure left an unused sector before it. */
        }
    }

    /// Entry point for [`IterMode::ByTime`] iteration.
    fn next_by_time_entry(&mut self) -> Option<Result<TslEntry<T>, F::Error>> {
        match self.mode {
            IterMode::ByTime { from, to } => self.next_by_time(from, to),
            IterMode::Forward | IterMode::Reverse => None,
        }
    }

    /// By-time iteration (`fdb_tsl_iter_by_time`).
    fn next_by_time(&mut self, from: T, to: T) -> Option<Result<TslEntry<T>, F::Error>> {
        let forward = from <= to;
        loop {
            if self.done {
                return None;
            }
            if self.in_sector {
                let index = self.next_index;
                let layout = TsdbImpl::<F, T>::layout();
                let next = if forward {
                    next_tsl_addr(index, self.sec_end_idx, layout.log_idx_size)
                } else {
                    prev_tsl_addr(
                        index,
                        self.sec_addr,
                        layout.log_idx_size,
                        layout.sec_hdr_size,
                    )
                };
                match next {
                    Some(next) => self.next_index = next,
                    None => self.in_sector = false,
                }
                let entry = match self.read_entry(index) {
                    None => continue,
                    Some(Err(e)) => return Some(Err(e)),
                    Some(Ok(entry)) => entry,
                };
                if entry.status == TslStatus::Unused {
                    /* erased index slot: keep looking in this sector */
                    continue;
                }
                let in_range = if forward {
                    entry.time >= from && entry.time <= to
                } else {
                    entry.time <= from && entry.time >= to
                };
                if in_range {
                    return Some(Ok(entry));
                }
                /* the timestamps are ordered: the first record outside the
                 * range means the scan is over */
                self.done = true;
                return None;
            }

            if self.sector_addr == FAILED_ADDR {
                self.done = true;
                return None;
            }
            let sec_addr = self.sector_addr;
            let sec_size = self.db.storage.sec_size();
            let max_size = self.db.storage.max_size();
            self.traversed_len = self.traversed_len.saturating_add(sec_size);
            let mut sec = SecInfo::<T>::default();
            if let Err(e) = TsdbImpl::<F, T>::read_sec_into(
                &mut self.db.storage,
                self.db.max_len,
                sec_addr,
                false,
                &mut sec,
            ) {
                self.done = true;
                return Some(Err(e));
            }
            self.sector_addr = if forward {
                next_sector_addr(sec.addr, self.traversed_len, sec_size, max_size)
            } else {
                prev_sector_addr(sec.addr, self.traversed_len, sec_size, max_size)
            }
            .unwrap_or(FAILED_ADDR);

            if sec.status == SectorStoreStatus::Using || sec.status == SectorStoreStatus::Full {
                let sec = if sec.status == SectorStoreStatus::Using {
                    self.db.cur_sec
                } else {
                    sec
                };
                let enter = self.found_start
                    || if forward {
                        (sec_addr == self.start_sector && from <= sec.start_time)
                            || from <= sec.end_time
                    } else {
                        (sec_addr == self.start_sector && from >= sec.end_time)
                            || from >= sec.start_time
                    };
                if enter {
                    self.found_start = true;
                    let layout = TsdbImpl::<F, T>::layout();
                    if sector_has_entries(&sec, layout.sec_hdr_size) {
                        let first = sec.addr + layout.sec_hdr_size;
                        let start = match self.db.search_start_tsl_addr(
                            first,
                            sec.addr + sec_size,
                            sec.end_idx,
                            from,
                            to,
                        ) {
                            Ok(start) => start,
                            Err(e) => {
                                self.done = true;
                                return Some(Err(e));
                            }
                        };
                        self.sec_first = sec.addr + layout.sec_hdr_size;
                        self.sec_last = sec.addr + sec_size - layout.log_idx_raw_size;
                        self.sec_addr = sec.addr;
                        self.sec_end_idx = sec.end_idx;
                        self.next_index = start;
                        self.in_sector = true;
                    }
                }
            }
            /* EMPTY sectors contain no records; keep walking (the C version
             * stops here, but skipping is required to stay consistent after a
             * power failure left an unused sector in the ring). */
        }
    }
}

impl<'a, F: Flash, T: Timestamp> Iterator for TslIterator<'a, F, T> {
    type Item = Result<TslEntry<T>, F::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        /* Forward is by far the common mode: call it directly so the hot loop
         * does not pay for an indirect call. The fn pointer is kept for the
         * other modes, so a build that never constructs them still drops their
         * bodies (the design note on the `next` field). */
        if let IterMode::Forward = self.mode {
            return self.next_forward();
        }
        let next = self.next;
        next(self)
    }
}
