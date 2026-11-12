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

Because every frame is resolved *before* storage, a stored batch is
self-contained: no `vmlinux`, no build ids, no `.debug` files are needed to read
it back, and a profile of a process that has already exited remains readable.

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
  parallel *inside* DuckDB (`threads`, all cores by default; `--duckdb-threads`
  caps it per connection and applies to every tenant); what the mutex
  serializes is the short per-query setup and the aggregation call itself.
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

The UI is part of the server, not a separate app: `lidlesseye-server` embeds
`web/dist` with `rust-embed`, so `cargo build` produces a binary that serves the
HTML, the ES modules and the API from one port. There is no bundler, no
`node_modules` in the build path, and no second container to deploy.

The design follows from that constraint. Files are plain ES modules, the
flamegraph and charts are drawn on a single canvas per view (the data is a
fixed-size array; a chart library would add weight and a second animation
model), and all state lives in one object in `app.js` from which query
parameters are derived as a pure function. That is what makes a URL reproduce a
view exactly, and what lets the shell swap views without a router.

The histogram behind the time-range selector is `GET /api/v1/timeseries` over
the tenant's extent — the same endpoint the time-series view uses, at 120
buckets instead of 90 — summed across series. It is fetched separately from the
view's query so a slow or failed histogram never blocks the flamegraph.

See [ui.md](ui.md) for the controls and the URL model.
