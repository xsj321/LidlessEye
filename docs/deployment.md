# Deployment

Three shapes, from simplest to most production-like.

## 1. One binary, one host

```bash
sudo ./lidlesseye serve \
  --agent \
  --data-dir /var/lib/lidlesseye \
  --listen 0.0.0.0:4040 \
  --retention 7d \
  --max-disk 50GB \
  --frequency 99
```

This profiles *every* process on the machine and serves the dashboard from the
same process. Good for a dev box, a single VM, or "I want to know what this
server is doing right now".

Requirements:

| Requirement | Why | Check |
| --- | --- | --- |
| Linux ≥ 4.17 with BTF | BPF maps are declared with BTF; the object has no CO-RE relocations | `ls /sys/kernel/btf/vmlinux` |
| root, or `CAP_BPF`+`CAP_PERFMON`+`CAP_SYS_ADMIN` | loading programs, `BPF_MAP_TYPE_STACK_TRACE` | `id -u` |
| `perf_event_paranoid ≤ 2` | the on-CPU sampler opens a perf event per CPU | `cat /proc/sys/kernel/perf_event_paranoid` |
| `clang` (build time only) | compiles `profile.bpf.c`; a prebuilt object is committed as a fallback | `clang --version` |

If attaching a kprobe fails the collector logs a warning and keeps profiling
on-CPU; `lidlesseye serve --agent --no-offcpu` skips it deliberately.

## 2. Server + agents

The clean split for more than one machine:

```bash
# storage and query box
lidlesseye serve --data-dir /var/lib/lidlesseye --listen 0.0.0.0:4040 --retention 7d

# every node you want to profile
sudo lidlesseye collect --server http://storage:4040 --frequency 99 --flush 10s \
     --label env=prod --label region=eu-west
```

`collect` symbolizes locally and pushes `POST /ingest` every `--flush` seconds
(default 10s), with exponential backoff on failure. Before it starts sampling it
polls `/api/v1/health` for up to 60s, because a fresh install schedules the
agent DaemonSet before the server's Service has endpoints. Nothing is spooled to
disk: if the server is unreachable the batch is retried, then dropped, so a long
outage loses data rather than filling the node's disk.

Labels worth setting: `env`, `region`, `cluster`, `node`. The `node` label
defaults to the machine's hostname; inside a pod that is the pod name, which
changes on every rollout, so the Helm chart passes `spec.nodeName` as
`LIDLESSEYE_NODE_NAME` and the agent prefers it.

Kubernetes labels come from each process's cgroup when the agent runs with
`hostPID`: `k8s_pod_uid`, `k8s_qos_class` and `k8s_container`. They identify
*where* a stack was sampled without talking to the API server, which is why the
set stops at the UID — pod and namespace names are not in the cgroup path.

### Authentication

```bash
export LIDLESSEYE_AUTH_TOKEN=$(head -c 32 /dev/urandom | base64)
lidlesseye serve ...          # honours the env var
lidlesseye collect ...        # and so does the agent
```

The token is compared in constant time and applies to ingest and queries; the
dashboard prompts for it (it is kept in `localStorage`).

### Storage sizing

A CPU profile of a busy 32-core node at 99 Hz produces roughly
`100–300 KiB` per flush after symbolization, i.e. a few hundred MiB per node per
day. 10 nodes for 7 days fits comfortably in 20–50 GiB. Retention is enforced
two ways — `--retention 7d` (age) and `--max-disk 50GB` (oldest-first trim) —
and `lidlesseye fsck` verifies each tenant database plus the schema's referential
invariants (`--fix` drops dangling rows rather than the file).

Two sizing caveats come from DuckDB rather than from the sampler:

- The cap is measured against the estimated size of the *retained* batches
  (`/api/v1/stats` → `logical_bytes`), not the file size. DuckDB frees deleted
  rows inside existing row groups without returning blocks to the filesystem,
  so `total_bytes` stays at its high-water mark. Provision the volume for that
  high-water mark; freed space is reused by later writes.
- A database file is locked by one process. The Helm chart uses a single replica
  with the `Recreate` strategy for that reason; do not point two servers, or a
  `serve` process and a CLI `fsck`, at the same data directory. Use a read-only
  replica with its own copy instead.

## 3. Kubernetes

```bash
helm install lidlesseye deploy/helm/lidlesseye \
  --namespace observability --create-namespace \
  --set server.persistence.size=100Gi \
  --set server.auth.enabled=true \
  --set server.metrics.serviceMonitor.enabled=true
```

What the chart creates:

| Object | Purpose |
| --- | --- |
| `Deployment` (1 replica, `Recreate` strategy) | server: storage, API, dashboard |
| `PersistentVolumeClaim` | the data directory (or `persistence.enabled=false` for `emptyDir`) |
| `Service` (`ClusterIP`) | API + UI on port 4040 |
| `DaemonSet` | one agent per node, `hostPID: true`, `privileged: true` |
| `Secret` (when auth is on) | the bearer token, shared by server and agents |
| `ServiceMonitor` (optional) | scrapes `/metrics` |
| `ServiceAccount` | shared by both workloads |

### Local clusters (Docker Desktop, kind, minikube)

There is no registry to push to, so build the image locally and give every
build a **distinct tag**. A cluster caches the image by tag at pod creation, and
`imagePullPolicy: IfNotPresent` will happily keep running the previous build:

```bash
docker build -f deploy/docker/Dockerfile -t ghcr.io/lidlesseye/lidlesseye:0.1.0-dev1 .
helm install lidlesseye deploy/helm/lidlesseye -n lidlesseye --create-namespace \
  --set image.tag=0.1.0-dev1 \
  --set server.persistence.size=5Gi
```

Reach the dashboard with `kubectl -n lidlesseye port-forward svc/lidlesseye 4040:4040`.
`NodePort` and `LoadBalancer` services are not reliably exposed on the host by
these distributions, so port-forward is the dependable option.

### Privileges the agent needs

The DaemonSet is privileged on purpose: `CAP_BPF`/`CAP_PERFMON` to load
programs, `CAP_SYS_ADMIN` for `BPF_MAP_TYPE_STACK_TRACE` and
`BPF_F_USER_STACK`, and `hostPID` so `/proc/<pid>/maps` actually describes the
processes being sampled. A namespace with the `restricted` Pod Security
Standard label will reject the DaemonSet; label the namespace
`pod-security.kubernetes.io/enforce=privileged` or install the agent separately.

The server needs none of this — run it unprivileged, on a node pool with a real
disk. The exception is `server.profiling.enabled`, which makes the server sample
its own node; the chart then adds `hostPID`, a privileged container and the
debugfs mount to that pod, because the BPF programs cannot load without them.

### Tenants

Agents write to `agent.tenant` (default `default`). If `server.tenants.allowed`
is non-empty, that tenant has to be in the list or the server rejects every
upload with `403`. Agents and the dashboard always send the header alias
`X-Tenant-Id`, never the configured `server.tenants.header`, so renaming the
header for a gateway in front of the server cannot divert their traffic.

### Multi-node without `hostPID`?

Not supported yet. Without the host PID namespace the agent only sees its own
container's `/proc`, and every stack would symbolize as `[unknown]`.

### Taints

The agent tolerates every `NoSchedule` taint by default so that it also covers
control-plane and dedicated nodes. Remove the toleration if you only want
profiles from "normal" nodes:

```yaml
agent:
  tolerations: []
```

## Verifying an image or a chart before shipping it

Two scripts check the artefacts rather than the source tree. Both need only
Docker, so they work on a laptop that has no Rust toolchain:

```bash
./scripts/deploy-smoke.sh     # build deploy/docker/Dockerfile, run it, exercise the API
./scripts/ui-smoke.sh         # drive the dashboard in Chromium
helm lint deploy/helm/lidlesseye
helm template demo deploy/helm/lidlesseye -f my-values.yaml
```

`deploy-smoke.sh` is the one that catches packaging mistakes: a release build
that dropped the embedded dashboard, an image whose data directory is not
writable, an ingest path that only fails outside `cargo test`. It asserts the
health endpoint, the embedded assets, a real ingest, the flamegraph total,
tenant isolation, the diff endpoint, and that `/metrics` carries no tenant ids.

## Choosing parameters

| Flag | Default | Notes |
| --- | --- | --- |
| `--frequency` | 99 | samples per second per CPU. 99 is plenty for flamegraphs; 999 gives finer detail at ~10x the samples and proportional storage |
| `--flush` | 10s | how often the kernel maps are drained. Shorter = fresher data, more batches (one transaction per batch) |
| `--no-cpu` / `--no-offcpu` | both on | off-CPU needs kprobes; disable it on locked-down kernels |
| `--service-name` | process name | set it to aggregate a node under one service; the process name stays as a `process_name` label |
| `--max-disk` | 50GB | soft cap on retained data; oldest batches are dropped first |
| `--duckdb-threads` | all cores | threads DuckDB may use per query. All tenants share this process-wide setting |
| `--read-only` | off | rejects ingest, useful for a query-only server with its own copy of the data directory |

## Monitoring

Scrape `/metrics`. The two metrics that matter operationally:

- `lidlesseye_ingest_errors_total` — agents failing to push (check auth, disk
  full, body limit)
- `lidlesseye_query_duration_seconds_total` / `lidlesseye_queries_total` — mean
  query latency; a rising mean means more batches (more rows in `samples`) fall
  inside the dashboard's default range (lower `--retention`, narrow the range,
  or give DuckDB more threads)

`lidlesseye_storage_bytes` vs your volume size is the other one to alert on.
Compare it with `logical_bytes` from `/api/v1/stats`: a large gap means freed
space is sitting in the file waiting to be reused, not that retention has
stopped working.

## Troubleshooting

**`failed to load eBPF: the verifier rejected the program`**
The kernel is too old or the object was built for a newer one. Check
`ls /sys/kernel/btf/vmlinux`; rebuild with `LIDLESSEYE_BUILD_BPF=1`.

**`could not attach the CPU sampler to any CPU`**
`perf_event_paranoid` is 3, or the container lacks `CAP_PERFMON`:
`sysctl -w kernel.perf_event_paranoid=2`.

**Everything symbolizes as `[unknown]`**
The agent cannot read the profiled processes' files. In Kubernetes that is
almost always a missing `hostPID: true`; in Docker it is a missing
`--pid=host`. Binaries deleted at runtime fall back to the symbolic path in
`/proc/<pid>/maps` and cannot be opened at all — that is expected.

**off-CPU profiles are empty**
`finish_task_switch` may be absent or renamed on this kernel. Run
`RUST_LOG=lidlesseye_ebpf=debug lidlesseye collect --server …` and look for
`no candidate symbol could be attached`. The on-CPU profiler keeps working.

**The dashboard is empty but agents report success**
Check the profile type: agents produce `cpu` and `off_cpu`, the UI defaults to
`cpu`. A node that is idle for a minute legitimately has almost no CPU samples;
switch to `wall` to see blocked time.

**`open_files` / memory grows forever**
With DuckDB there is one file per tenant, never one per report, so service-name
cardinality cannot create files — but it does create distinct label strings in
`samples`, which cost dictionary space. Set `--service-name` to aggregate, or
lower `--retention`. If the resident set grows with the number of distinct
frames, that is the `frames` dictionary plus the in-memory frame cache;
`--duckdb-threads` bounds DuckDB's own worker memory.

**`database is locked` / `Could not set lock on file …`**
Another process has the tenant database open. DuckDB allows one writer per
file: stop the second `serve` instance (or the CLI `query`/`fsck` running
against the same data dir) and retry. Two replicas must each have their own
volume.
