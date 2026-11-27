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

import { useCallback, useEffect, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent, KeyboardEvent as ReactKeyboardEvent } from "react";
import { createPortal } from "react-dom";
import { Crosshair, GripVertical, X } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useT } from "@/lib/i18n";
import { api } from "@/lib/api";
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
  /** The map behind the drawer is limited to this node and its neighbours. */
  focus: boolean;
  onFocusChange: (focus: boolean) => void;
  /**
   * How much of the window the drawer covers, and whether it is open. The map
   * centres and fits against the part it can actually be seen in; without
   * this, a search hit lands under the panel.
   */
  onLayout?: (width: number, open: boolean) => void;
}

export function NodePanel({
  node,
  nodes,
  edges,
  range,
  onSelect,
  onClose,
  focus,
  onFocusChange,
  onLayout,
}: NodePanelProps) {
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
  // Memory profiling is not something to switch on for the cluster, so the
  // answer is per workload and the switch for the node that is open is drawn
  // from the same list every agent polls.
  const [directives, setDirectives] = useState<string[]>([]);
  // Two switches on the same workload: reading a JVM asks it for a dump, while
  // the native one injects a shared library into the process. Different risk,
  // so they are drawn and confirmed separately.
  const [allocDirectives, setAllocDirectives] = useState<string[]>([]);
  // The native reader is only offered when the server says it can deliver
  // samples; the switch would otherwise promise data that never arrives.
  const [nativeSupported, setNativeSupported] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [confirmingNative, setConfirmingNative] = useState(false);
  const [saving, setSaving] = useState(false);
  const workload = current?.id ?? "";
  const memoryOn = workload !== "" && directives.includes(workload);
  const nativeOn = workload !== "" && allocDirectives.includes(workload);
  useEffect(() => {
    let cancelled = false;
    api
      .profilingDirectives()
      .then((reply) => {
        if (cancelled) return;
        setDirectives(reply.workloads ?? []);
        setAllocDirectives(reply.alloc_workloads ?? []);
        setNativeSupported(reply.native_memory?.supported === true);
      })
      .catch(() => {
        /* the switch stays off; the panel is still usable */
      });
    return () => {
      cancelled = true;
    };
  }, [workload]);
  const applyDirective = useCallback(
    async (enabled: boolean, kind: "jvm" | "alloc" = "jvm") => {
      if (!workload) return;
      setSaving(true);
      try {
        const reply = await api.setProfilingDirective(workload, enabled, kind);
        setDirectives(reply.workloads ?? []);
        setAllocDirectives(reply.alloc_workloads ?? []);
        setNativeSupported(reply.native_memory?.supported === true);
      } catch {
        /* the list is re-read on the next open; nothing to undo locally */
      } finally {
        setSaving(false);
        setConfirming(false);
        setConfirmingNative(false);
      }
    },
    [workload],
  );
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
  // The neighbourhood has a natural width: three columns of cards, the gap
  // between them and room for their container rows. Until the operator drags
  // the edge by hand, the drawer takes that width and the map fits inside it.
  const [autoWidth, setAutoWidth] = useState(true);
  const [preferredWidth, setPreferredWidth] = useState(DEFAULT_WIDTH);
  const onPreferredWidth = useCallback((next: number) => setPreferredWidth(next), []);

  useEffect(() => {
    const next = storedWidth();
    widthRef.current = next;
    setWidth(next);
  }, []);

  // Opening a different node is a new question, so it starts from the map's
  // natural width again. A width the operator dragged to is kept only while
  // that node stays open.
  useEffect(() => {
    if (node) setAutoWidth(true);
  }, [node?.id]);

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

  useEffect(() => {
    if (!open || !autoWidth || preferredWidth <= 0) return;
    applyWidth(preferredWidth);
  }, [open, autoWidth, preferredWidth, width]);

  // Focus is a request to look at the map: while it is on, the drawer gives
  // most of the window back, and it returns to the width it had when the lens
  // is dropped. A hand-dragged width is restored too — the toggle is a mode,
  // not a resize.
  const restoreWidth = useRef<number | null>(null);
  useEffect(() => {
    if (!open) {
      restoreWidth.current = null;
      return;
    }
    if (focus && restoreWidth.current === null) {
      restoreWidth.current = widthRef.current;
      setAutoWidth(false);
      const room = Math.round(window.innerWidth * 0.38);
      applyWidth(Math.max(MIN_WIDTH, Math.min(widthRef.current, room)));
    } else if (!focus && restoreWidth.current !== null) {
      applyWidth(restoreWidth.current);
      restoreWidth.current = null;
      setAutoWidth(true);
    }
    // Width is read through a ref on purpose: including it would re-run this
    // effect on every resize and undo the drag.
  }, [focus, open]);

  // Reported once per change rather than per frame: the map only needs the
  // final width to know which part of itself is visible.
  useEffect(() => {
    onLayout?.(width, open);
  }, [onLayout, width, open]);

  const persistWidth = () => {
    try {
      window.localStorage.setItem(WIDTH_KEY, String(widthRef.current));
    } catch {
      // Storage is optional; the in-memory width still applies this session.
    }
  };

  const onResizeDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    event.preventDefault();
    setAutoWidth(false);
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
    if (event.key === "ArrowLeft") {
      setAutoWidth(false);
      applyWidth(widthRef.current + step);
    } else if (event.key === "ArrowRight") {
      setAutoWidth(false);
      applyWidth(widthRef.current - step);
    } else if (event.key === "Home") {
      setAutoWidth(true);
      applyWidth(preferredWidth);
    } else return;
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

  // Pods and containers arrive with the map node itself; `data` carries the
  // same lists for the history response, so prefer the map's own copy and fall
  // back to the history's when the map had no names to send.
  const detailed: ServiceMapNode | null = current
    ? (current.instances?.length ?? 0) > 0 || (current.containers?.length ?? 0) > 0
      ? current
      : data.instances.length > 0 || data.containers.length > 0
        ? { ...current, instances: data.instances, containers: data.containers }
        : current
    : null;

  const tabProps: TabProps | null = current
    ? { node: detailed ?? current, data, inbound, outbound, windowSec, nodes, onSelect }
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
      data-auto-width={autoWidth ? "true" : "false"}
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
            {/* Focus lives here rather than on the map: selecting a node is
                what opens this drawer, and the drawer covers the corner of the
                map where a floating control would sit. */}
            {/* Memory profiling is a per-workload decision with a cost the
                operator should see before it is switched on, so the switch
                asks first and says what it does. */}
            <Button
              variant={memoryOn ? "secondary" : "ghost"}
              size="xs"
              data-testid="map-memory"
              aria-pressed={memoryOn}
              aria-label={t("JVM memory profiling")}
              title={t("Read this workload's JVM heap through the JVM's own recorder")}
              disabled={saving}
              onClick={() => {
                if (memoryOn) {
                  void applyDirective(false);
                } else {
                  setConfirming(true);
                }
              }}
              className="shrink-0 gap-1"
            >
              <span className="hidden sm:inline">{memoryOn ? t("Memory: on") : t("Memory")}</span>
            </Button>
            {/* The native switch injects a shared library into the target's
                process, so it is a second, separately confirmed control. It is
                only drawn when the server reports that the reader delivers
                samples — a switch that silently collects nothing is worse than
                no switch. */}
            {nativeSupported && (
              <Button
                variant={nativeOn ? "secondary" : "ghost"}
                size="xs"
                data-testid="map-native-memory"
                aria-pressed={nativeOn}
                aria-label={t("Native memory profiling")}
                title={t("Hook malloc inside this workload's processes with the userspace eBPF runtime")}
                disabled={saving}
                onClick={() => {
                  if (nativeOn) {
                    void applyDirective(false, "alloc");
                  } else {
                    setConfirmingNative(true);
                  }
                }}
                className="shrink-0 gap-1"
              >
                <span className="hidden sm:inline">
                  {nativeOn ? t("Native: on") : t("Native")}
                </span>
              </Button>
            )}
            <Button
              variant={focus ? "secondary" : "ghost"}
              size="xs"
              data-testid="map-focus"
              aria-pressed={focus}
              aria-label={t("Focus on the selected node")}
              title={t("Focus: show only this node's neighbours")}
              onClick={() => onFocusChange(!focus)}
              className="shrink-0 gap-1"
            >
              <Crosshair />
              <span className="hidden sm:inline">{t("Focus")}</span>
            </Button>
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

          {/* No rule under the map: the tab rail directly below already draws
              the boundary, and two rules one hairline apart read as a box
              inside a box rather than as two regions. */}
          <div className="px-3 py-2">
            <Neighborhood
              node={detailed ?? current}
              nodes={nodes}
              edges={edges}
              windowSec={windowSec}
              onSelect={onSelect}
              onPreferredWidth={onPreferredWidth}
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
          <Dialog open={confirming} onOpenChange={setConfirming}>
            <DialogContent className="max-w-md" data-testid="map-memory-confirm">
              <DialogHeader>
                <DialogTitle>{t("Read this workload's JVM heap?")}</DialogTitle>
                <DialogDescription>
                  {t("Profiling a JVM asks it for a flight recording, so it is switched on per workload and only for Java processes.")}
                </DialogDescription>
              </DialogHeader>
              <ul className="list-disc space-y-1.5 pl-4 text-xs text-muted-foreground">
                <li>
                  {t("The image must be a JDK: the reader calls jcmd and jfr, and skips a JRE silently.")}
                </li>
                <li>
                  {t("Once per collection interval the JVM pauses briefly to dump and read a recording; expect a few megabytes of churn.")}
                </li>
                <li>
                  {t("Allocation bytes come from the JVM's own sampling, so compare stacks rather than trusting absolute totals.")}
                </li>
                <li>{t("Other runtimes need their own reader; this switch only reaches Java.")}</li>
              </ul>
              <DialogFooter>
                <Button variant="ghost" size="sm" onClick={() => setConfirming(false)}>
                  {t("Cancel")}
                </Button>
                <Button
                  size="sm"
                  data-testid="map-memory-enable"
                  disabled={saving}
                  onClick={() => void applyDirective(true)}
                >
                  {saving ? t("Switching on…") : t("Switch on")}
                </Button>
              </DialogFooter>
            </DialogContent>
          </Dialog>
          {/* The native switch is the one that touches the target: it injects a
              shared library into every matching process, so it says so. */}
          <Dialog open={confirmingNative} onOpenChange={setConfirmingNative}>
            <DialogContent className="max-w-md" data-testid="map-native-confirm">
              <DialogHeader>
                <DialogTitle>{t("Inject the native memory reader?")}</DialogTitle>
                <DialogDescription>
                  {t("This adds a helper container to the pod and loads a shared library into every matching process to hook malloc. The bytes are real allocations, but the hook lives in the target from now on.")}
                </DialogDescription>
              </DialogHeader>
              <ul className="list-disc space-y-1.5 pl-4 text-xs text-muted-foreground">
                <li>
                  {t("A helper (ephemeral) container is added to the pod; the library is injected into its processes, which are not restarted. Ephemeral containers cannot be removed — delete the pod to undo.")}
                </li>
                <li>
                  {t("The target must link libc dynamically, run glibc 2.38 or newer (Ubuntu 24.04+, Debian trixie+), and ship libelf.so.1. Static and musl binaries are skipped with a warning.")}
                </li>
                <li>
                  {t("The agent needs permission to patch pods/ephemeralcontainers in this namespace.")}
                </li>
                <li>
                  {t("An injected hook can crash a process that is already fragile. Switch it on for one workload while you are watching it, not everywhere.")}
                </li>
                <li>
                  {t("Only alloc_space is measured: free is not hooked, so there is no live-heap number, and the totals are relative between stacks. Frames inside the C library resolve to its nearest exported symbol unless the image ships debug symbols.")}
                </li>
              </ul>
              <DialogFooter>
                <Button variant="ghost" size="sm" onClick={() => setConfirmingNative(false)}>
                  {t("Cancel")}
                </Button>
                <Button
                  size="sm"
                  data-testid="map-native-enable"
                  disabled={saving}
                  onClick={() => void applyDirective(true, "alloc")}
                >
                  {saving ? t("Injecting…") : t("Inject and switch on")}
                </Button>
              </DialogFooter>
            </DialogContent>
          </Dialog>
        </>
      ) : null}
    </aside>
  );

  // Nothing is rendered during the static export; the drawer is hidden until
  // a node is selected, so rendering it on the server would add nothing.
  return portalRoot ? createPortal(panelNode, portalRoot) : null;
}
