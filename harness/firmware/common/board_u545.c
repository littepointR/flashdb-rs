/*
 * FlashDB firmware measurement harness -- NUCLEO-U545RE-Q board backend (C).
 *
 * Mirrors harness/firmware/rust/main.rs `mod board` so the C reference image
 * and the Rust image are measured and read back in exactly the same way.
 *
 * Memory-mapped registers used here are read/written directly (no HAL, no CMSIS
 * headers) to keep the harness footprint negligible:
 *   DWT  at 0xE000_1000 (DEMCR enable via 0xE000_EDFC)
 *   RCC  at 0x4602_0C00 (STM32U5)
 */
#include "board_u545.h"

/* ------------------------------------------------------------------------- */
/* RAM result buffer (read back by the probe)                                 */
/* ------------------------------------------------------------------------- */

/*
 * `.results` is a NOLOAD output section (see harness/firmware/u545re/board.ld):
 * the image carries no load data for it and the boot code neither copies nor
 * zeroes it, so BOARD_OUT.done/.len hold whatever SRAM contained after reset.
 * board_reset_output() must run before the first write, exactly like the Rust
 * firmware's `board::reset_output()`.
 */
__attribute__((section(".results"), used))
struct board_out BOARD_OUT;

void board_reset_output(void)
{
    BOARD_OUT.done = 0u;
    BOARD_OUT.len = 0u;
}

void board_sh_write0(const char *s)
{
    uint32_t n = BOARD_OUT.len;

    while (*s != '\0' && n < (uint32_t)sizeof(BOARD_OUT.buf)) {
        BOARD_OUT.buf[n++] = *s++;
    }
    BOARD_OUT.len = n;
}

void board_exit(void)
{
    BOARD_OUT.done = BOARD_DONE_MAGIC;
    for (;;) {
        __asm__ volatile("nop");
    }
}

/* ------------------------------------------------------------------------- */
/* DWT cycle counter                                                          */
/* ------------------------------------------------------------------------- */

#define DEMCR     (*(volatile uint32_t *)0xE000EDFCu)
#define DWT_CTRL  (*(volatile uint32_t *)0xE0001000u)
#define DWT_CYCCNT (*(volatile uint32_t *)0xE0001004u)

void board_cycles_init(void)
{
    DEMCR |= (1u << 24);   /* TRCENA */
    DWT_CYCCNT = 0u;
    DWT_CTRL |= 1u;        /* CYCCNTENA */
}

uint32_t board_cycles(void)
{
    return DWT_CYCCNT;
}

/* ------------------------------------------------------------------------- */
/* System clock decode (same logic as the Rust firmware's board::core_hz)     */
/* ------------------------------------------------------------------------- */

#define RCC_ICSCR1 (*(volatile uint32_t *)0x46020C08u)
#define RCC_CFGR1  (*(volatile uint32_t *)0x46020C1Cu)
#define RCC_CSR    (*(volatile uint32_t *)0x46020CF4u)

uint32_t board_core_hz(void)
{
    uint32_t cfgr1 = RCC_CFGR1;
    uint32_t icscr1 = RCC_ICSCR1;
    uint32_t csr = RCC_CSR;
    uint32_t sw = cfgr1 & 0x3u;         /* 0 = MSIS */
    uint32_t msis_range;

    if ((icscr1 & (1u << 23)) != 0u) {
        /* MSIRGSEL = 1: range in RCC_ICSCR1.MSISRANGE */
        msis_range = (icscr1 >> 28) & 0xFu;
    } else {
        /* MSIRGSEL = 0 (reset state): range in RCC_CSR.MSISSRANGE */
        msis_range = (csr >> 12) & 0xFu;
    }

    if (sw == 0u && msis_range == 4u) {
        return 4000000u; /* STM32U5 reset clock: MSIS range 4 = 4 MHz */
    }
    return 0u;
}
