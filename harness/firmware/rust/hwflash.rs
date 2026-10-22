//! Minimal STM32U5 internal-flash driver for the on-silicon endurance and
//! long-run tests (features `flash-endurance` / `flash-longrun`).
//!
//! The board firmware runs in the Cortex-M33 non-secure state (TZEN=0, same as
//! the RCC decode in `board`), so the **non-secure** FLASH register block at
//! `0x4002_2000` is used (`FLASH_NS` in the ST HAL). Register offsets/bits are
//! taken from the STM32U5 CMSIS header (FLASH_TypeDef / FLASH_NSCR / FLASH_NSSR,
//! RM0456 family).
//!
//! Only the eight 8 KiB pages 24..31 of bank 1 (`0x0803_0000..0x0803_FFFF`) are
//! touched: the firmware image occupies the first pages, and page 31 is the last
//! page of bank 1 in both single- and dual-bank configurations, so the test can
//! never erase code. All writes are 128-bit quad-word programs, matching the
//! device's programming granularity (`WRITE_GRAN_BITS = 128`).

use flashdb::Flash;

/// Base address of the STM32U5 internal flash (bank 1).
pub const FLASH_BASE: u32 = 0x0800_0000;
/// Internal-flash page (erase block) size: 8 KiB.
pub const PAGE_SIZE: u32 = 8 * 1024;
/// First scratch page (page 24) and the number of pages reserved for tests.
pub const SCRATCH_BASE: u32 = FLASH_BASE + 24 * PAGE_SIZE;
pub const SCRATCH_PAGES: u32 = 8;
pub const SCRATCH_END: u32 = SCRATCH_BASE + SCRATCH_PAGES * PAGE_SIZE;

// Non-secure FLASH register block (AHB1 non-secure + 0x2000).
const REG_BASE: u32 = 0x4002_2000;
const R_ACR: u32 = 0x00;
const R_NSKEYR: u32 = 0x08;
const R_NSSR: u32 = 0x20;
const R_NSCR: u32 = 0x28;
const R_ECCR: u32 = 0x30;

const KEY1: u32 = 0x4567_0123;
const KEY2: u32 = 0xCDEF_89AB;

// FLASH_NSCR bits.
const NSCR_PG: u32 = 1 << 0;
const NSCR_PER: u32 = 1 << 1;
const NSCR_PNB_SHIFT: u32 = 3;
const NSCR_STRT: u32 = 1 << 16;
const NSCR_LOCK: u32 = 1 << 31; // FLASH_NSSR bits.
const NSSR_EOP: u32 = 1 << 0;
const NSSR_OPERR: u32 = 1 << 1;
const NSSR_PROGERR: u32 = 1 << 3;
const NSSR_WRPERR: u32 = 1 << 4;
const NSSR_PGAERR: u32 = 1 << 5;
const NSSR_SIZERR: u32 = 1 << 6;
const NSSR_PGSERR: u32 = 1 << 7;
const NSSR_OPTWERR: u32 = 1 << 13;
const NSSR_BSY: u32 = 1 << 16;
const NSSR_WDW: u32 = 1 << 17;
const SR_ERRORS: u32 = NSSR_OPERR
    | NSSR_PROGERR
    | NSSR_WRPERR
    | NSSR_PGAERR
    | NSSR_SIZERR
    | NSSR_PGSERR
    | NSSR_OPTWERR;

/// Timeout for polling BUSY/WDW. The flash interface stalls instruction fetch
/// from flash while busy, so this only bounds software bugs / a wedged
/// controller; it is not a wall-clock timeout.
const WAIT_LIMIT: u32 = 20_000_000;

/// Driver error: `op` identifies the failing step, `status` is the raw NSSR
/// snapshot (or the offending address for range/alignment errors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HwError {
    pub op: u8,
    pub status: u32,
}

/// Operations reported in [`HwError::op`].
pub const OP_RANGE: u8 = 1;
pub const OP_ALIGN: u8 = 2;
pub const OP_UNLOCK: u8 = 3;
pub const OP_TIMEOUT: u8 = 4;
pub const OP_ERASE: u8 = 5;
pub const OP_PROGRAM: u8 = 6;
pub const OP_BOUNDS: u8 = 7;
pub const OP_VERIFY: u8 = 8;

#[inline(always)]
fn reg(off: u32) -> u32 {
    unsafe { core::ptr::read_volatile((REG_BASE + off) as *const u32) }
}

#[inline(always)]
fn set_reg(off: u32, value: u32) {
    unsafe { core::ptr::write_volatile((REG_BASE + off) as *mut u32, value) }
}

/// One database partition living on the internal flash.
pub struct RealFlash {
    base: u32,
    size: u32,
    /// Number of page erases performed.
    pub erases: u32,
    /// Number of quad-words programmed.
    pub programs: u32,
    /// Last raw NSSR value (for failure reporting).
    pub last_status: u32,
}

impl RealFlash {
    /// Create a partition `size` bytes at absolute flash address `base`,
    /// unlock the flash controller and clear stale status flags.
    ///
    /// `base`/`size` must be page aligned and lie inside the scratch pages.
    pub fn new(base: u32, size: u32) -> Result<Self, HwError> {
        if base % PAGE_SIZE != 0
            || size % PAGE_SIZE != 0
            || base < SCRATCH_BASE
            || base.checked_add(size).map_or(true, |end| end > SCRATCH_END)
        {
            return Err(HwError {
                op: OP_RANGE,
                status: base,
            });
        }
        let mut this = Self {
            base,
            size,
            erases: 0,
            programs: 0,
            last_status: 0,
        };
        // Clear stale EOP/error flags from a previous run and unlock.
        set_reg(R_NSSR, NSSR_EOP | SR_ERRORS);
        if reg(R_NSCR) & NSCR_LOCK != 0 {
            set_reg(R_NSKEYR, KEY1);
            set_reg(R_NSKEYR, KEY2);
        }
        if reg(R_NSCR) & NSCR_LOCK != 0 {
            this.last_status = reg(R_NSSR);
            return Err(HwError {
                op: OP_UNLOCK,
                status: this.last_status,
            });
        }
        Ok(this)
    }

    #[inline]
    pub fn base(&self) -> u32 {
        self.base
    }

    #[inline]
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Wait until the controller is idle (BUSY and WDW cleared).
    fn wait_idle(&mut self) -> Result<(), HwError> {
        let mut n = 0u32;
        loop {
            let sr = reg(R_NSSR);
            if sr & (NSSR_BSY | NSSR_WDW) == 0 {
                return Ok(());
            }
            n += 1;
            if n >= WAIT_LIMIT {
                self.last_status = sr;
                return Err(HwError {
                    op: OP_TIMEOUT,
                    status: sr,
                });
            }
        }
    }

    /// Check and clear the operation result: error flags are fatal, EOP is
    /// acknowledged.
    fn finish(&mut self, op: u8) -> Result<(), HwError> {
        let sr = reg(R_NSSR);
        self.last_status = sr;
        if sr & SR_ERRORS != 0 {
            set_reg(R_NSSR, sr & (SR_ERRORS | NSSR_EOP));
            return Err(HwError { op, status: sr });
        }
        if sr & NSSR_EOP != 0 {
            set_reg(R_NSSR, NSSR_EOP);
        }
        Ok(())
    }

    /// Absolute-range check for the raw operations.
    #[inline]
    fn in_scratch(&self, abs: u32, len: u32) -> bool {
        abs >= self.base
            && abs
                .checked_add(len)
                .map_or(false, |end| end <= self.base + self.size)
    }

    /// Erase one 8 KiB page (raw absolute address).
    pub fn raw_erase_page(&mut self, abs: u32) -> Result<(), HwError> {
        if abs % PAGE_SIZE != 0 || !self.in_scratch(abs, PAGE_SIZE) {
            return Err(HwError {
                op: OP_RANGE,
                status: abs,
            });
        }
        self.wait_idle()?;
        set_reg(R_NSSR, NSSR_EOP | SR_ERRORS);
        let page = (abs - FLASH_BASE) / PAGE_SIZE;
        // Bank 1 (BKER = 0), page number, page erase, start.
        set_reg(R_NSCR, (page << NSCR_PNB_SHIFT) | NSCR_PER | NSCR_STRT);
        self.wait_idle()?;
        self.finish(OP_ERASE)?;
        self.erases += 1;
        Ok(())
    }

    /// Program one 128-bit quad-word (raw absolute address).
    pub fn raw_program_quadword(&mut self, abs: u32, data: &[u8; 16]) -> Result<(), HwError> {
        if abs % 16 != 0 || !self.in_scratch(abs, 16) {
            return Err(HwError {
                op: OP_RANGE,
                status: abs,
            });
        }
        self.wait_idle()?;
        set_reg(R_NSSR, NSSR_EOP | SR_ERRORS);
        set_reg(R_NSCR, NSCR_PG);
        // With PG set, four 32-bit writes to the flash address program the
        // quad-word (same sequence as STM32Cube HAL_FLASH_Program_QuadWord).
        for i in 0..4u32 {
            let word = u32::from_le_bytes([
                data[i as usize * 4],
                data[i as usize * 4 + 1],
                data[i as usize * 4 + 2],
                data[i as usize * 4 + 3],
            ]);
            unsafe {
                core::ptr::write_volatile((abs + i * 4) as *mut u32, word);
            }
        }
        self.wait_idle()?;
        set_reg(R_NSCR, reg(R_NSCR) & !NSCR_PG);
        self.finish(OP_PROGRAM)?;
        self.programs += 1;
        Ok(())
    }

    /// Read one 32-bit word (raw absolute address).
    pub fn raw_read_u32(&self, abs: u32) -> u32 {
        unsafe { core::ptr::read_volatile(abs as *const u32) }
    }

    /// Copy flash contents out (raw absolute address).
    pub fn raw_read(&self, abs: u32, buf: &mut [u8]) {
        unsafe {
            core::ptr::copy_nonoverlapping(abs as *const u8, buf.as_mut_ptr(), buf.len());
        }
    }

    /// Whether `len` bytes at `abs` are all erased (`0xFF`), word-wise.
    pub fn raw_is_erased(&self, abs: u32, len: u32) -> bool {
        let mut off = 0u32;
        while off + 4 <= len {
            let w = unsafe { core::ptr::read_volatile((abs + off) as *const u32) };
            if w != 0xFFFF_FFFF {
                return false;
            }
            off += 4;
        }
        while off < len {
            let b = unsafe { core::ptr::read_volatile((abs + off) as *const u8) };
            if b != 0xFF {
                return false;
            }
            off += 1;
        }
        true
    }

    /// Read the controller status/access registers (diagnostics).
    pub fn registers(&self) -> [u32; 4] {
        [reg(R_ACR), reg(R_NSSR), reg(R_NSCR), reg(R_ECCR)]
    }
}

impl Flash for RealFlash {
    type Error = HwError;

    const WRITE_GRAN_BITS: u32 = 128;
    const ERASED_BYTE: u8 = 0xFF;

    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        let abs = self.base.wrapping_add(addr);
        if !self.in_scratch(abs, buf.len() as u32) {
            return Err(HwError {
                op: OP_BOUNDS,
                status: abs,
            });
        }
        unsafe {
            core::ptr::copy_nonoverlapping(abs as *const u8, buf.as_mut_ptr(), buf.len());
        }
        Ok(())
    }

    fn write(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        let abs = self.base.wrapping_add(addr);
        if abs % 16 != 0 || buf.len() % 16 != 0 || !self.in_scratch(abs, buf.len() as u32) {
            return Err(HwError {
                op: if abs % 16 != 0 || buf.len() % 16 != 0 {
                    OP_ALIGN
                } else {
                    OP_BOUNDS
                },
                status: abs,
            });
        }
        for (i, chunk) in buf.chunks(16).enumerate() {
            let mut qw = [0u8; 16];
            qw.copy_from_slice(chunk);
            self.raw_program_quadword(abs + (i * 16) as u32, &qw)?;
        }
        Ok(())
    }

    fn erase(&mut self, addr: u32, len: u32) -> Result<(), Self::Error> {
        let abs = self.base.wrapping_add(addr);
        if abs % PAGE_SIZE != 0 || len % PAGE_SIZE != 0 || !self.in_scratch(abs, len) {
            return Err(HwError {
                op: OP_ALIGN,
                status: abs,
            });
        }
        let mut off = 0u32;
        while off < len {
            self.raw_erase_page(abs + off)?;
            off += PAGE_SIZE;
        }
        Ok(())
    }
}
