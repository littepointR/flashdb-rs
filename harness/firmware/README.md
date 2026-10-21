# Bare-metal measurement harness (Cortex-M3 / QEMU)

This directory contains a bare-metal firmware harness that measures the **same
deterministic workload** twice, on an emulated ARM Cortex-M3:

* `C FlashDB` — the unmodified upstream C implementation in `reference/FlashDB`
  (FAL storage mode, `FDB_WRITE_GRAN=8`, KVDB + TSDB), and
* `Rust flashdb` — the Rust rewrite in `crates/flashdb` (KVDB; the Rust TSDB
  API is not wired into this harness yet, see below).

Both images are built with a small shared harness (startup code, linker script,
semihosting console, stack painting) so the reported numbers are directly
comparable: **ROM** (`.text`/`.rodata`/`.data`), **static RAM**
(`.data`/`.bss`), and **worst-case stack high-water**.

The harness runs entirely in QEMU (`qemu-system-arm -M mps2-an385`) and prints
one machine-parsable line per run through ARM semihosting (no UART driver):

```
RESULT text=9720 rodata=1632 data=56 bss=1008 sim_flash=32768 stack=1192 \
       stack_init=696 stack_limit=8192 kv_writes=1596 kv_gets=64 \
       kv_iterated=64 kv_checksum=0x4b3486a4 kv_iter_checksum=0x0e250daa \
       ts_appends=200 ts_iterated=200 ts_checksum=0x10d90680 rc=0
```

---

## Quick start

```bash
# C FlashDB baseline: build, print sections, run in QEMU
./scripts/fw_c.sh

# Rust rewrite: build with cargo (release profile), print sections, run in QEMU
./scripts/fw_rust.sh

# Optional variants
./scripts/fw_c.sh --build-only
./scripts/fw_c.sh --clean
./scripts/fw_rust.sh size           # opt-level="z" + LTO "size" profile
./scripts/fw_rust.sh --build-only
./scripts/fw_rust.sh --clean
```

Exit status is 0 only when the firmware actually ran and printed a `RESULT`
line with `rc=0`; a hang (hard fault) or a workload failure makes the script
fail with the QEMU log to aid debugging.

Build products (ignored by git) land in `harness/firmware/build/`:

```
build/
├── c/
│   ├── obj/*.o          # one object per source
│   ├── fw_c.elf         # C firmware
│   ├── fw_c.map         # linker map
│   └── qemu.log         # last QEMU output
└── rust/
    ├── target/...       # cargo target dir (stand-alone workspace)
    ├── fw_rust_release.elf
    ├── fw_rust_size.elf # after `./scripts/fw_rust.sh size`
    ├── cargo.log
    └── qemu.log
```

### Verified environment

The harness was developed and verified on this machine:

| tool | version / path |
|---|---|
| `arm-none-eabi-gcc` | GCC 16.1.0, `/opt/homebrew/bin` (binutils 2.46) |
| `qemu-system-arm` | QEMU 11.0.0, `/opt/homebrew/bin` |
| `rustc` / `cargo` | 1.96.1 with `thumbv7m-none-eabi` installed |
| host | macOS arm64 |

Do **not** link with the host Apple linker: it lacks `--gc-sections` and does
not understand the bare-metal linker script. Both builds drive
`arm-none-eabi-ld` through `arm-none-eabi-gcc` (Rust sets
`CARGO_TARGET_THUMBV7M_NONE_EABI_LINKER=arm-none-eabi-gcc`).

### QEMU machine choice

Both Cortex-M3 machines were tested and boot a semihosting binary here:
`mps2-an385` and `lm3s6965evb`. The harness uses **`mps2-an385`** because it
provides 4 MiB of RAM and 4 MiB of code SRAM (writes to the code region were
verified), comfortably fitting the databases, the simulated NOR flash and the
8 KiB painted stack. `lm3s6965evb` (64 KiB SRAM / 256 KiB flash) would also fit
the current partitions but leaves little headroom.

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
factory-fresh part. (The C side uses FAL partitions over this region; the Rust
side implements `flashdb::Flash` directly over it.)

### Section sizes (`arm-none-eabi-size -A`)

The scripts print the authoritative ELF breakdown and the firmware also reports
the same numbers in the `RESULT` line, computed from linker-defined symbols
(`__text_start/__text_end`, `__rodata_*`, `__data_*`, `__bss_*`,
`__sim_flash_*`) so both sources can be cross-checked. Definition of the
reported fields:

| field | meaning |
|---|---|
| `text` | `.text` = vector table + code (per-function sections, `--gc-sections`) |
| `rodata` | `.rodata` = string literals / constant tables |
| `data` | `.data` size (also contributes to ROM as its load image) |
| `bss` | `.bss` = zero-initialised static RAM |
| `sim_flash` | size of the simulated NOR region (not MCU RAM, not ROM) |
| ROM total | `text + rodata + data` |
| static RAM | `data + bss` |
| `stack` | worst-case stack high-water, see below |
| `stack_limit` | painted stack region (8 KiB) |

### Stack high-water

`common/startup_armv7m.S` fills the whole 8 KiB stack region with
`0xA5A5A5A5` in a register-only loop twice: once at reset and again
immediately before `bl main` (so the boot code's own stack usage is not
attributed to the workload). `main()` records `stack_init` after database
initialisation, then runs the workload. After both phases return to a shallow
frame, `stack_high_water_bytes()` (`common/stack.c`, mirrored in Rust) scans
from `_stack_bottom` upward for the lowest word that no longer holds the
pattern; the distance to `_estack`, rounded up to 8 bytes, is the reported
`stack` value. Because the stack is only painted once, the final value is the
maximum of init and workload.

Caveats:

* A frame word that happens to contain exactly `0xA5A5A5A5` is invisible; this
  can only under-report, never over-report.
* The scan runs after the workload returned; the deepest frame is not popped,
  so its marks remain.
* Interrupt handlers are never enabled in this firmware, so ISR stack usage is
  not included. Add a deliberate interrupt if that is ever needed.
* Usage beyond 8 KiB would run below `_stack_bottom` and corrupt memory rather
  than being reported — `stack` approaching `stack_limit` is a failure mode.
* `stack_init`/`stack` include the harness driver code; the two firmwares run
  the same workload shape but not byte-identical driver code, so compare trends
  and library-attributable differences rather than single bytes.

### Semihosting

`common/semihost.c` uses ARM semihosting (`BKPT 0xAB`): `SYS_WRITE0` (0x04) for
NUL-terminated strings and `SYS_EXIT` (0x18) with
`ADP_Stopped_ApplicationExit`. QEMU must run with `-semihosting` (the scripts
do). No UART or clock device is needed. The Rust side uses the same calls via
`core::arch::asm!`.

The decimal formatters deliberately avoid division so the harness itself does
not change the link set (whether `__aeabi_uidiv`/`__aeabi_uldivmod` from
libgcc/compiler-builtins are needed is decided by the library under test).

### Workload (`common/workload_spec.h`, mirrored in `rust/main.rs`)

Deterministic: no RNG, no real clock, no host input. Two consecutive runs print
identical checksums.

* **KVDB** (16 KiB partition = 4 × 4 KiB sectors):
  * 64 × `set` — keys `kv.00`…`kv.3f`, 24-byte string values
  * 32 × update (keys 0–31)
  * 1500 × `set` churn over 16 keys — forces several sector GC cycles
  * 64 × `get` + full value verification, checksummed in key-index order
  * full iteration (different code path), checksummed in iteration order
* **TSDB** (16 KiB partition, C only for now): 200 ×
  `fdb_tsl_append_with_ts` with deterministic 16-byte payloads and fake
  timestamps, then full iteration + checksum.

`kv_checksum` is computed over the values read back in key-index order, so it
is **implementation independent**: the C and Rust builds currently produce the
same value (`0x4b3486a4`), which validates the Rust crate's on-flash format and
read path against C. `kv_iter_checksum` covers `(name, value)` in *iteration*
order; C and Rust currently iterate in different physical orders, so these
values differ and are reported separately on purpose. `ts_checksum` covers the
deterministic payload stream and already matches between the two firmwares.

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
  `FDB_WRITE_GRAN=8`, and `FDB_PRINT(...)` compiled out (a release firmware
  with logging disabled; otherwise printf would pull in a libc).
* `fal_cfg.h` — static partition table: `fdb_kvdb1` 16 KiB @ 0,
  `fdb_tsdb1` 16 KiB @ 16 KiB on `norflash0`; `FAL_PRINTF` compiled out.

The Homebrew `arm-none-eabi-gcc` ships only GCC's freestanding headers, so
`c/include/` also provides minimal `stdio.h`, `string.h`, `stdlib.h`,
`inttypes.h`, `time.h` shims. `c/libc_stubs.c` implements the handful of libc
functions FlashDB/FAL use (`memcpy`, `memmove`, `memset`, `memcmp`, `strlen`,
`strcmp`, `strncmp`, `strncpy`). `c/fal_ram_sim.c` is the FAL NOR driver over
the `.sim_flash` region (program = bitwise AND, erase = `0xFF` on 4 KiB
blocks).

The script also prints per-object sizes of the FlashDB/FAL objects before
`--gc-sections` as a library-only ROM upper bound (currently ~10.3 KiB across
KVDB/TSDB/FAL/utils), which is useful to separate the library from the
harness.

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

(`scripts/fw_rust.sh` sets `CARGO_TARGET_THUMBV7M_NONE_EABI_LINKER` and the
cargo target dir, and passes exactly these flags.)

Build profile:

* `release`: `opt-level="s"`, `lto=true`, `codegen-units=1`, `panic="abort"`.
  LTO is required for dead-code elimination here: stable rustc does not expose
  per-function sections, so without LTO the whole crate (including the
  not-yet-used TSDB module) is kept in one `.text` section and cannot be
  garbage-collected.
* `size`: `opt-level="z"`, `lto=true` — for the later C-vs-Rust size-profile
  comparison (`./scripts/fw_rust.sh size`).

The firmware provides `_start`-style startup from the shared
`common/startup_armv7m.S` via `core::arch::global_asm!(include_str!(...))`, a
panic handler, semihosting output, the stack scan, and a small `Flash`
implementation over the `.sim_flash` NOLOAD region.

**Why not `flashdb-sim::RamFlash` in the firmware?** `RamFlash<SIZE, WG>` owns
its `[u8; SIZE]` buffer by value. In a bare-metal image a 16 KiB `static` with
a non-zero initialiser lands in `.data`, i.e. 16 KiB of *ROM*, and in the
static-RAM metric — neither is acceptable for a footprint measurement (it
would also misrepresent external flash as MCU RAM). The firmware therefore
implements the ~60-line `flashdb::Flash` trait over a `#[link_section =
".sim_flash"]` static, exactly like the C side's FAL simulator; the driver is
harness code present in both images. `flashdb-sim` remains the simulator for
the host tests/benchmarks.

### Wiring in the real TSDB (next step)

`tsdb_phase_placeholder()` in `rust/main.rs` currently emits the same
deterministic 16-byte payloads as the C TSDB workload but does not use the
crate; it keeps no RAM storage, so it does not pollute the static-RAM number.
When the Rust TSDB API is available:

1. Add a second 16 KiB backing store in `rust/main.rs` (the `.sim_flash` region
   in the linker script is 32 KiB, so it fits after the KV store):

   ```rust
   #[repr(C, align(4096))]
   struct TsFlashStore([u8; 16384]);
   #[link_section = ".sim_flash"]
   static mut TS_SIM_FLASH: TsFlashStore = TsFlashStore([0u8; 16384]);
   ```

   and a second `impl Flash for TsSimFlash` (same code as `SimFlash`, pointing
   at `TS_SIM_FLASH`; the two `align(4096)` statics are placed at +0 and
   +16 KiB by the linker).

2. Keep the TSDB object in `.bss` like the KVDB (a `MaybeUninit` cell), e.g.
   `Tsdb<&'static mut ...>` mounted with the TSDB API, then replace the body of
   `tsdb_phase_placeholder()` with the real append/iterate calls. Keep the
   `ts_appends`, `ts_iterated` and `ts_checksum` accounting identical so the
   `RESULT` line stays comparable with C.
3. Keep `sim_flash` reporting correct: the Rust `sim_flash` field comes from
   `__sim_flash_start/end`, so it will grow from 16 KiB to 32 KiB automatically
   once the second store exists.
4. Remove the placeholder comment in `scripts/fw_rust.sh` that says TSDB is a
   placeholder.

---

## Current measurements (macOS arm64, QEMU 11.0.0, mps2-an385)

C baseline: `./scripts/fw_c.sh`; Rust: `./scripts/fw_rust.sh` (release profile).

| metric | C FlashDB | Rust flashdb (KVDB real, TSDB placeholder) |
|---|---|---|
| `.text` | 9720 | 12580 |
| `.rodata` | 1632 | 1444 |
| `.data` | 56 | 0 |
| ROM total (`text+rodata+data`) | **11408** | **14024** |
| `.bss` | 1008 | 552 |
| static RAM (`data+bss`) | **1064** | **552** |
| simulated NOR (excluded) | 32768 | 16384 (TS partition pending) |
| stack high-water | **1192** | **2288** |
| stack after init | 696 | 1440 |
| stack limit | 8192 | 8192 |
| KV checksum (index-order, cross-impl) | `0x4b3486a4` | `0x4b3486a4` |
| KV iteration checksum (order-sensitive) | `0x0e250daa` | `0x2c2ba40a` |
| TS checksum (payload stream) | `0x10d90680` | `0x10d90680` (placeholder) |
| KV writes / gets / iterated | 1596 / 64 / 64 | 1596 / 64 / 64 |
| TS appends / iterated | 200 / 200 | 200 / 200 (placeholder) |

`./scripts/fw_rust.sh size` (opt-level="z" + LTO) gives text=11700,
rodata=1444, data=0, bss=552 and stack=2024 for the same workload — use the
`size` profile when a smallest-code comparison is wanted.

C compilation is reproducible byte for byte. The Rust row is from a clean
`--clean` rebuild; LTO/codegen layout can vary by a few tens of bytes of
`.text` and ~1% of stack between rebuilds (checksums and section classes do
not change), so treat single-byte differences as noise.

How to read this honestly:

* Numbers include the harness (~1.5–3 KiB depending on what LTO inlined), so
  compare the two rows, not absolute vendor claims.
* The C image contains the whole FAL layer (~0.8 KiB) plus `-lgcc` helpers; the
  Rust image contains compiler-builtins memcpy/`__aeabi_*` helpers (~1.5 KiB)
  and the Rust `Flash` driver. The C library-only ROM upper bound (pre-`gc`) is
  printed by `scripts/fw_c.sh` (~10.3 KiB for KVDB+TSDB+FAL+utils).
* Rust static RAM is lower because the crate has no sector-cache table and a
  smaller DB struct; Rust stack is higher in this build — `stack_init` shows
  most of the difference already appears during mount.
* `kv_checksum` equality is a strong functional cross-check: both
  implementations store and read back the same bytes for this workload.
* `sim_flash` is reported but excluded from ROM/RAM on purpose: it models an
  external NOR part, not on-chip memory.
