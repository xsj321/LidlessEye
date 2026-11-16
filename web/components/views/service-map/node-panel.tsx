"use client";

// The panel behind a click.
//
// It slides in from the right over the map instead of replacing it: the operator
// usually wants to keep the topology in view while reading one node, and a
// modal would hide exactly the context the numbers are about.
//
// Layout, top to bottom: what the node is, one hop of its neighbourhood (the
// same cards and edges as the map, so the jump from "here" to "there" is a
// click), then the seven tabs. The neighbourhood is the reason the panel is a
// drawer and not a page: walking the graph one hop at a time is the common
// path, and every tab re-queries for the node now in focus.

import { useEffect, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent, KeyboardEvent as ReactKeyboardEvent } from "react";
import { createPortal } from "react-dom";
import { GripVertical, X } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useT } from "@/lib/i18n";
import { KIND, healthTone, nodeLabel } from "@/lib/servicemap-graph";
import type { ServiceMapEdge, ServiceMapNode } from "@/lib/types";
import { cn } from "@/lib/utils";

import { Neighborhood } from "./neighborhood";
import {
  CpuTab,
  DnsTab,
  InstanceTab,
  MemoryTab,
  NetTab,
  ProfilingTab,
  SloTab,
} from "./panel-tabs";
import type { TabProps } from "./panel-tabs";
import { useNodeData } from "./use-node-data";

const TABS: ReadonlyArray<{ value: string; label: string }> = [
  { value: "slo", label: "SLO" },
  { value: "instances", label: "Instances" },
  { value: "cpu", label: "CPU" },
  { value: "memory", label: "Memory" },
  { value: "net", label: "Net" },
  { value: "dns", label: "DNS" },
  { value: "profiling", label: "Profiling" },
];

const TONE_TEXT = {
  ok: "text-[#6fce9b]",
  warn: "text-primary",
  muted: "text-muted-foreground",
};

// The drawer is resized by dragging its left edge. Width is a view preference
// like the histogram fold, so it lives in localStorage rather than the URL and
// survives a reload without changing what a shared link shows.
const WIDTH_KEY = "lidlesseye.map.panel.width";
const DEFAULT_WIDTH = 560;
const MIN_WIDTH = 360;
const EDGE_KEEP_FREE = 200;

function storedWidth(): number {
  if (typeof window === "undefined") return DEFAULT_WIDTH;
  try {
    const value = Number(window.localStorage.getItem(WIDTH_KEY));
    return Number.isFinite(value) && value > 0 ? value : DEFAULT_WIDTH;
  } catch {
    return DEFAULT_WIDTH;
  }
}

export interface NodePanelProps {
  node: ServiceMapNode | null;
  nodes: Map<string, ServiceMapNode>;
  edges: ServiceMapEdge[];
  /** Query window, shared with the map and the flamegraph. */
  range: { from: number; until: number };
  onSelect: (id: string) => void;
  onClose: () => void;
}

export function NodePanel({ node, nodes, edges, range, onSelect, onClose }: NodePanelProps) {
  const t = useT();
  // The last node stays mounted while the panel slides out, so the drawer does
  // not empty itself in the middle of its own animation.
  const [shown, setShown] = useState<ServiceMapNode | null>(node);
  useEffect(() => {
    if (node) setShown(node);
  }, [node]);

  const current = node ?? shown;
  const open = Boolean(node);
  const data = useNodeData(current, range);
  const panel = useRef<HTMLElement | null>(null);
  const returnTo = useRef<HTMLElement | null>(null);

  // Portal the drawer to the body so it spans the whole viewport rather than
  // the map card's box; the toolbar and time bar stay visible behind it.
  const [portalRoot, setPortalRoot] = useState<HTMLElement | null>(null);
  useEffect(() => {
    setPortalRoot(document.body);
  }, []);

  const [width, setWidth] = useState(DEFAULT_WIDTH);
  const widthRef = useRef(DEFAULT_WIDTH);
  const dragging = useRef(false);
  const dragStart = useRef<{ x: number; width: number } | null>(null);
  const [resizing, setResizing] = useState(false);

  useEffect(() => {
    const next = storedWidth();
    widthRef.current = next;
    setWidth(next);
  }, []);

  const clampWidth = (value: number) => {
    if (typeof window === "undefined") return DEFAULT_WIDTH;
    const min = Math.min(MIN_WIDTH, Math.max(240, window.innerWidth - EDGE_KEEP_FREE));
    const max = Math.max(min, window.innerWidth - EDGE_KEEP_FREE);
    return Math.round(Math.min(max, Math.max(min, value)));
  };

  const applyWidth = (value: number) => {
    const next = clampWidth(value);
    widthRef.current = next;
    setWidth(next);
  };

  const persistWidth = () => {
    try {
      window.localStorage.setItem(WIDTH_KEY, String(widthRef.current));
    } catch {
      // Storage is optional; the in-memory width still applies this session.
    }
  };

  const onResizeDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    dragging.current = true;
    dragStart.current = { x: event.clientX, width: widthRef.current };
    setResizing(true);
  };

  const onResizeMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!dragging.current || !dragStart.current) return;
    // Dragging left widens the panel: the edge being dragged is on its left.
    applyWidth(dragStart.current.width + (dragStart.current.x - event.clientX));
  };

  const onResizeEnd = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!dragging.current) return;
    dragging.current = false;
    dragStart.current = null;
    setResizing(false);
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    persistWidth();
  };

  const onResizeKey = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    const step = event.shiftKey ? 64 : 24;
    if (event.key === "ArrowLeft") applyWidth(widthRef.current + step);
    else if (event.key === "ArrowRight") applyWidth(widthRef.current - step);
    else if (event.key === "Home") applyWidth(DEFAULT_WIDTH);
    else return;
    event.preventDefault();
    persistWidth();
  };

  // Focus follows the panel, both ways. Opening moves focus into the drawer so
  // the next Tab presses land in the content the click just revealed; closing
  // puts it back on the card that opened it, because a keyboard user who
  // dismissed the panel was standing there.
  useEffect(() => {
    if (open) {
      returnTo.current = document.activeElement as HTMLElement | null;
      panel.current?.focus({ preventScroll: true });
    } else {
      const target = returnTo.current;
      returnTo.current = null;
      if (target && document.contains(target)) target.focus({ preventScroll: true });
    }
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  const windowSec = Math.max(1, (range.until - range.from) / 1e9);

  const inbound = current
    ? edges.filter((edge) => edge.server === current.id)
    : [];
  const outbound = current
    ? edges.filter((edge) => edge.client === current.id)
    : [];

  const tabProps: TabProps | null = current
    ? { node: current, data, inbound, outbound, windowSec, nodes, onSelect }
    : null;

  const palette = current ? (KIND[current.kind ?? "workload"] ?? KIND.workload) : KIND.workload;
  const pods = current?.pods ?? 0;
  const ready = Math.min(current?.ready ?? 0, pods);
  const tone = current ? healthTone(current) : "muted";

  const panelNode = (
    <aside
      ref={panel}
      id="map-panel"
      role="dialog"
      // Not modal: the map stays live on purpose, so the reader can click the
      // next card without dismissing this one first. `aria-modal="false"` is
      // what tells assistive tech the rest of the page is still usable.
      aria-modal="false"
      aria-labelledby="map-panel-title"
      tabIndex={-1}
      data-testid="map-panel"
      data-state={open ? "open" : "closed"}
      data-width={width}
      style={{ width }}
      // `inert` rather than only `aria-hidden`: the drawer is translated fully
      // off-screen, and without it every button inside it stays in the tab
      // order at a position nobody can see.
      inert={!open}
      className={cn(
        // `fixed` and the body portal give the drawer the full window height,
        // independent of how far down the page the map card happens to sit.
        "fixed inset-y-0 right-0 z-40 flex flex-col",
        "border-l bg-card/95 shadow-[-24px_0_48px_-32px_rgba(0,0,0,0.95)] backdrop-blur-sm",
        "outline-none",
        // Suppress the slide transition while dragging, or the edge lags the
        // pointer by one animation frame.
        !resizing &&
          "motion-safe:transition-transform motion-safe:duration-300 motion-safe:ease-[cubic-bezier(0.32,0.72,0,1)]",
        open ? "translate-x-0" : "pointer-events-none translate-x-full",
      )}
    >
      <div
        role="separator"
        aria-orientation="vertical"
        aria-label={t("Resize node panel")}
        aria-valuemin={MIN_WIDTH}
        aria-valuemax={typeof window === "undefined" ? 1280 : Math.max(MIN_WIDTH, window.innerWidth - EDGE_KEEP_FREE)}
        aria-valuenow={width}
        tabIndex={open ? 0 : -1}
        data-testid="map-panel-resize"
        data-dragging={resizing ? "true" : "false"}
        title={t("Drag to resize · arrow keys to nudge")}
        onPointerDown={onResizeDown}
        onPointerMove={onResizeMove}
        onPointerUp={onResizeEnd}
        onPointerCancel={onResizeEnd}
        onKeyDown={onResizeKey}
        className={cn(
          "group absolute inset-y-0 -left-1 z-10 flex w-2 cursor-col-resize touch-none items-center justify-center",
          "outline-none focus-visible:ring-2 focus-visible:ring-ring",
        )}
      >
        <span
          aria-hidden="true"
          className="pointer-events-none absolute inset-y-0 left-1/2 w-px -translate-x-1/2 bg-border opacity-60 transition-opacity group-hover:opacity-100 group-focus-visible:opacity-100"
        />
        <span
          aria-hidden="true"
          className="pointer-events-none flex h-10 w-4 items-center justify-center rounded-full border bg-card text-muted-foreground opacity-0 shadow-sm transition-opacity group-hover:opacity-100 group-focus-visible:opacity-100"
        >
          <GripVertical className="h-4 w-3" />
        </span>
      </div>
      {current ? (
        <>
          <header className="flex items-start gap-2 border-b px-3 py-2.5">
            <span
              aria-hidden="true"
              className="mt-1 h-4 w-[3px] shrink-0 rounded-full"
              style={{ backgroundColor: palette.bar }}
            />
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-1.5">
                <span id="map-panel-title" className="truncate text-sm font-medium">
                  {nodeLabel(current)}
                </span>
                <Badge variant="secondary" className="shrink-0 text-[9px] uppercase">
                  {current.kind ?? "workload"}
                </Badge>
                {pods ? (
                  <span className={cn("shrink-0 font-mono text-[10px]", TONE_TEXT[tone])}>
                    {ready}/{pods}
                  </span>
                ) : null}
              </div>
              <div
                className="mt-0.5 truncate font-mono text-[10px] text-muted-foreground"
                title={current.id}
              >
                {current.id}
              </div>
            </div>
            <Button
              variant="ghost"
              size="xs"
              aria-label={t("Close panel")}
              data-testid="map-panel-close"
              onClick={onClose}
            >
              <X />
            </Button>
          </header>

          <div className="border-b px-3 py-2">
            <div className="mb-1 flex items-center justify-between text-[9px] uppercase tracking-wide text-muted-foreground">
              <span>{t("Neighbours")}</span>
              <span className="font-mono normal-case tracking-normal">
                {inbound.length} in · {outbound.length} out
              </span>
            </div>
            <Neighborhood
              node={current}
              nodes={nodes}
              edges={edges}
              windowSec={windowSec}
              onSelect={onSelect}
            />
          </div>

          <Tabs defaultValue="slo" className="flex min-h-0 flex-1 flex-col gap-0">
            <div className="border-b px-2">
              <TabsList variant="line" className="h-8 w-full justify-start gap-0">
                {TABS.map((tab) => (
                  <TabsTrigger
                    key={tab.value}
                    value={tab.value}
                    data-testid={`map-tab-${tab.value}`}
                    className="flex-none px-2 text-[11px] data-[state=active]:text-foreground"
                  >
                    {t(tab.label)}
                  </TabsTrigger>
                ))}
              </TabsList>
            </div>
            <ScrollArea className="min-h-0 flex-1">
              <div className="p-3">
                {tabProps ? (
                  <>
                    <TabsContent value="slo">
                      <SloTab {...tabProps} />
                    </TabsContent>
                    <TabsContent value="instances">
                      <InstanceTab {...tabProps} />
                    </TabsContent>
                    <TabsContent value="cpu">
                      <CpuTab {...tabProps} />
                    </TabsContent>
                    <TabsContent value="memory">
                      <MemoryTab {...tabProps} />
                    </TabsContent>
                    <TabsContent value="net">
                      <NetTab {...tabProps} />
                    </TabsContent>
                    <TabsContent value="dns">
                      <DnsTab {...tabProps} />
                    </TabsContent>
                    <TabsContent value="profiling">
                      <ProfilingTab {...tabProps} />
                    </TabsContent>
                  </>
                ) : null}
              </div>
            </ScrollArea>
          </Tabs>
          <Separator />
          <footer className="px-3 py-1.5 text-[9.5px] text-muted-foreground/70">
            {t("Window from the toolbar · the same one the flamegraph uses")}
          </footer>
        </>
      ) : null}
    </aside>
  );

  // Nothing is rendered during the static export; the drawer is hidden until
  // a node is selected, so rendering it on the server would add nothing.
  return portalRoot ? createPortal(panelNode, portalRoot) : null;
}
