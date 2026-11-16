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
import { X } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
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

  return (
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
      // `inert` rather than only `aria-hidden`: the drawer is translated fully
      // off-screen, and without it every button inside it stays in the tab
      // order at a position nobody can see.
      inert={!open}
      className={cn(
        "absolute inset-y-0 right-0 z-20 flex w-[560px] max-w-[calc(100%-16px)] flex-col",
        "border-l bg-card/95 shadow-[-24px_0_48px_-32px_rgba(0,0,0,0.95)] backdrop-blur-sm",
        "outline-none motion-safe:transition-transform motion-safe:duration-300",
        "motion-safe:ease-[cubic-bezier(0.32,0.72,0,1)]",
        open ? "translate-x-0" : "pointer-events-none translate-x-full",
      )}
    >
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
              aria-label="Close panel"
              data-testid="map-panel-close"
              onClick={onClose}
            >
              <X />
            </Button>
          </header>

          <div className="border-b px-3 py-2">
            <div className="mb-1 flex items-center justify-between text-[9px] uppercase tracking-wide text-muted-foreground">
              <span>Neighbours</span>
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
                    {tab.label}
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
            Window from the toolbar · the same one the flamegraph uses
          </footer>
        </>
      ) : null}
    </aside>
  );
}
