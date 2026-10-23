//! FlashDB firmware measurement harness -- Rust firmware.
//!
//! Runs the REAL Rust `flashdb` crate (crates/flashdb) over the no_std RAM
//! flash simulator (crates/flashdb-sim) on a bare-metal Cortex-M3 and prints
//! the same RESULT line as the C baseline firmware.
//!
//! Status of the two databases:
//!   * KVDB: real crate, workload identical in shape to the C firmware
//!     (64 keys x 24-byte values, 32 updates, 1500 churn rewrites over 16
//!     keys, 64 verified gets, full iteration).  Partition 16 KiB = 4 x 4 KiB,
//!     matching the C harness.
//!   * TSDB: real crate, workload identical in shape to the C firmware
//!     (200 x append of a 16-byte deterministic payload with fake timestamps,
//!     then full forward iteration + checksum).  Separate 16 KiB partition in
//!     the second half of the `.sim_flash` region; rollover disabled.
//!
//! Everything else -- startup assembly, linker script, semihosting, stack
//! painting, RESULT format, libc shims -- is shared with the C firmware so the
//! two images are directly comparable.
//!
//! Build: scripts/fw_rust.sh (cargo, target thumbv7m-none-eabi).
#![no_std]
#![no_main]
// The internal-flash test builds replace the standard measurement workload at
// compile time, so the workload constants/helpers are intentionally unused there.
#![cfg_attr(
    any(
        feature = "flash-endurance",
        feature = "flash-longrun",
        feature = "flash-parity",
        feature = "flash-fault-write",
        feature = "flash-fault-verify",
        feature = "flash-reboot-soak",
        feature = "flash-erase-fault"
    ),
    allow(dead_code)
)]

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::panic::PanicInfo;

use flashdb::{Config, Flash, Kvdb, Tsdb};

// The SAME startup assembly file as the C firmware.  It is plain GNU-syntax
// ARM assembly, accepted by both arm-none-eabi-gcc and LLVM's integrated
// assembler (which is what global_asm! uses).
core::arch::global_asm!(include_str!("../common/startup_armv7m.S"));

// ---------------------------------------------------------------------------
// Workload shape -- must match harness/firmware/common/workload_spec.h
// ---------------------------------------------------------------------------
const KV_KEYS: u32 = 64;
const KV_UPDATES: u32 = 32;
const KV_CHURN: u32 = 1500;
const KV_CHURN_KEYS: u32 = 16;
const KV_VALUE_LEN: usize = 24;

const TS_APPENDS: u32 = 200;
const TS_PAYLOAD_LEN: usize = 16;
const TS_BASE_TIME: u32 = 1000;
/// Maximum payload accepted by one TSDB append (C harness uses 64; only an
/// upper-bound validation in the Rust API, the on-flash format is identical).
const TS_MAX_LEN: u32 = 32;

/// Real-board rollover phase (`board-u545`), and the same workload under
/// QEMU (`qemu-cycles`, so its cost can be measured without silicon): append
/// past the ring capacity of the 16 KiB / 4-sector partition (~448 entries of
/// 16-byte payload), then verify the live window and the remount recovery.
/// Kept out of the default QEMU image so the documented baseline stays
/// unchanged.
#[cfg(any(feature = "board-u545", feature = "qemu-cycles"))]
const TS_ROLL_APPENDS: u32 = 1200;

const SECTOR_SIZE: u32 = 4096;
const KV_PARTITION_SIZE: u32 = 16 * 1024;
const TS_PARTITION_SIZE: u32 = 16 * 1024;

const STACK_PAINT_WORD: u32 = 0xA5A5_A5A5;

/// Bytes below the current SP left untouched by `stack_repaint()`: no code
/// path pushes more than this between reading SP and the repaint loop.
#[cfg(feature = "stack-phases")]
const STACK_REPAINT_MARGIN: usize = 32;

// Semihosting operation numbers (ARM semihosting spec).
const SYS_WRITE0: u32 = 0x04;
const SYS_EXIT: u32 = 0x18;
const ADP_STOPPED_APP_EXIT: u32 = 0x20026;

/* ======================================================================== */
/* Linker symbols                                                           */
/* ======================================================================== */

extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __data_end: u8;
    static __bss_start: u8;
    static __bss_end: u8;
    static __sim_flash_start: u8;
    static __sim_flash_end: u8;
    static _stack_bottom: u8;
    static _estack: u8;
}

/* ======================================================================== */
/* Board (NUCLEO-U545RE-Q) output and cycle counters                         */
/* ======================================================================== */

#[cfg(feature = "board-u545")]
mod board {
    /// Result buffer read back by the debug probe after the run.
    #[repr(C)]
    pub struct BoardOut {
        /// Set to `DONE` when the firmware has finished (host polls this).
        pub done: u32,
        /// Number of valid bytes in `buf` (including the NUL terminator).
        pub len: u32,
        pub buf: [u8; 4096],
    }

    pub const DONE: u32 = 0x454E4F44; // "DONE"

    #[link_section = ".results"]
    #[no_mangle]
    pub static mut BOARD_OUT: BoardOut = BoardOut {
        done: 0,
        len: 0,
        buf: [0; 4096],
    };

    /// Clear the result buffer before the first write.
    ///
    /// `.results` is a `NOLOAD` section (see board.ld) so the image carries no
    /// load data for it and the boot code neither copies nor zeroes it: the
    /// static initializer above is discarded and SRAM contents after reset are
    /// undefined. The host reads `done`/`len`, so both must be reset here.
    pub fn reset_output() {
        unsafe {
            let out = &raw mut BOARD_OUT;
            (*out).done = 0;
            (*out).len = 0;
        }
    }

    /// Semihosting replacement used by the board build: SYS_WRITE0 appends the
    /// NUL-terminated string to the RAM buffer, SYS_EXIT marks it complete and
    /// spins so the probe can halt and read it.
    ///
    /// Deliberately *not* inlined: the append loop is ~60 B and every
    /// `write_bytes()` call site would otherwise carry its own copy.  With
    /// `#[inline(always)]` on the wrapper below, LTO used to duplicate the
    /// loop into `main` + `print_result` (+~800 B of ROM on the board image
    /// versus the QEMU build, which uses the 3-instruction `bkpt` path).
    #[inline(never)]
    pub fn semihost(op: u32, arg: usize) -> u32 {
        unsafe {
            let out = &raw mut BOARD_OUT;
            match op {
                0x04 => {
                    let s = arg as *const u8;
                    let mut i = 0usize;
                    let mut n = (*out).len as usize;
                    while i < 4096 {
                        let b = core::ptr::read_volatile(s.add(i));
                        if b == 0 || n >= 4096 {
                            break;
                        }
                        core::ptr::write_volatile((*out).buf.as_mut_ptr().add(n), b);
                        n += 1;
                        i += 1;
                    }
                    (*out).len = n as u32;
                }
                0x18 => {
                    (*out).done = DONE;
                    loop {
                        core::hint::spin_loop();
                    }
                }
                _ => {}
            }
        }
        0
    }

    const DEMCR: *mut u32 = 0xE000_EDFC as *mut u32;
    const DWT_CTRL: *mut u32 = 0xE000_1000 as *mut u32;
    const DWT_CYCCNT: *mut u32 = 0xE000_1004 as *mut u32;

    pub fn init_cycles() {
        unsafe {
            core::ptr::write_volatile(DEMCR, core::ptr::read_volatile(DEMCR) | (1 << 24));
            core::ptr::write_volatile(DWT_CYCCNT, 0);
            core::ptr::write_volatile(DWT_CTRL, core::ptr::read_volatile(DWT_CTRL) | 1);
        }
    }

    #[inline(always)]
    pub fn cycles() -> u32 {
        unsafe { core::ptr::read_volatile(DWT_CYCCNT) }
    }

    // RCC (STM32U5 family, base 0x4602_0C00) -- read-only decode of the system
    // clock, so the `core_hz` reported to the host cannot drift from silicon.
    // This measurement firmware never reconfigures the clock tree, so SYSCLK is
    // the reset clock: MSIS with range index 4, which is 4 MHz on this board
    // (the same value CubeMX reports for the NUCLEO-U545RE-Q default clock tree;
    // confirmed against the measured run time on silicon). Any other state is
    // reported as 0 = unknown so the host prints raw cycles instead of a wrong
    // microsecond conversion.
    const RCC_ICSCR1: *const u32 = 0x4602_0C08 as *const u32;
    const RCC_CFGR1: *const u32 = 0x4602_0C1C as *const u32;
    const RCC_CSR: *const u32 = 0x4602_0CF4 as *const u32;

    pub fn core_hz() -> u32 {
        unsafe {
            let cfgr1 = core::ptr::read_volatile(RCC_CFGR1);
            let icscr1 = core::ptr::read_volatile(RCC_ICSCR1);
            let csr = core::ptr::read_volatile(RCC_CSR);
            let sw = cfgr1 & 0b11; // 0 = MSIS
            let msis_range = if icscr1 & (1 << 23) != 0 {
                // MSIRGSEL = 1: range in RCC_ICSCR1.MSISRANGE
                (icscr1 >> 28) & 0xF
            } else {
                // MSIRGSEL = 0 (reset state): range in RCC_CSR.MSISSRANGE
                (csr >> 12) & 0xF
            };
            if sw == 0 && msis_range == 4 {
                4_000_000
            } else {
                0
            }
        }
    }
}

/* ======================================================================== */
/* On-silicon internal-flash endurance / long-run tests                      */
/* (`flash-endurance` / `flash-longrun`; see realflash.rs for the driver)    */
/* ======================================================================== */

#[cfg(feature = "flash-endurance")]
mod flash_endurance;
#[cfg(feature = "flash-erase-fault")]
mod flash_erase_fault;
#[cfg(feature = "flash-fault-verify")]
mod flash_fault_verify;
#[cfg(feature = "flash-fault-write")]
mod flash_fault_write;
#[cfg(feature = "flash-longrun")]
mod flash_longrun;
#[cfg(feature = "flash-parity")]
mod flash_parity;
#[cfg(feature = "flash-reboot-soak")]
mod flash_reboot_soak;
#[cfg(any(
    feature = "flash-endurance",
    feature = "flash-longrun",
    feature = "flash-parity",
    feature = "flash-fault-write",
    feature = "flash-fault-verify",
    feature = "flash-reboot-soak",
    feature = "flash-erase-fault"
))]
mod hwflash;

#[cfg(all(feature = "flash-endurance", feature = "flash-longrun"))]
compile_error!(
    "features `flash-endurance` and `flash-longrun` are mutually exclusive: \
     they are separate board runs"
);

/* ======================================================================== */
/* Semihosting output (no libc, no core::fmt)                               */
/* ======================================================================== */

#[inline(always)]
unsafe fn semihost(op: u32, arg: usize) -> u32 {
    #[cfg(feature = "board-u545")]
    {
        board::semihost(op, arg)
    }
    #[cfg(not(feature = "board-u545"))]
    {
        let mut r0 = op;
        core::arch::asm!(
            "bkpt 0xab",
            inout("r0") r0,
            in("r1") arg,
            options(nostack, preserves_flags)
        );
        r0
    }
}

fn write_bytes(s: &[u8]) {
    // SYS_WRITE0 (0x04): r1 = NUL-terminated string.  Call sites pass either
    // byte-string literals that end in 0 or stack buffers whose last byte is 0.
    unsafe {
        semihost(SYS_WRITE0, s.as_ptr() as usize);
    }
}

fn write_u32(mut value: u32) {
    // Decimal digits extracted by constant-division: `/ 10` and `% 10` on an
    // unsigned 32-bit value lower to a multiply-high sequence on Cortex-M3
    // (no __aeabi_uidiv call), which is much smaller than a powers-of-ten
    // subtraction loop and needs no rodata table.  Max 10 digits.
    let mut buf = [0u8; 11];
    let mut p = 10;

    buf[10] = 0;
    loop {
        p -= 1;
        let q = value / 10;
        buf[p] = b'0' + (value - q * 10) as u8;
        value = q;
        if value == 0 {
            break;
        }
    }
    write_bytes(&buf[p..]);
}

fn write_hex32(value: u32) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 11];

    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..8 {
        buf[2 + i] = HEX[((value >> (28 - 4 * i)) & 0xf) as usize];
    }
    buf[10] = 0;
    write_bytes(&buf[..]);
}

fn exit() -> ! {
    unsafe {
        semihost(SYS_EXIT, ADP_STOPPED_APP_EXIT as usize);
    }
    loop {}
}

/* ======================================================================== */
/* QEMU cycle/instruction proxy (`qemu-cycles` feature, QEMU builds only)   */
/*                                                                          */
/* What QEMU 8.2 actually models on mps2-an385 (Cortex-M3), measured with    */
/* the probe below:                                                         */
/*   * semihosting SYS_TICKFREQ (0x31) works and reports 1 GHz;             */
/*   * semihosting SYS_ELAPSED (0x30) returns 0 in every argument form, so   */
/*     it is NOT usable as a time source;                                   */
/*   * the DWT cycle counter is not modelled (DWT_CTRL reads 0, CYCCNT      */
/*     never advances);                                                     */
/*   * SysTick IS modelled and runs off QEMU's virtual clock.  Under         */
/*     `qemu-system-arm -icount shift=0` one instruction consumes exactly    */
/*     one virtual nanosecond, so a SysTick tick is a fixed, deterministic   */
/*     number of instructions (MPS2-AN385 SysTick = 25 MHz -> 1 tick = 40    */
/*     ns = 40 instructions).  That makes the phase boundaries measurable    */
/*     without silicon, including the board rollover workload, and repeated  */
/*     runs are bit-identical.                                               */
/*                                                                          */
/* The counter is used free-running (24-bit down-counter, no interrupt), so  */
/* it costs no stack and cannot perturb the workload's own stack baseline.   */
/* A phase must stay below 2^24 ticks (~670 M instructions) for its delta to */
/* be unambiguous; each reported value is a separate phase delta, and the    */
/* total is their sum.  Read tick deltas as `delta(earlier, later)`.         */
/* ======================================================================== */

#[cfg(all(feature = "qemu-cycles", feature = "board-u545"))]
compile_error!(
    "features `qemu-cycles` and `board-u545` are mutually exclusive: the board \
     image reports DWT cycles, the QEMU proxy reports SysTick virtual ticks"
);

#[cfg(feature = "qemu-cycles")]
mod qcycles {
    use super::{write_bytes, write_hex32, write_u32};

    /// ARM semihosting SYS_ELAPSED / SYS_TICKFREQ (probed, not used for timing:
    /// QEMU 8.2 returns 0 from SYS_ELAPSED).
    const SYS_ELAPSED: u32 = 0x30;
    const SYS_TICKFREQ: u32 = 0x31;

    /// Cortex-M3 DWT registers (probed; not modelled by QEMU).
    const DWT_CTRL: *const u32 = 0xE000_1000 as *const u32;
    const DWT_CYCCNT: *const u32 = 0xE000_1004 as *const u32;

    /// SysTick (NVIC) registers: CTRL / LOAD / VAL.
    const SYST_CTRL: *mut u32 = 0xE000_E010 as *mut u32;
    const SYST_LOAD: *mut u32 = 0xE000_E014 as *mut u32;
    const SYST_VAL: *mut u32 = 0xE000_E018 as *mut u32;
    /// CTRL: ENABLE | CLKSOURCE=processor clock, no TICKINT (no exceptions).
    const SYST_CTRL_FREE_RUNNING: u32 = 0x5;

    pub const TICK_MASK: u32 = 0x00FF_FFFF;

    /// Instructions retired by one `spin()` call (2-instruction asm loop, 10000
    /// iterations: both instructions execute on every iteration, including the
    /// last one where the conditional branch falls through).
    pub const SPIN_INSTRUCTIONS: u32 = 20_000;

    /// Busy-loop sink so the probe delta cannot be optimised away.
    static mut PROBE_SINK: u32 = 0;
    /// SysTick ticks measured for exactly `SPIN_INSTRUCTIONS` instructions;
    /// printed so the host can convert ticks to an instruction estimate
    /// (instructions = ticks * SPIN_INSTRUCTIONS / spin_ticks).
    static mut PROBE_SPIN_TICKS: u32 = 0;

    /// Semihosting call returning both R0 and R1 (probe only).
    #[inline(always)]
    fn semihost2(op: u32, arg: u32) -> (u32, u32) {
        let mut r0 = op;
        let mut r1 = arg;
        unsafe {
            core::arch::asm!(
                "bkpt 0xab",
                inout("r0") r0,
                inout("r1") r1,
                options(nostack, preserves_flags)
            );
        }
        (r0, r1)
    }

    /// Exactly `SPIN_INSTRUCTIONS` retired instructions (2 per iteration plus
    /// the final conditional branch), used to calibrate ticks -> instructions.
    #[inline(never)]
    fn spin() {
        let mut n: u32 = 10_000;
        unsafe {
            core::arch::asm!(
                "1:",
                "subs {n}, {n}, #1",
                "bne 1b",
                n = inout(reg) n => _,
                options(nomem, nostack)
            );
        }
    }

    /// Start the free-running SysTick 24-bit down-counter.
    #[inline(always)]
    pub fn init() {
        unsafe {
            core::ptr::write_volatile(SYST_LOAD, TICK_MASK);
            core::ptr::write_volatile(SYST_VAL, 0);
            core::ptr::write_volatile(SYST_CTRL, SYST_CTRL_FREE_RUNNING);
        }
    }

    /// Current SysTick value (counts down).
    #[inline(always)]
    pub fn now() -> u32 {
        unsafe { core::ptr::read_volatile(SYST_VAL) }
    }

    /// Ticks elapsed between two `now()` samples (handles one wrap).
    #[inline(always)]
    pub fn delta(earlier: u32, later: u32) -> u32 {
        earlier.wrapping_sub(later) & TICK_MASK
    }

    /// Ticks measured for `SPIN_INSTRUCTIONS` instructions (calibration).
    pub fn spin_ticks() -> u32 {
        unsafe { PROBE_SPIN_TICKS }
    }

    /// Print one `QEMU_PROBE` line with the raw evidence for which time source
    /// the emulator supports (and the tick calibration).  Called once, before
    /// `init()` and before the first measured phase.
    #[inline(never)]
    pub fn probe() {
        let (freq_lo, freq_hi) = semihost2(SYS_TICKFREQ, 0);
        let (e32_a, _) = semihost2(SYS_ELAPSED, 0);
        let (e64a_lo, e64a_hi) = semihost2(SYS_ELAPSED, 1);
        let (e_bad, _) = semihost2(SYS_ELAPSED, 2);
        let dwt_ctrl = unsafe { core::ptr::read_volatile(DWT_CTRL) };
        let cyc_a = unsafe { core::ptr::read_volatile(DWT_CYCCNT) };

        // Free-running SysTick, then time a known-instruction-count spin.
        init();
        let spin_a = now();
        spin();
        let spin_b = now();
        let spin_delta = delta(spin_a, spin_b);
        unsafe { PROBE_SPIN_TICKS = spin_delta };

        // Kept so the probe is not optimised into nothing; also gives a second
        // non-zero delta sample for the volatile-store loop.
        let mut x = 0u32;
        let mut i = 0u32;
        while i < 100 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223) ^ i;
            unsafe { core::ptr::write_volatile(&raw mut PROBE_SINK, x) };
            i += 1;
        }

        let (e32_b, _) = semihost2(SYS_ELAPSED, 0);
        let (e64b_lo, e64b_hi) = semihost2(SYS_ELAPSED, 1);
        let cyc_b = unsafe { core::ptr::read_volatile(DWT_CYCCNT) };

        write_bytes(b"QEMU_PROBE tickfreq=\0");
        write_hex32(freq_lo);
        write_bytes(b" tickfreq_hi=\0");
        write_hex32(freq_hi);
        write_bytes(b" e32_a=\0");
        write_hex32(e32_a);
        write_bytes(b" e32_b=\0");
        write_hex32(e32_b);
        write_bytes(b" e64a_lo=\0");
        write_hex32(e64a_lo);
        write_bytes(b" e64b_lo=\0");
        write_hex32(e64b_lo);
        write_bytes(b" e64_hi_a=\0");
        write_hex32(e64a_hi);
        write_bytes(b" e64_hi_b=\0");
        write_hex32(e64b_hi);
        write_bytes(b" e_arg2=\0");
        write_hex32(e_bad);
        write_bytes(b" dwt_ctrl=\0");
        write_hex32(dwt_ctrl);
        write_bytes(b" dwt_cyc_delta=\0");
        write_hex32(cyc_b.wrapping_sub(cyc_a));
        write_bytes(b" spin_insn=\0");
        write_u32(SPIN_INSTRUCTIONS);
        write_bytes(b" spin_ticks=\0");
        write_u32(spin_delta);
        write_bytes(b" syst_mask=\0");
        write_hex32(TICK_MASK);
        write_bytes(b"\n\0");
    }
}

#[cold]
#[inline(never)]
fn fail(stage: &[u8], code: u32) -> ! {
    write_bytes(b"ERROR stage=\0");
    write_bytes(stage);
    write_bytes(b" code=\0");
    write_u32(code);
    write_bytes(b"\nRESULT rc=1\n\0");
    exit()
}

/* --------------------------------------------------------------------- */
/* Failure handling                                                        */
/*                                                                         */
/* One cold, never-inlined exit path; call sites use short shared tags     */
/* instead of a unique NUL-terminated string per operation (the tags are   */
/* only printed when a workload step fails, they are not part of RESULT).  */
/* --------------------------------------------------------------------- */

const TAG_KV_MOUNT: &[u8] = b"kv_mount\0";
const TAG_TS_MOUNT: &[u8] = b"ts_mount\0";
const TAG_KV: &[u8] = b"kv\0";
const TAG_TS: &[u8] = b"ts\0";
#[cfg(any(feature = "board-u545", feature = "qemu-cycles"))]
const TAG_TS_ROLL: &[u8] = b"ts_roll\0";

/// Unwrap a database operation, exiting through `fail()` on error.
#[inline(always)]
fn ok_or_fail<T, E>(r: core::result::Result<T, E>, tag: &[u8]) -> T {
    match r {
        Ok(v) => v,
        Err(_) => fail(tag, 0),
    }
}

/* ======================================================================== */
/* Panic handler                                                            */
/*                                                                          */
/* Deliberately minimal: a panic means a firmware bug, and a plain `loop {}` */
/* keeps core::fmt/panicking machinery out of the image (the C baseline's   */
/* FDB_ASSERT does the equivalent `while (1);` when logging is compiled     */
/* out).  A hang is reported by the host script's QEMU timeout.             */
/* ======================================================================== */

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

/* ======================================================================== */
/* Minimal libc + compiler-builtins overrides                               */
/*                                                                          */
/* On ARM, LLVM lowers memcpy/memset/memmove intrinsics to __aeabi_* calls.  */
/* By default those resolve to compiler-builtins' generic implementations    */
/* (~1.5 KiB of ROM: memcpy 744 B, __aeabi_memset 220 B, __aeabi_memset4    */
/* 184 B, __aeabi_memclr4 174 B, __aeabi_memcpy4 156 B, __aeabi_memcpy 12 B).*/
/* The C baseline links tiny hand-written stubs instead (c/libc_stubs.c,     */
/* ~230 B of code for the same set), so the Rust firmware supplies strong    */
/* definitions here: compiler-builtins' symbols are weak and lose.  This is  */
/* harness-only code -- the library under measurement is unchanged.          */
/*                                                                          */
/* Volatile accesses stop LLVM from recognising these loops and turning them */
/* back into calls to themselves (a classic no_std memcpy pitfall).          */
/* ======================================================================== */

#[inline(never)]
unsafe fn copy_fwd(dst: *mut u8, src: *const u8, n: usize) {
    let mut i = 0;
    while i < n {
        core::ptr::write_volatile(dst.add(i), core::ptr::read_volatile(src.add(i)));
        i += 1;
    }
}

#[inline(never)]
unsafe fn copy_back(dst: *mut u8, src: *const u8, n: usize) {
    let mut i = n;
    while i > 0 {
        i -= 1;
        core::ptr::write_volatile(dst.add(i), core::ptr::read_volatile(src.add(i)));
    }
}

#[inline(never)]
unsafe fn fill(dst: *mut u8, byte: u8, n: usize) {
    let mut i = 0;
    while i < n {
        core::ptr::write_volatile(dst.add(i), byte);
        i += 1;
    }
}

#[inline(never)]
unsafe fn compare(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let va = core::ptr::read_volatile(a.add(i));
        let vb = core::ptr::read_volatile(b.add(i));
        if va != vb {
            return va as i32 - vb as i32;
        }
        i += 1;
    }
    0
}

/* ---- C standard names (memcpy & friends) ------------------------------- */

#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    copy_fwd(dst, src, n);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if (dst as usize) <= (src as usize) {
        copy_fwd(dst, src, n);
    } else {
        copy_back(dst, src, n);
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memset(dst: *mut u8, c: i32, n: usize) -> *mut u8 {
    fill(dst, c as u8, n);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    compare(a, b, n)
}

#[no_mangle]
pub unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    compare(a, b, n)
}

#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const u8) -> usize {
    let mut n = 0;
    while core::ptr::read_volatile(s.add(n)) != 0 {
        n += 1;
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn strcmp(a: *const u8, b: *const u8) -> i32 {
    let mut i = 0;
    loop {
        let ca = core::ptr::read_volatile(a.add(i));
        let cb = core::ptr::read_volatile(b.add(i));
        if ca != cb || ca == 0 {
            return ca as i32 - cb as i32;
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn strncmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let ca = core::ptr::read_volatile(a.add(i));
        let cb = core::ptr::read_volatile(b.add(i));
        if ca != cb || ca == 0 {
            return ca as i32 - cb as i32;
        }
        i += 1;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn strncpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        let c = core::ptr::read_volatile(src.add(i));
        core::ptr::write_volatile(dst.add(i), c);
        if c == 0 {
            i += 1;
            while i < n {
                core::ptr::write_volatile(dst.add(i), 0);
                i += 1;
            }
            break;
        }
        i += 1;
    }
    dst
}

/* ---- AAPCS runtime helpers (__aeabi_*): compiler-builtins are weak ----- */

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memcpy(dst: *mut u8, src: *const u8, n: usize) {
    copy_fwd(dst, src, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memcpy4(dst: *mut u8, src: *const u8, n: usize) {
    copy_fwd(dst, src, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memcpy8(dst: *mut u8, src: *const u8, n: usize) {
    copy_fwd(dst, src, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memmove(dst: *mut u8, src: *const u8, n: usize) {
    memmove(dst, src, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memmove4(dst: *mut u8, src: *const u8, n: usize) {
    memmove(dst, src, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memmove8(dst: *mut u8, src: *const u8, n: usize) {
    memmove(dst, src, n);
}

/* Note the AAPCS argument order: (dest, length, value). */
#[no_mangle]
pub unsafe extern "C" fn __aeabi_memset(dst: *mut u8, n: usize, c: i32) {
    fill(dst, c as u8, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memset4(dst: *mut u8, n: usize, c: i32) {
    fill(dst, c as u8, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memset8(dst: *mut u8, n: usize, c: i32) {
    fill(dst, c as u8, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memclr(dst: *mut u8, n: usize) {
    fill(dst, 0, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memclr4(dst: *mut u8, n: usize) {
    fill(dst, 0, n);
}

#[no_mangle]
pub unsafe extern "C" fn __aeabi_memclr8(dst: *mut u8, n: usize) {
    fill(dst, 0, n);
}

/* ======================================================================== */
/* Section sizes + stack high-water (identical method to the C firmware)    */
/* ======================================================================== */

#[repr(C)]
struct Metrics {
    text: u32,
    rodata: u32,
    data: u32,
    bss: u32,
    sim_flash: u32,
    stack: u32,
    stack_init: u32, /* mount/init phase (paint is fresh before main) */
    stack_limit: u32,
    kv_writes: u32,
    kv_gets: u32,
    kv_iterated: u32,
    kv_checksum: u32,
    kv_iter_checksum: u32,
    ts_appends: u32,
    ts_iterated: u32,
    ts_checksum: u32,
    /// Diagnostic per-phase peaks; only present in `--phases` builds and
    /// always at the end so `print_result`'s repr(C) table walk stays valid.
    #[cfg(feature = "stack-phases")]
    st_set: u32,
    #[cfg(feature = "stack-phases")]
    st_get: u32,
    #[cfg(feature = "stack-phases")]
    st_iter: u32,
    #[cfg(feature = "stack-phases")]
    st_gc: u32,
    #[cfg(feature = "stack-phases")]
    st_ts_append: u32,
    #[cfg(feature = "stack-phases")]
    st_ts_iter: u32,
    /// Real-board rollover phase results (`board-u545` only; zero otherwise).
    /// Appended after the diagnostic block so `print_result`'s table walk over
    /// the leading fields is unaffected.
    #[cfg(feature = "board-u545")]
    ts_roll_appends: u32,
    #[cfg(feature = "board-u545")]
    ts_roll_erases: u32,
    #[cfg(feature = "board-u545")]
    ts_roll_violations: u32,
    #[cfg(feature = "board-u545")]
    ts_roll_count: u32,
    #[cfg(feature = "board-u545")]
    ts_roll_first_time: u32,
    #[cfg(feature = "board-u545")]
    ts_roll_last_time: u32,
    #[cfg(feature = "board-u545")]
    ts_roll_checksum: u32,
    #[cfg(feature = "board-u545")]
    ts_roll_remount_count: u32,
    #[cfg(feature = "board-u545")]
    ts_roll_remount_checksum: u32,
    #[cfg(feature = "board-u545")]
    ts_roll_continue_ok: u32,
    /// FNV-1a over the raw KV / TS partition images after the whole workload
    /// (board only): proves that both implementations leave byte-identical
    /// flash images, not just identical logical results.
    #[cfg(feature = "board-u545")]
    kv_image_checksum: u32,
    #[cfg(feature = "board-u545")]
    ts_image_checksum: u32,
}

impl Metrics {
    const fn zeroed() -> Self {
        Metrics {
            text: 0,
            rodata: 0,
            data: 0,
            bss: 0,
            sim_flash: 0,
            stack: 0,
            stack_init: 0,
            stack_limit: 0,
            kv_writes: 0,
            kv_gets: 0,
            kv_iterated: 0,
            kv_checksum: 0,
            kv_iter_checksum: 0,
            ts_appends: 0,
            ts_iterated: 0,
            ts_checksum: 0,
            #[cfg(feature = "stack-phases")]
            st_set: 0,
            #[cfg(feature = "stack-phases")]
            st_get: 0,
            #[cfg(feature = "stack-phases")]
            st_iter: 0,
            #[cfg(feature = "stack-phases")]
            st_gc: 0,
            #[cfg(feature = "stack-phases")]
            st_ts_append: 0,
            #[cfg(feature = "stack-phases")]
            st_ts_iter: 0,
            #[cfg(feature = "board-u545")]
            ts_roll_appends: 0,
            #[cfg(feature = "board-u545")]
            ts_roll_erases: 0,
            #[cfg(feature = "board-u545")]
            ts_roll_violations: 0,
            #[cfg(feature = "board-u545")]
            ts_roll_count: 0,
            #[cfg(feature = "board-u545")]
            ts_roll_first_time: 0,
            #[cfg(feature = "board-u545")]
            ts_roll_last_time: 0,
            #[cfg(feature = "board-u545")]
            ts_roll_checksum: 0,
            #[cfg(feature = "board-u545")]
            ts_roll_remount_count: 0,
            #[cfg(feature = "board-u545")]
            ts_roll_remount_checksum: 0,
            #[cfg(feature = "board-u545")]
            ts_roll_continue_ok: 0,
            #[cfg(feature = "board-u545")]
            kv_image_checksum: 0,
            #[cfg(feature = "board-u545")]
            ts_image_checksum: 0,
        }
    }
}

fn sym_addr(sym: *const u8) -> usize {
    sym as usize
}

fn fill_section_sizes(m: &mut Metrics) {
    unsafe {
        m.text = (sym_addr(&__text_end) - sym_addr(&__text_start)) as u32;
        m.rodata = (sym_addr(&__rodata_end) - sym_addr(&__rodata_start)) as u32;
        m.data = (sym_addr(&__data_end) - sym_addr(&__data_start)) as u32;
        m.bss = (sym_addr(&__bss_end) - sym_addr(&__bss_start)) as u32;
        m.sim_flash = (sym_addr(&__sim_flash_end) - sym_addr(&__sim_flash_start)) as u32;
    }
}

fn stack_limit_bytes() -> u32 {
    unsafe { (sym_addr(&_estack) - sym_addr(&_stack_bottom)) as u32 }
}

fn stack_high_water_bytes() -> u32 {
    unsafe {
        let bottom = sym_addr(&_stack_bottom) as *const u32;
        let top = sym_addr(&_estack) as *const u32;
        let mut p = bottom;

        while p < top && core::ptr::read_volatile(p) == STACK_PAINT_WORD {
            p = p.add(1);
        }
        let used = (top as usize - p as usize) as u32;
        (used + 7) & !7
    }
}

/// Repaint the unused part of the stack below the current SP with 0xA5.
///
/// Used for per-phase stack attribution: after repainting, a high-water scan
/// reports the peak of the phase that runs next (plus the live caller frames),
/// without marks left by earlier phases.  Only memory below the current SP is
/// touched, so live frames are preserved.
#[cfg(feature = "stack-phases")]
#[inline(never)]
fn stack_repaint() {
    unsafe {
        let sp: usize;
        core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags));
        let bottom = sym_addr(&_stack_bottom);
        let end = sp.saturating_sub(STACK_REPAINT_MARGIN);
        let mut p = bottom;
        while p < end {
            core::ptr::write_volatile(p as *mut u32, STACK_PAINT_WORD);
            p += 4;
        }
    }
}

/* ======================================================================== */
/* Static storage for the databases                                         */
/*                                                                          */
/* The C firmware keeps its fdb_kvdb/fdb_tsdb structs in .bss, not on the    */
/* stack.  Do the same here so the stack high-water comparison is fair: the  */
/* flash simulator and the mounted database live in .bss, and main() only    */
/* holds a shallow reference to them.                                       */
/* ======================================================================== */

/// One 16 KiB simulated NOR partition (4 x 4 KiB erase blocks).
///
/// Both stores live in the `.sim_flash` NOLOAD memory region of the linker
/// script (32 KiB total: KV at +0, TS at +16 KiB) -- the same region the C
/// firmware's FAL simulator uses -- so they are neither part of ROM nor
/// counted in .data/.bss: they model external NOR parts.
#[repr(C, align(4096))]
struct SimFlashStore<const N: usize>([u8; N]);

#[link_section = ".sim_flash"]
static mut KV_SIM_FLASH: SimFlashStore<{ KV_PARTITION_SIZE as usize }> =
    SimFlashStore([0u8; KV_PARTITION_SIZE as usize]);

#[link_section = ".sim_flash"]
static mut TS_SIM_FLASH: SimFlashStore<{ TS_PARTITION_SIZE as usize }> =
    SimFlashStore([0u8; TS_PARTITION_SIZE as usize]);

#[cfg(feature = "board-u545")]
static mut SIM_SET_BIT_ATTEMPTS: u32 = 0;
#[cfg(feature = "board-u545")]
static mut SIM_ERASES: u32 = 0;
#[cfg(feature = "board-u545")]
static mut SIM_OOB: u32 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SimFlashError {
    OutOfBounds,
    UnalignedErase,
}

/// Minimal NOR flash driver over one [`SimFlashStore`] region.
///
/// `flashdb-sim::RamFlash` is the same simulator used by the host benchmarks,
/// but it owns its buffer by value; in a bare-metal image that buffer would
/// land in `.data` (16 KiB of ROM!) and in the static-RAM metric.  For the
/// footprint measurement the buffers must live in the detached `.sim_flash`
/// region, so the firmware implements the small `Flash` trait here.  One
/// implementation serves both partitions through a base pointer; the driver is
/// harness code present in the C image too (fal_ram_sim.c), so it does not tilt
/// the comparison.
#[derive(Clone, Copy)]
struct SimFlash {
    base: *mut u8,
    len: usize,
}

impl SimFlash {
    const fn new(base: *mut u8, len: usize) -> Self {
        Self { base, len }
    }

    fn check(&self, addr: u32, len: usize) -> Result<usize, SimFlashError> {
        let start = addr as usize;
        let end = match start.checked_add(len) {
            Some(end) => end,
            None => {
                #[cfg(feature = "board-u545")]
                unsafe {
                    SIM_OOB = SIM_OOB.wrapping_add(1);
                }
                return Err(SimFlashError::OutOfBounds);
            }
        };
        if end > self.len {
            #[cfg(feature = "board-u545")]
            unsafe {
                SIM_OOB = SIM_OOB.wrapping_add(1);
            }
            return Err(SimFlashError::OutOfBounds);
        }
        Ok(start)
    }

    /// Put the simulated part into its factory-fresh (erased) state.
    #[inline(never)]
    fn erase_all(&self) {
        unsafe {
            core::ptr::write_bytes(self.base, 0xFF, self.len);
        }
    }
}

impl Flash for SimFlash {
    type Error = SimFlashError;

    const WRITE_GRAN_BITS: u32 = 8;

    #[inline(never)]
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        let start = self.check(addr, buf.len())?;
        unsafe {
            core::ptr::copy_nonoverlapping(self.base.add(start), buf.as_mut_ptr(), buf.len());
        }
        Ok(())
    }

    #[inline(never)]
    fn write(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        let start = self.check(addr, buf.len())?;
        let mut i = 0;
        while i < buf.len() {
            unsafe {
                // NOR programming can only clear bits (1 -> 0).
                let old = core::ptr::read_volatile(self.base.add(start + i));
                let new = old & buf[i];
                #[cfg(feature = "board-u545")]
                if new != buf[i] {
                    // A bit that is 0 in flash was asked to become 1.
                    SIM_SET_BIT_ATTEMPTS = SIM_SET_BIT_ATTEMPTS.wrapping_add(1);
                }
                core::ptr::write_volatile(self.base.add(start + i), new);
            }
            i += 1;
        }
        Ok(())
    }

    #[inline(never)]
    fn erase(&mut self, addr: u32, len: u32) -> Result<(), Self::Error> {
        if addr % SECTOR_SIZE != 0 || len % SECTOR_SIZE != 0 {
            return Err(SimFlashError::UnalignedErase);
        }
        let start = self.check(addr, len as usize)?;
        #[cfg(feature = "board-u545")]
        unsafe {
            SIM_ERASES = SIM_ERASES.wrapping_add(1);
        }
        unsafe {
            core::ptr::write_bytes(self.base.add(start), 0xFF, len as usize);
        }
        Ok(())
    }
}

/// Reset/read the NOR simulation counters (board rollover phase).
#[cfg(feature = "board-u545")]
fn sim_reset_stats() {
    unsafe {
        SIM_SET_BIT_ATTEMPTS = 0;
        SIM_ERASES = 0;
        SIM_OOB = 0;
    }
}

#[cfg(feature = "board-u545")]
fn sim_violations() -> u32 {
    unsafe { SIM_SET_BIT_ATTEMPTS.wrapping_add(SIM_OOB) }
}

#[cfg(feature = "board-u545")]
fn sim_erases() -> u32 {
    unsafe { SIM_ERASES }
}

/// FNV-1a over a whole simulated partition: the on-flash image fingerprint used
/// to prove C/Rust write-identical images on silicon.
#[cfg(feature = "board-u545")]
#[inline(never)]
fn image_checksum(flash: SimFlash) -> u32 {
    let mut h = 0u32;
    let mut i = 0usize;
    while i < flash.len {
        let b = unsafe { core::ptr::read_volatile(flash.base.add(i)) };
        h ^= b as u32;
        h = h.wrapping_mul(16_777_619);
        i += 1;
    }
    h
}

fn kv_flash() -> SimFlash {
    unsafe {
        SimFlash::new(
            core::ptr::addr_of_mut!(KV_SIM_FLASH.0) as *mut u8,
            KV_PARTITION_SIZE as usize,
        )
    }
}

fn ts_flash() -> SimFlash {
    unsafe {
        SimFlash::new(
            core::ptr::addr_of_mut!(TS_SIM_FLASH.0) as *mut u8,
            TS_PARTITION_SIZE as usize,
        )
    }
}

// The database structs live in .bss, mirroring the C firmware's static
// fdb_kvdb/fdb_tsdb, so main()'s frame stays shallow and the stack measurement
// is comparable.
type KvDb = Kvdb<SimFlash>;
type TsDb = Tsdb<SimFlash>;

struct DbCell<T>(UnsafeCell<MaybeUninit<T>>);
unsafe impl<T> Sync for DbCell<T> {}

static KV_DB: DbCell<KvDb> = DbCell(UnsafeCell::new(MaybeUninit::uninit()));
static TS_DB: DbCell<TsDb> = DbCell(UnsafeCell::new(MaybeUninit::uninit()));

#[inline(never)]
fn kv_mount() {
    let flash = kv_flash();
    flash.erase_all();
    // Placement initialization: the database (including its 512-byte name
    // cache) is built directly in .bss, so mounting costs no stack copy.
    // FlashDB-C has the same property because fdb_kvdb_init() takes a pointer.
    let slot = unsafe { &mut *KV_DB.0.get() };
    let _ = ok_or_fail(
        Kvdb::mount_into(slot, flash, Config::new(SECTOR_SIZE, KV_PARTITION_SIZE)),
        TAG_KV_MOUNT,
    );
}

#[inline(never)]
fn ts_mount() {
    let flash = ts_flash();
    flash.erase_all();
    // Placement initialization (mirrors Kvdb::mount_into): the TSDB struct is
    // built directly in .bss so mounting costs no stack copy of the database.
    let slot = unsafe { &mut *TS_DB.0.get() };
    let _ = ok_or_fail(
        Tsdb::mount_into(
            slot,
            flash,
            Config::new(SECTOR_SIZE, TS_PARTITION_SIZE),
            TS_MAX_LEN,
        ),
        TAG_TS_MOUNT,
    );
    let db = unsafe { slot.assume_init_mut() };
    db.set_rollover(false);
}

fn kv() -> &'static mut KvDb {
    unsafe { &mut *(*KV_DB.0.get()).as_mut_ptr() }
}

fn ts() -> &'static mut TsDb {
    unsafe { &mut *(*TS_DB.0.get()).as_mut_ptr() }
}

/* ======================================================================== */
/* Deterministic data generation (byte-identical to c/main.c)               */
/* ======================================================================== */

const HEX: &[u8; 16] = b"0123456789abcdef";

fn make_kv_key(out: &mut [u8; 5], idx: u32) {
    out[0] = b'k';
    out[1] = b'v';
    out[2] = b'.';
    out[3] = HEX[((idx >> 4) & 0xf) as usize];
    out[4] = HEX[(idx & 0xf) as usize];
}

#[inline(always)]
fn hex_digit(v: u32) -> u8 {
    unsafe { *HEX.get_unchecked((v & 0xf) as usize) }
}

fn make_kv_value(out: &mut [u8; KV_VALUE_LEN], idx: u32, gen: u32) {
    // Raw pointer writes: the length is a compile-time constant here, so this
    // avoids bounds-check branches (and their panic paths) in the hot loop.
    let p = out.as_mut_ptr();
    unsafe {
        *p = b'v';
        *p.add(1) = hex_digit(gen >> 12);
        *p.add(2) = hex_digit(gen >> 8);
        *p.add(3) = hex_digit(gen >> 4);
        *p.add(4) = hex_digit(gen);
        *p.add(5) = b'-';
        *p.add(6) = hex_digit(idx >> 4);
        *p.add(7) = hex_digit(idx);
        *p.add(8) = b'-';
        let mut k = 9;
        while k < KV_VALUE_LEN {
            *p.add(k) = hex_digit(idx.wrapping_mul(7) + gen.wrapping_mul(13) + k as u32);
            k += 1;
        }
    }
}

fn make_ts_payload(out: &mut [u8; TS_PAYLOAD_LEN], idx: u32) {
    // Raw pointer writes: fixed compile-time length, avoids bounds-check
    // branches in the 200-iteration append loop.
    let tick = TS_BASE_TIME + idx;
    let p = out.as_mut_ptr();
    unsafe {
        *p = idx as u8;
        *p.add(1) = (idx >> 8) as u8;
        *p.add(2) = (idx >> 16) as u8;
        *p.add(3) = (idx >> 24) as u8;
        *p.add(4) = tick as u8;
        *p.add(5) = (tick >> 8) as u8;
        *p.add(6) = (tick >> 16) as u8;
        *p.add(7) = (tick >> 24) as u8;
        let mut k = 8;
        while k < TS_PAYLOAD_LEN {
            *p.add(k) = (idx.wrapping_mul(31) + (k as u32).wrapping_mul(7)) as u8;
            k += 1;
        }
    }
}

/// The generation stored for `idx` after the whole KV phase; mirrors c/main.c.
fn expected_gen(idx: u32) -> u32 {
    if idx < KV_CHURN_KEYS {
        let last_n = (KV_CHURN - 1) - ((KV_CHURN - 1 - idx) % KV_CHURN_KEYS);
        2 + last_n
    } else if idx < KV_UPDATES {
        1
    } else {
        0
    }
}

fn fnv1a(mut h: u32, data: &[u8]) -> u32 {
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(16_777_619);
    }
    h
}

/* ======================================================================== */
/* KVDB phase -- real Rust flashdb crate                                    */
/* ======================================================================== */

/// One "make key/value and set" step, shared by the initial, update and churn
/// loops (single out-of-line copy instead of three inlined expansions).
#[inline(never)]
fn kv_put(db: &mut KvDb, idx: u32, gen: u32) {
    let mut key = [0u8; 5];
    let mut value = [0u8; KV_VALUE_LEN];

    make_kv_key(&mut key, idx);
    make_kv_value(&mut value, idx, gen);
    if db.set(&key, &value).is_err() {
        fail(TAG_KV, idx);
    }
}

/// One "get and verify the expected value" step (value check + memcmp).
#[inline(never)]
fn kv_get_verified(db: &mut KvDb, idx: u32, buf: &mut [u8; KV_VALUE_LEN]) {
    let mut key = [0u8; 5];
    let mut expected = [0u8; KV_VALUE_LEN];

    make_kv_key(&mut key, idx);
    make_kv_value(&mut expected, idx, expected_gen(idx));
    match db.get(&key, buf) {
        Ok(Some(got)) if got == KV_VALUE_LEN => {}
        _ => fail(TAG_KV, idx),
    }
    if unsafe { memcmp(buf.as_ptr(), expected.as_ptr(), KV_VALUE_LEN) } != 0 {
        fail(TAG_KV, idx);
    }
}

#[inline(never)]
fn kv_phase(m: &mut Metrics) {
    let db = kv();
    let mut read_buf = [0u8; KV_VALUE_LEN];
    let mut iter_buf = [0u8; KV_VALUE_LEN];

    // 1. initial set of all keys, 2. update half of them (one loop: C does
    //    the same two sequential passes; the branch is once per iteration).
    let mut i = 0u32;
    while i < KV_KEYS + KV_UPDATES {
        if i < KV_KEYS {
            kv_put(db, i, 0);
        } else {
            kv_put(db, i - KV_KEYS, 1);
        }
        i += 1;
    }
    m.kv_writes += KV_KEYS + KV_UPDATES;

    // 3. GC churn: rewrite a small key set many times so the database has to
    //    erase and migrate sectors (same pattern as the C workload).
    let mut n = 0u32;
    while n < KV_CHURN {
        kv_put(db, n % KV_CHURN_KEYS, 2 + n);
        m.kv_writes += 1;
        n += 1;
    }

    // 4. get every key, verify and checksum its value (index order, so this
    //    checksum is implementation-order independent and matches the C side).
    i = 0;
    while i < KV_KEYS {
        kv_get_verified(db, i, &mut read_buf);
        m.kv_gets += 1;
        m.kv_checksum = fnv1a(m.kv_checksum, &read_buf);
        i += 1;
    }

    // 5. iterate the whole database (different code path from point lookup).
    //    This checksum depends on iteration order, so it is reported
    //    separately from kv_checksum.
    let mut it = db.iter();
    while let Some(entry) = it.next() {
        let entry = ok_or_fail(entry, TAG_KV);
        m.kv_iter_checksum = fnv1a(m.kv_iter_checksum, entry.name());
        let got = ok_or_fail(it.read_value(&entry, &mut iter_buf), TAG_KV);
        let got_slice =
            unsafe { core::slice::from_raw_parts(iter_buf.as_ptr(), got.min(iter_buf.len())) };
        m.kv_iter_checksum = fnv1a(m.kv_iter_checksum, got_slice);
        m.kv_iterated += 1;
    }

    if m.kv_iterated != KV_KEYS {
        fail(TAG_KV, m.kv_iterated);
    }
}

/* ======================================================================== */
/* TSDB phase -- real Rust flashdb crate                                    */
/*                                                                          */
/* 200 deterministic 16-byte payloads with fake timestamps (1000..1199),    */
/* then a full forward iteration that reads each payload back and checksums */
/* it in iteration order.  This mirrors the C workload exactly, so         */
/* ts_checksum is directly comparable.                                      */
/* ======================================================================== */

#[inline(never)]
fn tsdb_phase(m: &mut Metrics) {
    let db = ts();
    let mut payload = [0u8; TS_PAYLOAD_LEN];
    let mut read_buf = [0u8; TS_PAYLOAD_LEN];
    let mut i = 0u32;

    // append phase
    while i < TS_APPENDS {
        make_ts_payload(&mut payload, i);
        ok_or_fail(db.append((TS_BASE_TIME + i) as i32, &payload), TAG_TS);
        m.ts_appends += 1;
        i += 1;
    }

    // forward iteration phase
    let mut it = db.iter();
    let mut n = 0u32;
    while let Some(entry) = it.next() {
        let entry = ok_or_fail(entry, TAG_TS);
        let got = ok_or_fail(it.read_blob(&entry, &mut read_buf), TAG_TS);
        if got != TS_PAYLOAD_LEN {
            fail(TAG_TS, got as u32);
        }
        m.ts_iterated += 1;
        let got_slice =
            unsafe { core::slice::from_raw_parts(read_buf.as_ptr(), got.min(read_buf.len())) };
        m.ts_checksum = fnv1a(m.ts_checksum, got_slice);
        n += 1;
    }
    if n != TS_APPENDS {
        fail(TAG_TS, n);
    }
}

/* ======================================================================== */
/* Rollover phase: board (`board-u545`) and QEMU proxy (`qemu-cycles`)      */
/* ======================================================================== */

/// One rollover-phase run: the board reports every field in RESULT (so the C
/// reference has to reproduce them field by field); the QEMU cycle proxy runs
/// the identical workload purely to time it (its fields are printed on the
/// QEMU_CYCLES line, not in RESULT).
#[cfg(any(feature = "board-u545", feature = "qemu-cycles"))]
struct RolloverResult {
    appends: u32,
    erases: u32,
    violations: u32,
    count: u32,
    first_time: i32,
    last_time: i32,
    checksum: u32,
    remount_count: u32,
    remount_checksum: u32,
    continue_ok: u32,
}

/// Fill the TSDB ring past its capacity, verify the live window, then recover
/// it from flash.  This is the on-silicon counterpart of the host-side
/// `tsdb_rollover` tests: the same invariants (contiguous live window, payload
/// integrity, ring still accepting appends after remount, no illegal NOR
/// programming).  The board simulation counters only exist in the
/// `board-u545` image, so those two invariants are checked there.
#[cfg(any(feature = "board-u545", feature = "qemu-cycles"))]
#[inline(never)]
fn ts_rollover_workload() -> RolloverResult {
    #[cfg(feature = "board-u545")]
    sim_reset_stats();
    ts().set_rollover(true);

    let mut payload = [0u8; TS_PAYLOAD_LEN];
    let mut i = 0u32;
    while i < TS_ROLL_APPENDS {
        let idx = TS_APPENDS + i;
        make_ts_payload(&mut payload, idx);
        ok_or_fail(
            ts().append((TS_BASE_TIME + idx) as i32, &payload),
            TAG_TS_ROLL,
        );
        i += 1;
    }

    let (count, first, last, checksum) = ts_roll_scan();
    if count == 0 {
        fail(TAG_TS_ROLL, 1);
    }
    if first + count as i32 - 1 != last {
        fail(TAG_TS_ROLL, 2);
    }

    // Remount from flash: the ring state must come back identically (mirrors
    // the C `fdb_tsdb_deinit` + `fdb_tsdb_init` recovery below).
    let slot = unsafe { &mut *TS_DB.0.get() };
    ok_or_fail(
        Tsdb::mount_into(
            slot,
            ts_flash(),
            Config::new(SECTOR_SIZE, TS_PARTITION_SIZE),
            TS_MAX_LEN,
        ),
        TAG_TS_ROLL,
    );
    let (count2, first2, last2, checksum2) = ts_roll_scan();
    if count2 != count || first2 != first || last2 != last || checksum2 != checksum {
        fail(TAG_TS_ROLL, 3);
    }

    // The ring must keep accepting appends after recovery.
    let idx = TS_APPENDS + TS_ROLL_APPENDS;
    make_ts_payload(&mut payload, idx);
    ok_or_fail(
        ts().append((TS_BASE_TIME + idx) as i32, &payload),
        TAG_TS_ROLL,
    );
    if ts().last_time() != (TS_BASE_TIME + idx) as i32 {
        fail(TAG_TS_ROLL, 4);
    }

    let mut result = RolloverResult {
        appends: TS_ROLL_APPENDS,
        erases: 0,
        violations: 0,
        count,
        first_time: first,
        last_time: last,
        checksum,
        remount_count: count2,
        remount_checksum: checksum2,
        continue_ok: 1,
    };

    #[cfg(feature = "board-u545")]
    {
        result.erases = sim_erases();
        result.violations = sim_violations();
        if result.erases == 0 {
            fail(TAG_TS_ROLL, 5);
        }
        if result.violations != 0 {
            fail(TAG_TS_ROLL, 6);
        }
    }

    result
}

/// Board wrapper: run the rollover workload and publish its fields in RESULT.
#[cfg(feature = "board-u545")]
#[inline(never)]
fn ts_rollover_phase(m: &mut Metrics) {
    let r = ts_rollover_workload();
    m.ts_roll_appends = r.appends;
    m.ts_roll_erases = r.erases;
    m.ts_roll_violations = r.violations;
    m.ts_roll_count = r.count;
    m.ts_roll_first_time = r.first_time as u32;
    m.ts_roll_last_time = r.last_time as u32;
    m.ts_roll_checksum = r.checksum;
    m.ts_roll_remount_count = r.remount_count;
    m.ts_roll_remount_checksum = r.remount_checksum;
    m.ts_roll_continue_ok = r.continue_ok;
}

/// Iterate the TSDB live window: verify contiguous increasing timestamps and
/// the expected payload of every entry, and checksum the payloads.
#[cfg(any(feature = "board-u545", feature = "qemu-cycles"))]
#[inline(never)]
fn ts_roll_scan() -> (u32, i32, i32, u32) {
    let mut it = ts().iter();
    let mut payload = [0u8; TS_PAYLOAD_LEN];
    let mut expected = [0u8; TS_PAYLOAD_LEN];
    let mut count = 0u32;
    let mut first = 0i32;
    let mut last = 0i32;
    let mut checksum = 0u32;
    while let Some(entry) = it.next() {
        let entry = ok_or_fail(entry, TAG_TS_ROLL);
        if count > 0 && entry.time != last + 1 {
            fail(TAG_TS_ROLL, 7);
        }
        let got = ok_or_fail(it.read_blob(&entry, &mut payload), TAG_TS_ROLL);
        if got != TS_PAYLOAD_LEN {
            fail(TAG_TS_ROLL, 8);
        }
        make_ts_payload(&mut expected, (entry.time - TS_BASE_TIME as i32) as u32);
        if payload != expected {
            fail(TAG_TS_ROLL, 9);
        }
        checksum = fnv1a(checksum, &payload);
        if count == 0 {
            first = entry.time;
        }
        last = entry.time;
        count += 1;
    }
    (count, first, last, checksum)
}

#[cfg(feature = "board-u545")]
#[inline(never)]
fn print_rollover_result(m: &Metrics) {
    write_bytes(b"RESULT ts_roll_appends=\0");
    write_u32(m.ts_roll_appends);
    write_bytes(b" ts_roll_erases=\0");
    write_u32(m.ts_roll_erases);
    write_bytes(b" ts_roll_violations=\0");
    write_u32(m.ts_roll_violations);
    write_bytes(b" ts_roll_count=\0");
    write_u32(m.ts_roll_count);
    write_bytes(b" ts_roll_first_time=\0");
    write_u32(m.ts_roll_first_time);
    write_bytes(b" ts_roll_last_time=\0");
    write_u32(m.ts_roll_last_time);
    write_bytes(b" ts_roll_checksum=\0");
    write_hex32(m.ts_roll_checksum);
    write_bytes(b" ts_roll_remount_count=\0");
    write_u32(m.ts_roll_remount_count);
    write_bytes(b" ts_roll_remount_checksum=\0");
    write_hex32(m.ts_roll_remount_checksum);
    write_bytes(b" ts_roll_continue_ok=\0");
    write_u32(m.ts_roll_continue_ok);
    write_bytes(b" kv_image_checksum=\0");
    write_hex32(m.kv_image_checksum);
    write_bytes(b" ts_image_checksum=\0");
    write_hex32(m.ts_image_checksum);
    write_bytes(b"\n\0");
}

/* ======================================================================== */
/* Entry point                                                              */
/* ======================================================================== */

/// Per-phase stack attribution.
///
/// Runs AFTER the headline scan, so it never changes the reported `stack`
/// (the stack is repainted between phases, which would otherwise erase the
/// headline high-water marks).  Each phase is measured from a freshly painted
/// stack, so the number is that phase's peak plus the live caller frames.
/// Database state: the full workload has already run, so iteration sees all
/// 64 keys; the churn phase re-runs the same 1500-write GC pattern.
#[cfg(feature = "stack-phases")]
#[inline(never)]
fn stack_diagnostics(m: &mut Metrics) {
    let db = kv();
    let mut key = [0u8; 5];
    let mut value = [0u8; KV_VALUE_LEN];
    let mut read_buf = [0u8; KV_VALUE_LEN];
    let mut iter_buf = [0u8; KV_VALUE_LEN];

    // (b) one set
    stack_repaint();
    make_kv_key(&mut key, 0);
    make_kv_value(&mut value, 0, 0x3000);
    if db.set(&key, &value).is_err() {
        fail(b"st_set\0", 0);
    }
    m.st_set = stack_high_water_bytes();

    // (c) one get
    stack_repaint();
    match db.get(&key, &mut read_buf) {
        Ok(Some(got)) if got == KV_VALUE_LEN => {}
        _ => fail(b"st_get\0", 0),
    }
    m.st_get = stack_high_water_bytes();

    // (d) full iteration
    stack_repaint();
    let mut it = db.iter();
    let mut n = 0u32;
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => fail(b"st_iter\0", n),
        };
        if it.read_value(&entry, &mut iter_buf).is_err() {
            fail(b"st_iter_read\0", n);
        }
        n += 1;
    }
    if n != KV_KEYS {
        fail(b"st_iter_count\0", n);
    }
    m.st_iter = stack_high_water_bytes();

    // (e) GC churn (same write pattern as the measured workload)
    stack_repaint();
    n = 0;
    while n < KV_CHURN {
        let idx = n % KV_CHURN_KEYS;
        make_kv_key(&mut key, idx);
        make_kv_value(&mut value, idx, 0x4000 + n);
        if db.set(&key, &value).is_err() {
            fail(b"st_gc\0", n);
        }
        n += 1;
    }
    m.st_gc = stack_high_water_bytes();

    // (f) one TSDB append, (g) full TSDB iteration.  The diagnostic append
    //     uses the next timestamp after whatever is already in the ring:
    //     on QEMU that is TS_BASE_TIME + TS_APPENDS (200 workload appends, a
    //     compile-time constant so the QEMU image stays byte-identical), but on
    //     the board the rollover phase has filled the ring to
    //     TS_BASE_TIME + TS_APPENDS + TS_ROLL_APPENDS first, so a hard-coded
    //     timestamp would be rejected (time <= last_time).
    let tsdb = ts();
    let mut ts_payload = [0u8; TS_PAYLOAD_LEN];
    let mut ts_buf = [0u8; TS_PAYLOAD_LEN];

    let next_time: i32 = if cfg!(feature = "board-u545") {
        tsdb.last_time() + 1
    } else {
        (TS_BASE_TIME + TS_APPENDS) as i32
    };
    stack_repaint();
    make_ts_payload(&mut ts_payload, (next_time - TS_BASE_TIME as i32) as u32);
    if tsdb.append(next_time, &ts_payload).is_err() {
        fail(b"st_ts_append\0", 0);
    }
    m.st_ts_append = stack_high_water_bytes();

    stack_repaint();
    let mut ts_it = tsdb.iter();
    let mut ts_n = 0u32;
    while let Some(entry) = ts_it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => fail(b"st_ts_iter\0", ts_n),
        };
        let got = match ts_it.read_blob(&entry, &mut ts_buf) {
            Ok(g) => g,
            Err(_) => fail(b"st_ts_iter_read\0", ts_n),
        };
        if got != TS_PAYLOAD_LEN {
            fail(b"st_ts_iter_len\0", got as u32);
        }
        ts_n += 1;
    }
    // QEMU (no rollover) has a fixed live window: the 200 workload records plus
    // this diagnostic append.  The board rollover phase leaves a sliding window
    // whose length depends on the ring capacity, so only sanity-check it there.
    #[cfg(not(feature = "board-u545"))]
    if ts_n != TS_APPENDS + 1 {
        fail(b"st_ts_iter_count\0", ts_n);
    }
    #[cfg(feature = "board-u545")]
    if ts_n == 0 {
        fail(b"st_ts_iter_count\0", ts_n);
    }
    m.st_ts_iter = stack_high_water_bytes();
}

#[inline(never)]
fn print_result(m: &Metrics) {
    // Field order must match the `Metrics` declaration (repr(C), all u32).
    const LABELS: [&[u8]; 16] = [
        b"RESULT text=\0",
        b" rodata=\0",
        b" data=\0",
        b" bss=\0",
        b" sim_flash=\0",
        b" stack=\0",
        b" stack_init=\0",
        b" stack_limit=\0",
        b" kv_writes=\0",
        b" kv_gets=\0",
        b" kv_iterated=\0",
        b" kv_checksum=\0",
        b" kv_iter_checksum=\0",
        b" ts_appends=\0",
        b" ts_iterated=\0",
        b" ts_checksum=\0",
    ];
    let values = m as *const Metrics as *const u32;
    let mut i = 0;
    while i < LABELS.len() {
        write_bytes(LABELS[i]);
        // checksums are printed in hex; labels 11 and 12 are the KV checksums,
        // 15 is the TS checksum (see LABELS order).
        match i {
            11 | 12 | 15 => write_hex32(unsafe { *values.add(i) }),
            _ => write_u32(unsafe { *values.add(i) }),
        }
        #[cfg(feature = "stack-phases")]
        if i == 7 {
            // Diagnostic fields go right after stack_limit, matching C's
            // harness_print_result() field order.
            write_bytes(b" st_set=\0");
            write_u32(m.st_set);
            write_bytes(b" st_get=\0");
            write_u32(m.st_get);
            write_bytes(b" st_iter=\0");
            write_u32(m.st_iter);
            write_bytes(b" st_gc=\0");
            write_u32(m.st_gc);
            write_bytes(b" st_ts_append=\0");
            write_u32(m.st_ts_append);
            write_bytes(b" st_ts_iter=\0");
            write_u32(m.st_ts_iter);
        }
        i += 1;
    }
    write_bytes(b" rc=0\n\0");
}

/// Standard KVDB+TSDB measurement workload (QEMU and board comparison).
fn standard_workload() -> ! {
    let mut m = Metrics::zeroed();
    fill_section_sizes(&mut m);
    m.stack_limit = stack_limit_bytes();

    // QEMU cycle/instruction proxy: probe the time sources, start the
    // free-running SysTick, then take the phase-timing baseline (mod qcycles).
    #[cfg(feature = "qemu-cycles")]
    qcycles::probe();
    #[cfg(feature = "qemu-cycles")]
    qcycles::init();
    #[cfg(feature = "qemu-cycles")]
    let q_start = qcycles::now();

    #[cfg(feature = "board-u545")]
    board::reset_output();
    #[cfg(feature = "board-u545")]
    board::init_cycles();
    #[cfg(feature = "board-u545")]
    let t0 = board::cycles();

    // Database init (compares with the C fdb_kvdb_init/fdb_tsdb_init phase).
    kv_mount();
    ts_mount();

    #[cfg(feature = "board-u545")]
    let cycles_mount = board::cycles().wrapping_sub(t0);
    #[cfg(feature = "qemu-cycles")]
    let q_after_mount = qcycles::now();
    #[cfg(feature = "qemu-cycles")]
    let q_mount = qcycles::delta(q_start, q_after_mount);
    #[cfg(feature = "board-u545")]
    let t_kv = board::cycles();

    // High-water reached by initialisation, before the workload starts.
    m.stack_init = stack_high_water_bytes();

    kv_phase(&mut m);

    #[cfg(feature = "board-u545")]
    let cycles_kv = board::cycles().wrapping_sub(t_kv);
    #[cfg(feature = "qemu-cycles")]
    let q_after_kv = qcycles::now();
    #[cfg(feature = "qemu-cycles")]
    let q_kv = qcycles::delta(q_after_mount, q_after_kv);
    #[cfg(feature = "board-u545")]
    let t_ts = board::cycles();

    tsdb_phase(&mut m);

    #[cfg(feature = "board-u545")]
    let cycles_ts = board::cycles().wrapping_sub(t_ts);
    #[cfg(feature = "qemu-cycles")]
    let q_after_ts = qcycles::now();
    #[cfg(feature = "qemu-cycles")]
    let q_ts = qcycles::delta(q_after_kv, q_after_ts);

    // Rollover phase (ring fill + wrap + recovery), measured as its own phase
    // and included in the final stack scan and total.  On the board it feeds the
    // RESULT fields; under the QEMU proxy it exists only to be timed.
    #[cfg(feature = "board-u545")]
    let t_roll = board::cycles();
    #[cfg(feature = "qemu-cycles")]
    let q_roll_start = qcycles::now();
    #[cfg(feature = "board-u545")]
    ts_rollover_phase(&mut m);
    #[cfg(feature = "qemu-cycles")]
    let q_roll_result = ts_rollover_workload();
    #[cfg(feature = "board-u545")]
    let cycles_roll = board::cycles().wrapping_sub(t_roll);
    #[cfg(feature = "qemu-cycles")]
    let q_roll = qcycles::delta(q_roll_start, qcycles::now());

    // Final scan after both phases have returned to this shallow frame.
    m.stack = stack_high_water_bytes();

    // Per-phase attribution (does not affect m.stack: repaints happen after).
    #[cfg(feature = "stack-phases")]
    stack_diagnostics(&mut m);

    print_result(&m);

    #[cfg(feature = "qemu-cycles")]
    {
        // Deterministic SysTick-tick cost of each phase (QEMU virtual time; see
        // mod qcycles for the calibration).  Deliberately a separate line so
        // the documented QEMU RESULT baseline (feature off) stays identical.
        let total = q_mount
            .wrapping_add(q_kv)
            .wrapping_add(q_ts)
            .wrapping_add(q_roll);
        write_bytes(b"QEMU_CYCLES unit=systick spin_insn=\0");
        write_u32(qcycles::SPIN_INSTRUCTIONS);
        write_bytes(b" spin_ticks=\0");
        write_u32(qcycles::spin_ticks());
        write_bytes(b" mount=\0");
        write_u32(q_mount);
        write_bytes(b" kv=\0");
        write_u32(q_kv);
        write_bytes(b" ts=\0");
        write_u32(q_ts);
        write_bytes(b" roll=\0");
        write_u32(q_roll);
        write_bytes(b" total=\0");
        write_u32(total);
        write_bytes(b" roll_appends=\0");
        write_u32(q_roll_result.appends);
        write_bytes(b" roll_count=\0");
        write_u32(q_roll_result.count);
        write_bytes(b" roll_checksum=\0");
        write_hex32(q_roll_result.checksum);
        write_bytes(b" remount_count=\0");
        write_u32(q_roll_result.remount_count);
        write_bytes(b" remount_checksum=\0");
        write_hex32(q_roll_result.remount_checksum);
        write_bytes(b" continue_ok=\0");
        write_u32(q_roll_result.continue_ok);
        write_bytes(b" stack=\0");
        write_u32(m.stack);
        write_bytes(b"\n\0");
    }

    #[cfg(feature = "board-u545")]
    {
        // Raw partition fingerprints: byte-level C/Rust image identity.
        m.kv_image_checksum = image_checksum(kv_flash());
        m.ts_image_checksum = image_checksum(ts_flash());
        print_rollover_result(&m);
    }

    #[cfg(feature = "board-u545")]
    {
        write_bytes(b"BOARD cycles_mount=\0");
        write_u32(cycles_mount);
        write_bytes(b" cycles_kv=\0");
        write_u32(cycles_kv);
        write_bytes(b" cycles_ts=\0");
        write_u32(cycles_ts);
        write_bytes(b" cycles_roll=\0");
        write_u32(cycles_roll);
        write_bytes(b" cycles_total=\0");
        write_u32(board::cycles());
        write_bytes(b" core_hz=\0");
        write_u32(board::core_hz());
        write_bytes(b"\n\0");
    }
    exit()
}

/// Firmware entry point.
///
/// The on-silicon internal-flash tests replace the standard measurement
/// workload at compile time: `flash-endurance` runs the endurance loop and
/// `flash-longrun` runs the KVDB+TSDB long-run workload (both use the real
/// STM32U5 flash controller, see `realfash`). Without either feature the
/// standard comparison workload runs (QEMU measurement or board acceptance).
#[no_mangle]
pub extern "C" fn main() -> ! {
    #[cfg(feature = "flash-endurance")]
    {
        flash_endurance::run()
    }
    #[cfg(feature = "flash-longrun")]
    {
        flash_longrun::run()
    }
    #[cfg(feature = "flash-parity")]
    {
        flash_parity::run()
    }
    #[cfg(feature = "flash-fault-write")]
    {
        flash_fault_write::run()
    }
    #[cfg(feature = "flash-fault-verify")]
    {
        flash_fault_verify::run()
    }
    #[cfg(feature = "flash-reboot-soak")]
    {
        flash_reboot_soak::run()
    }
    #[cfg(feature = "flash-erase-fault")]
    {
        flash_erase_fault::run()
    }
    #[cfg(not(any(
        feature = "flash-endurance",
        feature = "flash-longrun",
        feature = "flash-parity",
        feature = "flash-fault-write",
        feature = "flash-fault-verify",
        feature = "flash-reboot-soak",
        feature = "flash-erase-fault"
    )))]
    {
        standard_workload()
    }
}
