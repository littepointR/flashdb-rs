#!/usr/bin/env bash
#
# FlashDB-rs real-hardware test -- NUCLEO-U545RE-Q (STM32U545RE, Cortex-M33).
#
# Builds the Rust firmware with the `board-u545` feature (RAM result buffer +
# DWT cycle counters instead of semihosting), flashes it through the on-board
# ST-LINK/V3, runs the workload, then reads the result buffer back over SWD and
# prints it.
#
# Flash backends, in preference order for --tool auto:
#   probe-rs  preferred: implemented in the host tool, so it does not depend on
#             the probe's own flash loader or on the ST-LINK firmware revision.
#             This is the backend the board was verified with (see README.md).
#   pyocd     used when probe-rs is missing; may need a current ST-LINK firmware.
#   openocd   last resort; drives target reset, which the ST-LINK V3 firmware on
#             some boards does not like.
#
# Build: uses arm-none-eabi-gcc as the linker when available, otherwise links
# with the rust-lld that ships inside the Rust toolchain, so an ARM GCC install
# is not strictly required.
#
# Usage:
#   harness/firmware/u545re/run_rust.sh              # build + flash + run
#   harness/firmware/u545re/run_rust.sh --build-only # build only
#   harness/firmware/u545re/run_rust.sh --no-build   # reuse the ELF and run
#   ... --tool probe-rs|pyocd|openocd                # force a probe backend
#   ... --chip STM32U545RE                           # probe-rs chip name
#
# Environment: NUCLEO_TOOL, NUCLEO_CHIP, NUCLEO_TIMEOUT (seconds to wait for
# DONE, default 120).  Requirements: cargo/rustc with the
# thumbv8m.main-none-eabi target, python3, and one flash backend.
#
# On Windows without bash, use run_rust.ps1 instead (same firmware and output).
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
CHIP="${NUCLEO_CHIP:-STM32U545RE}"
DONE_TIMEOUT="${NUCLEO_TIMEOUT:-120}"

usage() {
    cat >&2 <<'EOF'
Usage: harness/firmware/u545re/run_rust.sh [options]

  --build-only        build the ELF and print its section sizes, do not flash
  --no-build          reuse an existing ELF and go straight to flash + run
  --tool <backend>    probe-rs (default when installed), pyocd or openocd
  --chip <name>       probe-rs chip name (default STM32U545RE)
  -h, --help          show this help

Environment: NUCLEO_TOOL, NUCLEO_CHIP, NUCLEO_TIMEOUT (default 120 s).
Requires cargo/rustc with the thumbv8m.main-none-eabi target, python3 and one
flash backend.  On Windows without bash, use run_rust.ps1 instead.
EOF
    exit 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        --build-only) BUILD_ONLY=1; shift ;;
        --no-build)   DO_BUILD=0; shift ;;
        --tool)       TOOL="${2:?--tool needs a value}"; shift 2 ;;
        --chip)       CHIP="${2:?--chip needs a value}"; shift 2 ;;
        -h|--help)    usage ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

die() { echo "run_rust.sh: ERROR: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
pick() { for c in "$@"; do if have "$c"; then printf '%s' "$c"; return 0; fi; done; return 1; }

# Windows/Git-Bash friendliness: cargo, rustc and probe-rs are native Windows
# binaries there, so hand them native paths when cygpath is available.
winpath() {
    if have cygpath; then cygpath -w "$1"; else printf '%s' "$1"; fi
}

PY=$(pick python3 python) || die "python3 not found (needed to parse the result buffer)"

# ---------------------------------------------------------------------------
# Toolchain detection (ARM GCC optional)
# ---------------------------------------------------------------------------
have cargo || die "cargo not found"
have rustc || die "rustc not found"
[ -d "$RUST_DIR" ] || die "Rust firmware source dir not found: $RUST_DIR"
[ -f "$BOARD/board.ld" ] || die "linker script not found: $BOARD/board.ld"

if ! rustc --print target-libdir --target "$TARGET" >/dev/null 2>&1; then
    die "Rust target $TARGET is not installed. Install it with: rustup target add $TARGET"
fi

LINK_ARGS=()
if have arm-none-eabi-gcc; then
    LINKER=arm-none-eabi-gcc
    LINK_ARGS=(-C link-arg=-nostartfiles -C link-arg=-nostdlib \
               -C link-arg=-Wl,--gc-sections -C link-arg=-lgcc)
else
    # No ARM GCC: link the no_std image with the rust-lld bundled in the
    # toolchain.  compiler_builtins provides the mem* helpers, so -lgcc is not
    # needed; --gc-sections goes to LLD directly.
    SYSROOT=$(rustc --print sysroot)
    HOST=$(rustc -vV | sed -n 's/^host: //p')
    if [ -x "$SYSROOT/lib/rustlib/$HOST/bin/rust-lld" ]; then
        LINKER="$SYSROOT/lib/rustlib/$HOST/bin/rust-lld"
    elif [ -x "$SYSROOT/lib/rustlib/$HOST/bin/rust-lld.exe" ]; then
        LINKER="$SYSROOT/lib/rustlib/$HOST/bin/rust-lld.exe"
    else
        LINKER=$(pick rust-lld) || die "arm-none-eabi-gcc not found and rust-lld is missing from the Rust toolchain"
    fi
    LINK_ARGS=(-C link-arg=--gc-sections)
fi

NM=$(pick arm-none-eabi-nm llvm-nm) || die "no symbol tool found (need arm-none-eabi-nm or llvm-nm)"

if [ "$DO_BUILD" = "1" ]; then
    echo "== building Rust firmware for $TARGET (board-u545) =="
    echo "   linker: $LINKER"
    mkdir -p "$OUT"
    BUILD_LOG="$OUT/cargo.log"
    if ! (
        cd "$RUST_DIR"
        export CARGO_TARGET_DIR="$(winpath "$OUT/target")"
        export CARGO_TARGET_THUMBV8M_MAIN_NONE_EABI_LINKER="$LINKER"
        cargo rustc --bin fw-rust --target "$TARGET" --release --offline \
            --features board-u545 -- \
            "${LINK_ARGS[@]}" \
            -C "link-arg=-T$(winpath "$BOARD/board.ld")"
    ) >"$BUILD_LOG" 2>&1; then
        echo "cargo build failed:" >&2
        tail -40 "$BUILD_LOG" >&2
        die "cargo build failed (full log: $BUILD_LOG)"
    fi
    WARNINGS=$(grep -c '^warning' "$BUILD_LOG" 2>/dev/null || true)
    echo "   cargo: ok, ${WARNINGS:-0} warning line(s) (see $BUILD_LOG)"
    cp "$OUT/target/$TARGET/release/fw-rust" "$ELF"
    if have llvm-size; then
        llvm-size -A "$ELF" | sed -n '2,8p'
    elif have arm-none-eabi-size; then
        arm-none-eabi-size -A "$ELF" | sed -n '2,8p'
    fi
    echo
fi

[ -f "$ELF" ] || die "missing $ELF (run without --no-build)"

# ---------------------------------------------------------------------------
# Symbols: BOARD_OUT is where the firmware leaves its output (defined by board.ld)
# ---------------------------------------------------------------------------
sym_addr() {
    "$NM" "$(winpath "$ELF")" | awk -v s="$1" '$3==s {print $1; found=1} END {exit !found}'
}
ESTACK=$(sym_addr _estack) || die "could not resolve _estack"
OUT_ADDR=$(sym_addr BOARD_OUT) || die "could not resolve BOARD_OUT"
RESET=$(sym_addr Reset_Handler) || die "could not resolve Reset_Handler"
# Entry point: Reset_Handler with the Thumb bit set (0x...40 -> 0x...41).
ENTRY=$(printf '0x%x' $((0x$RESET + 1)))
# BoardOut = { done: u32, len: u32, buf: [u8; 4096] }
OUT_SIZE=$((8 + 4096))

if [ "$TOOL" = "auto" ]; then
    if have probe-rs; then TOOL=probe-rs
    elif have pyocd; then TOOL=pyocd
    else TOOL=openocd
    fi
fi

echo "== flash + run on NUCLEO-U545RE-Q ($TOOL) =="
echo "   entry    : $(printf '0x%08x' $((0x$RESET + 1)))"
echo "   estack   : 0x$ESTACK"
echo "   BOARD_OUT: 0x$OUT_ADDR (0x$(printf '%x' "$OUT_SIZE") bytes)"
echo

if [ "$BUILD_ONLY" = "1" ]; then
    echo "build only: skipping flash/run"
    exit 0
fi

LOG="$OUT/$TOOL.log"
DUMP="$OUT/board_out.bin"
rm -f "$DUMP" "$LOG"
FLASH_OK=1

run_probe_rs() {
    have probe-rs || { echo "probe-rs not found" >&2; return 1; }
    # probe-rs implements STM32U5 programming in the host, so this works with
    # the factory ST-LINK firmware of the board.
    if ! probe-rs download --chip "$CHIP" --verify --reset \
            --log-file "$(winpath "$LOG")" "$(winpath "$ELF")" >"$OUT/probe-rs.out" 2>&1; then
        return 1
    fi
    # The workload runs at the STM32U5 reset clock (MSIS 4 MHz) and takes a few
    # seconds, so poll the DONE flag instead of guessing a fixed delay.
    local t0 now v
    t0=$(date +%s)
    while :; do
        v=$(probe-rs read --chip "$CHIP" b32 "0x$OUT_ADDR" 1 2>/dev/null || true)
        case "$v" in *454e4f44*) break ;; esac
        now=$(date +%s)
        if [ $((now - t0)) -ge "$DONE_TIMEOUT" ]; then
            echo "timed out waiting for the DONE flag (${DONE_TIMEOUT}s)" >&2
            return 1
        fi
        sleep 1
    done
    probe-rs read --chip "$CHIP" b8 "0x$OUT_ADDR" "$OUT_SIZE" \
        -o "$(winpath "$DUMP")" -f binary >>"$OUT/probe-rs.out" 2>&1
}

run_pyocd() {
    have pyocd || return 1
    pyocd flash -t stm32u545retxq -f 1m -e auto "$(winpath "$ELF")" >"$LOG" 2>&1 || return 1
    # Programming resets the target, which starts the image from 0x08000000.
    # The 4 MHz reset clock makes the workload take ~6.5 s before DONE is set.
    sleep 9
    pyocd commander -t stm32u545retxq -c "halt" \
        -c "savemem 0x$OUT_ADDR 0x$(printf '%x' "$OUT_SIZE") $(winpath "$DUMP")" -c "quit" >>"$LOG" 2>&1
}

run_openocd() {
    have openocd || { echo "openocd not found" >&2; return 1; }
    # All target events are overridden and the reset line is only asserted
    # through the flash algorithm: the ST-LINK V3 firmware on this board can hang
    # when OpenOCD drives reset events.
    openocd -f interface/stlink.cfg -f target/stm32u5x.cfg \
        -c "adapter speed 200" \
        -c "stm32u5x.cpu configure -event examine-end {}" \
        -c "stm32u5x.cpu configure -event reset-init {}" \
        -c "stm32u5x.cpu configure -event reset-start {}" \
        -c "stm32u5x.cpu configure -work-area-phys 0x20030000 -work-area-size 0x4000" \
        -c "init" \
        -c "halt" \
        -c "flash write_image erase $(winpath "$ELF")" \
        -c "verify_image $(winpath "$ELF")" \
        -c "reg pc $ENTRY" \
        -c "reg sp 0x$ESTACK" \
        -c "reg msp 0x$ESTACK" \
        -c "mww 0xE000ED08 0x08000000" \
        -c "resume" \
        -c "runtest 9000" \
        -c "halt" \
        -c "dump_image $(winpath "$DUMP") 0x$OUT_ADDR 0x$(printf '%x' "$OUT_SIZE")" \
        -c "shutdown" >"$LOG" 2>&1
}

case "$TOOL" in
    probe-rs) run_probe_rs || FLASH_OK=0 ;;
    pyocd)
        if ! run_pyocd; then
            echo "pyOCD failed; falling back to OpenOCD" >&2
            LOG="$OUT/openocd.log"
            run_openocd || FLASH_OK=0
        fi
        ;;
    openocd) run_openocd || FLASH_OK=0 ;;
    *) die "unknown tool: $TOOL (use probe-rs, pyocd or openocd)" ;;
esac

if [ "$FLASH_OK" != "1" ]; then
    echo "flash/run failed with $TOOL; last lines of the log:" >&2
    tail -20 "$LOG" >&2
    echo >&2
    echo "If the ST-LINK cannot be opened or times out, unplug/replug the board's" >&2
    echo "USB cable (the probe firmware can hang after a failed flash) and retry." >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Parse the result buffer: done: u32, len: u32, then the NUL-terminated text
# ---------------------------------------------------------------------------
[ -f "$DUMP" ] || die "result buffer was not read back to $DUMP"
"$PY" - "$(winpath "$DUMP")" <<'PY'
import struct
import sys

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
hz = int(fields.get("core_hz", "0") or "0")
if hz > 0:
    for k in ("cycles_mount", "cycles_kv", "cycles_ts", "cycles_roll", "cycles_total"):
        if k in fields:
            c = int(fields[k])
            print(f"{k:14s} = {c:12d} cycles  ({c * 1e6 / hz:11.1f} us @ {hz/1e6:.0f} MHz)")
else:
    for k in ("cycles_mount", "cycles_kv", "cycles_ts", "cycles_roll", "cycles_total"):
        if k in fields:
            print(f"{k:14s} = {int(fields[k]):12d} cycles  (core_hz unknown)")
print("done flag:", "OK" if done == 0x454E4F44 else f"MISSING (0x{done:08x})")
if done != 0x454E4F44:
    sys.exit(1)
if fields.get("rc") != "0":
    print("workload reported a failure (rc != 0)", file=sys.stderr)
    sys.exit(1)
PY
echo
echo "$TOOL log: $LOG"
