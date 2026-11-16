"use client";

// The service map.
//
// ECharts' `graph` series draws on canvas and keeps pan/zoom in the browser, so
// a cluster with a few hundred services stays interactive without one DOM node
// per service — which is what a React-rendered node-link diagram would cost.
// Only the force layout is computed in JS; hit testing and the animation loop
// are ECharts' own.
//
// Clicking a node selects it; the panel on the right then offers the jump into
// the profiles of that service, filtered by the same labels the agent wrote.

import { useEffect, useMemo, useRef, useState } from "react";
import * as echarts from "echarts/core";
import { GraphChart } from "echarts/charts";
import { LegendComponent, TooltipComponent } from "echarts/components";
import { CanvasRenderer } from "echarts/renderers";
import { ArrowUpRight, Info } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Separator } from "@/components/ui/separator";
import { formatBytes, formatCount, formatDuration } from "@/lib/format";
import { dashboardStore } from "@/lib/store";
import type { LabelFilter, ServiceMapNode } from "@/lib/types";
import { useDashboard } from "@/lib/use-dashboard";

echarts.use([GraphChart, TooltipComponent, LegendComponent, CanvasRenderer]);

const COLORS = {
  workload: "#6ba7f7",
  service: "#b48ef2",
  external: "#6b7280",
  hot: "#f2a65a",
  text: "#e7eaf0",
  muted: "#96a0b2",
  border: "#232b36",
  card: "#151a21",
};

/** Blue -> orange, so a service that burns CPU stands out from a busy one. */
function heatColor(cpu: number, max: number): string {
  if (!max || cpu <= 0) return COLORS.workload;
  const t = Math.min(1, Math.sqrt(cpu / max));
  const from = [0x6b, 0xa7, 0xf7];
  const to = [0xf2, 0xa6, 0x5a];
  const mix = from.map((channel, i) => Math.round(channel + (to[i] - channel) * t));
  return `rgb(${mix.join(",")})`;
}

function nodeLabel(node: ServiceMapNode): string {
  if (node.kind === "external") return "external";
  return node.service || node.workload || node.id;
}

/** What a click on this node should filter the flamegraph by. */
function profileFilters(node: ServiceMapNode): LabelFilter[] | null {
  if (node.kind === "external") return null;
  if (node.kind === "service") {
    return node.service ? [{ label: "service_name", op: "=", value: node.service }] : null;
  }
  const name = node.workload || node.service;
  if (!name) return null;
  const filters: LabelFilter[] = [];
  if (node.namespace) filters.push({ label: "k8s_namespace", op: "=", value: node.namespace });
  filters.push({ label: "k8s_workload", op: "=", value: name });
  return filters;
}

export function ServiceMapView() {
  const state = useDashboard();
  const map = state.serviceMap;
  const container = useRef<HTMLDivElement | null>(null);
  const chart = useRef<echarts.ECharts | null>(null);
  const [selected, setSelected] = useState<string | null>(null);

  const selectedNode = useMemo(
    () => map?.nodes.find((node) => node.id === selected) ?? null,
    [map, selected],
  );

  // Rebuild the chart when the data changes.
  useEffect(() => {
    const element = container.current;
    if (!element || !map) return;

    const instance = chart.current ?? echarts.init(element, undefined, { renderer: "canvas" });
    chart.current = instance;

    const maxCpu = Math.max(1, ...map.nodes.map((node) => node.cpu_ns ?? 0));
    const known = new Set(map.nodes.map((node) => node.id));

    const nodes = map.nodes.map((node) => {
      const connections = node.connections ?? 0;
      const size = 10 + Math.log10(1 + connections) * 9 + (node.kind === "external" ? 2 : 0);
      return {
        id: node.id,
        name: nodeLabel(node),
        value: connections,
        symbolSize: Math.min(46, size),
        itemStyle: {
          color:
            node.kind === "external" ? COLORS.external : heatColor(node.cpu_ns ?? 0, maxCpu),
          borderColor: COLORS.card,
          borderWidth: 2,
          opacity: node.kind === "external" ? 0.75 : 1,
        },
        label: { color: node.kind === "external" ? COLORS.muted : COLORS.text },
        // Carried through so the tooltip and the click handler need no lookup.
        raw: node,
      };
    });

    const links = map.edges
      .filter((edge) => known.has(edge.client) && known.has(edge.server))
      .map((edge) => ({
        source: edge.client,
        target: edge.server,
        value: edge.connections,
        name: edge.protocol,
        lineStyle: {
          width: 0.8 + Math.min(5, Math.log10(1 + edge.connections) * 1.6),
          color: edge.retransmits ? COLORS.hot : "#3a4553",
          opacity: 0.6,
          curveness: 0.12,
        },
      }));

    instance.setOption(
      {
        backgroundColor: "transparent",
        animationDuration: 500,
        animationEasingUpdate: "quinticInOut",
        tooltip: {
          confine: true,
          backgroundColor: COLORS.card,
          borderColor: COLORS.border,
          textStyle: { color: COLORS.text, fontSize: 11 },
          formatter: (params: unknown) => {
            const p = params as { dataType?: string; data?: Record<string, unknown> };
            if (p.dataType === "edge") {
              const data = p.data as { name?: string; value?: number };
              return `${data.name ?? "tcp"}<br/>${formatCount(data.value ?? 0)} connections`;
            }
            const raw = (p.data as { raw?: ServiceMapNode }).raw;
            if (!raw) return "";
            const lines = [
              `<b>${nodeLabel(raw)}</b>`,
              raw.namespace ? `${raw.namespace} · ${raw.kind}` : String(raw.kind ?? ""),
            ];
            if (raw.cpu_ns) lines.push(`CPU ${formatDuration(raw.cpu_ns)}`);
            if (raw.connections) lines.push(`${formatCount(raw.connections)} new connections`);
            if (raw.rx_bytes || raw.tx_bytes) {
              lines.push(`${formatBytes(raw.rx_bytes ?? 0)} in · ${formatBytes(raw.tx_bytes ?? 0)} out`);
            }
            if (raw.retransmits) lines.push(`${formatCount(raw.retransmits)} retransmits`);
            return lines.filter(Boolean).join("<br/>");
          },
        },
        series: [
          {
            type: "graph",
            layout: "force",
            roam: true,
            draggable: true,
            // A graph laid out with roam stays where the layout put it, so the
            // initial spread has to fit: magnification would push the leaves
            // past the canvas edge, where they simply are not drawn.
            zoom: 1,
            force: { repulsion: 420, edgeLength: [80, 170], gravity: 0.09 },
            emphasis: { focus: "adjacency", lineStyle: { opacity: 0.9 } },
            label: {
              show: true,
              position: "right",
              distance: 4,
              fontSize: 10,
              formatter: (params: unknown) => (params as { name?: string }).name ?? "",
            },
            // Two pods of one workload can sit on the same coordinates while the
            // layout settles; a hidden label beats two unreadable ones.
            labelLayout: { hideOverlap: true },
            edgeSymbol: ["none", "arrow"],
            edgeSymbolSize: [0, 5],
            edgeLabel: {
              show: map.edges.length <= 24,
              fontSize: 9,
              color: COLORS.muted,
              formatter: (params: unknown) => (params as { name?: string }).name ?? "",
            },
            data: nodes,
            links,
          },
        ],
      },
      { notMerge: true },
    );

    instance.off("click");
    instance.on("click", (params: unknown) => {
      const p = params as { dataType?: string; data?: { id?: string } };
      if (p.dataType === "node" && p.data?.id) setSelected(p.data.id);
    });

    const observer = new ResizeObserver(() => instance.resize());
    observer.observe(element);
    return () => observer.disconnect();
  }, [map]);

  // ECharts owns a canvas outside React's tree; drop it on unmount.
  useEffect(() => {
    return () => {
      chart.current?.dispose();
      chart.current = null;
    };
  }, []);

  if (!map) {
    return (
      <EmptyState
        info={
          state.empty ?? {
            title: "No service map yet",
            hint: "The map is built by the agent from the pods' TCP tables and stored with the profiles.",
          }
        }
      />
    );
  }

  const totalConnections = map.edges.reduce((sum, edge) => sum + edge.connections, 0);
  const workloads = map.nodes.filter((node) => node.kind === "workload").length;
  const services = map.nodes.filter((node) => node.kind === "service").length;
  const filters = selectedNode ? profileFilters(selectedNode) : null;

  return (
    <div className="flex min-h-0 flex-1 gap-3">
      <div className="flex min-w-0 flex-1 flex-col gap-2">
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-[11px] text-muted-foreground">
          <span data-testid="map-summary">
            {workloads} workloads · {services} services · {map.edges.length} edges ·{" "}
            {formatCount(totalConnections)} new connections
          </span>
          <span className="flex items-center gap-1">
            <span className="inline-block size-2 rounded-full" style={{ background: COLORS.workload }} />
            workload
          </span>
          <span className="flex items-center gap-1">
            <span className="inline-block size-2 rounded-full" style={{ background: COLORS.service }} />
            ClusterIP service
          </span>
          <span className="flex items-center gap-1">
            <span className="inline-block size-2 rounded-full" style={{ background: COLORS.external }} />
            external
          </span>
          <span className="flex items-center gap-1">
            <span className="inline-block size-2 rounded-full" style={{ background: COLORS.hot }} />
            retransmits
          </span>
          {map.truncated ? <Badge variant="secondary">clipped to 600 nodes</Badge> : null}
        </div>
        <div
          ref={container}
          className="min-h-[420px] flex-1 rounded-lg border bg-card/40"
          data-testid="service-map-canvas"
        />
      </div>

      <Card className="w-72 shrink-0 gap-2 p-3" data-testid="map-detail">
        {selectedNode ? (
          <>
            <div className="flex items-start justify-between gap-2">
              <div className="min-w-0">
                <div className="truncate text-sm font-medium">{nodeLabel(selectedNode)}</div>
                <div className="truncate text-[11px] text-muted-foreground">
                  {selectedNode.namespace || "—"} · {selectedNode.kind}
                </div>
              </div>
              <Badge variant="outline" className="shrink-0 text-[10px]">
                {selectedNode.id}
              </Badge>
            </div>
            <Separator />
            <dl className="grid grid-cols-2 gap-y-1.5 text-[11px]">
              <dt className="text-muted-foreground">CPU</dt>
              <dd className="text-right font-mono">
                {selectedNode.cpu_ns ? formatDuration(selectedNode.cpu_ns) : "—"}
              </dd>
              <dt className="text-muted-foreground">Connections</dt>
              <dd className="text-right font-mono">{formatCount(selectedNode.connections ?? 0)}</dd>
              <dt className="text-muted-foreground">In / out</dt>
              <dd className="text-right font-mono">
                {formatBytes(selectedNode.rx_bytes ?? 0)} / {formatBytes(selectedNode.tx_bytes ?? 0)}
              </dd>
              <dt className="text-muted-foreground">Retransmits</dt>
              <dd className="text-right font-mono">{formatCount(selectedNode.retransmits ?? 0)}</dd>
            </dl>
            {filters ? (
              <Button
                size="sm"
                className="mt-1 w-full"
                data-testid="map-profile"
                onClick={() => dashboardStore.showProfile(filters)}
              >
                <ArrowUpRight />
                Profile this service
              </Button>
            ) : (
              <p className="text-[11px] text-muted-foreground">
                Traffic to or from outside the cluster has no profiles behind it.
              </p>
            )}
            <p className="text-[10px] text-muted-foreground">
              The window is the one selected above, the same as the flamegraph.
            </p>
          </>
        ) : (
          <>
            <div className="flex items-center gap-1.5 text-sm font-medium">
              <Info className="size-3.5 text-muted-foreground" />
              Service map
            </div>
            <p className="text-[11px] text-muted-foreground">
              Node size is new connections, colour is on-CPU time from the profiles. Click a node
              to see its traffic and jump to its flamegraph.
            </p>
            <Separator />
            <div className="space-y-1">
              {[...map.nodes]
                .sort((a, b) => (b.connections ?? 0) - (a.connections ?? 0))
                .slice(0, 8)
                .map((node) => (
                  <button
                    key={node.id}
                    type="button"
                    onClick={() => setSelected(node.id)}
                    className="flex w-full items-center justify-between gap-2 rounded px-1 py-0.5 text-left text-[11px] hover:bg-secondary/60"
                  >
                    <span className="truncate">{nodeLabel(node)}</span>
                    <span className="shrink-0 font-mono text-muted-foreground">
                      {formatCount(node.connections ?? 0)}
                    </span>
                  </button>
                ))}
            </div>
          </>
        )}
      </Card>
    </div>
  );
}
