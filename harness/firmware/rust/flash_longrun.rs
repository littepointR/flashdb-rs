//! On-silicon long-run test (`flash-longrun` feature).
//!
//! Runs the real `flashdb` crate on **internal STM32U5 flash** (not the RAM
//! simulator): a 4 x 8 KiB KVDB partition and a 4 x 8 KiB TSDB partition in the
//! scratch pages, with 128-bit quad-word programming (`WRITE_GRAN_BITS = 128`,
//! the device's programming granularity).
//!
//! The workload alternates KV update rounds and TS append rounds until a
//! ~60 s DWT cycle budget is used, and verifies every value/payload plus the
//! mount/remount state:
//!
//! * KV: 32 keys x 32 B values, rewritten every round; full read-back compare,
//!   remount every 4 rounds and compare again.
//! * TS: 48 appends x 32 B payloads per round with increasing timestamps and
//!   rollover enabled; the whole live window is verified (contiguous
//!   timestamps, exact payload) and remounted every 4 rounds.
//!
//! The controller status is checked by the driver after every program/erase; a
//! hardware error aborts with the raw NSSR value.

use super::hwflash::{HwError, RealFlash, PAGE_SIZE, SCRATCH_BASE};
use super::{board, fail, stack_high_water_bytes, write_bytes, write_hex32, write_u32};
use flashdb::{Config, Error, Kvdb, Tsdb, TslStatus};

/// Time budget for the workload loop (~60 s at the measured core clock).
const BUDGET_SECONDS: u32 = 60;
/// Safety cap on rounds (the time budget normally stops first).
const MAX_ROUNDS: u32 = 100_000;

const SEC_SIZE: u32 = PAGE_SIZE;
const SECTORS: u32 = 4;
const PART_SIZE: u32 = SEC_SIZE * SECTORS;
/// KV partition: scratch pages 24..27.
const KV_BASE: u32 = SCRATCH_BASE;
/// TS partition: scratch pages 28..31.
const TS_BASE: u32 = SCRATCH_BASE + PART_SIZE;

const KV_KEYS: u32 = 32;
const KV_VALUE_LEN: usize = 32;
const TS_PER_ROUND: u32 = 48;
const TS_PAYLOAD_LEN: usize = 32;
const TS_MAX_LEN: u32 = 64;
const REMOUNT_EVERY: u32 = 4;

fn kv_config() -> Config {
    Config::new(SEC_SIZE, PART_SIZE)
}

fn ts_config() -> Config {
    Config::new(SEC_SIZE, PART_SIZE)
}

/// 3-byte key for key index `i` (ASCII digits, no NUL).
fn kv_key(i: u32) -> [u8; 3] {
    [b'k', b'0' + ((i / 10) % 10) as u8, b'0' + (i % 10) as u8]
}

/// Deterministic 32-byte KV value for (key, generation).
fn kv_value(i: u32, generation: u32) -> [u8; KV_VALUE_LEN] {
    let mut v = [0u8; KV_VALUE_LEN];
    let mut j = 0;
    while j < KV_VALUE_LEN {
        v[j] = (i as u8)
            .wrapping_mul(31)
            .wrapping_add((generation as u8).wrapping_mul(17))
            .wrapping_add((j as u8).wrapping_mul(7))
            ^ 0xA5;
        j += 1;
    }
    v
}

/// Deterministic 32-byte TS payload for a timestamp.
fn ts_payload(time: i32) -> [u8; TS_PAYLOAD_LEN] {
    let mut v = [0u8; TS_PAYLOAD_LEN];
    let t = time as u32;
    let mut j = 0;
    while j < TS_PAYLOAD_LEN {
        v[j] = (t as u8)
            .wrapping_mul(13)
            .wrapping_add((j as u8).wrapping_mul(11))
            ^ 0x5A;
        j += 1;
    }
    v
}

fn hw_of(e: Error<HwError>) -> HwError {
    match e {
        Error::FlashRead(hw) | Error::FlashWrite(hw) | Error::FlashErase(hw) => hw,
        // Geometry/config errors do not come from the controller.
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

/// Read back every KV key and compare with the expected generation.
fn kv_verify(kv: &mut Kvdb<RealFlash>, generation: u32) {
    let mut buf = [0u8; KV_VALUE_LEN];
    let mut i = 0;
    while i < KV_KEYS {
        let n = match kv.get(&kv_key(i), &mut buf) {
            Ok(Some(n)) => n,
            Ok(None) => fail(b"kv_missing\0", i),
            Err(e) => db_fail(b"kv_get\0", i, e),
        };
        if n != KV_VALUE_LEN || buf != kv_value(i, generation) {
            fail(b"kv_value\0", i);
        }
        i += 1;
    }
}

/// Walk the live TS window: contiguous timestamps and exact payloads.
fn ts_verify(ts: &mut Tsdb<RealFlash>, last_time: i32, round: u32) -> u32 {
    let mut expected_time: i32 = -1;
    let mut count = 0u32;
    let mut buf = [0u8; TS_PAYLOAD_LEN];
    let mut it = ts.iter();
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => db_fail(b"ts_iter\0", round, e),
        };
        if expected_time >= 0 && entry.time != expected_time + 1 {
            fail(b"ts_gap\0", round);
        }
        if entry.log_len as usize != TS_PAYLOAD_LEN {
            fail(b"ts_len\0", round);
        }
        let n = match it.read_blob(&entry, &mut buf) {
            Ok(n) => n,
            Err(e) => db_fail(b"ts_blob\0", round, e),
        };
        if n != TS_PAYLOAD_LEN || buf != ts_payload(entry.time) {
            fail(b"ts_value\0", round);
        }
        expected_time = entry.time;
        count += 1;
    }
    if count == 0 || expected_time != last_time {
        fail(b"ts_window\0", round);
    }
    count
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

    let kv_flash = RealFlash::new(KV_BASE, PART_SIZE)
        .unwrap_or_else(|e| db_fail(b"kv_flash\0", 0, Error::FlashErase(e)));
    let mut kv =
        Kvdb::mount(kv_flash, kv_config()).unwrap_or_else(|e| db_fail(b"kv_mount\0", 0, e));
    kv.format()
        .unwrap_or_else(|e| db_fail(b"kv_format\0", 0, e));

    let ts_flash = RealFlash::new(TS_BASE, PART_SIZE)
        .unwrap_or_else(|e| db_fail(b"ts_flash\0", 0, Error::FlashErase(e)));
    let mut ts = Tsdb::mount(ts_flash, ts_config(), TS_MAX_LEN)
        .unwrap_or_else(|e| db_fail(b"ts_mount\0", 0, e));
    ts.clean().unwrap_or_else(|e| db_fail(b"ts_clean\0", 0, e));

    // Initial KV fill so every round is an update round.
    let mut i = 0;
    while i < KV_KEYS {
        let v = kv_value(i, 0);
        kv.set(&kv_key(i), &v)
            .unwrap_or_else(|e| db_fail(b"kv_set\0", 0, e));
        i += 1;
    }

    let mut kv_erases = 0u32;
    let mut ts_erases = 0u32;
    let mut kv_remounts = 0u32;
    let mut ts_remounts = 0u32;
    let mut kv_sets = KV_KEYS;
    let mut ts_appends = 0u32;
    let mut ts_time: i32 = 1000;
    let mut rounds = 0u32;
    let start = board::cycles();

    loop {
        if rounds > 0 && board::cycles().wrapping_sub(start) >= budget {
            break;
        }
        if rounds >= MAX_ROUNDS {
            break;
        }
        let generation = rounds + 1;

        // ---- KV round: rewrite every key, verify, remount periodically ----
        let mut i = 0;
        while i < KV_KEYS {
            let v = kv_value(i, generation);
            kv.set(&kv_key(i), &v)
                .unwrap_or_else(|e| db_fail(b"kv_set\0", generation, e));
            kv_sets += 1;
            i += 1;
        }
        kv_verify(&mut kv, generation);
        if generation % REMOUNT_EVERY == 0 {
            let flash = kv.into_flash();
            kv_erases += flash.erases;
            kv = Kvdb::mount(flash, kv_config())
                .unwrap_or_else(|e| db_fail(b"kv_remount\0", generation, e));
            kv_remounts += 1;
            kv_verify(&mut kv, generation);
        }

        // ---- TS round: append, verify the live window, remount periodically ----
        let mut a = 0;
        while a < TS_PER_ROUND {
            let p = ts_payload(ts_time);
            ts.append(ts_time, &p)
                .unwrap_or_else(|e| db_fail(b"ts_append\0", ts_time as u32, e));
            ts_time += 1;
            ts_appends += 1;
            a += 1;
        }
        let live = ts_verify(&mut ts, ts_time - 1, generation);
        if live > PART_SIZE / 16 {
            fail(b"ts_live\0", live);
        }
        if generation % REMOUNT_EVERY == 0 {
            let flash = ts.into_flash();
            ts_erases += flash.erases;
            ts = Tsdb::mount(flash, ts_config(), TS_MAX_LEN)
                .unwrap_or_else(|e| db_fail(b"ts_remount\0", generation, e));
            ts_remounts += 1;
            ts_verify(&mut ts, ts_time - 1, generation);
        }

        rounds += 1;
    }

    let elapsed = board::cycles().wrapping_sub(start);

    // Final remount: both databases must come back with intact data.
    let flash = kv.into_flash();
    kv_erases += flash.erases;
    let mut kv2 =
        Kvdb::mount(flash, kv_config()).unwrap_or_else(|e| db_fail(b"kv_final\0", rounds, e));
    kv_verify(&mut kv2, rounds);
    let flash = kv2.into_flash();
    kv_erases += flash.erases;

    let flash = ts.into_flash();
    ts_erases += flash.erases;
    let mut ts2 = Tsdb::mount(flash, ts_config(), TS_MAX_LEN)
        .unwrap_or_else(|e| db_fail(b"ts_final\0", rounds, e));
    ts_verify(&mut ts2, ts_time - 1, rounds);
    let query_count = match ts2.query_count(0, i32::MAX, TslStatus::Write) {
        Ok(n) => n as u32,
        Err(e) => db_fail(b"ts_qcount\0", rounds, e),
    };
    let flash = ts2.into_flash();
    ts_erases += flash.erases;

    let stack = stack_high_water_bytes();
    let mut checksum = 0x811C_9DC5u32;
    let mut j = 0u32;
    while j < KV_KEYS {
        for &b in &kv_value(j, rounds) {
            checksum ^= b as u32;
            checksum = checksum.wrapping_mul(0x0100_0193);
        }
        j += 1;
    }
    // Checksum the last (up to) 64 TS payloads of the live window.
    let mut t = ts_time - (ts_time - 1000).min(64);
    while t < ts_time {
        for &b in &ts_payload(t) {
            checksum ^= b as u32;
            checksum = checksum.wrapping_mul(0x0100_0193);
        }
        t += 1;
    }

    write_bytes(b"RESULT kind=longrun rounds=\0");
    write_u32(rounds);
    write_bytes(b" kv_sets=\0");
    write_u32(kv_sets);
    write_bytes(b" ts_appends=\0");
    write_u32(ts_appends);
    write_bytes(b" ts_time=\0");
    write_u32(ts_time as u32);
    write_bytes(b" query_count=\0");
    write_u32(query_count);
    write_bytes(b" kv_remounts=\0");
    write_u32(kv_remounts);
    write_bytes(b" ts_remounts=\0");
    write_u32(ts_remounts);
    write_bytes(b" elapsed_cycles=\0");
    write_u32(elapsed);
    write_bytes(b" budget_cycles=\0");
    write_u32(budget);
    write_bytes(b" checksum=\0");
    write_hex32(checksum);
    write_bytes(b" errors=0 verify_ok=1 rc=0\n\0");

    write_bytes(b"BOARD core_hz=\0");
    write_u32(hz);
    write_bytes(b" kv_erases=\0");
    write_u32(kv_erases);
    write_bytes(b" ts_erases=\0");
    write_u32(ts_erases);
    write_bytes(b" stack=\0");
    write_u32(stack);
    write_bytes(b" stack_limit=\0");
    write_u32(stack_limit);
    write_bytes(b"\n\0");

    super::exit()
}
