// Stacked time-series chart on a canvas.
//
// One canvas, one draw pass, no chart library: the data is a fixed number of
// buckets per series, so a plain 2D context is both the smallest and the
// fastest option. The legend is React's, because it is a list of buttons that
// toggle series visibility — the canvas only reports which series it drew.

import { formatClock, formatValue } from "@/lib/format";
import type { TimeseriesPayload } from "@/lib/types";

export const SERIES_PALETTE = [
  "#f2a65a",
  "#6ba7f7",
  "#4fbf8b",
  "#e05c5c",
  "#b48ef2",
  "#e2c044",
  "#57c7d4",
  "#f2789f",
  "#9bb64a",
  "#8d93a8",
];

const PAD = { top: 14, right: 16, bottom: 26, left: 62 };

export interface SeriesLegendEntry {
  name: string;
  label: string;
  color: string;
  hidden: boolean;
}

export interface SeriesHover {
  clock: string;
  total: string;
  rows: Array<{ label: string; value: string }>;
}

export interface ChartCallbacks {
  onLegend?: (entries: SeriesLegendEntry[]) => void;
  onHover?: (hover: SeriesHover | null, clientX: number, clientY: number) => void;
}

export class TimeSeriesChart {
  private readonly canvas: HTMLCanvasElement;
  private readonly wrap: HTMLElement;
  private readonly ctx: CanvasRenderingContext2D;
  private readonly callbacks: ChartCallbacks;

  private hidden = new Set<string>();
  private hoverIndex = -1;
  private data: TimeseriesPayload | null = null;
  private unit = "nanoseconds";
  private resizeObserver: ResizeObserver | null;

  constructor(canvas: HTMLCanvasElement, wrap: HTMLElement, callbacks: ChartCallbacks = {}) {
    this.canvas = canvas;
    this.wrap = wrap;
    this.callbacks = callbacks;
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("2D canvas context unavailable");
    this.ctx = ctx;

    canvas.addEventListener("mousemove", (event) => {
      const rect = canvas.getBoundingClientRect();
      this.hoverIndex = this.indexAt(event.clientX - rect.left, rect.width);
      this.draw();
      this.emitHover(event);
    });
    canvas.addEventListener("mouseleave", () => {
      this.hoverIndex = -1;
      this.callbacks.onHover?.(null, 0, 0);
      this.draw();
    });

    this.resizeObserver = new ResizeObserver(() => this.draw());
    this.resizeObserver.observe(wrap);
  }

  destroy() {
    this.resizeObserver?.disconnect();
    this.resizeObserver = null;
    this.callbacks.onHover?.(null, 0, 0);
  }

  setData(payload: TimeseriesPayload | null, unit: string) {
    this.data = payload;
    this.unit = unit ?? "nanoseconds";
    this.hoverIndex = -1;
    this.emitLegend();
    this.draw();
  }

  /** Show/hide one series; the legend calls this. */
  toggleSeries(name: string) {
    if (this.hidden.has(name)) this.hidden.delete(name);
    else this.hidden.add(name);
    this.emitLegend();
    this.draw();
  }

  private visible() {
    return (this.data?.series ?? []).filter((s) => !this.hidden.has(s.service_name));
  }

  private seriesLabel(series: { service_name: string; labels?: Record<string, string> }) {
    const labels = Object.entries(series.labels ?? {})
      .filter(([k]) => k !== "node" && k !== "service_name" && k !== "process_name")
      .map(([k, v]) => `${k}=${v}`)
      .join(" ");
    return labels ? `${series.service_name} ${labels}` : series.service_name;
  }

  private emitLegend() {
    const rows = this.data?.series ?? [];
    this.callbacks.onLegend?.(
      rows.map((series, i) => ({
        name: series.service_name,
        label: this.seriesLabel(series),
        color: SERIES_PALETTE[i % SERIES_PALETTE.length],
        hidden: this.hidden.has(series.service_name),
      })),
    );
  }

  private indexAt(px: number, width: number) {
    const data = this.data;
    if (!data) return -1;
    const plotWidth = width - PAD.left - PAD.right;
    if (px < PAD.left || px > PAD.left + plotWidth) return -1;
    const buckets = this.bucketCount();
    if (!buckets) return -1;
    const ratio = (px - PAD.left) / plotWidth;
    return Math.min(buckets - 1, Math.max(0, Math.floor(ratio * buckets)));
  }

  private bucketCount() {
    return this.data?.series?.[0]?.points?.length ?? 0;
  }

  private emitHover(event: MouseEvent) {
    const data = this.data;
    if (!data || this.hoverIndex < 0) {
      this.callbacks.onHover?.(null, event.clientX, event.clientY);
      return;
    }
    const bucketNs = data.from_ns + this.hoverIndex * data.step_ns;
    const rows = this.visible()
      .map((series) => ({
        label: this.seriesLabel(series),
        value: series.points[this.hoverIndex] ?? 0,
      }))
      .filter((row) => row.value > 0)
      .sort((a, b) => b.value - a.value)
      .slice(0, 8);
    const total = rows.reduce((acc, row) => acc + row.value, 0);
    this.callbacks.onHover?.(
      {
        clock: formatClock(bucketNs),
        total: formatValue(total, this.unit),
        rows: rows.map((row) => ({
          label: row.label,
          value: formatValue(row.value, this.unit),
        })),
      },
      event.clientX,
      event.clientY,
    );
  }

  draw() {
    const data = this.data;
    const width = Math.max(320, this.wrap.clientWidth);
    const height = 260;
    const dpr = window.devicePixelRatio || 1;
    if (
      this.canvas.width !== Math.round(width * dpr) ||
      this.canvas.height !== Math.round(height * dpr)
    ) {
      this.canvas.width = Math.round(width * dpr);
      this.canvas.height = Math.round(height * dpr);
      this.canvas.style.width = `${width}px`;
      this.canvas.style.height = `${height}px`;
    }
    const ctx = this.ctx;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, height);
    ctx.font = "11px system-ui, sans-serif";

    if (!data || !this.bucketCount()) {
      ctx.fillStyle = "#96a0b2";
      ctx.textAlign = "center";
      ctx.fillText("No samples in this range.", width / 2, height / 2);
      ctx.textAlign = "left";
      return;
    }

    const plotWidth = width - PAD.left - PAD.right;
    const plotHeight = height - PAD.top - PAD.bottom;
    const buckets = this.bucketCount();
    let max = 0;
    for (let i = 0; i < buckets; i += 1) {
      let stacked = 0;
      for (const series of this.visible()) stacked += series.points[i] ?? 0;
      if (stacked > max) max = stacked;
    }
    if (max <= 0) max = 1;
    max *= 1.08;

    const xFor = (i: number) => PAD.left + (i / buckets) * plotWidth;
    const barWidth = Math.max(1, plotWidth / buckets - 0.5);

    // Grid + y axis
    ctx.strokeStyle = "rgba(255,255,255,0.06)";
    ctx.fillStyle = "#6b7585";
    ctx.lineWidth = 1;
    for (let g = 0; g <= 4; g += 1) {
      const value = (max * g) / 4;
      const y = PAD.top + plotHeight - (g / 4) * plotHeight;
      ctx.beginPath();
      ctx.moveTo(PAD.left, y + 0.5);
      ctx.lineTo(PAD.left + plotWidth, y + 0.5);
      ctx.stroke();
      ctx.textAlign = "right";
      ctx.fillText(formatValue(value, this.unit), PAD.left - 8, y + 4);
    }

    // Stacked bars
    const columnTotals = new Array<number>(buckets).fill(0);
    const seriesList = this.visible();
    seriesList.forEach((series) => {
      const original = this.data!.series.indexOf(series);
      ctx.fillStyle = SERIES_PALETTE[original % SERIES_PALETTE.length];
      ctx.globalAlpha = 0.85;
      for (let i = 0; i < buckets; i += 1) {
        const value = series.points[i] ?? 0;
        if (value <= 0) continue;
        const base = columnTotals[i];
        const yBottom = PAD.top + plotHeight - (base / max) * plotHeight;
        const yTop = PAD.top + plotHeight - ((base + value) / max) * plotHeight;
        const h = Math.max(1, yBottom - yTop);
        ctx.fillRect(xFor(i), yTop, barWidth, h);
        columnTotals[i] += value;
      }
      ctx.globalAlpha = 1;
    });

    // X axis ticks
    ctx.fillStyle = "#6b7585";
    ctx.textAlign = "center";
    const tickCount = Math.min(8, buckets);
    for (let t = 0; t < tickCount; t += 1) {
      const index = Math.round((t / Math.max(1, tickCount - 1)) * (buckets - 1));
      const ns = data.from_ns + index * data.step_ns;
      ctx.fillText(formatClock(ns), xFor(index) + barWidth / 2, height - 8);
    }
    ctx.textAlign = "left";

    // Hover crosshair
    if (this.hoverIndex >= 0) {
      const px = xFor(this.hoverIndex) + barWidth / 2;
      ctx.strokeStyle = "rgba(242,166,90,0.75)";
      ctx.beginPath();
      ctx.moveTo(px, PAD.top);
      ctx.lineTo(px, PAD.top + plotHeight);
      ctx.stroke();
    }
  }
}
