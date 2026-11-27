#!/usr/bin/env python3
"""Allocation entry resolution (task B2).

The audit in ``audit.py`` answers "which allocator is loaded". This answers the
harder question: **where does this process's ``malloc`` actually go?**

Four checks, each of which can fail on its own:

  1. dynamic symbols   - does the executable import ``malloc`` at all?
  2. relocations       - is there a JUMP_SLOT relocation for it?
  3. runtime GOT       - what address does the GOT slot hold *right now*?
  4. ownership         - which mapped file and which function is that address?

Static parsing alone only ever proves "candidate". Only step 3/4 read the real
resolution, and they need ``/proc/<pid>/mem``, so they must run where the target
process is visible: a ``hostPID: true`` pod with ``CAP_SYS_PTRACE``.

Verdicts
    resolved-to-libc   GOT points at libc.so.6 and the symbol is a malloc-family
    interposed:<name>  GOT points at another module (LD_PRELOAD, tcmalloc, ...)
    lazy-unbound       GOT still points into the caller's PLT resolver stub
    unresolved         GOT slot is zero / unmapped
    unknown            we could not read the ELF, the GOT or the owner

Usage (inside a hostPID pod, or on the node):
    entry.py --pid 1234
    entry.py --ns-filter lidlesseye-agent     # only pids whose cgroup matches
    entry.py --all --format json --out dir
"""

from __future__ import annotations

import argparse
import json
import os
import struct
import sys

MALLOC_FAMILY = {
    "malloc", "calloc", "realloc", "reallocarray", "free", "cfree",
    "aligned_alloc", "posix_memalign", "memalign", "valloc", "pvalloc",
    "malloc_usable_size", "malloc_trim", "mallopt",
}
CXX_FAMILY = {
    "_Znwm", "_Znam", "_ZdlPv", "_ZdaPv", "_ZdlPvm", "_ZdaPvm",
    "_ZnwmSt11align_val_t", "_ZdlPvSt11align_val_t",
}

SHT_DYNSYM, SHT_RELA, SHT_REL = 11, 4, 9
DT_NULL, DT_PLTRELSZ, DT_PLTREL, DT_JMPREL = 0, 2, 20, 23
STT_FUNC, STT_GNU_IFUNC = 2, 10


def _u(fmt: str, data: bytes, off: int):
    return struct.unpack_from(fmt, data, off)


class Elf:
    """Just enough ELF64 to read dynsym, PLT relocations and load bias."""

    def __init__(self, path: str):
        with open(path, "rb") as handle:
            self.data = handle.read()
        if self.data[:4] != b"\x7fELF" or self.data[4] != 2:
            raise ValueError("not an ELF64 file")
        (self.shoff,) = _u("<Q", self.data, 0x28)
        (self.shentsize,) = _u("<H", self.data, 0x3A)
        (self.shnum,) = _u("<H", self.data, 0x3C)
        (self.phoff,) = _u("<Q", self.data, 0x20)
        (self.phentsize,) = _u("<H", self.data, 0x36)
        (self.phnum,) = _u("<H", self.data, 0x38)
        self.sections = []
        for index in range(self.shnum):
            off = self.shoff + index * self.shentsize
            (name, stype, _flags, _addr, soff, size, link, info, _align,
             entsize) = _u("<IIQQQQIIQQ", self.data, off)
            self.sections.append({
                "name_off": name, "type": stype, "offset": soff, "size": size,
                "link": link, "info": info, "entsize": entsize,
            })
        self.shstrtab = self._string_table(
            self.sections[next(i for i, s in enumerate(self.sections)
                               if s["type"] == 3)]["offset"])

    def _string_table(self, offset: int) -> bytes:
        end = self.data.index(b"\x00", offset)
        # dynstr/shstrtab run to the next section start; read generously
        tail = self.data[offset:]
        stop = tail.find(b"\x00\x00\x00\x00")
        return tail[: stop + 1] if stop > 0 else tail

    def _str(self, table: bytes, off: int) -> str:
        end = table.index(b"\x00", off)
        return table[off:end].decode("utf-8", "replace")

    def dynsym(self):
        sym = next((s for s in self.sections if s["type"] == SHT_DYNSYM), None)
        if sym is None:
            return []
        strs = self.data[self.sections[sym["link"]]["offset"]:] \
            if sym["link"] < len(self.sections) else b""
        out = []
        count = sym["size"] // 24 if sym["entsize"] == 24 else 0
        for index in range(count):
            off = sym["offset"] + index * 24
            (name, info, _other, shndx, value, size) = _u("<IBBHQQ", self.data, off)
            end = strs.index(b"\x00", name)
            out.append({
                "name": strs[name:end].decode("utf-8", "replace"),
                "type": info & 0xF, "bind": info >> 4,
                "shndx": shndx, "value": value, "size": size,
            })
        return out

    def plt_relocations(self):
        """GOT slots that resolve to a function: JUMP_SLOT and GLOB_DAT.

        A lazily bound PLT uses R_X86_64_JUMP_SLOT; a binary built with
        ``-fno-plt`` or full RELRO binds the same pointers eagerly through
        R_X86_64_GLOB_DAT. Both end up in the GOT, so both are read.
        """
        out = []
        for section in self.sections:
            if section["type"] not in (SHT_RELA, SHT_REL):
                continue
            rel_size = 24 if section["type"] == SHT_RELA else 16
            count = section["size"] // rel_size if section["entsize"] in (0, rel_size) else 0
            for index in range(count):
                off = section["offset"] + index * rel_size
                r_offset, r_info = _u("<QQ", self.data, off)
                r_type = r_info & 0xFFFFFFFF
                r_sym = r_info >> 32
                if r_type == 7:        # R_X86_64_JUMP_SLOT
                    kind = "jump-slot"
                elif r_type == 6:      # R_X86_64_GLOB_DAT
                    kind = "glob-dat"
                else:
                    continue
                out.append({"got_offset": r_offset, "symbol": r_sym,
                            "kind": kind})
        return out

    def text_segments(self):
        """(vaddr_start, vaddr_end, offset_of_that_segment) for PT_LOAD segments."""
        out = []
        for index in range(self.phnum):
            off = self.phoff + index * self.phentsize
            (p_type, p_flags, p_offset, p_vaddr, _paddr, p_filesz, _memsz,
             _align) = _u("<IIQQQQQQ", self.data, off)
            if p_type == 1:
                out.append({
                    "vaddr": p_vaddr, "size": p_filesz, "offset": p_offset,
                    "exec": bool(p_flags & 1),
                })
        return out


def parse_maps(pid: int):
    regions = []
    with open(f"/proc/{pid}/maps", "r", encoding="utf-8", errors="replace") as handle:
        for line in handle:
            parts = line.split(maxsplit=5)
            if len(parts) < 6:
                continue
            start, end = (int(x, 16) for x in parts[0].split("-"))
            offset = int(parts[2], 16)
            path = parts[5].strip()
            if not path.startswith("/"):
                continue
            regions.append({"start": start, "end": end, "offset": offset,
                            "perms": parts[1], "path": path})
    return regions


def region_of(regions, addr: int):
    for region in regions:
        if region["start"] <= addr < region["end"]:
            return region
    return None


_elf_cache: dict[str, Elf] = {}


def elf_for(pid: int, path: str) -> Elf | None:
    """Parse the ELF *as the target sees it*.

    The probe pod has its own libc, and a symbol read from that copy would belong
    to a different build than the one the target mapped: the offsets would not
    line up. Read through ``/proc/<pid>/root`` first, fall back to the plain path
    only when the target's root is not readable.

    The cache is keyed by (pid, path) on purpose: two pods can map the same
    library path from different base images, and reusing the first parse would
    attribute the second pod's addresses to the wrong build.
    """
    key = f"{pid}:{path}"
    if key in _elf_cache:
        return _elf_cache[key]
    elf = None
    for candidate in (f"/proc/{pid}/root{path}", path):
        try:
            elf = Elf(candidate)
            break
        except (OSError, ValueError, IndexError):
            continue
    _elf_cache[key] = elf
    return elf


def owner_of(pid: int, addr: int, regions):
    """(module, symbol) that contains addr, by mapping then symbol table."""
    region = region_of(regions, addr)
    if region is None:
        return None, None
    offset = addr - region["start"] + region["offset"]
    elf = elf_for(pid, region["path"])
    best = None
    if elf is not None:
        try:
            for symbol in elf.dynsym():
                if symbol["type"] not in (STT_FUNC, STT_GNU_IFUNC):
                    continue
                if symbol["value"] == 0 or not symbol["name"]:
                    continue
                size = symbol["size"] or 0
                if symbol["value"] <= offset and (
                    offset < symbol["value"] + size if size else True
                ):
                    if best is None or symbol["value"] > best["value"]:
                        best = symbol
        except (OSError, ValueError, IndexError):
            best = None
    return region["path"], (best["name"] if best else None)


def read_quad(pid: int, addr: int):
    try:
        with open(f"/proc/{pid}/mem", "rb") as handle:
            handle.seek(addr)
            return struct.unpack("<Q", handle.read(8))[0]
    except (OSError, struct.error):
        return None


def resolve(pid: int, syms: set[str]) -> dict:
    result = {"pid": pid, "verdicts": {}, "limits": [],
              "cgroup": cgroup_of(pid).strip()}
    try:
        with open(f"/proc/{pid}/comm") as handle:
            result["comm"] = handle.read().strip()
        result["exe"] = os.readlink(f"/proc/{pid}/exe")
    except OSError as error:
        result["limits"].append(f"cannot read process: {error}")
        return result

    regions = parse_maps(pid)
    exe_region = next((r for r in regions if r["path"] == result["exe"]
                       and r["offset"] == 0), None)
    if exe_region is None:
        result["limits"].append("executable has no offset-0 mapping")
        return result

    elf = elf_for(pid, result["exe"])
    if elf is None:
        result["limits"].append("cannot parse the executable")
        return result

    segments = elf.text_segments()
    header = next((s for s in segments if s["offset"] == 0), None)
    if header is None:
        result["limits"].append("executable has no PT_LOAD at offset 0")
        return result
    # Load bias: the mapping at file offset 0 starts at (link-time vaddr + bias),
    # so an ET_EXEC binary gets bias 0 and a PIE gets its address-space base.
    bias = exe_region["start"] - header["vaddr"]

    try:
        symbols = elf.dynsym()
        relocations = elf.plt_relocations()
    except (OSError, ValueError, IndexError) as error:
        result["limits"].append(f"cannot parse dynamic tables: {error}")
        return result

    imported = {s["name"] for s in symbols if s["name"] in syms}
    result["imports"] = sorted(imported)

    text = [(seg["vaddr"] + bias, seg["vaddr"] + bias + seg["size"])
            for seg in segments if seg["exec"]]

    for relocation in relocations:
        index = relocation["symbol"]
        if index >= len(symbols):
            continue
        name = symbols[index]["name"]
        if name not in syms:
            continue
        got = bias + relocation["got_offset"]
        value = read_quad(pid, got)
        entry = {"got": got, "kind": relocation["kind"]}
        if value is None:
            entry["verdict"] = "unknown"
            result["limits"].append("cannot read /proc/<pid>/mem (permission)")
        elif value == 0:
            entry["verdict"] = "unresolved"
        elif any(start <= value < end for start, end in text):
            # The slot still holds the lazy resolver stub inside our own text.
            entry["verdict"] = "lazy-unbound"
        else:
            module, symbol = owner_of(pid, value, regions)
            entry["value"] = value
            entry["module"] = module
            entry["symbol"] = symbol
            if module and module.endswith("libc.so.6"):
                entry["verdict"] = "resolved-to-libc"
            elif module:
                entry["verdict"] = f"interposed:{os.path.basename(module)}"
            else:
                entry["verdict"] = "unknown"
        result["verdicts"][name] = entry

    preload = []
    try:
        with open(f"/proc/{pid}/environ", "rb") as handle:
            for item in handle.read().split(b"\x00"):
                if item.startswith(b"LD_PRELOAD="):
                    preload = item.split(b"=", 1)[1].decode().split(":")
    except OSError:
        result["limits"].append("cannot read environments")
    result["preload"] = [p for p in preload if p]
    if not result["verdicts"]:
        # No GOT slot for any allocation entry. That is a finding, not a gap:
        # a Go binary allocates through its runtime, a static binary through
        # its own copy - hooking the dynamic `malloc` would cover neither.
        result["note"] = ("no dynamic allocation entry: the process does not "
                          "import malloc/calloc/realloc/operator new")
    return result


def cgroup_of(pid: int) -> str:
    try:
        with open(f"/proc/{pid}/cgroup") as handle:
            return handle.read()
    except OSError:
        return ""


def pids(filter_text: str | None, kubepods: bool = False):
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        pid = int(name)
        cgroup = cgroup_of(pid)
        if kubepods and "kubepods" not in cgroup:
            continue
        if filter_text and filter_text not in cgroup:
            continue
        yield pid


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=int, action="append", default=[])
    parser.add_argument("--ns-filter", default=None,
                        help="only pids whose cgroup contains this text")
    parser.add_argument("--all", action="store_true", help="every process")
    parser.add_argument("--kubepods", action="store_true",
                        help="only processes inside kubepods cgroups")
    parser.add_argument("--format", choices=["md", "json"], default="md")
    parser.add_argument("--out", default=None)
    args = parser.parse_args()

    targets = args.pid or (list(pids(args.ns_filter, args.kubepods))
                              if (args.all or args.ns_filter or args.kubepods) else [])
    if not targets:
        parser.error("give --pid, or --all / --ns-filter")

    syms = MALLOC_FAMILY | CXX_FAMILY
    rows = []
    for pid in targets:
        try:
            rows.append(resolve(pid, syms))
        except Exception as error:  # noqa: BLE001 - a probe reports, never crashes
            rows.append({"pid": pid, "limits": [f"probe failed: {error}"],
                         "verdicts": {}})

    if args.format == "json":
        text = json.dumps(rows, indent=2)
    else:
        lines = ["| pid | process | symbol | verdict | module | symbol |",
                 "|---|---|---|---|---|---|"]
        for row in rows:
            if not row.get("verdicts"):
                # A process with no dynamic allocation entry is itself the
                # answer: hooking malloc would cover nothing.
                note = row.get("note") or row.get("limits", ["nothing parsed"])[0]
                lines.append(f"| {row['pid']} | {row.get('comm', '?')} | n/a | "
                             f"{note} | | |")
            for name, entry in sorted(row.get("verdicts", {}).items()):
                lines.append(
                    f"| {row['pid']} | {row.get('comm', '?')} | `{name}` | "
                    f"{entry['verdict']} | {entry.get('module', '')} | "
                    f"{entry.get('symbol', '')} |")
            for limit in row.get("limits", []):
                lines.append(f"| {row['pid']} | {row.get('comm', '?')} | n/a | "
                             f"limit: {limit} | | |")
        text = "\n".join(lines)

    if args.out:
        os.makedirs(args.out, exist_ok=True)
        path = os.path.join(args.out, "entries." + args.format)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text + "\n")
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
