# FlashDB-rs — design

This document explains how the Rust rewrite is put together, why it is not a
line-by-line translation of FlashDB-C, and where the performance and RAM/ROM
head-room comes from. It is meant to be read together with the code: module
docs in `crates/flashdb/src/*.rs` cover the details of each algorithm.

## 1. Goals and constraints

1. **Same on-flash format as FlashDB 2.2.x.** Data written by FlashDB-C must
   mount in Rust and vice versa, for the same sector size, write granularity,
   timestamp width and byte order. This is the compatibility contract that
   makes the rewrite a drop-in replacement instead of a new database.
2. **At least the same performance and RAM/ROM footprint.**
   The acceptance evidence is in `docs/BENCHMARKS.md`: host micro-benchmarks
   with flash-op/byte accounting, and a Cortex-M3/QEMU firmware comparison.
3. **`no_std`, no heap, no global state, no `unsafe` in the algorithms.**
   The crate is `#![no_std] #![deny(unsafe_code)]`; all buffers are stack or
   caller-provided; a database *owns* its flash view (`Kvdb<F>`), so several
   databases/drivers can coexist without a FAL-style global registry or lock.
   The single audited `unsafe` block is the placement initializer
   (`mount_into`), which writes every field of a database exactly once into a
   caller-provided `MaybeUninit` so a `static` database costs no stack copy.
   Everything above it (parsing, CRC, GC, recovery, iterators) is safe code.
4. **Rust-native API.** `Result` with the driver's own error type, iterators
   instead of callbacks, compile-time configuration, typed statuses.

## 2. Layering

```
                 user code
                    │
        ┌───────────┴───────────┐
        │                       │
     Kvdb<F, CACHE>          Tsdb<F, T>
        │                       │
        └───────────┬───────────┘
                    │
               Storage<F>                 geometry + bounds checks,
                    │                     aligned writes, status coding
                 Flash trait               user driver (NOR/NAND partition)
```

| module | contents |
|---|---|
| `flash.rs` | `Flash` trait: `read`/`write`/`erase`, `WRITE_GRAN_BITS`, `ERASED_BYTE`. Implemented for `&mut F`, so a DB can borrow a driver. |
| `storage.rs` | `Config` (sector size, partition size), validation, bounds-checked access, FlashDB's aligned-write and status-table primitives. |
| `low.rs` | Byte-level format: magic words, status enums, CRC32, write-granularity layout tables extracted from the C structs, status encode/decode. |
| `kvdb.rs` | KVDB: mount/recovery, get/set/delete, iteration, GC. |
| `tsdb.rs` | TSDB: mount/recovery, append, forward/reverse/by-time iteration, rollover, clean. |
| `error.rs` | `Error<E>` + `Result<T, E>`; `E` is `Flash::Error`. |

Layering has one important consequence: the KV and TS algorithms never see a
raw flash device, only `Storage<F>`. Bounds checking, write-granule padding and
status-table coding exist in exactly one place, which is also where the
compatibility rules are enforced.

## 3. On-flash format compatibility

FlashDB's format depends on `FDB_WRITE_GRAN` in subtle ways: the status tables
are one write-granule chunk per status (bit-packed for granule 1), and the C
structs contain conditional padding. The layout tables in `low.rs`
(`KvLayout`, `TsLayout`) were produced by compiling the original C headers and
printing `sizeof`/`offsetof` for every granularity (1/8/32/64/128/256 bits) and
both timestamp widths, then encoded as `const fn` tables. The unit tests check
that the layout tables are self-consistent; the differential tests in
`harness/` check images byte for byte.

Layout selection is driven by the flash driver:

```rust
impl Flash for MyNor {
    type Error = MyError;
    const WRITE_GRAN_BITS: u32 = 8;   // NOR: any byte; STM32F4: 8; F1: 32 ...
    // ...
}
```

All size/offset computations are `#[inline(always)]` functions taking
`F::WRITE_GRAN_BITS`; because the value is a constant of the concrete flash
type, the optimiser folds every branch and the layout becomes a compile-time
constant, without const-generic arithmetic in the public API.

Byte order is little-endian, matching FlashDB's default configuration (the C
`FDB_BIG_ENDIAN` option is not supported; all current embedded Rust targets are
little-endian).

## 4. KVDB: what changes vs the C implementation

### 4.1 Name cache — O(1) instead of a 64-entry linear scan

C keeps two tables: a 64-entry KV cache (`name_crc16`, `active`, `addr`, 8 B
each, 512 B) and an 8-entry sector cache (`kvdb_sec_info`, ≈320 B).

Rust keeps **one** 2-way set-associative cache of 64 entries, 8 B each
(512 B) plus a 4-byte LRU bitmap. A lookup hashes the key (FNV-1a) and probes
one set (two tags), instead of comparing against all 64 entries and reading 64
bytes of key name from flash on every hit.

The sector cache is gone entirely: the currently active sector is tracked in
the DB struct (`cur_sec`), so appends normally do not re-traverse a sector.
There is no separate cache table to keep coherent.

The cache is a compile-time parameter: `KvdbImpl<F, CACHE>`. Aliases:

| alias | cache entries | RAM |
|---|---|---|
| `Kvdb<F>` | 64 (2-way × 32 sets) | 516 B |
| `KvdbSmallCache<F>` | 16 | 132 B |
| `KvdbNoCache<F>` | 0 | 0 B |

### 4.2 Lookup reads only what is needed

`fdb_kvdb.c` `read_kv()` computes a CRC32 over the whole value on *every*
record it touches — including records that are merely walked past while
searching for a key. Rust separates the two concerns:

* `read_kv_header()` reads the fixed-size header, validates magic/length
  consistency, and decodes the status. Traversal uses the header's length to
  jump directly to the next record (C rescans the following bytes for the magic
  word on every step).
* A key match reads only the key name.
* The value's CRC is verified when the value is actually read, and the copy
  into the caller's buffer happens *in the same pass* as the CRC computation.
  C reads the value once for `read_kv()` and again in `get_kv()`.

A miss (`kv_get_miss`) therefore costs one header read per record on flash,
compared to header+name+full value CRC in C.

### 4.3 Iterators and blobs

C's `fdb_kv_iterator` + `fdb_kv_iterate(&itr)` pair is replaced by a real
`Iterator<Item = Result<KvEntry, Error<E>>>`. `KvEntry` is a plain `Copy`
struct (address, lengths, status, key bytes); the value is read on demand with
`KvIterator::read_value(&entry, &mut buf)`, so iteration does not copy values
the caller does not need. This also removes the `void *` callback + `void *arg`
contract and its implicit aliasing rules.

### 4.4 Writes

`set()` follows the same crash-consistency protocol as C (allocate → find old
→ `PRE_DELETE` → write new record → `DELETED`), so power loss at any flash op
leaves a recoverable state. The differences are bookkeeping only:

* header and name are written with one call each, no temporary struct copy;
* the sector status transition is written only when it changes;
* the "sector dirty" status read is skipped when the active sector is already
  known to be dirty;
* the new record's cache entry survives the completing delete
  (`last_is_complete_del` in C, an explicit flag here).

Net: `kv_update_1k` performs 7 flash writes per update like C (7089 vs 7061)
but far fewer reads (7 391 vs 11 146) thanks to 4.2.

### 4.5 Garbage collection

The GC policy is ported faithfully (collect dirty sectors when the number of
empty sectors drops to the threshold; move live records; re-format; stop as
soon as the previously formatted sector has enough free space). Two cheap
improvements: the stop-check uses the known free space of the sector just
formatted instead of re-traversing it, and traversal again uses direct
next-record addressing.

## 5. TSDB

Same layout and algorithms (index top-down, payload bottom-up, `end_info[2]`
fast range query, rollover ring), same recovery rules, expressed as iterators:

* `db.iter()`, `db.iter_reverse()`, `db.iter_by_time(from, to)` — the latter
  handles `from > to` as a reverse range, like C.
* `Iterator<Item = Result<TslEntry<T>>>` + `TslIterator::read_blob()`.
* Timestamp width is a type parameter: `Tsdb<F>` (i32, FlashDB default) and
  `Tsdb64<F>` (i64, `FDB_USING_TIMESTAMP_64BIT`). The type selects the layout
  table, so the on-flash format matches the corresponding C build.
* Append takes an explicit timestamp (`append(time, data)`) instead of calling
  a `get_time` function pointer through the DB; `append_now()` is provided for
  tests/RTC-less devices.
* `Tsdb` stores no function pointers, so it is a small plain struct (140 B on
  a 32-bit target vs C's 160 B).

## 6. Error handling

```rust
pub enum Error<E> {
    FlashRead(E), FlashWrite(E), FlashErase(E),
    InvalidConfig, UnsupportedWriteGran, NotFormatted,
    SavedFull, InvalidName, ReadCorrupted, OutOfBounds,
}
```

`E` is the driver's error type; a SPI-NOR driver can expose timeouts/bus errors
without losing them in a single `FDB_READ_ERR`. There is no `FDB_ASSERT` that
spins forever: configuration errors are returned, and corrupted data is
reported as `ReadCorrupted` (or repaired during mount where FlashDB repairs).

## 7. What a caller sees

```rust
let mut db = Kvdb::mount(flash, Config::new(4096, 16 * 1024))?;
db.set(b"boot_count", &42u32.to_le_bytes())?;

let mut buf = [0u8; 16];
if let Some(n) = db.get(b"boot_count", &mut buf)? { /* ... */ }

let mut it = db.iter();
while let Some(entry) = it.next() {
    let entry = entry?;              // KvEntry: key, lengths, status, address
    let mut value = [0u8; 64];
    let n = it.read_value(&entry, &mut value)?;
    // ...
}
```

No `deinit()` is needed: dropping the DB (or `into_flash()`) returns the
driver. A mount is a normal constructor, so two DBs can be mounted on two
different partitions of the same driver when the driver is split into
partition views by the user's `Flash` implementations.

Firmware usually keeps its databases in `.bss`. The by-value constructors
return the whole database struct, which on a small MCU means copying it (and
its 512-byte name cache) through the stack. The placement variants avoid that:

```rust
static mut DB: MaybeUninit<Kvdb<MyFlash>> = MaybeUninit::uninit();
// SAFETY: single-threaded init before any other use.
unsafe { Kvdb::mount_into(&mut *addr_of_mut!(DB), flash, cfg)? };
```

This mirrors FlashDB-C's `fdb_kvdb_init(db, ...)` pointer API; in the Cortex-M3
comparison it removes ~1 KB of mount stack usage.

## 8. Performance model

FlashDB's cost dominates in flash operations, so the rewrite is judged on
(1) number of flash reads/writes/erases and bytes, and (2) CPU per operation.
The design targets both:

| lever | effect |
|---|---|
| validate headers instead of CRC-scanning values during search | turns O(value size) search reads into O(1) header reads |
| single-pass CRC+copy for `get` | halves value reads on reads |
| direct next-record addressing | one read per record instead of two during traversals |
| O(1) 2-way name cache | removes the 64-entry scan and the 64-byte key read on every hit |
| no sector-cache table, cached active sector | fewer re-traversals, less RAM |
| CRC32 table generated at compile time (`const fn`) | no RAM cost, 1 KiB `.rodata` like C |
| `#[inline(always)]` layout accessors | zero-cost write-granularity specialisation |

Numbers and methodology are in `docs/BENCHMARKS.md`.

## 9. Testing strategy

1. **Unit tests** (`cargo test --lib`) — CRC, status coding, layout tables.
2. **Smoke/behaviour tests** (`crates/flashdb/tests/`) — CRUD, persistence,
   GC stress, remount, power-cut recovery by fault injection at every flash
   operation, all write granularities supported.
3. **C parity tests** — the original `tests/fdb_kvdb_tc.c` and
   `fdb_tsdb_tc.c` ported to Rust integration tests.
4. **Differential image tests** — C writes an image, Rust mounts and reads it;
   Rust writes an image, C mounts and reads it; deterministic workloads must
   produce byte-identical flash images.
5. **Benchmarks** — identical scenarios on both implementations with a
   counting RAM flash; see `harness/SCENARIOS.md`.
6. **Firmware comparison** — the same workload built for Cortex-M3
   (`thumbv7m-none-eabi`, `-Os` / `opt-level="s"`+LTO), run under QEMU with
   semihosting; reports `.text/.rodata/.data/.bss`, stack high-water and
   workload checksums.

## 10. Known deviations from FlashDB-C

These are deliberate and documented; none changes the on-flash format for
valid data.

* **Error reporting**: `remove()` returns `bool` (key existed) instead of
  `FDB_KV_NAME_ERR`; corrupted reads return `Error::ReadCorrupted` where C
  silently treats a CRC failure during lookup as "not found".
* **`find()` does not verify the value CRC**; the CRC is checked when the
  value is read (`get`/`read_value`). C verifies during lookup and again on
  read.
* **KV iteration yields only committed (`Write`) records**, as C does, but
  validates header consistency instead of CRC-scanning each value; use
  `read_value` for integrity-checked access.
* **TSDB iteration skips empty sectors in reverse mode** and recovers sectors
  with damaged end-info instead of hitting `FDB_ASSERT(0)`/hanging as C can.
  `last_time` recovery is more defensive for all-full databases.
* **Big-endian flash layout** (`FDB_BIG_ENDIAN`) is not supported.
* **File mode** (`FDB_USING_FILE_*`) is not ported: the crate targets raw
  flash. A file-backed `Flash` implementation is a user-side adapter.
* **64-bit timestamps** are selected with the type parameter instead of a
  build macro; the resulting format matches the C option.
* **Write granularities 64/128/256** have layout tables and algorithms, but
  are exercised less than 1/8/32 (the common NOR / STM32 cases).
* **`FDB_KV_AUTO_UPDATE`** (bump a stored version number and re-apply defaults)
  is not ported as a built-in option; it is a small policy layer on top of
  `mount_with_defaults` / `set` and is left to the application.
* **`fdb_kv_print` / `fdb_tsl_iter` debug printing** is replaced by iterators
  plus the user's own formatting, and `fdb_kvdb_control` lock hooks are
  unnecessary because a database is borrowed exclusively (`&mut self`).
