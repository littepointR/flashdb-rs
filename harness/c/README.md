# C reference benchmark harness

Baseline implementation for the FlashDB → Rust comparison protocol. This
directory builds the **unmodified upstream FlashDB sources** from
`reference/FlashDB/` against a RAM-backed NOR simulator and runs every scenario
in [`../SCENARIOS.md`](../SCENARIOS.md), emitting one JSON document per
(`FDB_WRITE_GRAN`, optimization) pair.

Nothing under `reference/FlashDB/` is modified; the harness supplies the two
missing configuration headers (`fdb_cfg.h`, `fal_cfg.h`), the FAL flash device /
partition tables and the simulator (`sim_flash.c`).

## Files

| file | purpose |
|---|---|
| `fdb_cfg.h` | FAL mode, KVDB+TSDB, caches pinned to 64/8, no `FDB_DEBUG_ENABLE`, `FDB_WRITE_GRAN` **not** defined here (must come from `-D`) |
| `fal_cfg.h` | one 32 KiB NOR device `norflash0`, partitions `fdb_kvdb1` (0, 16 KiB) and `fdb_tsdb1` (16 KiB, 16 KiB) |
| `sim_flash.h/.c` | RAM NOR simulator + FAL device ops, op/byte accounting, fault-injection hook, library log routing |
| `bench.h` / `bench_common.c` | timing (`CLOCK_MONOTONIC`), FNV-1a-32 checksums, snapshot helpers, measured-phase accounting, JSON writer |
| `bench_kv.c` | KV scenarios |
| `bench_ts.c` | TS scenarios |
| `bench.c` | entry point, `--mode bench|check|probe` |
| `image_tool.c` | raw 32 KiB image writer/reader for the C⇄Rust on-flash compatibility proof |
| `../scripts/c_bench.sh` | build + run both write granularities (`-O2`, optional `-Os`) and write JSON |
| `../scripts/c_test.sh` | behavioural self-checks + image round-trip |
| `../scripts/c_image.sh` | build + `write`/`read`/`selfcheck` for `image_tool` |

Results are written to `harness/results/` (git-ignored):
`c_wg8_host.json`, `c_wg1_host.json` for `-O2`, and `*_host_os.json` for `-Os`.

## Build & run

```sh
# benchmark, default: both WGs, -O2, 5 runs -> harness/results/c_wg{8,1}_host.json
scripts/c_bench.sh

# single variant / size build
scripts/c_bench.sh --wg 8 --opt Os --runs 5
scripts/c_bench.sh --opt both --wg 8

# behavioural self-check (exit != 0 on failure)
scripts/c_test.sh

# image tool (C<->Rust flash compatibility)
scripts/c_image.sh write /tmp/c_wg8.bin 8
scripts/c_image.sh read  /tmp/rust_wg8.bin 8
scripts/c_image.sh selfcheck 8
```

Only `cc` (Apple clang), the C99/POSIX system headers and the reference
sources are needed. Compile flags (also recorded in `config.cflags`):

```
-std=c99 -O2 -Wall -Wextra -Wno-unused-parameter
-DFDB_WRITE_GRAN=8|1
-I harness/c -I reference/FlashDB/inc -I reference/FlashDB/port/fal/inc
```

Library logging is **off by default** (`FDB_DEBUG_ENABLE` undefined and
`FDB_PRINT`/`FAL_PRINTF` are routed to a disabled stderr logger); set
`C_HARNESS_VERBOSE=1` to see upstream logs. stdout is reserved for JSON /
image-tool summaries.

## Flash simulator semantics and accounting

* 32 KiB RAM device, 4096-byte erase blocks, starts fully erased (0xFF).
* **write** = bitwise AND with the new data (only 1→0; attempts to set a bit
  are counted and do not fail, like real NOR).
* **erase** must be 4096-byte aligned and sized; sets the range to 0xFF.
* Counters `reads, read_bytes, writes, write_bytes, erases, erase_bytes` are
  incremented per successful FAL op; `erase_bytes` counts erased bytes (not
  blocks). Counters are reset after each scenario's setup and snapshotted
  immediately after the timed phase; a checksum/verification afterwards does
  not pollute them.
* Fault injection: `sim_fault_arm(n)` lets n ops succeed, then fails every op
  with `-1` without touching memory (power-cut hook for the equivalence tests).

## Scenario definitions (must match the Rust harness byte-for-byte)

All values are produced by:

```
kv_value(seed, i, len):  buf[j] = (uint8_t)(seed + i*31 + j*7)
ts_payload(i):           buf[j] = (uint8_t)(0x5A + i*13 + j*5)   (32 bytes)
TS timestamps:           t_i = 1000 + i (strictly increasing)
```

| scenario | ops | setup (excluded) | measured phase |
|---|---|---|---|
| `kv_set_small` | 64 | format | k=0..63: set `"kv{k%8}"` = `kv_value(0x11,k,8)` |
| `kv_update_1k` | 1000 | format, set `counter`=`kv_value(0x22,0,32)` | k=0..999: set `counter`=`kv_value(0x22,k,32)` |
| `kv_get_hot` | 2000 | 32 keys `"key{i}"`=`kv_value(0x33,i,32)` + 128 warmup gets | 2000 gets `key{k%32}` into 64 B buf; hits = returned>0 |
| `kv_get_miss` | 500 | same populated DB | 500 gets `"absent{i}"`, i=0..499 |
| `kv_delete` | 192 | same populated DB | 3 rounds × 32 keys × {delete(key_i); set(key_i, kv_value(0x33,i,32))} = 96 del + 96 set |
| `kv_iterate` | 100 | same populated DB | 100 × full iterate, count entries + sum `value_len` (values not read) |
| `kv_mount` | 100 | same populated DB | 100 × (deinit + init), count successes |
| `kv_large` | 50 | format | k=0..49: set `"big"`=`kv_value(0x44,k,1024)` |
| `tsl_append` | successes | format, rollover off | append t=1000+i until `FDB_SAVED_FULL` or 4000; calls include the full-detection call |
| `tsl_append_rollover` | 3×capacity | format, rollover on | 3 × capacity appends, t=1000+i |
| `tsl_iter_fwd` | 20 | populate to full | 20 × forward iterate, count + sum payload len |
| `tsl_iter_rev` | 20 | populate to full | 20 × reverse iterate |
| `tsl_by_time` | 500 | populate to full | 500 × `iter_by_time(from,to)` with the protocol LCG ranges |
| `tsl_query_count` | 500 | populate to full | 500 × `query_count(from,to,WRITE)`, same ranges |
| `tsl_mount` | 50 | populate to full | 50 × (deinit + init) |
| `tsl_clean` | 20 | populate to full | 20 × (clean + repopulate to full) |

"Populate to full" = append with increasing timestamps until the DB reports
`FDB_SAVED_FULL` (rollover disabled). The `tsl_append_rollover` capacity is
measured once in a setup-phase probe (append-to-full count, reported as
`extra.capacity`); the measured phase issues exactly `3*capacity` appends.

LCG for the ranged queries:

```
state = 12345
next(): state = (state * 1103515245 + 12345) mod 2^31
per query: off = next() % 3000; len = 50 + next() % 451
           from = 1000 + off; to = from + len   (inclusive)
```

## Checksums

FNV-1a **32-bit**, reported as a decimal number:
`hash = 0x811C9DC5; for each byte: hash = (hash ^ b) * 0x01000193 (wrapping)`.

Per scenario:

| scenarios | checksum input |
|---|---|
| `kv_*` | KV snapshot: `u32(entry_count)` then entries **sorted by key**: `u32(key_len)`, key bytes, `u32(value_len)`, value bytes. For `kv_get_hot/miss` this checks the stored state (unchanged by reads); `kv_delete`/`kv_iterate`/`kv_mount` check the final state. |
| `tsl_append`, `tsl_append_rollover`, `tsl_iter_fwd`, `tsl_iter_rev`, `tsl_clean` | TS snapshot: forward-iteration records `u32(status)`, `u32(time)`, `u32(log_len)`, payload bytes; then `u32(entry_count)`. |
| `tsl_by_time`, `tsl_query_count` | `u32(500)`, then `u32(count_of_query_q)` for q=0..499. |
| `tsl_mount` | TS snapshot taken **immediately before** the mount phase, plus `extra.mount_ok`. Upstream FlashDB cannot iterate a full no-rollover DB after remount (see deviations), so post-mount iteration is not used. |

The u32 serialisation is little-endian (`b0 = v & 0xFF`, ...).

## JSON document

One object per build/variant (stdout **and** the `--out` file):

```json
{
  "impl": "c",
  "version": "fdb-2.2.99",
  "opt": "O2",
  "write_gran": 8,
  "config": {
    "write_gran": 8, "write_gran_bytes": 1, "sector_size": 4096,
    "sectors_per_db": 4, "partition_size": 16384,
    "partitions": ["fdb_kvdb1", "fdb_tsdb1"],
    "kv_cache_table_size": 64, "sector_cache_table_size": 8,
    "debug_log": false, "ts_timestamp_bits": 32, "ts_max_log_len": 32,
    "runs": 5, "opt": "O2", "compiler": "...", "host": "...", "cflags": "..."
  },
  "footprint": {"sizeof_kvdb": 1064, "sizeof_tsdb": 160, ...},
  "semantics": { ...machine-readable notes about formulas/checksums... },
  "scenarios": [
    {
      "name": "kv_update_1k",
      "ops": 1000,
      "hits": 0,
      "total_ns_median": 361000,
      "ns_per_op": 361.0,
      "runs": 5,
      "checksum": 2676218997,
      "errcodes": {"ok": 1000},
      "flash": {"reads": 7823, "read_bytes": 231708, "writes": 7089,
                "write_bytes": 63243, "erases": 14, "erase_bytes": 57344},
      "extra": {"samples_ns": [...], "total_ns_min": ..., "total_ns_max": ...,
                "flash_consistent": true, "errs_consistent": true, ...}
    }
  ]
}
```

* `total_ns_median` is the median of the `runs` measured-phase wall-clock
  durations in ns; `ns_per_op = total_ns_median / ops`.
* `errcodes` counts the `fdb_err_t` return codes observed **during the measured
  phase**, only for non-zero codes, with the shared key names
  `ok, read_err, write_err, erase_err, name_err, saved_full, init_failed`
  (plus `part_not_found`, `name_exist` if they ever occur).
  `fdb_kv_get_blob` returns a length rather than an error code; those calls are
  counted in `hits` (number of gets returning > 0 bytes) instead.
* `hits` is non-zero only for `kv_get_hot` (2000) / `kv_get_miss` (0); all
  other scenarios report 0. Mount success counts live in `extra.mount_ok`.
* `extra` is the extension point: per-run samples, min/max, consistency flags,
  entry counts, capacities, sum of returned bytes, GC erases, etc.
* `flash` is the measured-phase delta; `erase_bytes` is the sum of erased
  sector bytes (4 KiB per erase). Derived weighted cost (documented, not
  hardware truth): `read_bytes + 10*write_bytes + 100*erase_bytes`.

## Image tool (C⇄Rust on-flash compatibility)

`scripts/c_image.sh write <out.bin> <wg>` writes a raw 32768-byte image:
`[0x0000,0x4000)` = KV partition, `[0x4000,0x8000)` = TS partition
(4×4096-byte sectors each).

Deterministic workload (exact, both implementations must produce the same
image):

```
KV (value helper kv_value(seed,i,len) as above):
  for i in 0..7:   set("img{i}", kv_value(0x55, i, 8 << (i % 4)))   # 8,16,32,64,8,16,32,64
  for i in 0..3:   set("img{i}", kv_value(0x56, i, 16))             # updates
  delete("img7")
  for k in 0..399: set("churn", kv_value(0x57, k, 64))              # GC churn
TS (rollover off, max_len 256):
  for i in 0..19:  append(ts_payload(i), t = 1000 + i)
```

`read` loads the image and mounts KV+TS in **not-formatable** mode (never
re-formats), runs `fdb_kvdb_check`, and prints (iteration order):

```
KV key=<key> len=<value_len> fnv=<fnv1a32(value) decimal>
TS time=<t> status=<int> len=<log_len> fnv=<fnv1a32(payload) decimal>
KVCOUNT=<n> TSCOUNT=<n>
```

Mount/check failures print `ERROR=<code>` (numeric `fdb_err_t`; 255 = I/O) and
exit 2. `write` prints the same summary to stdout and the workload flash
counts to stderr:

```
KVFLASH reads=.. read_bytes=.. writes=.. write_bytes=.. erases=.. erase_bytes=..
TSFLASH ...
IMAGEFLASH ...   (both partitions together; this is the number to compare with Rust)
```

`selfcheck` writes the image twice (byte-identical check), reads it back and
diffs the summaries, printing the SHA-256 and the flash counts.

## Deviations from `harness/SCENARIOS.md` (reported to the Lead)

1. **Checksum is FNV-1a 32, decimal**, per the Lead's updated contract for
   comparable JSON; `SCENARIOS.md` only gave FNV-1a as an example.
2. **`tsl_append` full count is 308 (WG=8) / 336 (WG=1)**, not "≈400": with
   4×4096 sectors, a 32-byte payload and the FDB log-index header, capacity is
   fixed. The harness reports the measured count and `FDB_SAVED_FULL`.
3. **`tsl_mount` checksum is taken from the pre-mount populated state.** After
   remounting a *full, no-rollover* DB, upstream FlashDB leaves
   `oldest_addr=0xFFF` and `cur_sec.addr=0xFFFFFFFF`, so a post-mount iteration
   yields 0 entries. The mount itself succeeds and is counted; the data
   checksum comes from the prepared state. (The Rust side must decide whether
   to reproduce this upstream quirk; for parity, compare the same pre-mount
   checksum and `extra.mount_ok`.)
4. **`kv_delete` = 3 rounds × 32 × (delete+set) = 96 deletes + 96 sets = 192
   ops.** The `SCENARIOS.md` parenthetical "192 del + 192 set" does not match
   the described loop; the loop interpretation is used (Lead confirmed
   "= 96 dels + 96 sets").
5. `kv_iterate`'s measured loop intentionally does not read values (only
   `value_len` sums); values are read only by the post-phase snapshot used for
   the checksum.
6. `kv_get_hot` / `kv_get_miss` have no `fdb_err_t` calls, so their
   `errcodes` are empty; hits are reported in `hits`.
7. Ranged-query ranges may extend past the last populated timestamp (by
   design: `from = 1000 + state % 3000`).
8. `tsl_append_rollover` sizes itself from the measured capacity
   (append-to-full probe) rather than a hard-coded ~400.
