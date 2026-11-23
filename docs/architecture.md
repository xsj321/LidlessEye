# Architecture

## Crates

```
lidlesseye-core      data model, DuckDB store, query engine, selectors
lidlesseye-ebpf      aya loader, BPF C programs, collector loop, symbolization
lidlesseye-server    axum router, HTTP handlers, embedded UI, metrics
lidlesseye           the embeddable `LidlessEye` builder + the `lidlesseye` CLI
```

The dependency graph is a straight line: `core` knows nothing about eBPF,
`ebpf` and `server` both depend on `core`, and the top-level crate wires them
together. That is what makes both of these possible at the same time:

- `lidlesseye serve --agent` — everything in one process;
- `lidlesseye collect --server …` — the agent alone, pushing to a remote server.

## Data flow

```
perf_event(CPU_CLOCK)  ─┐
kprobe:schedule         ├─► BPF hash maps ──► drain (flush interval) ──► symbolize
kprobe:finish_task_switch ┘   (tgid, ustack,  (user space thread)        (blazesym)
                               kstack) -> n
                                                     │
                                                     ▼
                                          Report { strings, stacks }
                                                     │
                                       ┌─────────────┴─────────────┐
                                       ▼                           ▼
                                direct ingest                 POST /ingest
                                (same process)                (remote agent)
                                       └─────────────┬─────────────┘
                                                     ▼
                                     profile batch per (type, service, labels, window)
                                                     │
                                                     ▼
                                       query engine (SQL over samples)
```

## Sampling model

### On-CPU

One `perf_event_open` with `PERF_COUNT_SW_CPU_CLOCK` per online CPU, in
`PerfEventScope::AllProcessesOneCpu`, at `frequency_hz` (default 99). Every hit
runs `lidlesseye_cpu`, which captures the user stack and the kernel stack with
`bpf_get_stackid` into a `BPF_MAP_TYPE_STACK_TRACE` map and bumps a counter in a
`BPF_MAP_TYPE_HASH` keyed by `(tgid, user_stack_id, kernel_stack_id)`.

A count is worth `1e9 / frequency_hz` nanoseconds of CPU time; user space
multiplies the counter by that period when it drains the map. Nothing is
sampled twice, nothing is stored per event beyond an `__sync_fetch_and_add`.

### Off-CPU

- `kprobe:schedule` — the task is still `current`, so
  `bpf_get_current_pid_tgid` identifies who is about to sleep. The program
  records `(start_ns, stack_id)` in `offcpu_inflight` keyed by `tgid`.
- `kprobe:finish_task_switch` — `current` is now the task that just resumed.
  If it has an inflight entry, `now - start` nanoseconds are added to
  `offcpu_totals` under the *blocking* stack, and the entry is removed.

Intervals below 1 ms are dropped (scheduler noise), and a single interval is
clamped to one hour so a stuck process cannot dominate a profile. The symbol
`finish_task_switch` is usually cloned by the compiler into
`finish_task_switch.isra.0`, so the loader tries a list of candidate names and
attaches the program to whichever exists.

A task that is blocking cannot always be unwound in user space — the registers
at that moment often point somewhere the frame-pointer walk cannot follow — so
the start probe falls back to the kernel stack. Both stacks live in the same
`stacks` map and an id alone says nothing about which, so the sample also records
the kind and the id is written to the matching slot of `stack_key`. Without that
flag a kernel stack sits in `user_stack`, the user-space resolver is asked to
name `0xffff...` in a process that never executed those addresses, and the whole
off-CPU profile comes back as frames in no mapping.

### PID namespaces

A pid does not identify a task on its own. The kernel keeps one number per PID
namespace in `task_struct->thread_pid->numbers[]`, indexed by the namespace's
level (`0` = the initial namespace), and `/proc` lists the tasks of exactly one
namespace: the reader's. Getting this wrong is silent — the profile looks
plausible and is attributed to an unrelated process — so the programs resolve
it explicitly instead of relying on a helper's convention:

- the loader reads this process's namespace id from `/proc/self/ns/pid` and
  injects it as `const volatile __u32 agent_ns_inum`. The id is the
  `ns_common.inum` from the link target (`pid:[4026532502]`), *not* the file's
  inode: nsfs hands out anonymous inodes, so the two numbers are unrelated;
- per sample, `current_tgid()` walks `group_leader->thread_pid->numbers[level]`
  from the deepest level down and returns the `nr` whose `numbers[].ns` *is* the
  agent's namespace. The group leader, not the calling thread: `thread_pid`
  belongs to the individual thread, and keying on it would turn every worker
  thread into a series of its own.

A host agent, or a pod with `hostPID: true`, is an ancestor of every container's
namespace, so every container has a matching entry and a single agent can
profile the whole node. A task outside that tree — a kernel thread, when the
agent itself runs in a nested namespace — has no entry at all; it is counted in
`unattributed` rather than given a number that would name some other process.

These are the only kernel struct reads in the object, and they are CO-RE
relocations: no `vmlinux.h`, no kernel headers, no fixed offsets. `clang -g`
records the field accesses in `.BTF.ext` and aya rewrites them from
`/sys/kernel/btf/vmlinux` when the object is loaded, so the same object works on
kernels whose `task_struct` or `struct pid` has a different layout — this file
only names the fields it needs, and they are matched by name.

### Symbolization

The agent owns symbolization (`blazesym`), not the server:

- user stacks: `Source::Process(tgid)`, with `/proc/<pid>/map_files` preferred
  and the symbolic paths from `/proc/<pid>/maps` as a fallback (Docker's
  default seccomp profile blocks `map_files`);
- kernel stacks: `Source::Kernel`, i.e. `/proc/kallsyms` + kallsyms lookup;
- results are cached per `(tgid, address)`, and the cache is dropped wholesale
  past 200k entries (a container restart invalidates it anyway).

Addresses `blazesym` cannot name fall back to a module cache that opens the
binary itself and reads two tables, in this order: the ELF symbol table
(`.symtab` / `.dynsym`), then the Go function table in `.gopclntab` — which is
all a `-ldflags="-s -w"` binary has left, and what most of a Kubernetes node is
built from (containerd, kubelet, kube-proxy, CoreDNS, etcd).

That fallback has to survive a *batch* failure, not just an `Unknown` answer.
`symbolize` is all-or-nothing: one address whose file cannot be `stat`ed fails
the whole call, which is exactly what a container binary does when the agent
looks for it in its own mount namespace. A failed batch is therefore retried one
address at a time — that isolates the poison address and keeps blazesym's inline
information for the rest — and whatever still fails goes through the module
cache. Without this, one unreadable file turns every frame of the stack into an
address, which is the same symptom as "nothing has symbols" but has a completely
different cause.

Opening the binary is the part that goes wrong in a cluster. `/proc/<pid>/maps`
names files as the *target* sees them, and the agent runs in its own mount
namespace, so `fs::read(path)` fails on every container. Four routes are tried,
and the plain path is deliberately *not* first: `/usr/local/bin/foo` usually
exists in the agent's own image too, and symbolizing against that copy produces
either nothing or names from the wrong build.

1. `/proc/<pid>/root<path>` — the target's namespace; needs ptrace read
   permission, not `CAP_SYS_ADMIN`, so it works on hardened clusters.
2. `<host root><path>` — for charts that mount the host filesystem at `/host`.
3. `path` — correct only when the agent shares the target's namespace.
4. `/proc/<pid>/map_files/<start>-<end>` — the exact file behind a mapping,
   deleted files included; requires `CAP_SYS_ADMIN`.

Candidates are matched by **inode**, not by `(device, inode)`: an overlayfs
reports the same inode under a different `st_dev` than `/proc/<pid>/maps`, so
requiring both rejects the correct file. A candidate that opens a *different*
file is discarded rather than used as a last resort. When nothing can be opened,
the agent logs the path and the routes it tried once per binary and counts it in
`symbol_module_failures`, which is the number that explains a wall of unnamed
frames.

The file is **mapped**, not read. That distinction is not an optimisation: a
`read_to_end` into a `Vec` costs as much heap as the binary is large, so the
agent used to cap modules at 256 MiB — and the binaries that exceed it are the
Go services a node actually runs (Grafana is 490 MB, Prometheus is not much
smaller), whose function table is the only thing that can name a frame. With a
private read-only mapping only the touched pages are charged: the section
headers and the tables that are parsed. The two fallbacks that walk the whole
file — the `go1.x` token scan and the magic scan for a renamed `.gopclntab` —
are bounded at 64 MiB for the same reason, and the section-based path, which is
the normal one, still applies to any size.

Because every frame is resolved *before* storage, a stored batch is
self-contained: no `vmlinux`, no build ids, no `.debug` files are needed to read
it back, and a profile of a process that has already exited remains readable.

A stack **ends** where it leaves the process's address space. A broken
frame-pointer chain does not fail politely: the unwinder reads the stack itself
and returns whatever it finds there — a string, the constant `2`, a saved
register — and every frame after that point is the same garbage. Those values
are counted (`truncated` in the coverage line) and dropped rather than stored,
because no symbolizer can name them and they are what turns a flamegraph into a
wall of addresses. The check uses the *original* return address, not the `ip - 1`
probe (which can point just outside a mapping whose first byte is the return
address), and it accepts any mapping, file-backed or not: anonymous memory is
where JIT code lives, and a JIT frame is a real frame. `perf` stops at the same
point for the same reason.

### Labels, and what names a series

Two enrichments run per process, and they fail independently:

- **from the cgroup path** (always, no privileges): the pod UID, the QoS class
  and a `k8s_container` marker. The path is not a name — it is a UID — so this
  alone cannot say *what* was profiled.
- **from the pod index** (only when the API is reachable): namespace, pod name,
  owning workload and kind, plus the service name taken from
  `app.kubernetes.io/name` or the workload. `lidlesseye-ebpf` only holds the
  snapshot; the HTTP client lives in the `lidlesseye` crate, so the collector
  keeps working with no API, no service account and no network.

The series name stays the process name unless the pod index knows better, which
is what makes a host install, a container and a cluster behave identically up to
the point where the cluster has something more to say.

## Storage: DuckDB

Samples live in DuckDB 2.0 LTS ("Cinnamon Teal"), one database file per tenant:

```text
<data-dir>/tenants/<tenant>/lidlesseye.duckdb
```

Four tables:

| Table | Columns | Role |
| --- | --- | --- |
| `batches` | `batch_id, profile_type, service_name, labels, start_ns, end_ns, total_value, sample_count, bytes, created_ns` | one row per ingest; what a query plans against and what `/metrics` counts |
| `samples` | `batch_id, profile_type, service_name, labels, start_ns, end_ns, stack_id, value` | one row per stored `(stack, value)` |
| `stacks` | `stack_id, frames BLOB` | frame ids packed little-endian, leaf first |
| `frames` | `frame_id, module, symbol, file, line` | global dictionary, one row per distinct frame |

`samples` repeats the series columns instead of joining `batches` on every
query. A batch is one `(service, type, label set, window)`, so those values are
constant over a contiguous run of rows and DuckDB's run-length compression makes
them nearly free. What that buys is a read path with no join at all.

Stacks are packed ids rather than a delimited string because a stack is walked
on every flamegraph query. Converting text into frame ids at query time would
move work from ingest (once) to query (every time).

### Why DuckDB, and why it replaced the previous format

Version 0.1 wrote its own columnar segment format (`.leseg`). It worked and it
was fast, but it was a storage engine: header layout, dictionary encoding,
CRC32, crash-safe `fsync` + `rename`, retention by deleting files, and a `fsck`
that re-verified every checksum. Every one of those had to be re-verified
whenever anything around it changed.

DuckDB takes that list over: transactional writes, compression, zone maps,
checkpointing and recovery. What is left here is the schema above, which is the
part that is genuinely specific to profiling.

The trade-offs are real:

- **Writes are slower.** A batch is a transaction with a WAL append, not an
  `fsync` + `rename` of a file nobody else has open.
- **The file does not shrink.** DuckDB marks deleted rows free inside existing
  row groups instead of returning blocks to the filesystem. `prune` therefore
  measures the disk cap against the estimated size of the *retained* batches
  (`SUM(batches.bytes)`), not against the file size; the file grows to the
  high-water mark and stays there.
- **One process per database file.** DuckDB takes a file lock, so two
  `lidlesseye serve` processes cannot share a data directory. The old format
  allowed that, at the price of two writers racing on the same index.
- **The binary needs `libduckdb.so` at runtime.** DuckDB is linked against the
  prebuilt shared library that `libduckdb-sys` downloads at build time. That
  keeps a cold build to seconds instead of compiling the whole C++ engine, at
  the cost of a build-time network fetch and a runtime loader dependency. The
  release image installs the library under `/usr/local/lib`; a bare-metal
  install has to ship it alongside the binary. Offline builds can point
  `DUCKDB_LIB_DIR` at a vendored copy. Changing the dependency to
  `features = ["bundled"]` produces a self-contained binary instead, but a
  cold build then takes tens of minutes and several GB of RAM.
- **Queries that used to early-terminate now scan.** The old format sorted
  samples by value so `top` could stop early; SQL has to aggregate the whole
  window. Zone maps still skip row groups outside the time range, and the
  aggregation happens in the engine rather than in a Rust loop.

What it buys: the samples are queryable with plain SQL by anyone with a DuckDB
client, aggregation is the engine's job rather than hand-written code, and
future questions ("how much CPU per namespace last Tuesday") do not need a new
columnar reader.

### Integrity checks

`lidlesseye fsck` runs `CHECKPOINT` (which fails loudly on a damaged file) and
then verifies the two invariants only this schema can violate: every
`samples.stack_id` resolves to a row in `stacks`, and every frame id packed into
a stack blob resolves to a row in `frames`. `--fix` drops the offending rows in
one transaction and rolls back on error. It never deletes the database file: a
tenant is one file, and the old file-backed store's "delete the bad segment"
behaviour would destroy all of its good data.

### Migrating from `.leseg`

The store detects leftover `.leseg` files and logs a warning; it does not read
them. There is no converter, because the old format and the new schema carry the
same information and the sampler can simply re-ingest. A deployment that must
keep old history should export it with the version that wrote it.

## Storage: Parquet + S3

A second backend trades the single DuckDB file for a tiered layout of Parquet
objects. Select it with `--storage-mode=parquet` (or `server.storage.mode:
parquet` in Helm; see [deployment.md](deployment.md)). DuckDB 2.0 is still the
query engine — this mode changes only where the durable rows live.

```text
<data-dir>/tenants/<tenant>/
  parquet/
    <table>/h<epoch-hour>-<rows>.parquet   # small local files (staging)
    manifest.json                          # cache of archived S3 object keys
  # no lidlesseye.duckdb file: the hot tier is in memory
s3://<bucket>/<prefix>/<tenant>/<table>/hour=<epoch-hour>.parquet
```

The lifecycle has three steps, run by a background task roughly every 10 s:

1. **Hot** — writes land in an in-memory DuckDB `<table>_hot` table. Recent
   windows are served straight from memory.
2. **Flush** — `flush_hot` copies the hot rows into one small ZSTD-compressed
   Parquet file per table, named with the current epoch hour, and empties the
   hot table. Queries still see everything because the canonical table names
   are VIEWs over `hot UNION read_parquet(<explicit local files>)`.
3. **Archive** — once a local file is older than `--parquet-local-ttl`
   (default `1h`, so whole hours are archived), `archive_local` merges that
   hour's files into one large object, `COPY`s it to S3, records the key in
   `manifest.json`, then deletes the local copies. Reads for older windows now
   stream from S3.

The design goal is near-zero-copy: DuckDB scans Parquet directly, pushing
filters and projections into the scan and skipping row groups with zonemaps.
A query over S3 uses HTTP range reads (the object store returns `206 Partial
Content`); nothing is downloaded into the database first. All six tables are
stored this way, including the service-map `net_nodes` / `net_edges` rows.

Two details make this robust:

- **No LIST on the query path.** VIEWs are built from an explicit list of file
  paths (local files from `read_dir`, S3 objects from the manifest), not a glob.
  Binding a query therefore never contacts S3, and an empty or temporarily
  unreachable bucket is a non-event. The manifest is written atomically before
  local files are removed, so a crash can leave a harmless extra file but never
  lose an object.
- **S3 is the source of truth.** On open, a best-effort `glob()` of each S3
  prefix rebuilds the manifest from the bucket. Losing `manifest.json` (PVC
  rebuild, replacement node, first start against an existing bucket) therefore
  cannot hide archived data. Discovery failure is non-fatal so a misconfigured
  or offline S3 never blocks startup.

Both modes keep the same schema and the same read SQL, and both are selected at
install time — an existing DuckDB deployment is unaffected. Parquet mode needs
an S3-compatible endpoint; AWS S3, MinIO and other S3 API implementations work,
with path-style URLs by default. See [deployment.md](deployment.md) for the
Helm values and credentials.

## Query engine

`flamegraph`, `top` and `timeseries` share one pipeline:

1. **Select** — one SQL pass over `samples` filters the time window and the
   profile type, groups by `(stack_id, service_name, labels)` and sums the
   values. Matchers on `service_name` are pushed into the `WHERE` clause;
   matchers on other labels (glob included) run in Rust afterwards, because the
   label set is one encoded string and `label != value` cannot be expressed on
   it correctly.
2. **Remap** — the frames those stacks reference are loaded once and interned
   into a query-wide `FrameDict`, so the same function coming from different
   batches collapses.
3. **Fold** — stacks are walked leaf-first and accumulated:
   - `flamegraph` inserts them into a `TreeBuilder` (capped at 400k nodes; the
     excess is folded into a `(truncated)` node instead of being dropped
     silently), then compacts the tree into the flat
     `names`/`levels`/`values` triple the UI consumes;
   - `top` accumulates `self` (leaf frame only) and `total` (every frame in the
     stack) per frame, then sorts by self value;
   - `timeseries` reads `batches` only — no samples — and splits a batch's
     total value proportionally over the buckets it overlaps, per series
     (`service_name` + labels).
4. **Serve** — the result structs are `Serialize`, so the HTTP layer is a
   `Json(...)` call.

Selectors support `=`, `!=`, `=~`, `!~` where `=~`/`!~` are **glob** matches
(`*`, `?`), not regexes: they cover what people actually write on labels and
keep the hot path allocation-free.

`wall` is resolved at query time by reading `cpu` and `off_cpu` and adding them,
so nothing is duplicated on disk.

## Concurrency

- One `Connection` per tenant behind a mutex. DuckDB's Rust binding has no
  separate `Database` handle, and opening the same file twice inside one
  process is a lock conflict rather than a second reader — so serializing
  access is the only option, not a design choice. Queries still run in
  parallel *inside* DuckDB (`--duckdb-threads`, four by default, applies to
  every tenant); what the mutex serializes is the short per-query setup and the
  aggregation call itself.
- Every store read runs on `spawn_blocking`, including the ones that look too
  cheap to bother (`/series`, `/profile-types`, `/stats`). A blocking call in
  an async handler does not just slow that request down — it removes one of the
  runtime's four worker threads, and four of them take the whole process with
  them, liveness probe included.
- `--query-concurrency` (default 4) caps how many reads are in flight. The
  extra requests wait as futures, which costs nothing, rather than as parked
  threads, which cost a stack each and queue on the same mutex anyway. A caller
  that waits longer than 15 s is told `503 server is busy` instead of being
  left to hit the 60 s request timeout.
- Ingestion is `spawn_blocking` (file I/O and the transaction), queries are
  `spawn_blocking` too, so a slow flamegraph cannot stall the accept loop.
- The collector runs on its own OS thread; it captures a `tokio::runtime::Handle`
  before starting because the callback has no runtime context of its own.
- Retention runs as a periodic background task: delete expired batches, then
  trim oldest-first until the retained batches fit under `max_disk_bytes`.

## Overhead

Per sample, the kernel does one stack-id lookup plus one hash increment; the
maps are per-CPU-friendly hashes and the aggregation never leaves the kernel.
User-space work is proportional to the number of *distinct* stacks per flush
interval, not to the number of samples, so a busy machine costs roughly the same
as an idle one. The default 99 Hz is the usual trade-off; 999 Hz is fine on
modern hardware, and the sample rate is recorded in each report
(`sample_rate_hz`).

## Storage benchmark

One folded file with 20 000 stacks, ingested through the CLI into a fresh tenant
(release build, Docker Desktop / WSL, DuckDB v2.0.0, no tuning). Each number is
a whole CLI invocation, so it includes process startup and opening the database:

| Operation | Runs | Median |
| --- | --- | --- |
| `ingest` 20 000 stacks | 2203 / 2122 / 2084 ms | 2122 ms |
| `query --top` | 200 / 122 / 124 ms | 124 ms |
| `query --tree` | 127 / 125 / 123 ms | 125 ms |
| database file after ingest | — | 2.6 MiB |

The interesting part is the size: 20 000 stacks with a shared frame dictionary
and packed frame ids occupy a few MiB, not a few hundred MiB of JSON. Query time
is dominated by CLI startup plus DuckDB's scan; `--top` and `--tree` read the
same `samples` rows and differ only in the Rust-side fold.

These numbers were measured with the shipped configuration — DuckDB 2.0.0
linked against its prebuilt shared library. Treat the figures as an
order-of-magnitude guide for a single tenant on a laptop-class machine, not as
a production SLO — the host was a Windows Docker Desktop VM.

## Dashboard

The UI is part of the server, not a separate app: `web/dist` is a Next.js static
export built by `make ui` and embedded by `lidlesseye-server` with `rust-embed`.
A release is still one executable serving the HTML, the JS chunks and the API
from one port — no second container and no Node runtime in production.

The ordering is the Makefile's job, not the developer's: `make build`,
`make release` and `make test` all depend on the phony `ui` target, which runs
`next build` and `sync` before `cargo` starts. The dependency is deliberately not
`web/dist/index.html`: an export left behind by an interrupted build, a branch
switch or a restored directory still has a fresh mtime, and trusting it would
ship an old dashboard. Only `npm install` is gated on the lockfile (through the
marker npm writes into `node_modules`), so a no-op rebuild costs one `next build`.

`web/dist` is not committed: it is a build artefact, and keeping
it out of git removes a megabyte of generated chunks from every UI diff.
`lidlesseye-server`'s `build.rs` mirrors it into the crate's `ui/` folder, which
is the directory `rust-embed` reads: that folder travels with `cargo package`,
so the crate on crates.io embeds the same dashboard a checkout does. When
neither exists (a fresh checkout, no Node) the script creates an empty `ui/` so
the crate still compiles — the binary then answers 503
`UI is not embedded in this build` at `/`, while the API keeps working. The
Dockerfile builds the export in its own Node stage and copies it into the Rust
builder, so the image never depends on local state.

Components are React 19 with shadcn/ui (Radix primitives, Tailwind v4) under
`app/`. Navigation, tables, selects, the filter rows and the error banner are
shadcn components; `components.json` records the style so `npx shadcn add` keeps
working. The three drawings — flamegraph, stacked time series, histogram brush —
stay on a single canvas per view and are *wrapped*, not reimplemented: the data
is a fixed-size typed array, DOM/SVG flamegraphs fall over around 20k nodes, and
zoom/pan/hover is a binary search plus one draw pass that a chart library would
replace with a slower and different interaction model. Their classes live in
`lib/canvas/` and own painting only; toolbars, breadcrumbs, legends and tooltips
are React, fed by callbacks.

State lives in one store (`lib/store.ts`) read through `useSyncExternalStore`,
and the query window is derived from the chosen domain as a pure function. That
is what makes a URL reproduce a view exactly. Routing stays hash-based
(`#/flamegraph?…`), so the single exported `index.html` serves every view and
pre-existing deep links keep working.

The service map is DOM, not canvas: one absolutely positioned `Card` per node,
one SVG for every edge, both inside a single transformed layer so pan and zoom
are one CSS transform and no element is re-measured while dragging. A few dozen
cards are cheap to lay out and can hold real components — a card with three
lines of text, a status glyph, a heat bar — which a canvas would have to
re-implement. Positions come from `lib/servicemap-layout.ts`, which ranks the
graph by dependency depth so entry points sit on the left and what they call
sits to their right, then groups each column by kind so the canvas can draw a
labelled band per region. Ranking is the honest metric for a call graph and it
is also why a large cluster produces a wide drawing: the automatic fit keeps a
readable scale and lets the map overflow, and search plus one-hop focus are how
a reader moves around it. The kind bar and the faint tint say what a node is, and
the hairline along the bottom of the card is the on-CPU time of the profiles
behind it, so the map and the flamegraph are two views of the same window.

## Service map

The map answers "who talks to whom, what they say to each other, and what it
costs". It is a *network* graph — edges with connection counts, bytes,
retransmits, round-trip time and an application protocol — plus the profiles
already stored, joined per workload.

### Two collectors, one rule set

The agent prefers an eBPF collector and falls back to the pods' TCP tables.

*eBPF* (`crates/lidlesseye-ebpf/bpf/net.bpf.c`, its own object so a failure
here cannot take the profiler down) hooks four kernel functions:

| Program | Symbol | What it adds |
| --- | --- | --- |
| `lidlesseye_tcp_state` | `tcp_set_state` | a new connection, and which end opened it |
| `lidlesseye_tcp_send` | `tcp_sendmsg` | bytes sent and the application protocol in the first payload |
| `lidlesseye_tcp_recv` | `tcp_cleanup_rbuf` | bytes received |
| `lidlesseye_tcp_retrans` | `tcp_retransmit_skb` | retransmits |

Kprobes rather than tracepoints or fentry: `sock:inet_sock_set_state` has three
arguments on 6.x and four on 5.x, and an fentry program on `tcp_sendmsg` fails
to load because the verifier cannot size the anonymous union inside `msghdr`
through BTF. A kprobe's arguments are in registers on every release, so the
only version-dependent piece is the `pt_regs` layout, which CO-RE resolves by
field name. The probes attach by a candidate list of symbols (`tcp_set_state`,
then `.isra.0`, `.constprop.0`) because the compiler is free to clone them.

Each program writes into a `flows` hash keyed by the 5-tuple, and a per-socket
map remembers the key so the byte counters can find it again without re-parsing
the sock. Every window the map is drained and removed, which is what makes a
window's numbers a delta rather than a running total.

Because the handshake is seen, the *direction* is not a guess: a socket that
reaches `TCP_ESTABLISHED` from `TCP_SYN_SENT` is the client, one that reaches it
from `TCP_SYN_RECV` is the server. Addresses are the ones the application wrote,
before kube-proxy rewrites them, so a pod-to-ClusterIP connection is reported as
`workload → service` and the hairpin-masqueraded copy at the server end is
recognised (its peer is a private address that names no pod) and dropped instead
of being counted as an outside client.

Connections that predate the agent — a keep-alive between kube-apiserver and
kube-scheduler, for instance — never see a handshake. For those the role is
inferred from the pod spec: a pod listening on the local port is the server, a
peer whose pod declares the remote port makes this pod the client. When neither
holds the connection is dropped rather than drawn backwards.

### Application protocols and request detail

Two programs sit on `tcp_sendmsg`, and the split is not cosmetic: the fifteen
per-protocol parsers and the payload copy together exceed the verifier's
one-million-instruction ceiling, and a single program that tried to do both was
rejected outright (63k states, 1.1M processed instructions). Verified
separately, each stays well inside the budget.

**`lidlesseye_tcp_classify`** names the protocol of a flow, once per window
(the counters are drained every interval, so `protocols` starts at zero again
for the next window's first send). It reads a window of the request and every
check uses a *constant* byte offset — the earlier masked-variable indexing is
what cost one verifier state per possible offset. Framing formats are
recognised by comparing their declared length against the size of the write.
It covers HTTP/1, HTTP/2, TLS, PostgreSQL, MySQL, MongoDB, Redis, Memcached,
Kafka, ZooKeeper, FoundationDB, DNS, Cassandra and ClickHouse; checks that need
to walk a variable-length header are deliberately shallow, because the port
guess covers the rest and a wrong label is worse than a generic one.

**`lidlesseye_tcp_send`** counts bytes and samples the first writes into a perf
ring buffer — at most twelve per connection per window on the client side,
three on the server side, so a chatty socket cannot flood the ring. Bulk writes
are never copied: a TLS record or a result set cannot name a host or a
statement. The agent parses the samples and fills two fields on the edge:

* **`host`** — the `Host:` header of an HTTP/1.x request. This is the name the
  client asked for, which a ClusterIP or a load balancer address cannot tell
  you, and it is what turns a row of `external` connections into a list of
  destinations. HTTP/2 keeps its headers in a compressed HPACK block and
  contributes nothing.
* **`sql`** — the text of a MySQL `COM_QUERY` / `COM_STMT_PREPARE` or a
  PostgreSQL `Query` / `Parse` message. Prepared statements are what modern
  drivers use, so both commands are read.

A statement longer than the sample is marked with `…`. The agent also adds the
protocol it recognises to the edge's bit set, which is how a TLS flow — whose
plaintext the classifier cannot see — still gets labelled.

Encrypted traffic needs the optional TLS uprobe (`LIDLESSEYE_SSL_UPROBE=1` on
the agent). It attaches `SSL_write` / `SSL_write_ex` to every `libssl`,
GnuTLS and NSS it can find — system directories plus each `/proc/<pid>/root`,
deduplicated by inode — and emits the same samples, keyed by cgroup instead of
by socket. It is off by default: every TLS write pays a user-memory read. The
same uprobe is what covers MySQL and PostgreSQL over TLS, not just HTTPS.

The protocol label falls back to the server port's guess when the classifier
recognised nothing and no sample was parsed. A recognised payload always wins:
`9200` is normally an HTTP API, but a native ClickHouse frame on that port is
ClickHouse.

Two offsets the payload copy needs — where the pointer sits inside
`struct msghdr` and `struct iov_iter` — are read from the running kernel's BTF
at startup and patched into the program as constants. Kernel 6.16 renamed
`iov_iter.iov` to `__iov` and moved it; a CO-RE access by name resolves on one
layout and fails on the other, and a failed relocation becomes an invalid call
that the verifier rejects.

*Fallback* (`/proc/<pid>/net/tcp`) is what the agent used before the eBPF
collector existed and still runs when the programs cannot be loaded.
`/proc/<pid>/net/tcp` of any process in a pod is resolved by the kernel in that
pod's network namespace, so one `hostPID` agent can read every pod's TCP table
without `setns`. It has no bytes and no RTT, and polling only sees what is open
at the instant of the scan, so it counts `TIME_WAIT`/`CLOSE_WAIT`/`FIN_WAIT`
alongside `ESTABLISHED` to recover the connections that closed between scans;
`SYN_SENT` is left out, because a refused connect is not a connection.

Attribution rules:

| Case | Who counts the edge |
| --- | --- |
| pod → pod | the client's agent; the server's side skips it, or every edge would double |
| pod → ClusterIP | the client, resolved through the Service index (`default/kubernetes`, not "external") |
| outside → pod | the server, as `external → workload` |
| pod → outside | the client, as `workload → external` |

Two details decide whether the map names the right thing:

*Peers are resolved by address and port.* Every `hostNetwork` pod on a node
reports that node's address, so "172.18.0.4:9100" is only the node exporter if
the port is part of the lookup. The pod index therefore keeps the container
ports from `spec.containers[].ports` and prefers the pod that declares the port
before falling back to any pod on the address.

*A shared network namespace is not the pod's.* For a `hostNetwork` pod,
`/proc/<pid>/net/tcp` is the node's table: every socket on the machine. Read
naively, kube-proxy, kindnet and node-exporter each come out with the whole
node's traffic (their byte counters are identical, which is the tell). The agent
compares `/proc/<pid>/ns/net` against pid 1's and, for a match, keeps only the
sockets whose inode appears in that process's `/proc/<pid>/fd`. The interface
counters have no per-pod equivalent in that case, so such pods report no bytes
rather than the node's totals; their connections are still counted. The eBPF
collector needs no such test — it never reads a table — but it shares the
consequence: a `hostNetwork` pod is identified by the node address plus a port
it declares, so its listening side is named correctly while its own outgoing
sockets, on an ephemeral port, fall back to whichever pod shares that address.

Retransmits from `/proc` are a delta against the previous scan. Windows are
appended to `net_nodes`/`net_edges` and summed by the query, which is what makes
the map follow the same `from`/`until` as the flamegraph. CPU is joined by
label: the query groups `samples` of type `cpu` by
`k8s_namespace`/`k8s_workload`, which is the same string the agent uses as a
node id.

### Replicas

A node also carries how many pods the workload has and how many of them are
ready, read from the same cluster-wide pod list the peer lookup uses
(`status.phase` and the `Ready` condition). The map is per workload, so these
are gauges on the node rather than a per-pod view: `3/3` is a healthy
Deployment, `2/3` is one that is running but not serving, and that distinction
is invisible in the traffic counters — a pod that fails its readiness probe
still accepts and refuses connections identically.

Two aggregation rules follow from them being gauges. Merging two agents'
batches takes the larger value, because every agent reads the same list and
summing would multiply the replicas by the number of nodes. Reading a window
takes `arg_max(pods, (bucket_ns, pods))` — the newest sample in the range, with
the count itself breaking a same-bucket tie so two agents' rows resolve to one
consistently — rather than the maximum, so a workload that scaled down does not
keep reporting its peak; every column comes from the same row, so the count and
the instance list always describe the same moment.

### Instances

The count says a workload is short a pod; the list says which one. Each node
carries up to 24 pods — name, the node running it, readiness and phase — sorted
not-ready first and then by name, so a panel's table opens on the row that is
worth reading instead of alphabetically 24 mostly-identical lines. The host
comes from `spec.nodeName`: the `/proc` collector already walks the cluster pod
list for peer resolution, and the eBPF collector keeps the same index, so the
field costs one more assignment on a path that already exists.

The list is a gauge too, stored as a JSON column rather than a join table so a
window read stays a single scan. Merging two agents' batches takes the longer
list rather than the union — the union of two identical reads is the same list
twice. On read it is parsed back per row; a row written before the column
existed carries the `'[]'` default and the panel degrades to a count.

### Containers

A pod is not one process. A sidecar opens its own connections, and folding them
into the workload hides which of the two is talking. Every edge therefore names
the container on each end when the agent can tell:

* the **client** container comes from the *cgroup that dialled*. The handshake
  completes in softirq, where `bpf_get_current_cgroup_id()` is whatever task the
  CPU was running — so the probe takes it earlier, when the socket enters
  `SYN_SENT`: `connect()` goes through `tcp_set_state` in the caller's context,
  and that id is carried on the socket until the connection is established.
  User space resolves it to a name in two steps: the cgroup directory's kernfs
  inode *is* the id the helper returns, and the pod list maps the container id
  in the cgroup path (`cri-containerd-<id>.scope`, `crio-<id>.scope`,
  `docker-<id>.scope`, or a bare id on cgroup v1) to `status.containerStatuses`
  and finally to a name;
* when a connection predates the agent there is no handshake to record, so it
  falls back to the socket itself: a pod's containers share one network
  namespace, and the missing link between "a port exists" and "this container
  owns it" is the socket inode in `/proc/<pid>/fd`, matched against the
  namespace table and the same cgroup-derived name;
* the **server** container is inferred from the port the peer declares in
  `spec.containers[].ports`. A port no container claims is left empty rather
  than guessed: with two containers the guess would put a sidecar's traffic on
  the app.

Containers also become an edge key, so two containers of one pod talking to the
same peer are two rows in the store, not one. The node carries the container
names it runs, as a gauge merged like the replica counts. The one-hop map under
a clicked node draws a row per container for **every** workload in the drawing,
not only the one that was clicked, and anchors every edge on the row that owns
it; traffic the agent could not attribute stays on the workload card. That
drawing has its own pan and zoom and reports its natural width to the drawer,
so the panel opens wide enough to read the hop instead of shrinking it.

Two limits are worth stating. A `hostNetwork` pod shares the node's address, so
an outgoing socket on an ephemeral port can still be credited to the wrong *pod*
— the container name, when the cgroup was recorded, is exact regardless. And a
connection that both opened and closed between two drains, on a socket the
handshake map no longer knows, arrives with no container at all.

### The outside world, by host

`external` is an aggregate, and an aggregate is a dead end: three APIs and a
database behind one grey node cannot answer "which dependency got slower". The
agent already records the HTTP `Host:` on an edge (and the SNI/host of a TLS
connection when the uprobe is on), so the query splits external edges by that
value and synthesizes one node per host. Two details make it honest:

* `net_edges` groups by a computed `host` key **only for `server_id = 'external'`**,
  so an internal edge keeps one row per protocol/container pair and the panel's
  per-container rows are unaffected;
* the aggregate node keeps the traffic that never named a host (encrypted
  without the uprobe, or not HTTP at all), and is dropped when nothing is left.
  Hosts are capped at the busiest [`MAX_EXTERNAL_HOSTS`], because a crawler can
  name thousands of them and the map only holds [`MAX_NODES`].

A host node has no rows of its own in `net_nodes`, so its history comes from the
edges that named it (`net_host_edge_history`), matched on `lower(host)` because
that is how the node id is built.

### History for the panel

The map shows one window; the panel that opens on a node draws that window's
*shape*, which needs buckets rather than totals:
`GET /api/v1/servicemap/history?node=&from=&until=&buckets=` returns up to 600
buckets of `rx_bytes`, `tx_bytes`, `connections`, `retransmits`, `rtt_us`,
`pods` and `ready`, twice — `points` for the node's own counters and `edges` for
the same buckets seen through every edge touching it.

The bucketing happens inside DuckDB, `CAST((bucket_ns - from) / step AS BIGINT)
AS slot`, because `bucket_ns` at today's epoch is ~1.7e18 and past a double's
exact integer range; subtracting the range start first keeps the numerator at
hours, not decades. Doing the division in Rust would misalign buckets by
hundreds of microseconds at the window's start, and doing it in the browser
would round the timestamps themselves.

What each column means follows from how the map is built. Counters sum across
agents, because two agents genuinely saw different halves of the traffic.
Gauges take `max`, because the same replica count reported twice is still one
count. `rtt_us` is `sum(rtt_us_sum) / sum(rtt_samples)` — weighted by how many
samples produced it, not the mean of per-edge means, or an edge that closed two
connections would weigh as much as one that closed two thousand. Egress and
ingress are split with `client_id = ?`, so "out" means the same thing when the
node sits on the server end of an edge as when it sits on the client end.

`buckets` defaults to 60 and the step is derived as `(until - from) / buckets`,
so a caller never has to compute a step that lines up with the window it asked
for.

### Known limits

Stated rather than hidden, because each one changes how a number on screen
should be read: the protocol sniffer names the wire protocol but does not count
requests, so an edge still reports connections, not requests, and a TLS socket
whose payload was never visible keeps its port-derived label; traffic in a
pod's own namespace (`localhost`) is ignored; a long-lived idle connection is
counted when it appears, not re-counted per window; and the round-trip time is
sampled once, when the connection closes, so a connection still open in this
window contributes bytes but no latency. Static
pods are owned by their `Node` in `ownerReferences`; treating that as a workload
would merge every control-plane component on a node into one entry, so it is
ignored and each pod keeps its own name.

The histogram behind the time-range selector is `GET /api/v1/timeseries` over
the tenant's extent — the same endpoint the time-series view uses, at 120
buckets instead of 90 — summed across series. It is fetched separately from the
view's query so a slow or failed histogram never blocks the flamegraph.

See [ui.md](ui.md) for the controls and the URL model.
