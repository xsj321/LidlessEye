"use client";

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { RotateCcw, Search } from "lucide-react";

import {
  Breadcrumb,
  BreadcrumbItem,
  BreadcrumbList,
  BreadcrumbSeparator,
} from "@/components/ui/breadcrumb";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Flamegraph } from "@/lib/canvas/flamegraph";
import type { Crumb, FlameHover, LegendEntry } from "@/lib/canvas/flamegraph";
import { formatPercent } from "@/lib/format";
import type { DiffPayload, FlamegraphPayload } from "@/lib/types";

function shorten(label: string) {
  return label.length > 40 ? `${label.slice(0, 39)}…` : label;
}

/**
 * React wrapper around the canvas flamegraph.
 *
 * The drawing class is unchanged; this component owns the toolbar, breadcrumb
 * and legend (so they are real buttons with real focus rings) and forwards the
 * callbacks. The tooltip is rendered here for the same reason: it is a shadcn
 * surface, and positioning it from a `fixed` layer is what lets the canvas
 * report plain viewport coordinates.
 */
export function FlamegraphCanvas({
  payload,
}: {
  payload: FlamegraphPayload | DiffPayload;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const graphRef = useRef<Flamegraph | null>(null);
  const tooltipRef = useRef<HTMLDivElement>(null);
  const posRef = useRef({ x: 0, y: 0 });
  const hoverRef = useRef<FlameHover | null>(null);

  const [crumbs, setCrumbs] = useState<Crumb[]>([]);
  const [match, setMatch] = useState("");
  const [legend, setLegend] = useState<LegendEntry[]>([]);
  const [zoom, setZoom] = useState(1);
  const [search, setSearch] = useState("");
  const [hover, setHover] = useState<FlameHover | null>(null);

  const positionTooltip = useCallback(() => {
    const el = tooltipRef.current;
    if (!el) return;
    const pad = 14;
    const rect = el.getBoundingClientRect();
    let left = posRef.current.x + pad;
    let top = posRef.current.y + pad;
    if (left + rect.width > window.innerWidth - 8) left = posRef.current.x - rect.width - pad;
    if (top + rect.height > window.innerHeight - 8) top = posRef.current.y - rect.height - pad;
    el.style.left = `${Math.max(8, left)}px`;
    el.style.top = `${Math.max(8, top)}px`;
  }, []);

  useEffect(() => {
    const canvas = canvasRef.current;
    const scroll = scrollRef.current;
    if (!canvas || !scroll) return;

    const graph = new Flamegraph(canvas, scroll, {
      onCrumbs: setCrumbs,
      onMatch: setMatch,
      onLegend: setLegend,
      onZoom: setZoom,
      onHover: (next, clientX, clientY) => {
        posRef.current = { x: clientX, y: clientY };
        const changed = (next?.index ?? -1) !== (hoverRef.current?.index ?? -1);
        if (changed) {
          hoverRef.current = next;
          setHover(next);
        }
        positionTooltip();
      },
    });
    graphRef.current = graph;

    return () => {
      graph.destroy();
      graphRef.current = null;
    };
  }, [positionTooltip]);

  useEffect(() => {
    graphRef.current?.setData(payload);
  }, [payload]);

  useEffect(() => {
    graphRef.current?.setSearch(search);
  }, [search]);

  // The tooltip's height depends on the frame name, so it has to be measured
  // again once the new content is in the DOM.
  useLayoutEffect(() => {
    positionTooltip();
  }, [hover, positionTooltip]);

  return (
    <div className="flex min-h-0 flex-col gap-1.5">
      <div className="flex flex-wrap items-center gap-2">
        <div className="relative">
          <Search className="pointer-events-none absolute top-1/2 left-2 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            type="search"
            placeholder="Search frames…"
            aria-label="Search frames"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            className="h-7 w-56 pl-7 text-xs"
          />
        </div>
        <Button variant="ghost" size="xs" onClick={() => graphRef.current?.reset()}>
          <RotateCcw />
          Reset zoom
        </Button>
        <span className="flex-1" />
        <span className="text-[11px] text-muted-foreground" data-testid="flamegraph-match">
          {match}
        </span>
      </div>

      <div className="min-h-5">
        {crumbs.length ? (
          <Breadcrumb>
            <BreadcrumbList className="gap-1 text-xs sm:gap-1">
              {crumbs.map((crumb, index) => (
                <span key={`${crumb.index}-${index}`} className="flex items-center gap-1">
                  {index > 0 ? <BreadcrumbSeparator className="[&>svg]:size-3" /> : null}
                  <BreadcrumbItem>
                    <button
                      type="button"
                      title={crumb.label}
                      className="max-w-[320px] truncate rounded-sm px-1 font-mono text-[11px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
                      onClick={() => graphRef.current?.focusCrumb(index)}
                    >
                      {shorten(crumb.label)}
                    </button>
                  </BreadcrumbItem>
                </span>
              ))}
            </BreadcrumbList>
          </Breadcrumb>
        ) : (
          <span className="text-[11px] text-faint">
            Click a frame to zoom, Esc to go back.
          </span>
        )}
      </div>

      <div ref={scrollRef} className="fg-scroll rounded-md border bg-background/40">
        <canvas ref={canvasRef} className="fg-canvas" />
      </div>

      <div className="fg-legend flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-muted-foreground">
        {legend.map((entry, index) => (
          <span key={`${entry.text}-${index}`} className="flex items-center gap-1.5">
            {entry.swatch ? (
              <span
                className="inline-block size-2 rounded-[2px]"
                style={{
                  background:
                    entry.swatch === "diff-up" ? "hsl(358 60% 42%)" : "hsl(148 60% 42%)",
                }}
              />
            ) : null}
            {entry.text}
          </span>
        ))}
        {zoom < 1 ? (
          <span className="text-faint" data-role="zoom">
            {formatPercent(zoom, 1)} of total shown
          </span>
        ) : null}
      </div>

      {hover ? (
        <div
          ref={tooltipRef}
          className="pointer-events-none fixed z-50 max-w-xs rounded-md border bg-popover px-2.5 py-2 text-xs text-popover-foreground shadow-md"
          role="tooltip"
        >
          <div className="mb-1 max-w-[300px] truncate font-mono text-[11px] font-medium">
            {hover.title}
          </div>
          {hover.rows.map((row) => (
            <div key={row.key} className="flex items-center gap-3 leading-5">
              <span className="w-20 shrink-0 text-muted-foreground">{row.key}</span>
              <span className="font-mono text-[11px]">{row.value}</span>
            </div>
          ))}
        </div>
      ) : null}
    </div>
  );
}
