// Stacked time-series chart on a canvas.
//
// One canvas, one draw pass, no chart library: the data is a fixed number of
// buckets per series, so a plain 2D context is both the smallest and the
// fastest option.

import { formatClock, formatValue } from "./format.js";

const PAD = { top: 14, right: 16, bottom: 26, left: 62 };
const PALETTE = [
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

export class TimeSeriesChart {
  constructor(host) {
    this.host = host;
    host.classList.add("panel");
    this.canvas = document.createElement("canvas");
    this.canvas.className = "chart-canvas";
    this.ctx = this.canvas.getContext("2d");
    this.wrap = document.createElement("div");
    this.wrap.className = "chart-wrap";
    this.wrap.appendChild(this.canvas);
    this.legend = document.createElement("div");
    this.legend.className = "legend";
    host.append(this.wrap, this.legend);

    this.hidden = new Set();
    this.hoverIndex = -1;
    this.data = null;
    this.unit = "nanoseconds";

    this.canvas.addEventListener("mousemove", (event) => {
      const rect = this.canvas.getBoundingClientRect();
      this.hoverIndex = this.indexAt(event.clientX - rect.left, rect.width);
      this.draw();
      this.showTooltip(event);
    });
    this.canvas.addEventListener("mouseleave", () => {
      this.hoverIndex = -1;
      this.tooltipEl()?.remove();
      this.draw();
    });
    this.legend.addEventListener("click", (event) => {
      const item = event.target.closest("[data-name]");
      if (!item) return;
      const name = item.dataset.name;
      if (this.hidden.has(name)) this.hidden.delete(name);
      else this.hidden.add(name);
      this.renderLegend();
      this.draw();
    });

    this.resizeObserver = new ResizeObserver(() => this.draw());
    this.resizeObserver.observe(this.wrap);
  }

  destroy() {
    this.resizeObserver?.disconnect();
    this.tooltipEl()?.remove();
    this.host.innerHTML = "";
  }

  setData(payload, unit) {
    this.data = payload;
    this.unit = unit ?? "nanoseconds";
    this.hoverIndex = -1;
    this.renderLegend();
    this.draw();
  }

  visible() {
    return (this.data?.series ?? []).filter((s) => !this.hidden.has(s.service_name));
  }

  seriesLabel(series) {
    const labels = Object.entries(series.labels ?? {})
      .filter(([k]) => k !== "node" && k !== "service_name" && k !== "process_name")
      .map(([k, v]) => `${k}=${v}`)
      .join(" ");
    return labels ? `${series.service_name} ${labels}` : series.service_name;
  }

  renderLegend() {
    const rows = this.data?.series ?? [];
    if (!rows.length) {
      this.legend.innerHTML = "";
      return;
    }
    this.legend.innerHTML = rows
      .map((series, i) => {
        const off = this.hidden.has(series.service_name) ? " off" : "";
        const color = PALETTE[this.colorIndex(series, i)];
        return `<span class="legend-item${off}" data-name="${escape(series.service_name)}">
          <span class="legend-swatch" style="background:${color}"></span>${escape(this.seriesLabel(series))}
        </span>`;
      })
      .join("");
  }

  colorIndex(series, fallback) {
    const rows = this.data?.series ?? [];
    const i = rows.indexOf(series);
    return (i >= 0 ? i : fallback) % PALETTE.length;
  }

  indexAt(px, width) {
    const data = this.data;
    if (!data) return -1;
    const plotWidth = width - PAD.left - PAD.right;
    if (px < PAD.left || px > PAD.left + plotWidth) return -1;
    const buckets = this.bucketCount();
    if (!buckets) return -1;
    const ratio = (px - PAD.left) / plotWidth;
    return Math.min(buckets - 1, Math.max(0, Math.floor(ratio * buckets)));
  }

  bucketCount() {
    return this.data?.series?.[0]?.points?.length ?? 0;
  }

  totals() {
    const buckets = this.bucketCount();
    const out = new Array(buckets).fill(0);
    for (const series of this.visible()) {
      for (let i = 0; i < buckets; i += 1) out[i] += series.points[i] ?? 0;
    }
    return out;
  }

  tooltipEl() {
    return this.wrap.querySelector(".chart-tooltip");
  }

  showTooltip(event) {
    if (this.hoverIndex < 0) {
      this.tooltipEl()?.remove();
      return;
    }
    let el = this.tooltipEl();
    if (!el) {
      el = document.createElement("div");
      el.className = "tooltip chart-tooltip";
      this.wrap.appendChild(el);
    }
    const data = this.data;
    const bucketNs = data.from_ns + this.hoverIndex * data.step_ns;
    const rows = this.visible()
      .map((series) => ({ label: this.seriesLabel(series), value: series.points[this.hoverIndex] ?? 0 }))
      .filter((row) => row.value > 0)
      .sort((a, b) => b.value - a.value)
      .slice(0, 8);
    const total = rows.reduce((acc, row) => acc + row.value, 0);
    el.innerHTML = `
      <div class="tt-title">${formatClock(bucketNs)}</div>
      <div class="tt-row"><span class="tt-key">total</span>${formatValue(total, this.unit)}</div>
      ${rows
        .map(
          (row) =>
            `<div class="tt-row"><span class="tt-key">${formatValue(row.value, this.unit)}</span>${escape(row.label)}</div>`,
        )
        .join("")}
    `;
    const rect = this.wrap.getBoundingClientRect();
    const tooltipRect = el.getBoundingClientRect();
    let left = event.clientX - rect.left + 16;
    if (left + tooltipRect.width > rect.width - 8) left = event.clientX - rect.left - tooltipRect.width - 16;
    el.style.left = `${Math.max(8, left)}px`;
    el.style.top = `${Math.max(8, event.clientY - rect.top + 12)}px`;
    el.hidden = false;
  }

  draw() {
    const data = this.data;
    const width = Math.max(320, this.wrap.clientWidth);
    const height = 260;
    const dpr = window.devicePixelRatio || 1;
    if (this.canvas.width !== Math.round(width * dpr) || this.canvas.height !== Math.round(height * dpr)) {
      this.canvas.width = Math.round(width * dpr);
      this.canvas.height = Math.round(height * dpr);
      this.canvas.style.width = `${width}px`;
      this.canvas.style.height = `${height}px`;
    }
    const ctx = this.ctx;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, height);
    ctx.font = '11px system-ui, sans-serif';

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
    const totals = this.totals();
    let max = 0;
    for (let i = 0; i < buckets; i += 1) {
      let stacked = 0;
      for (const series of this.visible()) stacked += series.points[i] ?? 0;
      if (stacked > max) max = stacked;
    }
    if (max <= 0) max = 1;
    max *= 1.08;

    const xFor = (i) => PAD.left + (i / buckets) * plotWidth;
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
    const columnTotals = new Array(buckets).fill(0);
    const seriesList = this.visible();
    seriesList.forEach((series, order) => {
      const original = this.data.series.indexOf(series);
      ctx.fillStyle = PALETTE[original % PALETTE.length];
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
      void order;
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
      void totals;
    }
  }
}

function escape(text) {
  return String(text).replace(/[&<>"]/g, (c) =>
    c === "&" ? "&amp;" : c === "<" ? "&lt;" : c === ">" ? "&gt;" : "&quot;",
  );
}
