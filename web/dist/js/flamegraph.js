// A canvas flamegraph.
//
// The API returns the flat `names` / `levels` / `values` triple, which is what
// makes this fast: the whole graph is laid out into typed arrays once per data
// load, then every zoom, pan or hover is a binary search plus a single draw
// pass over the frames that are actually on screen. DOM-based flamegraphs fall
// over somewhere around 20k nodes; this one stays smooth at the 200k nodes the
// query engine is willing to hand out.

import { formatPercent, formatValue } from "./format.js";

const ROW_HEIGHT = 22;
const FONT = '11px ui-monospace, "SF Mono", Menlo, Consolas, monospace';
const MIN_FRAME_PX = 0.4;
const MIN_LABEL_PX = 32;

const HUES = [22, 38, 8, 46, 190, 210, 265, 320, 160, 350];

function hashString(text) {
  let h = 0x811c9dc5;
  for (let i = 0; i < text.length; i += 1) {
    h ^= text.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

function frameColor(name, depth) {
  const hue = HUES[hashString(name) % HUES.length] + (depth % 3) * 1.5;
  const light = 34 + ((hashString(name) >> 8) % 14);
  return `hsl(${hue} 58% ${light}%)`;
}

export class Flamegraph {
  constructor(host, { onSelect } = {}) {
    this.host = host;
    this.onSelect = onSelect ?? null;
    this.data = null;
    this.view = { x0: 0, x1: 1 };
    this.crumbs = [];
    this.search = "";
    this.tooltip = document.getElementById("tooltip");

    host.classList.add("panel");
    host.innerHTML = `
      <div class="fg-toolbar">
        <input type="search" placeholder="Search frames…" aria-label="Search frames" />
        <button class="ghost" data-action="reset">Reset zoom</button>
        <span class="spacer"></span>
        <span class="muted" data-role="match"></span>
      </div>
      <div class="fg-crumbs" data-role="crumbs"></div>
      <div class="fg-scroll"><canvas class="fg-canvas"></canvas></div>
      <div class="fg-legend" data-role="legend"></div>
    `;

    this.canvas = host.querySelector("canvas");
    this.ctx = this.canvas.getContext("2d");
    this.scroll = host.querySelector(".fg-scroll");
    this.crumbsEl = host.querySelector('[data-role="crumbs"]');
    this.matchEl = host.querySelector('[data-role="match"]');
    this.legendEl = host.querySelector('[data-role="legend"]');
    this.searchEl = host.querySelector('input[type="search"]');

    this.searchEl.addEventListener("input", () => {
      this.search = this.searchEl.value.trim().toLowerCase();
      this.computeMatches();
      this.draw();
    });
    host.querySelector('[data-action="reset"]').addEventListener("click", () => this.reset());
    this.crumbsEl.addEventListener("click", (event) => {
      const button = event.target.closest("button[data-index]");
      if (!button) return;
      this.focusCrumb(Number(button.dataset.index));
    });

    this.bindPointer();
    this.resizeObserver = new ResizeObserver(() => this.draw());
    this.resizeObserver.observe(this.scroll);
  }

  destroy() {
    this.resizeObserver?.disconnect();
    this.host.innerHTML = "";
  }

  // ---- data ------------------------------------------------------------

  /** Lay the flat arrays out into x / width / depth order. */
  setData(data) {
    this.data = data;
    this.view = { x0: 0, x1: 1 };
    this.crumbs = [];
    this.loadedAt = performance.now();

    // A diff payload carries two windows per node. The layout is driven by the
    // larger of the two so that both a hot spot that disappeared and one that
    // appeared keep a visible width; colour carries the delta.
    const isDiff = Array.isArray(data.diff) && Array.isArray(data.left);
    this.diff = isDiff ? { left: data.left, right: data.right, diff: data.diff } : null;
    const n = data.names.length;
    const rawValues = isDiff
      ? data.names.map((_, i) => Math.max(data.left[i] ?? 0, data.right[i] ?? 0))
      : data.values;
    const total = isDiff
      ? rawValues[0] || 1
      : data.total > 0
        ? data.total
        : 1;
    this.total = total;

    if (isDiff) {
      let maxAbs = 0;
      for (let i = 0; i < n; i += 1) {
        const d = Math.abs(data.diff[i] ?? 0);
        if (d > maxAbs) maxAbs = d;
      }
      this.maxAbsDiff = maxAbs || 1;
    } else {
      this.maxAbsDiff = 0;
    }

    this.n = n;
    const parent = Int32Array.from(data.levels);
    const value = Float64Array.from(rawValues);
    this.parent = parent;
    this.value = value;

    const childCount = new Int32Array(n);
    for (let i = 1; i < n; i += 1) {
      const p = parent[i];
      if (p >= 0 && p < n && p !== i) childCount[p] += 1;
      else parent[i] = -1;
    }
    const childStart = new Int32Array(n + 1);
    for (let i = 0; i < n; i += 1) childStart[i + 1] = childStart[i] + childCount[i];
    const fill = childStart.slice(0, n);
    const childList = new Int32Array(childStart[n]);
    const selfValue = Float64Array.from(value);
    for (let i = 1; i < n; i += 1) {
      const p = parent[i];
      if (p < 0) continue;
      childList[fill[p]] = i;
      fill[p] += 1;
      selfValue[p] -= value[i];
    }
    for (let i = 0; i < n; i += 1) if (selfValue[i] < 0) selfValue[i] = 0;

    // Biggest child first: the eye lands on what matters.
    for (let i = 0; i < n; i += 1) {
      const start = childStart[i];
      const end = childStart[i + 1];
      if (end - start > 1) {
        const slice = Array.from(childList.subarray(start, end));
        slice.sort((a, b) => value[b] - value[a]);
        childList.set(slice, start);
      }
    }

    const x = new Float64Array(n);
    const w = new Float64Array(n);
    const depth = new Int32Array(n).fill(-1);
    x[0] = 0;
    w[0] = 1;
    depth[0] = 0;

    const byDepth = n > 0 ? [[0]] : [];
    const cursor = new Float64Array(n);
    let frontier = n > 0 ? [0] : [];
    let level = 0;
    while (frontier.length) {
      const next = [];
      for (const p of frontier) {
        for (let k = childStart[p]; k < childStart[p + 1]; k += 1) {
          const c = childList[k];
          if (depth[c] !== -1) continue;
          depth[c] = level + 1;
          const childWidth = value[c] / total;
          x[c] = cursor[p];
          w[c] = childWidth;
          // Children of this node start where the node itself starts, so the
          // cursor has to follow the node rather than the parent.
          cursor[c] = x[c];
          cursor[p] += childWidth;
          next.push(c);
        }
      }
      if (!next.length) break;
      byDepth.push(next);
      frontier = next;
      level += 1;
    }

    this.childStart = childStart;
    this.childList = childList;
    this.selfValue = selfValue;
    this.x = x;
    this.w = w;
    this.depth = depth;
    this.byDepth = byDepth;
    this.colors = new Array(n);

    this.computeMatches();
    this.renderCrumbs();
    this.renderLegend();
    this.draw();
  }

  computeMatches() {
    const { n } = this;
    this.matches = null;
    if (!this.search || !n) {
      this.matchEl.textContent = "";
      return;
    }
    const set = new Uint8Array(n);
    let count = 0;
    for (let i = 0; i < n; i += 1) {
      if (this.data.names[i].toLowerCase().includes(this.search)) {
        set[i] = 1;
        count += 1;
      }
    }
    this.matches = set;
    this.matchEl.textContent = count
      ? `${count} matching frame${count === 1 ? "" : "s"}`
      : "no match";
  }

  // ---- geometry --------------------------------------------------------

  width() {
    return Math.max(320, this.scroll.clientWidth);
  }

  span() {
    return this.view.x1 - this.view.x0;
  }

  renderLegend() {
    const { data } = this;
    if (!data) {
      this.legendEl.textContent = "";
      return;
    }
    let parts;
    if (this.diff) {
      const delta = (data.total_right ?? 0) - (data.total_left ?? 0);
      // A zero baseline has no percentage: dividing by it would print "+0.0%"
      // for a window that did not exist, which reads as "no change at all".
      const ratio = data.total_left ? (delta / data.total_left) * 100 : null;
      const sign = delta > 0 ? "+" : delta < 0 ? "−" : "";
      const percent =
        ratio !== null ? ` (${sign}${Math.abs(ratio).toFixed(1)}%)` : " (new)";
      parts = [
        `<span class="legend-swatch diff-up"></span>baseline ${formatValue(data.total_left, data.unit)}`,
        `<span class="legend-swatch diff-down"></span>comparison ${formatValue(data.total_right, data.unit)}`,
        `delta ${sign}${formatValue(Math.abs(delta), data.unit)}${percent}`,
        `${data.names.length} frames`,
      ];
    } else {
      parts = [
        `total ${formatValue(data.total, data.unit)}`,
        `${data.samples ?? 0} samples`,
        `${data.names.length} frames`,
      ];
      if (data.truncated) parts.push("truncated — lower max_nodes? see docs");
    }
    this.legendEl.innerHTML = parts
      .map((text) => `<span>${text}</span>`)
      .join("");
  }

  renderCrumbs() {
    if (!this.crumbs.length) {
      this.crumbsEl.innerHTML = `<span class="faint">Click a frame to zoom, Esc to go back.</span>`;
      return;
    }
    const buttons = this.crumbs
      .map((index, i) => {
        const label = this.data.names[index];
        const short = label.length > 40 ? `${label.slice(0, 39)}…` : label;
        const arrow = i === 0 ? "" : `<span class="faint">›</span>`;
        return `<button data-index="${i}" title="${label.replace(/"/g, "&quot;")}">${arrow}${short}</button>`;
      })
      .join("");
    this.crumbsEl.innerHTML = buttons;
  }

  color(index) {
    let color = this.colors[index];
    if (!color) {
      if (this.diff) {
        const delta = this.diff.diff[index] ?? 0;
        if (delta === 0) {
          color = "hsl(220 8% 30%)";
        } else {
          const scale = Math.min(1, Math.abs(delta) / this.maxAbsDiff);
          const light = 24 + scale * 18;
          // Red means "more time here", green means "less": the same
          // convention every profiler's comparison view uses.
          const hue = delta > 0 ? 358 : 148;
          color = `hsl(${hue} 60% ${light}%)`;
        }
      } else {
        color = frameColor(this.data.names[index], this.depth[index]);
      }
      this.colors[index] = color;
    }
    return color;
  }

  // ---- drawing ---------------------------------------------------------

  draw() {
    const width = this.width();
    const rows = this.byDepth.length;
    const height = Math.max(ROW_HEIGHT, rows * ROW_HEIGHT);
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

    if (!this.n) {
      ctx.fillStyle = "#96a0b2";
      ctx.font = '13px system-ui, sans-serif';
      ctx.textAlign = "center";
      ctx.fillText("No samples in this range.", width / 2, 64);
      ctx.textAlign = "left";
      return;
    }

    const { x, w, byDepth } = this;
    const { x0, x1 } = this.view;
    const scale = width / (x1 - x0);
    const base = this.total > 0 ? this.total : 1;

    ctx.textBaseline = "middle";
    ctx.font = FONT;

    for (let d = 0; d < rows; d += 1) {
      const nodes = byDepth[d];
      const y = d * ROW_HEIGHT;

      // First frame whose right edge is still on screen.
      let lo = 0;
      let hi = nodes.length;
      while (lo < hi) {
        const mid = (lo + hi) >> 1;
        const i = nodes[mid];
        if (x[i] + w[i] < x0) lo = mid + 1;
        else hi = mid;
      }

      for (let j = lo; j < nodes.length; j += 1) {
        const i = nodes[j];
        if (x[i] > x1) break;
        const px = (x[i] - x0) * scale;
        const pw = w[i] * scale;
        if (pw < MIN_FRAME_PX) continue;

        const matched = this.matches && this.matches[i] === 1;
        ctx.fillStyle = matched ? "#f8e7c8" : this.color(i);
        ctx.fillRect(px, y + 1, Math.max(pw - 0.6, 0.6), ROW_HEIGHT - 2);

        if (matched) {
          ctx.strokeStyle = "#ffd489";
          ctx.lineWidth = 1;
          ctx.strokeRect(px + 0.5, y + 1.5, Math.max(pw - 1.6, 0.6), ROW_HEIGHT - 3);
        }

        if (pw >= MIN_LABEL_PX) {
          const name = this.data.names[i];
          const pct = w[i];
          ctx.save();
          ctx.beginPath();
          ctx.rect(px + 1, y + 1, pw - 2, ROW_HEIGHT - 2);
          ctx.clip();
          ctx.fillStyle = "rgba(12, 10, 8, 0.92)";
          const text = pw > 220 ? `${name}  ${formatPercent(pct, 0)}` : name;
          ctx.fillText(text, px + 5, y + ROW_HEIGHT / 2);
          ctx.restore();
        }
      }
    }

    // Hairline separators make deep stacks readable.
    ctx.fillStyle = "rgba(10, 12, 16, 0.55)";
    for (let d = 1; d < rows; d += 1) ctx.fillRect(0, d * ROW_HEIGHT - 1, width, 1);

    this.renderAxisNote(base);
  }

  renderAxisNote() {
    this.legendEl.dataset.zoom =
      this.view.x0 === 0 && this.view.x1 === 1
        ? ""
        : `${formatPercent(this.span(), 1)} of total shown`;
    const extra = this.legendEl.dataset.zoom;
    const existing = this.legendEl.querySelector("[data-role=zoom]");
    if (!extra) {
      existing?.remove();
      return;
    }
    if (existing) existing.textContent = extra;
    else {
      const span = document.createElement("span");
      span.dataset.role = "zoom";
      span.className = "faint";
      span.textContent = extra;
      this.legendEl.appendChild(span);
    }
  }

  // ---- interaction -----------------------------------------------------

  bindPointer() {
    const canvas = this.canvas;
    let dragging = false;
    let moved = 0;
    let lastX = 0;

    canvas.addEventListener("mousemove", (event) => {
      const rect = canvas.getBoundingClientRect();
      const cx = event.clientX - rect.left;
      const cy = event.clientY - rect.top;
      if (dragging) {
        const dx = event.clientX - lastX;
        lastX = event.clientX;
        moved += Math.abs(dx);
        this.pan(dx);
        return;
      }
      this.hover(cx, cy, event);
    });

    canvas.addEventListener("mousedown", (event) => {
      if (event.button !== 0) return;
      dragging = true;
      moved = 0;
      lastX = event.clientX;
      canvas.style.cursor = "grabbing";
    });

    window.addEventListener("mouseup", () => {
      if (!dragging) return;
      dragging = false;
      canvas.style.cursor = "pointer";
    });

    canvas.addEventListener("click", (event) => {
      if (moved > 4) return;
      const rect = canvas.getBoundingClientRect();
      const index = this.hitTest(event.clientX - rect.left, event.clientY - rect.top);
      if (index > 0) this.zoomTo(index);
    });

    canvas.addEventListener("dblclick", () => this.reset());

    canvas.addEventListener("mouseleave", () => {
      this.tooltip.hidden = true;
    });

    // Wheel events are passive by default in some browsers; preventDefault
    // keeps the page still while zooming.
    canvas.addEventListener(
      "wheel",
      (event) => {
        if (!this.n) return;
        event.preventDefault();
        const rect = canvas.getBoundingClientRect();
        const cx = Math.min(Math.max(event.clientX - rect.left, 0), rect.width);
        this.zoomAt(cx / rect.width, Math.exp(-event.deltaY * 0.0018));
      },
      { passive: false },
    );

    this.keyHandler = (event) => {
      if (event.target.matches("input, select, textarea")) return;
      if (event.key === "Escape") this.zoomOut();
      else if (event.key === "+" || event.key === "=") this.zoomAt(0.5, 1.5);
      else if (event.key === "-") this.zoomAt(0.5, 1 / 1.5);
      else if (event.key === "0") this.reset();
    };
    window.addEventListener("keydown", this.keyHandler);
  }

  hitTest(cx, cy) {
    if (!this.n) return -1;
    const depth = Math.floor(cy / ROW_HEIGHT);
    if (depth < 0 || depth >= this.byDepth.length) return -1;
    const nodes = this.byDepth[depth];
    const unit = this.view.x0 + (cx / this.width()) * this.span();
    let lo = 0;
    let hi = nodes.length - 1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      const i = nodes[mid];
      if (unit < this.x[i]) hi = mid - 1;
      else if (unit > this.x[i] + this.w[i]) lo = mid + 1;
      else return i;
    }
    return -1;
  }

  hover(cx, cy, event) {
    const index = this.hitTest(cx, cy);
    const tooltip = this.tooltip;
    if (index < 0) {
      tooltip.hidden = true;
      this.canvas.style.cursor = "default";
      return;
    }
    this.canvas.style.cursor = "pointer";
    const total = this.total || 1;
    const value = this.value[index];
    const self = this.selfValue[index];
    let detail;
    if (this.diff) {
      const left = this.diff.left[index] ?? 0;
      const right = this.diff.right[index] ?? 0;
      const delta = right - left;
      const ratio = left ? (delta / left) * 100 : null;
      const sign = delta > 0 ? "+" : delta < 0 ? "−" : "";
      detail = `
        <div class="tt-row"><span class="tt-key">baseline</span>${formatValue(left, this.data.unit)}</div>
        <div class="tt-row"><span class="tt-key">comparison</span>${formatValue(right, this.data.unit)}</div>
        <div class="tt-row"><span class="tt-key">delta</span>${sign}${formatValue(Math.abs(delta), this.data.unit)}${
          ratio === null ? "" : ` · ${sign}${Math.abs(ratio).toFixed(1)}%`
        }</div>
        <div class="tt-row"><span class="tt-key">share</span>${formatPercent(value / total)} of max window</div>
      `;
    } else {
      detail = `
        <div class="tt-row"><span class="tt-key">total</span>${formatValue(value, this.data.unit)} · ${formatPercent(value / total)}</div>
        <div class="tt-row"><span class="tt-key">self</span>${formatValue(self, this.data.unit)} · ${formatPercent(self / total)}</div>
        <div class="tt-row"><span class="tt-key">depth</span>${this.depth[index]}</div>
      `;
    }
    tooltip.innerHTML = `
      <div class="tt-title">${escape(this.data.names[index])}</div>
      ${detail}
    `;
    tooltip.hidden = false;
    const pad = 14;
    const rect = tooltip.getBoundingClientRect();
    let left = event.clientX + pad;
    let top = event.clientY + pad;
    if (left + rect.width > window.innerWidth - 8) left = event.clientX - rect.width - pad;
    if (top + rect.height > window.innerHeight - 8) top = event.clientY - rect.height - pad;
    tooltip.style.left = `${Math.max(8, left)}px`;
    tooltip.style.top = `${Math.max(8, top)}px`;
  }

  zoomTo(index) {
    this.crumbs.push(index);
    this.view = { x0: this.x[index], x1: this.x[index] + this.w[index] };
    this.renderCrumbs();
    this.draw();
    this.onSelect?.(this.data.names[index]);
  }

  focusCrumb(position) {
    this.crumbs = this.crumbs.slice(0, position + 1);
    const index = this.crumbs[position];
    this.view = { x0: this.x[index], x1: this.x[index] + this.w[index] };
    this.renderCrumbs();
    this.draw();
  }

  zoomOut() {
    if (this.crumbs.length <= 1) {
      this.reset();
      return;
    }
    this.focusCrumb(this.crumbs.length - 2);
  }

  reset() {
    this.crumbs = [];
    this.view = { x0: 0, x1: 1 };
    this.renderCrumbs();
    this.draw();
  }

  zoomAt(fraction, factor) {
    const span = this.span();
    const nextSpan = Math.min(1, Math.max(1e-6, span / factor));
    const anchor = this.view.x0 + fraction * span;
    let x0 = anchor - fraction * nextSpan;
    x0 = Math.min(Math.max(x0, 0), 1 - nextSpan);
    this.view = { x0, x1: x0 + nextSpan };
    this.draw();
  }

  pan(deltaPx) {
    const shift = (-deltaPx / this.width()) * this.span();
    let x0 = this.view.x0 + shift;
    x0 = Math.min(Math.max(x0, 0), 1 - this.span());
    this.view = { x0, x1: x0 + this.span() };
    this.draw();
  }
}

function escape(text) {
  return String(text).replace(/[&<>"]/g, (c) =>
    c === "&" ? "&amp;" : c === "<" ? "&lt;" : c === ">" ? "&gt;" : "&quot;",
  );
}
