//! On-silicon real internal-flash parity test (`flash-parity` feature).
//!
//! Runs the real `flashdb` crate on **internal STM32U5 flash** (not the RAM
//! simulator) with the same geometry, deterministic workload and checksums as
//! the host model (`crates/flashdb-bench/src/bin/parity_model.rs`), which runs
//! the workload on `flashdb-sim`:
//!
//! * KV partition: 4 x 8 KiB pages at `SCRATCH_BASE` (bank-1 pages 24..27);
//! * TS partition: 4 x 8 KiB pages at `SCRATCH_BASE + 32 KiB` (pages 28..31);
//! * `WRITE_GRAN_BITS = 128` (the device's quad-word programming granularity);
//! * then format/clean, the deterministic workload, a remount with full
//!   value/payload verification and iteration, and an FNV-1a-32 fingerprint of
//!   both raw 32 KiB partition images.
//!
//! The whole workload lives in `parity_workload.rs`, which the host model
//! compiles verbatim; a board/model mismatch therefore means the *real flash
//! bytes* differ from the simulator's, not that the two workloads drifted.
//!
//! Pass criteria (host side, `run_flash_parity.ps1`): `rc=0`, `errors=0`,
//! `verify_ok=1`, and every checksum / entry count / raw-image FNV equal to the
//! host model's. A failure prints an `ERROR stage=<tag> code=<n>` line (plus
//! `hw_op`/`hw_status` when the STM32 flash controller reported the error),
//! followed by `RESULT rc=1`.

use super::hwflash::{HwError, RealFlash, PAGE_SIZE, SCRATCH_BASE};
use super::{
    board, fail, stack_high_water_bytes, stack_limit_bytes, write_bytes, write_hex32, write_u32,
};
use flashdb::{Error, Flash};

// The deterministic workload is shared verbatim with the host model
// (`crates/flashdb-bench/src/bin/parity_model.rs`); see that file's module docs.
#[path = "parity_workload.rs"]
mod parity_workload;

use parity_workload::{self as workload, Outcome};

/// Database sector (erase block) size: one internal-flash page.
const SEC_SIZE: u32 = workload::SEC_SIZE;
/// Partition size: 4 sectors.
const PART_SIZE: u32 = workload::PART_SIZE;
/// KV partition: scratch pages 24..27.
const KV_BASE: u32 = SCRATCH_BASE;
/// TS partition: scratch pages 28..31.
const TS_BASE: u32 = SCRATCH_BASE + PART_SIZE;

/// The shared workload geometry must line up with the STM32U5 page size and the
/// scratch region reserved for the real-flash tests.
const _: () = assert!(SEC_SIZE == PAGE_SIZE);
const _: () = assert!(2 * PART_SIZE <= super::hwflash::SCRATCH_PAGES * PAGE_SIZE);

/// Extract the controller error out of a database error (non-flash errors do
/// not come from the controller and are reported as zero).
fn hw_of(e: Error<HwError>) -> HwError {
    match e {
        Error::FlashRead(hw) | Error::FlashWrite(hw) | Error::FlashErase(hw) => hw,
        _ => HwError { op: 0, status: 0 },
    }
}

/// Report a hardware/controller failure and stop the firmware.
fn hw_fail(stage: &[u8], code: u32, hw: HwError) -> ! {
    write_bytes(b"hw_op=\0");
    write_u32(hw.op as u32);
    write_bytes(b" hw_status=\0");
    write_hex32(hw.status);
    fail(stage, code)
}

/// Report a workload failure (database operation or verification mismatch).
fn report_failure(e: workload::ParityError<HwError>) -> ! {
    match e {
        workload::ParityError::Db { stage, err } => {
            hw_fail(workload::stage_tag(stage), 0, hw_of(err))
        }
        workload::ParityError::Verify { stage, index } => fail(workload::stage_tag(stage), index),
    }
}

/// Create the real-flash partition and erase it, so the workload always starts
/// from the same factory-fresh state as the host model's simulator (the parity
/// run must not depend on what a previous run left in the scratch pages).
fn make_partition(base: u32, tag: &[u8]) -> RealFlash {
    let mut flash = match RealFlash::new(base, PART_SIZE) {
        Ok(flash) => flash,
        Err(hw) => hw_fail(tag, 0, hw),
    };
    if let Err(hw) = flash.erase(0, PART_SIZE) {
        hw_fail(tag, 1, hw);
    }
    flash
}

pub fn run() -> ! {
    board::reset_output();
    board::init_cycles();
    let hz = match board::core_hz() {
        0 => 4_000_000,
        h => h,
    };
    let stack_limit = stack_limit_bytes();

    // Both partitions are erased first: the host model's RamFlash is born
    // erased, so this makes the starting state identical on both sides.
    let kv_flash = make_partition(KV_BASE, b"kv_flash\0");
    let ts_flash = make_partition(TS_BASE, b"ts_flash\0");

    let outcome: Outcome = workload::run(kv_flash, ts_flash).unwrap_or_else(|e| report_failure(e));
    let stack = stack_high_water_bytes();

    write_bytes(b"RESULT kind=flashparity kv_entries=\0");
    write_u32(outcome.kv_entries);
    write_bytes(b" kv_checksum=\0");
    write_hex32(outcome.kv_checksum);
    write_bytes(b" ts_entries=\0");
    write_u32(outcome.ts_entries);
    write_bytes(b" ts_checksum=\0");
    write_hex32(outcome.ts_checksum);
    write_bytes(b" kv_image_fnv=\0");
    write_hex32(outcome.kv_image_fnv);
    write_bytes(b" ts_image_fnv=\0");
    write_hex32(outcome.ts_image_fnv);
    write_bytes(b" stack=\0");
    write_u32(stack);
    write_bytes(b" stack_limit=\0");
    write_u32(stack_limit);
    write_bytes(b" errors=0 verify_ok=1 rc=0\n\0");

    write_bytes(b"BOARD core_hz=\0");
    write_u32(hz);
    write_bytes(b" wg=128\0");
    write_bytes(b"\n\0");

    super::exit()
}
