/*
 * FlashDB firmware measurement harness -- stack painting / high-water scan.
 */
#ifndef HARNESS_STACK_H
#define HARNESS_STACK_H

#include <stdint.h>

/* Size in bytes of the painted stack region (8 KiB, see the linker script). */
uint32_t stack_limit_bytes(void);

/*
 * Worst-case stack usage in bytes, measured by scanning the 0xA5-painted
 * region for the lowest modified word.  Call AFTER the workload has returned
 * to the top-level frame.
 */
uint32_t stack_high_water_bytes(void);

#endif /* HARNESS_STACK_H */
