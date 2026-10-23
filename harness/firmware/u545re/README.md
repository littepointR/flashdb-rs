# NUCLEO-U545RE-Q real-hardware test

Board: **NUCLEO-U545RE-Q** — STM32U545RE (Cortex-M33 r0p4, 512 KiB flash,
192 KiB SRAM1 + 64 KiB SRAM2), on-board ST-LINK/V3 (VID:PID `0483:374e`).

The test runs the **real `flashdb` crate** (no host code) on the MCU:

* KVDB: 64 keys × 24 B, 32 updates, 1500-update GC churn, 64 verified gets,
  full iteration over a 16 KiB partition (4 × 4 KiB sectors);
* TSDB: 200 appends × 16 B payload, full forward iteration over a second
  16 KiB partition; then a rollover phase (1 200 appends past the ring
  capacity + live-window scan + remount + post-recovery append);
* the acceptance workload uses the **RAM NOR simulator** in the `.sim_flash`
  region, so this stage measures real Cortex-M33 CPU behaviour and checks
  correctness on silicon; the separate endurance/long-run images drive the
  **real STM32U5 internal flash controller** (see below).

Results are written to a RAM buffer (`BOARD_OUT`, `.results` section) instead of
semihosting, so they can be read back through any debug probe. DWT cycle
counters report mount/KV/TS/rollover/total cycles. `RESULT` fields are identical
to the QEMU measurement firmware, so the checksums can be compared directly with
`scripts/fw_rust.sh` / `scripts/fw_c.sh`.

## Status: acceptance PASS (2026-10-23)

`harness/firmware/u545re/compare.ps1` builds **both** firmware images with the
same GCC 11.3 / Rust toolchain, flashes each in turn through the on-board
ST-LINK/V3 with probe-rs, runs the identical workload and evaluates 11
acceptance checks. **The verdict is PASS in both the stable and the nightly
`size` build** (exit status 0; reports committed as
[`docs/acceptance/board_compare_nightly.txt`](../../../docs/acceptance/board_compare_nightly.txt)
and
[`docs/acceptance/board_compare_size_stable.txt`](../../../docs/acceptance/board_compare_size_stable.txt)):

| metric on silicon | C FlashDB | Rust `size` (stable) | Rust nightly | verdict |
|---|---:|---:|---:|---|
| ROM (text+rodata+data) | 13 360 B | **13 228 B** | **13 204 B** | PASS |
| static RAM (data+bss) | 1 072 B | **640 B** | **640 B** | PASS |
| stack high-water | 1 240 B | **1 080 B** | **1 080 B** | PASS |
| static + stack RAM | 2 312 B | **1 720 B** | **1 720 B** | PASS |
| cycles mount | 521 663 | **274 790** | **274 820** | PASS |
| cycles KV | 34 398 933 | **21 703 993** | **21 600 093** | PASS |
| cycles TS | 391 359 | **343 462** | **344 469** | PASS |
| cycles rollover | 2 554 338 | **2 069 693** | **2 075 206** | PASS |
| cycles total | 38 161 041 | **24 714 339** | **24 616 989** | PASS |
| 21 logical `RESULT` fields | — | identical | identical | PASS |
| both partition image checksums | — | identical | identical | PASS |

Every phase is faster than C (total CPU −35.2 % stable / −35.5 % nightly) while
the image is smaller (ROM −132 B / −156 B) and static+peak-stack RAM is 25.6 %
lower. The CPU-fastest `release` profile (`-RustProfile release
-RustFeatures small-crc`) is −44.2 % total CPU and 1 048 B stack, but its ROM is
15 128 B, so it passes every check except ROM.

Exact acceptance configuration (all defaults of the scripts):

* `size` profile (`opt-level="z"`, fat LTO, `codegen-units=1`, `panic=abort`);
* `small-crc` (64-byte nibble CRC table instead of 1 KiB);
* LLVM `-C llvm-args=-inline-threshold=50` (the `-Oz` default inliner is too
  conservative — see `docs/OPTIMIZATION.md` §2);
* LLD `--icf=all`;
* nightly build adds `-Z build-std=core`, `panic="immediate-abort"` and
  `-Z location-detail=none` (smallest image).

Firmware output captured from `BOARD_OUT` (Rust nightly; the stable build differs
only in ROM/cycles, the C reference prints identical workload/rollover/image
fields and its own BOARD cycle line):

```
RESULT text=12352 rodata=852 data=0 bss=640 sim_flash=32768 stack=1080 stack_init=424 stack_limit=8192 kv_writes=1596 kv_gets=64 kv_iterated=64 kv_checksum=0x4b3486a4 kv_iter_checksum=0x0e250daa ts_appends=200 ts_iterated=200 ts_checksum=0x10d90680 rc=0
RESULT ts_roll_appends=1200 ts_roll_erases=9 ts_roll_violations=0 ts_roll_count=392 ts_roll_first_time=2008 ts_roll_last_time=2399 ts_roll_checksum=0xb46ee0c0 ts_roll_remount_count=392 ts_roll_remount_checksum=0xb46ee0c0 ts_roll_continue_ok=1 kv_image_checksum=0x69f5868d ts_image_checksum=0x95091ba6
BOARD cycles_mount=274820 cycles_kv=21600093 cycles_ts=344469 cycles_roll=2075206 cycles_total=24616989 core_hz=4000000
```

### Board vs. QEMU baseline

The QEMU Cortex-M3 (`mps2-an385`) build of the same source produces identical
logical results — `kv_writes`, `kv_gets`, `kv_iterated`, `ts_appends`,
`ts_iterated`, all three workload checksums and `rc` — and differs only in the
section sizes/stack of the board support code (DWT counters, RAM result buffer,
RCC clock decode, rollover phase, ≈+1.5 KiB text) and its code/rodata split. For
the board, use the tables in this file; the QEMU ROM/RAM table lives in
`docs/BENCHMARKS.md`.

To reproduce the QEMU side, run `scripts/fw_rust.sh`, `scripts/fw_rust.ps1` on
Windows, or build the same source for `thumbv7m-none-eabi` against
`harness/firmware/common/mps2_an385.ld` and run:

```sh
qemu-system-arm -M mps2-an385 -cpu cortex-m3 -nographic -semihosting -kernel fw-rust
```

### Cycle counts

Board, Rust `size` + `small-crc` + threshold 50 (DWT, 4 MHz MSIS reset clock):

| phase | cycles | time @ 4 MHz |
|---|---:|---:|
| mount (KVDB + TSDB init) | 274 790 | 68.7 ms |
| KV workload (set/update/churn/get/iterate) | 21 703 993 | 5.40 s |
| TS workload (append/iterate) | 343 462 | 86.1 ms |
| rollover phase (1 200 appends + remount) | 2 069 693 | 518.8 ms |
| total | 24 714 339 | 6.15 s |

Cycle counts are clock-independent (DWT counts CPU cycles); the board is left on
its reset clock, so wall-clock time is dominated by the 4 MHz MSIS. Run-to-run
variation is ≤600 cycles (≈0.002 %) on `cycles_kv`/`cycles_total` — the host
polls the `DONE` flag over SWD while the workload runs. With a configured
160 MHz clock the same workload would take ≈154 ms.

`core_hz` is not assumed: the firmware decodes the STM32U5 RCC registers
(`CFGR1.SW`, `ICSCR1.MSIRGSEL`, `CSR.MSISSRANGE`) at runtime and reports
`core_hz=4000000` for the reset clock. If the clock tree is ever reconfigured it
reports `core_hz=0`, and the host prints raw cycles instead of a wrong
microsecond conversion.

## C vs Rust on identical silicon

The C reference has a board build as well (`scripts/fw_c.sh --board-u545`,
`-DFDB_BOARD_U545`): same harness, same workload, same DWT measurement points and
the same RAM result buffer, so FlashDB-C and the Rust rewrite can be compared on
the same MCU at the same clock with the same NOR simulator.
`harness/firmware/u545re/compare.ps1` builds both images, flashes them in turn,
and prints the table plus the acceptance verdict. Run-to-run cycle jitter is
≤0.002 %.

Measured on 2026-10-23 (STM32U545RE at the 4 MHz MSIS reset clock):

| metric | C FlashDB | Rust `size` stable | Rust nightly | Rust `release` |
|---|---:|---:|---:|---:|
| ROM (text+rodata+data) | 13 360 B | **13 228 B (−1.0 %)** | **13 204 B (−1.2 %)** | 15 128 B (+13.2 %) |
| static RAM (data+bss) | 1 072 B | **640 B (−40.3 %)** | **640 B (−40.3 %)** | **640 B (−40.3 %)** |
| stack high-water | 1 240 B | **1 080 B (−12.9 %)** | **1 080 B (−12.9 %)** | **1 048 B (−15.5 %)** |
| static + stack RAM | 2 312 B | **1 720 B (−25.6 %)** | **1 720 B (−25.6 %)** | **1 688 B (−27.0 %)** |
| cycles mount | 521 663 | **274 790 (−47.3 %)** | **274 820 (−47.3 %)** | **338 808 (−35.5 %)** |
| cycles KV | 34 398 933 | **21 703 993 (−36.9 %)** | **21 600 093 (−37.2 %)** | **18 529 245 (−46.1 %)** |
| cycles TS | 391 359 | **343 462 (−12.2 %)** | **344 469 (−12.0 %)** | **291 349 (−25.6 %)** |
| cycles rollover | 2 554 338 | **2 069 693 (−19.0 %)** | **2 075 206 (−19.0 %)** | **1 828 442 (−28.4 %)** |
| cycles total | 38 161 041 | **24 714 339 (−35.2 %)** | **24 616 989 (−35.5 %)** | **21 277 175 (−44.2 %)** |

Every workload, rollover and image field (21 in total: write/get/iterate counts,
three workload checksums, the rollover window/erases/recovery fields and both
partition image checksums) is identical between the two implementations in every
configuration.

Verdict:

* **ROM: parity achieved on silicon** — the Rust `.text` is still 1 184 B larger
  than C's, but the `small-crc` 64-byte table and shorter strings make `.rodata`
  1 284 B smaller, so the total is −132 B (stable) / −156 B (nightly).
* **Stack: parity achieved** — 1 080 B vs 1 240 B (the 1 048 B `release` build is
  even lower).
* **RAM: clearly ahead** — static RAM −40.3 %, static+stack −25.6 %.
* **CPU: every phase faster** — total −35.2 % stable / −44.2 % release.

## Rollover and on-flash image identity on silicon

The board workload ends with a rollover phase that is compiled into both images
(`board-u545` / `-DFDB_BOARD_U545` only, so the QEMU baseline is unchanged):

1. enable rollover and append 1 200 entries (≈3× the 4-sector ring capacity) with
   deterministic 16-byte payloads and timestamps 1 200…2 399;
2. iterate the live window and require **contiguous increasing timestamps and
   the exact expected payload of every surviving entry**;
3. **remount** (`Tsdb::mount_into` / `fdb_tsdb_deinit`+`fdb_tsdb_init`) and require
   the same window count, first/last timestamp and checksum;
4. append once more and require success and the advanced `last_time`;
5. require at least one sector erase during the phase, and **zero** NOR
   set-bit attempts / out-of-bounds accesses from the simulator's counters;
6. FNV-1a checksum **both raw partition images** (`kv_image_checksum`,
   `ts_image_checksum`) after the whole workload, so C and Rust must leave
   byte-identical flash contents — including the wrapped ring state — not just
   equal logical results.

All of this is verified field-for-field against the C reference by
`compare.ps1`; the rollover phase costs ≈2.07 M cycles (518.8 ms @ 4 MHz, i.e. 8.4 %
of the `size`-profile total).

The acceptance check is built into `compare.ps1` (exit status 0 only when all
criteria pass):

```powershell
# recommended: stable toolchain; passes all 11 checks
harness/firmware/u545re/compare.ps1 -RustProfile size -RustFeatures small-crc
# smallest image: nightly -Z build-std + panic=immediate-abort
harness/firmware/u545re/compare.ps1 -Nightly
# CPU-fastest build (fails only the ROM check)
harness/firmware/u545re/compare.ps1 -RustProfile release -RustFeatures small-crc
```

## Real internal-flash tests (2026-10-23)

Five board images exercise the **STM32U5 flash controller itself** instead of
the RAM NOR simulator: `run_flash_parity.ps1`, `run_flash_fault.ps1`,
`run_flash_reboot_soak.ps1` and `run_flash_tests.ps1 -Test endurance|longrun`.
They are build-time feature variants of the same firmware (`flash-parity`,
`flash-fault-write` / `flash-fault-verify`, `flash-reboot-soak`,
`flash-endurance`, `flash-longrun`).

Driver (`harness/firmware/rust/hwflash.rs`): non-secure FLASH register block
(`0x4002_2000`, TZEN=0), page erase (8 KiB), 128-bit quad-word programming,
NSSR error flags checked after every operation. It only touches the scratch
pages 24..31 of bank 1 (`0x0803_0000..0x0803_FFFF`), far above the firmware
image, so a test can never erase code. Hardware runs are serialised: one runner
at a time.

### Image parity (`run_flash_parity.ps1`)

The crate writes a deterministic 40-key KV + 120-entry TS workload
(`WRITE_GRAN=128`, 4 × 8 KiB pages each, shared source file
`harness/firmware/rust/parity_workload.rs`) on real internal flash, remounts,
verifies every value/payload and FNVs both raw partitions. The identical
workload is compiled into a host model
(`crates/flashdb-bench/src/bin/parity_model.rs`) running on `flashdb-sim` with
strict 128-bit chunk checking; the runner compares all six deterministic fields.

Measured 2026-10-23: **PASS** — board and model agree exactly:

```
MODEL  kind=flashparity wg=128 kv_entries=41 kv_checksum=0xe271b9e1 ts_entries=120 ts_checksum=0x43bc2d05 kv_image_fnv=0x199f5d76 ts_image_fnv=0x481067fb
BOARD  kind=flashparity ... same six fields ..., stack=3832 rc=0
```

### Power-loss injection (`run_flash_fault.ps1`)

A never-ending writer (KV churn with generation-encoded values + TS appends with
rollover, both on real internal flash) is stopped by an **external reset** while
it is programming/erasing. A verifier image then remounts both databases and
requires: mount ok, `Kvdb::check()` clean, every live KV value
generation-consistent, TS window contiguous with exact payloads, DB writable
again, remount clean. The writer leaves a fixed-address SRAM trace
(`TRACE_ADDR=0x2001_0000`) so the interrupted flash operation is reported.

Measured 2026-10-23: **PASS** — 30/30 cycles (seed 99, 1–500 ms delays) and
20/20 cycles (seed 1234, 20–250 ms) recovered with `rc=0`, with traces showing
resets inside quad-word **program** windows and between operations. No
erase-window reset was observed: a debug-probe reset appears to be serviced
after the flash controller finishes the long erase (documented limitation in
`docs/TESTING.md`; deterministic mid-erase injection needs an on-chip
watchdog/RAM-ISR source).

The test found a real recovery-contract issue: after a reset during a KV record
write, mount succeeded and the data was usable, but `Kvdb::check()` rejected the
torn record that recovery had already quarantined. `check()` now skips exactly
the two states the sector walk skips for torn records (`PRE_WRITE` and recovered
`ERR_HDR`) and still reports damage to committed records; the new host test
`crates/flashdb/tests/kv_powercut_check.rs` sweeps every flash op at wg 8 and
128.

### Reboot soak (`run_flash_reboot_soak.ps1`)

One flash download, then 20 boots with 19 `NVIC_SystemReset`s: each boot verifies
the previous round from flash, writes 64 KV updates + 20 TS appends and resets
itself; the final boot verifies the complete persisted state (8 keys, TS window
ending at 400). Measured: **PASS** — `rounds=20 boots=20 resets=19 kv_sets=1280
ts_appends=400 kv_keys=8 ts_entries=217 ts_first=184 ts_last=400
checksum=0x70fb5136`, 0 errors, stack 3 880 B.

### Endurance (`-Test endurance`)

Every cycle: program a quad-word (every 16th cycle the full 8 KiB page = 512
quad-words), read back and compare, erase the page, verify the whole page reads
`0xFF`, rotating over **all 8 scratch pages**; DWT budget 240 s; NSSR checked
every cycle.

Measured 2026-10-23: **PASS**

| metric | value |
|---|---:|
| P/E cycles (page erase + program) | **22 493** (2 812 × 5 pages, 2 811 × 3) |
| quad-words programmed | 740 967 (≈11.9 MB) |
| bytes erased | ≈184 MB (22 509 erases × 8 KiB) |
| read-back / erase verification failures | **0** |
| controller errors (NSSR OPERR/PROGERR/WRPERR/PGAERR/SIZERR/PGSERR/OPTWERR) | **0** |
| elapsed | 960 015 503 cycles = 240.00 s |
| per P/E cycle | ≈10.7 ms (including verification) |
| `FLASH_ACR` / final `NSSR` / `ECCR` | `0x00000000` / `0x00000000` / `0x00000000` |
| stack high-water | 168 B |

```
RESULT kind=endurance pe_cycles=22493 pages=8 pe_p0=2812 pe_p1=2812 pe_p2=2812 pe_p3=2812 pe_p4=2812 pe_p5=2811 pe_p6=2811 pe_p7=2811 programs=740967 erases_total=22509 elapsed_cycles=960015503 budget_cycles=960000000 checksum=0x52a4a712 errors=0 verify_ok=1 erased_ok=1 rc=0
BOARD core_hz=4000000 acr=0x00000000 nssr=0x00000000 nscr=0x40000000 eccr=0x00000000 stack=168 stack_limit=8192 last_nssr=0x00000000
```

### Long-run (`-Test longrun`)

Runs the real `flashdb` crate on internal flash: KVDB 32 KiB and TSDB 32 KiB
(4 × 8 KiB sectors each), `WRITE_GRAN_BITS = 128`. Alternating rounds: KV
rewrite of 32 keys × 32 B with full read-back compare; 48 TS appends × 32 B with
rollover enabled, then the whole live window is verified (contiguous timestamps,
exact payloads). Both databases are remounted every 4 rounds and at the end.

Measured 2026-10-23: **PASS**

| metric | value |
|---|---:|
| rounds | 72 |
| KV sets / TS appends | 2 336 / 3 456 |
| KV / TS sector erases | 663 / 742 |
| KV / TS remounts (all verified) | 18 / 18 |
| TS live window after the run | 223 entries, contiguous, exact payloads |
| verification failures / controller errors | **0 / 0** |
| elapsed | 243 794 503 cycles = 60.95 s |
| stack high-water | 3 536 B |

```
RESULT kind=longrun rounds=72 kv_sets=2336 ts_appends=3456 ts_time=4456 query_count=223 kv_remounts=18 ts_remounts=18 elapsed_cycles=243794503 budget_cycles=240000000 checksum=0xbd0c7185 errors=0 verify_ok=1 rc=0
BOARD core_hz=4000000 kv_erases=663 ts_erases=742 stack=3536 stack_limit=8192
```

Raw result lines are archived in `docs/acceptance/flash_*.txt`. Re-run:

```powershell
harness/firmware/u545re/run_flash_parity.ps1
harness/firmware/u545re/run_flash_fault.ps1 -Iterations 30 -Seed 99
harness/firmware/u545re/run_flash_reboot_soak.ps1
harness/firmware/u545re/run_flash_tests.ps1 -Test endurance -TimeoutSec 400
harness/firmware/u545re/run_flash_tests.ps1 -Test longrun -TimeoutSec 300
```
## How to run

Prerequisites: Rust with the `thumbv8m.main-none-eabi` target, `python3`, and one
flash backend — **probe-rs** (recommended, and what was verified) with pyOCD or
OpenOCD as alternatives. `arm-none-eabi-gcc` is **not** required for the Rust
image (it links with the `rust-lld` bundled in the Rust toolchain); the C
reference image does need it (the GNU Tools for STM32 bundled with STM32CubeIDE
are auto-detected by the PowerShell scripts).

Windows (PowerShell, verified on this machine):

```powershell
harness/firmware/u545re/run_rust.ps1                 # build + flash + run (Rust)
harness/firmware/u545re/run_c.ps1                    # build + flash + run (C reference)
harness/firmware/u545re/compare.ps1                  # both, side by side + verdict
harness/firmware/u545re/run_rust.ps1 -BuildOnly      # build only (no hardware)
harness/firmware/u545re/run_rust.ps1 -Profile size -Features small-crc
harness/firmware/u545re/compare.ps1 -Nightly         # smallest layout: nightly -Z build-std
harness/firmware/u545re/run_flash_tests.ps1 -Test endurance   # real internal-flash endurance
harness/firmware/u545re/run_flash_tests.ps1 -Test longrun     # real internal-flash DB long-run
```

Build/measurement knobs (shared by `run_rust.ps1` and `compare.ps1`):

| parameter | default | meaning |
|---|---|---|
| `-InlineThreshold N` | `50` | LLVM `-C llvm-args=-inline-threshold=N`; `-1` omits the flag (old flag-free baseline) |
| `-Nightly` | off | nightly `-Z build-std`, `panic=immediate-abort`, `-Z location-detail=none` |
| `-NoIcf` | off | disable LLD `--icf=all` (measurement only) |
| `-OptimizeForSize` | off | nightly `-Z build-std-features=optimize_for_size` (−80 B measured) |
| `-LtoO3` | off | LLD `--lto-O3` (−48 B measured; composes with the previous) |
| `-OutDir` | `build/u545re` | artifact directory (ELF names carry feature/threshold suffixes) |
| `-NoBuild` | off | reuse the existing ELF |

`-Nightly` builds the measured smallest layout: nightly toolchain,
`-Z build-std=core` at `-Oz`, `panic=immediate-abort`, LLD `--icf=all`,
`-Z location-detail=none`; it auto-installs `nightly` + `rust-src` when missing
and produces `fw_rust_u545re_size_nightly.elf`.

POSIX shells (same firmware, same output):

```sh
harness/firmware/u545re/run_rust.sh             # build + flash + run
harness/firmware/u545re/run_rust.sh --build-only
harness/firmware/u545re/run_rust.sh --no-build
harness/firmware/u545re/run_rust.sh --tool pyocd
scripts/fw_c.sh --board-u545                    # build the C board image (no QEMU)
```

Both board images use the `board-u545` feature / `-DFDB_BOARD_U545`, flash the
image, poll the `DONE` flag, read `BOARD_OUT` back over SWD and verify `rc=0`.
The internal-flash tests use their own `flash-endurance` / `flash-longrun`
features (`run_flash_tests.ps1`) and write their own `RESULT kind=...` lines.
Artifacts land in `harness/firmware/build/u545re/` and
`harness/firmware/build/c_u545/`: ELF, probe log, raw result buffer
(`board_out.bin`), decoded text (`fw_output.txt`) and, for `compare.ps1`, a
report (`build/u545re/board_compare*.txt`).

## What was fixed during bring-up and optimisation

1. **Result buffer was never initialised.** `.results` is a `NOLOAD` section, so
   the boot code does not copy or zero it and the `#[link_section = ".results"]`
   initializer is discarded: `BOARD_OUT.len` was whatever SRAM held after reset,
   so the firmware wrote its output past the end of the buffer and the host saw
   `done = DONE` with a garbage length. Fixed with `board::reset_output()` called
   before the first write.
2. **`core_hz` was hard-coded to 16 MHz.** That is the clock the QEMU firmware
   assumes, not the board's: the STM32U5 boots on MSIS at 4 MHz and this firmware
   never reconfigures the clock tree. The firmware now decodes the RCC registers
   and reports the real value (0 = unknown, host reports cycles only).
3. **Probe programming works with probe-rs.** Earlier OpenOCD/pyOCD attempts
   (on macOS, with the factory ST-LINK firmware) hung the probe on this board.
   probe-rs implements STM32U5 programming in the host tool instead of using the
   probe's flash loader, and flashes + verifies this image in ≈0.6 s. No ST-LINK
   firmware update was needed for that path.
4. **The C reference got the same board backend**, so the comparison is
   like-for-like: `harness/firmware/common/board_u545.c` implements the RAM
   result buffer, DWT counters and RCC clock decode for the C build
   (`scripts/fw_c.sh --board-u545`), mirroring the Rust `board-u545` feature.
5. **The Rust board backend was accidentally huge.** `#[inline(always)]` on the
   semihost shim cloned the `board::semihost` match + 4096-byte append loop into
   every `write_bytes()` call site, making the board image 840 B larger than the
   QEMU image (C: 392 B). `board::semihost` is now `#[inline(never)]`: board
   delta 400 B, ROM −440 B.
6. **Two crate stack fixes** found while chasing the stack metric: `Kvdb`'s
   64-slot name cache is now initialised in place instead of materialising a
   512 B `[CacheSlot::EMPTY; 64]` stack temporary (mount stack 944 → 488 B, C:
   696 B), and `Storage::write_status` no longer copies its 32 B scratch through
   a second buffer.
7. **`-z` inliner was the CPU blocker.** LLVM's `-Oz` inline threshold is too low
   for this call-heavy workload: `-C llvm-args=-inline-threshold=50` cuts TS CPU
   by 22 % and rollover by 21 % on silicon, and even shrinks the image by 84 B.
   Thresholds ≥56 cross an inlining cliff (+0.6…1.1 KiB ROM) and were rejected.
8. **Stack/ROM refactor** (this round): out-param `read_sector_info` /
   `read_kv_header` / `read_tsl` / `read_sec_into`, addr-only KV lookups instead
   of materialising the 88-byte `KvEntry`, single-pass `alloc_scan`, GC
   `move_kv` streaming the key hash, cold seal/recovery paths, and a `wg<=8`
   direct-byte `write_status`. Net: `text` −1 056 B / stack −408 B (QEMU
   phases), board ROM 14 572 → 13 228 B, board stack 1 456 → 1 080 B.
9. **Board + `stack-phases` diagnostic bug**: the per-phase `st_ts_append`
   diagnostic appended with a timestamp below `last_time()` (the rollover phase
   had already advanced it past 2 399), so a phases-instrumented board image
   failed with `rc=1`. It now uses `last_time()+1`; the QEMU RESULT is unchanged.
10. **Real internal-flash driver and test suite.** The board workload previously
    only used the RAM NOR simulator. `hwflash.rs` now drives the STM32U5 flash
    controller directly (non-secure register block, page erase, 128-bit
    quad-word programming, NSSR error checks) over the reserved scratch region
    (bank-1 pages 24..31). Five feature-gated test images use it: image parity
    (6/6 fields identical to the host model), power-loss injection (50/50
    externally-reset cycles recovered), 20-boot/19-reset soak, 240 s endurance
    (22 493 P/E cycles across 8 pages, 0 errors) and 60 s KVDB+TSDB long-run
    (2 336 KV sets, 3 456 appends, 1 405 erases, 0 errors).
11. **C-compat bug for `WRITE_GRAN > 8`.** The differential image matrix found
    that FlashDB-C encodes a status granule as `00 FF..FF` and decodes only its
    first byte, while Rust zeroed the whole granule and required all bytes to be
    zero: Rust read C images for wg 32/64/128/256 as empty and wrote
    non-interchangeable images. The private helpers in `low.rs` now match C
    byte-for-byte (1/8 unchanged); this also removed ~110 B from the board ROM
    and made the host matrix green for wg 1/8/32/64/128.
12. **`Kvdb::check()` false positive after a power cut.** The silicon
    power-loss test found that a torn `PRE_WRITE` record (incomplete header that
    recovery already quarantines as `ERR_HDR`) made `check()` return
    `ReadCorrupted` although mount and the data were fine. `check()` now skips
    exactly the states the sector walk skips for torn records and still reports
    real damage to committed records; `kv_powercut_check.rs` sweeps every flash
    op of a KV update at wg 8 and 128.


## Probe/tooling notes

* If a flash attempt ever leaves the probe wedged (`bulk transfer failed: pipe is
  stalled`, `Fail reading CTRL/STAT`), unplug/replug the board's USB cable;
  software resets of the probe are not always enough.
* Close applications that claim the ST-LINK USB device (debuggers, vendor
  programming tools, some chat apps) before flashing.
* The board's original demo firmware was erased during the first bring-up
  attempts; it now runs this harness image.

## Follow-up

* **Optional ROM margin** (not needed for acceptance): thin-pointer print labels
  in the Rust harness (~100–150 B measured budget). The two nightly knobs were
  re-measured on the final tree and are now **no-ops** for `.text` (12 352 B with
  or without `-Z build-std-features=optimize_for_size` + `--lto-O3`).
* **Qualification work** (lab, not dev board): PVT/voltage-margin endurance,
  retention/bake testing and a multi-hour/multi-day soak. The 240 s endurance
  and 60 s long-run are functional stress tests, not datasheet qualification.
* **Mid-erase reset injection**: the external debug reset is serviced after the
  flash controller finishes an erase, so no erase-window reset was observed even
  across 50 power-loss cycles. A deterministic on-chip watchdog/RAM-ISR reset is
  the next step for that specific window (traces already cover program windows).
* **Production-grade flash driver**: the test driver is single-bank,
  scratch-pages-only and omits RDP/WRP/option bytes, dual-bank, ECC
  retry/logging, suspend/resume and DMA; the crate itself is driver-agnostic.
* **CPU without hardware**: `scripts/fw_rust.ps1 -Cycles` runs the QEMU
  firmware with a SysTick + `-icount` instruction-tick proxy (40
  instructions/tick, deterministic, ratios meaningful; board ROM must come from
  a board build). Per-function stack frames:
  `harness/firmware/tools/stack_sizes.py <nightly-elf>`.
* Optional: run the workload at a configured high clock (PLL) for production-like
  wall-clock numbers. The cycle counts above already answer the CPU-cost
  question and are the clock-independent metric.
