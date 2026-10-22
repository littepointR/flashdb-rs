/*
 * FlashDB firmware measurement harness -- NUCLEO-U545RE-Q (STM32U545RE) C side.
 *
 * The C firmware is normally measured under QEMU through ARM semihosting.
 * On the real board there is no semihosting console, so this backend replaces
 * it with the same scheme the Rust firmware uses under its `board-u545`
 * feature:
 *
 *   * output is appended to `BOARD_OUT`, a static buffer placed in the
 *     `.results` NOLOAD section, which the host reads back over SWD;
 *   * a DONE magic word tells the host the run has finished;
 *   * DWT counters provide mount/KV/TS/total cycle counts;
 *   * `core_hz` is decoded from the RCC registers instead of assumed.
 *
 * Both firmware images therefore produce the same `RESULT` + `BOARD` lines and
 * can be compared field by field on identical silicon.
 */
#ifndef HARNESS_BOARD_U545_H
#define HARNESS_BOARD_U545_H

#include <stdint.h>

#define BOARD_DONE_MAGIC 0x454E4F44u /* "DONE" little-endian */

/* Layout read back by the host; must match the Rust `board::BoardOut`. */
struct board_out {
    volatile uint32_t done; /* BOARD_DONE_MAGIC once the run finished */
    volatile uint32_t len;  /* valid bytes in buf, including the NUL */
    char buf[4096];
};

extern struct board_out BOARD_OUT;

/* `.results` is NOLOAD: call this before the first sh_write0(). */
void board_reset_output(void);

/* DWT cycle counter. */
void board_cycles_init(void);
uint32_t board_cycles(void);

/* SYSCLK as decoded from RCC; 0 means "unknown, do not convert cycles". */
uint32_t board_core_hz(void);

/* Semihosting replacements used by harness/firmware/common/semihost.c. */
void board_sh_write0(const char *s);
void board_exit(void) __attribute__((noreturn));

#endif /* HARNESS_BOARD_U545_H */
