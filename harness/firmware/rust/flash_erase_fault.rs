//! On-silicon deterministic mid-erase reset injection (`flash-erase-fault`).
//!
//! The external debug-probe power-loss test (`run_flash_fault.ps1`) catches
//! quad-word programming windows and gaps between operations, but never lands
//! inside a page erase because executing `wait_idle` from Bank 1 Flash stalls
//! CPU instruction fetch until the Bank 1 erase completes.
//!
//! This test closes that gap on real STM32U5 silicon:
//! * `hwflash::RealFlash::arm_erase_reset` diverts the target page erase to an
//!   SRAM-resident routine (`ram_erase_and_reset` in `.data`).
//! * Because the routine runs from SRAM1 (`0x2000_xxxx`) with interrupts
//!   masked, starting a page erase on Bank 1 (`0x0803_xxxx`) does **not** stall
//!   instruction fetch.  The routine polls `FLASH_NSSR` live from SRAM,
//!   confirms `BSY == 1` (`bit 16`), spins a programmable number of SRAM loops
//!   inside the active erase window, records `[abs_addr, nssr_at_reset, loops]`
//!   into `.results` NOLOAD SRAM, and asserts `SCB->AIRCR = SYSRESETREQ` while
//!   `BSY == 1`.
//! * On the next boot, the firmware verifies that `nssr_at_reset` had `BSY == 1`
//!   at the instant of reset, inspects the interrupted scratch page, mounts
//!   both `Kvdb` and `Tsdb`, runs `Kvdb::check()`, validates every surviving
//!   key and TS entry, performs fresh writes to both databases, and advances to
//!   the next round.
//! * Even rounds inject the reset during a **KVDB garbage-collection page
//!   erase**; odd rounds inject the reset during a **TSDB rollover page
//!   erase**.

use super::hwflash::{HwError, RealFlash, PAGE_SIZE, SCRATCH_BASE, SCRATCH_END};
use super::{board, fail, stack_high_water_bytes, write_bytes, write_hex32, write_u32};
use flashdb::{Config, Error, Kvdb, Tsdb, TslStatus};

const SEC_SIZE: u32 = PAGE_SIZE;
const SECTORS: u32 = 4;
const PART_SIZE: u32 = SEC_SIZE * SECTORS;
const KV_BASE: u32 = SCRATCH_BASE;
const TS_BASE: u32 = SCRATCH_BASE + PART_SIZE;

const ROUNDS: u32 = 20;
const KV_KEYS: u32 = 8;
/// 384-byte values so ~20 updates fill an 8 KiB sector and trigger GC erase
/// quickly in every even round.
const KV_VALUE_LEN: usize = 384;
const TS_PAYLOAD_LEN: usize = 32;
const TS_MAX_LEN: u32 = 64;

const NSSR_BSY: u32 = 1 << 16;

const EFAULT_MAGIC: u32 = 0x4546_4C54; // "EFLT"
const EFAULT_SCHEMA: u32 = 0x0200_0000 | (ROUNDS << 16) | (KV_VALUE_LEN as u32);

/// SRAM delay-loop sweep across the ~1.5 ms page-erase busy window (~686 SRAM
/// poll iterations on STM32U5 at boot clock). Every entry is < 686 so
/// `SYSRESETREQ` always fires while `FLASH_NSSR.BSY == 1`.
const DELAY_TABLE: [u32; 10] = [0, 4, 16, 32, 64, 128, 192, 256, 384, 512];

#[repr(C)]
#[derive(Clone, Copy)]
struct EraseFaultState {
    magic: u32,
    schema: u32,
    /// Round currently armed (or completed).
    round: u32,
    /// `1` when a mid-erase reset was armed for `round` and we expect to wake
    /// up from that reset on the next boot.
    armed: u32,
    boot_count: u32,
    resets: u32,
    bsy_confirmed: u32,
    kv_erase_resets: u32,
    ts_erase_resets: u32,
    page_erased_after: u32,
    page_partial_after: u32,
    next_gen: u32,
    last_addr: u32,
    last_sr: u32,
    last_loops: u32,
}

#[link_section = ".results"]
static mut EF_STATE: EraseFaultState = EraseFaultState {
    magic: 0,
    schema: 0,
    round: 0,
    armed: 0,
    boot_count: 0,
    resets: 0,
    bsy_confirmed: 0,
    kv_erase_resets: 0,
    ts_erase_resets: 0,
    page_erased_after: 0,
    page_partial_after: 0,
    next_gen: 0,
    last_addr: 0,
    last_sr: 0,
    last_loops: 0,
};

/// 3 words written directly by `ram_erase_and_reset` in SRAM right before
/// `SYSRESETREQ`: `[abs_addr, nssr_at_reset, sram_loops]`.
#[link_section = ".results"]
static mut ERASE_OUT: [u32; 3] = [0; 3];

/// Count of STM32U5 Flash ECC double-error NMIs (`FLASH_ECCR.ECCD`) raised and
/// cleared while reading partially-erased scratch pages during recovery.
#[link_section = ".results"]
static mut ECC_NMIS: u32 = 0;

#[repr(C, align(128))]
struct RamVectors([u32; 16]);

static mut RAM_VECTORS: RamVectors = RamVectors([0; 16]);

/// On STM32U5 (RM0456 Section 7.4.6), reading a 128-bit quad-word from a page
/// whose erase was aborted mid-pulse can trigger a hardware ECC double-error
/// (`FLASH_ECCR.ECCD = 1`), which asserts the Cortex-M33 NMI line.  Clearing
/// `ECCD | ECCC` (`0xC000_0000`) in `FLASH_ECCR` acknowledges the ECC event
/// and lets the recovery scan continue.
unsafe extern "C" fn ecc_nmi_handler() {
    const FLASH_ECCR: *mut u32 = 0x4002_2030 as *mut u32;
    let eccr = core::ptr::read_volatile(FLASH_ECCR);
    if (eccr & 0xC000_0000) != 0 {
        core::ptr::write_volatile(FLASH_ECCR, eccr | 0xC000_0000);
        core::arch::asm!("dsb", options(nomem, nostack, preserves_flags));
        let p = core::ptr::addr_of_mut!(ECC_NMIS);
        core::ptr::write_volatile(p, core::ptr::read_volatile(p).wrapping_add(1));
    } else {
        loop {
            core::hint::spin_loop();
        }
    }
}

fn install_ecc_nmi_vector() {
    unsafe {
        let src = 0x0800_0000 as *const u32;
        let dst = core::ptr::addr_of_mut!(RAM_VECTORS.0) as *mut u32;
        let mut i = 0usize;
        while i < 16 {
            core::ptr::write_volatile(dst.add(i), core::ptr::read_volatile(src.add(i)));
            i += 1;
        }
        core::ptr::write_volatile(dst.add(2), ecc_nmi_handler as usize as u32);
        core::arch::asm!("dsb", options(nomem, nostack, preserves_flags));
        core::ptr::write_volatile(0xE000_ED08 as *mut u32, dst as u32);
        core::arch::asm!("dsb", options(nomem, nostack, preserves_flags));
        core::arch::asm!("isb", options(nomem, nostack, preserves_flags));
    }
}

fn state_load() -> EraseFaultState {
    unsafe {
        let p = core::ptr::addr_of!(EF_STATE) as *const u32;
        EraseFaultState {
            magic: core::ptr::read_volatile(p),
            schema: core::ptr::read_volatile(p.add(1)),
            round: core::ptr::read_volatile(p.add(2)),
            armed: core::ptr::read_volatile(p.add(3)),
            boot_count: core::ptr::read_volatile(p.add(4)),
            resets: core::ptr::read_volatile(p.add(5)),
            bsy_confirmed: core::ptr::read_volatile(p.add(6)),
            kv_erase_resets: core::ptr::read_volatile(p.add(7)),
            ts_erase_resets: core::ptr::read_volatile(p.add(8)),
            page_erased_after: core::ptr::read_volatile(p.add(9)),
            page_partial_after: core::ptr::read_volatile(p.add(10)),
            next_gen: core::ptr::read_volatile(p.add(11)),
            last_addr: core::ptr::read_volatile(p.add(12)),
            last_sr: core::ptr::read_volatile(p.add(13)),
            last_loops: core::ptr::read_volatile(p.add(14)),
        }
    }
}

fn state_store(s: EraseFaultState) {
    unsafe {
        let p = core::ptr::addr_of_mut!(EF_STATE) as *mut u32;
        core::ptr::write_volatile(p, s.magic);
        core::ptr::write_volatile(p.add(1), s.schema);
        core::ptr::write_volatile(p.add(2), s.round);
        core::ptr::write_volatile(p.add(3), s.armed);
        core::ptr::write_volatile(p.add(4), s.boot_count);
        core::ptr::write_volatile(p.add(5), s.resets);
        core::ptr::write_volatile(p.add(6), s.bsy_confirmed);
        core::ptr::write_volatile(p.add(7), s.kv_erase_resets);
        core::ptr::write_volatile(p.add(8), s.ts_erase_resets);
        core::ptr::write_volatile(p.add(9), s.page_erased_after);
        core::ptr::write_volatile(p.add(10), s.page_partial_after);
        core::ptr::write_volatile(p.add(11), s.next_gen);
        core::ptr::write_volatile(p.add(12), s.last_addr);
        core::ptr::write_volatile(p.add(13), s.last_sr);
        core::ptr::write_volatile(p.add(14), s.last_loops);
    }
}

fn fresh_state() -> EraseFaultState {
    EraseFaultState {
        magic: EFAULT_MAGIC,
        schema: EFAULT_SCHEMA,
        round: 0,
        armed: 0,
        boot_count: 0,
        resets: 0,
        bsy_confirmed: 0,
        kv_erase_resets: 0,
        ts_erase_resets: 0,
        page_erased_after: 0,
        page_partial_after: 0,
        next_gen: 1,
        last_addr: 0,
        last_sr: 0,
        last_loops: 0,
    }
}

fn kv_config() -> Config {
    Config::new(SEC_SIZE, PART_SIZE)
}

fn ts_config() -> Config {
    Config::new(SEC_SIZE, PART_SIZE)
}

fn kv_key(i: u32) -> [u8; 3] {
    [b'e', b'0' + ((i / 10) % 10) as u8, b'0' + (i % 10) as u8]
}

fn kv_value(i: u32, generation: u32) -> [u8; KV_VALUE_LEN] {
    let mut v = [0u8; KV_VALUE_LEN];
    v[0] = i as u8;
    v[1] = (i >> 8) as u8;
    v[2] = (i >> 16) as u8;
    v[3] = (i >> 24) as u8;
    v[4] = generation as u8;
    v[5] = (generation >> 8) as u8;
    v[6] = (generation >> 16) as u8;
    v[7] = (generation >> 24) as u8;
    let mut j = 8;
    while j < KV_VALUE_LEN {
        v[j] = (i
            .wrapping_mul(31)
            .wrapping_add(generation.wrapping_mul(17))
            .wrapping_add((j as u32).wrapping_mul(7))
            ^ 0x5A) as u8;
        j += 1;
    }
    v
}

fn ts_payload(time: i32) -> [u8; TS_PAYLOAD_LEN] {
    let t = time as u32;
    let mut v = [0u8; TS_PAYLOAD_LEN];
    v[0] = t as u8;
    v[1] = (t >> 8) as u8;
    v[2] = (t >> 16) as u8;
    v[3] = (t >> 24) as u8;
    let mut j = 4;
    while j < TS_PAYLOAD_LEN {
        v[j] = (t.wrapping_mul(11).wrapping_add((j as u32).wrapping_mul(13)) ^ 0xA5) as u8;
        j += 1;
    }
    v
}

fn hw_of(e: Error<HwError>) -> HwError {
    match e {
        Error::FlashRead(hw) | Error::FlashWrite(hw) | Error::FlashErase(hw) => hw,
        _ => HwError { op: 0, status: 0 },
    }
}

fn db_fail(stage: &[u8], code: u32, e: Error<HwError>) -> ! {
    let hw = hw_of(e);
    write_bytes(b"hw_op=\0");
    write_u32(hw.op as u32);
    write_bytes(b" hw_status=\0");
    write_hex32(hw.status);
    fail(stage, code)
}

fn fail_hw(stage: &[u8], code: u32, e: HwError) -> ! {
    write_bytes(b"hw_op=\0");
    write_u32(e.op as u32);
    write_bytes(b" hw_status=\0");
    write_hex32(e.status);
    fail(stage, code)
}

/// Verify that all 8 keys exist, pass CRC32, and carry a self-consistent
/// `(key_idx, generation)` payload.
fn verify_kv_all(kv: &mut Kvdb<RealFlash>, stage: &[u8]) -> u32 {
    match kv.check() {
        Ok(()) => {}
        Err(e) => db_fail(stage, 0x10, e),
    }
    let mut buf = [0u8; KV_VALUE_LEN];
    let mut k = 0u32;
    while k < KV_KEYS {
        match kv.get(&kv_key(k), &mut buf) {
            Ok(Some(n)) if n == KV_VALUE_LEN => {
                let idx = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let gen = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                if idx != k || gen == 0 || buf != kv_value(k, gen) {
                    fail(stage, 0x100 | k);
                }
            }
            Ok(Some(_)) => fail(stage, 0x200 | k),
            Ok(None) => fail(stage, 0x300 | k),
            Err(e) => db_fail(stage, 0x400 | k, e),
        }
        k += 1;
    }
    KV_KEYS
}

/// Verify the TSDB live window after recovery.
///
/// When a mid-erase reset hits a TSDB rollover page erase at `erased_sec_off`:
/// * If the erase pulse progressed far enough to erase the sector's magic word,
///   `Tsdb::mount` detects `!sec.check_ok` and formats the partition empty
///   (`entries == 0`).
/// * If the erase pulse was aborted within the first microseconds (before the
///   sector header magic word flipped), that one sector is left partially
///   erased with all 4 sector headers still marked `Full`, while the other 3
///   sectors are untouched and must have intact, contiguous entries.
fn verify_ts_window(
    ts: &mut Tsdb<RealFlash>,
    erased_sec_off: Option<u32>,
    stage: &[u8],
) -> (u32, i32, i32) {
    let mut buf = [0u8; TS_PAYLOAD_LEN];
    let mut entries = 0u32;
    let mut first = 0i32;
    let mut last = 0i32;
    let mut it = ts.iter();
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => db_fail(stage, entries, e),
        };
        if let Some(sec_off) = erased_sec_off {
            if entry.index_addr >= sec_off && entry.index_addr < sec_off + SEC_SIZE {
                continue;
            }
        }
        if entry.status != TslStatus::Write {
            continue;
        }
        if entry.time <= 0 || entry.log_len as usize != TS_PAYLOAD_LEN {
            fail(stage, 0x500 | entries);
        }
        if entries > 0 && entry.time != last.wrapping_add(1) {
            fail(stage, 0x600 | entries);
        }
        let n = match it.read_blob(&entry, &mut buf) {
            Ok(n) => n,
            Err(e) => db_fail(stage, 0x700 | entries, e),
        };
        if n != TS_PAYLOAD_LEN || buf != ts_payload(entry.time) {
            fail(stage, 0x800 | entries);
        }
        if entries == 0 {
            first = entry.time;
        }
        last = entry.time;
        entries += 1;
    }
    if erased_sec_off.is_none() && ts.last_time() != last {
        fail(stage, 0x900 | (ts.last_time() as u32));
    }
    (entries, first, last)
}

pub fn run() -> ! {
    install_ecc_nmi_vector();
    board::reset_output();
    board::init_cycles();

    let mut st = state_load();
    let erase_fired =
        unsafe { core::ptr::read_volatile(core::ptr::addr_of!(ERASE_OUT) as *const u32) != 0 };
    let resume = st.magic == EFAULT_MAGIC
        && st.schema == EFAULT_SCHEMA
        && st.armed == 1
        && erase_fired
        && st.round < ROUNDS;
    if !resume {
        st = fresh_state();
        unsafe {
            core::ptr::write_volatile(core::ptr::addr_of_mut!(ECC_NMIS), 0);
        }
    }
    st.boot_count = st.boot_count.wrapping_add(1);
    state_store(st);

    if !resume {
        // Boot 0: format both partitions and seed baseline records so both
        // databases are ready to trigger real sector erases.
        let kv_flash = match RealFlash::new(KV_BASE, PART_SIZE) {
            Ok(f) => f,
            Err(e) => fail_hw(b"init_kv_flash\0", 0, e),
        };
        let mut kv = match Kvdb::mount(kv_flash, kv_config()) {
            Ok(db) => db,
            Err(e) => db_fail(b"init_kv_mount\0", 0, e),
        };
        if let Err(e) = kv.format() {
            db_fail(b"init_kv_format\0", 0, e);
        }
        let mut k = 0u32;
        while k < KV_KEYS {
            let g = st.next_gen;
            st.next_gen = st.next_gen.wrapping_add(1);
            if let Err(e) = kv.set(&kv_key(k), &kv_value(k, g)) {
                db_fail(b"init_kv_set\0", k, e);
            }
            k += 1;
        }

        let ts_flash = match RealFlash::new(TS_BASE, PART_SIZE) {
            Ok(f) => f,
            Err(e) => fail_hw(b"init_ts_flash\0", 0, e),
        };
        let mut ts = match Tsdb::mount(ts_flash, ts_config(), TS_MAX_LEN) {
            Ok(db) => db,
            Err(e) => db_fail(b"init_ts_mount\0", 0, e),
        };
        if let Err(e) = ts.clean() {
            db_fail(b"init_ts_clean\0", 0, e);
        }
        ts.set_rollover(true);
        // Seed 350 entries (99 per 8 KiB sector -> sectors 0..2 full, sector 3
        // more than half full).
        let mut t = 1i32;
        while t <= 350 {
            if let Err(e) = ts.append(t, &ts_payload(t)) {
                db_fail(b"init_ts_append\0", t as u32, e);
            }
            t += 1;
        }
        state_store(st);
    } else {
        // Woke up from the mid-erase reset injected in `st.round`.
        // Immediately disarm the NOLOAD state so any subsequent reset or
        // reflash starts cleanly from Boot 0.
        st.armed = 0;
        state_store(st);
        let (abs, sr_at_reset, loops) = unsafe {
            let p = core::ptr::addr_of_mut!(ERASE_OUT) as *mut u32;
            let a = core::ptr::read_volatile(p);
            let s = core::ptr::read_volatile(p.add(1));
            let l = core::ptr::read_volatile(p.add(2));
            core::ptr::write_volatile(p, 0);
            core::ptr::write_volatile(p.add(1), 0);
            core::ptr::write_volatile(p.add(2), 0);
            (a, s, l)
        };
        if abs < SCRATCH_BASE || abs >= SCRATCH_END || abs % PAGE_SIZE != 0 {
            fail(b"erase_trace_addr\0", abs);
        }
        if (sr_at_reset & NSSR_BSY) == 0 {
            fail(b"erase_not_busy\0", sr_at_reset);
        }
        let expected_loops = DELAY_TABLE[(st.round as usize) % DELAY_TABLE.len()];
        if loops != expected_loops {
            fail(b"erase_finished_before_reset\0", loops);
        }
        st.resets = st.resets.wrapping_add(1);
        st.bsy_confirmed = st.bsy_confirmed.wrapping_add(1);
        if st.round % 2 == 0 {
            st.kv_erase_resets = st.kv_erase_resets.wrapping_add(1);
        } else {
            st.ts_erase_resets = st.ts_erase_resets.wrapping_add(1);
        }
        st.last_addr = abs;
        st.last_sr = sr_at_reset;
        st.last_loops = loops;

        // Inspect whether the interrupted page finished erasing or was aborted
        // with non-0xFF words remaining.
        let probe = match RealFlash::new(SCRATCH_BASE, PART_SIZE * 2) {
            Ok(f) => f,
            Err(e) => fail_hw(b"probe_flash\0", 0, e),
        };
        if probe.raw_is_erased(abs, PAGE_SIZE) {
            st.page_erased_after = st.page_erased_after.wrapping_add(1);
        } else {
            st.page_partial_after = st.page_partial_after.wrapping_add(1);
        }

        // Remount both databases, verify integrity, and perform post-recovery writes.
        let kv_flash = match RealFlash::new(KV_BASE, PART_SIZE) {
            Ok(f) => f,
            Err(e) => fail_hw(b"rec_kv_flash\0", st.round, e),
        };
        let mut kv = match Kvdb::mount(kv_flash, kv_config()) {
            Ok(db) => db,
            Err(e) => db_fail(b"rec_kv_mount\0", st.round, e),
        };
        verify_kv_all(&mut kv, b"rec_kv_verify\0");
        // Post-recovery write to all 8 keys.
        let mut k = 0u32;
        while k < KV_KEYS {
            let g = st.next_gen;
            st.next_gen = st.next_gen.wrapping_add(1);
            if let Err(e) = kv.set(&kv_key(k), &kv_value(k, g)) {
                db_fail(b"rec_kv_set\0", st.round, e);
            }
            k += 1;
        }
        verify_kv_all(&mut kv, b"rec_kv_reverify\0");

        let ts_flash = match RealFlash::new(TS_BASE, PART_SIZE) {
            Ok(f) => f,
            Err(e) => fail_hw(b"rec_ts_flash\0", st.round, e),
        };
        let mut ts = match Tsdb::mount(ts_flash, ts_config(), TS_MAX_LEN) {
            Ok(db) => db,
            Err(e) => db_fail(b"rec_ts_mount\0", st.round, e),
        };
        ts.set_rollover(true);
        let erased_ts_off = if st.round % 2 == 1 && abs >= TS_BASE && abs < TS_BASE + PART_SIZE {
            Some(abs - TS_BASE)
        } else {
            None
        };
        let (entries, _, last) = verify_ts_window(&mut ts, erased_ts_off, b"rec_ts_verify\0");
        if erased_ts_off.is_some() {
            // If the erase was aborted before the sector header magic word
            // flipped, the 3 untouched sectors were verified above and all 4
            // sector headers remain marked Full with the interrupted page
            // partially erased; clean the partition to complete the format
            // before re-seeding for the next rollover round.
            if entries > 0 {
                if let Err(e) = ts.clean() {
                    db_fail(b"rec_ts_clean\0", st.round, e);
                }
            }
            let mut t = 1i32;
            while t <= 350 {
                if let Err(e) = ts.append(t, &ts_payload(t)) {
                    db_fail(b"rec_ts_reseed\0", st.round, e);
                }
                t += 1;
            }
        } else {
            if entries == 0 {
                fail(b"rec_ts_empty\0", st.round);
            }
            let mut a = 0u32;
            let mut next_t = last.wrapping_add(1);
            while a < 4 {
                if let Err(e) = ts.append(next_t, &ts_payload(next_t)) {
                    db_fail(b"rec_ts_append\0", st.round, e);
                }
                next_t = next_t.wrapping_add(1);
                a += 1;
            }
        }
        verify_ts_window(&mut ts, None, b"rec_ts_reverify\0");

        st.round = st.round.wrapping_add(1);
        st.armed = 0;
        state_store(st);
    }

    // If more rounds remain, arm the next mid-erase reset and drive the target
    // database until it erases a sector.
    if st.round < ROUNDS {
        let r = st.round;
        let delay = DELAY_TABLE[(r as usize) % DELAY_TABLE.len()];
        unsafe {
            let p = core::ptr::addr_of_mut!(ERASE_OUT) as *mut u32;
            core::ptr::write_volatile(p, 0);
            core::ptr::write_volatile(p.add(1), 0);
            core::ptr::write_volatile(p.add(2), 0);
        }
        st.armed = 1;
        state_store(st);

        let out_ptr = unsafe { core::ptr::addr_of_mut!(ERASE_OUT) as *mut u32 };
        if r % 2 == 0 {
            // Even round: trigger mid-erase reset during KVDB GC page erase.
            let mut kv_flash = match RealFlash::new(KV_BASE, PART_SIZE) {
                Ok(f) => f,
                Err(e) => fail_hw(b"arm_kv_flash\0", r, e),
            };
            kv_flash.arm_erase_reset(1, delay, out_ptr);
            let mut kv = match Kvdb::mount(kv_flash, kv_config()) {
                Ok(db) => db,
                Err(e) => db_fail(b"arm_kv_mount\0", r, e),
            };
            let mut step = 0u32;
            while step < 256 {
                let k = step % KV_KEYS;
                let g = st.next_gen;
                st.next_gen = st.next_gen.wrapping_add(1);
                state_store(st);
                if let Err(e) = kv.set(&kv_key(k), &kv_value(k, g)) {
                    db_fail(b"arm_kv_set\0", step, e);
                }
                step += 1;
            }
            fail(b"kv_erase_did_not_fire\0", r);
        } else {
            // Odd round: trigger mid-erase reset during TSDB rollover page erase.
            let mut ts_flash = match RealFlash::new(TS_BASE, PART_SIZE) {
                Ok(f) => f,
                Err(e) => fail_hw(b"arm_ts_flash\0", r, e),
            };
            ts_flash.arm_erase_reset(1, delay, out_ptr);
            let mut ts = match Tsdb::mount(ts_flash, ts_config(), TS_MAX_LEN) {
                Ok(db) => db,
                Err(e) => db_fail(b"arm_ts_mount\0", r, e),
            };
            ts.set_rollover(true);
            let mut next_t = ts.last_time().wrapping_add(1);
            let mut step = 0u32;
            while step < 256 {
                if let Err(e) = ts.append(next_t, &ts_payload(next_t)) {
                    db_fail(b"arm_ts_append\0", step, e);
                }
                next_t = next_t.wrapping_add(1);
                step += 1;
            }
            fail(b"ts_erase_did_not_fire\0", r);
        }
    }

    // ---- All `ROUNDS` mid-erase resets completed and verified ----
    let kv_flash = match RealFlash::new(KV_BASE, PART_SIZE) {
        Ok(f) => f,
        Err(e) => fail_hw(b"final_kv_flash\0", 0, e),
    };
    let mut kv = match Kvdb::mount(kv_flash, kv_config()) {
        Ok(db) => db,
        Err(e) => db_fail(b"final_kv_mount\0", 0, e),
    };
    let kv_keys = verify_kv_all(&mut kv, b"final_kv\0");

    let ts_flash = match RealFlash::new(TS_BASE, PART_SIZE) {
        Ok(f) => f,
        Err(e) => fail_hw(b"final_ts_flash\0", 0, e),
    };
    let mut ts = match Tsdb::mount(ts_flash, ts_config(), TS_MAX_LEN) {
        Ok(db) => db,
        Err(e) => db_fail(b"final_ts_mount\0", 0, e),
    };
    ts.set_rollover(true);
    let (ts_entries, ts_first, ts_last) = verify_ts_window(&mut ts, None, b"final_ts\0");
    let stack = stack_high_water_bytes();
    let ecc_nmis = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(ECC_NMIS)) };

    write_bytes(b"RESULT kind=erase_fault rounds=\0");
    write_u32(ROUNDS);
    write_bytes(b" rounds_done=\0");
    write_u32(st.round);
    write_bytes(b" boots=\0");
    write_u32(st.boot_count);
    write_bytes(b" resets=\0");
    write_u32(st.resets);
    write_bytes(b" bsy_confirmed=\0");
    write_u32(st.bsy_confirmed);
    write_bytes(b" kv_erase_resets=\0");
    write_u32(st.kv_erase_resets);
    write_bytes(b" ts_erase_resets=\0");
    write_u32(st.ts_erase_resets);
    write_bytes(b" page_erased_after=\0");
    write_u32(st.page_erased_after);
    write_bytes(b" page_partial_after=\0");
    write_u32(st.page_partial_after);
    write_bytes(b" ecc_nmis=\0");
    write_u32(ecc_nmis);
    write_bytes(b" last_addr=\0");
    write_hex32(st.last_addr);
    write_bytes(b" last_sr=\0");
    write_hex32(st.last_sr);
    write_bytes(b" last_loops=\0");
    write_u32(st.last_loops);
    write_bytes(b" kv_keys=\0");
    write_u32(kv_keys);
    write_bytes(b" ts_entries=\0");
    write_u32(ts_entries);
    write_bytes(b" ts_first=\0");
    write_u32(ts_first as u32);
    write_bytes(b" ts_last=\0");
    write_u32(ts_last as u32);
    write_bytes(b" stack=\0");
    write_u32(stack);
    write_bytes(b" errors=0 verify_ok=1 rc=0\n\0");

    st.magic = 0;
    st.armed = 0;
    state_store(st);

    super::exit()
}
