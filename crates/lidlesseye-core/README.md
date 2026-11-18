# lidlesseye-core

The profile model, the DuckDB-backed store and the query engine behind
[LidlessEye](https://github.com/xsj321/LidlessEye) — continuous eBPF profiling
for Linux.

Profiles are append-only rows filtered by time and grouped by stack, which is
what DuckDB already does well: this crate adds the schema, the ingest path, the
retention and quota accounting, and the flamegraph/diff queries on top.

```rust
use lidlesseye_core::{Frame, ProfileType, ReportBuilder, Store, StoreConfig};

let store = Store::open(StoreConfig::new("./data"))?;
let mut report = ReportBuilder::new("checkout", ProfileType::cpu(), 0, 10_000_000_000);
report.add_stack([Frame::new("/bin/checkout", "main")], 1_000_000);
store.append(&report.finish())?;
```

Most programs should depend on [`lidlesseye`](https://crates.io/crates/lidlesseye)
instead: it re-exports this crate together with the collector and the server.

Documentation: [storage and queries](https://github.com/xsj321/LidlessEye/blob/master/docs/architecture.md).

Licensed under [Apache-2.0](https://github.com/xsj321/LidlessEye/blob/master/LICENSE).
