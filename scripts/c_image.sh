#!/usr/bin/env bash
#
# C-side image tooling for the FlashDB <-> Rust on-flash compatibility proof.
#
#   scripts/c_image.sh write <out.bin> [wg] [opt]   # build deterministic image
#   scripts/c_image.sh read  <in.bin>  [wg] [opt]   # mount image, print summary
#   scripts/c_image.sh selfcheck [wg] [opt]         # write twice, read back, diff
#
# wg:  8 (default), 1, 32, 64, 128 or 256 -- must match the FDB_WRITE_GRAN the
#      tool is compiled with (all six values FlashDB-C supports)
# opt: O2 (default) or Os
#
# Image format: raw 32768 bytes
#   [0x0000,0x4000) KV partition "fdb_kvdb1" (4 x 4096-byte sectors)
#   [0x4000,0x8000) TS partition "fdb_tsdb1" (4 x 4096-byte sectors)
#
# The deterministic workload and the exact summary text format are documented
# at the top of harness/c/image_tool.c. `write` prints the summary to stdout
# and the concrete flash-op counts of the workload to stderr (KVFLASH/TSFLASH/
# IMAGEFLASH lines).
#
# Cross-checking with the Rust harness:
#   scripts/c_image.sh write /tmp/c_wg8.bin 8
#   <rust image reader> /tmp/c_wg8.bin 8      # must print the identical summary
#   <rust image writer> /tmp/rust_wg8.bin 8
#   scripts/c_image.sh read /tmp/rust_wg8.bin 8
#
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
CC=${CC:-cc}
REF="$ROOT/reference/FlashDB"
HARNESS="$ROOT/harness/c"
RESULTS="$ROOT/harness/results"
BUILD="$RESULTS/c_build"

usage() {
    sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'
}

if [ $# -lt 1 ]; then
    usage
    exit 2
fi

CMD=$1
shift

case "$CMD" in
    write|read)
        if [ $# -lt 1 ]; then
            usage
            exit 2
        fi
        PATH_ARG=$1; shift
        WG=${1:-8}; [ $# -gt 0 ] && shift
        OPT=${1:-O2}
        ;;
    selfcheck)
        WG=${1:-8}; [ $# -gt 0 ] && shift
        OPT=${1:-O2}
        PATH_ARG=""
        ;;
    -h|--help)
        usage
        exit 0
        ;;
    *)
        echo "unknown command '$CMD'" >&2
        usage >&2
        exit 2
        ;;
esac

case "$WG" in
    8|1|32|64|128|256) ;;
    *) echo "wg must be 1, 8, 32, 64, 128 or 256 (got '$WG')" >&2; exit 2 ;;
esac
case "$OPT" in
    O2|Os) ;;
    *) echo "opt must be O2 or Os (got '$OPT')" >&2; exit 2 ;;
esac

SRCS=(
    "$HARNESS/image_tool.c"
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

build_image_tool() {
    local dir="$BUILD/image-wg${WG}-${OPT}"
    local bin="$dir/image_tool"

    if [ -x "$bin" ] && [ "$bin" -nt "$HARNESS/image_tool.c" ] && \
       [ "$bin" -nt "$HARNESS/bench_common.c" ] && [ "$bin" -nt "$HARNESS/bench_kv.c" ] && \
       [ "$bin" -nt "$HARNESS/bench_ts.c" ] && [ "$bin" -nt "$HARNESS/sim_flash.c" ]; then
        printf '%s\n' "$bin"
        return 0
    fi
    mkdir -p "$dir"
    echo "== build image_tool wg=${WG} opt=${OPT} ==" >&2
    # `-std=c99` hides the POSIX clock API on glibc (Linux/WSL), and
    # harness/c/bench_common.c references clock_gettime(CLOCK_MONOTONIC) even
    # though the image tool never measures time; request POSIX.1b explicitly
    # (the same flags scripts/c_bench.sh uses) so the tool also builds with
    # WSL gcc as well as Windows clang.
    "$CC" -std=c99 -D_POSIX_C_SOURCE=199309L "-$OPT" -Wall -Wextra -Wno-unused-parameter \
        -DFDB_WRITE_GRAN="$WG" -DBENCH_OPT="$OPT" \
        "-DBENCH_BUILD_FLAGS=\"-std=c99 -D_POSIX_C_SOURCE=199309L -$OPT\"" "-DBENCH_HOST=\"$(uname -sm)\"" \
        "${INCLUDES[@]}" "${SRCS[@]}" \
        -o "$bin"
    printf '%s\n' "$bin"
}

case "$CMD" in
    write)
        BIN=$(build_image_tool)
        "$BIN" write "$PATH_ARG" "$WG"
        ;;
    read)
        BIN=$(build_image_tool)
        "$BIN" read "$PATH_ARG" "$WG"
        ;;
    selfcheck)
        BIN=$(build_image_tool)
        dir="$BUILD/image-roundtrip-wg${WG}-${OPT}"
        mkdir -p "$dir"
        img1="$dir/image_a.bin"
        img2="$dir/image_b.bin"
        sum_w="$dir/summary_written.txt"
        sum_r="$dir/summary_read.txt"
        stats="$dir/flash_stats.txt"

        "$BIN" write "$img1" "$WG" > "$sum_w" 2> "$stats"
        "$BIN" write "$img2" "$WG" > /dev/null 2> "$stats"
        if ! cmp -s "$img1" "$img2"; then
            echo "FAIL: image write is not deterministic" >&2
            exit 1
        fi
        "$BIN" read "$img1" "$WG" > "$sum_r" 2>/dev/null
        if ! diff -u "$sum_w" "$sum_r" > "$dir/summary.diff"; then
            echo "FAIL: write/read summary mismatch (see $dir/summary.diff)" >&2
            cat "$dir/summary.diff" >&2
            exit 1
        fi

        img_size=$(wc -c < "$img1" | tr -d ' ')
        if [ "$img_size" != "32768" ]; then
            echo "FAIL: image size $img_size != 32768" >&2
            exit 1
        fi
        if command -v shasum >/dev/null 2>&1; then
            sha=$(shasum -a 256 "$img1" | awk '{print $1}')
        elif command -v sha256sum >/dev/null 2>&1; then
            sha=$(sha256sum "$img1" | awk '{print $1}')
        else
            sha="(no sha256 tool)"
        fi

        echo "PASS: image round-trip wg=${WG} opt=${OPT}"
        echo "  image:    $img1 ($img_size bytes)"
        echo "  sha256:   $sha"
        echo "  kvc/t":   "$(tail -1 "$sum_r")"
        echo "  flash:"
        sed 's/^/    /' "$stats"
        ;;
esac
