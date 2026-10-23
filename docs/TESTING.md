# Testing — inventory, coverage and known gaps

Testing happens in seven layers. The same workload (see
`harness/firmware/common/workload_spec.h` and `harness/SCENARIOS.md`) is used
wherever possible so results are comparable across layers.

| layer | what it proves | how to run |
|---|---|---|
| 1. Host unit + integration tests | crate behaviour, on-flash layouts, C parity, power-cut recovery, rollover, error paths | `cargo test -p flashdb --release` |
| 2. C reference behavioural checks | the C semantics the Rust tests are modelled on (including rollover safety) | `scripts/c_test.sh` |
| 3. Differential host tests | C and Rust produce byte-identical flash images for **all six** write granularities and identical op counts | `scripts/rust_compat.sh`, `scripts/compare_bench.py` |
| 4. QEMU firmware comparison | ROM / static RAM / stack / workload checksums of the two firmware images on a Cortex-M3 | `scripts/fw_c.sh`, `scripts/fw_rust.sh` |
| 5. On-silicon acceptance (NUCLEO-U545RE-Q) | the real crate on a Cortex-M33: identical 21-field workload output, on-flash image identity, RAM/ROM/stack and DWT cycles, C vs Rust | `harness/firmware/u545re/compare.ps1` |
| 6. On-silicon **real internal flash** | real 8 KiB page erase + 128-bit quad-word programming: image parity, power-loss injection, reboot soak, endurance, KVDB/TSDB long-run | `harness/firmware/u545re/{run_flash_parity,run_flash_fault,run_flash_reboot_soak,run_flash_tests}.ps1` |
| 7. CI | fmt, workspace tests (default + `std`), rustdoc `-D warnings`, QEMU firmware build, and a manual self-hosted hardware workflow | `.github/workflows/ci.yml`, `.github/workflows/hardware.yml` |

## Host test suite

`cargo test -p flashdb --release` → **128 test functions + 7 doctests = 135
tests**, plus **3 `#[ignore]`d long-run / large-data stress tests**
(`long_run.rs`, see below). `--features std` adds one more test (the
`std::error::Error` leaf).

| file | tests | covers |
|---|---:|---|
| `src/lib.rs` unit tests | 6 | low-level decode helpers, CRC path |
| `kv_smoke.rs` | 7 | set/get/update/delete/iterate, stats, power-cut sweep on KV ops |
| `kv_formats.rs` | 7 | every supported write granularity mounts/serves the same data |
| `kvdb_c_parity.rs` | 17 | C test cases ported assertion-for-assertion + C-generated images |
| `error_paths.rs` | 39 | every-flash-op fault sweeps with typed `FlashRead/Write/Erase`, config/driver rejection, invalid names/values, corrupted and damaged images, KV PreWrite/PreDelete/GC recovery, cache hash collisions, padded-granule reads, TS forged sector states, 64/128/256-bit granularities, iterator boundary errors |
| `kv_powercut_check.rs` | 2 | after a reset at **every** flash op of a KV update, remount + `Kvdb::check()` + get + write all succeed (torn `PRE_WRITE`/`ERR_HDR` headers are not corruption) |
| `tsdb_format.rs` | 15 | sector headers, rollover default, full-with-rollover-off |
| `tsdb_iter.rs` | 8 | forward / reverse / by-time iterators, deleted records, boundaries |
| `tsdb_rollover.rs` | 5 | ring live window, sector `end_info` bytes, 10 wraps, by-time across the wrap, full no-rollover DB |
| `tsdb_recovery.rs` | 3 | power cut at **every** write (no wrap, across a wrap), interrupted `end_info` twice |
| `tsdb_c_parity.rs` | 15 | C/Rust image + summary parity for TS workflows |
| `error_surface.rs` | 4 (+1 with `--features std`) | every `Error` variant's text, driver-error passthrough, `Error` traits, `std::error::Error` leaf |
| `long_run.rs` | 3 (**ignored** by default) | long-run soak + large-data: 100 k KV updates on 512 KiB, 500 TS ring wraps (154 k appends), 256 KiB/1 MiB partitions with 4 KiB values |
| doctests | 7 | documented API examples |

### Coverage

Measured with `cargo llvm-cov -p flashdb --release --summary-only`
(cargo-llvm-cov 0.9.1, rustc 1.99), 2026-10-23. `--features std` gives the same
numbers (the total includes the extra `std` leaf test).

| file | lines | functions | regions |
|---|---:|---:|---:|
| `error.rs` | 100.00 % (13) | 100 % | 100 % |
| `flash.rs` | 100.00 % (9) | 100 % | 100 % |
| `low.rs` | 96.36 % (522) | 95.83 % | 91.84 % |
| `kvdb.rs` | 93.34 % (1441) | 100 % | 88.53 % |
| `storage.rs` | 82.40 % (125) | 93.75 % | 72.25 % |
| `tsdb.rs` | 94.54 % (1008) | 100 % | 88.09 % |
| **total** | **93.84 % (3118 lines, 192 missed)** | **98.94 % (2/189)** | **87.91 %** |

The remaining line misses are dead/defensive branches (unreachable through the
public driver contract), release-only artefacts (`const fn` tables, compiled-out
`debug_assert!` code) and error variants no source path constructs
(`OutOfBounds`, `NotFormatted`, `write_status(0)`); the exact lines are listed by
`cargo llvm-cov --open` / `--show-missing-lines`. No test-only hooks or `unsafe`
reach-ins were added to the library.

### Long-run / large-data stress tests

The normal suite stays fast (≈2 s), so the multi-thousand-operation soak and
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

They are deterministic and RAM-simulator based, so they are cheap in CI.

## C reference and differential layers

* `scripts/c_test.sh` compiles and runs the reference FlashDB test suite
  (`reference/FlashDB/tests/`) with the harness config.
* `scripts/rust_compat.sh` covers **all six C write granularities** (1, 8, 32,
  64, 128, 256 bits): both implementations write a deterministic 32 KiB image
  and each mounts the other's image. Result: **95/97 enforced checks pass, 0
  FAIL, 2 expected XFAIL, exit 0** (raw matrix:
  [`docs/acceptance/host_compat_matrix.txt`](acceptance/host_compat_matrix.txt)).
  * Rust images are **byte-identical** (SHA-256, 32 768 bytes) to C images for
    `WRITE_GRAN` 1/8/32/64/128, with identical logical summaries both ways.
  * `WRITE_GRAN=256`: all logical checks pass, but FlashDB-C's tool is itself
    nondeterministic (20 uninitialised TS log-index padding bytes per record, its
    own selfcheck fails) → expected XFAIL, diff proven padding-only.
  * This layer found a real C-compat bug for `WRITE_GRAN > 8` in `low.rs`
    (C encodes a status granule as `00 FF..FF` and decodes on its first byte;
    Rust zeroed/required the whole granule). Fixed with a 2-hunk private change:
    wg 32/64/128/256 images are now interchangeable, 1/8 unchanged. The fix also
    reduced the board image by ~110 B.
* `scripts/compare_bench.py` checks host benchmark checksums/flash counters; see
  `docs/BENCHMARKS.md` §2/§3 for the refreshed 2026-10-23 numbers.

## QEMU firmware comparison

`scripts/fw_c.sh` / `scripts/fw_rust.sh` build the identical workload for the
C and Rust firmwares on `mps2-an385` (Cortex-M3, semihosting) and compare ROM,
static RAM, stack high-water and the workload checksums. The `-z` profile's
cycle count can additionally be measured with the deterministic QEMU proxy
(`scripts/fw_rust.sh qemu-cycles`); results are in `docs/BENCHMARKS.md` §4.

## On-silicon board tests

All on-silicon measurements use the NUCLEO-U545RE-Q (STM32U545RE, Cortex-M33),
ST-LINK/V3 and probe-rs 0.32, with DWT cycle counters and a RAM result buffer
read back over SWD (`harness/firmware/u545re/board_common.ps1`). The clock is
the 4 MHz MSIS reset clock. Hardware runs are serialised: one runner at a time.

### 5. Acceptance: C vs Rust on identical silicon

`harness/firmware/u545re/compare.ps1` builds both firmwares from the same
workload, runs them back-to-back and compares 21 logical fields, image
checksums, ROM/RAM/stack and cycles. Final result on 2026-10-23 (three
configurations):

| metric | C (`-Os`) | Rust stable `size`+`small-crc` | Rust nightly `size`+`small-crc` |
|---|---:|---:|---:|
| ROM `.text+.rodata+.data` | 13 360 B | **13 228 B** (−132) | **13 204 B** (−156) |
| static RAM (`.data+.bss`) | 1 072 B | 640 B (−432) | 640 B (−432) |
| stack high-water | 1 240 B | 1 080 B (−160) | 1 080 B (−160) |
| cycles mount | 521 663 | 274 790 (−47.3 %) | 274 820 (−47.3 %) |
| cycles KV | 34 398 902 | 21 703 993 (−36.9 %) | 21 600 093 (−37.2 %) |
| cycles TS | 391 362 | 343 462 (−12.2 %) | 344 469 (−12.0 %) |
| cycles rollover | 2 554 344 | 2 069 693 (−19.0 %) | 2 075 206 (−18.8 %) |
| cycles total | 38 160 955 | 24 714 339 (−35.2 %) | 24 616 989 (−35.5 %) |
| logical fields | — | 21/21 identical | 21/21 identical |

The `release` profile is a CPU-first configuration and trades ROM for speed:
ROM 15 128 B (+1 768 vs C, the one expected acceptance exception), stack
1 048 B (−192), everything else faster: cycles total 21 277 175 (−44.2 %).
Full reports are committed under `docs/acceptance/board_compare_*.txt`.

### 6. Real internal flash (bank-1 scratch pages 24..31 only)

These tests program/erase the STM32U5's **real** internal flash (8 KiB page
erase, 128-bit quad-word programming), never the firmware pages. The scratch
region is `0x0803_0000..0x0803_FFFF`.

| test | runner | what it does | result (2026-10-23) |
|---|---|---|---|
| image parity | `run_flash_parity.ps1` | crate writes a deterministic KV+TS image on internal flash (`WRITE_GRAN=128`, sec 8 KiB, 4+4 pages), remounts and verifies every value/payload, FNV over both raw partitions; compared with the identical workload run on the host model with strict 128-bit chunks | **PASS** — all 6 fields equal: `kv_entries=41 kv_checksum=0xe271b9e1 ts_entries=120 ts_checksum=0x43bc2d05 kv_image_fnv=0x199f5d76 ts_image_fnv=0x481067fb`, stack 3 832 B |
| power-loss injection | `run_flash_fault.ps1` | a never-ending KV/TS writer is stopped by an **external reset** at random points inside real flash work (30 cycles seed 99 + 20 cycles seed 1234); a verifier remounts, runs `check()`, validates every generation/payload/TS window, writes again and remounts | **PASS** — 50/50 resets recovered `rc=0`; traces show resets inside program windows and between operations; `check()` required a fix (below) |
| reboot soak | `run_flash_reboot_soak.ps1` | 20 boots, 19 `NVIC_SystemReset`s: each boot verifies the previous round, writes 64 KV updates + 20 TS appends, resets itself; final boot verifies the complete persisted state | **PASS** — `rounds=20 boots=20 resets=19 kv_sets=1280 ts_appends=400 kv_keys=8 ts_last=400` |
| endurance | `run_flash_tests.ps1 -Test endurance` | 240 s of page erase + full-page quad-word program + read-back verify + erase verify, rotating over all 8 scratch pages; NSSR error flags checked every cycle | **PASS** — 22 493 P/E cycles (2 811–2 812/page), 740 967 quad-word programs, 22 509 erases, 0 errors, 960 015 503 cycles @4 MHz |
| KVDB/TSDB long-run | `run_flash_tests.ps1 -Test longrun` | 32 KiB KV + 32 KiB TS on internal flash for 60 s of mixed writes, rollover, queries and remounts, with per-round verification | **PASS** — 72 rounds, 2 336 KV sets, 3 456 TS appends, 1 405 erases, 18+18 remounts (60.95 s, stack 3 536 B), 0 errors |

The power-loss test found a **recovery-contract issue** that host fault
injection had not exercised (the host sweeps used `WRITE_GRAN=8`): after a reset
during a KV record write, mount succeeded and the data was usable, but
`Kvdb::check()` returned `ReadCorrupted` for the torn record that recovery had
already quarantined. `check()` now skips exactly the two states the sector walk
already skips for torn records (`PRE_WRITE` and recovered `ERR_HDR`) and still
reports damage to committed records; `kv_powercut_check.rs` sweeps every flash
op of a KV update at wg 8 and 128 as a regression test.

### Reproducing all hardware runs

```powershell
# Host layers
cargo test -p flashdb --release
cargo test -p flashdb --release --features std
cargo test -p flashdb --release --test long_run -- --ignored --nocapture
pwsh -NoProfile -File scripts/fw_rust.ps1 -Profile size -SmallCrc   # QEMU
# Board: acceptance and real internal flash
pwsh -NoProfile -File harness/firmware/u545re/compare.ps1 -Nightly
pwsh -NoProfile -File harness/firmware/u545re/compare.ps1 -RustProfile size -RustFeatures small-crc
pwsh -NoProfile -File harness/firmware/u545re/compare.ps1 -RustProfile release -RustFeatures small-crc
pwsh -NoProfile -File harness/firmware/u545re/run_flash_parity.ps1
pwsh -NoProfile -File harness/firmware/u545re/run_flash_fault.ps1 -Iterations 30 -Seed 99
pwsh -NoProfile -File harness/firmware/u545re/run_flash_reboot_soak.ps1
pwsh -NoProfile -File harness/firmware/u545re/run_flash_tests.ps1 -Test endurance -TimeoutSec 400
pwsh -NoProfile -File harness/firmware/u545re/run_flash_tests.ps1 -Test longrun -TimeoutSec 300
```

The real-flash suites are **build-time feature variants** of the same firmware
crate (`flash-parity`, `flash-fault-write`, `flash-fault-verify`,
`flash-reboot-soak`, `flash-endurance`, `flash-longrun`); none of them is part of
the standard acceptance image, and each runner reflashes the board.

## Continuous integration

`.github/workflows/ci.yml` (push / PR / manual) runs on ubuntu-latest:

* `host`: `cargo fmt --all --check`, `cargo test --workspace --release`,
  `cargo test -p flashdb --release --features std`,
  `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`.
* `firmware-qemu`: installs `qemu-system-arm`, `gcc-arm-none-eabi` and
  `python3`, fetches the reference FlashDB (`scripts/fetch_reference.sh`), then
  runs `scripts/fw_c.sh` and `scripts/fw_rust.sh` (default profile); firmware
  build directories are uploaded on failure.

`.github/workflows/hardware.yml` is **manual** (`workflow_dispatch`) and targets
a self-hosted Windows runner labelled `stm32u5` with a NUCLEO-U545RE-Q attached
via probe-rs; it runs `compare.ps1 -Nightly`, endurance and long-run flash tests
and uploads the result logs. It is the only part of the suite that needs
physical hardware.

## Known gaps

* **PVT / qualification**: no voltage-margin, temperature or retention/bake
  testing; those need a qualification lab, not a dev board. The endurance run
  is a functional/stress check (22.6 k P/E cycles on 8 pages, 0 errors), not a
  datasheet-endurance qualification.
* **Soak duration**: the longest on-silicon continuous runs are 240 s
  (endurance) and 60 s (KVDB/TSDB); no multi-hour or multi-day soak yet.
* **Mid-erase reset injection**: the power-loss test resets the target through
  the debug probe. Traces show resets inside quad-word **program** windows and
  between operations, but none inside an erase window — a debug reset request
  appears to be serviced only after the flash controller finishes the long
  erase. Deterministic mid-erase injection needs an on-chip watchdog/RAM-ISR
  reset source (future work).
* **Production-grade flash driver**: the on-silicon driver lives in the test
  harness (`harness/firmware/rust/hwflash.rs`): single bank, scratch pages only,
  no RDP/WRP/option bytes, dual-bank, ECC error retry/logging, suspend/resume or
  DMA. The crate itself is driver-agnostic (`Flash` trait).
* **`WRITE_GRAN=256` byte identity**: FlashDB-C's own image tool is
  nondeterministic at 256 bits (uninitialised TS padding), so byte-identity is
  unprovable against this reference version; the Rust side writes erased
  padding and all logical checks pass.
* **QEMU ROM**: the Cortex-M3 QEMU image is still +224 B vs C because the
  harness's rodata split differs; the board acceptance image is smaller than C
  in all `size` configurations (see above). The `release` profile is +1 768 B
  ROM by design (it optimises CPU, not size).
* **CI hardware leg**: `hardware.yml` exists but no self-hosted runner is
  attached to this repository, so the hardware legs remain manual here.
* **Coverage**: 93.84 % lines / 98.94 % functions; the remaining lines are
  listed above and are dead defensive branches, const-eval artefacts or error
  variants the public contract does not construct.
