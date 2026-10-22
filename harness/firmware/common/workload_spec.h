/*
 * FlashDB firmware measurement harness -- workload specification.
 *
 * The SAME workload shape must run in the C firmware (real FlashDB) and in the
 * Rust firmware (placeholder until the flashdb crate is wired in, then real
 * crate).  The values below are mirrored by constants in
 * harness/firmware/rust/main.rs -- keep them in sync.
 *
 * Deterministic by construction: every value is a pure function of the loop
 * index, there is no real clock and no RNG, so checksums are stable across
 * runs and can be compared between the C and Rust builds once the Rust side
 * runs the real library.
 */
#ifndef HARNESS_WORKLOAD_SPEC_H
#define HARNESS_WORKLOAD_SPEC_H

/* ---- KVDB phase ------------------------------------------------------- */
/* Number of distinct keys initially written with fdb_kv_set(). */
#define HW_KV_KEYS             64
/* Of those, how many get a second (updated) value. */
#define HW_KV_UPDATES          32
/* Rewrites used to force sector GC churn; cycles over HW_KV_CHURN_KEYS keys. */
#define HW_KV_CHURN            1500
#define HW_KV_CHURN_KEYS       16
/* String value length (excluding NUL) of every KV value. */
#define HW_KV_VALUE_LEN        24

/* ---- TSDB phase ------------------------------------------------------- */
/* Number of time-series log entries appended. */
#define HW_TS_APPENDS          200
/* Payload bytes appended per log entry. */
#define HW_TS_PAYLOAD_LEN      16
/* Fake clock origin; harness_get_time() returns base + append index. */
#define HW_TS_BASE_TIME        1000
/* Real-board rollover phase (FDB_BOARD_U545 only): appends past the ring
 * capacity of the 16 KiB / 4-sector TS partition (~448 x 16-byte entries), so
 * the ring wraps several times before the live window and recovery are
 * verified.  Mirrored by TS_ROLL_APPENDS in rust/main.rs. */
#define HW_TS_ROLL_APPENDS     1200

/* ---- Database geometry ------------------------------------------------ */
/* FAL flash device: 32 KiB simulated NOR, 4 KiB erase blocks.
 * This matches the Rust firmware: one 16 KiB KV partition + one 16 KiB TS
 * partition (two RamFlash arrays in .bss on the Rust side). */
#define HW_SIM_FLASH_LEN       (32u * 1024u)
#define HW_SIM_FLASH_BLK_SIZE  (4u * 1024u)
/* KVDB partition: 16 KiB = 4 sectors of 4 KiB. */
#define HW_KVDB_PART_OFFSET    (0u)
#define HW_KVDB_PART_LEN       (16u * 1024u)
/* TSDB partition: 16 KiB = 4 sectors of 4 KiB. */
#define HW_TSDB_PART_OFFSET    (16u * 1024u)
#define HW_TSDB_PART_LEN       (16u * 1024u)
/* TSDB max_len argument: upper bound for one log blob. */
#define HW_TSDB_MAX_LEN        64u

#endif /* HARNESS_WORKLOAD_SPEC_H */
