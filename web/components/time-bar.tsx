"use client";

import { useEffect, useRef, useState } from "react";
import { ChartNoAxesCombined, ChevronDown, ChevronUp, ZoomIn, ZoomOut } from "lucide-react";

import { TimeRangePicker } from "@/components/time-range-picker";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { domainText, selectionText, TimeBrush } from "@/lib/canvas/timebrush";
import type { BrushHover } from "@/lib/canvas/timebrush";
import { formatBytes, formatClock } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { dashboardStore } from "@/lib/store";
import {
  fromLocalInput,
  nowNs,
  resolveBaseline,
  resolveDomain,
  resolveWindow,
  toLocalInput,
} from "@/lib/time";
import type { Window } from "@/lib/time";
import type { MapMetric, TimeseriesPayload } from "@/lib/types";
import { useDashboard } from "@/lib/use-dashboard";
import { cn } from "@/lib/utils";

/** The map's brush plots one of three counters the agent already records. */
const MAP_METRICS: ReadonlyArray<{ value: MapMetric; label: string; noun: string }> = [
  { value: "connections", label: "Requests", noun: "connections" },
  { value: "bytes", label: "Bytes", noun: "bytes" },
  { value: "retransmits", label: "Retransmits", noun: "retransmits" },
];

/**
 * One histogram brush. The class owns the canvas; React owns the readout, and
 * gets the in-progress selection through `onPreview` so the numbers follow the
 * handle instead of waiting for pointer-up.
 */
function Brush({
  label,
  noun,
  bytes = false,
  domain,
  selection,
  histogram,
  onCommit,
  ready,
  t,
}: {
  label: string;
  /** What one bar counts, for the hover readout. */
  noun: string;
  /** Format the bucket total as a byte count rather than a plain number. */
  bytes?: boolean;
  domain: Window;
  selection: Window;
  histogram: TimeseriesPayload | null;
  onCommit: (from: number, until: number) => void;
  ready: boolean;
  t: (key: string) => string;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const brushRef = useRef<TimeBrush | null>(null);
  const commitRef = useRef(onCommit);
  const [preview, setPreview] = useState<Window | null>(null);
  const [hover, setHover] = useState<BrushHover | null>(null);

  commitRef.current = onCommit;

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const brush = new TimeBrush(canvas, {
      onChange: (from, until) => {
        setPreview(null);
        commitRef.current(from, until);
      },
      onPreview: (from, until) => {
        setPreview(from !== null && until !== null ? { from, until } : null);
      },
      onHover: setHover,
    });
    brushRef.current = brush;
    return () => {
      brush.destroy();
      brushRef.current = null;
    };
  }, []);

  // Order matters: the domain defines the coordinate space the selection is
  // painted in, so it has to be set before the selection.
  useEffect(() => {
    brushRef.current?.setDomain(domain.from, domain.until);
  }, [domain.from, domain.until]);

  useEffect(() => {
    brushRef.current?.setSelection(selection.from, selection.until);
  }, [selection.from, selection.until]);

  useEffect(() => {
    brushRef.current?.setHistogram(histogram);
  }, [histogram]);

  const shown = preview ?? selection;

  // Zooming the brush is the timeline's own control: ±2×, anchored on the
  // window's centre, clamped to the domain and to the smallest span the brush
  // will accept. It commits immediately, like dragging does.
  const zoom = (factor: number) => {
    const domainSpan = domain.until - domain.from;
    const min = Math.max(1e9, domainSpan / 300);
    const span = Math.min(domainSpan, Math.max(min, (selection.until - selection.from) * factor));
    const centre = (selection.from + selection.until) / 2;
    const from = Math.min(Math.max(centre - span / 2, domain.from), domain.until - span);
    onCommit(Math.round(from), Math.round(from + span));
  };

  const hoverText = hover
    ? hover.count === null
      ? formatClock(hover.bucketFrom)
      : `${formatClock(hover.bucketFrom)} · ${
          bytes ? formatBytes(hover.count) : `${hover.count.toLocaleString()} ${noun}`
        }`
    : "";

  return (
    <div className="space-y-0.5 brush">
      <div className="flex items-center gap-2">
        <span className="brush-label text-[11px] font-medium text-foreground">{label}</span>
        <span className="font-mono text-[11px] text-foreground/80" data-testid="brush-range">
          {ready ? selectionText(shown) : "–"}
        </span>
        <span
          className="font-mono text-[11px] text-muted-foreground"
          data-role="domain"
        >
          {ready ? domainText(domain) : ""}
        </span>
        <span className="flex-1" />
        {/* Fixed slot: the readout must not shove the controls sideways as the
            pointer crosses buckets. */}
        <span
          className="min-w-[9.5rem] text-right font-mono text-[11px] text-foreground/70 tabular-nums"
          data-testid="brush-hover"
          aria-live="off"
        >
          {hoverText}
        </span>
        <Button
          variant="ghost"
          size="icon-xs"
          className="brush-zoom"
          aria-label={t("Zoom the window out")}
          title={t("Zoom the window out")}
          onClick={() => zoom(2)}
        >
          <ZoomOut />
        </Button>
        <Button
          variant="ghost"
          size="icon-xs"
          className="brush-zoom"
          aria-label={t("Zoom the window in")}
          title={t("Zoom the window in")}
          onClick={() => zoom(0.5)}
        >
          <ZoomIn />
        </Button>
        <Button
          variant="ghost"
          size="xs"
          className="brush-action"
          title={t("Reset this window to the whole range")}
          onClick={() => onCommit(domain.from, domain.until)}
        >
          {t("Full")}
        </Button>
      </div>
      <canvas ref={canvasRef} className="time-brush" height={68} />
    </div>
  );
}

function AbsoluteRangeInputs({
  domain,
  t,
}: {
  domain: Window;
  t: (key: string) => string;
}) {
  const [from, setFrom] = useState(() => toLocalInput(domain.from));
  const [until, setUntil] = useState(() => toLocalInput(domain.until));

  useEffect(() => {
    setFrom(toLocalInput(domain.from));
  }, [domain.from]);
  useEffect(() => {
    setUntil(toLocalInput(domain.until));
  }, [domain.until]);

  const apply = () => {
    const start = fromLocalInput(from);
    const end = fromLocalInput(until);
    if (start === null || end === null) {
      dashboardStore.setBanner({ tone: "warning", title: t("Pick both ends of the range.") });
      return;
    }
    dashboardStore.setAbsoluteDomain(start, end);
  };

  return (
    <div className="flex items-center gap-1.5" id="abs-range" data-testid="absolute-range">
      <label className="flex items-center gap-1">
        <span className="text-[11px] text-muted-foreground">{t("From")}</span>
        <Input
          type="datetime-local"
          id="abs-from"
          step={1}
          value={from}
          onChange={(event) => setFrom(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") apply();
          }}
          className="h-7 w-[190px] text-xs"
          aria-label={t("Absolute range start")}
        />
      </label>
      <span className="text-muted-foreground">→</span>
      <label className="flex items-center gap-1">
        <span className="text-[11px] text-muted-foreground">{t("To")}</span>
        <Input
          type="datetime-local"
          id="abs-until"
          step={1}
          value={until}
          onChange={(event) => setUntil(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") apply();
          }}
          className="h-7 w-[190px] text-xs"
          aria-label={t("Absolute range end")}
        />
      </label>
      <Button variant="outline" size="xs" id="abs-apply" onClick={apply}>
        {t("Apply")}
      </Button>
      <Button
        variant="ghost"
        size="xs"
        id="abs-now"
        title={t("Move the end of the range to now")}
        onClick={() => {
          const start = fromLocalInput(from) ?? domain.from;
          dashboardStore.setAbsoluteDomain(start, nowNs());
        }}
      >
        {t("Now")}
      </Button>
    </div>
  );
}

const COLLAPSED_KEY = "lidlesseye.histogram.collapsed";

function storedCollapsed(): boolean {
  if (typeof window === "undefined") return false;
  try {
    return window.localStorage.getItem(COLLAPSED_KEY) === "1";
  } catch {
    return false;
  }
}

export function TimeBar() {
  const state = useDashboard();
  const t = useT();
  // The histogram is useful for picking a window but expensive in vertical
  // space once the window is set. Collapsing is a local view preference: it
  // stays out of the URL, survives a reload and keeps the brush mounted so its
  // selection and histogram data are still there when it opens again.
  const [collapsed, setCollapsed] = useState(false);

  useEffect(() => {
    setCollapsed(storedCollapsed());
  }, []);

  const toggle = () => {
    setCollapsed((current) => {
      const next = !current;
      try {
        window.localStorage.setItem(COLLAPSED_KEY, next ? "1" : "0");
      } catch {
        /* storage is optional */
      }
      return next;
    });
  };

  const domain = resolveDomain(state);
  const win = resolveWindow(state);
  const baseline = resolveBaseline(state);

  const hint =
    state.histEmpty && state.dataRange
      ? `${t("No samples here; data spans")} ${formatClock(state.dataRange.from)} → ${formatClock(
          state.dataRange.until,
        )}`
      : "";

  return (
    <div className="space-y-1.5 border-b px-3 py-2" data-testid="time-bar">
      <div className="flex flex-wrap items-center gap-2">
        <ToggleGroup
          id="range-mode"
          type="single"
          size="sm"
          variant="outline"
          value={state.mode}
          onValueChange={(value) => {
            if (value === "relative" || value === "absolute") dashboardStore.setMode(value);
          }}
          aria-label={t("How the time range is chosen")}
        >
          <ToggleGroupItem value="relative" data-mode="relative" className="h-7 px-2 text-xs">
            {t("Relative")}
          </ToggleGroupItem>
          <ToggleGroupItem value="absolute" data-mode="absolute" className="h-7 px-2 text-xs">
            {t("Absolute")}
          </ToggleGroupItem>
        </ToggleGroup>

        {/* The relative picker sits right next to the mode switch; both answer
            "which window?", so they belong together. */}
        {state.mode === "relative" ? <TimeRangePicker /> : null}

        {state.mode === "absolute" ? <AbsoluteRangeInputs domain={domain} t={t} /> : null}

        {/* The map's brush used to answer one question — "when was there
            traffic?" — with one number. The agent records three counters per
            edge, and which one matters depends on the incident: a drop in
            connections, a spike in bytes, or retransmits that explain both. */}
        {state.view === "servicemap" ? (
          <ToggleGroup
            id="map-metric"
            data-testid="map-metric"
            type="single"
            size="sm"
            variant="outline"
            value={state.mapMetric}
            onValueChange={(value) => {
              if (value) dashboardStore.setMapMetric(value as MapMetric);
            }}
            aria-label={t("What the time chart counts")}
          >
            {MAP_METRICS.map((metric) => (
              <ToggleGroupItem
                key={metric.value}
                value={metric.value}
                className="h-7 px-2 text-xs"
              >
                {t(metric.label)}
              </ToggleGroupItem>
            ))}
          </ToggleGroup>
        ) : null}

        <span className="flex-1" />

        {hint ? <span className="text-[11px] text-muted-foreground">{hint}</span> : null}
        {hint ? (
          <Button
            variant="ghost"
            size="xs"
            id="range-hint-use"
            onClick={() => dashboardStore.useDataRange()}
          >
            {t("Use data range")}
          </Button>
        ) : null}

        <Button
          variant="ghost"
          size="xs"
          id="histogram-toggle"
          data-testid="histogram-toggle"
          aria-expanded={!collapsed}
          aria-controls="time-histogram"
          title={t(collapsed ? "Show the histogram" : "Hide the histogram")}
          onClick={toggle}
          className="gap-1 text-muted-foreground"
        >
          {collapsed ? <ChevronDown /> : <ChevronUp />}
          <ChartNoAxesCombined />
          {t(collapsed ? "Show chart" : "Hide chart")}
        </Button>
      </div>

      {/* A zero-height grid row keeps both brushes mounted: the canvas keeps
          its domain, selection and histogram, and the ResizeObserver redraws it
          at the right width when the row opens again. */}
      <div
        id="time-histogram"
        data-testid="time-histogram"
        data-collapsed={collapsed ? "true" : "false"}
        className={cn(
          "grid motion-safe:transition-[grid-template-rows,opacity] motion-safe:duration-200",
          collapsed ? "grid-rows-[0fr] opacity-0" : "grid-rows-[1fr] opacity-100",
        )}
      >
        <div className="min-h-0 overflow-hidden">
          {state.view === "compare" ? (
            <div className="grid gap-1.5 pt-1">
              {/* One host per brush: a brush paints into its own canvas, so two
                  brushes in one row would fight over the same width. */}
              <Brush
                label={t("A · Baseline")}
                noun={t("samples")}
                domain={domain}
                selection={baseline}
                histogram={state.histData}
                ready={state.ready}
                t={t}
                onCommit={(from, until) => dashboardStore.setBaseline(from, until)}
              />
              <Brush
                label={t("B · Comparison")}
                noun={t("samples")}
                domain={domain}
                selection={win}
                histogram={state.histData}
                ready={state.ready}
                t={t}
                onCommit={(from, until) => dashboardStore.setWindow(from, until)}
              />
            </div>
          ) : (
            <div className="pt-1">
              <Brush
                label={t(
                  state.view === "servicemap"
                    ? (MAP_METRICS.find((metric) => metric.value === state.mapMetric)?.label ??
                        "Requests")
                    : "Time range",
                )}
                noun={t(
                  state.view === "servicemap"
                    ? (MAP_METRICS.find((metric) => metric.value === state.mapMetric)?.noun ??
                        "connections")
                    : "samples",
                )}
                bytes={state.view === "servicemap" && state.mapMetric === "bytes"}
                domain={domain}
                selection={win}
                histogram={state.histData}
                ready={state.ready}
                t={t}
                onCommit={(from, until) => dashboardStore.setWindow(from, until)}
              />
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
