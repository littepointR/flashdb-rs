//! Real-flash power-loss injection: the **writer** image (`flash-fault-write`).
//!
//! Continuously churns a KVDB (16 keys x 32 B values) and a TSDB (32 B
//! payloads, rollover on) that live on the STM32U5 internal flash (scratch
//! pages 24..31, quad-word programming, see `hwflash.rs`).  The mix of KV
//! rewrites and TS appends drives both 128-bit programs and 8 KiB page erases
//! of the real controller.
//!
//! The image never prints `DONE` and never exits: the host runner
//! (`run_flash_fault.ps1`) flashes it, lets it run for a randomized 20..250 ms
//! and then resets the MCU with probe-rs, so the reset lands inside real
//! program and erase windows.  The verifier image (`flash_fault_verify.rs`)
//! then mounts the interrupted databases and checks the recovery invariants.
//!
//! Post-mortem handshake: every flash operation is published to the fixed
//! SRAM trace block in `hwflash` (`trace_begin_op` / `trace_end_op`), which
//! survives the warm reset.  When the injected reset restarts this image, the
//! trace is still valid, so the image halts *before touching any flash memory*
//! and leaves it (and `BOARD_OUT`) untouched for the verifier to read.  The
//! verifier clears the trace after reading it, so only a fresh flash download
//! starts the workload - the reset the runner injects never does.
//!
//! Foreign-data cleanup: the runner may start from scratch pages left by
//! another board test (long-run DB, parity image, an earlier interrupted
//! session).  A fresh writer boot keeps the databases only when they already
//! contain this workload's data (all live values are generation-consistent);
//! otherwise it formats/reformats them once, so the first injected reset always
//! hits the workload and not someone else's bytes.

use super::hwflash::{self, HwError, RealFlash, PAGE_SIZE, SCRATCH_BASE};
use super::{board, write_bytes, write_u32};
use flashdb::{Config, Error, Kvdb, Tsdb, TslStatus};

/// Partition geometry, identical in `flash_fault_verify.rs`:
/// KVDB = scratch pages 24..27 (4 x 8 KiB), TSDB = scratch pages 28..31.
const SEC_SIZE: u32 = PAGE_SIZE;
const SECTORS: u32 = 4;
const PART_SIZE: u32 = SEC_SIZE * SECTORS;
const KV_BASE: u32 = SCRATCH_BASE;
const TS_BASE: u32 = SCRATCH_BASE + PART_SIZE;

/// Workload shape, a cross-image contract with `flash_fault_verify.rs`.
const KV_KEYS: u32 = 16;
const KV_VALUE_LEN: usize = 32;
const TS_PAYLOAD_LEN: usize = 32;
const TS_MAX_LEN: u32 = 64;
/// TS appends after every full KV rewrite round (keeps both DBs writing).
const TS_PER_KV_ROUND: u32 = 8;

/// Writer failure stages published in `HwTrace::error` (high 16 bits); a packed
/// driver `op`/status pair is stored in the low 16 bits when there is one.
const ERR_KV_FLASH: u32 = 1;
const ERR_KV_MOUNT: u32 = 2;
const ERR_KV_FORMAT: u32 = 3;
const ERR_TS_FLASH: u32 = 4;
const ERR_TS_MOUNT: u32 = 5;
const ERR_TS_CLEAN: u32 = 6;
const ERR_KV_SET: u32 = 7;
const ERR_TS_APPEND: u32 = 8;

fn kv_config() -> Config {
    Config::new(SEC_SIZE, PART_SIZE)
}

fn ts_config() -> Config {
    Config::new(SEC_SIZE, PART_SIZE)
}

/// Three-byte key for key index `i` (`f00`..`f15`).
fn kv_key(i: u32) -> [u8; 3] {
    [b'f', b'0' + ((i / 10) % 10) as u8, b'0' + (i % 10) as u8]
}

/// Deterministic 32-byte value for (key index, generation).
///
/// The first eight bytes carry the key index and generation little-endian so
/// the verifier can decode them and re-derive the rest of the value: a
/// recovered value is "generation consistent" when it equals
/// `kv_value(decoded_index, decoded_generation)` for its own key.  That check
/// does not need to know how many updates completed before the reset.
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

/// Deterministic 32-byte TS payload for a timestamp (time embedded little
/// endian in the first four bytes, then a pattern derived from it).
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

/// Extract the driver error of a database error (zero for layout/config errors,
/// which cannot come from the controller).
fn hw_of(e: Error<HwError>) -> HwError {
    match e {
        Error::FlashRead(hw) | Error::FlashWrite(hw) | Error::FlashErase(hw) => hw,
        _ => HwError { op: 0, status: 0 },
    }
}

/// Publish the failure stage (plus a packed driver error if there is one) in
/// the trace and stop in a spin loop: the writer must never print `DONE` (the
/// verifier image is the one that reports results).  The verifier's RESULT
/// line carries `trace_error`, so a writer-side failure is still visible to the
/// host.
#[cold]
#[inline(never)]
fn writer_fail(stage: u32, e: Option<Error<HwError>>) -> ! {
    let mut packed = 0u32;
    if let Some(e) = e {
        let hw = hw_of(e);
        packed = ((hw.op as u32) << 8) | (hw.status & 0xFF);
    }
    hwflash::trace_note_error(stage, packed);
    write_bytes(b"FAULT_WRITE ERROR stage=\0");
    write_u32(stage);
    write_bytes(b"\n\0");
    loop {
        core::hint::spin_loop();
    }
}

/// Halt path for the warm restart caused by the injected reset: keep the trace
/// and the output buffer of the interrupted run intact for the verifier image.
#[cold]
#[inline(never)]
fn warm_restart_halt() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// Whether a live KV value is one this workload could have written: `value`
/// must equal `kv_value(i, generation)` for the key index `i` it encodes.
fn kv_decode(value: &[u8; KV_VALUE_LEN]) -> Option<(u32, u32)> {
    let i = u32::from_le_bytes([value[0], value[1], value[2], value[3]]);
    let generation = u32::from_le_bytes([value[4], value[5], value[6], value[7]]);
    if i < KV_KEYS && kv_value(i, generation) == *value {
        Some((i, generation))
    } else {
        None
    }
}

/// Whether the TSDB live window is the one this workload wrote: every
/// committed entry's payload must equal `ts_payload(entry.time)`.  An empty
/// window (freshly formatted database) counts as ours.
fn ts_is_ours(ts: &mut Tsdb<RealFlash>) -> bool {
    let mut buf = [0u8; TS_PAYLOAD_LEN];
    let mut it = ts.iter();
    while let Some(entry) = it.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => return false,
        };
        match entry.status {
            TslStatus::Write => {
                if entry.time <= 0 || entry.log_len as usize != TS_PAYLOAD_LEN {
                    return false;
                }
                match it.read_blob(&entry, &mut buf) {
                    Ok(n) if n == TS_PAYLOAD_LEN && buf == ts_payload(entry.time) => {}
                    _ => return false,
                }
            }
            TslStatus::PreWrite => {}
            _ => return false,
        }
    }
    true
}

pub fn run() -> ! {
    // Warm restart after the runner's injected reset: the trace (and the
    // databases) must be left exactly as the interrupted run left them.
    if hwflash::trace_valid() {
        warm_restart_halt();
    }
    hwflash::trace_init();
    board::reset_output();
    board::init_cycles();
    write_bytes(b"FAULT_WRITE start\n\0");

    let mut kv_flash = match RealFlash::new(KV_BASE, PART_SIZE) {
        Ok(f) => f,
        Err(e) => writer_fail(ERR_KV_FLASH, Some(Error::FlashErase(e))),
    };
    // Publish every controller operation for the verifier's post-mortem.
    kv_flash.enable_trace();
    let mut kv = match Kvdb::mount(kv_flash, kv_config()) {
        Ok(db) => db,
        Err(e) => writer_fail(ERR_KV_MOUNT, Some(e)),
    };

    let mut ts_flash = match RealFlash::new(TS_BASE, PART_SIZE) {
        Ok(f) => f,
        Err(e) => writer_fail(ERR_TS_FLASH, Some(Error::FlashErase(e))),
    };
    ts_flash.enable_trace();
    let mut ts = match Tsdb::mount(ts_flash, ts_config(), TS_MAX_LEN) {
        Ok(db) => db,
        Err(e) => writer_fail(ERR_TS_MOUNT, Some(e)),
    };
    ts.set_rollover(true);

    // Keep the databases only when they already hold this workload's data;
    // otherwise start from a clean partition (first cycle of a session).
    let kv_ours = {
        let stats = match kv.stats() {
            Ok(s) => s,
            Err(e) => writer_fail(ERR_KV_MOUNT, Some(e)),
        };
        if stats.key_count == 0 {
            true
        } else {
            let mut buf = [0u8; KV_VALUE_LEN];
            match kv.get(&kv_key(0), &mut buf) {
                Ok(Some(n)) if n == KV_VALUE_LEN => matches!(kv_decode(&buf), Some((0, _))),
                _ => false,
            }
        }
    };
    if !kv_ours {
        match kv.format() {
            Ok(()) => {}
            Err(e) => writer_fail(ERR_KV_FORMAT, Some(e)),
        }
    }
    if !ts_is_ours(&mut ts) {
        match ts.clean() {
            Ok(()) => {}
            Err(e) => writer_fail(ERR_TS_CLEAN, Some(e)),
        }
    }

    // Never-ending churn: every round rewrites all KV keys with a new
    // generation and appends a few TS payloads.  The volume drives the
    // databases' GC/rollover paths, i.e. real 8 KiB page erases interleaved
    // with 128-bit quad-word programs.
    let mut generation = 1u32;
    loop {
        let mut i = 0;
        while i < KV_KEYS {
            match kv.set(&kv_key(i), &kv_value(i, generation)) {
                Ok(()) => {}
                Err(e) => writer_fail(ERR_KV_SET, Some(e)),
            }
            i += 1;
        }
        generation = generation.wrapping_add(1);

        let mut a = 0;
        while a < TS_PER_KV_ROUND {
            let time = ts.last_time().wrapping_add(1);
            let payload = ts_payload(time);
            match ts.append(time, &payload) {
                Ok(()) => {}
                Err(e) => writer_fail(ERR_TS_APPEND, Some(e)),
            }
            a += 1;
        }
    }
}
