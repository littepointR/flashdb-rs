//! Host model for the on-silicon real-flash parity test.
//!
//! Runs the exact workload that `harness/firmware/rust/flash_parity.rs` runs on
//! STM32U5 **internal flash**, but on `flashdb-sim::RamFlash` on the host, with
//! the same geometry (two 4 x 8 KiB partitions, `WRITE_GRAN_BITS = 128`) and the
//! same deterministic workload. The workload itself is compiled from
//! `harness/firmware/rust/parity_workload.rs`, so board and model cannot drift.
//!
//! The printed `MODEL` line carries the expected values (checksums, entry
//! counts and FNV-1a-32 fingerprints of both raw partition images) that
//! `harness/firmware/u545re/run_flash_parity.ps1` compares the board's `RESULT`
//! line against. The workload is run twice to prove it is deterministic.
//!
//! Usage:
//! ```text
//! cargo run --release -p flashdb-bench --bin parity_model
//! ```
//!
//! Exit status: 0 on success, 2 on a workload failure, 3 if two consecutive
//! host runs differ (the workload/geometry is not deterministic).

// The firmware and the host model must run the *same* source: the file is
// shared verbatim with `harness/firmware/rust/flash_parity.rs`.
#[path = "../../../../harness/firmware/rust/parity_workload.rs"]
mod parity_workload;

use flashdb_sim::RamFlash;
use parity_workload as workload;

/// One partition of the deterministic image, with the same size and write
/// granularity as the internal-flash partitions on the board.
const PART_SIZE: usize = workload::PART_SIZE as usize;
type Nor = RamFlash<PART_SIZE, 128>;

/// Run the workload once on freshly erased simulated flash.
fn run_once() -> workload::Outcome {
    // RamFlash::new is factory-erased (0xFF), exactly like the firmware's
    // scratch pages after its explicit page erase. Strict write checking makes
    // the simulator reject write chunks that are not a whole 128-bit granule,
    // like the real STM32U5 quad-word programming and `RealFlash::write`.
    let mut kv_flash = Nor::new(workload::SEC_SIZE);
    kv_flash.set_strict_write(true);
    let mut ts_flash = Nor::new(workload::SEC_SIZE);
    ts_flash.set_strict_write(true);
    match workload::run(kv_flash, ts_flash) {
        Ok(outcome) => outcome,
        Err(e) => {
            let stage = workload::stage_tag(e.stage());
            let stage = stage.strip_suffix(&[0]).unwrap_or(stage);
            eprintln!(
                "parity_model: workload failed at stage {} ({:?}): {:?}",
                String::from_utf8_lossy(stage),
                e.stage(),
                e
            );
            std::process::exit(2);
        }
    }
}

fn main() {
    let first = run_once();
    let second = run_once();
    if first != second {
        eprintln!("parity_model: workload is not deterministic (two host runs differ)");
        std::process::exit(3);
    }

    println!(
        "MODEL kind=flashparity wg=128 sec_size={} part_size={} \
         kv_entries={} kv_checksum=0x{:08x} ts_entries={} ts_checksum=0x{:08x} \
         kv_image_fnv=0x{:08x} ts_image_fnv=0x{:08x} rc=0",
        workload::SEC_SIZE,
        workload::PART_SIZE,
        first.kv_entries,
        first.kv_checksum,
        first.ts_entries,
        first.ts_checksum,
        first.kv_image_fnv,
        first.ts_image_fnv,
    );
}
