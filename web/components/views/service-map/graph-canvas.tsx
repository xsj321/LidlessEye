"use client";

// The map canvas: one transformed layer holding the edges (SVG) and the cards
// (DOM), plus the pan and zoom that move it.
//
// The layout runs in world pixels at scale 1 (see `lib/servicemap-graph`), and
// this component only ever changes the CSS transform on the wrapper — no
// element is re-measured or re-laid-out while panning, which is what keeps a
// hundred cards smooth to drag.
//
// The first fit is computed rather than assumed: a graph wider than the
// viewport starts zoomed out just enough to be read as a whole, but never
// below the point where a card's 9px metadata stops being legible.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Maximize2, Minus, Plus } from "lucide-react";

import { Button } from "@/components/ui/button";
import { useSettings } from "@/lib/settings";
import { placeNodes, worldBounds } from "@/lib/servicemap-graph";
import type { Box } from "@/lib/servicemap-graph";
import type { ServiceMapEdge, ServiceMapNode } from "@/lib/types";

import { EdgeLayer } from "./edge-layer";
import { NodeCard } from "./node-card";

const ZOOM = { min: 0.34, max: 2.6, step: 1.22 };

export interface ServiceGraphProps {
  nodes: ServiceMapNode[];
  edges: ServiceMapEdge[];
  windowSec: number;
  selected: string | null;
  onSelect: (id: string | null) => void;
  testId?: string;
}

interface View {
  x: number;
  y: number;
  k: number;
}

function clamp(value: number, low: number, high: number): number {
  return Math.min(high, Math.max(low, value));
}

export function ServiceGraph({
  nodes,
  edges,
  windowSec,
  selected,
  onSelect,
  testId,
}: ServiceGraphProps) {
  const { edgeLabels, labelZoom } = useSettings();
  const container = useRef<HTMLDivElement | null>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  const [view, setView] = useState<View>({ x: 0, y: 0, k: 1 });
  const lastFit = useRef("");

  // Callback ref: the component renders nothing useful before the first
  // payload, so an effect keyed on `[]` would observe a null element.
  const observer = useRef<ResizeObserver | null>(null);
  const attach = useCallback((node: HTMLDivElement | null) => {
    container.current = node;
    observer.current?.disconnect();
    observer.current = null;
    if (!node) return;
    setSize({ width: node.clientWidth, height: node.clientHeight });
    const created = new ResizeObserver((entries) => {
      const box = entries[0]?.contentRect;
      if (!box) return;
      setSize((previous) =>
        Math.abs(previous.width - box.width) < 8 && Math.abs(previous.height - box.height) < 8
          ? previous
          : { width: box.width, height: box.height },
      );
    });
    created.observe(node);
    observer.current = created;
  }, []);

  const boxes = useMemo<Map<string, Box>>(
    () => placeNodes(nodes, edges, { width: size.width || 900, height: size.height || 620 }),
    [nodes, edges, size.width, size.height],
  );
  const world = useMemo(() => worldBounds(boxes.values()), [boxes]);

  const fit = useCallback(() => {
    if (!size.width || !size.height) return;
    const k = clamp(
      Math.min((size.width - 32) / world.width, (size.height - 32) / world.height),
      ZOOM.min,
      1,
    );
    setView({
      k,
      x: (size.width - world.width * k) / 2,
      y: (size.height - world.height * k) / 2,
    });
  }, [size.width, size.height, world.width, world.height]);

  // Refit only when the canvas is resized or the graph changes size. A plain
  // effect on `fit` would also undo a zoom on every auto-refresh.
  useEffect(() => {
    const signature = `${size.width}x${size.height}:${Math.round(world.width)}x${Math.round(world.height)}`;
    if (signature === lastFit.current) return;
    lastFit.current = signature;
    fit();
  }, [fit, size.width, size.height, world.width, world.height]);

  // Wheel is attached by hand: React's synthetic handler is passive, and a
  // passive listener cannot prevent the page from scrolling under the map.
  useEffect(() => {
    const element = container.current;
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

  const drag = useRef<{ x: number; y: number; vx: number; vy: number; moved: boolean } | null>(null);

  const zoomBy = (factor: number) => {
    setView((previous) => {
      const k = clamp(previous.k * factor, ZOOM.min, ZOOM.max);
      const ratio = k / previous.k;
      const px = size.width / 2;
      const py = size.height / 2;
      return { k, x: px - (px - previous.x) * ratio, y: py - (py - previous.y) * ratio };
    });
  };

  const cpuMax = useMemo(
    () => Math.max(1, ...nodes.map((node) => node.cpu_ns ?? 0)),
    [nodes],
  );

  return (
    <div
      ref={attach}
      data-testid={testId}
      className="relative h-full w-full cursor-grab touch-none overflow-hidden rounded-lg border bg-card/40 active:cursor-grabbing"
      onPointerDown={(event) => {
        if (event.button !== 0) return;
        const target = event.target as Element | null;
        // The zoom buttons sit inside this surface; a press on one must not
        // start a pan, and its release must not count as "clicked the
        // background".
        if (target?.closest("[data-map-controls]")) return;
        // A press on a card belongs to the card: capturing the pointer here
        // would retarget the compatibility click event to this element and the
        // card would never see its own click. Panning starts on the background.
        if (target?.closest('[data-testid="map-node"]')) return;
        (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
        drag.current = {
          x: event.clientX,
          y: event.clientY,
          vx: view.x,
          vy: view.y,
          moved: false,
        };
      }}
      onPointerMove={(event) => {
        const state = drag.current;
        if (!state) return;
        const dx = event.clientX - state.x;
        const dy = event.clientY - state.y;
        if (Math.abs(dx) > 3 || Math.abs(dy) > 3) state.moved = true;
        setView((previous) => ({ ...previous, x: state.vx + dx, y: state.vy + dy }));
      }}
      onPointerUp={(event) => {
        const state = drag.current;
        drag.current = null;
        // A press that never moved is a click: on a card it selects (the card
        // handles its own click), on the background it clears. Checking the
        // target here rather than trusting the click keeps the two from
        // fighting over the same gesture.
        const onCard = (event.target as Element | null)?.closest('[data-testid="map-node"]');
        if (state && !state.moved && !onCard) onSelect(null);
      }}
      onPointerCancel={() => {
        drag.current = null;
      }}
    >
      <div
        className="absolute left-0 top-0 origin-top-left"
        style={{
          width: world.width,
          height: world.height,
          transform: `translate(${view.x}px, ${view.y}px) scale(${view.k})`,
        }}
      >
        <EdgeLayer
          edges={edges}
          boxes={boxes}
          windowSec={windowSec}
          selected={selected}
          showLabels={edgeLabels && view.k >= labelZoom}
          width={world.width}
          height={world.height}
        />
        {nodes.map((node) => {
          const box = boxes.get(node.id);
          if (!box) return null;
          const incident =
            !selected || node.id === selected || edges.some(
              (edge) =>
                (edge.client === selected && edge.server === node.id) ||
                (edge.server === selected && edge.client === node.id),
            );
          return (
            <NodeCard
              key={node.id}
              node={node}
              box={box}
              cpu={node.cpu_ns ?? 0}
              cpuMax={cpuMax}
              selected={node.id === selected}
              dimmed={!incident}
              onSelect={onSelect}
              testId="map-node"
            />
          );
        })}
      </div>

      <div
        data-map-controls
        className="absolute right-2 top-2 flex items-center gap-1 rounded-md border bg-card/80 p-0.5 backdrop-blur"
      >
        <Button
          variant="ghost"
          size="xs"
          aria-label="Zoom in"
          onClick={() => zoomBy(ZOOM.step)}
        >
          <Plus />
        </Button>
        <Button
          variant="ghost"
          size="xs"
          aria-label="Zoom out"
          onClick={() => zoomBy(1 / ZOOM.step)}
        >
          <Minus />
        </Button>
        <Button variant="ghost" size="xs" aria-label="Fit to view" onClick={fit}>
          <Maximize2 />
        </Button>
      </div>

      <div className="pointer-events-none absolute bottom-2 left-3 text-[10px] text-muted-foreground/70">
        drag to pan · scroll to zoom · click a card
      </div>
    </div>
  );
}
