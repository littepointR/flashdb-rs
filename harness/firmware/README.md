# Bare-metal measurement harness (Cortex-M3 / QEMU)

This directory contains a bare-metal firmware harness that measures the **same
deterministic workload** twice, on an emulated ARM Cortex-M3:

* `C FlashDB` — the unmodified upstream C implementation in `reference/FlashDB`
  (FAL storage mode, `FDB_WRITE_GRAN=8`, KVDB + TSDB), and
* `Rust flashdb` — the Rust rewrite in `crates/flashdb` (real KVDB **and**
  TSDB, both over simulated NOR partitions).

Both images share the same harness (startup code, linker script, semihosting
console, stack painting, workload geometry), so the reported numbers are
directly comparable: **ROM** (`.text`/`.rodata`/`.data`), **static RAM**
(`.data`/`.bss`) and **worst-case stack high-water**. Everything runs in QEMU
(`qemu-system-arm -M mps2-an385`) and prints one machine-parsable line per run
through ARM semihosting (no UART driver):

```
RESULT text=11116 rodata=508 data=0 bss=624 sim_flash=32768 stack=1008 \
       stack_init=336 stack_limit=8192 kv_writes=1596 kv_gets=64 \
       kv_iterated=64 kv_checksum=0x4b3486a4 kv_iter_checksum=0x0e250daa \
       ts_appends=200 ts_iterated=200 ts_checksum=0x10d90680 rc=0
```

The same Rust firmware also runs on a **NUCLEO-U545RE-Q** (STM32U545RE,
Cortex-M33) with probe-rs for on-silicon verification: see
[`u545re/README.md`](u545re/README.md).

---

## Quick start

```bash
# C FlashDB baseline: build, print sections, run in QEMU
./scripts/fw_c.sh

# Rust rewrite: build with cargo (release profile), print sections, run in QEMU
./scripts/fw_rust.sh

# Variants (flags compose unless noted)
./scripts/fw_rust.sh size            # opt-level="z" + LTO "size" profile
./scripts/fw_rust.sh --phases        # diagnostic build: per-phase stack peaks
./scripts/fw_rust.sh --symbols       # symbol-level ROM/RAM attribution tables
./scripts/fw_rust.sh --small-crc     # 64-byte nibble CRC table
./scripts/fw_rust.sh size --phases --symbols
./scripts/fw_c.sh --phases           # same per-phase diagnostic run for C
./scripts/fw_c.sh --symbols          # C symbol attribution + per-object sizes
./scripts/fw_c.sh --build-only       # build + sizes only, no QEMU
./scripts/fw_c.sh --clean            # remove build products (both scripts)
```

Windows/PowerShell (same firmware, same numbers — see also
[`u545re/README.md`](u545re/README.md)):

```powershell
pwsh -NoProfile -File scripts/fw_rust.ps1 -Profile size -SmallCrc -Phases
pwsh -NoProfile -File scripts/fw_rust.ps1 -Profile size -SmallCrc -Cycles   # QEMU cycle proxy
pwsh -NoProfile -File scripts/fw_rust.ps1 -Profile release -BuildOnly
pwsh -NoProfile -File scripts/fw_rust.ps1 -InlineThreshold -1               # flag-free A/B
```


Exit status is 0 only when the firmware actually ran and printed a `RESULT`
line with `rc=0`; a hang (hard fault) or a workload failure makes the script
fail with the QEMU log.

`--phases` builds are **diagnostic only**: they add the per-phase stack
instrumentation and therefore extra ROM (see the tables below). The default
build is the one that produces the headline numbers.

Build products (git-ignored) land in `harness/firmware/build/`:

```
build/
├── c/
│   ├── obj/*.o                 # one object per source
│   ├── fw_c.elf                # C firmware (baseline)
│   ├── fw_c.map / qemu.log
└── rust/
    ├── target/...              # cargo target dir (stand-alone workspace)
    ├── fw_rust_release.elf     # baseline, release profile
    ├── fw_rust_size.elf        # opt-level="z" profile
    ├── fw_rust_release_phases.elf / ..._size_phases.elf
    ├── cargo.log / qemu.log
```

### Verified environment

| tool | version / path |
|---|---|
| `arm-none-eabi-gcc` | GCC 11.3 (GNU Tools for STM32 bundled with STM32CubeIDE 1.13.2 on Windows); GCC 16.1.0 on the original macOS authoring machine |
| `qemu-system-arm` | QEMU 8.2.0 (Windows), QEMU 11.0.0 (macOS) |
| `rustc` / `cargo` | 1.99.0 with `thumbv7m-none-eabi` installed (nightly for the `-Nightly` layout) |
| host | Windows 11 / PowerShell 7 (verified this round), macOS arm64 |

Do **not** link with the host linker: it lacks `--gc-sections` and does not
understand the bare-metal linker script. Both builds drive
`arm-none-eabi-ld` through `arm-none-eabi-gcc` (Rust sets
`CARGO_TARGET_THUMBV7M_NONE_EABI_LINKER=arm-none-eabi-gcc`); on Windows the
scripts auto-detect the CubeIDE toolchain.

### QEMU machine choice

Both Cortex-M3 machines were tested and boot a semihosting binary here:
`mps2-an385` and `lm3s6965evb`. The harness uses **`mps2-an385`**: 4 MiB RAM and
4 MiB of writable code SRAM (verified with a write/read probe), comfortably
fitting the databases, the simulated NOR flash and the 8 KiB painted stack.

---

## What is measured, and how

### Memory map (`common/mps2_an385.ld`)

| region | address | use |
|---|---|---|
| `CODE` | `0x00000000`, 1 MiB | vector table, `.text`, `.rodata`, `.data` load image |
| `SIMFLASH` | `0x00100000`, 32 KiB | simulated external NOR (`.sim_flash`, `NOLOAD`) |
| `RAM` | `0x20000000`, 4 MiB | `.data`, `.bss`, 8 KiB stack at the top |

The simulated NOR backing store lives in its own memory region and is
`NOLOAD`: it models an external flash chip, is **not** part of ROM and is **not**
counted in `.data`/`.bss`. Both firmwares erase it to `0xFF` at boot like a
factory-fresh part.

### Section sizes (`arm-none-eabi-size -A`)

The scripts print the authoritative ELF breakdown; the firmware also reports
the same numbers in `RESULT`, computed from linker-defined symbols
(`__text_start/__text_end`, `__rodata_*`, `__data_*`, `__bss_*`,
`__sim_flash_*`) so both sources cross-check.

| field | meaning |
|---|---|
| `text` | `.text` = vector table + code (function sections, `--gc-sections`) |
| `rodata` | `.rodata` = string literals, CRC table, constant tables |
| `data` | `.data` size (also contributes to ROM as its load image) |
| `bss` | `.bss` = zero-initialised static RAM |
| `sim_flash` | simulated NOR region (not MCU RAM, not ROM) |
| ROM total | `text + rodata + data` |
| static RAM | `data + bss` |
| `stack` | worst-case stack high-water, see below |
| `stack_init` | high-water right after database init (= mount/init phase) |
| `stack_limit` | painted stack region (8 KiB) |

### Stack high-water

`common/startup_armv7m.S` paints the whole 8 KiB stack region with `0xA5A5A5A5`
at reset and again immediately before `bl main` (so boot code is not
attributed to the workload). `main()` records `stack_init` after database
initialisation and `stack` after the whole workload. `stack_high_water_bytes()`
(`common/stack.c`, mirrored in Rust) scans from `_stack_bottom` upward for the
lowest word that no longer holds the pattern; the distance to `_estack`,
rounded up to 8 bytes, is the peak. Because the stack is painted only at boot,
`stack` is the maximum over init + workload.

#### Per-phase attribution (`--phases`)

To find *where* the peak comes from, the `--phases` diagnostic build adds
`stack_repaint()`: it fills only the unused stack below the current SP with
`0xA5`, runs exactly one phase, and scans again. The measured peak is that
phase plus the live caller frames. Between phases the flash contents are left
as the previous phases made them (the workload has already completed once), so
the KVDB still contains all 64 keys when `get`/`iterate`/`GC` are measured.

`RESULT` then also reports `st_set`, `st_get`, `st_iter`, `st_gc`
(mount = `stack_init`). The code is behind the `stack-phases` cargo feature /
`-DHARNESS_STACK_PHASES` macro so it never affects the baseline image.

Caveats:

* A frame word equal to `0xA5A5A5A5` is invisible; it can only under-report.
* Per-phase numbers include the diagnostic function's own frame (~100–300 B),
  identically in both firmwares, so compare them against each other rather
  than against the headline `stack`.
* Repainting erases earlier phases' marks, so the diagnostic run is a separate
  build from the headline run — never mix the two `stack` values.
* Interrupt handlers are never enabled; ISR stack usage is not included.
* Usage beyond 8 KiB would run below `_stack_bottom` and corrupt memory rather
  than be reported.
* `stack_init`/`stack` include harness driver code; the two firmwares run the
  same workload shape but not byte-identical driver code.

### Semihosting

`common/semihost.c` uses ARM semihosting (`BKPT 0xAB`): `SYS_WRITE0` (0x04) for
NUL-terminated strings and `SYS_EXIT` (0x18) with
`ADP_Stopped_ApplicationExit`. QEMU must run with `-semihosting` (the scripts
do). The Rust side uses the same calls via `core::arch::asm!`.

Both print paths are symmetric and run **after** the final stack scan, so
semihosting/formatting code is never charged to the measured stack. The decimal
formatters avoid division so the harness itself does not pull in
`__aeabi_uidiv`/`__aeabi_uldivmod`.

### Workload (`common/workload_spec.h`, mirrored in `rust/main.rs`)

Deterministic: no RNG, no real clock, no host input. Two consecutive runs print
identical checksums.

* **KVDB** (16 KiB partition = 4 × 4 KiB sectors):
  * 64 × `set` — keys `kv.00`…`kv.3f`, 24-byte values
  * 32 × update (keys 0–31)
  * 1500 × `set` churn over 16 keys — forces several sector GC cycles
  * 64 × `get` + full value verification, checksummed in key-index order
  * full iteration (different code path), checksummed in iteration order
* **TSDB**: 200 × append with deterministic 16-byte payloads and fake
  timestamps, then full iteration + checksum.
  * C: real `fdb_tsl_append_with_ts` / `fdb_tsl_iter`.
  * Rust: real `Tsdb::mount` / `append` / `iter` over a second 16 KiB
    simulated NOR partition. The payload stream is byte-identical to C, so
    `ts_checksum` is directly comparable (currently equal: `0x10d90680`).

`kv_checksum` is computed over values read back in key-index order and is
**implementation independent**: C and Rust produce the same value
(`0x4b3486a4`), which validates the Rust crate's on-flash format and read path.
`kv_iter_checksum` covers `(name, value)` in *iteration* order; C and Rust
iterate in different physical orders today, so it is reported separately.
`ts_checksum` matches between the two firmwares.

---

## C firmware

`scripts/fw_c.sh` compiles, unmodified:

* `reference/FlashDB/src/{fdb,fdb_kvdb,fdb_tsdb,fdb_utils}.c`
* `reference/FlashDB/port/fal/src/{fal,fal_flash,fal_partition}.c`

plus the harness, with:

```
-mcpu=cortex-m3 -mthumb -Os -std=gnu11
-ffunction-sections -fdata-sections -fno-common
-fno-builtin -fno-stack-protector -ffreestanding
-fno-asynchronous-unwind-tables -fno-unwind-tables
-DFDB_WRITE_GRAN=8
-nostdlib -nostartfiles -Wl,--gc-sections -T common/mps2_an385.ld -lgcc
```

Configuration headers (`c/include/`):

* `fdb_cfg.h` — `FDB_USING_KVDB`, `FDB_USING_TSDB`, `FDB_USING_FAL_MODE`,
  `FDB_WRITE_GRAN=8`, `FDB_PRINT(...)` compiled out (release firmware with
  logging disabled; otherwise printf would pull in a libc).
* `fal_cfg.h` — static partition table: `fdb_kvdb1` 16 KiB @ 0,
  `fdb_tsdb1` 16 KiB @ 16 KiB on `norflash0`; `FAL_PRINTF` compiled out.

The Homebrew `arm-none-eabi-gcc` ships only GCC's freestanding headers, so
`c/include/` also provides minimal `stdio.h`, `string.h`, `stdlib.h`,
`inttypes.h`, `time.h` shims, and `c/libc_stubs.c` implements the handful of
libc functions FlashDB/FAL use. `c/fal_ram_sim.c` is the FAL NOR driver over
the `.sim_flash` region (program = bitwise AND, erase = `0xFF` on 4 KiB
blocks).

The script prints per-object sizes of the FlashDB/FAL objects before
`--gc-sections` as a library-only ROM upper bound; `--symbols` adds the full
symbol table and per-object report.

## Rust firmware

`harness/firmware/rust/` is a stand-alone cargo workspace (empty `[workspace]`
table so the repo workspace is untouched) whose only dependency is the real
`crates/flashdb` crate. It builds for `thumbv7m-none-eabi` with:

```bash
cargo rustc --bin fw-rust --target thumbv7m-none-eabi --offline --release -- \
    -C link-arg=-Tcommon/mps2_an385.ld \
    -C link-arg=-nostartfiles -C link-arg=-nostdlib \
    -C link-arg=-Wl,--gc-sections -C link-arg=-lgcc
```

Build profiles and codegen knobs:

* `release`: `opt-level="s"`, `lto=true`, `codegen-units=1`, `panic="abort"`.
* `size`: `opt-level="z"`, `lto=true` (`./scripts/fw_rust.sh size`).
* `-InlineThreshold 50` (default; `-1` = off): LLVM
  `-C llvm-args=-inline-threshold=50`. The `-Oz` default inliner is too
  conservative for the flashdb call chains: this flag is −84 B ROM and
  −22 % TS / −21 % rollover CPU on silicon. See `docs/OPTIMIZATION.md`.
* `-Icf`: LLD `--icf=all` (GNU ld has no ICF; the QEMU scripts switch to
  rust-lld when this knob is used). Board links always use it (−28 B).
* Harness features (off by default, for diagnostics/measurement only):
  `stack-phases` (`--phases`), `qemu-cycles` (`-Cycles`, QEMU SysTick +
  `-icount` cycle proxy) and `small-crc` (`--small-crc`/`-SmallCrc`,
  passthrough of the crate's 64-byte nibble CRC table).
* The nightly path (`scripts/fw_rust.ps1 -Nightly` / `board_common.ps1
  -Nightly`) adds `-Z build-std=core`, `panic="immediate-abort"`,
  `-Z location-detail=none` and `--icf=all`; it produces the smallest image.
* Both databases use the crate's placement mounts (`Kvdb::mount_into`,
  `Tsdb::mount_into`), so they are built directly in `.bss` without a stack
  copy of the database structs.
* LTO is required for dead-code elimination on stable: rustc does not expose
  per-function sections, so without LTO the whole crate (including the unused
  TSDB module) is kept in one `.text` section and cannot be garbage-collected.

### Harness-side size work (all in the firmware crate, not in `crates/`)

1. **Tiny strong `__aeabi_*`/mem replacements.** On ARM, LLVM lowers
   `memcpy`/`memset`/`memmove` intrinsics to `__aeabi_*` calls, which by
   default resolve to compiler-builtins' generic implementations (~1.5 KiB:
   `memcpy` 744 B, `__aeabi_memset` 220 B, `__aeabi_memset4` 184 B,
   `__aeabi_memclr4` 174 B, `__aeabi_memcpy4` 156 B, ...). The firmware
   defines strong `memcpy`/`memset`/`memmove`/`memcmp`/`strlen`/`strcmp`/... and
   `__aeabi_mem{cpy,move,set,clr}{,4,8}` functions; compiler-builtins' are weak,
   so the tiny ones win. Measured on the same crate state: release ROM
   14,244 B → 12,660 B (**−1,584 B**), size ROM 13,740 B → 12,148 B
   (**−1,592 B**). The C baseline links an equivalent tiny libc stub set, so
   this is apples-to-apples.
2. **Minimal panic handler** (`fn panic(_) -> ! { loop {} }`). No
   `core::fmt`/formatting machinery is linked: the only remaining panic-related
   symbols are three tiny trampolines (`panic_fmt` 6 B, `panic_bounds_check`
   8 B, `panic_const_rem_by_zero` 8 B) that branch to the handler. The C
   baseline's `FDB_ASSERT` is the equivalent `while (1);`.
3. **Out-of-line NOR driver + mem helpers.** The `flashdb::Flash` implementation
   over the `.sim_flash` region calls the tiny mem functions instead of
   duplicating volatile byte loops at every monomorphised call site (release
   −244 B; at opt-level="z" it is +64 B, a documented trade-off; the release
   profile keeps it).
4. **Per-phase diagnostics behind a feature** (`stack-phases`) so the
   attribution instrumentation never inflates the baseline image.
5. **Phase boundaries marked `#[inline(never)]`** so LTO cannot union the
   workload phases' locals into one giant `main` frame: release stack
   2,312 B → 2,000 B (**−312 B**) for +16 B of ROM.
6. **Error-plumbing / loop de-duplication + table-driven RESULT printing**
   (the "harness ROM pass"). One cold `fail()` path with shared stage tags and
   `ok_or_fail()` instead of per-site `match`/`fail` sequences; shared
   `kv_put()` / `kv_get_verified()` helpers instead of three inlined KV set
   loops; `#[repr(C)] Metrics` printed by one loop over a label table instead
   of 16 expanded `write_bytes`+`write_u32` pairs. Measured against a frozen
   crate snapshot (HEAD harness vs final harness, both `--profile size`):
   `.text` 14,524 → 14,324 B, `.rodata` 1,392 → 1,520 B, ROM 15,916 → 15,844 B
   (−72 B); release profile 17,464 → 17,340 B (−124 B); stack unchanged,
   checksums unchanged. Modest because ~2.5 KiB of the reported "harness"
   bucket is crate mount/append code that LTO inlined into the harness
   wrappers (measured by forcing those calls through opaque `black_box`
   function pointers: harness bucket 6,456 → 3,880 B, crate 7,396 → 10,434 B,
   total ROM +476 B). Forcing the separation is therefore not a ROM win; the
   right fix is crate-side de-inlining.
7. **Second harness ROM pass (task-7).** Compact decimal formatter (constant
   division by 10 → multiply-high on Cortex-M3, no libgcc helper, no
   powers-of-ten table), merged the initial+update KV loops, pointer-based TS
   payload generation, four shared stage tags instead of seven, smaller
   formatter buffer. Result on the frozen crate: `.text` 13,096 → 13,016 B,
   `.rodata` 1,520 → 1,448 B, ROM 14,616 → **14,464 B** (`-z`); `-s` 16,156 →
   **16,024 B**; stack/RAM unchanged, checksums unchanged. Genuine-harness
   estimate (opaque-boundary method) 3,080 → **2,452 B** (`-z`) and 3,030 →
   2,752 B (`-s`). Negative results: forcing crate calls out of line
   (`black_box` barriers on `set`/`append`/both `mount_into`) gives genuine
   harness 2,452 B but grows overall ROM by 604 B (`-z`) / 328 B (`-s`), so it
   stays measurement-only; a plain `#[inline(never)]` wrapper around the
   crate mount does not change inlining; inlining the single-use get/verify
   helper grew ROM +12 B and stack +32 B (reverted); `thin` LTO and
   `codegen-units=16` were both worse. `--phases`/`--symbols`/`--small-crc`
   stay feature-gated so the baseline image is untouched.

The firmware provides `_start`-style startup from the shared
`common/startup_armv7m.S` via `core::arch::global_asm!(include_str!(...))`, the
panic handler, semihosting output, the stack scan, and the small `Flash`
implementation.

**Why not `flashdb-sim::RamFlash` in the firmware?** `RamFlash<SIZE, WG>` owns
its `[u8; SIZE]` buffer by value. In a bare-metal image a 16 KiB `static` with
a non-zero initialiser lands in `.data` (16 KiB of *ROM*) and in the static-RAM
metric — neither is acceptable for a footprint measurement, and it would
misrepresent external flash as MCU RAM. The firmware therefore implements the
`flashdb::Flash` trait over a `#[link_section = ".sim_flash"]` static, exactly
like the C side's FAL simulator. `flashdb-sim` remains the simulator for host
tests/benchmarks.

### TSDB integration (done)

The Rust firmware uses the real TSDB crate over a **second 16 KiB `.sim_flash`
partition** (KV at `0x00100000`, TS at `0x00104000`; the linker region is
32 KiB). The TSDB object lives in `.bss` like the KVDB:

```rust
let flash = ts_flash();                       // SimFlash over TS_SIM_FLASH
flash.erase_all();                            // factory-fresh NOR
let mut slot = MaybeUninit::<Tsdb<_>>::uninit();
Tsdb::mount_into(&mut slot, flash, Config::new(4096, 16384), 32)?;
let db = unsafe { slot.assume_init_mut() };
db.set_rollover(false);
// 200 x append(1000 + i, 16-byte deterministic payload)
// then forward iteration + read_blob + checksum
```

`max_len` is 32 (C uses 64; it only bounds accepted payload sizes — the
on-flash format is identical). Rollover is disabled as requested for the
benchmark; with 200 × 16 B payloads the 16 KiB partition is nowhere near full
so it does not change behaviour. The `ts_appends`, `ts_iterated` and
`ts_checksum` accounting is unchanged, and the forward iterator reproduces
C's payload order exactly (equal checksum).

`--phases` also measures the TSDB phases (`st_ts_append`, `st_ts_iter`, see
below). Both databases are erased and mounted on a fresh simulated NOR at
boot, exactly like the C firmware's first-boot path.

---

## Measurement results

Current working tree, measured 2026-10-23. The headline numbers use the
acceptance configuration: `size` (`-z`) or `release` (`-s`) profile with
`small-crc`, LLD `--icf=all` and `-C llvm-args=-inline-threshold=50` (the
default of `scripts/fw_rust.ps1`; pass `-InlineThreshold -1` to reproduce the
old flag-free baseline). All numbers are whole-image sections from
`arm-none-eabi-size -A`, cross-checked against the linker-symbol values in the
`RESULT` line. `sim_flash` (32 KiB simulated external NOR) is never counted as
ROM or MCU RAM.

### Headline: ROM, static RAM, stack

| metric | C (FlashDB+FAL, `-Os`) | Rust `-s` | Rust `-z` |
|---|---|---|---|
| `.text` | 9720 | 12836 | 11116 |
| `.rodata` | 1632 | 508 | 508 |
| `.data` | 56 | 0 | 0 |
| **ROM total** (`text+rodata+data`) | **11408** | **13344** | **11624** |
| `.bss` | 1008 | 624 | 624 |
| **static RAM** (`data+bss`) | **1064** | **624** | **624** |
| **stack high-water** | **1192** | **1008** | **1008** |
| **total RAM** (static+stack) | **2256** | **1632** | **1632** |
| stack after init (= mount phase) | 696 | 296 | 336 |
| stack limit | 8192 | 8192 | 8192 |
| simulated NOR (excluded) | 32768 | 32768 | 32768 |
| KV checksum / iter checksum | `0x4b3486a4` / `0x0e250daa` | same | same |
| TS checksum | `0x10d90680` | same | same |

The QEMU `-z` image is still +216 B ROM over C because its `.text` is larger;
the `.rodata` advantage is smaller than on the board. **On real silicon the
acceptance passes** (ROM −28 B stable / −64 B nightly) — see
[`u545re/README.md`](u545re/README.md); the board is the normative footprint
target because it includes the rollover workload and the board backend.

### Default-CRC reference (continuity)

| metric | Rust `-s` default CRC | Rust `-z` default CRC |
|---|---|---|
| `.text` | 12712 | 11256 |
| `.rodata` | 1468 | 1468 |
| **ROM total** | **14180** | **12724** |
| `.bss` / static RAM | 624 / 624 | 624 / 624 |
| stack high-water | 1008 | 1008 |

`small-crc` saves 836 B (`-s`) / 1100 B (`-z`) of the QEMU image versus the
default CRC table, at ~2× CRC32 CPU cost; stack/RAM are unchanged and all
checksums stay identical.

### Per-phase stack (`--phases` builds, threshold-50 codegen)

| phase | C | Rust `-s` | Rust `-z` |
|---|---|---|---|
| mount/init (`stack_init`) | 752 | 320 | 360 |
| one KV set | 736 | 608 | 688 |
| one KV get | 720 | 632 | 728 |
| full KV iteration | 560 | 656 | 760 |
| KV GC churn (1500 writes) | 1248 | 976 | 1056 |
| one TS append | 528 | 496 | 568 |
| full TS iteration | 568 | 656 | 768 |
| headline `stack` (non-diagnostic build) | 1192 | 1008 | 1008 |

Diagnostic-build ROM: C 12032, Rust `-s` 15144, Rust `-z` 13044.
Per-phase numbers include each firmware's diagnostic frame (~100–300 B,
present on both sides); compare deltas, not absolutes. The headline stack
values come from the non-diagnostic build.

### ROM attribution (board nightly ELF, generated by `--symbols`)

| bucket | bytes | share |
|---|---|---|
| flashdb crate | 7754 | 61.8 % |
| harness (`fw_rust`) | 4538 | 36.2 % |
| toolchain mem/aeabi | 138 | 1.1 % |
| outliner stubs | 108 | 0.9 % |
| `.text` (size -A) | 12444 | 100 % |

Largest symbols: `kv_phase` 764, `kv_mount` 640, `gc_collect_by_free_size` 616,
TS `mount_into` 548, `move_kv` 436, TS `read_sec_into` 432, `alloc_kv` 416,
`kv_put` 352, `main` 352. The harness bucket includes crate mount/append bodies
inlined by LTO into the workload wrappers, so treat it as an upper bound.

### What is harness-side vs crate-side

* **Static RAM**: Rust wins clearly (624 vs 1064 B QEMU; 640 vs 1072 B board).
* **Stack**: Rust wins (1008 vs 1192 B QEMU; 1072 vs 1240 B board).
* **ROM**: board parity achieved; QEMU `-z` is +216 B. The residual is `.text`
  (crate safe code, error plumbing, monomorphisation), which on the board is
  offset by the smaller `.rodata` (64-byte CRC table + shorter labels).
* **CPU**: the board acceptance is faster than C in every phase (total −34.6 %
  stable `size`, −43.0 % `release`). The QEMU cycle proxy
  (`scripts/fw_rust.ps1 -Cycles`, SysTick + `-icount`, 40 instructions/tick,
  deterministic) is available for A/B work without hardware; use ratios, not
  absolutes.

### Reproducibility

C compilation is byte-for-byte reproducible. Rust LTO/codegen layout can vary
by a few tens of bytes of `.text` between rebuilds (checksums and section
classes do not change). The tables are from clean builds on the current tree;
re-run both scripts after any crate change. On Windows, `scripts/fw_rust.ps1`
exposes the same knobs (`-Profile`, `-SmallCrc`, `-Phases`, `-Symbols`,
`-BuildOnly`, `-InlineThreshold`, `-Cycles`).

---
## Troubleshooting

* **Hang / no `RESULT` line**: usually a hard fault or QEMU started without
  `-semihosting`. The scripts print the full QEMU log and fail. A Rust panic
  now just spins in the panic handler, so it shows up as the QEMU timeout.
* **`Rust target thumbv7m-none-eabi is not installed`**: run
  `rustup target add thumbv7m-none-eabi`.
* **`cargo` wants the network**: the firmware crate has only a path
  dependency; the script always passes `--offline`. If the crate changes,
  rebuild with `--clean` if cargo's fingerprint looks stale.
* **`arm-none-eabi-gcc: stdio.h: No such file`**: the Homebrew toolchain ships
  no newlib headers; the shims in `c/include/` provide the few FlashDB needs.
* **Symbol report empty / odd buckets**: `--symbols` uses `arm-none-eabi-nm`
  and needs `python3`; LTO moves some crate code into the harness's `main`, so
  treat buckets as indicative (see note in the report).
