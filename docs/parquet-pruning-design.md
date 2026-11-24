# Parquet/S3 查询性能事故：根因与修复（0.1.19 落地记录）

> 本文是 0.1.19 的实现记录，不再包含"待评审"的方案选项。
> 被否决的两条错误结论保留在第 2 节，避免以后重复踩坑。

## 1. 现象

0.1.16 引入"内存热层 + Parquet/S3"后，本地集群出现：

- `/api/v1/tenants`、`/api/v1/stats`、`/api/v1/timeseries`、`/metrics` 从数百毫秒
  劣化到 40–60 秒超时（HTTP 层 60 秒返回 408），前端页面因此空白；
- 同一时刻 `/api/v1/version`（不碰 store）仍是 10 ms —— HTTP 层健康，问题在 store 层；
- server 日志 `store_lock_wait requester=write ... wait_ms=75000~313000`；
- agent 随之 `report queue full` / 放弃上报；
- 重启 server 短暂恢复，几分钟后再次劣化。

## 2. 先排除的错误结论

### 2.1 不是数据量

`globex` 只有 90 行、`default` 有数百万行，两者在故障时**都会** 408。
`shop-stress` 的 stats 扫描日志是 `elapsed_ms≈1500`，不是几十秒。

### 2.2 不是 Parquet 文件数量

实测单个 S3 请求（经 kube-proxy + port-forward 打到 s3mock）：

```
HEAD hour=498557.parquet  0.006s × 8 次，稳定 6 ms
```

84 个文件 × 2 次请求 × 6 ms ≈ **1 秒**。DuckDB 2.x 可以把 S3 上的 Parquet
直接当作表来读，文件数量本身不是瓶颈。

真正的成本不是"文件多"，而是**每次查询都要为它给出的每个路径付一次 footer
往返**（见 3.1）。所以正确的方向是"别给那么多路径"，而不是"把文件合少"。

## 3. 根因（按贡献排序）

### 3.1 canonical 表是 VIEW，覆盖整个保留窗口

0.1.16 的实现：

```sql
CREATE VIEW samples AS
  SELECT * FROM samples_hot
  UNION ALL BY NAME
  SELECT * FROM read_parquet([全部本地文件, 全部 S3 对象],
                             hive_partitioning=false, union_by_name=true);
```

`read_parquet([...])` 必须打开所列**每一个**路径的 footer 才能知道 schema 和
row group 布局。VIEW 定义里是整条保留窗口的全部文件（`default` 租户 84 个，
`shop-stress` 更多），于是——

> **查 1 小时，要付 8 天的 footer 成本。**

前端每几秒轮询一次，每次轮询都把全部 footer 重读一遍。

### 3.2 元数据缓存默认关闭

DuckDB 的这两个设置默认都是 `false`，代码里也从未打开：

| 设置 | 默认 | 作用 |
|---|---|---|
| `enable_http_metadata_cache` | `false` | 缓存 HTTP/对象元数据（ETag、长度） |
| `parquet_metadata_cache` | `false` | 缓存 Parquet schema / row group 统计 |

**直接证据**：s3mock 最近 300 条请求里，`shop-stress/batches` 的**同一个**
`hour=*.parquet` 被 GET/HEAD 了 **151 次**；`zeta`、`acme`、`stress-trial` 各 30 次。
正常的一次性读取不会这样。

### 3.3 单连接串行

每个 tenant 只有一把 `conn: Mutex<Connection>`：读、ingest、flush 共用一条连接。
一个宽扫描执行期间，后续请求全部排队；排满 60 秒就是 HTTP 408。

### 3.4 请求路径做全表扫描

`/api/v1/stats`、`/api/v1/tenants`（每个 tenant 一行）、`/metrics`
（`registry.total_stats()` 遍历所有 tenant）都要执行：

```sql
SELECT COUNT(*), SUM(total_value), SUM(bytes), MIN(start_ns), MAX(end_ns) FROM batches
SELECT COUNT(*) FROM (SELECT DISTINCT service_name, profile_type, labels FROM batches)
```

`stats_snapshot` 本来有 30 秒内存缓存兜底，但 `archive_local` 和
`compact_local` 成功后都会 `*stats_cache = None`（每 10 秒一轮），
`/metrics` 又走 `total_stats()` 绕开缓存逐个 tenant 现算。
于是缓存每 10 秒被打掉一次，10 个 tenant × 1.5 秒 = 15 秒的全量扫描被反复触发。
`bind()` 里还有一次启动期 `total_stats()`，在十租户环境下让 liveness 探针直接超时被杀。

### 3.5 写路径无超时

读走 `read_conn()`（45 秒 `try_lock_for`，超时 503），但写走 `lock_conn()`
→ `self.conn.lock()`，**没有超时**。一次慢扫描会把 agent 的 ingest 无限期排在后面
（日志里 75–313 秒），agent 超时重试又加重队列，形成雪崩。

### 3.6 `http_timeout` 单位写错（秒 vs 毫秒）

DuckDB 的 `http_timeout` 单位是**秒**，默认 30。代码写的是
`SET http_timeout=15000`，注释意图是"15 秒"，实际是 **15000 秒 ≈ 4.16 小时**。
一旦某个 S3 连接半开，这次 `recv` 会挂起 4 小时；watchdog 的 `interrupt()`
只置标志位，打不断阻塞的 socket 读 —— 这就是"重启就好、不重启永远不好"。

### 3.7 因果链

```
VIEW 覆盖整个保留窗口
  → 每次 poll 为每个文件付一次 footer 往返（且缓存全关，无法复用）
  → stats 扫描 1~2 s，10 个 tenant 串成 15 s
  → 每 10 s 的 archive/compact 又清空 stats 缓存
  → 前端高频轮询持续占住唯一一条 conn
  → 写（agent ingest）在无超时的 lock 上排队 → 上报超时 → 重试 → 队列更长
  → 偶发半开连接遇上 http_timeout=15000 秒 → 锁被占 4 小时 → 全站 408
```

## 4. 修复

### 4.1 取消 VIEW：按 hour 分片的文件索引 + 每查询动态 span

`parquet.rs` 不再维护"扁平的全部文件列表"，也不再建 VIEW，而是把文件按
**数据所属的 epoch hour** 分组，查询时按时间窗只挑相关的 hour：

```rust
pub(crate) struct TableFiles {
    by_hour: BTreeMap<u64, Vec<String>>,
    unknown: Vec<String>,   // 名字里没有 hour 的旧文件：每个窗口都挂载
}

pub(crate) fn table_span(state, table, from_ns, until_ns) -> String {
    // → "(SELECT * FROM samples_hot UNION ALL BY NAME
    //     SELECT * FROM read_parquet(['...hour=498557/part-...', ...],
    //                                hive_partitioning=false, union_by_name=true))"
}
```

要点：

- `table_span_all`（`0..u64::MAX`）用于没有时间窗的基数统计，其余查询一律走
  `table_span`，一个 1 小时窗口只列出 1 个对象。
- `unknown`（无法归入某小时的文件）在**每个**窗口都挂载 —— 分片不允许藏起
  它无法归因的数据。
- `union_by_name=true` 不是可选项：升级会加列，归档里还留着加列之前的文件，
  按位置 UNION 会让 binder 直接拒绝。
- `hour_window` 对 `until` 做独占处理，窗口边界不会多挂一个 hour。

### 4.2 新布局（旧布局继续可读）

| 位置 | 0.1.19 写入路径 | 仍然被索引的旧路径 |
|---|---|---|
| 本地 | `parquet/<table>/hour=<n>/part-<stamp>-<rows>.parquet` | `<table>/h<hour>-<stamp>-<rows>.parquet`、`h<hour>-final-<stamp>.parquet` |
| 本地合并 | `hour=<n>/final-<unique>.parquet` | — |
| S3 | `{base}/{table}/hour=<n>-part-<stamp>.parquet` | `{table}/hour=<n>.parquet` |

本地用 `hour=<n>/` 目录分片，S3 用平坦 key —— 这个不对称是刻意的：`read_dir`
免费，而 `glob()` 是**逐层展开**的。S3 上如果也用 `hour=<n>/` 前缀，启动时每个
已归档的小时都要一次 LIST：十租户集群实测每个租户 12 个模式的 LIST 要 18–51 秒，
直接超过 startupProbe 的 300 秒预算。改成平坦 key 后，整个租户只有
`glob('{base}/*/*.parquet')` 一次调用（前缀一次 + 每张表一次）。

`discover_s3_objects` 用这一个模式同时覆盖新旧布局（`hour=<n>-part-…`、
`hour=<n>.parquet`、`h<n>-…`），按 key 里的 `hour=` 入索引，**不做任何对象迁移**。

分片的小时**必须用整数除法**。DuckDB 的 `/` 是浮点，`CAST(double AS BIGINT)`
四舍五入：`start_ns/1e9/3600` 会把 13:43 的事件算成第 14 小时，于是每个小时的后
半段数据都落在下一小时的目录里，"窗口截止到现在"的查询看不到它们 —— 这是 0.1.19
开发过程中真实踩到的坑（由 CLI 的 ingest→query 往返测试暴露）。改成 `//`。

旧布局文件的小时取自**写入时的挂钟**，跨小时边界的 flush 会把 13:59 的行命名为
14:00。因此只要某张表还存在旧布局文件，它的查询窗口就向前后各放宽一小时：
多挂两个对象，换"升级后不丢边界数据"。

### 4.3 多读连接池（读写分离）

```rust
struct ReaderPool { slots: Vec<ReaderSlot>, free: Mutex<Vec<usize>>, cv: Condvar, .. }
```

- `Connection::try_clone()` 派生出 N 条连接（`--query-concurrency`，默认 4，
  clamp 1..=32），共享同一个 in-memory 数据库，靠 DuckDB 的 MVCC 并发读。
- 克隆出来的连接是**全新会话**，`SET` 是会话级的，所以 `tune_connection` 与
  `configure_s3` 必须在每个槽位上重新执行一次。
- 写连接 `conn` 仍独享，`interrupt()` 只打断读槽位，绝不打断写事务。
- 读等待仍是有界的（45 秒），拿不到就返回 `Error::Busy` → HTTP 503 + `Retry-After`。

### 4.4 开启元数据缓存并修正超时单位

```rust
SET http_timeout=15;                    // 秒，不是毫秒
SET http_retries=2;
SET enable_http_metadata_cache=true;
SET parquet_metadata_cache=true;
```

全部用 `let _ =` 忽略失败（旧版 httpfs 不认新名字时不应影响启动）。

### 4.5 目录统计改为可维护的聚合

- 行数/求和/时间边界是一次性读出来的 `RowAgg`，之后由写路径**维护**，不再有 TTL：
  - `append` 在**释放写连接之前**把自己的行加进去；
  - `prune` 在仍持有写连接时 `recount` 一次并装回；
  - `flush_hot` / `flush_hot_when_idle` / `compact_local` / `archive_local` 是行数不变的
    搬迁，不碰聚合；服务图写入只动 `net_*`，也不碰。
- 关键顺序：`recount` 走 `write_conn()`（独占写连接），`append` 在放锁前 `add`。
  这样不存在"已提交但尚未计入聚合"的窗口，recount 不可能跨过它而漏掉一行。
  （早一版用读连接算完再写缓存，正好丢掉这个窗口内的 batch，被
  `pruning_while_ingesting_keeps_the_index_consistent` 抓到，
  现在由 `a_poll_during_ingest_never_undercounts` 守着。）
- 读聚合只是拿锁 + clone；`total_bytes` 命中时用 `disk_bytes()` 刷新（一次目录遍历）。
- distinct series 没有精确增量形式，仍是扫描，但换成 10 分钟一档。
- `/api/v1/stats`、`/api/v1/tenants`、`/metrics` 全部改走 `stats_cached()`，
  请求路径不再做全表扫描。
- `bind()` 不再在启动时 `total_stats()`；改为 `serve_with_shutdown` 里的后台任务
  逐个 tenant 预热，不阻塞 liveness。

### 4.6 有界写等待

```rust
const WRITE_LOCK_WAIT: Duration = Duration::from_secs(30);
```

ingest（`append`、`append_servicemap`）改为 `write_conn()?`，拿不到写连接就返回
`Error::Busy`，让 agent 快速回压重试，而不是把 9–13 次重试全堆在队列里。

### 4.7 并发读下的暂存表名

`load_stacks` / `load_frames` 原来往一个固定名字的临时表里灌数据再 JOIN。
单连接时代这是安全的；并发读之后两个请求会互相 `DELETE`。改为原子递增的
`lidlesseye_stage_<n>`。

## 5. 验证

1. **单元测试**：`a_query_window_names_only_the_hours_it_needs`（1 小时窗口只列出
   1 个对象且不丢行）、`a_flush_files_rows_under_the_hour_they_describe`（小时后半段
   写入的行能按自己的小时查到）、`an_upgraded_legacy_file_stays_visible_beside_its_hour`
   （旧布局文件跨小时仍可见）、`several_reads_can_be_held_at_the_same_time`、
   `interrupt_stops_a_running_query_from_another_thread`。
2. **本地集群**：连续调 3 次 `/api/v1/stats`，第 2、3 次对 s3mock 的数据 GET/HEAD
   应接近 0。
3. `/api/v1/stats`、`/api/v1/tenants`、`/metrics` 的 p95 应 < 1 s。
4. 日志不再出现 `wait_ms > 5000` 的 `store_lock_wait`。
5. 把 s3mock 停掉再恢复，应在 15 秒内返回错误而不是挂死 4 小时。
6. **升级回归**：用 0.1.18 写出的数据目录（含 S3 对象，新旧两种命名）直接启动
   新版本，tenant 能打开、历史数据可查、不触发任何 DDL。

## 6. 未做与不做

- **不做 hive 分区目录迁移**：目录化解决的是"文件多"，实测文件多不是瓶颈。
  新写入用 `hour=<n>/` 前缀是因为它让分区信息来自**路径本身**，读取时不需要
  再猜文件属于哪个小时；旧对象仍按 key 名解析，不需要搬迁。
- **不做 schema 迁移**：本次没有任何表结构变化，DuckDB 里只有内存表和
  `.tmp` 暂存，升级只是替换镜像。
