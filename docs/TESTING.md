# Testing — inventory, coverage and known gaps

Testing happens in five layers. The same workload (see
`harness/firmware/common/workload_spec.h` and `harness/SCENARIOS.md`) is used
wherever possible so results are comparable across layers.

| layer | what it proves | how to run |
|---|---|---|
| 1. Host unit + integration tests | crate behaviour, on-flash layouts, C parity, power-cut recovery, rollover | `cargo test -p flashdb --release` |
| 2. C reference behavioural checks | the C semantics the Rust tests are modelled on (including rollover safety) | `scripts/c_test.sh` |
| 3. Differential host tests | C and Rust produce byte-identical flash images and identical op counts | `scripts/rust_compat.sh`, `scripts/compare_bench.py` |
| 4. QEMU firmware comparison | ROM / static RAM / stack / workload checksums of the two firmware images on a Cortex-M3 | `scripts/fw_c.sh`, `scripts/fw_rust.sh` |
| 5. On-silicon board tests (NUCLEO-U545RE-Q) | the real crate on a Cortex-M33: correctness, rollover, on-flash image identity, RAM/ROM/stack and DWT cycles, C vs Rust | `harness/firmware/u545re/compare.ps1` |

## Host test suite

`cargo test -p flashdb --release` → **94 tests** (87 test functions + 7 doctests).

| file | tests | covers |
|---|---:|---|
| `kv_smoke.rs` | 6 | set/get/update/delete/iterate, stats, power-cut sweep on KV ops |
| `kv_formats.rs` | 4 | every supported write granularity mounts/serves the same data |
| `kvdb_c_parity.rs` | 17 | C test cases ported assertion-for-assertion + C-generated images |
| `tsdb_format.rs` | 7 | sector headers, rollover default, full-with-rollover-off |
| `tsdb_iter.rs` | 15 | forward / reverse / by-time iterators, deleted records, boundaries |
| `tsdb_rollover.rs` | 5 | ring live window, sector `end_info` bytes, 10 wraps, by-time across the wrap, full no-rollover DB |
| `tsdb_recovery.rs` | 3 | power cut at **every** write (no wrap, across a wrap), interrupted `end_info` twice |
| `tsdb_c_parity.rs` | 8 | C/Rust image + summary parity for TS workflows |
| `error_surface.rs` | 4 (+1 with `--features std`) | every `Error` variant's text, driver-error passthrough, `Error` traits, `std::error::Error` leaf |
| doctests | 7 | documented API examples |

The C reference layer (2) asserts the corresponding C invariants, e.g. for
rollover: 2×capacity appends succeed, sectors are erased while rolling, the entry
count stays at capacity, no NOR set-bit attempts, no out-of-bounds access, and
remount after rollover succeeds (`harness/c/bench.c: check_ts_rollover`).

## On-silicon tests in detail

`harness/firmware/u545re/compare.ps1` builds both images, flashes each in turn
and runs the following checks on the real MCU (11 acceptance checks, exit status
0 only if all pass). Two configurations currently pass all checks
(`-RustProfile size -RustFeatures small-crc` and `-Nightly`); the `release`
profile passes everything except the ROM row:

* 21 logical fields must be identical between C and Rust: KV/TS counters and
  checksums, rollover window (`count`, first/last time), 9 sector erases, 0 NOR
  violations, remount count/checksum, post-recovery append, `rc`, and the FNV-1a
  checksums of both **raw 16 KiB flash partitions**;
* ROM, static RAM, stack high-water and static+stack must be ≤ C;
* mount, KV, TS, **rollover** and total DWT cycles must be ≤ C;
* both images must report the same `core_hz` (4 MHz, decoded from RCC).

Current result: **PASS 11/11** — ROM 13 332 B (stable) / 13 296 B (nightly) vs
13 360 B, stack 1 072 B vs 1 240 B, static RAM 640 B vs 1 072 B, and every cycle
phase below C (total −34.6 % / −34.4 %). Report:
`harness/firmware/build/u545re/board_compare*.txt`.

Repeatability: two consecutive runs of each image must produce byte-identical
`RESULT` lines (workload and rollover/image lines). Currently verified.

### Without hardware

* QEMU firmware ROM + per-phase stack:
  `scripts/fw_rust.ps1 -Profile size -SmallCrc -Phases` (and `.sh` for POSIX).
  `-InlineThreshold -1` reproduces the old flag-free baseline; threshold 50 is
  the default measured configuration.
* QEMU cycle proxy: `scripts/fw_rust.ps1 -Profile size -SmallCrc -Cycles`
  (SysTick + `-icount shift=0`, 40 instructions/tick, deterministic). Use it for
  A/B ratios only — board ROM must come from a board build.
* Per-function stack frames: build nightly with `-Z emit-stack-sizes` and run
  `harness/firmware/tools/stack_sizes.py <elf>`.


## Coverage (host suite, crate text)

Measured with `cargo llvm-cov -p flashdb --release --summary-only`:

| file | line coverage | function coverage |
|---|---:|---:|
| `error.rs` | 100.0 % | 100.0 % |
| `flash.rs` | 100.0 % | 100.0 % |
| `low.rs` | 94.3 % | 92.0 % |
| `tsdb.rs` | 85.3 % | 83.9 % |
| `kvdb.rs` | 82.9 % | 92.0 % |
| `storage.rs` | 80.3 % | 87.5 % |
| **total** | **85.7 %** (2 895 lines, 413 missed) | **89.2 %** (176 fns, 19 missed) |

Regions: 78.6 %. Not measured by this metric: the firmware harness and the C
reference (they are exercised on silicon / by `scripts/c_test.sh` instead, and
they are measurement scaffolding rather than the library API).

Note: the `--features std` run reports 85.0 % because some instrumented test
binaries are reused from a different feature build (llvm-cov warns about
mismatched function data); the default-feature number above is the authoritative
one. The `std` feature itself was broken before this round (`extern crate std`
was `#[cfg(test)]` only) and now has a regression test.

## Known gaps

* **Real internal-flash driver**: the board workload still runs on the RAM NOR
  simulator. Erase-block behaviour, endurance and power loss on the STM32U5
  flash controller itself are not yet exercised on silicon.
* **Hardware fault injection**: power-cut recovery is covered on the host
  simulator only (every write position), not on the MCU.
* **Nightly size build**: now measured and part of the acceptance
  (`compare.ps1 -Nightly`, ROM 13 296 B, 21 fields identical) — no longer a gap;
  the QEMU-side nightly ROM figure is not tracked separately.
* **`--features std`** is compiled and tested, but not part of the default
  coverage run.
* Broken or unreachable branches inside `kvdb.rs`/`tsdb.rs` (≈17 % of lines) are
  mostly error paths and rare recovery corners; see the per-file report via
  `cargo llvm-cov --open` for the exact lines if more coverage is wanted.

