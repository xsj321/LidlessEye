# Architecture

## Crates

```
lidlesseye-core      data model, LESEG segments, store, query engine, selectors
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
                                     LESEG segment per (type, service, labels, window)
                                                     │
                                                     ▼
                                       query engine (mmap, no decode)
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

`bpf_get_current_pid_tgid` returns PIDs in the **initial** namespace, but an
agent inside a container can only read `/proc/<pid>` for processes in *its*
namespace. The BPF program therefore uses
`bpf_get_ns_current_pid_tgid(dev, ino, …)` when the loader injects a
`const volatile struct ns_config { dev, ino }` (read from
`/proc/self/ns/pid` via `stat`). On a host, or with `hostPID: true`, the
translation is the identity.

### Symbolization

The agent owns symbolization (`blazesym`), not the server:

- user stacks: `Source::Process(tgid)`, with `/proc/<pid>/map_files` preferred
  and the symbolic paths from `/proc/<pid>/maps` as a fallback (Docker's
  default seccomp profile blocks `map_files`);
- kernel stacks: `Source::Kernel`, i.e. `/proc/kallsyms` + kallsyms lookup;
- results are cached per `(tgid, address)`, and the cache is dropped wholesale
  past 200k entries (a container restart invalidates it anyway).

Because every frame is resolved *before* storage, a stored segment is
self-contained: no `vmlinux`, no build ids, no `.debug` files are needed to read
it back, and a profile of a process that has already exited remains readable.

## The `LESEG` format

```
┌────────┬────────┬─────────────┬─────────────┬───────────────┬─────────┐
│ header │ strtab │   frames    │ stack_frames│     stacks    │ samples │
│ 128 B  │ UTF-8  │ 16 B/frame  │  4 B/entry  │   8 B/stack   │ 16 B/ea │
└────────┴────────┴─────────────┴─────────────┴───────────────┴─────────┘
```

| Section | Layout | Notes |
| --- | --- | --- |
| header | magic `LESEG\0\0\x01`, version, offsets, time window, `total_value`, `sample_count`, `crc32` | 128 bytes, enough to plan a query without touching the rest |
| strtab | UTF-8 | one copy of every module / symbol / file name used in the segment |
| frames | `u32 module, u32 symbol, u32 file, u32 line` | dictionary-encoded: 16 bytes per distinct frame |
| stack_frames | `u32[]` | all stacks concatenated; a stack is an `(offset, len)` slice |
| stacks | `u32 offset, u32 len` | 8 bytes per distinct stack, however deep |
| samples | `u32 stack_id, u32 pad, u64 value` | sorted by value descending |

Design consequences:

- **Zero-copy reads.** The query engine `mmap`s the file and walks the arrays in
  place. There is no deserialization step and no per-sample allocation, so a
  256 MiB segment costs no heap.
- **Early termination.** Samples are value-sorted, so `top` and truncated
  flamegraphs can stop scanning once the remaining rows cannot make the cut.
- **Cheap planning.** `SegmentMeta::overlaps` and label matching need only the
  128-byte header plus the labels blob; the store keeps an in-memory index of
  all segments and never opens a file to reject it.
- **Crash safety.** A segment is written to `*.leseg.tmp`, `fsync`'d, then
  `rename`d into place — an interrupted write leaves a `.tmp` file, never a
  half-valid segment. `lidlesseye fsck` re-verifies every CRC32.

Files live at `<root>/<profile_type>/<service_slug>/<start_ns>-<end_ns>.leseg`.

### Why not DuckDB / Parquet / an LSM?

A profile query is "scan the segments overlapping this time window and
aggregate three integer columns". That is sequential I/O over immutable files —
exactly what mmap + columnar layout does best — and it needs no query planner,
no SQL parser, no C++ runtime in the server process. Parquet would work, but its
row-group/page indirection is overhead for 100 KiB files, and an embedded
analytical engine would multiply the binary size and the cold-start time of an
embedded library. The format is versioned (`FORMAT_VERSION`), so a future
version can add compression or per-frame deduplication without a migration.

## Query engine

`flamegraph`, `top` and `timeseries` share one pipeline:

1. **Select** — the store's index yields segments whose window overlaps the
   request and whose service/labels match the selector.
2. **Remap** — each segment's local frame ids are mapped into a query-wide
   `FrameDict`, so the same function from different segments collapses.
3. **Fold** — stacks are walked leaf-first and accumulated:
   - `flamegraph` inserts them into a `TreeBuilder` (capped at 400k nodes; the
     excess is folded into a `(truncated)` node instead of being dropped
     silently), then compacts the tree into the flat
     `names`/`levels`/`values` triple the UI consumes;
   - `top` accumulates `self` (leaf frame only) and `total` (every frame in the
     stack) per frame, then sorts by self value;
   - `timeseries` splits a segment's total value proportionally over the
     buckets it overlaps, per series (`service_name` + labels).
4. **Serve** — the result structs are `Serialize`, so the HTTP layer is a
   `Json(...)` call.

Selectors support `=`, `!=`, `=~`, `!~` where `=~`/`!~` are **glob** matches
(`*`, `?`), not regexes: they cover what people actually write on labels and
keep the hot path allocation-free.

`wall` is resolved at query time by reading `cpu` and `off_cpu` and adding them,
so nothing is duplicated on disk.

## Concurrency

- The store keeps an `RwLock<Index>`; ingest takes the write lock only to
  insert the new segment metadata, queries take the read lock for the segment
  list and then work outside it.
- Ingestion is `spawn_blocking` (file I/O), queries are `spawn_blocking` too, so
  a slow flamegraph cannot stall the accept loop.
- The collector runs on its own OS thread; it captures a `tokio::runtime::Handle`
  before starting because the callback has no runtime context of its own.
- Retention runs as a periodic background task: drop expired segments, then
  trim oldest-first until `max_disk_bytes` is satisfied.

## Overhead

Per sample, the kernel does one stack-id lookup plus one hash increment; the
maps are per-CPU-friendly hashes and the aggregation never leaves the kernel.
User-space work is proportional to the number of *distinct* stacks per flush
interval, not to the number of samples, so a busy machine costs roughly the same
as an idle one. The default 99 Hz is the usual trade-off; 999 Hz is fine on
modern hardware, and the sample rate is recorded in each report
(`sample_rate_hz`).
