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
// A node is a card, not a dot: the name has to be readable without hovering, and
// the namespace and replica count are what turn a box into something an operator
// can act on. The card's two lines therefore decide its width, and the width
// decides the gaps — which is why the layout works in pixels and the series is
// inset until ECharts' fit is the identity.
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

/** Fill of a node card before any CPU tint: the hue says what the node is. */
const KIND_FILL = {
  workload: "#1b2a3f",
  service: "#2a2140",
  external: "#242830",
} as const;

/** Geometry of a node card, in pixels. The graph is drawn at 1:1, so these are
 *  the sizes on screen. */
const CARD = {
  height: 42,
  /** Space between the card's edge and its text. */
  padding: 11,
  minWidth: 120,
  maxWidth: 210,
};

/** Horizontal advance of the dashboard font. Approximate on purpose: the two
 *  lines are clipped to the width this produces, so an overestimate costs a
 *  little padding and an underestimate a clipped letter. */
const ADVANCE = { name: 6.45, meta: 5.45 };

function clip(text: string, maxWidth: number, advance: number): string {
  const room = Math.max(1, Math.floor(maxWidth / advance));
  return text.length <= room ? text : `${text.slice(0, room - 1)}…`;
}

/** Rich-text templates are a mini language: a brace or a pipe in a workload
 *  name would be read as markup. */
function escapeMarkup(text: string): string {
  return text.replace(/[{}|]/g, " ");
}

/** Blue -> orange, so a service that burns CPU stands out from a busy one.
 *
 * The tint is mixed into the card instead of replacing it: white text on a
 * saturated orange is unreadable, and a card full of unreadable names is worse
 * than one that only hints at the heat. */
function cardFill(node: ServiceMapNode, cpu: number, max: number): string {
  const base = KIND_FILL[node.kind ?? "workload"] ?? KIND_FILL.workload;
  const t = !max || cpu <= 0 ? 0 : Math.min(1, Math.sqrt(cpu / max)) * 0.52;
  const from = [1, 3, 5].map((i) => parseInt(base.slice(i, i + 2), 16));
  const heat = [0xf2, 0xa6, 0x5a];
  const mix = from.map((channel, i) => Math.round(channel + (heat[i] - channel) * t));
  return `rgb(${mix.join(",")})`;
}

function nodeLabel(node: ServiceMapNode | undefined): string {
  if (!node) return "—";
  if (node.kind === "external") return "external";
  return node.service || node.workload || node.id;
}

/** The second line of a card: where the node lives, and how healthy it is.
 *
 * `suffix` is drawn in the tone's colour and is what the eye catches first, so
 * it carries the number that changes — ready replicas — rather than the name
 * of the namespace, which does not. */
interface CardMeta {
  text: string;
  suffix?: string;
  tone: "ok" | "warn" | "muted";
}

function cardMeta(node: ServiceMapNode): CardMeta {
  const namespace = node.namespace || "—";
  if (node.kind === "external") return { text: "outside the cluster", tone: "muted" };
  if (node.kind === "service") return { text: namespace, suffix: "ClusterIP", tone: "muted" };
  const pods = node.pods ?? 0;
  if (pods === 0) return { text: namespace, tone: "muted" };
  const ready = Math.min(node.ready ?? 0, pods);
  return {
    text: namespace,
    suffix: `${ready}/${pods}`,
    tone: ready === pods ? "ok" : "warn",
  };
}

function metaLine(meta: CardMeta): string {
  return meta.suffix ? `${meta.text} · ${meta.suffix}` : meta.text;
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

    // One width for every card: a column of equal cards reads as a list, where
    // per-node widths read as a ragged edge. The widest name decides it.
    const metas = new Map(map.nodes.map((node) => [node.id, cardMeta(node)]));
    const widest = map.nodes.reduce((widest, node) => {
      const name = nodeLabel(node).length * ADVANCE.name;
      const meta = metaLine(metas.get(node.id) ?? { text: "", tone: "muted" }).length * ADVANCE.meta;
      return Math.max(widest, name, meta);
    }, 0);
    const cardWidth = Math.round(
      Math.min(CARD.maxWidth, Math.max(CARD.minWidth, widest + CARD.padding * 2)),
    );
    const textWidth = cardWidth - CARD.padding * 2;

    // The layout works in pixels and the chart draws them 1:1 (the series is
    // inset below so that ECharts' fit is the identity), which is what keeps a
    // card's glyphs the size they were measured at.
    const positions = layeredLayout(
      map.nodes.map((node) => ({ id: node.id, connections: node.connections ?? 0 })),
      map.edges,
      {
        width: Math.max(320, width - cardWidth - 32),
        height: Math.max(240, height - CARD.height - 32),
        minRowGap: CARD.height + 14,
      },
    );

    let spanX = 0;
    let spanY = 0;
    for (const point of positions.values()) {
      spanX = Math.max(spanX, point.x);
      spanY = Math.max(spanY, point.y);
    }
    // Centre the drawing when it is smaller than the canvas, and keep half a
    // card of margin when it is not: a node centre sits on the edge of the
    // series rect, so without the margin the outer columns are cut in half.
    const insetX = Math.max(cardWidth / 2 + 16, (width - spanX) / 2);
    const insetY = Math.max(CARD.height / 2 + 16, (height - spanY) / 2);

    const nodes = map.nodes.map((node) => {
      const point = positions.get(node.id);
      const meta = metas.get(node.id) ?? { text: "", tone: "muted" };
      const tail = meta.suffix ? ` · ${meta.suffix}` : "";
      const prefix = clip(
        escapeMarkup(meta.text),
        textWidth - tail.length * ADVANCE.meta,
        ADVANCE.meta,
      );
      const card =
        `{name|${escapeMarkup(clip(nodeLabel(node), textWidth, ADVANCE.name))}}\n` +
        `{meta|${prefix}}` +
        (meta.suffix ? `{${meta.tone}|${escapeMarkup(tail)}}` : "");
      return {
        id: node.id,
        name: nodeLabel(node),
        value: node.connections ?? 0,
        x: point?.x ?? 0,
        y: point?.y ?? 0,
        symbol: "roundRect",
        symbolSize: [cardWidth, CARD.height],
        itemStyle: {
          color: cardFill(node, node.cpu_ns ?? 0, maxCpu),
          borderColor: "rgba(255,255,255,0.14)",
          borderWidth: 1,
          opacity: node.kind === "external" ? 0.9 : 1,
        },
        // The two lines were fitted to the card above; the series formatter
        // only has to hand them over.
        card,
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
          // An arrowhead ends at the target's centre, which a card covers, so
          // direction is read from the columns: left calls right. The
          // exception is the one worth spotting, and it is dashed.
          type:
            (positions.get(edge.client)?.x ?? 0) > (positions.get(edge.server)?.x ?? 0)
              ? "dashed"
              : "solid",
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
            if (raw.pods) lines.push(`${raw.ready ?? 0}/${raw.pods} replicas ready`);
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
            // Positions come from the layered layout, in pixels. Insetting the
            // series by the content's margin turns ECharts' fit into the
            // identity, so a card is drawn at the size it was measured at
            // instead of being scaled to whatever the canvas aspect implies.
            layout: "none",
            left: insetX,
            right: insetX,
            top: insetY,
            bottom: insetY,
            roam: true,
            draggable: true,
            emphasis: {
              focus: "adjacency",
              // The selected card is the one with the white edge; with the
              // neighbours dimmed it is otherwise just "the one that is not
              // faded", which is easy to lose among six.
              itemStyle: { borderColor: "#dfe7f2", borderWidth: 1.5 },
              lineStyle: { opacity: 0.95 },
            },
            // Fading to the default 0.1 erases the text as well as the card,
            // and a map with no context in it is not a map.
            blur: { itemStyle: { opacity: 0.28 }, label: { opacity: 0.28 } },
            label: {
              show: true,
              position: "inside",
              // Two lines, already fitted to the card: the name in the body
              // colour, then where it lives and its ready replicas.
              formatter: (params: unknown) => (params as { data?: { card?: string } }).data?.card ?? "",
              rich: {
                name: { fontSize: 11, lineHeight: 15, color: "#e9eef6", fontWeight: 500 },
                meta: { fontSize: 9, lineHeight: 13, color: "#93a0b4" },
                ok: { fontSize: 9, lineHeight: 13, color: "#6fce9b" },
                warn: { fontSize: 9, lineHeight: 13, color: COLORS.hot },
                muted: { fontSize: 9, lineHeight: 13, color: "#8b95a6" },
              },
            },
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
            <span
              className="inline-block h-2.5 w-4 rounded-[3px]"
              style={{ background: KIND_FILL.workload, boxShadow: "inset 0 0 0 1px rgba(255,255,255,0.18)" }}
            />
            workload
          </span>
          <span className="flex items-center gap-1">
            <span
              className="inline-block h-2.5 w-4 rounded-[3px]"
              style={{ background: KIND_FILL.service, boxShadow: "inset 0 0 0 1px rgba(255,255,255,0.18)" }}
            />
            ClusterIP service
          </span>
          <span className="flex items-center gap-1">
            <span
              className="inline-block h-2.5 w-4 rounded-[3px]"
              style={{ background: KIND_FILL.external, boxShadow: "inset 0 0 0 1px rgba(255,255,255,0.18)" }}
            />
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
              <dt className="text-muted-foreground">Replicas</dt>
              <dd className="text-right font-mono">
                {selectedNode.pods ? (
                  <span
                    style={{
                      color:
                        (selectedNode.ready ?? 0) < (selectedNode.pods ?? 0)
                          ? COLORS.hot
                          : "#6fce9b",
                    }}
                  >
                    {Math.min(selectedNode.ready ?? 0, selectedNode.pods ?? 0)}/{selectedNode.pods}
                  </span>
                ) : (
                  "—"
                )}
              </dd>
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
              Columns are dependency depth: what a service calls sits to its right. Each card names
              the service, then its namespace and how many of its replicas are ready; the fill
              warms with on-CPU time, and a dashed edge calls back upstream. Click a card to see
              what it talks to.
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
