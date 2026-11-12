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

Status codes used throughout:

| Code | Meaning |
| --- | --- |
| `400` | malformed request: bad tenant id, unparsable time, invalid selector, invalid report, bad payload |
| `401` | authentication is configured and the token is missing or wrong |
| `403` | authenticated, but not for this tenant (scoped token, allow-list, read-only server) |
| `404` | unknown endpoint, or a tenant that has no data yet |
| `413` | request body above `max_body_bytes` |
| `429` | a per-tenant limit was hit (`max_series`, `max_tenants`) |
| `500` | storage or I/O failure |

## Multi-tenancy

Every request belongs to exactly one tenant. It is chosen by a header, the same
one Pyroscope and the Grafana datasource send:

```
X-Scope-OrgID: acme
```

The header name is configurable (`--tenant-header`). `x-tenant-id` is accepted
as an alias. Rules:

- No header on an unauthenticated server → the `default` tenant.
- No header with a tenant-scoped token → the token's tenant.
- An empty header, or two headers with different values, → `400`. Neither is
  guessed, because guessing wrong writes a customer's data into someone else's
  store.
- Tenant ids are case-insensitive, `[A-Za-z0-9._-]`, at most 64 bytes. They are
  lowercased, because the id is also a directory name and NTFS/APFS are
  case-insensitive.

See [multi-tenancy.md](multi-tenancy.md) for the storage layout, limits and
operational notes.

## `POST /ingest`

Accepts one report or a batch:

```json
{ "reports": [ … ] }
```

Query parameters can override fields, which makes ad-hoc ingestion practical:
`service_name` (alias `name`), `profile_type` (alias `type`), `from`, `until`,
and any number of `label=key=value`.

### Formats

`?format=` picks the decoder explicitly; without it the `Content-Type` decides,
falling back to JSON:

| `format` | Content types that imply it | What it is |
| --- | --- | --- |
| `json` | anything else | the report schema below |
| `pprof` / `protobuf` / `proto` | `application/vnd.google.protobuf`, `application/x-protobuf` | a pprof protobuf profile, gzipped or not |
| `folded` / `collapsed` / `text` | `text/plain` | `stackcollapse` text: `main;serve;handle 123` |

`pprof` and `folded` need a service name (`?service_name=` or `?name=`); a
pprof profile whose `sample_type` is empty defaults to `cpu`. Per-sample pprof
labels become labels of their own series, which is how the Pyroscope SDKs tag
pod/span data.

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
and `?`). `service_name` is matched against the batch's service column, every
other label against its encoded label set.

Repeated `match[]=` parameters are ANDed, and `service_name=` /
`label=`+`label_value=` are accepted as shorthands.

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
- `wall` is *computed* (`cpu + off_cpu`) but may also be stored directly, which
  is what a wall-clock profiler that samples sleeping threads produces.
- `service_name` is at most 512 bytes, labels at most 64 keys of 128 bytes with
  1024-byte values, and a stack at most 2048 frames.
- `end_ns` must be greater than `start_ns`.

Response:

```json
{ "accepted": 1, "segments": 1, "samples": 2, "bytes": 812, "format": "json",
  "tenant": "acme",
  "files": ["batch-7"] }
```

`segments` and `files` are kept for compatibility with the previous file-backed
store: one accepted report is one row in `batches`, so `files` now contains the
batch ids (`batch-<id>`) rather than filenames. The ids are also the primary key
of the `batches` table and can be looked up with any DuckDB client. `bytes` is
the size of the request body, not the space the batch occupies in the database.

A batch is all-or-nothing: if any report is invalid, or a per-tenant limit would
be exceeded, nothing is written and the response is `400` / `429`. That keeps a
retry from duplicating the reports that did fit.

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
| `max_nodes` | `1024` | cap on tree size (clamped to 16…50 000) |
| `group_by` | — | insert one frame per value of this label, right under the root |
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

With `group_by=pod` the children of the root are synthetic frames named
`pod=<value>`; every stack below them has that label value. The frame names the
label as well as the value, because a tree can be grouped by any label.

```bash
curl -s 'http://localhost:4040/api/v1/flamegraph?profile_type=wall&from=now-15m' \
  | jq '{total, unit, frames: (.names | length)}'
```

## `GET /api/v1/top`

| Parameter | Default | Meaning |
| --- | --- | --- |
| `limit` | `50` | rows, clamped to 1…1 000 |
| `group_by` | — | split the table per label value |

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
| `buckets` | `60` | number of buckets (clamped to 1…1 000) |
| `step` | — | explicit bucket width (`10s`, `1m`); overrides `buckets` |
| `group_by` | — | return one series per value of this label |

```json
{
  "from_ns": 1700000000000000000,
  "until_ns": 1700003600000000000,
  "step_ns": 60000000000,
  "series": [
    {
      "service_name": "checkout",
      "labels": { "env": "prod" },
      "group": null,
      "points": [0, 120000000, 340000000]
    }
  ]
}
```

A batch contributes proportionally to every bucket its window overlaps, so a
10-second batch spread over a 60-second bucket does not spike.

With `group_by=pod`, `group` is `pod=<value>` (matching the flamegraph frame)
and `service_name` holds the bare label value.

## `GET /api/v1/series`

Known `(service, profile_type, labels)` combinations in the range:

```json
{
  "tenant": "acme",
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

`segment_count` appears in `/series`, `/profile_types`, `/stats`, `/tenants`
and the `lidlesseye_segments*` metrics. The name is kept for compatibility with
clients written against the earlier file-backed store; it now counts rows in
`batches`, i.e. ingest batches, one per accepted report.

## `GET /api/v1/diff`

Compares two windows of the same selection and returns one tree carrying both.

| Parameter | Default | Meaning |
| --- | --- | --- |
| `from`, `until` | last hour | the window being examined |
| `baseline_from`, `baseline_until` | the same span, immediately before | what it is compared against |
| `profile_type` / `type` | `cpu` | profile type |
| `selector` | — | label selector |
| `max_nodes` | `1024` | cap on tree size (clamped to 16…50 000) |

```json
{
  "names": ["root", "main", "handle_request"],
  "levels": [-1, 0, 1],
  "left":  [2000000000, 2000000000, 500000000],
  "right": [3000000000, 3000000000, 1800000000],
  "diff":  [1000000000, 1000000000, 1300000000],
  "total_left": 2000000000,
  "total_right": 3000000000,
  "unit": "nanoseconds",
  "truncated": false
}
```

`left` is the baseline, `right` the window, `diff` is `right - left` per node.
A node present in only one window gets a zero on the other side, which is what
makes a regression or a disappearance visible in the comparison view.

```bash
curl -s 'http://localhost:4040/api/v1/diff?from=now-1h&baseline_from=now-2h&baseline_until=now-1h'
```

## `GET /api/v1/label-names`

`/api/v1/labels` is an alias. Keys seen in the range:

```json
{ "label_names": ["service_name", "env", "pod"], "labels": ["service_name", "env", "pod"] }
```

Both spellings are returned so Pyroscope and Prometheus style clients work
unchanged.

## `GET /api/v1/tenants`

Administration endpoint. A tenant-scoped token only ever sees its own tenant.

```json
{
  "tenants": [
    {
      "id": "acme",
      "series": 12,
      "stats": { "segment_count": 480, "total_bytes": 8416712,
                 "logical_bytes": 6093120, "total_value": 991233,
                 "earliest_ns": 1700000000000000000, "latest_ns": 1700003600000000000 },
      "limits": { "retention_ns": 604800000000000, "max_disk_bytes": null, "max_series": 5000 }
    }
  ],
  "count": 1,
  "max_tenants": 1000
}
```

Reading this endpoint never creates a tenant; a tenant appears once something
has been written to it.

## `GET /api/v1/stats`

```json
{
  "version": "0.1.0",
  "tenant": "acme",
  "uptime_seconds": 812,
  "store": { "segment_count": 480, "total_bytes": 8416712,
             "logical_bytes": 6093120, "total_value": 991233,
             "earliest_ns": 1700000000000000000, "latest_ns": 1700003600000000000 },
  "series": 12,
  "tenants": 3,
  "ingest_reports": 480,
  "ingest_samples": 9120,
  "queries": 57
}
```

Every counter here belongs to the caller's tenant. Process-wide totals are on
`/metrics` instead, which is what an operator scrapes; a tenant-scoped token
must not be able to read another tenant's ingest volume. `tenants` and
`uptime_seconds` are the only process-level numbers, and neither is sensitive.

Inside `store`, `total_bytes` is the physical size DuckDB reports for the
tenant's database and `logical_bytes` is `SUM(batches.bytes)`, the
estimated size of the data still retained. They diverge after retention or
eviction: DuckDB frees rows inside existing row groups without shrinking the
file, so `total_bytes` stays at its high-water mark while `logical_bytes`
falls. The disk cap is enforced against `logical_bytes`; use `total_bytes` for
actual volume provisioning.

## Operations

| Endpoint | Response |
| --- | --- |
| `GET /api/v1/health` | `{"status":"ok","version":"0.1.0","uptime_seconds":42}`, always 200 |
| `GET /api/v1/ready` | `204` when the store holds no batches yet, `200` otherwise — both mean "ready to serve" |
| `GET /api/v1/stats` | store counters (batches, physical/logical bytes, value, earliest/latest), series count, tenant count, and ingest/query counters — all for the caller's tenant |
| `GET /api/v1/version` | `{"name":"lidlesseye","version":"0.1.0","multi_tenant":true,"tenant_header":"X-Scope-OrgID"}` |
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
lidlesseye_tenants             (gauge)
```

The `lidlesseye_segments*` series are named for the previous file-backed store;
they count ingest batches (rows in `batches`). `lidlesseye_storage_bytes` is the
physical size of all tenant database files combined. There is deliberately no
`segment`-per-tenant label: metrics are tenant-agnostic, a label carrying
`X-Scope-OrgID` would let any scraper enumerate customers. Per-tenant numbers
are available from `/api/v1/tenants` to an operator holding the right token.

## Compression and limits

Request and response bodies are gzip-compressed transparently when the client
asks for it (`Accept-Encoding: gzip`) and when the client sends
`Content-Encoding: gzip`. An agent pushing a lot of JSON per flush should use
it; `curl --compressed` and any well-behaved HTTP client do it for free.

Request bodies are capped at 64 MiB (`ServerConfig::max_body_bytes` when
embedding) and every request has a 60-second timeout.
