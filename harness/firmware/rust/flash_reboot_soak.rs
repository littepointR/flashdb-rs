//! On-silicon reboot soak (`flash-reboot-soak` feature).
//!
//! FlashDB's power-cut recovery is covered per flash operation on the host
//! simulator; this image adds the complementary *real-silicon reboot* angle:
//! the MCU resets itself (`NVIC_SystemReset`, i.e. a warm reset that preserves
//! SRAM but restarts the firmware from flash) between write batches, so every
//! boot mounts both databases from the internal flash image left by the
//! previous boot and verifies the previous batch before writing the next one.
//!
//! Round bookkeeping lives in a `NOLOAD` `.results`-section static: board.ld
//! neither loads nor zeroes that section and the startup code does not touch
//! it, so it survives the warm reset (the same mechanism `BOARD_OUT` relies on
//! for the post-run read-back).  A schema word invalidates the state when the
//! workload constants or the database layout change, and the state is cleared
//! after the final verification so a new run always starts from round 0.
//!
//! Workload (cross-checked after every reboot and exhaustively at the end):
//! * KVDB: 8 keys x 32 B, rewritten `KV_SETS_PER_ROUND` times per round with a
//!   deterministic per-write generation (the last write of each key in a round
//!   is known, so every boot can check the previous round exactly);
//! * TSDB: `TS_PER_ROUND` appends per round with timestamps 1, 2, 3, ... and
//!   rollover enabled, so the ring wraps twice over the 20 rounds and exercises
//!   real sector sealing/reuse across reboots.
//!
//! The final boot remounts both databases, verifies the complete expected
//! state (all key generations, contiguous TS window ending at the last append,
//! exact payloads, CRC check) and prints `RESULT ... rc=0` + `DONE`.

use super::hwflash::{HwError, RealFlash, PAGE_SIZE, SCRATCH_BASE};
use super::{board, fail, stack_high_water_bytes, write_bytes, write_hex32, write_u32};
use flashdb::{Config, Error, Kvdb, Tsdb, TslStatus};

/// Partition geometry: KVDB = scratch pages 24..27, TSDB = pages 28..31.
const SEC_SIZE: u32 = PAGE_SIZE;
const SECTORS: u32 = 4;
const PART_SIZE: u32 = SEC_SIZE * SECTORS;
const KV_BASE: u32 = SCRATCH_BASE;
const TS_BASE: u32 = SCRATCH_BASE + PART_SIZE;

/// Number of self-reset rounds (each round is one boot/write/reset cycle; the
/// last boot runs the final verification instead of resetting).
const ROUNDS: u32 = 20;
const KV_KEYS: u32 = 8;
const KV_VALUE_LEN: usize = 32;
const KV_SETS_PER_ROUND: u32 = 64;
const KV_LAST_N_PER_KEY: u32 = KV_SETS_PER_ROUND - KV_KEYS;
const TS_PER_ROUND: u32 = 20;
const TS_PAYLOAD_LEN: usize = 32;
const TS_MAX_LEN: u32 = 64;

/// Validity of the NOLOAD round counter; changing the workload constants
/// changes the schema word, so stale state from another build is ignored.
const SOAK_MAGIC: u32 = 0x534F_414B; // "SOAK"
const SOAK_SCHEMA: u32 =
    0x0200_0000 | (ROUNDS << 16) | (KV_SETS_PER_ROUND << 8) | (TS_PER_ROUND & 0xFF);

/// Round bookkeeping that survives `NVIC_SystemReset` (`.results` is NOLOAD:
/// the linker neither loads nor zeroes it, and the startup code does not touch
/// it - see `board.ld` and `common/startup_armv7m.S`).
#[repr(C)]
#[derive(Clone, Copy)]
struct SoakState {
    magic: u32,
    schema: u32,
    /// Batches completed so far; also the round this boot has to run.
    rounds_done: u32,
    /// `1` while a batch is being written.  A boot that finds the flag set
    /// knows a previous attempt was killed mid-batch (host interrupted, no
    /// reset) and restarts from round 0 instead of continuing from a state the
    /// flash does not match.
    in_batch: u32,
    boot_count: u32,
    kv_sets: u32,
    ts_appends: u32,
    /// Cumulative real-flash controller operations over the whole run.
    kv_erases: u32,
    kv_programs: u32,
    ts_erases: u32,
    ts_programs: u32,
}

/// Fresh round-counter state (start of a run; the databases are re-formatted).
fn fresh_state() -> SoakState {
    SoakState {
        magic: SOAK_MAGIC,
        schema: SOAK_SCHEMA,
        rounds_done: 0,
        in_batch: 0,
        boot_count: 0,
        kv_sets: 0,
        ts_appends: 0,
        kv_erases: 0,
        kv_programs: 0,
        ts_erases: 0,
        ts_programs: 0,
    }
}

#[link_section = ".results"]
static mut SOAK_STATE: SoakState = SoakState {
    magic: 0,
    schema: 0,
    rounds_done: 0,
    in_batch: 0,
    boot_count: 0,
    kv_sets: 0,
    ts_appends: 0,
    kv_erases: 0,
    kv_programs: 0,
    ts_erases: 0,
    ts_programs: 0,
};

/// `SoakState` is eleven `u32`s in declaration order; access each word with a
/// volatile load/store so the compiler cannot cache the state across the reset
/// and the stores are ordered before the reset request.
fn state_load() -> SoakState {
    unsafe {
        let p = core::ptr::addr_of!(SOAK_STATE) as *const u32;
        SoakState {
            magic: core::ptr::read_volatile(p),
            schema: core::ptr::read_volatile(p.add(1)),
            rounds_done: core::ptr::read_volatile(p.add(2)),
            in_batch: core::ptr::read_volatile(p.add(3)),
            boot_count: core::ptr::read_volatile(p.add(4)),
            kv_sets: core::ptr::read_volatile(p.add(5)),
            ts_appends: core::ptr::read_volatile(p.add(6)),
            kv_erases: core::ptr::read_volatile(p.add(7)),
            kv_programs: core::ptr::read_volatile(p.add(8)),
            ts_erases: core::ptr::read_volatile(p.add(9)),
            ts_programs: core::ptr::read_volatile(p.add(10)),
        }
    }
}

fn state_store(s: SoakState) {
    unsafe {
        let p = core::ptr::addr_of_mut!(SOAK_STATE) as *mut u32;
        core::ptr::write_volatile(p, s.magic);
        core::ptr::write_volatile(p.add(1), s.schema);
        core::ptr::write_volatile(p.add(2), s.rounds_done);
        core::ptr::write_volatile(p.add(3), s.in_batch);
        core::ptr::write_volatile(p.add(4), s.boot_count);
        core::ptr::write_volatile(p.add(5), s.kv_sets);
        core::ptr::write_volatile(p.add(6), s.ts_appends);
        core::ptr::write_volatile(p.add(7), s.kv_erases);
        core::ptr::write_volatile(p.add(8), s.kv_programs);
        core::ptr::write_volatile(p.add(9), s.ts_erases);
        core::ptr::write_volatile(p.add(10), s.ts_programs);
    }
}

/// Cortex-M system reset request (`NVIC_SystemReset`: AIRCR SYSRESETREQ with
/// the VECTKEY unlock; SRAM is preserved, the core restarts from the vector
/// table in flash).
fn system_reset() -> ! {
    const AIRCR: *mut u32 = 0xE000_ED0C as *mut u32;
    unsafe {
        core::arch::asm!("dsb", options(nomem, nostack, preserves_flags));
        core::ptr::write_volatile(AIRCR, 0x05FA_0004);
        core::arch::asm!("dsb", options(nomem, nostack, preserves_flags));
        core::arch::asm!("isb", options(nomem, nostack, preserves_flags));
    }
    loop {
        core::hint::spin_loop();
    }
}

fn kv_config() -> Config {
    Config::new(SEC_SIZE, PART_SIZE)
}

fn ts_config() -> Config {
    Config::new(SEC_SIZE, PART_SIZE)
}

fn kv_key(i: u32) -> [u8; 3] {
    [b's', b'0' + ((i / 10) % 10) as u8, b'0' + (i % 10) as u8]
}

/// Deterministic 32-byte value for (key index, generation); the first eight
/// bytes carry index and generation so an on-flash value can be checked
/// without knowing the write history.
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
            .wrapping_mul(29)
            .wrapping_add(generation.wrapping_mul(19))
            .wrapping_add((j as u32).wrapping_mul(5))
            ^ 0xC3) as u8;
        j += 1;
    }
    v
}

/// Generation of the `n`-th KV write of round `r`.
fn kv_gen(round: u32, n: u32) -> u32 {
    round.wrapping_mul(KV_SETS_PER_ROUND).wrapping_add(n)
}

/// Deterministic 32-byte TS payload for a timestamp.
fn ts_payload(time: i32) -> [u8; TS_PAYLOAD_LEN] {
    let t = time as u32;
    let mut v = [0u8; TS_PAYLOAD_LEN];
    v[0] = t as u8;
    v[1] = (t >> 8) as u8;
    v[2] = (t >> 16) as u8;
    v[3] = (t >> 24) as u8;
    let mut j = 4;
    while j < TS_PAYLOAD_LEN {
        v[j] = (t.wrapping_mul(7).wrapping_add((j as u32).wrapping_mul(13)) ^ 0x96) as u8;
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

/// Check that key `i` holds exactly `expected` (point get).
fn check_key(kv: &mut Kvdb<RealFlash>, i: u32, expected: &[u8; KV_VALUE_LEN], stage: &[u8]) {
    let mut buf = [0u8; KV_VALUE_LEN];
    match kv.get(&kv_key(i), &mut buf) {
        Ok(Some(n)) if n == KV_VALUE_LEN && &buf == expected => {}
        Ok(Some(n)) => fail(stage, 0x100 | ((i & 0xFF) << 8) | (n as u32 & 0xFF)),
        Ok(None) => fail(stage, 0x200 | i),
        Err(e) => db_fail(stage, 0x300 | i, e),
    }
}

fn mount_kv() -> Kvdb<RealFlash> {
    let flash = match RealFlash::new(KV_BASE, PART_SIZE) {
        Ok(f) => f,
        Err(e) => fail_hw(b"kv_flash\0", 0, e),
    };
    match Kvdb::mount(flash, kv_config()) {
        Ok(db) => db,
        Err(e) => db_fail(b"kv_mount\0", 0, e),
    }
}

fn mount_ts() -> Tsdb<RealFlash> {
    let flash = match RealFlash::new(TS_BASE, PART_SIZE) {
        Ok(f) => f,
        Err(e) => fail_hw(b"ts_flash\0", 0, e),
    };
    match Tsdb::mount(flash, ts_config(), TS_MAX_LEN) {
        Ok(db) => db,
        Err(e) => db_fail(b"ts_mount\0", 0, e),
    }
}

/// Read-only scan of the whole TS window: committed entries must be
/// contiguous and carry exactly `ts_payload(time)`; no torn records are
/// expected for a clean self-reset between completed batches.
fn ts_scan(ts: &mut Tsdb<RealFlash>) -> (u32, i32, i32) {
    let mut buf = [0u8; TS_PAYLOAD_LEN];
    let mut entries = 0u32;
    let mut first = 0i32;
    let mut last = 0i32;
    let mut it = ts.iter();
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => db_fail(b"ts_iter\0", entries, e),
        };
        if entry.status != TslStatus::Write {
            fail(b"ts_status\0", 0x400 | entry.status as u32);
        }
        if entry.time <= 0 {
            fail(b"ts_time\0", 0x500 | entry.time as u32);
        }
        if entry.log_len as usize != TS_PAYLOAD_LEN {
            fail(b"ts_len\0", 0x600 | entry.log_len);
        }
        if entries > 0 && entry.time != last.wrapping_add(1) {
            fail(b"ts_gap\0", 0x700 | entries);
        }
        let n = match it.read_blob(&entry, &mut buf) {
            Ok(n) => n,
            Err(e) => db_fail(b"ts_blob\0", entries, e),
        };
        if n != TS_PAYLOAD_LEN || buf != ts_payload(entry.time) {
            fail(b"ts_value\0", 0x800 | entry.time as u32);
        }
        if entries == 0 {
            first = entry.time;
        }
        last = entry.time;
        entries += 1;
    }
    (entries, first, last)
}

/// Verify the batch written in `round` (used by the next boot before writing
/// on): every key must hold the last value that round wrote and the TS ring's
/// last timestamp must be the last append of that round.
fn verify_round(kv: &mut Kvdb<RealFlash>, ts: &mut Tsdb<RealFlash>, round: u32) {
    let mut k = 0;
    while k < KV_KEYS {
        check_key(
            kv,
            k,
            &kv_value(k, kv_gen(round, KV_LAST_N_PER_KEY + k)),
            b"resume_kv\0",
        );
        k += 1;
    }
    let expected_last = (round.wrapping_add(1).wrapping_mul(TS_PER_ROUND)) as i32;
    if ts.last_time() != expected_last {
        fail(b"resume_ts\0", ts.last_time() as u32);
    }
}

/// Whether the mounted KV database cannot be this soak's data (any live key
/// outside the `s00`..`s07` set, or no live keys at all).
///
/// Used only to recognise a stale round counter: another board test may have
/// rewritten the scratch pages while a previous run's counter survived in SRAM
/// (a warm reset does not clear it).  A database that *does* look like this
/// soak's but holds wrong values is a real persistence failure and is reported
/// by [`verify_round`], never silently re-formatted.
fn kv_foreign_or_empty(kv: &mut Kvdb<RealFlash>) -> bool {
    let mut count = 0u32;
    let mut it = kv.iter();
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => return false,
        };
        let name = entry.name();
        if name.len() != 3 || name[0] != b's' {
            return true;
        }
        let idx = ((name[1].wrapping_sub(b'0')) as u32)
            .wrapping_mul(10)
            .wrapping_add((name[2].wrapping_sub(b'0')) as u32);
        if idx >= KV_KEYS || kv_key(idx) != [name[0], name[1], name[2]] {
            return true;
        }
        count += 1;
    }
    count == 0
}

pub fn run() -> ! {
    board::reset_output();
    board::init_cycles();

    let mut st = state_load();
    let mut resume = st.magic == SOAK_MAGIC
        && st.schema == SOAK_SCHEMA
        && st.in_batch == 0
        && st.rounds_done <= ROUNDS;

    // Mount first, then validate a resumable counter against the databases:
    // this also covers the first boot of a run, where the partitions may hold
    // another board test's data (the round-0 path formats/reformats them).
    let mut kv = mount_kv();
    let mut ts = mount_ts();
    ts.set_rollover(true);

    let mut round = if resume { st.rounds_done } else { 0 };
    if resume && round > 0 && kv_foreign_or_empty(&mut kv) {
        write_bytes(b"SOAK stale round counter, restarting from round 0\n\0");
        resume = false;
        round = 0;
    }
    if !resume {
        st = fresh_state();
    }
    // Persist the boot count before touching flash: a reset in the middle of
    // this boot must not lose it (the count proves how many resets happened).
    st.boot_count = st.boot_count.wrapping_add(1);
    st.rounds_done = round;
    st.in_batch = 0;
    state_store(st);

    write_bytes(b"SOAK boot round=\0");
    write_u32(round);
    write_bytes(b" of \0");
    write_u32(ROUNDS);
    write_bytes(b"\n\0");

    if round == 0 {
        // Fresh run: start from a known-empty state regardless of what a
        // previous board test left in the scratch pages.
        match kv.format() {
            Ok(()) => {}
            Err(e) => db_fail(b"kv_format\0", 0, e),
        }
        match ts.clean() {
            Ok(()) => {}
            Err(e) => db_fail(b"ts_clean\0", 0, e),
        }
    } else {
        // Every reboot must find the previous boot's data intact before it
        // writes the next batch.
        verify_round(&mut kv, &mut ts, round - 1);
        match kv.check() {
            Ok(()) => {}
            Err(e) => db_fail(b"resume_kv_check\0", round, e),
        }
    }

    // If the counter already says "all rounds done" (resumed after a host
    // interruption during the final verification), skip straight to it.
    if round < ROUNDS {
        // Mark the batch in progress: a boot that finds this flag set (the host
        // killed the run mid-batch, so no self-reset happened) restarts from
        // round 0 instead of continuing from a state the flash does not match.
        st.in_batch = 1;
        state_store(st);

        // Deterministic batch for this round: KV ring rewrites (GC/erase churn)
        // followed by TS appends (sector sealing/ring wrap churn).
        let mut n = 0;
        while n < KV_SETS_PER_ROUND {
            let key = n % KV_KEYS;
            match kv.set(&kv_key(key), &kv_value(key, kv_gen(round, n))) {
                Ok(()) => {}
                Err(e) => db_fail(b"kv_set\0", n, e),
            }
            n += 1;
        }
        let mut a = 0;
        while a < TS_PER_ROUND {
            let time = (round
                .wrapping_mul(TS_PER_ROUND)
                .wrapping_add(a)
                .wrapping_add(1)) as i32;
            let payload = ts_payload(time);
            match ts.append(time, &payload) {
                Ok(()) => {}
                Err(e) => db_fail(b"ts_append\0", time as u32, e),
            }
            a += 1;
        }

        // Publish progress, then reset (or run the final verification).
        st.rounds_done = round + 1;
        st.in_batch = 0;
        st.kv_sets = st.kv_sets.wrapping_add(KV_SETS_PER_ROUND);
        st.ts_appends = st.ts_appends.wrapping_add(TS_PER_ROUND);
        state_store(st);

        if round + 1 < ROUNDS {
            // The controller operation counters live in the RealFlash driver
            // instance, which is re-created on the next boot: fold them into the
            // persisted state before the self-reset.
            let flash = kv.into_flash();
            st.kv_erases = st.kv_erases.wrapping_add(flash.erases);
            st.kv_programs = st.kv_programs.wrapping_add(flash.programs);
            let flash = ts.into_flash();
            st.ts_erases = st.ts_erases.wrapping_add(flash.erases);
            st.ts_programs = st.ts_programs.wrapping_add(flash.programs);
            state_store(st);
            system_reset();
        }
    }

    // ---- Final boot: verify the complete expected state from flash ----
    let kv_flash = kv.into_flash();
    let kv_first_erases = kv_flash.erases;
    let kv_first_programs = kv_flash.programs;
    let mut kv = match Kvdb::mount(kv_flash, kv_config()) {
        Ok(db) => db,
        Err(e) => db_fail(b"final_kv_mount\0", 0, e),
    };
    match kv.check() {
        Ok(()) => {}
        Err(e) => db_fail(b"final_kv_check\0", 0, e),
    }
    let mut k = 0;
    while k < KV_KEYS {
        check_key(
            &mut kv,
            k,
            &kv_value(k, kv_gen(ROUNDS - 1, KV_LAST_N_PER_KEY + k)),
            b"final_kv\0",
        );
        k += 1;
    }
    let mut iterated = 0u32;
    {
        let mut it = kv.iter();
        while let Some(entry) = it.next() {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => db_fail(b"final_kv_iter\0", iterated, e),
            };
            if entry.value_len as usize != KV_VALUE_LEN {
                fail(b"final_kv_len\0", iterated);
            }
            iterated += 1;
        }
    }
    if iterated != KV_KEYS {
        fail(b"final_kv_count\0", iterated);
    }
    let kv_flash = kv.into_flash();
    let kv_erases = st
        .kv_erases
        .wrapping_add(kv_first_erases)
        .wrapping_add(kv_flash.erases);
    let kv_programs = st
        .kv_programs
        .wrapping_add(kv_first_programs)
        .wrapping_add(kv_flash.programs);

    let ts_flash = ts.into_flash();
    let ts_first_erases = ts_flash.erases;
    let ts_first_programs = ts_flash.programs;
    let mut ts = match Tsdb::mount(ts_flash, ts_config(), TS_MAX_LEN) {
        Ok(db) => db,
        Err(e) => db_fail(b"final_ts_mount\0", 0, e),
    };
    ts.set_rollover(true);
    let (ts_entries, ts_first, ts_last) = ts_scan(&mut ts);
    let expected_last = (ROUNDS.wrapping_mul(TS_PER_ROUND)) as i32;
    if ts_entries == 0 || ts_last != expected_last {
        fail(b"final_ts_window\0", ts_entries);
    }
    if ts_first != ts_last - (ts_entries as i32 - 1) {
        fail(b"final_ts_first\0", ts_first as u32);
    }
    if ts.last_time() != expected_last {
        fail(b"final_ts_last\0", ts.last_time() as u32);
    }
    let qc = match ts.query_count(0, i32::MAX, TslStatus::Write) {
        Ok(n) => n as u32,
        Err(e) => db_fail(b"final_ts_query\0", ts_entries, e),
    };
    if qc != ts_entries {
        fail(b"final_ts_query_count\0", qc);
    }

    // FNV-1a over the final logical state: 8 KV values + the whole TS live
    // window.  Deterministic for a given workload/geometry, so the host can
    // compare it across runs.
    let mut checksum = 0x811C_9DC5u32;
    let mut k = 0;
    while k < KV_KEYS {
        for &b in &kv_value(k, kv_gen(ROUNDS - 1, KV_LAST_N_PER_KEY + k)) {
            checksum ^= b as u32;
            checksum = checksum.wrapping_mul(0x0100_0193);
        }
        k += 1;
    }
    let mut it = ts.iter();
    let mut buf = [0u8; TS_PAYLOAD_LEN];
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => db_fail(b"final_ts_iter\0", ts_entries, e),
        };
        let n = match it.read_blob(&entry, &mut buf) {
            Ok(n) => n,
            Err(e) => db_fail(b"final_ts_blob\0", ts_entries, e),
        };
        if n != TS_PAYLOAD_LEN {
            fail(b"final_ts_len\0", n as u32);
        }
        let mut j = 0;
        while j < TS_PAYLOAD_LEN {
            checksum ^= buf[j] as u32;
            checksum = checksum.wrapping_mul(0x0100_0193);
            j += 1;
        }
    }
    let ts_flash = ts.into_flash();
    let ts_erases = st
        .ts_erases
        .wrapping_add(ts_first_erases)
        .wrapping_add(ts_flash.erases);
    let ts_programs = st
        .ts_programs
        .wrapping_add(ts_first_programs)
        .wrapping_add(ts_flash.programs);

    let stack = stack_high_water_bytes();

    // RESULT first, then invalidate the persisted state so the next run starts
    // from round 0 (the output buffer is untouched by that).
    write_bytes(b"RESULT kind=reboot_soak rounds=\0");
    write_u32(ROUNDS);
    write_bytes(b" rounds_done=\0");
    write_u32(st.rounds_done);
    write_bytes(b" boots=\0");
    write_u32(st.boot_count);
    write_bytes(b" resets=\0");
    write_u32(st.rounds_done.saturating_sub(1));
    write_bytes(b" kv_sets=\0");
    write_u32(st.kv_sets);
    write_bytes(b" ts_appends=\0");
    write_u32(st.ts_appends);
    write_bytes(b" kv_keys=\0");
    write_u32(iterated);
    write_bytes(b" ts_entries=\0");
    write_u32(ts_entries);
    write_bytes(b" ts_first=\0");
    write_u32(ts_first as u32);
    write_bytes(b" ts_last=\0");
    write_u32(ts_last as u32);
    write_bytes(b" checksum=\0");
    write_hex32(checksum);
    write_bytes(b" errors=0 verify_ok=1 rc=0\n\0");

    write_bytes(b"BOARD kv_erases=\0");
    write_u32(kv_erases);
    write_bytes(b" kv_programs=\0");
    write_u32(kv_programs);
    write_bytes(b" ts_erases=\0");
    write_u32(ts_erases);
    write_bytes(b" ts_programs=\0");
    write_u32(ts_programs);
    write_bytes(b" stack=\0");
    write_u32(stack);
    write_bytes(b" stack_limit=\0");
    write_u32(super::stack_limit_bytes());
    write_bytes(b"\n\0");

    st.magic = 0;
    state_store(st);

    super::exit()
}
