//! Real-flash power-loss injection: the **verifier** image (`flash-fault-verify`).
//!
//! Runs after `flash-fault-write.rs` was interrupted by an external reset.  It
//! mounts both databases from the interrupted internal-flash state and checks
//! the recovery invariants established by the host power-cut tests on real
//! silicon (see `crates/flashdb/tests/tsdb_recovery.rs` and `kv_smoke.rs`):
//!
//! * **mount must succeed** - this is the failure the test exists to catch;
//! * every committed KV record passes its CRC (`Kvdb::check`), every live value
//!   decodes as one the writer could have produced (key index + generation
//!   self-consistency), point gets and iteration agree;
//! * the TSDB live window is contiguous with self-consistent payloads, torn
//!   records are at most PRE_WRITE entries (never committed), `last_time` is
//!   the newest committed timestamp, and reverse iteration mirrors forward;
//! * both databases **accept new writes after the interruption** and the new
//!   state survives a remount (`rc=0` only then).
//!
//! The RESULT line also carries the writer's post-mortem trace (`trace_*`
//! fields, see `hwflash::HwTrace`), i.e. which flash operation the injected
//! reset interrupted, so the host can tell resets inside program/erase windows
//! from resets between operations.

use super::hwflash::{self, HwError, RealFlash, PAGE_SIZE, SCRATCH_BASE};
use super::{board, fail, stack_high_water_bytes, write_bytes, write_hex32, write_u32};
use flashdb::{Config, Error, Kvdb, Tsdb, TslStatus};

/// Partition geometry, identical in `flash_fault_write.rs`:
/// KVDB = scratch pages 24..27 (4 x 8 KiB), TSDB = scratch pages 28..31.
const SEC_SIZE: u32 = PAGE_SIZE;
const SECTORS: u32 = 4;
const PART_SIZE: u32 = SEC_SIZE * SECTORS;
const KV_BASE: u32 = SCRATCH_BASE;
const TS_BASE: u32 = SCRATCH_BASE + PART_SIZE;

/// Workload shape, a cross-image contract with `flash_fault_write.rs`.
const KV_KEYS: u32 = 16;
const KV_VALUE_LEN: usize = 32;
const TS_PAYLOAD_LEN: usize = 32;
const TS_MAX_LEN: u32 = 64;

fn kv_config() -> Config {
    Config::new(SEC_SIZE, PART_SIZE)
}

fn ts_config() -> Config {
    Config::new(SEC_SIZE, PART_SIZE)
}

fn kv_key(i: u32) -> [u8; 3] {
    [b'f', b'0' + ((i / 10) % 10) as u8, b'0' + (i % 10) as u8]
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
            ^ 0xA5) as u8;
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
        v[j] = (t.wrapping_mul(13).wrapping_add((j as u32).wrapping_mul(11)) ^ 0x5A) as u8;
        j += 1;
    }
    v
}

/// Decode a live KV value: returns `(key index, generation)` when the whole
/// value is exactly `kv_value(index, generation)`, i.e. one this workload could
/// have written.
fn decode_kv(value: &[u8; KV_VALUE_LEN]) -> Option<(u32, u32)> {
    let i = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
    let generation = u32::from_le_bytes([value[4], value[5], value[6], value[7]]);
    if i < KV_KEYS && kv_value(i, generation) == *value {
        Some((i, generation))
    } else {
        None
    }
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

fn hw_fail(stage: &[u8], code: u32, e: HwError) -> ! {
    write_bytes(b"hw_op=\0");
    write_u32(e.op as u32);
    write_bytes(b" hw_status=\0");
    write_hex32(e.status);
    fail(stage, code)
}

/// Result of scanning the TSDB live window.
struct TsScan {
    entries: u32,
    first: i32,
    last: i32,
    prewrite: u32,
}

/// Walk the whole TSDB live window and verify the recovery invariants.
///
/// Committed (`Write`) entries must have contiguous, increasing timestamps and
/// exactly the payload this workload writes for that timestamp.  Torn
/// (`PreWrite`) index records are allowed (a single reset can leave one at the
/// append position; after the database accepts more appends it may sit inside
/// the window), must never be readable, and never count as committed data.
fn ts_scan(ts: &mut Tsdb<RealFlash>, stage: &[u8]) -> TsScan {
    let mut buf = [0u8; TS_PAYLOAD_LEN];
    let mut entries = 0u32;
    let mut first = 0i32;
    let mut last = 0i32;
    let mut prewrite = 0u32;
    let mut it = ts.iter();
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => db_fail(stage, entries, e),
        };
        match entry.status {
            TslStatus::Write => {
                if entry.time <= 0 {
                    fail(stage, 0x1000 | entry.time as u32);
                }
                if entry.log_len as usize != TS_PAYLOAD_LEN {
                    fail(stage, 0x2000 | entry.log_len);
                }
                if entries > 0 && entry.time != last.wrapping_add(1) {
                    fail(stage, 0x3000 | entries);
                }
                let n = match it.read_blob(&entry, &mut buf) {
                    Ok(n) => n,
                    Err(e) => db_fail(stage, 0x4000 | entries, e),
                };
                if n != TS_PAYLOAD_LEN || buf != ts_payload(entry.time) {
                    fail(stage, 0x5000 | entry.time as u32);
                }
                if entries == 0 {
                    first = entry.time;
                }
                last = entry.time;
                entries += 1;
            }
            TslStatus::PreWrite => {
                prewrite += 1;
                if entry.log_len != TS_MAX_LEN || entry.time != 0 {
                    fail(stage, 0x6000 | prewrite);
                }
                let n = match it.read_blob(&entry, &mut buf) {
                    Ok(n) => n,
                    Err(e) => db_fail(stage, 0x7000 | prewrite, e),
                };
                if n != 0 {
                    fail(stage, 0x8000 | prewrite);
                }
            }
            _ => fail(stage, 0x9000 | entry.status as u32),
        }
    }
    TsScan {
        entries,
        first,
        last,
        prewrite,
    }
}

/// Point-get every key and require exactly `generation`'s value for all of
/// them (used after the verifier's own writes and after the remount).
fn kv_expect_all(kv: &mut Kvdb<RealFlash>, generation: u32, stage: &[u8]) {
    let mut i = 0;
    while i < KV_KEYS {
        let mut buf = [0u8; KV_VALUE_LEN];
        match kv.get(&kv_key(i), &mut buf) {
            Ok(Some(n)) => {
                if n != KV_VALUE_LEN || buf != kv_value(i, generation) {
                    fail(stage, 0x100 | i);
                }
            }
            Ok(None) => fail(stage, 0x200 | i),
            Err(e) => db_fail(stage, 0x300 | i, e),
        }
        i += 1;
    }
}

pub fn run() -> ! {
    // 1. Post-mortem trace of the interrupted writer run (fixed SRAM address,
    //    see hwflash.rs).  Clear it immediately: even if this verification
    //    fails, the next writer download must start from the fresh-run path.
    let trace = hwflash::trace_snapshot();
    let trace_valid =
        trace.magic == hwflash::TRACE_MAGIC && trace.magic_inv == !hwflash::TRACE_MAGIC;
    let (trace_seq, trace_op, trace_addr, trace_erases, trace_programs, trace_error) =
        if trace_valid {
            (
                trace.seq,
                trace.op,
                trace.addr,
                trace.erases,
                trace.programs,
                trace.error,
            )
        } else {
            (0, 0, 0, 0, 0, 0)
        };
    hwflash::trace_clear();

    board::reset_output();
    board::init_cycles();
    let hz = match board::core_hz() {
        0 => 4_000_000,
        h => h,
    };
    let stack_limit = super::stack_limit_bytes();

    // 2. Mount both databases from the interrupted state.  A mount error is
    //    the failure this whole test exists to catch.
    let kv_flash = match RealFlash::new(KV_BASE, PART_SIZE) {
        Ok(f) => f,
        Err(e) => hw_fail(b"kv_flash\0", 0, e),
    };
    let mut kv = match Kvdb::mount(kv_flash, kv_config()) {
        Ok(db) => db,
        Err(e) => db_fail(b"kv_mount\0", 0, e),
    };

    // 3. KV recovery invariants: per-record CRC, generation-consistent values,
    //    iterator/point-get/stat agreement.
    match kv.check() {
        Ok(()) => {}
        Err(e) => db_fail(b"kv_check\0", 0, e),
    }
    let mut kv_keys = 0u32;
    let mut kv_max_gen = 0u32;
    let mut mask = 0u32;
    {
        let mut buf = [0u8; KV_VALUE_LEN];
        let mut it = kv.iter();
        while let Some(entry) = it.next() {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => db_fail(b"kv_iter\0", kv_keys, e),
            };
            let name = entry.name();
            if name.len() != 3 || name[0] != b'f' {
                fail(b"kv_key\0", kv_keys);
            }
            let idx = ((name[1].wrapping_sub(b'0')) as u32)
                .wrapping_mul(10)
                .wrapping_add((name[2].wrapping_sub(b'0')) as u32);
            if idx >= KV_KEYS || kv_key(idx) != [name[0], name[1], name[2]] {
                fail(b"kv_key\0", kv_keys);
            }
            if mask & (1 << idx) != 0 {
                fail(b"kv_dup\0", idx);
            }
            let n = match it.read_value(&entry, &mut buf) {
                Ok(n) => n,
                Err(e) => db_fail(b"kv_read\0", idx, e),
            };
            if n != KV_VALUE_LEN {
                fail(b"kv_len\0", idx);
            }
            let (dec_idx, dec_gen) = match decode_kv(&buf) {
                Some(pair) => pair,
                None => fail(b"kv_generation\0", idx),
            };
            if dec_idx != idx {
                fail(b"kv_index\0", idx);
            }
            if dec_gen > kv_max_gen {
                kv_max_gen = dec_gen;
            }
            mask |= 1 << idx;
            kv_keys += 1;
        }
    }
    let stats = match kv.stats() {
        Ok(s) => s,
        Err(e) => db_fail(b"kv_stats\0", kv_keys, e),
    };
    if stats.key_count != kv_keys {
        fail(b"kv_stats_count\0", stats.key_count);
    }
    // Point gets must agree with iteration, and every value must be one this
    // workload could have written (generation self-consistency checked above).
    let mut i = 0;
    while i < KV_KEYS {
        let present = mask & (1 << i) != 0;
        let mut buf = [0u8; KV_VALUE_LEN];
        match kv.get(&kv_key(i), &mut buf) {
            Ok(Some(n)) => {
                if !present || n != KV_VALUE_LEN {
                    fail(b"kv_get\0", i);
                }
                match decode_kv(&buf) {
                    Some((dec_idx, dec_gen)) if dec_idx == i && dec_gen <= kv_max_gen => {}
                    _ => fail(b"kv_get_generation\0", i),
                }
            }
            Ok(None) => {
                if present {
                    fail(b"kv_get_missing\0", i);
                }
            }
            Err(e) => db_fail(b"kv_get\0", i, e),
        }
        i += 1;
    }

    // 4. The interrupted database must accept new writes: write every key with
    //    a generation one past anything recovered, then read it all back.
    let kv_gen = kv_max_gen.wrapping_add(1);
    if kv_gen == 0 {
        fail(b"kv_generation\0", 0xFFFF_FFFF);
    }
    let mut i = 0;
    while i < KV_KEYS {
        match kv.set(&kv_key(i), &kv_value(i, kv_gen)) {
            Ok(()) => {}
            Err(e) => db_fail(b"kv_set\0", i, e),
        }
        i += 1;
    }
    kv_expect_all(&mut kv, kv_gen, b"kv_write\0");

    // 5. TS recovery invariants.
    let ts_flash = match RealFlash::new(TS_BASE, PART_SIZE) {
        Ok(f) => f,
        Err(e) => hw_fail(b"ts_flash\0", 0, e),
    };
    let mut ts = match Tsdb::mount(ts_flash, ts_config(), TS_MAX_LEN) {
        Ok(db) => db,
        Err(e) => db_fail(b"ts_mount\0", 0, e),
    };
    ts.set_rollover(true);
    let scan = ts_scan(&mut ts, b"ts_scan\0");
    if scan.entries == 0 {
        // A damaged sector header makes mount re-format the whole TSDB: the
        // documented recovery action is data loss, not corruption (same as
        // FlashDB-C).  The database must then be cleanly empty.
        if scan.prewrite != 0 {
            fail(b"ts_empty_prewrite\0", scan.prewrite);
        }
        if ts.last_time() != 0 {
            fail(b"ts_empty_last\0", ts.last_time() as u32);
        }
    } else if ts.last_time() != scan.last {
        fail(b"ts_last_time\0", ts.last_time() as u32);
    }
    let ts_query = if scan.prewrite == 0 {
        match ts.query_count(0, i32::MAX, TslStatus::Write) {
            Ok(n) => n as u32,
            Err(e) => db_fail(b"ts_query\0", scan.entries, e),
        }
    } else {
        // A torn record reports time 0, which breaks the by-time assumption
        // (same limitation as C); skip the by-time count in that case.
        scan.entries
    };
    if ts_query != scan.entries {
        fail(b"ts_query_count\0", ts_query);
    }
    // Reverse iteration must mirror forward iteration.
    let (rev_count, rev_first, rev_last) = {
        let mut rev_count = 0u32;
        let mut rev_first = 0i32;
        let mut rev_last = 0i32;
        let mut it = ts.iter_reverse();
        while let Some(entry) = it.next() {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => db_fail(b"ts_reverse\0", rev_count, e),
            };
            if entry.status == TslStatus::Write {
                if rev_count == 0 {
                    rev_first = entry.time;
                }
                rev_last = entry.time;
                rev_count += 1;
            }
        }
        (rev_count, rev_first, rev_last)
    };
    if rev_count != scan.entries {
        fail(b"ts_reverse_count\0", rev_count);
    }
    if scan.entries > 0 && (rev_first != scan.last || rev_last != scan.first) {
        fail(b"ts_reverse_order\0", rev_count);
    }

    // 6. The interrupted TSDB must accept a new append.
    let next_time = ts.last_time().wrapping_add(1);
    if next_time <= 0 {
        fail(b"ts_time\0", next_time as u32);
    }
    let payload = ts_payload(next_time);
    match ts.append(next_time, &payload) {
        Ok(()) => {}
        Err(e) => db_fail(b"ts_append\0", next_time as u32, e),
    }
    if ts.last_time() != next_time {
        fail(b"ts_append_last\0", ts.last_time() as u32);
    }

    // 7. Remount both databases: the recovered + continued state must persist.
    let kv_flash = kv.into_flash();
    let mut kv = match Kvdb::mount(kv_flash, kv_config()) {
        Ok(db) => db,
        Err(e) => db_fail(b"kv_remount\0", 0, e),
    };
    match kv.check() {
        Ok(()) => {}
        Err(e) => db_fail(b"kv_recheck\0", 0, e),
    }
    kv_expect_all(&mut kv, kv_gen, b"kv_remount_value\0");
    let kv_flash = kv.into_flash();
    let kv_erases = kv_flash.erases;
    let kv_programs = kv_flash.programs;

    let ts_flash = ts.into_flash();
    let mut ts = match Tsdb::mount(ts_flash, ts_config(), TS_MAX_LEN) {
        Ok(db) => db,
        Err(e) => db_fail(b"ts_remount\0", 0, e),
    };
    ts.set_rollover(true);
    let scan2 = ts_scan(&mut ts, b"ts_remount_scan\0");
    if scan2.entries == 0 || scan2.last != next_time {
        fail(b"ts_remount_window\0", scan2.entries);
    }
    if ts.last_time() != next_time {
        fail(b"ts_remount_last\0", ts.last_time() as u32);
    }
    let ts_flash = ts.into_flash();
    let ts_erases = ts_flash.erases;
    let ts_programs = ts_flash.programs;

    let stack = stack_high_water_bytes();

    write_bytes(b"RESULT kind=fault_verify trace_valid=\0");
    write_u32(trace_valid as u32);
    write_bytes(b" trace_seq=\0");
    write_u32(trace_seq);
    write_bytes(b" trace_op=\0");
    write_u32(trace_op);
    write_bytes(b" trace_addr=\0");
    write_hex32(trace_addr);
    write_bytes(b" trace_erases=\0");
    write_u32(trace_erases);
    write_bytes(b" trace_programs=\0");
    write_u32(trace_programs);
    write_bytes(b" trace_error=\0");
    write_hex32(trace_error);
    write_bytes(b" kv_keys=\0");
    write_u32(kv_keys);
    write_bytes(b" kv_writes=\0");
    write_u32(KV_KEYS);
    write_bytes(b" kv_gen=\0");
    write_u32(kv_gen);
    write_bytes(b" kv_erases=\0");
    write_u32(kv_erases);
    write_bytes(b" kv_programs=\0");
    write_u32(kv_programs);
    write_bytes(b" ts_entries=\0");
    write_u32(scan.entries);
    write_bytes(b" ts_first=\0");
    write_u32(scan.first as u32);
    write_bytes(b" ts_last=\0");
    write_u32(scan.last as u32);
    write_bytes(b" ts_prewrite=\0");
    write_u32(scan.prewrite);
    write_bytes(b" ts_appends=\0");
    write_u32(1);
    write_bytes(b" ts_entries_after=\0");
    write_u32(scan2.entries);
    write_bytes(b" ts_erases=\0");
    write_u32(ts_erases);
    write_bytes(b" ts_programs=\0");
    write_u32(ts_programs);
    write_bytes(b" remount_ok=1 errors=0 verify_ok=1 rc=0\n\0");

    write_bytes(b"BOARD core_hz=\0");
    write_u32(hz);
    write_bytes(b" stack=\0");
    write_u32(stack);
    write_bytes(b" stack_limit=\0");
    write_u32(stack_limit);
    write_bytes(b"\n\0");

    super::exit()
}
