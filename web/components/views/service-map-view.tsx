"use client";

// The service map.
//
// A layered graph read left to right: entry points on the left, what they call
// to their right. Nodes are DOM cards (`Card`), edges are one SVG, and both sit
// in a single transformed layer so pan and zoom are a CSS transform rather than
// a re-layout.
//
// The map answers "who talks to whom, how much, and how fast" — every edge is
// labelled with its rate and its round-trip time — and a click opens the panel
// that answers "and how is it doing", one hop of neighbours plus seven tabs of
// measurements.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Search, X } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { EmptyState } from "@/components/empty-state";
import { Input } from "@/components/ui/input";
import { formatCount, formatDuration } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { EDGE_DASH, EDGE_STYLE, KIND, nodeLabel, searchNodes } from "@/lib/servicemap-graph";
import { dashboardStore } from "@/lib/store";
import type { ServiceMapNode } from "@/lib/types";
import { useDashboard } from "@/lib/use-dashboard";
import { cn } from "@/lib/utils";

import { ServiceGraph } from "./service-map/graph-canvas";
import { NodePanel } from "./service-map/node-panel";

function LegendSwatch({ kind }: { kind: keyof typeof KIND }) {
  return (
    <span className="relative inline-flex h-2.5 w-4 overflow-hidden rounded-[3px] border border-white/10">
      <span className="h-full w-[2px]" style={{ background: KIND[kind].bar }} />
      <span className="h-full flex-1" style={{ background: KIND[kind].tint }} />
    </span>
  );
}

/**
 * One line of the edge key: the stroke the map would draw, and the label.
 *
 * A sample is not decoration here — `strokeDasharray` and the colour are read
 * from the same constants the canvas uses, so tuning the map cannot leave the
 * legend telling a different story.
 */
function EdgeSample({
  tone,
  label,
}: {
  tone: "idle" | "emphasis" | "warn" | "external";
  label: string;
}) {
  const color = EDGE_STYLE[tone];
  const dashed = tone === "warn" || tone === "external";
  return (
    <span className="flex items-center gap-1">
      <svg viewBox="0 0 26 8" className="h-2 w-[26px] shrink-0" aria-hidden="true">
        <line
          x1="0"
          y1="4"
          x2="18"
          y2="4"
          stroke={color}
          strokeWidth={tone === "emphasis" ? 2.2 : 1.6}
          strokeLinecap="round"
          strokeDasharray={dashed ? EDGE_DASH[tone as "warn" | "external"] : undefined}
        />
        <path d="M18.4 1.3 L24.4 4 L18.4 6.7 z" fill={color} />
      </svg>
      {label}
    </span>
  );
}

function StatusGlyphLegend({ tone }: { tone: "ok" | "warn" | "muted" }) {
  const color = tone === "ok" ? "#4fbf8b" : tone === "warn" ? "#f2a65a" : "#6b7585";
  return (
    <svg viewBox="0 0 12 12" className="size-2.5" aria-hidden="true">
      {tone === "ok" ? <circle cx="6" cy="6" r="4" fill={color} /> : null}
      {tone === "warn" ? <path d="M6 1.3 11.2 10.5H0.8z" fill={color} /> : null}
      {tone === "muted" ? (
        <circle cx="6" cy="6" r="3.5" fill="none" stroke={color} strokeWidth="1.4" />
      ) : null}
    </svg>
  );
}

export function ServiceMapView() {
  const state = useDashboard();
  const t = useT();
  const map = state.serviceMap;
  const [selected, setSelected] = useState<string | null>(null);
  // Focus belongs to the view, not to the canvas: the control that turns it on
  // lives in the node panel, which is open exactly when a node is selected.
  const [focus, setFocus] = useState(false);

  // ---- search ------------------------------------------------------------
  //
  // The box sits in the map's own row rather than floating over the canvas:
  // an overlay covers the first column of cards, which is exactly where the
  // entry points are.
  const [query, setQuery] = useState("");
  const [searchOpen, setSearchOpen] = useState(false);
  const [cursor, setCursor] = useState(0);
  const searchInput = useRef<HTMLInputElement | null>(null);
  const [center, setCenter] = useState<{ id: string; nonce: number } | null>(null);

  const allNodes = useMemo(() => map?.nodes ?? [], [map]);
  const matches = useMemo(() => searchNodes(allNodes, query), [allNodes, query]);
  const highlight = useMemo(
    () => (searchOpen && query ? new Set(matches.map((node) => node.id)) : undefined),
    [searchOpen, query, matches],
  );

  const locate = useCallback((node: ServiceMapNode) => {
    setQuery(nodeLabel(node));
    setSearchOpen(false);
    setCursor(0);
    // A hit is a jump, and a lens on the previous node would hide it.
    setFocus(false);
    setSelected(node.id);
    setCenter((previous) => ({ id: node.id, nonce: (previous?.nonce ?? 0) + 1 }));
  }, []);

  // A result list that stays open after the reader has moved on covers the map
  // it was opened to explain. Any press outside the box dismisses it.
  useEffect(() => {
    if (!searchOpen) return;
    const onPointerDown = (event: PointerEvent) => {
      const target = event.target as Element | null;
      if (target?.closest("[data-map-search]")) return;
      setSearchOpen(false);
    };
    window.addEventListener("pointerdown", onPointerDown);
    return () => window.removeEventListener("pointerdown", onPointerDown);
  }, [searchOpen]);

  // `/` and Ctrl/Cmd-K reach the box from anywhere on the page, the way they do
  // in every tool with a canvas of things. A keystroke that starts inside a
  // control belongs to that control.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (target?.matches?.("input, textarea, select, [contenteditable='true']")) return;
      const wantsSearch =
        event.key === "/" || ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k");
      if (!wantsSearch) return;
      event.preventDefault();
      searchInput.current?.focus();
      searchInput.current?.select();
      setSearchOpen(true);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  const byId = useMemo(
    () => new Map<string, ServiceMapNode>((map?.nodes ?? []).map((node) => [node.id, node])),
    [map],
  );

  // A refresh can drop the node the panel is showing (the window moved, the
  // workload went away). Closing the panel is the honest reaction: the numbers
  // it would keep displaying describe a node that is no longer on the map.
  useEffect(() => {
    if (selected && !byId.has(selected)) {
      setSelected(null);
      setFocus(false);
    }
  }, [byId, selected]);

  const onSelect = useCallback((id: string | null) => {
    setSelected(id);
    // Focus is a lens on one node; without that node there is nothing to look
    // through, so clearing the selection also drops the lens.
    if (!id) setFocus(false);
  }, []);

  const onSelectRequired = useCallback((id: string) => {
    setSelected(id);
  }, []);

  const close = useCallback(() => {
    setSelected(null);
    setFocus(false);
  }, []);

  // The drawer covers the right-hand part of the map. The canvas needs that
  // number to centre a search hit or fit a focused drawing where it can be
  // seen, so the panel reports its width whenever it changes.
  const [panelWidth, setPanelWidth] = useState(0);
  const onPanelLayout = useCallback((width: number, open: boolean) => {
    setPanelWidth((previous) => {
      const next = open ? width : 0;
      return previous === next ? previous : next;
    });
  }, []);

  // Pinned to the payload, not recomputed per render: a relative range moves
  // with the clock, and a window that changes on every render would make the
  // panel's data hook re-query in a loop. Declared before the empty-state
  // return so the hook order does not change when the first payload lands.
  const range = useMemo(
    () => (map ? dashboardStore.resolveWindow() : { from: 0, until: 0 }),
    [map],
  );

  if (!map) {
    return (
      <EmptyState
        info={
          state.empty
            ? {
                ...state.empty,
                title: t(state.empty.title),
                hint: t(state.empty.hint),
              }
            : {
                title: t("No service map yet"),
                hint: t(
                  "The map is built by the agent from the pods' TCP tables and stored with the profiles.",
                ),
              }
        }
      />
    );
  }

  const windowSec = Math.max(1, (range.until - range.from) / 1e9);
  const totalConnections = map.edges.reduce((sum, edge) => sum + (edge.connections ?? 0), 0);
  const workloads = map.nodes.filter((node) => node.kind === "workload").length;
  const services = map.nodes.filter((node) => node.kind === "service").length;
  const selectedNode = selected ? (byId.get(selected) ?? null) : null;
  const cpuOfSelected = selectedNode?.cpu_ns ?? 0;

  return (
    <div className="relative flex h-full min-h-0 flex-col gap-2">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1 pr-1 text-[11px] text-muted-foreground">
        <span data-testid="map-summary">
          {workloads} {t("workloads")} · {services} {t("services")} · {map.edges.length}{" "}
          {t("edges")} · {formatCount(totalConnections)} {t("new connections")}
        </span>
        <span className="flex items-center gap-1">
          <LegendSwatch kind="workload" />
          {t("workload")}
        </span>
        <span className="flex items-center gap-1">
          <LegendSwatch kind="service" />
          ClusterIP
        </span>
        <span className="flex items-center gap-1">
          <LegendSwatch kind="external" />
          {t("external")}
        </span>
        {/* A divider, because the swatches before it describe nodes and the
            samples after it describe the lines between them: two different
            questions that would otherwise read as one long row. */}
        <span aria-hidden="true" className="h-3 w-px bg-border" />
        {/* The edge key. Four inks, in the order a reader meets them: an
            ordinary hop, the hops of whatever is selected, a retransmit
            warning, and a call that leaves the cluster. The samples are drawn
            from the same constants the canvas strokes with, so the legend
            cannot end up describing a style the map no longer draws. */}
        <span
          className="flex items-center gap-2"
          role="group"
          aria-label={t("Edge legend")}
          data-testid="map-edge-legend"
        >
          <EdgeSample tone="idle" label={t("call")} />
          <EdgeSample tone="emphasis" label={t("selected")} />
          <EdgeSample tone="warn" label={t("retransmit")} />
          <EdgeSample tone="external" label={t("outside")} />
        </span>
        <span className="flex items-center gap-1">
          <StatusGlyphLegend tone="ok" />
          {t("all ready")}
        </span>
        <span className="flex items-center gap-1">
          <StatusGlyphLegend tone="warn" />
          {t("degraded")}
        </span>
        <span className="flex items-center gap-1">
          <StatusGlyphLegend tone="muted" />
          {t("no replicas")}
        </span>
        <span className="flex items-center gap-1">
          <span className="inline-block h-[2px] w-4 rounded bg-[#f2a65a]" />
          CPU
        </span>
        <span className="text-muted-foreground/70">
          {t("arrow = direction · width = traffic")}
        </span>
        {map.truncated ? <Badge variant="secondary">{t("clipped to 600 nodes")}</Badge> : null}
        {/* Search: the only way to find one card among sixty without reading
            sixty cards. The list opens over the map, not over the legend. */}
        <div className="relative ml-auto w-[248px]" data-testid="map-search-wrap" data-map-search="">
          <Search
            aria-hidden="true"
            className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground"
          />
          <Input
            ref={searchInput}
            id="map-search"
            data-testid="map-search"
            value={query}
            role="combobox"
            aria-expanded={searchOpen && Boolean(query)}
            aria-controls="map-search-results"
            aria-autocomplete="list"
            aria-label={t("Find a node")}
            placeholder={t("Find a node")}
            className="h-7 pl-7 pr-12 text-xs"
            onFocus={() => setSearchOpen(true)}
            onChange={(event) => {
              setQuery(event.target.value);
              setCursor(0);
              setSearchOpen(true);
            }}
            onKeyDown={(event) => {
              if (event.key === "ArrowDown") {
                event.preventDefault();
                setCursor((current) => Math.min(current + 1, Math.max(matches.length - 1, 0)));
              } else if (event.key === "ArrowUp") {
                event.preventDefault();
                setCursor((current) => Math.max(current - 1, 0));
              } else if (event.key === "Enter") {
                const hit = matches[cursor] ?? matches[0];
                if (hit) {
                  event.preventDefault();
                  locate(hit);
                }
              } else if (event.key === "Escape") {
                event.preventDefault();
                if (query) setQuery("");
                else setSearchOpen(false);
                searchInput.current?.blur();
              }
            }}
          />
          {query ? (
            <button
              type="button"
              aria-label={t("Clear search")}
              className="absolute right-7 top-1/2 -translate-y-1/2 rounded p-0.5 text-muted-foreground hover:text-foreground"
              onClick={() => {
                setQuery("");
                setSearchOpen(false);
                searchInput.current?.focus();
              }}
            >
              <X className="size-3" />
            </button>
          ) : (
            <kbd className="pointer-events-none absolute right-2 top-1/2 -translate-y-1/2 rounded border border-white/10 px-1 font-mono text-[9px] text-muted-foreground">
              /
            </kbd>
          )}

          {searchOpen && query ? (
            <div
              id="map-search-results"
              role="listbox"
              aria-label={t("Find a node")}
              className="absolute left-0 top-full z-40 mt-1 w-[300px] overflow-hidden rounded-md border bg-popover shadow-xl"
            >
              {matches.length ? (
                matches.map((node, index) => (
                  <button
                    key={node.id}
                    type="button"
                    role="option"
                    aria-selected={index === cursor}
                    data-testid="map-search-hit"
                    className={cn(
                      "flex w-full items-baseline gap-2 px-2 py-1.5 text-left text-[11px]",
                      index === cursor ? "bg-accent text-foreground" : "text-foreground/80",
                    )}
                    onPointerEnter={() => setCursor(index)}
                    onClick={() => locate(node)}
                  >
                    <span className="min-w-0 flex-1 truncate">{nodeLabel(node)}</span>
                    <span className="shrink-0 truncate font-mono text-[9px] text-muted-foreground">
                      {node.namespace || node.kind}
                    </span>
                  </button>
                ))
              ) : (
                <p className="px-2 py-1.5 text-[11px] text-muted-foreground">
                  {t("No node matches")}
                </p>
              )}
            </div>
          ) : null}
        </div>
        {selectedNode ? (
          <span className="flex items-center gap-2 text-[10px]">
            <span className="text-foreground">{nodeLabel(selectedNode)}</span>
            {cpuOfSelected ? <span>· CPU {formatDuration(cpuOfSelected)}</span> : null}
            <span>
              · {selectedNode.connections ?? 0} {t("connections")}
            </span>
          </span>
        ) : null}
      </div>

      <div className="min-h-0 flex-1" data-testid="service-map-canvas">
        <ServiceGraph
          nodes={map.nodes}
          edges={map.edges}
          windowSec={windowSec}
          selected={selected}
          onSelect={onSelect}
          focus={focus}
          onFocusChange={setFocus}
          center={center}
          highlight={highlight}
          insetRight={panelWidth}
        />
      </div>

      <NodePanel
        node={selectedNode}
        nodes={byId}
        edges={map.edges}
        range={range}
        onSelect={onSelectRequired}
        onClose={close}
        focus={focus && Boolean(selectedNode)}
        onFocusChange={setFocus}
        onLayout={onPanelLayout}
      />
    </div>
  );
}
