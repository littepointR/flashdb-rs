//! TEMPORARY micro-benchmark, deleted before completion.
use flashdb::{Config, Flash, Tsdb};
use flashdb_sim::RamFlash;
use std::time::Instant;

const SEC: u32 = 4096;
const MAX: u32 = 16384;
const MAX_LEN: u32 = 32;

fn payload(i: usize) -> [u8; 32] {
    let mut b = [0u8; 32];
    for (j, v) in b.iter_mut().enumerate() {
        *v = ((0x5Au32 + i as u32 * 13 + j as u32 * 5) & 0xFF) as u8;
    }
    b
}

#[test]
fn micro() {
    let mut db: Tsdb<RamFlash<16384, 8>> =
        Tsdb::mount(RamFlash::new(SEC), Config::new(SEC, MAX), MAX_LEN).unwrap();
    db.set_rollover(false);
    let mut i = 0;
    while db.append(1000 + i, &payload(i as usize)).is_ok() {
        i += 1;
    }
    let flash = db.into_flash();
    let mut holder = Some(flash);
    let n = 20_000;
    // warm up
    for _ in 0..100 {
        let f = holder.take().unwrap();
        let db = Tsdb::mount(f, Config::new(SEC, MAX), MAX_LEN).unwrap();
        holder = Some(db.into_flash());
    }
    let start = Instant::now();
    for _ in 0..n {
        let f = holder.take().unwrap();
        let db = Tsdb::mount(f, Config::new(SEC, MAX), MAX_LEN).unwrap();
        holder = Some(db.into_flash());
    }
    let ns = start.elapsed().as_nanos() as f64 / n as f64;
    eprintln!("MOUNT {ns:.1} ns");
    let mut flash = holder.take().unwrap();

    // Raw floor: 8 reads of 44 bytes each.
    let n2 = 200_000;
    let start = Instant::now();
    let mut buf = [0u8; 44];
    for _ in 0..n2 {
        for s in 0..4u32 {
            flash.read(s * SEC, &mut buf).unwrap();
            flash.read(s * SEC, &mut buf).unwrap();
        }
    }
    let ns = start.elapsed().as_nanos() as f64 / n2 as f64;
    eprintln!("8RAW {ns:.1} ns");

    // Iteration: 308 entries per pass.
    let mut db = Tsdb::mount(flash, Config::new(SEC, MAX), MAX_LEN).unwrap();
    let passes = 2000;
    for _ in 0..100 {
        let _ = db.iter().count();
    }
    let start = Instant::now();
    for _ in 0..passes {
        let _ = db.iter().count();
    }
    let ns = start.elapsed().as_nanos() as f64 / passes as f64;
    eprintln!("ITER {ns:.1} ns/pass");

    let start = Instant::now();
    for _ in 0..passes {
        let _ = db.iter_reverse().count();
    }
    let ns = start.elapsed().as_nanos() as f64 / passes as f64;
    eprintln!("ITER_REV {ns:.1} ns/pass");

    let mut seed = 12345u64;
    let start = Instant::now();
    let q = 20_000;
    for _ in 0..q {
        seed = (seed * 1103515245 + 12345) & 0x7FFF_FFFF;
        let off = (seed % 3000) as i32;
        let len = 50 + ((seed >> 8) % 451) as i32;
        let from = 1000 + off;
        let to = from + len;
        let _ = db.iter_by_time(from, to).count();
    }
    let ns = start.elapsed().as_nanos() as f64 / q as f64;
    eprintln!("BYTIME {ns:.1} ns/query");

    // Cost of moving the 16 KiB RAM flash by value (mount takes F by value).
    let last_time = db.last_time();
    let mut holder = Some(db.into_flash());
    let n3 = 200_000;
    let start = Instant::now();
    for _ in 0..n3 {
        let f = holder.take().unwrap();
        holder = Some(f);
    }
    let ns = start.elapsed().as_nanos() as f64 / n3 as f64;
    eprintln!("MOVE_OPTION {ns:.1} ns");
    let _ = (last_time, buf[0]);
}
