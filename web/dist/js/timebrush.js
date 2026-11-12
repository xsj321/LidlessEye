// Histogram time-range selector.
//
// A small canvas showing sample density over the **domain** — the range chosen
// with the time picker (a relative preset or an absolute from/until). Inside
// that domain a draggable window picks the analysis range: drag the middle to
// move, the edges to resize, or empty space to draw a new window. When the
// window still covers the whole domain there is nothing to move, so a drag
// inside starts a new selection instead (zoom, then pan).
//
// The component never fetches: app.js feeds it the histogram (from
// /api/v1/timeseries over the domain) and the selection, and reacts to
// `onChange` — fired once per gesture, on pointer-up, not during the drag.

import { formatClock, formatDuration } from "./format.js";

const HEIGHT = 68; // css px: bars + axis labels + head room
const BAR_TOP = 8;
const BAR_BOTTOM = 48;
const HANDLE_PX = 8; // grabable width of each edge, in css px
const MIN_SPAN_FRACTION = 1 / 300; // of the domain; below this resizing stops

export class TimeBrush {
  /**
   * @param {HTMLElement} host container, rendered into
   * @param {{ label: string, onChange: (from: number, until: number) => void }} options
   */
  constructor(host, { label, onChange } = {}) {
    this.host = host;
    this.label = label ?? "Time range";
    this.onChange = onChange ?? (() => {});
    this.domain = null; // {from, until} in ns
    this.histogram = null; // {from_ns, until_ns, step_ns, totals: number[]}
    this.sel = null; // {from, until} in ns
    this.drag = null; // active gesture: {mode, anchorNs, grabNs, startSel}

    host.innerHTML = `
      <div class="brush">
        <div class="brush-head">
          <span class="brush-label">${escape(this.label)}</span>
          <span class="brush-range mono" data-role="range">–</span>
          <span class="brush-domain faint mono" data-role="domain"></span>
        </div>
        <canvas class="time-brush" height="${HEIGHT}"></canvas>
      </div>
    `;
    this.headEl = host.querySelector(".brush-head");
    this.canvas = host.querySelector("canvas.time-brush");
    this.rangeEl = host.querySelector('[data-role="range"]');
    this.domainEl = host.querySelector('[data-role="domain"]');
    this.ctx = this.canvas.getContext("2d");

    this.canvas.addEventListener("pointerdown", (event) => this.onPointerDown(event));
    this.canvas.addEventListener("pointermove", (event) => this.onPointerMove(event));
    this.canvas.addEventListener("pointerup", (event) => this.onPointerUp(event));
    this.canvas.addEventListener("pointerleave", () => {
      if (!this.drag) this.canvas.style.cursor = "";
    });

    this.resizeObserver = new ResizeObserver(() => this.draw());
    this.resizeObserver.observe(this.canvas);
  }

  destroy() {
    this.resizeObserver?.disconnect();
    this.host.innerHTML = "";
  }

  /** The range the bracket may slide in; also what the histogram spans. */
  setDomain(from, until) {
    this.domain = { from, until };
    this.draw();
  }

  /** Bucket totals from /api/v1/timeseries, summed over its series. */
  setHistogram(payload) {
    if (!payload) {
      this.histogram = null;
      this.draw();
      return;
    }
    const buckets = payload.series?.[0]?.points?.length ?? 0;
    const totals = new Array(buckets).fill(0);
    for (const series of payload.series ?? []) {
      for (let i = 0; i < buckets; i += 1) totals[i] += series.points?.[i] ?? 0;
    }
    this.histogram = {
      from_ns: payload.from_ns,
      until_ns: payload.until_ns,
      step_ns: payload.step_ns,
      totals,
    };
    this.draw();
  }

  /** External selection update (picker, hash, auto-refresh). */
  setSelection(from, until) {
    this.sel = { from, until };
    this.draw();
  }

  selection() {
    return this.sel ? { ...this.sel } : null;
  }

  // ---- geometry ---------------------------------------------------------

  plotWidth() {
    return Math.max(10, this.canvas.clientWidth);
  }

  xFor(ns) {
    const { from, until } = this.domain;
    return ((ns - from) / (until - from)) * this.plotWidth();
  }

  nsFor(px) {
    const { from, until } = this.domain;
    return from + (px / this.plotWidth()) * (until - from);
  }

  minSpan() {
    if (!this.domain) return 1e9;
    return Math.max(1e9, (this.domain.until - this.domain.from) * MIN_SPAN_FRACTION);
  }

  edgePx() {
    return {
      left: this.xFor(this.sel.from),
      right: this.xFor(this.sel.until),
    };
  }

  /** Is the window still the whole domain? Then a drag must select, not move. */
  coversDomain() {
    if (!this.domain || !this.sel) return true;
    return this.sel.until - this.sel.from >= (this.domain.until - this.domain.from) * 0.999;
  }

  // ---- interaction ------------------------------------------------------

  onPointerDown(event) {
    if (!this.domain || !this.sel) return;
    const rect = this.canvas.getBoundingClientRect();
    const px = event.clientX - rect.left;
    const { left, right } = this.edgePx();
    this.canvas.setPointerCapture(event.pointerId);
    let mode;
    if (Math.abs(px - left) <= HANDLE_PX) mode = "resizeL";
    else if (Math.abs(px - right) <= HANDLE_PX) mode = "resizeR";
    else if (px > left && px < right && !this.coversDomain()) mode = "move";
    else mode = "create";
    this.drag = {
      mode,
      anchorNs: this.nsFor(px),
      grabNs: this.nsFor(px),
      startSel: { ...this.sel },
      moved: false,
    };
    this.canvas.dataset.drag = mode;
    this.canvas.style.cursor = mode === "create" ? "crosshair" : "ew-resize";
  }

  onPointerMove(event) {
    const rect = this.canvas.getBoundingClientRect();
    const px = event.clientX - rect.left;
    if (!this.drag) {
      if (!this.domain || !this.sel) return;
      const { left, right } = this.edgePx();
      const near = Math.abs(px - left) <= HANDLE_PX || Math.abs(px - right) <= HANDLE_PX;
      const inside = px > left && px < right && !this.coversDomain();
      this.canvas.style.cursor = near ? "ew-resize" : inside ? "grab" : "crosshair";
      return;
    }
    if (Math.abs(px - this.xFor(this.drag.anchorNs)) > 3) this.drag.moved = true;
    const ns = clamp(this.nsFor(px), this.domain.from, this.domain.until);
    const start = this.drag.startSel;
    const min = this.minSpan();

    if (this.drag.mode === "move") {
      const span = start.until - start.from;
      const shift = ns - this.drag.grabNs;
      let from = start.from + shift;
      from = clamp(from, this.domain.from, this.domain.until - span);
      this.sel = { from, until: from + span };
    } else if (this.drag.mode === "resizeL") {
      this.sel = {
        from: clamp(Math.min(ns, start.until - min), this.domain.from, start.until - min),
        until: start.until,
      };
    } else if (this.drag.mode === "resizeR") {
      this.sel = {
        from: start.from,
        until: clamp(Math.max(ns, start.from + min), start.from + min, this.domain.until),
      };
    } else {
      // create: bracket between the anchor and the pointer
      let from = Math.min(this.drag.anchorNs, ns);
      let until = Math.max(this.drag.anchorNs, ns);
      if (until - from < min) {
        const mid = (from + until) / 2;
        from = mid - min / 2;
        until = mid + min / 2;
      }
      this.sel = {
        from: clamp(from, this.domain.from, this.domain.until - min),
        until: clamp(until, this.domain.from + min, this.domain.until),
      };
    }
    this.draw();
  }

  onPointerUp(event) {
    if (!this.drag) return;
    const drag = this.drag;
    this.drag = null;
    delete this.canvas.dataset.drag;
    this.canvas.style.cursor = "";
    // A plain click inside a narrowed window jumps, keeping the span; a click
    // while the window covers everything has nowhere to go, so it is ignored.
    if (drag.mode === "create" && !drag.moved && !this.coversDomain()) {
      const rect = this.canvas.getBoundingClientRect();
      const px = event.clientX - rect.left;
      const ns = clamp(this.nsFor(px), this.domain.from, this.domain.until);
      const span = drag.startSel.until - drag.startSel.from;
      let from = ns - span / 2;
      from = clamp(from, this.domain.from, this.domain.until - span);
      this.sel = { from, until: from + span };
    }
    this.draw();
    if (this.sel) this.onChange(Math.round(this.sel.from), Math.round(this.sel.until));
  }

  // ---- painting ---------------------------------------------------------

  draw() {
    const width = this.plotWidth();
    const dpr = window.devicePixelRatio || 1;
    if (this.canvas.width !== Math.round(width * dpr)) {
      this.canvas.width = Math.round(width * dpr);
      this.canvas.style.width = `${width}px`;
    }
    this.canvas.height = Math.round(HEIGHT * dpr);
    this.canvas.style.height = `${HEIGHT}px`;
    const ctx = this.ctx;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, HEIGHT);

    if (this.domain) {
      this.domainEl.textContent = `of ${formatClock(this.domain.from)}–${formatClock(
        this.domain.until,
      )}`;
    }
    if (this.sel) {
      this.canvas.dataset.from = String(this.sel.from);
      this.canvas.dataset.until = String(this.sel.until);
      this.rangeEl.textContent = `${formatClock(this.sel.from)} → ${formatClock(
        this.sel.until,
      )} · ${formatDuration(this.sel.until - this.sel.from)}`;
    } else {
      this.rangeEl.textContent = "–";
    }
    if (!this.domain) return;

    const { left, right } = this.sel ? this.edgePx() : { left: -1, right: -1 };
    const barBottom = BAR_BOTTOM;
    const barTop = BAR_TOP;
    const barHeight = barBottom - barTop;

    // Histogram bars: muted outside the window, accent inside.
    const hist = this.histogram;
    let max = 0;
    if (hist) {
      for (const total of hist.totals) if (total > max) max = total;
    }
    if (hist && max > 0) {
      const bars = hist.totals.length;
      const barW = Math.max(1, width / bars - 0.5);
      for (let i = 0; i < bars; i += 1) {
        const value = hist.totals[i];
        if (value <= 0) continue;
        const x = (i / bars) * width;
        const h = Math.max(1, (value / max) * barHeight);
        const inWindow = this.sel && x + barW > left && x < right;
        ctx.fillStyle = inWindow ? "rgba(242,166,90,0.75)" : "rgba(107,167,247,0.30)";
        ctx.fillRect(x, barBottom - h, barW, h);
      }
    } else {
      ctx.fillStyle = "#6b7585";
      ctx.font = "11px system-ui, sans-serif";
      ctx.textAlign = "center";
      ctx.fillText("no samples in this range", width / 2, barBottom - barHeight / 2);
      ctx.textAlign = "left";
    }

    if (!this.sel) return;

    // Window: soft fill, hard edges, handles on the edges.
    ctx.fillStyle = "rgba(242,166,90,0.10)";
    ctx.fillRect(left, barTop - 4, right - left, barBottom - barTop + 8);
    ctx.strokeStyle = "rgba(242,166,90,0.85)";
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    ctx.moveTo(left + 0.5, barTop - 4);
    ctx.lineTo(left + 0.5, barBottom + 4);
    ctx.moveTo(right - 0.5, barTop - 4);
    ctx.lineTo(right - 0.5, barBottom + 4);
    ctx.stroke();
    for (const x of [left, right]) {
      ctx.fillStyle = "rgba(242,166,90,0.9)";
      const handleW = 6;
      roundRect(ctx, x - handleW / 2, barBottom - 10, handleW, 20, 3);
      ctx.fill();
      ctx.fillStyle = "#0e1014";
      ctx.fillRect(x - 0.5, barBottom - 5, 1, 10);
    }

    // Axis: domain ends plus a mid tick, so the window has landmarks.
    ctx.fillStyle = "#6b7585";
    ctx.font = "10px system-ui, sans-serif";
    ctx.textAlign = "left";
    ctx.fillText(formatClock(this.domain.from), 2, HEIGHT - 6);
    ctx.textAlign = "right";
    ctx.fillText(formatClock(this.domain.until), width - 2, HEIGHT - 6);
    ctx.textAlign = "center";
    ctx.fillText(formatClock((this.domain.from + this.domain.until) / 2), width / 2, HEIGHT - 6);
    ctx.textAlign = "left";
  }
}

function clamp(v, lo, hi) {
  return Math.min(hi, Math.max(lo, v));
}

function roundRect(ctx, x, y, w, h, r) {
  ctx.beginPath();
  ctx.moveTo(x + r, y);
  ctx.arcTo(x + w, y, x + w, y + h, r);
  ctx.arcTo(x + w, y + h, x, y + h, r);
  ctx.arcTo(x, y + h, x, y, r);
  ctx.arcTo(x, y, x + w, y, r);
  ctx.closePath();
}

function escape(text) {
  return String(text).replace(/[&<>"]/g, (c) =>
    c === "&" ? "&amp;" : c === "<" ? "&lt;" : c === ">" ? "&gt;" : "&quot;",
  );
}
