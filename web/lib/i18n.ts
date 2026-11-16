// Interface language.
//
// Settings live in localStorage (see settings.ts), so the chosen language is
// stable across reloads but never travels with a shared link — exactly like
// the rest of the tool's behaviour preferences. Translation keys are the
// English source strings themselves: the English dictionary is therefore the
// identity function, and adding a language only means adding one map here.

"use client";

import { useSettings } from "./settings";
import type { Language } from "./settings";

export type { Language } from "./settings";

const ZH: Record<string, string> = {
  // Shell / navigation
  Refresh: "刷新",
  "Reload (R)": "重新加载 (R)",
  Tenant: "租户",
  default: "默认",
  Settings: "设置",
  "Application navigation": "应用导航",
  "Dashboard views": "仪表盘视图",
  "Collapse navigation": "折叠导航",
  "Expand navigation": "展开导航",
  "Service map": "服务地图",
  Overview: "总览",
  Flamegraph: "火焰图",
  Compare: "对比",
  "Top functions": "热点函数",
  "Time series": "时序",
  Labels: "标签",

  // Toolbar
  Profile: "性能类型",
  "Profile type": "性能类型",
  "Group by": "分组",
  none: "无",
  Range: "范围",
  "Custom range": "自定义范围",
  "Last 5 minutes": "最近 5 分钟",
  "Last 15 minutes": "最近 15 分钟",
  "Last hour": "最近 1 小时",
  "Last 6 hours": "最近 6 小时",
  "Last 24 hours": "最近 24 小时",
  "Last 7 days": "最近 7 天",
  Auto: "自动",
  "Auto-refresh (interval set in Settings)": "自动刷新（间隔在设置中调整）",
  Apply: "应用",
  "Re-run the query with the current filters": "使用当前筛选条件重新查询",

  // Time bar
  Relative: "相对时间",
  Absolute: "绝对时间",
  "How the time range is chosen": "时间范围选择方式",
  absolute: "绝对",
  "ends now": "截至当前",
  From: "开始",
  To: "结束",
  Now: "现在",
  "Absolute range start": "绝对范围开始时间",
  "Absolute range end": "绝对范围结束时间",
  "Move the end of the range to now": "将结束时间设为当前",
  "Pick both ends of the range.": "请选择时间范围的起止时间。",
  "Reset this window to the whole range": "将窗口重置为整个范围",
  Full: "全部",
  "No samples here; data spans": "此处无采样；数据范围为",
  "Use data range": "使用数据范围",
  "Show the histogram": "显示柱状图",
  "Hide the histogram": "隐藏柱状图",
  "Show chart": "显示图表",
  "Hide chart": "隐藏图表",
  "Time range": "时间范围",
  Requests: "请求数",
  "A · Baseline": "A · 基线",
  "B · Comparison": "B · 对比",

  // Settings
  General: "通用",
  "Preferences are stored in this browser. They do not travel with a shared link.":
    "偏好设置保存在此浏览器中，不会随分享链接传递。",
  "Start-up": "启动",
  "Default view": "默认视图",
  "Used only when the URL does not name a view.": "仅当 URL 未指定视图时使用。",
  "Default range": "默认范围",
  "The relative window on a fresh load.": "首次加载时使用的相对时间窗口。",
  "Auto-refresh interval": "自动刷新间隔",
  "How often the current view reloads when auto-refresh is on.":
    "开启自动刷新后，当前视图的刷新频率。",
  "Show connections/s and mean RTT on each edge.": "在每条连线上显示每秒连接数与平均 RTT。",
  "Edge labels": "连线标签",
  "Label zoom threshold": "标签显示缩放阈值",
  "Labels appear once the map is zoomed past this point.": "地图缩放到此程度后显示标签。",
  Charts: "图表",
  "Flamegraph frame cap": "火焰图帧数上限",
  "Higher caps draw more frames but cost memory.": "上限越高绘制的帧越多，但内存开销更大。",
  "Histogram buckets": "柱状图桶数",
  "Resolution of the time brush above the views.": "上方时间刷的分辨率。",
  "Reset to defaults": "恢复默认",
  Done: "完成",
  Language: "语言",
  "Interface language": "界面语言",
  English: "English",
  Chinese: "简体中文",

  // Service map
  "No service map yet": "尚无服务地图",
  "The map is built by the agent from the pods' TCP tables and stored with the profiles.":
    "服务地图由 Agent 根据 Pod 的 TCP 连接表构建，并与性能数据一同存储。",
  workloads: "工作负载",
  services: "服务",
  edges: "连线",
  "new connections": "新建连接",
  workload: "工作负载",
  external: "外部",
  "all ready": "全部就绪",
  degraded: "降级",
  "no replicas": "无副本",
  "edge labels: new connections/s · mean RTT": "连线标签：每秒新建连接数 · 平均 RTT",
  "clipped to 600 nodes": "已截断至 600 个节点",
  connections: "连接",
  "No traffic in this range": "该时间范围内无流量",
  "The service map is built from the pods' TCP tables on each node. Widen the range, or check that the agent is running with pod metadata enabled.":
    "服务地图来自各节点 Pod 的 TCP 连接表。请扩大时间范围，或检查 Agent 是否已启用 Pod 元数据。",

  // Node panel
  Neighbours: "上下游",
  "Close panel": "关闭面板",
  "Resize node panel": "调整节点面板宽度",
  "Drag to resize · arrow keys to nudge": "拖动调整宽度 · 方向键微调",
  "Window from the toolbar · the same one the flamegraph uses":
    "时间范围来自工具栏 · 与火焰图使用相同范围",
  Instances: "实例",
  Memory: "内存",
  Net: "网络",
  DNS: "DNS",
  Profiling: "性能分析",
};

/** Translate one source string; unknown keys fall back to the English source. */
export function translate(key: string, language: Language): string {
  return language === "zh" ? (ZH[key] ?? key) : key;
}

/** Reusable translator bound to the preference stored in localStorage. */
export function useT(): (key: string) => string {
  const { language } = useSettings();
  return (key: string) => translate(key, language);
}
