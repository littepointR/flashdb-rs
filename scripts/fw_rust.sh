#!/usr/bin/env bash
#
# FlashDB Rust rewrite -- Rust firmware build + QEMU measurement.
#
# Builds the bare-metal Rust measurement firmware in harness/firmware/rust/
# (real `flashdb` crate + a small RAM NOR driver placed in the detached
# .sim_flash region) for the ARM MPS2-AN385 board (Cortex-M3), then runs it
# under qemu-system-arm and prints the same semihosting RESULT line as
# scripts/fw_c.sh: ROM/RAM section sizes, worst-case stack, workload checksums.
#
# The Rust TSDB API is not integrated yet; the TSDB part of the workload is a
# placeholder that produces the same deterministic payload stream as the C
# firmware, so ts_checksum is already directly comparable.
#
# Usage:
#   scripts/fw_rust.sh                 # release profile (opt-level="s", LTO)
#   scripts/fw_rust.sh size            # "size" profile (opt-level="z", LTO)
#   scripts/fw_rust.sh --build-only    # build + sizes only, no QEMU
#   scripts/fw_rust.sh --clean         # remove cargo target dir first
#   scripts/fw_rust.sh --help
#
# Environment overrides:
#   CARGO=cargo  RUSTC=rustc  QEMU=qemu-system-arm
#   QEMU_TIMEOUT=30  QEMU_MACHINE=mps2-an385  QEMU_CPU=cortex-m3
#
# Exit status: 0 when the firmware ran and printed a RESULT line with rc=0.
#
set -u

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
FW="$ROOT/harness/firmware"
RUST_DIR="$FW/rust"
BUILD="$FW/build/rust"
ELF_DIR="$BUILD"
LINKER="$FW/common/mps2_an385.ld"

CARGO="${CARGO:-cargo}"
RUSTC="${RUSTC:-rustc}"
QEMU="${QEMU:-qemu-system-arm}"
QEMU_MACHINE="${QEMU_MACHINE:-mps2-an385}"
QEMU_CPU="${QEMU_CPU:-cortex-m3}"
QEMU_TIMEOUT="${QEMU_TIMEOUT:-30}"
TARGET="thumbv7m-none-eabi"

die() {
  echo "fw_rust.sh: ERROR: $*" >&2
  exit 1
}

usage() {
  sed -n '2,31p' "$0" | sed 's/^# \{0,1\}//'
  exit 0
}

PROFILE="release"
BUILD_ONLY=0
CLEAN=0
for arg in "$@"; do
  case "$arg" in
    --help|-h) usage ;;
    --build-only) BUILD_ONLY=1 ;;
    --clean) CLEAN=1 ;;
    release) PROFILE="release" ;;
    size) PROFILE="size" ;;
    *) die "unknown argument: $arg (try --help)" ;;
  esac
done

command -v "$CARGO" >/dev/null 2>&1 || die "cargo not found: $CARGO (set CARGO=...)"
command -v "$RUSTC" >/dev/null 2>&1 || die "rustc not found: $RUSTC (set RUSTC=...)"
command -v "$QEMU" >/dev/null 2>&1 || die "QEMU not found: $QEMU (set QEMU=...)"
[ -d "$RUST_DIR" ] || die "Rust firmware source dir not found: $RUST_DIR"
[ -f "$LINKER" ] || die "linker script not found: $LINKER"

if ! "$RUSTC" --print target-libdir --target "$TARGET" >/dev/null 2>&1; then
  die "Rust target $TARGET is not installed.
       Install it with: rustup target add $TARGET"
fi

case "$PROFILE" in
  release) PROFILE_ARGS=(--release) ;;
  size)    PROFILE_ARGS=(--profile size) ;;
  *) die "unsupported profile: $PROFILE (use release or size)" ;;
esac

if [ "$CLEAN" = "1" ]; then
  rm -rf "$BUILD"
fi
mkdir -p "$ELF_DIR"

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
echo "== Rust flashdb firmware ($PROFILE profile) =="
echo "   rustc    : $("$RUSTC" --version)"
echo "   cargo    : $("$CARGO" --version)"
echo "   target   : $TARGET / Cortex-M3 / MPS2-AN385 (qemu: $QEMU_MACHINE)"
echo "   crate    : crates/flashdb (real Rust rewrite), crates/flashdb-sim not used"
echo "              in the firmware image: the RAM NOR driver lives in main.rs"
echo "   output   : $ELF_DIR/fw_rust_$PROFILE.elf"
echo

BUILD_LOG="$BUILD/cargo.log"
(
  cd "$RUST_DIR" || exit 1
  CARGO_TARGET_DIR="$BUILD/target" \
  CARGO_TARGET_THUMBV7M_NONE_EABI_LINKER="arm-none-eabi-gcc" \
  "$CARGO" rustc --bin fw-rust --target "$TARGET" --offline "${PROFILE_ARGS[@]}" -- \
      -C link-arg=-T"$LINKER" \
      -C link-arg=-nostartfiles \
      -C link-arg=-nostdlib \
      -C link-arg=-Wl,--gc-sections \
      -C link-arg=-lgcc
) >"$BUILD_LOG" 2>&1
BUILD_RC=$?

WARNINGS=$(grep -c '^warning' "$BUILD_LOG" 2>/dev/null || true)
if [ "$BUILD_RC" -ne 0 ]; then
  echo "cargo build failed:" >&2
  cat "$BUILD_LOG" >&2
  die "cargo build failed (see $BUILD_LOG)"
fi
echo "   cargo: ok, ${WARNINGS:-0} warning line(s) from dependencies (see $BUILD_LOG)"
echo

CARGO_ELF="$BUILD/target/$TARGET/$PROFILE/fw-rust"
[ -f "$CARGO_ELF" ] || die "expected binary not found: $CARGO_ELF"
ELF="$ELF_DIR/fw_rust_$PROFILE.elf"
cp "$CARGO_ELF" "$ELF"
SIZE="${SIZE:-arm-none-eabi-size}"
command -v "$SIZE" >/dev/null 2>&1 || die "size tool not found: $SIZE (set SIZE=...)"

# ---------------------------------------------------------------------------
# Size report
# ---------------------------------------------------------------------------
echo "-- ELF sections (arm-none-eabi-size -A) --"
"$SIZE" -A "$ELF"
echo
echo "   defined workloads: KVDB 64 set + 32 update + 1500 churn + 64 verified get"
echo "                      + full iterate (real flashdb crate); TSDB placeholder"
echo "                      200 append + 200 'iterate' (deterministic payloads)"
echo "   simulated NOR: 16 KiB KVDB partition in the .sim_flash NOLOAD region"
echo "                  (no TS partition until the real TSDB API is wired in)"
echo

if [ "$BUILD_ONLY" = "1" ]; then
  echo "build only: skipping QEMU"
  exit 0
fi

# ---------------------------------------------------------------------------
# Run under QEMU (semihosting console; no UART needed)
# ---------------------------------------------------------------------------
echo "-- QEMU run ($QEMU_MACHINE, timeout ${QEMU_TIMEOUT}s) --"
RUN_LOG="$BUILD/qemu.log"
TIMEOUT_CMD=()
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_CMD=(timeout "$QEMU_TIMEOUT")
elif command -v gtimeout >/dev/null 2>&1; then
  TIMEOUT_CMD=(gtimeout "$QEMU_TIMEOUT")
elif command -v perl >/dev/null 2>&1; then
  # macOS has no coreutils `timeout`; perl's alarm survives exec().
  TIMEOUT_CMD=(perl -e 'alarm shift; exec @ARGV' "$QEMU_TIMEOUT")
fi

if [ "${#TIMEOUT_CMD[@]}" -gt 0 ]; then
  "${TIMEOUT_CMD[@]}" "$QEMU" -M "$QEMU_MACHINE" -cpu "$QEMU_CPU" \
      -nographic -semihosting -kernel "$ELF" >"$RUN_LOG" 2>&1
  QEMU_RC=$?
else
  echo "   warning: no timeout wrapper available; running without timeout" >&2
  "$QEMU" -M "$QEMU_MACHINE" -cpu "$QEMU_CPU" \
      -nographic -semihosting -kernel "$ELF" >"$RUN_LOG" 2>&1
  QEMU_RC=$?
fi

cat "$RUN_LOG"
echo

if ! grep -q '^RESULT ' "$RUN_LOG"; then
  echo "fw_rust.sh: ERROR: firmware did not print a RESULT line (QEMU exit $QEMU_RC)." >&2
  echo "         A hang here usually means a hard fault or a missing --semihosting" >&2
  echo "         flag; the log above is the full output." >&2
  exit 1
fi

RESULT=$(grep -m1 '^RESULT ' "$RUN_LOG")
echo "$RESULT"
case "$RESULT" in
  *" rc=0"*) exit 0 ;;
  *) echo "fw_rust.sh: ERROR: workload reported a failure (see RESULT line)" >&2; exit 1 ;;
esac
