#!/usr/bin/env bash
#
# FlashDB-rs real-hardware test -- NUCLEO-U545RE-Q (STM32U545RE, Cortex-M33).
#
# Builds the Rust firmware with the `board-u545` feature (RAM result buffer +
# DWT cycle counters instead of semihosting), flashes it through the on-board
# ST-LINK/V3 with OpenOCD, runs the workload, then reads the result buffer back
# and prints it. No reset command is used: the ST-LINK V3 firmware on this board
# hangs when OpenOCD tries to drive the target reset, so the image is started by
# loading PC/SP/VTOR after flashing.
#
# Usage:
#   harness/firmware/u545re/run_rust.sh              # build + flash + run
#   harness/firmware/u545re/run_rust.sh --build-only # build only
#   harness/firmware/u545re/run_rust.sh --no-build   # reuse the ELF and run
#   ... --tool pyocd|openocd                        # force a probe backend
#
# Requirements: arm-none-eabi-gcc/objcopy/nm, OpenOCD with ST-LINK support, and
# a NUCLEO-U545RE-Q connected over USB. On macOS, applications that open the
# ST-LINK USB device (e.g. some chat/security-key apps) must be closed first.
#
set -euo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
FW="$ROOT/harness/firmware"
RUST_DIR="$FW/rust"
BOARD="$FW/u545re"
OUT="$FW/build/u545re"
TARGET=thumbv8m.main-none-eabi
ELF="$OUT/fw_rust_u545re.elf"
BUILD_ONLY=0
DO_BUILD=1
TOOL="${NUCLEO_TOOL:-auto}"
for arg in "$@"; do
    case "$arg" in
        --build-only) BUILD_ONLY=1 ;;
        --no-build) DO_BUILD=0 ;;
        --tool) TOOL="${2:-}"; shift ;;
        -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

need() { command -v "$1" >/dev/null 2>&1 || { echo "$1 not found" >&2; exit 1; }; }
need arm-none-eabi-gcc
need arm-none-eabi-nm
need arm-none-eabi-readelf
need openocd
need python3
need cargo

if [ "$DO_BUILD" = "1" ]; then
    echo "== building Rust firmware for $TARGET (board-u545) =="
    mkdir -p "$OUT"
    (
        cd "$RUST_DIR"
        CARGO_TARGET_DIR="$OUT/target" \
        CARGO_TARGET_THUMBV8M_MAIN_NONE_EABI_LINKER=arm-none-eabi-gcc \
        cargo rustc --bin fw-rust --target "$TARGET" --release \
            --features board-u545 -- \
            -C link-arg=-T"$BOARD/board.ld" \
            -C link-arg=-nostartfiles \
            -C link-arg=-nostdlib \
            -C link-arg=-Wl,--gc-sections \
            -C link-arg=-lgcc
    )
    cp "$OUT/target/$TARGET/release/fw-rust" "$ELF"
    arm-none-eabi-size -A "$ELF" | sed -n '2,8p'
    echo
fi

[ -f "$ELF" ] || { echo "missing $ELF (run without --no-build)" >&2; exit 1; }

ENTRY=$(arm-none-eabi-readelf -h "$ELF" | awk "/Entry point/{print \$4}")
ESTACK=$(arm-none-eabi-nm "$ELF" | awk '$3=="_estack"{print $1}')
OUT_ADDR=$(arm-none-eabi-nm "$ELF" | awk '$3=="BOARD_OUT"{print $1}')
OUT_SIZE=$(python3 - "$ELF" <<'PY'
import subprocess, sys, struct
elf = sys.argv[1]
# BoardOut = { done: u32, len: u32, buf: [u8; 4096] }
print(8 + 4096)
PY
)
[ -n "${ENTRY:-}" ] && [ -n "${ESTACK:-}" ] && [ -n "${OUT_ADDR:-}" ] || {
    echo "could not resolve entry/estack/BOARD_OUT symbols" >&2
    exit 1
}

if [ "$TOOL" = "auto" ]; then
    if command -v pyocd >/dev/null 2>&1; then TOOL=pyocd; else TOOL=openocd; fi
fi

echo "== flash + run on NUCLEO-U545RE-Q ($TOOL) =="
echo "   entry   : $ENTRY"
echo "   estack  : 0x$ESTACK"
echo "   BOARD_OUT: 0x$OUT_ADDR (0x$OUT_SIZE bytes)"
echo

if [ "$BUILD_ONLY" = "1" ]; then
    echo "build only: skipping flash/run"
    exit 0
fi

LOG="$OUT/$TOOL.log"
DUMP="$OUT/board_out.bin"
rm -f "$DUMP" "$LOG"
FLASH_OK=1
if [ "$TOOL" = "pyocd" ]; then
    # pyOCD is the primary backend: its STM32U5 support handles the secure
    # debug state and the flash algorithm did not crash the ST-LINK on this
    # board (OpenOCD's flash path did). Programming resets the target, which
    # starts the image from the vector table at 0x08000000.
    if ! pyocd flash -t stm32u545retxq -f 1m -e auto "$ELF" >"$LOG" 2>&1; then
        FLASH_OK=0
    fi
    sleep 3
    if [ "$FLASH_OK" = "1" ]; then
        if ! pyocd commander -t stm32u545retxq -c "halt" \
                -c "savemem 0x$OUT_ADDR 0x$OUT_SIZE $DUMP" -c "quit" >>"$LOG" 2>&1; then
            FLASH_OK=0
        fi
    fi
fi
if [ "$TOOL" != "pyocd" ] || [ "$FLASH_OK" != "1" ]; then
    # OpenOCD backend (fallback): the ST-LINK V3 firmware on this board hangs
    # when it drives reset events, so all target events are overridden and the
    # reset line is only asserted through the flash algorithm itself. On a
    # healthy probe this path works, but it left the probe hung on this board.
    LOG="$OUT/openocd.log"
    if ! openocd -f interface/stlink.cfg -f target/stm32u5x.cfg \
        -c "adapter speed 200" \
        -c "stm32u5x.cpu configure -event examine-end {}" \
        -c "stm32u5x.cpu configure -event reset-init {}" \
        -c "stm32u5x.cpu configure -event reset-start {}" \
        -c "stm32u5x.cpu configure -work-area-phys 0x20030000 -work-area-size 0x4000" \
        -c "init" \
        -c "halt" \
        -c "flash write_image erase $ELF" \
        -c "verify_image $ELF" \
        -c "reg pc $ENTRY" \
        -c "reg sp 0x$ESTACK" \
        -c "reg msp 0x$ESTACK" \
        -c "mww 0xE000ED08 0x08000000" \
        -c "resume" \
        -c "runtest 2000" \
        -c "halt" \
        -c "dump_image $DUMP 0x$OUT_ADDR 0x$OUT_SIZE" \
        -c "shutdown" >"$LOG" 2>&1; then
    echo "openocd failed; last lines:" >&2
    tail -20 "$LOG" >&2
    echo >&2
    echo "If the ST-LINK cannot be opened or times out, unplug/replug the board's" >&2
    echo "USB cable (the probe firmware can hang after a failed flash) and retry." >&2
    exit 1
    fi
fi

python3 - "$DUMP" <<'PY'
import sys, struct
raw = open(sys.argv[1], "rb").read()
done, length = struct.unpack_from("<II", raw, 0)
text = raw[8:8 + min(length, len(raw) - 8)].decode("ascii", "replace")
print("-- firmware output --")
sys.stdout.write(text)
if not text.endswith("\n"):
    print()
print("-- parsed --")
fields = {}
for line in text.splitlines():
    if line.startswith(("RESULT ", "BOARD ")):
        for tok in line.split()[1:]:
            if "=" in tok:
                k, v = tok.split("=", 1)
                fields[k] = v
result = {k: v for k, v in fields.items() if k.startswith(("kv_", "ts_", "rc"))}
print("RESULT fields:", " ".join(f"{k}={v}" for k, v in result.items()))
if "cycles_total" in fields:
    hz = int(fields.get("core_hz", "16000000"))
    for k in ("cycles_mount", "cycles_kv", "cycles_ts", "cycles_total"):
        if k in fields:
            c = int(fields[k])
            print(f"{k:14s} = {c:10d} cycles  ({c * 1e6 / hz:9.1f} us @ {hz/1e6:.0f} MHz)")
print("done flag:", "OK" if done == 0x454E4F44 else f"MISSING (0x{done:08x})")
if done != 0x454E4F44:
    sys.exit(1)
PY
echo
echo "openocd log: $LOG"
