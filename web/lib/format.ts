// Formatting helpers shared by every view.
//
// These are pure functions over the units the API reports, ported unchanged
// from the pre-React dashboard so the numbers read exactly the same.

const TIME_UNITS = [
  { ns: 1e9, suffix: "s" },
  { ns: 1e6, suffix: "ms" },
  { ns: 1e3, suffix: "µs" },
  { ns: 1, suffix: "ns" },
];

/** Format a raw value given the profile unit reported by the API. */
export function formatValue(value: number | null | undefined, unit = "nanoseconds"): string {
  if (value == null || Number.isNaN(value)) return "–";
  if (unit === "nanoseconds") return formatDuration(value);
  if (unit === "bytes") return formatBytes(value);
  return formatCount(value);
}

export function formatDuration(ns: number): string {
  if (!Number.isFinite(ns)) return "–";
  const sign = ns < 0 ? "-" : "";
  const v = Math.abs(ns);
  if (v === 0) return "0s";
  if (v >= 60e9) {
    const minutes = Math.floor(v / 60e9);
    const seconds = (v - minutes * 60e9) / 1e9;
    return `${sign}${minutes}m${seconds.toFixed(seconds < 10 ? 1 : 0)}s`;
  }
  for (const unit of TIME_UNITS) {
    if (v >= unit.ns || unit.ns === 1) {
      const scaled = v / unit.ns;
      const digits = scaled >= 100 ? 0 : scaled >= 10 ? 1 : 2;
      return `${sign}${scaled.toFixed(digits)}${unit.suffix}`;
    }
  }
  return `${sign}${v}ns`;
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes)) return "–";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  let v = bytes;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${v.toFixed(v >= 10 || i === 0 ? 0 : 1)} ${units[i]}`;
}

export function formatCount(n: number): string {
  if (!Number.isFinite(n)) return "–";
  if (Math.abs(n) < 1000) return String(n);
  const units: Array<[number, string]> = [
    [1e12, "T"],
    [1e9, "G"],
    [1e6, "M"],
    [1e3, "k"],
  ];
  for (const [scale, suffix] of units) {
    if (Math.abs(n) >= scale) {
      return `${(n / scale).toFixed(n / scale >= 10 ? 0 : 1)}${suffix}`;
    }
  }
  return String(n);
}

/** `0.1234` -> `12.3%` */
export function formatPercent(fraction: number, digits = 1): string {
  if (!Number.isFinite(fraction)) return "–";
  return `${(fraction * 100).toFixed(digits)}%`;
}

export function formatTimestamp(ns: number): string {
  const date = new Date(Number(ns) / 1e6);
  return date.toLocaleString(undefined, {
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

export function formatClock(ns: number): string {
  const date = new Date(Number(ns) / 1e6);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

export function formatRelative(ns: number): string {
  const seconds = Math.round((Date.now() - Number(ns) / 1e6) / 1000);
  if (!Number.isFinite(seconds)) return "–";
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)}h ago`;
  return `${Math.round(seconds / 86400)}d ago`;
}

/** `{app="api", env="prod"}` from a label map, omitting internal labels. */
export function formatLabels(
  labels: Record<string, string> | undefined,
  skip: string[] = ["node", "service_name", "process_name"],
): string {
  const parts = Object.entries(labels ?? {})
    .filter(([k]) => !skip.includes(k))
    .map(([k, v]) => `${k}="${v}"`);
  return parts.length ? `{${parts.join(", ")}}` : "";
}

export function debounce<A extends unknown[]>(fn: (...args: A) => void, ms: number) {
  let handle: ReturnType<typeof setTimeout> | null = null;
  return (...args: A) => {
    if (handle) clearTimeout(handle);
    handle = setTimeout(() => fn(...args), ms);
  };
}
