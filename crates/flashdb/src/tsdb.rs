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
    decode_status, get_u32, put_u32, ts_layout_32, ts_layout_64, wg_align, SectorStoreStatus,
    TsLayout, TslStatus, FAILED_ADDR, TSDB_SECTOR_MAGIC,
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

    #[inline]
    fn to_i64(self) -> i64 {
        self as i64
    }

    #[inline]
    fn from_i64(value: i64) -> Self {
        value.clamp(i32::MIN as i64, i32::MAX as i64) as i32
    }

    #[inline]
    fn write_le(self, buf: &mut [u8], off: usize) {
        buf[off..off + 4].copy_from_slice(&self.to_le_bytes());
    }

    #[inline]
    fn read_le(buf: &[u8], off: usize) -> Self {
        i32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
    }
}

impl Timestamp for i64 {
    const WIDTH: u32 = 64;

    #[inline]
    fn to_i64(self) -> i64 {
        self
    }

    #[inline]
    fn from_i64(value: i64) -> Self {
        value
    }

    #[inline]
    fn write_le(self, buf: &mut [u8], off: usize) {
        buf[off..off + 8].copy_from_slice(&self.to_le_bytes());
    }

    #[inline]
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
    check_ok: bool,
    status: SectorStoreStatus,
    start_time: T,
    end_time: T,
    end_idx: u32,
    end_info_stat: [TslStatus; 2],
    remain: u32,
    empty_idx: u32,
    empty_data: u32,
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
    layout: TsLayout,
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

/// Iterator over the entries of a [`Tsdb`].
///
/// Created by [`TsdbImpl::iter`], [`TsdbImpl::iter_reverse`] or
/// [`TsdbImpl::iter_by_time`]. Call [`TslIterator::read_blob`] to read the
/// payload of the entry that was just yielded.
pub struct TslIterator<'a, F: Flash, T: Timestamp = i32> {
    db: &'a mut TsdbImpl<F, T>,
    mode: IterMode<T>,
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
    /// Bookkeeping of the sector currently being iterated.
    sec: SecInfo<T>,
}

// ---------------------------------------------------------------------------
// Layout and small helpers
// ---------------------------------------------------------------------------

/// Resolve the on-flash layout for the flash's write granularity and the
/// database's timestamp width.
#[inline]
fn ts_layout<F: Flash, T: Timestamp>() -> Option<TsLayout> {
    if T::WIDTH == 64 {
        ts_layout_64(F::WRITE_GRAN_BITS)
    } else {
        ts_layout_32(F::WRITE_GRAN_BITS)
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
fn next_sector_addr(sec_addr: u32, traversed_len: u32, sec_size: u32, max_size: u32) -> Option<u32> {
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
fn prev_sector_addr(sec_addr: u32, traversed_len: u32, sec_size: u32, max_size: u32) -> Option<u32> {
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

/// `get_next_tsl_addr()`.
#[inline]
fn next_tsl_addr<T: Timestamp>(sec: &SecInfo<T>, pre_index: u32, log_idx_size: u32) -> Option<u32> {
    if sec.status == SectorStoreStatus::Empty {
        return None;
    }
    if pre_index + log_idx_size <= sec.end_idx {
        Some(pre_index + log_idx_size)
    } else {
        None
    }
}

/// `get_last_tsl_addr()`.
#[inline]
fn prev_tsl_addr<T: Timestamp>(
    sec: &SecInfo<T>,
    pre_index: u32,
    log_idx_size: u32,
    sec_hdr_size: u32,
) -> Option<u32> {
    if sec.status == SectorStoreStatus::Empty {
        return None;
    }
    if pre_index >= sec.addr + sec_hdr_size + log_idx_size {
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

/// Decode a TSDB sector header from its raw bytes (`read_sector_info`).
#[inline]
fn decode_sector_header<T: Timestamp>(
    addr: u32,
    raw: &[u8],
    layout: &TsLayout,
    wg: u32,
) -> SecInfo<T> {
    let mut sec = SecInfo {
        addr,
        ..Default::default()
    };
    if get_u32(raw, layout.sec_magic_off as usize) != TSDB_SECTOR_MAGIC {
        /* invalid magic: check_ok stays false */
        return sec;
    }
    sec.check_ok = true;
    sec.status = SectorStoreStatus::from_index(decode_status(
        wg,
        SECTOR_STORE_STATUS_NUM,
        &raw[layout.sec_store_off as usize..],
    ));
    sec.start_time = T::read_le(raw, layout.sec_start_time_off as usize);
    sec.end_info_stat[0] = TslStatus::from_index(decode_status(
        wg,
        TSL_STATUS_NUM,
        &raw[layout.sec_end0_stat_off as usize..],
    ));
    sec.end_info_stat[1] = TslStatus::from_index(decode_status(
        wg,
        TSL_STATUS_NUM,
        &raw[layout.sec_end1_stat_off as usize..],
    ));
    if sec.end_info_stat[0] == TslStatus::Write {
        sec.end_time = T::read_le(raw, layout.sec_end0_time_off as usize);
        sec.end_idx = get_u32(raw, layout.sec_end0_idx_off as usize);
    } else if sec.end_info_stat[1] == TslStatus::Write {
        sec.end_time = T::read_le(raw, layout.sec_end1_time_off as usize);
        sec.end_idx = get_u32(raw, layout.sec_end1_idx_off as usize);
    }
    sec
}

/// Decode one index record from its raw bytes (`read_tsl`).
#[inline]
fn decode_tsl<T: Timestamp>(
    index_addr: u32,
    raw: &[u8],
    layout: &TsLayout,
    wg: u32,
    max_len: u32,
) -> TslEntry<T> {
    let status = TslStatus::from_index(decode_status(wg, TSL_STATUS_NUM, raw));
    let mut entry = TslEntry {
        status,
        time: T::default(),
        log_len: 0,
        index_addr,
        log_addr: FAILED_ADDR,
    };
    if status == TslStatus::PreWrite || status == TslStatus::Unused {
        /* An index record without valid metadata: report the worst case payload
         * size and no payload address, exactly like C. */
        entry.log_len = max_len;
        entry.log_addr = FAILED_ADDR;
        entry.time = T::default();
    } else {
        entry.log_len = get_u32(raw, layout.log_idx_loglen_off as usize);
        entry.log_addr = get_u32(raw, layout.log_idx_logaddr_off as usize);
        entry.time = T::read_le(raw, layout.log_idx_ts_off as usize);
    }
    entry
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
        let layout = ts_layout::<F, T>().ok_or(Error::UnsupportedWriteGran)?;
        let sec_size = storage.sec_size();
        let max_size = storage.max_size();
        if max_len >= sec_size {
            /* FlashDB asserts max_len < sec_size. */
            return Err(Error::InvalidConfig);
        }

        let mut db = Self {
            storage,
            cur_sec: SecInfo::default(),
            last_time: T::default(),
            max_len,
            rollover: true,
            layout,
        };

        /* Check all sector headers (fdb_tsdb_init + check_sec_hdr_cb). */
        let mut check_failed = false;
        let mut empty_num = 0usize;
        let mut empty_addr = 0u32;
        /* Newest FULL sector (largest end timestamp). Only used when a
         * database without rollover is mounted: C then guesses the last
         * physical sector, which is wrong after an interrupted wrap. */
        let mut latest_full_addr = FAILED_ADDR;
        let mut latest_full_time = T::default();
        let mut latest_full_info = SecInfo::default();
        /* The last sector read by the check loop is always the last physical
         * sector; the check loop result can be reused for the final
         * read_sector_info() of the current sector instead of reading flash
         * twice. No write happens in between, so both are identical. */
        let mut last_sector_info = SecInfo::default();
        let mut cur_sector_info = SecInfo::default();
        let mut addr = 0u32;
        let mut traversed_len = 0u32;
        loop {
            traversed_len = traversed_len.saturating_add(sec_size);
            /* sector_iterator() reads the header once with traversal=false and
             * once more (with traversal) when the status filter matches. */
            let _ = db.read_sector_info(addr, false)?;
            let sec = db.read_sector_info(addr, true)?;
            if sec.addr == max_size - sec_size {
                last_sector_info = sec;
            }
            if !sec.check_ok {
                check_failed = true;
                break;
            } else if sec.status == SectorStoreStatus::Using {
                if db.cur_sec.addr == FAILED_ADDR {
                    db.cur_sec = sec;
                } else {
                    /* multiple sectors are in use: the DB is damaged */
                    check_failed = true;
                    break;
                }
            } else if sec.status == SectorStoreStatus::Empty {
                empty_num += 1;
                empty_addr = sec.addr;
                if empty_num == 1 && db.cur_sec.addr == FAILED_ADDR {
                    db.cur_sec = sec;
                }
            } else if sec.status == SectorStoreStatus::Full && sec.end_time > latest_full_time {
                latest_full_time = sec.end_time;
                latest_full_addr = sec.addr;
                latest_full_info = sec;
            }
            if db.cur_sec.addr == sec.addr {
                cur_sector_info = sec;
            }
            match next_sector_addr(sec.addr, traversed_len, sec_size, max_size) {
                Some(next) => addr = next,
                None => break,
            }
        }

        if check_failed {
            /* Headers are damaged: re-format the whole database. */
            db.format_all()?;
            /* fdb_tsdb_init reads the current sector once more after formatting. */
            db.cur_sec = db.read_sector_info(0, true)?;
        } else {
            let latest_addr = if empty_num > 0 {
                empty_addr
            } else if db.cur_sec.addr == FAILED_ADDR {
                /* Every sector is full (rollover disabled, or a power failure
                 * right at the wrap). C reads an invalid current sector here.
                 * Use the sector with the newest end info, falling back to the
                 * last physical sector. */
                let addr = if latest_full_addr != FAILED_ADDR {
                    latest_full_addr
                } else {
                    max_size - sec_size
                };
                db.cur_sec.addr = addr;
                addr
            } else if db.rollover {
                db.cur_sec.addr
            } else {
                /* There is no empty sector. */
                let addr = max_size - sec_size;
                db.cur_sec.addr = addr;
                addr
            };
            /* db->cur_sec is the latest sector, the next one is the oldest. */
            db.storage.oldest_addr = if latest_addr + sec_size >= max_size {
                0
            } else {
                latest_addr + sec_size
            };

            let cur_addr = db.cur_sec.addr;
            /* Reuse the header already decoded by the check loop (see above). */
            let cached = if cur_addr == max_size - sec_size {
                Some(last_sector_info)
            } else if cur_addr == latest_full_addr {
                Some(latest_full_info)
            } else if cur_addr == cur_sector_info.addr {
                Some(cur_sector_info)
            } else {
                None
            };
            db.cur_sec = match cached {
                Some(sec) => sec,
                None => db.read_sector_info(cur_addr, true)?,
            };
            /* Recover the last saved timestamp (fdb_tsdb_init). */
            match db.cur_sec.status {
                SectorStoreStatus::Using => {
                    db.last_time = db.cur_sec.end_time;
                    if db.last_time == T::default() {
                        /* Power loss right after the sector was opened: there
                         * is no committed record in it, so take the end
                         * timestamp of the previous (sealed) sector. C reads an
                         * uninitialized end_time in this situation. */
                        db.last_time = db.read_previous_sector_end(cur_addr)?;
                    }
                }
                SectorStoreStatus::Full => {
                    /* Every sector is full (rollover disabled). C leaves
                     * last_time at 0 here; the end info is available, so use
                     * it instead of reporting a meaningless timestamp. */
                    db.last_time = db.cur_sec.end_time;
                }
                SectorStoreStatus::Empty => {
                    if db.storage.oldest_addr != db.cur_sec.addr {
                        db.last_time = db.read_previous_sector_end(cur_addr)?;
                    }
                }
                SectorStoreStatus::Unused => {}
            }
        }

        Ok(db)
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

        self.update_sec_status(data.len() as u32, time)?;
        self.write_tsl(time, data)?;

        /* recalculate the current using sector info */
        let log_idx_size = self.layout.log_idx_size;
        let aligned = wg_align(data.len(), F::WRITE_GRAN_BITS) as u32;
        let cur = &mut self.cur_sec;
        cur.end_idx = cur.empty_idx;
        cur.end_time = time;
        cur.empty_idx = cur.empty_idx.saturating_add(log_idx_size);
        cur.empty_data = cur.empty_data.saturating_sub(aligned);
        cur.remain = cur.remain.saturating_sub(log_idx_size + aligned);
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
            sector_addr: start,
            traversed_len: 0,
            next_index: 0,
            in_sector: false,
            found_start: false,
            done: false,
            start_sector: start,
            sec: SecInfo::default(),
        }
    }

    /// Iterate from the newest entry to the oldest.
    pub fn iter_reverse(&mut self) -> TslIterator<'_, F, T> {
        let start = self.cur_sec.addr;
        TslIterator {
            db: self,
            mode: IterMode::Reverse,
            sector_addr: start,
            traversed_len: 0,
            next_index: 0,
            in_sector: false,
            found_start: false,
            done: false,
            start_sector: start,
            sec: SecInfo::default(),
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
            sector_addr: start,
            traversed_len: 0,
            next_index: 0,
            in_sector: false,
            found_start: false,
            done: false,
            start_sector: start,
            sec: SecInfo::default(),
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
        let sec_hdr_size = self.layout.sec_hdr_size as usize;
        let blob_size = self.layout.log_idx_size as usize
            + wg_align(self.max_len as usize, F::WRITE_GRAN_BITS);
        let n_sec = self.storage.sector_count() as usize;
        n_sec * (sec_size.saturating_sub(sec_hdr_size) / blob_size)
    }

    /// Change the status of an entry (as yielded by an iterator).
    pub fn set_status(&mut self, entry: &TslEntry<T>, status: TslStatus) -> Result<(), F::Error> {
        self.storage.write_status(
            entry.index_addr,
            TSL_STATUS_NUM,
            tsl_status_index(status),
        )
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
        let prev_addr = if addr >= sec_size {
            addr - sec_size
        } else {
            max_size - sec_size
        };
        let prev = self.read_sector_info(prev_addr, false)?;
        Ok(prev.end_time)
    }

    /// Read one index record (`read_tsl()`).
    ///
    /// The stack buffer is sized per write granularity/timestamp width instead
    /// of always using the largest layout, which keeps this per-entry hot path
    /// cheap on the common 1/8/32-bit configurations.
    fn read_tsl(&mut self, index_addr: u32) -> Result<TslEntry<T>, F::Error> {
        let layout = self.layout;
        let wg = F::WRITE_GRAN_BITS;
        let size = layout.log_idx_raw_size as usize;
        let max_len = self.max_len;
        let entry = if wg <= 8 {
            /* at most 24 B: 24 B for 64-bit timestamps at WG=1/8 */
            let mut raw = [F::ERASED_BYTE; 24];
            self.storage.read(index_addr, &mut raw[..size])?;
            decode_tsl(index_addr, &raw[..size], &layout, wg, max_len)
        } else if wg <= 32 {
            /* at most 40 B (64-bit timestamps at WG=32) */
            let mut raw = [F::ERASED_BYTE; 40];
            self.storage.read(index_addr, &mut raw[..size])?;
            decode_tsl(index_addr, &raw[..size], &layout, wg, max_len)
        } else if wg <= 64 {
            /* at most 56 B */
            let mut raw = [F::ERASED_BYTE; 56];
            self.storage.read(index_addr, &mut raw[..size])?;
            decode_tsl(index_addr, &raw[..size], &layout, wg, max_len)
        } else if wg <= 128 {
            /* at most 96 B */
            let mut raw = [F::ERASED_BYTE; 96];
            self.storage.read(index_addr, &mut raw[..size])?;
            decode_tsl(index_addr, &raw[..size], &layout, wg, max_len)
        } else {
            /* at most 192 B (WG=256) */
            let mut raw = [F::ERASED_BYTE; 192];
            self.storage.read(index_addr, &mut raw[..size])?;
            decode_tsl(index_addr, &raw[..size], &layout, wg, max_len)
        };
        Ok(entry)
    }

    /// Read and decode one sector header (`read_sector_info()`), including the
    /// `empty_idx` / `empty_data` / `remain` bookkeeping for a USING sector.
    fn read_sector_info(&mut self, addr: u32, traversal: bool) -> Result<SecInfo<T>, F::Error> {
        let layout = self.layout;
        let wg = F::WRITE_GRAN_BITS;
        let sec_size = self.storage.sec_size();
        let size = layout.sec_raw_size as usize;
        /* The stack buffer is sized per write granularity and timestamp width
         * (92 B for the common 32-bit timestamp / 8-bit write granularity
         * case) instead of zero-filling the largest 640 B layout. */
        let mut sec = if wg <= 8 {
            /* at most 56 B (64-bit timestamps at WG=8) */
            let mut raw = [F::ERASED_BYTE; 56];
            self.storage.read(addr, &mut raw[..size])?;
            decode_sector_header(addr, &raw[..size], &layout, wg)
        } else if wg <= 32 {
            /* at most 92 B (64-bit timestamps at WG=32) */
            let mut raw = [F::ERASED_BYTE; 92];
            self.storage.read(addr, &mut raw[..size])?;
            decode_sector_header(addr, &raw[..size], &layout, wg)
        } else if wg <= 64 {
            /* at most 160 B */
            let mut raw = [F::ERASED_BYTE; 160];
            self.storage.read(addr, &mut raw[..size])?;
            decode_sector_header(addr, &raw[..size], &layout, wg)
        } else if wg <= 128 {
            /* at most 320 B */
            let mut raw = [F::ERASED_BYTE; 320];
            self.storage.read(addr, &mut raw[..size])?;
            decode_sector_header(addr, &raw[..size], &layout, wg)
        } else {
            /* at most 640 B (WG=256) */
            let mut raw = [F::ERASED_BYTE; 640];
            self.storage.read(addr, &mut raw[..size])?;
            decode_sector_header(addr, &raw[..size], &layout, wg)
        };
        if !sec.check_ok {
            return Ok(sec);
        }
        let end_info_valid =
            sec.end_info_stat[0] == TslStatus::Write || sec.end_info_stat[1] == TslStatus::Write;

        /* The TSL payloads are saved from the sector bottom, the indexes from
         * the sector top. */
        sec.empty_idx = addr + layout.sec_hdr_size;
        sec.empty_data = addr + sec_size;
        sec.remain = sec.empty_data - sec.empty_idx;

        /* Traverse the sector to rebuild the bookkeeping. C only does this for
         * USING sectors; a FULL sector without a usable end info pair can only
         * result from a power failure during the end-info write, where C would
         * hit assert(0). Scan it as well so the entries stay iterable. */
        let scan = sec.status == SectorStoreStatus::Using
            || (sec.status == SectorStoreStatus::Full && !end_info_valid);
        if scan && (traversal || sec.status == SectorStoreStatus::Full) {
            let sector_end = addr + sec_size;
            let mut index = sec.empty_idx;
            loop {
                /* A real record always fits between the index cursor and the
                 * payload cursor; anything else is erased free space or a
                 * damaged sector, stop before reading into the payload area. */
                if index.saturating_add(layout.log_idx_raw_size) > sec.empty_data
                    || index.saturating_add(layout.log_idx_raw_size) > sector_end
                {
                    break;
                }
                let tsl = self.read_tsl(index)?;
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
        }

        Ok(sec)
    }

    /// Erase one sector and write the EMPTY status + magic word
    /// (`format_sector()`).
    fn format_sector(&mut self, addr: u32) -> Result<(), F::Error> {
        let layout = self.layout;
        self.storage.erase_sector(addr)?;
        self.storage.write_status(
            addr + layout.sec_store_off,
            SECTOR_STORE_STATUS_NUM,
            sector_store_status_index(SectorStoreStatus::Empty),
        )?;
        /* the magic block is padded with erased bytes to uint32 alignment */
        let mut magic = [F::ERASED_BYTE; MAX_ALIGN];
        put_u32(&mut magic, 0, TSDB_SECTOR_MAGIC);
        self.storage.write(
            addr + layout.sec_magic_off,
            &magic[..layout.uint32_align as usize],
        )
    }

    /// Format every sector and reset the RAM bookkeeping (`tsl_format_all()`).
    fn format_all(&mut self) -> Result<(), F::Error> {
        let sec_size = self.storage.sec_size();
        let max_size = self.storage.max_size();
        let mut addr = 0u32;
        let mut traversed_len = 0u32;
        loop {
            traversed_len = traversed_len.saturating_add(sec_size);
            /* sector_iterator() reads each header before the callback runs. */
            let _ = self.read_sector_info(addr, false)?;
            self.format_sector(addr)?;
            match next_sector_addr(addr, traversed_len, sec_size, max_size) {
                Some(next) => addr = next,
                None => break,
            }
        }
        self.storage.oldest_addr = 0;
        self.last_time = T::default();
        self.cur_sec = self.read_sector_info(0, false)?;
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
        let layout = self.layout;
        let (stat_off, time_off, idx_off) = if slot == 0 {
            (
                layout.sec_end0_stat_off,
                layout.sec_end0_time_off,
                layout.sec_end0_idx_off,
            )
        } else {
            (
                layout.sec_end1_stat_off,
                layout.sec_end1_time_off,
                layout.sec_end1_idx_off,
            )
        };
        /* TSL_TIME_ALIGN_SIZE */
        let time_len = (layout.sec_end0_time_off - layout.sec_start_time_off) as usize;

        self.storage.write_status(
            sec_addr + stat_off,
            TSL_STATUS_NUM,
            tsl_status_index(TslStatus::PreWrite),
        )?;
        let mut time = [F::ERASED_BYTE; MAX_ALIGN];
        end_time.write_le(&mut time, 0);
        self.storage
            .write(sec_addr + time_off, &time[..time_len])?;
        let mut index = [F::ERASED_BYTE; MAX_ALIGN];
        put_u32(&mut index, 0, end_idx);
        self.storage.write(
            sec_addr + idx_off,
            &index[..layout.uint32_align as usize],
        )?;
        self.storage.write_status(
            sec_addr + stat_off,
            TSL_STATUS_NUM,
            tsl_status_index(TslStatus::Write),
        )
    }

    /// Seal the current sector if the new record does not fit, advance to the
    /// next sector (formatting it when it still holds old data) and make an
    /// EMPTY sector USING (`update_sec_status()`).
    fn update_sec_status(&mut self, data_len: u32, cur_time: T) -> Result<(), F::Error> {
        let layout = self.layout;
        let wg = F::WRITE_GRAN_BITS;
        let sec_size = self.storage.sec_size();
        let max_size = self.storage.max_size();
        let mut cur = self.cur_sec;
        let aligned = wg_align(data_len as usize, wg) as u32;
        let need = layout.log_idx_size + aligned;

        if cur.status == SectorStoreStatus::Using && cur.remain < need {
            /* save the end node index and timestamp of the previous record */
            let end_idx = cur.empty_idx - layout.log_idx_size;
            let end_time = self.last_time;
            if cur.end_info_stat[0] == TslStatus::Unused {
                self.write_end_info(cur.addr, end_idx, end_time, 0)?;
                cur.end_info_stat[0] = TslStatus::Write;
            } else if cur.end_info_stat[1] == TslStatus::Unused {
                self.write_end_info(cur.addr, end_idx, end_time, 1)?;
                cur.end_info_stat[1] = TslStatus::Write;
            }
            /* change current sector to full */
            self.storage.write_status(
                cur.addr + layout.sec_store_off,
                SECTOR_STORE_STATUS_NUM,
                sector_store_status_index(SectorStoreStatus::Full),
            )?;
            cur.status = SectorStoreStatus::Full;

            /* calculate next sector address */
            let new_sec_addr = if cur.addr + sec_size < max_size {
                cur.addr + sec_size
            } else if self.rollover {
                0
            } else {
                /* not rollover */
                self.cur_sec = cur;
                return Err(Error::SavedFull);
            };

            let mut next = self.read_sector_info(new_sec_addr, false)?;
            if next.status != SectorStoreStatus::Empty {
                /* the next sector still contains old data: it becomes the new
                 * current sector after being formatted, and the sector after
                 * it becomes the oldest one */
                self.storage.oldest_addr = if new_sec_addr + sec_size < max_size {
                    new_sec_addr + sec_size
                } else {
                    0
                };
                self.format_sector(new_sec_addr)?;
                next = self.read_sector_info(new_sec_addr, false)?;
            }
            cur = next;
        } else if cur.status == SectorStoreStatus::Full {
            /* database full */
            self.cur_sec = cur;
            return Err(Error::SavedFull);
        }

        if cur.status == SectorStoreStatus::Empty {
            /* change the sector to using */
            cur.status = SectorStoreStatus::Using;
            cur.start_time = cur_time;
            self.storage.write_status(
                cur.addr + layout.sec_store_off,
                SECTOR_STORE_STATUS_NUM,
                sector_store_status_index(SectorStoreStatus::Using),
            )?;
            /* save the start timestamp */
            let mut time = [F::ERASED_BYTE; MAX_ALIGN];
            cur_time.write_le(&mut time, 0);
            let time_len = (layout.sec_end0_time_off - layout.sec_start_time_off) as usize;
            self.storage
                .write(cur.addr + layout.sec_start_time_off, &time[..time_len])?;
        }

        self.cur_sec = cur;
        Ok(())
    }

    /// Write one TSL: PRE_WRITE status, index record, aligned payload, Write
    /// status (`write_tsl()`).
    fn write_tsl(&mut self, time: T, data: &[u8]) -> Result<(), F::Error> {
        let layout = self.layout;
        let wg = F::WRITE_GRAN_BITS;
        let idx_addr = self.cur_sec.empty_idx;
        let log_addr = self.cur_sec.empty_data - wg_align(data.len(), wg) as u32;

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
        let mut block = [F::ERASED_BYTE; MAX_ALIGN];
        debug_assert!(block_len <= block.len());
        time.write_le(&mut block, 0);
        put_u32(&mut block, len_off, data.len() as u32);
        put_u32(&mut block, addr_off, log_addr);
        self.storage
            .write(idx_addr + ts_off, &block[..block_len])?;
        /* write the blob data */
        self.storage.write_align(log_addr, data)?;
        /* write the status by write granularity */
        self.storage.write_status(
            idx_addr,
            TSL_STATUS_NUM,
            tsl_status_index(TslStatus::Write),
        )
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
        let log_idx_size = self.layout.log_idx_size;
        let raw_size = self.layout.log_idx_raw_size;
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
        loop {
            let mid = start + align_to((end - start) / 2, log_idx_size);
            let tsl = self.read_tsl(mid)?;
            if tsl.time < from {
                start = mid + log_idx_size;
            } else if tsl.time > from {
                end = mid.wrapping_sub(log_idx_size);
            } else {
                return Ok(mid);
            }
            if start > end {
                if from > to && start <= last {
                    let tsl = self.read_tsl(start)?;
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
    pub fn read_blob(
        &mut self,
        entry: &TslEntry<T>,
        buf: &mut [u8],
    ) -> Result<usize, F::Error> {
        if entry.log_addr == FAILED_ADDR || entry.log_len == 0 {
            return Ok(0);
        }
        let len = core::cmp::min(buf.len() as u32, entry.log_len) as usize;
        self.db.storage.read(entry.log_addr, &mut buf[..len])?;
        Ok(len)
    }

    /// Index address range that can hold an index record in the current
    /// sector. Reading outside of it would leave the sector (or would read
    /// payload bytes as an index record).
    fn index_bounds(&self) -> (u32, u32) {
        let layout = self.db.layout;
        let first = self.sec.addr + layout.sec_hdr_size;
        let last = self.sec.addr + self.db.storage.sec_size() - layout.log_idx_raw_size;
        (first, last)
    }

    /// Read the entry at `index` if it is inside the current sector's index
    /// area; yields the `Result` otherwise.
    fn read_entry(&mut self, index: u32) -> Option<Result<TslEntry<T>, F::Error>> {
        let (first, last) = self.index_bounds();
        if index < first || index > last {
            return None;
        }
        match self.db.read_tsl(index) {
            Ok(entry) => Some(Ok(entry)),
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }

    /// Forward or reverse iteration over every entry (`fdb_tsl_iter`,
    /// `fdb_tsl_iter_reverse`).
    fn next_direction(&mut self, forward: bool) -> Option<Result<TslEntry<T>, F::Error>> {
        loop {
            if self.done {
                return None;
            }
            if self.in_sector {
                let index = self.next_index;
                let layout = self.db.layout;
                let next = if forward {
                    next_tsl_addr(&self.sec, index, layout.log_idx_size)
                } else {
                    prev_tsl_addr(&self.sec, index, layout.log_idx_size, layout.sec_hdr_size)
                };
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
            let sec = match self.db.read_sector_info(sec_addr, false) {
                Ok(sec) => sec,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };
            self.sector_addr = if forward {
                next_sector_addr(sec.addr, self.traversed_len, sec_size, max_size)
            } else {
                prev_sector_addr(sec.addr, self.traversed_len, sec_size, max_size)
            }
            .unwrap_or(FAILED_ADDR);

            if sec.status == SectorStoreStatus::Using || sec.status == SectorStoreStatus::Full {
                let sec = if sec.status == SectorStoreStatus::Using {
                    /* copy the current using sector's RAM status */
                    self.db.cur_sec
                } else {
                    sec
                };
                let layout = self.db.layout;
                if sector_has_entries(&sec, layout.sec_hdr_size) {
                    self.next_index = if forward {
                        sec.addr + layout.sec_hdr_size
                    } else {
                        sec.end_idx
                    };
                    self.sec = sec;
                    self.in_sector = true;
                }
            }
            /* Sectors without entries (EMPTY, or damaged headers) are skipped
             * in both directions. C's reverse iterator stops at the first
             * sector without data, which misses older data when a power
             * failure left an unused sector before it. */
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
                let layout = self.db.layout;
                let next = if forward {
                    next_tsl_addr(&self.sec, index, layout.log_idx_size)
                } else {
                    prev_tsl_addr(&self.sec, index, layout.log_idx_size, layout.sec_hdr_size)
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
            let sec = match self.db.read_sector_info(sec_addr, false) {
                Ok(sec) => sec,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };
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
                    let layout = self.db.layout;
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
                        self.sec = sec;
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
        match self.mode {
            IterMode::Forward => self.next_direction(true),
            IterMode::Reverse => self.next_direction(false),
            IterMode::ByTime { from, to } => self.next_by_time(from, to),
        }
    }
}
