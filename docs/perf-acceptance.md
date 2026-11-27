# Performance acceptance (0.1.157-perf)

What this file is: the numbers we will hold ourselves to, the environment they
were taken in, and the method, so a future regression can be called a
regression rather than "it felt slower".

## Environment

| Item | Value |
|---|---|
| Cluster | Docker Desktop single-node (`docker-desktop`), master + one worker |
| Kernel | `6.18.33.1-microsoft-standard-WSL2` |
| Deployment | `le-disk-lidlesseye` 2 replicas (one writes), `le-disk-reader` 1 (read-only) |
| Dataset | 128 Parquet files / 54 hours / 9.0 MB, ~1 500 samples/hour |
| Query path | `kubectl port-forward` → Service → either replica; times include the forward |
| Client | Windows PowerShell 7 + curl 8, same machine as the cluster |
| Version | `docker.io/xsj321/lidlesseye:0.1.157-perf` |

Numbers are `curl -w "%{time_total}"`, 3 warm-up calls discarded, 20 measured
calls per endpoint, `?fresh=1` so the 10 s answer cache is bypassed. They are
therefore *cold* query latencies including one process spawn and one forward hop,
i.e. a pessimistic view of what the browser sees once the cache is warm.

## Results

| Endpoint | p50 | p95 | p99 | max |
|---|---|---|---|---|
| `flamegraph?type=cpu&range=1h` | 15.7 ms | 19.8 ms | 32.9 ms | 32.9 ms |
| `flamegraph?type=cpu&range=6h` | 16.8 ms | 24.7 ms | 27.8 ms | 27.8 ms |
| `flamegraph?type=cpu&range=all` | 15.0 ms | 18.7 ms | 19.2 ms | 19.2 ms |
| `top?type=cpu&range=1h` | 14.8 ms | 27.4 ms | 36.5 ms | 36.5 ms |
| `servicemap?range=1h` | 19.4 ms | 23.4 ms | 24.2 ms | 24.2 ms |
| `timeseries?type=cpu&range=1h` | 9.9 ms | 13.0 ms | 20.0 ms | 20.0 ms |
| `memory/coverage` | 9.9 ms | 24.2 ms | 27.4 ms | 27.4 ms |
| `capabilities` | 3.7 ms | 26.2 ms | 27.8 ms | 27.8 ms |

Two things worth reading out of this table:

1. **Window length barely matters.** 1 h, 6 h and *all* are within 2 ms of each
   other at p50. Parquet is partitioned by `hour=`, so a query opens only the
   hours it needs, and an hour is currently 2–3 files. This is why rollup was
   deferred (Sprint 10): there is nothing to buy.
2. **p99 is 2× p50 on the small endpoints.** `capabilities` p50 is 3.7 ms but
   p99 is 27.8 ms; those endpoints do no query work at all, so the spread is the
   client and the forward, not the server.

## Invariants asserted alongside the timings

| Invariant | Evidence |
|---|---|
| `flamegraph.total == top.total_value` | 7 == 7 on a synthetic tenant (`X-Scope-OrgID: t1`, one folded batch of 7 µs) |
| Tenant isolation | the same batch is invisible to `t2`: `flamegraph` 0, `/series` empty |
| Invalid tenant ids rejected | `X-Scope-OrgID: ../../etc` → 400, `a b` → 400 |
| Cache does not change answers | `fresh=1` and cached responses agreed to the last digit on all endpoints |
| Standby redirects instead of failing | write to the Service answered `307` to the writer, then `202` |

## Regression red lines

A change that breaks one of these needs a written reason, not a footnote:

* `flamegraph` p50 > 30 ms at any window;
* `servicemap` p50 > 60 ms;
* `flamegraph.total != top.total_value` on the same window;
* any endpoint 5xx under a single-worker load test;
* the standby answering writes with anything but `307` (or `403` when it is the
  read-only deployment).

## Reproducing

```powershell
$eps = "flamegraph?type=cpu&range=1h","flamegraph?type=cpu&range=all",
       "top?type=cpu&range=1h","servicemap?range=1h",
       "timeseries?type=cpu&range=1h","memory/coverage","capabilities"
foreach ($e in $eps) {
  1..3 | ForEach-Object { curl.exe -s -o NUL "http://127.0.0.1:PORT/api/v1/$e&fresh=1" }
  $t = 1..20 | ForEach-Object { [double](curl.exe -s -o NUL -w "%{time_total}" "http://127.0.0.1:PORT/api/v1/$e&fresh=1") }
  # p50/p95/p99 from $t after sorting
}
```

Note the port: a rollout replaces the pod a `port-forward` is bound to, so the
forward must be re-created after every deploy.
