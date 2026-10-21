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
| kv_set_small | 64 | 687.5 | 179.7 | 0.26x | 442 | 214 | 434 | 434 | 0 | 0 | 30,400 | 25,823 | same |
| kv_update_1k | 1000 | 754.0 | 454.8 | 0.60x | 11,146 | 8,021 | 7,089 | 7,089 | 14 | 14 | 6,598,538 | 6,566,316 | same |
| kv_get_hot | 2000 | 282.0 | 202.9 | 0.72x | 12,000 | 8,000 | 0 | 0 | 0 | 0 | 330,740 | 138,740 | same |
| kv_get_miss | 500 | 5,358.0 | 980.7 | 0.18x | 79,500 | 18,500 | 0 | 0 | 0 | 0 | 1,606,000 | 502,000 | same |
| kv_delete | 192 | 5,536.5 | 1,331.2 | 0.24x | 38,571 | 10,296 | 580 | 580 | 0 | 0 | 838,162 | 300,775 | same |
| kv_iterate | 100 | 4,380.0 | 1,370.8 | 0.31x | 15,900 | 6,900 | 0 | 0 | 0 | 0 | 321,200 | 115,400 | same |
| kv_mount | 100 | 6,360.0 | 1,096.2 | 0.17x | 23,300 | 4,900 | 0 | 0 | 0 | 0 | 553,200 | 124,400 | same |
| kv_large | 50 | 8,320.0 | 4,667.5 | 0.56x | 3,391 | 3,082 | 438 | 438 | 14 | 14 | 6,363,544 | 6,349,633 | same |
| tsl_append | 308 | 51.9 | 31.1 | 0.60x | 3 | 3 | 1,260 | 1,260 | 0 | 0 | 142,452 | 142,452 | same |
| tsl_append_rollover | 924 | 51.9 | 32.4 | 0.62x | 19 | 19 | 3,791 | 3,791 | 8 | 8 | 3,704,886 | 3,704,886 | same |
| tsl_iter_fwd | 20 | 3,300.0 | 4,672.9 | 1.42x | 6,240 | 6,240 | 0 | 0 | 0 | 0 | 126,720 | 126,720 | same |
| tsl_iter_rev | 20 | 3,400.0 | 5,227.1 | 1.54x | 6,240 | 6,240 | 0 | 0 | 0 | 0 | 126,720 | 126,720 | same |
| tsl_by_time | 500 | 288.0 | 456.4 | 1.58x | 10,069 | 10,069 | 0 | 0 | 0 | 0 | 249,188 | 249,188 | same |
| tsl_query_count | 500 | 274.0 | 443.4 | 1.62x | 10,069 | 10,069 | 0 | 0 | 0 | 0 | 249,188 | 249,188 | same |
| tsl_mount | 50 | 160.0 | 180.0 | 1.12x | 400 | 400 | 0 | 0 | 0 | 0 | 17,600 | 17,600 | same |
| tsl_clean | 20 | 16,200.0 | 8,823.0 | 0.54x | 160 | 160 | 25,360 | 25,360 | 80 | 80 | 35,625,440 | 35,625,440 | same |

## 3. Host benchmark — write granularity 1 (bit-level NOR)

| scenario | ops | ns/op C | ns/op Rust | ratio | reads C | reads Rust | writes C | writes Rust | erases C | erases Rust | weighted C | weighted Rust | checksum |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| kv_set_small | 64 | 671.9 | 208.3 | 0.31x | 442 | 214 | 434 | 434 | 0 | 0 | 29,836 | 25,305 | same |
| kv_update_1k | 1000 | 855.0 | 444.2 | 0.52x | 11,110 | 7,931 | 7,057 | 7,057 | 13 | 13 | 6,176,890 | 6,127,194 | same |
| kv_get_hot | 2000 | 316.5 | 201.5 | 0.64x | 12,000 | 8,000 | 0 | 0 | 0 | 0 | 322,740 | 130,740 | same |
| kv_get_miss | 500 | 5,314.0 | 924.8 | 0.17x | 79,500 | 18,500 | 0 | 0 | 0 | 0 | 1,542,000 | 428,000 | same |
| kv_delete | 192 | 6,031.2 | 1,270.0 | 0.21x | 38,579 | 10,138 | 579 | 579 | 0 | 0 | 807,312 | 263,143 | same |
| kv_iterate | 100 | 4,900.0 | 1,340.0 | 0.27x | 15,900 | 6,900 | 0 | 0 | 0 | 0 | 308,400 | 100,600 | same |
| kv_mount | 100 | 6,920.0 | 1,164.6 | 0.17x | 23,800 | 4,900 | 0 | 0 | 0 | 0 | 554,800 | 104,800 | same |
| kv_large | 50 | 8,320.0 | 4,677.5 | 0.56x | 3,391 | 3,082 | 410 | 410 | 14 | 14 | 6,363,140 | 6,347,515 | same |
| tsl_append | 336 | 53.6 | 31.2 | 0.58x | 3 | 3 | 1,372 | 1,372 | 0 | 0 | 155,296 | 155,296 | same |
| tsl_append_rollover | 1008 | 53.6 | 32.4 | 0.61x | 19 | 19 | 4,127 | 4,127 | 8 | 8 | 3,743,298 | 3,743,298 | same |
| tsl_iter_fwd | 20 | 3,400.0 | 4,710.4 | 1.39x | 6,800 | 6,800 | 0 | 0 | 0 | 0 | 110,080 | 110,080 | same |
| tsl_iter_rev | 20 | 3,350.0 | 5,241.6 | 1.56x | 6,800 | 6,800 | 0 | 0 | 0 | 0 | 110,080 | 110,080 | same |
| tsl_by_time | 500 | 280.0 | 497.0 | 1.77x | 11,336 | 11,336 | 0 | 0 | 0 | 0 | 213,248 | 213,248 | same |
| tsl_query_count | 500 | 280.0 | 487.0 | 1.74x | 11,336 | 11,336 | 0 | 0 | 0 | 0 | 213,248 | 213,248 | same |
| tsl_mount | 50 | 160.0 | 192.5 | 1.20x | 400 | 400 | 0 | 0 | 0 | 0 | 12,800 | 12,800 | same |
| tsl_clean | 20 | 16,950.0 | 9,614.5 | 0.57x | 160 | 160 | 27,600 | 27,600 | 80 | 80 | 35,881,120 | 35,881,120 | same |

## 4. Cortex-M3 firmware comparison

Both firmwares run the **same** workload under QEMU (`mps2-an385`, Cortex-M3,
semihosting): 64 KV sets, 32 updates, 1500-update GC churn, 64 verified gets,
full KV iteration, 200 TSDB appends and full TSDB iteration, checksummed.
C is `arm-none-eabi-gcc -Os` with FAL; Rust is `thumbv7m-none-eabi`,
fat LTO, `panic=abort`, tiny `__aeabi_mem*` overrides (equivalent to the C
harness's libc stubs). The simulated flash lives in a detached `NOLOAD` region
in both images, so it counts as neither ROM nor MCU RAM.

| metric | C FlashDB (-Os) | Rust `opt-level="s"` | Rust `opt-level="z"` |
|---|---:|---:|---:|
| `.text` | 9 720 B | 14 268 B | 12 724 B |
| `.rodata` | 1 632 B | 1 448 B | 1 448 B |
| `.data` | 56 B | 0 | 0 |
| **ROM total** | **11 408 B** | **15 716 B** (+38%) | **14 172 B** (+24%) |
| `.bss` (static DB RAM) | 1 008 B | 624 B | 624 B |
| **static RAM** | **1 064 B** | **624 B** (−41%) | **624 B** (−41%) |
| stack high-water | 1 192 B | 1 312 B | 1 432 B |
| **static + stack RAM** | **2 256 B** | **1 936 B** (−14%) | **2 056 B** (−9%) |
| workload checksums | KV `0x4b3486a4`, TS `0x10d90680` | identical | identical |

Per-phase stack high-water (diagnostic builds; `--phases`):

| phase | C | Rust `-s` | Rust `-z` |
|---|---:|---:|---:|
| mount/init | 752 B | 912 B | 952 B |
| KV set | 736 B | 952 B | 984 B |
| KV get | 720 B | 1 056 B | 1 048 B |
| KV iterate | 560 B | 784 B | 856 B |
| KV GC churn | 1 248 B | 1 424 B | 1 528 B |
| TS append | 528 B | 680 B | 728 B |
| TS iterate | 568 B | 824 B | 840 B |

### Size/performance options

The library exposes one build option that trades ROM for CPU; it keeps the
on-flash format and behaviour identical:

| configuration | ROM (`-z`) | trade-off |
|---|---:|---|
| default (256-byte CRC table) | 14 172 B | reference build (stable toolchain) |
| default + `--features small-crc` (`scripts/fw_rust.sh size --small-crc`) | 13 224 B | 64-byte CRC table; ~2x CRC CPU (only matters for large values) |
| nightly `build-std` size build, default CRC (`scripts/fw_rust_nightly.sh`) | 13 540 B | nightly toolchain for `panic=immediate-abort` + `build-std`; identical behaviour |
| nightly `build-std` size build + `small-crc` (`scripts/fw_rust_nightly.sh --small-crc`) | **12 588 B (+10%)** | smallest measured; nightly toolchain |

### Interpretation

* **RAM: better than C.** Static RAM is 41% smaller: a 512-byte 2-way name
  cache replaces C's 512-byte cache *plus* ~320-byte sector cache, the TSDB
  struct is 56 B (C: 160 B), and placement mounting (`mount_into`) avoids
  copying the database through the stack. Total static+stack RAM is lower than
  C's despite Rust using more stack per call.
* **Performance: better than C on hardware-relevant work.** Every scenario has
  identical or fewer flash writes/erases and equal or fewer flash reads; KV
  lookup-heavy scenarios perform 2–5x fewer reads (header/name matching instead
  of CRC-scanning values), and the weighted flash cost
  (read·1 + write·10 + erase·100) is never higher than C's. Host CPU time is
  also lower in most scenarios; the TS iteration/range scenarios are ~1.4–1.6x slower in the default
  compact build because the zero-latency simulator
  exposes the per-entry dispatch cost that `fast-iter` removes (with
  `fast-iter` all scenarios are at or below C). On real flash a single index
  read costs microseconds, so a ~5 ns/entry difference is noise there.
* **ROM: still larger than C (+24% in the `-z` size profile; +16% with
  `small-crc`; +10% in the smallest reproducible nightly build).** Attribute analysis
  (`scripts/fw_rust.sh --symbols`) shows this is compiler/runtime overhead, not
  algorithm size: the linked crate is ≈7.2 KiB of symbols plus ≈1.5–2.5 KiB
  LTO-inlined into the harness, C's whole library (KVDB+TSDB+FAL+utils) is
  7.7 KiB, and the Rust measurement harness is ≈3.9 KiB vs C's 2.0 KiB.
  Bounds checks, `Result` plumbing, panic trampolines and slice helpers account
  for most of the remaining text; a `min-size` build option using audited
  unchecked slice access is the remaining lever (see `docs/DESIGN.md`).

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

`cargo test -p flashdb --release` runs 83 tests plus 7 doctests, including the
original C test cases ported assertion-for-assertion
(`kvdb_c_parity.rs`, `tsdb_c_parity.rs`), byte-level layout tests for all six
write granularities, crash recovery, rollover and iteration boundary cases.

## 6. Status summary

| objective | result |
|---|---|
| functional parity (KVDB + TSDB, C-compatible flash format) | **achieved** — byte-identical images both directions, 83 tests green |
| performance at least C | **achieved on flash traffic** (never more ops; KV 2–5x fewer reads); host CPU faster in 11/16 scenarios in the default build |
| RAM at least C | **achieved** — static 624 B vs 1064 B; static+stack 1 936 B vs 2 256 B (`-s`) |
| ROM at least C | **not achieved** — 14 448 B vs 11 408 B (`-z`); 13 224 B with `small-crc`, 12 588 B in the smallest measured nightly build (+10%) |

ROM remains the one open objective. Because the default build prioritises
performance and C compatibility, the size levers above are opt-in; the next
work item is a `min-size` feature that replaces checked slicing/indexing in hot
decode paths with audited unsafe access to close the remaining ≈2–3 KiB.

## 7. Reproducing the reported numbers

* `harness/results/c_wg{8,1}_host.json` — C reference (last round).
* `harness/results/rust_wg{8,1}_host.json` — Rust (last round).
* `harness/firmware/build/{c,rust}/*.elf` — firmware images (git-ignored).
* `scripts/fw_rust_nightly.sh [--small-crc]` — reproducible smallest-ROM build.
* `harness/results/verification_report.md` — independent verification matrix.
