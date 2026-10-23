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
* **Timing.** `CLOCK_MONOTONIC`; the §2/§3 tables were produced by one 5-run
  round per implementation on Windows 11 (C: gcc 11.4 under WSL; Rust: rustc
  1.99 release) and report the median `ns/op`. Earlier revisions used the
  minimum of 3 interleaved C-then-Rust rounds (15 samples); the
  `scripts/bench_report.py --rounds` aggregator still supports that protocol.
  Flash counters are deterministic and do not depend on host load.
* **Checksums.** Final-state FNV-1a-32 snapshots (KV sorted by key; TS in
  forward order) and range-query checksums. Identical checksums prove both
  databases contain the same logical data after the workload.
* **Primary performance criterion** is flash traffic (reads/writes/erases and
  bytes): it is what costs time on real NOR/NAND. Host wall-clock is secondary
  and is dominated by the simulator's (zero-latency) memory accesses.

## 2. Host benchmark — write granularity 8

Measured 2026-10-23 on Windows 11 (Intel Core i9-14900KF): the C reference is
built with gcc 11.4 (Ubuntu 22.04 under WSL, `-O2`), the Rust benchmark with
rustc 1.99.0 (`--release`, thin LTO). Each harness runs 5 times and the table
reports the median `ns/op` (the deterministic flash counters are exact).
Absolute `ns/op` values are not comparable with the older macOS numbers, but
the same-host C↔Rust ratios are.

| scenario | ops | ns/op C | ns/op Rust | ratio | reads C | reads Rust | writes C | writes Rust | erases C | erases Rust | weighted C | weighted Rust | checksum |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| kv_set_small | 64 | 228.2 | 59.4 | 0.26x | 442 | 213 | 434 | 434 | 0 | 0 | 30,400 | 25,803 | same |
| kv_update_1k | 1000 | 349.8 | 147.3 | 0.42x | 11,146 | 7,781 | 7,089 | 7,089 | 14 | 14 | 6,598,538 | 6,561,516 | same |
| kv_get_hot | 2000 | 122.0 | 68.7 | 0.56x | 12,000 | 8,000 | 0 | 0 | 0 | 0 | 330,740 | 138,740 | same |
| kv_get_miss | 500 | 2,575.3 | 319.6 | 0.12x | 79,500 | 18,500 | 0 | 0 | 0 | 0 | 1,606,000 | 502,000 | same |
| kv_delete | 192 | 3,345.0 | 445.3 | 0.13x | 38,571 | 10,280 | 580 | 580 | 0 | 0 | 838,162 | 300,455 | same |
| kv_iterate | 100 | 2,568.8 | 509.0 | 0.20x | 15,900 | 6,900 | 0 | 0 | 0 | 0 | 321,200 | 115,400 | same |
| kv_mount | 100 | 4,828.4 | 330.0 | 0.07x | 23,300 | 4,100 | 0 | 0 | 0 | 0 | 553,200 | 108,400 | same |
| kv_large | 50 | 4,754.1 | 1,926.0 | 0.41x | 3,391 | 2,841 | 438 | 438 | 14 | 14 | 6,363,544 | 6,344,813 | same |
| tsl_append | 308 | 51.2 | 11.4 | 0.22x | 3 | 3 | 1,260 | 1,260 | 0 | 0 | 142,452 | 142,452 | same |
| tsl_append_rollover | 924 | 49.9 | 11.8 | 0.24x | 19 | 19 | 3,791 | 3,791 | 8 | 8 | 3,704,886 | 3,704,886 | same |
| tsl_iter_fwd | 20 | 2,111.1 | 2,045.0 | 0.97x | 6,240 | 6,240 | 0 | 0 | 0 | 0 | 126,720 | 126,720 | same |
| tsl_iter_rev | 20 | 2,055.8 | 2,660.0 | 1.29x | 6,240 | 6,240 | 0 | 0 | 0 | 0 | 126,720 | 126,720 | same |
| tsl_by_time | 500 | 166.8 | 192.0 | 1.15x | 10,069 | 10,069 | 0 | 0 | 0 | 0 | 249,188 | 249,188 | same |
| tsl_query_count | 500 | 164.8 | 190.8 | 1.16x | 10,069 | 10,069 | 0 | 0 | 0 | 0 | 249,188 | 249,188 | same |
| tsl_mount | 50 | 128.9 | 38.0 | 0.29x | 400 | 200 | 0 | 0 | 0 | 0 | 17,600 | 8,800 | same |
| tsl_clean | 20 | 14,518.5 | 3,420.0 | 0.24x | 160 | 80 | 25,360 | 25,360 | 80 | 80 | 35,625,440 | 35,621,920 | same |

Rust is faster or equal in **13/16** scenarios. The three host-side exceptions
are TSDB reverse / by-time / query-count iteration: the flash traffic is
identical, and the extra CPU is the simulator's zero-latency in-memory reads
(real NOR reads cost orders of magnitude more). Writes and erases are identical
in every scenario, and all checksums match.

## 3. Host benchmark — write granularity 1 (bit-level NOR)

Same host/toolchain as §2 (2026-10-23, Windows 11, gcc 11.4 / rustc 1.99,
median of 5 runs).

| scenario | ops | ns/op C | ns/op Rust | ratio | reads C | reads Rust | writes C | writes Rust | erases C | erases Rust | weighted C | weighted Rust | checksum |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| kv_set_small | 64 | 231.7 | 60.9 | 0.26x | 442 | 213 | 434 | 434 | 0 | 0 | 29,836 | 25,289 | same |
| kv_update_1k | 1000 | 331.2 | 139.7 | 0.42x | 11,110 | 7,707 | 7,057 | 7,057 | 13 | 13 | 6,176,890 | 6,123,610 | same |
| kv_get_hot | 2000 | 118.3 | 68.2 | 0.58x | 12,000 | 8,000 | 0 | 0 | 0 | 0 | 322,740 | 130,740 | same |
| kv_get_miss | 500 | 2,422.7 | 290.6 | 0.12x | 79,500 | 18,500 | 0 | 0 | 0 | 0 | 1,542,000 | 428,000 | same |
| kv_delete | 192 | 3,182.5 | 421.4 | 0.13x | 38,579 | 10,131 | 579 | 579 | 0 | 0 | 807,312 | 263,031 | same |
| kv_iterate | 100 | 2,475.8 | 499.0 | 0.20x | 15,900 | 6,900 | 0 | 0 | 0 | 0 | 308,400 | 100,600 | same |
| kv_mount | 100 | 4,390.5 | 309.0 | 0.07x | 23,800 | 4,100 | 0 | 0 | 0 | 0 | 554,800 | 92,000 | same |
| kv_large | 50 | 4,751.9 | 1,914.0 | 0.40x | 3,391 | 2,841 | 410 | 410 | 14 | 14 | 6,363,140 | 6,343,659 | same |
| tsl_append | 336 | 48.2 | 11.3 | 0.23x | 3 | 3 | 1,372 | 1,372 | 0 | 0 | 155,296 | 155,296 | same |
| tsl_append_rollover | 1008 | 50.5 | 11.6 | 0.23x | 19 | 19 | 4,127 | 4,127 | 8 | 8 | 3,743,298 | 3,743,298 | same |
| tsl_iter_fwd | 20 | 2,676.2 | 2,345.0 | 0.88x | 6,800 | 6,800 | 0 | 0 | 0 | 0 | 110,080 | 110,080 | same |
| tsl_iter_rev | 20 | 2,550.2 | 2,935.0 | 1.15x | 6,800 | 6,800 | 0 | 0 | 0 | 0 | 110,080 | 110,080 | same |
| tsl_by_time | 500 | 229.0 | 212.8 | 0.93x | 11,336 | 11,336 | 0 | 0 | 0 | 0 | 213,248 | 213,248 | same |
| tsl_query_count | 500 | 224.2 | 210.2 | 0.94x | 11,336 | 11,336 | 0 | 0 | 0 | 0 | 213,248 | 213,248 | same |
| tsl_mount | 50 | 133.3 | 36.0 | 0.27x | 400 | 200 | 0 | 0 | 0 | 0 | 12,800 | 6,400 | same |
| tsl_clean | 20 | 16,316.9 | 3,815.0 | 0.23x | 160 | 80 | 27,600 | 27,600 | 80 | 80 | 35,881,120 | 35,878,560 | same |

Rust is faster or equal in **15/16** scenarios (only reverse iteration is
slower on the host); writes/erases and checksums are identical everywhere.


Both firmwares run the **same** workload under QEMU (`mps2-an385`, Cortex-M3,
semihosting): 64 KV sets, 32 updates, 1500-update GC churn, 64 verified gets,
full KV iteration, 200 TSDB appends and full TSDB iteration, checksummed.
C is `arm-none-eabi-gcc -Os` with FAL; Rust is `thumbv7m-none-eabi`,
fat LTO, `panic=abort`, tiny `__aeabi_mem*` overrides (equivalent to the C
harness's libc stubs). The simulated flash lives in a detached `NOLOAD` region
in both images, so it counts as neither ROM nor MCU RAM. The Rust numbers below
use the acceptance configuration: `small-crc` plus
`-C llvm-args=-inline-threshold=50` (see §4b and `docs/OPTIMIZATION.md`).

| metric | C FlashDB (-Os) | Rust `opt-level="s"` | Rust `opt-level="z"` |
|---|---:|---:|---:|
| `.text` | 9 720 B | 12 844 B | 11 124 B |
| `.rodata` | 1 632 B | 508 B | 508 B |
| `.data` | 56 B | 0 | 0 |
| **ROM total** | **11 408 B** | **13 352 B** (+17%) | **11 632 B** (+2.0%) |
| `.bss` (static DB RAM) | 1 008 B | 624 B | 624 B |
| **static RAM** | **1 064 B** | **624 B** (−41%) | **624 B** (−41%) |
| stack high-water | 1 192 B | **1 016 B** (−15%) | **1 016 B** (−15%) |
| **static + stack RAM** | **2 256 B** | **1 640 B** (−27%) | **1 640 B** (−27%) |
| workload checksums | KV `0x4b3486a4`, TS `0x10d90680` | identical | identical |

Per-phase stack high-water (diagnostic builds; `--phases`, threshold-50 codegen):

| phase | C | Rust `-s` | Rust `-z` |
|---|---:|---:|---:|
| mount/init | 752 B | 328 B | 368 B |
| KV set | 736 B | 616 B | 696 B |
| KV get | 720 B | 640 B | 736 B |
| KV iterate | 560 B | 664 B | 768 B |
| KV GC churn | 1 248 B | 984 B | 1 064 B |
| TS append | 528 B | 504 B | 576 B |
| TS iterate | 568 B | 664 B | 776 B |

The QEMU firmware is still +224 B ROM over C in the `-z` configuration (the
board acceptance below passes because the board harness has a different
rodata/code split); RAM and stack are clearly better in both profiles.

### Size/performance options

The library exposes one build option that trades ROM for CPU; it keeps the
on-flash format and behaviour identical.

| configuration | ROM (QEMU, `-z`) | note |
|---|---:|---|
| default (256-entry CRC table) | 12 732 B | threshold-50 codegen |
| default + `--features small-crc` | **11 632 B** | 64-byte CRC table; accepted configuration |
| + `-InlineThreshold -1` (LLVM `-Oz` default) | 11 644 B | flag-free A/B reference |
| nightly `build-std` + `small-crc` | board ROM 13 204 B | smallest photographed image (`compare.ps1 -Nightly`) |

The size build also gets a free CPU win from the codegen flag: on silicon
`-C llvm-args=-inline-threshold=50` took the `size` profile from TS 531 033 to
414 319 cycles and rollover 3 184 271 to 2 502 609 cycles before the library
refactor (a further −65 k / −407 k came from the refactor), at −84 B ROM. See
`docs/OPTIMIZATION.md` for the full threshold sweep.

### 4b. On-silicon acceptance (NUCLEO-U545RE-Q)

`harness/firmware/u545re/compare.ps1` builds and flashes both images on the
same STM32U545RE and checks every metric plus 21 logical result fields. The
acceptance passes in two configurations (reports:
`harness/firmware/build/u545re/board_compare*.txt`):

| metric (on silicon) | C FlashDB | Rust `size` (stable) | Rust nightly | Rust `release` |
|---|---:|---:|---:|---:|
| ROM (text+rodata+data) | 13 360 B | **13 228 B** | **13 204 B** | 15 128 B |
| static RAM | 1 072 B | **640 B** | **640 B** | **640 B** |
| stack high-water | 1 240 B | **1 080 B** | **1 080 B** | **1 048 B** |
| static + stack | 2 312 B | **1 720 B** | **1 720 B** | **1 688 B** |
| cycles mount | 521 663 | **274 790** | **274 820** | **338 808** |
| cycles KV | 34 398 933 | **21 703 993** | **21 600 093** | **18 529 245** |
| cycles TS | 391 359 | **343 462** | **344 469** | **291 349** |
| cycles rollover | 2 554 338 | **2 069 693** | **2 075 206** | **1 828 442** |
| cycles total | 38 161 041 | **24 714 339** | **24 616 989** | **21 277 175** |
| acceptance verdict | — | **PASS (11/11)** | **PASS (11/11)** | ROM only FAIL |

Reports for all three runs are committed under `docs/acceptance/`
(`board_compare_nightly.txt`, `board_compare_size_stable.txt`,
`board_compare_release_stable.txt`).

All 21 logical fields (workload counts/checksums, rollover state, both raw
partition checksums, `rc`) are identical between C and Rust in every
configuration, and both images leave byte-identical flash partitions.

* **Stable `size` + `small-crc` + `-InlineThreshold 50`** is the recommended
  configuration: no nightly toolchain needed, all checks pass, ROM −132 B.
* **Nightly `-Z build-std` + `panic=immediate-abort` + ICF** is the smallest
  image (ROM −156 B) and needs `rustup toolchain install nightly --component rust-src`.
* **`release` (`-s`)** is the CPU-fastest build (total −44.2 %) but its ROM is
  +1 768 B, so it fails only the ROM check.

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
| **this round** | |
| `-C llvm-args=-inline-threshold=50` (LLVM `-Oz` default is too conservative) | **−84 B board ROM, TS −117 k cycles, rollover −682 k** |
| LLD `--icf=all` | **−28 B** |
| KV refactor (out-param headers/sectors, addr-only lookups, GC/alloc/move restructure) | **−1 056 B `text` (QEMU phases), −408 B stack** |
| TS refactor (de-sret decode, cold seal/scan, iterator fast path) | **−190 B TS symbols, −136 B st_ts_append** |
| `Storage::write_status` `wg<=8` direct byte (no 32-byte scratch/memclr) | **≈−350 B `text`, TS −60 k cycles on silicon** |
| **measured, reverted** | |
| global `#[inline(never)]` | +3 400 B |
| opaque `black_box` call barriers | +604 B |
| `#[inline(always)]` on `read_tsl` / `find_scan` / u32 helpers | +12…268 B |
| splitting `read_sector_info` into header+traversal | +160 B |
| `min-size` bounds-check removal (LLVM already elides most checks) | ±0 B |
| LLVM merge-functions / outliner / MIR flags, thin LTO, CU=16 | 0…+1 800 B |
| broad `#[inline(always)]` over storage/low leaf helpers | TS −65 k, but **+1 492 B** (rejected; the threshold flag is cheaper) |

The remaining `.text` is still larger than C's (11 124 B vs 9 720 B in the QEMU
`-z` build), but the `small-crc` 64-byte table and shorter strings make
`.rodata` 1 124 B smaller, so the QEMU ROM is only +224 B. On the real board the
ledger flips: Rust `.text` +1 184 B but `.rodata` −1 284 B, and C also carries
56 B `.data` → ROM **−156 B** (nightly) / **−132 B** (stable), i.e. at parity.
Further ROM work means `.text` work; the measured optional levers
(nightly `optimize_for_size` + `--lto-O3` are now no-ops on the final tree,
thin-pointer harness labels ~100–150 B) are listed in `OPTIMIZATION.md`.

## 5. On-flash compatibility (differential)

`scripts/rust_compat.sh` now covers **all six FlashDB-C write granularities**
(1, 8, 32, 64, 128, 256 bits; default `WG_LIST="8 1 32 64 128 256"`): for each
one, both implementations write the same deterministic 32 KiB image (KV + TS
partitions), then each mounts the other's image. Current result:
**95/97 enforced checks pass, 0 FAIL, 2 expected XFAIL, exit 0.**

* Rust-written images are **byte-identical** (SHA-256 on 32 768 bytes) to
  C-written images for `WRITE_GRAN` 1, 8, 32, 64 and 128.
* C mounts Rust images, and Rust mounts C images without formatting or erasing;
  the 29-line logical summaries (key/length/value checksum,
  timestamp/status/payload checksum) are identical both ways for all six
  granularities.
* **C-compat bug fixed in this round:** for `WRITE_GRAN > 8`, FlashDB-C encodes
  a status transition as a granule whose first byte is `0x00` and whose
  remaining bytes stay erased (`0xFF`), and decodes the highest zero *first
  byte*; the Rust encoder zeroed the whole granule and the decoder required the
  whole granule to be zero. Rust therefore could not read C images with
  `WRITE_GRAN` 32/64/128/256 (they mounted as empty) and its own images were
  not interchangeable (`crates/flashdb/src/low.rs`, private helpers only).
  With the fix all byte-identical granularities above are green and 1/8 are
  unchanged.
* **`WRITE_GRAN=256` is an expected XFAIL**: all logical/cross-mount checks
  pass, but FlashDB-C's image tool is itself nondeterministic — its
  `write_tsl()` programmes 20 uninitialised padding bytes per record (C's own
  selfcheck fails), while Rust writes erased `0xFF`. The script only XFAILs
  after proving C nondeterminism and that the diff is exactly those padding
  bytes; any other difference is a hard failure.
* Interrupted-operation recovery: the Rust test suite injects a flash fault at
  every write during a workload and remounts; the DB is always consistent and
  usable afterwards (C has no automated equivalent).
* The `continue_ff_addr` traversal rewrite was additionally differential-fuzzed
  against C's `_fdb_continue_ff_addr`: 1.8 M adversarial/random NOR patterns
  (granules 1…256 bits) → 0 mismatches, plus 31 corrupted-image read/write
  variants where C and Rust outputs are identical.


`cargo test -p flashdb --release` runs **135 tests (128 test functions + 7
doctests)**, including the original C test cases ported assertion-for-assertion
(`kvdb_c_parity.rs`, `tsdb_c_parity.rs`), byte-level layout tests for all six
write granularities, crash recovery (including across a ring wrap), the
every-flash-op fault sweeps and error-path coverage added this round, rollover
and iteration boundary cases, and error-type surface tests. Line coverage of the
crate is **93.84 %** (functions 98.94 %; `error.rs`/`flash.rs` 100 %, `low.rs`
96.36 %, `kvdb.rs` 93.34 %, `tsdb.rs` 94.54 %, `storage.rs` 82.40 %) —
reproduce with `cargo llvm-cov -p flashdb --release --summary-only` (identical
with `--features std`). See [`docs/TESTING.md`](TESTING.md) for the full test
inventory (host, QEMU, C reference, real-flash and CI layers) and the list of the
remaining uncovered defensive branches.


## 6. Status summary

| objective | result |
|---|---|
| functional parity (KVDB + TSDB, C-compatible flash format) | **achieved** — byte-identical host images for `WRITE_GRAN` 1/8/32/64/128 (95/97 enforced checks, wg 256 C-side XFAIL); 135 tests green; on silicon C and Rust print identical 21-field `RESULT` output and the same raw-partition image checksums; a real C-compat bug in the `WRITE_GRAN>8` status granule was found and fixed this round |
| real internal-flash behaviour | **achieved** — crate image written on real STM32U5 internal flash (`WRITE_GRAN=128`) is byte-identical to the host model (6/6 fields); 50/50 externally-reset power-loss cycles recover; 20-boot/19-reset soak passes; 240 s endurance = 22 493 P/E cycles on 8 pages, 0 errors; 60 s KV+TS long-run = 1 405 erases, 0 errors |
| performance at least C | **achieved** — flash traffic never higher (KV 2–5x fewer reads; TS mount/clean now half the reads); host CPU faster in 13/16 (wg 8) and 15/16 (wg 1) scenarios; **on silicon every phase is faster than C**: `release` total −44.2 %, stable `size` −35.2 %, nightly −35.5 % |
| RAM at least C | **achieved** — QEMU static 624 B vs 1 064 B, stack 1 016 B vs 1 192 B; on silicon static 640 B vs 1 072 B, stack 1 080 B vs 1 240 B, static+stack 1 720 B vs 2 312 B |
| ROM at least C | **achieved on silicon** — **13 228 B** (stable `size`+`small-crc`+`-InlineThreshold 50`) / **13 204 B** (nightly) vs C 13 360 B; the C-compat status fix also removed ~110 B. QEMU harness still +224 B because its rodata split differs |
| stack at least C | **achieved** — QEMU 1 008 B vs 1 192 B (`-z`); on silicon 1 080 B vs 1 240 B (`size`), 1 048 B (`release`) |

All five acceptance rows are green in the on-silicon verdict (`compare.ps1`,
11/11 checks), and the real-flash parity / fault-injection / soak / endurance /
long-run suites are committed and reproducible. Remaining opportunities are
qualification projects, not functional gaps: nightly `optimize_for_size` +
`--lto-O3` measured **unchanged** `.text` on the current tree (the earlier
−124 B figure belonged to a pre-refactor checkpoint), harness label trimming
(~100–150 B, optional), and the CPU-for-ROM `release` trade-off.


## 7. Reproducing the reported numbers

* Host benchmarks (§2/§3): C side `scripts/c_bench.sh --wg 8` (and `--wg 1`)
  under Linux/WSL with `CC=gcc`; Rust side
  `cargo run -p flashdb-bench --release -- --wg 8 --runs 5 --with-ts`; compare
  with `python scripts/compare_bench.py --wg 8 <c.json> <rust.json>`. The JSON
  files are written to `harness/results/` (git-ignored).
* Host C↔Rust image matrix (§5): `scripts/rust_compat.sh` (default
  `WG_LIST="8 1 32 64 128 256"`); under WSL use
  `wsl -e bash -lc 'export PATH=$HOME/.cargo/bin:$PATH; cd /mnt/d/.../flashdb-rs && CARGO_TARGET_DIR=target/flash-parity bash scripts/rust_compat.sh'`.
* `harness/firmware/build/{c,rust}/*.elf` — firmware images (git-ignored).
* `harness/firmware/u545re/compare.ps1 -Nightly` — full on-silicon acceptance
  (C rebuilt and flashed, 11 checks, exit status 0). The stable-toolchain
  equivalent is `compare.ps1 -RustProfile size -RustFeatures small-crc`; both
  pass. Reports: `harness/firmware/build/u545re/board_compare*.txt` (copies in
  `docs/acceptance/`).
* Real internal flash: `run_flash_parity.ps1`, `run_flash_fault.ps1`,
  `run_flash_reboot_soak.ps1`, `run_flash_tests.ps1 -Test endurance|longrun`
  (see `docs/TESTING.md` §6 for measured results and options).
* `scripts/fw_rust.ps1 -Profile size -SmallCrc -Phases` — QEMU ROM + per-phase
  stack (threshold 50 default; `-InlineThreshold -1` for the old flag-free
  baseline, `-Cycles` for the deterministic QEMU cycle proxy).
* `harness/firmware/tools/stack_sizes.py <elf>` — per-function stack frames from
  a nightly `-Z emit-stack-sizes` build.
* `cargo llvm-cov -p flashdb --release --summary-only` — host line/function
  coverage (also with `--features std`); full test inventory in
  [`TESTING.md`](TESTING.md).
* [`OPTIMIZATION.md`](OPTIMIZATION.md) — this round's measured optimisation
  results (build knobs, before/after checkpoints, remaining headroom).