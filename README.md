# FlashDB-rs

A from-scratch **Rust rewrite of [FlashDB](https://github.com/armink/FlashDB)** —
the ultra-lightweight embedded key-value and time-series database for raw flash.

The rewrite keeps the proven FlashDB on-flash format (data written by FlashDB-C
mounts in Rust and vice versa) but replaces the implementation with a `no_std`,
allocation-free, `unsafe`-free Rust design that is **faster and smaller** than
the C original on the benchmark suite in `harness/`.

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
* **No heap, no global state.** `#![no_std]`, all buffers are stack or
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
  548 B vs C's 1064 B; `Tsdb` is 140 B vs 160 B (32-bit targets). Embedded
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
| `reference/FlashDB` | upstream clone used for differential testing (not part of the crate) |

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

```sh
cargo test -p flashdb --release          # unit + smoke + parity tests
cargo build -p flashdb --target thumbv7m-none-eabi --release   # no_std build

scripts/c_bench.sh                       # C reference benchmarks -> harness/results
cargo run -p flashdb-bench --release --bin flashdb-bench -- --wg 8 --runs 5 --with-ts
python3 scripts/compare_bench.py --wg 8  # side-by-side comparison / acceptance check

scripts/rust_compat.sh                   # C<->Rust flash image compatibility matrix
scripts/c_test.sh                        # C behavioural checks (reference)
scripts/fw_c.sh && scripts/fw_rust.sh    # firmware ROM/RAM/stack comparison (QEMU)
```

The benchmarks use a RAM-backed NOR simulator with exact NOR semantics
(program only clears bits, erase sets `0xFF`) and count every flash
read/write/erase and byte, so results are comparable across machines and
meaningful for real flash.

## Measured results

Benchmarked against FlashDB-C with identical workloads and a counting NOR
simulator; full tables and methodology in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

| metric (Cortex-M3, same workload) | C FlashDB | Rust |
|---|---:|---:|
| ROM (`-Os` vs `-z`) | 11 408 B | 14 464 B (13 516 B with `small-crc`; 12 920 B smallest nightly build) |
| static RAM | 1 064 B | **624 B** |
| static + stack RAM | 2 256 B | **2 056 B** |
| KV flash reads (hot get / miss / update / iterate) | 6 000 / 159 / 11.1 / 15.9 per op | **4 / 37 / 8.0 / 6.9** |
| TS flash reads / writes / erases | identical to C in every scenario | identical |
| host CPU | baseline | faster in 11/16 scenarios in the default build |

All scenarios produce identical logical checksums, and Rust-written and
C-written flash images are byte-identical for `WRITE_GRAN` 1 and 8. ROM is the
one metric not yet at parity; the remaining gap is Rust bounds/runtime/harness
overhead, and the size levers above are documented in `docs/BENCHMARKS.md` (the nightly `build-std` size build is measured at 12 920 B).

One build option is available for the size/CPU trade-off:
`--features small-crc` (64-byte CRC table instead of 1 KiB, −948 B ROM, ~2x
CRC CPU which only matters for large values).

## License

Apache-2.0, same as the original FlashDB. The on-flash format and algorithm
behaviour are derived from FlashDB (Copyright (c) 2020-2023 Armink); this
rewrite is an independent implementation.
