"use client";

import { useEffect, useRef, useState } from "react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { domainText, selectionText, TimeBrush } from "@/lib/canvas/timebrush";
import { formatClock } from "@/lib/format";
import { dashboardStore } from "@/lib/store";
import { fromLocalInput, nowNs, resolveBaseline, resolveDomain, resolveWindow, toLocalInput } from "@/lib/time";
import type { Window } from "@/lib/time";
import type { TimeseriesPayload } from "@/lib/types";
import { useDashboard } from "@/lib/use-dashboard";

/**
 * One histogram brush. The class owns the canvas; React owns the readout, and
 * gets the in-progress selection through `onPreview` so the numbers follow the
 * handle instead of waiting for pointer-up.
 */
function Brush({
  label,
  domain,
  selection,
  histogram,
  onCommit,
  ready,
}: {
  label: string;
  domain: Window;
  selection: Window;
  histogram: TimeseriesPayload | null;
  onCommit: (from: number, until: number) => void;
  ready: boolean;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const brushRef = useRef<TimeBrush | null>(null);
  const commitRef = useRef(onCommit);
  const [preview, setPreview] = useState<Window | null>(null);

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
        <Button
          variant="ghost"
          size="xs"
          className="brush-action"
          title="Reset this window to the whole range"
          onClick={() => onCommit(domain.from, domain.until)}
        >
          Full
        </Button>
      </div>
      <canvas ref={canvasRef} className="time-brush" height={68} />
    </div>
  );
}

function AbsoluteRangeInputs({ domain }: { domain: Window }) {
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
      dashboardStore.setBanner({ tone: "warning", title: "Pick both ends of the range." });
      return;
    }
    dashboardStore.setAbsoluteDomain(start, end);
  };

  return (
    <div className="flex items-center gap-1.5" id="abs-range" data-testid="absolute-range">
      <label className="flex items-center gap-1">
        <span className="text-[11px] text-muted-foreground">From</span>
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
          aria-label="Absolute range start"
        />
      </label>
      <span className="text-muted-foreground">→</span>
      <label className="flex items-center gap-1">
        <span className="text-[11px] text-muted-foreground">To</span>
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
          aria-label="Absolute range end"
        />
      </label>
      <Button variant="outline" size="xs" id="abs-apply" onClick={apply}>
        Apply
      </Button>
      <Button
        variant="ghost"
        size="xs"
        id="abs-now"
        title="Move the end of the range to now"
        onClick={() => {
          const start = fromLocalInput(from) ?? domain.from;
          dashboardStore.setAbsoluteDomain(start, nowNs());
        }}
      >
        Now
      </Button>
    </div>
  );
}

export function TimeBar() {
  const state = useDashboard();
  const domain = resolveDomain(state);
  const win = resolveWindow(state);
  const baseline = resolveBaseline(state);

  const context =
    state.mode === "absolute"
      ? `absolute · ${dashboardStore.domainSpanText()}`
      : `${dashboardStore.rangeLabel().toLowerCase()} · ends now`;

  const hint =
    state.histEmpty && state.dataRange
      ? `No samples here; data spans ${formatClock(state.dataRange.from)} → ${formatClock(
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
          aria-label="How the time range is chosen"
        >
          <ToggleGroupItem value="relative" data-mode="relative" className="h-7 px-2 text-xs">
            Relative
          </ToggleGroupItem>
          <ToggleGroupItem value="absolute" data-mode="absolute" className="h-7 px-2 text-xs">
            Absolute
          </ToggleGroupItem>
        </ToggleGroup>

        <span className="text-[11px] text-muted-foreground">{context}</span>

        {state.mode === "absolute" ? <AbsoluteRangeInputs domain={domain} /> : null}

        <span className="flex-1" />

        {hint ? <span className="text-[11px] text-muted-foreground">{hint}</span> : null}
        {hint ? (
          <Button
            variant="ghost"
            size="xs"
            id="range-hint-use"
            onClick={() => dashboardStore.useDataRange()}
          >
            Use data range
          </Button>
        ) : null}
      </div>

      {state.view === "compare" ? (
        <div className="grid gap-1.5">
          {/* One host per brush: a brush paints into its own canvas, so two
              brushes in one row would fight over the same width. */}
          <Brush
            label="A · Baseline"
            domain={domain}
            selection={baseline}
            histogram={state.histData}
            ready={state.ready}
            onCommit={(from, until) => dashboardStore.setBaseline(from, until)}
          />
          <Brush
            label="B · Comparison"
            domain={domain}
            selection={win}
            histogram={state.histData}
            ready={state.ready}
            onCommit={(from, until) => dashboardStore.setWindow(from, until)}
          />
        </div>
      ) : (
        <Brush
          label="Time range"
          domain={domain}
          selection={win}
          histogram={state.histData}
          ready={state.ready}
          onCommit={(from, until) => dashboardStore.setWindow(from, until)}
        />
      )}
    </div>
  );
}
