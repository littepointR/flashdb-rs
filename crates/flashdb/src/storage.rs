//! Database geometry and the typed flash access layer.

use crate::error::{Error, Result};
use crate::flash::Flash;
use crate::low::{self, align_down, status_table_size, wg_bytes};

/// Database geometry and mount options.
///
/// `sec_size` is the erase block used by the database (FlashDB's "sector");
/// it must be a power of two and a multiple of the physical erase block.
/// `max_size` is the size of the database partition.
///
/// ```
/// use flashdb::Config;
///
/// // 4 sectors of 4 KiB that must never be formatted implicitly.
/// let cfg = Config::new(4096, 16 * 1024).not_formattable(true);
/// assert_eq!(cfg.sector_count(), 4);
/// assert!(cfg.not_formattable);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Erase-block size used by the database.
    pub sec_size: u32,
    /// Total partition size available to the database.
    pub max_size: u32,
    /// Never format implicitly during mount (FlashDB's
    /// `FDB_KVDB_CTRL_SET_NOT_FORMAT` / `FDB_TSDB_CTRL_SET_NOT_FORMAT`).
    ///
    /// By default a mount repairs damaged or missing sector headers by
    /// formatting them (and formats a blank partition). With this flag set,
    /// mounting instead fails with [`Error::NotFormatted`] and leaves the
    /// flash untouched. Explicit `format` / `clean` calls are not affected.
    pub not_formattable: bool,
}

impl Config {
    /// Create a configuration with implicit formatting enabled.
    pub const fn new(sec_size: u32, max_size: u32) -> Self {
        Self {
            sec_size,
            max_size,
            not_formattable: false,
        }
    }

    /// Builder form of [`Config::not_formattable`].
    #[must_use]
    pub const fn not_formattable(mut self, not_formattable: bool) -> Self {
        self.not_formattable = not_formattable;
        self
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
        let Config {
            sec_size, max_size, ..
        } = config;
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
    pub(crate) fn read_status(&mut self, addr: u32, status_num: usize) -> Result<usize, F::Error> {
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
        let wg = F::WRITE_GRAN_BITS;
        if wg <= 8 {
            // One write-granule chunk is a single byte for both supported small
            // granularities: wg == 8 stores one byte per status index, wg == 1
            // stores the whole table bit-packed (MSB first). Encode the byte
            // here instead of clearing a 32-byte scratch and calling
            // encode_status: this is the hottest write path (every record
            // status transition) and the scratch made every inlined call site
            // emit a 32-byte __aeabi_memclr.
            let (off, byte) = if wg == 1 {
                let bit = status - 1;
                (bit / 8, 0xFFu8 >> ((bit % 8) + 1))
            } else {
                (status - 1, 0u8)
            };
            return self.write(addr + off as u32, &[byte]);
        }
        let mut scratch = [0u8; 32];
        let (offset, bytes) = low::encode_status(wg, status, &mut scratch);
        // `bytes` borrows the local scratch, `self` is a disjoint borrow, so the
        // status can be written straight out of the scratch buffer (a second
        // 32-byte copy-out buffer only grows the caller's stack frame).
        let len = bytes.len();
        self.write(addr + offset as u32, &bytes[..len])
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

#[cfg(test)]
mod tests {
    use super::*;
    use core::convert::Infallible;

    struct MemFlash<const WG: u32, const ERASED: u8 = 0xFF> {
        data: [u8; 2048],
    }

    impl<const WG: u32, const ERASED: u8> MemFlash<WG, ERASED> {
        fn new() -> Self {
            Self {
                data: [ERASED; 2048],
            }
        }
    }

    impl<const WG: u32, const ERASED: u8> Flash for MemFlash<WG, ERASED> {
        type Error = Infallible;
        const WRITE_GRAN_BITS: u32 = WG;
        const ERASED_BYTE: u8 = ERASED;

        fn read(&mut self, addr: u32, buf: &mut [u8]) -> core::result::Result<(), Self::Error> {
            let s = addr as usize;
            buf.copy_from_slice(&self.data[s..s + buf.len()]);
            Ok(())
        }

        fn write(&mut self, addr: u32, buf: &[u8]) -> core::result::Result<(), Self::Error> {
            let s = addr as usize;
            for (i, b) in buf.iter().enumerate() {
                self.data[s + i] &= *b;
            }
            Ok(())
        }

        fn erase(&mut self, addr: u32, len: u32) -> core::result::Result<(), Self::Error> {
            let s = addr as usize;
            self.data[s..s + len as usize].fill(ERASED);
            Ok(())
        }
    }

    #[test]
    fn config_validation_and_getters() {
        assert_eq!(Config::new(0, 1024).sector_count(), 0);
        let cfg = Config::new(512, 2048).not_formattable(true);
        assert_eq!(cfg.sector_count(), 4);
        assert!(cfg.not_formattable);

        assert!(matches!(
            Storage::new(MemFlash::<8>::new(), Config::new(0, 2048)),
            Err(Error::InvalidConfig)
        ));
        assert!(matches!(
            Storage::new(MemFlash::<8>::new(), Config::new(500, 2000)),
            Err(Error::InvalidConfig)
        ));
        assert!(matches!(
            Storage::new(MemFlash::<8>::new(), Config::new(512, 512)),
            Err(Error::InvalidConfig)
        ));
        assert!(matches!(
            Storage::new(MemFlash::<8>::new(), Config::new(512, 1500)),
            Err(Error::InvalidConfig)
        ));
        assert!(matches!(
            Storage::new(MemFlash::<16>::new(), Config::new(512, 2048)),
            Err(Error::UnsupportedWriteGran)
        ));
        assert!(matches!(
            Storage::new(MemFlash::<8, 0x00>::new(), Config::new(512, 2048)),
            Err(Error::InvalidConfig)
        ));
    }

    #[test]
    fn bounds_checking_and_sector_ops() {
        let mut st = Storage::new(MemFlash::<8>::new(), Config::new(512, 2048)).unwrap();
        assert_eq!(st.sec_size(), 512);
        assert_eq!(st.max_size(), 2048);
        assert_eq!(st.sector_count(), 4);
        assert!(st.is_sector_aligned(0));
        assert!(st.is_sector_aligned(1024));
        assert!(!st.is_sector_aligned(513));

        let mut buf = [0u8; 16];
        assert_eq!(st.read(2040, &mut buf), Err(Error::OutOfBounds));
        assert_eq!(st.read(u32::MAX - 4, &mut buf), Err(Error::OutOfBounds));
        assert_eq!(st.write(2040, &buf), Err(Error::OutOfBounds));
        assert_eq!(st.erase(1536, 1024), Err(Error::OutOfBounds));

        st.write(512, &[0x12, 0x34]).unwrap();
        st.erase_sector(512).unwrap();
        let mut verify = [0u8; 2];
        st.read(512, &mut verify).unwrap();
        assert_eq!(verify, [0xFF, 0xFF]);
        let _flash = st.into_flash();
    }

    fn exercise_write_align_and_status<const WG: u32>() {
        let mut st = Storage::new(MemFlash::<WG>::new(), Config::new(512, 2048)).unwrap();
        let chunk = wg_bytes(WG);

        // Empty write_align, partial chunk, exact chunk, multi-chunk + tail.
        st.write_align(0, &[]).unwrap();
        st.write_align(0, &[0xAA, 0x55, 0x0F]).unwrap();
        let mut back = [0u8; 32];
        st.read(0, &mut back[..chunk.max(3)]).unwrap();
        assert_eq!(&back[..3], &[0xAA, 0x55, 0x0F]);
        for &b in &back[3..chunk.max(3)] {
            assert_eq!(b, 0xFF);
        }

        st.erase_sector(0).unwrap();
        let payload = [0x5Au8; 35];
        st.write_align(0, &payload[..chunk]).unwrap();
        st.erase_sector(0).unwrap();
        st.write_align(0, &payload).unwrap();
        let mut back35 = [0u8; 35];
        st.read(0, &mut back35).unwrap();
        assert_eq!(back35, payload);

        // Status table: status 0 is a no-op, then step through states 1..5.
        st.erase_sector(512).unwrap();
        assert_eq!(st.read_status(512, 5).unwrap(), 0);
        st.write_status(512, 5, 0).unwrap();
        assert_eq!(st.read_status(512, 5).unwrap(), 0);
        for s in 1..5 {
            st.write_status(512, 5, s).unwrap();
            assert_eq!(st.read_status(512, 5).unwrap(), s);
        }
    }

    #[test]
    fn write_align_and_status_across_all_granularities() {
        exercise_write_align_and_status::<1>();
        exercise_write_align_and_status::<8>();
        exercise_write_align_and_status::<32>();
        exercise_write_align_and_status::<64>();
        exercise_write_align_and_status::<128>();
        exercise_write_align_and_status::<256>();
    }
}
