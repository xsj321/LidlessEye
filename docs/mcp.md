# MCP server

LidlessEye ships a small [Model Context Protocol](https://modelcontextprotocol.io/) server that lets an AI assistant query the service map and profiling data through tools. The adapter is read-only: it calls the same HTTP API the dashboard uses and never writes to the store.

[中文版本](mcp.zh-CN.md)

## Design

```
AI client ── MCP stdio JSON-RPC ──> lidlesseye-mcp ── HTTP GET ──> lidlesseye server
```

The MCP process is a stdio child process, so **stdout is reserved for JSON-RPC** and all logs go to stderr. It does not link DuckDB or resolve tenants itself; the server remains the single owner of authentication, tenant isolation and query cancellation.

Every tool returns the same typed object:

```json
{
  "summary": "The profile totals 183000000 ns from 183 sample(s); top frames are ranked by inclusive value.",
  "query_context": {
    "endpoint": "/api/v1/flamegraph",
    "tenant": "default",
    "params": { "from": "now-1h", "until": "now", "profile_type": "cpu" }
  },
  "unit": "ns",
  "truncated": false,
  "data": { "total": 183000000, "samples": 183, "top_frames": [] },
  "suggestions": []
}
```

- `summary` states what the numbers mean and in which unit.
- `query_context` echoes the exact endpoint, tenant and parameters so the model can explain and repeat a query.
- `data` is **summary-first**: totals first, then a bounded top-N. A flamegraph never returns all frames.
- `truncated` says explicitly when rows or buckets were omitted.
- `suggestions` gives safe next calls when an answer is empty or a selector is invalid.

Tool errors use MCP `isError: true` with actionable text, for example telling the model to set a token after HTTP 401 or to narrow the window after HTTP 408/503.

## Build and run

```bash
cargo build --release -p lidlesseye-mcp
./target/release/lidlesseye-mcp \
  --base-url http://127.0.0.1:4040 \
  --tenant default
```

After a release, the same binary is available as `cargo install lidlesseye-mcp`.

| Flag | Environment variable | Default | Meaning |
| --- | --- | --- | --- |
| `--base-url` | `LIDLESSEYE_BASE_URL` | `http://127.0.0.1:4040` | LidlessEye server address |
| `--token` | `LIDLESSEYE_AUTH_TOKEN` | unset | Bearer token for authenticated servers |
| `--tenant` | `LIDLESSEYE_TENANT` | `default` | Tenant sent on tenant-scoped queries |
| `--tenant-header` | `LIDLESSEYE_TENANT_HEADER` | `X-Scope-OrgID` | Header carrying the tenant |
| `--timeout-seconds` | `LIDLESSEYE_TIMEOUT_SECONDS` | `60` | HTTP request timeout |

Example client configuration:

```json
{
  "mcpServers": {
    "lidlesseye": {
      "command": "/absolute/path/to/lidlesseye-mcp",
      "env": {
        "LIDLESSEYE_BASE_URL": "http://127.0.0.1:4040",
        "LIDLESSEYE_TENANT": "default",
        "LIDLESSEYE_AUTH_TOKEN": "replace-me-if-auth-is-enabled"
      }
    }
  }
}
```

Do not run the binary bare in a terminal and wait for output: a stdio MCP server waits for an `initialize` message from its client before serving requests.

## Tools

| Tool | What it returns | Important arguments |
| --- | --- | --- |
| `get_health` | server status and version | none |
| `list_tenants` | tenants and their data volume | none |
| `list_profile_types` | profile types, units and totals in a window | `from`, `until`, selectors |
| `list_series` | services, profile types and label sets | `from`, `until`, selectors |
| `list_label_values` | most common values of one label | `name`, `limit`, window |
| `get_service_map` | top nodes and edges, connections/bytes/retransmits, RTT in milliseconds | `from`, `until` |
| `get_service_map_history` | one node's own and incident-edge counters over time | `node`, `buckets`, optional `protocol` (for example `dns`), window |
| `get_service_map_histogram` | per-bucket traffic total and peak position | `metric`, `buckets`, window |
| `get_flamegraph_summary` | total, sample count and hottest inclusive frames | `profile_type`, `max_frames`, window |
| `get_top` | hottest functions by self value with share of total | `profile_type`, `limit`, window |
| `get_timeseries` | per-series totals, peaks and bucket count | `profile_type`, `buckets`/`step`, `max_series` |
| `diff_profiles` | ranked regressions and improvements between two windows | baseline window, `profile_type`, window |

All time arguments accept `now-1h`, RFC3339 or unix timestamps. `until` must be later than `from`. For a multi-tool investigation, use the **same absolute `from` and `until`** in every call; relative `now` values are re-evaluated by the server on each request and can shift between calls.

## Suggested analysis loop

1. `get_health`, then `list_tenants` if the tenant is uncertain.
2. `list_profile_types` and `list_series` to discover what data exists and what unit applies.
3. `get_service_map` or `get_service_map_histogram` to find the interesting service and time bucket.
4. `get_top` and `get_flamegraph_summary` for the hot functions; `list_label_values` to refine a selector such as `pod`.
5. `get_timeseries` to locate the start of a change, then `diff_profiles` with explicit absolute baseline and comparison windows.
6. `get_service_map_history` on a node whose edges show retransmits or high RTT.

Positive values in `diff_profiles` are regressions, negative values improvements. Memory profiles may be `inuse_space`/`inuse_objects` (a level) or `alloc_space`/`alloc_objects` (a rate); never add the two kinds together.

## Limits and safety

- Every tool is read-only. Ingestion, tenant administration and settings are out of scope.
- Results are intentionally bounded. Use the dashboard for a full flamegraph or a complete node list.
- A wide window can still take seconds or return HTTP 408/503. Narrow the window or use a longer `--timeout-seconds`.
- The token is sent only to the configured server. Keep the client configuration file private and set `RUST_LOG` only to `warn` (the default) unless debugging.
- RTT is reported in milliseconds; profiling values keep the endpoint's own unit, which is echoed in `unit`.
