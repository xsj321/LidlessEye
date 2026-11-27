# LidlessEye 产品化 Sprint 计划

本文把路线图（M1–M5）拆成可独立验收的 Sprint。每个 Sprint 的完成定义相同：

1. 代码合入工作树，`tsc`/编译通过；
2. 构建 `xsj321/lidlesseye:<新 tag>`（**每次必须换新 tag**，节点 containerd 会复用旧 digest）；
3. 部署到 `disk` 命名空间并 rollout 成功；
4. 用真实数据实测，数字写进 `docs/change-report-*.zh-CN.md`；
5. 不回归既有基线（见 §2）。

---

## 1. 当前基线（回归红线）

| 项 | 基线 |
|---|---|
| flamegraph 1h | 15 ms |
| flamegraph 6h | 28 ms |
| flamegraph 全部（10h） | 67 ms |
| top 1h | 15 ms |
| servicemap 1h | 16 ms |
| timeseries 1h | 9 ms |
| `flamegraph.total` == `top.total_value` == `Σ self_value` | 必须逐位相等 |
| CPU 采样开销 | ~1% |
| Native Hook | 230 ns / malloc+free（40× 基线） |

任何 Sprint 造成上述任一项退化超过 20%，视为该 Sprint 未完成。

---

## 2. Sprint 列表

### Sprint 1 — 数据契约与 alloc_space 语义（A01）
**产出**：`temporality`（delta/cumulative/gauge）与 `unit` 成为一等字段；Go/JVM 自报的累计量打 `temporality=cumulative`，注入读取器打 `delta`；查询侧按 temporality 选择聚合（不再硬编码 profile_type 列表）；UI 在数值旁标注"区间增量 / 进程启动至今 / 快照"。
**验收**：同一进程同一窗口，累计与增量分开显示；进程重启后新序列不产生负数；既有报告查询无回归。

### Sprint 2 — Runtime Capability Matrix（A04）
**产出**：`GET /api/v1/capabilities?workload=` 返回每种采集能力的 `supported/eligible/enabled/attaching/active/degraded/unsupported` + `reason`（`unsupported_libc`、`static_binary`、`missing_runtime_tool`、`permission_denied`、`no_target_process`、`attach_failed`、`no_samples`、`runtime_dead`）。agent 每次探测上报，服务端聚合。
**验收**：关闭/开启开关，状态在 `disabled → enabled → attaching → active` 之间正确迁移；不支持的镜像给出明确 reason。

### Sprint 3 — 全局 Coverage（A02）
**产出**：Coverage 四层聚合（workload → pod → container → process），字段 `expected/discovered/eligible/enabled/active/sampled/unsupported/failed`；API 返回多层结构；UI 显示 `2/3 Pods · 3/6 Processes` 并可下钻；带 `last_seen`。
**验收**：多副本 workload 显示 Pod 维度比例；Pod 重建后按 UID 识别新实例。

### Sprint 4 — 进程级内存与 Top-N（A03）
**产出**：`GET /api/v1/memory/processes`（namespace/workload/pod/range/sort/top）；采集端按 `VmRSS` 初筛，对 Top-N 读 `smaps_rollup`；字段含 `pid`、`process_start_time`、`comm`、`executable`、`rss/pss/private_dirty/swap`；读不到时给缺失原因而不是 0。
**验收**：能回答"这个 Pod 里哪个进程吃内存最多"。

### Sprint 5 — Memory 页三层重构（A12 前半）
**产出**：Overview（集群/命名空间/workload/pod 内存 + 趋势 + coverage 摘要）→ Process Breakdown（Top-N 列表，可点）→ Process Details（RSS/PSS 时间线、可用的 profile 类型、跳转到火焰图）。
**验收**：从 Memory 页两次点击可到具体进程的 allocation 火焰图。

### Sprint 6 — Memory Series 持久化（A06）
**产出**：`process_rss`/`process_pss`/`cgroup_memory` 作为 gauge 写入现有 series 存储；查询侧按 gauge 聚合（每桶取 last）；趋势跨服务端重启保留，保留期可配。
**验收**：重启服务端后趋势仍在；进程结束后历史仍可查。

**实现（0.1.158–0.1.159）**：趋势本身（`Coverage` 的 `MemoryPoint` 环形缓冲，480 点 ≈ 2 小时）落在 `<data-dir>/coverage.json`：
- `Coverage::with_path(path)` 启动时读回，`record()` 之后整表重写（先写 `coverage.json.tmp` 再 `rename`，崩溃最多丢掉最后一次重写，绝不会留下半截文件）；
- 文件带 `version: 1`，未来格式变了会被识别为"看不懂、忽略"，而不是被误读成空趋势；
- **只有写者允许重写该文件**（`set_persistent(lease.is_active())` 在租约 tick 里更新）：两个副本共享同一卷，standby 若拿自己那半份报告覆盖，会删掉写者积累的点。
- 进程结束后历史仍可查这一条**已补上（0.1.161）**：`memory_coverage_report` 在更新内存快照的同时，把三个读数写成 gauge 样本入 series 存储——`process_rss`、`process_pss`、`pod_cgroup_memory`，每条带 `service_name` 与 `temporality=gauge` 两个标签，栈名就是 gauge 名（这些读数没有调用链，空栈会被判为不可解析而丢掉）。查询走既有的 `/api/v1/timeseries`，数据落 Parquet，所以进程退出、服务重启都不影响历史。

**实测**：写者 `POST /api/v1/memory/coverage`（`process_rss_bytes=50000000`）→ 文件 `346 B` 落在两个副本共享的卷上；`kubectl delete pod` 同时删掉**两个**副本，新副本起来后连续 4 次 `GET /api/v1/memory/coverage` 全部返回 `50000000` ⇒ 趋势跨重启存活、两个副本读出同一份。

gauge 入库实测（0.1.161，shim 每 15 s 上报）：`/api/v1/series` 出现三条 —— `pod_cgroup_memory|disk/le-alloc-target|4 batches`、`process_rss|…|4`、`process_pss|…|4`；`/api/v1/timeseries?type=process_pss&range=15m` 返回 60 个 60 s 桶、其中 2 个非零（正好是刚上报的两分钟），标签为 `{service_name=disk/le-alloc-target, temporality=gauge}`。

**两个遗留缺陷已修（0.1.162–0.1.167）**

1. **gauge 的 `total_value` 变成"最后一个值"**。`series_inner` 的 `arg_max` 分支本来就是用 `instr(labels,'temporality=gauge')` 判的，但漏在条件里列出的类型名，而只靠标签命中的路径在当时的行上不稳；现在把三个 gauge 名直接写进 `profile_type IN (…)` 白名单（`store.rs`），标签判定保留。证据：同一 workload 连报两次 `process_rss=3000` 与 `7000` → `/series` 返回 `batches=2 total=7000`（求和会是 10000），`pod_cgroup_memory` 9000→12000 时 total 也是 12000；`alloc_space` 这类 delta 仍是求和（1.24 GB / 368 批），语义没有被连带改坏。
2. **两副本不再各看一半**。capabilities 与 processes 也像 coverage 一样镜像到共享卷上的 `capabilities.json` / `processes.json`（`version: 1`，写 `.tmp` 再 `rename`）；`record()` 先合并文件再写回，`list()` / `top()` 在回答前重新读一次，所以哪半边收到上报都不影响两个副本的答案。写方向仍然收口到写者：`POST /api/v1/{capabilities,memory/coverage,memory/processes}` 在 standby 上先 `307` 到写者的同样的 path（保留 query），拿不到写者地址才回 503。读方向不重定向——浏览器跨源跟随 307 会撞 CORS，让 standby 直接读共享文件更稳。
   证据：对写者 `POST` 返回 200、对 standby 返回 307；随后两个副本 `GET /api/v1/memory/coverage`、`/capabilities`、`/memory/processes` 对同一 workload 返回完全相同的 `rss=3000 cgroup=9000`、`pods_active=1`、`processes=1 rss=999888`；`/series` 在两边给出同一组 `batches=223 total=11102304042`。

**顺带修的两处**：`process_rss` / `process_pss` / `pod_cgroup_memory` 的单位从默认 `nanoseconds` 改成 `bytes`（`ProfileType::unit()`，否则页面把 100 MB 显示成 "1.2 s"）；standby 的读通取日志从每次请求一条 INFO 降到启动时的单条（`load_quiet(path, false)`）。

**已知边界（记为限制）**：三个 JSON 文件由写者维护、standby 只读；写者被删时新写者会从文件接管（`set_persistent(true)` 时先 `reload()`），但两次 tick 之间同一 Pod 的两份报告可能被对方覆盖一次，下一轮上报即自愈。

### Sprint 7 — Native Runtime Manager（A05）
**产出**：把散落的 bpftime 逻辑收敛成状态机（DISABLED/CHECKING/CREATING/ATTACHING/ACTIVE/DEGRADED/UNSUPPORTED）；ACTIVE 需满足：目标 pid 存在且 start_time 一致、hits 中见该 pid、样本送达后端；容器退出后允许用新唯一名字重建（有次数上限），达到上限转 DEGRADED 并提示重建 Pod；注入审计日志（who/when/why）。
**验收**：杀掉 ephemeral 容器后能自动重建；连续失败后状态为 DEGRADED 而非静默。

### Sprint 8 — Agent 磁盘缓冲与幂等 ingest（A08）
**产出**：agent 发送失败落本地 spool（容量上限 + 淘汰日志）；每批带 `agent_id/session_id/sequence/batch_id/checksum`；服务端按 batch_id 幂等；只有收到持久化 ACK 才删 spool。
**验收**：断开服务端 60 s，恢复后数据补齐且不重复。

### Sprint 9 — 周期 fsck 与数据丢失根因（A07）
**产出**：lightweight 巡检（新文件 footer/行数/字典引用）+ full audit（CLI）；发现坏数据打标记与计数；产出 9.5 小时丢失事件的根因报告（含证据与排除项）。
**验收**：注入一个坏 footer 能被检出；报告落到 `docs/incidents/`。**状态：完成（0.1.143）** —— `crates/lidlesseye-server/src/fsck.rs`，每小时跑一次，也可 `POST /api/v1/fsck` 立即跑；报告在 `docs/incidents/2026-11-25-data-loss.zh-CN.md`。实测：128 文件 / 54 小时 / 9.0 MB / 1 ms；放入合法探针后删除，`vanished` 精确列出路径；截断文件记入 `unreadable`。仍缺：把 `vanished` 接入指标（Sprint 16）。

### Sprint 10 — Rollup 与查询路由（A09 前半）
**产出**：1 分钟与 5 分钟两级 rollup 表；查询按窗口自动选精度；flamegraph/top 必须同精度同窗口，保持总量一致。
**验收**：24h/7d 窗口延迟显著下降且总量与原始数据一致（±0.1%）。

**实测（0.1.143，`disk` 租户 54 小时 / 128 文件）**：先测再建，结论与计划假设相反。

| 端点 | 1h | 24h | 7d |
|---|---|---|---|
| flamegraph | 51 ms | 38 ms | 31 ms |
| top | 35 ms | 45 ms | 32 ms |
| timeseries | 40 ms | 29 ms | 28 ms |

长窗口**不比一小时慢**：Parquet 按 `hour=` 分区裁剪，查询只打开窗口覆盖的那几小时的文件，成本随"窗口内小时数 × 每小时文件数"增长，而当前每小时只有 1–2 个文件。**结论：现在建 rollup 没有可测的收益，属于为想象中的负载写代码。** 推迟到出现真实数据规模时（判定门槛见下），并把资源转去做真正有证据的项。

**触发 rollup 的量化门槛**（任一条成立再做）：
1. 某端点 p95 在 7d 窗口超过 500 ms（当前 31 ms，差 16 倍）；
2. 单租户小时文件数平均 > 6（当前 54 小时 / 128 文件 ≈ 2.4）；
3. 单次 flamegraph 需要打开的 Parquet 文件数 > 500。

到那时 rollup 的设计已经写在这里：1 分钟表 `(bucket_ns, profile_type, service_name, labels, stack_id, value, sample_count)`，由 flush 时从 samples 聚合产生，查询按窗口对齐情况在 `samples` 与 `rollup_1m` 之间路由，总量必须由同一窗口同一精度算出（flamegraph 与 top 共用一次聚合）。

### Sprint 11 — 查询缓存与租户配额（A09 后半）
**产出**：结果缓存（key 含 tenant/type/window/labels/allocator/mode/水位）；水位推进只失效受影响范围；每租户并发与 CPU 预算。
**验收**：仪表盘重复查询命中缓存；高并发下写入不被阻塞。

**已实现（0.1.145）——缓存部分，按"先证明负载形状"的做法砍到了最小**
- `crates/lidlesseye-server/src/cache.rs`：包住 `/api/v1/{flamegraph,top,timeseries,servicemap}` 的 GET，TTL **10 s**、上限 512 条、单条 ≤ 8 MiB、按 `tenant + path + query` 做 key（租户是答案身份的一部分，两个租户问同一窗口绝不能互相看到）。响应带 `x-lidlesseye-cache: hit|miss`；`?fresh=1` 显式绕过。命中计数在 `GET /api/v1/cache`。
- **刻意不做水位失效**：仪表盘的真实负载不是"窗口在变"，而是同一窗口被 4–6 个面板在同一秒问 4–6 次；10 s TTL 已经覆盖这个形状，而水位失效在每秒都有写入的场景里会让命中率接近零。代价是答案最多旧 10 s，因此响应头必须自报 hit/miss —— 缓存没人能看见，就没人能相信。
- 实测（7d 窗口，同机 curl 含进程启动开销）：`servicemap` 冷 60.8 ms → 30.2 ms；`top` 44.9 → 14.0 ms；`timeseries` 28.0 → 13.9 ms。首次请求 `miss`、后两次 `hit`，`/api/v1/cache` 报 `{"entries":1,"hits":2,"misses":1,"ttl_seconds":10}`。
- **未做**：每租户并发与 CPU 预算。理由：当前没有证据显示读路径会饿死写入（`read_slot` 已经是信号量），加配额属于给想象中的争用写代码；等 `/metrics` 出现读排队证据再做。

### Sprint 12 — S3 归档（A10，方案 A）
**产出**：本地热 + 小时归档到 S3（对象不可变、上传后校验、manifest 记录），本地 LRU 淘汰前确认对象与 manifest 都在；S3 不可用时本地写继续、到阈值进入保护状态。
**验收**：删掉本地文件后能仅从 S3 查回同一小时数据。

**验收已完成（0.1.146，本地 docker + adobe/s3mock + `--storage-mode s3 --s3-url-style path`）**
- 归档：`--parquet-local-ttl 5s` 下注入一批 folded 数据，`parquet compaction ... uploaded=4`，桶里出现 `lidlesseye/default/{batches,frames,samples,stacks}/hour=498812-part-*.parquet` 与 `.archive-probe`，**本地活文件数 4 → 0**（`.trash` 里仍有回收副本，那是本地保留策略，不是活文件）。
- 读路径真的走 S3：停掉 s3mock 后同一窗口的查询返回
  `{"error":"database error: IO Error: Could not connect to server error for HTTP HEAD to 'http://host.docker.internal:9090/le/lidlesseye/default/samples/hour%3D498812-part-….parquet'"`
  —— 报错里带着 `s3://` 对象对应的 HTTP 请求，这比"能查到数"更能证明读的是远端；重启服务后 `discovered archived S3 objects objects=4`，同一窗口仍能查回数据。
- S3 不可用时写入继续：`parquet archive target is not writable; pausing archival retry_in_secs=600`，同一期间 ingest 仍返回 `{"accepted":1,...,"queued":true}`，本地照常落盘。
- 顺带修掉一个真 bug：folded 上传没带 `from`/`until` 时，`params.range` 退回**查询默认值（最近一小时）**，于是每批样本都被记到一小时前，仪表盘 5 分钟窗口看不到（数据在盘上却"消失"）。改为不指定窗口时记 `[now-1s, now]`；实测 5 分钟窗口注入后立即 `total=100`。
- **未做**：manifest 文件（当前以 LIST 结果为准重建远端索引，等价于把桶本身当 manifest）与"到阈值进入保护状态"的显式状态机（当前是暂停归档 + 本地继续写，没有硬阈值）。两者都不影响上面的验收，留待 Sprint 13 与 Active/Standby 一起做。

### Sprint 13 — Active/Standby（A11 第一阶段）
**产出**：Lease 选主 + fencing token；Standby 不写；切换后 WAL/归档可恢复；文档写明 RPO/RTO。
**验收**：杀掉 leader，standby 接管，无双写。

**验收已完成（0.1.147–0.1.149）**。不引入 etcd：两个副本本来就共享同一个 PVC，租约就写在 `<data_dir>/active.lease`（`{holder, fence, expires_ns, addr}`，`TTL = 10 s`、`TICK = 3 s`，临时文件 + rename 写入）。

| 观测 | 结果 |
|---|---|
| 两副本同一 PVC 启动 | 一个 `active:true` 一个 `active:false`，租约文件同一份，`fence` 相同 |
| standby 的 `POST /ingest` | `307 Temporary Redirect` → `Location: http://<active-pod-ip>:4040/ingest?…`（查询参数保持原样） |
| 杀掉持租约者（裸 pod 对，排除 Deployment 立刻补副本的干扰） | **12 s** 后另一个接管，`fence 4 → 5`，随后 `POST /ingest` 返回 `202` |
| 切换期间同一小时的 parquet 文件数 | 15 → 15（无双写） |
| 经 Service 写入 6 次 | 6 × `202`（standby 的 307 被客户端跟随） |
| 回归 | health/stats/profile_types/capabilities/role/cache 全 200，`flamegraph 1h total=5.26e12` |

- 关键设计：`fence` 只在易主时 +1（稳态续约不变），所以一个副本能证明"我从未是 writer"（fence 随读到的持有者同步）；写不进去时主动降级为只读，宁可停写也不要两个写者。
- 已知边界（写进文档）：Deployment 会立刻补副本，补出来的新 pod 可能先抢到租约——单写者性质不变，但不是"存活者接管"。要严格存活者接管需 StatefulSet + `podManagementPolicy: Parallel`；现在不做。
- RPO：standby 只读、不 flush，所以崩溃点最多丢一个 flush 周期（10 s）的内存样本（WAL 段在共享卷上，重启后重放）；RTO：`TTL + TICK ≈ 10–13 s`（实测 12 s）。

### Sprint 14 — 查询/写入分离（A11 第二阶段）
**产出**：查询前端 + 写入端分离部署（同一二进制不同角色），查询节点读 Parquet + 归档，不直接读未 flush 的热表（用短延迟缓冲兜）。

**验收已完成（0.1.150）**。`--read-only` 现在是一个真正的角色，而不只是"拒收写入"：

- `Lease::passive()`：只读副本**完全不碰租约文件**（不读、不写、不参与选主），`GET /api/v1/role` 返回 `{"passive":true,"active":false,"lease_file":""}`。
- 这是一个必须先修的洞：删掉写者之后，只读副本会立刻抢到租约并开始续期，然后把每个写请求 307 重定向到**自己**——一条永远送不进去的写入路径。
- 只读副本的 `POST /ingest` 返回 `403 {"error":"this replica is read-only; send writes to the writer deployment"}`（写者的 standby 仍是 307 到持有者，两者语义不同，别混）。

部署：新增 `le-disk-reader`（1 副本，同一 PVC，`--read-only`）+ `le-disk-lidlesseye-read` Service。

| 观测 | 结果 |
|---|---|
| 只读副本租约 | `passive:true, active:false, fence:0, lease_file:""` |
| 只读副本 `POST /ingest` | `403 this replica is read-only` |
| 只读副本读 1 h 火焰图 | `total=5.603e12 samples=1537`（同期写者 `5.638e12 / 1549`） |
| 杀掉写者后 | 新写者接管（`fence 7 → 8`），只读副本仍是 `fence:0, passive:true`，租约文件里的 holder 始终是 lidlesseye 写者 |

**延迟边界（写进文档，不掩饰）**：只读副本只读 Parquet，看不到写者内存里的热表，所以它的数据最多比写者旧一个 flush 周期（10 s）。上面 1537 vs 1549 的差就是这个窗口，不是丢数据。要更短的可见性延迟就把 `--flush-interval` 调小，代价是文件数变多（Sprint 10 的 rollup 触发门槛就是按文件数定的）。

**验收中抓到的静默错误（0.1.151 → 0.1.152 修掉）**：把只读副本接上之后，同一条查询在写者和只读副本上给出**互相矛盾**的答案——只读副本报 `vis-test total=11`，而写者报 `0`，连 `/api/v1/series` 里都没有这个服务。

根因不是只读副本，而是**所有非写副本**：Parquet 索引只在打开时扫描一次，写者靠每次 flush 顺手推进索引，而 standby 从不 flush，于是它的索引永远停在进程启动那一刻。两个副本在一个 Service 后面，读请求随机落到任意一个，同一个问题就会得到"有数据"和"没数据"两个答案——这是静默的错误答案，比慢更糟。

修法：所有**非持有者**副本每 5 s 重扫一次本地 Parquet 索引（`Parquet::refresh_local_index` → `Store::refresh_parquet_index`，任务是 `if lease.is_active() { continue; }` 的门控循环，因为角色会变）。

修完的实测（0.1.152，两个写者副本 + 一个只读副本，经 Service 连打 8 次）：
- `flamegraph 1h`：`5563000000022, 5563000000022, 5562000000022 ×6` —— 只差 0.02 %，差异就是活跃副本热表里还没 flush 的那 10 s。
- `vis-test`（前一版里写者看不到、只读副本能看到的那条）：8 次全部 `11`。
- 只读副本可见性延迟实测 **12.2 s**（flush 10 s + 重扫 5 s 的最坏情况）。

### Sprint 15 — Service Map ↔ Profiling 联动（A12 后半）
**产出**：从节点抽屉直接进入该 workload 的进程列表与火焰图；Compare 支持内存/分配对比。

**实现（0.1.156）**：`ProfilingTab` 尾部两个按钮 —— `Open full flamegraph view`（沿用当前 profile type）与 `Open allocation flamegraph`（固定 `alloc_space`），都走 `dashboardStore.showProfile(filters, type)`，窗口与 selector 一起带过去；新增 `ProcessList`，用 `/api/v1/memory/processes?workload=<ns>/<name>` 拿 Top-8 进程，展示 `RSS / PSS`，加载中、失败、无 reader 三种状态分别有明确文案。Compare 侧 `GET /api/v1/diff?type=alloc_space` 返回 200，分配对比可直接用。

**两个踩坑（都由联调暴露）**：
1. service 节点的 `workload` 是**空字符串**而不是 `undefined`，`node.workload ?? node.service` 会保留空串 ⇒ 进程列表恒显示"不是 workload"。必须用真值判断 `node.workload || node.service`。
2. 镜像里前端是 **rust-embed 编进二进制**的（`ui/` → build.rs），改了 `web/` 一定要重建镜像；只 `kubectl set image` 到旧 tag 不会更新。判定活着的不是旧前端：`curl` 拿首页 HTML 里的 `/_next/static/chunks/*.js` 逐个 grep 新文案。

### Sprint 16 — 自监控与运维（运营）
**产出**：agent/server 自身指标（ingest rate、WAL backlog、converter lag、flush 延迟、归档落后、查询延迟与队列、租户用量、字典缓存、fsck 失败、retention 状态）；`/metrics` 与 UI 运维页。

**实现（0.1.155）**：`/metrics` 在原有计数器之外新增十项 gauge —— `lidlesseye_replica_active`、`lidlesseye_lease_fence`、`lidlesseye_cache_entries/hits_total/misses_total`、`lidlesseye_fsck_findings`（unreadable + vanished + empty_hours 求和）、`lidlesseye_fsck_elapsed_seconds`、`lidlesseye_process_rss_bytes`、`lidlesseye_process_threads`。后两项读 `/proc/self/status`（`self_metric`），因为服务进程自身的占用此前完全没有暴露。实测：`replica_active 1`、`lease_fence 11`、`fsck_elapsed_seconds 0.100`、`process_rss_bytes 452263936`、`process_threads 132`。

**已知缺口（本轮实测发现，未修）**：一个**不产生样本的 pod**（例如只跑 `sleep` 的 `le-alloc-target`）不会进入 agent 的 `PodIndex`，于是 alloc 开关永远找不到它去注入读取器，日志每 30 s 重复 `no running pod for the workload workload=disk/le-alloc-target pods=12`——pod 明明在 Running，只是没有 CPU 样本。连带后果：`/api/v1/capabilities` 为空、`/api/v1/memory/processes` 返回 `pods:0`，Memory 页的进程列与 Service Map 抽屉的 Processes 区都只能显示"没有 reader 在上报"。修法应当是让 directive 轮询直接从 K8s API 列 pod（而不是依赖采样驱动的 PodIndex），列在 Sprint 7 的后续项里。

### Sprint 17 — 文档与交付
**产出**：安装/升级/故障恢复/backup-restore 文档；多租户安全测试；性能验收报告（P50/P95/P99）。

**实现（0.1.157）**：
- `docs/operations.md`：安装、升级（含"每次构建必须换新 tag，否则 pod 报新 tag 跑旧二进制"、容器名是 `server` 不是 `lidlesseye`、部署 Running 之后才能 `image prune`）、回滚、备份与恢复（备什么、怎么恢复、用 `POST /api/v1/fsck` 证明恢复成功）、健康与自监控指标、故障排查表（standby 503、只读 403、capabilities 空、两副本答案不一致、查询慢 10–100× 因为开了 malloc 探针、spool 堆积）、容量规划。
- `docs/perf-acceptance.md`：环境、方法（3 次预热丢弃 + 20 次测量 + `?fresh=1` 绕缓存）、p50/p95/p99/max 表、四条不变式（`flamegraph.total == top.total_value`、租户隔离、非法租户被拒、缓存不改变答案）、五条回归红线。
- 实测（0.1.157，`disk`，两副本 + 只读副本）：flamegraph 1h/6h/all p50 = 15.7 / 16.8 / 15.0 ms，top 1h 14.8 ms，servicemap 19.4 ms，timeseries 9.9 ms，memory coverage 9.9 ms，capabilities 3.7 ms；p99 最大 36.5 ms。
- 隔离测试：`X-Scope-OrgID: t1` 写入 folded 7 µs 后 t1 可见（`total=7`，与 `top.total_value=7` 相等），t2 完全看不到（`flamegraph` 0、`/series` 空）；`../../etc` 与 `a b` 均返回 400。
- 顺带修掉的运维缺口（0.1.157）：alloc 开关过去只从"采样驱动的 PodIndex"里找 pod，**空闲 pod 永远匹配不到**；现在 PodIndex 未命中时会回退到 `ClusterApi::list_all_pods()`，并给 agent 补了 `pods get/list/watch` 的 ClusterRole（此前 agent 在 `disk` 命名空间根本没有列 pod 的权限，回退会静默失败）。修复后 `le-alloc-target`（只跑 `sleep`）也能被注入读取器：`capabilities` = `active, 2/2 attached, 1/1 pod`，`memory/processes` 返回 2 行。

---

## 3. 执行规则

- **顺序**：S1 → S2 → S3 → S4 → S5 必须串行（互相依赖契约）；S6/S7/S8/S9 可交错；S10 依赖 S1；S12 依赖 S9；S13 依赖 S12。
- **镜像**：每个 Sprint 一个新 tag；构建后先验证二进制里确有新字符串（`grep -c`），再部署。
- **构建代理**：`--build-arg HTTPS_PROXY=http://host.docker.internal:6478`（WSL2 里 `127.0.0.1` 不是 Windows 回环）；基础镜像先逐个 `docker pull` 重试成功，再 `--pull=false` 构建。
- **清理纪律**：部署 Running 之后才允许 `docker image prune`；否则会删掉未部署的新镜像（已发生过一次，导致 25 分钟不可用）。
- **验证**：每个 Sprint 至少一条 `curl` 证据 + 一条 UI 截图证据 + 一条日志证据。
