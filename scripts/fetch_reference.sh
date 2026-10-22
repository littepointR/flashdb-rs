#!/usr/bin/env bash
#
# Fetch the FlashDB C reference sources used by the differential harnesses.
# The Rust crate itself has no dependency on this checkout; it is only needed
# for scripts/c_bench.sh, scripts/c_test.sh, scripts/rust_compat.sh and the C
# side of the firmware comparison.
#
# The harness pins upstream FlashDB 2.2.99 so results stay reproducible.
# Override FLASHDB_DIR / FLASHDB_REF / FLASHDB_URL for a local mirror.
#
set -euo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
DEST="${FLASHDB_DIR:-$ROOT/reference/FlashDB}"
REF="${FLASHDB_REF:-db0afd954ea0f11397e43f6baf3654979d446345}"
URL="${FLASHDB_URL:-https://github.com/armink/FlashDB}"

if [ -d "$DEST/.git" ]; then
    have=$(git -C "$DEST" rev-parse HEAD 2>/dev/null || true)
    if [ "$have" = "$REF" ]; then
        echo "reference/FlashDB: already at $REF (FlashDB 2.2.99)"
        exit 0
    fi
    echo "reference/FlashDB: updating ${have:-unknown} -> $REF"
else
    mkdir -p "$(dirname "$DEST")"
    git init -q "$DEST"
    git -C "$DEST" remote add origin "$URL"
fi

git -C "$DEST" fetch --quiet --depth 1 origin "$REF"
git -C "$DEST" checkout --quiet --detach FETCH_HEAD
echo "reference/FlashDB: ready at $(git -C "$DEST" rev-parse --short HEAD) (FlashDB 2.2.99)"
