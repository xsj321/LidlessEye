"use client";

// Data behind the node panel.
//
// Four queries, fired together when a node is opened: the node's own history,
// the CPU profiles behind it, whatever memory profile type the server has, and
// the top functions of the same window. They are cached per (node, window) so
// switching tabs or walking to a neighbour re-renders instantly instead of
// re-querying four times.
//
// The selector is the interesting part: a profile series is labelled by
// `k8s_namespace` + `k8s_workload` for a workload and by `service_name` for a
// ClusterIP, which is the same split the map uses to build node ids. Getting it
// wrong is silent — the panel simply shows an empty chart.

import { useEffect, useRef, useState } from "react";

import { api } from "@/lib/api";
import { composeSelector } from "@/lib/selector";
import { settingsStore } from "@/lib/settings";
import { profileFilters } from "@/lib/servicemap-graph";
import { iso } from "@/lib/time";
import type {
  FlamegraphPayload,
  LabelFilter,
  MapHistoryPayload,
  ServiceMapNode,
  TimeseriesPayload,
  TopPayload,
} from "@/lib/types";

export interface ProfileUnit {
  name: string;
  unit: string;
}

export interface NodeData {
  history: MapHistoryPayload | null;
  cpu: TimeseriesPayload | null;
  memory: TimeseriesPayload | null;
  /** Which profile type backed the memory chart, if any. */
  memoryType: string | null;
  top: TopPayload | null;
  /** This node's own stack tree, filtered by the same labels as `top`. */
  flamegraph: FlamegraphPayload | null;
  units: ProfileUnit[];
  loading: boolean;
  error: string | null;
}

const EMPTY: NodeData = {
  history: null,
  cpu: null,
  memory: null,
  memoryType: null,
  top: null,
  flamegraph: null,
  units: [],
  loading: false,
  error: null,
};

/** Profile types that are not CPU but describe the heap. */
const MEMORY_HINT = /mem|rss|heap|alloc|object|resident/i;

const BUCKETS = 72;

const cache = new Map<string, NodeData>();
const MAX_ENTRIES = 24;

function cachePut(key: string, value: NodeData) {
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
  const [data, setData] = useState<NodeData>(EMPTY);
  const token = useRef(0);
  // Timestamps in the request are ISO strings on the wire; the cache key has
  // to be the resolved window, not the object identity.
  const key = node ? `${node.id}|${window.from}|${window.until}` : "";

  useEffect(() => {
    if (!node) {
      setData(EMPTY);
      return;
    }
    const cached = cache.get(key);
    if (cached) {
      setData(cached);
      return;
    }

    const current = ++token.current;
    const selector = selectorOf(node);
    // The API parses a bare number as unix *seconds*; nanoseconds have to be
    // labelled or sent as an ISO timestamp. `iso()` is what every other view
    // uses, and it keeps the request readable in a network panel.
    const params: Record<string, string | number> = {
      from: iso(window.from),
      until: iso(window.until),
      buckets: BUCKETS,
    };

    setData({ ...EMPTY, loading: true });

    const load = async () => {
      const historyPromise = api.serviceMapHistory({ ...params, node: node.id });
      const typesPromise = api.profileTypes({ from: params.from, until: params.until });
      const cpuPromise = api.timeseries({ ...params, profile_type: "cpu", selector });
      const topPromise = api.top({ ...params, profile_type: "cpu", selector, limit: 12 });
      // An external node has no labels to match profiles; asking for a
      // flamegraph there would draw the whole cluster under the wrong name.
      const flamegraphPromise = selector
        ? api
            .flamegraph({
              ...params,
              profile_type: "cpu",
              selector,
              max_nodes: settingsStore.getSnapshot().flamegraphMaxNodes,
            })
            .then((result) => (result.total ? result : null))
            .catch(() => null)
        : Promise.resolve(null);

      const [history, types, cpu, top, flamegraph] = await Promise.all([
        historyPromise,
        typesPromise.catch(() => ({ profile_types: [] })),
        cpuPromise.catch(() => null),
        topPromise.catch(() => null),
        flamegraphPromise,
      ]);

      const units: ProfileUnit[] = (types.profile_types ?? []).map((row) => ({
        name: row.name,
        unit: row.unit,
      }));
      const memoryType = units.find((unit) => MEMORY_HINT.test(unit.name))?.name ?? null;
      const memory = memoryType
        ? await api
            .timeseries({ ...params, profile_type: memoryType, selector })
            .catch(() => null)
        : null;

      return { history, cpu, memory, memoryType, top, flamegraph, units };
    };

    load()
      .then((result) => {
        const ready: NodeData = { ...result, loading: false, error: null };
        cachePut(key, ready);
        if (current === token.current) setData(ready);
      })
      .catch((err: unknown) => {
        if (current !== token.current) return;
        setData({
          ...EMPTY,
          loading: false,
          error: err instanceof Error ? err.message : String(err),
        });
      });
  }, [key, node, window.from, window.until]);

  return data;
}

/** `[[ms, value], …]` from a sparse response, for a `time` axis. */
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
