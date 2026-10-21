/*
 * FlashDB firmware measurement harness -- ARM semihosting output.
 *
 * Uses the QEMU "semihosting" interface (BKPT 0xAB on Cortex-M) so the
 * firmware needs no UART driver and no libc.  Operation numbers are the
 * ARM semihosting spec values:
 *   SYS_WRITE0 (0x04): r1 = pointer to NUL-terminated string
 *   SYS_EXIT   (0x18): r1 = ADP_Stopped_ApplicationExit (0x20026)
 *
 * QEMU must be started with `-semihosting` (or
 * `-semihosting-config enable=on,target=native`).  Without it the BKPT
 * traps to the hard-fault handler and the run hangs until the host timeout.
 */
#ifndef HARNESS_SEMIHOST_H
#define HARNESS_SEMIHOST_H

#include <stdint.h>

#define SEMIHOST_SYS_WRITE0 0x04u
#define SEMIHOST_SYS_EXIT   0x18u
#define SEMIHOST_ADP_STOPPED_APP_EXIT 0x20026u

/* Write a NUL-terminated string to the host console. */
void sh_write0(const char *s);

/* Write an unsigned 32-bit value in decimal. */
void sh_write_u32(uint32_t value);

/* Write a 32-bit value as 0xXXXXXXXX. */
void sh_write_hex32(uint32_t value);

/*
 * Print exactly one line of the form:
 *   RESULT text=.. rodata=.. data=.. bss=.. sim_flash=.. stack=.. ...
 * Every harness build must print a line starting with "RESULT " so the host
 * scripts can parse it.  Failure lines start with "ERROR ".
 */
struct harness_metrics {
    /* Section sizes, in bytes, derived from linker symbols (common/harness.c).
     * These match `arm-none-eabi-size -A` for .text/.rodata/.data/.bss. */
    uint32_t text;
    uint32_t rodata;
    uint32_t data;
    uint32_t bss;
    uint32_t sim_flash;

    /* Stack measurement, in bytes.  stack_limit is the painted region size. */
    uint32_t stack;        /* worst-case high-water from boot (after init+workload) */
    uint32_t stack_init;   /* high-water right after database init, before workload
                              (= isolated "mount/init" phase peak) */
    uint32_t stack_limit;
#ifdef HARNESS_STACK_PHASES
    /* Isolated per-phase peaks from common/stack.c stack_repaint() scans.
     * Only present in the diagnostic build (`scripts/fw_c.sh --phases`). */
    uint32_t st_set;
    uint32_t st_get;
    uint32_t st_iter;
    uint32_t st_gc;
    uint32_t st_ts_append;
    uint32_t st_ts_iter;
#endif

    /* Workload results. */
    uint32_t kv_writes;
    uint32_t kv_gets;
    uint32_t kv_iterated;
    uint32_t kv_checksum;      /* FNV-1a over values read in key-index order */
    uint32_t kv_iter_checksum; /* FNV-1a over (name,value) in iteration order */
    uint32_t ts_appends;
    uint32_t ts_iterated;
    uint32_t ts_checksum;
};

void harness_print_result(const struct harness_metrics *m);

/* Print "ERROR stage=<stage> code=<code>" and terminate the emulator. */
void harness_fail(const char *stage, int code) __attribute__((noreturn));

/* Never returns. */
void harness_exit(void) __attribute__((noreturn));

/* Fill the static-size fields from the linker symbols. */
void harness_fill_section_sizes(struct harness_metrics *m);

#endif /* HARNESS_SEMIHOST_H */
