# NUCLEO-U545RE-Q real-hardware test

Board: **NUCLEO-U545RE-Q** — STM32U545RE (Cortex-M33 r0p4, 512 KiB flash,
192 KiB SRAM1 + 64 KiB SRAM2), on-board ST-LINK/V3 (VID:PID `0483:374e`).

The test runs the **real `flashdb` crate** (no host code) on the MCU:

* KVDB: 64 keys × 24 B, 32 updates, 1500-update GC churn, 64 verified gets,
  full iteration over a 16 KiB partition (4 × 4 KiB sectors);
* TSDB: 200 appends × 16 B payload, full forward iteration over a second
  16 KiB partition;
* both databases use the **RAM NOR simulator** in the `.sim_flash` region, so
  this stage measures real Cortex-M33 CPU behaviour and checks correctness on
  silicon. A real internal-flash driver is a follow-up step.

Results are written to a RAM buffer (`BOARD_OUT`, `.results` section) instead of
semihosting, so they can be read back through any debug probe. DWT cycle
counters report mount/KV/TS/total cycles. `RESULT` fields are identical to the
QEMU measurement firmware, so the checksums can be compared directly with
`scripts/fw_rust.sh` / `scripts/fw_c.sh`.

## Build and run

Requirements: `arm-none-eabi-gcc`/`nm`/`readelf`, `openocd` or `pyocd`, and the
board connected over USB.

```sh
# build only (no hardware needed)
harness/firmware/u545re/run_rust.sh --build-only

# build + flash + run; pyOCD is preferred, OpenOCD is the fallback
harness/firmware/u545re/run_rust.sh

# force a backend or skip the build
harness/firmware/u545re/run_rust.sh --tool openocd
harness/firmware/u545re/run_rust.sh --no-build
```

The firmware feature is `board-u545` (see `harness/firmware/rust/Cargo.toml`);
the default QEMU firmware build is unchanged.

## Known hardware/tooling issue (as of bring-up on macOS)

The on-board ST-LINK/V3 reports firmware **V3J17M10**, which predates full
STM32U5 programming support. On this board, every flash programming attempt
(OpenOCD flash algorithm, pyOCD `DebugDeviceUnlock`/load, `st-flash`) makes the
ST-LINK firmware hang with USB pipe stalls:

```
libusb: error [submit_bulk_transfer] bulk transfer failed: pipe is stalled
Error: Fail reading CTRL/STAT register. Force reconnect
```

Reading memory, halting and running already-flashed code work, but programming
does not. Recovery currently requires a **physical USB replug**; software
(`libusb_reset_device`, repeated probe opens) is not sufficient.

Fix: update the ST-LINK firmware with **STSW-LINK007** (ST-LinkUpgrade) or
STM32CubeProgrammer, both from ST. After the update, run the command above
again. Alternatively use an external probe supported by OpenOCD/pyOCD.

Also close applications that open the ST-LINK USB device (on macOS the ChatGPT
desktop app claims the device through `AppleUSBHostDeviceUserClient`, which
makes `probe-rs`/OpenOCD/pyOCD fail with "interfaces are claimed" or USB
timeouts).

## Status

* build: verified (`thumbv8m.main-none-eabi`, LLD/GCC link, `BOARD_OUT` symbol
  resolves, result buffer and DWT code included);
* default QEMU firmware: unaffected (feature-gated);
* hardware run: **blocked on the ST-LINK firmware issue above** — the board's
  original demo firmware was erased by the first failed flash attempt, so the
  MCU currently has no valid image and must be reflashed once the probe can
  program again.
