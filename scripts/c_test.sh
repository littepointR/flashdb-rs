#!/usr/bin/env bash
#
# C reference harness behavioural self-check (no Rust needed).
#
#   scripts/c_test.sh [--wg 8|1|both] [--runs N] [--quick]
#
# What it does:
#   1. builds harness/c/bench.c for each write granularity (-O2, C99),
#   2. runs `c_bench --mode check`, which asserts the behaviour the protocol
#      relies on: FAL/NOR simulator semantics (clear-only writes, erase 0xFF,
#      block-aligned erases, fault injection), KV set/get/delete/iterate/mount,
#      delete-missing-key returns FDB_KV_NAME_ERR, GC actually runs after
#      1000 updates, TS append/full/mount/clean/rollover/timestamp ordering,
#      checksum determinism, and no NOR bit-set or out-of-bounds accesses,
#   3. runs the image tool round-trip (`scripts/c_image.sh selfcheck`): write
#      the deterministic 32 KiB image twice, compare bytes, dump it, read it
#      back and verify the printed summary matches the writer's summary.
#
# Exit status: 0 when everything passed, 1 when any check failed.
#
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
CC=${CC:-cc}
REF="$ROOT/reference/FlashDB"
HARNESS="$ROOT/harness/c"
RESULTS="$ROOT/harness/results"
BUILD="$RESULTS/c_build"

WG_LIST="8 1"
FAILURES=0
CHECKS=0

while [ $# -gt 0 ]; do
    case "$1" in
        --wg) WG_LIST=$2; shift 2 ;;
        -h|--help) sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

mkdir -p "$BUILD"

SRCS=(
    "$HARNESS/bench.c"
    "$HARNESS/bench_common.c"
    "$HARNESS/bench_kv.c"
    "$HARNESS/bench_ts.c"
    "$HARNESS/sim_flash.c"
    "$REF/src/fdb.c"
    "$REF/src/fdb_kvdb.c"
    "$REF/src/fdb_tsdb.c"
    "$REF/src/fdb_utils.c"
    "$REF/port/fal/src/fal.c"
    "$REF/port/fal/src/fal_flash.c"
    "$REF/port/fal/src/fal_partition.c"
)
INCLUDES=(-I"$HARNESS" -I"$REF/inc" -I"$REF/port/fal/inc")

for wg in $WG_LIST; do
    dir="$BUILD/check-wg${wg}"
    bin="$dir/c_bench"
    mkdir -p "$dir"
    echo "== build check binary wg=${wg} =="
    "$CC" -std=c99 -O2 -Wall -Wextra -Wno-unused-parameter \
        -DFDB_WRITE_GRAN="$wg" -DBENCH_OPT=O2 \
        "-DBENCH_BUILD_FLAGS=\"-std=c99 -O2\"" "-DBENCH_HOST=\"$(uname -sm)\"" \
        "${INCLUDES[@]}" "${SRCS[@]}" \
        -o "$bin"

    echo "== run self-check wg=${wg} =="
    set +e
    "$bin" --mode check > "$dir/check.log" 2>&1
    rc=$?
    set -e
    grep -E '^(FAIL|==)' "$dir/check.log" || true
    CHECKS=$((CHECKS + 1))
    if [ $rc -ne 0 ]; then
        FAILURES=$((FAILURES + 1))
        echo "FAIL: self-check wg=${wg} (see $dir/check.log)" >&2
        tail -30 "$dir/check.log" >&2
    else
        echo "PASS: self-check wg=${wg} ($(grep -c '^PASS' "$dir/check.log") assertions)"
    fi

    echo "== run image round-trip wg=${wg} =="
    set +e
    "$ROOT/scripts/c_image.sh" selfcheck "$wg"
    rc=$?
    set -e
    CHECKS=$((CHECKS + 1))
    if [ $rc -ne 0 ]; then
        FAILURES=$((FAILURES + 1))
        echo "FAIL: image round-trip wg=${wg}" >&2
    else
        echo "PASS: image round-trip wg=${wg}"
    fi
done

echo
if [ $FAILURES -ne 0 ]; then
    echo "c_test: FAIL (${FAILURES}/${CHECKS} groups failed)"
    exit 1
fi
echo "c_test: OK (all ${CHECKS} groups passed)"
