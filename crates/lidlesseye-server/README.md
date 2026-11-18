# lidlesseye-server

The HTTP server behind [LidlessEye](https://github.com/xsj321/LidlessEye): the
ingest API, the query API, Prometheus metrics and the dashboard, which is
compiled into the binary with `rust-embed`.

```rust
use lidlesseye_server::ServerConfig;

let server = lidlesseye_server::LidlessEye::bind(ServerConfig::new("./data")).await?;
println!("listening on {}", server.local_addr());
server.serve().await?;
```

`ServerConfig` is the multi-tenant surface: `X-Scope-OrgID` tenants, per-tenant
tokens, retention, disk and series limits. `ingest_for()` keeps the same
authorisation path as a request when you write in-process.

The published crate carries the dashboard export in `ui/`, so a dependency on
it (or `cargo install lidlesseye`) serves the real UI, not a placeholder. In a
checkout, `make ui` builds it and `build.rs` mirrors it into the crate.

Most programs should depend on [`lidlesseye`](https://crates.io/crates/lidlesseye)
instead. Documentation:
[embedding](https://github.com/xsj321/LidlessEye/blob/master/docs/embedding.md)
and the [HTTP API](https://github.com/xsj321/LidlessEye/blob/master/docs/api.md).

Licensed under [Apache-2.0](https://github.com/xsj321/LidlessEye/blob/master/LICENSE).
