# 0.1.74 – 0.1.89：把 malloc 读取器从内核 uprobe 换成 bpftime

## 结论先说

**注入链路通了，样本没回来。** 因此这个开关在 UI 上是隐藏的（服务端
`GET /api/v1/profiling/directives` 返回 `native_memory.supported = false`），
不会出现"打开了却什么都没有"的开关。JVM 那条（`Memory` 按钮）不受影响，照常可用。

## 为什么换

内核 uprobe 挂在 libc 的 `malloc`/`free` 上时，实测每次 64 字节分配要 **79 µs**
系统时间（同机本地容器 50 ns），agent 常驻期间按 workload 关闭的代价是整机的
glibc 分配都慢 1800 倍。bpftime 把同一件事放进目标进程内做，官方口径
uprobe 3224 ns → 315 ns。

用户的口径：允许为目标功能注入 `.so`，只限这一个功能，开关必须强调风险并要求确认。

## 做了什么

| 部件 | 位置 | 说明 |
|---|---|---|
| eBPF 程序 | `deploy/bpftime/alloc.bpf.c` | `uprobe/libc.so.6:malloc`，`key=(pid, 用户态栈)`，`{bytes,count}` |
| 用户态 loader | `deploy/bpftime/loader.c` | libbpf 加载 + `bpf_program__attach_uprobe_opts`，每 N 秒排空 map 打一行 JSON |
| 读取器 | `crates/lidlesseye-ebpf/src/bpftime.rs` | loader 子进程、注入、JSON 解析、`resolve_user` 符号化 |
| 接线 | `collector.rs` | 按 workload 开关过滤，产出 `alloc_space`，标签 `profile_source=bpftime-userspace` |
| 指令 | `crates/lidlesseye-server/src/directives.rs` | v2：`workloads` 变 `{workload: kind}`，`kind ∈ {jvm, alloc}` |
| API | `api.rs` | `GET/PUT /api/v1/profiling/directives`，新增 `native_memory.supported` |
| 前端 | `node-panel.tsx` | `Native` 按钮 + 独立确认框（四条风险），仅在 `supported` 为真时绘制 |
| 镜像 | `deploy/docker/Dockerfile` | 多阶段构建 bpftime；运行镜像改 `ubuntu:24.04`（bpftime 需要 glibc ≥ 2.38）+ `libncurses6 libtinfo6` |

## 一路上修掉的坑（都已提交）

1. `.dockerignore` 整目录排除 `deploy/`，`COPY deploy/bpftime` 直接报 not found；
   改成 `deploy/*` + `!deploy/bpftime`。
2. 运行镜像 bookworm（glibc 2.36）加载 bpftime 报 `GLIBC_2.38 not found`，换
   `ubuntu:24.04`；`docker pull` 要带代理 `127.0.0.1:6478`。
3. 缺 `libncurses6`/`libtinfo6` 时 `bpftime attach` 报 cannot open shared object。
4. **`bpftime attach` 静默失败**：CLI 默认把日志写 `~/.bpftime/runtime.log`，而 agent
   容器 `readOnlyRootFilesystem`，写日志失败后在注入前就退出。加
   `BPFTIME_LOG_OUTPUT=/dev/stderr` 才看得见，也才注入成功。
5. 缺 `libbpftime-agent-transformer.so`（真正改写入口的那块）。
6. 同名录制叠加：`JFR`/bpftime 的旧句柄不清理会叠加，已加幂等与去重。

## 为什么还没有样本

bpftime 的 loader（server）与注入到目标进程的 agent 靠一段 **POSIX 共享内存**通信，
落在 `/dev/shm`，而 Kubernetes 给每个 pod 挂独立的 `/dev/shm`。

| 尝试 | 结果 |
|---|---|
| 在 agent 里 `mount --bind /proc/<pid>/root/dev/shm /dev/shm` | 服务进程内 **EINVAL**；同一 pod 的 shell 手工执行同样命令却成功 |
| 先挂 tmpfs 再 bind | 仍 EINVAL |
| 把 loader + 全部依赖（12 个文件）拷进目标 pod 的 `/tmp/.lidlesseye`，用 `nsenter -t <pid> -m` 启动 | loader 起得来、`counting malloc` 打印、注入成功（`/proc/<pid>/maps` 有 agent），但 map 30 秒内一行数据都没有 |
| 同一组命令直接在目标 pod 内部跑（不走 nsenter） | **立刻出 JSON**（`{"pid":86,"bytes":4096,"count":1}`），退出时 segfault |

所以问题收敛到"经 nsenter 在目标 mount ns 里启动 loader"这条路径：pid namespace 不同、
`LD_PRELOAD` 的解析与 shm 段的发现都可能失效（日志里那两条
`cannot be preloaded … ignored`，其中一条来自 `nsenter`/`timeout` 自身，另一条是否
真的被忽略无法区分）。

## 下一步（按代价排序）

1. **不改代码的办法**：让目标 pod 与 agent 共享 `/dev/shm`（hostPath/tmpfs 同名挂载），
   需要改目标 workload 的 PodSpec，不适合平台侧单方面做。
2. **在 loader 里打印 map 查询结果**，确认是"没有样本"还是"shm 段没连上"，再决定
   是补 `bpf_map_lookup_elem` 的错误分支，还是换通信方式。
3. 让 bpftime 以 `unshare(CLONE_NEWPID)` + `settid` 的方式起 loader（等价于
   `nsenter -a`，但由我们自己 fork，不经过 `nsenter` 的 `proc/self` 解析）。
4. 若仍不通，退回"按 pid 注入 + 进程内自报"或只在目标 pod 内跑一个 sidecar。

## 复现命令

```bash
# 打开一个 workload 的原生读取器（服务端）
curl -X PUT -H 'Content-Type: application/json' \
  --data-binary '{"workload":"ns/name","enabled":true,"kind":"alloc"}' \
  http://127.0.0.1:4040/api/v1/profiling/directives

# agent 侧观察
kubectl -n disk logs ds/le-disk-lidlesseye-agent | grep -E 'bpftime|loader'
```

预期（当前版本）：`bpftime scan found=… allowed=… attached=… samples=0`。
样本回来后把 `api.rs` 里的 `native_memory.supported` 改成 `true`，前端按钮自动出现。

---

# 追加（0.1.90）：Allocator CPU 视图，零注入

在等 bpftime 数据通道的同时，先把"不注入、不 Hook"的能力做出来：CPU profile 里
本来就有每个进程的栈和叶子帧，allocator 的帧就是普通帧。所以过滤放在查询侧。

## 用法

```
GET /api/v1/flamegraph?type=cpu&range=1h&allocator=any              # 调用栈里出现 allocator 的样本
GET /api/v1/flamegraph?type=cpu&range=1h&allocator=any&mode=self    # 采样点正落在 allocator 内部
GET /api/v1/flamegraph?type=cpu&range=1h&allocator=glibc&mode=self
GET /api/v1/top?type=cpu&range=1h&allocator=any&limit=20
```

`allocator` 取值：`any` / `glibc` / `jemalloc` / `tcmalloc` / `mimalloc`。
`mode`：缺省 `inclusive`（栈里含 allocator 帧），`self`（叶子帧在 allocator 内）。

匹配规则：先看模块（`libc.so`、`jemalloc`、`tcmalloc`、`mimalloc`），再看符号；
glibc 额外匹配 `_int_malloc` / `_int_free` / `malloc_consolidate` 这些内部函数，
因为真正的时间花在那里，不在调用者看到的 `malloc` 上。`any` 另外包含
`libstdc++` 的 `operator new/delete`。

## 实测（`disk` ns，1 小时窗口，2026-11-26 16:2x）

| 查询 | total |
|---|---|
| CPU 全量 | 5 281 s |
| `allocator=any`（inclusive） | 43 s（**0.81 %**） |
| `allocator=any&mode=self` | 13 s（0.25 %） |
| `allocator=glibc&mode=self` | 11 s（0.21 %） |

含义：这个集群 1 小时里只有 0.8 % 的 CPU 花在分配器里，火焰图上看不出分配器瓶颈。
换句话说——在这个负载上，为 malloc 装 Hook 本身就是纯亏。这组数字也是"要不要开
L1"的判据。

## 还没做

- 前端没有开关，只能用 URL 参数（下一步在火焰图工具栏加一个 `Allocator` 复选）。
- `diff` / `timeseries` 未接入同一过滤。
- 数据是"allocator 的 CPU 时间"，不是"分配字节数"，两者不能互相替代。


## 0.1.91 L0 allocator UI

The allocator view was only reachable by hand-writing query parameters. 0.1.91 makes it a
first-class control on the flame graph, which is the view where a call path is on screen and
`self` vs `inclusive` actually means something.

### Controls

- Toolbar: an `Allocator` switch (off by default). Switching it on adds
  `allocator=any` and `mode=inclusive`; switching it off removes both parameters rather
  than sending `allocator=none`, so the server keeps treating "no parameter" as the
  unfiltered profile.
- `Family` select: `all allocators`, `glibc`, `jemalloc`, `tcmalloc`, `mimalloc`.
- `Mode` select: `Inclusive` (any frame on the path is inside the allocator) or
  `Self` (the leaf frame is).

### Share strip

With the filter on, the strip above the canvas states the number that decides whether a
native reader is worth its cost:

```
ALLOCATOR CPU  6.00s of 1h30m  0.11 %  (self)  Allocator is unlikely to be the bottleneck …
```

The total comes from one extra unfiltered query for the same window, issued only while the
filter is on, and the advice thresholds are 1 % and 5 %: below 1 % the allocator is
unlikely to be the bottleneck, above 5 % a native reader may pay off. The hint never
blocks the view — the filtered profile stays on screen.

### Verified on the cluster

`disk` namespace, one hour, three requests against the same window:

| view | total |
|---|---|
| unfiltered | 5 281 s |
| `allocator=any&mode=inclusive` | 29 s (0.53 %) |
| `allocator=any&mode=self` | 6 s (0.11 %) |

The filtered canvas shows only allocator frames (`libc_malloc`, `malloc`, `free`,
`operator new`, `_ZdIPv`), which is what the filter is supposed to prove.

### Not done

- No URL persistence: reloading the page drops the allocator selection (the parameters are
  still honoured by the API and can be bookmarked by hand).
- `top` accepts the same parameters but has no UI control yet.

## 0.1.92 跨 Pod bpftime PoC：结论（2026-11-26）

用户要求先修跨 Pod 路径、再进 PR3。结论：**未通过**，且失败点在"注入后的 agent 未激活"，不在 loader、map 或共享内存。

证据链（四阶段，逐层独立取证）：

| 阶段 | 观察 | 结论 |
|---|---|---|
| Attach | `bpftime attach` 在 `-m -p` / `-a` 下均返回 0 并打印 `Successfully injected. ID: 1`；目标 `/proc/<pid>/maps` 里没有稳定出现 `libbpftime-agent`；目标 RSS 由 1.5 MB 短暂升到 4.2 MB 后回落 | 注入动作执行了，但没有留下持久映射 |
| Callback | 给 BPF 程序加的 `hits` map 有计数，但 pid 全是 agent 命名空间里的辅助进程（`29126`/`29170` 之类），从来没有目标进程 | 回调链路本身通，目标从未进入 |
| Map | `allocs`/`stacks` 有数据，单轮 12 万条样本行 | map 更新正常 |
| Loader | loader 在目标 ns 内启动成功（`counting malloc` + `hits map fd 4`） | loader 正常 |
| IPC | `/dev/shm` 跨 pod 不共享已确认；用 `mount --bind /proc/<pid>/root/dev/shm /dev/shm` 把目标 shm 挂给 agent 后，两侧写入同一 tmpfs（已验证文件互见） | 这一步修掉后仍无目标样本 |

补充：只进 `-m` 时 `bpftime attach` 会报 `Failed to inject: Unable to find process with pid <host pid>`（命名空间内 pid 不同）；改用内层 pid + `-a` 后改为 `(frida) Frida-CRITICAL: Unable to locate the libc` 并 SIGABRT。

结论：bpftime 的 frida 式 attach 在我们"DaemonSet（hostPID）→ 目标 pod"的部署关系下无法真正插桩目标进程。可以排除的：BPF 程序编译、map 类型、loader 输出、共享内存隔离（均已单独验证可工作）。

下一步三个候选（按成本排序）：
1. **宿主级 bpftime daemon**：把 loader/server 跑在宿主命名空间（不注入时用内核 uprobe 模式），目标侧只保留 attach 客户端；与 bpftime 的 `BPFTIME_HOST_PROCFS` 设计一致。
2. **目标 pod 内的 ephemeral container**（`kubectl debug` 式），在目标 pod 自己的命名空间里跑 loader 与 attach，彻底绕开跨 pod 注入。
3. 保留现有内核 uprobe 读取器，配 per-TID 采样（1/64 时约 1.2 µs/次），作为"不注入"的兜底。

在 PoC 通过之前，UI 的 `Native` 开关保持隐藏（`native_memory.supported=false`），避免给用户一个打开后没有数据的按钮。

## 0.1.92 宿主级 daemon PoC（方案 1）结果：同样未通过，且找到更底层的原因

按用户给的停止条件执行，结论：**停在这条路径，转方案 2。**

四阶段实测（一个 glibc workload：`/tmp/victim`，宿主 pid 50086 / 容器内 pid 30）：

| 阶段 | 结果 |
|---|---|
| 1. Host discovery | 通过：`host_pid=50086 container_pid=30 starttime=1036711 exe=/tmp/victim libc=/usr/lib/x86_64-linux-gnu/libc.so.6` |
| 2. Frida injection | `attach rc=0` + `Successfully injected. ID: 1`；目标 RSS 1380 kB → 4192 kB 并保持；但 `/proc/50086/maps` 只有 43 行、没有任何 `bpftime`/`agent` 映射；1 s / 5 s / 30 s 三次检查都稳定在这个状态 |
| 3. Callback | 目标 PID 在 `hits` 中从未出现（用户要求的目标 PID 单独出现 = 未达成） |
| 4. Sample delivery | 目标 PID 的 `allocs`/`stacks` 为空；userspace 模式下 loader 一条样本都没有 |

补充实验（关键）：`BPFTIME_RUN_WITH_KERNEL=true`、不做任何注入，直接挂内核 uprobe —— loader 产出 20 581 条样本、9 个 pid，**没有一个是目标**（50086 / 30 都不在）。说明连内核 uprobe 路径也没命中目标。

原因（已定量确认，这是比 Frida 更底层的问题）：uprobe 是按「文件」挂的，而**同一个 libc 在不同 pod 里是不同文件**：

```
agent  rootfs libc: dev 2097197  ino 1133181
target rootfs libc: dev 2097269  ino 1133181
agent  pid 1 实际映射: 00:5e      ino 10970
```

inode 号相同、设备号不同 ⇒ 是三个不同的文件。宿主级 loader 用路径解析 libc，挂上的是「自己看到的那个文件」，目标进程执行的却是另一个 ⇒ 探针注册成功、永远不触发。只有「通过 `/proc/<pid>/root` 按目标自己的路径解析」才能命中目标实际映射的文件 —— 这正是我们 Rust 读取器一直在做的事（所以它有数据）。

## 决策

按用户定的停止条件（宿主级能找到目标但注入后映射立刻消失 / 仍失败 ⇒ 转方案 2）：

- **放弃**"DaemonSet + nsenter / 宿主级 daemon + Frida 注入"这条路线。
- **下一步做方案 2**：目标 Pod 内 ephemeral container（`kubectl debug` 式），让 loader 与注入器运行在**目标的 mount/pid 命名空间内**，路径解析与目标一致，`/dev/shm` 天然共享。
  - 需要的权限：`pods/ephemeralcontainers` 的 `create/patch`（当前 DaemonSet 没有）。
  - 若集群不给这个权限，则退到「用 /proc/<pid>/root 解析目标文件 + 现有 Rust 内核 uprobe 读取器 + per-TID 采样」，并接受约 1.2 µs/次的开销。
- 方案 3（内核 uprobe + 采样）按用户指示继续保留为兜底。

在 PoC 通过前，`Native` 开关保持隐藏（`native_memory.supported=false`）。

## 方案 2 验证：ephemeral container 可行（2026-11-26，最小 PoC 通过）

权限现状：

```
kubectl auth whoami                            -> kubernetes-admin（cluster-admin）
kubectl auth can-i create pods/ephemeralcontainers -n disk   -> yes
kubectl auth can-i create pods/ephemeralcontainers -n disk \
  --as=system:serviceaccount:disk:le-disk-lidlesseye         -> no
kubectl auth can-i patch pods -n disk --as=...le-disk-lidlesseye -> no
```

即：**集群管理员可创建，但 agent 的 ServiceAccount 不行**，要落地必须给 `le-disk-lidlesseye` 加一条最小 RBAC（`pods/ephemeralcontainers` 的 `create`+`patch`）。

关键前提实测（`kubectl debug --target=app`）：

| 检查 | 结果 |
|---|---|
| 是否共享目标容器 PID 命名空间 | **是**：从 debug 容器能看到 app 容器的 `pid 1 sleep 7200`、`pid 18 sleep 3000` |
| 是否共享 `/dev/shm` | **是**：app 容器里 `touch /dev/shm/marker`，debug 容器 `ls /dev/shm` 看得到 |
| libc 文件身份 | debug 容器看到 `2097271:1133181`，与 app 容器一致（同一 pod 的 mount 视图） |

端到端 PoC（一个 glibc workload `victim`，容器内 pid 74）：

```
lidlesseye-bpftime: counting malloc, interval 5s
attach rc=0  ->  Injecting to 74 / Successfully injected. ID: 1
{"pid":74,"bytes":1706786816,"count":416696,"stack":"0x5d77ecfb71aa"}
```

**目标 pid 74 的样本出现了，单轮 416 696 次 malloc 全部计数。** 对比：宿主级 daemon 与 nsenter 方案下同一目标 0 条样本。

结论：方案 2 成立。前置条件两条 —— ① 目标 Pod 能被 agent 注入 ephemeral container（需要 RBAC），② 集群运行时支持 `--target` 的 PID 共享（本集群支持）。

## 0.1.108 跨 Pod 原生内存读取器：端到端验收

方案：agent 发现某 workload 打开了 `kind=alloc`，就 PATCH 该 Pod 的 `ephemeralContainers`，加一个
`lidlesseye-alloc` 容器（`--target` 与目标共享 PID namespace、/dev/shm 和 libc 文件），容器里跑
`lidlesseye bpftime-shim`：起 bpftime loader、attach 目标进程、把样本解析成 folded、POST 回 `/ingest`。

四个真 bug（都已修）：

| 现象 | 根因 | 修法 |
|---|---|---|
| `attach` 打印 usage 后 exit 1 | ephemeral 容器没有 `HOME`，CLI 找不到 `/root/.bpftime` 的 .so | `--install-location` + `HOME=/root` |
| 上传"成功"但没数据 | `/ingest` 与 `/api/v1/ingest` 拼错，落到 UI 兜底路由返回 `200 text/html` | `ingest_url()` 统一到 `/ingest`，非 2xx 变硬错误 |
| 栈里是 `opendir`、`0x5dad…` | release profile `strip = "symbols"` | 改 `strip = "debuginfo"` |
| 服务名筛选查不到 | folded 只写 service_name 列，不写 label | 服务端把它镜像进 labels |
| 最近几分钟窗口查不到 | sidecar 镜像是旧的 0.1.102（env `LIDLESSEYE_SIDECAR_IMAGE` 未随 release 更新），旧 shim 不带 `from/until` | 更新 env 并重建 Pod |

验收结果（`disk/le-alloc-target`，0.1.108）：

- shim 每 15 s 一批：`tick received=39 stacks=9 unresolved=0` → `posted stacks=9 samples=39`；
- `flamegraph?type=alloc_space&selector=service_name="disk/le-alloc-target"` 在 **3 分钟窗口**返回
  `total=597659 samples=33`，帧是真实的 Rust 栈（`tokio::runtime::task::raw::poll`、`TenantStore::compaction_tick`）；
- agent rollout restart 后：`ephemeralContainers` 仍为 1（没有重复注入），shim 继续上报
  （`total=691881 samples=27`）。

已知限制：

- ephemeral 容器不能删除，关掉开关只能删 Pod；开关文案已写明；
- 目标镜像需要 `libelf.so.1` 且是动态链接的 glibc ≥ 2.38；
- `_int_malloc` 等 libc 内部帧只能落到最近导出符号（装 `libc6-dbg` 才精确）；
- 只测 `alloc_space`，`free` 未挂，没有存活堆数字。
## 0.1.109–0.1.110 PR3-B：按 TID 的泊松采样

`deploy/bpftime/alloc.bpf.c`：

- `SAMPLING_RATE 64`。采样判定是一次 `bpf_get_prandom_u32() & 63`：只花一次随机数，等价于固定概率的几何（泊松）抽样。用 `-ln(u)/p` 的精确算法要在 Hook 里做浮点，成本比它想省的栈展开还高。
- 63/64 的调用在取 size 之前就返回，不做 unwind、不做 map 查找——这正是"进 Hook 只做轻量工作"。
- 命中时 `bytes * SAMPLING_RATE`、`count + SAMPLING_RATE`，所以总量仍是无偏估计，而不是把数字变小 64 倍。
- 新增 `sampled` map（每 pid 累计），loader 的 `{"hits":…}` 行现在同时给出 `calls` 与 `samples`，可以直接看出实际采样比。

`deploy/bpftime/loader.c`：`drain_hits(hits_fd, sampled_fd)` 打印 `{"hits":true,"pid":N,"calls":M,"samples":S}`；启动日志变成 `counting malloc, interval Ns, sampling 1/64`。

`crates/lidlesseye/src/shim.rs` 修掉一个真缺陷：`attach_candidates()` 原来只在启动时跑一次，**shim 起来之后才启动的进程（重启的 workload、fork 出来的 worker）永远不被注入**。现在每个 tick 都重新扫一次 `/proc`，并跳过 `/proc/<pid>/maps` 里已有 `libbpftime-agent` 的 pid（比让 CLI 重复 attach 更便宜可靠）。

验证（`disk/le-alloc-target`，0.1.110）：

- `tick alloc_rows=3..4 hit_rows=3`，`tick received=10..13 stacks=3`：探针在跑、采样后的行数远少于未采样的 22 行；
- 新起的 churn 进程（shim 之后才启动）被注入并出样本，证明"每 tick 重扫"生效；
- `flamegraph?type=alloc_space&selector=service_name="disk/le-alloc-target"` 4 分钟窗口 `total=32561109440 samples=26`，栈是 `_start` / `__libc_start_main`。

已知限制：验证是结构性的（行数与上报链路），没有做采样前后 `alloc_space` 总量的统计等价性检验；`weight` 意义上的无偏只由 `bytes*64` 的代码路径保证。
## 0.1.111–0.1.112 PR3-C（传输对账）与 PR3-D（集成）

**PR3-C：不再"悄悄丢行"。**

- `deploy/bpftime/loader.c` 每个 tick 结束在 stdout 打一行 `{"batch":N,"rows":K}`，K 是本轮真正输出的行数。
- `crates/lidlesseye/src/shim.rs` 按批次对账：读到的行数不等于 K 就打 warn 并把差额累加进 `lost`，`tick` 日志现在带 `lost=` 与 `dropped=`。管道背压、读端被 kill 这类"看起来像空闲"的故障从此可见。
- 待上报的栈表加了上限 `MAX_PENDING_STACKS = 4096`：服务端不可达时丢新栈并计数，而不是一直涨到 OOM——OOM 会让整个读取器消失，比丢几个栈更糟。
- ingest URL 带上 `label=alloc.reader=bpftime-userspace&label=alloc.sampling=poisson-1%2F64`，这样"采样并放大过的数字"和数据来源在查询侧可见，能和未采样的计数区分开。

**PR3-D：`alloc_space` 成为一等公民。**

- `/api/v1/profile_types` 已经把 `alloc_space` 作为独立 profile type 返回（`unit: bytes`，batch_count 417）。
- 前端所有视图都走 `formatValue(value, unit)`，`unit === "bytes"` 走 `formatBytes`，所以火焰图/概览/热点表自动显示 GB/MB，不需要改视图。
- 火焰图工具栏的 `Allocator` 开关现在只在 `type === "cpu"` 时出现（`web/components/toolbar.tsx`）：allocator 占比条是 CPU 侧的概念，在 `alloc_space` 视图里没有意义。

验证（0.1.112）：

- 浏览器打开 `#/flamegraph?range=1h&type=alloc_space`：Profile 下拉里有 `alloc_space`，画布显示 `_start → __libc_start_main → main → perl_parse → perl_run → Perl_runops_standard → Perl_sv_...`；
- 按 label 过滤可用：`selector=alloc.reader="bpftime-userspace"` 6 分钟窗口 `total=10000192128 samples=10`；
- shim 日志 `tick received=6 ... lost=0 dropped=0`，与 `tick alloc_rows=6` 对得上。
## 0.1.113 PR3-E：三档开销实测（结论：Hook 本身贵，采样救不了它）

测量方法：`target\tmp\victim.c`（20 000 000 次 `malloc(64)`/`free`，写一个字节防止被优化掉），
在同一个 pod、同一个二进制上分别用三种 bpf 程序配置运行；每次运行前 `bpftime attach` 已经完成，
所以测到的是"已插桩"的稳态。采样率改为运行时可配（`cfg` array map，loader 第三个参数），
一次构建就能测多档。

| 配置 | ns / (malloc+free) | 相对基线 |
|---|---|---|
| 无读取器 | **5.7** | 1× |
| Hook-only（`rate=1048576`，永不取栈） | **193.7** | 34× |
| 采样 1/64（`rate=64`） | **230.1** | 40× |
| 每次调用都取栈（`rate=1`） | >10 分钟未跑完（>30 µs/次） | — |

结论与之前的估算相反，需要改口：

- **开销几乎全在 Hook 本身**：193.7 ns 的 pair 成本里，采样把它压到 230 ns（多了 36 ns 才是取栈），
  也就是说"1/64 采样 ≈ 58 ns"这个估算错了——它只算了取栈，没算 bpftime 每次调用进 trampoline 的固定成本。
- 一次 malloc ≈ 97 ns 的固定成本（193.7 / 2 次调用），是 315 ns 内核 uprobe 的三分之一，
  但仍然是无插桩（2.8 ns/call）的 34 倍。
- `rate=1` 在这个分配率下根本跑不完：20M 次分配 10 分钟没结束，说明全量取栈不可用。

因此 PR3-B 的采样策略仍然值得保留（它把**取栈**这块最大的可变量隔离掉了，且避免了 map 被打满），
但"1/64 就能把总成本降到几十 ns"是不成立的。下一步真正有效的方向是降低**每次调用**的成本：
bpftime 的 trampoline 是否可裁剪、能否只挂 `malloc` 而不挂 `free`（当前只挂 malloc）、
以及是否能用一个更轻的内联计数器替代 per-call 的用户态 eBPF 调用。
## 0.1.114 读取器自愈：loader 重启与 terminated 告警

`crates/lidlesseye/src/shim.rs`：loader 从"启动一次、死了就 `return Ok(())`"改成被监督的子进程。
`spawn_loader()` 抽出启动逻辑（先起 loader、sleep 1500 ms、再 attach；stdout 用线程转发进 channel），
`run()` 的外层循环在 channel 关闭时 `kill` + `wait` + 等 2 s 后重新启动，`restarts` 计数进 ``warn!(restarts)``。
原因很直接：**Kubernetes 不会重启 ephemeral 容器**，shim 一退出这个 pod 就永远没有读取器了，
而那正是这台机器 VM 崩过一次之后发生的事（`lidlesseye-alloc` exit 255，之后 40 分钟无数据）。

`crates/lidlesseye/src/k8s.rs` 的 `ensure_alloc_shim`：当一个 pod 的 spec 里已经有 `lidlesseye-alloc` 时，
不再静默返回，而是读 `status.ephemeralContainerStatuses[].state.terminated`，有则打
`the allocator shim in this pod exited and cannot be restarted; delete the pod to get a new one`
（带 exitCode 与 reason）。ephemeral 容器删不掉，所以"删 pod"是唯一恢复手段，至少要说出来。

验证（0.1.114，`disk/le-alloc-target`）：

- 在 ephemeral 容器里 `pkill -f lidlesseye-bpftime` 杀掉 loader，日志出现第二条
  `lidlesseye-bpftime: counting malloc, interval 30s, sampling 1/64`、重新 `injected pid`；
- 随后在 app 容器起 churn 负载，`tick received=9 stacks=3` → `posted stacks=3 samples=9`，数据继续入库；
- 新起的进程仍然被每 tick 的 `/proc` 重扫捕获（`injected pid 65`）。
### 补充：97 ns/call 花在哪里（三个最小探针的分解）

用三个只做一件事的 probe 对象（`deploy/bpftime/bench/{empty,percpu,hash}.bpf.c`，Loader 原样复用），
同一个 victim、同一个 pod、同样先 attach 再计时：

| 程序体 | ns / (malloc+free) | 相对基线 |
|---|---|---|
| 无读取器 | 5.7 | — |
| `empty`：进 hook 立刻返回 | **141.8** | +136 |
| `percpu`：+ `bpf_get_current_pid_tgid` + 单元素 percpu 数组计数 | 155.8 | +150 |
| `hash`：+ 按 pid 的 hash 计数器 + `bpf_get_prandom_u32` | 187.2 | +182 |
| 线上程序（1/64 采样 + cfg 查表 + 取栈 + 放大） | 230.1 | +224 |

结论：

- **每对调用 136 ns 是 trampoline 的纯成本**（68 ns/次），占线上开销的 61%，任何程序体优化都动不了它。
- 按 pid 的 hash 计数器 + 随机数约 31 ns/pair；percpu 计数只要 14 ns/pair——但换成 percpu 就失去按 pid 的 callback 归因，而那个归因是定位"探针没跑 / map 丢了"的唯一手段，不值得为 8% 换掉。
- 线上程序比 `hash` 多出的 43 ns 是 cfg 查表 + 采样判定 + 取栈 + 字节放大，符合 520/64 的量级。

所以 PR3-E 的最终结论：**这套 runtime 下 141.8 ns/pair 是地板，我们停在 230（地板的 1.6×），多出来的都是想要的诊断与采样**。
要再降一个数量级，只能换掉"每次调用都进用户态 eBPF"这件事本身，而不是继续调探针。
### 0.1.115–0.1.117 Memory 页与 `GET /api/v1/memory/coverage`

L1（Native Allocation）的透明度接口与页面骨架落地：

- `crates/lidlesseye-server/src/api.rs` 新增 `memory_coverage`：把 `alloc_space` 的 series 按 workload 折叠，返回
  `requested_alloc_bytes`、`batches`、`sampling_interval`（从 `alloc.sampling=poisson-1/64` 解析）、`sampling_label`、
  `reader`（`bpftime-userspace` / null=运行时自报）、`enabled`（是否还有 alloc directive）、`first_seen_ns`/`last_seen_ns`。
  路由 `/api/v1/memory/coverage` 注册在 `lib.rs`。
- 故意不返回 `coverage`（进程级覆盖）：那份数据在目标 pod 里、日志里有，服务端没有；字段给 `null` 并附
  `coverage_note`，而不是编一个数字。
- 前端新增 `web/components/views/memory-view.tsx`（表格 + Refresh）、`View` 类型加 `"memory"`、侧边栏加
  `Memory`（`MemoryStick` 图标）、`dashboard.tsx` 路由、`lib/api.ts` 的 `memoryCoverage()`。
- 实测（1 h 窗口）：`disk/le-alloc-target` 509 GiB / 265 batches / `poisson-1/64` / `bpftime-userspace` / reader on；
  `node-profiles`（Go runtime 自报）72 TiB / 11 565 batches / reader off。

**已知问题（下一步要修）**：`node-profiles` 的 72 TiB 明显是错的。运行时的 `alloc_space` 是"进程启动至今的累计值"，
而 `series()` 把它当作区间增量求和，11565 个 batch 叠加就放大了。修法二选一：让 Go/JVM 读取器上报**增量**，
或者让 `series()` 对 `reader is null`（运行时自报）的 `alloc_space` 用 `arg_max` 取最新值而不是 `SUM`。
`inuse_space` 已经走 `arg_max`（gauge 语义），`alloc_space` 需要按来源分开处理。
### 0.1.118–0.1.119：gauge 修正、覆盖率回传、L0 内存

三件事一起落地（用户 "123一起走"）。

**1. `alloc_space` 的 gauge 语义（`crates/lidlesseye-core/src/store.rs`，`series_inner`）**

原来是 `CASE WHEN profile_type IN ('inuse_space','inuse_objects') THEN arg_max(...) ELSE SUM(...)`。
运行时自报的 `alloc_space` 是"进程启动至今的累计值"，被 `SUM` 起来后 `node-profiles` 显示 **72 TiB**。改成：

```sql
CASE WHEN profile_type IN ('inuse_space','inuse_objects','memory_rss','memory_pss')
          OR (profile_type = 'alloc_space' AND instr(labels,'alloc.reader=') = 0)
     THEN arg_max(total_value, end_ns) ELSE SUM(total_value) END
```

判据是标签：注入的读取器带 `alloc.reader=`，它发的是区间增量，仍然求和；运行时自报不带，取最新一个批次。
修正后同一窗口 `node-profiles` 从 72 TiB 降到 311 MB（仍是估算值，但量级对了）。

**2. 覆盖率回传（`POST /api/v1/memory/coverage`）**

- 新增 `crates/lidlesseye-server/src/coverage.rs`：`Coverage` 保存每个 workload 的最近一份 `CoverageReport{detected, attached, skipped, memory_bytes, at_ns}`；
  旧报告不会覆盖新报告（重试到达晚于新状态时按 `at_ns` 丢弃）。
- `AppState::Inner` 加 `coverage: Arc<Coverage>`，`GET /api/v1/memory/coverage` 在折叠完 series 后把报告贴到对应行；
  **只有报告没有样本的 workload 也会出现在列表里**——"开了但一个进程都没挂上"正是运维要看见的状态。
- shim 侧：`attach_candidates()` 改成返回 `AttachStats{detected, attached, skipped}`（`musl`、`attach-failed` 分别计数），
  每个 tick 调 `post_coverage()` 上报。失败只 `debug!`：覆盖率是诊断，不该因为它失败就停掉采样。
- 实测：`disk/le-alloc-target` 返回 `coverage: {detected:2, attached:2, uncovered:0, memory_bytes:10724982784}`。

**3. L0：pod 内存**

shim 每次上报时读 `/sys/fs/cgroup/memory.current`（cgroup v2）或 `/sys/fs/cgroup/memory/memory.usage_in_bytes`（v1），
随 coverage 一起回传，Memory 页 "Pod memory" 列显示。这是最粗的一档（整个 pod、含 page cache），
但它**是量出来的**，比在服务端猜一个数字诚实。逐进程 RSS/PSS 仍需在 agent 侧扫 `/proc/*/status` 与 `smaps_rollup`，尚未做。

UI：Memory 表新增 `Coverage`（`2/2 processes`，跳过原因拼在逗号后）与 `Pod memory` 两列，
页脚文案从"尚未回传"改为解释覆盖率的含义。

已知未做：L0 逐进程 RSS/PSS/Private Dirty 与趋势图；L2（jemalloc/tcmalloc 原生 profiling）；`allocation_rate`、
`coverage` 的相对时间序列（当前只有最新值）。
### 0.1.120–0.1.121：L0 落到进程级（RSS / PSS）

shim 现在每个 tick 顺带报三件事（`crates/lidlesseye/src/shim.rs`）：

- `pod_memory_bytes()`：`/sys/fs/cgroup/memory.current`（v2）或 `memory/memory.usage_in_bytes`（v1），pod 级。
- `pod_slice()`：从 `/proc/self/cgroup` 里取本 pod 的 UID。**坑**：slice 不叫 `pod<uid>.slice`，
  而是 `kubelet-kubepods-besteffort-pod<uid>.slice`，所以不能用 `starts_with("pod")`，
  要 `rfind("pod")`（`kubepods` 里也有 pod 这个词）。
- `process_memory(uid)`：对本 pod 的每个 pid 读 `/proc/<pid>/status` 的 `VmRSS` 与
  `/proc/<pid>/smaps_rollup` 的 `Pss`，求和并计数。

`CoverageReport`/`CoverageBody` 增加 `process_count`、`process_rss_bytes`、`process_pss_bytes`，
`GET /api/v1/memory/coverage` 原样透出；Memory 页新增 RSS / PSS 两列，"Pod memory" 改名 "Pod cgroup"。

实测（`disk/le-alloc-target`，0.1.121）：

| 指标 | 值 |
|---|---|
| 进程数 | 4 |
| RSS | 155 MiB |
| PSS | 100 MiB |
| Pod cgroup | 9.6 GiB |

**三个数字对不上是正常的，而且要一起看**：PSS < RSS 说明有共享页被分摊；cgroup 远大于两者，
是因为它含 page cache（页缓存不算进程常驻）。只给一个数就会让人误判。

已知未做：逐进程的 Private Dirty、按时间的趋势图（现在只有最新一次上报）、
以及"哪个进程占得最多"的分组视图。
### 0.1.122：进程内存趋势

`crates/lidlesseye-server/src/coverage.rs` 从"只留最新一份报告"改为 `Entry { report, points: VecDeque<MemoryPoint> }`：
每次上报若带 RSS/PSS 就压入一个 `MemoryPoint { at_ns, rss_bytes, pss_bytes, cgroup_bytes }`，
环形上限 `MAX_POINTS = 480`（每 15 s 一个点 ≈ 2 小时）。

`GET /api/v1/memory/coverage` 的每个 workload 增加 `history` 数组（有历史才给，空的不给）。

前端 `memory-view.tsx`：
- 行可点击展开（只有存在历史时才可点），展开行渲染 `MemoryTrend`；
- `MemoryTrend` 用一张 SVG 画三条线：cgroup（淡、实线）、PSS（半透明虚线）、RSS（实线），
  共用字节轴，**纵轴按窗口内峰值归一**（不是按最后一个点，否则先涨后落的尖峰会被压平）；
- 图下一行读出最新三个值和点数、峰值。

实测（`disk/le-alloc-target`）：19 点，RSS 155 MiB / PSS 100 MiB / cgroup 9.6 GiB，峰值 9.6 GiB。

两个运维细节：
- 历史只在内存里，**服务端重启就清空**（不落盘，避免给一个会误导的"持久曲线"）。
- 期间发现 alloc directive 被一条早期测试写进去的 `default/kubernetes` 覆盖了，
  导致目标 workload 的开关丢失、shim 不再注入（页面显示 `reader off`）。已 `PUT` 关掉那条、重新打开
  `disk/le-alloc-target`，并重建 pod 让新 shim 生效。
## Sprint 1 — 数据契约：temporality 与 alloc_space 语义（已完成 0.1.124）

**问题**：`alloc_space` 一个名字两种含义。Go/JVM 报的是"进程启动至今累计"，注入读取器报的是"窗口内增量"。服务端之前靠 `profile_type` 白名单猜聚合方式，猜不中的部分按 SUM 处理，于是 6 GB 被加成 72 TB。

**改动**
1. `crates/lidlesseye-ebpf/src/collector.rs`：Go 读取器（`profile_source=go-runtime`）与 JVM 读取器（`profile_source=jvm-jfr`）产出的报告加标签 `temporality=cumulative`。
2. `crates/lidlesseye/src/shim.rs`：注入读取器的 ingest URL 加 `label=temporality=delta`。
3. `crates/lidlesseye-core/src/store.rs` 的 `series_inner`：聚合判据从"profile_type 白名单"改为"标签优先"——
   ```sql
   CASE WHEN instr(labels,'temporality=cumulative') > 0
             OR instr(labels,'temporality=gauge') > 0
             ...
        THEN arg_max(total_value, end_ns) ELSE SUM(total_value) END
   ```
   旧行没有标签时仍走原有白名单，历史数据不受影响。
4. Memory 页新增 `Semantics` 列：注入读取器显示 `delta (this window)`，运行时自报显示 `cumulative (since start)`。

**验证**（`disk` 命名空间，40 分钟窗口）
- `/api/v1/series?profile_type=alloc_space` 里，新产生的 Go 行带 `temporality=cumulative`，旧行不带 —— 历史未被重标，符合"新老共存、不猜旧数据"的要求。
- `node-profiles` 显示 68 GB（进程累计的最新值），`disk/le-alloc-target` 显示 452 GB 但标注 `delta (this window)`。
- `tsc` 通过；其余端点无回归。

**遗留到后续 Sprint**：`alloc_space_total` 作为独立 profile type 的迁移（需要 agent 同时发两个类型一个周期，再切读路径），放在 Sprint 10 与 Rollup 一起做，避免两次改 schema。
## Sprint 2 — Runtime Capability Matrix（已完成 0.1.125/0.1.126）

**问题**：`native_memory.supported=true` 是全局开关，它回答不了"这个 workload 现在到底有没有在读"。开启、注入成功、真的在产生样本是三件事，之前只有一个布尔值。

**改动**
1. 新模块 `crates/lidlesseye-server/src/capabilities.rs`：
   - `State` 枚举 `supported / attaching / active / degraded / unsupported / disabled`；
   - `reason` 常量 `unsupported_libc`、`static_binary`、`missing_runtime_tool`、`missing_libelf`、`permission_denied`、`no_target_process`、`attach_failed`、`no_samples`、`runtime_dead`；
   - `Capability { workload, reader, state, reason, detected, attached, at_ns }`，按 `(workload, reader)` 只保留最新一份，旧报告丢弃（退出码死掉的读取器不能继续自称 active）；
   - `from_attach()` 把 shim 的 `AttachStats` 映射成 state+reason：0 个候选进程 → `unsupported/no_target_process`；有注入成功 → `active`；只有 musl → `unsupported/unsupported_libc`；其余 → `attaching/attach_failed`。四个单元测试覆盖新旧覆盖、乱序丢弃、musl 与无进程。
2. `AppState` 增加 `capabilities: Arc<Capabilities>`；`GET /api/v1/capabilities[?workload=]` 读取，`POST` 接受数组（读取器上报）。
3. `crates/lidlesseye/src/shim.rs`：每 tick 除 coverage 外再 `post_capability()`，把自己在目标 pod 里看到的事实（detected/attached/skipped）报上去。
4. Memory 页新增 `Capability` 列（`active` / `unknown`），与 `Semantics` 列并列。

**验证**（0.1.125 → 0.1.126）
- `GET /api/v1/capabilities` 返回 `{"capabilities":[{"at_ns":1795715515457424705,"attached":2,"detected":2,"reader":"native_alloc","state":"active","workload":"disk/le-alloc-target"}]}`。
- 重建 `le-alloc-target` pod 后状态从无到 `active`，与 coverage 的 `2/2 processes` 一致。
- UI 截图：`Capability` 列显示 `active`（目标 workload）与 `unknown`（只有运行时自报、没有 native 读取器的行 —— 诚实的未知，不是伪装的成功）。

**已知缺口（下一 Sprint 补）**：`go_heap` / `jvm_heap` / `cpu` 还没有 agent 上报，因此显示 `unknown`；它们的状态来自 collector 侧（是否看到 Go 运行时、是否找到 `jcmd`），需要在 collector 的 flush 里补一次上报。
## Sprint 3 — 全局 Coverage 的 Pod 维度（已完成 0.1.127–0.1.129）

**问题**：`2/2 processes` 只说明"被注入的那个 pod 里有 2 个进程"，不说明整个 workload 有几个 pod 在读。三副本里只有一个在读，和一个副本里两个进程都在读，是两种完全不同的故障。

**改动**
1. `capabilities.rs`：`Capability` 增加 `pod`、`pods_reported`、`pods_active`；存储键改为 `(workload, reader, pod)`，`list()` 折成每 workload 每 reader 一行，按状态优先级合并（active > degraded > attaching > supported > unsupported > disabled），并把 `detected/attached` 求和、`pods_*` 计数。
2. shim 上报里带 pod 名。`HOSTNAME` 在 ephemeral 容器里不可靠，改成 agent 创建 shim 时显式注入 `LIDLESSEYE_POD`（`crates/lidlesseye/src/k8s.rs`），shim 优先读它、回退 `HOSTNAME`（`crates/lidlesseye/src/shim.rs`）。
3. Memory 页 Capability 列改为 `active · 1/1 Pods · 2/2 Processes`。

**验证**
- `GET /api/v1/capabilities` 返回 `{"attached":2,"detected":2,"pods_active":1,"pods_reported":1,"state":"active","workload":"disk/le-alloc-target"}`。
- 新建 pod 确认 shim 收到 `LIDLESSEYE_POD=le-alloc-target-57bf597fb8-p265h`。
- UI 截图：Capability 列显示 `active · 1/1 Pods · 2/2 Processes`，与 Coverage 列的 `2/2 processes` 互相对照。

**尚未完成**：`expected`（控制器期望副本数）还拿不到 —— 需要 agent 读 workload 的 `spec.replicas`；当前只显示已上报口径，宁可少说也不假设。
## Sprint 4 — 进程级内存与 Top-N（已完成 0.1.130/0.1.131）

**问题**：pod cgroup 只回答"这个 pod 用了多少"，回答不了"六个进程里是哪个在长"。而进程级 `smaps_rollup` 会走页表，不能对每个进程都跑。

**改动**
1. 新模块 `crates/lidlesseye-server/src/processes.rs`：
   - `ProcessRow { pid, start_ns, comm, executable, rss_bytes, pss_bytes, private_dirty_bytes, swap_bytes, missing }`；
   - `ProcessReport { workload, pod, at_ns, processes, omitted }`，按 `(workload, pod)` 只留最新，旧报告丢弃（重启后的 pid 复用不会继承旧行）；
   - `top(workload, limit)` **先合并所有 pod 再截断** —— 先各自取 top 再合并会丢掉每个 pod 的第二大进程；两个单元测试分别覆盖"跨 pod 合并后再截断"和"乱序报告不覆盖新数据"。
2. `GET /api/v1/memory/processes?workload=&limit=`（默认 20，clamp 1..200），`POST` 接收报告。
3. shim（`crates/lidlesseye/src/shim.rs`）：`process_rows(limit)` 遍历 `/proc`，`VmRSS` 走一次 `status` 读，按 RSS 排序取 Top-N，**只有 Top-N 才读 `smaps_rollup`** 拿 PSS / Private_Dirty / Swap；读不到写 `missing: "smaps_rollup not readable"` 而不是 0。`start_ns` 用 `/proc/<pid>/stat` 的 `starttime`（HZ 按 100 假设，仅用于识别 pid 复用，注释写明）。
4. Memory 页展开行：趋势图下方新增进程表（PID / 进程 / RSS / PSS / Private dirty / Swap），取 Top-10。

**验证**（0.1.131）
- `GET /api/v1/memory/processes?workload=disk/le-alloc-target&limit=5` → 两行：`lidlesseye-bpftime`（pid 33，RSS 66.6 MB / PSS 45.1 MB / dirty 10.4 MB）与 `lidlesseye`（pid 11，RSS 66.3 MB / PSS 45.8 MB / dirty 8.7 MB）。
- UI 截图：展开行显示进程表，与行内 RSS 153 MiB / PSS 99 MiB 一致（差额是未列出的较小进程）。

**尚未完成**：`private_dirty` 的"读不到"与"确实为 0"在图上是同一个 0 B；后续把 `missing` 在数字列里显式画出来（当前只有 PSS 缺失时才显示原因）。
## Sprint 5 — Memory 页三层（已完成 0.1.132/0.1.133）

**问题**：一张表混着"分配了多少"（L1）和"进程占了多少"（L0），读者分不清哪个数字回答哪个问题；L2 根本不存在却连一句说明都没有。

**改动**
1. 摘要条：`6 workloads · 1 pods · 2 processes · 1 readers active`（来自 capability 上报）。
2. 表头改为 `Native allocation (L1)`：Requested / Batches / Sampling / Reader / Semantics / Capability / Coverage / State / Last sample。
3. 展开行标为 `Process memory (L0)`：趋势图（三条线共用字节轴、按窗口峰值归一、实线 RSS / 虚线 PSS / 淡线 cgroup）+ 进程 Top-10 表。
4. 页面底部新增 `Allocator native profiling (L2)` 虚线面板，明确写"本版本不读 jemalloc/tcmalloc 的 stats，这一层是空的而不是 0"。

**验证**（0.1.133）
- 截图：摘要条、L1 表、展开后的 `Process memory (L0)` + 趋势 + 进程表、底部 L2 面板全部在场，无重复区块。
- 数据：`disk/le-alloc-target` 行 RSS 151 MiB / PSS 98 MiB / cgroup 8.7 GiB，展开后两个进程 62+61 MiB RSS 与行内一致。
- `tsc` 通过；`GET /` 200。

**过程中踩的坑**：用行号脚本删除重复 JSX 块时把 `MemoryTrend`/`ProcessTable` 两个组件一起删掉了（它们夹在两个重复块之间），tsc 报 `Cannot find name`；按原语义重写并补回，`tsc` 复归 0。教训：删除 JSX 区块用 edit 匹配整块，不要用行号滑动窗口。
## Sprint 7 — Native Runtime Manager（0.1.134/0.1.135）

**问题**：ephemeral 容器永不重启、也永不删除。旧代码看到 `lidlesseye-alloc` 在 spec 里就返回"没事"，于是一次崩溃让这个 pod 永久失去读取器；而如果无脑补一个新容器，一个每次启动就死的镜像会把 pod 塞满。

**改动**
1. `crates/lidlesseye/src/k8s.rs`：把"检查 + 追加"拆成 `ensure_alloc_shim`（决策）与 `add_shim`（执行）；返回 `ShimOutcome { Running, Started, Exhausted { exit_code, reason } }`。
2. 重建：最新一个 shim 已 terminated 时，追加 `lidlesseye-alloc-2`、`-3`（名字不可复用，死掉的容器永远占着原名）。
3. 上限：`MAX_SHIM_REBUILDS = 3`；超过就返回 `Exhausted`，agent 打 `allocator shim is dead and cannot be rebuilt; replace the pod`。
4. `crates/lidlesseye/src/main.rs`：`match` 三个分支分别 info / 静默 / warn。

**验证**（0.1.135，`disk/le-alloc-target`）
- 新 pod 的 shim 是 `lidlesseye-alloc=docker.io/xsj321/lidlesseye:0.1.135-perf`；
- `pkill -9 -f bpftime-shim` 后，spec 变成 `lidlesseye-alloc,lidlesseye-alloc-2`，状态 `lidlesseye-alloc=terminated(137) | lidlesseye-alloc-2=running`；
- 重建后的容器照常上报：`capabilities` → `active, pods_active=1`；`flamegraph?type=alloc_space&selector=service_name="disk/le-alloc-target"` 8 分钟窗口 `total=2624 samples=1`。

**踩坑**
- 首版命名拼成 `lidlesseye-alloc--1`（前缀已带 `-`），修成 `{SHIM_PREFIX}{n}` 且编号从 2 开始。
- `pkill` 默认 SIGTERM 对容器 pid 1 无效，shim 会把 loader 拉起来 —— 这是自愈的正常路径；要测重建必须 `pkill -9`。
- 用 ASCII 清理脚本改 `crates/lidlesseye/src/main.rs` 时把 `out.push('…')` 里的非 ASCII 字符抹掉，产生 `empty character literal` 编译错误；改用 `push_str("...")` 并把 `take(max-1)` 改成 `take(max-3)`。

**遗留**：`Exhausted` 目前只写进 agent 日志，还没有把 `state=degraded, reason=runtime_dead` 推到 `/api/v1/capabilities`，UI 上仍显示为 `reader on` 缺少样本。下一轮补。
### Sprint 7 收尾（0.1.136/0.1.137）

- `exhausted` 不再只写日志：`crates/lidlesseye/src/publisher.rs` 新增 `report_reader_degraded(workload, pod, reason, at_ns)`，POST `/api/v1/capabilities`；agent 在 `ShimOutcome::Exhausted` 分支上报 `state=degraded, reason=runtime_dead`。
- 实测：连杀 4 次（`pkill -9`）后 spec 为 `lidlesseye-alloc, -2, -3, -4` 全部 terminated，`GET /api/v1/capabilities` → `{"attached":0,"detected":0,"pods_active":0,"pods_reported":1,"reason":"runtime_dead","state":"degraded"}` —— 正是"开着但没样本"应该给出的回答。
- 陈旧行会污染计数：删掉那个 pod 后仍显示 `pods_reported=2`。`capabilities.rs` 加 `STALE_AFTER_NS = 10 min`，`list()` 跳过超过十分钟未更新的报告；重建 pod 后计数回到 `1/1 Pods · 2/2 Processes`。
- 编译坑：`crate::now_ns()` 在 server crate 里不存在，用 `lidlesseye_core::now_ns()`。
## Sprint 8 — Agent 磁盘缓冲与幂等 ingest（0.1.138/0.1.139/0.1.140）

**问题**：上传失败超过 5 分钟的重试预算后，整批被丢掉——服务端重启一次，每个 profile 就出现一个洞。反过来，重试也可能把同一批送两遍（响应在回程丢了），而两份都会被计数，错误还看不见。

**缓冲（0.1.138）**——`crates/lidlesseye/src/publisher.rs`
- 新增 `Spool`：`LIDLESSEYE_SPOOL` 指向一个目录（agent 用 hostPath `/var/lib/lidlesseye/spool`，`DirectoryOrCreate`），每批写一个 `{ts_ns}-{seq}.json`，先写 `.tmp` 再 rename（半写文件会被当成截断的批次永远拒绝）。
- 放弃重试时改为落盘而不是丢弃；`SPOOL_KEEP = 720` 个文件（默认 5 分钟一批 ≈ 两天），超出丢最旧的并告警。
- 排空任务每 30 秒扫一次目录，按 2xx 才删；连续 20 分钟送不出去打一次 warn。
- 实测：`kubectl scale deploy --replicas=0` 停服务 7 分钟 → spool 出现 `01795723647630504541-00000000.json`（41 980 B）→ 恢复后文件数回到 0。

**幂等（0.1.139/0.1.140）**——`crates/lidlesseye-server/src/dedupe.rs`
- 新增 `Dedupe`：batch id → 首次接受时间，`CAPACITY = 8192`、`TTL_NS = 30 min`，`evict` 同时按容量与时间淘汰；两个单测（同 id 只算一次、最旧被淘汰）。
- `POST /ingest` 读 `x-lidlesseye-batch`；命中返回 `200 {"duplicate": true, ...}` 且不入库。
- **关键修正**：第一版在解析前就登记 id，于是一个 400（可修复的错误）的批次被永久黑名单，重试永远送不进来。改成 `seen()` 先查、入库成功后才 `remember()`；实测同一 id 首次 400、二次 400（都还能再送），而成功的一批第二次返回 `{"accepted":0,"batch":"probe-4","duplicate":true}`。
- agent 端：同一批的所有重试与落盘都用同一个 id（`post_batch_with(..., Some(&batch_id))`），spool 文件里也存这个 id，重启后重放仍被判为同一批。