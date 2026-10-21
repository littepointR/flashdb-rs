#!/usr/bin/env python3
"""Symbol-level ROM/RAM attribution for the FlashDB firmware measurement ELFs.

Used by scripts/fw_c.sh --symbols and scripts/fw_rust.sh --symbols.

It parses `arm-none-eabi-nm --print-size` output and reports:
  * the top-N largest code symbols,
  * a bucketed code-size attribution (library vs harness vs toolchain runtime),
  * the largest rodata/data/bss symbols,
  * optionally (C) per-object code sizes to isolate the library objects.

The two firmwares are classified differently:
  * Rust: mangled symbol names tell the crate (`flashdb`), the firmware harness
    (`fw_rust`), compiler-builtins/`__aeabi_*` memory helpers and `core`.
  * C:    symbol names do not carry the object file, so the harness symbols are
    matched by name and everything else in .text is the FlashDB/FAL library.

Usage:
    symbol_report.py ELF --kind rust|c [--top N] [--objdir DIR]

The script needs `arm-none-eabi-nm`; pass NM=... or add it to PATH.
"""

import argparse
import os
import re
import subprocess
import sys


def run(cmd):
    try:
        return subprocess.run(cmd, check=True, capture_output=True, text=True).stdout
    except FileNotFoundError:
        sys.exit("symbol_report: tool not found: %s" % cmd[0])
    except subprocess.CalledProcessError as exc:
        sys.exit("symbol_report: command failed: %s\n%s" % (" ".join(cmd), exc.stderr))


def section_sizes(nm_tool, elf):
    """Return {section: size} for allocated output sections (text/rodata/...)."""
    size_tool = nm_tool.replace("nm", "size")
    for cand in (size_tool, "arm-none-eabi-size"):
        try:
            out = run([cand, "-A", elf])
            break
        except SystemExit:
            continue
    else:
        return {}

    sizes = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 3 and parts[0].startswith("."):
            try:
                sizes[parts[0]] = int(parts[1])
            except ValueError:
                pass
    return sizes


def parse_nm(nm_tool, elf):
    """Return list of (size, type, name) for defined symbols with a size."""
    out = run([nm_tool, "--print-size", "--defined-only", elf])
    symbols = []
    for line in out.splitlines():
        parts = line.split()
        if len(parts) != 4:
            continue  # absolute symbols / no size
        _addr, size_hex, typ, name = parts
        try:
            size = int(size_hex, 16)
        except ValueError:
            continue
        if size == 0:
            continue
        symbols.append((size, typ, name))
    return symbols


class Bucketer:
    def __init__(self, kind):
        self.kind = kind

    def bucket(self, typ, name):
        if self.kind == "rust":
            if "flashdb" in name:
                return "flashdb crate"
            if "fw_rust" in name or name in (
                "main", "Reset_Handler", "Default_Handler", "g_vectors",
            ) or name.startswith(("__text", "__rodata", "__data", "__bss", "_sbss", "_estack", "_sdata")):
                return "harness (fw_rust)"
            if name.startswith("OUTLINED_FUNCTION"):
                return "outliner stubs"
            if ("compiler_builtins" in name) or ("__aeabi" in name) or re.search(
                r"(memcpy|memset|memmove|memcmp|bcmp|strlen|strcmp|strncmp|strncpy)$", name
            ):
                return "toolchain mem/aeabi"
            if "core" in name:
                return "core/panic"
            return "other"
        # C: identify harness symbols by name, everything else is library code.
        harness_exact = {
            "Reset_Handler", "Default_Handler", "main", "nor_flash0", "g_vectors",
            "memcpy", "memmove", "memset", "memcmp", "strlen", "strcmp",
            "strncmp", "strncpy",
        }
        harness_prefixes = (
            "sh_", "harness_", "stack_", "sim_flash_",
            "__aeabi", "__gnu", "__text", "__rodata", "__data", "__bss",
        )
        if name in harness_exact or name.startswith(harness_prefixes):
            return "harness (C)"
        if "g_sim_flash" in name:
            return "harness (C)"
        return "FlashDB/FAL library"


def fmt_size(n):
    return "%6d" % n


def print_table(title, rows, limit):
    print(title)
    if not rows:
        print("   (none)")
        return
    print("   %8s  %-6s  %s" % ("bytes", "kind", "symbol"))
    for size, typ, name in rows[:limit]:
        print("   %8d  %-6s  %s" % (size, typ, name))


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("elf")
    ap.add_argument("--kind", choices=("rust", "c"), required=True)
    ap.add_argument("--top", type=int, default=30)
    ap.add_argument("--objdir", default=None,
                    help="C only: directory with per-source .o files for per-object totals")
    args = ap.parse_args()

    nm_tool = os.environ.get("NM", "arm-none-eabi-nm")
    symbols = parse_nm(nm_tool, args.elf)
    sections = section_sizes(nm_tool, args.elf)
    bucketer = Bucketer(args.kind)

    code_types = set("TtWw")
    ro_types = set("Rr")
    data_types = set("Dd")
    bss_types = set("Bb")

    code = [s for s in symbols if s[1] in code_types]
    rodata = [s for s in symbols if s[1] in ro_types]
    data = [s for s in symbols if s[1] in data_types]
    bss = [s for s in symbols if s[1] in bss_types]

    print("== symbol attribution: %s ==" % args.elf)
    if sections:
        rom = sections.get(".text", 0) + sections.get(".rodata", 0) + sections.get(".data", 0)
        print("   .text=%d .rodata=%d .data=%d .bss=%d  => ROM=%d"
              % (sections.get(".text", 0), sections.get(".rodata", 0),
                 sections.get(".data", 0), sections.get(".bss", 0), rom))
    print("   sized symbols: code=%d rodata=%d data=%d bss=%d"
          % (sum(s[0] for s in code), sum(s[0] for s in rodata),
             sum(s[0] for s in data), sum(s[0] for s in bss)))
    print()

    # Bucketed code attribution
    buckets = {}
    for size, typ, name in code:
        b = bucketer.bucket(typ, name)
        buckets[b] = buckets.get(b, 0) + size
    total_code = sum(buckets.values()) or 1
    print("-- code size by bucket (sum of sized symbols in .text) --")
    for name, size in sorted(buckets.items(), key=lambda kv: -kv[1]):
        print("   %-24s %6d B  (%4.1f%%)" % (name, size, 100.0 * size / total_code))
    print("   %-24s %6d B" % ("total", sum(buckets.values())))
    print()

    print_table("-- top %d code symbols --" % args.top,
                sorted(code, reverse=True), args.top)
    print()

    if rodata:
        print_table("-- top 10 rodata symbols --", sorted(rodata, reverse=True), 10)
    else:
        print("-- rodata --")
        print("   (no named rodata symbols; %d B of string literals / constant tables)"
              % sections.get(".rodata", 0))
    if data:
        print()
        print_table("-- top 10 data symbols --", sorted(data, reverse=True), 10)
    print()
    print_table("-- top 10 bss symbols --", sorted(bss, reverse=True)[:10], 10)

    if args.objdir:
        print()
        print("-- per-object code size (pre --gc-sections) --")
        obj_syms = []
        total = 0
        for fname in sorted(os.listdir(args.objdir)):
            if not fname.endswith(".o"):
                continue
            path = os.path.join(args.objdir, fname)
            syms = parse_nm(nm_tool, path)
            code_bytes = sum(s[0] for s in syms if s[1] in code_types)
            ro_bytes = sum(s[0] for s in syms if s[1] in ro_types)
            obj_syms.append((code_bytes + ro_bytes, fname, code_bytes, ro_bytes))
            total += code_bytes + ro_bytes
        for size, fname, cb, rb in sorted(obj_syms, reverse=True):
            print("   %-24s text=%5d rodata=%5d  total=%5d" % (fname, cb, rb, size))
        print("   %-24s %d" % ("object total", total))

    print()
    print("note: buckets are based on symbol names; local static functions in the")
    print("      C library objects are counted as library code, and Rust inlining")
    print("      moves harness code into `main`, so treat buckets as indicative.")


if __name__ == "__main__":
    main()
