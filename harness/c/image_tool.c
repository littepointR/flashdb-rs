/*
 * Image tool: produces/consumes the raw 32 KiB flash image used for the
 * C <-> Rust on-flash compatibility proof.
 *
 * Image format: raw concatenation, exactly 32768 bytes
 *   [0x0000, 0x4000)  KV partition  "fdb_kvdb1" (4 x 4096-byte sectors)
 *   [0x4000, 0x8000)  TS partition  "fdb_tsdb1" (4 x 4096-byte sectors)
 *
 *   image_tool write <out.bin> <wg>
 *   image_tool read  <in.bin>  <wg>
 *
 * <wg> must equal the FDB_WRITE_GRAN the tool was compiled with.
 *
 * Deterministic workload (must be identical in the Rust implementation):
 *   KV, value helper kv_value(seed,i,len): byte[j] = (seed + i*31 + j*7) & 0xFF
 *     for i in 0..7:  set("img{i}", kv_value(0x55, i, 8 << (i % 4)))
 *                     -> lengths 8,16,32,64,8,16,32,64
 *     for i in 0..3:  set("img{i}", kv_value(0x56, i, 16))
 *     delete("img7")
 *     for k in 0..399: set("churn", kv_value(0x57, k, 64))
 *   TS, rollover disabled, max_len 256:
 *     for i in 0..19: append(payload = ts_payload(i) 32 bytes, t = 1000 + i)
 *
 * write mode prints the same summary as read mode to stdout and the concrete
 * flash-op counts of the workload to stderr, e.g.:
 *   KVFLASH reads=.. read_bytes=.. writes=.. write_bytes=.. erases=.. erase_bytes=..
 *   TSFLASH ...
 *   IMAGEFLASH ...   (total over both workloads)
 *
 * Summary format (exact):
 *   KV key=<key> len=<value_len> fnv=<fnv1a32(value) decimal>
 *   ... in KV iteration order ...
 *   TS time=<t> status=<int> len=<log_len> fnv=<fnv1a32(payload) decimal>
 *   ... in forward TS iteration order ...
 *   KVCOUNT=<n> TSCOUNT=<n>
 * On mount/check failure: prints "ERROR=<code>" and exits 2.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "bench.h"

#define IMAGE_FILE_SIZE  ((size_t)SIM_FLASH_SIZE)
#define IMAGE_TS_MAX_LEN 256u
#define IMAGE_ERR_IO     255

static fdb_kvdb_t image_kvdb(void)
{
    static struct fdb_kvdb db;

    return &db;
}

static fdb_tsdb_t image_tsdb(void)
{
    static struct fdb_tsdb db;

    return &db;
}

static fdb_time_t image_get_time(void)
{
    return 0;
}

static int fail_code(int code)
{
    printf("ERROR=%d\n", code);
    return 2;
}

/* ------------------------------------------------------------------ */
/* deterministic write workload                                        */
/* ------------------------------------------------------------------ */

static void kv_set_or_die(fdb_kvdb_t db, const char *key, const uint8_t *val, size_t len)
{
    struct fdb_blob blob;
    fdb_err_t rc = fdb_kv_set_blob(db, key, fdb_blob_make(&blob, val, len));

    if (rc != FDB_NO_ERR) {
        fprintf(stderr, "image_tool: set('%s') failed: %s\n", key, bench_err_name(rc));
        exit(3);
    }
}

static void image_build_kv_workload(fdb_kvdb_t db)
{
    char key[16];
    uint8_t val[64];
    uint32_t i;

    /* 8 initial keys, lengths 8,16,32,64,8,16,32,64 */
    for (i = 0; i < 8; i++) {
        size_t len = (size_t)8u << (i % 4u);

        bench_key(key, sizeof(key), "img", i);
        bench_value(val, sizeof(val), 0x55u, i);
        kv_set_or_die(db, key, val, len);
    }
    /* 4 updates (16-byte values) */
    for (i = 0; i < 4; i++) {
        bench_key(key, sizeof(key), "img", i);
        bench_value(val, sizeof(val), 0x56u, i);
        kv_set_or_die(db, key, val, 16);
    }
    /* delete img7 */
    {
        fdb_err_t rc = fdb_kv_del(db, "img7");

        if (rc != FDB_NO_ERR) {
            fprintf(stderr, "image_tool: delete('img7') failed: %s\n", bench_err_name(rc));
            exit(3);
        }
    }
    /* GC churn: 400 updates of a 64-byte value */
    for (i = 0; i < 400; i++) {
        bench_value(val, sizeof(val), 0x57u, i);
        kv_set_or_die(db, "churn", val, sizeof(val));
    }
}

static void image_build_ts_workload(fdb_tsdb_t db)
{
    uint32_t i;

    for (i = 0; i < 20; i++) {
        fdb_err_t rc = bench_ts_append(db, i, (fdb_time_t)(BENCH_TS_START_TIME + (int)i));

        if (rc != FDB_NO_ERR) {
            fprintf(stderr, "image_tool: ts append %u failed: %s\n", i, bench_err_name(rc));
            exit(3);
        }
    }
}

/* ------------------------------------------------------------------ */
/* summary                                                             */
/* ------------------------------------------------------------------ */

struct ts_summary_ctx {
    fdb_tsdb_t db;
    uint32_t count;
    int error;
};

static bool image_ts_cb(fdb_tsl_t tsl, void *arg)
{
    struct ts_summary_ctx *c = (struct ts_summary_ctx *)arg;
    struct fdb_blob blob;
    uint8_t payload[512];
    size_t len = tsl->log_len;

    if (len > sizeof(payload)) {
        c->error = FDB_READ_ERR;
        return true;
    }
    fdb_blob_make(&blob, payload, len);
    fdb_tsl_to_blob(tsl, &blob);
    if (fdb_blob_read((fdb_db_t)c->db, &blob) != len) {
        c->error = FDB_READ_ERR;
        return true;
    }
    printf("TS time=%d status=%d len=%u fnv=%u\n",
           (int)tsl->time, (int)tsl->status, (unsigned)len,
           (unsigned)bench_cs_bytes(BENCH_FNV32_OFFSET, payload, len));
    c->count++;
    return false;
}

/* Prints the deterministic summary; returns 0 on success, an fdb_err_t code
 * (or IMAGE_ERR_IO) on failure. */
static int image_print_summary(fdb_kvdb_t kv, fdb_tsdb_t ts)
{
    struct fdb_kv_iterator itr;
    struct ts_summary_ctx tsc;
    uint32_t kvcount = 0;

    fdb_kv_iterator_init(kv, &itr);
    while (fdb_kv_iterate(kv, &itr)) {
        size_t vlen = itr.curr_kv.value_len;
        uint8_t *buf = vlen ? (uint8_t *)malloc(vlen) : NULL;
        struct fdb_blob blob;

        if (vlen && buf == NULL) {
            return IMAGE_ERR_IO;
        }
        fdb_blob_make(&blob, buf, vlen);
        if (fdb_kv_get_blob(kv, itr.curr_kv.name, &blob) != vlen) {
            free(buf);
            return FDB_READ_ERR;
        }
        printf("KV key=%s len=%u fnv=%u\n", itr.curr_kv.name, (unsigned)vlen,
               (unsigned)bench_cs_bytes(BENCH_FNV32_OFFSET, buf, vlen));
        free(buf);
        kvcount++;
    }

    memset(&tsc, 0, sizeof(tsc));
    tsc.db = ts;
    fdb_tsl_iter(ts, image_ts_cb, &tsc);
    if (tsc.error != 0) {
        return tsc.error;
    }

    printf("KVCOUNT=%u TSCOUNT=%u\n", (unsigned)kvcount, (unsigned)tsc.count);
    return 0;
}

/* ------------------------------------------------------------------ */
/* setup helpers (image tool has its own TS partition handling so that  */
/* formatting TS does not erase the KV image)                           */
/* ------------------------------------------------------------------ */

static int image_format_and_build(void)
{
    fdb_kvdb_t kv = image_kvdb();
    fdb_tsdb_t ts = image_tsdb();
    fdb_err_t rc;
    struct flash_stats kv_stats, ts_stats;
    const struct fal_partition *ts_part;
    bool rollover = false;

    sim_flash_global_init();
    if (fal_init() <= 0) {
        fprintf(stderr, "image_tool: FAL init failed\n");
        return IMAGE_ERR_IO;
    }

    /* KV partition: format by erasing the whole simulated device first. */
    sim_flash_stats_reset();
    bench_kv_setup_format(kv);
    image_build_kv_workload(kv);
    kv_stats = sim_flash_stats_get();

    /* TS partition: erase only the TS partition, then let TSDB format it. */
    ts_part = fal_partition_find(BENCH_TS_PART_NAME);
    if (ts_part == NULL) {
        fprintf(stderr, "image_tool: TS partition not found\n");
        return IMAGE_ERR_IO;
    }
    if (fal_partition_erase_all(ts_part) < 0) {
        fprintf(stderr, "image_tool: TS partition erase failed\n");
        return IMAGE_ERR_IO;
    }
    memset(ts, 0, sizeof(*ts));
    sim_flash_stats_reset();
    rc = fdb_tsdb_init(ts, "tsdb", BENCH_TS_PART_NAME, image_get_time,
                       IMAGE_TS_MAX_LEN, NULL);
    if (rc != FDB_NO_ERR) {
        return (int)rc;
    }
    fdb_tsdb_control(ts, FDB_TSDB_CTRL_SET_ROLLOVER, &rollover);
    image_build_ts_workload(ts);
    ts_stats = sim_flash_stats_get();

    fprintf(stderr, "KVFLASH reads=%llu read_bytes=%llu writes=%llu write_bytes=%llu erases=%llu erase_bytes=%llu\n",
            (unsigned long long)kv_stats.reads, (unsigned long long)kv_stats.read_bytes,
            (unsigned long long)kv_stats.writes, (unsigned long long)kv_stats.write_bytes,
            (unsigned long long)kv_stats.erases, (unsigned long long)kv_stats.erase_bytes);
    fprintf(stderr, "TSFLASH reads=%llu read_bytes=%llu writes=%llu write_bytes=%llu erases=%llu erase_bytes=%llu\n",
            (unsigned long long)ts_stats.reads, (unsigned long long)ts_stats.read_bytes,
            (unsigned long long)ts_stats.writes, (unsigned long long)ts_stats.write_bytes,
            (unsigned long long)ts_stats.erases, (unsigned long long)ts_stats.erase_bytes);
    fprintf(stderr, "IMAGEFLASH reads=%llu read_bytes=%llu writes=%llu write_bytes=%llu erases=%llu erase_bytes=%llu\n",
            (unsigned long long)(kv_stats.reads + ts_stats.reads),
            (unsigned long long)(kv_stats.read_bytes + ts_stats.read_bytes),
            (unsigned long long)(kv_stats.writes + ts_stats.writes),
            (unsigned long long)(kv_stats.write_bytes + ts_stats.write_bytes),
            (unsigned long long)(kv_stats.erases + ts_stats.erases),
            (unsigned long long)(kv_stats.erase_bytes + ts_stats.erase_bytes));
    return 0;
}

static int image_mount_existing(void)
{
    fdb_kvdb_t kv = image_kvdb();
    fdb_tsdb_t ts = image_tsdb();
    fdb_err_t rc;
    bool not_format = true;

    if (fal_init() <= 0) {
        fprintf(stderr, "image_tool: FAL init failed\n");
        return IMAGE_ERR_IO;
    }

    memset(kv, 0, sizeof(*kv));
    fdb_kvdb_control(kv, FDB_KVDB_CTRL_SET_NOT_FORMAT, &not_format);
    rc = fdb_kvdb_init(kv, "kvdb", BENCH_KV_PART_NAME, NULL, NULL);
    if (rc != FDB_NO_ERR) {
        return (int)rc;
    }
    rc = fdb_kvdb_check(kv);
    if (rc != FDB_NO_ERR) {
        return (int)rc;
    }

    memset(ts, 0, sizeof(*ts));
    fdb_tsdb_control(ts, FDB_TSDB_CTRL_SET_NOT_FORMAT, &not_format);
    rc = fdb_tsdb_init(ts, "tsdb", BENCH_TS_PART_NAME, image_get_time,
                       IMAGE_TS_MAX_LEN, NULL);
    if (rc != FDB_NO_ERR) {
        return (int)rc;
    }
    return 0;
}

/* ------------------------------------------------------------------ */
/* file IO                                                             */
/* ------------------------------------------------------------------ */

static int image_write_file(const char *path)
{
    FILE *f = fopen(path, "wb");
    size_t n;

    if (f == NULL) {
        fprintf(stderr, "image_tool: cannot open '%s' for writing\n", path);
        return IMAGE_ERR_IO;
    }
    n = fwrite(sim_flash_image(), 1, IMAGE_FILE_SIZE, f);
    if (fclose(f) != 0 || n != IMAGE_FILE_SIZE) {
        fprintf(stderr, "image_tool: short write to '%s'\n", path);
        return IMAGE_ERR_IO;
    }
    return 0;
}

static int image_read_file(const char *path)
{
    FILE *f = fopen(path, "rb");
    size_t n;

    if (f == NULL) {
        fprintf(stderr, "image_tool: cannot open '%s' for reading\n", path);
        return IMAGE_ERR_IO;
    }
    sim_flash_global_init();
    n = fread(sim_flash_image(), 1, IMAGE_FILE_SIZE, f);
    if (n != IMAGE_FILE_SIZE) {
        fprintf(stderr, "image_tool: '%s' is %zu bytes, expected %zu\n",
                path, n, IMAGE_FILE_SIZE);
        fclose(f);
        return IMAGE_ERR_IO;
    }
    if (fgetc(f) != EOF) {
        fprintf(stderr, "image_tool: '%s' is larger than %zu bytes\n",
                path, IMAGE_FILE_SIZE);
        fclose(f);
        return IMAGE_ERR_IO;
    }
    fclose(f);
    return 0;
}

/* ------------------------------------------------------------------ */
/* main                                                                */
/* ------------------------------------------------------------------ */

static void usage(const char *argv0)
{
    fprintf(stderr,
            "usage: %s write <out.bin> <wg>   # build deterministic image + print summary\n"
            "       %s read  <in.bin>  <wg>   # mount image + print summary\n"
            "  <wg> must match the FDB_WRITE_GRAN the tool was compiled with (this build: %d)\n",
            argv0, argv0, (int)FDB_WRITE_GRAN);
}

int main(int argc, char **argv)
{
    const char *cmd, *path;
    int wg, err, sum_rc;

    if (argc != 4) {
        usage(argv[0]);
        return 2;
    }
    cmd = argv[1];
    path = argv[2];
    wg = atoi(argv[3]);

    if (wg != (int)FDB_WRITE_GRAN) {
        fprintf(stderr,
                "image_tool: <wg> %d does not match this build's FDB_WRITE_GRAN %d\n",
                wg, (int)FDB_WRITE_GRAN);
        return 2;
    }

    if (getenv("C_HARNESS_VERBOSE") != NULL) {
        fdb_host_log_enabled = 1;
    }
    sim_flash_global_init();

    if (strcmp(cmd, "write") == 0) {
        err = image_format_and_build();
        if (err != 0) {
            return fail_code(err);
        }
        sum_rc = image_print_summary(image_kvdb(), image_tsdb());
        if (sum_rc != 0) {
            return fail_code(sum_rc);
        }
        err = image_write_file(path);
        if (err != 0) {
            return fail_code(err);
        }
        return 0;
    }

    if (strcmp(cmd, "read") == 0) {
        err = image_read_file(path);
        if (err != 0) {
            return fail_code(err);
        }
        err = image_mount_existing();
        if (err != 0) {
            return fail_code(err);
        }
        sum_rc = image_print_summary(image_kvdb(), image_tsdb());
        if (sum_rc != 0) {
            return fail_code(sum_rc);
        }
        return 0;
    }

    usage(argv[0]);
    return 2;
}
