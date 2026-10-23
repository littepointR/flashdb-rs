//! RAM-backed flash simulator with operation accounting and fault injection.
//!
//! This crate is host/test infrastructure. It is `no_std` (no allocation) so
//! the same simulator can be used in bare-metal firmware harnesses and in
//! host benchmarks, and it deliberately models NOR semantics exactly like the
//! C reference harness: programming only clears bits, erasing sets bytes to
//! `0xFF`, and every flash operation is counted.
//!
//! ```
//! use flashdb_sim::{RamFlash, SimError};
//! use flashdb::Flash;
//!
//! let mut flash = RamFlash::<16384, 8>::new(4096);
//! flash.write(0, &[0x00]).unwrap();
//! assert_eq!(flash.data()[0], 0x00);
//! flash.erase(0, 4096).unwrap();
//! assert_eq!(flash.data()[0], 0xFF);
//! assert_eq!(flash.stats().erase_ops, 1);
//! ```

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core::fmt;
use flashdb::Flash;

/// Simulator error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimError {
    /// A fault was injected at this operation index.
    Injected,
    /// Access outside the simulated flash.
    OutOfBounds,
    /// Erase is not aligned to the simulated erase block.
    UnalignedErase,
    /// Write granularity violated (only checked when `strict` is enabled).
    UnalignedWrite,
}

impl fmt::Display for SimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SimError::Injected => f.write_str("simulated flash failure"),
            SimError::OutOfBounds => f.write_str("simulated flash access out of bounds"),
            SimError::UnalignedErase => f.write_str("erase not aligned to block"),
            SimError::UnalignedWrite => f.write_str("write not aligned to write granularity"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SimError {}

/// Flash operation counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Number of read operations.
    pub read_ops: u64,
    /// Total bytes read.
    pub read_bytes: u64,
    /// Number of write operations.
    pub write_ops: u64,
    /// Total bytes written (including no-op writes).
    pub write_bytes: u64,
    /// Number of erase operations.
    pub erase_ops: u64,
    /// Total bytes erased.
    pub erase_bytes: u64,
    /// Writes whose bytes were already identical (FlashDB writes erased
    /// padding bytes in some places; counted for transparency).
    pub noop_write_bytes: u64,
}

impl Stats {
    /// Weighted cost model used by the benchmark report:
    /// `read*1 + write*10 + erase*100` bytes.
    pub fn weighted_cost(&self) -> u64 {
        self.read_bytes + self.write_bytes * 10 + self.erase_bytes * 100
    }

    /// Reset all counters to zero.
    pub fn reset(&mut self) {
        *self = Stats::default();
    }
}

/// A RAM flash region.
///
/// * `SIZE` - size of the region in bytes.
/// * `WG` - write granularity in **bits**, exposed through
///   [`Flash::WRITE_GRAN_BITS`].
pub struct RamFlash<const SIZE: usize, const WG: u32> {
    data: [u8; SIZE],
    erase_size: u32,
    stats: Stats,
    strict_write: bool,
    /// Operation index at which the next access fails (`None` = no fault).
    fail_at_op: Option<u64>,
    op_index: u64,
    /// If set, only *writes* count towards the injected fault.
    fault_writes_only: bool,
    /// When set, a write/erase fault applies this many prefix bytes before
    /// returning `SimError::Injected` (simulating a mid-program or mid-erase
    /// power cut).
    tear_bytes: Option<usize>,
}

impl<const SIZE: usize, const WG: u32> RamFlash<SIZE, WG> {
    /// Create an erased (`0xFF`) flash with the given erase block size.
    pub const fn new(erase_size: u32) -> Self {
        Self {
            data: [0xFF; SIZE],
            erase_size,
            stats: Stats {
                read_ops: 0,
                read_bytes: 0,
                write_ops: 0,
                write_bytes: 0,
                erase_ops: 0,
                erase_bytes: 0,
                noop_write_bytes: 0,
            },
            strict_write: false,
            fail_at_op: None,
            op_index: 0,
            fault_writes_only: false,
            tear_bytes: None,
        }
    }

    /// Enable strict write-alignment checks (useful while developing a driver).
    pub fn set_strict_write(&mut self, strict: bool) {
        self.strict_write = strict;
    }

    /// Snapshot of the operation counters.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Reset the operation counters (keeps the flash contents intact).
    pub fn reset_stats(&mut self) {
        self.stats.reset();
    }

    /// Raw contents of the simulated flash region.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Mutable access to the raw simulated flash bytes (for crafting damaged
    /// or partially-erased images in tests).
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Erase-block size in bytes, as configured in [`RamFlash::new`].
    pub fn erase_size(&self) -> u32 {
        self.erase_size
    }

    /// Overwrite the contents directly (used to load a flash image captured
    /// from the C reference implementation).
    pub fn load(&mut self, bytes: &[u8]) -> Result<(), SimError> {
        if bytes.len() != SIZE {
            return Err(SimError::OutOfBounds);
        }
        self.data.copy_from_slice(bytes);
        Ok(())
    }

    /// Arm fault injection: the `n`-th flash operation from now on fails.
    pub fn fail_after_ops(&mut self, n: u64) {
        self.fail_at_op = Some(self.op_index + n);
        self.fault_writes_only = false;
        self.tear_bytes = None;
    }

    /// Arm fault injection: the `n`-th *write or erase* from now on fails.
    pub fn fail_after_writes(&mut self, n: u64) {
        self.fail_at_op = Some(self.op_index + n);
        self.fault_writes_only = true;
        self.tear_bytes = None;
    }

    /// Arm torn fault injection: when the `n`-th operation from now on is a
    /// write or erase, apply the first `tear_bytes` of the operation and then
    /// fail with [`SimError::Injected`].
    pub fn fail_torn_after_writes(&mut self, n: u64, tear_bytes: usize) {
        self.fail_at_op = Some(self.op_index + n);
        self.fault_writes_only = true;
        self.tear_bytes = Some(tear_bytes);
    }

    /// Disarm fault injection (the next operations succeed again).
    pub fn disarm_fault(&mut self) {
        self.fail_at_op = None;
        self.tear_bytes = None;
    }

    /// Number of flash operations performed since reset.
    pub fn op_index(&self) -> u64 {
        self.op_index
    }

    #[inline]
    fn begin_op(&mut self, is_write_like: bool) -> Result<(), SimError> {
        self.op_index += 1;
        if let Some(fail_at) = self.fail_at_op {
            if fail_at == self.op_index - 1 && (!self.fault_writes_only || is_write_like) {
                self.fail_at_op = None;
                return Err(SimError::Injected);
            }
        }
        Ok(())
    }

    #[inline]
    fn check_range(&self, addr: u32, len: usize) -> Result<(), SimError> {
        if (addr as u64) + (len as u64) > SIZE as u64 {
            Err(SimError::OutOfBounds)
        } else {
            Ok(())
        }
    }
}

impl<const SIZE: usize, const WG: u32> Flash for RamFlash<SIZE, WG> {
    type Error = SimError;

    const WRITE_GRAN_BITS: u32 = WG;

    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.begin_op(false)?;
        self.check_range(addr, buf.len())?;
        let start = addr as usize;
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        self.stats.read_ops += 1;
        self.stats.read_bytes += buf.len() as u64;
        Ok(())
    }

    fn write(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        if let Err(e) = self.begin_op(true) {
            if let Some(tear) = self.tear_bytes.take() {
                self.check_range(addr, buf.len())?;
                let n = if buf.len() <= 1 {
                    0
                } else {
                    tear.clamp(1, buf.len() - 1)
                };
                let start = addr as usize;
                for i in 0..n {
                    self.data[start + i] &= buf[i];
                }
            }
            return Err(e);
        }
        self.check_range(addr, buf.len())?;
        let gran = min_granule_bytes(WG);
        if self.strict_write {
            if addr as usize % gran != 0 || buf.len() % gran != 0 {
                return Err(SimError::UnalignedWrite);
            }
            if WG >= 8 {
                let start = addr as usize;
                for chunk in self.data[start..start + buf.len()].chunks(gran) {
                    if chunk.iter().any(|&b| b != 0xFF) {
                        return Err(SimError::UnalignedWrite);
                    }
                }
            }
        }
        let start = addr as usize;
        let mut changed = 0u64;
        for (i, b) in buf.iter().enumerate() {
            let slot = &mut self.data[start + i];
            let new = *slot & *b;
            if new != *slot {
                changed += 1;
            }
            *slot = new;
        }
        self.stats.write_ops += 1;
        self.stats.write_bytes += buf.len() as u64;
        self.stats.noop_write_bytes += buf.len() as u64 - changed;
        Ok(())
    }

    fn erase(&mut self, addr: u32, len: u32) -> Result<(), Self::Error> {
        if let Err(e) = self.begin_op(true) {
            if let Some(tear) = self.tear_bytes.take() {
                self.check_range(addr, len as usize)?;
                let n = tear.min(len as usize);
                let start = addr as usize;
                self.data[start..start + n].fill(0xFF);
            }
            return Err(e);
        }
        self.check_range(addr, len as usize)?;
        let er = self.erase_size as u64;
        if addr as u64 % er != 0 || len as u64 % er != 0 {
            return Err(SimError::UnalignedErase);
        }
        let start = addr as usize;
        self.data[start..start + len as usize].fill(0xFF);
        self.stats.erase_ops += 1;
        self.stats.erase_bytes += len as u64;
        Ok(())
    }
}

#[inline]
const fn min_granule_bytes(wg: u32) -> usize {
    if wg <= 8 {
        1
    } else {
        (wg / 8) as usize
    }
}

#[cfg(feature = "std")]
pub mod image {
    //! Helpers to persist/compare flash images from host tests.

    use std::fs;
    use std::io;
    use std::path::Path;

    /// Write a captured flash image to a file.
    pub fn save(path: impl AsRef<Path>, data: &[u8]) -> io::Result<()> {
        fs::write(path, data)
    }

    /// Read a flash image back from a file.
    pub fn load(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
        fs::read(path)
    }
}
