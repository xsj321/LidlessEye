"use client";

// The neighbourhood of the selected node: what calls it, what it calls.
//
// This is the map, filtered to one hop. It is laid out by hand rather than by
// the layered layout: a column of callers, the node, a column of callees, and
// on every edge the same rate and latency the big map shows. The point of the
// panel is to answer "what breaks if this goes away", and that question is
// about direction, so the direction is fixed on screen.
//
// A column with nothing in it is dropped and its space is given to the others:
// a leaf that only calls things would otherwise sit against empty space while
// its two cards squeezed into the right half of the drawer.

import { useCallback, useMemo, useRef, useState } from "react";

import { edgeWeight, nodeLabel } from "@/lib/servicemap-graph";
import type { Box } from "@/lib/servicemap-graph";
import type { ServiceMapEdge, ServiceMapNode } from "@/lib/types";

import { EdgeLayer } from "./edge-layer";
import { NodeCard } from "./node-card";

/** How many peers a column shows before it says "+n more". */
const MAX_PEERS = 6;

const SIZE = { height: 36, gap: 8, pad: 10, gapX: 66 };

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

  const upPeers = useMemo(() => inbound.slice(0, MAX_PEERS), [inbound]);
  const downPeers = useMemo(() => outbound.slice(0, MAX_PEERS), [outbound]);

  const layout = useMemo(() => {
    const w = Math.max(300, width);
    const hasUp = upPeers.length > 0;
    const hasDown = downPeers.length > 0;
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

    const rows = Math.max(upPeers.length, downPeers.length, 1);
    const stack = rows * SIZE.height + (rows - 1) * SIZE.gap;
    const height = stack + 2 * SIZE.pad;

    const columnTop = (count: number) =>
      SIZE.pad + Math.max(0, (stack - (count * SIZE.height + (count - 1) * SIZE.gap)) / 2);

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

    upPeers.forEach((edge, index) => place(edge.client, xs[upColumn], index, upPeers.length));
    downPeers.forEach((edge, index) => place(edge.server, xs[downColumn], index, downPeers.length));
    boxes.set(node.id, {
      x: xs[selfColumn],
      y: SIZE.pad + (stack - SIZE.height) / 2,
      w: columnWidth,
      h: SIZE.height,
    });

    return { boxes, height, width: w };
  }, [width, node.id, upPeers, downPeers]);

  const shown = useMemo(
    () =>
      edges.filter(
        (edge) =>
          (edge.server === node.id && layout.boxes.has(edge.client)) ||
          (edge.client === node.id && layout.boxes.has(edge.server)),
      ),
    [edges, node.id, layout.boxes],
  );

  let cpuMax = 1;
  for (const id of layout.boxes.keys()) cpuMax = Math.max(cpuMax, nodes.get(id)?.cpu_ns ?? 0);

  const hiddenIn = inbound.length - upPeers.length;
  const hiddenOut = outbound.length - downPeers.length;

  return (
    <div ref={attach} data-testid="map-neighborhood">
      {width > 0 ? (
        <div className="relative" style={{ width: layout.width, height: layout.height }}>
          <EdgeLayer
            edges={shown}
            boxes={layout.boxes}
            windowSec={windowSec}
            selected={node.id}
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
