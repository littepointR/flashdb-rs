//! Low-level on-flash format primitives shared by KVDB and TSDB.
//!
//! # Compatibility
//!
//! Every constant here is derived from the *original FlashDB 2.2.x* C layout,
//! verified field by field against `offsetof()`/`sizeof()` of the C structs on
//! a standard 32-bit ABI. The on-flash format is therefore byte-for-byte
//! compatible, which means:
//!
//! * a Rust database can mount data written by FlashDB-C, and
//! * FlashDB-C can mount data written by this crate,
//!
//! for the same write granularity, sector size and timestamp width.
//!
//! Layout depends on the flash write granularity in non-obvious ways because
//! the C structs embed write-granularity-sized status tables and explicit
//! padding arrays. All variations are captured in [`KvLayout`] / [`TsLayout`].
//!
//! # Status encoding
//!
//! Flash can only clear bits, so status fields are *monotonic*: index 0 is
//! "erased" and never written; writing status `n > 0` clears a write-granule
//! sized chunk at offset `n - 1`. Reading takes the highest cleared chunk.
//! For granularity 1 (bit level) the chunks are bits, packed MSB first.

/// Address value that means "none".
pub const FAILED_ADDR: u32 = 0xFFFF_FFFF;

/// Value of the KVDB sector `combined` field when a sector stands alone.
pub const SECTOR_NOT_COMBINED: u32 = 0xFFFF_FFFF;
/// Value of the KVDB sector `combined` field when sectors are combined.
pub const SECTOR_COMBINED: u32 = 0x0000_0000;

/// KVDB sector magic word: `FDB1` little endian.
pub const KVDB_SECTOR_MAGIC: u32 = 0x3042_4446;
/// KV node magic word: `KV00` little endian.
pub const KV_MAGIC: u32 = 0x3030_564B;
/// TSDB sector magic word: `TSL0` little endian.
pub const TSDB_SECTOR_MAGIC: u32 = 0x304C_5354;

/// Maximum key length in bytes (FlashDB `FDB_KV_NAME_MAX`).
pub const KV_NAME_MAX: usize = 64;
/// String value buffer size of the legacy `fdb_kv_get()` API.
pub const STR_KV_VALUE_MAX_SIZE: usize = 128;

// ---------------------------------------------------------------------------
// Status values (must match the C enums, they are stored numerically)
// ---------------------------------------------------------------------------

/// KV node status, FlashDB `fdb_kv_status_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum KvStatus {
    /// Never used / erased.
    Unused = 0,
    /// Header is being written (power-loss marker).
    PreWrite = 1,
    /// Committed record.
    Write = 2,
    /// Record is being deleted (power-loss marker).
    PreDelete = 3,
    /// Deleted record.
    Deleted = 4,
    /// Header failed an integrity check and was quarantined.
    ErrHdr = 5,
}

impl KvStatus {
    #[inline]
    pub(crate) const fn from_index(index: usize) -> Self {
        match index {
            0 => KvStatus::Unused,
            1 => KvStatus::PreWrite,
            2 => KvStatus::Write,
            3 => KvStatus::PreDelete,
            4 => KvStatus::Deleted,
            _ => KvStatus::ErrHdr,
        }
    }
}

/// Sector store status, FlashDB `fdb_sector_store_status_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SectorStoreStatus {
    /// Not a valid sector (no magic).
    Unused = 0,
    /// Erased and ready for use.
    Empty = 1,
    /// Currently written to.
    Using = 2,
    /// Full, only valid for GC.
    Full = 3,
}

impl SectorStoreStatus {
    #[inline]
    pub(crate) const fn from_index(index: usize) -> Self {
        match index {
            0 => SectorStoreStatus::Unused,
            1 => SectorStoreStatus::Empty,
            2 => SectorStoreStatus::Using,
            _ => SectorStoreStatus::Full,
        }
    }
}

/// Sector dirty status, FlashDB `fdb_sector_dirty_status_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SectorDirtyStatus {
    /// Not a valid sector.
    Unused = 0,
    /// Holds only live records.
    False = 1,
    /// Holds at least one deleted record.
    True = 2,
    /// Garbage collection is in progress.
    Gc = 3,
}

impl SectorDirtyStatus {
    #[inline]
    pub(crate) const fn from_index(index: usize) -> Self {
        match index {
            0 => SectorDirtyStatus::Unused,
            1 => SectorDirtyStatus::False,
            2 => SectorDirtyStatus::True,
            _ => SectorDirtyStatus::Gc,
        }
    }
}

/// Time-series log status, FlashDB `fdb_tsl_status_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TslStatus {
    /// Never used / erased.
    Unused = 0,
    /// Index header is being written (power-loss marker).
    PreWrite = 1,
    /// Committed record.
    Write = 2,
    /// User defined status 1.
    UserStatus1 = 3,
    /// Deleted record.
    Deleted = 4,
    /// User defined status 2.
    UserStatus2 = 5,
}

impl TslStatus {
    #[inline]
    pub(crate) const fn from_index(index: usize) -> Self {
        match index {
            0 => TslStatus::Unused,
            1 => TslStatus::PreWrite,
            2 => TslStatus::Write,
            3 => TslStatus::UserStatus1,
            4 => TslStatus::Deleted,
            _ => TslStatus::UserStatus2,
        }
    }
}

// ---------------------------------------------------------------------------
// CRC32 (identical polynomial/algorithm to `fdb_calc_crc32`)
// ---------------------------------------------------------------------------

/// CRC32 lookup table, generated at compile time (no runtime init, no RAM).
const CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Calculate a chained CRC32, matching `fdb_calc_crc32(crc, buf, size)`.
///
/// Pass `crc = 0` for the first chunk and feed the previous return value for
/// subsequent chunks.
#[inline]
pub fn crc32(mut crc: u32, data: &[u8]) -> u32 {
    crc = !crc;
    let mut i = 0;
    // Unroll by four: still one table, noticeably fewer loop branches.
    while i + 4 <= data.len() {
        crc = CRC32_TABLE[((crc ^ data[i] as u32) & 0xFF) as usize] ^ (crc >> 8);
        crc = CRC32_TABLE[((crc ^ data[i + 1] as u32) & 0xFF) as usize] ^ (crc >> 8);
        crc = CRC32_TABLE[((crc ^ data[i + 2] as u32) & 0xFF) as usize] ^ (crc >> 8);
        crc = CRC32_TABLE[((crc ^ data[i + 3] as u32) & 0xFF) as usize] ^ (crc >> 8);
        i += 4;
    }
    while i < data.len() {
        crc = CRC32_TABLE[((crc ^ data[i] as u32) & 0xFF) as usize] ^ (crc >> 8);
        i += 1;
    }
    !crc
}

/// CRC32 of `0u32.to_le_bytes()` chained into `crc`, used for the `name_len`
/// and `value_len` prefix of a KV record CRC.
#[inline]
pub(crate) fn crc32_u32(crc: u32, value: u32) -> u32 {
    crc32(crc, &value.to_le_bytes())
}

// ---------------------------------------------------------------------------
// Generic helpers
// ---------------------------------------------------------------------------

/// Align `size` up to `align` (which must be a power of two).
#[inline]
pub(crate) const fn align_up(size: usize, align: usize) -> usize {
    if align <= 1 {
        size
    } else {
        (size + align - 1) & !(align - 1)
    }
}

/// Align `size` down to `align` (which must be a power of two).
#[inline]
pub(crate) const fn align_down(size: usize, align: usize) -> usize {
    if align <= 1 {
        size
    } else {
        size & !(align - 1)
    }
}

/// Program granule in bytes. Granularity `1` (bit level) uses single bytes.
#[inline]
pub(crate) const fn wg_bytes(wg_bits: u32) -> usize {
    if wg_bits <= 8 {
        1
    } else {
        (wg_bits / 8) as usize
    }
}

/// Align a size to the write granule (`FDB_WG_ALIGN`).
#[inline]
pub(crate) const fn wg_align(size: usize, wg_bits: u32) -> usize {
    align_up(size, wg_bytes(wg_bits))
}

// ---------------------------------------------------------------------------
// Status field coding
// ---------------------------------------------------------------------------

/// Size of a status table: bit-packed for granularity 1, one write-granule
/// chunk per status for larger granularities (`FDB_STATUS_TABLE_SIZE`).
#[inline]
pub(crate) const fn status_table_size(status_num: usize, wg_bits: u32) -> usize {
    if wg_bits == 1 {
        (status_num + 7) / 8
    } else {
        ((status_num - 1) * wg_bits as usize + 7) / 8
    }
}

/// Decode the highest written status index from a status table.
///
/// This is the Rust equivalent of `_fdb_get_status()`; it also accepts the
/// sparse encoding produced when a status jumps forward (e.g. erased -> Write).
#[inline]
pub(crate) fn decode_status(wg_bits: u32, status_num: usize, table: &[u8]) -> usize {
    if status_num <= 1 {
        return 0;
    }
    if wg_bits == 1 {
        let mut c = status_num - 1;
        while c > 0 {
            let index = c - 1; // chunk index
            if table[index / 8] & (0x80 >> (index % 8)) == 0 {
                return c;
            }
            c -= 1;
        }
        0
    } else {
        let chunk = (wg_bits / 8) as usize;
        let mut c = status_num - 1;
        while c > 0 {
            let index = c - 1;
            let off = index * chunk;
            if table[off..off + chunk].iter().all(|b| *b == 0) {
                return c;
            }
            c -= 1;
        }
        0
    }
}

/// Compute the byte range that has to be cleared to move a status field to
/// `status`.
///
/// Returns `(byte_offset, contents)`; an empty slice means "no flash write is
/// needed" (status 0 is the erased value). The caller writes `contents` at
/// `addr + byte_offset` within the status table.
#[inline]
pub(crate) fn encode_status<'a>(
    wg_bits: u32,
    status: usize,
    scratch: &'a mut [u8; 32],
) -> (usize, &'a [u8]) {
    debug_assert!(status > 0);
    if wg_bits == 1 {
        let bit_index = status - 1;
        scratch[0] = 0xFFu8 >> ((bit_index % 8) + 1);
        (bit_index / 8, &scratch[..1])
    } else {
        let chunk = (wg_bits / 8) as usize;
        for b in scratch[..chunk].iter_mut() {
            *b = 0;
        }
        ((status - 1) * chunk, &scratch[..chunk])
    }
}

// ---------------------------------------------------------------------------
// KVDB layout
// ---------------------------------------------------------------------------

/// Byte layout of FlashDB's KVDB structures for one write granularity.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KvLayout {
    /// `SECTOR_HDR_DATA_SIZE`: aligned size of `struct sector_hdr_data`.
    pub sec_hdr_size: u32,
    /// Raw `sizeof(struct sector_hdr_data)` (may be larger than the aligned
    /// header for granularities above 32 bits; used for the header write).
    pub sec_raw_size: u32,
    pub sec_store_off: u32,
    pub sec_dirty_off: u32,
    pub sec_magic_off: u32,
    /// `KV_HDR_DATA_SIZE`: aligned size of `struct kv_hdr_data`.
    pub kv_hdr_size: u32,
    /// Raw `sizeof(struct kv_hdr_data)`.
    pub kv_raw_size: u32,
    pub kv_magic_off: u32,
    pub kv_len_off: u32,
    pub kv_crc_off: u32,
    pub kv_name_len_off: u32,
    pub kv_value_len_off: u32,
    pub kv_status_size: u32,
    pub store_status_size: u32,
    pub dirty_status_size: u32,
    /// `FDB_SEC_REMAIN_THRESHOLD`: minimum free bytes kept in a sector.
    pub sec_remain_threshold: u32,
}

/// Return the KVDB layout for a write granularity, or `None` if unsupported.
pub(crate) const fn kv_layout(wg_bits: u32) -> Option<KvLayout> {
    // Values verified against the C structs with offsetof()/sizeof().
    let l = match wg_bits {
        1 => KvLayout {
            sec_hdr_size: 16,
            sec_raw_size: 16,
            sec_store_off: 0,
            sec_dirty_off: 1,
            sec_magic_off: 4,
            kv_hdr_size: 24,
            kv_raw_size: 24,
            kv_magic_off: 4,
            kv_len_off: 8,
            kv_crc_off: 12,
            kv_name_len_off: 16,
            kv_value_len_off: 20,
            kv_status_size: 1,
            store_status_size: 1,
            dirty_status_size: 1,
            sec_remain_threshold: 88,
        },
        8 => KvLayout {
            sec_hdr_size: 20,
            sec_raw_size: 20,
            sec_store_off: 0,
            sec_dirty_off: 3,
            sec_magic_off: 8,
            kv_hdr_size: 28,
            kv_raw_size: 28,
            kv_magic_off: 8,
            kv_len_off: 12,
            kv_crc_off: 16,
            kv_name_len_off: 20,
            kv_value_len_off: 24,
            kv_status_size: 5,
            store_status_size: 3,
            dirty_status_size: 3,
            sec_remain_threshold: 92,
        },
        32 => KvLayout {
            sec_hdr_size: 36,
            sec_raw_size: 36,
            sec_store_off: 0,
            sec_dirty_off: 12,
            sec_magic_off: 24,
            kv_hdr_size: 40,
            kv_raw_size: 40,
            kv_magic_off: 20,
            kv_len_off: 24,
            kv_crc_off: 28,
            kv_name_len_off: 32,
            kv_value_len_off: 36,
            kv_status_size: 20,
            store_status_size: 12,
            dirty_status_size: 12,
            sec_remain_threshold: 104,
        },
        64 => KvLayout {
            sec_hdr_size: 64,
            sec_raw_size: 64,
            sec_store_off: 0,
            sec_dirty_off: 24,
            sec_magic_off: 48,
            kv_hdr_size: 64,
            kv_raw_size: 64,
            kv_magic_off: 40,
            kv_len_off: 44,
            kv_crc_off: 48,
            kv_name_len_off: 52,
            kv_value_len_off: 56,
            kv_status_size: 40,
            store_status_size: 24,
            dirty_status_size: 24,
            sec_remain_threshold: 128,
        },
        128 => KvLayout {
            sec_hdr_size: 112,
            sec_raw_size: 112,
            sec_store_off: 0,
            sec_dirty_off: 48,
            sec_magic_off: 96,
            kv_hdr_size: 112,
            kv_raw_size: 112,
            kv_magic_off: 80,
            kv_len_off: 84,
            kv_crc_off: 88,
            kv_name_len_off: 92,
            kv_value_len_off: 96,
            kv_status_size: 80,
            store_status_size: 48,
            dirty_status_size: 48,
            sec_remain_threshold: 176,
        },
        256 => KvLayout {
            sec_hdr_size: 224,
            sec_raw_size: 224,
            sec_store_off: 0,
            sec_dirty_off: 96,
            sec_magic_off: 192,
            kv_hdr_size: 224,
            kv_raw_size: 196,
            kv_magic_off: 160,
            kv_len_off: 164,
            kv_crc_off: 168,
            kv_name_len_off: 172,
            kv_value_len_off: 176,
            kv_status_size: 160,
            store_status_size: 96,
            dirty_status_size: 96,
            sec_remain_threshold: 288,
        },
        _ => return None,
    };
    Some(l)
}

/// Length of the sector header block written from `sec_magic_off` onwards.
#[inline]
pub(crate) const fn kv_sec_magic_block_len(l: &KvLayout) -> usize {
    (l.sec_raw_size - l.sec_magic_off) as usize
}

/// Length of the KV header block written from `kv_magic_off` onwards.
#[inline]
pub(crate) const fn kv_hdr_magic_block_len(l: &KvLayout) -> usize {
    (l.kv_raw_size - l.kv_magic_off) as usize
}

// ---------------------------------------------------------------------------
// TSDB layout
// ---------------------------------------------------------------------------

/// Byte layout of FlashDB's TSDB structures for one write granularity and
/// timestamp width.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TsLayout {
    pub sec_hdr_size: u32,
    pub sec_raw_size: u32,
    pub sec_store_off: u32,
    pub sec_magic_off: u32,
    pub sec_start_time_off: u32,
    pub sec_end0_time_off: u32,
    pub sec_end0_idx_off: u32,
    pub sec_end0_stat_off: u32,
    pub sec_end1_time_off: u32,
    pub sec_end1_idx_off: u32,
    pub sec_end1_stat_off: u32,
    pub sec_store_status_size: u32,
    pub tsl_status_size: u32,
    /// `TSL_UINT32_ALIGN_SIZE`: alignment of a u32 field in the TS structs.
    pub uint32_align: u32,
    /// `TSL_TIME_ALIGN_SIZE`: alignment of one timestamp.
    pub time_align: u32,
    pub log_idx_size: u32,
    pub log_idx_raw_size: u32,
    pub log_idx_status_size: u32,
    pub log_idx_ts_off: u32,
    pub log_idx_loglen_off: u32,
    pub log_idx_logaddr_off: u32,
}

/// TSDB layout for 32-bit timestamps (`fdb_time_t == int32_t`).
pub(crate) const fn ts_layout_32(wg_bits: u32) -> Option<TsLayout> {
    let l = match wg_bits {
        1 => TsLayout {
            sec_hdr_size: 32,
            sec_raw_size: 32,
            sec_store_off: 0,
            sec_magic_off: 1,
            sec_start_time_off: 5,
            sec_end0_time_off: 9,
            sec_end0_idx_off: 13,
            sec_end0_stat_off: 17,
            sec_end1_time_off: 18,
            sec_end1_idx_off: 22,
            sec_end1_stat_off: 26,
            sec_store_status_size: 1,
            tsl_status_size: 1,
            uint32_align: 4,
            time_align: 4,
            log_idx_size: 16,
            log_idx_raw_size: 16,
            log_idx_status_size: 1,
            log_idx_ts_off: 4,
            log_idx_loglen_off: 8,
            log_idx_logaddr_off: 12,
        },
        8 => TsLayout {
            sec_hdr_size: 44,
            sec_raw_size: 44,
            sec_store_off: 0,
            sec_magic_off: 3,
            sec_start_time_off: 7,
            sec_end0_time_off: 11,
            sec_end0_idx_off: 15,
            sec_end0_stat_off: 19,
            sec_end1_time_off: 24,
            sec_end1_idx_off: 28,
            sec_end1_stat_off: 32,
            sec_store_status_size: 3,
            tsl_status_size: 5,
            uint32_align: 4,
            time_align: 4,
            log_idx_size: 20,
            log_idx_raw_size: 20,
            log_idx_status_size: 5,
            log_idx_ts_off: 8,
            log_idx_loglen_off: 12,
            log_idx_logaddr_off: 16,
        },
        32 => TsLayout {
            sec_hdr_size: 80,
            sec_raw_size: 80,
            sec_store_off: 0,
            sec_magic_off: 12,
            sec_start_time_off: 16,
            sec_end0_time_off: 20,
            sec_end0_idx_off: 24,
            sec_end0_stat_off: 28,
            sec_end1_time_off: 48,
            sec_end1_idx_off: 52,
            sec_end1_stat_off: 56,
            sec_store_status_size: 12,
            tsl_status_size: 20,
            uint32_align: 4,
            time_align: 4,
            log_idx_size: 32,
            log_idx_raw_size: 32,
            log_idx_status_size: 20,
            log_idx_ts_off: 20,
            log_idx_loglen_off: 24,
            log_idx_logaddr_off: 28,
        },
        64 => TsLayout {
            sec_hdr_size: 160,
            sec_raw_size: 160,
            sec_store_off: 0,
            sec_magic_off: 24,
            sec_start_time_off: 32,
            sec_end0_time_off: 40,
            sec_end0_idx_off: 48,
            sec_end0_stat_off: 56,
            sec_end1_time_off: 96,
            sec_end1_idx_off: 104,
            sec_end1_stat_off: 112,
            sec_store_status_size: 24,
            tsl_status_size: 40,
            uint32_align: 8,
            time_align: 8,
            log_idx_size: 56,
            log_idx_raw_size: 56,
            log_idx_status_size: 40,
            log_idx_ts_off: 40,
            log_idx_loglen_off: 44,
            log_idx_logaddr_off: 48,
        },
        128 => TsLayout {
            sec_hdr_size: 320,
            sec_raw_size: 320,
            sec_store_off: 0,
            sec_magic_off: 48,
            sec_start_time_off: 64,
            sec_end0_time_off: 80,
            sec_end0_idx_off: 96,
            sec_end0_stat_off: 112,
            sec_end1_time_off: 192,
            sec_end1_idx_off: 208,
            sec_end1_stat_off: 224,
            sec_store_status_size: 48,
            tsl_status_size: 80,
            uint32_align: 16,
            time_align: 16,
            log_idx_size: 96,
            log_idx_raw_size: 96,
            log_idx_status_size: 80,
            log_idx_ts_off: 80,
            log_idx_loglen_off: 84,
            log_idx_logaddr_off: 88,
        },
        256 => TsLayout {
            sec_hdr_size: 640,
            sec_raw_size: 640,
            sec_store_off: 0,
            sec_magic_off: 96,
            sec_start_time_off: 128,
            sec_end0_time_off: 160,
            sec_end0_idx_off: 192,
            sec_end0_stat_off: 224,
            sec_end1_time_off: 384,
            sec_end1_idx_off: 416,
            sec_end1_stat_off: 448,
            sec_store_status_size: 96,
            tsl_status_size: 160,
            uint32_align: 32,
            time_align: 32,
            log_idx_size: 192,
            log_idx_raw_size: 192,
            log_idx_status_size: 160,
            log_idx_ts_off: 160,
            log_idx_loglen_off: 164,
            log_idx_logaddr_off: 168,
        },
        _ => return None,
    };
    Some(l)
}

/// TSDB layout for 64-bit timestamps (`FDB_USING_TIMESTAMP_64BIT`).
pub(crate) const fn ts_layout_64(wg_bits: u32) -> Option<TsLayout> {
    let l = match wg_bits {
        1 => TsLayout {
            sec_hdr_size: 44,
            sec_raw_size: 44,
            sec_store_off: 0,
            sec_magic_off: 1,
            sec_start_time_off: 5,
            sec_end0_time_off: 13,
            sec_end0_idx_off: 21,
            sec_end0_stat_off: 25,
            sec_end1_time_off: 26,
            sec_end1_idx_off: 34,
            sec_end1_stat_off: 38,
            sec_store_status_size: 1,
            tsl_status_size: 1,
            uint32_align: 4,
            time_align: 8,
            log_idx_size: 24,
            log_idx_raw_size: 24,
            log_idx_status_size: 1,
            log_idx_ts_off: 8,
            log_idx_loglen_off: 16,
            log_idx_logaddr_off: 20,
        },
        8 => TsLayout {
            sec_hdr_size: 56,
            sec_raw_size: 56,
            sec_store_off: 0,
            sec_magic_off: 3,
            sec_start_time_off: 7,
            sec_end0_time_off: 15,
            sec_end0_idx_off: 23,
            sec_end0_stat_off: 27,
            sec_end1_time_off: 32,
            sec_end1_idx_off: 40,
            sec_end1_stat_off: 44,
            sec_store_status_size: 3,
            tsl_status_size: 5,
            uint32_align: 4,
            time_align: 8,
            log_idx_size: 24,
            log_idx_raw_size: 24,
            log_idx_status_size: 5,
            log_idx_ts_off: 8,
            log_idx_loglen_off: 16,
            log_idx_logaddr_off: 20,
        },
        32 => TsLayout {
            sec_hdr_size: 92,
            sec_raw_size: 92,
            sec_store_off: 0,
            sec_magic_off: 12,
            sec_start_time_off: 16,
            sec_end0_time_off: 24,
            sec_end0_idx_off: 32,
            sec_end0_stat_off: 36,
            sec_end1_time_off: 56,
            sec_end1_idx_off: 64,
            sec_end1_stat_off: 68,
            sec_store_status_size: 12,
            tsl_status_size: 20,
            uint32_align: 4,
            time_align: 8,
            log_idx_size: 40,
            log_idx_raw_size: 40,
            log_idx_status_size: 20,
            log_idx_ts_off: 24,
            log_idx_loglen_off: 32,
            log_idx_logaddr_off: 36,
        },
        64 => TsLayout {
            sec_hdr_size: 160,
            sec_raw_size: 160,
            sec_store_off: 0,
            sec_magic_off: 24,
            sec_start_time_off: 32,
            sec_end0_time_off: 40,
            sec_end0_idx_off: 48,
            sec_end0_stat_off: 56,
            sec_end1_time_off: 96,
            sec_end1_idx_off: 104,
            sec_end1_stat_off: 112,
            sec_store_status_size: 24,
            tsl_status_size: 40,
            uint32_align: 8,
            time_align: 8,
            log_idx_size: 56,
            log_idx_raw_size: 56,
            log_idx_status_size: 40,
            log_idx_ts_off: 40,
            log_idx_loglen_off: 48,
            log_idx_logaddr_off: 52,
        },
        128 => TsLayout {
            sec_hdr_size: 320,
            sec_raw_size: 320,
            sec_store_off: 0,
            sec_magic_off: 48,
            sec_start_time_off: 64,
            sec_end0_time_off: 80,
            sec_end0_idx_off: 96,
            sec_end0_stat_off: 112,
            sec_end1_time_off: 192,
            sec_end1_idx_off: 208,
            sec_end1_stat_off: 224,
            sec_store_status_size: 48,
            tsl_status_size: 80,
            uint32_align: 16,
            time_align: 16,
            log_idx_size: 96,
            log_idx_raw_size: 96,
            log_idx_status_size: 80,
            log_idx_ts_off: 80,
            log_idx_loglen_off: 88,
            log_idx_logaddr_off: 92,
        },
        256 => TsLayout {
            sec_hdr_size: 640,
            sec_raw_size: 640,
            sec_store_off: 0,
            sec_magic_off: 96,
            sec_start_time_off: 128,
            sec_end0_time_off: 160,
            sec_end0_idx_off: 192,
            sec_end0_stat_off: 224,
            sec_end1_time_off: 384,
            sec_end1_idx_off: 416,
            sec_end1_stat_off: 448,
            sec_store_status_size: 96,
            tsl_status_size: 160,
            uint32_align: 32,
            time_align: 32,
            log_idx_size: 192,
            log_idx_raw_size: 192,
            log_idx_status_size: 160,
            log_idx_ts_off: 160,
            log_idx_loglen_off: 168,
            log_idx_logaddr_off: 172,
        },
        _ => return None,
    };
    Some(l)
}

// ---------------------------------------------------------------------------
// Little-endian field helpers (FlashDB's default byte order)
// ---------------------------------------------------------------------------

#[inline]
pub(crate) fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[inline]
pub(crate) fn put_u32(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

#[inline]
pub(crate) fn get_i32(buf: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[inline]
pub(crate) fn put_i32(buf: &mut [u8], off: usize, value: i32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

#[inline]
pub(crate) fn get_i64(buf: &[u8], off: usize) -> i64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    i64::from_le_bytes(b)
}

#[inline]
pub(crate) fn put_i64(buf: &mut [u8], off: usize, value: i64) {
    buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_vectors() {
        // Standard CRC-32 (reflected, poly 0xEDB88320), same as zlib.
        assert_eq!(crc32(0, b""), 0x0000_0000);
        assert_eq!(crc32(0, b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(0, &[0u8; 32]), 0x190A_55AD);
        // chained calls must equal one call
        let data = b"chain me across chunks";
        assert_eq!(
            crc32(crc32(crc32(0, &data[..5]), &data[5..11]), &data[11..]),
            crc32(0, data)
        );
    }

    #[test]
    fn status_roundtrip_wg1() {
        for status in 1..6 {
            let mut scratch = [0u8; 32];
            let mut table = [0xFFu8; 1];
            let (off, bytes) = encode_status(1, status, &mut scratch);
            table[off..off + bytes.len()].copy_from_slice(bytes);
            assert_eq!(decode_status(1, 6, &table), status);
        }
    }

    #[test]
    fn status_roundtrip_wg8() {
        let mut table = [0xFFu8; 5];
        for status in 1..6 {
            let mut scratch = [0u8; 32];
            let (off, bytes) = encode_status(8, status, &mut scratch);
            table[off..off + bytes.len()].copy_from_slice(bytes);
            assert_eq!(decode_status(8, 6, &table), status);
        }
    }

    #[test]
    fn status_jump_forward() {
        // Erased -> Write directly (2) must decode as 2 even though chunk 0
        // is still 0xFF.
        let mut table = [0xFFu8; 5];
        let mut scratch = [0u8; 32];
        let (off, bytes) = encode_status(8, 2, &mut scratch);
        table[off..off + bytes.len()].copy_from_slice(bytes);
        assert_eq!(decode_status(8, 6, &table), 2);
    }

    #[test]
    fn layout_tables_present_for_all_granularities() {
        for wg in [1u32, 8, 32, 64, 128, 256] {
            let kv = kv_layout(wg).unwrap();
            assert!(kv.kv_hdr_size >= kv.kv_raw_size);
            assert!(kv.sec_hdr_size >= kv.sec_raw_size);
            assert_eq!(kv.kv_status_size, status_table_size(6, wg) as u32);
            assert!(ts_layout_32(wg).is_some());
            assert!(ts_layout_64(wg).is_some());
        }
        assert!(kv_layout(16).is_none());
    }
}
