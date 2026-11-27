#!/usr/bin/env python3
"""Allocator Discovery & Coverage Audit for LidlessEye.

Answers one question before any native allocation hooking: *what does this pod
actually allocate through?*  A loaded libc proves nothing on its own -- C++ and
Rust routinely route around it -- so every verdict here is backed by an
observation (loaded object, LD_PRELOAD, or a symbol string in the binary) and
carries a confidence, never a claim.

The probe runs only shell builtins, `readlink`, `tr`, `grep` and `sort`, so it
works in distroless and busybox images alike. No host binary, no ptrace, no
execution of the target: reading `/proc/<pid>/maps` and grepping the mapped
objects is what a debugger would do, minus the side effects.

Usage:
  python tools/allocator-audit/audit.py --namespace disk --format md
  python tools/allocator-audit/audit.py --namespace disk --pod le-disk-lidlesseye-abc --json out.json
"""

from __future__ import annotations

import argparse
import csv
import json
import subprocess
import sys
from dataclasses import asdict, dataclass, field
from pathlib import Path

KUBECTL = ["kubectl"]

# Families we can name, most specific first: a preloaded allocator wins over
# libc even when both are mapped, because symbol interposition resolves to it.
FAMILIES = [
    ("jemalloc", "libjemalloc", "je_malloc"),
    ("tcmalloc", "libtcmalloc", "tc_malloc"),
    ("mimalloc", "libmimalloc", "mi_malloc"),
]

# The remote probe. One line per process, `|` separated; every field is a single
# line by construction (readlink/comm/tr/grep), so the parser never guesses.
PROBE = r"""
set -u
# Only processes that share our PID namespace. A pod with `hostPID` (our agent
# DaemonSet has it) can see every process on the node, and cgroups cannot
# separate them from inside a private cgroup namespace -- /proc/<pid>/cgroup is
# just "0::/" there. The PID namespace is the honest gate: it keeps the pod's own
# processes and drops the node's.
mine=$(readlink /proc/self/ns/mnt 2>/dev/null)
for p in /proc/[0-9]*; do
  pid=${p#/proc/}
  # A pod with `hostPID` shares the node's PID namespace, so the PID namespace
  # cannot tell its processes from kubelet's. The mount namespace can: container
  # processes live in their own, node daemons in the host's.
  ns=$(readlink "$p/ns/mnt" 2>/dev/null) || continue
  [ "$ns" = "$mine" ] || continue
  exe=$(readlink "$p/exe" 2>/dev/null) || continue
  comm=$(tr -d '\n' < "$p/comm" 2>/dev/null)
  # Only library lines: the map file itself is far too big for one line.
  libs=$(grep -Eo '/([^/]+/)*lib(jemalloc|tcmalloc|mimalloc|stdc\+\+|gcc_s|c\.so|m\.so|pthread)[^ ]*' "$p/maps" 2>/dev/null | sort -u | tr '\n' ',')
  preload=$(tr '\0' '\n' < "$p/environ" 2>/dev/null | grep -E '^LD_PRELOAD=' | head -1 | cut -d= -f2-)
  # `grep -c` prints 0 and exits 1 when nothing matches, so the exit status must
  # not be "fixed" with `|| echo 0` -- that appends a second line and splits the
  # record. ${x:-0} covers the case where the file itself is unreadable.
  je=$(grep -ac 'je_malloc' "$p/root$exe" 2>/dev/null)
  tc=$(grep -ac 'tc_malloc' "$p/root$exe" 2>/dev/null)
  mi=$(grep -ac 'mi_malloc' "$p/root$exe" 2>/dev/null)
  go=$(grep -ac 'runtime\.mheap' "$p/root$exe" 2>/dev/null)
  rust=$(grep -ac '__rust_alloc' "$p/root$exe" 2>/dev/null)
  new=$(grep -ac '_Znwm' "$p/root$exe" 2>/dev/null)
  printf 'REC|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s\n' "$pid" "${comm:-?}" "$exe" "${libs:-}" "${preload:-}" "${je:-0}" "${tc:-0}" "${mi:-0}" "${go:-0}" "${rust:-0}" "${new:-0}"
done
"""


@dataclass
class Finding:
    namespace: str
    pod: str
    container: str
    pid: int
    comm: str
    executable: str
    allocator: str
    linkage: str
    malloc_entry: str
    operator_new: str
    evidence: list[str] = field(default_factory=list)
    coverage: dict[str, str] = field(default_factory=dict)
    confidence: str = "low"


def kubectl(*args: str, stdin: str | None = None) -> str:
    """Run kubectl, returning stdout.

    The probe goes in as **bytes**: Python's text mode would rewrite the newlines
    of the script to CRLF on Windows, and a shell that sees `set -u\\r` reports
    "Illegal option" before running a single line of it.
    """
    proc = subprocess.run(
        [*KUBECTL, *args],
        input=stdin.encode("utf-8") if stdin is not None else None,
        capture_output=True,
    )
    if proc.returncode != 0:
        message = proc.stderr.decode("utf-8", "replace").strip()
        raise RuntimeError(message or f"kubectl {' '.join(args)} failed")
    return proc.stdout.decode("utf-8", "replace")


def probe_pod(namespace: str, pod: str, container: str | None) -> list[list[str]]:
    args = ["-n", namespace, "exec", "-i"] + ([f"-c{container}"] if container else [])
    args += [pod, "--", "sh", "-s"]
    out = kubectl(*args, stdin=PROBE)
    rows = []
    for line in out.splitlines():
        if line.startswith("REC|"):
            rows.append(line.split("|"))
    return rows


def classify(row: list[str], namespace: str, pod: str, container: str) -> Finding | None:
    _, pid, comm, exe, libs, preload, je, tc, mi, go, rust, new = row
    # The probe's own shell is always visible; auditing it would add one useless
    # "glibc / malloc" row per container.
    if comm in ("sh", "dash", "bash"):
        return None
    libs_list = [part for part in libs.split(",") if part]

    def has(fragment: str) -> bool:
        return any(fragment in path for path in libs_list)

    evidence: list[str] = []
    allocator = "unknown"
    entry = "unknown"
    confidence = "low"

    for name, lib, symbol in FAMILIES:
        if has(lib):
            allocator, entry = name, symbol
            evidence.append(f"{lib} mapped")
            confidence = "medium"
            break
    if allocator == "unknown" and has("libc.so"):
        allocator, entry = "glibc", "malloc"
        evidence.append("libc.so mapped, no alternative allocator")
        confidence = "medium"

    # LD_PRELOAD is the strongest signal: it decides symbol resolution before
    # any library the binary was linked against, so it outranks maps.
    if preload:
        evidence.append(f"LD_PRELOAD={preload}")
        lowered = preload.lower()
        for name, lib, symbol in FAMILIES:
            if lib in lowered or name in lowered:
                allocator, entry, confidence = name, symbol, "high"
                break

    # A symbol *string* inside the executable is weak evidence: it appears when
    # a library is statically linked, but also by accident (our own server embeds
    # the word for its allocator filter). It annotates, it does not decide.
    for name, lib, symbol, count in (
        ("jemalloc", "libjemalloc", "je_malloc", je),
        ("tcmalloc", "libtcmalloc", "tc_malloc", tc),
        ("mimalloc", "libmimalloc", "mi_malloc", mi),
    ):
        if count.isdigit() and int(count) > 0:
            evidence.append(f"{symbol} string in executable (weak)")

    if go.isdigit() and int(go) > 0:
        allocator, entry, confidence = "go-runtime", "runtime.mallocgc", "high"
        evidence.append("runtime.mheap string in executable")
    if rust.isdigit() and int(rust) > 0:
        evidence.append("__rust_alloc string in executable (GlobalAlloc may bypass libc)")

    linkage = "dynamic" if has("libc.so") else ("static (go)" if allocator == "go-runtime" else "static?")
    if allocator == "unknown" and not has("libc.so"):
        # No libc mapped and no allocator we recognise: the process may be
        # static, or the executable may be unreadable from this container.
        evidence.append("no libc in maps")

    operator_new = "unknown"
    if new.isdigit() and int(new) > 0:
        operator_new = (
            f"-> {entry}" if allocator not in ("unknown", "go-runtime") else "-> unknown"
        )
        evidence.append("_Znwm string in executable")

    coverage = {
        "malloc": "candidate" if entry != "unknown" else "unknown",
        "calloc": "candidate" if entry != "unknown" else "unknown",
        "realloc": "pending",
        "operator_new": "candidate" if operator_new != "unknown" else "unknown",
        "custom_allocator": "unknown",
    }
    return Finding(
        namespace=namespace,
        pod=pod,
        container=container or "",
        pid=int(pid),
        comm=comm or "?",
        executable=exe or "?",
        allocator=allocator,
        linkage=linkage,
        malloc_entry=entry,
        operator_new=operator_new,
        evidence=evidence,
        coverage=coverage,
        confidence=confidence,
    )


def targets(namespace: str, pod: str | None) -> list[tuple[str, str, list[str]]]:
    if pod:
        spec = json.loads(kubectl("-n", namespace, "get", "pod", pod, "-o", "json"))
        return [(pod, spec["metadata"]["uid"], [c["name"] for c in spec["spec"]["containers"]])]
    listing = json.loads(kubectl("-n", namespace, "get", "pods", "-o", "json"))
    out = []
    for item in listing["items"]:
        if item.get("status", {}).get("phase") != "Running":
            continue
        out.append(
            (
                item["metadata"]["name"],
                item["metadata"]["uid"],
                [c["name"] for c in item["spec"]["containers"]],
            )
        )
    return out


def render_markdown(findings: list[Finding]) -> str:
    lines = [
        "| workload | pid | allocator | linkage | malloc entry | operator new | confidence |",
        "|---|---|---|---|---|---|---|",
    ]
    for f in sorted(findings, key=lambda f: (f.pod, f.pid)):
        workload = f"{f.pod}/{f.comm}" if f.comm != "?" else f.pod
        lines.append(
            f"| {workload} | {f.pid} | {f.allocator} | {f.linkage} | "
            f"`{f.malloc_entry}` | {f.operator_new} | {f.confidence} |"
        )
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--namespace", default="default")
    parser.add_argument("--pod", default=None, help="audit one pod instead of the namespace")
    parser.add_argument("--format", choices=["md", "json", "csv"], default="md")
    parser.add_argument("--json", default=None, help="also write the raw findings here")
    parser.add_argument("--out", default=None, help="directory for json/csv artefacts")
    args = parser.parse_args()

    findings: list[Finding] = []
    for pod, uid, containers in targets(args.namespace, args.pod):
        for container in containers:
            try:
                rows = probe_pod(args.namespace, pod, container)
            except RuntimeError as err:
                print(f"skip {pod}/{container}: {err}", file=sys.stderr)
                continue
            for row in rows:
                if len(row) != 12:
                    continue
                finding = classify(row, args.namespace, pod, container)
                if finding:
                    findings.append(finding)

    if args.format == "md":
        print(render_markdown(findings))
    elif args.format == "json":
        print(json.dumps([asdict(f) for f in findings], indent=2))
    else:
        writer = csv.DictWriter(
            sys.stdout, fieldnames=list(asdict(findings[0]).keys()) if findings else ["pod"]
        )
        writer.writeheader()
        for f in findings:
            row = {k: (json.dumps(v) if isinstance(v, (list, dict)) else v) for k, v in asdict(f).items()}
            writer.writerow(row)

    if args.out:
        out = Path(args.out)
        out.mkdir(parents=True, exist_ok=True)
        (out / "findings.json").write_text(
            json.dumps([asdict(f) for f in findings], indent=2), encoding="utf-8"
        )
        with (out / "findings.csv").open("w", newline="", encoding="utf-8") as handle:
            writer = csv.DictWriter(
                handle, fieldnames=list(asdict(findings[0]).keys()) if findings else ["pod"]
            )
            writer.writeheader()
            for f in findings:
                row = {
                    k: (json.dumps(v) if isinstance(v, (list, dict)) else v)
                    for k, v in asdict(f).items()
                }
                writer.writerow(row)
        (out / "findings.md").write_text(render_markdown(findings) + "\n", encoding="utf-8")
        print(f"\nartefacts in {out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
