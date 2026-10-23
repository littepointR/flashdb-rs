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

// ---------------------------------------------------------------------------
// Cross-image power-loss trace (fault-injection harness)
//
// The `flash-fault-write` and `flash-fault-verify` images are separate builds,
// so their linker-assigned RAM layout (and therefore the address of a static in
// `.results`) differs between them.  The writer image therefore publishes its
// "which flash operation was in flight when the external reset hit" post-mortem
// at a *fixed* address in the unused SRAM1 gap: the static sections end at
// ~0x2000_9500 (0.6 KiB .data/.bss + 32 KiB `.sim_flash` + 4 KiB `.results`)
// and the stack starts at `_stack_bottom` = 0x2002_6000 (see board.ld), so
// 0x2001_0000 is safely between them in every board image.
// ---------------------------------------------------------------------------

/// Fixed SRAM address of the fault-injection trace block shared by the
/// `flash-fault-write` and `flash-fault-verify` images.
pub const TRACE_ADDR: u32 = 0x2001_0000;

/// `HwTrace::op` value while no flash operation is in flight.
pub const TOP_IDLE: u32 = 0;
/// `HwTrace::op` value written when a writer image starts its workload (i.e.
/// the reset hit before the first erase/program).
pub const TOP_BOOT: u32 = 0x100;

/// Magic/anti-magic pair of a valid trace block.  Two complementary words make
/// random SRAM contents (or leftovers from an unrelated firmware) extremely
/// unlikely to be mistaken for a trace.
pub const TRACE_MAGIC: u32 = 0x4654_5243; // "FTRC"

/// Post-mortem trace of the last flash operation started by a writer image.
///
/// Written with volatile stores by [`RealFlash`] when tracing is enabled, so it
/// survives a warm reset (SRAM is not cleared by a Cortex-M system reset) and
/// the verifier image can report which window the reset landed in.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HwTrace {
    /// [`TRACE_MAGIC`] when the block is valid.
    pub magic: u32,
    /// `!magic` (second validity word).
    pub magic_inv: u32,
    /// Number of operations started since the writer image booted.
    pub seq: u32,
    /// Operation in flight: [`TOP_IDLE`], [`TOP_BOOT`], [`OP_ERASE`] or
    /// [`OP_PROGRAM`].
    pub op: u32,
    /// Absolute flash address of the operation in flight.
    pub addr: u32,
    /// Completed page erases since the writer booted.
    pub erases: u32,
    /// Completed quad-word programs since the writer booted.
    pub programs: u32,
    /// Writer failure code (`0` = none); set by the writer image's error path.
    pub error: u32,
}

const T_MAGIC: u32 = 0;
const T_MAGIC_INV: u32 = 1;
const T_SEQ: u32 = 2;
const T_OP: u32 = 3;
const T_ADDR: u32 = 4;
const T_ERASES: u32 = 5;
const T_PROGRAMS: u32 = 6;
const T_ERROR: u32 = 7;

#[inline(always)]
fn trace_read(field: u32) -> u32 {
    unsafe { core::ptr::read_volatile((TRACE_ADDR + field * 4) as *const u32) }
}

#[inline(always)]
fn trace_write(field: u32, value: u32) {
    unsafe { core::ptr::write_volatile((TRACE_ADDR + field * 4) as *mut u32, value) }
}

/// Whether the trace block holds a complete trace from a previous writer run.
pub fn trace_valid() -> bool {
    trace_read(T_MAGIC) == TRACE_MAGIC && trace_read(T_MAGIC_INV) == !TRACE_MAGIC
}

/// Start a fresh trace block for a new writer run (invalidates first, so a
/// reset in the middle of the initialization leaves an invalid trace).
pub fn trace_init() {
    trace_write(T_MAGIC, 0);
    trace_write(T_MAGIC_INV, 0);
    trace_write(T_SEQ, 0);
    trace_write(T_OP, TOP_BOOT);
    trace_write(T_ADDR, 0);
    trace_write(T_ERASES, 0);
    trace_write(T_PROGRAMS, 0);
    trace_write(T_ERROR, 0);
    trace_write(T_MAGIC_INV, !TRACE_MAGIC);
    trace_write(T_MAGIC, TRACE_MAGIC);
}

/// Invalidate the trace block (called by the verifier image once it has copied
/// the trace, so the next writer download starts from the fresh-run path).
pub fn trace_clear() {
    trace_write(T_MAGIC, 0);
    trace_write(T_MAGIC_INV, 0);
}

/// Record the start of one flash operation (before the controller is touched).
pub fn trace_begin_op(op: u32, abs: u32) {
    let seq = trace_read(T_SEQ).wrapping_add(1);
    trace_write(T_OP, op);
    trace_write(T_ADDR, abs);
    trace_write(T_SEQ, seq);
}

/// Record the successful completion of the operation last passed to
/// [`trace_begin_op`] (`op` selects the counter to bump).
pub fn trace_end_op(op: u32) {
    trace_write(T_OP, TOP_IDLE);
    let field = if op == OP_ERASE as u32 {
        T_ERASES
    } else {
        T_PROGRAMS
    };
    trace_write(field, trace_read(field).wrapping_add(1));
}

/// Record a driver/controller error in the trace (`op` and raw status are
/// packed so the verifier can print them).
pub fn trace_note_error(op: u32, status: u32) {
    trace_write(T_ERROR, (op << 16) | (status & 0xFFFF));
}

/// Copy the trace block out (fields are read with volatile loads).
pub fn trace_snapshot() -> HwTrace {
    HwTrace {
        magic: trace_read(T_MAGIC),
        magic_inv: trace_read(T_MAGIC_INV),
        seq: trace_read(T_SEQ),
        op: trace_read(T_OP),
        addr: trace_read(T_ADDR),
        erases: trace_read(T_ERASES),
        programs: trace_read(T_PROGRAMS),
        error: trace_read(T_ERROR),
    }
}

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
    /// Publish every erase/program to the fixed-address [`HwTrace`] block.
    /// Off by default (the endurance/long-run/parity images keep the driver
    /// without trace overhead); the power-loss writer image enables it.
    trace: bool,
    /// When non-zero (`flash-erase-fault` feature), trigger a mid-erase system
    /// reset from SRAM on the `erase_reset_at`-th page erase of this driver.
    #[cfg(feature = "flash-erase-fault")]
    erase_reset_at: u32,
    #[cfg(feature = "flash-erase-fault")]
    erase_reset_delay: u32,
    #[cfg(feature = "flash-erase-fault")]
    erase_reset_out: *mut u32,
}

/// SRAM-resident helper that starts a Bank 1 page erase, observes `NSSR.BSY=1`
/// from SRAM (without fetching instructions from Bank 1 Flash, which would
/// stall the CPU until the erase completes), spins `delay_loops` iterations
/// inside the active erase window, records `[abs, sr_at_reset, loops]` into
/// `out_ptr` in NOLOAD SRAM, and issues `SYSRESETREQ` while `BSY == 1`.
#[cfg(feature = "flash-erase-fault")]
#[link_section = ".data"]
#[inline(never)]
unsafe fn ram_erase_and_reset(abs: u32, nscr_val: u32, delay_loops: u32, out_ptr: *mut u32) -> ! {
    core::arch::asm!("cpsid i", options(nomem, nostack, preserves_flags));
    core::arch::asm!("dsb", options(nomem, nostack, preserves_flags));
    core::arch::asm!("isb", options(nomem, nostack, preserves_flags));

    let nscr = (REG_BASE + R_NSCR) as *mut u32;
    let nssr = (REG_BASE + R_NSSR) as *const u32;
    let aircr = 0xE000_ED0C as *mut u32;

    core::ptr::write_volatile(nscr, nscr_val);
    core::arch::asm!("dsb", options(nomem, nostack, preserves_flags));

    let mut sr = core::ptr::read_volatile(nssr);
    let mut wait_bsy = 0u32;
    while (sr & NSSR_BSY) == 0 && wait_bsy < 1024 {
        sr = core::ptr::read_volatile(nssr);
        wait_bsy = wait_bsy.wrapping_add(1);
    }

    let mut loops = 0u32;
    while loops < delay_loops {
        let cur = core::ptr::read_volatile(nssr);
        if (cur & NSSR_BSY) == 0 {
            break;
        }
        sr = cur;
        loops = loops.wrapping_add(1);
    }

    if !out_ptr.is_null() {
        core::ptr::write_volatile(out_ptr, abs);
        core::ptr::write_volatile(out_ptr.add(1), sr);
        core::ptr::write_volatile(out_ptr.add(2), loops);
    }
    core::arch::asm!("dsb", options(nomem, nostack, preserves_flags));

    core::ptr::write_volatile(aircr, 0x05FA_0004);
    core::arch::asm!("dsb", options(nomem, nostack, preserves_flags));
    loop {
        core::hint::spin_loop();
    }
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
            trace: false,
            #[cfg(feature = "flash-erase-fault")]
            erase_reset_at: 0,
            #[cfg(feature = "flash-erase-fault")]
            erase_reset_delay: 0,
            #[cfg(feature = "flash-erase-fault")]
            erase_reset_out: core::ptr::null_mut(),
        };
        // Clear stale EOP/error/ECC flags from a previous run and unlock.
        set_reg(R_NSSR, NSSR_EOP | SR_ERRORS);
        set_reg(R_ECCR, 0xC000_0000);
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

    /// Enable publishing of every erase/program to the fixed-address
    /// [`HwTrace`] block (power-loss writer image only; see [`TRACE_ADDR`]).
    pub fn enable_trace(&mut self) {
        self.trace = true;
    }

    /// Arm a deterministic mid-erase reset on the `nth` page erase (`1`-based)
    /// performed by this driver instance.  When that erase starts, an
    /// SRAM-resident routine starts the hardware page erase, waits `delay`
    /// SRAM poll loops while `NSSR.BSY == 1`, writes `[abs, nssr, loops]` to
    /// `out_words3` (3 × `u32` in NOLOAD SRAM), and asserts `SYSRESETREQ`.
    #[cfg(feature = "flash-erase-fault")]
    pub fn arm_erase_reset(&mut self, nth: u32, delay: u32, out_words3: *mut u32) {
        self.erase_reset_at = nth;
        self.erase_reset_delay = delay;
        self.erase_reset_out = out_words3;
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
        if self.trace {
            trace_begin_op(OP_ERASE as u32, abs);
        }
        let page = (abs - FLASH_BASE) / PAGE_SIZE;
        let nscr_val = (page << NSCR_PNB_SHIFT) | NSCR_PER | NSCR_STRT;
        #[cfg(feature = "flash-erase-fault")]
        if self.erase_reset_at != 0 && self.erases.wrapping_add(1) == self.erase_reset_at {
            unsafe {
                ram_erase_and_reset(abs, nscr_val, self.erase_reset_delay, self.erase_reset_out);
            }
        }
        // Bank 1 (BKER = 0), page number, page erase, start.
        set_reg(R_NSCR, nscr_val);
        self.wait_idle()?;
        let result = self.finish(OP_ERASE);
        if self.trace {
            match result {
                Ok(()) => trace_end_op(OP_ERASE as u32),
                Err(e) => trace_note_error(e.op as u32, e.status),
            }
        }
        result?;
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
        if self.trace {
            trace_begin_op(OP_PROGRAM as u32, abs);
        }
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
        let result = self.finish(OP_PROGRAM);
        if self.trace {
            match result {
                Ok(()) => trace_end_op(OP_PROGRAM as u32),
                Err(e) => trace_note_error(e.op as u32, e.status),
            }
        }
        result?;
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
