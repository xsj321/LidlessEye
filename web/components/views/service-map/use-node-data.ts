"use client";

// Data behind the node panel.
//
// The panel asks two independent questions, so they are loaded separately:
//
//   * network history — the node's own counters, plus an optional DNS-filtered
//     copy for the DNS tab;
//   * profiles — time series, top functions and a flamegraph for one selected
//     profile type.
//
// Keeping profile types separate matters. A node can have CPU, off-CPU and
// four heap types in the same window, and a silent heuristic cannot answer
// which one the operator meant to inspect. The tabs now ask for the type they
// need and expose a native select for the rest.

import { useCallback, useEffect, useState } from "react";

import { api } from "@/lib/api";
import { composeSelector } from "@/lib/selector";
import { settingsStore } from "@/lib/settings";
import { profileFilters } from "@/lib/servicemap-graph";
import { iso } from "@/lib/time";
import type {
  FlamegraphPayload,
  LabelFilter,
  MapHistoryPayload,
  ServiceInstance,
  ServiceMapNode,
  TimeseriesPayload,
  TopPayload,
} from "@/lib/types";

export interface ProfileUnit {
  name: string;
  unit: string;
  totalValue?: number;
  batchCount?: number;
  virtual?: boolean;
}

export interface NodeData {
  history: MapHistoryPayload | null;
  /** The same window restricted to `protocol=dns`, for the DNS tab. */
  dnsHistory: MapHistoryPayload | null;
  /** Pods behind the node; read with the history, not with the map. */
  instances: ServiceInstance[];
  /** Container names behind the node, for the per-container rows. */
  containers: string[];
  /** Profile type behind `profile`, `top` and `flamegraph`. */
  profileType: string | null;
  /** All types with data behind this node in the selected window. */
  units: ProfileUnit[];
  profile: TimeseriesPayload | null;
  top: TopPayload | null;
  flamegraph: FlamegraphPayload | null;
  loading: boolean;
  profileLoading: boolean;
  error: string | null;
  setProfileType: (name: string) => void;
}

interface BaseData {
  history: MapHistoryPayload | null;
  dnsHistory: MapHistoryPayload | null;
  instances: ServiceInstance[];
  containers: string[];
  units: ProfileUnit[];
}

interface ProfileData {
  profile: TimeseriesPayload | null;
  top: TopPayload | null;
  flamegraph: FlamegraphPayload | null;
}

const EMPTY: NodeData = {
  history: null,
  dnsHistory: null,
  instances: [],
  containers: [],
  profileType: null,
  units: [],
  profile: null,
  top: null,
  flamegraph: null,
  loading: false,
  profileLoading: false,
  error: null,
  setProfileType: () => {},
};

/** Types whose values are nanoseconds of wall-clock time. */
export const CPU_TYPES = ["cpu", "off_cpu", "wall"] as const;

/** Heap types the agent can produce, in the order an operator reads them. */
export const MEMORY_TYPES = [
  "inuse_space",
  "alloc_space",
  "inuse_objects",
  "alloc_objects",
] as const;

const MEMORY_HINT = /mem|rss|heap|alloc|object|resident/i;

export function isCpuType(name: string | null | undefined): boolean {
  return Boolean(name && (CPU_TYPES as readonly string[]).includes(name));
}

export function isMemoryType(unit: ProfileUnit): boolean {
  return (
    (MEMORY_TYPES as readonly string[]).includes(unit.name) ||
    unit.unit === "bytes" ||
    unit.unit === "count" ||
    MEMORY_HINT.test(unit.name)
  );
}

/**
 * Which heap type to show first. "What is this service holding?" is the
 * question a node panel is asked, so in-use bytes wins over an allocation
 * rate, and bytes win over object counts because the axis is formatted that
 * way. The dropdown still offers every type present.
 */
export function pickMemoryType(units: ProfileUnit[]): string | null {
  for (const name of MEMORY_TYPES) {
    if (units.some((unit) => unit.name === name)) return name;
  }
  return units.find(isMemoryType)?.name ?? null;
}

/** CPU is the default question; a heap-only workload still gets a usable tab. */
function defaultProfileType(units: ProfileUnit[]): string | null {
  for (const name of CPU_TYPES) {
    if (units.some((unit) => unit.name === name)) return name;
  }
  return units[0]?.name ?? null;
}

const BUCKETS = 72;
const MAX_ENTRIES = 24;

const baseCache = new Map<string, BaseData>();
const profileCache = new Map<string, ProfileData>();

function cachePut<T>(cache: Map<string, T>, key: string, value: T) {
  if (cache.size >= MAX_ENTRIES) {
    const oldest = cache.keys().next().value;
    if (oldest) cache.delete(oldest);
  }
  cache.set(key, value);
}

export function selectorsFor(node: ServiceMapNode): LabelFilter[] {
  return profileFilters(node) ?? [];
}

/** The labels a node's profiles carry, as a selector string. */
export function selectorOf(node: ServiceMapNode): string {
  return composeSelector(selectorsFor(node));
}

export function useNodeData(
  node: ServiceMapNode | null,
  window: { from: number; until: number },
): NodeData {
  const [base, setBase] = useState<BaseData | null>(null);
  const [profileType, setProfileTypeState] = useState<string | null>(null);
  const [profileData, setProfileData] = useState<ProfileData | null>(null);
  const [baseLoading, setBaseLoading] = useState(false);
  const [profileLoading, setProfileLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const selector = node ? selectorOf(node) : "";
  // Timestamps in the request are ISO strings on the wire; the cache key has
  // to be the resolved window, not the object identity.
  const key = node
    ? `${node.id}|${window.from}|${window.until}|${selector}`
    : "";

  const setProfileType = useCallback((name: string) => {
    setProfileTypeState((current) => (current === name ? current : name));
  }, []);

  useEffect(() => {
    setBase(null);
    setProfileTypeState(null);
    setProfileData(null);
    setError(null);
  }, [key]);

  useEffect(() => {
    if (!node) return;
    const cached = baseCache.get(key);
    if (cached) {
      setBase(cached);
      setProfileTypeState((current) =>
        current && cached.units.some((unit) => unit.name === current)
          ? current
          : defaultProfileType(cached.units),
      );
      return;
    }

    const controller = new AbortController();
    const signal = controller.signal;
    // The API parses a bare number as unix *seconds*; nanoseconds have to be
    // labelled or sent as an ISO timestamp. `iso()` is what every other view
    // uses, and it keeps the request readable in a network panel.
    const params: Record<string, string | number> = {
      from: iso(window.from),
      until: iso(window.until),
      buckets: BUCKETS,
    };

    setBaseLoading(true);
    setError(null);

    const load = async (): Promise<BaseData> => {
      const historyPromise = api.serviceMapHistory(
        { ...params, node: node.id },
        signal,
      );
      // DNS is an application-protocol filter on the same buckets. If an older
      // server ignores it, the tab still works off the edge list and says so.
      const dnsPromise = api
        .serviceMapHistory({ ...params, node: node.id, protocol: "dns" }, signal)
        .catch(() => null);
      const typesPromise = selector
        ? api.profileTypes({ ...params, selector }, signal)
        : Promise.resolve({ profile_types: [] });

      const [history, dnsHistory, types] = await Promise.all([
        historyPromise,
        dnsPromise,
        typesPromise.catch(() => ({ profile_types: [] })),
      ]);
      return {
        history,
        dnsHistory,
        instances: history.instances ?? [],
        containers: history.containers ?? [],
        units: (types.profile_types ?? []).map((row) => ({
          name: row.name,
          unit: row.unit,
          totalValue: row.total_value,
          batchCount: row.batch_count,
          virtual: row.virtual,
        })),
      };
    };

    load()
      .then((value) => {
        cachePut(baseCache, key, value);
        if (signal.aborted) return;
        setBase(value);
        setProfileTypeState((current) =>
          current && value.units.some((unit) => unit.name === current)
            ? current
            : defaultProfileType(value.units),
        );
      })
      .catch((err: unknown) => {
        if (signal.aborted) return;
        setError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (!signal.aborted) setBaseLoading(false);
      });

    return () => controller.abort();
  }, [key, node, selector, window.from, window.until]);

  useEffect(() => {
    if (!node || !selector || !profileType) {
      setProfileData(null);
      return;
    }
    const profileKey = `${key}|${profileType}`;
    const cached = profileCache.get(profileKey);
    if (cached) {
      setProfileData(cached);
      return;
    }

    const controller = new AbortController();
    const signal = controller.signal;
    const params: Record<string, string | number> = {
      from: iso(window.from),
      until: iso(window.until),
      buckets: BUCKETS,
      profile_type: profileType,
      selector,
    };
    setProfileLoading(true);
    setProfileData(null);

    const load = async (): Promise<ProfileData> => {
      const [profile, top, flamegraph] = await Promise.all([
        api.timeseries(params, signal),
        api.top({ ...params, limit: 12 }, signal).catch(() => null),
        api
          .flamegraph(
            { ...params, max_nodes: settingsStore.getSnapshot().flamegraphMaxNodes },
            signal,
          )
          .then((result) => (result.total ? result : null))
          .catch(() => null),
      ]);
      return { profile, top, flamegraph };
    };

    load()
      .then((value) => {
        cachePut(profileCache, profileKey, value);
        if (!signal.aborted) setProfileData(value);
      })
      .catch((err: unknown) => {
        if (signal.aborted) return;
        setError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (!signal.aborted) setProfileLoading(false);
      });

    return () => controller.abort();
  }, [key, node, selector, profileType, window.from, window.until]);

  return {
    history: base?.history ?? null,
    dnsHistory: base?.dnsHistory ?? null,
    instances: base?.instances ?? [],
    containers: base?.containers ?? [],
    profileType,
    units: base?.units ?? [],
    profile: profileData?.profile ?? null,
    top: profileData?.top ?? null,
    flamegraph: profileData?.flamegraph ?? null,
    loading: baseLoading || Boolean(profileType && profileLoading),
    profileLoading,
    error,
    setProfileType,
  };
}

/** `[[ms, value], …]` from one series of a sparse response. */
export function toTimePoints(
  payload: TimeseriesPayload | null,
  series: number,
): Array<[number, number]> {
  const row = payload?.series?.[series];
  if (!row) return [];
  return row.points
    .map((value, index): [number, number] => [
      (payload.from_ns + index * payload.step_ns) / 1e6,
      value,
    ])
    .filter(([, value]) => Number.isFinite(value));
}

/** Sum of a set of series points: one line instead of n. */
export function totalTimePoints(payload: TimeseriesPayload | null): Array<[number, number]> {
  if (!payload?.series?.length) return [];
  const length = Math.max(...payload.series.map((row) => row.points.length));
  const out: Array<[number, number]> = [];
  for (let index = 0; index < length; index += 1) {
    let sum = 0;
    for (const row of payload.series) sum += row.points[index] ?? 0;
    out.push([(payload.from_ns + index * payload.step_ns) / 1e6, sum]);
  }
  return out;
}

export function seriesTotal(payload: TimeseriesPayload | null): number {
  if (!payload?.series?.length) return 0;
  return payload.series.reduce(
    (sum, row) => sum + row.points.reduce((inner, value) => inner + value, 0),
    0,
  );
}
