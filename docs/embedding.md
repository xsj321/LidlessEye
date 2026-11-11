# Embedding LidlessEye

`crates/lidlesseye` is the crate you add to your own project. It re-exports the
other three, so one dependency is enough:

```toml
[dependencies]
lidlesseye = "0.1"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

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

    eye.run().await
}
```

`build()` binds the socket immediately (errors surface at startup, not on the
first request) and, with `.agent(true)`, starts the collector.

## Three modes

### Server only — you feed it

```rust
let eye = LidlessEye::builder()
    .data_dir("./profiles")
    .listen("0.0.0.0:4040")
    .build()
    .await?;
eye.run().await
```

Accept profiles over `POST /ingest`, or push them in-process with
`eye.ingest(report)`.

### Server + built-in collector — profile this process' machine

```rust
let eye = LidlessEye::builder()
    .data_dir("./profiles")
    .agent(true)
    .frequency_hz(99)
    .flush_interval(Duration::from_secs(5))
    .collect_cpu(true)
    .collect_off_cpu(true)
    .build()
    .await?;
```

`.agent(true)` requires Linux and root. On any other platform it returns an
error; call `.agent(cfg!(target_os = "linux") && is_root())` if you want one
binary that adapts.

### Background — don't block your own `main`

```rust
let instance = LidlessEye::builder()
    .data_dir("./profiles")
    .listen("127.0.0.1:0")   // ephemeral port
    .build()
    .await?
    .spawn();

println!("profiling UI on {}", instance.base_url());
// … your service keeps running …
instance.shutdown().await;
```

Dropping a `RunningInstance` stops the HTTP server and the collector.

## Producing profiles

`ReportBuilder` is the ingestion API. It takes **symbolized** frames, leaf
first, with values already in nanoseconds (or bytes/counts for allocation
profiles):

```rust
use lidlesseye::core::{Frame, ProfileType, ReportBuilder};

let mut builder = ReportBuilder::new("my-service", ProfileType::cpu(), start_ns, end_ns)
    .label("env", "prod")
    .label("region", "eu-west")
    .sample_rate_hz(99);

builder.add_stack(
    vec![
        Frame::new("/app/server", "handle_request"),
        Frame::new("/app/server", "main"),
    ],
    120_000_000, // 120 ms of CPU time
);

let report = builder.finish();
report.validate()?;      // rejects malformed index tables
eye.ingest(report).await?;
```

A complete, runnable version is `cargo run --example synthetic`.

Anything can be a source: a language runtime that walks its own stacks, a JIT
that keeps a shadow stack, a batch job that measures allocations, a test harness
that wants a diff of two runs. Profile types are free-form strings, so
`ProfileType::new("lock_contention")` works without a schema change.

## Custom collectors

If you want the eBPF machinery but your own flush policy, drop one level down:

```rust
use lidlesseye::ebpf::{CollectorConfig, start};

let handle = start(CollectorConfig {
    service_name: Some("sidecar".into()),
    frequency_hz: 199,
    flush_interval: Duration::from_secs(3),
    exclude_self: false,
    ..Default::default()
}, |report| {
    // called from the collector thread once per flush
    println!("{} stacks", report.stacks.len());
})?;

// later
let stats = handle.stats();
handle.stop();
```

The callback runs on the collector's own OS thread — capture a
`tokio::runtime::Handle` if you need to hop back into async code (that is
exactly what `Builder::build` does).

`CollectorConfig` also carries `labels`, `exclude_pids`,
`enrich_kubernetes_labels` (derive `namespace`/`pod`/`container` from
`/proc/<pid>/cgroup`) and `node_label`.

## Querying without the server

The query engine works directly on a `Store`, which is handy for a CLI, a cron
job or a test:

```rust
use lidlesseye::core::{Store, StoreConfig, QueryOptions, Selector, flamegraph, top};

let store = Store::open(StoreConfig::new("./profiles"))?;
let opts = QueryOptions::new("cpu", from_ns, until_ns, Selector::default());

let fg = flamegraph(&store, &opts)?;
println!("{} of CPU in {} frames", fg.total, fg.names.len());

for row in top(&store, &opts, 10)?.items {
    println!("{:<40} {}", row.name, row.self_value);
}
```

## Pushing to a remote server

`publisher::start` gives you the agent's uploader: batched, gzip-friendly,
exponential backoff.

```rust
use lidlesseye::PublisherConfig;

let (publisher, uploader) = lidlesseye::publisher::start(
    PublisherConfig::new("http://storage:4040")
)?;

publisher.send(report);          // non-blocking
// publisher.send_now(report).await; // flush immediately
// uploader.abort();             // stop the background task
```

## Feature checklist

| Need | Setting |
| --- | --- |
| Require a bearer token | `.auth_token(Some(token))` |
| Serve a browser from another origin | `.cors(Some("*".into()))` |
| Keep data forever | `.retention(None).max_disk_bytes(None)` |
| Query-only replica of a data directory | `.read_only(true)` |
| Aggregate a whole machine under one service | `.service_name("my-service")` |
| One series per process (default) | leave `service_name` unset |

## Performance notes

- `build()` is cheap: it binds a socket, opens the store and (optionally) loads
  the BPF object. There is no background thread until `spawn()`/`run()`.
- Ingestion writes one file per report; at 10-second flushes that is ~8.6k files
  per day per service, which any filesystem handles. Very short flush intervals
  combined with high service cardinality is the one pattern to avoid.
- Queries never block ingestion: the store's index is an `RwLock` held only for
  the metadata, and the actual scans run in `spawn_blocking`.
