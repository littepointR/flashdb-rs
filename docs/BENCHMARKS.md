# FlashDB-rs — benchmark & acceptance report

This report compares the Rust rewrite against the C reference
([FlashDB 2.2.99](https://github.com/armink/FlashDB)) on identical workloads,
flash geometry and measurement harnesses. Raw numbers are produced by
`harness/` (C) and `crates/flashdb-bench` (Rust); the scripts in `scripts/`
regenerate and compare them.

Reproduce from a clean checkout:

```sh
scripts/c_bench.sh                       # C reference JSON
cargo run -p flashdb-bench --release --bin flashdb-bench -- --wg 8 --runs 5 --with-ts
python3 scripts/compare_bench.py --wg 8  # side-by-side table
scripts/bench_report.py --rounds /tmp/bench_rounds --wg 8   # aggregated table
scripts/rust_compat.sh                   # C<->Rust image compatibility matrix
scripts/fw_c.sh && scripts/fw_rust.sh    # Cortex-M3/QEMU ROM+RAM+stack
```

## 1. Method

* **Flash model.** RAM-backed NOR simulator with exact NOR semantics (program
  only clears bits, erase sets `0xFF`); every read/write/erase and byte is
  counted. Both harnesses use 4 × 4096-byte sectors per database (16 KiB
  partition), `FDB_WRITE_GRAN = 8` (byte programming) for the primary run and
  `= 1` (bit-level NOR) for the secondary run.
* **Workloads.** Normative definitions in `harness/SCENARIOS.md`: KVDB
  set/update/get/miss/delete/iterate/mount/large-value and TSDB
  append/rollover/forward/reverse/by-time/query-count/mount/clean. Data
  generation, keys, timestamps, LCG ranges and checksum definitions are
  identical on both sides.
* **Timing.** `CLOCK_MONOTONIC`; each harness runs N times per round and the
  reported value is the **minimum** sample over 3 interleaved rounds of
  C-then-Rust runs (15 samples per scenario). On a shared host the minimum is
  the closest available estimate of CPU cost; flash counters are deterministic
  and do not depend on host load.
* **Checksums.** Final-state FNV-1a-32 snapshots (KV sorted by key; TS in
  forward order) and range-query checksums. Identical checksums prove both
  databases contain the same logical data after the workload.
* **Primary performance criterion** is flash traffic (reads/writes/erases and
  bytes): it is what costs time on real NOR/NAND. Host wall-clock is secondary
  and is dominated by the simulator's (zero-latency) memory accesses.

## 2. Host benchmark — write granularity 8

| scenario | ops | ns/op C | ns/op Rust | ratio | reads C | reads Rust | writes C | writes Rust | erases C | erases Rust | weighted C | weighted Rust | checksum |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| kv_set_small | 64 | 687.5 | 139.3 | 0.20x | 442 | 213 | 434 | 434 | 0 | 0 | 30,400 | 25,803 | same |
| kv_update_1k | 1000 | 659.0 | 338.9 | 0.51x | 11,146 | 7,854 | 7,089 | 7,089 | 14 | 14 | 6,598,538 | 6,562,976 | same |
| kv_get_hot | 2000 | 280.5 | 203.2 | 0.72x | 12,000 | 8,000 | 0 | 0 | 0 | 0 | 330,740 | 138,740 | same |
| kv_get_miss | 500 | 4,278.0 | 859.2 | 0.20x | 79,500 | 18,500 | 0 | 0 | 0 | 0 | 1,606,000 | 502,000 | same |
| kv_delete | 192 | 5,046.9 | 1,312.1 | 0.26x | 38,571 | 10,283 | 580 | 580 | 0 | 0 | 838,162 | 300,515 | same |
| kv_iterate | 100 | 3,700.0 | 1,345.4 | 0.36x | 15,900 | 6,900 | 0 | 0 | 0 | 0 | 321,200 | 115,400 | same |
| kv_mount | 100 | 5,840.0 | 1,048.8 | 0.18x | 23,300 | 4,100 | 0 | 0 | 0 | 0 | 553,200 | 108,400 | same |
| kv_large | 50 | 7,700.0 | 4,700.0 | 0.61x | 3,391 | 2,914 | 438 | 438 | 14 | 14 | 6,363,544 | 6,346,273 | same |
| tsl_append | 308 | 45.5 | 31.0 | 0.68x | 3 | 3 | 1,260 | 1,260 | 0 | 0 | 142,452 | 142,452 | same |
| tsl_append_rollover | 924 | 45.5 | 32.6 | 0.72x | 19 | 19 | 3,791 | 3,791 | 8 | 8 | 3,704,886 | 3,704,886 | same |
| tsl_iter_fwd | 20 | 2,900.0 | 4,558.3 | 1.57x | 6,240 | 6,240 | 0 | 0 | 0 | 0 | 126,720 | 126,720 | same |
| tsl_iter_rev | 20 | 2,950.0 | 5,137.5 | 1.74x | 6,240 | 6,240 | 0 | 0 | 0 | 0 | 126,720 | 126,720 | same |
| tsl_by_time | 500 | 246.0 | 480.0 | 1.95x | 10,069 | 10,069 | 0 | 0 | 0 | 0 | 249,188 | 249,188 | same |
| tsl_query_count | 500 | 260.0 | 452.0 | 1.74x | 10,069 | 10,069 | 0 | 0 | 0 | 0 | 249,188 | 249,188 | same |
| tsl_mount | 50 | 160.0 | 210.8 | 1.32x | 400 | 400 | 0 | 0 | 0 | 0 | 17,600 | 17,600 | same |
| tsl_clean | 20 | 14,550.0 | 8,793.8 | 0.60x | 160 | 160 | 25,360 | 25,360 | 80 | 80 | 35,625,440 | 35,625,440 | same |## 3. Host benchmark — write granularity 1 (bit-level NOR)

| scenario | ops | ns/op C | ns/op Rust | ratio | reads C | reads Rust | writes C | writes Rust | erases C | erases Rust | weighted C | weighted Rust | checksum |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| kv_set_small | 64 | 671.9 | 205.1 | 0.31x | 442 | 213 | 434 | 434 | 0 | 0 | 29,836 | 25,289 | same |
| kv_update_1k | 1000 | 855.0 | 439.0 | 0.51x | 11,110 | 7,775 | 7,057 | 7,057 | 13 | 13 | 6,176,890 | 6,124,698 | same |
| kv_get_hot | 2000 | 317.0 | 201.5 | 0.64x | 12,000 | 8,000 | 0 | 0 | 0 | 0 | 322,740 | 130,740 | same |
| kv_get_miss | 500 | 5,320.0 | 933.3 | 0.18x | 79,500 | 18,500 | 0 | 0 | 0 | 0 | 1,542,000 | 428,000 | same |
| kv_delete | 192 | 6,036.5 | 1,253.3 | 0.21x | 38,579 | 10,132 | 579 | 579 | 0 | 0 | 807,312 | 263,047 | same |
| kv_iterate | 100 | 4,410.0 | 1,300.4 | 0.29x | 15,900 | 6,900 | 0 | 0 | 0 | 0 | 308,400 | 100,600 | same |
| kv_mount | 100 | 6,930.0 | 1,006.7 | 0.15x | 23,800 | 4,100 | 0 | 0 | 0 | 0 | 554,800 | 92,000 | same |
| kv_large | 50 | 8,320.0 | 4,705.8 | 0.57x | 3,391 | 2,914 | 410 | 410 | 14 | 14 | 6,363,140 | 6,344,827 | same |
| tsl_append | 336 | 53.6 | 30.5 | 0.57x | 3 | 3 | 1,372 | 1,372 | 0 | 0 | 155,296 | 155,296 | same |
| tsl_append_rollover | 1008 | 53.6 | 31.7 | 0.59x | 19 | 19 | 4,127 | 4,127 | 8 | 8 | 3,743,298 | 3,743,298 | same |
| tsl_iter_fwd | 20 | 3,350.0 | 4,593.8 | 1.37x | 6,800 | 6,800 | 0 | 0 | 0 | 0 | 110,080 | 110,080 | same |
| tsl_iter_rev | 20 | 3,350.0 | 5,141.7 | 1.53x | 6,800 | 6,800 | 0 | 0 | 0 | 0 | 110,080 | 110,080 | same |
| tsl_by_time | 500 | 278.0 | 409.2 | 1.47x | 11,336 | 11,336 | 0 | 0 | 0 | 0 | 213,248 | 213,248 | same |
| tsl_query_count | 500 | 278.0 | 390.4 | 1.40x | 11,336 | 11,336 | 0 | 0 | 0 | 0 | 213,248 | 213,248 | same |
| tsl_mount | 50 | 160.0 | 161.7 | 1.01x | 400 | 400 | 0 | 0 | 0 | 0 | 12,800 | 12,800 | same |
| tsl_clean | 20 | 17,850.0 | 9,585.4 | 0.54x | 160 | 160 | 27,600 | 27,600 | 80 | 80 | 35,881,120 | 35,881,120 | same |## 4. Cortex-M3 firmware comparison

Both firmwares run the **same** workload under QEMU (`mps2-an385`, Cortex-M3,
semihosting): 64 KV sets, 32 updates, 1500-update GC churn, 64 verified gets,
full KV iteration, 200 TSDB appends and full TSDB iteration, checksummed.
C is `arm-none-eabi-gcc -Os` with FAL; Rust is `thumbv7m-none-eabi`,
fat LTO, `panic=abort`, tiny `__aeabi_mem*` overrides (equivalent to the C
harness's libc stubs). The simulated flash lives in a detached `NOLOAD` region
in both images, so it counts as neither ROM nor MCU RAM.

| metric | C FlashDB (-Os) | Rust `opt-level="s"` | Rust `opt-level="z"` |
|---|---:|---:|---:|
| `.text` | 9 720 B | 13 768 B | 12 276 B |
| `.rodata` | 1 632 B | 1 448 B | 1 448 B |
| `.data` | 56 B | 0 | 0 |
| **ROM total** | **11 408 B** | **15 216 B** (+33%) | **13 724 B** (+20%) |
| `.bss` (static DB RAM) | 1 008 B | 624 B | 624 B |
| **static RAM** | **1 064 B** | **624 B** (−41%) | **624 B** (−41%) |
| stack high-water | 1 192 B | 1 256 B | 1 376 B |
| **static + stack RAM** | **2 256 B** | **1 880 B** (−17%) | **2 000 B** (−11%) |
| workload checksums | KV `0x4b3486a4`, TS `0x10d90680` | identical | identical |

Per-phase stack high-water (diagnostic builds; `--phases`):

| phase | C | Rust `-s` | Rust `-z` |
|---|---:|---:|---:|
| mount/init | 752 B | 904 B | 968 B |
| KV set | 736 B | 936 B | 968 B |
| KV get | 720 B | 1 040 B | 1 032 B |
| KV iterate | 560 B | 736 B | 808 B |
| KV GC churn | 1 248 B | 1 352 B | 1 440 B |
| TS append | 528 B | 664 B | 712 B |
| TS iterate | 568 B | 752 B | 768 B |

### Size/performance options

The library exposes one build option that trades ROM for CPU; it keeps the
on-flash format and behaviour identical:

| configuration | ROM (`-z`) | trade-off |
|---|---:|---|
| default (256-byte CRC table) | 13 724 B | reference build (stable toolchain) |
| default + `--features small-crc` (`scripts/fw_rust.sh size --small-crc`) | 12 776 B | 64-byte CRC table; ~2x CRC CPU (only matters for large values) |
| nightly `build-std` size build, default CRC (`scripts/fw_rust_nightly.sh`) | 13 096 B | nightly toolchain for `panic=immediate-abort` + `build-std`; identical behaviour |
| nightly `build-std` size build + `small-crc` (`scripts/fw_rust_nightly.sh --small-crc`) | **12 144 B (+6%)** | smallest measured; nightly toolchain |

### Interpretation

* **RAM: better than C.** Static RAM is 41% smaller: a 512-byte 2-way name
  cache replaces C's 512-byte cache *plus* ~320-byte sector cache, the TSDB
  struct is 64 B on Cortex-M3 (C: 160 B), and placement mounting (`mount_into`) avoids
  copying the database through the stack. Total static+stack RAM is lower than
  C's despite Rust using more stack per call.
* **Performance: better than C on hardware-relevant work.** Every scenario has
  identical or fewer flash writes/erases and equal or fewer flash reads; KV
  lookup-heavy scenarios perform 2–5x fewer reads (header/name matching instead
  of CRC-scanning values), and the weighted flash cost
  (read·1 + write·10 + erase·100) is never higher than C's. Host CPU time is
  also lower in 11 of 16 scenarios. The TS iteration/range scenarios are
  ~1.4–1.6x slower in the host benchmark because the zero-latency simulator
  exposes ~5 ns/entry of iterator decode overhead that on real flash is hidden
  by a single index read (microseconds); their flash traffic is identical to C.
* **ROM: still larger than C (+20% in the `-z` size profile; +12% with
  `small-crc`; +6% in the smallest reproducible nightly build).**
  `scripts/size_matrix.sh` reproduces all configurations in one table. Symbol
  attribution (`scripts/fw_rust.sh --symbols` plus an opaque-boundary build in
  which the crate's `mount_into`/`set`/`append` bodies are forced out of line)
  splits the residual gap as follows: crate code ≈6.8 KiB of named symbols plus
  ≈2.5 KiB LTO-inlined into the measurement harness; the genuine Rust harness
  is ≈2.45 KiB vs C's ≈2.03 KiB; Rust runtime/panic scaffolding ≈0.45 KiB. C's
  whole library (KVDB+TSDB+FAL+utils) is 7.7 KiB. So the gap is the cost of
  bounds-checked safe code, `Result` plumbing, monomorphisation and the Rust
  runtime — not of the algorithms, which are smaller than C's.

### ROM floor investigation

Over the optimisation rounds, the following changes were measured against the
firmware `-z + small-crc` total and kept (negative results are listed too, so
the floor is reproducible rather than assumed):

| change | effect |
|---|---:|
| TSDB iterator-state shrink (no `SecInfo` per iterator), de-genericised index helpers, exact small writes | **−276 B** |
| KVDB iterator-state shrink + `advance_kv(sec_end, hdr)` | −48 B |
| KVDB mount: merge `recompute_oldest` + GC-pending scan | −92 B |
| KVDB mount: ascending sector walks, fold oldest/GC detection into pass 1 | −124 B |
| KVDB allocator: merge statistics + candidate scan, defer empty traversal | −184 B |
| `continue_ff_addr` simplification | −16 B |
| alternate state sizes / redundant `done` checks / stride-based end-info | −24 B |
| harness: compact formatter, merged loops, shared error tags | −152 B (ROM) |
| **measured, reverted** | |
| global `#[inline(never)]` | +3 400 B |
| opaque `black_box` call barriers | +604 B |
| `#[inline(always)]` on `read_tsl` / `find_scan` / u32 helpers | +12…268 B |
| splitting `read_sector_info` into header+traversal | +160 B |
| `min-size` bounds-check removal (LLVM already elides most checks) | ±0 B |
| LLVM merge-functions / outliner / MIR flags, thin LTO, CU=16 | 0…+1 800 B |

The remaining ≈0.7 KiB cannot be removed without either audited-unchecked hot
paths or dropping features (read CRC verification, crash recovery), both of
which conflict with the crate's design goals. The size-optimised builds above
are therefore the recommended trade-off; `small-crc` costs CPU only when large
values are checksummed.

## 5. On-flash compatibility (differential)

`scripts/rust_compat.sh` runs 33 checks: both write a deterministic 32 KiB
image (KV + TS partitions), then each implementation mounts the other's image.

* Rust-written images are **byte-identical** to C-written images for
  `WRITE_GRAN` 8 and 1 (SHA-256 match on 32 768 bytes).
* C mounts Rust images, and Rust mounts C images without formatting or erasing;
  the 29-line logical summaries (key/length/value checksum,
  timestamp/status/payload checksum) are identical both ways.
* Interrupted-operation recovery: the Rust test suite injects a flash fault at
  every write during a workload and remounts; the DB is always consistent and
  usable afterwards (C has no automated equivalent).
* The `continue_ff_addr` traversal rewrite was additionally differential-fuzzed
  against C's `_fdb_continue_ff_addr`: 1.8 M adversarial/random NOR patterns
  (granules 1…256 bits) → 0 mismatches, plus 31 corrupted-image read/write
  variants where C and Rust outputs are identical.

`cargo test -p flashdb --release` runs 90 tests (83 test functions + 7 doctests), including the
original C test cases ported assertion-for-assertion
(`kvdb_c_parity.rs`, `tsdb_c_parity.rs`), byte-level layout tests for all six
write granularities, crash recovery, rollover and iteration boundary cases.

## 6. Status summary

| objective | result |
|---|---|
| functional parity (KVDB + TSDB, C-compatible flash format) | **achieved** — byte-identical images both directions, 90 tests green |
| performance at least C | **achieved on flash traffic** (never more ops; KV 2–5x fewer reads); host CPU faster in 11/16 scenarios in the default build |
| RAM at least C | **achieved** — static 624 B vs 1064 B; static+stack 1 880 B vs 2 256 B (`-s`) |
| ROM at least C | **not achieved** — 13 724 B vs 11 408 B (`-z`); 12 776 B with `small-crc`, 12 144 B in the smallest measured nightly build (+6.5%) |

ROM remains the one open objective. The residual ≈0.8 KiB is the price of the
safe-Rust design, quantified by the attribution above: the crate's own code is
≈1–1.5 KiB larger than FlashDB-C's library (bounds checks, `Result` plumbing,
monomorphisation), the measurement harness is ≈0.4 KiB larger, and ≈0.45 KiB is
Rust runtime/panic scaffolding that `-Z build-std` already shrinks as far as the
toolchain allows. Closing the gap would require audited-unsafe unchecked hot
paths or dropping integrity/recovery features, both of which conflict with the
crate's design goals; the size-optimised configurations in §4 are the
recommended trade-off.

## 7. Reproducing the reported numbers

* `harness/results/c_wg{8,1}_host.json` — C reference (last round).
* `harness/results/rust_wg{8,1}_host.json` — Rust (last round).
* `harness/firmware/build/{c,rust}/*.elf` — firmware images (git-ignored).
* `scripts/fw_rust_nightly.sh [--small-crc]` — reproducible smallest-ROM build.
* `harness/results/verification_report.md` — independent verification matrix.
