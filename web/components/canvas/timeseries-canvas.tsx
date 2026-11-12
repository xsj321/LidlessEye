"use client";

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";

import { TimeSeriesChart } from "@/lib/canvas/chart";
import type { SeriesHover, SeriesLegendEntry } from "@/lib/canvas/chart";
import type { TimeseriesPayload } from "@/lib/types";
import { cn } from "@/lib/utils";

/**
 * React wrapper around the canvas stacked-bar chart.
 *
 * The legend is a real list of buttons here rather than markup inside the
 * drawing class: toggling a series is a state change the canvas has to be told
 * about, and button semantics (focus, Enter) come for free.
 */
export function TimeseriesCanvas({
  payload,
  unit,
}: {
  payload: TimeseriesPayload;
  unit: string;
}) {
  const wrapRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const chartRef = useRef<TimeSeriesChart | null>(null);
  const tooltipRef = useRef<HTMLDivElement>(null);
  const posRef = useRef({ x: 0, y: 0 });
  const hoverRef = useRef<SeriesHover | null>(null);

  const [legend, setLegend] = useState<SeriesLegendEntry[]>([]);
  const [hover, setHover] = useState<SeriesHover | null>(null);

  const positionTooltip = useCallback(() => {
    const el = tooltipRef.current;
    const wrap = wrapRef.current;
    if (!el || !wrap) return;
    const rect = wrap.getBoundingClientRect();
    const width = el.getBoundingClientRect().width;
    let left = posRef.current.x - rect.left + 16;
    if (left + width > rect.width - 8) left = posRef.current.x - rect.left - width - 16;
    el.style.left = `${Math.max(8, left)}px`;
    el.style.top = `${Math.max(8, posRef.current.y - rect.top + 12)}px`;
  }, []);

  useEffect(() => {
    const canvas = canvasRef.current;
    const wrap = wrapRef.current;
    if (!canvas || !wrap) return;

    const chart = new TimeSeriesChart(canvas, wrap, {
      onLegend: setLegend,
      onHover: (next, clientX, clientY) => {
        posRef.current = { x: clientX, y: clientY };
        const changed = (next?.clock ?? "") !== (hoverRef.current?.clock ?? "");
        if (changed) {
          hoverRef.current = next;
          setHover(next);
        }
        positionTooltip();
      },
    });
    chartRef.current = chart;

    return () => {
      chart.destroy();
      chartRef.current = null;
    };
  }, [positionTooltip]);

  useEffect(() => {
    chartRef.current?.setData(payload, unit);
  }, [payload, unit]);

  useLayoutEffect(() => {
    positionTooltip();
  }, [hover, positionTooltip]);

  return (
    <div className="space-y-2">
      <div ref={wrapRef} className="relative">
        <canvas ref={canvasRef} />
        {hover ? (
          <div
            ref={tooltipRef}
            className="pointer-events-none absolute z-20 min-w-[180px] rounded-md border bg-popover px-2.5 py-2 text-xs text-popover-foreground shadow-md"
            role="tooltip"
          >
            <div className="mb-1 font-mono text-[11px] font-medium">{hover.clock}</div>
            <div className="flex items-center gap-3 leading-5">
              <span className="w-14 shrink-0 text-muted-foreground">total</span>
              <span className="font-mono text-[11px]">{hover.total}</span>
            </div>
            {hover.rows.map((row) => (
              <div key={row.label} className="flex items-center gap-3 leading-5">
                <span className="w-14 shrink-0 font-mono text-[11px]">{row.value}</span>
                <span className="max-w-[240px] truncate text-muted-foreground">{row.label}</span>
              </div>
            ))}
          </div>
        ) : null}
      </div>

      <div className="flex flex-wrap gap-x-3 gap-y-1.5">
        {legend.map((entry) => (
          <button
            key={entry.name}
            type="button"
            data-name={entry.name}
            onClick={() => chartRef.current?.toggleSeries(entry.name)}
            className={cn(
              "flex items-center gap-1.5 rounded-sm px-1 py-0.5 text-[11px] transition-opacity hover:bg-accent",
              entry.hidden && "opacity-40",
            )}
            title={entry.hidden ? "Show this series" : "Hide this series"}
          >
            <span
              className="inline-block size-2 rounded-[2px]"
              style={{ background: entry.color }}
            />
            <span className="max-w-[280px] truncate">{entry.label}</span>
          </button>
        ))}
      </div>
    </div>
  );
}
