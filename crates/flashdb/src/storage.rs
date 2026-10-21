//! Database geometry and the typed flash access layer.

use crate::error::{Error, Result};
use crate::flash::Flash;
use crate::low::{self, align_down, status_table_size, wg_bytes};

/// Database geometry.
///
/// `sec_size` is the erase block used by the database (FlashDB's "sector");
/// it must be a power of two and a multiple of the physical erase block.
/// `max_size` is the size of the database partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Erase-block size used by the database.
    pub sec_size: u32,
    /// Total partition size available to the database.
    pub max_size: u32,
}

impl Config {
    /// Create a configuration.
    pub const fn new(sec_size: u32, max_size: u32) -> Self {
        Self { sec_size, max_size }
    }

    /// Number of sectors in the database.
    pub const fn sector_count(&self) -> u32 {
        if self.sec_size == 0 {
            0
        } else {
            self.max_size / self.sec_size
        }
    }
}

/// Typed access to the flash partition backing one database.
///
/// It adds three things on top of [`Flash`]:
///
/// * bounds checking of every access against the partition size,
/// * write-granularity aware aligned writes (FlashDB's
///   `_fdb_flash_write_align`),
/// * status-field read/write (FlashDB's `_fdb_read/write_status`) with the
///   correct encoding for the driver's write granularity.
pub(crate) struct Storage<F: Flash> {
    flash: F,
    sec_size: u32,
    max_size: u32,
    /// Start address of the oldest sector, used by both databases.
    pub(crate) oldest_addr: u32,
}

impl<F: Flash> Storage<F> {
    pub(crate) fn new(flash: F, config: Config) -> Result<Self, F::Error> {
        let Config { sec_size, max_size } = config;
        if sec_size == 0 || !sec_size.is_power_of_two() {
            return Err(Error::InvalidConfig);
        }
        if max_size < sec_size * 2 || max_size % sec_size != 0 {
            return Err(Error::InvalidConfig);
        }
        if low::kv_layout(F::WRITE_GRAN_BITS).is_none() {
            return Err(Error::UnsupportedWriteGran);
        }
        if F::ERASED_BYTE != 0xFF {
            // The on-flash format (magic words, status tables, erased-value
            // padding) is defined in terms of 0xFF-erased flash, exactly like
            // FlashDB. Reject drivers that model erased flash differently
            // instead of silently producing an incompatible format.
            return Err(Error::InvalidConfig);
        }
        Ok(Self {
            flash,
            sec_size,
            max_size,
            oldest_addr: 0,
        })
    }

    #[inline]
    pub(crate) fn sec_size(&self) -> u32 {
        self.sec_size
    }

    #[inline]
    pub(crate) fn max_size(&self) -> u32 {
        self.max_size
    }

    #[inline]
    pub(crate) fn sector_count(&self) -> u32 {
        self.max_size / self.sec_size
    }

    #[inline]
    fn check(&self, addr: u32, len: usize) -> Result<(), F::Error> {
        if (addr as u64) + (len as u64) > self.max_size as u64 {
            Err(Error::OutOfBounds)
        } else {
            Ok(())
        }
    }

    #[inline]
    pub(crate) fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), F::Error> {
        self.check(addr, buf.len())?;
        self.flash.read(addr, buf).map_err(Error::FlashRead)
    }

    #[inline]
    pub(crate) fn write(&mut self, addr: u32, buf: &[u8]) -> Result<(), F::Error> {
        self.check(addr, buf.len())?;
        self.flash.write(addr, buf).map_err(Error::FlashWrite)
    }

    #[inline]
    pub(crate) fn erase(&mut self, addr: u32, len: u32) -> Result<(), F::Error> {
        self.check(addr, len as usize)?;
        self.flash.erase(addr, len).map_err(Error::FlashErase)
    }

    /// Erase one database sector.
    #[inline]
    pub(crate) fn erase_sector(&mut self, addr: u32) -> Result<(), F::Error> {
        debug_assert_eq!(addr % self.sec_size, 0);
        self.erase(addr, self.sec_size)
    }

    /// FlashDB's `_fdb_flash_write_align`: write `buf`, padding the final
    /// partial write-granule with erased bytes.
    pub(crate) fn write_align(&mut self, addr: u32, buf: &[u8]) -> Result<(), F::Error> {
        let chunk = wg_bytes(F::WRITE_GRAN_BITS);
        if chunk == 1 {
            return self.write(addr, buf);
        }
        debug_assert_eq!(addr as usize % chunk, 0);
        let full = align_down(buf.len(), chunk);
        if full > 0 {
            self.write(addr, &buf[..full])?;
        }
        let remain = buf.len() - full;
        if remain > 0 {
            let mut pad = [F::ERASED_BYTE; 32];
            pad[..remain].copy_from_slice(&buf[full..]);
            self.write(addr + full as u32, &pad[..chunk])?;
        }
        Ok(())
    }

    /// Read a status table and decode it into a status index.
    ///
    /// The buffer size is chosen at compile time from the flash write
    /// granularity, so no dynamic memory is needed and the unused branches are
    /// removed by the optimiser for every concrete flash type.
    pub(crate) fn read_status(
        &mut self,
        addr: u32,
        status_num: usize,
    ) -> Result<usize, F::Error> {
        let wg = F::WRITE_GRAN_BITS;
        let n = status_table_size(status_num, wg);
        if wg <= 32 {
            let mut table = [0u8; 20];
            self.read(addr, &mut table[..n])?;
            Ok(low::decode_status(wg, status_num, &table[..n]))
        } else if wg <= 64 {
            let mut table = [0u8; 40];
            self.read(addr, &mut table[..n])?;
            Ok(low::decode_status(wg, status_num, &table[..n]))
        } else if wg <= 128 {
            let mut table = [0u8; 80];
            self.read(addr, &mut table[..n])?;
            Ok(low::decode_status(wg, status_num, &table[..n]))
        } else {
            let mut table = [0u8; 160];
            self.read(addr, &mut table[..n])?;
            Ok(low::decode_status(wg, status_num, &table[..n]))
        }
    }

    /// Write a status index into a status table.
    ///
    /// Writing status 0 is a no-op: 0 is the erased value.
    pub(crate) fn write_status(
        &mut self,
        addr: u32,
        status_num: usize,
        status: usize,
    ) -> Result<(), F::Error> {
        debug_assert!(status < status_num);
        if status == 0 {
            return Ok(());
        }
        let mut scratch = [0u8; 32];
        let (offset, bytes) = low::encode_status(F::WRITE_GRAN_BITS, status, &mut scratch);
        // Copy out of the scratch borrow so the flash borrow is free.
        let mut out = [0u8; 32];
        let len = bytes.len();
        out[..len].copy_from_slice(bytes);
        self.write(addr + offset as u32, &out[..len])
    }

    /// Whether `addr` is sector aligned.
    #[inline]
    pub(crate) fn is_sector_aligned(&self, addr: u32) -> bool {
        addr % self.sec_size == 0
    }
}

impl<F: Flash> Storage<F> {
    /// Consume the storage and return the flash driver.
    pub(crate) fn into_flash(self) -> F {
        self.flash
    }
}
