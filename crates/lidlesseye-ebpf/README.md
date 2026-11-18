# lidlesseye-ebpf

The eBPF collector behind [LidlessEye](https://github.com/xsj321/LidlessEye):
on-CPU sampling via `PERF_COUNT_SW_CPU_CLOCK`, off-CPU sampling via `schedule`
kprobes, connection accounting from the TCP stack, and symbolisation in the
agent with [blazesym](https://github.com/libbpf/blazesym).

Aggregation happens in the kernel, so overhead does not grow with the sampling
rate, and any process is profilable — no SDK, no `LD_PRELOAD`, no recompilation.

```rust
use lidlesseye_ebpf::{start, CollectorConfig};

let handle = start(CollectorConfig::default(), |report| {
    println!("{}: {} stacks", report.service_name, report.stacks.len());
})?;
```

Requires root (or `CAP_BPF` + `CAP_PERFMON` + `CAP_SYS_ADMIN`) and Linux ≥ 4.17.
The crate compiles on other platforms; `start()` returns an error there.

The BPF objects are checked in, so building needs no BPF toolchain. Set
`LIDLESSEYE_BUILD_BPF=1` to recompile them from `bpf/*.c` with clang.

Most programs should depend on [`lidlesseye`](https://crates.io/crates/lidlesseye)
instead. Documentation:
[collection](https://github.com/xsj321/LidlessEye/blob/master/docs/architecture.md).

Licensed under [Apache-2.0](https://github.com/xsj321/LidlessEye/blob/master/LICENSE).
The BPF programs are dual-licensed under GPL-2.0 (`LICENSE-BPF`).
