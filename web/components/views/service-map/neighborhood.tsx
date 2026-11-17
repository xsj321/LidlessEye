"use client";

// The neighbourhood of the selected node: what calls it, what it calls.
//
// This is a different drawing from the big map, not a crop of it. The big map
// answers "what exists"; this one answers "which part of this workload holds
// the connection", so each workload is a single card that owns its container
// rows rather than a card and a second set of loose boxes underneath it. One
// DOM subtree per workload means a long workload name or a third container can
// no longer push a row on top of its neighbour: the browser stacks them.
//
// Direction is fixed: callers on the left, the selected workload in the
// middle, callees on the right. Every card carries the same rate, protocol and
// latency as the big map, and every card is itself clickable, so walking the
// graph is one hop per click.
//
// Layout is decided in world pixels at scale 1, independent of the drawer's
// width. The drawer asks for a width that fits that world and the view then
// scales to whatever it actually got, which keeps resizing from changing where
// the cards sit.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent, WheelEvent as ReactWheelEvent } from "react";
import { Maximize2, Minus, Plus } from "lucide-react";

import { KIND, edgeWeight, formatRate, nodeLabel, nodeMeta } from "@/lib/servicemap-graph";
import type { Box } from "@/lib/servicemap-graph";
import type { ServiceMapEdge, ServiceMapNode } from "@/lib/types";
import { Button } from "@/components/ui/button";

import { EdgeLayer } from "./edge-layer";
import {
  NEIGHBOR_CARD,
  NeighborCard,
  neighborCardHeight,
  neighborRowCenter,
  neighborRowOffset,
} from "./neighbor-card";
import type { NeighborContainer } from "./neighbor-card";

/** How many peers a column shows before it says "+n more". */
const MAX_PEERS = 6;
/** Containers drawn inside one workload card, before a "+n more" row. */
const MAX_CONTAINERS = 6;

const SIZE = {
  pad: 20,
  /** The strip above the cards that holds the CALLERS / CALLEES caption. */
  headerRow: 16,
  /** Room between two columns for the edge label. */
  gapX: 92,
  cardGapY: 16,
  minCard: 214,
  maxCard: 300,
};

const ZOOM = { min: 0.5, max: 2.4, step: 1.22 };
const MAX_VIEW_HEIGHT = 440;
const MIN_VIEW_HEIGHT = 260;

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

interface NeighborRow {
  id: string;
  ownerId: string;
  name: string;
  /** Row top, relative to the top of its workload card. */
  offset: number;
}

interface Layout {
  /** Workload cards — the only things a label must avoid. */
  cards: Map<string, Box>;
  /**
   * Edge endpoints. Every container row has two point-sized anchors: `<key>@out`
   * on the card's right border and `<key>@in` on its left, matching the two
   * ports drawn on the card. A curve leaves a source's `@out` and lands on a
   * target's `@in`, so it runs container-to-container at the row's own height.
   * Workloads whose edge could not be attributed to a container get `<id>@out`
   * / `<id>@in` at the card's vertical centre.
   */
  anchors: Map<string, Box>;
  rows: NeighborRow[];
  rowNames: Map<string, string>;
  columns: string[][];
  /** Caption above each side, so "upstream" and "downstream" read at a glance. */
  captions: { x: number; w: number; label: string }[];
  height: number;
  width: number;
  selfBox: Box;
}

/** A point-sized box whose centre is (x, y): edgePath reads its edges. */
function port(x: number, y: number): Box {
  return { x: x - 0.5, y: y - 0.5, w: 1, h: 1 };
}

/** Strip the `@in` / `@out` suffix back to a row or workload key. */
function anchorBase(key: string): string {
  return key.replace(/@(in|out)$/, "");
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

  // Containers worth a row, in reading order: the ones that actually carried
  // traffic in this window first (heaviest at the top), then the rest of what
  // the pod list declares. A long tail collapses into one "+n more" row rather
  // than making a single card taller than the viewport.
  const containers = useMemo(() => {
    const traffic = new Map<string, Map<string, number>>();
    const add = (owner: string, name: string | undefined, weight: number) => {
      if (!name || !shownIds.includes(owner)) return;
      const slot = traffic.get(owner) ?? new Map<string, number>();
      slot.set(name, (slot.get(name) ?? 0) + weight);
      traffic.set(owner, slot);
    };
    for (const edge of edges) {
      const weight = edgeWeight(edge);
      add(edge.client, edge.client_container, weight);
      add(edge.server, edge.server_container, weight);
    }
    const out = new Map<string, { names: string[]; extra: number }>();
    for (const id of shownIds) {
      const active = [...(traffic.get(id)?.entries() ?? [])]
        .sort((a, b) => b[1] - a[1])
        .map(([name]) => name);
      const declared = (nodes.get(id)?.containers ?? []).filter(
        (name) => name && !active.includes(name),
      );
      const all = [...active, ...declared];
      out.set(id, {
        names: all.slice(0, MAX_CONTAINERS),
        extra: Math.max(0, all.length - MAX_CONTAINERS),
      });
    }
    return out;
  }, [edges, nodes, shownIds]);

  const layout = useMemo<Layout>(() => {
    const rowCountOf = (id: string) => {
      const info = containers.get(id);
      return (info?.names.length ?? 0) + (info?.extra ? 1 : 0);
    };
    const rowIndexOf = (id: string, name: string | undefined): number => {
      if (!name) return -1;
      return (containers.get(id)?.names ?? []).indexOf(name);
    };
    const heightOf = (id: string) => neighborCardHeight(rowCountOf(id));

    const textWidth = Math.max(
      12,
      ...shownIds.flatMap((id) => {
        const meta = nodes.get(id);
        return [
          nodeLabel(meta).length * 6.6,
          (meta ? nodeMeta(meta) : "").length * 5.4,
          ...(containers.get(id)?.names ?? []).map((name) => name.length * 6.1 + 46),
        ];
      }),
    );
    const cardWidth = clamp(Math.round(textWidth + 48), SIZE.minCard, SIZE.maxCard);

    // Only the columns that exist take width: a node with only callees is a
    // two-column drawing, not a three-column one with an empty left edge.
    const columns: { key: "up" | "self" | "down"; ids: string[] }[] = [];
    const upIds = upGroups.map((group) => group.id).filter((id) => nodes.has(id));
    const downIds = downGroups.map((group) => group.id).filter((id) => nodes.has(id));
    if (upIds.length) columns.push({ key: "up", ids: upIds });
    if (nodes.has(node.id)) columns.push({ key: "self", ids: [node.id] });
    if (downIds.length) columns.push({ key: "down", ids: downIds });

    const xOf = (index: number) => SIZE.pad + index * (cardWidth + SIZE.gapX);
    const span =
      columns.length * cardWidth + Math.max(0, columns.length - 1) * SIZE.gapX;
    const width = span + 2 * SIZE.pad;

    const selfColumn = Math.max(
      0,
      columns.findIndex((column) => column.key === "self"),
    );
    const selfH = heightOf(node.id);
    // Self is placed at y=0 in a provisional frame; every peer is positioned
    // relative to it and the whole drawing is shifted down afterwards.
    const selfCard: Box = { x: xOf(selfColumn), y: 0, w: cardWidth, h: selfH };

    // Traffic with no container attribution attaches to the header edge, so
    // both the alignment and the anchor use that same height.
    const headerMid = NEIGHBOR_CARD.header / 2;
    /** Y (in the provisional frame) of the self row a peer should line up to. */
    const selfRowY = (container: string | undefined) => {
      const index = rowIndexOf(node.id, container);
      return index >= 0 ? neighborRowCenter(index) : headerMid;
    };
    /** Y of the peer row that carries the edge, relative to the peer's card. */
    const peerRowY = (id: string, container: string | undefined) => {
      const index = rowIndexOf(id, container);
      return index >= 0 ? neighborRowCenter(index) : headerMid;
    };

    const heaviest = (group: PeerGroup) =>
      group.edges.reduce(
        (best, edge) => (edgeWeight(edge) > edgeWeight(best) ? edge : best),
        group.edges[0],
      );

    const cards = new Map<string, Box>();
    cards.set(node.id, selfCard);

    columns.forEach((column, columnIndex) => {
      if (column.key === "self") return;
      const fromPeer = column.key === "up";
      const groups = fromPeer ? upGroups : downGroups;
      // Line each peer's connecting container up with the self row it talks to,
      // so the hop is a straight horizontal line between two ports. Peers that
      // target the same row are then pushed apart to keep them from touching.
      const placed = column.ids
        .map((id) => {
          const group = groups.find((candidate) => candidate.id === id);
          if (!group) return null;
          const edge = heaviest(group);
          const selfContainer = fromPeer ? edge.server_container : edge.client_container;
          const peerContainer = fromPeer ? edge.client_container : edge.server_container;
          return {
            id,
            wanted: selfRowY(selfContainer) - peerRowY(id, peerContainer),
            h: heightOf(id),
          };
        })
        .filter((item): item is { id: string; wanted: number; h: number } => item != null)
        .sort((a, b) => a.wanted - b.wanted);

      let bottom = Number.NEGATIVE_INFINITY;
      for (const item of placed) {
        const y = Math.max(item.wanted, bottom + SIZE.cardGapY);
        cards.set(item.id, { x: xOf(columnIndex), y, w: cardWidth, h: item.h });
        bottom = y + item.h;
      }
    });

    // Shift everything so the top of the drawing clears the caption strip.
    let minY = Number.POSITIVE_INFINITY;
    let maxY = Number.NEGATIVE_INFINITY;
    for (const card of cards.values()) {
      minY = Math.min(minY, card.y);
      maxY = Math.max(maxY, card.y + card.h);
    }
    const top = SIZE.pad + SIZE.headerRow;
    const shift = top - minY;
    for (const card of cards.values()) card.y += shift;
    const height = maxY - minY + top + SIZE.pad;

    const anchors = new Map<string, Box>();
    const rows: NeighborRow[] = [];
    const rowNames = new Map<string, string>();
    for (const [id, card] of cards) {
      // Fallback for traffic the agent could not attribute to a container:
      // the header edge, where the workload's name is, not the middle of the
      // body — a curve there would strike through the container rows.
      const mid = card.y + headerMid;
      anchors.set(`${id}@out`, port(card.x + card.w, mid));
      anchors.set(`${id}@in`, port(card.x, mid));
      (containers.get(id)?.names ?? []).forEach((name, index) => {
        const key = rowId(id, name);
        const offset = neighborRowOffset(index);
        const centre = card.y + neighborRowCenter(index);
        // The out port is the circle on the card's right border, the in port
        // the one on its left — the exact points NeighborCard draws.
        anchors.set(`${key}@out`, port(card.x + card.w, centre));
        anchors.set(`${key}@in`, port(card.x, centre));
        rowNames.set(key, name);
        rows.push({ id: key, ownerId: id, name, offset });
      });
    }

    const captions: Layout["captions"] = columns
      .map((column, index) =>
        column.key === "self"
          ? null
          : {
              x: xOf(index),
              w: cardWidth,
              label: column.key === "up" ? "CALLERS" : "CALLEES",
            },
      )
      .filter((caption): caption is Layout["captions"][number] => caption != null);

    return {
      cards,
      anchors,
      rows,
      rowNames,
      columns: columns.map((column) => column.ids),
      captions,
      height,
      width,
      selfBox: cards.get(node.id) ?? selfCard,
    };
  }, [upGroups, downGroups, node.id, nodes, containers, shownIds, rowId]);

  // One line per pair of container rows. A client leaves its row's right port
  // (`@out`) and a server is entered at its row's left port (`@in`), so the
  // curve is drawn from one container to the other rather than from card to
  // card. An edge with no container stays on its workload's card centre.
  const shown = useMemo(() => {
    const out: ServiceMapEdge[] = [];
    const endpoint = (
      owner: string,
      container: string | undefined,
      side: "out" | "in",
    ) => {
      const candidate = container ? rowId(owner, container) : "";
      // An edge the agent could not attribute down to a container stays on the
      // workload card instead of disappearing from the drawing.
      const base = candidate && layout.anchors.has(`${candidate}@${side}`) ? candidate : owner;
      return `${base}@${side}`;
    };
    const emit = (group: PeerGroup, fromPeer: boolean) => {
      const pairs = new Map<string, ServiceMapEdge[]>();
      for (const edge of group.edges) {
        const client = endpoint(fromPeer ? group.id : node.id, edge.client_container, "out");
        const server = endpoint(fromPeer ? node.id : group.id, edge.server_container, "in");
        if (!layout.anchors.has(client) || !layout.anchors.has(server)) continue;
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
            layout.rowNames.get(anchorBase(client)) ?? "",
            layout.rowNames.get(anchorBase(server)) ?? "",
          ),
        );
      }
    };
    for (const group of upGroups) emit(group, true);
    for (const group of downGroups) emit(group, false);
    return out;
  }, [upGroups, downGroups, layout.anchors, layout.rowNames, node.id, rowId]);

  /** Connections per row, and which ports actually carry an edge. */
  const { perRow, usedPorts, usedCardPorts } = useMemo(() => {
    const totals = new Map<string, number>();
    const ports = new Map<string, { in?: boolean; out?: boolean }>();
    const cardPorts = new Map<string, { in?: boolean; out?: boolean }>();
    const mark = (anchor: string) => {
      const side = anchor.endsWith("@out") ? "out" : anchor.endsWith("@in") ? "in" : null;
      if (!side) return;
      const base = anchorBase(anchor);
      // A container port for a row, or the header port of a workload whose
      // traffic could not be attributed to one.
      const target = layout.rowNames.has(base)
        ? ports
        : layout.cards.has(base)
          ? cardPorts
          : null;
      if (!target) return;
      const state = target.get(base) ?? {};
      state[side] = true;
      target.set(base, state);
    };
    for (const edge of shown) {
      const client = anchorBase(edge.client);
      const server = anchorBase(edge.server);
      totals.set(client, (totals.get(client) ?? 0) + (edge.connections ?? 0));
      totals.set(server, (totals.get(server) ?? 0) + (edge.connections ?? 0));
      mark(edge.client);
      mark(edge.server);
    }
    return { perRow: totals, usedPorts: ports, usedCardPorts: cardPorts };
  }, [shown, layout.rowNames, layout.cards]);

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
    // A press on a card belongs to the card: capturing it here would retarget
    // the click away from the NeighborCard, and clicking a peer would no longer
    // walk the graph to that workload's own neighbourhood.
    if (target?.closest("[data-map-controls]") || target?.closest("[data-map-card]")) {
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
  for (const id of layout.cards.keys()) cpuMax = Math.max(cpuMax, nodes.get(id)?.cpu_ns ?? 0);

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
            {/* Upstream and downstream are captioned once, above the cards,
                so a glance tells which way the traffic flows without reading
                a legend. */}
            {layout.captions.map((caption) => (
              <div
                key={caption.label}
                className="pointer-events-none absolute text-[8.5px] font-medium uppercase tracking-[0.14em] text-muted-foreground/60"
                style={{ left: caption.x, top: SIZE.pad - 4, width: caption.w }}
              >
                {caption.label}
              </div>
            ))}
            <EdgeLayer
              edges={shown}
              boxes={layout.anchors}
              // Container ports are edge endpoints, not obstacles: a label
              // only has to clear the card that owns them.
              avoid={[...layout.cards.values()]}
              windowSec={windowSec}
              // Container rows are not nodes: highlighting by id would dim every
              // edge that ends on one, so the panel leaves the whole hop lit.
              selected={null}
              width={layout.width}
              height={layout.height}
              testId="map-neighborhood-edges"
            />
            {layout.columns.flat().map((id) => {
              const peer = nodes.get(id);
              const box = layout.cards.get(id);
              if (!peer || !box) return null;
              const info = containers.get(id);
              const rows: NeighborContainer[] = (info?.names ?? []).map((name) => ({
                name,
                rate: rate(perRow.get(rowId(id, name)) ?? 0),
                ports: usedPorts.get(rowId(id, name)) ?? {},
              }));
              if (info?.extra) {
                rows.push({ name: `+${info.extra} more containers`, rate: 0, more: true });
              }
              return (
                <NeighborCard
                  key={id}
                  node={peer}
                  box={box}
                  containers={rows}
                  cardPorts={usedCardPorts.get(id)}
                  cpu={peer.cpu_ns ?? 0}
                  cpuMax={cpuMax}
                  selected={id === node.id}
                  onSelect={onSelect}
                  testId={id === node.id ? "map-neighborhood-self" : "map-neighborhood-peer"}
                />
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
