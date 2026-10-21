//! Port of the upstream FlashDB KVDB test suite
//! (`reference/FlashDB/tests/fdb_kvdb_tc.c`) to the Rust API.
//!
//! The original test runs under the RT-Thread utest framework on top of a
//! POSIX file backend. Only the *semantics* are portable, so every test below
//! mirrors one upstream test case, adapted to:
//!
//! * `Kvdb::mount(flashdb_sim::RamFlash<16384, WG>, Config::new(4096, 16384))`
//!   instead of `fdb_kvdb_init()` on a 4-sector RAM/FAL partition,
//! * `into_flash()` + `mount()` as a "reboot" (the whole flash state is kept),
//! * typed `Result`/`Option` instead of C error codes / out-parameters.
//!
//! Intentional API mapping (documented instead of asserted, per the porting
//! contract):
//!
//! | upstream C                                   | Rust equivalent                |
//! |----------------------------------------------|--------------------------------|
//! | `fdb_kv_set_blob(key, NULL, 0)` deletes       | `Kvdb::remove(key)`            |
//! | `fdb_kv_del` returns `FDB_KV_NAME_ERR` for a  | `remove()` returns `Ok(false)` |
//! | missing key                                   |                                |
//! | `fdb_kv_get_blob` returns the full value len  | `get()` returns the copied len,|
//! | even when the caller buffer is smaller        | `find().value_len` is the full |
//! |                                               | length                         |
//! | `fdb_kv_set_default()` (no default KVs)       | `Kvdb::format()`               |
//! | `struct fdb_kvdb.parent.oldest_addr`          | not exposed; we assert the     |
//! |                                               | observable equivalent: the     |
//! |                                               | exact iteration order, which   |
//! |                                               | starts at `oldest_addr`        |
//!
//! The two write granularities used by the C harness (`FDB_WRITE_GRAN` 8 and 1)
//! are generated from one implementation, exactly like upstream is compiled
//! twice. `TEST_KV_VALUE_LEN` is recomputed with the same macro arithmetic as
//! the upstream file (including its intentional use of the raw, unwrapped
//! struct sizes) so that the GC sector layout is identical.

use flashdb::{Config, Kvdb};
use flashdb_sim::RamFlash;

const SECTOR_SIZE: u32 = 4096;
const SECTOR_NUM: u32 = 4;
const KV_NAME_MAX: u32 = 64;

// ---------------------------------------------------------------------------
// upstream macro arithmetic (`fdb_kvdb_tc.c` lines 71-104)
// ---------------------------------------------------------------------------

fn wg_bytes(wg: u32) -> u32 {
    (wg + 7) / 8
}

fn align_up(value: u32, align: u32) -> u32 {
    if align <= 1 {
        value
    } else {
        (value + align - 1) & !(align - 1)
    }
}

/// `FDB_STATUS_TABLE_SIZE(status_number)` (fdb_low_lvl.h).
fn status_table_size(wg: u32, status_num: u32) -> u32 {
    if wg == 1 {
        (status_num * wg + 7) / 8
    } else {
        ((status_num - 1) * wg + 7) / 8
    }
}

/// `TEST_KV_VALUE_LEN` exactly as the upstream test derives it.
fn test_kv_value_len(wg: u32) -> usize {
    let w = wg_bytes(wg);
    // _TKV_SEC_HDR_RAW_SZ = store + dirty + magic + combined + reserved
    let sec_hdr_raw = status_table_size(wg, 4) + status_table_size(wg, 4) + 4 + 4 + 4;
    let sec_hdr = align_up(sec_hdr_raw, w);
    // _TKV_KV_HDR_RAW_SZ = status + magic + len + crc32 + name_len + value_len
    let kv_hdr_raw = status_table_size(wg, 6) + 4 + 4 + 4 + 1 + 4;
    let kv_hdr = align_up(kv_hdr_raw, w);
    let base = kv_hdr + align_up(3, w); // "kv0".."kv5"
    let usable = SECTOR_SIZE - sec_hdr;
    // ceil((usable - 3*base) / 4), then align
    align_up((usable - 3 * base + 3) / 4, w) as usize
}

const _: u32 = SECTOR_SIZE;
const _: u32 = KV_NAME_MAX;

// The upstream file derives the value length from macros; for the two
// supported granularities that yields these values (verified with a C probe
// built from the reference implementation):
//   WG=8 -> 1001, WG=1 -> 1005.
#[test]
fn upstream_value_len_matches_c_probe() {
    assert_eq!(test_kv_value_len(8), 1001);
    assert_eq!(test_kv_value_len(1), 1005);
}

// ---------------------------------------------------------------------------
// shared table helpers
// ---------------------------------------------------------------------------

/// One row of `struct test_kv` in the C file.
#[derive(Clone)]
struct TestKv {
    name: &'static str,
    /// Byte filling the `value_len`-byte payload (the C test uses a string
    /// literal's first byte and lets the rest be adjacent rodata; the payload
    /// content is irrelevant for GC layout, so we use a deterministic fill).
    fill: u8,
    /// `value_len`, or a multiple of the base value length for kv4/kv5.
    repeats: usize,
    /// Expected sector index of the live record (`addr / sector_size`).
    sector: u32,
    /// Only rows flagged `changed` are written (upstream `is_changed`).
    changed: bool,
}

fn row(name: &'static str, fill: u8, sector: u32, changed: bool) -> TestKv {
    TestKv {
        name,
        fill,
        repeats: 1,
        sector,
        changed,
    }
}

macro_rules! kvdb_parity_for_wg {
    ($modname:ident, $wg:literal) => {
        mod $modname {
            use super::*;

            type Nor = RamFlash<16384, $wg>;
            const WG: u32 = $wg;

            fn cfg() -> Config {
                Config::new(SECTOR_SIZE, SECTOR_SIZE * SECTOR_NUM)
            }

            fn fresh() -> Kvdb<Nor> {
                Kvdb::mount(Nor::new(SECTOR_SIZE), cfg()).unwrap()
            }

            fn reboot(db: Kvdb<Nor>) -> Kvdb<Nor> {
                Kvdb::mount(db.into_flash(), cfg()).unwrap()
            }

            fn base_len() -> usize {
                test_kv_value_len(WG)
            }

            fn payload(row: &TestKv) -> Vec<u8> {
                let len = base_len() * row.repeats;
                (0..len)
                    .map(|i| row.fill.wrapping_add((i as u8).wrapping_mul(7)))
                    .collect()
            }

            /// All live records in iteration order: `(name, value, sector)`.
            fn live<const SIZE: usize>(
                db: &mut Kvdb<RamFlash<SIZE, $wg>>,
            ) -> Vec<(String, Vec<u8>, u32)> {
                let mut out = Vec::new();
                let mut it = db.iter();
                while let Some(entry) = it.next() {
                    let entry = entry.unwrap();
                    let mut buf = vec![0u8; entry.value_len as usize];
                    let n = it.read_value(&entry, &mut buf).unwrap();
                    buf.truncate(n);
                    out.push((
                        String::from_utf8(entry.name().to_vec()).unwrap(),
                        buf,
                        entry.addr / SECTOR_SIZE,
                    ));
                }
                out
            }

            /// `test_save_fdb_by_kvs`: write the rows flagged `changed`.
            fn save(db: &mut Kvdb<Nor>, rows: &[TestKv]) {
                for row in rows.iter().filter(|r| r.changed) {
                    db.set(row.name.as_bytes(), &payload(row)).unwrap();
                }
            }

            /// `test_check_fdb_by_kvs`: every expected row is present with the
            /// right value, length and sector, and the live set is exactly the
            /// expected set. Additionally the iteration order must match the C
            /// reference's (this is the observable form of `oldest_addr`:
            /// iteration always starts at `oldest_addr` and walks ring order).
            fn check<const SIZE: usize>(db: &mut Kvdb<RamFlash<SIZE, $wg>>, expected: &[TestKv]) {
                let got = live(db);
                assert_eq!(
                    got.len(),
                    expected.len(),
                    "live record count: got {:?}, expected {} rows",
                    got.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
                    expected.len()
                );
                for (i, exp) in expected.iter().enumerate() {
                    let (name, value, sector) = &got[i];
                    assert_eq!(name, exp.name, "record {i}: name/order mismatch");
                    assert_eq!(
                        *sector, exp.sector,
                        "record {i} ({name}): sector mismatch (oldest_addr / GC layout)"
                    );
                    assert_eq!(
                        value,
                        &payload(exp),
                        "record {i} ({name}): value mismatch"
                    );
                    // Also exercise the random-access read path.
                    let mut buf = value.clone();
                    let n = db.get(exp.name.as_bytes(), &mut buf).unwrap().unwrap();
                    assert_eq!(&buf[..n], value.as_slice(), "record {i} ({name}): get() mismatch");
                }
                db.check().unwrap();
            }

            /// `test_fdb_by_kvs`: save the changed rows, then check all of them.
            fn fdb_by_kvs(db: &mut Kvdb<Nor>, rows: &[TestKv]) {
                save(db, rows);
                check(db, rows);
            }

            fn reboot_and_check(db: Kvdb<Nor>, expected: &[TestKv]) -> Kvdb<Nor> {
                let mut db = reboot(db);
                check(&mut db, expected);
                db
            }

            /// Rows of one GC phase, all base-sized.
            fn phase(rows: &[(&'static str, u8, u32, bool)]) -> Vec<TestKv> {
                rows.iter()
                    .map(|(name, fill, sector, changed)| row(name, *fill, *sector, *changed))
                    .collect()
            }

            // -- test_fdb_kvdb_init_check ----------------------------------

            /// Upstream asserts `oldest_addr == sector 0` on a fresh database.
            /// Rust does not expose `oldest_addr`; the observable equivalent is
            /// that the first record is appended to sector 0 and stays there
            /// across a reboot.
            #[test]
            fn init_check_oldest_sector_is_zero() {
                let mut db = fresh();
                let mut got = live(&mut db);
                assert!(got.is_empty());

                db.set(b"probe", b"x").unwrap();
                got = live(&mut db);
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].2, 0, "first record must land in sector 0");

                let mut db = reboot(db);
                got = live(&mut db);
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].2, 0, "oldest sector after remount must be 0");
            }

            // -- test_fdb_create_kv_blob / change / del --------------------

            #[test]
            fn create_change_and_delete_kv_blob() {
                let mut db = fresh();
                let blob = [0x11u8, 0x22, 0x33, 0x44];

                // create (fdb_kv_set_blob)
                db.set(b"kv_blob_test", &blob).unwrap();
                let mut buf = [0u8; 16];
                let n = db.get(b"kv_blob_test", &mut buf).unwrap().unwrap();
                assert_eq!(n, blob.len());
                assert_eq!(&buf[..n], &blob);

                // change (fdb_kv_set_blob again)
                let blob2 = [0xAAu8; 8];
                db.set(b"kv_blob_test", &blob2).unwrap();
                let n = db.get(b"kv_blob_test", &mut buf).unwrap().unwrap();
                assert_eq!(n, blob2.len());
                assert_eq!(&buf[..n], &blob2);

                // C's small-buffer semantics: `blob.saved.len` (the full value
                // length) is available through `find().value_len`.
                let mut small = [0u8; 3];
                let n = db.get(b"kv_blob_test", &mut small).unwrap().unwrap();
                assert_eq!(n, 3, "get() copies at most buf.len() bytes");
                let full = db.find(b"kv_blob_test").unwrap().unwrap().value_len;
                assert_eq!(full as usize, blob2.len(), "find().value_len is the full length");

                // delete: C uses set_blob(NULL, 0); Rust uses remove().
                assert!(db.remove(b"kv_blob_test").unwrap());
                assert!(db.get(b"kv_blob_test", &mut buf).unwrap().is_none());

                // remount keeps the deletion.
                let mut db = reboot(db);
                assert!(db.get(b"kv_blob_test", &mut buf).unwrap().is_none());
            }

            #[test]
            fn delete_missing_key_is_not_an_error() {
                // Upstream: fdb_kv_del() returns FDB_KV_NAME_ERR. Rust's
                // documented mapping: remove() returns Ok(false).
                let mut db = fresh();
                assert!(!db.remove(b"never-existed").unwrap());
            }

            // -- test_fdb_create_kv / change / del (string API) -----------

            #[test]
            fn create_change_and_delete_kv_string() {
                let mut db = fresh();
                let mut buf = [0u8; 64];

                db.set_str("kv_test", "1234").unwrap();
                assert_eq!(db.get_str(b"kv_test", &mut buf).unwrap(), Some("1234"));

                db.set_str("kv_test", "567890").unwrap();
                assert_eq!(db.get_str(b"kv_test", &mut buf).unwrap(), Some("567890"));

                assert!(db.remove(b"kv_test").unwrap());
                assert_eq!(db.get_str(b"kv_test", &mut buf).unwrap(), None);
                // `oldest_addr` stays at sector 0 after the delete; the
                // observable equivalent is that a new record still lands there.
                let mut db = reboot(db);
                db.set_str("after_reboot", "ok").unwrap();
                let got = live(&mut db);
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].2, 0);
            }

            // -- test_fdb_gc -----------------------------------------------

            #[test]
            fn gc_phases_match_c_layout() {
                let mut db = fresh();
                db.format().unwrap(); // fdb_kv_set_default

                // prepare1: kv0..kv2 in sector0, kv3 in sector1
                let p1 = phase(&[
                    ("kv0", b'0', 0, true),
                    ("kv1", b'1', 0, true),
                    ("kv2", b'2', 0, true),
                    ("kv3", b'3', 1, true),
                ]);
                fdb_by_kvs(&mut db, &p1);
                let mut db = reboot_and_check(db, &p1);

                // prepare2: update kv0 and kv3 (move to sector1)
                let p2 = phase(&[
                    ("kv1", b'1', 0, false),
                    ("kv2", b'2', 0, false),
                    ("kv0", b'0', 1, true),
                    ("kv3", b'3', 1, true),
                ]);
                fdb_by_kvs(&mut db, &p2);
                let mut db = reboot_and_check(db, &p2);

                // update all four: GC runs, live data ends up in sectors 2/3
                let p3 = phase(&[
                    ("kv0", b'0', 2, true),
                    ("kv1", b'1', 2, true),
                    ("kv2", b'2', 2, true),
                    ("kv3", b'3', 3, true),
                ]);
                fdb_by_kvs(&mut db, &p3);
                let mut db = reboot_and_check(db, &p3);

                // update all four again: GC rotates, kv2/kv3 land back in
                // sector0 while kv0/kv1 stay in sector3 (upstream asserts
                // oldest_addr == sector 2; our order check starts there).
                let p4 = phase(&[
                    ("kv0", b'0', 3, true),
                    ("kv1", b'1', 3, true),
                    ("kv2", b'2', 0, true),
                    ("kv3", b'3', 0, true),
                ]);
                fdb_by_kvs(&mut db, &p4);
                let _db = reboot_and_check(db, &p4);
            }

            // -- test_fdb_gc2 ---------------------------------------------

            #[test]
            fn gc2_big_values_match_c_layout() {
                let mut db = fresh();
                db.format().unwrap();

                let mut p1 = phase(&[
                    ("kv0", b'0', 0, true),
                    ("kv1", b'1', 0, true),
                    ("kv2", b'2', 0, true),
                    ("kv3", b'3', 1, true),
                ]);
                fdb_by_kvs(&mut db, &p1);
                let mut db = reboot_and_check(db, &p1);

                let p2 = phase(&[
                    ("kv1", b'1', 0, false),
                    ("kv2", b'2', 0, false),
                    ("kv0", b'0', 1, true),
                    ("kv3", b'3', 1, true),
                ]);
                fdb_by_kvs(&mut db, &p2);
                let mut db = reboot_and_check(db, &p2);

                // add the large kv4 (3x value length): occupies its own sector
                let mut p3 = phase(&[
                    ("kv1", b'1', 0, false),
                    ("kv2", b'2', 0, false),
                    ("kv0", b'0', 1, false),
                    ("kv3", b'3', 1, false),
                ]);
                p3.push(TestKv {
                    name: "kv4",
                    fill: b'4',
                    repeats: 3,
                    sector: 2,
                    changed: true,
                });
                fdb_by_kvs(&mut db, &p3);
                let mut db = reboot_and_check(db, &p3);

                // add kv5 (2x value length): triggers GC of sectors 0 and 1.
                // Expected iteration order (== reference C iteration order):
                // oldest sector 2 (kv4), then sector 3 (kv1,kv2,kv0), then
                // sector 0 (kv3,kv5).
                let mut p4 = phase(&[
                    ("kv4", b'4', 2, false),
                    ("kv1", b'1', 3, false),
                    ("kv2", b'2', 3, false),
                    ("kv0", b'0', 3, false),
                    ("kv3", b'3', 0, false),
                    ("kv5", b'5', 0, true),
                ]);
                // fix up sizes: kv5 is 2x, kv4 is 3x
                for r in p4.iter_mut() {
                    match r.name {
                        "kv5" => r.repeats = 2,
                        "kv4" => r.repeats = 3,
                        _ => {}
                    }
                }
                fdb_by_kvs(&mut db, &p4);
                let _db = reboot_and_check(db, &p4);
            }

            // -- test_fdb_scale_up ----------------------------------------

            /// 4 sectors -> 8 sectors on the same flash: the old data is
            /// preserved and new records are appended to the freshly formatted
            /// sectors. Expected sectors are the ones asserted by the upstream
            /// test's `new_kv_tbl`.
            #[test]
            fn scale_up_from_4_to_8_sectors() {
                const BIG_FLASH: usize = 32768;
                type BigNor = RamFlash<BIG_FLASH, $wg>;

                let mut db =
                    Kvdb::<BigNor>::mount(BigNor::new(SECTOR_SIZE), cfg()).unwrap();
                db.format().unwrap();

                let old = phase(&[
                    ("kv0", b'0', 0, true),
                    ("kv1", b'1', 0, true),
                    ("kv2", b'2', 0, true),
                    ("kv3", b'3', 1, true),
                ]);
                for r in &old {
                    db.set(r.name.as_bytes(), &payload(r)).unwrap();
                }
                check(&mut db, &old);

                // reboot, scaling the database up from 4 to 8 sectors
                let mut db = Kvdb::<BigNor>::mount(
                    db.into_flash(),
                    Config::new(SECTOR_SIZE, SECTOR_SIZE * 8),
                )
                .unwrap();
                check(&mut db, &old);

                let mut new = phase(&[
                    ("kv4", b'4', 0, true),
                    ("kv5", b'5', 0, true),
                    ("kv6", b'6', 0, true),
                    ("kv7", b'7', 0, true),
                ]);
                for _round in 0..3 {
                    for r in &new {
                        db.set(r.name.as_bytes(), &payload(r)).unwrap();
                    }
                }
                // Final sectors from the upstream table: kv4/kv5/kv6 in sector
                // 4, kv7 in sector 5 (old data still in sectors 0/1).
                new[0].sector = 4;
                new[1].sector = 4;
                new[2].sector = 4;
                new[3].sector = 5;

                // The live set is the 4 old + 4 new keys. Check membership,
                // value length/content and final sector for each key without
                // assuming an iteration order (the ring start moved).
                let mut got = live(&mut db);
                got.sort_by(|a, b| a.0.cmp(&b.0));
                let mut expected: Vec<(String, Vec<u8>, u32)> = Vec::new();
                for r in old.iter().chain(new.iter()) {
                    expected.push((r.name.to_string(), payload(r), r.sector));
                }
                expected.sort_by(|a, b| a.0.cmp(&b.0));
                assert_eq!(got.len(), expected.len());
                for (g, e) in got.iter().zip(expected.iter()) {
                    assert_eq!(g.0, e.0, "scale-up: name mismatch");
                    assert_eq!(g.1, e.1, "scale-up: value mismatch for {}", e.0);
                    assert_eq!(g.2, e.2, "scale-up: sector mismatch for {}", e.0);
                }
                db.check().unwrap();
            }

            // -- test_fdb_kvdb_set_default ---------------------------------

            #[test]
            fn set_default_clears_database() {
                let mut db = fresh();
                db.set(b"a", b"1").unwrap();
                db.set(b"b", b"2").unwrap();
                db.format().unwrap(); // fdb_kv_set_default with no default KVs
                assert_eq!(db.stats().unwrap().key_count, 0);
                db.check().unwrap();
                let mut db = reboot(db);
                assert_eq!(db.stats().unwrap().key_count, 0);
            }
        }
    };
}

kvdb_parity_for_wg!(wg8, 8);
kvdb_parity_for_wg!(wg1, 1);
