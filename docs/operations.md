# Operating LidlessEye

Everything in this file was exercised on a live cluster (`docker-desktop`, namespace
`disk`) at the version named next to it. When a procedure has a failure mode that
cost us time, it is written down here rather than left for the next person.

## 1. What runs where

| Component | Kind | State it owns |
|---|---|---|
| `le-disk-lidlesseye` | Deployment, 2 replicas | One of them holds the write lease; both serve reads |
| `le-disk-reader` | Deployment, 1 replica | `--read-only`: reads only, never takes the lease |
| `le-disk-lidlesseye-agent` | DaemonSet | eBPF sampling, reader injection, spool on disk |
| `le-disk-lidlesseye` Service | ClusterIP | Write traffic may go to either replica (307 redirects) |
| `le-disk-lidlesseye-read` Service | ClusterIP | Read-only replica only |

One PersistentVolumeClaim holds the WAL segments and the Parquet files. Both
replicas mount it, which is what makes the lease file (`<data-dir>/active.lease`)
meaningful: they can see each other's claim.

## 2. Install

```bash
helm install lidlesseye deploy/helm/lidlesseye \
  --namespace lidlesseye --create-namespace \
  --set server.persistence.size=50Gi \
  --set server.retention=7d
```

The chart's `appVersion` sets the image tag. A locally built image needs both the
tag and a pull policy that does not go looking for it upstream:

```bash
helm upgrade lidlesseye deploy/helm/lidlesseye -n lidlesseye \
  --set image.tag=0.1.157-perf --set image.pullPolicy=Never
```

## 3. Upgrade

The rule that matters: **every build gets a new tag**. Re-tagging an existing tag
does not update the node: containerd keeps the digest it already pulled, and the
pod comes up reporting the new tag while running the old binary. Verify with

```bash
docker run --rm --entrypoint sh xsj321/lidlesseye:<tag> \
  -c "grep -c '<a string only the new build has>' /usr/local/bin/lidlesseye"
kubectl -n disk exec deploy/le-disk-reader -- md5sum /usr/local/bin/lidlesseye
```

Sequence that worked repeatedly:

```bash
# 1. write the patch to a file -- PowerShell mangles inline JSON
'[{"op":"replace","path":"/spec/template/spec/containers/0/image","value":"docker.io/xsj321/lidlesseye:0.1.157-perf"}]' \
  | Set-Content target\tmp\p157.json -Encoding ascii -NoNewline

kubectl -n disk patch deploy le-disk-lidlesseye --type=json --patch-file=target\tmp\p157.json
kubectl -n disk set image deploy/le-disk-reader server=docker.io/xsj321/lidlesseye:0.1.157-perf

# 2. the agent injects the *reader* image into target pods; keep it in step
kubectl -n disk set env ds/le-disk-lidlesseye-agent \
  LIDLESSEYE_SIDECAR_IMAGE=docker.io/xsj321/lidlesseye:0.1.157-perf

kubectl -n disk rollout status deploy/le-disk-lidlesseye --timeout=420s
kubectl -n disk rollout status deploy/le-disk-reader --timeout=420s
kubectl -n disk rollout status ds/le-disk-lidlesseye-agent --timeout=300s

# 3. only now prune images
docker image prune -f
```

Two traps:

* **The container is named `server`, not `lidlesseye`.** A strategic patch that
  names a container that does not exist *adds* one; the pod then sits at `0/2`
  and the API server rejects the update with `containers[0].image: Required value`.
  Use a JSON patch that replaces index 0.
* **Do not `docker image prune` before the new image is Running.** A prune once
  deleted an image that had been built but not yet deployed and took the service
  down for 25 minutes.

Rollback is the same patch with the previous tag. Nothing in the PVC is migrated
by a rollout, so a rollback is only safe one version back; older binaries may not
understand newer Parquet columns.

## 4. Backup and restore

What is irreplaceable, in order:

1. `<data-dir>/tenants/<t>/tenants/<t>/parquet/` — every committed sample.
2. `<data-dir>/tenants/<t>/…/wal/` — the segments not yet converted. Losing these
   costs up to one conversion cycle.
3. `<data-dir>/directives.json`, `profiling-directives.json` — which workloads a
   human switched a reader on for. Small, and tedious to reconstruct.

`active.lease` and `cache/` are not worth backing up.

**Backup**

```bash
# The server writes 10 s of memory and flushes to Parquet; nothing needs to stop.
kubectl -n disk exec deploy/le-disk-lidlesseye -- \
  tar -C /var/lib/lidlesseye -czf - tenants default-tenant-id > backup.tgz
```

In `--storage-mode s3` the Parquet files are already remote and only the WAL
window is local, so backing up the bucket is enough.

**Restore**

```bash
kubectl -n disk scale deploy/le-disk-lidlesseye --replicas=0
kubectl -n disk exec deploy/le-disk-reader -- rm -rf /var/lib/lidlesseye/tenants   # reader mounts the same PVC
kubectl -n disk cp backup.tgz <a-pod-that-mounts-the-PVC>:/var/lib/lidlesseye/
# inside that pod:
tar -C /var/lib/lidlesseye -xzf /var/lib/lidlesseye/backup.tgz
kubectl -n disk scale deploy/le-disk-lidlesseye --replicas=2
```

Then prove the restore instead of assuming it:

```bash
curl -s -X POST http://127.0.0.1:4983/api/v1/fsck
# {"elapsed_ms":1,"tenants":[{"files":128,"hours":54,"bytes":9010313,
#   "unreadable":[],"vanished":[],"empty_hours":[]}]}
```

`unreadable` means a file is truncated or not Parquet (`PAR1` footer missing);
`vanished` means a file the previous run saw is gone. Both are exactly the cases
where queries keep answering while the database quietly shrinks.

## 5. Health and self-monitoring

| Signal | Where | Meaning |
|---|---|---|
| `lidlesseye_replica_active` | `/metrics` | 1 on the writer, 0 on the standby |
| `lidlesseye_lease_fence` | `/metrics` | Increases on every takeover; two different values in logs means a real handover |
| `lidlesseye_cache_hits_total` / `misses` | `/metrics` | A dashboard should sit well above 1:1 |
| `lidlesseye_fsck_findings` | `/metrics` | Non-zero means the storage layer disagrees with itself |
| `lidlesseye_process_rss_bytes` | `/metrics` | The server's own footprint, absent from every other view |
| `x-lidlesseye-cache: hit` | response header | Whether the 10 s answer cache served this request |
| `GET /api/v1/role` | API | `{holder, active, fence, lease_file}` |
| `GET /api/v1/capabilities` | API | Per reader: state, pods, attached processes |

Expected RPO/RTO with two replicas: RPO is one flush interval (10 s, the WAL on
the shared volume replays it); RTO is `TTL + TICK ≈ 10–13 s`.

## 6. Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Writes to the Service answer `503 this replica is a standby` | Standby has no `LIDLESSEYE_POD_IP`, so it cannot build a redirect | Set the downward-API env; it is `status.podIP` |
| Writes answer `307` in a loop | A `--read-only` replica took the lease (only possible before 0.1.150) | Upgrade; read-only replicas no longer take leases |
| Writes answer `403 this replica is read-only` | You pointed write traffic at `le-disk-lidlesseye-read` | Point it at `le-disk-lidlesseye` |
| A workload is switched on but `capabilities` is empty | The reader is not running: no pod matched, or the shim is dead | `kubectl logs ds/le-disk-lidlesseye-agent` — look for `no running pod` (fixed in 0.1.157: the agent now asks the API when the sample-driven index has no entry) or `allocator shim is dead` (replace the pod; ephemeral containers never restart) |
| Two replicas return different `total` for the same query | One is reading a stale local Parquet index | Since 0.1.152 every non-writer rescans every 5 s; a 12 s visibility lag is expected, a permanent difference is not |
| Queries are 10–100× slower than `docs/bench` numbers | The kernel-uprobe allocator reader is on for that workload | The reader costs ~230 ns per malloc+free; switch it off for throughput-sensitive workloads |
| Dashboard alternating between `hit` and `miss` | TTL is 10 s and panels ask every 15 s | Expected; raise `RUST_LOG` noise, not the TTL, unless panels really repeat within 10 s |
| Spool files accumulating on a node | Server unreachable | They drain automatically; 720 files are kept, oldest dropped first |
| `no memory profile in this window` for a Go service | The reader collects `alloc_space` only on allocation; an idle process allocates nothing | Look at a wider window, or at `alloc_space_total` once Sprint 10's split lands |

## 7. Capacity

* Retention is by whole hour files, so the PVC must hold the high-water mark of
  `retention`, not `maxDisk` alone.
* One hour of a moderately busy cluster produced 128 files / 9 MB in testing;
  a production node is dominated by `samples`, not `frames`.
* `queryConcurrency` (default 4) is the number of simultaneous readers. It caps
  queue depth, not CPU; a wide flamegraph is one reader regardless of CPU count.
