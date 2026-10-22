#!/usr/bin/env python3
"""Report per-function stack frame sizes from an ELF built with -Z emit-stack-sizes.

Usage:  python stack_sizes.py <elf> [--top N] [--filter substr]

Requires arm-none-eabi-nm (or llvm-nm) in PATH to map addresses to symbols.
Frame sizes are the static stack slot size LLVM reserved for each function; the
peak stack of a call chain is the sum of the frames on the deepest chain.
"""
import argparse
import shutil
import struct
import subprocess
import sys


def find_nm():
    for name in ("arm-none-eabi-nm", "llvm-nm", "nm"):
        path = shutil.which(name)
        if path:
            return path
    raise SystemExit("stack_sizes: no nm tool in PATH (need arm-none-eabi-nm or llvm-nm)")


def read_elf_section(path, section_name):
    """Minimal ELF32/64 little-endian section reader -> (bytes, endian)."""
    with open(path, "rb") as f:
        data = f.read()
    if data[:4] != b"\x7fELF":
        raise SystemExit("stack_sizes: not an ELF file")
    is64 = data[4] == 2
    endian = "<" if data[5] == 1 else ">"
    if is64:
        (e_shoff,) = struct.unpack_from(endian + "Q", data, 0x28)
        e_shentsize, e_shnum, e_shstrndx = struct.unpack_from(endian + "HHH", data, 0x3A)
    else:
        (e_shoff,) = struct.unpack_from(endian + "I", data, 0x20)
        e_shentsize, e_shnum, e_shstrndx = struct.unpack_from(endian + "HHH", data, 0x2E)
    sections = []
    for i in range(e_shnum):
        off = e_shoff + i * e_shentsize
        if is64:
            name, typ, flags, addr, offset, size = struct.unpack_from(endian + "IIQQQQ", data, off)
        else:
            name, typ, flags, addr, offset, size = struct.unpack_from(endian + "IIIIII", data, off)
        sections.append((name, typ, addr, offset, size))
    strtab_off = sections[e_shstrndx][3]
    for name_idx, typ, addr, offset, size in sections:
        end = data.index(b"\x00", strtab_off + name_idx)
        name = data[strtab_off + name_idx:end].decode("utf-8", "replace")
        if name == section_name:
            return data[offset:offset + size], endian, is64
    raise SystemExit("stack_sizes: section %s not found (build with -Z emit-stack-sizes)" % section_name)


def read_symbols(elf):
    nm = find_nm()
    out = subprocess.run([nm, "-S", "-C", "--defined-only", elf],
                         check=True, capture_output=True, text=True).stdout
    syms = []
    for line in out.splitlines():
        parts = line.split(None, 3)
        if len(parts) < 3:
            continue
        try:
            addr = int(parts[0], 16)
            size = int(parts[1], 16)
        except ValueError:
            # lines like "00000000 T symbol" have no size field
            try:
                addr = int(parts[0], 16)
            except ValueError:
                continue
            size = 0
        name = parts[3] if len(parts) > 3 else parts[2]
        syms.append((addr, size, name))
    syms.sort()
    return syms


def symbol_for(addr, syms):
    best = None
    for start, size, name in syms:
        if start <= addr and (size == 0 or addr < start + size):
            best = (start, size, name)
        elif start > addr:
            break
    return best[2] if best else "0x%08x" % addr


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("elf")
    ap.add_argument("--top", type=int, default=40)
    ap.add_argument("--filter", default="")
    args = ap.parse_args()

    raw, endian, is64 = read_elf_section(args.elf, ".stack_sizes")
    fmt = endian + ("QQ" if is64 else "II")
    step = 16 if fmt.endswith("QQ") else 8
    rows = []
    for off in range(0, len(raw) - step + 1, step):
        addr, size = struct.unpack_from(fmt, raw, off)
        if addr == 0:
            break
        rows.append((addr, size))
    syms = read_symbols(args.elf)
    named = [(size, symbol_for(addr, syms), addr) for addr, size in rows]
    named.sort(reverse=True)
    total = sum(s for s, _, _ in named)
    print("-- stack frames: %d functions, %d B of static stack slots --" % (len(named), total))
    shown = 0
    for size, name, addr in named:
        if args.filter and args.filter not in name:
            continue
        print("   %5d B  %s" % (size, name))
        shown += 1
        if shown >= args.top:
            break


if __name__ == "__main__":
    main()
