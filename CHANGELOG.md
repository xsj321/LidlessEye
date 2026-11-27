# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html): the
`0.x` series may still change the HTTP surface between minor versions, and the
Rust API follows the same rule.

## [0.3.3] - 2026-11-27

### Added

- **`docs/canvas-accessibility.md`** records why the flamegraph keeps the
  standard accessibility route (`role="img"` plus the frame list and export as
  text alternatives) instead of the experimental HTML-in-Canvas API, with the
  conditions that would make us revisit it: the API reaching Baseline in at
  least two engines, or a request to move keyboard focus between individual
  frames.

### Changed

- `.mwgrc` opts this project into the `modern-web-guidance` volatile-feature
  guidance (`allowVolatileFeatures: true`), so the canvas guidance can be
  retrieved and re-evaluated without a manual unlock each time.

## [0.3.2] - 2026-11-27

### Added

- **The flamegraph canvas has an accessible name.** Frames only exist as
  pixels, so assistive technology had nothing to announce. The canvas is now
  `role="img"` with a label that states the total and points at the text
  alternative (the frame list and the export), which is what the accessibility
  guidance asks for on complex charts.

## [0.3.1] - 2026-11-27

### Fixed

- **The flamegraph fills now clear the WCAG AA text-contrast floor.** Measured
  rather than eyeballed: with the label ink at `rgba(12, 10, 8, 0.92)` the old
  48–58 % lightness band bottomed out at **2.26:1** on the blue end
  (`hsl(245 62% 48%)`), well under the 4.5:1 required for text. The band is
  66–76 % now, which measures **5.02:1** in the same worst case and still keeps
  cool hues distinguishable from the warm diff colours.

## [0.3.0] - 2026-11-27

### Added

- **The receiving container is resolved from the reverse edge.** An agent can
  only name the container that opened a socket, so `server_container` arrived
  empty and curves ended at the peer's card. Each pod's agent also reports the
  opposite direction, whose `client_container` is exactly the container that
  accepted the connection here, so the service map now fills the missing side
  from it. Measured on the local cluster: 55 edges, 17 with a named sender and
  30 with a named receiver, up from 0 before this change.

## [0.2.9] - 2026-11-27

### Changed

- **The neighbourhood graph draws container-to-container edges.** Edges were
  merged by node pair and their `client_container`/`server_container` fields
  were blanked, so every curve anchored to the card and a pod with nine
  containers looked like a single conversation. They are merged per container
  pair now, and the drawing prefers the row anchors (`<workload>/<pod>/<container>@out|@in`)
  that `neighborhood.tsx` has always created, falling back to the card anchor
  when a container cannot be named.

### Note

- The receiving side often has no `server_container`: an agent can only name the
  container that opened the socket, not the one that accepted it. Those edges
  leave the client row and land on the peer's card, which is why a few curves
  end at a header rather than at a row. Resolving the peer would need a reverse
  lookup by address:port on the server, which changes the aggregation semantics
  and is not part of this release.

## [0.2.7] - 2026-11-27

### Added

- **Point-in-time mode.** The time range picker gained a third mode beside
  Relative and Absolute: name an instant (10:20) and the map and the memory page
  both answer for that moment instead of for a range. The window is the five
  minutes before the instant and the instant is held two minutes back when you
  pick "now", because a batch only counts once it is complete — a window that
  ended at the current second returned nothing, which is how the first version
  of this shipped and why the map looked empty.

### Changed

- **Durations on the service map are milliseconds, one unit.** Cards and the
  drawer used `formatDuration`, which switches between `1m33s`, `26.0 s` and
  `0.22 ms` depending on magnitude; side by side that reads as different kinds
  of number. `formatMillis` is used for every duration there now, while rates
  keep their `/s` and `/min` suffixes because a frequency is not a duration.

### Fixed

- The service map view memoised its window on the payload, so a time change
  between two payloads kept querying the old window.

## [0.2.3] - 2026-11-27

### Added

- **Language on both surfaces.** `GET /api/v1/memory/coverage` and
  `GET /api/v1/servicemap` now report the runtime behind a workload (`go`,
  `jvm`, `native`) derived from the reader's `profile_source` label, and both
  the Memory table and the map cards/containers show it as an icon. A workload
  whose only samples are kernel frames reports no language rather than a guess.
- **In-use memory on the Memory page.** `inuse_bytes` is folded from the
  `inuse_space` series next to `requested_alloc_bytes`, so a Go or JVM workload
  shows what it currently holds, not only what it ever asked for. Native
  (allocator-sampled) workloads report in-use only when the reader can follow
  frees, so the column stays `–` instead of a zero that looks measured.
- **The memory window is echoed back.** `GET /api/v1/memory/coverage` returns
  `from_ns`/`until_ns` and the page prints the window it is describing.

### Fixed

- **The Memory page ignored the time picker.** It queried `dataRange` — the
  extent of everything stored — so "Last hour" silently meant "since the
  beginning" and cumulative readers (correctly) reported a lifetime total:
  `node-profiles` showed 72 GiB requested over one day under a one-hour label.
  It now asks for the toolbar window and shows it, which makes the number
  self-explaining.
- **Edges with one end off the canvas.** The neighbourhood graph drew an edge
  whenever both anchors existed, including when the peer card was clamped
  outside the visible area, leaving a label floating with nothing attached.
  Edges are now dropped when either end is outside the canvas.

## [0.2.0] - 2026-11-27

The product release: trusted data, reliability, persistence, active/standby and
an operations manual. The HTTP surface gains read/write role separation, a
capability matrix, process-level memory and self-monitoring metrics.

### Added

- Profile data contract: every report carries `temporality` (`cumulative` for
  runtime-reported Go/JVM profiles, `delta` for the userspace allocator reader)
  and `gauge` for process and cgroup memory, so `/series` takes the last value
  instead of summing a running total. Memory series are now typed in bytes
  rather than falling back to nanoseconds.
- Capability matrix: `GET/POST /api/v1/capabilities` reports, per workload and
  pod, whether a reader is `supported`, `attaching`, `active`, `degraded`,
  `unsupported` or `disabled`, with a machine-readable reason and the attached
  and detected process counts.
- Global coverage: `GET/POST /api/v1/memory/coverage` folds allocation series
  and reader reports per workload into requested bytes, batches, sampling
  interval, reader, coverage and pod/process RSS, PSS and cgroup memory, and
  keeps a 480-point trend.
- Process-level memory: `GET/POST /api/v1/memory/processes` returns the top
  processes of a workload (RSS first, PSS and private dirty for the top N) from
  the reader inside the target pod.
- Memory page with three layers: process memory (L0), native allocation (L1)
  and an explicit placeholder for allocator-native profiling (L2).
- Native runtime manager: the agent creates the allocator reader as an
  ephemeral container, rebuilds it at most three times, and reports
  `degraded`/`runtime_dead` once the budget is spent.
- Agent disk spool: failed uploads are written to `LIDLESSEYE_SPOOL` and
  drained later; batches are idempotent through `x-lidlesseye-batch`.
- Periodic fsck (`GET/POST /api/v1/fsck`): proves the files it finds are
  readable Parquet and reports files that vanished between passes.
- Query cache for the four heavy GET endpoints, with `x-lidlesseye-cache` and
  `?fresh=1`.
- Active/standby through a lease file on the shared volume, with fencing
  tokens, 307 redirects to the writer, and a `read-only` role that neither
  votes nor serves writes.
- `GET /api/v1/role`, `GET /api/v1/cache` and new `/metrics` gauges for
  replica state, lease fence, cache and fsck findings, process RSS and threads.
- Helm chart 0.2.0 with the ephemeral-container RBAC the allocator reader
  needs.

### Changed

- Non-writer replicas refresh the Parquet index every five seconds, so two
  replicas behind one Service no longer disagree about which files exist.
- `--read-only` is a real role: it reports `passive`, refuses writes with 403
  and never acquires the lease.
- The agent lists pods through the Kubernetes API when the sampled pod index
  has no entry, which fixes allocator readers for idle workloads.

### Fixed

- Gauge series were summed instead of resolving to their last value.
- Live capability, coverage and process reports landed on whichever replica
  the Service picked, so the two replicas showed different numbers; they are
  redirected to the writer and mirrored through the shared volume.
- Folded uploads without an explicit window were stored one hour in the past.
- `strip = "symbols"` removed the symbol table user-space frames need.

## [0.1.6] - 2026-11-23

A failed write no longer wedges the tenant's DuckDB connection.

### Fixed

- A failed write in disk DuckDB mode could leave the tenant's only connection
  inside an aborted transaction. DuckDB then rejected every later statement —
  including reads — with `Current transaction is aborted (please ROLLBACK)`, so
  one bad ingest turned into a continuous stream of upload failures and
  `report queue full` drops. All write transactions now clear a stale aborted
  state before `BEGIN` and roll back on every error path, including a failing
  `COMMIT`.
- The query watchdog's interrupt was aimed at whatever DuckDB happened to be
  executing, not at the read that started it. A late tick could therefore land
  inside an ingest transaction and abort it. Interrupts are now scoped to a
  live read and are inert once that read has released the connection.

## [0.1.7] - 2026-11-23

The neighbourhood is per replica, a comparison against itself is exactly zero,
and the multi-language symbolization routes are exercised in one cluster.

### Fixed

- The context/upstream/downstream view used to draw one card per workload, so a
  three-replica Deployment looked like a single box and all of its pods' edges
  landed on the same container rows. Edges now carry the client and server pod
  UIDs through eBPF collection, storage (including schema migration and Parquet
  columns) and the API, and the neighbourhood draws one card per pod. Container
  rows and edge anchors belong to the specific replica that carried the
  traffic, with a fallback card for service/external peers and old data.
- Selecting the same absolute or relative window for both sides of a
  comparison could still produce a large increase or decrease. Relative time
  was resolved twice — once for the request and once for the header — and the
  few milliseconds of drift crossed a batch boundary, which the store counts as
  a whole sample. Both sides now come from one clock snapshot; identical
  windows are covered by a backend regression test and a browser assertion on
  the four API time parameters.
- The perf-map fallback was already able to name JIT frames, but the
  documented support matrix and the shipped fixtures did not cover the routes
  that produce those maps. `deploy/test/multilang/k8s.yaml` now starts Go, Node,
  C, C++, Rust, Python 3.12 (with the perf trampoline) and Java (with
  `-XX:+PreserveFramePointer` and perf-map-agent)
  pods from one ConfigMap. `docs/compatibility.md` records which routes are
  native and which need the runtime to publish symbols.

## [0.1.8] - 2026-11-23

The multi-language fixture now exercises the symbol routes that actually name
Node and JIT-compiled Java frames.

### Fixed

- `multilang-node` starts with `--perf-basic-prof-only-functions
  --interpreted-frames-native-stack` instead of plain `--perf-basic-prof`; the
  fixture now yields `nodeHotLeaf`, `nodeHotMid` and `nodeHotTop` in the
  flamegraph, not just V8 builtins and a generic `JS:*` frame.
- `multilang-java` adds `-XX:-Inline` alongside `-XX:+PreserveFramePointer`,
  `-XX:+UnlockDiagnosticVMOptions` and `-XX:+DebugNonSafepoints`, so
  perf-map-agent's map contains `javaHotLeaf`, `javaHotMid` and `javaHotTop`
  and all three appear in the flamegraph.
- `docs/compatibility.md` records the exact runtime flags for Node and JVM
  JIT frames, matching the fixture and the verified commands.

## [0.1.9] - 2026-11-23

The multi-language fixtures are permanent cluster residents and now exercise
memory as well as CPU, with the support matrix stating exactly what each
runtime's memory route can see.

### Changed

- `deploy/test/multilang/k8s.yaml` now describes long-running Deployments for
  Go, Node, C, C++, Rust, Python 3.12 and Java. Every fixture is compiled by the
  pod from a ConfigMap, so the set can be left in a test cluster without a
  private image build. The Go fixture also moved from a prebuilt image to
  `golang:1.23-alpine`, the Go version whose heap layout the reader is
  validated against.
- All fixtures except Java now allocate as well as burn CPU: Go through
  `make`/`new`, C/C++ through `malloc`, Rust through `Box<[u8; 4096]>`, Python
  through large `bytes` objects and Node through `Buffer.allocUnsafe`. Java
  stays CPU-only because the agent does not read JVM heap or TLABs. This gives
  `alloc_space` and `inuse_space` real, sustained input on every supported
  memory route.
- `docs/compatibility.md` replaces the CPU-only frame list with a CPU/memory
  capability table. It records Go runtime heap and native allocator uprobes as
  fully supported, Python and Node as partial (only malloc-backed allocations),
  and Java as unsupported for memory. It also states that `inuse_space` is the
  sum of blocks seen by the probes, not RSS or PSS.

### Fixed

- The fixture documentation no longer implies that every fixture leaf appears
  in a short top-N window; the reader capability and an individual sample are
  now described separately.

## [0.1.10] - 2026-11-23

Service-map types are a standalone crate, and AI clients can query both the
map and profiles through a read-only MCP server.

### Added

- `lidlesseye-servicemap` owns the service-map model, wire-protocol constants
  and pure query composition. It depends only on `serde` and exposes a
  `ServiceMapStore` seam, so the protocol can be shared with the eBPF collector
  and queried by tools without linking DuckDB. `lidlesseye-core` re-exports it
  as `lidlesseye_core::servicemap`, keeping every existing import path valid.
- `lidlesseye-mcp` is a stdio MCP server with twelve read-only tools:
  `get_health`, `list_tenants`, `list_profile_types`, `list_series`,
  `list_label_values`, `get_service_map`, `get_service_map_history`,
  `get_service_map_histogram`, `get_flamegraph_summary`, `get_top`,
  `get_timeseries` and `diff_profiles`. Each result has a summary, exact query
  context, explicit unit, truncation flag, ranked data subset and suggested
  next calls; errors use MCP `isError` with actionable feedback.
- [docs/mcp.md](docs/mcp.md) and [docs/mcp.zh-CN.md](docs/mcp.zh-CN.md) document
  client configuration, the tool table, the response envelope and a safe
  analysis workflow. Both READMEs, the documentation index and the
  architecture guide link to them.

### Changed

- `scripts/publish-crates.sh` publishes `lidlesseye-servicemap` before
  `lidlesseye-core` and includes `lidlesseye-mcp`, so the new crates are
  available from crates.io in dependency order.

## [0.1.11] - 2026-11-23

All seven node-panel tabs now answer their question from the data that exists,
and every chart exposes the same numbers as a semantic table.

### Added

- `GET /api/v1/servicemap/history` accepts an optional `protocol` filter and
  echoes it in the response. The DNS tab uses it for its own per-bucket lookup
  and latency series instead of ranking peers alone; the MCP history tool
  accepts the same parameter.
- Map edges and history buckets carry `rtt_samples`. Panel averages are now
  weighted by closed-connection samples, so a one-sample edge cannot outweigh
  a busy one.
- CPU, Memory and Profiling tabs expose native profile-type selectors over the
  types actually present behind the node. CPU also shows a per-series
  breakdown, Profiling shows the real top 12 and the series count, and opening
  the full flamegraph keeps the selected type and selector.
- Instances uses pod UIDs as stable keys and opens a pod's profiles with a
  `k8s_pod` matcher. Service and external nodes show backing endpoints instead
  of a "replicas belong to workloads" placeholder.
- Net adds a protocol ranking, container-aware peer rows and SQL statements
  with literals redacted. DNS explains exactly which fields the agent does not
  collect (qname, rcode, UDP).
- Every chart has a long description and a collapsible data table; browser
  smoke now walks all seven tabs and checks the table alternative.

### Fixed

- The Memory tab no longer prints the raw node id, and the Profiling tab no
  longer labels a series count as "Samples" or claims top 12 while drawing
  eight. The SLO tab has both callers and callees and derives its target label
  from the objective constant.
- `SLO` and `CPU` are translated in the Chinese tab rail.
- Two registries opening the same tenant directory at once no longer race on
  DuckDB's file header. Opens are serialized per canonical tenant directory, so
  a concurrent create cannot leave one caller with "not a valid DuckDB database
  file".
- Profile requests in the drawer are aborted when the node or window changes,
  and a profile-type change only re-queries the profile data rather than the
  network history.

## [0.1.12] - 2026-11-24

An upgrade no longer leaves an existing tenant database unopenable.

### Fixed

- `Store::open` now drops its own indexes before adding columns to an older
  `net_edges` table and rebuilds them afterwards. DuckDB refuses `ALTER TABLE`
  while an index depends on the table, so an upgrade from 0.1.10 left the
  `default` tenant closed with `Dependency Error: Cannot alter entry
  "net_edges" because there are entries that depend on it`.
- Parquet mode drops the canonical views before migrating `net_edges_hot` and
  restores them afterwards, for the same reason. Migration errors are no
  longer silently ignored there.
- A regression test opens a 0.1.10-shaped, indexed `net_edges` table with the
  current binary and verifies that `rtt_samples` and both indexes exist
  afterwards.

## [0.1.13] - 2026-11-24

Each replica in the one-hop map now owns the connections it actually carried.

### Fixed

- The eBPF flow mapper left `client_pod` and `server_pod` empty and aggregated
  edges only by workload, protocol and container. When several pods belonged to
  one workload their traffic was merged into a single edge, so the front-end
  had one UID to attach and drew every curve from the first replica's card.
  The observing pod's UID and the resolved peer pod's UID are now written to
  the edge and included in its aggregation key, preserving separate
  container-level connections for each replica.
- The browser smoke fixture now has three gateway replicas calling three
  different cart pods and asserts that every self and peer pod card carries an
  edge attributed to its own UID.

## [0.1.14] - 2026-11-24

The first dashboard request after a disk-DuckDB upgrade is no longer a silent
wait, and a current database is no longer rewritten just by opening it.

### Fixed

- `Store::open` used to run `CREATE INDEX IF NOT EXISTS` for all six indexes and
  `INSERT OR REPLACE INTO meta ('schema_version')` on **every** open, even when
  the schema was already current. On a large upgraded volume that writes the
  DuckDB catalog and WAL before the first query and can hold the tenant's only
  connection past the dashboard's timeout. The catalog is now checked first; a
  current database skips all DDL and version writes, and a real migration logs
  each stage separately.
- `/api/v1/tenants` computed stats and the distinct-series count with two scans
  of `batches` for every tenant on the dashboard's first request. Both now come
  from one cached `stats_checked()` call, in the tenant registry and in the
  single-tenant path.
- Startup and migration progress is visible with the default log filter: each
  store open reports the directory, DuckDB file, schema, id counters and total
  elapsed time; each tenant reports its own open time; a stats scan over one
  second is logged as well. `lidlesseye_core=info` is now included in the
  default filter and in the container image.

## [0.1.15] - 2026-11-24

Agents now ride out a rolling upgrade instead of dropping profiles and service
map windows while the server endpoint is briefly absent.

### Fixed

- Profile and service-map uploads retried transport failures for only ~12 s.
  During an upgrade the old pod terminates, the new one opens every tenant
  store before listening, and the Service endpoint returns later than that, so
  `ingest` and `servicemap/ingest` produced `error sending request` and dropped
  whole batches. Retries now use a five-minute budget with backoff from 250 ms
  to 10 s, which covers an ordinary rollout without holding a batch forever.
- Permanent 4xx responses are no longer retried; only transport errors, 408,
  429 and 5xx are. Final drop warnings now include attempt count and elapsed
  time so a real outage is distinguishable from a bad payload.

## [0.1.20] - 2026-11-25

Tenants are loaded on demand instead of all at start-up, and a cold tenant opens
fast enough that the first request is not a 40-second stall.

### Changed

- Tenant stores are no longer preloaded at start-up. A server with ten tenants
  used to open ten writers, ten maintenance connections and forty readers before
  answering its first request; the process ended up with around two thousand
  DuckDB threads parked in futexes, and even a tenant with a single row took
  seconds to query. A tenant is now opened the first time a request or an
  ingest names it. Catalogue and background passes use only tenants that are
  already open, so a periodic sweep cannot reopen the ones that were never
  asked for.
- Opening a tenant on the request path runs on the blocking pool, not on a
  Tokio worker. Four cold requests used to occupy all four workers and leave
  the liveness probe unanswered, which made Kubernetes kill a healthy process
  in a restart loop.
- Finding the id counters at open time reads the newest Parquet shard plus the
  hot tier rather than every archived object. With a few hundred objects this
  was thirty seconds of footer reads per cold open; ids are monotonic and
  archived by hour, so the maximum is in the newest shard.

### Fixed

- A failed S3 discovery left stale objects in the index. The bucket is now the
  source of truth: a successful LIST replaces the remote half of the index, so
  an object deleted by retention can no longer be named by a query and fail the
  whole panel with a 404. An empty or failed LIST keeps the previous index.
- A cloned read connection did not inherit the httpfs S3 credentials. They are
  connection-scoped, so a reader that skipped `configure_s3` returned 404 for
  every archived object while the configured writer looked healthy.

## [0.1.21] - 2026-11-25

Retention no longer deletes the frame dictionary, which is what emptied every
flamegraph and `top` listing in disk mode.

### Fixed

- `frames` is an unsharded dictionary filed under the `hour=0` sentinel. The
  retention pass read that sentinel as an hour, so its files looked like 1970
  data and the first sweep removed the whole dictionary while the samples that
  referenced it stayed. The result was a flamegraph collapsed to a root node,
  an empty `top`, and a service map that still worked — the data was gone, not
  the query. `hour=0` is now treated as "no date" for tables without a time
  column, exactly like a file with no hour tag: retention keeps it and only the
  disk cap may evict it, last. Legacy `stacks` files written before dictionaries
  were sharded into sample hours are covered by the same rule.

## [0.1.22] - 2026-11-25

The first write to a cold tenant no longer opens its store on a Tokio worker.

### Fixed

- Ingest, flush and service-map writes called `writable_tenant` inline, and
  that call opens the tenant when it is not loaded yet. In `s3` mode an open
  lists the prefix and reads every archive footer — tens of seconds. A
  restarted agent sends a burst, so all four workers could park inside the same
  open and the liveness probe timed out: Kubernetes killed a process that was
  only slow, then restarted it into the same cold open. The write path now
  awaits `writable_tenant_async`, which runs the open on the blocking pool;
  concurrent callers queue on the tenant's open lock there instead of on the
  runtime, and the queue is shared, so only one open ever runs.
- Dropping the open future does not cancel it: the blocking task finishes and
  caches the store, and the next caller joins the cached result rather than
  starting a second open.

## [0.1.23] - 2026-11-25

Ingest writes a whole upload in one transaction, and the UI says when it is
waiting instead of showing a blank panel.

### Changed

- One upload is one commit. `ingest_many` used to append report by report, so a
  few hundred reports meant a few hundred transactions on the single writer;
  the queue never drained and every query then timed out behind it. The batch
  path prepares every report first (so one bad report costs only itself) and
  inserts through DuckDB's `Appender`: measured on the same machine, a
  300-report upload went from 4.4 s to 13 ms at the store and from 4.7 s to
  66 ms over HTTP.
- The dashboard shows a spinner and the elapsed time while a query is in flight,
  and the service map no longer says "no service map yet" while its first
  window is still loading. A slow tenant used to be indistinguishable from one
  with no data.

### Fixed

- The allocation counters can go negative: BPF decrements `inuse_*` with
  `0 - size` on free, so a block allocated before the probe attached reads as
  `u64::MAX` once it is released. That value was sent as a profile value, the
  server refused it as implausible, and the agent dropped the whole 64-report
  batch after nine retries — twenty minutes of a tenant's data could disappear
  with no error the operator could see. The counters are clamped at zero.
- One invalid report no longer rejects its whole upload. The server validates
  per report now and answers with `rejected: [{index, error}]`, keeping the
  valid ones; an upload where nothing is valid is still a 400.

## [0.1.19] - 2026-11-24

Queries no longer pay for the whole retention window. The canonical tables stopped
being VIEWs over every archived object, reads stopped sharing one connection, and
the catalogue endpoints stopped scanning every tenant on every poll. Together these
were why the dashboard went blank: a 60-second HTTP timeout on `/api/v1/stats`,
`/api/v1/tenants` and `/metrics` while `/api/v1/version` answered in 10 ms.

The write path was restructured on top of that. Writes no longer contend for a
lock: one thread owns the writer connection and takes jobs from a queue, so an
agent waits in line instead of behind whatever background pass happens to hold the
connection — and flushing is one of those jobs, sliced small enough to take its
turn. On the local cluster a ten-tenant install went from "the hot tier never
drains, queries scan four million rows and the browser shows `INTERRUPT Error:
Interrupted!`" to a one-hour map in under a second.

### Fixed

- The hot tier could not drain on a tenant that kept receiving reports. Flushing
  tried the writer for 500 ms and skipped the tick "to stay out of the way of
  queries", but under steady ingest the writer is never free for 500 ms — every
  append is a writer transaction — so the pass never ran. The tier grew to four
  million rows, every query had to scan them (11–14 s for one hour of data on the
  local cluster), wide windows hit the 60 s HTTP ceiling, and the queue of agents
  waiting for the writer reached 30 s and answered 503. Flushing is now a job on
  the same queue as ingest, so it always gets a turn, and it moves at most 200 000
  rows per turn so the reports behind it still commit promptly. Dictionaries
  (`stacks`, `frames`) stay hot while a backlog remains — they are deduplicated by
  id, and deleting one that a published sample still names would lose a symbol —
  and are flushed whole by the pass that finishes the backlog.
- One slow query cancelled every other query in flight. The per-request watchdog
  interrupted *all* read connections, so the dashboard's map and histogram died
  with `INTERRUPT Error: Interrupted!` whenever the flamegraph next to them ran
  long. An interrupt now only reaches reads that have been running for the
  watchdog's full timeout, which are exactly the ones the request layer is about
  to abandon. An interrupted query also answers `503` with an explanation instead
  of `500 database error: INTERRUPT Error: Interrupted!`.
- A single report could poison the catalogue's running total. `StackSample.value`
  was never bounded and the per-report sum was a plain `u64` addition, which wraps
  in a release build; one value at `u64::MAX` saturated the tenant's `total_value`
  for the life of the process. The dashboard divides by that number, so every
  percentage read zero and the tenant looked empty while its data was intact.
  Validation now bounds one stack (1e15 in the profile's own unit) and the report
  total (1e18), and reports are summed with `checked_add`.
- A canonical table was
  `SELECT * FROM t_hot UNION ALL BY NAME SELECT * FROM read_parquet([every file])`.
  DuckDB must open the footer of every path it is given — over the network for S3 —
  so a one-hour query over eight days of history paid for all eight days, on every
  dashboard poll. Files are now indexed by the epoch hour of the data they hold and
  each query names only the hours its window can touch; a one-hour window names one
  object. Files whose name carries no hour are still mounted for every window, so
  sharding can never hide data it cannot attribute.
- `enable_http_metadata_cache` and `parquet_metadata_cache` both default to
  `false` and were never set. With them off, one archive object was GET/HEAD-ed
  151 times in a minute. Both are now on, per connection.
- `SET http_timeout=15000` was meant to be 15 seconds; DuckDB reads the unit as
  seconds, so it was 4.16 hours. A half-open S3 socket therefore held a connection
  for hours and the query watchdog could not break the blocking read — "restart the
  server and it works again". Now 15 seconds, with `http_retries=2`.
- Reads shared one `Mutex<Connection>` with ingest and flush, so a wide scan queued
  every other request until the 60-second HTTP deadline. Reads now use a pool of
  cloned connections (default 4, `--query-concurrency`), which share the in-memory
  database through MVCC. Each clone is a fresh session, so the tuning and S3
  settings are re-applied per slot. Interrupts are aimed only at readers, never at
  the writer.
- `/api/v1/stats`, `/api/v1/tenants` and `/metrics` each ran `COUNT(*)` plus
  `COUNT(DISTINCT …)` over the full archive on the request path. The counts,
  sums and time bounds are now a per-tenant aggregate that is read once and then
  *maintained*: an append folds its own row in before it gives the writer back, a
  prune subtracts the rows it just deleted (and recounts only when retention
  removed whole files, whose rows cannot be read any more), and flushing,
  compaction, archiving and service-map windows leave it alone (they move the
  same rows, or touch tables the aggregate does not cover). Reading it is a
  mutex and a clone — no scan, no TTL. Counting under
  the writer is what makes it exact: there is no instant in which a batch is
  committed but not yet counted, so the recount cannot overlap an append and
  lose it. An earlier cut of this counted on a read clone and cached the result
  afterwards, which dropped exactly those batches (the dashboard was one short
  until the cache expired); `a_poll_during_ingest_never_undercounts` now pins
  it. The distinct-series count has no exact incremental form, so it stays a
  scan on a ten-minute clock — that scan alone measured 11 s on a
  4-million-row tenant and used to stall one poll per minute. `bind()` no longer
  computes tenant totals before it starts serving, which was failing the
  liveness probe in a ten-tenant cluster; a background task warms the aggregate
  of each tenant after startup instead.
- Starting a server with ten tenants took longer than the startup probe's budget,
  so the pod was killed and restarted in a loop before it ever served. Two causes:
  object discovery issued twelve LISTs per tenant (two patterns per table), and
  tenants were opened one after another. Discovery is now a single
  `glob('{base}/*/*.parquet')` — one LIST for the prefix plus one per table, which
  is also why archived keys are flat — and tenants open four at a time. Measured
  on the local two-node cluster: 119 s for ten tenants instead of timing out.
  Opening a tenant no longer holds the registry lock either, so one slow tenant
  cannot block the others.
- Ingest waited on the writer with no timeout. Behind a slow scan that turned into
  75–313 second lock waits, agent upload timeouts, retries and `report queue full`.
  Ingest now waits at most 30 seconds and answers `503` with `Retry-After`, which
  the agent already treats as retryable.
- `load_stacks` and `load_frames` staged rows into one fixed-name temporary table.
  That was safe when reads were serialised; with concurrent readers two requests
  could clear each other's staging rows. The name is now unique per call.
- The hour a Parquet fragment is filed under was computed in SQL as
  `CAST(start_ns/1_000_000_000/3600 AS BIGINT)`. DuckDB's `/` is floating point
  and the cast rounds to nearest, so an event at 13:43 became hour 14: half of
  every hour's data sat in the next hour's directory and was invisible to a query
  whose window ended at "now". The partition now uses integer division (`//`),
  in both the flush and the legacy-export path.

### Changed

- `stacks` is sharded by the hour of the samples that name it, like every other
  table. It is a dictionary with no timestamp of its own, so it used to be
  written to `hour=0` and mounted for *every* window — a one-hour flamegraph read
  the entire dictionary history, 57 MB across 63 objects for the eight-day tenant
  on the local cluster, which is where the extra ten seconds per flamegraph and
  per `top` went. Stack ids are allocated per batch rather than deduplicated
  across batches, so a stack row is only ever referenced by samples from the same
  flush: filing it under the same hour is exact, not an approximation. Rows stay
  in the hot tier while later slices still need to resolve them and are cleared
  by the pass that drains the tier. Files written by earlier releases remain in
  `hour=0` and are mounted for any window that reaches back to the hour the first
  sharded file was written, so an upgrade loses no symbols; they age out with
  retention.
- Writes go through one writer thread and a FIFO queue instead of a mutex.
  DuckDB serialises write transactions anyway, so the only real choice is who
  waits, and a mutex made every caller hold a thread and a request slot while it
  queued — while a background pass could not get in at all. `Store::append` and
  `append_servicemap` now enqueue and wait for the answer, so the commit is still
  synchronous and a caller that gets `Ok` has its batch on disk; what changed is
  that the wait is a queue position rather than "however long a flush holds the
  connection". The queue is bounded at 512 jobs and a producer that cannot be
  admitted in 30 s gets `503` with `Retry-After`, which is the same signal the
  agent already retries on. `Store::open` keeps the direct path for tests and
  embedded use; `Store::open_shared` — what the tenant registry uses — starts the
  thread.
- New Parquet writes are sharded into `hour=<n>/` directories locally, and named
  `hour=<n>-part-<stamp>.parquet` on S3 — flat, because a `glob()` resolves one
  directory at a time and an `hour=<n>/` prefix cost one LIST per archived hour
  at start-up (twelve LISTs per tenant became one per table). Files from every
  earlier release (`h<hour>-<stamp>-<rows>.parquet`,
  `h<hour>-final-<stamp>.parquet`, `hour=<n>.parquet`) are still discovered and
  read; nothing is migrated or rewritten. Those files were tagged with the wall
  clock at write time, so a flush that straddled an hour boundary can hold the
  previous hour's rows; while such a file is still present, its window is
  widened by one hour on each side (two extra objects) instead of losing those
  rows after an upgrade.

### Upgrade notes

No schema change: the DuckDB database is in-memory and nothing durable changed
shape, so upgrading is an image swap. Existing local directories and S3 objects
are indexed at startup exactly as before — the index is rebuilt by scanning the
local tree and by one `glob()` per prefix, so a fresh pod against an existing
bucket still sees its history.

## [0.1.18] - 2026-11-24

Remove an S3 option that could not do what it claimed.

### Fixed

- 0.1.17 added `--s3-max-parts-per-file` for endpoints that reject multipart
  uploads. DuckDB's httpfs always writes through `CreateMultipartUpload`,
  `UploadPart` and `CompleteMultipartUpload` — there is no single-PUT code path
  — and the default `s3_uploader_max_filesize` made even a one-part setting
  invalid. The CLI flag, env var, Helm value and the setting itself are
  removed. `docs/deployment.md` now states the real requirement: an
  S3-compatible endpoint must implement multipart uploads, and a PUT-only test
  double cannot archive. Existing volumes and objects are unaffected.

## [0.1.17] - 2026-11-24

S3-compatible endpoints that do not implement multipart uploads can now archive
Parquet instead of backing off forever on HTTP 501.

### Added

- `--s3-max-parts-per-file` (env `LIDLESSEYE_S3_MAX_PARTS_PER_FILE`, Helm
  `server.storage.s3.maxPartsPerFile`) sets DuckDB's
  `s3_uploader_max_parts_per_file`. Set it to `1` to write each archive object
  with a single PUT on gateways and test servers that reject multipart uploads.
  Values outside DuckDB's accepted range (1..=10000) are rejected at startup.
  The default stays unset, so AWS S3 keeps multipart uploads.
- `docs/deployment.md` documents when to use the single-PUT mode and its
  memory trade-off.

### Upgrade notes

No storage layout change: existing Parquet files, DuckDB migrations and S3
objects are read exactly as before. The flag only changes how new archive
objects are uploaded.

## [0.1.16] - 2026-11-24

Storage no longer keeps a persistent DuckDB file that every agent and dashboard
request has to lock. Writes land in a process-local hot tier, then publish
immutable Parquet files; DuckDB is now the in-process query engine over both.

### Changed

- The canonical storage modes are `disk` and `s3`, with `disk` as the default.
  Neither mode creates or opens `lidlesseye.duckdb`. The legacy `duckdb` and
  `parquet` values are still accepted and mapped to `disk` / `s3`, so existing
  Helm values do not silently start an empty store.
- A tenant now has one hot connection for ingest and queries and a separate
  maintenance connection for Parquet compaction and S3 uploads. Long archive
  work can no longer hold up the request path behind the same store lock.
- Flush, compaction and retention write only Parquet. Files are created as
  `.tmp`, read back for a row-count check, then atomically published; readers
  apply a newest-final-wins rule so a crash cannot double-count superseded
  fragments. All tables in one flush share a generation stamp, and retention
  plus `maxDisk` evict whole generations from oldest to newest.
- Partitions use the data's own time column rather than the flush clock, so
  backfilled samples are retained according to when they happened.
- In `s3` mode the local files are unconfirmed staging. Uploads probe the
  destination, read the object back and compare row counts before staging is
  deleted; verification failures keep the only local copy. Staging is never
  removed by the local disk cap, and a full staging area returns HTTP 503 so
  agents can retry instead of silently writing data that may be lost.
- `server.storage.flushInterval` (CLI `--flush-interval`, default `10s`) is the
  hot tier's RPO. `POST /api/v1/flush` requests an immediate flush from the
  running server, and `lidlesseye flush --server …` calls that endpoint. CLI
  `ingest --data-dir` flushes before it exits, because another process cannot
  see the first process's in-memory hot tier.

### Added

- `/api/v1/stats` exposes per-connection lock waits and holders, and request
  logs identify `read_slot_wait`, `store_lock_wait`, `query_exec` and
  `store_lock_hold`. These diagnostics make it possible to confirm that an
  incident is lock contention before changing the connection model again.
- `lidlesseye_parquet_files_pruned_total` reports file-level retention work.
- On first open, an existing `lidlesseye.duckdb` is exported table by table to
  Parquet, verified by row count, then renamed to
  `lidlesseye.duckdb.migrated-<stamp>`; its WAL is retained under the same
  suffix. A second open does not migrate again.

### Fixed

- Multiple agents no longer queue behind a single-writer DuckDB file lock while
  dashboard queries are running.
- The disk cap now counts published Parquet bytes and removes complete old
  generations; it no longer loops over stale in-memory batch totals.

### Upgrade notes

This volume migration is one-way: an older server cannot read the Parquet-only
layout. Snapshot the PVC before upgrading, canary on a copy, and roll back by
restoring that snapshot rather than by redeploying the old image. See
`docs/deployment.md` → “Canary and rollback” for the full checklist.

## [Unreleased]

## [0.1.5] - 2026-11-23

A busy server stays alive and answers, instead of getting killed by its own
liveness probe.

### Fixed

- **A busy server stopped answering entirely, and kubelet killed it.** Five
  endpoints (`/api/v1/stats`, `/api/v1/series`, `/api/v1/profile-types`,
  `/api/v1/labels`, `/api/v1/label/{name}/values`) ran their DuckDB query as a
  plain blocking call inside the async handler. The runtime has four worker
  threads, so four concurrent panel refreshes owned all of them and nothing
  else could be polled — including `/api/v1/health`, which reads no store at
  all. The probe timed out, kubelet restarted the container (`exitCode 137`),
  and every request that had been waiting came back as the 60 s request
  timeout. All store reads now run off the runtime like the other query
  endpoints, and a request that cannot get a read slot within 15 s gets a 503
  that says so instead of a 408 that looks like a broken server.
- The number of in-flight store reads is capped (`--query-concurrency`,
  default 4, `server.queryConcurrency` in the chart). Uncapped, each waiting
  request held a blocking thread while it queued on the single DuckDB
  connection; a 12-hour-old pod had 1507 threads, could no longer fork, and had
  grown to 1.2 GB of RSS. The queue now lives in async land, where waiting
  costs no thread, and the blocking pool is capped at 64 as a backstop.
- A read that cannot get its tenant's single DuckDB connection within 45 s now
  fails with `503 store is busy`, rather than joining an unbounded queue that
  the request timeout would eventually turn into a 408. A busy store is an
  error, not zero rows: `/api/v1/stats` no longer reports an empty install when
  it was simply refused.
- Parquet mode ignored `--duckdb-threads`. It opens an in-memory database and
  called `Connection::open_in_memory()`, which has no config argument, so the
  configured value never reached DuckDB and every scan ran on one thread per
  core (20 on the machine that reported this) instead of 4.
- Leftover DuckDB spill files are deleted at open. They belong to the pid that
  wrote them and nothing reclaims them afterwards; one deployment carried 11 GB
  of `.duckdb-tmp` on the volume that also holds its tenants.
- A failed S3 archive no longer retries every ten seconds. Each attempt
  re-read and re-compressed the whole hour before failing identically, so a
  bucket that rejects multipart uploads turned the compaction tick into a
  permanent full-time scan. Failures now trip a per-tenant circuit breaker with
  exponential backoff (10 min → 20 min → 1 h), one failed hour no longer aborts
  the rest of the pass, and the log names the table, hour and retry delay.
- Background maintenance (hot flush, local compaction, S3 archive, retention)
  yields to queries: it waits at most half a second for the connection and
  starts only after the store has been unread for five minutes. A merge that
  took minutes on the single connection used to make every dashboard poll
  wait behind it. Passes are bounded to one hour of one table.
- The server bound its socket before opening every tenant store. In Parquet
  mode that lists each tenant's S3 prefix, which can take minutes on a large
  installation; a bound but unserved socket accepts connections and answers
  nothing, so the liveness probe timed out during startup and kubelet killed a
  process that was merely starting. Totals are now read before the socket is
  opened, and the chart has a startup probe with a five-minute budget.
- `/api/v1/stats` computes its row counts and distinct-series count in one
  cached snapshot instead of two uncached scans per poll.
- The tenant control is always on screen, seeded with `default`. It used to be
  hidden whenever the tenant list came back empty or had not loaded yet, which
  is the state a brand-new server is in — every query was being answered by
  `default` while the header showed no tenant at all, so an empty install
  looked like it had nowhere to write.

## [0.1.4] - 2026-11-23

A fresh install is usable again: the tenant catalogue can be read before a
tenant is named, which is the state every dashboard starts in.

### Fixed

- **A fresh install with `--allowed-tenant` had an unusable dashboard.**
  `GET /api/v1/tenants` resolved a tenant before answering, and the caller is a
  browser that does not know one yet, so a server whose allowlist excluded
  `default` refused the one request that could have named a usable tenant —
  403 on the catalogue, an empty picker, and no way to recover from the UI. The
  catalogue now checks only the token, ignores the tenant header, and filters
  its *rows* by the allowlist. An allowed tenant that has not been written to
  yet is listed with zero counts, because it exists as configuration and the
  first upload needs somewhere to go.
- A browser that stored a tenant id could get every request refused with 403
  after a tenant-scoped token was configured: the token pins the tenant and the
  persisted header named a different one. The dashboard now drops the header
  and retries once when the server says the token is scoped, instead of needing
  localStorage cleared by hand.
- `docs/api.md` showed `count` and `max_tenants` in the `/api/v1/tenants`
  response; the handler has never returned either.
- A tag published the image as `v0.1.3` and `0.1`, while the chart's
  `image.tag` resolves to `appVersion` — `0.1.3`, with no `v` — so a fresh
  `helm install` asked the registry for a tag nothing had created. The release
  workflow now publishes the bare version as well, and `docs/deployment.md`
  says to check both registries for it after a tag push, since a missing tag
  there is an `ImagePullBackOff` rather than a warning.

## [0.1.3] - 2026-11-23

Memory profiling arrives, in two readers that never touch the same process: the
Go runtime's own heap profile for Go, and the allocator's entry points for
everything else. The map also stops borrowing a unit per label, and its arrows
become geometry.

### Added

- **Memory profiling, for every language.** Go processes are read from the
  runtime's own profile chain (`runtime.mbuckets`), so `inuse_space` is the
  runtime's number at the runtime's sampling rate. Everything else is counted
  at the allocator: uprobes on `malloc`, `calloc`, `realloc`, `free`,
  `aligned_alloc` and `posix_memalign` (plus the `je_`, `mi_` and `tc_`
  prefixed families, so jemalloc, mimalloc and tcmalloc are read through their
  own names), attributed to the allocating stack. Four profile types —
  `alloc_space` and `alloc_objects` as rates, `inuse_space` and
  `inuse_objects` as levels — labelled `profile_source`, `runtime` and
  `allocator` so a runtime's own profile and an allocator's are never summed
  together. `--no-alloc` / `--no-heap` turn either reader off, `--heap-objects`
  adds the object counts. Validated against four programs' own ledgers: a C, a
  C++ and a Rust fixture match to the byte once their untracked internals
  (allocator headers, `Rc` boxes, pointer arrays) are accounted for, and a Go
  fixture agrees with `runtime.MemStats` to within 0.3 %.

### Changed

- A hop that climbs or falls much further than the column gap now sweeps out
  and plugs into the card's top or bottom edge instead of being forced into an
  S-curve with horizontal tangents at both ends — at a 4:1 height-to-width
  ratio that S collapsed into a near-vertical straight with two short stubs,
  which is what made the arrow on it look wrong.
- Map arrowheads are geometry, not SVG markers. A `marker` cannot inherit
  `stroke-opacity` — its fill stayed opaque while the line faded to 40 %, which
  is what made the head look pasted onto the line — its box scales with
  `stroke-width`, and on a dashed edge it can end up floating a few pixels past
  the last dash. The head is now a dart with a notched back, drawn in the same
  ink as its line at the same alpha, with a short solid neck that no dash phase
  can detach. Backward hops also hook into the card vertically instead of
  grazing its top edge, so their heads point into the card rather than along it.
  The head's axis is the chord of the curve it covers, not the tangent at the
  last pixel: on a bending line the two differ enough to make the head look
  rotated against its own stroke.
- The node inspector's Memory tab picks the level to plot instead of the first
  type whose name contains "memory": `inuse_space`, then `alloc_space`, then
  the object counts. A level and a rate are worded differently — "held now"
  against "allocated this interval" — because they answer different questions
  and only one of them adds up over time.
- Edge latency is printed in milliseconds, one unit for the whole map. A label
  used to choose its unit per value — `149µs` on one hop, `1.4ms` on the next —
  which is right in a panel, where the number is the subject, and wrong on a
  map, where the labels form a column the eye scans: the reader had to convert
  before seeing which hop was slower. Precision carries the magnitude instead
  (two decimals below a millisecond, one below a hundred, none above), so a
  label stays three digits wide, and a hop faster than that precision reads
  `<0.01ms` rather than `0ms`, because a zero is a claim the measurement cannot
  make.

### Fixed

- The compatibility table called the Go profile chain `mProf`. The reader walks
  `runtime.mbuckets` — the symbol the runtime actually exports — and the table
  no longer says that Go runtime profiles are unsupported while the row above
  it describes reading one.

### Known limits

- Memory profiling counts allocations, not memory. `inuse_*` is the sum of the
  blocks a probe saw allocated and not yet freed: allocations made before the
  probe attached are invisible, `mmap`-backed arenas and allocators that never
  call `malloc` (a custom `#[global_allocator]`) are out of scope, and there is
  no RSS/PSS series — so a heap level cannot be read as, or compared against,
  the process's resident memory. The Go reader is passive and never writes
  `MemProfileRate`; a process that disabled sampling is skipped and counted in
  the agent's `heap_disabled` statistic rather than reported as zero.

## [0.1.2] - 2026-11-22

The service map becomes something an operator can work with rather than look
at: search, a one-hop focus, a layout that stays readable when the graph is
forty columns wide, a key for the four edge styles, external traffic split by
the destination host, and a time chart that can plot bytes and retransmits as
well as connections.

### Added

- The four crates are on crates.io at `0.1.2`: `lidlesseye` (builder and CLI),
  `lidlesseye-server` (ingest, query API and the embedded dashboard),
  `lidlesseye-core` (model, DuckDB store, query engine) and `lidlesseye-ebpf`.
  `cargo install lidlesseye` and `cargo add lidlesseye` now resolve.
- The service map names the destination a client actually asked for. An HTTP
  request's `Host:` header and the text of a MySQL or PostgreSQL statement now
  travel from the probe to the edge, and the node panel shows them under
  *Requested hosts* and *Recent SQL*.
- An optional TLS uprobe (`LIDLESSEYE_SSL_UPROBE=1`) reads the plaintext of
  `SSL_write` from every libssl, GnuTLS and NSS it can find, so HTTPS and
  encrypted database traffic contribute the same detail. Off by default: every
  TLS write pays a user-memory read.
- The flamegraph reports how many samples a frame holds, not just how much
  time: every node carries its own sample count, shown on hover and in the
  details panel. The two ratios differ because a sample weighs a fixed amount,
  so "10% of the time in 2% of the samples" is visible instead of hidden.
- Hovering the histogram brush shows the bucket's time and sample count under
  a guide line, and the brush has its own −/+ zoom around the window centre.
- The flamegraph toolbar zooms in, out and back to the whole graph, and
  **Export** writes the current view as SVG or the payload plus view state as
  JSON.
- The compare view names both windows above the graph — clock times, lengths,
  totals, delta — and **Swap** exchanges them in one click.
- Two cards on the same side of a neighbourhood that talk to each other are
  joined by an arc drawn outside their column, so a set of callers no longer
  looks unrelated. The six heaviest pairs get a lane each and the note under
  the map counts the rest; a caller and a callee are not joined, because both
  already connect to the selected workload and a curve through it would claim
  something else.
- A workload that both calls and is called now gets **two cards**, one per
  side. Keying by node id made the second overwrite the first, which left a
  hole in one column and drew that side's hops to a card across the graph.
- The service map has a **node search** (`/` or Ctrl/Cmd-K): a ranked list over
  names, ids and namespaces; a hit is centred in the part of the canvas the
  drawer leaves visible, selected, and zoomed to a readable scale. Results
  close when the pointer goes anywhere else.
- A **focus mode** on the map: the selected node and its one-hop neighbours are
  relaid out on their own, the drawer narrows to give the map room, and a pill
  names the node in focus. Losing the selection ends the mode.
- The map groups its drawing into labelled **bands** by kind — workloads,
  ClusterIPs, external — and sorts each column by kind so a band is possible at
  all. Cards now carry a third line: new connections per second and on-CPU time
  for the window.
- Hovering a card drops every line that does not touch it to a hint, so the
  pointer answers "what does this talk to" on a map whose overview is a
  hairball — without selecting the node first.
- External traffic is split by destination host: `external/api.example.com`
  is its own node with its own history, built from the `Host:` the agent
  already reads, instead of one grey "outside the world" blob. Destinations
  with no observed host stay on the aggregate `external` node, and the split is
  capped at the busiest 40 hosts so a crawler cannot crowd out the cluster.
- The map draws **one stroke per pair** of nodes. The payload keeps an edge per
  protocol and container pair for the panel, so a pair with two protocols drew
  two identical curves: the top one hid the other, their labels landed on each
  other, and a dotted hop under a solid one looked like a line that stops in
  mid-air.
- The row above the map carries an **edge key** — `call`, `selected`,
  `retransmit`, `outside` — drawn from the same colour and dash constants the
  canvas strokes with, plus `arrow = direction · width = traffic`. Four line
  styles used to be learnable only by hovering one.
- Edges state direction and weight separately: arrowheads on the target end,
  stroke width from traffic, a bright emphasis colour for the selected node's
  hops, and a dotted cooler line for anything that leaves the cluster.
  Hovering a line reveals its full protocol, rate and RTT.
- The map's time chart plots one of three counters — connections, bytes or
  retransmits — from `/api/v1/servicemap/histogram?metric=…`, so the brush can
  answer "was it more calls, more data, or more retries?".
- The map has fit, zoom and reset controls. The automatic fit keeps a readable
  scale instead of shrinking a forty-column graph to a ribbon; *fit* still
  shows the whole drawing, and a card that takes keyboard focus pans into view.

### Changed

- `MapHistogram` returns `points` plus the `metric` it holds, instead of a
  `connections` array. The field named the counter, which stopped being true
  once the brush could plot bytes.
- The map canvas clips with `overflow: clip`. A hidden box is still a scroll
  container, so focusing a card scrolled the layer inside it and slid the whole
  drawing sideways with the transform unchanged.
- Edge labels default to appearing at 85 % zoom rather than 62 %: at the
  automatic fit of a large map they turned the drawing into a wall of pills,
  and direction plus width still carry the structure.
- The neighbourhood pans from anywhere on it, cards included, and its surface
  no longer selects text: dragging used to select the card labels, and the next
  drag became a native text drag that the browser turns into a search. The
  pointer is captured only after it moves, so a click on a card still opens it.
- Payload parsing moved to the agent. The kernel copies the first bytes of a
  request into a perf ring buffer and the agent reads the host and the SQL out
  of them. Protocol naming stays in the kernel, in its own program: the two
  halves are verified separately because together they crossed the verifier's
  instruction ceiling, which is why the send probe used to fail to load. The
  classifier now uses constant byte offsets instead of masked variable
  indexing, which is what made the state count fit.
- The payload pointer's offset is read from the running kernel's BTF instead of
  a CO-RE field name, because kernel 6.16 renamed `iov_iter.iov` to `__iov`.
- The flamegraph separates reading from acting. A click selects a frame and
  opens a details panel beside the canvas (self, total, depth, call stack, copy
  name, zoom in) instead of moving the viewport; double click zooms. Labels are
  fitted to their bar with an ellipsis instead of being cut mid-glyph.
- The query bar is one form: profile, grouping and range on the left, refresh
  behaviour on the right, and a line under it saying which window, grouping and
  filter count the server actually queried. The button that said `Apply` now
  says what it does — controls apply on change, so it only re-runs the query.
- The flamegraph's surroundings are now a `GraphWorkspace`
  (`components/analysis/`, contract in `lib/analysis/renderer`): search, zoom,
  breadcrumb, details and export live in one shell behind a renderer interface,
  so a second profile type reuses them instead of re-implementing analysis.

## [0.1.1] - 2026-11-18

A packaging release: the image and the chart now ship from the same place. No
collection, storage or dashboard behaviour changed since 0.1.0.

### Added

- The workspace crates are packaged for publication: `lidlesseye` (builder and
  CLI), `lidlesseye-server`, `lidlesseye-core` and `lidlesseye-ebpf`. The
  dashboard ships inside `lidlesseye-server` — `build.rs` mirrors `web/dist`
  into the crate's `ui/`, which is what `cargo package` uploads and what
  `rust-embed` reads.
- `scripts/publish-crates.sh` publishes them in dependency order, waits for each
  version to reach the index and skips what is already there.

### Changed

- The release image is published to Docker Hub as well as GHCR, and the Docker
  Hub copy is a manifest copy of the GHCR image rather than a second build, so
  both names resolve to the same digest.
- The chart no longer pins a development tag: `image.tag` defaults to
  `appVersion`, which is the tag the release workflow publishes. Installing the
  chart attached to 0.1.0 pulled `0.10.59-hopmap` instead of the version the
  chart claimed to be.
- `image.repository` stays `docker.io/xsj321/lidlesseye`, which is now what the
  release notes and README tell you to run.

## [0.1.0] - 2026-11-18

The first public release: one Rust binary that profiles Linux processes with
eBPF, stores the samples in DuckDB and serves a dashboard that opens on the
service map.

### Collection

- On-CPU sampling (`PERF_COUNT_SW_CPU_CLOCK`) and off-CPU sampling
  (`schedule` kprobes), plus the virtual `wall` profile that merges both.
  Aggregation happens in the kernel, so overhead does not grow with the
  sampling rate.
- Connection accounting from the TCP stack: new connections/s, bytes,
  retransmits and mean round-trip time per (client, server) pair.
- Application protocol detection: the tracer sniffs the first payload of
  `tcp_sendmsg` and labels a hop HTTP/1, HTTP/2, gRPC, Postgres, MySQL,
  MongoDB, Redis, Memcached, Cassandra, Kafka, ClickHouse, ZooKeeper or
  FoundationDB, falling back to the server port when payload is unavailable.
- Symbolization in the agent (blazesym), so short-lived processes stay
  readable and the server never needs the profiled machine's filesystem.
- Kubernetes enrichment: pods on the agent's node are listed so profiles are
  named after workloads, and a pod's containers are reported per node.
- No SDK, no `LD_PRELOAD`, no recompilation: any process in any language.

### Storage and query

- DuckDB 2.0 as the storage engine, one file per tenant, with retention and a
  `fsck` command.
- Optional tiered Parquet mode: memory hot tier → local Parquet → hourly S3
  archive, read back through zero-copy DuckDB scans. Archive merges are
  schema-tolerant (`union_by_name`), so an older file without a newer column
  does not fail the rotation.
- Flamegraph, top functions, time series, label exploration, differential
  comparison and service-map endpoints, all sharing one time range and filter
  language (`now-1h`, RFC 3339, unix seconds).

### Dashboard

- Service map built from the agents' TCP tables and Kubernetes metadata. Edges
  carry live connections/s and mean RTT with unit-aware formatting; the
  histogram counts requests, not CPU samples.
- A click opens a sliding, non-modal inspector with one hop of neighbours and
  seven tabs (SLO, instances, CPU, memory, network, DNS, profiling). The
  neighbourhood draws **container-level** connections: a workload is one card
  that owns its container rows, and a hop starts and ends on the row that holds
  the connection.
- Clicking a node inside the neighbourhood walks the graph to that node's own
  neighbourhood instead of only highlighting it.
- Flamegraphs with search, zoom, breadcrumbs, an embedded per-node view and a
  comparison mode that colours a regression red.
- Range picking the way a Grafana user expects: choose a window, then brush the
  part of it that matters.
- English and Simplified Chinese, light and dark themes, keyboard-reachable
  cards and a skip link, all state in the URL so a view can be shared by link.

### Operate

- Multi-tenancy through `X-Scope-OrgID`, per-tenant tokens, retention and
  quotas.
- Prometheus metrics, a container image, a Helm chart (server `Deployment` plus
  a privileged per-node agent `DaemonSet`) and an embeddable Rust API
  (`LidlessEye::builder()`).
- Storage modes: a single DuckDB file per tenant, or S3-backed Parquet for
  long-lived datasets.

### Known limits

- Collection is Linux-only (≥ 5.5 with BTF); storage and the query API are
  portable Rust.
- One server per volume — no clustering or replication.
- User stacks need frame pointers; heap profiling covers Go's own buckets and
  native allocator uprobes, not JVM/Python runtime-level heap layouts.

[Unreleased]: https://github.com/xsj321/LidlessEye/compare/v0.1.23...HEAD
[0.1.23]: https://github.com/xsj321/LidlessEye/compare/v0.1.22...v0.1.23
[0.1.22]: https://github.com/xsj321/LidlessEye/compare/v0.1.21...v0.1.22
[0.1.21]: https://github.com/xsj321/LidlessEye/compare/v0.1.20...v0.1.21
[0.1.20]: https://github.com/xsj321/LidlessEye/compare/v0.1.19...v0.1.20
[0.1.19]: https://github.com/xsj321/LidlessEye/compare/v0.1.18...v0.1.19
[0.1.18]: https://github.com/xsj321/LidlessEye/compare/v0.1.17...v0.1.18
[0.1.17]: https://github.com/xsj321/LidlessEye/compare/v0.1.16...v0.1.17
[0.1.16]: https://github.com/xsj321/LidlessEye/compare/v0.1.15...v0.1.16
[0.1.15]: https://github.com/xsj321/LidlessEye/compare/v0.1.14...v0.1.15
[0.1.14]: https://github.com/xsj321/LidlessEye/compare/v0.1.13...v0.1.14
[0.1.13]: https://github.com/xsj321/LidlessEye/compare/v0.1.12...v0.1.13
[0.1.12]: https://github.com/xsj321/LidlessEye/compare/v0.1.11...v0.1.12
[0.1.11]: https://github.com/xsj321/LidlessEye/compare/v0.1.10...v0.1.11
[0.1.10]: https://github.com/xsj321/LidlessEye/compare/v0.1.9...v0.1.10
[0.1.9]: https://github.com/xsj321/LidlessEye/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/xsj321/LidlessEye/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/xsj321/LidlessEye/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/xsj321/LidlessEye/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/xsj321/LidlessEye/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/xsj321/LidlessEye/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/xsj321/LidlessEye/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/xsj321/LidlessEye/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/xsj321/LidlessEye/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/xsj321/LidlessEye/releases/tag/v0.1.0
