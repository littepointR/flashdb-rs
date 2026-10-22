//! On-silicon flash endurance test (`flash-endurance` feature).
//!
//! Runs real program/erase cycles on one STM32U5 internal-flash page and
//! verifies every cycle:
//!
//! * program one quad-word, read it back and compare;
//! * erase the page, verify the whole 8 KiB page reads back `0xFF`;
//! * every 16th cycle program *all* 512 quad-words of the page, verify every
//!   word, then erase.
//!
//! The controller error flags (NSSR) are checked after every operation, so a
//! programming/erase/ECC failure aborts the run with the raw status word.
//! The loop is time-budgeted with the DWT cycle counter (~60 s at the 4 MHz
//! reset clock); the number of completed P/E cycles is reported.

use super::hwflash::{HwError, RealFlash, PAGE_SIZE, SCRATCH_BASE};
use super::{board, fail, stack_high_water_bytes, write_bytes, write_hex32, write_u32};

/// Endurance loop time budget: 60 s at the measured core clock.
const BUDGET_SECONDS: u32 = 120;
/// Safety cap (the time budget normally stops first).
const MAX_CYCLES: u32 = 20_000;
/// Every Nth cycle programs a full page instead of a single quad-word.
const FULL_PAGE_EVERY: u32 = 16;

/// Quad-word pattern for a P/E cycle. Mixes zero, `0xFF` and data bytes so the
/// program/ECC paths see both all-clear and all-set bit patterns.
fn quadword(cycle: u32) -> [u8; 16] {
    let mut q = [0u8; 16];
    let mut i = 0;
    while i < 16 {
        q[i] = match i % 4 {
            0 => 0x00,
            1 => 0xFF,
            2 => (cycle & 0xFF) as u8,
            _ => (cycle >> 8) as u8 ^ (i as u8).wrapping_mul(0x3D),
        };
        i += 1;
    }
    q
}

/// Word `n` of the full-page pattern for `cycle`.
fn page_word(cycle: u32, n: u32) -> u32 {
    cycle
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(n.wrapping_mul(0x85EB_CA6B))
        .rotate_left((n & 31) as u32)
}

fn print_hw_error(stage: &[u8], e: HwError) -> ! {
    write_bytes(b"hw_op=\0");
    write_u32(e.op as u32);
    write_bytes(b" hw_status=\0");
    write_hex32(e.status);
    fail(stage, e.status)
}

pub fn run() -> ! {
    board::reset_output();
    board::init_cycles();
    let hz = match board::core_hz() {
        0 => 4_000_000,
        h => h,
    };
    let budget = hz.saturating_mul(BUDGET_SECONDS);
    let stack_limit = super::stack_limit_bytes();

    let mut flash = RealFlash::new(SCRATCH_BASE, PAGE_SIZE)
        .unwrap_or_else(|e| print_hw_error(b"flash_init\0", e));
    let regs = flash.registers();

    // One warm-up erase/program/erase/verify cycle so a driver bug shows up
    // immediately (before the time-budgeted loop).
    let warm = quadword(0);
    flash
        .raw_erase_page(SCRATCH_BASE)
        .unwrap_or_else(|e| print_hw_error(b"warm_erase\0", e));
    if !flash.raw_is_erased(SCRATCH_BASE, PAGE_SIZE) {
        fail(b"warm_erased\0", 0);
    }
    flash
        .raw_program_quadword(SCRATCH_BASE, &warm)
        .unwrap_or_else(|e| print_hw_error(b"warm_program\0", e));
    let mut readback = [0u8; 16];
    flash.raw_read(SCRATCH_BASE, &mut readback);
    if readback != warm {
        fail(b"warm_verify\0", 0);
    }
    flash
        .raw_erase_page(SCRATCH_BASE)
        .unwrap_or_else(|e| print_hw_error(b"warm_erase2\0", e));
    if !flash.raw_is_erased(SCRATCH_BASE, PAGE_SIZE) {
        fail(b"warm_erased2\0", 1);
    }

    let mut checksum = 0x811C_9DC5u32;
    let mut cycles = 0u32;
    let start = board::cycles();
    loop {
        if cycles >= MAX_CYCLES || board::cycles().wrapping_sub(start) >= budget {
            break;
        }
        // Program + verify + erase + verify-erased, once per P/E cycle.
        if cycles % FULL_PAGE_EVERY == 0 {
            // Full-page program: 512 quad-words (8 KiB / 16 B).
            let mut w = 0u32;
            while w < PAGE_SIZE / 16 {
                let mut q = [0u8; 16];
                let mut i = 0;
                while i < 4 {
                    q[i * 4..i * 4 + 4]
                        .copy_from_slice(&page_word(cycles, w * 4 + i as u32).to_le_bytes());
                    i += 1;
                }
                flash
                    .raw_program_quadword(SCRATCH_BASE + w * 16, &q)
                    .unwrap_or_else(|e| print_hw_error(b"program\0", e));
                w += 1;
            }
            // Verify every word of the page.
            let mut n = 0u32;
            while n < (PAGE_SIZE / 4) {
                let word = flash.raw_read_u32(SCRATCH_BASE + n * 4);
                if word != page_word(cycles, n) {
                    fail(b"verify\0", n);
                }
                checksum ^= word;
                checksum = checksum.wrapping_mul(0x0100_0193);
                n += 1;
            }
        } else {
            let q = quadword(cycles);
            flash
                .raw_program_quadword(SCRATCH_BASE, &q)
                .unwrap_or_else(|e| print_hw_error(b"program\0", e));
            let mut readback = [0u8; 16];
            flash.raw_read(SCRATCH_BASE, &mut readback);
            if readback != q {
                fail(b"verify\0", cycles);
            }
            let mut i = 0;
            while i < 16 {
                checksum ^= readback[i] as u32;
                checksum = checksum.wrapping_mul(0x0100_0193);
                i += 1;
            }
        }
        flash
            .raw_erase_page(SCRATCH_BASE)
            .unwrap_or_else(|e| print_hw_error(b"erase\0", e));
        if !flash.raw_is_erased(SCRATCH_BASE, PAGE_SIZE) {
            fail(b"erased\0", cycles);
        }
        cycles += 1;
    }

    let elapsed = board::cycles().wrapping_sub(start);
    let pe = flash.erases;
    let stack = stack_high_water_bytes();

    write_bytes(b"RESULT kind=endurance pe_cycles=\0");
    write_u32(pe);
    write_bytes(b" programs=\0");
    write_u32(flash.programs);
    write_bytes(b" elapsed_cycles=\0");
    write_u32(elapsed);
    write_bytes(b" budget_cycles=\0");
    write_u32(budget);
    write_bytes(b" checksum=\0");
    write_hex32(checksum);
    write_bytes(b" errors=0 verify_ok=1 erased_ok=1 rc=0\n\0");

    write_bytes(b"BOARD core_hz=\0");
    write_u32(hz);
    write_bytes(b" acr=\0");
    write_hex32(regs[0]);
    write_bytes(b" nssr=\0");
    write_hex32(regs[1]);
    write_bytes(b" nscr=\0");
    write_hex32(regs[2]);
    write_bytes(b" eccr=\0");
    write_hex32(regs[3]);
    write_bytes(b" stack=\0");
    write_u32(stack);
    write_bytes(b" stack_limit=\0");
    write_u32(stack_limit);
    write_bytes(b" last_nssr=\0");
    write_hex32(flash.last_status);
    write_bytes(b"\n\0");

    super::exit()
}
