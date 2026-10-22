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
| 5. On-silicon board tests (NUCLEO-U545RE-Q) | the real crate on a Cortex-M33: correctness, rollover, on-flash image identity, RAM/ROM/stack and DWT cycles, C vs Rust; plus real internal-flash endurance and KVDB/TSDB long-run tests | `harness/firmware/u545re/compare.ps1`, `harness/firmware/u545re/run_flash_tests.ps1` |

## Host test suite

`cargo test -p flashdb --release` → **94 tests** (87 test functions + 7 doctests),
plus **3 `#[ignore]`d long-run / large-data stress tests** (`long_run.rs`, see
below).

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
| `long_run.rs` | 3 (**ignored** by default) | long-run soak + large-data: 100 k KV updates on 512 KiB, 500 TS ring wraps (154 k appends), 256 KiB/1 MiB partitions with 4 KiB values |
| doctests | 7 | documented API examples |

### Long-run / large-data stress tests

The normal suite stays fast (≈3 s), so the multi-thousand-operation soak and
large-partition cases live in `crates/flashdb/tests/long_run.rs` and are
**ignored by default**. Run them explicitly before a release (release mode
recommended):

```bash
cargo test -p flashdb --release --test long_run -- --ignored --nocapture
```

| test | workload | measured (2026-10-23, release, Windows) |
|---|---|---:|
| `kv_long_run_gc_soak` | 512 KiB KVDB (128 sectors), 2 000 keys × 64 B, **100 000 updates** with continuous GC/erase churn; checks key set, iterator count and a deterministic value checksum before/after remount; requires >500 sector erases | 2.76 s |
| `ts_many_ring_wraps_soak` | 16 KiB TSDB, 32 B payloads, **500 full ring wraps** (154 000 appends); checks contiguous live window, payload/timestamp checksum, `query_count`, ≥2 000 erases, and the same window after remount | <0.01 s |
| `large_partition_and_values` | **256 KiB KVDB with 3 800 B values** (8 GC rounds) and a **1 MiB TSDB with 4 000 B payloads** (400 appends → wrap); verifies every value/payload before and after remount | 0.01 s |

They are deterministic and RAM-simulator based; they prove long-run
bookkeeping/GC/rollover consistency at scale, not flash-endurance (P/E wear) or
real-flash timing — those remain in the known-gaps list.


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

Current result: **PASS 11/11** — ROM 13 340 B (stable) / 13 304 B (nightly) vs
13 360 B, stack 1 080 B vs 1 240 B, static RAM 640 B vs 1 072 B, and every cycle
phase below C (total −34.4 % / −35.0 %). Report:
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

### Real internal-flash endurance and long-run (`run_flash_tests.ps1`)

Two further board images drive the **STM32U5 flash controller itself** instead
of the RAM NOR simulator (driver: `harness/firmware/rust/hwflash.rs`, non-secure
FLASH register block, 8 KiB page erase, 128-bit quad-word programming, NSSR
error flags checked after every operation; scratch pages 24..31 of bank 1 only):

* `-Test endurance` — real page erase + quad-word program cycles with read-back
  and full-page erased verification every cycle (the full 8 KiB page is
  programmed every 16th cycle), time-budgeted (~120 s). Measured on 2026-10-23:
  **10 259 P/E cycles, 338 320 quad-words (≈2.7 MB) programmed, ≈84 MB erased,
  0 verification failures, 0 controller errors**; ~11.7 ms per P/E cycle at 4 MHz.
* `-Test longrun` — the real crate on internal flash: KVDB 32 KiB + TSDB 32 KiB,
  `WRITE_GRAN_BITS = 128`, alternating KV rewrite/TS append rounds for ~60 s,
  every value and the whole TS live window verified each round, both databases
  remounted and re-verified periodically. Measured on 2026-10-23: **65 rounds,
  2 112 KV sets, 3 120 TS appends, 1 038 sector erases, 0 verification failures,
  0 controller errors**; stack 3 440 B of the 8 KiB painted stack.

Full result lines and reproduction commands are in
[`harness/firmware/u545re/README.md`](../harness/firmware/u545re/README.md).


## Coverage (host suite, crate text)

Measured with `cargo llvm-cov -p flashdb --release --summary-only` on
2026-10-23 (after the ROM/stack refactor; 3 110 executable lines):

| file | line coverage | function coverage |
|---|---:|---:|
| `error.rs` | 100.0 % | 100.0 % |
| `flash.rs` | 100.0 % | 100.0 % |
| `low.rs` | 94.25 % | 92.0 % |
| `tsdb.rs` | 87.00 % | 81.4 % |
| `kvdb.rs` | 83.11 % | 90.7 % |
| `storage.rs` | 77.60 % | 87.5 % |
| **total** | **86.14 %** (3 110 lines, 431 missed) | **87.89 %** (190 fns, 23 missed) |

Regions: 78.93 %. Not measured by this metric: the firmware harness and the C
reference (they are exercised on silicon / by `scripts/c_test.sh` instead, and
they are measurement scaffolding rather than the library API), nor the ignored
long-run tests above (they are a separate opt-in run).

Note: llvm-cov warns about "mismatched function data" because doctest binaries
are not instrumented; the totals above are from the default-feature run and are
the authoritative numbers. The `std` feature itself was broken before the
previous round (`extern crate std` was `#[cfg(test)]` only) and now has a
regression test.


## Documentation build

`cargo doc --workspace --no-deps` generates the API documentation; it is clean
under `RUSTDOCFLAGS="-D warnings"` (missing-docs and broken intra-doc links are
enforced for `flashdb` and `flashdb-sim`). Open `target/doc/flashdb/index.html`
afterwards.

## Known gaps

* **Real internal-flash driver**: the standard board workload still runs on the
  RAM NOR simulator, but the controller itself is now exercised by
  `run_flash_tests.ps1` (page erase + 128-bit quad-word programming on the
  bank-1 scratch pages). It is a test driver only: no RDP/WRP/option-byte or
  power-loss-during-write handling, and only the bank-1 scratch region is used.
* **Hardware fault injection**: power-cut recovery is covered on the host
  simulator only (every write position) and not during an internal-flash write
  on the MCU.
* **Long-run / endurance is time-budgeted, not exhaustive**: the host
  `long_run.rs` covers 100 k-update GC churn, 500 rollover wraps and 4 KiB
  records on a 1 MiB partition; the on-silicon endurance run reaches 10 259 P/E
  cycles on one page (0 errors) and the long-run test does 1 038 real sector
  erases in 60 s. There is still no multi-day soak, no PVT/voltage-margin
  endurance sweep, and the on-silicon acceptance workload itself is ~7 s per run.
* **Nightly size build**: now measured and part of the acceptance
  (`compare.ps1 -Nightly`, ROM 13 304 B, 21 fields identical) — no longer a gap;
  the QEMU-side nightly ROM figure is not tracked separately.
* **`--features std`** is compiled and tested, but not part of the default
  coverage run.
* Broken or unreachable branches inside `kvdb.rs`/`tsdb.rs` (≈14 % of lines) are
  mostly error paths and rare recovery corners; see the per-file report via
  `cargo llvm-cov --open` for the exact lines if more coverage is wanted.
