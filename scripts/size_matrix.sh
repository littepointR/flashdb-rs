#!/usr/bin/env bash
#
# Print the full firmware ROM matrix (C reference + every Rust size build) in
# one table, so the numbers quoted in docs/BENCHMARKS.md and README.md can be
# reproduced and regression-checked with a single command.
#
# Usage:
#   scripts/size_matrix.sh              # build everything, then print
#   scripts/size_matrix.sh --no-build   # reuse existing ELFs (fast)
#
# Configurations:
#   -s / -z              release-ish "s"/"z" profiles (stable toolchain)
#   --small-crc          library feature: 64-byte nibble CRC table
#   nightly              -Z build-std=core + panic=immediate-abort + LLD ICF
#
set -euo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
FW="$ROOT/harness/firmware"
RUST_BUILD="$FW/build/rust"
C_ELF="$FW/build/c/fw_c.elf"
NO_BUILD=0
[ "${1:-}" = "--no-build" ] && NO_BUILD=1

rom_of() { # $1 = ELF; text+rodata+data, ignoring .sim_flash/.bss
    arm-none-eabi-size -A "$1" |
        awk '/^\.text|^\.rodata|^\.data/ { s += $2 } END { print s + 0 }'
}

if [ "$NO_BUILD" = "0" ]; then
    echo "building C reference ..."
    [ -f "$C_ELF" ] || "$ROOT/scripts/fw_c.sh" --build-only >/dev/null
    echo "building Rust stable profiles ..."
    "$ROOT/scripts/fw_rust.sh" --build-only >/dev/null
    "$ROOT/scripts/fw_rust.sh" --small-crc --build-only >/dev/null
    "$ROOT/scripts/fw_rust.sh" size --build-only >/dev/null
    "$ROOT/scripts/fw_rust.sh" size --small-crc --build-only >/dev/null
fi

[ -f "$C_ELF" ] || { echo "missing $C_ELF (run scripts/fw_c.sh --build-only)" >&2; exit 1; }
C_ROM=$(rom_of "$C_ELF")

if [ "$NO_BUILD" = "0" ]; then
    echo "building Rust nightly size builds ..."
    N_DEFAULT=$("$ROOT/scripts/fw_rust_nightly.sh" | awk -F'= ' '/^ROM /{print $2}' | awk '{print $1}')
    N_SMALL=$("$ROOT/scripts/fw_rust_nightly.sh" --small-crc | awk -F'= ' '/^ROM /{print $2}' | awk '{print $1}')
else
    # --no-build cannot reproduce nightly (temporary build), require rebuild.
    N_DEFAULT=""; N_SMALL=""
fi

row() { # label elf-or-value
    local label=$1 value=$2
    if [ -z "$value" ]; then
        printf '| %-34s | %8s | %8s |\n' "$label" "-" "-"
    else
        printf '| %-34s | %8d | %+7.1f%% |\n' "$label" "$value" \
            "$(awk -v a="$value" -v b="$C_ROM" 'BEGIN { printf (a - b) * 100.0 / b }')"
    fi
}

printf '\n| configuration                          | ROM (B)  | vs C       |\n'
printf '|----------------------------------------|---------:|-----------:|\n'
row "C reference (-Os, FAL)" "$C_ROM"
row "Rust -s default" "$(rom_of "$RUST_BUILD/fw_rust_release.elf")"
row "Rust -z default" "$(rom_of "$RUST_BUILD/fw_rust_size.elf")"
row "Rust -s + small-crc" "$(rom_of "$RUST_BUILD/fw_rust_release_smallcrc.elf")"
row "Rust -z + small-crc" "$(rom_of "$RUST_BUILD/fw_rust_size_smallcrc.elf")"
row "Rust nightly (-z) default" "$N_DEFAULT"
row "Rust nightly (-z) + small-crc" "$N_SMALL"
printf '\n'
