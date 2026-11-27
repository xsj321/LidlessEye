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
  // Two units, largest first: a thirty-day window reads "30d", not
  // "43200m0.0s", and the same rule makes a two-hour CPU total "2h" instead of
  // "120m0.0s".
  if (v >= 86_400e9) {
    const days = Math.floor(v / 86_400e9);
    const hours = Math.floor((v - days * 86_400e9) / 3600e9);
    return `${sign}${days}d${hours ? `${hours}h` : ""}`;
  }
  if (v >= 3600e9) {
    const hours = Math.floor(v / 3600e9);
    const minutes = Math.floor((v - hours * 3600e9) / 60e9);
    return `${sign}${hours}h${minutes ? `${minutes}m` : ""}`;
  }
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

/**
 * A duration in milliseconds, always the same unit.
 *
 * The service map compares values next to each other — a card's CPU beside a
 * neighbour's, an edge's latency beside its rate — and `formatDuration`'s
 * unit-switching is right for a total but wrong for a column: "1m33s" next to
 * "0.22ms" reads as two different kinds of number. Milliseconds throughout,
 * with a thousands separator once it gets long, keeps them comparable.
 */
export function formatMillis(ns: number): string {
  if (!Number.isFinite(ns)) return "–";
  const ms = ns / 1e6;
  if (ms === 0) return "0 ms";
  if (Math.abs(ms) >= 1000) return `${Math.round(ms).toLocaleString("en-US")} ms`;
  if (Math.abs(ms) >= 100) return `${ms.toFixed(0)} ms`;
  return `${ms.toFixed(ms >= 10 ? 1 : 2)} ms`;
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

/**
 * `12:04:11`, or `10-23 12:04` when the two ends are not on the same day.
 *
 * A range picker that says "14:04:40 → 14:04:40" for a thirty-day window is
 * not just terse, it is wrong: the two ends look identical. Anything that
 * prints a window rather than an instant should use this.
 */
export function formatClockRange(from: number, until: number): string {
  const a = new Date(Number(from) / 1e6);
  const b = new Date(Number(until) / 1e6);
  if (a.toDateString() === b.toDateString()) {
    return `${formatClock(from)} → ${formatClock(until)}`;
  }
  const pad = (n: number) => String(n).padStart(2, "0");
  const stamp = (d: Date) =>
    `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  return `${stamp(a)} → ${stamp(b)}`;
}

/** `10-23`, for an axis whose span is longer than a day. */
export function formatDayMonth(ns: number): string {
  const date = new Date(Number(ns) / 1e6);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
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
