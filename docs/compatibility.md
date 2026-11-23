# Pyroscope compatibility

LidlessEye is a drop-in *backend* for Pyroscope-style profiling: the same push
protocol, the same tenant header, the same query concepts. It is not a
reimplementation of Pyroscope's distributed storage, and it does not ship
language SDKs — the collector is eBPF, in-process, and the storage is one local
DuckDB database per tenant, queried with SQL rather than a custom reader.

Everything below was verified against the code in this repository, not against a
roadmap. `partial` means "does the job, with a documented difference"; `no`
means it is genuinely absent.

## Ingestion

| Capability | Status | Notes |
| --- | --- | --- |
| JSON report push (`POST /ingest`) | yes | columnar schema, batches, gzip |
| pprof protobuf push | yes | `?format=pprof`, gzip transparent, per-sample labels become series labels |
| `stackcollapse` / folded text | yes | `?format=folded`, root-first input reversed to leaf-first |
| `Content-Type` based decoder selection | yes | `application/vnd.google.protobuf`, `text/plain` |
| Query-param overrides (`name`, `spy-name`, `sampleRate`) | partial | `name`/`service_name` and `spy-name`/`profile_type` work; `sampleRate` is accepted as a label-free hint only |
| OTLP profiles (`/v1development/profiles`) | no | tracked; the OTLP pprof body is the next decoder |
| JFR (`format=jfr`) | no | JFR needs a Java-specific parser; folded-text conversion on the agent side works today |
| pprof over gRPC | no | HTTP only |
| Multi-tenant header `X-Scope-OrgID` | yes | configurable name, case-insensitive ids, per-tenant tokens |
| Auth (bearer) | yes | global token, per-tenant tokens, allow-list |
| Auth (basic) | no | use a reverse proxy, or bearer tokens |
| Per-tenant ingestion rate limit | no | series cap and disk cap exist; a sample-rate limiter does not |

## Query

| Capability | Status | Notes |
| --- | --- | --- |
| Flame graph (flat + nested) | yes | `max_nodes` bounded, truncated nodes aggregated |
| Comparison / diff view | yes | `/api/v1/diff`, two windows in one tree |
| Time series | yes | proportional bucketing, `step` or `buckets`, `group_by` |
| Top table (self + total) | yes | `/api/v1/top`, `limit` bounded |
| Label names / values | yes | `/api/v1/label-names` (alias `/api/v1/labels`), `/api/v1/label/{name}/values` |
| Profile types | yes | includes the computed `wall` type |
| Series listing | yes | `/api/v1/series`, scoped to the tenant |
| `group_by` label | yes | synthetic `label=value` frames, or split series |
| Callers / callees ("sandwich") view | no | the top table covers the common case; a callers/callees endpoint is tracked |
| Export a selection as pprof | no | tracked (`GET /api/v1/profile`) |
| Exemplars / span profiles | no | needs tracing integration in the agent |
| Delete data by selector | no | retention and the disk cap are the only deletion paths |

## Collector (eBPF)

| Capability | Status | Notes |
| --- | --- | --- |
| CPU sampling at N Hz | yes | `--frequency`, default 99 Hz |
| off-CPU (blocked) sampling | yes | `--no-offcpu` disables it |
| Wall clock profile | yes | computed as `cpu + off_cpu`, not stored twice |
| Kernel stacks | yes | kernel frames symbolized from the live kernel |
| User stacks + symbolization | yes | ELF symbols, DWARF inline frames, Go `.gopclntab` and JIT perf maps; **stack depth needs frame pointers**, see below |
| Process discovery | yes | every process with a stack is reported, per-PID |
| Every pod on the node | yes | needs `hostPID: true`; samples are named by the pid of the agent's PID namespace, see [PID namespaces](architecture.md#pid-namespaces) |
| Kubernetes labels | yes | `enrich_kubernetes_labels`: `k8s_pod_uid`, `k8s_qos_class`, `k8s_container`, plus `node` — read from the cgroup path, no API access |
| Pod names and workloads | yes | `list pods` on the node (`k8s_namespace`, `k8s_pod`, `k8s_workload`, `k8s_workload_kind`) and the service name taken from the workload; needs the `pod-reader` ClusterRole, and does nothing outside a cluster |
| Node label | yes | `node_label` |
| PID exclusion | yes | `exclude_pids` |
| Container / label based filtering | no | exclusion is by PID only |
| Heap / allocation profiling | yes | two readers: Go heaps read (never written) from the runtime's own bucket chain, `runtime.mbuckets`, and every other language through uprobes on the allocator (`malloc`/`calloc`/`realloc`/`free` and the jemalloc, mimalloc, tcmalloc and musl equivalents). Emits `alloc_space`/`alloc_objects` (rate) and `inuse_space`/`inuse_objects` (level), labelled `profile_source`, `runtime` and `allocator`. `--no-alloc` and `--no-heap` turn either off, `--heap-objects` adds the object counts. Not covered: `mmap`-backed arenas, allocators that never call `malloc` (a custom `#[global_allocator]`), allocations made before the probe attached, and RSS/PSS — `inuse_*` is the sum of live blocks seen, not resident memory |
| Python 3.12+ CPU frames (perf trampoline) | yes | `PYTHONPERFSUPPORT=1` or `python -X perf` publishes `/tmp/perf-<pid>.map`, which the agent already reads; the official `python:3.12-*` image is enough to see Python frames. Build the interpreter with `CFLAGS=-fno-omit-frame-pointer -mno-omit-leaf-frame-pointer` for deeper stacks through CPython itself. Ruby remains SDK-driven |
| JVM JIT CPU frames (perf map) | yes | run with `-XX:+PreserveFramePointer -XX:+UnlockDiagnosticVMOptions -XX:+DebugNonSafepoints -XX:-Inline` and have perf-map-agent refresh `/tmp/perf-<pid>.map`; those JIT addresses then use the same perf-map reader as V8. JFR itself is still not decoded — push it through an SDK as pprof/folded instead |
| JVM / Python / Ruby heap profiles | no | no runtime heap reader except Go's; use an SDK that emits pprof or folded text. Native allocator uprobes do see `malloc` calls made by these runtimes, but that is not a language-level heap profile |

## Operations

| Capability | Status | Notes |
| --- | --- | --- |
| Single binary + embedded UI | yes | `rust-embed`; the export is built by `make ui` (Node 20+) and baked in, nothing ships beside the binary |
| Helm chart | yes | `deploy/helm/lidlesseye` |
| Docker image | yes | `deploy/docker` |
| Retention per tenant | yes | `--retention`, per-tenant override |
| Disk cap per tenant | yes | `--max-disk`, oldest-first eviction |
| Series cap per tenant | yes | `--tenant-limit <tenant>:max_series=N`, `429` on the write that would exceed it |
| Tenant cap | yes | `--max-tenants` |
| Read-only replica | yes | `--read-only` |
| Prometheus metrics | yes | `/metrics`, no tenant labels on purpose |
| Health / readiness probes | yes | `/api/v1/health`, `/api/v1/ready` |
| `fsck` with checksum verification | yes | checksums verified on every read, plus `lidlesseye fsck --fix` |
| Distributed / clustered ingestion | no | one writer per data directory; scale by running one instance per node |
| Object storage (S3/GCS/Azure) | no | local disk only, by design |
| Horizontal query fan-out | no | complicates the tenancy story more than it is worth for a single-node design |
| gRPC API | no | HTTP/JSON |

## How frames get their names

A flamegraph leaf that reads `0x7f3c1a0042a0` is not a rendering problem: the
agent sampled an address and could not name it. Five sources are consulted, in
this order, and each one closes a hole the previous one cannot:

| Source | Covers | Notes |
| --- | --- | --- |
| ELF symbol tables | native code | `.symtab`, `.dynsym`, split debug info under `/usr/lib/debug/.build-id` |
| DWARF `.debug_info` | inlined frames | a release build inlines aggressively, so the leaf often exists only as an inlined copy; those frames are expanded and emitted leaf first |
| Go `.gopclntab` | stripped Go binaries | survives `-ldflags="-s -w"`, because the Go runtime needs it to unwind and to print panics |
| JIT / trampoline perf maps | JVM, V8/Node, CPython 3.12+ | `/tmp/perf-<pid>.map`, found through `/proc/<pid>/root` and `NSpid` so it works across mount and PID namespaces. Producers are `node --perf-basic-prof-only-functions --interpreted-frames-native-stack`, perf-map-agent with `-XX:+PreserveFramePointer -XX:+UnlockDiagnosticVMOptions -XX:+DebugNonSafepoints -XX:-Inline`, and `PYTHONPERFSUPPORT=1` / `python -X perf` |
| `kallsyms` / `vmlinux` | kernel | read live; `vmlinux` is preferred when it is installed |

Two details decide whether a stack looks complete or full of holes:

- **Return addresses are off by one.** Only the innermost frame is an
  instruction that really executed; every frame below it is the address of the
  instruction *after* a `call`. Resolving it verbatim lands on whatever symbol
  starts at that boundary — often the wrong function, which is worse than an
  unknown frame because it looks plausible. Every frame but the leaf is resolved
  as `ip - 1`, the same convention `perf` uses.
- **`unknown` has reasons.** Unresolved addresses are counted by cause
  (`missing_syms`, `missing_component`, `unmapped`, `unknown_addr`,
  `unsupported`) and printed by `lidlesseye collect`, because "install debug
  info" and "this is a JIT region" are different problems.

Measured on a 20-vCPU node with containerd, dockerd and a stripped Go init
running, 12 s of whole-machine sampling at 199 Hz
(`crates/lidlesseye-ebpf/tests/symbol_coverage.rs`):

| Build | Frames without a symbol | Unresolved addresses |
| --- | --- | --- |
| ELF symbolization only | 16.1 % | 464 |
| + inline expansion, return-address fix, Go pclntab | 3.9 % | 4 |

What remains is `libc.so.6`: distributions strip it, so its internal helpers
(`__memmove_avx_unaligned_erms` and friends) have no symbol outside the debug
package. Installing `libc6-dbg` (or the matching `debuginfo` package) resolves
them; nothing on the agent side can.

### When a frame is still an address

The agent explains itself once a minute, at info level, so a cluster nobody can
shell into is still diagnosable from its logs:

```text
INFO symbol coverage named=2090 truncated=2468 go=661 elfless=232 unnamed=3 binaries=1 parsed=21 unreadable=0
WARN frames from this binary stay unnamed path=/usr/local/bin/lidlesseye go="-" route="proc-root" \
     symbols=0 go_table=false frames=3 cause="no ELF symbol table and no Go function table"
```

Each offending binary is followed by up to three samples — the pid, the
process name, the instruction pointer and the mapping `maps` returned for it —
so a report can be reproduced on the node without another round trip:

```text
INFO unnamed frame sample path=/usr/lib/x86_64-linux-gnu/libstdc++.so.6.0.30 pid=185768 comm=lidlesseye \
     ip=7a4b77a6bdf0 map=7a4b77a6b000-7a4b77b6c000+99000 dev=0:18e inode=310569 vaddr="99df0" \
     cause="the address is outside every ELF symbol"
```

The sample carries the mapping's permissions too. An `ip` in a mapping without
`x` — or in **no** mapping at all, which is labelled `[unmapped]` — is the
stack walk, not the symbols: a process built without frame pointers hands
`bpf_get_stackid` a broken chain, and the kernel reports whatever the chain
pointed at (a heap string, the value `2`, a block of machine code). Those
frames can never be named by any symbolizer, and `cause` says so instead of
sending the reader after a build flag that would not have helped.

The summary says how much was named and by which source (`go` is `.gopclntab`,
`elfless` is the module fallback for files blazesym cannot reach), how many
addresses stayed unnamed, how many frames were `truncated` — values a broken
frame-pointer walk produced, dropped instead of stored — how many binaries were
parsed and how many could not be opened at all. Each offending binary then gets one line naming the file, its
Go release when it has build info, and the cause — because the causes have
different fixes:

| Cause | What it means | Fix |
| --- | --- | --- |
| `the file could not be opened through any route` | the agent cannot see the binary: no `hostPID`, a too-narrow security context, or a runtime whose root filesystem it cannot traverse | give the DaemonSet `hostPID: true` and `privileged: true` (the chart's default) |
| `no ELF symbol table and no Go function table` | the binary was stripped at build time and is not Go | build with `strip = "debuginfo"` (Rust), keep `.symtab`, or install the `-dbg`/`debuginfo` package |
| `the address is outside the Go function table` | the Go table parsed but this PC is not in it: a stale cached build, or a `-buildmode=plugin` object | restart the agent to drop its module cache; report it if it persists |
| `the address is outside every ELF symbol` | same, for native code | as above |

Kernel stacks have their own line: when `/proc/kallsyms` is empty or reports
zeroed addresses, the agent warns once — `kptr_restrict=2` or a dropped
`CAP_SYSLOG` hides every kernel symbol, and no user-side work can recover them.

**Go version is not one of the causes.** The pclntab reader is exercised against
Go 1.18, 1.20, 1.22, 1.23, 1.24 and 1.25, each built plain, `-ldflags="-s -w"`
and `-buildmode=pie -ldflags="-s -w"`, on amd64 and arm64
(`LIDLESSEYE_GO_FIXTURES=<dir> cargo test -p lidlesseye-ebpf --lib real_go_binaries`).
The header layout has been stable at magic `0xfffffff1` since Go 1.20, and the
three older layouts (1.16–1.19) are handled as well.

One limitation is worth stating plainly: samples from a process in **another
PID namespace** (another container, when the agent runs with `hostPID: true`)
are counted and dropped. `bpf_get_ns_current_pid_tgid` only answers for tasks
that share the agent's namespace, and a plain `bpf_get_current_pid_tgid` numbers
a task in *its own* namespace — resolving that to a `/proc` entry would silently
attribute the samples to an unrelated process. `lidlesseye collect` reports the
number as `in another PID namespace`.

Addresses in **anonymous memory** are a separate bucket, reported as
`[anonymous]`. That is where a JIT runtime puts its code: without a perf map
there is no symbol table to consult, and the address is emitted as-is. The
producer is runtime-specific — `node --perf-basic-prof-only-functions
--interpreted-frames-native-stack`, perf-map-agent with
`-XX:+PreserveFramePointer -XX:+UnlockDiagnosticVMOptions
-XX:+DebugNonSafepoints -XX:-Inline`, or CPython 3.12+ with
`PYTHONPERFSUPPORT=1` — and
the agent always reads the resulting `/tmp/perf-<pid>.map` through
`/proc/<pid>/root`. When none is published, `[anonymous]` is the honest answer:
it means "ask the runtime to publish its symbols", not "install debuginfo".

## What the differences mean in practice

- **Storage.** Pyroscope ships blocks to object storage and compacts them;
  LidlessEye writes one transactional batch per report into a local DuckDB file.
  A query is an aggregation over `samples` filtered by time and profile type,
  with zone maps skipping row groups outside the window. There is no compaction
  step and no background rewrite — but also no cross-node deduplication and no
  cold tier, and the file is single-writer.
- **Scaling.** Pyroscope scales horizontally into microservices. LidlessEye
  scales by running one instance per node (or per tenant group) — which is what
  an eBPF agent needs anyway, since the profiles are node-local.
- **Frame pointers.** The kernel unwinder walks the user stack through frame
  pointers, exactly like Pyroscope's eBPF collector. Code built without them
  ends the walk at the leaf, so a sample that lands inside that code is
  attributed to the leaf function instead of to its caller. The total CPU time
  is unaffected — no samples are lost — but the flamegraph is shallower.

  Measured on `crates/lidlesseye-ebpf/tests/ebpf_e2e.rs`, a 6-second spin in one
  thread, 199 Hz, `libc`-measured thread CPU time as the oracle:

  | Build | Time reaching the spinning function | Total accounted |
  | --- | --- | --- |
  | `RUSTFLAGS="-C force-frame-pointers=yes"` | 5.71 s of 6.00 s (95 %) | 6.33 s |
  | default | 1.22 s of 6.00 s (20 %) | 6.16 s |

  For Rust, that means `-C force-frame-pointers=yes` in `RUSTFLAGS` (or
  `force-frame-pointers = true` in `.cargo/config.toml`) for the *whole* graph
  including `std` — the precompiled `std` in the toolchain is built without
  frame pointers, so samples landing in it still stop at the leaf. The same
  applies to C/C++ (`-fno-omit-frame-pointer`) and to JITs, which is why
  Pyroscope documents the flag too.

- **SDKs.** Anything that speaks pprof or folded text can push to LidlessEye
  today, including the Pyroscope SDKs pointed at `LIDLESSEYE_SERVER`/`--server`.
  There is no LidlessEye SDK, and none is planned: the point is the agent.

## Verifying a claim

Every `yes` in this document has a test behind it:

```bash
cargo test --workspace            # unit + integration
cargo test -p lidlesseye-server --test round1   # boundaries and errors
cargo test -p lidlesseye-server --test round2   # security, quotas, formats
cargo test -p lidlesseye-server --test round3   # volume, concurrency, integrity
cargo test -p lidlesseye --test cli             # the shipped binary
cargo test -p lidlesseye --test embedded        # the crate API, in-process
```

### Every language, in one cluster

`deploy/test/multilang/k8s.yaml` starts long-running Deployments — one per
symbolization route — so they can be left in the cluster as a permanent test
fixture. Each pod compiles its fixture from a ConfigMap at start-up, so no
private image build is needed; the agent still needs its chart defaults of
`hostPID` and `privileged`. Apply it with `kubectl apply -f
deploy/test/multilang/k8s.yaml`, leave the pods running, and query
`/api/v1/flamegraph` or `/api/v1/top` with `profile_type=cpu`,
`alloc_space` or `inuse_space`.

The fixtures deliberately burn CPU **and** allocate: Go through `make`/`new`,
C/C++/Rust through `malloc`/`Box`, Python through large `bytes` objects and
Node through `Buffer.allocUnsafe`. That gives every supported memory reader
something to see. Java stays CPU-only on purpose.

| Pod | CPU frames | Memory route | Memory | Notes |
| --- | --- | --- | --- | --- |
| `multilang-go` | `main.hotA` / `main.hotB` (pclntab + ELF) | Go runtime heap (`runtime.mbuckets`) | yes | `alloc_space` / `inuse_space`, plus object counts with `--heap-objects`. The reader is verified against Go processes already in the cluster (`/app/goheap`, Prometheus with `runtime.mallocgc`); use a wide enough window to catch the fixture's ~4 KiB allocations. Go 1.23 is used because that is the reader's validated layout. |
| `multilang-c` | `c_hot_leaf` / `c_hot_mid` / `c_hot_top` (ELF) | allocator uprobes on `malloc`/`free` | yes | `alloc_space` / `inuse_space`; 256 live 4 KiB blocks cover the level metric. |
| `multilang-cpp` | `CppHot::leaf` / `CppHot::mid` / `CppHot::top` (mangled ELF) | allocator uprobes on `malloc`/`free` | yes | `CppHot::mid` / `CppHot::top` were observed in `alloc_space` during the cluster run. |
| `multilang-rust` | `rust_hot_leaf` / `rust_hot_mid` / `rust_hot_top` (`-C force-frame-pointers=yes`) | allocator uprobes via `Box<[u8; 4096]>` → system malloc | yes | 256 live boxes. `rust_hot_top` is often absent from a short top-N sample, not a symbolization failure. |
| `multilang-python` | `py_hot_leaf` / `py_hot_mid` / `py_hot_top` (perf trampoline) | allocator uprobes, large allocations only | partial | `bytes(4096)` goes through `malloc`; CPython's small-object `pymalloc` arenas come from `mmap` and are invisible. No runtime-level Python heap reader. |
| `multilang-node` | `nodeHotLeaf` / `nodeHotMid` / `nodeHotTop` (perf map) | allocator uprobes, external buffers only | partial | `Buffer.allocUnsafe` allocates outside V8; the V8 managed heap is `mmap`-backed and not read. No runtime-level Node heap reader. |
| `multilang-java` | `javaHotLeaf` / `javaHotMid` / `javaHotTop` (perf map) | — | no | JVM TLAB/heap allocations never call `malloc` per object, and JFR is not decoded. Native `malloc` from JVM internals may appear, but that is not a Java heap profile; use an SDK that emits pprof/folded. |

"Memory = yes" means `alloc_space` / `inuse_space` frames carry the language's
own function names. "Partial" means only the malloc-backed part is visible.
The table describes each route's capability; a short top-N window will not
always contain every fixture's leaf frames. None of these columns is RSS or
PSS; see
[USAGE.md](USAGE.md#memory-and-allocation-profiling) for why `inuse_space` is a
level, not a working set.

The agent's `symbol coverage` line is the second CPU oracle: `go=` counts
pclntab names and `unnamed` must not grow while these pods are running.
Perf-map names arrive with module `[jit]` in the flamegraph; they are tallied
internally but are not broken out on that log line yet.

The eBPF programs are tested against a real kernel, not a mock, which needs a
Linux host and root. Docker is enough:

```bash
# load every program, sample and symbolise this process, check the numbers
DEV_PRIVILEGED=1 DEV_ENV="RUSTFLAGS=-C force-frame-pointers=yes" \
    ./scripts/dev.sh cargo test -p lidlesseye-ebpf --test ebpf_e2e \
    -- --ignored --nocapture
```

Symbolization has a coverage test of its own. It samples the whole machine and
fails if more than a quarter of the frames come back as bare addresses:

```bash
DEV_PRIVILEGED=1 ./scripts/dev.sh cargo test -p lidlesseye-ebpf \
    --test symbol_coverage -- --ignored --nocapture
```

The dashboard has its own browser test, which also needs nothing but Docker:

```bash
./scripts/ui-smoke.sh    # Chromium: every tab, the tenant switch, the label explorer
```

It seeds two tenants, drives the UI and fails on any console error or on a
request that returns non-2xx, so "the page renders" is checked against real
API responses rather than a fixture.
