//! TEMPORARY probe (verifier scratch) - not part of the final test suite.
use flashdb::{Config, Kvdb};
use flashdb_sim::RamFlash;

#[test]
fn probe_first_record_addr() {
    let mut db = Kvdb::mount(RamFlash::<16384, 8>::new(4096), Config::new(4096, 16384)).unwrap();
    db.set(b"a", b"x").unwrap();
    let mut it = db.iter();
    while let Some(e) = it.next() {
        let e = e.unwrap();
        println!(
            "entry {} addr={} len={} value_len={}",
            String::from_utf8_lossy(e.name()),
            e.addr,
            e.len,
            e.value_len
        );
    }
    let f = db.into_flash();
    println!(
        "stats erases={} writes={} first24={:02x?}",
        f.stats().erase_ops,
        f.stats().write_ops,
        &f.data()[..24]
    );
    println!("sectors: {:02x?}", [
        &f.data()[0..20],
        &f.data()[4096..4116],
        &f.data()[8192..8212],
        &f.data()[12288..12308]
    ]);
}
