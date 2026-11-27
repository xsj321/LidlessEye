# Allocator Discovery & Coverage Audit

Answers the question every native-memory feature must answer first: **what does
this workload actually allocate through?**

A loaded `libc.so.6` proves nothing. C++ routes through `operator new`, Rust can
define its own `GlobalAlloc`, jemalloc can be preloaded, and a stripped static
binary shows no libraries at all. So each verdict here is backed by an
observation and carries a confidence instead of a claim.

## What it observes

| Signal | How | Weight |
|---|---|---|
| Loaded allocator objects | `/proc/<pid>/maps`, filtered to `lib*` lines | decides the family |
| `LD_PRELOAD` | `/proc/<pid>/environ` | strongest: interposition wins over linkage |
| Static linkage | no `libc.so` in maps | `static?` unless a Go marker is present |
| Go runtime | `runtime.mheap` string in the executable | `go-runtime`, high |
| Rust `GlobalAlloc` | `__rust_alloc` string in the executable | evidence only |
| C++ `operator new` | `_Znwm` string in the executable | evidence only |
| Allocator symbol strings | `je_malloc`, `tc_malloc`, `mi_malloc` in the executable | **weak** — annotates, never decides |

The probe runs only `readlink`, `tr`, `grep`, `sort` and a `for` loop, so it works
in distroless, Alpine and busybox images alike. It never executes the target and
never attaches to it: reading `/proc` is what a debugger would do, minus the
side effects.

## Two namespaces that bite

- **`hostPID` pods** (our agent DaemonSet) see every process on the node. Cgroups
  cannot separate them from inside a private cgroup namespace — `/proc/<pid>/cgroup`
  is just `0::/`. The **mount namespace** is the honest gate: container processes
  live in their own, node daemons in the host's.
- **PID reuse** is why records carry the PID *and* the comm/exe: a verdict is about
  a process, not a number.

## Usage

```powershell
python tools/allocator-audit/audit.py --namespace disk --format md
python tools/allocator-audit/audit.py --namespace disk --pod le-disk-lidlesseye-abc --format json
python tools/allocator-audit/audit.py --namespace disk --out out/audit
```

`--out` writes `findings.json`, `findings.csv` and `findings.md`. The CSV is the
input for the coverage matrix; the JSON keeps the evidence list per process.

## Reading the output

- `candidate` means "this entry exists and is reachable", never "verified": proof
  of an actual call path needs a probe (uprobe on the entry, or a benchmarked hook).
- `static?` means no `libc.so` was mapped **and** no Go marker was found — a static
  binary or an executable unreadable from this container. Both need a second look,
  neither is a verdict.
- A row whose allocator disagrees with the image is worth reading twice: the first
  audit found an orphaned `lidlesseye` process still mapped against
  `libjemalloc.so.2`, a library the image no longer ships.

## Entry resolution (task B2): `entry.py`

`audit.py` says *which allocator is loaded*. `entry.py` says *where a process's
`malloc` actually goes*, by reading the four things that can disagree:

| Check | What it proves |
|---|---|
| dynamic symbols | the executable imports `malloc` at all |
| relocations | there is a `JUMP_SLOT` (lazy PLT) or `GLOB_DAT` (`-fno-plt`/RELRO) slot |
| runtime GOT | the address the slot holds right now, read from `/proc/<pid>/mem` |
| ownership | which mapped module and which function that address belongs to |

Verdicts: `resolved-to-libc`, `interposed:<module>`, `lazy-unbound` (slot still
points at the caller's resolver stub), `unresolved` (zero), `unknown`, or
`no dynamic allocation entry` — the last one is a real answer: a Go or static
binary never calls the dynamic `malloc`, so hooking it would cover nothing.

It must run where the target processes are visible, with `CAP_SYS_PTRACE`:

```powershell
kubectl apply -f target\tmp\audit-b2.yaml     # hostPID + privileged + SYS_PTRACE
kubectl -n disk cp tools\allocator-audit\entry.py audit-b2:/tmp/entry.py
kubectl -n disk exec audit-b2 -- python3 /tmp/entry.py --kubepods --format md
```

First run on this cluster (60 processes): 62 slots `resolved-to-libc`, 6
`interposed:libstdc++.so.6.0.30` (C++ `operator new` on Node.js — the exact
false positive the static audit can only suspect), zero unreadable GOT slots.

## Hook cost: baseline, hook-only, hook + stack

`deploy/bpftime/victim.c` (a 200 000-iteration `malloc(64)`/`free` loop, built
dynamically so bpftime can rewrite its PLT) was run inside the cluster pod
`hookbench`, with the loader and the target in the same mount namespace:

| case | wall | ns per alloc+free | ns per hook |
|---|---|---|---|
| baseline, nothing attached | 1.2 ms | **3** | — |
| hook-only (`alloc_nostack.bpf.c`: count, no stack) | 41.7 ms | **104** | ≈ 50 |
| hook + user stack (`alloc.bpf.c`) | 457.0 ms | **1143** | ≈ 570 |

So the uprobe entry costs ~50 ns and walking plus hashing the user stack costs
~520 ns on top — the stack is 90 % of the per-call cost. That is the number the
sampling budget has to be built around: at a 1-in-64 sample the average cost per
allocation is `50 + 520/64 ≈ 58 ns`, still ~19× the tcache-fast baseline of 3 ns.
A 64-byte `malloc`/`free` pair is the cheapest possible workload, so the relative
overhead falls as real allocations get more expensive; the absolute numbers are
what matter for the budget.

Both numbers come from `bpftime attach` in the *same* pod as the target. Attaching
across pods still fails to deliver samples (the loader's maps stay empty when it
is started through `nsenter` into another mount namespace, see the bpftime
change report), so an integrated benchmark has to wait for that fix.
