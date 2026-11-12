// Time arithmetic for the dashboard.
//
// The state has two levels on purpose:
//
//   domain — the range you *chose* first (a relative preset, or an absolute
//            from/to). The histogram spans exactly this, so the bracket can
//            only slide inside it.
//   window — the bracket inside the domain; this is what queries use.
//
// Changing the domain resets the window to the whole domain. Dragging the
// bracket narrows or moves the window without touching the domain. The compare
// view keeps two windows (baseline and comparison) in the same domain.

export type RangeMode = "relative" | "absolute";

export const RANGES: ReadonlyArray<readonly [string, string]> = [
  ["5m", "Last 5 minutes"],
  ["15m", "Last 15 minutes"],
  ["1h", "Last hour"],
  ["6h", "Last 6 hours"],
  ["24h", "Last 24 hours"],
  ["7d", "Last 7 days"],
];

export interface Window {
  from: number;
  until: number;
}

/** The fields every time calculation reads. */
export interface TimeState {
  mode: RangeMode;
  range: string;
  domainFrom: number;
  domainUntil: number;
  fromNs: number;
  untilNs: number;
  baseFromNs: number;
  baseUntilNs: number;
}

export function nowNs(): number {
  return Date.now() * 1e6;
}

/** `1h` / `30m` / `7d` in nanoseconds. */
export function durationNs(text: string): number {
  const match = /^(\d+)([smhd])$/.exec(String(text).trim());
  if (!match) return 3_600_000_000_000;
  const value = Number(match[1]);
  const scale: Record<string, number> = { s: 1e9, m: 6e10, h: 3.6e12, d: 8.64e13 };
  return value * scale[match[2]];
}

export function rangeLabel(preset: string): string {
  return RANGES.find(([value]) => value === preset)?.[1] ?? preset;
}

/** The range chosen first: a relative preset, or the absolute inputs. */
export function resolveDomain(state: TimeState): Window {
  if (state.mode === "absolute" && state.domainUntil > state.domainFrom) {
    return { from: state.domainFrom, until: state.domainUntil };
  }
  const until = nowNs();
  return { from: until - durationNs(state.range), until };
}

/** The bracket, clamped into the domain; falls back to the whole domain. */
export function resolveWindow(state: TimeState): Window {
  const domain = resolveDomain(state);
  if (!state.fromNs || !state.untilNs || state.untilNs <= state.fromNs) return domain;
  const span = Math.min(state.untilNs - state.fromNs, domain.until - domain.from);
  let from = state.fromNs;
  if (from < domain.from) from = domain.from;
  if (from + span > domain.until) from = domain.until - span;
  return { from, until: from + span };
}

/** RFC3339, the format the server's time parser accepts. */
export function iso(ns: number): string {
  return new Date(Number(ns) / 1e6).toISOString();
}

/** `2026-11-12T20:15:00` in the browser's zone, for <input type=datetime-local>. */
export function toLocalInput(ns: number): string {
  const date = new Date(Number(ns) / 1e6);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(
    date.getHours(),
  )}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

export function fromLocalInput(value: string): number | null {
  if (!value) return null;
  const ms = new Date(value).getTime();
  return Number.isFinite(ms) ? ms * 1e6 : null;
}

/** The window the baseline brush starts with, always inside the domain. */
export function defaultBaseline(state: TimeState): Window & { split: number | null } {
  const domain = resolveDomain(state);
  const win = resolveWindow(state);
  const span = win.until - win.from;
  const full = span >= (domain.until - domain.from) * 0.999;
  if (full) {
    // Nothing precedes a full-domain window: compare the two halves.
    const mid = Math.floor((domain.from + domain.until) / 2);
    return { from: domain.from, until: mid, split: mid };
  }
  let from = win.from - span;
  let until = win.from;
  if (from < domain.from) from = domain.from;
  if (until - from < span * 0.25) {
    from = domain.from;
    until = Math.min(domain.from + span, win.from);
  }
  return { from, until, split: null };
}

export function resolveBaseline(state: TimeState): Window {
  const domain = resolveDomain(state);
  if (state.baseFromNs && state.baseUntilNs > state.baseFromNs) {
    const span = Math.min(state.baseUntilNs - state.baseFromNs, domain.until - domain.from);
    let from = Math.max(domain.from, state.baseFromNs);
    if (from + span > domain.until) from = domain.until - span;
    return { from, until: Math.round(from + span) };
  }
  return defaultBaseline(state);
}

export function domainSpanText(domain: Window): string {
  const span = domain.until - domain.from;
  const days = span / 86_400e9;
  if (days >= 1) return `${days.toFixed(days >= 10 ? 0 : 1)} days`;
  const hours = span / 3.6e12;
  if (hours >= 1) return `${hours.toFixed(hours >= 10 ? 0 : 1)} h`;
  return `${Math.round(span / 6e10)} min`;
}

/**
 * Entering compare with a full-domain window has no baseline to show, so the
 * domain is split in half and the second half becomes the window.
 *
 * Returns the fields to patch, or null when the state already has a baseline.
 */
export function prepareCompare(
  state: TimeState,
): Pick<TimeState, "fromNs" | "untilNs" | "baseFromNs" | "baseUntilNs"> | null {
  const domain = resolveDomain(state);
  const win = resolveWindow(state);
  const full = win.until - win.from >= (domain.until - domain.from) * 0.999;
  if (!state.baseFromNs && full) {
    const split = defaultBaseline(state);
    if (split.split) {
      return {
        fromNs: split.split,
        untilNs: domain.until,
        baseFromNs: domain.from,
        baseUntilNs: split.split,
      };
    }
  } else if (!state.baseFromNs) {
    const baseline = defaultBaseline(state);
    return {
      fromNs: state.fromNs,
      untilNs: state.untilNs,
      baseFromNs: baseline.from,
      baseUntilNs: baseline.until,
    };
  }
  return null;
}
