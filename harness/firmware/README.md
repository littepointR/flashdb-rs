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
RESULT text=11232 rodata=1428 data=0 bss=552 sim_flash=16384 stack=2000 \
       stack_init=2000 stack_limit=8192 kv_writes=1596 kv_gets=64 \
       kv_iterated=64 kv_checksum=0x4b3486a4 kv_iter_checksum=0x2c2ba40a \
       ts_appends=200 ts_iterated=200 ts_checksum=0x10d90680 rc=0
```

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
./scripts/fw_rust.sh size --phases --symbols
./scripts/fw_c.sh --phases           # same per-phase diagnostic run for C
./scripts/fw_c.sh --symbols          # C symbol attribution + per-object sizes
./scripts/fw_c.sh --build-only       # build + sizes only, no QEMU
./scripts/fw_c.sh --clean            # remove build products (both scripts)
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

Build profiles:

* `release`: `opt-level="s"`, `lto=true`, `codegen-units=1`, `panic="abort"`.
* `size`: `opt-level="z"`, `lto=true` (`./scripts/fw_rust.sh size`).
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
let mut db = Tsdb::mount(flash, Config::new(4096, 16384), 32)?;
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

Environment: macOS arm64, QEMU 11.0.0, `mps2-an385`. C: `./scripts/fw_c.sh`.
Rust: `./scripts/fw_rust.sh` (release = `opt-level="s"`) and
`./scripts/fw_rust.sh size` (`opt-level="z"`). Same crate revision for all
three rows; crate files were being edited concurrently, so re-run after
crate changes (numbers can move by a few tens of bytes).

The Rust workload is **KVDB + TSDB**, matching the C workload exactly.
Cross-implementation checksums are identical (`kv_checksum 0x4b3486a4`,
`kv_iter_checksum 0x0e250daa`, `ts_checksum 0x10d90680`).

### Headline ROM / RAM / stack (KVDB + TSDB)

| metric | C FlashDB | Rust release (`s`) | Rust size (`z`) |
|---|---|---|---|
| `.text` | 9720 | 15728 | 14220 |
| `.rodata` | 1632 | 1520 | 1520 |
| `.data` | 56 | 0 | 0 |
| ROM total | **11408** | **17248** | **15740** |
| `.bss` | 1008 | 624 | 624 |
| static RAM | **1064** | **624** | **624** |
| simulated NOR (excluded) | 32768 | 32768 | 32768 |
| stack high-water | **1192** | **1312** | **1432** |
| static+stack RAM | **2256** | **1936** | **2056** |
| stack limit | 8192 | 8192 | 8192 |
| KV writes / gets / iterated | 1596 / 64 / 64 | same | same |
| KV checksum | `0x4b3486a4` | `0x4b3486a4` | `0x4b3486a4` |
| KV iteration checksum | `0x0e250daa` | `0x0e250daa` | `0x0e250daa` |
| TS appends / iterated | 200 / 200 | 200 / 200 | 200 / 200 |
| TS checksum | `0x10d90680` | `0x10d90680` | `0x10d90680` |

Diagnostic (`--phases`) builds are larger by construction and are not part of
the comparison: C ROM 12032, Rust release ROM 19112, Rust size ROM 17416.

> Snapshot note: this table is from the current `crates/flashdb` working tree
> during the parallel crate optimisation pass (the crate was still being
> edited and was briefly uncompilable mid-edit; the harness itself is
> unchanged by that). Refresh with `./scripts/fw_rust.sh` after the crate work
> lands — the harness code is final for this round.

### Per-phase stack (bytes, `--phases` builds)

| phase | C | Rust release | Rust size |
|---|---|---|---|
| mount/init (`stack_init`) | 752 | 912 | 952 |
| one KV set | 736 | 952 | 984 |
| one KV get | 720 | 1056 | 1048 |
| full KV iteration | 560 | 784 | 856 |
| KV GC churn (1500 writes) | 1248 | 1424 | 1528 |
| one TS append | 528 | 680 | 728 |
| full TS iteration | 568 | 792 | 800 |
| headline `stack` (max) | 1192 | 1400 | 1504 |

Reading: C peaks in GC churn (1,248 B); Rust peaks in the diagnostic run's GC
churn too (1,400–1,528 B); per-operation phases are 100–300 B deeper in Rust.
The headline `stack` values above (1,312 B release / 1,432 B size) come from
the non-diagnostic build. Per-phase numbers include each firmware's diagnostic
frame (~100–300 B, present on both sides), so compare the deltas.

### ROM attribution (bucketed code symbols)

| bucket | C | Rust release (`s`) | Rust size (`z`) |
|---|---|---|---|
| library code | 7674 (FlashDB/FAL, KV+TS) | ~7.4 KiB (flashdb, KV+TS) | 7396 (flashdb, KV+TS) |
| harness code | 2030 | ~6.6 KiB¹ | 6364¹ |
| core/panic + outliner stubs | – | ~0.4 KiB | 408 |
| toolchain mem helpers | – (inside harness/libgcc) | 138 | 138 |
| `.text` (size -A) | 9720 | 15728 | 14220 |

¹ inflated by crate code that LTO inlined into the harness's phase wrappers
(`tsdb_phase` ~1.8 KiB, `ts_mount` ~1.2 KiB, `kv_mount` ~1.0 KiB include the
crate mount/append bodies). An opaque-call-boundary experiment puts the
genuine harness code at ~3.9 KiB: harness bucket 6,456 → 3,880 B while crate
goes 7,396 → 10,434 B and total ROM grows by 476 B. So roughly 2.5 KiB of the
"harness" column is crate code awaiting crate-side de-inlining, and the
remaining ~3.9 KiB is the real harness floor (workload verification, error
plumbing, print path, NOR driver, startup).

### What is harness-side vs crate-side (KV+TS baseline)

Harness-side work is unchanged from the KV-only measurements above
(mem overrides, minimal panic, out-of-line driver, feature-gated diagnostics,
`#[inline(never)]` phase boundaries). The remaining gap is crate-side:

* **Crate vs C library.** After the crate optimisation pass so far, the named
  crate symbols (7,396 B, KV+TS) are *below* C's FlashDB/FAL library symbols
  (7,674 B), but ~2.5 KiB of crate mount/append code is accounted to the
  harness bucket because LTO inlines it into the harness wrappers. The total
  ROM gap is therefore mostly (a) that inlined crate code + genuine Rust
  harness (~3.9 KiB) versus C's 2.0 KiB harness, plus (b) Rust runtime
  baseline (`core`/panic/slice helpers/outliner ~0.4 KiB).
* **Stack** is now much closer (release 1,312 B vs C 1,192 B; the KVDB
  placement API `Kvdb::mount_into` removed the mount by-value copy). Remaining
  per-phase deltas are 100–300 B and come from crate call depth.
* **Stack**: Rust mount ≈2,040 B vs C 752 B; per-op phases +150–350 B. The
  crate `mount` frames (KV and TS) dominate; candidate causes are the
  per-call scratch buffers and the inlined write paths. Crate-side
  de-inlining/exact-size buffers should address this.
* **Rust runtime baseline** ~0.4–0.5 KiB (panic trampolines, slice helpers,
  outliner stubs) inherent to `no_std` Rust + LTO.
* **Harness residual** after subtracting inlined crate code is a few hundred
  bytes (workload verification, print path, NOR driver) and is symmetrically
  present in the C image; shrinking it further would remove the checks that
  make the harness trustworthy.

### Reproducibility

C compilation is reproducible byte for byte. Rust LTO/codegen layout can vary
by a few tens of bytes of `.text` and ~1 % of stack between rebuilds
(checksums and section classes do not change). `crates/flashdb` is being
edited concurrently; the numbers above correspond to one crate revision —
re-run both scripts after crate changes.

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
