#!/usr/bin/env bash
#
# Reproducible "smallest ROM" measurement for the Rust firmware:
# nightly toolchain + build-std=core + panic=immediate-abort + LLD ICF.
#
# This is a *size-only* comparison build; the baseline measurement in
# scripts/fw_rust.sh stays on stable Rust. It builds an isolated copy of
# harness/firmware so no repository file is modified.
#
# Usage:
#   scripts/fw_rust_nightly.sh                 # 256-entry CRC table
#   scripts/fw_rust_nightly.sh --small-crc     # 64-byte nibble CRC table
#
# Requires: rustup nightly with rust-src (rustup component add rust-src
# --toolchain nightly), arm-none-eabi-size, and the thumbv7m target sources
# (fetched automatically by build-std).
set -euo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
SRC="$ROOT/harness/firmware"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/flashdb-nightly.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

FEATURES=""
if [ "${1:-}" = "--small-crc" ]; then
    FEATURES=', features = ["small-crc"]'
fi

cp -R "$SRC/rust" "$WORK/rust"
cp -R "$SRC/common" "$WORK/common"

python3 - "$WORK/rust/Cargo.toml" "$ROOT/crates/flashdb" "$FEATURES" <<'PY'
import sys
path, crate, features = sys.argv[1], sys.argv[2], sys.argv[3]
s = open(path).read()
s = 'cargo-features = ["panic-immediate-abort"]\n' + s
s = s.replace('flashdb = { path = "../../../crates/flashdb" }',
              f'flashdb = {{ path = "{crate}"{features} }}')
s = s.replace('panic = "abort"', 'panic = "immediate-abort"')
open(path, 'w').write(s)
PY

echo "== nightly size build (panic=immediate-abort, build-std=core, LLD ICF) =="
echo "   features: ${FEATURES:-default (256-entry CRC)}"
cd "$WORK/rust"
CARGO_TARGET_DIR="$WORK/target" \
    cargo +nightly rustc --bin fw-rust \
    --target thumbv7m-none-eabi \
    --profile size \
    -Z build-std=core \
    -- -C linker=rust-lld \
       -C link-arg=-T"$WORK/common/mps2_an385.ld" \
       -C link-arg=--gc-sections \
       -C link-arg=--icf=all \
       -Z location-detail=none \
       >/dev/null 2>&1

ELF="$WORK/target/thumbv7m-none-eabi/size/fw-rust"
echo
arm-none-eabi-size -A "$ELF" | grep -E '^\.text|^\.rodata|^\.data|^\.bss'
TEXT=$(arm-none-eabi-size -A "$ELF" | awk '/^\.text/{print $2}')
RODATA=$(arm-none-eabi-size -A "$ELF" | awk '/^\.rodata/{print $2}')
DATA=$(arm-none-eabi-size -A "$ELF" | awk '/^\.data/{print $2}')
echo
echo "ROM (text+rodata+data) = $((TEXT + RODATA + DATA)) B"
echo "C reference            = 11408 B"
