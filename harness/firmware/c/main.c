/*
 * FlashDB firmware measurement harness -- C firmware workload (baseline).
 *
 * Boots on qemu-system-arm -M mps2-an385, initialises the real C FlashDB
 * (reference/FlashDB, unmodified, FAL mode, FDB_WRITE_GRAN=8) on top of a
 * simulated 64 KiB NOR part, runs the deterministic workload described in
 * common/workload_spec.h and prints ONE semihosting RESULT line with the
 * section sizes (from linker symbols), the stack high-water mark (from the
 * 0xA5 paint) and workload checksums.
 *
 * Workload:
 *   KVDB : 64 x fdb_kv_set, 32 x update, 1500 x set churn over 16 keys (forces
 *          several GC cycles), 64 x get + full value verification, full
 *          iteration + checksum.
 *   TSDB : 200 x fdb_tsl_append_with_ts (16-byte payload, deterministic fake
 *          clock), full iteration + checksum.
 *
 * Everything is deterministic: no RNG, no real clock, no host input.  Two
 * consecutive runs must print identical checksums (scripts/fw_c.sh can be run
 * twice to confirm).
 */
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include <flashdb.h>
#include <fal.h>

#include "semihost.h"
#include "stack.h"
#include "workload_spec.h"

#ifdef FDB_BOARD_U545
#include "board_u545.h"
#include "sim_stats.h"
#endif

/* Both database objects are static: they live in .bss, which is exactly the
 * static RAM the measurement wants to capture. */
static struct fdb_kvdb g_kvdb;
static struct fdb_tsdb g_tsdb;

static uint32_t g_fake_time;

/* ------------------------------------------------------------------------- */
/* Deterministic data generation                                             */
/* ------------------------------------------------------------------------- */

static const char k_hex[] = "0123456789abcdef";

static void make_kv_key(char out[8], uint32_t idx)
{
    out[0] = 'k';
    out[1] = 'v';
    out[2] = '.';
    out[3] = k_hex[(idx >> 4) & 0xfu];
    out[4] = k_hex[idx & 0xfu];
    out[5] = '\0';
}

static void make_kv_value(char out[HW_KV_VALUE_LEN + 1], uint32_t idx, uint32_t gen)
{
    uint32_t k;

    out[0] = 'v';
    out[1] = k_hex[(gen >> 12) & 0xfu];
    out[2] = k_hex[(gen >> 8) & 0xfu];
    out[3] = k_hex[(gen >> 4) & 0xfu];
    out[4] = k_hex[gen & 0xfu];
    out[5] = '-';
    out[6] = k_hex[(idx >> 4) & 0xfu];
    out[7] = k_hex[idx & 0xfu];
    out[8] = '-';
    for (k = 9; k < HW_KV_VALUE_LEN; k++) {
        out[k] = k_hex[(idx * 7u + gen * 13u + k) & 0xfu];
    }
    out[HW_KV_VALUE_LEN] = '\0';
}

/* Little-endian encoding, mirrored byte-for-byte by the Rust firmware. */
static void make_ts_payload(uint8_t out[HW_TS_PAYLOAD_LEN], uint32_t idx)
{
    uint32_t tick = HW_TS_BASE_TIME + idx;
    uint32_t k;

    out[0] = (uint8_t)(idx);
    out[1] = (uint8_t)(idx >> 8);
    out[2] = (uint8_t)(idx >> 16);
    out[3] = (uint8_t)(idx >> 24);
    out[4] = (uint8_t)(tick);
    out[5] = (uint8_t)(tick >> 8);
    out[6] = (uint8_t)(tick >> 16);
    out[7] = (uint8_t)(tick >> 24);
    for (k = 8; k < HW_TS_PAYLOAD_LEN; k++) {
        out[k] = (uint8_t)(idx * 31u + k * 7u);
    }
}

static fdb_time_t harness_get_time(void)
{
    return (fdb_time_t)(HW_TS_BASE_TIME + g_fake_time++);
}

/* FNV-1a, 32 bit -- small, deterministic, no division. */
static uint32_t fnv1a(uint32_t h, const void *data, size_t len)
{
    const uint8_t *p = (const uint8_t *)data;

    while (len--) {
        h ^= *p++;
        h *= 16777619u;
    }
    return h;
}

/* ------------------------------------------------------------------------- */
/* Database initialisation                                                   */
/* ------------------------------------------------------------------------- */

static void databases_init(void)
{
    fdb_err_t err;

    if (fal_init() < 0) {
        harness_fail("fal_init", -1);
    }

    err = fdb_kvdb_init(&g_kvdb, "kvdb", "fdb_kvdb1", NULL, NULL);
    if (err != FDB_NO_ERR) {
        harness_fail("kvdb_init", (int)err);
    }

    err = fdb_tsdb_init(&g_tsdb, "tsdb", "fdb_tsdb1", harness_get_time,
                        HW_TSDB_MAX_LEN, NULL);
    if (err != FDB_NO_ERR) {
        harness_fail("tsdb_init", (int)err);
    }

#ifdef FDB_BOARD_U545
    /* Explicit, matching the Rust firmware's ts_mount(): the main workload does
     * not roll over, and both board harnesses must configure the DB alike.  The
     * QEMU build keeps its historical default (rollover on, never exercised by
     * the 200-append workload) so its documented RESULT stays unchanged. */
    {
        bool ro = false;
        fdb_tsdb_control(&g_tsdb, FDB_TSDB_CTRL_SET_ROLLOVER, &ro);
    }
#endif
}

/* ------------------------------------------------------------------------- */
/* KVDB phase                                                                */
/* ------------------------------------------------------------------------- */

static void kvdb_phase(struct harness_metrics *m)
{
    char key[8];
    char value[HW_KV_VALUE_LEN + 1];
    char read_buf[HW_KV_VALUE_LEN + 1];
    uint8_t iter_buf[HW_KV_VALUE_LEN + 1];
    struct fdb_blob blob;
    struct fdb_kv_iterator itr;
    fdb_err_t err;
    uint32_t i;
    uint32_t n;

    /* 1. initial set of all keys */
    for (i = 0; i < HW_KV_KEYS; i++) {
        make_kv_key(key, i);
        make_kv_value(value, i, 0);
        err = fdb_kv_set(&g_kvdb, key, value);
        if (err != FDB_NO_ERR) {
            harness_fail("kv_set", (int)err);
        }
        m->kv_writes++;
    }

    /* 2. update half of them */
    for (i = 0; i < HW_KV_UPDATES; i++) {
        make_kv_key(key, i);
        make_kv_value(value, i, 1);
        err = fdb_kv_set(&g_kvdb, key, value);
        if (err != FDB_NO_ERR) {
            harness_fail("kv_update", (int)err);
        }
        m->kv_writes++;
    }

    /* 3. GC churn: rewrite a small key set many times so the KVDB has to
     *    erase and migrate sectors.  This is the same access pattern a real
     *    "change a few settings repeatedly" firmware has. */
    for (n = 0; n < HW_KV_CHURN; n++) {
        uint32_t idx = n % HW_KV_CHURN_KEYS;
        make_kv_key(key, idx);
        make_kv_value(value, idx, 2u + n);
        err = fdb_kv_set(&g_kvdb, key, value);
        if (err != FDB_NO_ERR) {
            harness_fail("kv_churn", (int)err);
        }
        m->kv_writes++;
    }

    /* 4. get every key and verify its final content */
    for (i = 0; i < HW_KV_KEYS; i++) {
        size_t got;
        uint32_t expect_gen;

        if (i < HW_KV_CHURN_KEYS) {
            /* Last churn iteration that touched key i: n = i + 16 * k, with n
             * the largest value < HW_KV_CHURN.  Do not assume HW_KV_CHURN is a
             * multiple of HW_KV_CHURN_KEYS. */
            uint32_t last_n = (HW_KV_CHURN - 1u) -
                              ((HW_KV_CHURN - 1u - i) % HW_KV_CHURN_KEYS);
            expect_gen = 2u + last_n;
        } else if (i < HW_KV_UPDATES) {
            expect_gen = 1u;
        } else {
            expect_gen = 0u;
        }
        make_kv_key(key, i);
        make_kv_value(value, i, expect_gen);

        got = fdb_kv_get_blob(&g_kvdb, key,
                              fdb_blob_make(&blob, read_buf, HW_KV_VALUE_LEN));
        if (got != HW_KV_VALUE_LEN) {
            harness_fail("kv_get_len", (int)got);
        }
        if (memcmp(read_buf, value, HW_KV_VALUE_LEN) != 0) {
            harness_fail("kv_get_data", (int)i);
        }
        m->kv_gets++;
        m->kv_checksum = fnv1a(m->kv_checksum, read_buf, got);
    }

    /* 5. iterate the whole database: walks sector metadata and KV headers, a
     *    different code path from the point lookup above. */
    memset(&itr, 0, sizeof(itr));
    fdb_kv_iterator_init(&g_kvdb, &itr);
    while (fdb_kv_iterate(&g_kvdb, &itr)) {
        size_t got = fdb_blob_read(
            (fdb_db_t)&g_kvdb,
            fdb_kv_to_blob(&itr.curr_kv,
                           fdb_blob_make(&blob, iter_buf, sizeof(iter_buf))));

        if (got > sizeof(iter_buf)) {
            harness_fail("kv_iter_len", (int)got);
        }
        m->kv_iterated++;
        m->kv_iter_checksum = fnv1a(m->kv_iter_checksum, itr.curr_kv.name,
                                    itr.curr_kv.name_len);
        m->kv_iter_checksum = fnv1a(m->kv_iter_checksum, iter_buf, got);
    }

    if (m->kv_iterated != HW_KV_KEYS) {
        harness_fail("kv_iter_count", (int)m->kv_iterated);
    }
}

/* ------------------------------------------------------------------------- */
/* TSDB phase                                                                */
/* ------------------------------------------------------------------------- */

struct ts_iter_ctx {
    fdb_tsdb_t db;
    uint32_t count;
    uint32_t checksum;
    uint32_t bad;
};

static bool ts_iter_cb(fdb_tsl_t tsl, void *arg)
{
    struct ts_iter_ctx *ctx = (struct ts_iter_ctx *)arg;
    uint8_t buf[HW_TS_PAYLOAD_LEN] = {0};
    struct fdb_blob blob;
    size_t got;

    got = fdb_blob_read(
        (fdb_db_t)ctx->db,
        fdb_tsl_to_blob(tsl, fdb_blob_make(&blob, buf, sizeof(buf))));
    if (got != HW_TS_PAYLOAD_LEN) {
        ctx->bad++;
        return true;
    }
    ctx->count++;
    ctx->checksum = fnv1a(ctx->checksum, buf, got);
    return false; /* continue iterating */
}

static void tsdb_phase(struct harness_metrics *m)
{
    uint8_t payload[HW_TS_PAYLOAD_LEN];
    struct fdb_blob blob;
    struct ts_iter_ctx ctx;
    fdb_err_t err;
    uint32_t i;

    /* 200 deterministic append operations */
    for (i = 0; i < HW_TS_APPENDS; i++) {
        make_ts_payload(payload, i);
        fdb_blob_make(&blob, payload, sizeof(payload));
        err = fdb_tsl_append_with_ts(&g_tsdb, &blob,
                                     (fdb_time_t)(HW_TS_BASE_TIME + i));
        if (err != FDB_NO_ERR) {
            harness_fail("ts_append", (int)err);
        }
        m->ts_appends++;
    }

    /* iterate every log entry and checksum the payloads */
    ctx.db = &g_tsdb;
    ctx.count = 0;
    ctx.checksum = 0;
    ctx.bad = 0;
    fdb_tsl_iter(&g_tsdb, ts_iter_cb, &ctx);

    if (ctx.bad != 0) {
        harness_fail("ts_iter_read", (int)ctx.bad);
    }
    if (ctx.count != HW_TS_APPENDS) {
        harness_fail("ts_iter_count", (int)ctx.count);
    }
    m->ts_iterated = ctx.count;
    m->ts_checksum = ctx.checksum;
}

/* ------------------------------------------------------------------------- */
/* Per-phase stack attribution (diagnostic builds only)                       */
/* ------------------------------------------------------------------------- */

#ifdef HARNESS_STACK_PHASES
/*
 * Runs AFTER the headline scan, so it never changes the reported `stack`: the
 * stack is repainted between phases, which would otherwise erase the headline
 * high-water marks.  Each phase is measured from a freshly painted stack, so
 * the number is that phase's peak plus the live caller frames.  Mirrors the
 * Rust firmware's stack_diagnostics() exactly.
 */
static void stack_diagnostics(struct harness_metrics *m)
{
    char key[8];
    char value[HW_KV_VALUE_LEN + 1];
    char read_buf[HW_KV_VALUE_LEN + 1];
    uint8_t iter_buf[HW_KV_VALUE_LEN + 1];
    struct fdb_blob blob;
    struct fdb_kv_iterator itr;
    struct ts_iter_ctx ctx;
    uint8_t ts_payload[HW_TS_PAYLOAD_LEN];
    fdb_err_t err;
    size_t got;
    uint32_t n;

    /* (b) one set */
    stack_repaint();
    make_kv_key(key, 0);
    make_kv_value(value, 0, 0x3000u);
    err = fdb_kv_set(&g_kvdb, key, value);
    if (err != FDB_NO_ERR) {
        harness_fail("st_set", (int)err);
    }
    m->st_set = stack_high_water_bytes();

    /* (c) one get */
    stack_repaint();
    got = fdb_kv_get_blob(&g_kvdb, key,
                          fdb_blob_make(&blob, read_buf, HW_KV_VALUE_LEN));
    if (got != HW_KV_VALUE_LEN) {
        harness_fail("st_get", (int)got);
    }
    m->st_get = stack_high_water_bytes();

    /* (d) full iteration */
    stack_repaint();
    memset(&itr, 0, sizeof(itr));
    fdb_kv_iterator_init(&g_kvdb, &itr);
    n = 0;
    while (fdb_kv_iterate(&g_kvdb, &itr)) {
        got = fdb_blob_read(
            (fdb_db_t)&g_kvdb,
            fdb_kv_to_blob(&itr.curr_kv,
                           fdb_blob_make(&blob, iter_buf, sizeof(iter_buf))));
        if (got > sizeof(iter_buf)) {
            harness_fail("st_iter_read", (int)got);
        }
        n++;
    }
    if (n != HW_KV_KEYS) {
        harness_fail("st_iter_count", (int)n);
    }
    m->st_iter = stack_high_water_bytes();

    /* (e) GC churn (same write pattern as the measured workload) */
    stack_repaint();
    for (n = 0; n < HW_KV_CHURN; n++) {
        uint32_t idx = n % HW_KV_CHURN_KEYS;
        make_kv_key(key, idx);
        make_kv_value(value, idx, 0x4000u + n);
        err = fdb_kv_set(&g_kvdb, key, value);
        if (err != FDB_NO_ERR) {
            harness_fail("st_gc", (int)err);
        }
    }
    m->st_gc = stack_high_water_bytes();

    /* (f) one TSDB append, (g) full TSDB iteration.  The TS database already
     *     holds the 200 workload records; the diagnostic append uses the next
     *     timestamp, so the iteration sees 201 entries. */
    stack_repaint();
    make_ts_payload(ts_payload, HW_TS_APPENDS);
    fdb_blob_make(&blob, ts_payload, sizeof(ts_payload));
    err = fdb_tsl_append_with_ts(&g_tsdb, &blob,
                                 (fdb_time_t)(HW_TS_BASE_TIME + HW_TS_APPENDS));
    if (err != FDB_NO_ERR) {
        harness_fail("st_ts_append", (int)err);
    }
    m->st_ts_append = stack_high_water_bytes();

    stack_repaint();
    ctx.db = &g_tsdb;
    ctx.count = 0;
    ctx.checksum = 0;
    ctx.bad = 0;
    fdb_tsl_iter(&g_tsdb, ts_iter_cb, &ctx);
    if (ctx.bad != 0) {
        harness_fail("st_ts_iter_read", (int)ctx.bad);
    }
    if (ctx.count != HW_TS_APPENDS + 1) {
        harness_fail("st_ts_iter_count", (int)ctx.count);
    }
    m->st_ts_iter = stack_high_water_bytes();
}
#endif /* HARNESS_STACK_PHASES */

/* ------------------------------------------------------------------------- */
/* Real-board rollover phase (board build only)                              */
/* ------------------------------------------------------------------------- */

#ifdef FDB_BOARD_U545
/*
 * On-silicon counterpart of the host-side tsdb_rollover tests.  The workload
 * above appends 200 entries with rollover disabled; this phase enables it,
 * appends well past the ring capacity (HW_TS_ROLL_APPENDS, several wrap-
 * arounds), then checks:
 *   - the live window is a contiguous, increasing timestamp range;
 *   - every surviving payload still matches the deterministic generator;
 *   - the ring survives a deinit/init (remount) cycle unchanged;
 *   - the ring keeps accepting appends after recovery;
 *   - the NOR simulator saw no set-bit attempts and no out-of-bounds access,
 *     and at least one sector was erased while rolling.
 * The Rust firmware runs the identical phase and prints the same RESULT fields,
 * so compare.ps1 can require field-for-field equality.
 */
struct ts_roll_ctx {
    fdb_tsdb_t db;
    uint32_t count;
    uint32_t first;
    uint32_t last;
    uint32_t checksum;
    uint8_t order_ok;
    uint8_t payload_ok;
};

static bool ts_roll_cb(fdb_tsl_t tsl, void *arg)
{
    struct ts_roll_ctx *ctx = (struct ts_roll_ctx *)arg;
    uint8_t buf[HW_TS_PAYLOAD_LEN];
    uint8_t expected[HW_TS_PAYLOAD_LEN];
    struct fdb_blob blob;
    size_t got;
    uint32_t idx;

    got = fdb_blob_read(
        (fdb_db_t)ctx->db,
        fdb_tsl_to_blob(tsl, fdb_blob_make(&blob, buf, sizeof(buf))));
    if (got != HW_TS_PAYLOAD_LEN) {
        ctx->payload_ok = 0;
        return true;
    }
    if (ctx->count > 0 && tsl->time != (fdb_time_t)(ctx->last + 1u)) {
        ctx->order_ok = 0;
    }
    idx = (uint32_t)(tsl->time - HW_TS_BASE_TIME);
    make_ts_payload(expected, idx);
    if (memcmp(buf, expected, HW_TS_PAYLOAD_LEN) != 0) {
        ctx->payload_ok = 0;
    }
    if (ctx->count == 0) {
        ctx->first = (uint32_t)tsl->time;
    }
    ctx->last = (uint32_t)tsl->time;
    ctx->checksum = fnv1a(ctx->checksum, buf, got);
    ctx->count++;
    return false; /* continue iterating */
}

static void ts_roll_scan(struct ts_roll_ctx *ctx)
{
    ctx->db = &g_tsdb;
    ctx->count = 0;
    ctx->first = 0;
    ctx->last = 0;
    ctx->checksum = 0;
    ctx->order_ok = 1;
    ctx->payload_ok = 1;
    fdb_tsl_iter(&g_tsdb, ts_roll_cb, ctx);
    if (!ctx->order_ok) {
        harness_fail("ts_roll_order", 0);
    }
    if (!ctx->payload_ok) {
        harness_fail("ts_roll_payload", 0);
    }
}

static void ts_rollover_phase(struct harness_metrics *m)
{
    uint8_t payload[HW_TS_PAYLOAD_LEN];
    struct fdb_blob blob;
    struct ts_roll_ctx before;
    struct ts_roll_ctx after;
    fdb_err_t err;
    fdb_time_t last;
    bool ro = true;
    uint32_t i;
    uint32_t idx;
    uint32_t erases;
    uint32_t violations;

    sim_nor_stats_reset();
    fdb_tsdb_control(&g_tsdb, FDB_TSDB_CTRL_SET_ROLLOVER, &ro);

    for (i = 0; i < HW_TS_ROLL_APPENDS; i++) {
        idx = HW_TS_APPENDS + i;
        make_ts_payload(payload, idx);
        fdb_blob_make(&blob, payload, sizeof(payload));
        err = fdb_tsl_append_with_ts(&g_tsdb, &blob,
                                     (fdb_time_t)(HW_TS_BASE_TIME + idx));
        if (err != FDB_NO_ERR) {
            harness_fail("ts_roll_append", (int)err);
        }
        m->ts_roll_appends++;
    }

    ts_roll_scan(&before);
    if (before.count == 0) {
        harness_fail("ts_roll_count", 0);
    }
    if (before.first + before.count - 1u != before.last) {
        harness_fail("ts_roll_window", 0);
    }
    m->ts_roll_count = before.count;
    m->ts_roll_first_time = before.first;
    m->ts_roll_last_time = before.last;
    m->ts_roll_checksum = before.checksum;

    /* Remount from flash: the ring state must come back identically. */
    fdb_tsdb_deinit(&g_tsdb);
    err = fdb_tsdb_init(&g_tsdb, "tsdb", "fdb_tsdb1", harness_get_time,
                        HW_TSDB_MAX_LEN, NULL);
    if (err != FDB_NO_ERR) {
        harness_fail("ts_roll_remount", (int)err);
    }
    ts_roll_scan(&after);
    m->ts_roll_remount_count = after.count;
    m->ts_roll_remount_checksum = after.checksum;
    if (after.count != before.count || after.first != before.first ||
        after.last != before.last || after.checksum != before.checksum) {
        harness_fail("ts_roll_remount_diff", 0);
    }

    /* The ring keeps accepting appends after recovery. */
    idx = HW_TS_APPENDS + HW_TS_ROLL_APPENDS;
    make_ts_payload(payload, idx);
    fdb_blob_make(&blob, payload, sizeof(payload));
    err = fdb_tsl_append_with_ts(&g_tsdb, &blob,
                                 (fdb_time_t)(HW_TS_BASE_TIME + idx));
    if (err != FDB_NO_ERR) {
        harness_fail("ts_roll_continue", (int)err);
    }
    fdb_tsdb_control(&g_tsdb, FDB_TSDB_CTRL_GET_LAST_TIME, &last);
    if (last != (fdb_time_t)(HW_TS_BASE_TIME + idx)) {
        harness_fail("ts_roll_last_time", 0);
    }
    m->ts_roll_continue_ok = 1;

    erases = sim_nor_erases();
    violations = sim_nor_set_bit_attempts() + sim_nor_bounds_violations();
    m->ts_roll_erases = erases;
    m->ts_roll_violations = violations;
    if (erases == 0) {
        harness_fail("ts_roll_erases", 0);
    }
    if (violations != 0) {
        harness_fail("ts_roll_nor", (int)violations);
    }
}
#endif /* FDB_BOARD_U545 */

#ifdef FDB_BOARD_U545
extern uint8_t __sim_flash_start[];

/* FNV-1a over a whole simulated partition: the on-flash image fingerprint used
 * to prove C/Rust write-identical images on silicon. */
static uint32_t image_checksum(uint32_t offset, uint32_t len)
{
    uint32_t h = 0;
    uint32_t i;

    for (i = 0; i < len; i++) {
        h ^= (uint32_t)__sim_flash_start[offset + i];
        h *= 16777619u;
    }
    return h;
}
#endif

/* ------------------------------------------------------------------------- */
/* Entry point                                                               */
/* ------------------------------------------------------------------------- */

int main(void)
{
    struct harness_metrics m;

    memset(&m, 0, sizeof(m));
    harness_fill_section_sizes(&m);
    m.stack_limit = stack_limit_bytes();

#ifdef FDB_BOARD_U545
    /* Real-board build: RAM output buffer + DWT cycle counters, mirroring the
     * Rust firmware's `board-u545` feature so both images report identical
     * RESULT/BOARD fields.  Cycle phases are delimited at the same points as
     * in harness/firmware/rust/main.rs. */
    uint32_t t0, t_kv, t_ts, t_roll;
    uint32_t cycles_mount, cycles_kv, cycles_ts, cycles_roll;

    board_reset_output();
    board_cycles_init();
    t0 = board_cycles();
#endif

    databases_init();

#ifdef FDB_BOARD_U545
    cycles_mount = board_cycles() - t0;
    t_kv = board_cycles();
#endif

    /* High-water reached by database init, before the workload starts. */
    m.stack_init = stack_high_water_bytes();

    kvdb_phase(&m);

#ifdef FDB_BOARD_U545
    cycles_kv = board_cycles() - t_kv;
    t_ts = board_cycles();
#endif

    tsdb_phase(&m);

#ifdef FDB_BOARD_U545
    cycles_ts = board_cycles() - t_ts;

    /* Real-board rollover phase: ring fill + wrap + recovery. */
    t_roll = board_cycles();
    ts_rollover_phase(&m);
    cycles_roll = board_cycles() - t_roll;
#endif

    /* Final scan after both phases have returned to this shallow frame.  The
     * scan sees the deepest frame of the whole run, including init. */
    m.stack = stack_high_water_bytes();

#ifdef HARNESS_STACK_PHASES
    /* Per-phase attribution (does not affect m.stack: repaints happen after). */
    stack_diagnostics(&m);
#endif

    harness_print_result(&m);

#ifdef FDB_BOARD_U545
    /* Raw partition fingerprints: byte-level C/Rust image identity. */
    m.kv_image_checksum = image_checksum(HW_KVDB_PART_OFFSET, HW_KVDB_PART_LEN);
    m.ts_image_checksum = image_checksum(HW_TSDB_PART_OFFSET, HW_TSDB_PART_LEN);
    harness_print_rollover(&m);

    /* Cycle report consumed by harness/firmware/u545re/run_board.ps1. */
    sh_write0("BOARD cycles_mount=");
    sh_write_u32(cycles_mount);
    sh_write0(" cycles_kv=");
    sh_write_u32(cycles_kv);
    sh_write0(" cycles_ts=");
    sh_write_u32(cycles_ts);
    sh_write0(" cycles_roll=");
    sh_write_u32(cycles_roll);
    sh_write0(" cycles_total=");
    sh_write_u32(board_cycles());
    sh_write0(" core_hz=");
    sh_write_u32(board_core_hz());
    sh_write0("\n");
#endif

    harness_exit();
    return 0;
}
