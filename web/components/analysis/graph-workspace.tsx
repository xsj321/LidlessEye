"use client";

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import {
  Check,
  Copy,
  Download,
  Maximize2,
  RotateCcw,
  Search,
  X,
  ZoomIn,
  ZoomOut,
} from "lucide-react";

import {
  Breadcrumb,
  BreadcrumbItem,
  BreadcrumbList,
  BreadcrumbSeparator,
} from "@/components/ui/breadcrumb";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import type { Crumb, FlameHover, FlameNodeDetail, LegendEntry } from "@/lib/canvas/flamegraph";
import type { GraphRenderer, RendererFactory } from "@/lib/analysis/renderer";
import { formatPercent, formatValue } from "@/lib/format";
import type { DiffPayload, FlamegraphPayload } from "@/lib/types";

function shorten(label: string) {
  return label.length > 40 ? `${label.slice(0, 39)}…` : label;
}

/** Save a string as a file without a server round trip. */
function download(name: string, text: string, type: string) {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  document.body.appendChild(link);
  link.click();
  link.remove();
  // Revoking immediately can cancel the download in some browsers; a tick is
  // enough for the click to have been handed to the download manager.
  window.setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/**
 * The selected frame, in words.
 *
 * It sits beside the canvas rather than under it: the graph is the thing being
 * read, and a panel that pushes it up or eats its height makes deep stacks
 * scroll. Everything here is also available on hover — the panel is what makes
 * it persistent, selectable and copyable.
 */
function FrameDetails({
  detail,
  onClose,
  onZoom,
  onPick,
  onCopy,
  copied,
}: {
  detail: FlameNodeDetail;
  onClose: () => void;
  onZoom: () => void;
  onPick: (index: number) => void;
  onCopy: () => void;
  copied: boolean;
}) {
  const stackRef = useRef<HTMLOListElement>(null);

  // A deep frame is the common case in a real profile, and its row is at the
  // bottom of the stack list — off-screen unless the list follows the
  // selection. `nearest` keeps the page itself from jumping.
  useEffect(() => {
    stackRef.current
      ?.querySelector('[aria-current="true"]')
      ?.scrollIntoView({ block: "nearest" });
  }, [detail.index]);

  const hasSamples = detail.samples > 0;
  const cells: Array<{ label: string; primary: string; secondary: string }> = [
    {
      label: "Self",
      primary: formatPercent(detail.selfShare, 1),
      secondary: formatValue(detail.selfValue, detail.unit),
    },
    {
      label: "Total",
      primary: formatPercent(detail.totalShare, 1),
      secondary: formatValue(detail.totalValue, detail.unit),
    },
    ...(hasSamples
      ? [
          {
            label: "Samples",
            primary: String(detail.samples),
            secondary: formatPercent(detail.sampleShare, 1),
          },
        ]
      : []),
    {
      label: "Depth",
      primary: String(detail.depth),
      secondary: "deep",
    },
  ];

  return (
    <aside
      aria-label="Selected frame"
      data-testid="frame-details"
      className="w-full shrink-0 rounded-md bg-card p-2.5 md:w-72"
    >
      <div className="mb-1.5 flex items-center gap-2">
        <h3 className="text-[11px] font-medium tracking-wide text-muted-foreground uppercase">
          Selected frame
        </h3>
        <span className="flex-1" />
        <button
          type="button"
          onClick={onClose}
          aria-label="Close frame details"
          className="rounded-sm p-0.5 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
        >
          <X className="size-3.5" />
        </button>
      </div>

      <p className="mb-2 font-mono text-[11px] leading-snug break-all text-foreground">
        {detail.name}
      </p>

      <dl className="mb-2.5 grid grid-cols-2 gap-1.5 text-center">
        {cells.map((cell) => (
          <div key={cell.label} className="rounded-sm bg-secondary/50 px-1.5 py-1">
            <dt className="text-[9px] uppercase text-muted-foreground">{cell.label}</dt>
            <dd className="font-mono text-[11px]">{cell.primary}</dd>
            <dd className="font-mono text-[9px] text-muted-foreground">{cell.secondary}</dd>
          </div>
        ))}
      </dl>

      <div className="mb-1 text-[9px] uppercase tracking-wide text-muted-foreground">Call stack</div>
      <ol ref={stackRef} className="mb-2.5 max-h-40 space-y-0.5 overflow-y-auto">
        {detail.chain.map((node, index) => {
          const current = index === detail.chain.length - 1;
          return (
            <li key={`${node.index}-${index}`}>
              <button
                type="button"
                onClick={() => onPick(node.index)}
                aria-current={current ? "true" : undefined}
                className={
                  current
                    ? "w-full truncate rounded-sm bg-accent px-1 py-0.5 text-left font-mono text-[10px] text-accent-foreground"
                    : "w-full truncate rounded-sm px-1 py-0.5 text-left font-mono text-[10px] text-muted-foreground transition-colors hover:bg-accent/60 hover:text-foreground"
                }
                title={node.name}
              >
                {node.name}
              </button>
            </li>
          );
        })}
      </ol>

      <div className="flex items-center gap-1.5">
        <Button variant="secondary" size="xs" onClick={onZoom}>
          <Maximize2 />
          Zoom in
        </Button>
        <Button variant="ghost" size="xs" onClick={onCopy} title="Copy the frame name (C)">
          {copied ? <Check /> : <Copy />}
          {copied ? "Copied" : "Copy name"}
        </Button>
      </div>
    </aside>
  );
}

/**
 * The analysis shell.
 *
 * Owns every part of reading a profile that is not drawing: search, zoom
 * controls, breadcrumb, export, the details panel and the tooltip. A renderer
 * (see `lib/analysis/renderer`) plugs in and only draws. The flamegraph is one
 * such renderer; a second profile type reuses this shell unchanged.
 */
export function GraphWorkspace({
  payload,
  createRenderer,
  fileStem,
  header,
}: {
  payload: FlamegraphPayload | DiffPayload;
  createRenderer: RendererFactory;
  /** Base name for exports, e.g. `lidlesseye-cpu-1h`. */
  fileStem: string;
  /** Optional strip above the toolbar, e.g. the compare A/B summary. */
  header?: ReactNode;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const graphRef = useRef<GraphRenderer | null>(null);
  // The frames live only in pixels, so the canvas carries a name a screen
  // reader can announce and points at the text alternative (the frame list and
  // the export) instead of being a silent box.
  const canvasLabel =
    "total" in payload && typeof payload.total === "number"
      ? `Flamegraph totalling ${formatValue(payload.total, payload.unit)}. The same numbers are available in the frame list below and in the export.`
      : `Flamegraph comparing two windows. The same numbers are available in the frame list below and in the export.`;
  const tooltipRef = useRef<HTMLDivElement>(null);
  const posRef = useRef({ x: 0, y: 0 });
  const hoverRef = useRef<FlameHover | null>(null);

  const [crumbs, setCrumbs] = useState<Crumb[]>([]);
  const [match, setMatch] = useState("");
  const [legend, setLegend] = useState<LegendEntry[]>([]);
  const [zoom, setZoom] = useState(1);
  const [search, setSearch] = useState("");
  const [hover, setHover] = useState<FlameHover | null>(null);
  const [selected, setSelected] = useState<FlameNodeDetail | null>(null);
  const [copied, setCopied] = useState(false);
  const [exportOpen, setExportOpen] = useState(false);

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

  const copy = useCallback((name: string) => {
    void navigator.clipboard?.writeText(name).then(() => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1400);
    });
  }, []);

  useEffect(() => {
    const canvas = canvasRef.current;
    const scroll = scrollRef.current;
    if (!canvas || !scroll) return;

    const graph = createRenderer(canvas, scroll, {
      onCrumbs: setCrumbs,
      onMatch: setMatch,
      onLegend: setLegend,
      onZoom: setZoom,
      onSelectNode: setSelected,
      onCopyName: copy,
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
  }, [createRenderer, copy, positionTooltip]);

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

  const stamp = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
  const exportFile = (extension: string) => {
    const graph = graphRef.current;
    if (!graph) return;
    if (extension === "svg") {
      download(`${fileStem}-${stamp}.svg`, graph.toSvg(fileStem), "image/svg+xml");
    } else {
      download(`${fileStem}-${stamp}.json`, graph.toJson(), "application/json");
    }
  };

  return (
    <div className="flex min-h-0 flex-col gap-1.5">
      {header}

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

        <div className="flex items-center">
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="icon-xs"
                aria-label="Zoom out"
                onClick={() => graphRef.current?.zoomOut()}
              >
                <ZoomOut />
              </Button>
            </TooltipTrigger>
            <TooltipContent>Zoom out (−)</TooltipContent>
          </Tooltip>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="icon-xs"
                aria-label="Zoom in"
                onClick={() => graphRef.current?.zoomIn()}
              >
                <ZoomIn />
              </Button>
            </TooltipTrigger>
            <TooltipContent>Zoom in (+)</TooltipContent>
          </Tooltip>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="icon-xs"
                aria-label="Reset zoom"
                onClick={() => graphRef.current?.reset()}
              >
                <RotateCcw />
              </Button>
            </TooltipTrigger>
            <TooltipContent>Reset zoom (0)</TooltipContent>
          </Tooltip>
        </div>

        <span className="flex-1" />

        <span className="text-[11px] text-muted-foreground" data-testid="flamegraph-match">
          {match}
        </span>

        <Popover open={exportOpen} onOpenChange={setExportOpen}>
          <PopoverTrigger asChild>
            <Button variant="outline" size="xs" id="flame-export">
              <Download />
              Export
            </Button>
          </PopoverTrigger>
          <PopoverContent align="end" className="w-52 p-1">
            {/* The exports describe the view on screen, not the whole profile:
                a reader who zoomed in wants exactly what they are looking at. */}
            <button
              type="button"
              onClick={() => {
                exportFile("svg");
                setExportOpen(false);
              }}
              className="flex w-full flex-col rounded-sm px-2 py-1.5 text-left transition-colors hover:bg-accent"
            >
              <span className="text-xs">Vector (SVG)</span>
              <span className="text-[10px] text-muted-foreground">
                The current view, as drawn
              </span>
            </button>
            <button
              type="button"
              onClick={() => {
                exportFile("json");
                setExportOpen(false);
              }}
              className="flex w-full flex-col rounded-sm px-2 py-1.5 text-left transition-colors hover:bg-accent"
            >
              <span className="text-xs">Data (JSON)</span>
              <span className="text-[10px] text-muted-foreground">
                Payload, zoom and selection
              </span>
            </button>
          </PopoverContent>
        </Popover>
      </div>

      {/* The breadcrumb is always present: an empty row left the reader with
          no answer to "how deep am I", and it grew a second line the moment
          they zoomed. At the root it says so. */}
      <div className="flex min-h-5 flex-wrap items-center gap-x-3">
        <Breadcrumb>
          <BreadcrumbList className="gap-1 text-xs sm:gap-1">
            {crumbs.length === 0 ? (
              <BreadcrumbItem>
                <span
                  className="px-1 font-mono text-[11px] text-foreground"
                  aria-current="page"
                  data-role="crumb-root"
                >
                  All frames
                </span>
              </BreadcrumbItem>
            ) : (
              <>
                <BreadcrumbItem>
                  <button
                    type="button"
                    className="rounded-sm px-1 font-mono text-[11px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
                    onClick={() => graphRef.current?.reset()}
                    data-role="crumb-root"
                  >
                    All frames
                  </button>
                </BreadcrumbItem>
                {crumbs.map((crumb, index) => (
                  <span key={`${crumb.index}-${index}`} className="flex items-center gap-1">
                    <BreadcrumbSeparator className="[&>svg]:size-3" />
                    <BreadcrumbItem>
                      <button
                        type="button"
                        title={crumb.label}
                        aria-current={index === crumbs.length - 1 ? "page" : undefined}
                        className={
                          index === crumbs.length - 1
                            ? "max-w-[320px] truncate rounded-sm px-1 font-mono text-[11px] text-foreground"
                            : "max-w-[320px] truncate rounded-sm px-1 font-mono text-[11px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
                        }
                        onClick={() => graphRef.current?.focusCrumb(index)}
                      >
                        {shorten(crumb.label)}
                      </button>
                    </BreadcrumbItem>
                  </span>
                ))}
              </>
            )}
          </BreadcrumbList>
        </Breadcrumb>
        <span className="text-[11px] text-faint">
          Click for details · double click to zoom · C copies the name
        </span>
      </div>

      <div className="flex min-h-0 flex-col gap-2 md:flex-row">
        <div className="flex min-w-0 flex-1 flex-col gap-1.5">
          <div ref={scrollRef} className="fg-scroll rounded-md border bg-background/40">
            <canvas
              ref={canvasRef}
              className="fg-canvas"
              // The frames live only in pixels, so the canvas carries a name a
              // screen reader can announce and points at the text alternative
              // (the frame list and the export) instead of being a silent box.
              role="img"
              aria-label={canvasLabel}
            />
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
        </div>

        {selected ? (
          <FrameDetails
            detail={selected}
            onClose={() => graphRef.current?.clearSelection()}
            onZoom={() => graphRef.current?.zoomTo(selected.index)}
            onPick={(index) => graphRef.current?.select(index)}
            copied={copied}
            onCopy={() => copy(selected.name)}
          />
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
