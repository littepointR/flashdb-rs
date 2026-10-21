#!/usr/bin/env python3
"""Aggregate interleaved benchmark rounds into markdown tables.

Usage:
    scripts/bench_report.py --rounds /tmp/bench_rounds --wg 8
    scripts/bench_report.py --rounds /tmp/bench_rounds --wg 8 --rewrite docs/BENCHMARKS.md

Rounds must contain c{wg}_N.json (C reference) and r{wg}_N.json (Rust). Timing
uses the global minimum across every sample of every round — the most robust
estimate available on a shared, noisy host; flash counters are deterministic.
"""

import argparse
import glob
import json
import os
import re

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MARKER = {8: "<!-- HOST_TABLE_WG8 -->", 1: "<!-- HOST_TABLE_WG1 -->"}


def load_rounds(pattern):
    docs = []
    for path in sorted(glob.glob(pattern)):
        with open(path, "r", encoding="utf-8") as f:
            docs.append(json.load(f))
    return docs


def scenario(doc, name):
    for s in doc["scenarios"]:
        if s["name"] == name:
            return s
    return None


def agg(docs):
    """Map scenario name -> (min phase ns, first scenario dict)."""
    out = {}
    for doc in docs:
        for s in doc["scenarios"]:
            samples = (s.get("extra") or {}).get("samples_ns")
            if not samples:
                samples = [s["total_ns_median"]]
            m = min(samples)
            if s["name"] not in out or m < out[s["name"]][0]:
                out[s["name"]] = (m, s)
    return out


def weighted(flash):
    return flash["read_bytes"] + flash["write_bytes"] * 10 + flash["erase_bytes"] * 100


def build_table(c_agg, r_agg, wg):
    order = [s["name"] for s in c_agg["_order"]]
    lines = []
    lines.append(
        "| scenario | ops | ns/op C | ns/op Rust | ratio | reads C | reads Rust | "
        "writes C | writes Rust | erases C | erases Rust | weighted C | weighted Rust | checksum |"
    )
    lines.append("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|")
    bad = []
    for name in order:
        c_ns, cs = c_agg[name]
        r_ns, rs = r_agg[name]
        ops = cs["ops"] or 1
        cf, rf = cs["flash"], rs["flash"]
        ck = "same" if cs["checksum"] == rs["checksum"] else "DIFF"
        if ck == "DIFF":
            bad.append(f"{name}: checksum")
        for key in ("reads", "writes", "erases"):
            if rf[key] > cf[key]:
                bad.append(f"{name}: {key} {rf[key]} > {cf[key]}")
        ratio = r_ns / c_ns if c_ns else 0
        lines.append(
            f"| {name} | {ops} | {c_ns / ops:,.1f} | {r_ns / ops:,.1f} | {ratio:.2f}x | "
            f"{cf['reads']:,} | {rf['reads']:,} | {cf['writes']:,} | {rf['writes']:,} | "
            f"{cf['erases']} | {rf['erases']} | {weighted(cf):,} | {weighted(rf):,} | {ck} |"
        )
    return lines, bad


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", default="/tmp/bench_rounds")
    ap.add_argument("--wg", type=int, default=8)
    ap.add_argument("--rewrite", help="markdown file whose marker is replaced")
    args = ap.parse_args()

    c_docs = load_rounds(os.path.join(args.rounds, f"c{args.wg}_*.json"))
    r_docs = load_rounds(os.path.join(args.rounds, f"r{args.wg}_*.json"))
    if not c_docs or not r_docs:
        raise SystemExit("no round JSON files found")

    c_agg = agg(c_docs)
    r_agg = agg(r_docs)
    c_agg["_order"] = [{"name": n} for n in c_agg if n != "_order"]
    lines, bad = build_table(c_agg, r_agg, args.wg)
    table = "\n".join(lines)
    print(table)
    print()
    if bad:
        print("violations:", "; ".join(bad))
    else:
        print("all scenarios: identical checksums and no more flash traffic than C")

    if args.rewrite:
        path = args.rewrite
        if not os.path.isabs(path):
            path = os.path.join(ROOT, path)
        with open(path, "r", encoding="utf-8") as f:
            text = f.read()
        marker = MARKER[args.wg]
        if marker not in text:
            raise SystemExit(f"marker {marker} not found in {path}")
        text = text.replace(marker, table)
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)
        print(f"\nupdated {path}")


if __name__ == "__main__":
    main()
