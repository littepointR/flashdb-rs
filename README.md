# FlashDB-rs

A from-scratch **Rust rewrite of [FlashDB](https://github.com/armink/FlashDB)** —
the ultra-lightweight embedded key-value and time-series database for raw flash.

The rewrite keeps the proven FlashDB on-flash format (data written by FlashDB-C
mounts in Rust and vice versa) but replaces the implementation with a `no_std`,
allocation-free Rust design that is **faster and uses less RAM** than the C
original on the benchmark suite in `harness/`. With the size build plus
`small-crc` and the `-InlineThreshold 50` codegen flag it now also reaches
**ROM and stack parity on real silicon**, with a full C-vs-Rust acceptance
(11/11 checks) passing on a NUCLEO-U545RE-Q: see
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) for the measured numbers and
[`docs/OPTIMIZATION.md`](docs/OPTIMIZATION.md) for the optimisation record.


```rust
use flashdb::{Config, Flash, Kvdb};

struct NorFlash { /* SPI driver for your chip / partition */ }
impl Flash for NorFlash {
    type Error = core::convert::Infallible;
    const WRITE_GRAN_BITS: u32 = 8;     // 1 = NOR, 8 = STM32F2/F4, 32 = STM32F1 ...
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> { unimplemented!() }
    fn write(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> { unimplemented!() }
    fn erase(&mut self, addr: u32, len: u32) -> Result<(), Self::Error> { unimplemented!() }
}

# fn demo(flash: NorFlash) -> Result<(), flashdb::Error<core::convert::Infallible>> {
// 4 erase blocks of 4 KiB = one 16 KiB partition.
let mut db = Kvdb::mount(flash, Config::new(4096, 16 * 1024))?;

db.set(b"boot_count", &42u32.to_le_bytes())?;

let mut value = [0u8; 16];
if let Some(n) = db.get(b"boot_count", &mut value)? {
    let count = u32::from_le_bytes(value[..n].try_into().unwrap());
    let _ = count;
}

let mut it = db.iter();
while let Some(entry) = it.next() {
    let entry = entry?;                       // key, length, status, address
    let mut buf = [0u8; 64];
    let n = it.read_value(&entry, &mut buf)?; // verifies the record CRC32
    let _ = (&entry.name(), n);
}
# Ok(())
# }
```

## What is different from the C implementation

This is a rewrite, not a translation. The on-flash format is the same; the
internals are Rust-native:

* **No global state.** A database owns its flash through generics
  (`Kvdb<F>` / `Tsdb<F, T>`), instead of a FAL registry and a global lock.
  Implement the `Flash` trait for a partition view and pass it in.
* **No heap.** `#![no_std]`, all buffers are stack or
  caller-provided. The crate is `#![deny(unsafe_code)]` with two audited placement-initializer
  functions (`Kvdb::mount_into`, `Tsdb::mount_into`) so that a database stored
  in a `static` (the normal MCU pattern) needs no stack copy.
* **Typed errors.** `Error<E>` carries the flash driver's own error and says
  which operation failed; no `FDB_ASSERT` busy-loop, no error codes lost.
* **Iterators instead of callbacks.** KV and TS iteration are real
  `Iterator`s; TS supports forward, reverse and by-time modes. Values are read
  on demand, so iteration does not copy payloads you do not need.
* **Compile-time configuration.** Write granularity is a constant of the flash
  driver (`WRITE_GRAN_BITS`); layout tables, status encodings and buffer sizes
  specialise per concrete flash type with zero runtime cost. Timestamp width is
  a type parameter (`Tsdb<F>` = 32-bit, `Tsdb64<F>` = 64-bit).
* **Less flash traffic.** Lookups validate headers and match names without
  CRC-ing every value they walk past; `get` verifies the CRC *while* copying
  into the caller's buffer instead of reading the value twice.
* **Less RAM.** A 2-way set-associative 64-entry name cache (516 B incl. LRU)
  replaces C's 512 B cache *plus* ≈320 B sector cache. The `Kvdb` struct is
  560 B on Cortex-M3 (C: 1064 B); `Tsdb` is 64 B (C: 160 B). Embedded
  users can mount both databases directly into `static`s
  (`Kvdb::mount_into`, `Tsdb::mount_into`), avoiding the stack copy of the
  database struct that a by-value constructor would otherwise need.
* **Fewer CPU cycles.** Direct next-record addressing (no magic-word rescan per
  record), O(1) cache probing instead of a 64-entry linear scan, no per-record
  full-value CRC during traversal.

See [`docs/DESIGN.md`](docs/DESIGN.md) for the architecture and
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) for measured results.

## Repository layout

| path | contents |
|---|---|
| `crates/flashdb/` | the library: `no_std`, no dependencies |
| `crates/flashdb-sim/` | RAM NOR simulator with op/byte accounting + fault injection (host tests/benches, also `no_std`) |
| `crates/flashdb-bench/` | Rust side of the benchmark protocol + C/C++ image tool |
| `harness/c/` | C reference harness (RAM FAL, benchmarks, image tool, behavioural checks) |
| `harness/firmware/` | Cortex-M3/QEMU firmware comparison (C vs Rust): `.text/.rodata/.data/.bss`, stack high-water |
| `harness/SCENARIOS.md` | the normative benchmark protocol shared by C and Rust |
| `reference/FlashDB` | upstream clone used for differential testing, fetched by script (not part of the crate) |

## Real-hardware test

`harness/firmware/u545re/` runs the crate on a **NUCLEO-U545RE-Q**
(STM32U545RE, Cortex-M33): the same KVDB+TSDB workload as the QEMU firmware,
but on silicon, with DWT cycle counters and a RAM result buffer read back over
SWD. The acceptance workload uses the RAM NOR simulator for C/Rust comparison;
two further images drive the **real STM32U5 internal flash controller** for
endurance and long-run tests. See
[`harness/firmware/u545re/README.md`](harness/firmware/u545re/README.md).

Verified on silicon: the firmware completes with `rc=0`, both `RESULT` lines are
byte-identical over repeated runs, and all workload checksums
(`kv_checksum`, `kv_iter_checksum`, `ts_checksum`) match the QEMU Cortex-M3
baseline exactly. The C reference has the same board backend, so the comparison
is like-for-like on identical silicon, and **the acceptance passes in both the
stable and the nightly `size` build**: ROM **13 228 B / 13 204 B** vs C 13 360 B,
stack 1 080 B vs 1 240 B, static RAM 640 B vs 1 072 B, and every workload phase
is faster than C (total CPU −35.2 % stable / −35.5 % nightly; the CPU-fast
`release` profile is −44.2 % but larger ROM). The board run also covers **TSDB
rollover** (1 200 appends past capacity, live-window/remount/NOR-safety checks)
and proves the C and Rust images leave **byte-identical flash partitions**.
Flashing uses **probe-rs**, which programs the STM32U5 independently of the
probe's own flash loader; the earlier OpenOCD/pyOCD probe hang does not apply to
that path, and no ST-LINK firmware update was needed.

Real internal flash (bank-1 scratch pages only) is exercised by five dedicated
images: a deterministic KV+TS image written with `WRITE_GRAN=128` is
**byte-identical to the host model** on silicon; an external-reset
**power-loss** injector recovers **50/50** interrupted cycles; a **20-boot/19-reset
soak** verifies persistence and remounting; 240 s of page erase/program
**endurance** reaches **22 493 P/E cycles** on all 8 scratch pages (740 967
quad-words, 22 509 erases, 0 errors); and a 60 s **KVDB+TSDB long-run** does
72 rounds with 1 405 sector erases and 18+18 verified remounts (0 errors).
The power-loss test found and fixed a `Kvdb::check()` false positive on torn
`PRE_WRITE` headers; the host C-compat matrix also found and fixed a
`WRITE_GRAN>8` status-granule mismatch (~110 B ROM side benefit). Full tables,
reproduction commands and the exact acceptance configuration:
[`harness/firmware/u545re/README.md`](harness/firmware/u545re/README.md); test
inventory and coverage: [`docs/TESTING.md`](docs/TESTING.md).

## Compatibility

For the same sector size, write granularity and timestamp width, the on-flash
format is byte-compatible with FlashDB 2.2.x:

* a Rust `Kvdb`/`Tsdb` mounts data written by FlashDB-C;
* FlashDB-C mounts data written by this crate;
* deterministic write workloads produce **identical flash images** (verified by
  `scripts/rust_compat.sh`, see `docs/BENCHMARKS.md`).

Supported write granularities: 1 (bit-level NOR), 8 (byte programming), 32, 64,
128, 256 bits (the common NOR and STM32 families). Little-endian only.
File-system mode (`FDB_USING_FILE_*`) is not ported; implement `Flash` over a
file if needed. See “Known deviations” in `docs/DESIGN.md`.

## Building and testing

Prerequisites: Rust (`cargo`), a C99 compiler, `arm-none-eabi-*` plus QEMU for
the firmware comparison, and the upstream FlashDB sources for the differential
C harness:

```sh
scripts/fetch_reference.sh               # clone FlashDB 2.2.x into reference/FlashDB
cargo test -p flashdb --release          # unit + smoke + parity tests
cargo test -p flashdb --release --test long_run -- --ignored   # long-run/large-data soak
cargo doc --workspace --no-deps          # API documentation -> target/doc/flashdb
cargo build -p flashdb --target thumbv7m-none-eabi --release   # no_std build

scripts/c_bench.sh                       # C reference benchmarks -> harness/results
cargo run -p flashdb-bench --release --bin flashdb-bench -- --wg 8 --runs 5 --with-ts
python3 scripts/compare_bench.py --wg 8  # side-by-side comparison / acceptance check

scripts/rust_compat.sh                   # C<->Rust flash image compatibility matrix
scripts/c_test.sh                        # C behavioural checks (reference)
scripts/fw_c.sh && scripts/fw_rust.sh    # firmware ROM/RAM/stack comparison (QEMU)
```

On real hardware (`NUCLEO-U545RE-Q`, see
[`harness/firmware/u545re/README.md`](harness/firmware/u545re/README.md)):

```sh
harness/firmware/u545re/run_rust.sh       # POSIX: build + flash + run (probe-rs/pyOCD/OpenOCD)
harness/firmware/u545re/run_rust.ps1      # Windows/PowerShell: build + flash + run (probe-rs)
harness/firmware/u545re/run_flash_parity.ps1       # real internal flash: C-model image parity
harness/firmware/u545re/run_flash_fault.ps1        # real internal flash: power-loss injection
harness/firmware/u545re/run_flash_reboot_soak.ps1  # real internal flash: 20-boot soak
harness/firmware/u545re/run_flash_tests.ps1 -Test endurance   # real internal flash: 240 s P/E
harness/firmware/u545re/run_flash_tests.ps1 -Test longrun     # real internal flash: 60 s DB soak
```

The benchmarks use a RAM-backed NOR simulator with exact NOR semantics
(program only clears bits, erase sets `0xFF`) and count every flash
read/write/erase and byte, so results are comparable across machines and
meaningful for real flash.

## Measured results

Benchmarked against FlashDB-C with identical workloads and a counting NOR
simulator; full tables and methodology in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

| metric (Cortex-M3 QEMU, same workload) | C FlashDB | Rust |
|---|---:|---:|
| ROM (`-Os` vs `-z` + `small-crc` + `-InlineThreshold 50`) | 11 408 B | 11 632 B (+2.0 %); on silicon **13 228 B (stable) / 13 204 B (nightly) vs C 13 360 B (parity)** |
| static RAM | 1 064 B | **624 B** |
| stack high-water | 1 192 B | **1 008 B** |
| static + stack RAM | 2 256 B | **1 632 B** |
| KV flash reads (hot get / miss / update / iterate) | 6 000 / 159 / 11.1 / 15.9 per op | **4 / 37 / 8.0 / 6.9** |
| TS flash reads / writes / erases | identical to C in every scenario | identical |

All scenarios produce identical logical checksums, and Rust-written and
C-written flash images are byte-identical for `WRITE_GRAN` 1, 8, 32, 64 and 128
(all six granularities are checked; wg 256 is a documented C-side XFAIL because
the C reference tool is nondeterministic there). On the real
board the C-vs-Rust acceptance passes every check (ROM, static RAM, stack,
static+stack, all five cycle phases, 21 logical fields, both raw image
checksums); the QEMU harness image is still +224 B ROM because of its different
rodata/code split, while the board image is −132/−156 B. The optimisation record
and the remaining optional levers are in `docs/OPTIMIZATION.md`.

CI (`.github/workflows/ci.yml`) runs formatting, the full host suite (default and
`--features std`), rustdoc with `-D warnings` and the QEMU firmware build on
every push/PR; `.github/workflows/hardware.yml` is a manual self-hosted-runner
workflow for the NUCLEO-U545RE-Q real-hardware suite.

Two build options matter for the size/CPU trade-off:
`--features small-crc` (64-byte CRC table instead of 1 KiB, −960 B rodata, ~2x
CRC CPU which only matters for large values) and the codegen flag
`-C llvm-args=-inline-threshold=50` (the `-Oz` default inliner is too
conservative: −84 B ROM and −22 % TS / −21 % rollover CPU on silicon). On the
board, the `release` profile remains the CPU-fastest option (total −43 %) at the
cost of ROM.

## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
* MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

This crate is an independent Rust implementation of the FlashDB on-flash format
and behaviour. FlashDB itself (https://github.com/armink/FlashDB, Copyright (c)
2020 Armink, Apache-2.0) is not included in this repository; it is fetched by
`scripts/fetch_reference.sh` for differential testing. See [NOTICE](NOTICE) for
the attribution details.
