# MCP 服务端

LidlessEye 附带一个轻量的 [Model Context Protocol](https://modelcontextprotocol.io/) 服务端，让 AI 助手通过工具查询服务地图和 profiling 数据。它是**只读**的：所有请求都走看板使用的 HTTP API，不会写入存储。

[English](mcp.md)

## 设计

```
AI 客户端 ── MCP stdio JSON-RPC ──> lidlesseye-mcp ── HTTP GET ──> lidlesseye server
```

MCP 进程通常是 AI 客户端的 stdio 子进程，所以 **stdout 只用于 JSON-RPC**，日志全部写 stderr。它不链接 DuckDB，也不自行解析租户；认证、租户隔离和查询取消仍由 server 统一负责。

每个工具都返回同一种结构化对象：

```json
{
  "summary": "该 profile 在当前窗口合计 183000000 ns，来自 183 个样本；top_frames 按 inclusive value 排序。",
  "query_context": {
    "endpoint": "/api/v1/flamegraph",
    "tenant": "default",
    "params": { "from": "now-1h", "until": "now", "profile_type": "cpu" }
  },
  "unit": "ns",
  "truncated": false,
  "data": { "total": 183000000, "samples": 183, "top_frames": [] },
  "suggestions": []
}
```

- `summary` 用一句话说明数字的含义和单位。
- `query_context` 回显精确的端点、租户与参数，方便模型解释并复用同一个查询。
- `data` 遵循 **summary first**：先总量，再有限 top-N；火焰图不会返回全部帧。
- `truncated` 明确标记是否为了控制上下文而省略了行或桶。
- `suggestions` 在数据为空或选择器无效时给出安全的下一步。

工具错误使用 MCP 的 `isError: true`，并返回可自行修正的提示，例如 HTTP 401 时提示设置 token，HTTP 408/503 时提示缩小时间窗。

## 构建和运行

```bash
cargo build --release -p lidlesseye-mcp
./target/release/lidlesseye-mcp \
  --base-url http://127.0.0.1:4040 \
  --tenant default
```

发布后也可以用 `cargo install lidlesseye-mcp` 安装。

| 参数 | 环境变量 | 默认值 | 含义 |
| --- | --- | --- | --- |
| `--base-url` | `LIDLESSEYE_BASE_URL` | `http://127.0.0.1:4040` | LidlessEye server 地址 |
| `--token` | `LIDLESSEYE_AUTH_TOKEN` | 未设置 | 认证 server 的 Bearer token |
| `--tenant` | `LIDLESSEYE_TENANT` | `default` | 租户查询携带的 tenant |
| `--tenant-header` | `LIDLESSEYE_TENANT_HEADER` | `X-Scope-OrgID` | 携带租户的 header |
| `--timeout-seconds` | `LIDLESSEYE_TIMEOUT_SECONDS` | `60` | HTTP 请求超时 |

客户端配置示例：

```json
{
  "mcpServers": {
    "lidlesseye": {
      "command": "/absolute/path/to/lidlesseye-mcp",
      "env": {
        "LIDLESSEYE_BASE_URL": "http://127.0.0.1:4040",
        "LIDLESSEYE_TENANT": "default",
        "LIDLESSEYE_AUTH_TOKEN": "开启认证时再填写"
      }
    }
  }
}
```

不要在终端里直接运行后等待输出：stdio MCP server 会先等待客户端发送 `initialize`。

## 工具清单

| 工具 | 返回内容 | 主要参数 |
| --- | --- | --- |
| `get_health` | 服务状态与版本 | 无 |
| `list_tenants` | 租户及数据量 | 无 |
| `list_profile_types` | 窗口内的 profile 类型、单位与总量 | `from`、`until`、选择器 |
| `list_series` | 服务、profile 类型与标签集合 | `from`、`until`、选择器 |
| `list_label_values` | 某个标签最常见的取值 | `name`、`limit`、时间窗 |
| `get_service_map` | top 节点与边，连接数/字节/重传，RTT 单位毫秒 | `from`、`until` |
| `get_service_map_history` | 某节点自身及关联边的历史计数 | `node`、`buckets`、时间窗 |
| `get_service_map_histogram` | 每桶流量总量与峰值位置 | `metric`、`buckets`、时间窗 |
| `get_flamegraph_summary` | 总量、样本数与最热 inclusive 帧 | `profile_type`、`max_frames`、时间窗 |
| `get_top` | self value 最热函数及占比 | `profile_type`、`limit`、时间窗 |
| `get_timeseries` | 每条序列的总量、峰值与桶数 | `profile_type`、`buckets`/`step`、`max_series` |
| `diff_profiles` | 两个窗口之间按增减排序的帧 | 基线窗口、`profile_type`、比较窗口 |

所有时间参数都接受 `now-1h`、RFC3339 或 unix 时间戳，`until` 必须晚于 `from`。一次多工具分析中请给每次调用传入**相同的绝对 `from` 和 `until`**；相对时间 `now` 会在每次请求时重新求值，可能漂移。

## 推荐的排查顺序

1. `get_health`；租户不确定时再调用 `list_tenants`。
2. `list_profile_types` 与 `list_series`，先确认有什么数据、单位是什么。
3. `get_service_map` 或 `get_service_map_histogram`，找到目标服务和异常时间桶。
4. `get_top`、`get_flamegraph_summary` 找热点函数；`list_label_values` 细化诸如 `pod` 的选择器。
5. `get_timeseries` 定位变化的起点，再用显式绝对时间的 `diff_profiles`。
6. 如果边上有重传或高 RTT，对对应节点调用 `get_service_map_history`。

`diff_profiles` 中正值是回退，负值是改善。内存 profile 可能是 `inuse_space`/`inuse_objects`（水平量），也可能是 `alloc_space`/`alloc_objects`（速率）；不要把两类数值相加。

## 限制与安全

- 所有工具只读，不支持导入、租户管理或改设置。
- 返回结果有意做了裁剪；完整火焰图或完整节点列表请使用看板。
- 极宽的时间窗仍可能耗时数秒或返回 HTTP 408/503，请缩小窗口或增大 `--timeout-seconds`。
- token 只发送给配置中的 server。请妥善保管客户端配置；除非排障，保持默认 `RUST_LOG=warn`。
- RTT 用毫秒报告；profiling 数值沿用端点自己的单位，单位会回显在 `unit` 中。
