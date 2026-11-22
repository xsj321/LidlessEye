// User preferences that are not part of the URL.
//
// The hash is the source of truth for anything that describes *what you are
// looking at*: view, window, filters, tenant. Settings are different — they
// describe *how the tool behaves*, and following a link should not silently
// change them. Keeping them in localStorage makes a reload feel like the same
// tool, while keeping them out of the hash makes a shared link reproducible.
//
// Same shape as the dashboard store: one object, one subscribe channel, one
// write path that validates and persists.

"use client";

import { useSyncExternalStore } from "react";

import { RANGES } from "./time";
import type { View } from "./types";
import { VIEWS } from "./types";

export type Language = "en" | "zh";

export interface Settings {
  /** Interface language. A behaviour preference, so it stays out of the URL. */
  language: Language;
  /** Landing view when a URL carries no view (a bare/bookmarked root URL). */
  defaultView: View;
  /** Relative range used on a fresh load when the URL has no range. */
  defaultRange: string;
  /** Auto-refresh period in milliseconds. */
  autoRefreshMs: number;
  /** Draw rate/RTT pills on service-map edges. */
  edgeLabels: boolean;
  /** Zoom level from which edge labels appear (0 = always, 1 = never). */
  labelZoom: number;
  /** Cap on frames requested from /flamegraph. */
  flamegraphMaxNodes: number;
  /** Number of histogram buckets used by the time brush. */
  histogramBuckets: number;
}

export const DEFAULT_SETTINGS: Settings = {
  language: "en",
  defaultView: "servicemap",
  defaultRange: "1h",
  autoRefreshMs: 30_000,
  edgeLabels: true,
  // Labels are the third layer of information on an edge, after direction and
  // width. At the automatic fit of a wide map (around 0.72) there can be two
  // hundred of them, and they turn the drawing into a wall of pills: keep them
  // for the zoom level where a reader is actually reading one.
  labelZoom: 0.85,
  flamegraphMaxNodes: 50_000,
  histogramBuckets: 120,
};

const STORAGE_KEY = "lidlesseye.settings";

// Seconds-first cadences: operators watching a live incident want a fast
// heartbeat (5s), while idle dashboards should not hammer the server.
export const MIN_AUTO_REFRESH_MS = 5_000;
export const MAX_AUTO_REFRESH_MS = 3_600_000;
const AUTO_REFRESH_OPTIONS = [
  5_000,
  10_000,
  15_000,
  30_000,
  60_000,
  120_000,
  300_000,
] as const;
const MAX_NODES_OPTIONS = [10_000, 50_000, 100_000] as const;
const BUCKET_OPTIONS = [60, 120, 240] as const;

function pick<T>(value: unknown, options: readonly T[], fallback: T): T {
  return options.includes(value as T) ? (value as T) : fallback;
}

/** Auto-refresh period in ms, clamped to a [5s, 1h] safe window. */
function clampMs(value: unknown): number {
  const n = Number(value);
  if (!Number.isFinite(n) || n <= 0) return DEFAULT_SETTINGS.autoRefreshMs;
  return Math.min(MAX_AUTO_REFRESH_MS, Math.max(MIN_AUTO_REFRESH_MS, Math.round(n)));
}

/** Validate every field: localStorage is user-editable and survives upgrades. */
export function sanitiseSettings(raw: unknown): Settings {
  const value = (raw ?? {}) as Partial<Settings>;
  const zoom = Number(value.labelZoom);
  return {
    language: pick<Language>(value.language, ["en", "zh"], DEFAULT_SETTINGS.language),
    defaultView: pick<View>(value.defaultView, VIEWS, DEFAULT_SETTINGS.defaultView),
    defaultRange: pick(
      value.defaultRange,
      RANGES.map(([name]) => name),
      DEFAULT_SETTINGS.defaultRange,
    ),
    // A range clamp (not a whitelist) so the dialog can offer presets and a
    // free-form seconds input without the value being silently reset.
    autoRefreshMs: clampMs(value.autoRefreshMs),
    edgeLabels: typeof value.edgeLabels === "boolean" ? value.edgeLabels : DEFAULT_SETTINGS.edgeLabels,
    labelZoom: Number.isFinite(zoom)
      ? Math.min(1, Math.max(0, zoom))
      : DEFAULT_SETTINGS.labelZoom,
    flamegraphMaxNodes: pick(
      value.flamegraphMaxNodes,
      MAX_NODES_OPTIONS,
      DEFAULT_SETTINGS.flamegraphMaxNodes,
    ),
    histogramBuckets: pick(
      value.histogramBuckets,
      BUCKET_OPTIONS,
      DEFAULT_SETTINGS.histogramBuckets,
    ),
  };
}

function load(): Settings {
  if (typeof window === "undefined") return DEFAULT_SETTINGS;
  try {
    const stored = window.localStorage.getItem(STORAGE_KEY);
    return stored ? sanitiseSettings(JSON.parse(stored)) : DEFAULT_SETTINGS;
  } catch {
    return DEFAULT_SETTINGS;
  }
}

export class SettingsStore {
  private state: Settings = load();
  private readonly listeners = new Set<() => void>();

  getSnapshot = (): Settings => this.state;

  /** The server-rendered snapshot must not read browser storage. */
  getServerSnapshot = (): Settings => DEFAULT_SETTINGS;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  update(patch: Partial<Settings>) {
    this.state = sanitiseSettings({ ...this.state, ...patch });
    if (typeof window !== "undefined") {
      try {
        window.localStorage.setItem(STORAGE_KEY, JSON.stringify(this.state));
      } catch {
        // Private windows and blocked storage still get an in-memory setting.
      }
    }
    for (const listener of this.listeners) listener();
  }

  reset() {
    this.update(DEFAULT_SETTINGS);
  }
}

export const settingsStore = new SettingsStore();

export function useSettings(): Settings {
  return useSyncExternalStore(
    settingsStore.subscribe,
    settingsStore.getSnapshot,
    settingsStore.getServerSnapshot,
  );
}

export { AUTO_REFRESH_OPTIONS, BUCKET_OPTIONS, MAX_NODES_OPTIONS };
