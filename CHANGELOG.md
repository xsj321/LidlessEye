# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html): the
`0.x` series may still change the HTTP surface between minor versions, and the
Rust API follows the same rule.

## [Unreleased]

## [0.1.1] - 2026-11-18

A packaging release: the image and the chart now ship from the same place, and
the crates are on crates.io. No collection, storage or dashboard behaviour
changed since 0.1.0.

### Added

- The workspace crates are published: `lidlesseye` (builder and CLI),
  `lidlesseye-server`, `lidlesseye-core` and `lidlesseye-ebpf`, so
  `cargo add lidlesseye` is enough to embed the whole stack. The dashboard ships
  inside `lidlesseye-server` — `build.rs` mirrors `web/dist` into the crate's
  `ui/`, which is what `cargo package` uploads and what `rust-embed` reads.
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

[Unreleased]: https://github.com/xsj321/LidlessEye/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/xsj321/LidlessEye/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/xsj321/LidlessEye/releases/tag/v0.1.0
