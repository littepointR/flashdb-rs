# ROM / stack optimisation — round closed, parity achieved (2026-10-23)

Context: on the NUCLEO-U545RE-Q the Rust image was 14,572 B (`size` +
`small-crc`) vs 13,360 B for C FlashDB (+9.1 %), peak stack 1,456 B vs 1,240 B
(+17.4 %), and the `size` profile's TS/rollover CPU cost was +35.7 % / +24.7 %
over C. The previous revision of this note ranked the remaining levers and
estimated parity at −1.2…−1.7 KB. **This round implemented those levers and the
acceptance now passes on silicon:**

## 1. Final on-silicon acceptance — PASS (2026-10-23)

`harness/firmware/u545re/compare.ps1 -Nightly` (C reference rebuilt and flashed
on the same MCU; 11 acceptance checks, exit status 0):

| metric | C FlashDB | Rust (final config) | delta |
|---|---:|---:|---:|
| ROM (text+rodata+data) | 13 360 B | **13 304 B** | **−56 B (−0.4 %)** |
| static RAM (data+bss) | 1 072 B | **640 B** | −432 B (−40.3 %) |
| stack high-water | 1 240 B | **1 080 B** | −160 B (−12.9 %) |
| static + stack RAM | 2 312 B | **1 720 B** | −592 B (−25.6 %) |
| cycles mount | 521 628 | **274 982** | −47.3 % |
| cycles KV (set/update/churn/get/iterate) | 34 398 791 | **21 780 634** | −36.7 % |
| cycles TS (append/iterate) | 391 386 | **347 267** | −11.3 % |
| cycles rollover (1 200 appends + remount) | 2 554 303 | **2 086 645** | −18.3 % |
| cycles total | 38 160 832 | **24 811 950** | −35.0 % |

All 21 logical `RESULT` fields are identical (workload counts/checksums,
rollover window/erases/recovery, both raw-partition FNV checksums, `rc`), and
both images report the same `core_hz`.

Final configuration (all reproducible from the repo):

* `size` profile (`opt-level="z"`, fat LTO, `codegen-units=1`, `panic=abort`),
* `small-crc` (64-byte nibble CRC table),
* nightly `-Z build-std=core` + `panic="immediate-abort"` + `-Z location-detail=none`,
* LLD `--icf=all` (identical-code folding),
* `-C llvm-args=-inline-threshold=50` (see §2 — the decisive build knob),
* `board-u545` firmware with the RAM NOR simulator (the workload is C-identical).

The ROM win comes from `.text` still being larger than C's (12 452 B vs
11 168 B) being offset by the smaller `.rodata` (852 B vs 2 136 B: 64-byte
`small-crc` table vs C's 1 KiB table, and shorter label strings). Stack and CPU
are now clearly ahead.

## 2. The decisive build knob: `-C llvm-args=-inline-threshold=50`

The `-z` profile's inliner is too conservative for this workload: the hot paths
are chains of small helpers (status encode, storage read/write, sector-header
decode) called 4–30 times per record. Raising the LLVM inline threshold costs
almost no ROM and removes most of that call overhead.

Measured on silicon, stable `size` + `small-crc` board image, before the library
refactor (so the deltas are purely the flag):

| configuration | ROM | stack | TS cycles | rollover cycles |
|---|---:|---:|---:|---:|
| no flag (`-InlineThreshold -1`) | 14 572 B | 1 456 B | 531 033 | 3 184 271 |
| **threshold 50** | **14 488 B** | **1 432 B** | **414 319** | **2 502 609** |
| threshold 64 | 15 584 B | 1 432 B | 381 463 | 2 305 587 |
| threshold 75 | 15 604 B | 1 464 B | 374 639 | 2 269 670 |
| threshold 100 | 15 492 B | 1 464 B | 373 787 | 2 265 699 |

Threshold 50 is the sweet spot: it *shrinks* the image by 84 B and cuts the
`-z` TS/rollover cost by 22 % / 21 %. 56 and above cross an inlining cliff,
recovering a further ~33 k TS cycles for +568…+1 096 B ROM — only worth it if
ROM has that much slack (the refactored tree reached parity at 50, so 50 stayed).

## 3. Ranked items from the previous note — all resolved

1. **Nightly `build-std` configuration (item 1): adopted** as the size build.
   With threshold 50 it saves a further 164 B (14 488 → 14 324 B, pre-refactor)
   and is now the canonical acceptance build (`compare.ps1 -Nightly`).
2. **KV get/set frame + ROM trim (item 2): done** (`kvdb.rs`, private API only):
   * de-sret: `read_kv_header`/`read_sector_info` now fill caller-owned
     out-params; `read_sector_info` split into header-only + traversal halves;
   * internal `find_header_into`/`find_scan_header_into`/`name_matches`: the hot
     set/get/remove/GC paths no longer build the 88-byte `KvEntry` (with its
     64-byte key buffer) — they stream the key hash / compare in 32-byte chunks;
   * `read_value_verified` takes a `KvHeader`; the truncated-value tail and the
     damaged-flash scanner are `#[cold]` out of line;
   * `alloc_scan` is a single out-param pass with early exit; `update_sec_status`
     operates on `cur_sec` through split borrows; GC `move_kv` streams the key
     hash from flash (no name buffer) and its recovery-only duplicate scan is
     `#[cold]`; exact-size header read/write buffers; byte-wise magic scan
     (removed the last `low::get_u32` caller); one out-of-line `write_status`
     wrapper instead of 11 inlined status-write expansions.
   * QEMU (`size`+small-crc+phases): `.text` 13 348 → 12 292 B (−1 056),
     headline stack 1 432 → 1 024 B (−408): `st_gc` 1 472→1 056,
     `st_get` 1 048→728, `st_set` 992→688, `st_iter` 824→760.
3. **KV write-path restructure (item 3): done** — same work as item 2 (the
   alloc/GC/move chain now shares the out-param sector walk; `alloc_kv`+
   `gc_collect_by_free_size`+`move_kv` are ~430 B smaller together).
4. **Harness trim (item 4): partially done** — the board+`stack-phases` runtime
   bug was fixed (`st_ts_append` diagnostic used a timestamp below
   `last_time()` after the rollover phase), and board rollover/print paths were
   consolidated by the earlier round. A further ~100–150 B is measured but
   **not landed**: print labels are passed as fat `&[u8]` slices whose length is
   never used, so a thin-pointer label emitter would roughly halve the per-field
   cost (`print_rollover_result` alone is 220 B). Left as future work to keep
   this round's passing build frozen.
5. **ICF (item 5): done** — `--icf=all` is now enabled in all board links and
   measured at **−28 B** ROM. (QEMU/GNU-ld builds can opt into rust-lld with
   `scripts/fw_rust.ps1 -Icf`.)

Additional (not in the ranked list): a `wg <= 8` fast path in
`Storage::write_status` encodes the single status byte directly instead of
zeroing a 32-byte scratch and calling `encode_status`. Every inlined call site
had been emitting a 32-byte `__aeabi_memclr` before each one-byte status write;
on the board this took TS 409 509 → 348 888 cycles and rollover
2 461 714 → 2 095 317, plus ~350 B of `.text`. (The same idea does not apply to
`read_status`, which must scan the whole table.)

## 4. Stack: per-phase attribution (QEMU firmware, `--phases`)

| phase | C | Rust pre-round (`size`) | Rust final (`size`, th50) | final (`release`, th50) |
|---|---:|---:|---:|---:|
| one KV set | 736 | 992 | **688** | 608 |
| one KV get | 720 | 1 048 | **728** | 632 |
| KV iterate | 560 | 824 | **760** | 656 |
| GC churn | 1 248 | 1 472 | **1 056** | 976 |
| one TS append | 528 | 704 | **568** | 496 |
| TS iterate | 568 | 784 | **768** | 656 |
| headline (`board`, final) | **1 240** | 1 456 | **1 072** | — |

The headline improved by −384 B; the remaining `st_ts_iter`/`st_iter` excess is
dominated by the harness's own phase-wrapper frames (~300–380 B in the `size`
profile) plus the public `KvEntry` (88 B) that the iterator must yield — i.e.
measurement scaffolding and public API, not library hot frames. A
`-Z emit-stack-sizes` report helper now exists:
`harness/firmware/tools/stack_sizes.py`.

## 5. Remaining measured headroom (not needed for acceptance)

* `harness/firmware/rust/main.rs` harness trim: thin-pointer print labels,
  ~100–150 B `.text` (measured budget, unlanded).
* Nightly `-Z build-std-features=optimize_for_size` (−80 B) and LLD
  `--lto-O3` (−48 B), compose to **−124 B** (measured at the pre-refactor
  checkpoint; both are now selectable knobs: `-OptimizeForSize`, `-LtoO3`).
* `.text` is still +1 276 B vs C; with `small-crc` the rodata advantage
  (−1 284 B) is what buys the ROM parity, so further ROM wins mean text work.
* For cycle experiments without hardware: `scripts/fw_rust.ps1 -Cycles`
  (SysTick + QEMU `-icount`; 40 instructions/tick, deterministic; use ratios,
  not absolutes — the board ROM figure must come from a board build).

## 6. Reproduce

```powershell
# full acceptance on the board (C rebuilt, Rust nightly build, all checks)
harness/firmware/u545re/compare.ps1 -Nightly

# same Rust image, build only (no hardware)
harness/firmware/u545re/run_rust.ps1 -BuildOnly -Nightly

# QEMU firmware: ROM + per-phase stack (threshold 50 is the default)
scripts/fw_rust.ps1 -Profile size -SmallCrc -Phases

# flag-free A/B reference (old documented numbers)
scripts/fw_rust.ps1 -Profile size -SmallCrc -Phases -InlineThreshold -1

# cycle proxy (QEMU, deterministic instruction-tick counter)
scripts/fw_rust.ps1 -Profile size -SmallCrc -Cycles

# per-function stack frames (nightly -Z emit-stack-sizes ELF)
harness/firmware/tools/stack_sizes.py <elf>
```

The three acceptance reports behind the tables above are committed as
[`docs/acceptance/board_compare_nightly.txt`](acceptance/board_compare_nightly.txt),
[`docs/acceptance/board_compare_size_stable.txt`](acceptance/board_compare_size_stable.txt)
and [`docs/acceptance/board_compare_release_stable.txt`](acceptance/board_compare_release_stable.txt).

