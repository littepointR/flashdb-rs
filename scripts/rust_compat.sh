#!/usr/bin/env bash
#
# C <-> Rust on-flash image compatibility matrix.
#
#   scripts/rust_compat.sh [--wg 1|8|32|64|128|256|all] [--out-dir DIR]
#
# For every write granularity (FDB_WRITE_GRAN; 1, 8, 32, 64, 128 and 256 by
# default) this checks:
#
#   1. Rust selfcheck: deterministic write (twice, byte-compare), read back,
#      summary equal, SHA-256 printed.
#   2. Rust write -> Rust read: summary must equal the writer's summary.
#   3. Rust write -> C read: the C reference must mount the Rust image and
#      print exactly the same summary.
#   4. C write -> C read: reference self-consistency (sanity of the harness).
#   5. C write -> Rust read: the Rust crate must mount the C image WITHOUT
#      formatting it and print exactly the same summary.
#   6. Image SHA-256 of the Rust-written image == SHA-256 of the C-written
#      image (byte-identical on-flash format).
#
# Expected failure (XFAIL), counted separately from FAIL: FlashDB-C's
# `write_tsl()` copies `sizeof(struct log_idx_data) - LOG_IDX_TS_OFFSET` bytes
# out of an *uninitialised* stack struct, so it writes the per-record padding
# (`LOG_IDX_PADDING_SIZE` bytes: 0 for wg 32, 4 for wg 64/128, 20 for wg 256) as
# stack garbage. For wg 256 this makes the C tool's own output nondeterministic,
# so byte-identity against the C image is not well-defined. In that case the
# byte-identity check is reported as XFAIL iff (a) the C tool was shown to be
# nondeterministic (two C writes differ), (b) the diff report is PADDING-ONLY
# (Rust writes erased padding there), and (c) every logical check for that
# granularity still passes. Any other mismatch is a hard FAIL.
#
# When the SHAs differ, a first-difference report is produced: first offset,
# KV/TS region, sector index, offset inside the sector, contiguous diff ranges,
# and a padding-only vs real-difference classification. Flash-op counters are
# reported for information only (they are allowed to differ: the Rust crate
# intentionally has no sector cache, but the on-flash bytes must match).
#
# Artifacts are written to harness/results/rust_compat/ (matrix.txt plus the
# raw summaries, stats and diffs). Exit status: 0 iff no hard FAIL remains
# (XFAIL lines are expected C-reference defects and do not fail the matrix).
#
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
RESULTS="$ROOT/harness/results"
OUT=""
WG_LIST="8 1 32 64 128 256"

usage() {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --wg)
            case "${2:-}" in
                1|8|32|64|128|256) WG_LIST="${2}"; shift 2 ;;
                both) WG_LIST="8 1"; shift 2 ;;
                all) WG_LIST="8 1 32 64 128 256"; shift 2 ;;
                *) echo "invalid --wg '${2:-}' (use 1, 8, 32, 64, 128, 256, both or all)" >&2; exit 2 ;;
            esac
            ;;
        --out-dir)
            OUT=${2:?--out-dir needs a value}
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [ -z "$OUT" ]; then
    OUT="$RESULTS/rust_compat"
fi
mkdir -p "$OUT"
rm -f "$OUT"/*.txt "$OUT"/*.diff "$OUT"/*.summary "$OUT"/*.stats "$OUT"/*.report.txt 2>/dev/null || true
MATRIX="$OUT/matrix.txt"
: > "$MATRIX"

PASS=0
FAIL=0
XFAIL=0

say()  { printf '%s\n' "$*"; }
pass() { PASS=$((PASS + 1)); say "PASS  $1"; printf 'PASS  %s\n' "$1" >> "$MATRIX"; }
fail() {
    FAIL=$((FAIL + 1))
    say "FAIL  $1"
    printf 'FAIL  %s\n' "$1" >> "$MATRIX"
}
# Expected, evidence-checked failure (a defect of the C reference, not Rust):
# does not count as a hard failure and does not affect the exit status.
xfail() {
    XFAIL=$((XFAIL + 1))
    say "XFAIL $1"
    printf 'XFAIL %s\n' "$1" >> "$MATRIX"
}
info() { say "INFO  $1"; printf 'INFO  %s\n' "$1" >> "$MATRIX"; }

sha256_file() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        echo "(no sha256 tool)"
    fi
}

# check_same <description> <file-a> <file-b>
check_same() {
    local name=$1 a=$2 b=$3
    local diff_file="$OUT/$(printf '%s' "$name" | tr ' /()=<>' '_').diff"
    if [ ! -f "$a" ] || [ ! -f "$b" ]; then
        fail "$name (missing file: $a / $b)"
        return
    fi
    if diff -u "$a" "$b" >"$diff_file" 2>&1; then
        rm -f "$diff_file"
        pass "$name"
    else
        fail "$name (see ${diff_file#"$ROOT"/})"
        sed -n '1,12p' "$diff_file" | sed 's/^/      /'
    fi
}

# check_path <description> <path>  -> file must exist and be 32768 bytes
check_image_size() {
    local name=$1 path=$2
    if [ -f "$path" ]; then
        local size
        size=$(wc -c <"$path" | tr -d ' ')
        if [ "$size" = "32768" ]; then
            pass "$name (32768 bytes)"
        else
            fail "$name (size $size != 32768)"
        fi
    else
        fail "$name (file missing: $path)"
    fi
}

# first_diff <rust-img> <c-img> <out>  -> human readable report
first_diff() {
    local a=$1 b=$2 out=$3
    if command -v python3 >/dev/null 2>&1; then
        python3 - "$a" "$b" >"$out" <<'PY'
import sys

a = open(sys.argv[1], 'rb').read()
b = open(sys.argv[2], 'rb').read()
n = min(len(a), len(b))

diffs = [i for i in range(n) if a[i] != b[i]]
if len(a) != len(b):
    print(f"size mismatch: {len(a)} != {len(b)}")
if not diffs:
    print("no byte differences in the common prefix")
    sys.exit(0)

first = diffs[0]
region = "KV" if first < 16384 else "TS"
rel = first if first < 16384 else first - 16384
print(f"first differing byte: offset {first} (0x{first:04x}) region={region} "
      f"sector={rel // 4096} offset_in_sector={rel % 4096} "
      f"rust=0x{a[first]:02x} c=0x{b[first]:02x}")

ranges = []
start = prev = diffs[0]
for i in diffs[1:]:
    if i == prev + 1:
        prev = i
        continue
    ranges.append((start, prev))
    start = prev = i
ranges.append((start, prev))

print(f"total differing bytes: {len(diffs)} in {len(ranges)} contiguous ranges")

padding_only = True
for (lo, hi) in ranges:
    ra = a[lo:hi + 1]
    rb = b[lo:hi + 1]
    kinds = []
    if all(x == 0xFF for x in ra):
        kinds.append("rust=erased")
    if all(x == 0xFF for x in rb):
        kinds.append("c=erased")
    if not kinds:
        padding_only = False
    if len(ranges) <= 20:
        print(f"  {lo:5d}..{hi:5d} len={hi - lo + 1:4d} "
              f"rust[:8]={ra[:8].hex(' ')} c[:8]={rb[:8].hex(' ')} "
              f"{' '.join(kinds)}")

print("classification:", "PADDING-ONLY (erased flash vs data)" if padding_only
      else "REAL DIFFERENCE (both sides contain non-erased data)")

# Sector-level summary: which sectors differ.
for sec in range(8):
    lo = sec * 4096
    hi = lo + 4096
    cnt = sum(1 for i in diffs if lo <= i < hi)
    if cnt:
        part = "KV" if sec < 4 else "TS"
        print(f"  {part} sector {sec % 4}: {cnt} differing bytes")
PY
    else
        # Fallback without python3: byte positions are 1-based in cmp -l.
        {
            echo "python3 unavailable; using cmp -l"
            cmp -l "$a" "$b" | head -40
            echo "..."
            cmp -l "$a" "$b" | wc -l
        } >"$out" 2>&1
    fi
}

echo "== Rust <-> C image compatibility matrix =="
echo "   write granularities: $WG_LIST"
echo "   artifacts:           ${OUT#"$ROOT"/}"
echo

# ---------------------------------------------------------------------------
# build Rust tool
# ---------------------------------------------------------------------------
echo "== build =="
if (cd "$ROOT" && cargo build --quiet --release -p flashdb-bench --bin image_tool) \
        >"$OUT/build_rust.log" 2>&1; then
    pass "build Rust image tool"
else
    fail "build Rust image tool (see ${OUT#"$ROOT"/}/build_rust.log)"
    tail -20 "$OUT/build_rust.log" | sed 's/^/      /'
    echo
    echo "rust_compat: FAIL (cannot build)"
    exit 1
fi
RUST_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
case "$RUST_TARGET_DIR" in
    /*) ;;
    *) RUST_TARGET_DIR="$ROOT/$RUST_TARGET_DIR" ;;
esac
RUST_BIN="$RUST_TARGET_DIR/release/image_tool"

# ---------------------------------------------------------------------------
# per write granularity
# ---------------------------------------------------------------------------
for wg in $WG_LIST; do
    echo
    echo "== WG=${wg} =="
    WG_FAIL_BASE=$FAIL

    RUST_IMG="$OUT/rust_wg${wg}.bin"
    C_IMG="$OUT/c_wg${wg}.bin"
    RUST_SELF="$OUT/rust_wg${wg}.selfcheck.txt"
    RUST_WRITE_SUM="$OUT/rust_wg${wg}.rust_write.summary"
    RUST_WRITE_STATS="$OUT/rust_wg${wg}.rust_write.stats"
    RUST_READ_RUST="$OUT/rust_wg${wg}.rust_read_rust.summary"
    C_READ_RUST="$OUT/rust_wg${wg}.c_read_rust.summary"
    C_WRITE_SUM="$OUT/c_wg${wg}.c_write.summary"
    C_WRITE_STATS="$OUT/c_wg${wg}.c_write.stats"
    C_READ_C="$OUT/c_wg${wg}.c_read_c.summary"
    RUST_READ_C="$OUT/c_wg${wg}.rust_read_c.summary"

    # 0. C reference self-check (deterministic image + read-back).
    #
    # FlashDB-C writes uninitialised per-record padding for write granularities
    # above 8 bits (see the header): for wg 256 its own image is therefore not
    # deterministic. Distinguish that C-side defect from a real failure by
    # writing the image twice and comparing the raw files; only if the C tool is
    # nondeterministic is the selfcheck reported as an expected failure.
    C_NONDET=0
    if "$ROOT/scripts/c_image.sh" selfcheck "$wg" \
            >"$OUT/c_wg${wg}.selfcheck.txt" 2>&1; then
        pass "WG${wg} C reference selfcheck (deterministic write + read back)"
    else
        "$ROOT/scripts/c_image.sh" write "$OUT/c_wg${wg}.det_a.bin" "$wg" >/dev/null 2>&1
        "$ROOT/scripts/c_image.sh" write "$OUT/c_wg${wg}.det_b.bin" "$wg" >/dev/null 2>&1
        if ! cmp -s "$OUT/c_wg${wg}.det_a.bin" "$OUT/c_wg${wg}.det_b.bin"; then
            C_NONDET=1
            xfail "WG${wg} C reference selfcheck: C tool output is nondeterministic (FlashDB-C writes uninitialised log-index padding; see ${OUT#"$ROOT"/}/c_wg${wg}.selfcheck.txt)"
        else
            fail "WG${wg} C reference selfcheck (see ${OUT#"$ROOT"/}/c_wg${wg}.selfcheck.txt)"
        fi
        rm -f "$OUT/c_wg${wg}.det_a.bin" "$OUT/c_wg${wg}.det_b.bin"
    fi

    # 1. Rust selfcheck.
    if "$RUST_BIN" selfcheck "$wg" >"$RUST_SELF" 2>>"$RUST_SELF"; then
        pass "WG${wg} Rust selfcheck (write twice, compare, read back)"
    else
        fail "WG${wg} Rust selfcheck (see ${OUT#"$ROOT"/}/rust_wg${wg}.selfcheck.txt)"
        sed -n '1,12p' "$RUST_SELF" | sed 's/^/      /'
    fi
    RUST_SELF_SHA=$(awk '/sha256:/ {print $2; exit}' "$RUST_SELF")

    # 2. Rust write -> Rust read.
    if "$RUST_BIN" write "$RUST_IMG" "$wg" >"$RUST_WRITE_SUM" 2>"$RUST_WRITE_STATS"; then
        pass "WG${wg} Rust write image"
    else
        fail "WG${wg} Rust write image (see ${OUT#"$ROOT"/}/rust_wg${wg}.rust_write.stats)"
    fi
    check_image_size "WG${wg} Rust image size" "$RUST_IMG"

    RUST_SHA=$(sha256_file "$RUST_IMG")
    if [ -n "${RUST_SELF_SHA:-}" ] && [ "$RUST_SELF_SHA" = "$RUST_SHA" ]; then
        pass "WG${wg} Rust selfcheck sha256 == written image sha256"
    else
        fail "WG${wg} Rust selfcheck sha256 (${RUST_SELF_SHA:-missing}) != written image (${RUST_SHA:-missing})"
    fi

    if "$RUST_BIN" read "$RUST_IMG" "$wg" >"$RUST_READ_RUST" 2>>"$RUST_READ_RUST"; then
        pass "WG${wg} Rust read of Rust image (mount without formatting)"
    else
        fail "WG${wg} Rust read of Rust image returned an error"
    fi
    check_same "WG${wg} Rust read(Rust image) summary == Rust write summary" \
        "$RUST_WRITE_SUM" "$RUST_READ_RUST"

    # 3. Rust write -> C read (cross implementation).
    if "$ROOT/scripts/c_image.sh" read "$RUST_IMG" "$wg" \
            >"$C_READ_RUST" 2>"$OUT/rust_wg${wg}.c_read.err"; then
        pass "WG${wg} C read of Rust image (mounts cleanly)"
    else
        fail "WG${wg} C read of Rust image (see ${OUT#"$ROOT"/}/rust_wg${wg}.c_read.err)"
        sed -n '1,8p' "$OUT/rust_wg${wg}.c_read.err" | sed 's/^/      /'
    fi
    check_same "WG${wg} C read(Rust image) summary == Rust write summary" \
        "$RUST_WRITE_SUM" "$C_READ_RUST"

    # 4. C write -> C read (reference self-consistency).
    if "$ROOT/scripts/c_image.sh" write "$C_IMG" "$wg" \
            >"$C_WRITE_SUM" 2>"$C_WRITE_STATS"; then
        pass "WG${wg} C write image"
    else
        fail "WG${wg} C write image (see ${OUT#"$ROOT"/}/c_wg${wg}.c_write.stats)"
    fi
    check_image_size "WG${wg} C image size" "$C_IMG"

    C_SHA=$(sha256_file "$C_IMG")

    if "$ROOT/scripts/c_image.sh" read "$C_IMG" "$wg" >"$C_READ_C" 2>>"$C_READ_C"; then
        pass "WG${wg} C read of C image"
    else
        fail "WG${wg} C read of C image"
    fi
    check_same "WG${wg} C read(C image) summary == C write summary" \
        "$C_WRITE_SUM" "$C_READ_C"

    # 5. C write -> Rust read (the interesting direction for a rewrite).
    if "$RUST_BIN" read "$C_IMG" "$wg" >"$RUST_READ_C" 2>>"$RUST_READ_C"; then
        pass "WG${wg} Rust read of C image (must not format/erase it)"
    else
        fail "WG${wg} Rust read of C image returned an error"
        sed -n '1,8p' "$RUST_READ_C" | sed 's/^/      /'
    fi
    check_same "WG${wg} Rust read(C image) summary == C write summary" \
        "$C_WRITE_SUM" "$RUST_READ_C"

    # 6. Byte-identical images.
    echo "      WG${wg} sha256 rust: $RUST_SHA"
    echo "      WG${wg} sha256 c:    $C_SHA"
    if [ -n "$RUST_SHA" ] && [ "$RUST_SHA" = "$C_SHA" ]; then
        pass "WG${wg} image sha256 rust == C (byte-identical image)"
    else
        REPORT="$OUT/rust_wg${wg}_firstdiff.txt"
        first_diff "$RUST_IMG" "$C_IMG" "$REPORT"
        if [ "$C_NONDET" -eq 1 ] && [ "$FAIL" -eq "$WG_FAIL_BASE" ] \
                && grep -q "classification: PADDING-ONLY" "$REPORT"; then
            xfail "WG${wg} image sha256 rust != C: diff is PADDING-ONLY and the C image is nondeterministic (FlashDB-C writes uninitialised log-index padding; Rust writes erased 0xFF). Byte-identity is undefined on the C side for this granularity; all logical checks passed."
            sed -n '1,3p' "$REPORT" | sed 's/^/      /'
        else
            fail "WG${wg} image sha256 rust != C"
            sed -n '1,20p' "$REPORT" | sed 's/^/      /'
            echo "      full report: ${REPORT#"$ROOT"/}"
        fi
    fi

    # Flash-op counters: informational only.
    if [ -s "$RUST_WRITE_STATS" ] && [ -s "$C_WRITE_STATS" ]; then
        info "WG${wg} flash op counters (informational, not a pass criterion):"
        while IFS= read -r line; do
            case "$line" in
                *KVFLASH*|*TSFLASH*|*IMAGEFLASH*)
                    echo "      rust  $line" | tee -a "$MATRIX"
                    ;;
            esac
        done <"$RUST_WRITE_STATS"
        while IFS= read -r line; do
            case "$line" in
                *KVFLASH*|*TSFLASH*|*IMAGEFLASH*)
                    echo "      c     $line" | tee -a "$MATRIX"
                    ;;
            esac
        done <"$C_WRITE_STATS"
    fi
done

# ---------------------------------------------------------------------------
# summary
# ---------------------------------------------------------------------------
echo
echo "== matrix =="
cat "$MATRIX"
TOTAL=$((PASS + FAIL + XFAIL))
echo
echo "rust_compat: ${PASS}/${TOTAL} checks passed (${XFAIL} expected XFAIL, ${FAIL} failed)"
if [ "$FAIL" -ne 0 ]; then
    echo "rust_compat: FAIL (${FAIL} failed; artifacts in ${OUT#"$ROOT"/})"
    exit 1
fi
echo "rust_compat: OK (all enforced checks passed; Rust and C images are byte-identical except for documented C-reference XFAILs)"
exit 0
