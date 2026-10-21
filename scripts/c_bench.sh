#!/usr/bin/env bash
#
# Build and run the C reference benchmark harness for the FlashDB -> Rust
# comparison protocol (harness/SCENARIOS.md).
#
#   scripts/c_bench.sh [--opt O2|Os|both] [--runs N] [--wg 8|1|both] [--out-dir DIR]
#
# Defaults: --opt O2 --runs 5 --wg both --out-dir harness/results
#
# Results (one JSON document per (write granularity, opt level)):
#   harness/results/c_wg8_host.json    FDB_WRITE_GRAN=8, -O2
#   harness/results/c_wg1_host.json    FDB_WRITE_GRAN=1, -O2
#   harness/results/c_wg8_host_os.json FDB_WRITE_GRAN=8, -Os (with --opt Os/both)
#   harness/results/c_wg1_host_os.json FDB_WRITE_GRAN=1, -Os
#
# Only cc (Apple clang on macOS), the C99 system headers and the reference
# FlashDB sources under reference/FlashDB are required. Reference sources are
# compiled as-is (never modified); the harness supplies fdb_cfg.h, fal_cfg.h and
# the RAM FAL simulator.
#
# Compile flags (documented; also recorded in each JSON under config.cflags):
#   -std=c99 -O2 -Wall -Wextra -Wno-unused-parameter
#   -D FDB_WRITE_GRAN=<8|1>              protocol variant (fdb_cfg.h never pins it)
#   -I harness/c                          fdb_cfg.h, fal_cfg.h, harness headers
#   -I reference/FlashDB/inc              flashdb.h, fdb_def.h, fdb_low_lvl.h
#   -I reference/FlashDB/port/fal/inc     fal.h, fal_def.h
#   no -DFDB_DEBUG_ENABLE                 logging stays disabled (errors -> stderr)
#
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
CC=${CC:-cc}
REF="$ROOT/reference/FlashDB"
HARNESS="$ROOT/harness/c"
RESULTS="$ROOT/harness/results"
BUILD="$RESULTS/c_build"

OPT=O2
RUNS=5
WG_LIST="8 1"
OUT_DIR="$RESULTS"

while [ $# -gt 0 ]; do
    case "$1" in
        --opt) OPT=${2:?--opt needs a value}; shift 2 ;;
        --runs) RUNS=${2:?--runs needs a value}; shift 2 ;;
        --wg) WG_LIST=$2; shift 2 ;;
        --out-dir) OUT_DIR=$2; shift 2 ;;
        -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

case "$OPT" in
    O2) OPT_LIST="O2" ;;
    Os) OPT_LIST="Os" ;;
    both) OPT_LIST="O2 Os" ;;
    *) echo "invalid --opt '$OPT' (use O2, Os or both)" >&2; exit 2 ;;
esac

mkdir -p "$OUT_DIR" "$BUILD"

FLAGS="-std=c99 -Wno-unused-parameter"
HOST_STR="$(uname -sm)"
BASE_SRCS=(
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

build_and_run() {
    local wg=$1 opt=$2
    local dir="$BUILD/wg${wg}-${opt}"
    local bin="$dir/c_bench"
    local suffix=""
    local json

    [ "$opt" = "Os" ] && suffix="_os"
    json="$OUT_DIR/c_wg${wg}_host${suffix}.json"
    mkdir -p "$dir"

    echo "== build wg=${wg} opt=${opt} =="
    # shellcheck disable=SC2086
    "$CC" ${FLAGS} "-$opt" \
        -DFDB_WRITE_GRAN="$wg" \
        -DBENCH_OPT="$opt" \
        "-DBENCH_BUILD_FLAGS=\"-std=c99 -$opt $FLAGS -DFDB_WRITE_GRAN=$wg\"" \
        "-DBENCH_HOST=\"$HOST_STR\"" \
        "${INCLUDES[@]}" \
        "${BASE_SRCS[@]}" \
        -o "$bin"

    echo "== run   wg=${wg} opt=${opt} runs=${RUNS} -> ${json} =="
    "$bin" --mode bench --runs "$RUNS" --out "$json" >/dev/null
}

for opt in $OPT_LIST; do
    for wg in $WG_LIST; do
        build_and_run "$wg" "$opt"
    done
done

# Print a compact summary and validate the JSON with Python when available.
if command -v python3 >/dev/null 2>&1; then
    python3 - "$OUT_DIR" <<'PY'
import json
import os
import sys

out_dir = sys.argv[1]
files = []
for opt in ("O2", "Os"):
    for wg in ("8", "1"):
        name = f"c_wg{wg}_host{'_os' if opt == 'Os' else ''}.json"
        path = os.path.join(out_dir, name)
        if os.path.exists(path):
            files.append((opt, wg, path))

for opt, wg, path in files:
    with open(path) as f:
        doc = json.load(f)
    print(f"\n== {os.path.basename(path)} (impl={doc['impl']} opt={doc['opt']} wg={doc['write_gran']}) ==")
    print(f"{'scenario':22s} {'ops':>6s} {'median_ns':>12s} {'ns/op':>10s} "
          f"{'writes':>7s} {'wbytes':>9s} {'erases':>7s} {'read_b':>9s}")
    for s in doc["scenarios"]:
        fl = s["flash"]
        print(f"{s['name']:22s} {s['ops']:6d} {s['total_ns_median']:12d} "
              f"{s['ns_per_op']:10.2f} {fl['writes']:7d} {fl['write_bytes']:9d} "
              f"{fl['erases']:7d} {fl['read_bytes']:9d}")
PY
else
    echo "note: python3 not found; skipping summary table (JSON files are still written)" >&2
fi
