# lidlesseye-servicemap

Service-map model, wire-protocol constants and pure query composition for
[LidlessEye](https://github.com/xsj321/LidlessEye).

This crate deliberately knows nothing about DuckDB, async runtimes or tenants.
It defines the node/edge/history shapes shared by the agent, server and eBPF
tests, plus `ServiceMapStore`: the small storage seam that the real store
implements. `lidlesseye-core` re-exports it as `lidlesseye_core::servicemap`, so
existing paths keep working.

```rust
use lidlesseye_servicemap::{ServiceMapStore, servicemap};

fn map<S: ServiceMapStore>(store: &S, from_ns: u64, until_ns: u64) -> Result<(), S::Error> {
    let _map = servicemap(store, from_ns, until_ns)?;
    Ok(())
}
```

See the [architecture guide](../../docs/architecture.md) for how the map is
collected and queried.
