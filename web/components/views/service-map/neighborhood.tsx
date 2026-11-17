"use client";

// The neighbourhood of the selected node: what calls it, what it calls.
//
// This is the map, filtered to one hop. It is laid out by hand rather than by
// the layered layout: a column of callers, the node, a column of callees, and
// on every edge the same rate and latency the big map shows. The point of the
// panel is to answer "what breaks if this goes away", and that question is
// about direction, so the direction is fixed on screen.
//
// A workload is not one process. Under its card the containers it runs get a
// row each, and an edge is anchored to the container that actually opened the
// connection — so a sidecar's traffic reads as the sidecar's, not as the
// workload's. Traffic the agent could not attribute stays on the workload card.
//
// A column with nothing in it is dropped and its space is given to the others:
// a leaf that only calls things would otherwise sit against empty space while
// its two cards squeezed into the right half of the drawer.

import { useCallback, useMemo, useRef, useState } from "react";

import { KIND, edgeWeight, formatRate, nodeLabel } from "@/lib/servicemap-graph";
import type { Box } from "@/lib/servicemap-graph";
import type { ServiceMapEdge, ServiceMapNode } from "@/lib/types";

import { EdgeLayer } from "./edge-layer";
import { NodeCard } from "./node-card";

/** How many peers a column shows before it says "+n more". */
const MAX_PEERS = 6;

const SIZE = { height: 36, gap: 8, pad: 10, gapX: 66 };

/** A container row under the workload card: shorter, indented, no glyph. */
const ROW = { height: 24, gap: 5, indent: 14 };

export interface NeighborhoodProps {
  node: ServiceMapNode;
  nodes: Map<string, ServiceMapNode>;
  edges: ServiceMapEdge[];
  windowSec: number;
  onSelect: (id: string) => void;
}

function clamp(value: number, low: number, high: number): number {
  return Math.min(high, Math.max(low, value));
}

/** One peer, with every container-level edge that points at it. */
interface PeerGroup {
  id: string;
  edges: ServiceMapEdge[];
  weight: number;
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
 * Collapse the edges that land on one row of the drawing.
 *
 * Several protocols between the same two containers are one line: the panel
 * answers "who talks to whom", and three stacked curves to the same card would
 * only make the rate unreadable. RTT is weighted by connections, so a busy
 * slow edge is not averaged away by an idle fast one.
 */
function fold(edges: ServiceMapEdge[], client: string, server: string): ServiceMapEdge {
  let connections = 0;
  let retransmits = 0;
  let tx = 0;
  let rx = 0;
  let rttWeighted = 0;
  let rttWeight = 0;
  for (const edge of edges) {
    const count = edge.connections ?? 0;
    connections += count;
    retransmits += edge.retransmits ?? 0;
    tx += edge.tx_bytes ?? 0;
    rx += edge.rx_bytes ?? 0;
    const weight = Math.max(count, 1);
    rttWeighted += (edge.rtt_us ?? 0) * weight;
    rttWeight += (edge.rtt_us ?? 0) > 0 ? weight : 0;
  }
  return {
    client,
    server,
    protocol: "tcp",
    connections,
    retransmits,
    tx_bytes: tx,
    rx_bytes: rx,
    rtt_us: rttWeight > 0 ? Math.round(rttWeighted / rttWeight) : 0,
  };
}

export function Neighborhood({ node, nodes, edges, windowSec, onSelect }: NeighborhoodProps) {
  const [width, setWidth] = useState(0);
  const observer = useRef<ResizeObserver | null>(null);
  const attach = useCallback((element: HTMLDivElement | null) => {
    observer.current?.disconnect();
    observer.current = null;
    if (!element) return;
    setWidth(element.clientWidth);
    const created = new ResizeObserver((entries) => {
      const box = entries[0]?.contentRect;
      if (box) setWidth(box.width);
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

  // The containers worth a row: what the pod list says the workload runs, plus
  // anything an edge named — the latter covers a pod that scaled away between
  // the pod list refresh and this window.
  const containers = useMemo(() => {
    const names = new Set<string>(node.containers ?? []);
    for (const edge of edges) {
      if (edge.client === node.id && edge.client_container) names.add(edge.client_container);
      if (edge.server === node.id && edge.server_container) names.add(edge.server_container);
    }
    return [...names].filter(Boolean).sort();
  }, [edges, node]);

  const rowId = useCallback((name: string) => `${node.id}::${name}`, [node.id]);

  /** The row an edge starts or ends on: a container when one is named. */
  const rowOf = useCallback(
    (name: string | undefined) => (name && containers.includes(name) ? rowId(name) : node.id),
    [containers, node.id, rowId],
  );

  const layout = useMemo(() => {
    const w = Math.max(300, width);
    const hasUp = upGroups.length > 0;
    const hasDown = downGroups.length > 0;
    const columns = 1 + (hasUp ? 1 : 0) + (hasDown ? 1 : 0);
    const columnWidth = clamp(
      Math.round((w - 2 * SIZE.pad - (columns - 1) * SIZE.gapX) / columns),
      108,
      196,
    );

    // Three columns sit at the edges and the middle; two sit at the edges, so
    // the label on the single hop has room; one is centred.
    const xs =
      columns === 3
        ? [SIZE.pad, Math.round((w - columnWidth) / 2), w - columnWidth - SIZE.pad]
        : columns === 2
          ? [SIZE.pad, w - columnWidth - SIZE.pad]
          : [Math.round((w - columnWidth) / 2)];

    const rows = Math.max(upGroups.length, downGroups.length, 1);
    const stack = rows * SIZE.height + (rows - 1) * SIZE.gap;
    const containerStack = containers.length
      ? SIZE.gap + containers.length * ROW.height + (containers.length - 1) * ROW.gap
      : 0;
    const middleStack = SIZE.height + containerStack;
    const tallest = Math.max(stack, middleStack);
    const height = tallest + 2 * SIZE.pad;

    const columnTop = (count: number) =>
      SIZE.pad + Math.max(0, (tallest - (count * SIZE.height + (count - 1) * SIZE.gap)) / 2);

    const boxes = new Map<string, Box>();
    const place = (id: string, x: number, index: number, total: number) => {
      boxes.set(id, {
        x,
        y: columnTop(total) + index * (SIZE.height + SIZE.gap),
        w: columnWidth,
        h: SIZE.height,
      });
    };

    const upColumn = hasUp ? 0 : -1;
    const selfColumn = hasUp ? 1 : 0;
    const downColumn = hasUp ? 2 : 1;

    upGroups.forEach((group, index) => place(group.id, xs[upColumn], index, upGroups.length));
    downGroups.forEach((group, index) => place(group.id, xs[downColumn], index, downGroups.length));

    const selfX = xs[selfColumn];
    const selfY = SIZE.pad + Math.max(0, (tallest - middleStack) / 2);
    boxes.set(node.id, { x: selfX, y: selfY, w: columnWidth, h: SIZE.height });

    const containerRows = containers.map((name, index) => {
      const id = rowId(name);
      const box: Box = {
        x: selfX + ROW.indent,
        y: selfY + SIZE.height + SIZE.gap + index * (ROW.height + ROW.gap),
        w: columnWidth - ROW.indent,
        h: ROW.height,
      };
      boxes.set(id, box);
      return { id, name, box };
    });

    return { boxes, containerRows, height, width: w, selfBox: boxes.get(node.id)! };
  }, [width, node.id, upGroups, downGroups, containers, rowId]);

  // One line per (peer, container) pair, anchored on the row the edge names.
  const shown = useMemo(() => {
    const out: ServiceMapEdge[] = [];
    for (const group of upGroups) {
      const rows = new Set<string>();
      for (const edge of group.edges) rows.add(rowOf(edge.server_container));
      for (const row of rows) {
        if (!layout.boxes.has(row)) continue;
        const matching = group.edges.filter((edge) => rowOf(edge.server_container) === row);
        out.push(fold(matching, group.id, row));
      }
    }
    for (const group of downGroups) {
      const rows = new Set<string>();
      for (const edge of group.edges) rows.add(rowOf(edge.client_container));
      for (const row of rows) {
        if (!layout.boxes.has(row)) continue;
        const matching = group.edges.filter((edge) => rowOf(edge.client_container) === row);
        out.push(fold(matching, row, group.id));
      }
    }
    return out;
  }, [upGroups, downGroups, layout.boxes, rowOf]);

  /** Connections each container row carries, for the row's own number. */
  const perContainer = useMemo(() => {
    const totals = new Map<string, { connections: number }>();
    for (const edge of shown) {
      for (const id of [edge.client, edge.server]) {
        if (!id.startsWith(`${node.id}::`)) continue;
        const slot = totals.get(id) ?? { connections: 0 };
        slot.connections += edge.connections ?? 0;
        totals.set(id, slot);
      }
    }
    return totals;
  }, [shown, node.id]);

  let cpuMax = 1;
  for (const id of layout.boxes.keys()) cpuMax = Math.max(cpuMax, nodes.get(id)?.cpu_ns ?? 0);

  const hiddenIn = allUp.length - upGroups.length;
  const hiddenOut = allDown.length - downGroups.length;
  const palette = KIND[node.kind ?? "workload"] ?? KIND.workload;
  const rate = (connections: number) =>
    windowSec > 0 ? Math.max(0, connections) / windowSec : 0;

  return (
    <div ref={attach} data-testid="map-neighborhood">
      {width > 0 ? (
        <div className="relative" style={{ width: layout.width, height: layout.height }}>
          <EdgeLayer
            edges={shown}
            boxes={layout.boxes}
            windowSec={windowSec}
            // Container rows are not nodes: highlighting by id would dim every
            // edge that ends on one, so the panel leaves the whole hop lit.
            selected={null}
            width={layout.width}
            height={layout.height}
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

          {/* A spine from the workload card down its containers: the rows are
              the pod's parts, not separate services, and a line says so
              without another legend. */}
          {layout.containerRows.length ? (
            <span
              aria-hidden="true"
              className="absolute w-px rounded"
              style={{
                left: layout.selfBox.x + ROW.indent / 2,
                top: layout.selfBox.y + layout.selfBox.h,
                height:
                  layout.containerRows[layout.containerRows.length - 1].box.y +
                  layout.containerRows[layout.containerRows.length - 1].box.h -
                  (layout.selfBox.y + layout.selfBox.h) -
                  4,
                backgroundColor: `${palette.bar}55`,
              }}
            />
          ) : null}

          {layout.containerRows.map(({ id, name, box }) => (
            <div
              key={id}
              data-testid="map-container-row"
              title={`${nodeLabel(node)} · ${name}`}
              className="absolute flex select-none items-center gap-1 overflow-hidden rounded border border-white/5 bg-white/[0.05] pl-1.5 pr-1.5"
              style={{
                left: box.x,
                top: box.y,
                width: box.w,
                height: box.h,
                borderLeft: `2px solid ${palette.bar}99`,
              }}
            >
              <span className="min-w-0 flex-1 truncate font-mono text-[9.5px] text-foreground/75">
                {name}
              </span>
              {(perContainer.get(id)?.connections ?? 0) > 0 ? (
                <span className="shrink-0 font-mono text-[8.5px] text-muted-foreground">
                  {formatRate(rate(perContainer.get(id)?.connections ?? 0))}
                </span>
              ) : null}
            </div>
          ))}

          {inbound.length === 0 && outbound.length === 0 ? (
            <p className="absolute inset-0 flex items-center justify-center text-[11px] text-muted-foreground">
              {nodeLabel(node)} has no observed peers in this window.
            </p>
          ) : null}
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
