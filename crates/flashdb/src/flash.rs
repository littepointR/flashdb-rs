//! Flash abstraction.
//!
//! FlashDB-C reaches the flash through FAL, a global registry of named flash
//! devices and partitions (`fal_partition_read(part, addr, ..)`). This crate
//! instead uses a plain trait: a database *owns* its flash view through
//! generics, which removes global mutable state, linker-time device tables and
//! the need for a lock around the underlying driver.
//!
//! A `Flash` implementation represents one database partition: addresses are
//! relative to the start of the partition. Implementors are responsible for
//! translating those addresses to the physical flash.
//!
//! ```
//! use flashdb::Flash;
//!
//! /// A NOR flash driver with 4 KiB erase blocks and byte programming.
//! struct MyNorFlash { /* SPI handle, CS pin, ... */ }
//!
//! impl Flash for MyNorFlash {
//!     type Error = core::convert::Infallible;
//!
//!     const WRITE_GRAN_BITS: u32 = 8;
//!
//!     fn read(&mut self, _addr: u32, _buf: &mut [u8]) -> Result<(), Self::Error> {
//!         Ok(())
//!     }
//!     fn write(&mut self, _addr: u32, _buf: &[u8]) -> Result<(), Self::Error> {
//!         Ok(())
//!     }
//!     fn erase(&mut self, _addr: u32, _len: u32) -> Result<(), Self::Error> {
//!         Ok(())
//!     }
//! }
//! ```
//!
//! [`Flash`] is implemented for `&mut F as well, so a database can borrow a
//! driver instead of taking ownership:
//!
//! ```
//! # use flashdb::Flash;
//! # struct D;
//! # impl Flash for D {
//! #   type Error = core::convert::Infallible;
//! #   fn read(&mut self, _: u32, _: &mut [u8]) -> Result<(), Self::Error> { Ok(()) }
//! #   fn write(&mut self, _: u32, _: &[u8]) -> Result<(), Self::Error> { Ok(()) }
//! #   fn erase(&mut self, _: u32, _: u32) -> Result<(), Self::Error> { Ok(()) }
//! # }
//! fn take_borrowed(flash: &mut D) {
//!     // `&mut D: Flash` because of the blanket impl.
//!     let _db = flashdb::Kvdb::mount(flash, flashdb::Config::new(4096, 16384));
//! }
//! ```

/// A flash memory region that can be read, programmed and erased.
///
/// # Contract
///
/// * `read` must return the raw contents of the flash.
/// * `write` must only clear bits (`new = old & value`); programming a byte to
///   `0xFF` must leave it unchanged. This is true of NOR and of most NAND.
/// * `erase` must set a whole erase region to [`ERASED_BYTE`](Flash::ERASED_BYTE).
/// * Addresses are relative to the start of the database partition.
/// * Writes passed to `write` are always aligned to
///   `WRITE_GRAN_BITS / 8` bytes, except that the *last* chunk of an
///   unaligned value is padded to the granule by the database before being
///   handed to the driver.
pub trait Flash {
    /// Driver specific error.
    type Error;

    /// Program granularity in **bits**: 1, 8, 32, 64, 128 or 256.
    ///
    /// This mirrors FlashDB's `FDB_WRITE_GRAN`. `1` means "any byte may be
    /// programmed" (classic SPI NOR); `8` is used by STM32F2/F4 style on-chip
    /// flash; `32` by STM32F1; `64`/`128`/`256` by F7/H5/H7 respectively.
    ///
    /// 1, 8 and 32 are fully supported. The larger granules use the same
    /// layout tables and are supported by the algorithms, but are less
    /// commonly benchmarked.
    const WRITE_GRAN_BITS: u32 = 8;

    /// Byte value of erased flash. FlashDB requires `0xFF`.
    const ERASED_BYTE: u8 = 0xFF;

    /// Read `buf.len()` bytes from `addr`.
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error>;

    /// Program `buf` at `addr`. `buf.len()` is a multiple of the write granule,
    /// except for the final padded chunk of a value (see trait docs).
    fn write(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error>;

    /// Erase `len` bytes starting at `addr`. `len` is a multiple of the
    /// database sector size.
    fn erase(&mut self, addr: u32, len: u32) -> Result<(), Self::Error>;
}

impl<F: Flash + ?Sized> Flash for &mut F {
    type Error = F::Error;

    const WRITE_GRAN_BITS: u32 = F::WRITE_GRAN_BITS;
    const ERASED_BYTE: u8 = F::ERASED_BYTE;

    #[inline]
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        (**self).read(addr, buf)
    }

    #[inline]
    fn write(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        (**self).write(addr, buf)
    }

    #[inline]
    fn erase(&mut self, addr: u32, len: u32) -> Result<(), Self::Error> {
        (**self).erase(addr, len)
    }
}

/// A sub-region (partition) view over an underlying [`Flash`] driver.
///
/// FlashDB-C uses the global FAL partition table to map partition-relative
/// offsets (`0..max_size`) onto physical chip offsets (`offset..offset + size`).
/// `Partition<F>` provides the same offset translation without global state:
/// wrap an owned driver (`Partition<MySpiFlash>`) or a borrowed one
/// (`Partition<&mut MySpiFlash>`) and pass it to [`Kvdb::mount`](crate::Kvdb::mount)
/// or [`Tsdb::mount`](crate::Tsdb::mount).
///
/// ```
/// use flashdb::{Config, Flash, Kvdb, Partition, Tsdb};
///
/// # struct Chip([u8; 32768]);
/// # impl Flash for Chip {
/// #     type Error = core::convert::Infallible;
/// #     fn read(&mut self, a: u32, b: &mut [u8]) -> Result<(), Self::Error> {
/// #         b.copy_from_slice(&self.0[a as usize..a as usize + b.len()]); Ok(())
/// #     }
/// #     fn write(&mut self, a: u32, b: &[u8]) -> Result<(), Self::Error> {
/// #         for (i, x) in b.iter().enumerate() { self.0[a as usize + i] &= *x; } Ok(())
/// #     }
/// #     fn erase(&mut self, a: u32, l: u32) -> Result<(), Self::Error> {
/// #         self.0[a as usize..(a + l) as usize].fill(0xFF); Ok(())
/// #     }
/// # }
/// let mut chip = Chip([0xFF; 32768]);
/// {
///     let kv_part = Partition::new(&mut chip, 0, 16384);
///     let mut kv = Kvdb::mount(kv_part, Config::new(4096, 16384)).unwrap();
///     kv.set(b"k", b"v").unwrap();
/// }
/// {
///     let ts_part = Partition::new(&mut chip, 16384, 16384);
///     let mut ts = Tsdb::mount(ts_part, Config::new(4096, 16384), 64).unwrap();
///     ts.append(1, b"log").unwrap();
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition<F> {
    flash: F,
    offset: u32,
}

impl<F> Partition<F> {
    /// Create a partition view starting at byte `offset` of `flash`.
    ///
    /// Bounds against the partition length are enforced by the database's
    /// [`Config::max_size`](crate::Config#structfield.max_size). The `_size`
    /// parameter documents the partition capacity at the call site.
    #[inline]
    pub const fn new(flash: F, offset: u32, _size: u32) -> Self {
        Self { flash, offset }
    }

    /// Byte offset of this partition inside the underlying flash.
    #[inline]
    pub const fn offset(&self) -> u32 {
        self.offset
    }

    /// Shared reference to the underlying flash driver.
    #[inline]
    pub const fn inner(&self) -> &F {
        &self.flash
    }

    /// Mutable reference to the underlying flash driver.
    #[inline]
    pub fn inner_mut(&mut self) -> &mut F {
        &mut self.flash
    }

    /// Consume the partition wrapper and return the underlying flash driver.
    #[inline]
    pub fn into_inner(self) -> F {
        self.flash
    }
}

impl<F: Flash> Flash for Partition<F> {
    type Error = F::Error;

    const WRITE_GRAN_BITS: u32 = F::WRITE_GRAN_BITS;
    const ERASED_BYTE: u8 = F::ERASED_BYTE;

    #[inline]
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.flash.read(self.offset + addr, buf)
    }

    #[inline]
    fn write(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        self.flash.write(self.offset + addr, buf)
    }

    #[inline]
    fn erase(&mut self, addr: u32, len: u32) -> Result<(), Self::Error> {
        self.flash.erase(self.offset + addr, len)
    }
}
