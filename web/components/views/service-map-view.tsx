"use client";

// The service map.
//
// A layered graph read left to right: entry points on the left, what they call
// to their right. Nodes are DOM cards (`Card`), edges are one SVG, and both sit
// in a single transformed layer so pan and zoom are a CSS transform rather than
// a re-layout.
//
// The map answers "who talks to whom, how much, and how fast" — every edge is
// labelled with its rate and its round-trip time — and a click opens the panel
// that answers "and how is it doing", one hop of neighbours plus seven tabs of
// measurements.

import { useCallback, useEffect, useMemo, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { EmptyState } from "@/components/empty-state";
import { formatCount, formatDuration } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { KIND, nodeLabel } from "@/lib/servicemap-graph";
import { dashboardStore } from "@/lib/store";
import type { ServiceMapNode } from "@/lib/types";
import { useDashboard } from "@/lib/use-dashboard";

import { ServiceGraph } from "./service-map/graph-canvas";
import { NodePanel } from "./service-map/node-panel";

function LegendSwatch({ kind }: { kind: keyof typeof KIND }) {
  return (
    <span className="relative inline-flex h-2.5 w-4 overflow-hidden rounded-[3px] border border-white/10">
      <span className="h-full w-[2px]" style={{ background: KIND[kind].bar }} />
      <span className="h-full flex-1" style={{ background: KIND[kind].tint }} />
    </span>
  );
}

function StatusGlyphLegend({ tone }: { tone: "ok" | "warn" | "muted" }) {
  const color = tone === "ok" ? "#4fbf8b" : tone === "warn" ? "#f2a65a" : "#6b7585";
  return (
    <svg viewBox="0 0 12 12" className="size-2.5" aria-hidden="true">
      {tone === "ok" ? <circle cx="6" cy="6" r="4" fill={color} /> : null}
      {tone === "warn" ? <path d="M6 1.3 11.2 10.5H0.8z" fill={color} /> : null}
      {tone === "muted" ? (
        <circle cx="6" cy="6" r="3.5" fill="none" stroke={color} strokeWidth="1.4" />
      ) : null}
    </svg>
  );
}

export function ServiceMapView() {
  const state = useDashboard();
  const t = useT();
  const map = state.serviceMap;
  const [selected, setSelected] = useState<string | null>(null);

  const byId = useMemo(
    () => new Map<string, ServiceMapNode>((map?.nodes ?? []).map((node) => [node.id, node])),
    [map],
  );

  // A refresh can drop the node the panel is showing (the window moved, the
  // workload went away). Closing the panel is the honest reaction: the numbers
  // it would keep displaying describe a node that is no longer on the map.
  useEffect(() => {
    if (selected && !byId.has(selected)) setSelected(null);
  }, [byId, selected]);

  const onSelect = useCallback((id: string | null) => {
    setSelected(id);
  }, []);

  const onSelectRequired = useCallback((id: string) => {
    setSelected(id);
  }, []);

  const close = useCallback(() => setSelected(null), []);

  // Pinned to the payload, not recomputed per render: a relative range moves
  // with the clock, and a window that changes on every render would make the
  // panel's data hook re-query in a loop. Declared before the empty-state
  // return so the hook order does not change when the first payload lands.
  const range = useMemo(
    () => (map ? dashboardStore.resolveWindow() : { from: 0, until: 0 }),
    [map],
  );

  if (!map) {
    return (
      <EmptyState
        info={
          state.empty
            ? {
                ...state.empty,
                title: t(state.empty.title),
                hint: t(state.empty.hint),
              }
            : {
                title: t("No service map yet"),
                hint: t(
                  "The map is built by the agent from the pods' TCP tables and stored with the profiles.",
                ),
              }
        }
      />
    );
  }

  const windowSec = Math.max(1, (range.until - range.from) / 1e9);
  const totalConnections = map.edges.reduce((sum, edge) => sum + (edge.connections ?? 0), 0);
  const workloads = map.nodes.filter((node) => node.kind === "workload").length;
  const services = map.nodes.filter((node) => node.kind === "service").length;
  const selectedNode = selected ? (byId.get(selected) ?? null) : null;
  const cpuOfSelected = selectedNode?.cpu_ns ?? 0;

  return (
    <div className="relative flex h-full min-h-0 flex-col gap-2">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1 pr-1 text-[11px] text-muted-foreground">
        <span data-testid="map-summary">
          {workloads} {t("workloads")} · {services} {t("services")} · {map.edges.length}{" "}
          {t("edges")} · {formatCount(totalConnections)} {t("new connections")}
        </span>
        <span className="flex items-center gap-1">
          <LegendSwatch kind="workload" />
          {t("workload")}
        </span>
        <span className="flex items-center gap-1">
          <LegendSwatch kind="service" />
          ClusterIP
        </span>
        <span className="flex items-center gap-1">
          <LegendSwatch kind="external" />
          {t("external")}
        </span>
        <span className="flex items-center gap-1">
          <StatusGlyphLegend tone="ok" />
          {t("all ready")}
        </span>
        <span className="flex items-center gap-1">
          <StatusGlyphLegend tone="warn" />
          {t("degraded")}
        </span>
        <span className="flex items-center gap-1">
          <StatusGlyphLegend tone="muted" />
          {t("no replicas")}
        </span>
        <span className="flex items-center gap-1">
          <span className="inline-block h-[2px] w-4 rounded bg-[#f2a65a]" />
          CPU
        </span>
        <span className="text-muted-foreground/70">
          {t("edge labels: new connections/s · mean RTT")}
        </span>
        {map.truncated ? <Badge variant="secondary">{t("clipped to 600 nodes")}</Badge> : null}
        {selectedNode ? (
          <span className="ml-auto flex items-center gap-2 text-[10px]">
            <span className="text-foreground">{nodeLabel(selectedNode)}</span>
            {cpuOfSelected ? <span>· CPU {formatDuration(cpuOfSelected)}</span> : null}
            <span>
              · {selectedNode.connections ?? 0} {t("connections")}
            </span>
          </span>
        ) : null}
      </div>

      <div className="min-h-0 flex-1" data-testid="service-map-canvas">
        <ServiceGraph
          nodes={map.nodes}
          edges={map.edges}
          windowSec={windowSec}
          selected={selected}
          onSelect={onSelect}
        />
      </div>

      <NodePanel
        node={selectedNode}
        nodes={byId}
        edges={map.edges}
        range={range}
        onSelect={onSelectRequired}
        onClose={close}
      />
    </div>
  );
}
