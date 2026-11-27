# 0.1.57：集群慢的真因是 glibc 分配器，不是存储也不是查询算法

## 结论

同一台机器、同一内核、同一镜像、同一份数据：

| 位置 | 64 字节 `Box::new` 单次成本 |
|---|---|
| 本机 `docker run` 容器 | 48–57 ns |
| k8s pod（control-plane 节点） | 11 154 ns（约 200×） |
| k8s pod（worker 节点） | 78 907–86 993 ns（约 1800×） |

换成 musl 静态二进制，三处都是 50–59 ns；同一个 glibc 二进制 `LD_PRELOAD` jemalloc 后，pod 里变成 **39 ns**。也就是说：内核没问题、机器没问题、DuckDB 没问题，**是 glibc 的 malloc 在这个容器运行环境里每次分配要走约 80 µs 的内核时间**，而一次查询要分配几十万个小对象。

这解释了此前所有反常现象：

- pod 的 CPU 时间 91–97 % 是 `system`，`user` 几乎为零；
- 查询耗时与数据量、文件数、窗口大小都关系不大，只与"分配次数"相关；
- 冷启动 `schema` 20–25 s、`connections` 8–11 s、字典反序列化 9.8 s；
- 空闲也烧 2.7 核（writer/converter 每次写入都在分配）；
- 本地容器同镜像同数据只要 240–740 ms。

## 修复

`deploy/docker/Dockerfile` 运行时阶段：

```dockerfile
RUN apt-get update \
    && apt-get install -y --no-install-recommends libjemalloc2 \
    && rm -rf /var/lib/apt/lists/*
ENV LD_PRELOAD=/usr/lib/x86_64-linux-gnu/libjemalloc.so.2
```

用 `LD_PRELOAD` 而不是 Rust 的 `#[global_allocator]`，是因为 `libduckdb.so` 是独立的 C++ 动态库，它自己的 `malloc`/`new` 也必须被替换；Rust 侧的 global allocator 管不到它，而 DuckDB 恰恰是分配大户。

## 效果（集群 `disk` 命名空间，同一台机器）

| 指标 | 修复前 | 修复后 |
|---|---|---|
| `stats`（冷租户首请求） | 38 000 ms | **30 ms** |
| `in-memory store ready`（schema） | 20 827 ms | **40 ms** |
| `store ready` | 34 898 ms | **64 ms** |
| 字典从磁盘恢复 | 9 806 ms | **19 ms** |
| `servicemap` 1 h | 6 200 ms | **49 ms** |
| `timeseries` 1 h | 3 658 ms | **225 ms** |
| `top` 1 h | 3 600 ms | **150 ms** |
| `flamegraph` 1 h | 2 600–3 900 ms | **146 ms** |
| `flamegraph` 全部（3.8 h） | 11 330 ms | **710 ms** |

`probe2` 租户（62 849 批次 / 10 h）：`flamegraph` 1 h 254 ms、6 h 266 ms、10 h 286 ms；`top` 同量级；`servicemap` 1 h 83 ms。

功能校验：`flamegraph.total` 与 `top.total_value` 在 `default`、`probe2` 两个租户上逐位相等（19 032 000 000 000 / 11 817 000 000 000），`/servicemap` 节点数、边数与修复前一致（19 节点 / 48 边）——纯性能改动，结果不变。

## 仍然偏慢的一项

`timeseries` 随窗口增长：1 h 370 ms、6 h 2.36 s、10 h 3.48 s。它读 `batches` 表并把行搬到 Rust 侧分桶，宽窗口时行数线性上升。下一步应在 SQL 里按桶聚合（`GROUP BY bucket`），只把桶返回。

## 复现方式

```
# 分配器基准（同一二进制，三处运行）
rustc -O --target x86_64-unknown-linux-musl -o musl_m m.rs   # musl：三处都快
rustc -O -o bench m.rs                                       # glibc：pod 里慢 1800×
kubectl exec <pod> -- sh -c 'LD_PRELOAD=/tmp/libjemalloc.so.2 /tmp/bench'
```

## 追认：eBPF 分配器 uprobe 是主因，jemalloc 是绕过它

`crates/lidlesseye-ebpf/src/allocator.rs:730` 用 `UProbeScope::AllProcesses` 把
uprobe 挂到 libc 的 `malloc`/`free` 家族上，`collect_alloc` 默认开启
（`crates/lidlesseye/src/main.rs:477` 是 `collect_alloc(!args.no_alloc)`）。于是
节点上每个 glibc 进程的每次分配都要进一次内核走 BPF。

同一台机器、同一个 pod、同一个 glibc 二进制，按 agent 状态测：

| agent 状态 | 单次 64 字节分配 |
|---|---|
| 两套 agent（`disk` + `lidlesseye` 命名空间）全开，probe 挂满 | 79 000–87 000 ns |
| 只停一套 | 44 000 ns |
| 两套全停 | **2 644 ns** |
| 两套全开 + `LD_PRELOAD` jemalloc | **39–56 ns** |

所以：

- **uprobe 占了大头**（约 97 %）；
- jemalloc 之所以仍然快，是因为探针挂在 libc 的 `malloc` 符号地址上，而 jemalloc
  用自己的 `malloc` 替换了它，那段被插桩的指令根本不会执行；musl 静态二进制同理；
- 停掉 agent 后 glibc 仍有 2.6 µs（本地 57 ns），残余差异还没查清，但比 uprobe
  的影响小两个数量级。

**建议**：`collect_alloc` 改成默认关闭、按需开启（`--no-alloc` 反转成 `--alloc`），
或者至少把服务端自己的进程排除掉——用 node 级 `AllProcesses` 挂 malloc uprobe，
对节点上任何分配密集的进程都是灾难，而我们自己的 server 就在同一个节点上。
jemalloc 仍然保留：它让服务端不再受这个开关影响。


## 0.1.58–0.1.59：探针改 opt-in，去掉 jemalloc

0.1.57 的 jemalloc 是**症状修复**：真正的代价是 agent 在节点上给 libc 的 `malloc`/`free` 挂了全进程 uprobe，每次分配都要陷入内核跑 eBPF。把探针关掉后 glibc 自己就回到 135 ns/op，jemalloc 不再需要。

| 配置 | `alloc only`（64 B Box） | flamegraph 1h | flamegraph 全部 | servicemap 1h | timeseries 1h |
|---|---|---|---|---|---|
| 0.1.56，探针全开，glibc | 87 000 ns | 2 600 ms | 11 330 ms | 6 200 ms | 3 650 ms |
| 0.1.57，探针全开 + jemalloc | 39 ns | 146 ms | 710 ms | 49 ms | 225 ms |
| 0.1.58，探针默认关 + jemalloc | 135 ns | 15 ms | 69 ms | 20 ms | 9 ms |
| 0.1.59，探针默认关，无 jemalloc | 135 ns | 16 ms | 67 ms | 16 ms | 9 ms |

- 探针默认关：`--alloc` 才打开（`crates/lidlesseye/src/main.rs` 由 `--no-alloc` 改为 `--alloc`）。
- jemalloc 已从 `deploy/docker/Dockerfile` 移除；A/B 同一镜像、同一数据、只改 `LD_PRELOAD` 清空，两端耗时一致（flamegraph 1h 14.8 → 14.4 ms）。
- 一致性校验（`default`，一亿一千万 ns 窗口）：`flamegraph.total = top.total_value = Σ self_value = 10 973 000 000 000`，逐位相等。

### 为什么不做常驻的 memleak 式追踪

BCC/eunomia 的 `memleak`（uprobe on `malloc`/`free` + 指针哈希表 + 归零时回溯栈）用的正是我们这套机制，因此代价同源：每分配一次就一次 uprobe 命中。它适合**按需排查泄漏**（单进程 `-p`、限时、`--min-size` 过滤），不适合常驻采样。若要兼顾，只能是三种方向之一：采样周期（每 N 次分配陷一次）、目标限定（只挂指定 PID）、或换成运行时自报（Go `runtime.mbuckets`、JFR、`tracemalloc`）。
