# lidlesseye

Continuous, low-overhead profiling for Linux, in one Rust crate — the eBPF
collector, a DuckDB-backed store, an HTTP API and a dashboard, together.
[Project README](https://github.com/xsj321/LidlessEye) ·
[Embedding guide](https://github.com/xsj321/LidlessEye/blob/master/docs/embedding.md)

LidlessEye is a backend you embed, not a service you deploy: one
`LidlessEye::builder()` gives you "profile this process and show me the
flamegraph" with no external infrastructure.

```rust
use lidlesseye::{LidlessEye, Result};

#[tokio::main]
async fn main() -> Result<()> {
    let eye = LidlessEye::builder()
        .data_dir("./profiles")
        .listen("127.0.0.1:4040")
        .agent(true)
        .service_name("my-service")
        .build()
        .await?;

    // Dashboard at http://127.0.0.1:4040
    eye.run().await
}
```

`agent(false)` keeps the storage engine and the HTTP API without loading the
collector; `ingest()` and `store()` work before `run()`, so a host that only
wants to store profiles never opens a port.

Or run it as a binary:

```bash
cargo install lidlesseye
sudo lidlesseye serve --agent --data-dir ./data
```

Requires Linux ≥ 4.17 and root (or `CAP_BPF` + `CAP_PERFMON` + `CAP_SYS_ADMIN`)
for collection. The BPF objects are checked in, so building needs no clang. The
DuckDB engine is linked dynamically: `libduckdb.so` has to be on the loader path
at runtime, which is what the [container image](https://github.com/xsj321/LidlessEye/pkgs/container/lidlesseye)
already does — and why the image is the easier way to run the server.

Documentation: [usage](https://github.com/xsj321/LidlessEye/blob/master/docs/USAGE.md) ·
[architecture](https://github.com/xsj321/LidlessEye/blob/master/docs/architecture.md) ·
[HTTP API](https://github.com/xsj321/LidlessEye/blob/master/docs/api.md).

Licensed under [Apache-2.0](https://github.com/xsj321/LidlessEye/blob/master/LICENSE).
