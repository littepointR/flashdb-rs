/*
 * FlashDB firmware measurement harness -- simulated-NOR health counters.
 *
 * The C firmware's RAM NOR simulator can observe two classes of driver misuse
 * that a byte-oriented functional test would miss:
 *
 *   * a set-bit attempt  -- programming a 0 bit to 1 (impossible on real NOR;
 *     FlashDB relies on "write only clears bits", so this would silently
 *     corrupt a status table on hardware);
 *   * an out-of-bounds access -- a read/write/erase outside the partition,
 *     which on a real driver would hit another partition or fault.
 *
 * The counters are compiled only into the real-board build (`FDB_BOARD_U545`),
 * where the rollover phase resets them, drives the ring buffer through several
 * wrap-arounds and requires the counts to stay at zero.  QEMU builds and the
 * host harness keep their own accounting, so this adds nothing to those images.
 */
#ifndef HARNESS_SIM_STATS_H
#define HARNESS_SIM_STATS_H

#include <stdint.h>

void sim_nor_stats_reset(void);
uint32_t sim_nor_set_bit_attempts(void);
uint32_t sim_nor_erases(void);
uint32_t sim_nor_bounds_violations(void);

#endif /* HARNESS_SIM_STATS_H */
