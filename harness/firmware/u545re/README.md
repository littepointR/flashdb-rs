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
| ROM (text+rodata+data) | 13 360 B | **13 340 B** | **13 304 B** | PASS |
| static RAM (data+bss) | 1 072 B | **640 B** | **640 B** | PASS |
| stack high-water | 1 240 B | **1 080 B** | **1 080 B** | PASS |
| static + stack RAM | 2 312 B | **1 720 B** | **1 720 B** | PASS |
| cycles mount | 521 628 | **275 020** | **274 982** | PASS |
| cycles KV | 34 398 794 | **22 014 556** | **21 780 634** | PASS |
| cycles TS | 391 359 | **347 488** | **347 267** | PASS |
| cycles rollover | 2 554 345 | **2 087 865** | **2 086 645** | PASS |
| cycles total | 38 160 894 | **25 047 330** | **24 811 950** | PASS |
| 21 logical `RESULT` fields | — | identical | identical | PASS |
| both partition image checksums | — | identical | identical | PASS |

Every phase is faster than C (total CPU −34.4 % stable / −35.0 % nightly) while
the image is smaller (ROM −20 B / −56 B) and static+peak-stack RAM is 25.6 %
lower. The CPU-fastest `release` profile (`-RustProfile release
-RustFeatures small-crc`) is −43.0 % total CPU and 1 048 B stack, but its ROM is
15 264 B, so it passes every check except ROM.

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
RESULT text=12452 rodata=852 data=0 bss=640 sim_flash=32768 stack=1080 stack_init=424 stack_limit=8192 kv_writes=1596 kv_gets=64 kv_iterated=64 kv_checksum=0x4b3486a4 kv_iter_checksum=0x0e250daa ts_appends=200 ts_iterated=200 ts_checksum=0x10d90680 rc=0
RESULT ts_roll_appends=1200 ts_roll_erases=9 ts_roll_violations=0 ts_roll_count=392 ts_roll_first_time=2008 ts_roll_last_time=2399 ts_roll_checksum=0xb46ee0c0 ts_roll_remount_count=392 ts_roll_remount_checksum=0xb46ee0c0 ts_roll_continue_ok=1 kv_image_checksum=0x69f5868d ts_image_checksum=0x95091ba6
BOARD cycles_mount=274982 cycles_kv=21780634 cycles_ts=347267 cycles_roll=2086645 cycles_total=24811950 core_hz=4000000
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
| mount (KVDB + TSDB init) | 275 020 | 68.8 ms |
| KV workload (set/update/churn/get/iterate) | 22 014 556 | 5.50 s |
| TS workload (append/iterate) | 347 488 | 86.9 ms |
| rollover phase (1 200 appends + remount) | 2 087 865 | 522 ms |
| total | 25 047 330 | 6.26 s |

Cycle counts are clock-independent (DWT counts CPU cycles); the board is left on
its reset clock, so wall-clock time is dominated by the 4 MHz MSIS. Run-to-run
variation is ≤600 cycles (≈0.002 %) on `cycles_kv`/`cycles_total` — the host
polls the `DONE` flag over SWD while the workload runs. With a configured
160 MHz clock the same workload would take ≈156 ms.

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
| ROM (text+rodata+data) | 13 360 B | **13 340 B (−0.1 %)** | **13 304 B (−0.4 %)** | 15 264 B (+14.3 %) |
| static RAM (data+bss) | 1 072 B | **640 B (−40.3 %)** | **640 B (−40.3 %)** | **640 B (−40.3 %)** |
| stack high-water | 1 240 B | **1 080 B (−12.9 %)** | **1 080 B (−12.9 %)** | **1 048 B (−15.5 %)** |
| static + stack RAM | 2 312 B | **1 720 B (−25.6 %)** | **1 720 B (−25.6 %)** | **1 688 B (−27.0 %)** |
| cycles mount | 521 628 | **275 020 (−47.3 %)** | **274 982 (−47.3 %)** | **339 286 (−35.0 %)** |
| cycles KV | 34 398 794 | **22 014 556 (−36.0 %)** | **21 780 634 (−36.7 %)** | **18 957 543 (−44.9 %)** |
| cycles TS | 391 359 | **347 488 (−11.2 %)** | **347 267 (−11.3 %)** | **301 637 (−22.9 %)** |
| cycles rollover | 2 554 345 | **2 087 865 (−18.3 %)** | **2 086 645 (−18.3 %)** | **1 882 472 (−26.3 %)** |
| cycles total | 38 160 894 | **25 047 330 (−34.4 %)** | **24 811 950 (−35.0 %)** | **21 770 161 (−43.0 %)** |

Every workload, rollover and image field (21 in total: write/get/iterate counts,
three workload checksums, the rollover window/erases/recovery fields and both
partition image checksums) is identical between the two implementations in every
configuration.

Verdict:

* **ROM: parity achieved on silicon** — the Rust `.text` is still 1 284 B larger
  than C's, but the `small-crc` 64-byte table and shorter strings make `.rodata`
  1 284 B smaller, so the total is −20 B (stable) / −56 B (nightly).
* **Stack: parity achieved** — 1 080 B vs 1 240 B (the 1 048 B `release` build is
  even lower).
* **RAM: clearly ahead** — static RAM −40.3 %, static+stack −25.6 %.
* **CPU: every phase faster** — total −34.4 % stable / −43.0 % release.

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
`compare.ps1`; the rollover phase costs ≈2.1 M cycles (522 ms @ 4 MHz, i.e. 8.3 %
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

## Real internal-flash endurance and long-run tests (2026-10-23)

Two further board images exercise the **STM32U5 flash controller itself**
instead of the RAM NOR simulator:
`harness/firmware/u545re/run_flash_tests.ps1 -Test endurance|longrun` builds,
flashes and reads them back over SWD.

Driver (`harness/firmware/rust/hwflash.rs`): non-secure FLASH register block
(`0x4002_2000`, TZEN=0), page erase (8 KiB), 128-bit quad-word programming,
NSSR error flags checked after every operation. It only touches the scratch
pages 24..31 of bank 1 (`0x0803_0000..0x0803_FFFF`), far above the firmware
image, so a test can never erase code.

### Endurance (`-Test endurance`)

Every cycle: program a quad-word (every 16th cycle the full 8 KiB page =
512 quad-words), read back and compare, erase the page, verify the whole page
reads `0xFF`. Time-budgeted with DWT; default budget 120 s.

Measured on 2026-10-23 at the 4 MHz MSIS reset clock:

| metric | value |
|---|---:|
| P/E cycles (page erase + program) | **10 259** |
| quad-words programmed | 338 320 (≈2.7 MB) |
| bytes erased | ≈84 MB (10 259 × 8 KiB) |
| read-back / erase verification failures | **0** |
| controller errors (NSSR OPERR/PROGERR/WRPERR/PGAERR/SIZERR/PGSERR/OPTWERR) | **0** |
| elapsed | 480 223 433 cycles = 120.06 s |
| per P/E cycle | ≈11.7 ms (including verification) |
| `FLASH_ACR` / final `NSSR` / `ECCR` | `0x00000000` / `0x00000000` / `0x00000000` |
| stack high-water | 152 B |

```
RESULT kind=endurance pe_cycles=10259 programs=338320 elapsed_cycles=480223433 budget_cycles=480000000 checksum=0x7d7afb78 errors=0 verify_ok=1 erased_ok=1 rc=0
BOARD core_hz=4000000 acr=0x00000000 nssr=0x00000000 nscr=0x40000000 eccr=0x00000000 stack=152 stack_limit=8192 last_nssr=0x00000000
```

### Long-run (`-Test longrun`)

Runs the real `flashdb` crate on internal flash: KVDB 32 KiB (4 × 8 KiB
sectors) and TSDB 32 KiB (4 × 8 KiB sectors), `WRITE_GRAN_BITS = 128`.
Alternating rounds: KV rewrite of 32 keys × 32 B with full read-back compare;
48 TS appends × 32 B with rollover enabled, then the whole live window is
verified (contiguous timestamps, exact payloads). Both databases are remounted
every 4 rounds and at the end, and verified again. 60 s DWT budget.

Measured on 2026-10-23:

| metric | value |
|---|---:|
| rounds | 65 |
| KV sets / TS appends | 2 112 / 3 120 |
| KV / TS sector erases | 497 / 541 |
| KV / TS remounts (all verified) | 16 / 16 |
| TS live window after the run | 192 entries, contiguous, exact payloads |
| verification failures / controller errors | **0 / 0** |
| elapsed | 242 358 770 cycles = 60.59 s |
| stack high-water | 3 440 B |

```
RESULT kind=longrun rounds=65 kv_sets=2112 ts_appends=3120 ts_time=4120 query_count=192 kv_remounts=16 ts_remounts=16 elapsed_cycles=242358770 budget_cycles=240000000 checksum=0xf0f76d85 errors=0 verify_ok=1 rc=0
BOARD core_hz=4000000 kv_erases=497 ts_erases=541 stack=3440 stack_limit=8192
```

Both tests verify deterministically; the DWT time budget makes only the
cycle/round counts vary slightly between runs. Raw result lines are archived in
[`docs/acceptance/flash_endurance.txt`](../../../docs/acceptance/flash_endurance.txt)
and
[`docs/acceptance/flash_longrun.txt`](../../../docs/acceptance/flash_longrun.txt).
Re-run:

```powershell
harness/firmware/u545re/run_flash_tests.ps1 -Test endurance
harness/firmware/u545re/run_flash_tests.ps1 -Test longrun
harness/firmware/u545re/run_flash_tests.ps1 -Test endurance -BuildOnly
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
   phases), board ROM 14 572 → 13 340 B, board stack 1 456 → 1 080 B.
9. **Board + `stack-phases` diagnostic bug**: the per-phase `st_ts_append`
   diagnostic appended with a timestamp below `last_time()` (the rollover phase
   had already advanced it past 2 399), so a phases-instrumented board image
   failed with `rc=1`. It now uses `last_time()+1`; the QEMU RESULT is unchanged.
10. **Real internal-flash driver for endurance/long-run tests.** The board
    workload previously only used the RAM NOR simulator. `hwflash.rs` now drives
    the STM32U5 flash controller directly (non-secure register block, page erase,
    128-bit quad-word programming, NSSR error checks) over a reserved scratch
    region (bank-1 pages 24..31), and the two tests above exercise it on
    silicon — see the endurance (10 259 P/E cycles, 0 errors) and long-run
    (2 112 KV sets + 3 120 TS appends over internal flash, 1 038 erases, 0
    errors) results.

## Probe/tooling notes

* If a flash attempt ever leaves the probe wedged (`bulk transfer failed: pipe is
  stalled`, `Fail reading CTRL/STAT`), unplug/replug the board's USB cable;
  software resets of the probe are not always enough.
* Close applications that claim the ST-LINK USB device (debuggers, vendor
  programming tools, some chat apps) before flashing.
* The board's original demo firmware was erased during the first bring-up
  attempts; it now runs this harness image.

## Follow-up

* **Remaining optional ROM margin** (not needed for acceptance): thin-pointer
  print labels in the Rust harness (~100–150 B measured budget), nightly
  `-Z build-std-features=optimize_for_size` (−80 B) + LLD `--lto-O3` (−48 B).
  Both knobs are available on `run_rust.ps1`/`compare.ps1`
  (`-OptimizeForSize`, `-LtoO3`).
* **CPU without hardware**: `scripts/fw_rust.ps1 -Cycles` runs the QEMU
  firmware with a SysTick + `-icount` instruction-tick proxy (40
  instructions/tick, deterministic, ratios meaningful; board ROM must come from
  a board build). Per-function stack frames:
  `harness/firmware/tools/stack_sizes.py <nightly-elf>`.
* Internal-flash driver instead of the RAM NOR simulator — the remaining step to
  exercise the STM32U5 flash controller/erase-block behaviour on silicon.
* Optional: run the workload at a configured high clock (PLL) for production-like
  wall-clock numbers. The cycle counts above already answer the CPU-cost
  question and are the clock-independent metric.
