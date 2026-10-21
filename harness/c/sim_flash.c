/*
 * RAM-backed NOR flash simulator + FAL device for the C reference harness.
 *
 * See sim_flash.h for the semantics contract. This file also defines the
 * single FAL flash device declared by fal_cfg.h.
 */

#include <stdarg.h>
#include <stdio.h>
#include <string.h>

#include <fdb_cfg.h>
#include <fal.h>

#include "sim_flash.h"

static uint8_t nor_ram[SIM_FLASH_SIZE];
static struct flash_stats stats;
static uint64_t set_bit_attempts;
static uint64_t bounds_violations;

static int fault_armed;
static uint64_t fault_after_ops;
static uint64_t fault_ops_seen;

int fdb_host_log_enabled;

void fdb_host_log(const char *fmt, ...)
{
    va_list ap;

    if (!fdb_host_log_enabled) {
        return;
    }
    va_start(ap, fmt);
    vfprintf(stderr, fmt, ap);
    va_end(ap);
    fflush(stderr);
}

static void stats_reset_internal(void)
{
    memset(&stats, 0, sizeof(stats));
}

void sim_flash_global_init(void)
{
    memset(nor_ram, 0xFF, sizeof(nor_ram));
    stats_reset_internal();
    sim_nor_diag_reset();
    sim_fault_clear();
}

void sim_flash_erase_all(void)
{
    memset(nor_ram, 0xFF, sizeof(nor_ram));
}

uint8_t *sim_flash_image(void)
{
    return nor_ram;
}

size_t sim_flash_image_size(void)
{
    return sizeof(nor_ram);
}

struct flash_stats sim_flash_stats_get(void)
{
    return stats;
}

void sim_flash_stats_reset(void)
{
    stats_reset_internal();
}

uint64_t sim_nor_set_bit_attempts(void)
{
    return set_bit_attempts;
}

uint64_t sim_nor_bounds_violations(void)
{
    return bounds_violations;
}

void sim_nor_diag_reset(void)
{
    set_bit_attempts = 0;
    bounds_violations = 0;
}

void sim_fault_arm(uint64_t ops_before_failure)
{
    fault_armed = 1;
    fault_after_ops = ops_before_failure;
    fault_ops_seen = 0;
}

void sim_fault_clear(void)
{
    fault_armed = 0;
    fault_after_ops = 0;
    fault_ops_seen = 0;
}

uint64_t sim_fault_ops_attempted(void)
{
    return fault_ops_seen;
}

/* @return 1 when the operation must be failed by the fault injector. */
static int fault_should_fail(void)
{
    if (!fault_armed) {
        return 0;
    }
    if (fault_ops_seen >= fault_after_ops) {
        fault_ops_seen++;
        return 1;
    }
    fault_ops_seen++;
    return 0;
}

static int sim_read(long offset, uint8_t *buf, size_t size)
{
    if (fault_should_fail()) {
        return -1;
    }
    if (offset < 0 || size > SIM_FLASH_SIZE || (size_t)offset > SIM_FLASH_SIZE - size) {
        bounds_violations++;
        return -1;
    }
    memcpy(buf, nor_ram + offset, size);
    stats.reads++;
    stats.read_bytes += size;
    return (int)size;
}

static int sim_write(long offset, const uint8_t *buf, size_t size)
{
    size_t i;

    if (fault_should_fail()) {
        return -1;
    }
    if (offset < 0 || size > SIM_FLASH_SIZE || (size_t)offset > SIM_FLASH_SIZE - size) {
        bounds_violations++;
        return -1;
    }
    for (i = 0; i < size; i++) {
        uint8_t old_b = nor_ram[offset + i];
        uint8_t new_b = buf[i];
        if ((uint8_t)(old_b & new_b) != new_b) {
            /* tried to turn a 0 bit into 1: real NOR silently keeps it 0 */
            set_bit_attempts++;
        }
        nor_ram[offset + i] = (uint8_t)(old_b & new_b);
    }
    stats.writes++;
    stats.write_bytes += size;
    return (int)size;
}

static int sim_erase(long offset, size_t size)
{
    if (fault_should_fail()) {
        return -1;
    }
    if (size == 0 || offset < 0 || (offset % (long)SIM_FLASH_BLK_SIZE) != 0 ||
        (size % SIM_FLASH_BLK_SIZE) != 0 ||
        size > SIM_FLASH_SIZE || (size_t)offset > SIM_FLASH_SIZE - size) {
        bounds_violations++;
        return -1;
    }
    memset(nor_ram + offset, 0xFF, size);
    stats.erases++;
    stats.erase_bytes += size;
    return (int)size;
}

static int sim_init(void)
{
    /* The initial erased state is established by sim_flash_global_init(). */
    return 0;
}

const struct fal_flash_dev nor_flash0 = {
    .name = NOR_FLASH_DEV_NAME,
    .addr = 0,
    .len = SIM_FLASH_SIZE,
    .blk_size = SIM_FLASH_BLK_SIZE,
    .ops = {
        .init = sim_init,
        .read = sim_read,
        .write = sim_write,
        .erase = sim_erase,
    },
    .write_gran = FDB_WRITE_GRAN,
};
