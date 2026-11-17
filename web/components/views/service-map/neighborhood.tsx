"use client";

// The neighbourhood of the selected node: what calls it, what it calls.
//
// This is the map, filtered to one hop, with the same cards and edges as the
// big map. It is laid out by hand rather than by the layered layout: a column
// of callers, the node, a column of callees, and on every edge the same rate,
// protocol and latency the big map shows. The point of the panel is to answer
// "what breaks if this goes away", and that question is about direction, so
// the direction is fixed on screen.
//
// A workload is not one process. Every card in the drawing carries a row per
// container, and an edge is anchored to the container that actually opened
// the connection — so a sidecar's traffic reads as the sidecar's, not as the
// workload's. Traffic the agent could not attribute stays on the workload
// card. The drawing has its own pan and zoom: one hop can still be wider than
// a drawer, and shrinking the cards until their labels are unreadable is not a
// way to make it fit.
//
// Layout is decided in world pixels at scale 1, independent of the drawer's
// width. The drawer asks for a width that fits that world and then the view
// scales to whatever it actually got, which keeps resizing from changing where
// the cards sit.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent, WheelEvent as ReactWheelEvent } from "react";
import { Maximize2, Minus, Plus } from "lucide-react";

import { KIND, edgeWeight, formatRate, nodeLabel } from "@/lib/servicemap-graph";
import type { Box } from "@/lib/servicemap-graph";
import type { ServiceMapEdge, ServiceMapNode } from "@/lib/types";
import { Button } from "@/components/ui/button";

import { EdgeLayer } from "./edge-layer";
import { NodeCard } from "./node-card";

/** How many peers a column shows before it says "+n more". */
const MAX_PEERS = 6;

const CARD = { height: 36, gap: 10 };
const ROW = { height: 24, gap: 5, indent: 14 };
const SIZE = { pad: 12, gapX: 72, minCard: 154, maxCard: 214 };

const ZOOM = { min: 0.4, max: 2.4, step: 1.22 };
const MAX_VIEW_HEIGHT = 520;
const MIN_VIEW_HEIGHT = 220;

export interface NeighborhoodProps {
  node: ServiceMapNode;
  nodes: Map<string, ServiceMapNode>;
  edges: ServiceMapEdge[];
  windowSec: number;
  onSelect: (id: string) => void;
  /** The drawer uses this to choose a width that fits the drawing at scale 1. */
  onPreferredWidth?: (width: number) => void;
}

function clamp(value: number, low: number, high: number): number {
  return Math.min(high, Math.max(low, value));
}

/** One peer, with every edge that points at it. */
interface PeerGroup {
  id: string;
  edges: ServiceMapEdge[];
  weight: number;
}

interface ContainerRow {
  id: string;
  name: string;
  ownerId: string;
  box: Box;
}

interface Layout {
  boxes: Map<string, Box>;
  rows: ContainerRow[];
  height: number;
  width: number;
  selfBox: Box;
  rowNames: Map<string, string>;
}

function groupPeers(edges: ServiceMapEdge[], peerOf: (edge: ServiceMapEdge) => string): PeerGroup[] {
  const groups = new Map<string, PeerGroup>();
  for (const edge of edges) {
    const id = peerOf(edge);
    const group = groups.get(id) ?? { id, edges: [], weight: 0 };
    group.edges.push(edge);
    group.weight += edgeWeight(edge);
    groups.set(id, group);
  }
  return [...groups.values()].sort((a, b) => b.weight - a.weight);
}

/**
 * Collapse the edges that land on one pair of container rows.
 *
 * Several protocols between the same two containers are one line: the panel
 * answers "who talks to whom", and three stacked curves to the same row would
 * only make the rate unreadable. Every protocol name survives in the label,
 * though — that is the part the payload sniffer added and the port guess could
 * not know. RTT is weighted by connections, so a busy slow edge is not
 * averaged away by an idle fast one.
 */
function fold(
  edges: ServiceMapEdge[],
  client: string,
  server: string,
  clientContainer: string,
  serverContainer: string,
): ServiceMapEdge {
  let connections = 0;
  let retransmits = 0;
  let tx = 0;
  let rx = 0;
  let rttWeighted = 0;
  let rttWeight = 0;
  const protocols = new Set<string>();
  for (const edge of edges) {
    const count = edge.connections ?? 0;
    connections += count;
    retransmits += edge.retransmits ?? 0;
    tx += edge.tx_bytes ?? 0;
    rx += edge.rx_bytes ?? 0;
    const weight = Math.max(count, 1);
    rttWeighted += (edge.rtt_us ?? 0) * weight;
    rttWeight += (edge.rtt_us ?? 0) > 0 ? weight : 0;
    for (const name of (edge.protocol || "tcp").split("+").filter(Boolean)) {
      protocols.add(name);
    }
  }
  protocols.delete("tcp");
  return {
    client,
    server,
    protocol: [...protocols].join("+"),
    client_container: clientContainer,
    server_container: serverContainer,
    connections,
    retransmits,
    tx_bytes: tx,
    rx_bytes: rx,
    rtt_us: rttWeight > 0 ? Math.round(rttWeighted / rttWeight) : 0,
  };
}

export function Neighborhood({
  node,
  nodes,
  edges,
  windowSec,
  onSelect,
  onPreferredWidth,
}: NeighborhoodProps) {
  const [available, setAvailable] = useState(0);
  const [view, setView] = useState({ x: 0, y: 0, k: 1 });
  const viewport = useRef<HTMLDivElement | null>(null);
  const observer = useRef<ResizeObserver | null>(null);
  const attach = useCallback((element: HTMLDivElement | null) => {
    viewport.current = element;
    observer.current?.disconnect();
    observer.current = null;
    if (!element) return;
    setAvailable(element.clientWidth);
    const created = new ResizeObserver((entries) => {
      const box = entries[0]?.contentRect;
      if (box) setAvailable(box.width);
    });
    created.observe(element);
    observer.current = created;
  }, []);

  const inbound = useMemo(
    () =>
      edges.filter((edge) => edge.server === node.id).sort((a, b) => edgeWeight(b) - edgeWeight(a)),
    [edges, node.id],
  );
  const outbound = useMemo(
    () =>
      edges.filter((edge) => edge.client === node.id).sort((a, b) => edgeWeight(b) - edgeWeight(a)),
    [edges, node.id],
  );

  const allUp = useMemo(() => groupPeers(inbound, (edge) => edge.client), [inbound]);
  const allDown = useMemo(() => groupPeers(outbound, (edge) => edge.server), [outbound]);
  const upGroups = useMemo(() => allUp.slice(0, MAX_PEERS), [allUp]);
  const downGroups = useMemo(() => allDown.slice(0, MAX_PEERS), [allDown]);
  const shownIds = useMemo(
    () => [node.id, ...upGroups.map((group) => group.id), ...downGroups.map((group) => group.id)],
    [node.id, upGroups, downGroups],
  );

  const rowId = useCallback((owner: string, name: string) => `${owner}::${name}`, []);

  // The containers worth a row: what every pod list says the workload runs,
  // plus anything an edge named — the latter covers a pod that scaled away
  // between the pod list refresh and this window.
  const containers = useMemo(() => {
    const named = new Map<string, Set<string>>();
    const add = (owner: string, name: string | undefined) => {
      if (!name || !shownIds.includes(owner)) return;
      const slot = named.get(owner) ?? new Set<string>();
      slot.add(name);
      named.set(owner, slot);
    };
    for (const id of shownIds) {
      for (const name of nodes.get(id)?.containers ?? []) add(id, name);
    }
    for (const edge of edges) {
      add(edge.client, edge.client_container);
      add(edge.server, edge.server_container);
    }
    const out = new Map<string, string[]>();
    for (const [owner, names] of named) out.set(owner, [...names].filter(Boolean).sort());
    return out;
  }, [edges, nodes, shownIds]);

  const layout = useMemo<Layout>(() => {
    const columns = [upGroups.map((group) => group.id), [node.id], downGroups.map((group) => group.id)]
      .filter((column) => column.length > 0)
      .map((column) => column.filter((id) => nodes.has(id)));

    const labelLength = (id: string) => {
      const meta = nodes.get(id);
      return Math.max(nodeLabel(meta).length, (meta?.namespace ?? "").length + 6);
    };
    const widest = Math.max(
      ...shownIds.flatMap((id) => [labelLength(id), ...(containers.get(id) ?? []).map((name) => name.length + 2)]),
      12,
    );
    const cardWidth = clamp(Math.round(widest * 6.2 + 30), SIZE.minCard, SIZE.maxCard);
    const span = columns.length * cardWidth + Math.max(0, columns.length - 1) * SIZE.gapX;
    const width = span + 2 * SIZE.pad;
    const xs = columns.map((_, index) => SIZE.pad + index * (cardWidth + SIZE.gapX));

    const stackOf = (id: string) => {
      const names = containers.get(id) ?? [];
      const rows = names.length
        ? CARD.gap + names.length * ROW.height + (names.length - 1) * ROW.gap
        : 0;
      return CARD.height + rows;
    };
    const columnHeights = columns.map((column) =>
      column.reduce((sum, id) => sum + stackOf(id), 0) + Math.max(0, column.length - 1) * CARD.gap,
    );
    const tallest = Math.max(0, ...columnHeights);
    const height = tallest + 2 * SIZE.pad;

    const boxes = new Map<string, Box>();
    const rows: ContainerRow[] = [];
    const rowNames = new Map<string, string>();
    columns.forEach((column, columnIndex) => {
      let y = SIZE.pad + (tallest - columnHeights[columnIndex]) / 2;
      for (const id of column) {
        boxes.set(id, { x: xs[columnIndex], y, w: cardWidth, h: CARD.height });
        const names = containers.get(id) ?? [];
        let rowTop = y + CARD.height + CARD.gap;
        for (const name of names) {
          const idOfRow = rowId(id, name);
          const box = {
            x: xs[columnIndex] + ROW.indent,
            y: rowTop,
            w: cardWidth - ROW.indent,
            h: ROW.height,
          };
          boxes.set(idOfRow, box);
          rowNames.set(idOfRow, name);
          rows.push({ id: idOfRow, name, ownerId: id, box });
          rowTop += ROW.height + ROW.gap;
        }
        y += stackOf(id) + CARD.gap;
      }
    });

    return {
      boxes,
      rows,
      height,
      width,
      rowNames,
      selfBox: boxes.get(node.id) ?? { x: 0, y: 0, w: cardWidth, h: CARD.height },
    };
  }, [upGroups, downGroups, node.id, nodes, containers, shownIds, rowId]);

  // One line per pair of container rows, anchored on the containers the edge
  // actually names. An edge with no container stays on its workload's card.
  const shown = useMemo(() => {
    const out: ServiceMapEdge[] = [];
    const endpoint = (owner: string, container: string | undefined) => {
      const candidate = container ? rowId(owner, container) : "";
      // An edge the agent could not attribute down to a container stays on the
      // workload card instead of disappearing from the drawing.
      return candidate && layout.boxes.has(candidate) ? candidate : owner;
    };
    const emit = (group: PeerGroup, fromPeer: boolean) => {
      const pairs = new Map<string, ServiceMapEdge[]>();
      for (const edge of group.edges) {
        const client = endpoint(fromPeer ? group.id : node.id, edge.client_container);
        const server = endpoint(fromPeer ? node.id : group.id, edge.server_container);
        if (!layout.boxes.has(client) || !layout.boxes.has(server)) continue;
        const key = `${client}|${server}`;
        const slot = pairs.get(key) ?? [];
        slot.push(edge);
        pairs.set(key, slot);
      }
      for (const [key, matching] of pairs) {
        const [client, server] = key.split("|");
        out.push(
          fold(
            matching,
            client,
            server,
            layout.rowNames.get(client) ?? "",
            layout.rowNames.get(server) ?? "",
          ),
        );
      }
    };
    for (const group of upGroups) emit(group, true);
    for (const group of downGroups) emit(group, false);
    return out;
  }, [upGroups, downGroups, layout.boxes, layout.rowNames, node.id, rowId]);

  /** Connections each container row carries, for the row's own number. */
  const perRow = useMemo(() => {
    const totals = new Map<string, number>();
    for (const edge of shown) {
      totals.set(edge.client, (totals.get(edge.client) ?? 0) + (edge.connections ?? 0));
      totals.set(edge.server, (totals.get(edge.server) ?? 0) + (edge.connections ?? 0));
    }
    return totals;
  }, [shown]);

  const reportWidth = useCallback(
    (width: number) => onPreferredWidth?.(width),
    [onPreferredWidth],
  );
  useEffect(() => {
    reportWidth(layout.width + 64);
  }, [layout.width, reportWidth]);

  const viewHeight = clamp(Math.round(layout.height), MIN_VIEW_HEIGHT, MAX_VIEW_HEIGHT);
  const fit = useCallback(() => {
    if (available <= 0) return;
    const k = clamp(
      Math.min((available - 20) / layout.width, (viewHeight - 20) / layout.height),
      ZOOM.min,
      1,
    );
    setView({
      k,
      x: (available - layout.width * k) / 2,
      y: (viewHeight - layout.height * k) / 2,
    });
  }, [available, layout.width, layout.height, viewHeight]);

  // The first fit, and every fit after the drawer or the topology changes size.
  // A zoom is left alone while the payload refreshes with the same geometry.
  const lastFit = useRef("");
  useEffect(() => {
    const signature = `${Math.round(available)}x${Math.round(layout.width)}x${Math.round(layout.height)}`;
    if (signature === lastFit.current) return;
    lastFit.current = signature;
    fit();
  }, [fit, available, layout.width, layout.height]);

  // Wheel is attached by hand: React's synthetic wheel is passive, and a
  // passive listener cannot stop the page scrolling under the drawer.
  useEffect(() => {
    const element = viewport.current;
    if (!element) return;
    const onWheel = (event: WheelEvent) => {
      event.preventDefault();
      const rect = element.getBoundingClientRect();
      const px = event.clientX - rect.left;
      const py = event.clientY - rect.top;
      setView((previous) => {
        const k = clamp(previous.k * Math.exp(-event.deltaY * 0.0015), ZOOM.min, ZOOM.max);
        const ratio = k / previous.k;
        return { k, x: px - (px - previous.x) * ratio, y: py - (py - previous.y) * ratio };
      });
    };
    element.addEventListener("wheel", onWheel, { passive: false });
    return () => element.removeEventListener("wheel", onWheel);
  }, []);

  const zoomBy = (factor: number) => {
    setView((previous) => {
      const k = clamp(previous.k * factor, ZOOM.min, ZOOM.max);
      const ratio = k / previous.k;
      const px = available / 2;
      const py = viewHeight / 2;
      return { k, x: px - (px - previous.x) * ratio, y: py - (py - previous.y) * ratio };
    });
  };

  const drag = useRef<{ x: number; y: number; vx: number; vy: number } | null>(null);
  const onPointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    const target = event.target as Element | null;
    if (target?.closest("[data-map-controls]") || target?.closest('[data-testid="map-node"]')) {
      return;
    }
    event.currentTarget.setPointerCapture(event.pointerId);
    drag.current = { x: event.clientX, y: event.clientY, vx: view.x, vy: view.y };
  };
  const onPointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    const state = drag.current;
    if (!state) return;
    setView((previous) => ({
      ...previous,
      x: state.vx + event.clientX - state.x,
      y: state.vy + event.clientY - state.y,
    }));
  };
  const onPointerUp = () => {
    drag.current = null;
  };

  let cpuMax = 1;
  for (const id of layout.boxes.keys()) cpuMax = Math.max(cpuMax, nodes.get(id)?.cpu_ns ?? 0);

  const hiddenIn = allUp.length - upGroups.length;
  const hiddenOut = allDown.length - downGroups.length;
  const rate = (connections: number) => (windowSec > 0 ? Math.max(0, connections) / windowSec : 0);

  return (
    <div ref={attach} data-testid="map-neighborhood" data-layout-width={layout.width}>
      <div className="mb-1 flex items-center justify-between text-[9px] uppercase tracking-wide text-muted-foreground">
        <span>Neighbours</span>
        <span className="flex items-center gap-1 normal-case tracking-normal">
          <span className="mr-1 font-mono">
            {inbound.length} in · {outbound.length} out
          </span>
          <span data-map-controls className="flex items-center gap-0.5">
            <Button
              variant="ghost"
              size="xs"
              aria-label="Zoom neighbourhood in"
              data-testid="map-neighborhood-zoom-in"
              onClick={() => zoomBy(ZOOM.step)}
            >
              <Plus />
            </Button>
            <Button
              variant="ghost"
              size="xs"
              aria-label="Zoom neighbourhood out"
              data-testid="map-neighborhood-zoom-out"
              onClick={() => zoomBy(1 / ZOOM.step)}
            >
              <Minus />
            </Button>
            <Button
              variant="ghost"
              size="xs"
              aria-label="Fit neighbourhood to view"
              data-testid="map-neighborhood-fit"
              onClick={fit}
            >
              <Maximize2 />
            </Button>
          </span>
        </span>
      </div>
      {available > 0 ? (
        <div
          className="relative cursor-grab touch-none overflow-hidden rounded-md border bg-background/30 active:cursor-grabbing"
          style={{ height: viewHeight }}
          data-zoom={view.k.toFixed(3)}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerUp}
          onWheel={(event: ReactWheelEvent<HTMLDivElement>) => event.preventDefault()}
        >
          <div
            className="absolute left-0 top-0 origin-top-left"
            style={{
              width: layout.width,
              height: layout.height,
              transform: `translate(${view.x}px, ${view.y}px) scale(${view.k})`,
            }}
          >
            <EdgeLayer
              edges={shown}
              boxes={layout.boxes}
              windowSec={windowSec}
              // Container rows are not nodes: highlighting by id would dim every
              // edge that ends on one, so the panel leaves the whole hop lit.
              selected={null}
              width={layout.width}
              height={layout.height}
              testId="map-neighborhood-edges"
            />
            {[...layout.boxes.entries()].map(([id, box]) => {
              const peer = nodes.get(id);
              if (!peer) return null;
              return (
                <NodeCard
                  key={id}
                  node={peer}
                  box={box}
                  cpu={peer.cpu_ns ?? 0}
                  cpuMax={cpuMax}
                  compact
                  selected={id === node.id}
                  onSelect={onSelect}
                  testId={id === node.id ? "map-neighborhood-self" : "map-neighborhood-peer"}
                />
              );
            })}

            {/* A spine from each workload card down its containers: the rows
                are the pod's parts, not separate services, and a line says so
                without another legend. */}
            {layout.rows.length ? (
              <svg
                className="pointer-events-none absolute left-0 top-0 overflow-visible"
                width={layout.width}
                height={layout.height}
                aria-hidden="true"
              >
                {layout.rows.map((row) => {
                  const owner = layout.boxes.get(row.ownerId);
                  if (!owner) return null;
                  const meta = nodes.get(row.ownerId);
                  const palette = KIND[meta?.kind ?? "workload"] ?? KIND.workload;
                  return (
                    <line
                      key={`spine-${row.id}`}
                      x1={owner.x + ROW.indent / 2}
                      y1={owner.y + owner.h}
                      x2={owner.x + ROW.indent / 2}
                      y2={row.box.y + row.box.h}
                      stroke={`${palette.bar}55`}
                      strokeWidth={1}
                    />
                  );
                })}
              </svg>
            ) : null}

            {layout.rows.map((row) => {
              const meta = nodes.get(row.ownerId);
              const palette = KIND[meta?.kind ?? "workload"] ?? KIND.workload;
              const connections = perRow.get(row.id) ?? 0;
              return (
                <div
                  key={row.id}
                  data-testid="map-container-row"
                  data-node={row.ownerId}
                  data-container={row.name}
                  title={`${nodeLabel(meta)} · ${row.name}`}
                  className="absolute flex select-none items-center gap-1 overflow-hidden rounded border border-white/5 bg-white/[0.05] pl-1.5 pr-1.5"
                  style={{
                    left: row.box.x,
                    top: row.box.y,
                    width: row.box.w,
                    height: row.box.h,
                    borderLeft: `2px solid ${palette.bar}99`,
                  }}
                >
                  <span className="min-w-0 flex-1 truncate font-mono text-[9.5px] text-foreground/75">
                    {row.name}
                  </span>
                  {connections > 0 ? (
                    <span className="shrink-0 font-mono text-[8.5px] text-muted-foreground">
                      {formatRate(rate(connections))}
                    </span>
                  ) : null}
                </div>
              );
            })}

            {inbound.length === 0 && outbound.length === 0 ? (
              <p className="absolute inset-0 flex items-center justify-center text-[11px] text-muted-foreground">
                {nodeLabel(node)} has no observed peers in this window.
              </p>
            ) : null}
          </div>
          <span className="pointer-events-none absolute bottom-1 left-2 text-[9px] text-muted-foreground/60">
            drag to pan · scroll to zoom
          </span>
        </div>
      ) : null}
      {hiddenIn > 0 || hiddenOut > 0 ? (
        <p className="mt-1 text-[10px] text-muted-foreground">
          {hiddenIn > 0 ? `+${hiddenIn} more callers` : ""}
          {hiddenIn > 0 && hiddenOut > 0 ? " · " : ""}
          {hiddenOut > 0 ? `+${hiddenOut} more callees` : ""}
          {" — the full lists are in the Net tab."}
        </p>
      ) : null}
    </div>
  );
}
