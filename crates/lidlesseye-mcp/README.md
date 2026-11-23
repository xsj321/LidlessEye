# lidlesseye-mcp

A read-only [Model Context Protocol](https://modelcontextprotocol.io/) server
that lets AI clients query [LidlessEye](https://github.com/xsj321/LidlessEye)
service maps and profiling data over stdio.

```bash
cargo install lidlesseye-mcp
lidlesseye-mcp --base-url http://127.0.0.1:4040 --tenant default
```

The binary calls the LidlessEye HTTP API; it does not link DuckDB and does not
manage tenants itself. All tools return a compact envelope with a summary,
query context, explicit unit, truncation flag and ranked data.

See [docs/mcp.md](../../docs/mcp.md) for client configuration, the full tool
list and the recommended analysis workflow.
