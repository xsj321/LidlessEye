"use client";

// The map canvas: one transformed layer holding the edge bands, the edges
// (SVG) and the cards (DOM), plus the pan and zoom that move it.
//
// The layout runs in world pixels at scale 1 (see `lib/servicemap-graph`), and
// this component only ever changes the CSS transform on the wrapper — no
// element is re-measured or re-laid-out while panning, which is what keeps a
// hundred cards smooth to drag.
//
// Three things sit on top of the drawing, and each answers a question the
// drawing cannot:
//
//   search   which card is `payments`? Sixty cards with no address bar is a
//            reading exercise; typing four letters is not. The box lives in
//            the map's own row rather than on the canvas, so it never covers
//            the cards it is meant to find; a hit is centred and selected.
//   focus    what does this node talk to? The full graph is context; the
//            one-hop subgraph is the answer, re-laid out so the neighbours are
//            not scattered across the whole canvas.
//   fit      where did the drawing go? Zoom, fit and reset are one click each.
//
// The first fit is computed rather than assumed: a graph wider than the
// viewport starts zoomed out just enough to be read as a whole, but never
// below the point where a card's 9px metadata stops being legible.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Maximize2, Minus, Plus, RotateCcw, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { useT } from "@/lib/i18n";
import { useSettings } from "@/lib/settings";
import {
  mapZones,
  mergeParallelEdges,
  nodeLabel,
  placeNodes,
  worldBounds,
} from "@/lib/servicemap-graph";
import type { Box, MapZone } from "@/lib/servicemap-graph";
import type { ServiceMapEdge, ServiceMapNode } from "@/lib/types";

import { EdgeLayer } from "./edge-layer";
import { NodeCard } from "./node-card";

// The floor is a thumbnail, not a reading zoom: "fit the whole drawing" has to
// be able to keep its promise on a forty-column map, and at that size the map
// is an overview — cards are shapes, and search or focus is how you get to one.
const ZOOM = { min: 0.12, max: 2.6, step: 1.22 };
/** Below this a card's 9px metadata stops being readable; a search hit zooms
 *  in to here instead of leaving the reader with a 0.4-scale card. */
const READABLE_ZOOM = 0.85;
/**
 * The floor the automatic fit keeps.
 *
 * Fitting everything on screen only works while the graph is small. A layered
 * map of a real cluster can be forty columns wide — a chain of services, one
 * hop per column — and scaling that to the canvas leaves a ribbon of
 * unreadable slivers, which is worse than not seeing the whole thing. The
 * automatic fit therefore stops here and lets the map run off the edge; "fit"
 * on the toolbar still shows the whole drawing for an overview, and search and
 * focus are how you move around it.
 */
const AUTO_FIT_FLOOR = 0.72;

/**
 * Band colours. Fill stays under 6% so it never competes with a card, and the
 * border is the same hue at 18%: enough to see where a region ends, not enough
 * to read as a second card border.
 */
const ZONE_STYLE = {
  workload: { fill: "rgba(107,167,247,0.045)", border: "rgba(107,167,247,0.20)", text: "#8fb6e8" },
  service: { fill: "rgba(180,142,242,0.045)", border: "rgba(180,142,242,0.20)", text: "#c3a6f5" },
  external: { fill: "rgba(150,160,178,0.05)", border: "rgba(150,160,178,0.20)", text: "#9aa5b6" },
} as const;

const ZONE_LABEL: Record<MapZone["kind"], string> = {
  workload: "Workloads",
  service: "ClusterIP",
  external: "External",
};

export interface ServiceGraphProps {
  nodes: ServiceMapNode[];
  edges: ServiceMapEdge[];
  windowSec: number;
  selected: string | null;
  onSelect: (id: string | null) => void;
  /** Only draw the selected node and its one-hop neighbours. */
  focus?: boolean;
  onFocusChange?: (focus: boolean) => void;
  /**
   * Put this node in the middle of the canvas. `nonce` makes a repeat request
   * for the same node a new event — picking the same search hit twice should
   * still recentre after the reader has panned away.
   */
  center?: { id: string; nonce: number } | null;
  /** Cards to outline as search hits while the result list is open. */
  highlight?: Set<string>;
  /**
   * Pixels at the right edge covered by the node drawer. The canvas still
   * spans the full width — the drawer floats over it — but fitting and
   * centring have to aim at the part that can be seen.
   */
  insetRight?: number;
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
  focus = false,
  onFocusChange,
  center,
  highlight,
  insetRight = 0,
  testId,
}: ServiceGraphProps) {
  const { edgeLabels, labelZoom } = useSettings();
  const t = useT();
  const container = useRef<HTMLDivElement | null>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  const [view, setView] = useState<View>({ x: 0, y: 0, k: 1 });
  const lastFit = useRef("");

  const [hoverNode, setHoverNode] = useState<string | null>(null);
  const [hoverEdge, setHoverEdge] = useState<string | null>(null);

  // ---- focus -------------------------------------------------------------

  // Focus is a lens, not a mode: it only means something while a node is
  // selected, so losing the selection restores the whole map instead of
  // leaving an unrelated one-hop drawing behind.
  const focusing = focus && selected !== null;

  const drawing = useMemo(() => {
    // One stroke per pair: the payload keeps an edge per protocol and container
    // pair for the panel, and drawing all of them puts four identical curves on
    // the same pixels.
    const merged = mergeParallelEdges(edges);
    if (!focusing || !selected) return { nodes, edges: merged };
    const keep = new Set<string>([selected]);
    for (const edge of merged) {
      if (edge.client === selected) keep.add(edge.server);
      if (edge.server === selected) keep.add(edge.client);
    }
    // Edges *between* the neighbours are kept too: "what does this node talk
    // to" is only half the picture if the peers' own relationships vanish.
    return {
      nodes: nodes.filter((node) => keep.has(node.id)),
      edges: merged.filter((edge) => keep.has(edge.client) && keep.has(edge.server)),
    };
  }, [nodes, edges, focusing, selected]);

  // ---- size --------------------------------------------------------------

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
    () =>
      placeNodes(drawing.nodes, drawing.edges, {
        width: size.width || 900,
        height: size.height || 620,
      }),
    [drawing, size.width, size.height],
  );
  const world = useMemo(() => worldBounds(boxes.values()), [boxes]);
  const zones = useMemo(() => mapZones(drawing.nodes, boxes), [drawing.nodes, boxes]);
  const kinds = useMemo(
    () => new Map(nodes.map((node) => [node.id, node.kind ?? "workload"])),
    [nodes],
  );

  // ---- viewport ----------------------------------------------------------

  // The part of the canvas a reader can actually see. Everything that places
  // the drawing — fit, centring, zoom buttons — works in this width, so a
  // search hit does not land underneath the open drawer.
  const usableWidth = Math.max(240, size.width - insetRight);

  const fit = useCallback(
    (whole = false) => {
      if (!size.width || !size.height) return;
      const preferred = Math.min(
        (usableWidth - 32) / world.width,
        (size.height - 32) / world.height,
      );
      const k = whole
        ? clamp(preferred, ZOOM.min, 1)
        : clamp(preferred, AUTO_FIT_FLOOR, 1);
      // When the drawing is wider than the canvas at this zoom, start at its
      // beginning rather than centring it: the entry points are what the map
      // reads from, and a centred crop hides them.
      const x = world.width * k <= usableWidth ? (usableWidth - world.width * k) / 2 : 24 - 32 * k;
      const y =
        world.height * k <= size.height ? (size.height - world.height * k) / 2 : 24 - 24 * k;
      setView({ k, x, y });
    },
    [size.width, size.height, usableWidth, world.width, world.height],
  );

  // A zoom the reader asked for survives an auto-refresh; an explicit "fit the
  // whole drawing" also survives a window resize, which is why the mode is
  // remembered rather than inferred from the current scale.
  const fitMode = useRef<"auto" | "whole">("auto");

  // Refit only when the canvas is resized or the graph changes size. A plain
  // effect on `fit` would also undo a zoom on every auto-refresh. The drawer's
  // width is part of the signature only while focused: focus is a request to
  // look at the map, so the smaller drawing should be refitted into the room
  // the drawer gives back. Opening the drawer while browsing the whole map
  // must not yank the view instead.
  useEffect(() => {
    const signature = `${size.width}x${size.height}:${Math.round(world.width)}x${Math.round(world.height)}:${
      focusing ? Math.round(insetRight) : 0
    }`;
    if (signature === lastFit.current) return;
    lastFit.current = signature;
    fit(fitMode.current === "whole");
  }, [fit, size.width, size.height, world.width, world.height, focusing, insetRight]);

  const centerOnBox = useCallback(
    (box: Box, usable = usableWidth) => {
      if (!size.width || !size.height) return;
      setView((previous) => {
        const k = clamp(Math.max(previous.k, READABLE_ZOOM), ZOOM.min, ZOOM.max);
        return {
          k,
          x: usable / 2 - (box.x + box.w / 2) * k,
          y: size.height / 2 - (box.y + box.h / 2) * k,
        };
      });
    },
    [size.width, size.height, usableWidth],
  );

  // A search hit arrives as a request, not as a state change: the canvas owns
  // the viewport, so it is the only component that can centre on a box. The
  // request is kept pending rather than applied once, because the drawer is
  // still finding its natural width when the hit lands: the first centre uses
  // the width it has, and the ones after it correct for the width it settles
  // on. Clearing the selection ends the request.
  const pendingCenter = useRef<string | null>(null);

  useEffect(() => {
    if (center) pendingCenter.current = center.id;
  }, [center?.nonce]);

  useEffect(() => {
    if (!selected) pendingCenter.current = null;
  }, [selected]);

  useEffect(() => {
    const id = pendingCenter.current;
    if (!id) return;
    // Waits for the layout that contains the node: picking a hit while focused
    // swaps the drawing in the same render, and the old `boxes` has no entry
    // for the target.
    const box = boxes.get(id);
    if (!box) return;
    // Read the drawer's width from its committed attributes rather than from a
    // state prop: selecting a hit is what opens the drawer, so the prop is
    // still one render behind at exactly the moment this runs.
    const panel = document.getElementById("map-panel");
    const drawer =
      panel?.dataset.state === "open"
        ? Math.min(Number(panel.dataset.width) || 0, Math.max(0, size.width - 240))
        : 0;
    centerOnBox(box, size.width - drawer);
  }, [center?.nonce, boxes, centerOnBox, size.width, size.height, insetRight, selected]);

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
      const px = usableWidth / 2;
      const py = size.height / 2;
      return { k, x: px - (px - previous.x) * ratio, y: py - (py - previous.y) * ratio };
    });
  };

  const reset = () => {
    onFocusChange?.(false);
    setHoverEdge(null);
    setHoverNode(null);
    onSelect(null);
    // The drawing is about to grow back to the whole graph; fit after that
    // layout lands rather than fitting the focused one.
    fitMode.current = "auto";
    lastFit.current = "";
  };

  // Tabbing through sixty cards is a legitimate way to read a map, and with
  // `overflow: clip` the browser can no longer scroll a focused card into view.
  // Pan to it instead — the same thing a click on the card would have done.
  const ensureVisible = useCallback(
    (id: string) => {
      const box = boxes.get(id);
      if (!box || !size.width || !size.height) return;
      const left = -view.x / view.k;
      const top = -view.y / view.k;
      const right = left + usableWidth / view.k;
      const bottom = top + size.height / view.k;
      const cx = box.x + box.w / 2;
      const cy = box.y + box.h / 2;
      if (cx < left || cx > right || cy < top || cy > bottom) centerOnBox(box);
    },
    [boxes, view.x, view.y, view.k, size.width, size.height, usableWidth, centerOnBox],
  );

  const cpuMax = useMemo(
    () => Math.max(1, ...drawing.nodes.map((node) => node.cpu_ns ?? 0)),
    [drawing.nodes],
  );

  // The visible part of the world, in world pixels, with a margin so a label
  // does not pop in half-way across its own pill when panning.
  const labelArea = useMemo(() => {
    const k = view.k || 1;
    return {
      x: -view.x / k - 120,
      y: -view.y / k - 120,
      w: size.width / k + 240,
      h: size.height / k + 240,
    };
  }, [view.x, view.y, view.k, size.width, size.height]);

  // The surface uses `overflow-clip` rather than `overflow-hidden`: a hidden
  // box is still a scroll container to the browser, so focusing a card (Tab
  // order, or a test's scrollIntoView) scrolls the layer inside it and the
  // whole drawing slides sideways with the transform unchanged. Clip cannot be
  // scrolled at all, which is what a canvas wants.
  return (
    <div
      ref={attach}
      data-testid={testId}
      className="relative h-full w-full cursor-grab touch-none overflow-clip rounded-lg border bg-card/40 select-none active:cursor-grabbing"
      draggable={false}
      onDragStart={(event) => event.preventDefault()}
      onPointerDown={(event) => {
        if (event.button !== 0) return;
        const target = event.target as Element | null;
        // The controls and the search box sit inside this surface; a press on
        // one must not start a pan, and its release must not count as
        // "clicked the background".
        if (target?.closest("[data-map-controls]")) return;
        // Cards are draggable too: most of the surface is cards, and a pan that
        // only works on the gaps reads as broken. The capture waits for the
        // first real movement, because capturing here would retarget the click
        // to this element and the card would never see its own click.
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
        if (!state.moved) {
          if (Math.abs(dx) + Math.abs(dy) < 4) return;
          state.moved = true;
          try {
            (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
          } catch {
            /* the pointer may already be gone */
          }
        }
        setView((previous) => ({ ...previous, x: state.vx + dx, y: state.vy + dy }));
      }}
      onPointerUp={(event) => {
        const state = drag.current;
        drag.current = null;
        // A press that never moved is a click: on a card it selects (the card
        // handles its own click), on the background it clears. Checking the
        // target here rather than trusting the click keeps the two from
        // fighting over the same gesture.
        const onCard = (event.target as Element | null)?.closest("[data-map-card]");
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
        {/* Bands first, then edges, then cards: the stack is also the reading
            order — region, relationship, thing. */}
        {zones.map((zone) => {
          const style = ZONE_STYLE[zone.kind] ?? ZONE_STYLE.workload;
          return (
            <div
              key={`${zone.kind}:${zone.x}`}
              data-testid="map-zone"
              data-kind={zone.kind}
              className="pointer-events-none absolute rounded-xl border border-dashed"
              style={{
                left: zone.x,
                top: zone.y,
                width: zone.w,
                height: zone.h,
                borderColor: style.border,
                backgroundColor: style.fill,
              }}
            >
              <span
                className="absolute left-3 top-2 font-mono text-[9px] font-medium uppercase tracking-[0.14em]"
                style={{ color: style.text }}
              >
                {t(ZONE_LABEL[zone.kind])} · {zone.count}
              </span>
            </div>
          );
        })}

        <EdgeLayer
          edges={drawing.edges}
          boxes={boxes}
          windowSec={windowSec}
          selected={selected}
          showLabels={edgeLabels && view.k >= labelZoom}
          width={world.width}
          height={world.height}
          kinds={kinds}
          hoveredEdge={hoverEdge}
          hoveredNode={hoverNode}
          onHoverEdge={setHoverEdge}
          labelArea={labelArea}
        />
        {drawing.nodes.map((node) => {
          const box = boxes.get(node.id);
          if (!box) return null;
          const incident =
            !selected || node.id === selected || drawing.edges.some(
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
              windowSec={windowSec}
              selected={node.id === selected}
              dimmed={!incident}
              hovered={node.id === hoverNode}
              matched={highlight ? highlight.has(node.id) : false}
              onHover={setHoverNode}
              onFocusNode={ensureVisible}
              onSelect={onSelect}
              testId="map-node"
            />
          );
        })}
      </div>

      <div
        data-map-controls
        className="absolute right-2 top-2 z-20 flex items-center gap-0.5 rounded-md border bg-card/85 p-0.5 backdrop-blur"
      >
        <Button
          variant="ghost"
          size="xs"
          aria-label={t("Zoom in")}
          onClick={() => zoomBy(ZOOM.step)}
        >
          <Plus />
        </Button>
        <Button
          variant="ghost"
          size="xs"
          aria-label={t("Zoom out")}
          onClick={() => zoomBy(1 / ZOOM.step)}
        >
          <Minus />
        </Button>
        <span aria-hidden="true" className="mx-0.5 h-4 w-px bg-border" />
        <Button
          variant="ghost"
          size="xs"
          aria-label={t("Fit to view")}
          title={t("Fit the whole drawing in")}
          data-testid="map-fit"
          onClick={() => {
            fitMode.current = "whole";
            fit(true);
          }}
        >
          <Maximize2 />
        </Button>
        <Button
          variant="ghost"
          size="xs"
          data-testid="map-reset"
          aria-label={t("Reset the view")}
          title={t("Reset: clear the selection, the search and the focus")}
          onClick={reset}
        >
          <RotateCcw />
        </Button>
      </div>

      {/* Focus is switched on from the node panel, which is open exactly when
          a node is selected — the panel covers this corner of the map, so a
          button here would be unreachable at the moment it matters. The pill
          stays on the left so the state is visible and reversible. */}
      {focusing && selected ? (
        <div className="absolute bottom-7 left-3 z-20 flex items-center gap-1.5 rounded-md border bg-card/90 py-0.5 pl-2 pr-0.5 text-[10px] backdrop-blur">
          <span className="text-muted-foreground">{t("Focus")}</span>
          <span className="max-w-[12rem] truncate font-medium text-foreground">
            {nodeLabel(nodes.find((node) => node.id === selected))}
          </span>
          <Button
            variant="ghost"
            size="icon-xs"
            aria-label={t("Exit focus")}
            onClick={() => onFocusChange?.(false)}
          >
            <X />
          </Button>
        </div>
      ) : null}

      <div className="pointer-events-none absolute bottom-2 left-3 text-[10px] text-muted-foreground/70">
        {t("drag to pan · scroll to zoom · click a card · / to search")}
      </div>
    </div>
  );
}
