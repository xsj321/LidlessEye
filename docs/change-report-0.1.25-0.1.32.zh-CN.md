# 0.1.25 → 0.1.32 查询性能改造报告

## 一、问题与结论

现象：本地集群上「1 小时火焰图 4–8 s、服务地图 4 s」，写入也慢。

实测（`disk` 命名空间，tenant `default`，基线 0.1.24-rc1）：

| 端点 | 基线 | 现在（0.1.32） |
|---|---|---|
| stats | 111 ms | 41 ms |
| servicemap 1 h | 4818 ms | 3197 ms |
| timeseries 1 h | 2377 ms | 2141 ms |
| top 1 h | 3818 ms | 1616 ms |
| flamegraph 1 h | 3918 ms | 1587 ms |
| flamegraph 6 h | 9534 ms | 5519 ms |
| flamegraph all | 9812 ms | 8108 ms |

**根因不是 Parquet 格式，也不是行数。** 集群里总共只有 10 MB、约 5 万个 batch，
扫描量根本不是瓶颈；真正的固定开销是**每次查询都把整个字典重新读一遍**：

```
slow dictionary read stage="load_stacks" scanned=565420 wanted=69703 elapsed_ms=2966
slow dictionary read stage="load_frames" scanned=92670  wanted=19184 elapsed_ms=1375
```

一次 6 小时查询里，samples 只花 0.6 s，而 `load_stacks` + `load_frames` 花了 4.4 s。
`load_stacks` 原本用 `span_all("stacks")`，即**无视查询窗口、读取保留期内全部 stacks 文件**
（325 个文件、56 万行），只为取出 7 万个 id；`load_frames` 同理。

结论：把存储容器换成「按小时一个 DuckDB 文件」不会改善这一点——两个方案都要按
`stack_id` 做同样的 `GROUP BY`，也都要读同样的字典行。真正要做的是**让字典读得少、读得对**。

## 二、实际改动

### 1. 字典按查询窗口读（0.1.25）

`Store::load_stacks` 改为先用 `span("stacks", from, until)`，只有取不到的 id 才回退到
`span_all`。因为 stack 行写入时就是按「引用它的 samples 所在小时」分片的
（`parquet::flush_by_sample_hours`），所以窗口内的文件就是唯一可能包含这些 id 的集合。

效果：flamegraph 1 h 3918 → 1932 ms，top 1 h 3818 → 1915 ms，servicemap 4818 → 1853 ms。

### 2. 字典缓存（0.1.29）

`Store` 增加两个只读缓存：

* `stacks_by_id: RwLock<HashMap<u64, Arc<Vec<u32>>>>`
* `frames_by_id: RwLock<HashMap<u32, Arc<Frame>>>`

字典行一旦写入就不变，可以按 id 缓存、永不失效。命中直接返回，未命中才读 Parquet 并
只保留需要的 id（Rust 侧过滤）。上限 `DICT_CACHE_ROWS = 2_000_000`，超过即停止增长并回退
到直接读 Parquet——**最坏情况是旧延迟，不会 OOM，也不会读错**。

### 3. 让并发合并不再丢数据（0.1.26）

`compact_local` 现在也能合并「当前小时」，否则 10 s 一次的 flush 会让活动小时积累
300+ 个文件。合并与 flush 之间存在一个真实的数据丢失窗口：合并先列出文件、COPY，再发布
`final-*.parquet` 并 retire 输入；此时若 flush 写入一个新的 `part-*`，它的 mtime 早于刚发布的
final，而 `scan_local_table` 的「最新 final 为准」规则会让这个 part**对之后所有查询不可见**，
且没有任何报错。

修法：`ParquetState` 增加 `merge_gate: Arc<RwLock<()>>`；`write_hour_file` 全程持读锁，
活动小时合并在**列目录之前**持写锁并在锁内重新列一次目录。合并另外要求
`files.len() >= OPEN_HOUR_MIN_FILES`（8）且总字节 ≤ 1 GiB，避免大 COPY 卡住写入。

`MAX_MERGES_PER_PASS` 从 1 提到 `2 * TABLES.len()`：六张小合并合起来并不比一次大合并更久，
但能让每张表每轮都轮到。实测活动小时从 304 个文件降到 4 个。

### 4. 退役文件竞态（0.1.26）

`net_nodes` / `net_edges` 补上和 `Store::scan` 一样的 `is_retired_file` 重试，修掉
`servicemap` 偶发 500：

```
IO Error: Cannot open file ".../part-...-19.parquet"   (已被合并 retire)
```

### 5. 可观测性

新增日志：
`stage="scan"`（samples_ms / stacks_ms / frames_ms / total_ms / rows）、
`stage="load_stacks"` 与 `stage="load_frames"`（scanned / wanted / found / elapsed_ms）。
没有这三个数，前面所有猜测都只能靠猜。

## 三、试过但被否决的方案（都有实测数据）

| 方案 | 结果 | 原因 |
|---|---|---|
| `--duckdb-threads=1` | servicemap 14.2 s（更差） | DuckDB 每查询并行是真在干活 |
| `--flush-interval=120s` | flamegraph 6.0 s（更差） | 刷新变少但每次更大，写锁占用更久 |
| 全量预热字典（0.1.30/0.1.31） | 首查询 >60 s，top 直接 408 | 预热持读者连接做全字典读，等于把冷启动代价压到第一个请求上 |
| `WHERE stack_id IN (7 万个 id)` | flamegraph 20.9 s，top 32 s → 503 | DuckDB 为巨型 IN 列表规划表达式，代价爆炸 |
| 控制小时也能安全合并 | 已按上面的 gate 方案实现 | 直接合并会静默丢行，见 2.3 |

## 四、底线验证（功能全部正常）

* `/health`、`/ready`、`/stats`、`/tenants`、`/series`、`/timeseries`、`/diff`、
  `/servicemap`、`/servicemap/history`、`/flamegraph`、`/top`、`/ingest`、`/flush` 全部 200。
* 一致性：`top` 的 `self_value` 之和 = `top.total_value`（覆盖率 100.0%），
  且 `flamegraph.total` 与 `top.total_value` 一致（1 h 完全相同：11 133 000 000 000 ns）。
* 无 500、无超时；活动小时合并后行数经回读校验后才 retire 输入文件。
* 数据格式未变（仍是 Parquet），**旧数据继续可读**，不需要重导入。

## 五、回滚

镜像与参数都在 Deployment 上，回滚只需改回镜像：

```powershell
kubectl -n disk patch deploy/le-disk-lidlesseye --type=json `
  --patch-file=target\tmp\patch-024.json   # image: docker.io/xsj321/lidlesseye:0.1.24-rc1
```

参数保持 `serve --retention=7d --max-disk=4GB --tenant-header=X-Scope-OrgID --max-tenants=1000
--query-concurrency=4 --storage-mode=disk --flush-interval=10s` 不变。

## 六、仍待做（按收益排序）

1. **frames 字典的增量读**：目前未命中时仍会扫 `hour=0` 的全部 frames 文件（约 0.8 s）。
   已加 `frames_max_id` 水位线并在 SQL 里 `WHERE frame_id > 水位线`，但首次仍需全读。
2. **samples 扫描本身**（1 h 约 0.6–0.9 s）已经是剩余的大头；要再降只能做分钟级预聚合
   （`rollup(minute_ns, profile_type, service_name, labels, stack_id, value_sum, sample_count)`），
   `timeseries` 已经在走 `batches` 这条同样的思路。
3. **写入侧**：converter 目前约 2 s CPU / 8 MiB 段，且每 10 s 的 flush 会占用写连接 0.3–4 s。
   `duckdb_memory_limit` 仍是 `"1GB"`，值得提到 8–16 GB 再看。
4. `net_nodes` / `net_edges` 每 10 s 各一次写事务（1.2–1.7 s），可以合并到同一次 flush。
