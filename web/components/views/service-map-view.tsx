"use client";

// The service map.
//
// ECharts' `graph` series draws on canvas and keeps pan/zoom in the browser, so
// a cluster with a few hundred services stays interactive without one DOM node
// per service — which is what a React-rendered node-link diagram would cost.
// The layout is computed here, not by ECharts: a layered one (see
// `lib/servicemap-layout.ts`) reads left to right by dependency depth, which is
// what makes a topology legible.
//
// Clicking a node selects it, dims everything that is not its neighbour, and
// shows what it talks to — inbound and outbound edges with their protocol,
// connections, bytes and latency — plus the jump into its profiles.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as echarts from "echarts/core";
import { GraphChart } from "echarts/charts";
import { LegendComponent, TooltipComponent } from "echarts/components";
import { CanvasRenderer } from "echarts/renderers";
import { ArrowDownLeft, ArrowUpRight, Info } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Separator } from "@/components/ui/separator";
import { formatBytes, formatCount, formatDuration } from "@/lib/format";
import { layeredLayout } from "@/lib/servicemap-layout";
import { dashboardStore } from "@/lib/store";
import type { LabelFilter, ServiceMapEdge, ServiceMapNode } from "@/lib/types";
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

/** Pixels kept clear on each side: the rightmost column's label is drawn to the
 *  right of its node, so the graph must not reach the edge. */
const SIDE_ROOM = 150;

/** Blue -> orange, so a service that burns CPU stands out from a busy one. */function heatColor(cpu: number, max: number): string {
  if (!max || cpu <= 0) return COLORS.workload;
  const t = Math.min(1, Math.sqrt(cpu / max));
  const from = [0x6b, 0xa7, 0xf7];
  const to = [0xf2, 0xa6, 0x5a];
  const mix = from.map((channel, i) => Math.round(channel + (to[i] - channel) * t));
  return `rgb(${mix.join(",")})`;
}

function nodeLabel(node: ServiceMapNode | undefined): string {
  if (!node) return "—";
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

/** How many connections an edge is worth, for ordering and sizing. */
function weight(edge: ServiceMapEdge): number {
  return (edge.connections ?? 0) + (edge.tx_bytes ?? 0) / 4096;
}

/** One row of the inbound/outbound list. Clicking it selects the peer. */
function EdgeRow({
  edge,
  peerId,
  peer,
  direction,
  onSelect,
  testId,
}: {
  edge: ServiceMapEdge;
  peerId: string;
  peer: ServiceMapNode | undefined;
  direction: "in" | "out";
  onSelect: (id: string) => void;
  testId?: string;
}) {
  const bytes = direction === "in" ? (edge.tx_bytes ?? 0) : (edge.rx_bytes ?? 0);
  const label = nodeLabel(peer);
  const details = [
    label,
    `${formatCount(edge.connections ?? 0)} connections`,
    bytes ? `${formatBytes(bytes)}` : null,
    edge.rtt_us ? `${formatDuration(edge.rtt_us * 1000)} RTT` : null,
    edge.retransmits ? `${formatCount(edge.retransmits)} retransmits` : null,
  ]
    .filter(Boolean)
    .join(" · ");
  return (
    <button
      type="button"
      data-testid={testId}
      title={details}
      onClick={() => onSelect(peerId)}
      className="flex w-full items-center gap-1.5 rounded px-1 py-0.5 text-left text-[11px] hover:bg-secondary/60"
    >
      <span
        className="size-1.5 shrink-0 rounded-full"
        style={{ background: peer?.kind === "external" ? COLORS.external : COLORS.workload }}
      />
      <span className="min-w-0 flex-1 truncate">{label}</span>
      <span className="shrink-0 font-mono text-[10px] text-muted-foreground">
        {edge.protocol}
      </span>
      <span className="w-10 shrink-0 text-right font-mono">{formatCount(edge.connections ?? 0)}</span>
      <span className="w-12 shrink-0 text-right font-mono text-[10px] text-muted-foreground">
        {edge.rtt_us ? formatDuration(edge.rtt_us * 1000) : "—"}
      </span>
    </button>
  );
}

export function ServiceMapView() {
  const state = useDashboard();
  const map = state.serviceMap;
  const container = useRef<HTMLDivElement | null>(null);
  const chart = useRef<echarts.ECharts | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [canvas, setCanvas] = useState({ width: 0, height: 0 });

  const byId = useMemo(
    () => new Map((map?.nodes ?? []).map((node) => [node.id, node])),
    [map],
  );
  const selectedNode = selected ? (byId.get(selected) ?? null) : null;

  const inbound = useMemo(() => {
    if (!map || !selected) return [];
    return map.edges.filter((edge) => edge.server === selected).sort((a, b) => weight(b) - weight(a));
  }, [map, selected]);

  const outbound = useMemo(() => {
    if (!map || !selected) return [];
    return map.edges.filter((edge) => edge.client === selected).sort((a, b) => weight(b) - weight(a));
  }, [map, selected]);

  // Track the canvas box: the layout derives its gaps from it, so a wider
  // window spreads the columns instead of scaling the graph up.
  //
  // The ref is a callback, not a plain object ref: this component returns the
  // empty state (and no canvas) until the first payload arrives, so an effect
  // with `[]` deps would run while `container.current` is still null and never
  // observe anything.
  const observer = useRef<ResizeObserver | null>(null);
  const attachCanvas = useCallback((node: HTMLDivElement | null) => {
    container.current = node;
    observer.current?.disconnect();
    observer.current = null;
    if (!node) return;
    setCanvas({ width: node.clientWidth, height: node.clientHeight });
    const created = new ResizeObserver((entries) => {
      const box = entries[0]?.contentRect;
      if (!box) return;
      setCanvas((previous) =>
        Math.abs(previous.width - box.width) < 8 && Math.abs(previous.height - box.height) < 8
          ? previous
          : { width: box.width, height: box.height },
      );
    });
    created.observe(node);
    observer.current = created;
  }, []);

  // Rebuild the chart when the data or the canvas size changes.
  useEffect(() => {
    const element = container.current;
    if (!element || !map) return;
    // Before the first observation the box is unknown; measuring it now avoids
    // one frame laid out for the wrong aspect.
    const width = canvas.width || element.clientWidth;
    const height = canvas.height || element.clientHeight;
    if (width === 0 || height === 0) return;

    const instance = chart.current ?? echarts.init(element, undefined, { renderer: "canvas" });
    chart.current = instance;
    instance.resize();

    const maxCpu = Math.max(1, ...map.nodes.map((node) => node.cpu_ns ?? 0));
    const known = new Set(map.nodes.map((node) => node.id));
    // The label sits to the right of its node, so the drawing area stops short
    // of the container's right edge; otherwise the last column's labels are
    // painted past the canvas and clipped.
    const positions = layeredLayout(
      map.nodes.map((node) => ({ id: node.id, connections: node.connections ?? 0 })),
      map.edges,
      {
        width: Math.max(320, width - SIDE_ROOM * 2),
        height: Math.max(240, height - 24),
      },
    );

    const nodes = map.nodes.map((node) => {
      const connections = node.connections ?? 0;
      const size = 14 + Math.log10(1 + connections) * 7 + (node.kind === "external" ? 2 : 0);
      const point = positions.get(node.id);
      return {
        id: node.id,
        name: nodeLabel(node),
        value: connections,
        x: point?.x ?? 0,
        y: point?.y ?? 0,
        symbolSize: Math.min(42, size),
        itemStyle: {
          color:
            node.kind === "external" ? COLORS.external : heatColor(node.cpu_ns ?? 0, maxCpu),
          borderColor: COLORS.card,
          borderWidth: 2,
          opacity: node.kind === "external" ? 0.8 : 1,
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
        value: edge.connections ?? 0,
        name: edge.protocol,
        lineStyle: {
          width: 0.9 + Math.min(5, Math.log10(1 + (edge.connections ?? 0) + (edge.tx_bytes ?? 0) / 4096) * 1.6),
          color: edge.retransmits ? COLORS.hot : "#3a4553",
          opacity: 0.65,
          curveness: 0.1,
        },
      }));

    instance.setOption(
      {
        backgroundColor: "transparent",
        animationDuration: 400,
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
            // Positions come from the layered layout; letting ECharts fit the
            // bounding box means the column spacing scales with the canvas.
            layout: "none",
            roam: true,
            draggable: true,
            emphasis: { focus: "adjacency", lineStyle: { opacity: 0.95 } },
            label: {
              show: true,
              position: "right",
              distance: 6,
              fontSize: 11,
              // Workload names in a cluster are long (`kube-apiserver-desktop-
              // control-plane`); a fixed width keeps a label inside its column
              // instead of running into the next one. The tooltip and the panel
              // carry the full name.
              width: 140,
              overflow: "truncate",
              ellipsis: "…",
              formatter: (params: unknown) => (params as { name?: string }).name ?? "",
            },
            labelLayout: { hideOverlap: true },
            edgeSymbol: ["none", "arrow"],
            edgeSymbolSize: [0, 5],
            // The protocol on every edge turns the canvas into unreadable
            // hatching; the arrow shows the direction and the detail panel
            // lists the rest. The tooltip still names the protocol on hover.
            edgeLabel: { show: false },
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
  }, [map, canvas.width, canvas.height]);

  // Dim everything that is not the selected node's neighbour. Doing it with an
  // action instead of a rebuild keeps the pan and zoom the user chose.
  useEffect(() => {
    const instance = chart.current;
    if (!instance || !map) return;
    instance.dispatchAction({ type: "downplay", seriesIndex: 0 });
    if (!selected) return;
    const index = map.nodes.findIndex((node) => node.id === selected);
    if (index >= 0) instance.dispatchAction({ type: "highlight", seriesIndex: 0, dataIndex: index });
  }, [selected, map]);

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

  const totalConnections = map.edges.reduce((sum, edge) => sum + (edge.connections ?? 0), 0);
  const workloads = map.nodes.filter((node) => node.kind === "workload").length;
  const services = map.nodes.filter((node) => node.kind === "service").length;
  const filters = selectedNode ? profileFilters(selectedNode) : null;
  // Mean of the edges that closed a connection inside the window: a single
  // number for the panel, per-edge numbers in the lists below.
  const sampled = [...inbound, ...outbound].filter((edge) => edge.rtt_us);
  const latency = sampled.length
    ? sampled.reduce((sum, edge) => sum + (edge.rtt_us ?? 0), 0) / sampled.length
    : 0;

  return (
    <div className="flex h-full min-h-0 gap-3">
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
          ref={attachCanvas}
          className="min-h-[320px] flex-1 rounded-lg border bg-card/40"
          data-testid="service-map-canvas"
        />
      </div>

      <Card className="w-80 shrink-0 gap-2 overflow-y-auto p-3" data-testid="map-detail">
        {selectedNode ? (
          <>
            <div className="min-w-0">
              <div className="truncate text-sm font-medium">{nodeLabel(selectedNode)}</div>
              <div className="truncate text-[11px] text-muted-foreground">
                {selectedNode.namespace || "—"} · {selectedNode.kind}
              </div>
              {/* The node id is long (`namespace/workload-replicaset-hash`), so
                  it gets its own line rather than squeezing the name. */}
              <div
                className="truncate font-mono text-[10px] text-muted-foreground/80"
                title={selectedNode.id}
              >
                {selectedNode.id}
              </div>
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
              <dt className="text-muted-foreground">Latency</dt>
              <dd className="text-right font-mono">{latency ? formatDuration(latency * 1000) : "—"}</dd>
            </dl>
            <Separator />

            <div className="space-y-1" data-testid="map-inbound">
              <div className="flex items-center gap-1 text-[10px] uppercase tracking-wide text-muted-foreground">
                <ArrowDownLeft className="size-3" />
                Inbound · {inbound.length}
              </div>
              {inbound.length === 0 ? (
                <p className="px-1 text-[11px] text-muted-foreground">Nothing calls this service.</p>
              ) : (
                inbound.slice(0, 12).map((edge) => (
                  <EdgeRow
                    key={`in:${edge.client}:${edge.server}:${edge.protocol}`}
                    edge={edge}
                    peerId={edge.client}
                    peer={byId.get(edge.client)}
                    direction="in"
                    onSelect={setSelected}
                    testId="map-inbound-row"
                  />
                ))
              )}
              {inbound.length > 12 ? (
                <p className="px-1 text-[10px] text-muted-foreground">
                  +{inbound.length - 12} more
                </p>
              ) : null}
            </div>

            <div className="space-y-1" data-testid="map-outbound">
              <div className="flex items-center gap-1 text-[10px] uppercase tracking-wide text-muted-foreground">
                <ArrowUpRight className="size-3" />
                Outbound · {outbound.length}
              </div>
              {outbound.length === 0 ? (
                <p className="px-1 text-[11px] text-muted-foreground">This service calls nothing.</p>
              ) : (
                outbound.slice(0, 12).map((edge) => (
                  <EdgeRow
                    key={`out:${edge.client}:${edge.server}:${edge.protocol}`}
                    edge={edge}
                    peerId={edge.server}
                    peer={byId.get(edge.server)}
                    direction="out"
                    onSelect={setSelected}
                    testId="map-outbound-row"
                  />
                ))
              )}
              {outbound.length > 12 ? (
                <p className="px-1 text-[10px] text-muted-foreground">
                  +{outbound.length - 12} more
                </p>
              ) : null}
            </div>

            <Separator />
            {filters ? (
              <Button
                size="sm"
                className="w-full"
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
              Columns are dependency depth: what a service calls sits to its right. Node size is new
              connections, colour is on-CPU time. Click a node to see what it talks to.
            </p>
            <Separator />
            <div className="space-y-1">
              {[...map.nodes]
                .sort((a, b) => (b.connections ?? 0) - (a.connections ?? 0))
                .slice(0, 10)
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
