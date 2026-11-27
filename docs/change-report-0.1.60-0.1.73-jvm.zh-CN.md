# JVM 堆读取器与按 workload 开关（0.1.60–0.1.73）

## 1. 为什么不是把探针挂到 malloc 上

分配器探针（uprobe on `malloc`/`free`）在集群里实测每次 64 字节分配要 79 µs 系统时间，本地容器只要 50 ns——差 1600 倍，`--alloc` 因此改成默认关闭。要拿到内存画像而不碰目标进程，只能让运行时自己报：

| 运行时 | 机制 | 是否需要改业务代码 |
|---|---|---|
| Go | `runtime.mbuckets` | 否（已实现） |
| JVM | JFR `jdk.ObjectAllocationSample` / `jdk.OldObjectSample` | 否（本次实现） |
| Python / Node / Ruby / Rust-C++ | tracemalloc / inspector / … | 需要改启动方式并重启 |

## 2. 读取器怎么工作

`crates/lidlesseye-ebpf/src/jvmheap.rs`：

1. 扫 `/proc/*/comm`，`java` 即为候选。
2. 通过 `/proc/<pid>/cgroup` 拿到 pod UID，查 pod 索引得到 `namespace/workload`。
3. **只有被开关打开的 workload 才 attach**；其余进程连 `jcmd` 都不调用。
4. `nsenter -t <pid> -m -p -i -u -- <jcmd> <inner-pid> JFR.start name=lidlesseye settings=/tmp/lidlesseye.jfc disk=false`：
   设置文件写在目标容器的 `/tmp`，因为 attach 握手用的 unix socket 由**附加方**的 mount namespace 解析。
5. 每个采集周期 `JFR.dump` → `jfr print --json --stack-depth 24 --events jdk.ObjectAllocationSample,jdk.ObjectOldObjectSample` → 解析 → `JFR.stop` → 删 dump。
6. 报告带 `profile_source=jvm-jfr`、`runtime=java`、`sampling=runtime`，指标 `alloc_space` / `inuse_space`。

踩过的五个坑（都在代码注释里写了原因）：

- `jcmd` 的参数顺序是 `jcmd <pid> <command>`，写成 `jcmd <command> <pid>` 会得到 `Could not find any processes matching`。
- 同名录制会累积：旧 agent 留下的 `name=lidlesseye` 录制会一起跑，dump 里是它们之和。现在 `start` 前先 `stop_all`（最多 16 次）。
- 设置文件在目标的 `/tmp`，跨 agent 版本存活；"存在就跳过"会永远用第一版参数。现在每次 attach 都重写。
- 388 MB 的 JSON 会把 agent 打死。`MAX_DUMP_JSON = 24 MiB` 超限直接放弃这一轮，`--stack-depth` 降到 24，`throttle` 降到 5/s。
- 第一轮只 `JFR.start`、没有样本；把它当成"进程不存在"会让每个周期都重启录制。现在先记为 alive。

## 3. 数值口径（重要）

JFR 的 `weight` 是**估算**：它代表该线程自上次采样以来的分配量。实测单条事件可达 7 TB（`eventThread = "JFR Periodic Tasks"` 这类记账样本已丢弃，逐事件再加 2 GiB 上限），因此：

- `alloc_space` 只能做**相对比较**（哪条栈占得多）。
- `inuse_space` 是对象大小，可靠，但 `jdk.OldObjectSample` 只在老年代采样，短命对象的进程采不到。
- Go 的 `runtime.mbuckets` 同理，也是采样估算。

要给用户看绝对值之前，必须先换成"采样次数 × 已知步长"之类的口径，现在不行。

## 4. 按 workload 开关

服务端：

- `crates/lidlesseye-server/src/directives.rs`：`Directives`，存 `<data-dir>/profiling-directives.json`（`{"version":1,"workloads":["ns/name"]}`），写临时文件后 rename。
- `GET /api/v1/profiling/directives` → `{"workloads":[...]}`。
- `PUT /api/v1/profiling/directives`，body `{"workload":"ns/name","enabled":true}` → 返回更新后的列表。
- 默认空集，即**默认不读任何 JVM**。

agent（`crates/lidlesseye/src/main.rs`）：每 30 秒拉一次，写进 `CollectorConfig.jvm_workloads`（`Arc<RwLock<HashMap<String,bool>>>`），collector 每次采集时用它过滤；`--no-jvm` 仍可整体关闭。

前端（`web/components/views/service-map/node-panel.tsx`）：节点抽屉顶部新增 `Memory` 按钮。关闭状态下点击弹出确认框，写明：

- 镜像必须是 JDK（`jcmd`/`jfr`），JRE 会被静默跳过；
- 每个采集周期 JVM 会短暂停顿来 dump 与读取，有数 MB 抖动；
- 分配字节来自 JVM 自身采样，只能比较栈、不要相信绝对值；
- 这个开关只对 Java 生效。

确认后 `PUT` 打开；已打开时按钮显示 `Memory: on`，再点一次直接关闭（关闭无风险，不弹框）。

## 5. 验证

- 服务端：`GET` 空 → `PUT disk/jvm-churn true` → `GET` 返回该 workload → 文件落盘 `profiling-directives.json`。
- agent：开关打开前 `jvm scan found=1 not_requested=1`，打开后 `alive=1 samples=10`。
- 前端：节点抽屉 `Memory` 按钮 + 确认框渲染正常，文案如上。
- 上集群的镜像：`xsj321/lidlesseye:0.1.73-perf`；`default` 租户 1 小时火焰图 15 ms 级（0.1.59 起）。

## 6. 还没做的

- 非 Java 运行时（Python/Node/Ruby/Rust）需要各自的读取器；开关的数据结构留了按 runtime 扩展的位置（现在是 `Vec<String>`，将来是 `Vec<(workload, runtime)>`）。
- `alloc_space` 的口径换成可比较的量。
- 开关生效延迟最多 30 秒（agent 轮询间隔），UI 上没有显示"等待 agent 应用"的状态。
