#!/usr/bin/env bash
#
# Thin wrapper around the Rust image tool (Rust counterpart of c_image.sh).
#
#   scripts/rust_image.sh write <out.bin> [wg]   # build deterministic image + summary
#   scripts/rust_image.sh read  <in.bin>  [wg]   # mount image (never format) + summary
#   scripts/rust_image.sh selfcheck [wg]         # write twice, compare, read back, sha256
#
# wg defaults to 8 (or 1); it must match the FDB_WRITE_GRAN the C reference was
# built with when cross-checking images.
#
# The tool itself lives in crates/flashdb-bench/src/bin/image_tool.rs; image
# format and workload are documented there and in harness/c/image_tool.c.
#
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN="$ROOT/target/release/image_tool"

usage() {
    sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
}

if [ $# -lt 1 ]; then
    usage
    exit 2
fi

case "$1" in
    -h|--help|help)
        usage
        exit 0
        ;;
    write|read|selfcheck) ;;
    *)
        echo "rust_image: unknown command '$1'" >&2
        usage >&2
        exit 2
        ;;
esac

mkdir -p "$ROOT/target"
(
    cd "$ROOT"
    cargo build --quiet --release -p flashdb-bench --bin image_tool
)
exec "$BIN" "$@"
