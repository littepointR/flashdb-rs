//! Rust side of the shared benchmark protocol (`harness/SCENARIOS.md`).
//!
//! This harness mirrors `harness/c/` exactly: same scenarios, same data
//! generation, same LCG ranges, same setup/measure split, same checksum
//! definitions (final-state snapshots) and the same JSON contract, so the two
//! implementations can be compared field by field.
//!
//! Usage:
//!
//! ```text
//! cargo run -p flashdb-bench --release -- --wg 8 --runs 5
//! cargo run -p flashdb-bench --release -- --wg 1 --runs 5 --no-ts
//! ```
//!
//! The JSON is written to `harness/results/rust_wg<NN>_host.json`.

use flashdb::{Config, Error, Flash, Kvdb, KvdbImpl, KvdbNoCache, KvdbSmallCache, Timestamp, Tsdb, TsdbImpl, TslStatus};
use flashdb_sim::{RamFlash, SimError};
use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::rc::Rc;
use std::time::Instant;

const SEC_SIZE: u32 = 4096;
const MAX_SIZE: u32 = 16384;
const FLASH_SIZE: usize = 16384;
const DEFAULT_RUNS: u32 = 5;
const TS_MAX_LOG_LEN: u32 = 32;
const TS_PAYLOAD_LEN: usize = 32;
const TS_START_TIME: i32 = 1000;
const TS_APPEND_MAX_CALLS: u32 = 4000;
const TS_RANGE_QUERIES: u32 = 500;
const TS_LCG_SEED: u64 = 12345;
const BENCH_FNV32_OFFSET: u32 = 0x811C_9DC5;
const BENCH_FNV32_PRIME: u32 = 0x0100_0193;

// ---------------------------------------------------------------------------
// Instrumented flash
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Counters {
    reads: Cell<u64>,
    read_bytes: Cell<u64>,
    writes: Cell<u64>,
    write_bytes: Cell<u64>,
    erases: Cell<u64>,
    erase_bytes: Cell<u64>,
}

/// Plain copy used for JSON/reporting.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CounterSnapshot {
    reads: u64,
    read_bytes: u64,
    writes: u64,
    write_bytes: u64,
    erases: u64,
    erase_bytes: u64,
}

impl Counters {
    fn snapshot(&self) -> CounterSnapshot {
        CounterSnapshot {
            reads: self.reads.get(),
            read_bytes: self.read_bytes.get(),
            writes: self.writes.get(),
            write_bytes: self.write_bytes.get(),
            erases: self.erases.get(),
            erase_bytes: self.erase_bytes.get(),
        }
    }

    fn reset(&self) {
        self.reads.set(0);
        self.read_bytes.set(0);
        self.writes.set(0);
        self.write_bytes.set(0);
        self.erases.set(0);
        self.erase_bytes.set(0);
    }
}

struct BenchFlash<const WG: u32> {
    inner: RamFlash<FLASH_SIZE, WG>,
    counters: Rc<Counters>,
}

impl<const WG: u32> BenchFlash<WG> {
    fn new(counters: Rc<Counters>) -> Self {
        Self {
            inner: RamFlash::<FLASH_SIZE, WG>::new(SEC_SIZE),
            counters,
        }
    }
}

impl<const WG: u32> Flash for BenchFlash<WG> {
    type Error = SimError;
    const WRITE_GRAN_BITS: u32 = WG;

    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        let result = self.inner.read(addr, buf);
        if result.is_ok() {
            self.counters.reads.set(self.counters.reads.get() + 1);
            self.counters
                .read_bytes
                .set(self.counters.read_bytes.get() + buf.len() as u64);
        }
        result
    }

    fn write(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> {
        let result = self.inner.write(addr, buf);
        if result.is_ok() {
            self.counters.writes.set(self.counters.writes.get() + 1);
            self.counters
                .write_bytes
                .set(self.counters.write_bytes.get() + buf.len() as u64);
        }
        result
    }

    fn erase(&mut self, addr: u32, len: u32) -> Result<(), Self::Error> {
        let result = self.inner.erase(addr, len);
        if result.is_ok() {
            self.counters.erases.set(self.counters.erases.get() + 1);
            self.counters
                .erase_bytes
                .set(self.counters.erase_bytes.get() + len as u64);
        }
        result
    }
}

/// Zero-sized flash for footprint measurements (not counted in the DB size,
/// like FlashDB's `sizeof(struct fdb_kvdb)` which excludes the partition).
struct DummyFlash<const WG: u32>;

impl<const WG: u32> Flash for DummyFlash<WG> {
    type Error = SimError;
    const WRITE_GRAN_BITS: u32 = WG;
    fn read(&mut self, _: u32, _: &mut [u8]) -> Result<(), Self::Error> {
        unreachable!()
    }
    fn write(&mut self, _: u32, _: &[u8]) -> Result<(), Self::Error> {
        unreachable!()
    }
    fn erase(&mut self, _: u32, _: u32) -> Result<(), Self::Error> {
        unreachable!()
    }
}

// ---------------------------------------------------------------------------
// Data generation / checksums (normative, see SCENARIOS.md)
// ---------------------------------------------------------------------------

#[inline]
fn fnv_bytes(mut h: u32, data: &[u8]) -> u32 {
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(BENCH_FNV32_PRIME);
    }
    h
}

#[inline]
fn fnv_u32(h: u32, v: u32) -> u32 {
    fnv_bytes(h, &v.to_le_bytes())
}

fn kv_value(seed: u32, i: u32, len: usize, out: &mut [u8]) {
    for (j, b) in out[..len].iter_mut().enumerate() {
        *b = (seed + i * 31 + j as u32 * 7) as u8;
    }
}

fn ts_payload(i: u32, out: &mut [u8]) {
    for (j, b) in out.iter_mut().enumerate() {
        *b = (0x5Au32 + i * 13 + j as u32 * 5) as u8;
    }
}

/// Render `prefix + decimal(i)` into `out`; returns the length.
fn key_num(prefix: &[u8], i: u32, out: &mut [u8]) -> usize {
    out[..prefix.len()].copy_from_slice(prefix);
    let mut pos = prefix.len();
    let mut digits = [0u8; 10];
    let mut n = i;
    let mut count = 0;
    loop {
        digits[count] = b'0' + (n % 10) as u8;
        count += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for d in digits[..count].iter().rev() {
        out[pos] = *d;
        pos += 1;
    }
    pos
}

// ---------------------------------------------------------------------------
// Results / JSON
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct Errcodes {
    ok: u64,
    erase_err: u64,
    read_err: u64,
    write_err: u64,
    name_err: u64,
    saved_full: u64,
    init_failed: u64,
}

impl Errcodes {
    fn record<T>(&mut self, result: &Result<T, Error<SimError>>) {
        match result {
            Ok(_) => self.ok += 1,
            Err(e) => self.record_err(e),
        }
    }

    fn record_err(&mut self, e: &Error<SimError>) {
        match e {
            Error::FlashRead(_) => self.read_err += 1,
            Error::FlashWrite(_) => self.write_err += 1,
            Error::FlashErase(_) => self.erase_err += 1,
            Error::InvalidName => self.name_err += 1,
            Error::SavedFull => self.saved_full += 1,
            _ => self.init_failed += 1,
        }
    }

    fn to_json(&self) -> String {
        let mut out = String::from("{");
        let mut first = true;
        let mut push = |name: &str, v: u64, first: &mut bool| {
            if v > 0 {
                if !*first {
                    out.push(',');
                }
                *first = false;
                let _ = write!(out, "\"{name}\":{v}");
            }
        };
        push("ok", self.ok, &mut first);
        push("erase_err", self.erase_err, &mut first);
        push("read_err", self.read_err, &mut first);
        push("write_err", self.write_err, &mut first);
        push("name_err", self.name_err, &mut first);
        push("saved_full", self.saved_full, &mut first);
        push("init_failed", self.init_failed, &mut first);
        out.push('}');
        out
    }
}

#[derive(Debug, Clone)]
enum ExtraVal {
    U(u64),
    I(i64),
    F(f64),
    B(bool),
    S(String),
}

#[derive(Debug, Default)]
struct BodyOut {
    errcodes: Errcodes,
    hits: u64,
    ops: u64,
    extra: Vec<(&'static str, ExtraVal)>,
}

#[derive(Debug, Default)]
struct FinishOut {
    checksum: u32,
    extra: Vec<(&'static str, ExtraVal)>,
}

#[derive(Debug, Clone)]
struct Scenario {
    name: &'static str,
    ops: u64,
    hits: u64,
    runs: u32,
    total_ns_median: u64,
    ns_per_op: f64,
    checksum: u32,
    errcodes: Errcodes,
    flash: CounterSnapshot,
    extra: Vec<(&'static str, ExtraVal)>,
}

fn median(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    let n = values.len();
    if n == 0 {
        return 0;
    }
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2
    }
}

fn extra_json(extra: &[(&'static str, ExtraVal)]) -> String {
    let mut out = String::from("{");
    for (i, (k, v)) in extra.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{k}\":");
        match v {
            ExtraVal::U(x) => {
                let _ = write!(out, "{x}");
            }
            ExtraVal::I(x) => {
                let _ = write!(out, "{x}");
            }
            ExtraVal::F(x) => {
                let _ = write!(out, "{x:.6}");
            }
            ExtraVal::B(x) => {
                let _ = write!(out, "{x}");
            }
            ExtraVal::S(x) => {
                let _ = write!(out, "\"{x}\"");
            }
        }
    }
    out.push('}');
    out
}

impl Scenario {
    fn to_json(&self) -> String {
        let mut extra = String::new();
        for e in &self.extra {
            extra.push_str(&extra_json(core::slice::from_ref(e)));
        }
        // merge into one object: rebuild properly
        let mut merged = String::from("{");
        for (i, (k, v)) in self.extra.iter().enumerate() {
            if i > 0 {
                merged.push(',');
            }
            let _ = write!(merged, "\"{k}\":");
            match v {
                ExtraVal::U(x) => {
                    let _ = write!(merged, "{x}");
                }
                ExtraVal::I(x) => {
                    let _ = write!(merged, "{x}");
                }
                ExtraVal::F(x) => {
                    let _ = write!(merged, "{x:.6}");
                }
                ExtraVal::B(x) => {
                    let _ = write!(merged, "{x}");
                }
                ExtraVal::S(x) => {
                    let _ = write!(merged, "\"{x}\"");
                }
            }
        }
        merged.push('}');
        let _ = extra;
        format!(
            concat!(
                "{{\"name\":\"{}\",\"ops\":{},\"hits\":{},\"total_ns_median\":{},",
                "\"ns_per_op\":{:.3},\"runs\":{},\"checksum\":{},",
                "\"errcodes\":{},\"flash\":{{\"reads\":{},\"read_bytes\":{},",
                "\"writes\":{},\"write_bytes\":{},\"erases\":{},\"erase_bytes\":{}}},",
                "\"extra\":{}}}"
            ),
            self.name,
            self.ops,
            self.hits,
            self.total_ns_median,
            self.ns_per_op,
            self.runs,
            self.checksum,
            self.errcodes.to_json(),
            self.flash.reads,
            self.flash.read_bytes,
            self.flash.writes,
            self.flash.write_bytes,
            self.flash.erases,
            self.flash.erase_bytes,
            merged,
        )
    }
}

// ---------------------------------------------------------------------------
// Snapshot checksums (identical definitions to the C harness)
// ---------------------------------------------------------------------------

fn kv_snapshot_checksum<F: Flash>(db: &mut KvdbImpl<F, 64>) -> (u32, u32, u64) {
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    {
        let mut it = db.iter();
        while let Some(entry) = it.next() {
            let entry = match entry { Ok(e) => e, Err(_) => panic!("kv snapshot iterator") };
            let mut value = vec![0u8; entry.value_len as usize];
            let n = match it.read_value(&entry, &mut value) { Ok(n) => n, Err(_) => panic!("kv snapshot read") };
            value.truncate(n);
            entries.push((entry.name().to_vec(), value));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let count = entries.len() as u32;
    let mut value_bytes = 0u64;
    let mut h = BENCH_FNV32_OFFSET;
    h = fnv_u32(h, count);
    for (key, value) in &entries {
        h = fnv_u32(h, key.len() as u32);
        h = fnv_bytes(h, key);
        h = fnv_u32(h, value.len() as u32);
        h = fnv_bytes(h, value);
        value_bytes += value.len() as u64;
    }
    (h, count, value_bytes)
}

fn ts_snapshot_checksum<F: Flash>(db: &mut TsdbImpl<F, i32>) -> (u32, u32, u64) {
    let mut h = BENCH_FNV32_OFFSET;
    let mut count = 0u32;
    let mut payload_bytes = 0u64;
    {
        let mut it = db.iter();
        while let Some(entry) = it.next() {
            let entry = match entry { Ok(e) => e, Err(_) => panic!("ts snapshot iterator") };
            h = fnv_u32(h, entry.status as u32);
            h = fnv_u32(h, entry.time.to_i64() as u32);
            h = fnv_u32(h, entry.log_len);
            let mut buf = vec![0u8; entry.log_len as usize];
            let n = match it.read_blob(&entry, &mut buf) { Ok(n) => n, Err(_) => panic!("ts snapshot read") };
            h = fnv_bytes(h, &buf[..n]);
            count += 1;
            payload_bytes += n as u64;
        }
    }
    h = fnv_u32(h, count);
    (h, count, payload_bytes)
}

// ---------------------------------------------------------------------------
// Runners
// ---------------------------------------------------------------------------

fn bench_kv<const WG: u32, S, M, F>(
    name: &'static str,
    runs: u32,
    setup: S,
    body: M,
    finish: F,
) -> Scenario
where
    S: Fn(&mut Kvdb<BenchFlash<WG>>),
    M: Fn(&mut Kvdb<BenchFlash<WG>>) -> BodyOut,
    F: Fn(&mut Kvdb<BenchFlash<WG>>) -> FinishOut,
{
    let mut samples = Vec::with_capacity(runs as usize);
    let mut last_body = BodyOut::default();
    let mut last_finish = FinishOut::default();
    let mut last_flash = CounterSnapshot::default();
    let mut first_flash: Option<CounterSnapshot> = None;
    let mut flash_consistent = true;
    for _ in 0..runs {
        let counters = Rc::new(Counters::default());
        let flash = BenchFlash::<WG>::new(counters.clone());
        let mut db = Kvdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE)).expect("kv mount");
        setup(&mut db);
        counters.reset();
        let start = Instant::now();
        let body_out = body(&mut db);
        let elapsed = start.elapsed().as_nanos() as u64;
        let flash_out = counters.snapshot();
        let finish_out = finish(&mut db);
        samples.push(elapsed);
        last_body = body_out;
        last_finish = finish_out;
        last_flash = flash_out;
        match first_flash {
            None => first_flash = Some(flash_out),
            Some(first) => {
                if first != flash_out {
                    flash_consistent = false;
                }
            }
        }
        drop(db);
    }
    let median_ns = median(&mut samples);
    let mut extra = last_finish.extra;
    extra.extend(last_body.extra);
    extra.push((
        "flash_consistent",
        ExtraVal::B(flash_consistent),
    ));
    Scenario {
        name,
        ops: last_body.ops,
        hits: last_body.hits,
        runs,
        total_ns_median: median_ns,
        ns_per_op: if last_body.ops > 0 {
            median_ns as f64 / last_body.ops as f64
        } else {
            0.0
        },
        checksum: last_finish.checksum,
        errcodes: last_body.errcodes,
        flash: last_flash,
        extra,
    }
}

fn bench_ts<const WG: u32, S, M, F>(
    name: &'static str,
    runs: u32,
    setup: S,
    body: M,
    finish: F,
) -> Scenario
where
    S: Fn(&mut Tsdb<BenchFlash<WG>>),
    M: Fn(&mut Tsdb<BenchFlash<WG>>) -> BodyOut,
    F: Fn(&mut Tsdb<BenchFlash<WG>>) -> FinishOut,
{
    let mut samples = Vec::with_capacity(runs as usize);
    let mut last_body = BodyOut::default();
    let mut last_finish = FinishOut::default();
    let mut last_flash = CounterSnapshot::default();
    let mut first_flash: Option<CounterSnapshot> = None;
    let mut flash_consistent = true;
    for _ in 0..runs {
        let counters = Rc::new(Counters::default());
        let flash = BenchFlash::<WG>::new(counters.clone());
        let mut db =
            Tsdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE), TS_MAX_LOG_LEN).expect("ts mount");
        setup(&mut db);
        counters.reset();
        let start = Instant::now();
        let body_out = body(&mut db);
        let elapsed = start.elapsed().as_nanos() as u64;
        let flash_out = counters.snapshot();
        let finish_out = finish(&mut db);
        samples.push(elapsed);
        last_body = body_out;
        last_finish = finish_out;
        last_flash = flash_out;
        match first_flash {
            None => first_flash = Some(flash_out),
            Some(first) => {
                if first != flash_out {
                    flash_consistent = false;
                }
            }
        }
        drop(db);
    }
    let median_ns = median(&mut samples);
    let mut extra = last_finish.extra;
    extra.extend(last_body.extra);
    extra.push(("flash_consistent", ExtraVal::B(flash_consistent)));
    Scenario {
        name,
        ops: last_body.ops,
        hits: last_body.hits,
        runs,
        total_ns_median: median_ns,
        ns_per_op: if last_body.ops > 0 {
            median_ns as f64 / last_body.ops as f64
        } else {
            0.0
        },
        checksum: last_finish.checksum,
        errcodes: last_body.errcodes,
        flash: last_flash,
        extra,
    }
}

/// Mount-loop scenario: the DB is mounted/deinitialised repeatedly, so the
/// flash contents must persist between mounts. The post-phase state is used for
/// the checksum (KV) or the pre-mount state is captured by the caller (TS).
fn bench_kv_mount<const WG: u32>(runs: u32) -> Scenario {
    let mut samples = Vec::with_capacity(runs as usize);
    let mut last_flash = CounterSnapshot::default();
    let mut errcodes = Errcodes::default();
    let mut mount_ok = 0u64;
    let mut checksum = 0u32;
    let mut entries = 0u32;
    let mut value_bytes = 0u64;
    for _ in 0..runs {
        let counters = Rc::new(Counters::default());
        let flash = BenchFlash::<WG>::new(counters.clone());
        let mut db = Kvdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE)).expect("kv mount");
        setup_32_keys(&mut db);
        let mut holder = Some(db.into_flash());
        counters.reset();
        let start = Instant::now();
        let mut run_ok = 0u64;
        for _ in 0..100 {
            let flash = holder.take().expect("flash available");
            match Kvdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE)) {
                Ok(db) => {
                    run_ok += 1;
                    holder = Some(db.into_flash());
                }
                Err(e) => {
                    errcodes.record_err(&e);
                    break;
                }
            }
        }
        let elapsed = start.elapsed().as_nanos() as u64;
        samples.push(elapsed);
        last_flash = counters.snapshot();
        mount_ok = run_ok;
        let flash = holder.take().expect("flash after mounts");
        let mut db = Kvdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE)).expect("kv remount");
        let (cs, count, vb) = kv_snapshot_checksum(&mut db);
        checksum = cs;
        entries = count;
        value_bytes = vb;
    }
    let median_ns = median(&mut samples);
    Scenario {
        name: "kv_mount",
        ops: 100,
        hits: 0,
        runs,
        total_ns_median: median_ns,
        ns_per_op: median_ns as f64 / 100.0,
        checksum,
        errcodes,
        flash: last_flash,
        extra: vec![
            ("entries", ExtraVal::U(entries as u64)),
            ("value_bytes", ExtraVal::U(value_bytes)),
            ("mount_ok", ExtraVal::U(mount_ok)),
            ("flash_consistent", ExtraVal::B(true)),
        ],
    }
}

// ---------------------------------------------------------------------------
// KV scenario data/setup/body
// ---------------------------------------------------------------------------

fn setup_32_keys<F: Flash>(db: &mut KvdbImpl<F, 64>) {
    let mut key = [0u8; 16];
    let mut value = [0u8; 32];
    for i in 0..32u32 {
        let klen = key_num(b"key", i, &mut key);
        kv_value(0x33, i, 32, &mut value);
        if db.set(&key[..klen], &value).is_err() { panic!("setup set"); }
    }
}

fn kv_setup_hot<F: Flash>(db: &mut KvdbImpl<F, 64>) {
    setup_32_keys(db);
    let mut key = [0u8; 16];
    let mut buf = [0u8; 64];
    for i in 0..128u32 {
        let klen = key_num(b"key", i % 32, &mut key);
        let _ = db.get(&key[..klen], &mut buf);
    }
}

fn kv_finish_state<F: Flash>(db: &mut KvdbImpl<F, 64>) -> FinishOut {
    let (checksum, count, value_bytes) = kv_snapshot_checksum(db);
    FinishOut {
        checksum,
        extra: vec![
            ("entries", ExtraVal::U(count as u64)),
            ("value_bytes", ExtraVal::U(value_bytes)),
        ],
    }
}

fn kv_set_small_body<const WG: u32>(db: &mut Kvdb<BenchFlash<WG>>) -> BodyOut {
    let mut out = BodyOut::default();
    let mut key = [0u8; 8];
    let mut value = [0u8; 8];
    for k in 0..64u32 {
        let klen = key_num(b"kv", k % 8, &mut key);
        kv_value(0x11, k, 8, &mut value);
        let result = db.set(&key[..klen], &value);
        out.errcodes.record(&result);
        out.ops += 1;
    }
    out
}

fn kv_update_body<const WG: u32>(db: &mut Kvdb<BenchFlash<WG>>) -> BodyOut {
    let mut out = BodyOut::default();
    let mut value = [0u8; 32];
    for k in 0..1000u32 {
        kv_value(0x22, k, 32, &mut value);
        let result = db.set(b"counter", &value);
        out.errcodes.record(&result);
        out.ops += 1;
    }
    out
}

fn kv_get_hot_body<const WG: u32>(db: &mut Kvdb<BenchFlash<WG>>) -> BodyOut {
    let mut out = BodyOut::default();
    let mut key = [0u8; 16];
    let mut buf = [0u8; 64];
    let mut sum_bytes = 0u64;
    for i in 0..2000u32 {
        let klen = key_num(b"key", i % 32, &mut key);
        if let Ok(n) = db.get(&key[..klen], &mut buf) {
            if let Some(n) = n {
                out.hits += 1;
                sum_bytes += n as u64;
            }
        }
        out.ops += 1;
    }
    out.extra.push(("sum_returned_bytes", ExtraVal::U(sum_bytes)));
    out
}

fn kv_get_miss_body<const WG: u32>(db: &mut Kvdb<BenchFlash<WG>>) -> BodyOut {
    let mut out = BodyOut::default();
    let mut key = [0u8; 24];
    let mut buf = [0u8; 64];
    let mut sum_bytes = 0u64;
    for i in 0..500u32 {
        let klen = key_num(b"absent", i, &mut key);
        if let Ok(Some(n)) = db.get(&key[..klen], &mut buf) {
            out.hits += 1;
            sum_bytes += n as u64;
        }
        out.ops += 1;
    }
    out.extra.push(("sum_returned_bytes", ExtraVal::U(sum_bytes)));
    out
}

fn kv_delete_body<const WG: u32>(db: &mut Kvdb<BenchFlash<WG>>) -> BodyOut {
    let mut out = BodyOut::default();
    let mut key = [0u8; 16];
    let mut value = [0u8; 32];
    let mut del_calls = 0u64;
    let mut set_calls = 0u64;
    for _ in 0..3 {
        for i in 0..32u32 {
            let klen = key_num(b"key", i, &mut key);
            match db.remove(&key[..klen]) {
                Ok(_) => out.errcodes.ok += 1,
                Err(e) => out.errcodes.record_err(&e),
            }
            del_calls += 1;
            kv_value(0x33, i, 32, &mut value);
            let result = db.set(&key[..klen], &value);
            out.errcodes.record(&result);
            set_calls += 1;
        }
    }
    out.ops = del_calls + set_calls;
    out.extra.push(("del_calls", ExtraVal::U(del_calls)));
    out.extra.push(("set_calls", ExtraVal::U(set_calls)));
    out
}

fn kv_iterate_body<const WG: u32>(db: &mut Kvdb<BenchFlash<WG>>) -> BodyOut {
    let mut out = BodyOut::default();
    let mut last_count = 0u64;
    let mut last_sum = 0u64;
    for _ in 0..100 {
        let mut count = 0u64;
        let mut sum = 0u64;
        let mut it = db.iter();
        while let Some(entry) = it.next() {
            match entry {
                Ok(entry) => {
                    count += 1;
                    sum += entry.value_len as u64;
                }
                Err(e) => {
                    out.errcodes.record_err(&e);
                    break;
                }
            }
        }
        last_count = count;
        last_sum = sum;
    }
    out.ops = 100;
    out.extra.push(("entries_per_iter", ExtraVal::U(last_count)));
    out.extra
        .push(("sum_value_len_per_iter", ExtraVal::U(last_sum)));
    out
}

fn kv_large_body<const WG: u32>(db: &mut Kvdb<BenchFlash<WG>>) -> BodyOut {
    let mut out = BodyOut::default();
    let mut value = [0u8; 1024];
    for i in 0..50u32 {
        kv_value(0x44, i, 1024, &mut value);
        let result = db.set(b"big", &value);
        out.errcodes.record(&result);
        out.ops += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// TS scenario data/setup/body
// ---------------------------------------------------------------------------

fn ts_populate_full<F: Flash>(db: &mut TsdbImpl<F, i32>) -> u32 {
    let mut payload = [0u8; TS_PAYLOAD_LEN];
    let mut count = 0u32;
    for i in 0..TS_APPEND_MAX_CALLS {
        ts_payload(i, &mut payload);
        if db.append(TS_START_TIME + i as i32, &payload).is_err() {
            break;
        }
        count += 1;
    }
    count
}

fn ts_setup_full<F: Flash>(db: &mut TsdbImpl<F, i32>) {
    db.set_rollover(false);
    ts_populate_full(db);
}

fn ts_finish_snapshot<F: Flash>(db: &mut TsdbImpl<F, i32>) -> FinishOut {
    let (checksum, count, payload_bytes) = ts_snapshot_checksum(db);
    FinishOut {
        checksum,
        extra: vec![
            ("entries_after", ExtraVal::U(count as u64)),
            ("payload_bytes_after", ExtraVal::U(payload_bytes)),
        ],
    }
}

fn ts_append_body<const WG: u32>(db: &mut Tsdb<BenchFlash<WG>>) -> BodyOut {
    let mut out = BodyOut::default();
    let mut appended = 0u64;
    let mut calls = 0u64;
    let mut saved_full = false;
    let mut payload = [0u8; TS_PAYLOAD_LEN];
    for i in 0..TS_APPEND_MAX_CALLS {
        ts_payload(i, &mut payload);
        let result = db.append(TS_START_TIME + i as i32, &payload);
        calls += 1;
        match result {
            Ok(()) => {
                out.errcodes.ok += 1;
                appended += 1;
            }
            Err(Error::SavedFull) => {
                out.errcodes.saved_full += 1;
                saved_full = true;
                break;
            }
            Err(e) => {
                out.errcodes.record_err(&e);
                break;
            }
        }
    }
    out.ops = appended;
    out.extra.push(("appended", ExtraVal::U(appended)));
    out.extra.push(("append_calls", ExtraVal::U(calls)));
    out.extra.push(("saved_full", ExtraVal::B(saved_full)));
    out
}

fn ts_rollover_body<const WG: u32>(
    db: &mut Tsdb<BenchFlash<WG>>,
    capacity: u32,
) -> BodyOut {
    let mut out = BodyOut::default();
    let mut ok = 0u64;
    let mut calls = 0u64;
    let mut payload = [0u8; TS_PAYLOAD_LEN];
    for i in 0..capacity.saturating_mul(3) {
        ts_payload(i, &mut payload);
        let result = db.append(TS_START_TIME + i as i32, &payload);
        calls += 1;
        match result {
            Ok(()) => {
                out.errcodes.ok += 1;
                ok += 1;
            }
            Err(e) => out.errcodes.record_err(&e),
        }
    }
    out.ops = calls;
    out.extra.push(("capacity", ExtraVal::U(capacity as u64)));
    out.extra.push(("appends_ok", ExtraVal::U(ok)));
    out.extra.push(("append_calls", ExtraVal::U(calls)));
    out
}

fn ts_iter_body<const WG: u32>(db: &mut Tsdb<BenchFlash<WG>>, reverse: bool) -> BodyOut {
    let mut out = BodyOut::default();
    let mut last_count = 0u64;
    let mut last_sum = 0u64;
    for _ in 0..20 {
        let mut count = 0u64;
        let mut sum = 0u64;
        if reverse {
            let mut it = db.iter_reverse();
            while let Some(entry) = it.next() {
                match entry {
                    Ok(entry) => {
                        count += 1;
                        sum += entry.log_len as u64;
                    }
                    Err(e) => {
                        out.errcodes.record_err(&e);
                        break;
                    }
                }
            }
        } else {
            let mut it = db.iter();
            while let Some(entry) = it.next() {
                match entry {
                    Ok(entry) => {
                        count += 1;
                        sum += entry.log_len as u64;
                    }
                    Err(e) => {
                        out.errcodes.record_err(&e);
                        break;
                    }
                }
            }
        }
        last_count = count;
        last_sum = sum;
    }
    out.ops = 20;
    out.extra.push(("entries_per_iter", ExtraVal::U(last_count)));
    out.extra
        .push(("sum_payload_len_per_iter", ExtraVal::U(last_sum)));
    out
}

#[inline]
fn lcg_next(state: &mut u64) -> u64 {
    *state = (*state * 1_103_515_245 + 12_345) % 2_147_483_648;
    *state
}

fn ts_range_query(state: &mut u64) -> (i32, i32) {
    let off = lcg_next(state) % 3000;
    let len = 50 + lcg_next(state) % 451;
    let from = TS_START_TIME + off as i32;
    (from, from + len as i32)
}

fn ts_by_time_body_mode<const WG: u32>(
    db: &mut Tsdb<BenchFlash<WG>>,
    counts_out: Rc<RefCell<Vec<u32>>>,
) -> BodyOut {
    let mut out = BodyOut::default();
    let mut p = counts_out.borrow_mut();
    p.clear();
    let mut total = 0u64;
    let mut payload_sum = 0u64;
    let mut state = TS_LCG_SEED;
    for _ in 0..TS_RANGE_QUERIES {
        let (from, to) = ts_range_query(&mut state);
        let mut count = 0u32;
        let mut sum = 0u64;
        let mut it = db.iter_by_time(from, to);
        while let Some(entry) = it.next() {
            if let Ok(entry) = entry {
                count += 1;
                sum += entry.log_len as u64;
            }
        }
        p.push(count);
        total += count as u64;
        payload_sum += sum;
    }
    out.ops = TS_RANGE_QUERIES as u64;
    out.extra.push(("queries", ExtraVal::U(TS_RANGE_QUERIES as u64)));
    out.extra.push(("sum_matches", ExtraVal::U(total)));
    out.extra
        .push(("sum_payload_len", ExtraVal::U(payload_sum)));
    out
}

fn ts_query_count_body<const WG: u32>(
    db: &mut Tsdb<BenchFlash<WG>>,
    counts_out: Rc<RefCell<Vec<u32>>>,
) -> BodyOut {
    let mut out = BodyOut::default();
    let mut p = counts_out.borrow_mut();
    p.clear();
    let mut total = 0u64;
    let mut state = TS_LCG_SEED;
    for _ in 0..TS_RANGE_QUERIES {
        let (from, to) = ts_range_query(&mut state);
        let count = db
            .query_count(from, to, TslStatus::Write)
            .unwrap_or(0) as u32;
        p.push(count);
        total += count as u64;
    }
    out.ops = TS_RANGE_QUERIES as u64;
    out.extra.push(("queries", ExtraVal::U(TS_RANGE_QUERIES as u64)));
    out.extra.push(("sum_count", ExtraVal::U(total)));
    out
}

fn range_checksum(counts: &[u32]) -> u32 {
    let mut h = BENCH_FNV32_OFFSET;
    h = fnv_u32(h, TS_RANGE_QUERIES);
    for &c in counts {
        h = fnv_u32(h, c);
    }
    h
}

// ---------------------------------------------------------------------------
// Scenario lists
// ---------------------------------------------------------------------------

fn run_kv_scenarios<const WG: u32>(runs: u32) -> Vec<Scenario> {
    let mut v = Vec::new();
    v.push(bench_kv::<WG, _, _, _>(
        "kv_set_small",
        runs,
        |_db| {},
        kv_set_small_body::<WG>,
        kv_finish_state,
    ));
    v.push(bench_kv::<WG, _, _, _>(
        "kv_update_1k",
        runs,
        |db| {
            let mut value = [0u8; 32];
            kv_value(0x22, 0, 32, &mut value);
            db.set(b"counter", &value).expect("setup");
        },
        kv_update_body::<WG>,
        |db| {
            let mut out = kv_finish_state(db);
            let mut expect = [0u8; 32];
            let mut got = [0u8; 32];
            kv_value(0x22, 999, 32, &mut expect);
            let n = db.get(b"counter", &mut got).unwrap_or(None);
            out.extra.push((
                "final_value_len_ok",
                ExtraVal::B(n == Some(expect.len())),
            ));
            out.extra.push((
                "final_value_matches",
                ExtraVal::B(n == Some(expect.len()) && got == expect),
            ));
            out.extra.push(("gc_sector_erases", ExtraVal::U(0)));
            out
        },
    ));
    v.push(bench_kv::<WG, _, _, _>(
        "kv_get_hot",
        runs,
        kv_setup_hot,
        kv_get_hot_body::<WG>,
        kv_finish_state,
    ));
    v.push(bench_kv::<WG, _, _, _>(
        "kv_get_miss",
        runs,
        kv_setup_hot,
        kv_get_miss_body::<WG>,
        kv_finish_state,
    ));
    v.push(bench_kv::<WG, _, _, _>(
        "kv_delete",
        runs,
        kv_setup_hot,
        kv_delete_body::<WG>,
        kv_finish_state,
    ));
    v.push(bench_kv::<WG, _, _, _>(
        "kv_iterate",
        runs,
        kv_setup_hot,
        kv_iterate_body::<WG>,
        kv_finish_state,
    ));
    v.push(bench_kv_mount::<WG>(runs));
    v.push(bench_kv::<WG, _, _, _>(
        "kv_large",
        runs,
        |_db| {},
        kv_large_body::<WG>,
        kv_finish_state,
    ));
    // Fill in gc_sector_erases from the recorded flash counters (C reports the
    // erase count of the measured phase).
    for s in v.iter_mut() {
        if s.name == "kv_update_1k" {
            for e in s.extra.iter_mut() {
                if e.0 == "gc_sector_erases" {
                    e.1 = ExtraVal::U(s.flash.erases);
                }
            }
        }
    }
    v
}

fn ts_probe_capacity<const WG: u32>() -> u32 {
    let counters = Rc::new(Counters::default());
    let flash = BenchFlash::<WG>::new(counters);
    let mut db =
        Tsdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE), TS_MAX_LOG_LEN).expect("probe mount");
    db.set_rollover(false);
    ts_populate_full(&mut db)
}

fn run_ts_scenarios<const WG: u32>(runs: u32) -> Vec<Scenario> {
    let capacity = ts_probe_capacity::<WG>();
    let mut v = Vec::new();

    v.push(bench_ts::<WG, _, _, _>(
        "tsl_append",
        runs,
        |db| db.set_rollover(false),
        ts_append_body::<WG>,
        ts_finish_snapshot,
    ));
    v.push(bench_ts::<WG, _, _, _>(
        "tsl_append_rollover",
        runs,
        |db| db.set_rollover(true),
        move |db| ts_rollover_body::<WG>(db, capacity),
        ts_finish_snapshot,
    ));
    v.push(bench_ts::<WG, _, _, _>(
        "tsl_iter_fwd",
        runs,
        ts_setup_full,
        |db| ts_iter_body(db, false),
        ts_finish_snapshot,
    ));
    v.push(bench_ts::<WG, _, _, _>(
        "tsl_iter_rev",
        runs,
        ts_setup_full,
        |db| ts_iter_body(db, true),
        ts_finish_snapshot,
    ));

    let counts = Rc::new(RefCell::new(Vec::new()));
    let body_counts = counts.clone();
    v.push(bench_ts::<WG, _, _, _>(
        "tsl_by_time",
        runs,
        ts_setup_full,
        move |db| ts_by_time_body_mode::<WG>(db, body_counts.clone()),
        move |_db| FinishOut {
            checksum: range_checksum(&counts.borrow()),
            extra: vec![],
        },
    ));

    let counts2 = Rc::new(RefCell::new(Vec::new()));
    let body_counts2 = counts2.clone();
    v.push(bench_ts::<WG, _, _, _>(
        "tsl_query_count",
        runs,
        ts_setup_full,
        move |db| ts_query_count_body::<WG>(db, body_counts2.clone()),
        move |_db| FinishOut {
            checksum: range_checksum(&counts2.borrow()),
            extra: vec![],
        },
    ));

    v.push(bench_ts_mount::<WG>(runs));
    v.push(bench_ts::<WG, _, _, _>(
        "tsl_clean",
        runs,
        ts_setup_full,
        |db| {
            let mut out = BodyOut::default();
            let mut appends = 0u32;
            for _ in 0..20 {
                db.clean().expect("clean");
                appends = ts_populate_full(db);
            }
            out.ops = 20;
            out.extra.push(("cycles", ExtraVal::U(20)));
            out.extra
                .push(("appends_per_cycle", ExtraVal::U(appends as u64)));
            out
        },
        ts_finish_snapshot,
    ));
    v
}

fn bench_ts_mount<const WG: u32>(runs: u32) -> Scenario {
    let mut samples = Vec::with_capacity(runs as usize);
    let mut last_flash = CounterSnapshot::default();
    let mut errcodes = Errcodes::default();
    let mut mount_ok = 0u64;
    let mut checksum = 0u32;
    let mut populated = 0u32;
    for _ in 0..runs {
        let counters = Rc::new(Counters::default());
        let flash = BenchFlash::<WG>::new(counters.clone());
        let mut db =
            Tsdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE), TS_MAX_LOG_LEN).expect("ts mount");
        db.set_rollover(false);
        populated = ts_populate_full(&mut db);
        // Upstream FlashDB cannot iterate a full no-rollover DB after remount,
        // so the functional checksum is taken from the pre-mount state.
        checksum = ts_snapshot_checksum(&mut db).0;
        let mut holder = Some(db.into_flash());
        counters.reset();
        let start = Instant::now();
        let mut run_ok = 0u64;
        for _ in 0..50 {
            let flash = holder.take().expect("flash available");
            match Tsdb::mount(flash, Config::new(SEC_SIZE, MAX_SIZE), TS_MAX_LOG_LEN) {
                Ok(db) => {
                    run_ok += 1;
                    holder = Some(db.into_flash());
                }
                Err(e) => {
                    errcodes.record_err(&e);
                    break;
                }
            }
        }
        let elapsed = start.elapsed().as_nanos() as u64;
        samples.push(elapsed);
        last_flash = counters.snapshot();
        mount_ok = run_ok;
    }
    let median_ns = median(&mut samples);
    Scenario {
        name: "tsl_mount",
        ops: 50,
        hits: 0,
        runs,
        total_ns_median: median_ns,
        ns_per_op: median_ns as f64 / 50.0,
        checksum,
        errcodes,
        flash: last_flash,
        extra: vec![
            ("mount_ok", ExtraVal::U(mount_ok)),
            ("populated", ExtraVal::U(populated as u64)),
            ("checksum_from_pre_mount_state", ExtraVal::B(true)),
            ("flash_consistent", ExtraVal::B(true)),
        ],
    }
}

// ---------------------------------------------------------------------------
// JSON / main
// ---------------------------------------------------------------------------

fn footprint<const WG: u32>() -> String {
    format!(
        concat!(
            "{{\"kvdb_bytes\":{},\"kvdb_no_cache_bytes\":{},\"kvdb_16_cache_bytes\":{},",
            "\"tsdb_bytes\":{},",
            "\"note\":\"Rust struct sizes (KVDB default 64-entry cache, no cache, 16-entry; TSDB)\"}}"
        ),
        core::mem::size_of::<Kvdb<DummyFlash<WG>>>(),
        core::mem::size_of::<KvdbNoCache<DummyFlash<WG>>>(),
        core::mem::size_of::<KvdbSmallCache<DummyFlash<WG>>>(),
        core::mem::size_of::<Tsdb<DummyFlash<WG>>>(),
    )
}

fn emit_json<const WG: u32>(runs: u32, scenarios: &[Scenario], out_path: &str) -> String {
    let mut list = String::new();
    for (i, s) in scenarios.iter().enumerate() {
        if i > 0 {
            list.push(',');
        }
        list.push_str(&s.to_json());
    }
    let json = format!(
        concat!(
            "{{\"impl\":\"rust\",\"version\":\"{}\",\"opt\":\"release\",\"write_gran\":{},",
            "\"config\":{{\"write_gran\":{},\"sector_size\":{},\"sectors_per_db\":{},",
            "\"partition_size\":{},\"ts_timestamp_bits\":32,\"ts_max_log_len\":{},",
            "\"runs\":{},\"opt\":\"release\"}},",
            "\"footprint\":{},\"scenarios\":[{}]}}"
        ),
        env!("CARGO_PKG_VERSION"),
        WG,
        WG,
        SEC_SIZE,
        MAX_SIZE / SEC_SIZE,
        MAX_SIZE,
        TS_MAX_LOG_LEN,
        runs,
        footprint::<WG>(),
        list,
    );
    std::fs::create_dir_all("harness/results").ok();
    std::fs::write(out_path, &json).ok();
    json
}

fn main() {
    let mut wg: u32 = 8;
    let mut runs: u32 = DEFAULT_RUNS;
    let mut with_ts = true;
    let mut out: Option<String> = None;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--wg" => {
                i += 1;
                wg = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(8);
            }
            "--runs" => {
                i += 1;
                runs = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_RUNS);
            }
            "--no-ts" => with_ts = false,
            "--with-ts" => with_ts = true,
            "--out" => {
                i += 1;
                out = args.get(i).cloned();
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let out_path = out.unwrap_or_else(|| format!("harness/results/rust_wg{wg}_host.json"));
    match wg {
        1 => {
            let mut s = run_kv_scenarios::<1>(runs);
            if with_ts {
                s.extend(run_ts_scenarios::<1>(runs));
            }
            println!("{}", emit_json::<1>(runs, &s, &out_path));
        }
        8 => {
            let mut s = run_kv_scenarios::<8>(runs);
            if with_ts {
                s.extend(run_ts_scenarios::<8>(runs));
            }
            println!("{}", emit_json::<8>(runs, &s, &out_path));
        }
        32 => {
            let s = run_kv_scenarios::<32>(runs);
            println!("{}", emit_json::<32>(runs, &s, &out_path));
        }
        other => {
            eprintln!("unsupported write granularity {other} (supported: 1, 8, 32)");
            std::process::exit(2);
        }
    }
}
