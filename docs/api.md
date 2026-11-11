# HTTP API

Base URL: whatever `--listen` is set to (default `http://0.0.0.0:4040`).

All endpoints return JSON unless noted. Errors are JSON with a single `error`
field and an appropriate status:

```json
{ "error": "invalid selector: unterminated braces" }
```

When the server runs with `--auth-token` (or `LIDLESSEYE_AUTH_TOKEN`), every
endpoint except `/api/v1/health`, `/api/v1/ready`, `/api/v1/version` and
`/metrics` requires:

```
Authorization: Bearer <token>
```

## Time ranges

`from` and `until` accept any of:

| Form | Example |
| --- | --- |
| Unix seconds | `1700000000` |
| Unix with unit | `1700000000000ms`, `1700000000000000us`, `1700000000000000000ns` |
| RFC 3339 | `2024-11-14T22:13:20Z` |
| Relative | `now`, `now-15m`, `now-2h`, `now-7d` |

`until` defaults to `now`, `from` to one hour before `until`. Numbers without a
unit are interpreted by magnitude, so all three of `1700000000`,
`1700000000000` and `1700000000000000000` mean the same instant.

## Selectors

`selector=` takes a PromQL-flavoured label selector:

```
service_name="checkout"
{service_name="checkout", namespace="shop", pod=~"checkout-*"}
```

Operators: `=` (equal), `!=` (not equal), `=~` / `!~` (**glob** match with `*`
and `?`). `service_name` is matched against the report's service name, every
other label against the segment's label map.

Repeated `match[]=` parameters are ANDed, and `service_name=` /
`label=`+`label_value=` are accepted as shorthands.

## `POST /ingest`

Accepts one report or a batch:

```json
{ "reports": [ … ] }
```

Query parameters can override fields, which makes ad-hoc ingestion practical:
`service_name` (alias `name`), `profile_type` (alias `type`), `from`, `until`.

### Report schema

```json
{
  "service_name": "checkout",
  "labels": { "env": "prod", "pod": "checkout-7d9f-abcde" },
  "profile_type": "cpu",
  "start_ns": 1700000000000000000,
  "end_ns": 1700000010000000000,
  "sample_rate_hz": 99,
  "strings": ["/usr/bin/checkout", "handle_request", "main", ""],
  "stacks": [
    { "frames": [[0, 1, 3, 0], [0, 2, 3, 0]], "value": 42000000 }
  ]
}
```

- `frames` are `[module, symbol, file, line]` indices into `strings`.
- Frames are ordered **leaf first**.
- `value` is in the unit implied by the profile type (`cpu` / `off_cpu` /
  `wall` → nanoseconds, `alloc_space` / `inuse_space` → bytes, everything else →
  counts).
- `wall` is virtual and is rejected on ingest.
- `end_ns` must be greater than `start_ns`.

Response:

```json
{ "accepted": 1, "segments": 1, "samples": 2, "bytes": 812, "files": ["1700000000000000000-1700000010000000000.leseg"] }
```

```bash
curl -X POST http://localhost:4040/ingest \
  -H 'content-type: application/json' \
  --data @report.json
```

## `GET /api/v1/flamegraph`

| Parameter | Default | Meaning |
| --- | --- | --- |
| `profile_type` / `type` | `cpu` | profile type, or `wall` |
| `from`, `until` | last hour | time range |
| `selector` | — | label selector |
| `max_nodes` | `1024` | cap on tree size (clamped to 50 000) |
| `format` | `flat` | `tree` returns nested `{name, value, children}` |

Flat response:

```json
{
  "names": ["root", "main", "handle_request", "render_html"],
  "levels": [-1, 0, 1, 2],
  "values": [2850000000, 2850000000, 2850000000, 1700000000],
  "total": 2850000000,
  "unit": "nanoseconds",
  "samples": 6,
  "truncated": false
}
```

`levels[i]` is the **parent index** of node `i` (`-1` for the root), and
`values[i]` is the inclusive value of the node. Children are emitted before
their siblings, sorted by value.

```bash
curl -s 'http://localhost:4040/api/v1/flamegraph?profile_type=wall&from=now-15m' \
  | jq '{total, unit, frames: (.names | length)}'
```

## `GET /api/v1/top`

| Parameter | Default | Meaning |
| --- | --- | --- |
| `limit` | `50` | rows, clamped to 1 000 |

```json
{
  "unit": "nanoseconds",
  "total_value": 2850000000,
  "items": [
    {
      "name": "render_html",
      "module": "/app/server",
      "file": "/app/server/src/render.rs",
      "line": 42,
      "self_value": 1700000000,
      "total_value": 1700000000
    }
  ]
}
```

`self_value` counts stacks where the function is the leaf; `total_value` counts
every stack containing it. `total_value` at the top level is the denominator for
"share of total".

## `GET /api/v1/timeseries`

| Parameter | Default | Meaning |
| --- | --- | --- |
| `buckets` | `60` | number of buckets (clamped to 1 000) |
| `step` | — | explicit bucket width (`10s`, `1m`); overrides `buckets` |

```json
{
  "from_ns": 1700000000000000000,
  "until_ns": 1700003600000000000,
  "step_ns": 60000000000,
  "series": [
    {
      "service_name": "checkout",
      "labels": { "env": "prod" },
      "points": [0, 120000000, 340000000]
    }
  ]
}
```

A segment contributes proportionally to every bucket its window overlaps, so a
10-second segment spread over a 60-second bucket does not spike.

## `GET /api/v1/series`

Known `(service, profile_type, labels)` combinations in the range:

```json
{
  "series": [
    {
      "service_name": "checkout",
      "profile_type": "cpu",
      "labels": { "env": "prod" },
      "total_value": 2850000000,
      "segment_count": 42,
      "start_ns": 1700000000000000000,
      "end_ns": 1700003600000000000
    }
  ]
}
```

## `GET /api/v1/label/{name}/values`

```json
{ "label": "env", "values": [ { "value": "prod", "total": 123456 } ] }
```

## `GET /api/v1/profile_types`

```json
{
  "profile_types": [
    { "name": "cpu", "unit": "nanoseconds", "total_value": 527638125, "segment_count": 34 },
    { "name": "off_cpu", "unit": "nanoseconds", "total_value": 171054958510, "segment_count": 68 },
    { "name": "wall", "unit": "nanoseconds", "total_value": 171582596635, "segment_count": 102, "virtual": true }
  ]
}
```

`wall` is offered whenever `cpu` or `off_cpu` data exists, and is flagged
`"virtual": true`.

## Operations

| Endpoint | Response |
| --- | --- |
| `GET /api/v1/health` | `{"status":"ok","version":"0.1.0","uptime_seconds":42}`, always 200 |
| `GET /api/v1/ready` | `204` when the store holds no segments yet, `200` otherwise — both mean "ready to serve" |
| `GET /api/v1/stats` | store counters (segments, bytes, value, earliest/latest), series count, ingest counters |
| `GET /api/v1/version` | `{"name":"lidlesseye","version":"0.1.0"}` |
| `GET /metrics` | Prometheus text format |

### Metrics

```
lidlesseye_ingest_requests_total
lidlesseye_ingest_reports_total
lidlesseye_ingest_samples_total
lidlesseye_ingest_bytes_total
lidlesseye_ingest_errors_total
lidlesseye_queries_total
lidlesseye_query_duration_seconds_total
lidlesseye_query_errors_total
lidlesseye_segments_written_total
lidlesseye_segments_pruned_total
lidlesseye_uptime_seconds
lidlesseye_segments            (gauge)
lidlesseye_storage_bytes       (gauge)
lidlesseye_stored_value        (gauge)
```

## Compression and limits

Request and response bodies are gzip-compressed transparently when the client
asks for it (`Accept-Encoding: gzip`) and when the client sends
`Content-Encoding: gzip`. An agent pushing a lot of JSON per flush should use
it; `curl --compressed` and any well-behaved HTTP client do it for free.

Request bodies are capped at 64 MiB (`ServerConfig::max_body_bytes` when
embedding) and every request has a 60-second timeout.
