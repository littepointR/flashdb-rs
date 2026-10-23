//! Coverage for newly added features (`Config::not_formattable`,
//! `Kvdb::auto_update`, `Kvdb::read_value_at`, `Tsdb::read_blob_at`,
//! `Partition`, `sec_size`/`max_size`) and deterministic pseudo-random
//! state-machine model checks against in-memory reference models.

use core::mem::MaybeUninit;
use flashdb::{
    Config, Error, KvEntry, KvStats, Kvdb, KvdbImpl, Partition, Tsdb, Tsdb64, TslStatus,
    VER_NUM_KV_NAME,
};
use flashdb_sim::RamFlash;
use std::collections::BTreeMap;

const SEC: u32 = 4096;
const MAX: u32 = 16384;

// ---------------------------------------------------------------------------
// 1. Not-formattable mount (KVDB + TSDB)
// ---------------------------------------------------------------------------

#[test]
fn kv_not_formattable_rejects_blank_and_damaged_flash_without_writing() {
    let cfg = Config::new(SEC, MAX).not_formattable(true);

    // 1. Blank (all-0xFF) flash: rejected with NotFormatted, 0 writes/erases.
    let flash = RamFlash::<16384, 8>::new(SEC);
    let err = match Kvdb::mount(flash, cfg) {
        Ok(_) => panic!("expected NotFormatted on blank flash"),
        Err(e) => e,
    };
    assert_eq!(err, Error::NotFormatted);

    // Placement variant (`mount_into`) also rejects without writing or erasing.
    let mut flash = RamFlash::<16384, 8>::new(SEC);
    let mut slot = MaybeUninit::<Kvdb<&mut RamFlash<16384, 8>>>::uninit();
    assert_eq!(
        Kvdb::mount_into(&mut slot, &mut flash, cfg),
        Err(Error::NotFormatted)
    );
    assert_eq!(flash.stats().write_ops, 0);
    assert_eq!(flash.stats().erase_ops, 0);

    // 2. Format normally, populate keys, then remount with `not_formattable(true)`.
    let mut db = Kvdb::mount(flash, Config::new(SEC, MAX)).unwrap();
    assert_eq!(db.sec_size(), SEC);
    assert_eq!(db.max_size(), MAX);
    db.set(b"cal_gain", b"1024").unwrap();
    db.set(b"cal_offset", b"-7").unwrap();
    let mut flash = db.into_flash();

    let mut db = Kvdb::mount(&mut flash, cfg).unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(db.get(b"cal_gain", &mut buf).unwrap(), Some(4));
    assert_eq!(&buf[..4], b"1024");
    drop(db);

    // 3. Damage one sector header (sector 2): `not_formattable(true)` refuses to
    //    auto-format the damaged sector and leaves the flash image untouched.
    flash.data_mut()[(2 * SEC + 8) as usize] = 0x00;
    let snapshot = flash.data().to_vec();
    flash.reset_stats();
    let res = Kvdb::mount(&mut flash, cfg);
    assert_eq!(res.err(), Some(Error::NotFormatted));
    assert_eq!(flash.stats().write_ops, 0);
    assert_eq!(flash.stats().erase_ops, 0);
    assert_eq!(flash.data(), &snapshot[..]);
}

#[test]
fn ts_not_formattable_rejects_blank_and_damaged_flash_without_writing() {
    let cfg = Config::new(SEC, MAX).not_formattable(true);

    // 1. Blank flash: rejected with NotFormatted, 0 writes/erases.
    let mut flash = RamFlash::<16384, 8>::new(SEC);
    assert_eq!(
        Tsdb::mount(&mut flash, cfg, 64).err(),
        Some(Error::NotFormatted)
    );
    assert_eq!(flash.stats().write_ops, 0);
    assert_eq!(flash.stats().erase_ops, 0);

    let mut slot = MaybeUninit::<Tsdb<&mut RamFlash<16384, 8>>>::uninit();
    assert_eq!(
        Tsdb::mount_into(&mut slot, &mut flash, cfg, 64),
        Err(Error::NotFormatted)
    );
    assert_eq!(flash.stats().write_ops, 0);
    assert_eq!(flash.stats().erase_ops, 0);

    // 2. Format normally, append entries, remount with `not_formattable(true)`.
    let mut db = Tsdb::mount(&mut flash, Config::new(SEC, MAX), 64).unwrap();
    assert_eq!(db.sec_size(), SEC);
    assert_eq!(db.max_size(), MAX);
    db.append(100, b"sample-1").unwrap();
    db.append(200, b"sample-2").unwrap();
    drop(db);

    let mut db = Tsdb::mount(&mut flash, cfg, 64).unwrap();
    assert_eq!(db.last_time(), 200);
    assert_eq!(db.iter().count(), 2);
    drop(db);

    // 3. Damage sector 1 magic word: `not_formattable(true)` rejects without formatting.
    flash.data_mut()[(SEC + 3) as usize] = 0x00;
    let snapshot = flash.data().to_vec();
    flash.reset_stats();
    assert_eq!(
        Tsdb::mount(&mut flash, cfg, 64).err(),
        Some(Error::NotFormatted)
    );
    assert_eq!(flash.stats().write_ops, 0);
    assert_eq!(flash.stats().erase_ops, 0);
    assert_eq!(flash.data(), &snapshot[..]);
}

// ---------------------------------------------------------------------------
// 2. KV auto_update (FlashDB FDB_KV_AUTO_UPDATE / __ver_num__)
// ---------------------------------------------------------------------------

#[test]
fn kv_auto_update_lifecycle_and_64bit_version_compat() {
    let v1_defaults: &[(&str, &[u8])] = &[("baud", b"115200"), ("mode", b"normal")];
    let v2_defaults: &[(&str, &[u8])] = &[
        ("baud", b"9600"),    // changed in v2 defaults, but user value must win
        ("mode", b"normal"),  // unchanged
        ("timeout", b"5000"), // new in v2
    ];

    let mut flash = RamFlash::<16384, 8>::new(SEC);

    // First boot: mount empty and apply v1 via auto_update.
    let mut db = Kvdb::mount(&mut flash, Config::new(SEC, MAX)).unwrap();
    assert!(db.auto_update(1, v1_defaults).unwrap());

    let mut buf = [0u8; 32];
    assert_eq!(db.get(VER_NUM_KV_NAME, &mut buf).unwrap(), Some(4));
    assert_eq!(u32::from_le_bytes(buf[..4].try_into().unwrap()), 1);
    assert_eq!(db.get_str(b"baud", &mut buf).unwrap(), Some("115200"));
    assert_eq!(db.get_str(b"mode", &mut buf).unwrap(), Some("normal"));

    // User customises "baud".
    db.set_str("baud", "921600").unwrap();
    drop(db);

    // Second boot with the same version (1): no-op, 0 writes.
    flash.reset_stats();
    let mut db = Kvdb::mount(&mut flash, Config::new(SEC, MAX)).unwrap();
    assert!(!db.auto_update(1, v1_defaults).unwrap());
    assert_eq!(db.get_str(b"baud", &mut buf).unwrap(), Some("921600"));
    drop(db);
    assert_eq!(flash.stats().write_ops, 0);

    let mut db = Kvdb::mount(&mut flash, Config::new(SEC, MAX)).unwrap();

    // Upgrade to v2: inserts missing "timeout", preserves existing "baud".
    assert!(db.auto_update(2, v2_defaults).unwrap());
    assert_eq!(db.get_str(b"baud", &mut buf).unwrap(), Some("921600"));
    assert_eq!(db.get_str(b"mode", &mut buf).unwrap(), Some("normal"));
    assert_eq!(db.get_str(b"timeout", &mut buf).unwrap(), Some("5000"));
    assert_eq!(db.get(VER_NUM_KV_NAME, &mut buf).unwrap(), Some(4));
    assert_eq!(u32::from_le_bytes(buf[..4].try_into().unwrap()), 2);

    // Simulate a 64-bit C host `size_t` version record (8 bytes LE) and a weird-sized
    // record to exercise both branches of `auto_update`.
    db.set(VER_NUM_KV_NAME, &2u64.to_le_bytes()).unwrap();
    assert!(!db.auto_update(2, v2_defaults).unwrap());
    assert!(db.auto_update(3, v2_defaults).unwrap());

    db.set(VER_NUM_KV_NAME, b"bad").unwrap();
    assert!(db.auto_update(3, v2_defaults).unwrap());
}

// ---------------------------------------------------------------------------
// 3. Segmented value / blob reads (read_value_at, read_blob, read_blob_at)
// ---------------------------------------------------------------------------

#[test]
fn kv_and_ts_segmented_reads_verify_crc_and_bounds() {
    let mut flash = RamFlash::<16384, 128>::new(SEC);
    let mut db = Kvdb::mount(&mut flash, Config::new(SEC, MAX)).unwrap();

    // 300-byte patterned payload (requires 16-byte granule padding at WG=128).
    let mut payload = [0u8; 300];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(37).wrapping_add(11);
    }
    db.set(b"blob", &payload).unwrap();

    let entry = db.find(b"blob").unwrap().unwrap();
    assert_eq!(entry.value_len, 300);

    // Reassemble the 300-byte value in 29-byte segments via `Kvdb::read_value_at`.
    let mut assembled = Vec::new();
    let mut offset = 0u32;
    let mut chunk = [0u8; 29];
    loop {
        let n = db.read_value_at(&entry, offset, &mut chunk).unwrap();
        if n == 0 {
            break;
        }
        assembled.extend_from_slice(&chunk[..n]);
        offset += n as u32;
    }
    assert_eq!(assembled, payload);

    // Past-end offset returns Ok(0) while still checking the record CRC.
    assert_eq!(db.read_value_at(&entry, 300, &mut chunk).unwrap(), 0);
    assert_eq!(db.read_value_at(&entry, 9999, &mut chunk).unwrap(), 0);
    assert_eq!(db.read_value_at(&entry, 10, &mut []).unwrap(), 0);

    // Also test via `KvIterator::read_value_at`.
    let mut iter = db.iter();
    let it_entry = iter.next().unwrap().unwrap();
    let mut slice = [0u8; 50];
    let n = iter.read_value_at(&it_entry, 125, &mut slice).unwrap();
    assert_eq!(n, 50);
    assert_eq!(&slice[..50], &payload[125..175]);
    drop(iter);
    drop(db);

    // Corrupt the last byte of the value on flash: even reading the first 10 bytes
    // at offset 5 must fail CRC verification with `Error::ReadCorrupted`.
    let val_end = (entry.value_addr + entry.value_len - 1) as usize;
    flash.data_mut()[val_end] = 0x00;
    let mut db = Kvdb::mount(&mut flash, Config::new(SEC, MAX)).unwrap();
    assert_eq!(
        db.read_value_at(&entry, 5, &mut chunk),
        Err(Error::ReadCorrupted)
    );
    drop(db);

    // TSDB `Tsdb::read_blob`, `Tsdb::read_blob_at`, `TslIterator::read_blob_at`.
    let mut ts_flash = RamFlash::<16384, 64>::new(SEC);
    let mut ts = Tsdb::mount(&mut ts_flash, Config::new(SEC, MAX), 256).unwrap();
    ts.append(10, &payload[..150]).unwrap();

    let mut it = ts.iter();
    let tsl = it.next().unwrap().unwrap();
    let mut seg = [0u8; 40];
    assert_eq!(it.read_blob_at(&tsl, 100, &mut seg).unwrap(), 40);
    assert_eq!(&seg[..40], &payload[100..140]);
    assert_eq!(it.read_blob_at(&tsl, 140, &mut seg).unwrap(), 10);
    assert_eq!(&seg[..10], &payload[140..150]);
    assert_eq!(it.read_blob_at(&tsl, 150, &mut seg).unwrap(), 0);
    drop(it);

    // Read directly from `Tsdb` after the iterator is dropped.
    let mut full = [0u8; 160];
    assert_eq!(ts.read_blob(&tsl, &mut full).unwrap(), 150);
    assert_eq!(&full[..150], &payload[..150]);
    assert_eq!(ts.read_blob_at(&tsl, 50, &mut seg).unwrap(), 40);
    assert_eq!(&seg[..40], &payload[50..90]);
    assert_eq!(ts.read_blob_at(&tsl, 50, &mut []).unwrap(), 0);
}

// ---------------------------------------------------------------------------
// 4. Partition<F> multi-database sharing on a single flash chip
// ---------------------------------------------------------------------------

#[test]
fn partition_adapter_hosts_kvdb_and_tsdb_on_one_chip() {
    let mut chip = RamFlash::<32768, 8>::new(SEC);

    {
        let mut kv_part = Partition::new(&mut chip, 0, 16384);
        assert_eq!(kv_part.offset(), 0);
        assert_eq!(kv_part.inner().erase_size(), SEC);
        assert_eq!(kv_part.inner_mut().erase_size(), SEC);
        let mut kv = Kvdb::mount(&mut kv_part, Config::new(SEC, 16384)).unwrap();
        kv.set(b"device", b"stm32u5").unwrap();
        let _inner = kv_part.into_inner();
    }
    {
        let ts_part = Partition::new(&mut chip, 16384, 16384);
        assert_eq!(ts_part.offset(), 16384);
        let mut ts = Tsdb::mount(ts_part, Config::new(SEC, 16384), 64).unwrap();
        for t in 1..=50 {
            ts.append(t, &[t as u8; 16]).unwrap();
        }
    }

    // Remount both partitions and verify neither clobbered the other.
    {
        let kv_part = Partition::new(&mut chip, 0, 16384);
        let mut kv = Kvdb::mount(kv_part, Config::new(SEC, 16384)).unwrap();
        kv.check().unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(kv.get_str(b"device", &mut buf).unwrap(), Some("stm32u5"));
    }
    {
        let ts_part = Partition::new(&mut chip, 16384, 16384);
        let mut ts = Tsdb::mount(ts_part, Config::new(SEC, 16384), 64).unwrap();
        assert_eq!(ts.last_time(), 50);
        assert_eq!(ts.query_count(1, 50, TslStatus::Write).unwrap(), 50);
    }
}

// ---------------------------------------------------------------------------
// 5. Randomised state-machine model check: KVDB vs BTreeMap
// ---------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        (self.0 >> 32) as u32
    }
    fn range(&mut self, upper: u32) -> u32 {
        self.next_u32() % upper
    }
}

fn run_kv_model<const WG: u32, const CACHE: usize>(seed: u64, steps: usize) {
    const P_SEC: u32 = 2048;
    const P_MAX: u32 = 8192;
    let cfg = Config::new(P_SEC, P_MAX);
    let mut flash = RamFlash::<8192, WG>::new(P_SEC);
    flash.set_strict_write(WG <= 128);
    let mut db = KvdbImpl::<_, CACHE>::mount(flash, cfg).unwrap();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut rng = Rng(seed);

    for step in 0..steps {
        let op = rng.range(100);
        if op < 45 {
            // set (create or update) among 12 keys with lengths 1..=20 and values 0..=96 B
            let k_idx = rng.range(12);
            let k_len = 1 + (k_idx as usize % 20);
            let mut key = vec![b'a' + (k_idx as u8 % 26); k_len];
            key[0] = b'0' + k_idx as u8;
            let v_len = rng.range(97) as usize;
            let mut val = vec![0u8; v_len];
            for b in &mut val {
                *b = rng.next_u32() as u8;
            }
            match db.set(&key, &val) {
                Ok(()) => {
                    model.insert(key, val);
                }
                Err(Error::SavedFull) => {
                    // Partition legitimately full: verify state is still intact.
                }
                Err(e) => panic!("WG={WG} CACHE={CACHE} step={step}: set failed: {e:?}"),
            }
        } else if op < 65 {
            // remove
            let k_idx = rng.range(14);
            let k_len = 1 + (k_idx as usize % 20);
            let mut key = vec![b'a' + (k_idx as u8 % 26); k_len];
            key[0] = b'0' + k_idx as u8;
            let existed = db.remove(&key).unwrap();
            let model_existed = model.remove(&key).is_some();
            assert_eq!(existed, model_existed, "WG={WG} CACHE={CACHE} step={step}");
        } else if op < 85 {
            // get + segmented read
            let k_idx = rng.range(14);
            let k_len = 1 + (k_idx as usize % 20);
            let mut key = vec![b'a' + (k_idx as u8 % 26); k_len];
            key[0] = b'0' + k_idx as u8;
            let mut buf = [0u8; 128];
            let res = db.get(&key, &mut buf).unwrap();
            match model.get(&key) {
                None => assert_eq!(res, None),
                Some(expected) => {
                    assert_eq!(res, Some(expected.len()));
                    assert_eq!(&buf[..expected.len()], &expected[..]);
                    let entry: KvEntry = db.find(&key).unwrap().unwrap();
                    if !expected.is_empty() {
                        let off = rng.range(expected.len() as u32);
                        let mut sub = [0u8; 19];
                        let n = db.read_value_at(&entry, off, &mut sub).unwrap();
                        let exp_n = core::cmp::min(19, expected.len() - off as usize);
                        assert_eq!(n, exp_n);
                        assert_eq!(&sub[..n], &expected[off as usize..off as usize + n]);
                    }
                }
            }
        } else if op < 95 {
            // remount and verify `check()` + `stats()` + `iter()`
            let flash = db.into_flash();
            db = KvdbImpl::<_, CACHE>::mount(flash, cfg).unwrap();
            db.check().unwrap();
            let stats: KvStats = db.stats().unwrap();
            assert_eq!(stats.key_count as usize, model.len());
            let exp_val_bytes: usize = model.values().map(|v| v.len()).sum();
            assert_eq!(stats.value_bytes as usize, exp_val_bytes);

            let mut seen = BTreeMap::new();
            let mut it = db.iter();
            while let Some(e) = it.next() {
                let e = e.unwrap();
                let mut v = vec![0u8; e.value_len as usize];
                let n = it.read_value(&e, &mut v).unwrap();
                assert_eq!(n, v.len());
                seen.insert(e.name().to_vec(), v);
            }
            assert_eq!(seen, model);
        } else {
            // Faulted update: inject a torn write at a random write op, remount,
            // verify `check()`, and resync model for that single key.
            let k_idx = rng.range(12);
            let k_len = 1 + (k_idx as usize % 20);
            let mut key = vec![b'a' + (k_idx as u8 % 26); k_len];
            key[0] = b'0' + k_idx as u8;
            let new_val = vec![0xA5u8; (rng.range(48) + 1) as usize];
            let old_val = model.get(&key).cloned();

            let mut flash = db.into_flash();
            let fail_after = (rng.range(8) + 1) as u64;
            let tear = (rng.range(16) + 1) as usize;
            flash.fail_torn_after_writes(fail_after, tear);
            let mut tmp_db = KvdbImpl::<_, CACHE>::mount(flash, cfg).unwrap();
            let _ = tmp_db.set(&key, &new_val);
            let mut flash = tmp_db.into_flash();
            flash.disarm_fault();

            db = KvdbImpl::<_, CACHE>::mount(flash, cfg).unwrap();
            db.check().unwrap();
            let mut buf = [0u8; 128];
            match db.get(&key, &mut buf).unwrap() {
                None => {
                    assert!(old_val.is_none());
                    model.remove(&key);
                }
                Some(n) => {
                    let cur = buf[..n].to_vec();
                    assert!(
                        Some(&cur) == old_val.as_ref() || cur == new_val,
                        "WG={WG} CACHE={CACHE} step={step}: key state after torn write must be old or new"
                    );
                    model.insert(key, cur);
                }
            }
        }
    }
    db.check().unwrap();
}

#[test]
fn kv_randomised_model_across_granularities_and_cache_sizes() {
    run_kv_model::<1, 64>(0x1001, 250);
    run_kv_model::<8, 0>(0x1002, 250); // KvdbNoCache (0-way)
    run_kv_model::<8, 1>(0x1003, 250); // 1-slot direct-mapped cache
    run_kv_model::<8, 16>(0x1004, 250); // KvdbSmallCache
    run_kv_model::<32, 64>(0x1005, 250);
    run_kv_model::<64, 128>(0x1006, 250); // >32 cache sets fallback path
    run_kv_model::<128, 64>(0x1007, 250);
    run_kv_model::<256, 16>(0x1008, 250);
}

// ---------------------------------------------------------------------------
// 6. Randomised state-machine model check: TSDB across granularities
// ---------------------------------------------------------------------------

fn run_ts_model<const WG: u32>(seed: u64, steps: usize) {
    const T_SEC: u32 = 1024;
    const T_MAX: u32 = 4096;
    const MAX_LEN: u32 = 64;
    let cfg = Config::new(T_SEC, T_MAX);
    let mut flash = RamFlash::<4096, WG>::new(T_SEC);
    flash.set_strict_write(true);
    let mut db = Tsdb64::mount(flash, cfg, MAX_LEN).unwrap();
    let mut rng = Rng(seed);
    let mut history: Vec<(i64, TslStatus, Vec<u8>)> = Vec::new();
    let mut cur_time = 0i64;

    for step in 0..steps {
        let op = rng.range(100);
        if op < 55 {
            // append with monotonic timestamp
            cur_time += (rng.range(10) + 1) as i64;
            let len = rng.range(MAX_LEN + 1) as usize;
            let mut data = vec![0u8; len];
            for b in &mut data {
                *b = rng.next_u32() as u8;
            }
            db.append(cur_time, &data).unwrap();
            history.push((cur_time, TslStatus::Write, data));
        } else if op < 70 && !history.is_empty() {
            // transition a live entry's status (Write -> UserStatus1 -> Deleted)
            let entries: Vec<_> = db.iter().map(|e| e.unwrap()).collect();
            if !entries.is_empty() {
                let pick = &entries[rng.range(entries.len() as u32) as usize];
                let next_status = match pick.status {
                    TslStatus::Write => Some(TslStatus::UserStatus1),
                    TslStatus::UserStatus1 => Some(TslStatus::Deleted),
                    _ => None,
                };
                if let Some(ns) = next_status {
                    db.set_status(pick, ns).unwrap();
                    if let Some(h) = history.iter_mut().find(|h| h.0 == pick.time) {
                        h.1 = ns;
                    }
                }
            }
        } else if op < 90 {
            // verify forward, reverse, and by-time iteration against the suffix of `history`
            let forward: Vec<_> = db.iter().map(|e| e.unwrap()).collect();
            let mut reverse: Vec<_> = db.iter_reverse().map(|e| e.unwrap()).collect();
            reverse.reverse();
            assert_eq!(forward, reverse, "WG={WG} step={step}: reverse != forward");

            assert!(forward.len() <= history.len());
            let live_tail = &history[history.len() - forward.len()..];
            for (entry, (exp_t, exp_s, exp_data)) in forward.iter().zip(live_tail.iter()) {
                assert_eq!(entry.time, *exp_t);
                assert_eq!(entry.status, *exp_s);
                let mut buf = vec![0u8; exp_data.len() + 4];
                let n = db.read_blob(entry, &mut buf).unwrap();
                assert_eq!(n, exp_data.len());
                assert_eq!(&buf[..n], &exp_data[..]);
            }

            if !forward.is_empty() {
                let t_first = forward.first().unwrap().time;
                let t_last = forward.last().unwrap().time;
                let t_a = t_first + (rng.range((t_last - t_first + 1) as u32) as i64);
                let t_b = t_first + (rng.range((t_last - t_first + 1) as u32) as i64);
                let (lo, hi) = if t_a <= t_b { (t_a, t_b) } else { (t_b, t_a) };

                let expected_range: Vec<i64> = live_tail
                    .iter()
                    .filter(|(t, _, _)| *t >= lo && *t <= hi)
                    .map(|(t, _, _)| *t)
                    .collect();
                let fwd_range: Vec<i64> =
                    db.iter_by_time(lo, hi).map(|e| e.unwrap().time).collect();
                assert_eq!(
                    fwd_range, expected_range,
                    "WG={WG} step={step}: iter_by_time fwd"
                );

                let mut rev_range: Vec<i64> =
                    db.iter_by_time(hi, lo).map(|e| e.unwrap().time).collect();
                rev_range.reverse();
                assert_eq!(
                    rev_range, expected_range,
                    "WG={WG} step={step}: iter_by_time rev"
                );
            }
        } else {
            // remount and verify `last_time`
            let flash = db.into_flash();
            db = Tsdb64::mount(flash, cfg, MAX_LEN).unwrap();
            assert_eq!(db.last_time(), cur_time);
        }
    }
}

#[test]
fn ts_randomised_model_across_all_granularities() {
    run_ts_model::<1>(0x2001, 200);
    run_ts_model::<8>(0x2002, 200);
    run_ts_model::<32>(0x2003, 200);
    run_ts_model::<64>(0x2004, 200);
    run_ts_model::<128>(0x2005, 200);
    run_ts_model::<256>(0x2006, 200);
}
