# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html): the
`0.x` series may still change the HTTP surface between minor versions, and the
Rust API follows the same rule.

## [Unreleased]

### Changed

- A hop that climbs or falls much further than the column gap now sweeps out
  and plugs into the card's top or bottom edge instead of being forced into an
  S-curve with horizontal tangents at both ends — at a 4:1 height-to-width
  ratio that S collapsed into a near-vertical straight with two short stubs,
  which is what made the arrow on it look wrong.
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
  The head's axis is the chord of the curve it covers, not the tangent at the
  last pixel: on a bending line the two differ enough to make the head look
  rotated against its own stroke.

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
- User stacks need frame pointers; CPU and off-CPU only, no allocation
  profiling yet.

[Unreleased]: https://github.com/xsj321/LidlessEye/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/xsj321/LidlessEye/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/xsj321/LidlessEye/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/xsj321/LidlessEye/releases/tag/v0.1.0
