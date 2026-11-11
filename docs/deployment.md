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
(default 10s), with exponential backoff on failure. Nothing is spooled to disk:
if the server is unreachable the batch is retried, then dropped, so a long
outage loses data rather than filling the node's disk.

Labels worth setting: `env`, `region`, `cluster`, `node`. Kubernetes labels
(`namespace`, `pod`, `container`) are derived automatically from each process's
cgroup when the agent runs with `hostPID`.

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
and `lidlesseye fsck --fix` re-verifies every segment's CRC32.

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

### Privileges the agent needs

The DaemonSet is privileged on purpose: `CAP_BPF`/`CAP_PERFMON` to load
programs, `CAP_SYS_ADMIN` for `BPF_MAP_TYPE_STACK_TRACE` and
`BPF_F_USER_STACK`, and `hostPID` so `/proc/<pid>/maps` actually describes the
processes being sampled. A namespace with the `restricted` Pod Security
Standard label will reject the DaemonSet; label the namespace
`pod-security.kubernetes.io/enforce=privileged` or install the agent separately.

The server needs none of this — run it unprivileged, on a node pool with a real
disk.

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

## Choosing parameters

| Flag | Default | Notes |
| --- | --- | --- |
| `--frequency` | 99 | samples per second per CPU. 99 is plenty for flamegraphs; 999 gives finer detail at ~10x the samples and proportional storage |
| `--flush` | 10s | how often the kernel maps are drained. Shorter = fresher data, more segments |
| `--no-cpu` / `--no-offcpu` | both on | off-CPU needs kprobes; disable it on locked-down kernels |
| `--service-name` | process name | set it to aggregate a node under one service; the process name stays as a `process_name` label |
| `--max-disk` | 50GB | soft cap; oldest segments are dropped first |
| `--read-only` | off | rejects ingest, useful for a query-only replica of a data directory |

## Monitoring

Scrape `/metrics`. The two metrics that matter operationally:

- `lidlesseye_ingest_errors_total` — agents failing to push (check auth, disk
  full, body limit)
- `lidlesseye_query_duration_seconds_total` / `lidlesseye_queries_total` — mean
  query latency; a rising mean means the segment count per query is growing
  (raise retention trimming or narrow the dashboard's default range)

`lidlesseye_storage_bytes` vs your volume size is the other one to alert on.

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
Each segment is a file; a pathological cardinality in service names (one
service per short-lived process) creates many small segments. Set
`--service-name` to aggregate, or lower `--retention`.
