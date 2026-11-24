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
| Linux ≥ 5.5 with BTF | the object reads `task_struct` through CO-RE relocations, which the loader resolves from the kernel's own BTF, and needs `bpf_probe_read_kernel` | `ls /sys/kernel/btf/vmlinux` |
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

### What the agent profiles

The DaemonSet runs with `hostPID: true`, which is what makes it a *node*
profiler rather than a profiler of its own pod. Every task on the node is
sampled, and each sample is named with the pid that the agent's `/proc` can
resolve — the number the kernel keeps for the agent's own PID namespace, not the
one from the initial namespace (see
[PID namespaces](architecture.md#pid-namespaces)). A pod in another namespace
therefore shows up under its own process, cgroup and labels, and nothing is
silently attributed to lookalike pids.

Two consequences worth knowing:

- `unattributed` in the collector log counts samples of tasks that are *outside*
  the agent's namespace tree. On a node it is ~0; anything larger means the
  agent lost `hostPID` (or runs in a nested namespace of its own, as a
  kind-in-Docker node does — there a few kernel-thread samples are expected).
- Kubernetes labels come from each process's cgroup: `k8s_pod_uid`,
  `k8s_qos_class` and `k8s_container`. They identify *where* a stack was
  sampled without talking to the API server, which is why the set stops at the
  UID — pod and namespace names are not in the cgroup path.

### Naming series after the pod

A cgroup path says `pod 8f1c2e64-…`, which is precise and unreadable. When the
agent can reach the API server it also fills in what that UID *is*:

| Label | Example | Where it comes from |
| --- | --- | --- |
| `k8s_namespace` | `payments` | pod metadata |
| `k8s_pod` | `checkout-7d9f8b6c4-mn2xk` | pod metadata |
| `k8s_workload` | `checkout` | owner workload; the ReplicaSet's `pod-template-hash` is stripped so a rollout does not rename the service |
| `k8s_workload_kind` | `Deployment` | `ReplicaSet` whose hash was stripped, otherwise the owner kind |

The series name becomes the service instead of the binary: `app.kubernetes.io/name`
if the pod has it, else `app` / `k8s-app`, else the workload name. A pod that
runs `bash` in a Deployment called `cluster-spin` is therefore `cluster-spin`,
with `process_name=bash` kept as a label so the binary stays addressable.
`--service-name` still wins over all of it, and `--no-k8s-pod-metadata` turns the
lookup off entirely (the `k8s_*` cgroup labels stay).

Two kinds of pod stay under their process name, both deliberately. A *static*
pod — the control plane: `etcd`, `kube-apiserver`, `kube-scheduler` — is named in
its cgroup by a 32-character hash that the API never returns, so there is no UID
to look up; its process name is the name you would have chosen anyway. A pod
that has no owner (created directly, without a Deployment) has no workload
either, though it still gets `k8s_namespace` and `k8s_pod`.

The lookup is a `list pods` with a `spec.nodeName` field selector, refreshed
every `--k8s-pod-refresh` (30s by default). Pods are stored against their UID
from the moment they are sampled; only the *name* waits up to one interval, so a
short-lived pod can stay unnamed without losing its samples. The Helm chart
creates the `ClusterRole`/`ClusterRoleBinding` that allows exactly that
(`agent.podMetadata.createRBAC`); grant `list` on `pods` yourself and set it to
`false` if your cluster already provides it.

Outside a cluster nothing is attempted: `KUBERNETES_SERVICE_HOST` **and** a
mounted service account are both required, so a host install, a plain container
and a CI runner keep naming series after the process.

### Encrypted request detail

The service map reads a client's `Host:` header and the text of a MySQL or
PostgreSQL statement from the first bytes of a write. Plaintext traffic needs
nothing; TLS hides those bytes, so the agent can attach a uprobe to the TLS
library instead:

```bash
helm upgrade lidlesseye deploy/helm/lidlesseye \
  --reuse-values --set agent.sslUprobe=true
```

The chart sets `LIDLESSEYE_SSL_UPROBE=1`, which attaches `SSL_write` /
`SSL_write_ex` to every `libssl`, GnuTLS and NSS it finds — the system
directories and each `/proc/<pid>/root`, deduplicated by inode — so a container
with its own copy of OpenSSL is covered. It is off by default because every TLS
write then pays a user-memory read and a protocol check. Go binaries and Java
use their own TLS stacks and are not covered; HTTP/2 keeps its headers
compressed and contributes no host either way.

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
day. 10 nodes for 7 days fits comfortably in 20–50 GiB.

Neither canonical storage mode writes a persistent DuckDB file. DuckDB exists
only inside the server process, as the query engine for the current hot tier;
the durable dataset is Parquet files on the PVC (`disk`) or verified objects in
S3 (`s3`). This removes the single-writer file lock that made many agents queue
behind one tenant database.

- Writes go into an in-memory hot table and are flushed to Parquet every
  `--flush-interval` (default `10s`). A kill before a flush can lose at most
  that window; the value is the deployment's RPO. `lidlesseye ingest --data-dir`
  flushes before it exits, so a CLI import is durable when the command returns.
  To shorten the window on a running server, call
  `lidlesseye flush --server http://lidlesseye:4040 --tenant <id>` (or
  `POST /api/v1/flush`); opening the data directory from another process would
  only create a second, empty hot tier.
- Flushes are partitioned by the **data's** hour, so backfills and delayed
  reports are retained by their real timestamps. Completed hours are merged
  into one deterministic final file; the read side ignores superseded fragments
  even if a crash happened between publishing the final and deleting inputs.
- `--retention 7d` deletes whole hours whose end is older than the cutoff.
  `--max-disk 40GB` is a hard cap: oldest committed Parquet files are removed
  first, which can discard data earlier than the retention window.
- Compaction and S3 upload use a second in-memory DuckDB connection. A long
  merge or upload no longer holds the connection that serves queries/ingest.
- The PVC is still required in both modes: `disk` keeps its archive there and
  `s3` keeps unconfirmed staging files. Never delete local staging that has not
  been verified in the bucket.
- The chart keeps `replicaCount: 1` and `Recreate`: a single writer avoids two
  servers racing on the same tenant's Parquet files during a rollout.

Upgrade from releases that used `lidlesseye.duckdb` is automatic: on first open
the server exports every table into hour-partitioned Parquet files, verifies the
row counts, and renames the old file to `lidlesseye.duckdb.migrated-…` instead of
deleting it. Legacy mode values (`duckdb`, `parquet`) are accepted with a
warning and mapped to `disk` / `s3` so old Helm values do not start an empty
store. Keep the old PVC/backup until the first full retention cycle has passed.

### Canary and rollback

The storage upgrade is one-way for a data volume: once the legacy DuckDB file
has been exported, an older server cannot read the new Parquet-only layout.
Rollback therefore means restoring the pre-upgrade PVC snapshot, not just
redeploying the old image.

1. Snapshot the PVC (or take a DuckDB-consistent backup with the server
   stopped) and record `GET /api/v1/stats` for each tenant before the upgrade.
2. Canary on a copied volume first. Start the new image with the chart's
   `replicaCount: 1` / `Recreate` defaults, then watch the first-open logs:
   `exporting legacy DuckDB`, `verified … rows`, `store ready`.
3. Confirm the totals and flamegraph for the same windows still match, and that
   `tenants/default/lidlesseye.duckdb.migrated-…` plus
   `tenants/default/parquet/<table>/h…-legacy.parquet` exist. A second restart
   must not create another migrated file.
4. Check `/api/v1/stats` → `locks` for a steady `holder=idle` and
   `/metrics` for `lidlesseye_parquet_files_pruned_total`. Trigger an immediate
   flush with `lidlesseye flush --server http://lidlesseye:4040` if the RPO must
   be shortened before traffic is shifted.
5. Roll back by stopping the new server, restoring the pre-upgrade snapshot and
   redeploying the old image. Do not point the old image at a migrated volume.

```bash
# disk mode (default): local Parquet only
lidlesseye serve --storage-mode=disk \
  --retention=7d --max-disk=50GB --flush-interval=10s
```

### S3 archive mode

For longer-lived datasets, use `s3`: local fragments are merged per hour,
uploaded to S3, and only removed after the object has been read back. The
object index is rebuilt from deterministic keys at startup; no JSON manifest
or DuckDB database is persisted.

```bash
lidlesseye serve --storage-mode=s3 \
  --s3-bucket=profiles --s3-prefix=lidlesseye \
  --s3-endpoint=s3.example.com:9000 --s3-use-ssl=false \
  --s3-url-style=path --parquet-local-ttl=1h
# credentials: LIDLESSEYE_S3_ACCESS_KEY / LIDLESSEYE_S3_SECRET_KEY
```

```bash
helm install lidlesseye deploy/helm/lidlesseye \
  --namespace observability --create-namespace \
  --set server.storage.mode=s3 \
  --set server.storage.s3.bucket=profiles \
  --set server.storage.s3.endpoint=s3.example.com:9000 \
  --set server.storage.s3.useSSL=false \
  --set server.storage.s3.existingSecret=lidlesseye-s3
```

Relevant values under `server.storage`:

| Value | Default | Meaning |
| --- | --- | --- |
| `mode` | `disk` | `disk` (local Parquet) or `s3` (verified S3 archive) |
| `flushInterval` | `10s` | hot-tier flush cadence / maximum RPO |
| `localTTL` | `1h` | local fragment age before merge/upload |
| `s3.bucket` | `""` | bucket name (required in `s3` mode) |
| `s3.prefix` | `lidlesseye` | key prefix; the tenant id is appended per tenant |
| `s3.endpoint` | `""` | `host:port`; empty uses the AWS default endpoint |
| `s3.region` / `s3.useSSL` / `s3.urlStyle` | `us-east-1` / `true` / `path` | httpfs connection settings |
| `s3.existingSecret` | `""` | pre-created secret with `access-key` / `secret-key` |

DuckDB's httpfs writes every S3 object with the multipart upload API; there is
no single-PUT setting. An S3-compatible endpoint must implement
`CreateMultipartUpload`, `UploadPart` and `CompleteMultipartUpload`, otherwise
the archive probe fails with `501 Not Implemented` and archival stays paused
while local staging grows. The server keeps the only local copy in that case,
so no data is lost, but the staging area will eventually apply backpressure.
Test doubles must provide those three operations; a PUT-only mock is not
enough.

Object lifecycle/retention for the bucket must be configured outside the chart.
Budget for the storage class, requests and the longest unconfirmed staging
backlog you can tolerate; the server applies backpressure instead of deleting
the only copy when S3 is unavailable.

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

### Where the image comes from

The chart points at **Docker Hub** by default — `docker.io/xsj321/lidlesseye` —
and leaves `image.tag` empty, which the template resolves to `appVersion`. A
released chart therefore installs the image built from that same version, not
whatever tag the last person happened to set.

```bash
docker pull docker.io/xsj321/lidlesseye:0.1.3
# the same manifest, copied by digest rather than rebuilt:
docker pull ghcr.io/xsj321/lidlesseye:0.1.3
```

Both names come out of one build: the release workflow pushes the image to GHCR
and copies the manifest to Docker Hub, so the digests match and a pin in a values
file means the same bytes whichever registry a cluster can reach. For a
maintainer, Docker Hub publishing needs `DOCKERHUB_USERNAME` and
`DOCKERHUB_TOKEN` as repository secrets; without them the workflow warns and
publishes to GHCR only.

The tag the chart asks for is the version **without** the `v` (`0.1.3`, not
`v0.1.3`), because that is what `appVersion` holds. Publishing is only half of a
release: after a tag push, check that tag exists in both registries, since a
missing one is an `ImagePullBackOff` on a fresh install rather than a warning
anywhere:

```bash
docker buildx imagetools inspect docker.io/xsj321/lidlesseye:0.1.3
docker buildx imagetools inspect ghcr.io/xsj321/lidlesseye:0.1.3
```

Point a cluster at the mirror, or at a private copy, with two values:

```bash
helm install lidlesseye deploy/helm/lidlesseye \
  --set image.repository=ghcr.io/xsj321/lidlesseye \
  --set image.tag=0.1.3
```

### Local clusters (Docker Desktop, kind, minikube)

There is no registry to push to, so build the image locally and give every
build a **distinct tag**. A cluster caches the image by tag at pod creation, and
`imagePullPolicy: IfNotPresent` will happily keep running the previous build:

```bash
docker build -f deploy/docker/Dockerfile -t docker.io/xsj321/lidlesseye:0.1.3-dev1 .
helm install lidlesseye deploy/helm/lidlesseye -n lidlesseye --create-namespace \
  --set image.tag=0.1.3-dev1 \
  --set server.persistence.size=5Gi
```

Reach the dashboard with `kubectl -n lidlesseye port-forward svc/lidlesseye 4040:4040`.
`NodePort` and `LoadBalancer` services are not reliably exposed on the host by
these distributions, so port-forward is the dependable option.

### Clusters that cannot pull from a registry

An air-gapped cluster, a node behind a proxy that only allows `GET`, or a tag
whose push was rejected all end the same way: `ImagePullBackOff`. The image has
to be moved as a file. `scripts/push-image.sh` is the check that catches this
before the cluster does — it verifies the tag with a manifest lookup after
pushing, because `docker push` exits 0 on some transport failures and a progress
line is not a verdict:

```bash
scripts/push-image.sh xsj321/lidlesseye:0.1.3 --save /tmp/lidlesseye.tar
```

Then, on the node (or on a machine that can reach the nodes):

```bash
# containerd, the runtime of every distribution since 1.24
ctr -n k8s.io images import /tmp/lidlesseye.tar

# CRI-based node, no shell into containerd's namespace
crictl pull docker.io/xsj321/lidlesseye:0.1.3   # only if a registry is reachable
crictl images | grep lidlesseye

# kind: the tar goes through the host daemon, kind copies it into every node
kind load image-archive /tmp/lidlesseye.tar --name my-cluster

# k3s: its own containerd namespace and its own importer
k3s ctr images import /tmp/lidlesseye.tar

# Docker Desktop shares its image store with the built-in cluster,
# so `docker load` is enough there
docker load -i /tmp/lidlesseye.tar
```

`ctr images import` and `kind load` keep the tag from the tar, so the chart's
`image.tag` has to match the tag inside it — check with
`ctr -n k8s.io images ls | grep lidlesseye` and pass the same value to Helm. A
namespace mismatch is the usual reason an import "works" and the pod still
fails: `ctr -n k8s.io` is what kubelet reads, `ctr -n default` is what a bare
`ctr images import` writes.

One trap specific to publishing from Docker Desktop: the CLI and the daemon have
**separate** proxy settings. `docker manifest inspect`, `docker login` and
`docker search` run client-side and use `https_proxy`, while `docker push`,
`docker pull` and kubelet use the daemon's own proxy. On a network where only
one of the two reaches the registry, a push can succeed while `manifest inspect`
insists the tag does not exist. `scripts/push-image.sh` therefore verifies with
`docker pull`, which goes through the daemon — the same path a node takes.

Pin the image in the cluster by digest when the registry is unreliable
(`--set image.digest=sha256:...` is not supported yet, so the tag is the handle):
`docker images --digests` on the importing machine and the pod's
`status.containerStatuses[].imageID` both print it, and comparing the two is the
quickest way to prove the node is running the build you think it is.

For a private registry, the chart already supports both halves:

```bash
helm upgrade lidlesseye deploy/helm/lidlesseye -n lidlesseye --reuse-values \
  --set image.repository=harbor.example.com/observability/lidlesseye \
  --set image.tag=0.1.3 \
  --set-json 'imagePullSecrets=[{"name":"harbor-creds"}]'
```

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
| `--no-alloc` | allocation profiling on | the allocator uprobes; disable where uprobes are not permitted |
| `--no-heap` | Go heap reader on | reads `runtime.mbuckets` through `/proc/<pid>/mem`; needs no probe |
| `--heap-objects` | off | also report `alloc_objects` and `inuse_objects`; doubles the series count per stack |
| `--service-name` | process name | set it to aggregate a node under one service; the process name stays as a `process_name` label |
| `--max-disk` | 50GB | soft cap on retained data; oldest batches are dropped first |
| `--duckdb-threads` | all cores | threads DuckDB may use per query. All tenants share this process-wide setting |
| `--query-concurrency` | 4 | how many queries may read the store at once. Each one holds its tenant's DuckDB connection while it runs, so this is a queue depth, not a CPU count. `0` removes the cap |
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

**`failed to load eBPF: … missing target BTF info for type …`**
The kernel has no BTF for the loader to resolve the CO-RE relocations against,
so it cannot find where `task_struct.thread_pid` lives on this kernel. Check
`ls /sys/kernel/btf/vmlinux` (kernel ≥ 5.5, `CONFIG_DEBUG_INFO_BTF=y`); a
distribution kernel may ship it as a separate `kernel-debuginfo` package.

**`failed to load eBPF: the verifier rejected the program`**
The kernel is too old or the object was built for a newer one. Rebuild with
`LIDLESSEYE_BUILD_BPF=1`.

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

**The first dashboard request after an upgrade times out**
The server logs now show each startup stage with an `elapsed_ms` value:
`opening tenant stores`, `opening DuckDB file`, `DuckDB schema is current;
skipping migration` (or a `stage=tables|columns|indexes` line when a migration
really is needed), `store ready`, and `all tenant stores opened`. If a stats
scan takes more than a second, it is also logged. A database already on the
current schema no longer rebuilds its indexes or rewrites `meta` on every open,
and the tenant catalogue computes stats and the distinct-series count in one
scan instead of two.

**`dropping reports after repeated upload failures` during a rollout**
The agent retries transport errors and 408/429/5xx for up to five minutes,
with backoff capped at 10 s. A rolling upgrade can keep the server endpoint
away longer than the old ~12 s budget: the old pod terminates, the new one
opens every tenant store before listening, and only then does the Service get
an endpoint. If the warning still appears, check the server pod's startup
progress lines above; a genuinely permanent 4xx is dropped immediately instead
of being retried.

**`open_files` / memory grows forever**
With DuckDB there is one file per tenant, never one per report, so service-name
cardinality cannot create files — but it does create distinct label strings in
`samples`, which cost dictionary space. Set `--service-name` to aggregate, or
lower `--retention`. If the resident set grows with the number of distinct
frames, that is the `frames` dictionary plus the in-memory frame cache;
`--duckdb-threads` bounds DuckDB's own worker memory, and
`--query-concurrency` bounds how many of those workers exist at once. Spill
files under `<data dir>/tenants/<tenant>/.duckdb-tmp` are deleted when the
process opens the store, so that directory is only large while a wide scan is
running — a persistent multi-gigabyte `.duckdb-tmp` means the scans are wider
than `--duckdb-memory-limit` allows, not that cleanup is broken.

**Requests return `408` or `503`, and the pod keeps restarting**
Both mean the server was asked for more scans than it could run at once.
`503 server is busy` is the honest one: every read slot was occupied for longer
than 15 s, so the request was refused instead of queued indefinitely.
`408` is the 60 s request timeout firing, which is the last resort.

The failure this replaces was worse than a slow response: query endpoints used
to run their DuckDB call inside the async handler, so four concurrent panel
refreshes could occupy the whole runtime, `/api/v1/health` would not be
answered, and kubelet would restart the container (`exitCode 137`) while the
browser saw 408s. If you are on a release that predates the fix, upgrading is
the fix.

To make the work cheaper rather than more parallel: narrow the dashboard's
range, lower `--retention` (fewer files to open per query), and check that
`--duckdb-threads` is not set to `0`/unset on a many-core host. Each of the
`parquet` store's queries opens every archived object's footer, so the number
of *files*, not the number of rows, is what the per-query cost tracks.

**Locating a slow request (store lock vs. query vs. I/O)**

Every tenant store publishes lock telemetry. `/api/v1/stats` returns a `locks`
object with the current owner (`idle`, `read`, `write`, `flush`, `compact`,
`archive`, `prune`, `maintenance`), how long it has held the connection, the
cumulative/total/max wait times and the count of waits over 100 ms. The server
also logs:

- `stage=read_slot_wait` — time spent in the async read semaphore (15 s cap → 503);
- `stage=store_lock_wait` — time spent waiting for the tenant's DuckDB connection,
  including `requester`, `holder` and `holder_held_ms`;
- `stage=query_exec` — blocking query duration over one second;
- `stage=store_lock_hold` (debug) — release of a labelled owner.

A chain of `requester=read holder=write` lines during ingest points at write
serialization; `holder=compact|archive|flush` points at background work on the
same connection; `read_slot_wait` without `store_lock_wait` means the query
concurrency cap, not the store, is the bottleneck.

**`database is locked` / `Could not set lock on file …`**
Another process has the tenant database open. DuckDB allows one writer per
file: stop the second `serve` instance (or the CLI `query`/`fsck` running
against the same data dir) and retry. Two replicas must each have their own
volume.
