# 分配器读取器开销基线（长期回归用）

这份记录的目的是把"每次 malloc+free 贵多少"固定成可复现的数字，而不是依赖某次对话里的
结论。改动 bpftime 程序体、升级 runtime、换节点之后，重跑这张表并对比。

## 环境

| 项目 | 值 |
|---|---|
| 节点 | Docker Desktop Kubernetes v1.36.1，2 节点（control-plane / worker），业务 pod 在 worker |
| 内核 | `6.18.33.1-microsoft-standard-WSL2` |
| CPU | `12th Gen Intel(R) Core(TM) i7-12700KF`，容器内 `nproc=20` |
| C 库 | `glibc 2.39-0ubuntu8.9`（运行时镜像 `ubuntu:24.04`） |
| bpftime | `ghcr.io/eunomia-bpf/bpftime:latest`，源码 `git rev-parse` = `388d13f` |
| 被测二进制 | `target/tmp/victim.c`，`cc -O2` 编译，`malloc(64)` + 立即 `free`，写一个字节防止被优化掉 |
| 规模 | 每次 20 000 000 次 malloc+free；进程先 `sleep N` 让读取器完成 attach，再开始计时 |
| 负载 | 单线程、无其他负载；pod `securityContext.privileged: true` |
| 重复 | 基线重复 20 次（5.6–6.0）；三个探针与线上配置各测一次（每次 20 000 000 次迭代）。复测脚本见文末，尚未补齐每个探针的多轮重复 |

注意：表里的 `ns/pair` 是**整体均值**（wall / 迭代数），不是单次调用的分位数。
要拿到 p50/p95/p99 必须逐调用打点或写直方图，当前没有做——所以这张表只能比较"总量变化"，
不能推断线上尾延迟。

## 基线

| 配置 | ns / (malloc+free) | 相对无插桩 | 说明 |
|---|---|---|---|
| 无读取器 | **5.7**（范围 5.6–6.0） | 1× | 干净 glibc malloc/free |
| `empty`：进 hook 立刻返回 | **141.8** | 25× | 只剩 trampoline |
| `percpu`：+ 取 pid + percpu 计数 | **155.8** | 27× | 最小可用探针 |
| `hash`：+ 按 pid 的 hash 计数 + 随机数 | **187.2** | 33× | 线上程序的每次调用部分 |
| `hash` + 1/64 采样 + 取栈 + 放大 | **230.1** | 40× | 线上配置（`rate=64`） |
| 线上配置，`rate=1048576`（等同永不取栈） | 193.7 | 34× | 证明取栈只占约 36 ns/pair |
| 线上配置，`rate=1`（每次取栈） | >30 µs/次 | — | 20 000 000 次跑不完，不可用 |

## 分解

| 成本组成 | ns/pair | 占额外开销 |
|---|---|---|
| Trampoline（进 hook 立刻返回） | 136.1 | 61 % |
| Percpu 计数 + 取 pid | 14.0 | 6 % |
| Hash 计数 + 随机数（percpu 之上） | 31.4 | 14 % |
| 线上额外逻辑（cfg 查表、采样判定、取栈、字节放大） | 42.9 | 19 % |
| 合计额外开销 | 224.4 | 100 % |

结论：**68 ns/call 的 trampoline 是不可优化的地板**，占线上开销六成。程序体写得再省也只
能在剩下的四成里挤；真正能改变数量级的只有换掉"每次调用都进用户态 eBPF"这件事。

## 被测的三个探针

都在 `deploy/bpftime/bench/`：

- `empty.bpf.c`：hook 体只有 `return 0`。仍然声明 `allocs`/`stacks` 两个 map，并在
  `if ((unsigned long)ctx == 1UL)` 这种永不成立的分支里引用，因为 loader 找不到这两个 map
  会直接退出——那样测到的就不是"空的 hook"而是"没有 hook"。
- `percpu.bpf.c`：加 `bpf_get_current_pid_tgid()` 和一个单元素 percpu 数组的原子加。
- `hash.bpf.c`：加按 pid 的 hash 计数器（lookup → 原子加 → 首次 insert）和
  `bpf_get_prandom_u32()`，即线上程序在每次调用上做的全部事情。

## 怎么重跑

```sh
# 1. 编译（在 bpftime 镜像里，需要先有 libbpf.a）
docker run --rm -v "$PWD/deploy/bpftime/bench:/w" \
  ghcr.io/eunomia-bpf/bpftime:latest sh /w/build.sh

# 2. 起一个特权 pod 当被测环境
kubectl -n disk apply -f target/tmp/bench-pod.yaml

# 3. 拷进 victim 和三个 .bpf.o，逐个跑
#    loader 必须用 LD_PRELOAD=libbpftime-syscall-server.so 启动，
#    attach 必须带 --install-location /root/.bpftime 和 HOME=/root，
#    否则 attach 打印 usage 后 exit 1，测出来的还是基线。
```

## 已知的坑

- victim 必须"先睡再计时"：读取器按自己的节奏 attach，几毫秒就退出的进程根本不会被插桩，
  会静默产出一条和基线一样的数字。
- `pkill -f loader` 会连 `sh -c` 自己的命令行一起杀（exit 143），用 `pkill -x loader`。
- 同一 pod 里连续跑多次时，上一轮的 loader 必须杀干净；否则第二个 loader 抢同一个
  `/dev/shm` 段，数字会飘。
- 每次重新构建镜像必须用新 tag：节点 containerd 按 digest 复用，重新 tag 同一名字仍跑旧二进制。
