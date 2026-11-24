# Parquet/S3 查询裁剪修复方案（评审稿）

> 状态：待评审。本文只描述问题、根因和候选方案，尚未修改任何代码。
> 目标：代码层根治 + 本地集群验证；按项目规则，合并前需完成升级回归测试并打 patch 版。

## 1. 现象

0.1.16 引入"内存热层 + Parquet/S3"后，在本地集群观察到：

- 打开 dashboard（尤其 timeseries 页）后，`/api/v1/tenants`、`/api/v1/stats`、
  `/api/v1/timeseries` 陆续从数百毫秒劣化到 20–60 秒超时；
- server 日志出现大量 `store_lock_wait`（`requester=write/read`，`wait_ms` 从几秒
  涨到 50+ 秒）与 `query_exec elapsed_ms=45000`（读连接 45 秒超时）；
- agent 侧随之出现 `report queue full` / `dropping reports after repeated upload
  failures`；
- 重启 server 只能短暂恢复，页面继续轮询后再次劣化。

## 2. 根因（经受控实验确认）

**与数据量无关，与时间范围无关，只与查询必须打开的 Parquet 文件数量有关。**

受控证据（同一 pod、同一时刻）：

| 测试 | S3 对象数 | 查询范围 | 耗时 |
|---|---|---|---|
| `globex` timeseries | 2 | 8 天 | 0.48 s，200 |
| `default` timeseries | 84 | 1 小时 | 20 s+，超时 |
| `default` timeseries（停写入、刚重启） | 84 | 1 小时 | 1.1 s，200 |

`globex` 查 8 天比 `default` 查 1 小时快 40 倍，且两者数据量相差 45 倍——
排除了数据量和时间范围的解释。

机制：

1. `archive_local` 把每个 epoch hour 归档成
   `s3://<bucket>/<prefix>/<tenant>/<table>/hour=<n>.parquet`。
   `hour=<n>` 位于**对象名**中，不是 `hour=<n>/...` 形式的目录层级。
2. `refresh_views` 据此构造静态 VIEW：

   ```sql
   CREATE OR REPLACE VIEW batches AS
     SELECT * FROM batches_hot
     UNION ALL BY NAME
     SELECT * FROM read_parquet(
       ['s3://.../hour=498755.parquet', ... 全部对象 ...],
       hive_partitioning=false, union_by_name=true
     )
   ```

3. DuckDB 绑定 `read_parquet([...])` 时，为确定 schema（尤其
   `union_by_name=true`）会打开列表中**每一个**对象的 footer。对象在 S3 上，
   每次 footer 读取都是一次 httpfs HTTP 请求。
4. 因此 `WHERE start_ns < ? AND end_ns > ?` 无论多窄，都无法在 bind 阶段跳过
   任何文件；zone map 只能跳过**行组**，省不掉 footer 的远程 I/O。
5. 文件越多，单次查询的固定成本越高。`default/batches` 有 84 个 S3 对象，
   全部 tenant 合计 591 个；页面多个控件并发查询时，这些固定成本叠加，
   单个连接被依次占用到 45 秒超时，写请求排队，形成级联劣化。

`docs/architecture.md` 中"Binding a query therefore never contacts S3"的假设
不成立：**不 LIST 桶 ≠ 不访问 S3**，显式列表只是把 LIST 换成了逐对象 footer
读取。

## 3. 失败过的思路（不要重试）

- **换个 S3 mock / 支持 multipart**：已解决上传侧问题，但与查询侧 footer 数量无关。
- **调 `--query-concurrency` / 读槽位**：只是把排队换个地方，固定 I/O 成本不变。
- **加大 `READ_LOCK_WAIT` / HTTP 超时**：让卡死更久，不减少工作。
- **`SET max_filesize`/单 PUT 上传选项**：httpfs v2.0.0 无对应能力，0.1.18 已撤销。

## 4. 候选方案

### 方案 A — 把 hour 提升为 hive 分区目录（推荐）

把归档对象键从

```
<base>/<tenant>/<table>/hour=<n>.parquet
```

改为

```
<base>/<tenant>/<table>/hour=<n>/part-0.parquet
```

VIEW 改为按前缀 glob，并声明分区列：

```sql
CREATE OR REPLACE VIEW batches AS
  SELECT * FROM batches_hot
  UNION ALL BY NAME
  SELECT * FROM read_parquet(
    '<base>/<tenant>/batches/**/*.parquet',
    hive_partitioning=true,
    union_by_name=true,
    filename=false
  )
```

DuckDB 把 `hour` 暴露为分区列后，`WHERE start_ns < ? AND end_ns > ?`
仍不能直接裁 hour（谓词在 `start_ns` 上），需要**额外**把 hour 谓词下推：

```sql
... WHERE hour >= <from_hour> AND hour <= <until_hour>
```

由于 VIEW 是静态的，这条谓词要由查询层统一追加。两种落地方式：

- A1：`hour` 列进入 VIEW 的 SELECT 列表，所有查询的 SQL 生成处统一加
  `hour` 范围条件（改动面：query.rs / store.rs 的所有 VIEW 查询）；
- A2：用一个 table function / 宏接受 from/until 参数，按窗口生成文件列表。

优点：DuckDB 标准做法；一次迁移后长期受益；S3 LIST 由 glob 承担（启动时
已有 best-effort glob，运行时 bind 会 LIST，但只列目录不读 footer）。

缺点：

- 需要重写已有 591 个对象的 key（S3 不支持 rename，需 COPY + DELETE）；
- `hour` 列与现有 schema 合并要处理旧对象没有分区列的情况；
- 查询 SQL 改动面较大，需要覆盖 flamegraph / diff / top / timeseries /
  series / label-values / servicemap / histogram / stats 全部路径。

### 方案 B — 在 VIEW 文件列表生成时按"当前小时"裁剪，不按时窗裁剪

不现实：VIEW 是全局静态对象，无法按单个查询的时间窗变化。仅按"最近 N 小时"
裁剪会改变语义（历史查询失效），排除。

### 方案 C — 查询结果侧加缓存，降低重复查询的固定成本

对 `stats`/`series` 已有 30 s 缓存；对 timeseries 按 (tenant, type, from,
until, selector) 缓存。能显著缓解 dashboard 轮询，但：

- 首次查询仍要打开全部 footer；
- 缓存键随拖动时间栏爆炸；
- 不解决根因，只掩盖症状。

可作为 A 的补充，不单独采用。

### 方案 D — 每个 hour 一个独立 VIEW / 分区表，查询按小时 UNION 对应视图

按 hour 建子 VIEW（`batches_h498755` …），查询层根据窗口拼出需要的 hour
列表再 UNION。等价于手写分区裁剪。

优点：不动 S3 key，无数据迁移。
缺点：VIEW 数量随小时线性增长；每次归档都要 DDL；查询 SQL 生成复杂且
难以覆盖所有路径；仍需在 bind 时打开所选 hour 的 footer（但数量从 84 降到
窗口内小时数）。

## 5. 推荐组合

**方案 A（A1 变体）+ 方案 C 的 stats 缓存增强**：

1. 归档键改为 hour 目录分区；
2. VIEW 用 glob + `hive_partitioning=true`，`hour` 作为分区列暴露；
3. 在 store 层提供一个统一的"带 hour 裁剪"的查询入口，所有读路径复用，
   避免遗漏；
4. 保留并延长 stats 缓存，减少 `tenants`/`stats` 的重复扫描。

## 6. 迁移与升级兼容（强制要求）

按项目规则，不能只验证全新安装。必须覆盖：

1. **旧对象在线迁移**：server 启动时发现旧式 `hour=<n>.parquet` 对象，
   逐个 COPY 到 `hour=<n>/part-0.parquet`，校验行数一致后删除旧对象；
   迁移期间新旧 VIEW 并存，保证查询不中断。
2. **manifest 兼容**：`manifest.json` 同时接受新旧两种 key；迁移完成后
   重写 manifest；进程崩溃后可从头重放（COPY 是幂等的，按目标 key 判存）。
3. **回滚**：迁移不删除源对象直到目标校验通过；回滚时切回旧 VIEW 定义
   即可读到旧对象（保留一个可配置的宽限期，例如 24 h 后才清理旧 key）。
4. **升级回归测试**：用 0.1.18 真实产出的库 + S3 数据启动新版本，验证：
   - tenant 能打开、迁移完成；
   - 迁移前后查询行数一致；
   - 窄/宽范围查询均返回且耗时与文件数无关；
   - agent 持续写入的同时页面轮询不再劣化；
   - 中断迁移后重启能续跑。
5. **schema/DDL 检查**：`CREATE OR REPLACE VIEW` 会改动依赖对象；确认没有
   索引/外键依赖 VIEW，且最新 schema 时启动时不做多余 DDL，避免多连接争 WAL。

## 7. 验证计划（本地集群）

1. 部署改版镜像到本地 `lidlesseye` 命名空间，保留现有 S3 数据；
2. 观察启动日志：旧对象迁移完成、VIEW 重建、无 `Dependency Error`；
3. 对比迁移前后：
   - `default/batches` 查询耗时（目标：<2 s，且与窗口宽度无关）；
   - S3 对象访问次数（目标：只访问窗口内 hour 的分区）；
   - agent `report queue full` / upload failure 消失；
4. 压力验证：页面 tabs + 多租户轮询 10 分钟，`store_lock_wait` 回到百毫秒级。

## 8. 版本计划

- 修复走 patch 版本：`0.1.18` → `0.1.19`；
- 同步更新 `Cargo.toml`、`Cargo.lock`、各 crate 版本、
  `deploy/helm/lidlesseye/Chart.yaml`、`web/package.json`；
- 更新 `docs/architecture.md`（修正"不访问 S3"的错误描述）、
  `docs/deployment.md`（S3 key 格式变更与迁移说明）、`CHANGELOG.md`；
- 本地验证通过后打 tag `v0.1.19`，推 GitHub，CI/Release/镜像按既有流程；
- 按规则补齐 Docker Hub 镜像（若 secrets 未配置则手动 push）。

## 9. 需要评审确认的问题

1. 是否接受 S3 key 格式变更（涉及 591 个对象重写）？
2. 迁移宽限期取 24 h 是否合适？
3. `hour` 谓词下推采用 A1（改所有查询入口）还是 A2（参数化 table function）？
4. 是否需要同时把 `manifest.json` 升级为带版本的格式？
