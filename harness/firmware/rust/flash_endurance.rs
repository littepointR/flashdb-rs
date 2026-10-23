//! On-silicon flash endurance test (`flash-endurance` feature).
//!
//! Runs real program/erase cycles over **all eight** reserved STM32U5
//! internal-flash scratch pages (pages 24..31 of bank 1, 8 KiB each) and
//! verifies every cycle:
//!
//! * program one quad-word, read it back and compare;
//! * every 16th cycle program *all* 512 quad-words of the page, verify every
//!   word, then erase;
//! * erase the page, verify the whole 8 KiB page reads back `0xFF`.
//!
//! Cycles rotate round-robin over the eight pages (`cycle % 8`), so one run
//! samples all pages instead of wearing a single page, and the RESULT line
//! reports per-page P/E counters plus the total.
//!
//! The controller error flags (NSSR) are checked after every operation, so a
//! programming/erase/ECC failure aborts the run with the raw status word.
//! The loop is time-budgeted with the DWT cycle counter ([`BUDGET_SECONDS`],
//! 240 s at the 4 MHz reset clock); the number of completed P/E cycles is
//! reported.

use super::hwflash::{HwError, RealFlash, PAGE_SIZE, SCRATCH_BASE, SCRATCH_PAGES};
use super::{board, fail, stack_high_water_bytes, write_bytes, write_hex32, write_u32};

/// Endurance loop time budget: 240 s at the measured core clock.  With the
/// eight-page rotation this is ~2.5 k P/E cycles per page per run (the
/// STM32U5 flash endurance rating is 10 k erase/write cycles per page, so a
/// single run stays well inside it and the wear is spread over the whole
/// scratch region).
const BUDGET_SECONDS: u32 = 240;
/// Safety cap (the time budget normally stops first).
const MAX_CYCLES: u32 = 40_000;
/// Every Nth cycle programs a full page instead of a single quad-word.
const FULL_PAGE_EVERY: u32 = 16;
/// Reserve registers a static-size counter array independent of the driver's
/// page constant.
const PAGES: usize = SCRATCH_PAGES as usize;
/// The RESULT schema names one field per scratch page (`pe_p0`..`pe_p7`).
const _: () = assert!(SCRATCH_PAGES == 8);
const _: () = assert!(PAGES == 8);

/// Quad-word pattern for a P/E cycle. Mixes zero, `0xFF` and data bytes so the
/// program/ECC paths see both all-clear and all-set bit patterns.
fn quadword(cycle: u32, page: u32) -> [u8; 16] {
    let mut q = [0u8; 16];
    let mut i = 0;
    while i < 16 {
        q[i] = match i % 4 {
            0 => 0x00,
            1 => 0xFF,
            2 => (cycle & 0xFF) as u8,
            _ => ((cycle >> 8) as u8 ^ (i as u8).wrapping_mul(0x3D)).wrapping_add(page as u8),
        };
        i += 1;
    }
    q
}

/// Word `n` of the full-page pattern for (`cycle`, `page`).
fn page_word(cycle: u32, page: u32, n: u32) -> u32 {
    cycle
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(n.wrapping_mul(0x85EB_CA6B))
        .wrapping_add(page.wrapping_mul(0xC2B2_AE35))
        .rotate_left((n & 31) as u32)
}

fn print_hw_error(stage: &[u8], code: u32, e: HwError) -> ! {
    write_bytes(b"hw_op=\0");
    write_u32(e.op as u32);
    write_bytes(b" hw_status=\0");
    write_hex32(e.status);
    fail(stage, code)
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

    let mut flash = RealFlash::new(SCRATCH_BASE, PAGE_SIZE * SCRATCH_PAGES)
        .unwrap_or_else(|e| print_hw_error(b"flash_init\0", 0, e));
    let regs = flash.registers();

    // One warm-up erase/program/erase/verify cycle per page so a bad page or
    // driver bug shows up immediately (before the time-budgeted loop).  This
    // also proves every one of the eight reserved pages is usable.
    let mut page = 0u32;
    while page < SCRATCH_PAGES {
        let base = SCRATCH_BASE + page * PAGE_SIZE;
        flash
            .raw_erase_page(base)
            .unwrap_or_else(|e| print_hw_error(b"warm_erase\0", page, e));
        if !flash.raw_is_erased(base, PAGE_SIZE) {
            fail(b"warm_erased\0", page);
        }
        let warm = quadword(0, page);
        flash
            .raw_program_quadword(base, &warm)
            .unwrap_or_else(|e| print_hw_error(b"warm_program\0", page, e));
        let mut readback = [0u8; 16];
        flash.raw_read(base, &mut readback);
        if readback != warm {
            fail(b"warm_verify\0", page);
        }
        flash
            .raw_erase_page(base)
            .unwrap_or_else(|e| print_hw_error(b"warm_erase2\0", page, e));
        if !flash.raw_is_erased(base, PAGE_SIZE) {
            fail(b"warm_erased2\0", page);
        }
        page += 1;
    }

    // Per-page P/E counters reported to the host.
    let mut page_cycles = [0u32; PAGES];
    let mut checksum = 0x811C_9DC5u32;
    let mut cycles = 0u32;
    let start = board::cycles();
    loop {
        if cycles >= MAX_CYCLES || board::cycles().wrapping_sub(start) >= budget {
            break;
        }
        let page = (cycles % SCRATCH_PAGES) as u32;
        let base = SCRATCH_BASE + page * PAGE_SIZE;

        // Program + verify + erase + verify-erased, once per P/E cycle.
        if cycles % FULL_PAGE_EVERY == 0 {
            // Full-page program: 512 quad-words (8 KiB / 16 B).
            let mut w = 0u32;
            while w < PAGE_SIZE / 16 {
                let mut q = [0u8; 16];
                let mut i = 0;
                while i < 4 {
                    q[i * 4..i * 4 + 4]
                        .copy_from_slice(&page_word(cycles, page, w * 4 + i as u32).to_le_bytes());
                    i += 1;
                }
                flash
                    .raw_program_quadword(base + w * 16, &q)
                    .unwrap_or_else(|e| print_hw_error(b"program\0", (page << 16) | w, e));
                w += 1;
            }
            // Verify every word of the page.
            let mut n = 0u32;
            while n < (PAGE_SIZE / 4) {
                let word = flash.raw_read_u32(base + n * 4);
                if word != page_word(cycles, page, n) {
                    fail(b"verify\0", (page << 24) | n);
                }
                checksum ^= word;
                checksum = checksum.wrapping_mul(0x0100_0193);
                n += 1;
            }
        } else {
            let q = quadword(cycles, page);
            flash
                .raw_program_quadword(base, &q)
                .unwrap_or_else(|e| print_hw_error(b"program\0", page, e));
            let mut readback = [0u8; 16];
            flash.raw_read(base, &mut readback);
            if readback != q {
                fail(b"verify\0", (page << 24) | cycles);
            }
            let mut i = 0;
            while i < 16 {
                checksum ^= readback[i] as u32;
                checksum = checksum.wrapping_mul(0x0100_0193);
                i += 1;
            }
        }
        flash
            .raw_erase_page(base)
            .unwrap_or_else(|e| print_hw_error(b"erase\0", page, e));
        if !flash.raw_is_erased(base, PAGE_SIZE) {
            fail(b"erased\0", (page << 24) | cycles);
        }
        page_cycles[page as usize] = page_cycles[page as usize].wrapping_add(1);
        cycles += 1;
    }

    let elapsed = board::cycles().wrapping_sub(start);
    let pe = cycles;
    let stack = stack_high_water_bytes();

    write_bytes(b"RESULT kind=endurance pe_cycles=\0");
    write_u32(pe);
    write_bytes(b" pages=\0");
    write_u32(SCRATCH_PAGES);
    let mut page = 0u32;
    while page < SCRATCH_PAGES {
        // Field names pe_p0..pe_p7 are part of the reported schema.
        const PREFIXES: [&[u8]; 8] = [
            b" pe_p0=\0",
            b" pe_p1=\0",
            b" pe_p2=\0",
            b" pe_p3=\0",
            b" pe_p4=\0",
            b" pe_p5=\0",
            b" pe_p6=\0",
            b" pe_p7=\0",
        ];
        write_bytes(PREFIXES[page as usize]);
        write_u32(page_cycles[page as usize]);
        page += 1;
    }
    write_bytes(b" programs=\0");
    write_u32(flash.programs);
    write_bytes(b" erases_total=\0");
    write_u32(flash.erases);
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
