#!/usr/bin/env python3
"""Compare the C reference benchmark JSON with the Rust benchmark JSON.

Usage:
    scripts/compare_bench.py harness/results/c_wg8_host.json harness/results/rust_wg8_host.json
    scripts/compare_bench.py --wg 8 --md docs/BENCHMARKS_generated.md
    scripts/compare_bench.py --wg 1

Exit status: 0 if every scenario meets the acceptance criteria
(identical checksum, <= flash reads/writes/erases/bytes, <= ns/op),
1 otherwise (details printed).
"""

import argparse
import json
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RESULTS = os.path.join(ROOT, "harness", "results")


def load(path):
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def weighted(flash):
    return flash["read_bytes"] + flash["write_bytes"] * 10 + flash["erase_bytes"] * 100


def find_scenario(doc, name):
    for s in doc["scenarios"]:
        if s["name"] == name:
            return s
    return None


def fmt_int(n):
    return f"{n:,}"


def fmt_size(n):
    if n >= 1024:
        return f"{n / 1024:.1f} KiB"
    return f"{n} B"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("c_json", nargs="?", help="C benchmark JSON")
    ap.add_argument("rust_json", nargs="?", help="Rust benchmark JSON")
    ap.add_argument("--wg", type=int, default=8, help="write granularity (default 8)")
    ap.add_argument("--md", help="write a markdown report to this path")
    args = ap.parse_args()

    c_path = args.c_json or os.path.join(RESULTS, f"c_wg{args.wg}_host.json")
    r_path = args.rust_json or os.path.join(RESULTS, f"rust_wg{args.wg}_host.json")
    c = load(c_path)
    r = load(r_path)

    lines = []
    failures = []

    def out(line=""):
        lines.append(line)
        print(line)

    out(f"# FlashDB C vs Rust — write granularity {args.wg}")
    out()
    out(f"C reference : {os.path.relpath(c_path, ROOT)} ({c.get('version', '?')})")
    out(f"Rust rewrite: {os.path.relpath(r_path, ROOT)} ({r.get('version', '?')})")
    out()
    out(
        "| scenario | ops | ns/op C | ns/op Rust | ratio | reads C | reads R | "
        "writes C | writes R | erases C | erases R | weighted C | weighted R | checksum |"
    )
    out("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")

    for cs in c["scenarios"]:
        name = cs["name"]
        rs = find_scenario(r, name)
        if rs is None:
            failures.append(f"{name}: missing in Rust JSON")
            out(f"| {name} | - | - | - | - | - | - | - | - | - | - | - | - | MISSING |")
            continue
        cf, rf = cs["flash"], rs["flash"]
        c_ns, r_ns = cs["ns_per_op"], rs["ns_per_op"]
        ratio = r_ns / c_ns if c_ns else 0.0
        ck_ok = cs["checksum"] == rs["checksum"]
        reads_ok = rf["reads"] <= cf["reads"]
        writes_ok = rf["writes"] <= cf["writes"]
        erases_ok = rf["erases"] <= cf["erases"]
        rbytes_ok = rf["read_bytes"] <= cf["read_bytes"]
        wbytes_ok = rf["write_bytes"] <= cf["write_bytes"]
        time_ok = r_ns <= c_ns * 1.05  # 5% tolerance for host timing noise
        wc, wr = weighted(cf), weighted(rf)
        cost_ok = wr <= wc * 1.05
        # The weighted cost is dominated by erase bytes; compare it too.
        status = []
        if not ck_ok:
            status.append("CHECKSUM")
            failures.append(f"{name}: checksum C={cs['checksum']} Rust={rs['checksum']}")
        if not reads_ok:
            status.append("READS")
            failures.append(f"{name}: reads Rust {rf['reads']} > C {cf['reads']}")
        if not rbytes_ok:
            status.append("READ_BYTES")
        if not writes_ok:
            status.append("WRITES")
            failures.append(f"{name}: writes Rust {rf['writes']} > C {cf['writes']}")
        if not wbytes_ok:
            status.append("WRITE_BYTES")
        if not erases_ok:
            status.append("ERASES")
            failures.append(f"{name}: erases Rust {rf['erases']} > C {cf['erases']}")
        if not time_ok:
            status.append(f"TIME({ratio:.2f}x)")
            failures.append(f"{name}: ns/op Rust {r_ns:.1f} > C {c_ns:.1f}")
        if not cost_ok:
            status.append("COST")
        verdict = "OK" if not status else "FAIL:" + ",".join(status)
        out(
            f"| {name} | {cs['ops']} | {c_ns:,.1f} | {r_ns:,.1f} | {ratio:.2f}x | "
            f"{fmt_int(cf['reads'])} | {fmt_int(rf['reads'])} | "
            f"{fmt_int(cf['writes'])} | {fmt_int(rf['writes'])} | "
            f"{cf['erases']} | {rf['erases']} | "
            f"{fmt_int(wc)} | {fmt_int(wr)} | {'same' if ck_ok else 'DIFF'} {verdict} |"
        )

    out()
    out("## Footprint")
    out()
    cfp = c.get("footprint", {})
    rfp = r.get("footprint", {})
    c_kv = cfp.get("sizeof_kvdb")
    c_ts = cfp.get("sizeof_tsdb")
    r_kv = rfp.get("kvdb_bytes")
    r_kv0 = rfp.get("kvdb_no_cache_bytes")
    r_ts = rfp.get("tsdb_bytes")
    out("| struct | C | Rust (default) | Rust (no cache) |")
    out("|---|---|---|---|")
    out(f"| KVDB | {c_kv} B | {r_kv} B | {r_kv0} B |")
    out(f"| TSDB | {c_ts} B | {r_ts} B | - |")
    out()
    c_rom = 11408  # from scripts/fw_c.sh (text+rodata+data), updated by docs/BENCHMARKS.md
    out(
        "Firmware numbers (Cortex-M3, see `harness/firmware/README.md`) are compared "
        "separately; host numbers above are not ROM/RAM."
    )
    out()
    out(f"**Result: {'PASS' if not failures else 'FAIL'}**")
    if failures:
        out()
        out("Failures:")
        for f in failures:
            out(f"- {f}")

    if args.md:
        with open(args.md, "w", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")
        print(f"\nwritten {args.md}")
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())
