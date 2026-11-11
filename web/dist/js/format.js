// Formatting helpers shared by every view.

const TIME_UNITS = [
  { ns: 1e9, suffix: "s" },
  { ns: 1e6, suffix: "ms" },
  { ns: 1e3, suffix: "µs" },
  { ns: 1, suffix: "ns" },
];

/** Format a raw value given the profile unit reported by the API. */
export function formatValue(value, unit = "nanoseconds") {
  if (value == null || Number.isNaN(value)) return "–";
  if (unit === "nanoseconds") return formatDuration(value);
  if (unit === "bytes") return formatBytes(value);
  return formatCount(value);
}

export function formatDuration(ns) {
  if (!Number.isFinite(ns)) return "–";
  const sign = ns < 0 ? "-" : "";
  let v = Math.abs(ns);
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

export function formatBytes(bytes) {
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

export function formatCount(n) {
  if (!Number.isFinite(n)) return "–";
  if (Math.abs(n) < 1000) return String(n);
  const units = [
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
export function formatPercent(fraction, digits = 1) {
  if (!Number.isFinite(fraction)) return "–";
  return `${(fraction * 100).toFixed(digits)}%`;
}

export function formatTimestamp(ns) {
  const date = new Date(Number(ns) / 1e6);
  return date.toLocaleString(undefined, {
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

export function formatClock(ns) {
  const date = new Date(Number(ns) / 1e6);
  const pad = (n) => String(n).padStart(2, "0");
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

export function formatRelative(ns) {
  const seconds = Math.round((Date.now() - Number(ns) / 1e6) / 1000);
  if (!Number.isFinite(seconds)) return "–";
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)}h ago`;
  return `${Math.round(seconds / 86400)}d ago`;
}

/** Escape a string for safe interpolation into HTML. */
export function escapeHtml(text) {
  return String(text ?? "").replace(/[&<>"']/g, (c) => {
    switch (c) {
      case "&":
        return "&amp;";
      case "<":
        return "&lt;";
      case ">":
        return "&gt;";
      case '"':
        return "&quot;";
      default:
        return "&#39;";
    }
  });
}

/** `{app="api", env="prod"}` from a label map, omitting internal labels. */
export function formatLabels(labels, skip = ["node", "service_name", "process_name"]) {
  const parts = Object.entries(labels ?? {})
    .filter(([k]) => !skip.includes(k))
    .map(([k, v]) => `${k}="${v}"`);
  return parts.length ? `{${parts.join(", ")}}` : "";
}

export function debounce(fn, ms) {
  let handle = null;
  return (...args) => {
    clearTimeout(handle);
    handle = setTimeout(() => fn(...args), ms);
  };
}
