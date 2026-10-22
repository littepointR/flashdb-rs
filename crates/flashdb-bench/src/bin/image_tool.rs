//! Raw flash-image writer/reader for the C <-> Rust on-flash compatibility
//! proof. This is the Rust counterpart of `harness/c/image_tool.c`; the two
//! tools must produce byte-identical 32768-byte images and print the same
//! summary for the same image.
//!
//! Usage:
//! ```text
//! image_tool write <out.bin> <wg>   # deterministic workload -> image + summary
//! image_tool read  <in.bin>  <wg>   # mount image (NO format), print summary
//! image_tool selfcheck <wg>         # write twice, compare, read back, sha256
//! ```
//!
//! Image format: raw concatenation, exactly 32768 bytes
//!   [0x0000,0x4000)  KV partition  "fdb_kvdb1" (4 x 4096-byte sectors)
//!   [0x4000,0x8000)  TS partition  "fdb_tsdb1" (4 x 4096-byte sectors)
//!
//! Deterministic workload (identical to the C tool):
//!   KV, value helper kv_value(seed,i,len): byte[j] = (seed + i*31 + j*7) & 0xFF
//!     for i in 0..7:  set("img{i}", kv_value(0x55, i, 8 << (i % 4)))
//!                     -> lengths 8,16,32,64,8,16,32,64
//!     for i in 0..3:  set("img{i}", kv_value(0x56, i, 16))
//!     delete("img7")
//!     for k in 0..399: set("churn", kv_value(0x57, k, 64))
//!   TS, rollover disabled, max_len 256:
//!     for i in 0..19: append(payload = ts_payload(i) 32 bytes, t = 1000 + i)
//!
//! Summary format (exact, identical to the C tool):
//!   KV key=<key> len=<value_len> fnv=<fnv1a32(value) decimal>
//!   TS time=<t> status=<int> len=<log_len> fnv=<fnv1a32(payload) decimal>
//!   KVCOUNT=<n> TSCOUNT=<n>
//! On mount/check failure it prints `ERROR=<code>` (numeric FlashDB
//! `fdb_err_t`, 255 = I/O) and exits 2.

use std::fs;
use std::io::Write as _;
use std::process::{Command, Stdio};

use flashdb::{Config, Error, Kvdb, Tsdb, TslStatus};
use flashdb_sim::{RamFlash, SimError};

/// FlashDB sector (erase block) size.
const SEC_SIZE: u32 = 4096;
/// Size of one database partition.
const PART_SIZE: u32 = 16384;
/// Size of one flash simulator, i.e. one partition.
const FLASH_SIZE: usize = PART_SIZE as usize;
/// Full image size: KV partition followed by TS partition.
const IMAGE_SIZE: usize = 2 * FLASH_SIZE;
/// TSDB maximum log length used by the image workload.
const TS_MAX_LEN: u32 = 256;
/// First timestamp of the image workload.
const TS_START_TIME: i32 = 1000;
/// TS workload entry count.
const TS_ENTRY_COUNT: u32 = 20;
/// Number of churn updates (exactly 400; documented in the task contract).
const CHURN_UPDATES: u32 = 400;

/// FlashDB `fdb_err_t` values (reference/FlashDB/inc/fdb_def.h).
const FDB_ERASE_ERR: i32 = 1;
const FDB_READ_ERR: i32 = 2;
const FDB_WRITE_ERR: i32 = 3;
const FDB_KV_NAME_ERR: i32 = 5;
const FDB_SAVED_FULL: i32 = 7;
const FDB_INIT_FAILED: i32 = 8;
/// C image tool's `IMAGE_ERR_IO`.
const IMAGE_ERR_IO: i32 = 255;

// ---------------------------------------------------------------------------
// deterministic data generation (identical to harness/c/bench_common.c)
// ---------------------------------------------------------------------------

/// `kv_value(seed, i, len)`: `buf[j] = (seed + i*31 + j*7) & 0xFF`.
fn kv_value(seed: u32, i: u32, len: usize) -> Vec<u8> {
    (0..len)
        .map(|j| {
            (seed
                .wrapping_add(i.wrapping_mul(31))
                .wrapping_add((j as u32).wrapping_mul(7))
                & 0xFF) as u8
        })
        .collect()
}

/// `ts_payload(i)`: 32 bytes, `buf[j] = (0x5A + i*13 + j*5) & 0xFF`.
fn ts_payload(i: u32) -> [u8; 32] {
    let mut buf = [0u8; 32];
    for (j, b) in buf.iter_mut().enumerate() {
        *b = (0x5Au32
            .wrapping_add(i.wrapping_mul(13))
            .wrapping_add((j as u32).wrapping_mul(5))
            & 0xFF) as u8;
    }
    buf
}

/// FNV-1a 32-bit, reported as decimal (same as `bench_cs_bytes` in C).
fn fnv1a32(data: &[u8]) -> u32 {
    let mut h = 0x811C_9DC5u32;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

// ---------------------------------------------------------------------------
// errors / exit codes
// ---------------------------------------------------------------------------

enum ToolError {
    /// A database operation failed; mapped to its `fdb_err_t` numeric code.
    Db(Error<SimError>),
    /// The image could not be mounted without modifying (formatting) it.
    WouldFormat,
    /// File or tooling error (C's `IMAGE_ERR_IO`).
    Io(String),
}

impl ToolError {
    fn code(&self) -> i32 {
        match self {
            ToolError::Db(e) => db_error_code(e),
            ToolError::WouldFormat => IMAGE_ERR_IO,
            ToolError::Io(_) => IMAGE_ERR_IO,
        }
    }
}

/// Map the Rust error back to the C `fdb_err_t` the image tool would report.
fn db_error_code(e: &Error<SimError>) -> i32 {
    match e {
        Error::FlashRead(_) | Error::ReadCorrupted => FDB_READ_ERR,
        Error::FlashWrite(_) => FDB_WRITE_ERR,
        Error::FlashErase(_) => FDB_ERASE_ERR,
        Error::SavedFull => FDB_SAVED_FULL,
        Error::InvalidName => FDB_KV_NAME_ERR,
        Error::NotFormatted
        | Error::InvalidConfig
        | Error::UnsupportedWriteGran
        | Error::OutOfBounds => FDB_INIT_FAILED,
        _ => IMAGE_ERR_IO,
    }
}

impl From<Error<SimError>> for ToolError {
    fn from(e: Error<SimError>) -> Self {
        ToolError::Db(e)
    }
}

// ---------------------------------------------------------------------------
// summary printing (identical text to harness/c/image_tool.c)
// ---------------------------------------------------------------------------

type Nor<const WG: u32> = RamFlash<FLASH_SIZE, WG>;

/// Build the summary text of a mounted database pair.
///
/// * KV: one line per record in iteration order, `KV key=... len=... fnv=...`.
/// * TS: one line per record in forward order, `TS time=... status=...`.
/// * Final line `KVCOUNT=<n> TSCOUNT=<n>`.
fn summary_text<const WG: u32>(
    kv: &mut Kvdb<Nor<WG>>,
    ts: &mut Tsdb<Nor<WG>>,
) -> Result<String, ToolError> {
    let mut out = String::new();
    let mut kvcount: u32 = 0;

    let mut it = kv.iter();
    while let Some(entry) = it.next() {
        let entry = entry?;
        let vlen = entry.value_len as usize;
        let mut buf = vec![0u8; vlen];
        let got = it.read_value(&entry, &mut buf)?;
        if got != vlen {
            return Err(ToolError::Db(Error::ReadCorrupted));
        }
        out.push_str(&format!(
            "KV key={} len={} fnv={}\n",
            String::from_utf8_lossy(entry.name()),
            vlen,
            fnv1a32(&buf),
        ));
        kvcount += 1;
    }

    let mut tscount: u32 = 0;
    let mut it = ts.iter();
    while let Some(entry) = it.next() {
        let entry = entry?;
        let len = entry.log_len as usize;
        let mut buf = vec![0u8; len];
        let got = it.read_blob(&entry, &mut buf)?;
        /* C's image_ts_cb reads min(blob size, log_len) and reports
         * FDB_READ_ERR when it comes back short (e.g. PRE_WRITE entries whose
         * payload address is FDB_DATA_UNUSED). */
        if got != len {
            return Err(ToolError::Db(Error::ReadCorrupted));
        }
        out.push_str(&format!(
            "TS time={} status={} len={} fnv={}\n",
            entry.time,
            tsl_status_index(entry.status),
            len,
            fnv1a32(&buf),
        ));
        tscount += 1;
    }

    out.push_str(&format!("KVCOUNT={} TSCOUNT={}\n", kvcount, tscount));
    Ok(out)
}

/// Numeric `fdb_tsl_status_t` value, matching `(int)tsl->status` in C.
fn tsl_status_index(status: TslStatus) -> u32 {
    match status {
        TslStatus::Unused => 0,
        TslStatus::PreWrite => 1,
        TslStatus::Write => 2,
        TslStatus::UserStatus1 => 3,
        TslStatus::Deleted => 4,
        TslStatus::UserStatus2 => 5,
    }
}

// ---------------------------------------------------------------------------
// deterministic write workload
// ---------------------------------------------------------------------------

/// Run the deterministic workload on two fresh 16 KiB partitions and return
/// `(image, summary, kv_stats, ts_stats)`.
fn build_image<const WG: u32>(
) -> Result<(Vec<u8>, String, flashdb_sim::Stats, flashdb_sim::Stats), ToolError> {
    let cfg = Config::new(SEC_SIZE, PART_SIZE);

    /* --- KV partition --- */
    let kv_flash = Nor::<WG>::new(SEC_SIZE);
    let mut kv = Kvdb::mount(kv_flash, cfg)?;

    for i in 0..8u32 {
        let len = 8usize << (i % 4);
        kv.set(format!("img{i}").as_bytes(), &kv_value(0x55, i, len))?;
    }
    for i in 0..4u32 {
        kv.set(format!("img{i}").as_bytes(), &kv_value(0x56, i, 16))?;
    }
    kv.remove(b"img7")?;
    for k in 0..CHURN_UPDATES {
        kv.set(b"churn", &kv_value(0x57, k, 64))?;
    }

    /* --- TS partition (independent 16 KiB flash) --- */
    let ts_flash = Nor::<WG>::new(SEC_SIZE);
    let mut ts = Tsdb::mount(ts_flash, cfg, TS_MAX_LEN)?;
    /* C: fdb_tsdb_control(SET_ROLLOVER, false) after init. */
    ts.set_rollover(false);
    for i in 0..TS_ENTRY_COUNT {
        ts.append(TS_START_TIME + i as i32, &ts_payload(i))?;
    }

    let summary = summary_text(&mut kv, &mut ts)?;

    let kv_flash = kv.into_flash();
    let ts_flash = ts.into_flash();
    let kv_stats = kv_flash.stats();
    let ts_stats = ts_flash.stats();

    let mut image = Vec::with_capacity(IMAGE_SIZE);
    image.extend_from_slice(kv_flash.data());
    image.extend_from_slice(ts_flash.data());
    debug_assert_eq!(image.len(), IMAGE_SIZE);

    Ok((image, summary, kv_stats, ts_stats))
}

// ---------------------------------------------------------------------------
// image reading: mount WITHOUT formatting, verify data is untouched
// ---------------------------------------------------------------------------

/// Load `image` and print its summary.
///
/// The image must mount cleanly: neither database may format/erase/rewrite
/// anything. Rust's `mount` has no C-style "not formatable" flag, so the
/// guarantee is enforced here by comparing the flash contents after mounting
/// with the input image; any difference is reported as an error instead of
/// silently accepting a reformat.
fn read_image_bytes<const WG: u32>(image: &[u8]) -> Result<String, ToolError> {
    if image.len() != IMAGE_SIZE {
        return Err(ToolError::Io(format!(
            "image is {} bytes, expected {}",
            image.len(),
            IMAGE_SIZE
        )));
    }
    let (kv_bytes, ts_bytes) = image.split_at(FLASH_SIZE);
    let cfg = Config::new(SEC_SIZE, PART_SIZE);

    let mut kv_flash = Nor::<WG>::new(SEC_SIZE);
    kv_flash
        .load(kv_bytes)
        .map_err(|e| ToolError::Io(format!("cannot load KV image: {e}")))?;
    let mut kv = Kvdb::mount(kv_flash, cfg)?;
    kv.check()?;

    let mut ts_flash = Nor::<WG>::new(SEC_SIZE);
    ts_flash
        .load(ts_bytes)
        .map_err(|e| ToolError::Io(format!("cannot load TS image: {e}")))?;
    let mut ts = Tsdb::mount(ts_flash, cfg, TS_MAX_LEN)?;

    /* Build the summary first (read-only), then prove no data changed. */
    let summary = summary_text(&mut kv, &mut ts)?;

    let kv_flash = kv.into_flash();
    let ts_flash = ts.into_flash();
    if kv_flash.data() != kv_bytes || ts_flash.data() != ts_bytes {
        return Err(ToolError::WouldFormat);
    }

    Ok(summary)
}

// ---------------------------------------------------------------------------
// file IO / sha256
// ---------------------------------------------------------------------------

fn load_image_file(path: &str) -> Result<Vec<u8>, ToolError> {
    let bytes = fs::read(path).map_err(|e| ToolError::Io(format!("cannot read '{path}': {e}")))?;
    if bytes.len() != IMAGE_SIZE {
        return Err(ToolError::Io(format!(
            "'{path}' is {} bytes, expected {}",
            bytes.len(),
            IMAGE_SIZE
        )));
    }
    Ok(bytes)
}

fn save_image_file(path: &str, image: &[u8]) -> Result<(), ToolError> {
    fs::write(path, image).map_err(|e| ToolError::Io(format!("cannot write '{path}': {e}")))
}

/// SHA-256 through `shasum`/`sha256sum` (both are ubiquitous on the hosts this
/// tool targets; no external crates are available in this workspace).
fn sha256_hex(data: &[u8]) -> Result<String, ToolError> {
    fn run(prog: &str, args: &[&str], data: &[u8]) -> Option<String> {
        let mut child = Command::new(prog)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        {
            let mut stdin = child.stdin.take()?;
            stdin.write_all(data).ok()?;
        }
        let out = child.wait_with_output().ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        text.split_whitespace().next().map(|s| s.to_string())
    }

    run("shasum", &["-a", "256"], data)
        .or_else(|| run("sha256sum", &[], data))
        .ok_or_else(|| ToolError::Io("no shasum/sha256sum available".to_string()))
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

fn format_stats(tag: &str, s: &flashdb_sim::Stats) -> String {
    format!(
        "{}FLASH reads={} read_bytes={} writes={} write_bytes={} erases={} erase_bytes={}",
        tag, s.read_ops, s.read_bytes, s.write_ops, s.write_bytes, s.erase_ops, s.erase_bytes
    )
}

fn cmd_write<const WG: u32>(path: &str) -> Result<(), ToolError> {
    let (image, summary, kv_stats, ts_stats) = build_image::<WG>()?;
    save_image_file(path, &image)?;

    print!("{summary}");
    let image_stats = flashdb_sim::Stats {
        read_ops: kv_stats.read_ops + ts_stats.read_ops,
        read_bytes: kv_stats.read_bytes + ts_stats.read_bytes,
        write_ops: kv_stats.write_ops + ts_stats.write_ops,
        write_bytes: kv_stats.write_bytes + ts_stats.write_bytes,
        erase_ops: kv_stats.erase_ops + ts_stats.erase_ops,
        erase_bytes: kv_stats.erase_bytes + ts_stats.erase_bytes,
        noop_write_bytes: kv_stats.noop_write_bytes + ts_stats.noop_write_bytes,
    };
    eprintln!("{}", format_stats("KV", &kv_stats));
    eprintln!("{}", format_stats("TS", &ts_stats));
    eprintln!("{}", format_stats("IMAGE", &image_stats));
    Ok(())
}

fn cmd_read<const WG: u32>(path: &str) -> Result<(), ToolError> {
    let image = load_image_file(path)?;
    let summary = read_image_bytes::<WG>(&image)?;
    print!("{summary}");
    Ok(())
}

fn cmd_selfcheck<const WG: u32>() -> Result<(), ToolError> {
    let (image_a, summary_a, kv_stats, ts_stats) = build_image::<WG>()?;
    let (image_b, _, _, _) = build_image::<WG>()?;
    if image_a != image_b {
        return Err(ToolError::Io(
            "image write is not deterministic (two writes differ)".to_string(),
        ));
    }

    /* Read the image back through the full mount/verify/summary path. */
    let summary_read = read_image_bytes::<WG>(&image_a)?;
    if summary_a != summary_read {
        return Err(ToolError::Io(
            "write/read summary mismatch for the same image".to_string(),
        ));
    }

    let sha = sha256_hex(&image_a)?;
    let counts = summary_read.lines().last().unwrap_or("");
    let image_stats = flashdb_sim::Stats {
        read_ops: kv_stats.read_ops + ts_stats.read_ops,
        read_bytes: kv_stats.read_bytes + ts_stats.read_bytes,
        write_ops: kv_stats.write_ops + ts_stats.write_ops,
        write_bytes: kv_stats.write_bytes + ts_stats.write_bytes,
        erase_ops: kv_stats.erase_ops + ts_stats.erase_ops,
        erase_bytes: kv_stats.erase_bytes + ts_stats.erase_bytes,
        noop_write_bytes: kv_stats.noop_write_bytes + ts_stats.noop_write_bytes,
    };

    println!("PASS: image round-trip wg={WG}");
    println!("  image:    (in-memory) {IMAGE_SIZE} bytes");
    println!("  sha256:   {sha}");
    println!("  kvc/t:    {counts}");
    println!("  flash:");
    println!("    {}", format_stats("KV", &kv_stats));
    println!("    {}", format_stats("TS", &ts_stats));
    println!("    {}", format_stats("IMAGE", &image_stats));
    Ok(())
}

fn usage(argv0: &str) {
    eprintln!(
        "usage: {argv0} write <out.bin> <wg>   # build deterministic image + print summary
       {argv0} read  <in.bin>  <wg>   # mount image (never format) + print summary
       {argv0} selfcheck <wg>         # write twice, compare, read back, sha256
  <wg> is the flash write granularity in bits: 8 or 1"
    );
}

fn run<const WG: u32>(cmd: &str, path: Option<&str>) -> i32 {
    let result = match cmd {
        "write" => cmd_write::<WG>(path.expect("path required")),
        "read" => cmd_read::<WG>(path.expect("path required")),
        "selfcheck" => cmd_selfcheck::<WG>(),
        _ => unreachable!(),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            match &e {
                ToolError::Io(msg) => eprintln!("image_tool: {msg}"),
                ToolError::WouldFormat => eprintln!(
                    "image_tool: mounting would format or modify the image; refusing to touch it"
                ),
                ToolError::Db(err) => eprintln!("image_tool: database error: {err}"),
            }
            println!("ERROR={}", e.code());
            2
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let argv0 = args.first().map(String::as_str).unwrap_or("image_tool");

    if args.len() < 2 {
        usage(argv0);
        std::process::exit(2);
    }
    let cmd = args[1].as_str();
    let (path, wg) = match cmd {
        "write" | "read" => {
            if args.len() != 4 {
                usage(argv0);
                std::process::exit(2);
            }
            (Some(args[2].as_str()), args[3].parse::<u32>().unwrap_or(0))
        }
        "selfcheck" => {
            if args.len() != 3 {
                usage(argv0);
                std::process::exit(2);
            }
            (None, args[2].parse::<u32>().unwrap_or(0))
        }
        "-h" | "--help" | "help" => {
            usage(argv0);
            std::process::exit(0);
        }
        other => {
            eprintln!("image_tool: unknown command '{other}'");
            usage(argv0);
            std::process::exit(2);
        }
    };

    let code = match wg {
        8 => run::<8>(cmd, path),
        1 => run::<1>(cmd, path),
        other => {
            eprintln!("image_tool: <wg> must be 8 or 1 (got {other})");
            std::process::exit(2);
        }
    };
    std::process::exit(code);
}
