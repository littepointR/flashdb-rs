#!/usr/bin/env bash
#
# FlashDB Rust rewrite -- C FlashDB baseline firmware build + QEMU measurement.
#
# Builds the UNMODIFIED C FlashDB (reference/FlashDB) together with the
# bare-metal measurement harness in harness/firmware/ for the ARM MPS2-AN385
# board (Cortex-M3), then runs it under qemu-system-arm and prints the single
# semihosting RESULT line with ROM/RAM section sizes, worst-case stack and
# deterministic workload checksums.
#
# `--board-u545` builds the same harness for the real NUCLEO-U545RE-Q
# (STM32U545RE, Cortex-M33) instead: RAM result buffer + DWT cycles
# (-DFDB_BOARD_U545, harness/firmware/u545re/board.ld).  No QEMU is run; flash
# and read back with harness/firmware/u545re/run_c.ps1 (Windows) or compare
# against the Rust image with harness/firmware/u545re/compare.ps1.
#
# Usage:
#   scripts/fw_c.sh                 # build + run (baseline build)
#   scripts/fw_c.sh --phases        # diagnostic build with per-phase stack peaks
#   scripts/fw_c.sh --symbols       # also print symbol-level size attribution
#   scripts/fw_c.sh --build-only    # build, print sizes, do not run QEMU
#   scripts/fw_c.sh --board-u545    # build the NUCLEO-U545RE-Q image (no QEMU)
#   scripts/fw_c.sh --clean         # remove build products first
#   scripts/fw_c.sh --help
#
# Environment overrides:
#   CC=arm-none-eabi-gcc   SIZE=arm-none-eabi-size   QEMU=qemu-system-arm
#   QEMU_TIMEOUT=30        QEMU_MACHINE=mps2-an385   QEMU_CPU=cortex-m3
#
# Exit status: 0 when the firmware ran and printed a RESULT line with rc=0.
#
set -u

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
FW="$ROOT/harness/firmware"
REF="$ROOT/reference/FlashDB"
BUILD="$FW/build/c"
OBJ="$BUILD/obj"
ELF="$BUILD/fw_c.elf"
MAP="$BUILD/fw_c.map"

BOARD=0
MCPU=cortex-m3
LD="$FW/common/mps2_an385.ld"

CC="${CC:-arm-none-eabi-gcc}"
SIZE="${SIZE:-arm-none-eabi-size}"
QEMU="${QEMU:-qemu-system-arm}"
QEMU_MACHINE="${QEMU_MACHINE:-mps2-an385}"
QEMU_CPU="${QEMU_CPU:-cortex-m3}"
QEMU_TIMEOUT="${QEMU_TIMEOUT:-30}"

# FlashDB configuration for the baseline (matches harness/firmware/c/include):
#   FAL mode, NOR flash (byte programmable => FDB_WRITE_GRAN=8), KVDB + TSDB,
#   library logging compiled out, no file mode.
CFLAGS=(
  -mthumb -mfloat-abi=soft
  -Os -std=gnu11 -Wall
  -ffunction-sections -fdata-sections -fno-common
  -fno-builtin -fno-stack-protector -ffreestanding
  -fno-asynchronous-unwind-tables -fno-unwind-tables
  -DFDB_WRITE_GRAN=8
  -I"$FW/common"
  -I"$FW/c/include"
  -I"$REF/inc"
  -I"$REF/port/fal/inc"
)

# FlashDB + FAL sources (upstream, never modified).
LIB_SRCS=(
  "$REF/src/fdb.c"
  "$REF/src/fdb_kvdb.c"
  "$REF/src/fdb_tsdb.c"
  "$REF/src/fdb_utils.c"
  "$REF/port/fal/src/fal.c"
  "$REF/port/fal/src/fal_flash.c"
  "$REF/port/fal/src/fal_partition.c"
)

# Harness sources (startup, semihosting, stack scan, workload, RAM NOR sim).
HARNESS_SRCS=(
  "$FW/common/startup_armv7m.S"
  "$FW/common/semihost.c"
  "$FW/common/stack.c"
  "$FW/c/main.c"
  "$FW/c/fal_ram_sim.c"
  "$FW/c/libc_stubs.c"
)

die() {
  echo "fw_c.sh: ERROR: $*" >&2
  exit 1
}

usage() {
  cat >&2 <<'EOF'
Usage: scripts/fw_c.sh [options]

  --build-only        build, print sizes, do not run QEMU
  --phases            diagnostic build with per-phase stack peaks
  --symbols           print symbol-level size attribution
  --board-u545        build the NUCLEO-U545RE-Q image (Cortex-M33, no QEMU)
  --clean             remove build products first
  -h, --help          show this help

Environment: CC, SIZE, QEMU, QEMU_TIMEOUT, QEMU_MACHINE, QEMU_CPU.
Requires reference/FlashDB (scripts/fetch_reference.sh) and arm-none-eabi-gcc.
EOF
  exit 0
}

BUILD_ONLY=0
CLEAN=0
PHASES=0
SYMBOLS=0
for arg in "$@"; do
  case "$arg" in
    --help|-h) usage ;;
    --build-only) BUILD_ONLY=1 ;;
    --clean) CLEAN=1 ;;
    --phases) PHASES=1 ;;
    --symbols) SYMBOLS=1 ;;
    --board-u545) BOARD=1 ;;
    *) die "unknown argument: $arg (try --help)" ;;
  esac
done

if [ "$BOARD" = "1" ]; then
  # Real-board image: Cortex-M33, board linker script, RAM result buffer and
  # DWT cycle counters instead of semihosting (see common/board_u545.c).
  BUILD="$FW/build/c_u545"
  OBJ="$BUILD/obj"
  ELF="$BUILD/fw_c_u545.elf"
  MAP="$BUILD/fw_c_u545.map"
  MCPU=cortex-m33
  LD="$FW/u545re/board.ld"
  CFLAGS+=(-DFDB_BOARD_U545)
  HARNESS_SRCS+=("$FW/common/board_u545.c")
fi
CFLAGS+=(-mcpu="$MCPU")

if [ "$PHASES" = "1" ]; then
  # Diagnostic build: adds the per-phase stack-attribution code (common/stack.c,
  # c/main.c).  It is NOT the baseline: the extra code changes ROM/stack, so
  # use the default build for the headline numbers.
  CFLAGS+=(-DHARNESS_STACK_PHASES=1)
fi

command -v "$CC" >/dev/null 2>&1 || die "C compiler not found: $CC (set CC=...)"
command -v "$SIZE" >/dev/null 2>&1 || die "size tool not found: $SIZE (set SIZE=...)"
if [ "$BOARD" = "0" ]; then
  command -v "$QEMU" >/dev/null 2>&1 || die "QEMU not found: $QEMU (set QEMU=...)"
fi
[ -d "$REF/src" ] || die "reference FlashDB clone not found at $REF"

if [ "$CLEAN" = "1" ]; then
  rm -rf "$BUILD"
fi
mkdir -p "$OBJ"

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
echo "== C FlashDB baseline firmware =="
echo "   compiler : $("$CC" --version | head -1)"
if [ "$BOARD" = "1" ]; then
  echo "   target   : cortex-m33 / thumbv8m / NUCLEO-U545RE-Q (RAM output + DWT)"
else
  echo "   target   : cortex-m3 / thumbv7m / MPS2-AN385 (qemu: $QEMU_MACHINE)"
fi
echo "   config   : FDB_WRITE_GRAN=8, FAL mode, KVDB+TSDB, logging compiled out"
if [ "$PHASES" = "1" ]; then
  echo "   mode     : DIAGNOSTIC (per-phase stack attribution; not the baseline build)"
fi
echo "   output   : $ELF"
echo

COMPILE_FAIL=0
OBJS=()
for src in "${LIB_SRCS[@]}" "${HARNESS_SRCS[@]}"; do
  name=$(basename "${src%.*}")
  obj="$OBJ/$name.o"
  if ! "$CC" "${CFLAGS[@]}" -c "$src" -o "$obj" 2>"$BUILD/compile.log"; then
    echo "   FAILED to compile: $src" >&2
    cat "$BUILD/compile.log" >&2
    COMPILE_FAIL=1
    continue
  fi
  if [ -s "$BUILD/compile.log" ]; then
    echo "   warning(s) from $(basename "$src"):" >&2
    sed 's/^/     /' "$BUILD/compile.log" >&2
  fi
  OBJS+=("$obj")
done
[ "$COMPILE_FAIL" = "0" ] || die "compilation failed"

if ! "$CC" -mcpu="$MCPU" -mthumb -mfloat-abi=soft -nostdlib -nostartfiles \
      -Wl,--gc-sections -Wl,-Map="$MAP" -T"$LD" \
      -o "$ELF" "${OBJS[@]}" -lgcc 2>"$BUILD/link.log"; then
  cat "$BUILD/link.log" >&2
  die "link failed (map file: $MAP)"
fi
[ -s "$BUILD/link.log" ] && sed 's/^/   ld: /' "$BUILD/link.log"

# ---------------------------------------------------------------------------
# Size report
# ---------------------------------------------------------------------------
echo "-- ELF sections (arm-none-eabi-size -A) --"
"$SIZE" -A "$ELF"
echo

# Library-only ROM estimate: sum of allocated sections of the FlashDB/FAL
# objects BEFORE --gc-sections removes unreferenced functions.  The final ELF
# is authoritative; this isolates the library from the harness.
echo "-- FlashDB/FAL objects before --gc-sections (library ROM upper bound) --"
LIB_ROM_TOTAL=0
LIB_RAM_TOTAL=0
for src in "${LIB_SRCS[@]}"; do
  obj="$OBJ/$(basename "${src%.*}").o"
  rom=$(awk '$1 ~ /^\.(text|rodata|data)/ { s += $2 } END { print s + 0 }' \
        <("$SIZE" -A "$obj"))
  ram=$(awk '$1 ~ /^\.bss/ { s += $2 } END { print s + 0 }' <("$SIZE" -A "$obj"))
  printf '   %-18s ROM=%5s B  bss=%4s B\n' "$(basename "$obj")" "$rom" "$ram"
  LIB_ROM_TOTAL=$((LIB_ROM_TOTAL + rom))
  LIB_RAM_TOTAL=$((LIB_RAM_TOTAL + ram))
done
echo "   --------------------------------------------------"
printf '   %-18s ROM=%5s B  bss=%4s B (upper bound)\n' "library total" "$LIB_ROM_TOTAL" "$LIB_RAM_TOTAL"
echo
echo "   defined workloads: KVDB 64 set + 32 update + 1500 churn + 64 verified get"
echo "                      + full iterate; TSDB 200 append + full iterate"
echo "   simulated NOR: 16 KiB KVDB partition + 16 KiB TSDB partition"
echo
if [ "$SYMBOLS" = "1" ]; then
  command -v python3 >/dev/null 2>&1 || die "python3 is required for --symbols"
  python3 "$FW/tools/symbol_report.py" "$ELF" --kind c --objdir "$OBJ" || die "symbol report failed"
  echo
fi

if [ "$BOARD" = "1" ]; then
  echo "   real-board image: flash + read back with"
  echo "     harness/firmware/u545re/run_c.ps1        (Windows)"
  echo "     harness/firmware/u545re/compare.ps1      (C vs Rust on identical silicon)"
  echo "   build only: no QEMU run for the NUCLEO-U545RE-Q image"
  exit 0
fi

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
  echo "fw_c.sh: ERROR: firmware did not print a RESULT line (QEMU exit $QEMU_RC)." >&2
  echo "         A hang here usually means a hard fault or a missing --semihosting" >&2
  echo "         flag; the log above is the full output." >&2
  exit 1
fi

RESULT=$(grep -m1 '^RESULT ' "$RUN_LOG")
echo "$RESULT"
case "$RESULT" in
  *" rc=0"*) exit 0 ;;
  *) echo "fw_c.sh: ERROR: workload reported a failure (see RESULT line)" >&2; exit 1 ;;
esac
