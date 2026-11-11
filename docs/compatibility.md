# Pyroscope compatibility

LidlessEye is a drop-in *backend* for Pyroscope-style profiling: the same push
protocol, the same tenant header, the same query concepts. It is not a
reimplementation of Pyroscope's distributed storage, and it does not ship
language SDKs — the collector is eBPF, in-process, and the storage is a local
immutable-segment format designed to be faster than a block store on the same
disk.

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
| User stacks + ELF symbolization | yes | module/symbol/file/line resolved before the report is written |
| Process discovery | yes | every process with a stack is reported, per-PID |
| Kubernetes labels | yes | `enrich_kubernetes_labels`, namespace/pod/container/node |
| Node label | yes | `node_label` |
| PID exclusion | yes | `exclude_pids` |
| Container / label based filtering | no | exclusion is by PID only |
| Heap / allocation profiling | no | needs uprobes on the allocator; out of scope for this release |
| Go/Ruby/Python runtime profiles | no | those are SDK-driven; push them as pprof or folded instead |

## Operations

| Capability | Status | Notes |
| --- | --- | --- |
| Single binary + embedded UI | yes | `rust-embed`, no separate frontend process |
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

## What the differences mean in practice

- **Storage.** Pyroscope ships blocks to object storage and compacts them;
  LidlessEye writes one immutable segment per report and memory-maps it. A query
  touches exactly the segments whose time window overlaps, with no compaction
  step and no background rewrite. The trade is that there is no cross-node
  deduplication and no cold tier.
- **Scaling.** Pyroscope scales horizontally into microservices. LidlessEye
  scales by running one instance per node (or per tenant group) — which is what
  an eBPF agent needs anyway, since the profiles are node-local.
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
```
