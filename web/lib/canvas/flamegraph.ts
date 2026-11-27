// A canvas flamegraph.
//
// The API returns the flat `names` / `levels` / `values` triple, which is what
// makes this fast: the whole graph is laid out into typed arrays once per data
// load, then every zoom, pan or hover is a binary search plus a single draw
// pass over the frames that are actually on screen. DOM-based flamegraphs fall
// over somewhere around 20k nodes; this one stays smooth at the 200k nodes the
// query engine is willing to hand out.
//
// Motion is drawn, not CSS: the bars grow out of their parents' left edges on
// load, zooms interpolate their viewport, and hovering traces the ancestry of
// the frame under the cursor. Everything is skippable — under
// `prefers-reduced-motion: reduce` each effect renders its final state in one
// frame — and `canvas[data-motion]` is set while an animation is running so a
// test can wait for the graph to settle instead of racing it.
//
// The class owns painting and interaction only. Toolbars, breadcrumbs and the
// legend are React's, fed through the callbacks below; that is also what keeps
// `destroy()` honest — every listener it binds to `window` is removed here.

import { formatPercent, formatValue } from "@/lib/format";
import type { DiffPayload, FlamegraphPayload } from "@/lib/types";

const ROW_HEIGHT = 22;
const FONT = '11px ui-monospace, "SF Mono", Menlo, Consolas, monospace';
const MIN_FRAME_PX = 0.4;
const MIN_LABEL_PX = 32;

const INTRO_MS = 420;
const INTRO_STAGGER_MS = 45;
const INTRO_MAX_MS = 900;
const ZOOM_MS = 260;

// Frame hues stay in the cool half of the wheel. Warm hues are the diff
// palette (orange growth, green shrinkage) and the accent, so a normal frame
// in that range would read as a finding. Identity is still stable per function
// name, which is what makes the same frame recognisable across two views.
const HUES = [190, 200, 210, 220, 232, 245, 258, 270, 285, 300];

const easeOutCubic = (t: number) => 1 - (1 - t) ** 3;
const easeInOutCubic = (t: number) =>
  t < 0.5 ? 4 * t ** 3 : 1 - (-2 * t + 2) ** 3 / 2;
const lerp = (a: number, b: number, t: number) => a + (b - a) * t;

/** Frame names are symbol paths; `&` and `<` are legal in them. */
function escapeXml(text: string): string {
  return text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function hashString(text: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < text.length; i += 1) {
    h ^= text.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

function frameColor(name: string, depth: number): string {
  const hash = hashString(name);
  // Cool hues have to stay cool — a warm frame would read as a diff finding —
  // but 34–48 % lightness on a near-black canvas left the fills as smudges
  // barely separated from the grooves between rows. 48–58 % keeps the labels
  // legible while giving each frame an edge; the hue still derives from the
  // name, so the same function keeps the same colour across views.
  const hue = HUES[hash % HUES.length] + (depth % 3) * 1.5;
  const light = 48 + ((hash >> 8) % 11);
  return `hsl(${hue} 62% ${light}%)`;
}

export interface Crumb {
  index: number;
  label: string;
}

export interface FlameHover {
  index: number;
  title: string;
  rows: Array<{ key: string; value: string }>;
}

/**
 * What the details panel shows for the selected frame. Enough to answer "what
 * is this, how much of the profile is it, and where does it sit" without
 * hovering again, and copyable so it can be pasted into a ticket.
 */
export interface FlameNodeDetail {
  index: number;
  name: string;
  depth: number;
  /** Time attributed to the frame itself, and to it plus everything below. */
  selfValue: number;
  totalValue: number;
  /** Shares of the whole window, 0..1. */
  selfShare: number;
  totalShare: number;
  unit: string;
  /** Sample rows under this frame, and its share of the window's samples. */
  samples: number;
  sampleShare: number;
  /** Root … frame, for the call-stack list. */
  chain: Array<{ index: number; name: string }>;
}

export interface LegendEntry {
  text: string;
  swatch?: "diff-up" | "diff-down";
}

export interface FlamegraphCallbacks {
  onSelectNode?: (detail: FlameNodeDetail | null) => void;
  onCrumbs?: (crumbs: Crumb[]) => void;
  onMatch?: (text: string) => void;
  /** Zoom fraction of the whole graph currently visible, 0..1. */
  onZoom?: (fraction: number) => void;
  onHover?: (hover: FlameHover | null, clientX: number, clientY: number) => void;
  onLegend?: (entries: LegendEntry[]) => void;
  /** `C` was pressed with a frame selected or hovered; React owns the clipboard. */
  onCopyName?: (name: string) => void;
}

type Payload = FlamegraphPayload | DiffPayload;

interface DiffData {
  left: number[];
  right: number[];
  diff: number[];
}

export class Flamegraph {
  private readonly canvas: HTMLCanvasElement;
  private readonly scroll: HTMLElement;
  private readonly ctx: CanvasRenderingContext2D;
  private readonly callbacks: FlamegraphCallbacks;

  private data: Payload | null = null;
  private diff: DiffData | null = null;
  private view = { x0: 0, x1: 1 };
  private crumbs: number[] = [];
  private search = "";

  // Motion state. `intro` grows the bars in, `zoomAnim` interpolates the
  // viewport, `frame` is the single pending paint, and `hovered` is the frame
  // under the cursor plus the ancestry highlighted behind it.
  private intro: { start: number; stagger: number } | null = null;
  private zoomAnim: {
    from: { x0: number; x1: number };
    to: { x0: number; x1: number };
    start: number;
    duration: number;
  } | null = null;
  private frame: number | null = null;
  private hovered = -1;
  private selected = -1;
  private reducedMotion = false;

  private n = 0;
  private total = 1;
  private maxAbsDiff = 0;
  private parent = new Int32Array(0);
  private value = new Float64Array(0);
  private selfValue = new Float64Array(0);
  private childStart = new Int32Array(0);
  private childList = new Int32Array(0);
  private x = new Float64Array(0);
  private w = new Float64Array(0);
  private depth = new Int32Array(0);
  private byDepth: number[][] = [];
  private colors: string[] = [];
  private matches: Uint8Array | null = null;

  private readonly motionQuery: MediaQueryList | null;
  private readonly onMotionChange: (event: MediaQueryListEvent) => void;
  private readonly onWindowMouseUp: () => void;
  private readonly keyHandler: (event: KeyboardEvent) => void;

  constructor(
    canvas: HTMLCanvasElement,
    scroll: HTMLElement,
    callbacks: FlamegraphCallbacks = {},
  ) {
    this.canvas = canvas;
    this.scroll = scroll;
    this.callbacks = callbacks;
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("2D canvas context unavailable");
    this.ctx = ctx;

    this.motionQuery = window.matchMedia?.("(prefers-reduced-motion: reduce)") ?? null;
    this.reducedMotion = this.motionQuery?.matches ?? false;
    this.onMotionChange = (event: MediaQueryListEvent) => {
      this.reducedMotion = event.matches;
      if (this.reducedMotion) this.finishMotion();
    };
    this.motionQuery?.addEventListener?.("change", this.onMotionChange);

    // `mouseup` on the canvas misses the release outside it, so drag state has
    // to be observed at the window. Kept as a field so `destroy()` can unbind
    // it: this listener used to leak one closure (and one whole decoded graph)
    // per mount.
    this.onWindowMouseUp = () => {
      if (!this.dragging) return;
      this.dragging = false;
      this.canvas.style.cursor = "pointer";
    };

    this.keyHandler = (event: KeyboardEvent) => {
      if ((event.target as HTMLElement | null)?.matches?.("input, select, textarea")) return;
      if (event.key === "Escape") {
        if (this.selected !== -1) {
          this.clearSelection();
          return;
        }
        this.zoomOut();
      }
      else if (event.key === "+" || event.key === "=") this.zoomAt(0.5, 1.5);
      else if (event.key === "-") this.zoomAt(0.5, 1 / 1.5);
      else if (event.key === "0") this.reset();
      else if ((event.key === "c" || event.key === "C") && this.data) {
        // The selection wins over the cursor: it is the frame the details
        // panel is describing, and it survives the pointer leaving.
        const index = this.selected >= 0 ? this.selected : this.hovered;
        if (index >= 0) {
          event.preventDefault();
          this.callbacks.onCopyName?.(this.data.names[index]);
        }
      }
    };

    this.bindPointer();
    this.resizeObserver = new ResizeObserver(() => this.draw());
    this.resizeObserver.observe(this.scroll);
  }

  private resizeObserver: ResizeObserver | null;
  private dragging = false;

  destroy() {
    this.cancelFrame();
    this.resizeObserver?.disconnect();
    this.resizeObserver = null;
    window.removeEventListener("mouseup", this.onWindowMouseUp);
    window.removeEventListener("keydown", this.keyHandler);
    this.motionQuery?.removeEventListener?.("change", this.onMotionChange);
    this.callbacks.onHover?.(null, 0, 0);
  }

  // ---- motion ----------------------------------------------------------

  /** One pending paint at a time, however many callers ask for it. */
  private scheduleFrame() {
    if (this.frame !== null) return;
    this.frame = requestAnimationFrame(() => {
      this.frame = null;
      this.draw();
    });
  }

  private cancelFrame() {
    if (this.frame !== null) cancelAnimationFrame(this.frame);
    this.frame = null;
  }

  /** `canvas[data-motion]` tells a test when the graph is still moving. */
  private syncMotion() {
    const kind = this.zoomAnim ? "zoom" : this.intro ? "intro" : null;
    if (kind) this.canvas.dataset.motion = kind;
    else delete this.canvas.dataset.motion;
  }

  /** Jump every in-flight animation to its final state. */
  private finishMotion() {
    this.intro = null;
    this.zoomAnim = null;
    this.cancelFrame();
    this.syncMotion();
    this.draw();
  }

  private startIntro() {
    this.hovered = -1;
    const rows = this.byDepth.length;
    if (this.reducedMotion || !rows) {
      this.intro = null;
      this.syncMotion();
      return;
    }
    this.intro = {
      start: performance.now(),
      // A 60-row stack should not take three seconds to appear.
      stagger: Math.min(INTRO_STAGGER_MS, INTRO_MAX_MS / rows),
    };
    this.syncMotion();
  }

  /** 0..1 for row `depth`, eased and staggered behind the row above it. */
  private introProgress(depth: number, now: number): number {
    const intro = this.intro;
    if (!intro) return 1;
    const elapsed = now - intro.start - depth * intro.stagger;
    if (elapsed <= 0) return 0;
    return easeOutCubic(Math.min(1, elapsed / INTRO_MS));
  }

  /** Interpolate the viewport instead of snapping to it. */
  private animateView(target: { x0: number; x1: number }, duration = ZOOM_MS) {
    if (this.reducedMotion) {
      this.zoomAnim = null;
      this.view = { ...target };
      this.syncMotion();
      this.draw();
      return;
    }
    this.zoomAnim = {
      from: { ...this.view },
      to: { ...target },
      start: performance.now(),
      duration,
    };
    this.syncMotion();
    this.scheduleFrame();
  }

  /** A pan or a wheel zoom has to win over a running transition. */
  private cancelViewAnimation() {
    if (!this.zoomAnim) return;
    this.zoomAnim = null;
    this.syncMotion();
  }

  // ---- data ------------------------------------------------------------

  /** Lay the flat arrays out into x / width / depth order. */
  setData(data: Payload) {
    this.data = data;
    this.view = { x0: 0, x1: 1 };
    this.crumbs = [];
    if (this.selected !== -1) {
      this.selected = -1;
      this.callbacks.onSelectNode?.(null);
    }
    this.zoomAnim = null;

    // A diff payload carries two windows per node. The layout is driven by the
    // larger of the two so that both a hot spot that disappeared and one that
    // appeared keep a visible width; colour carries the delta.
    const isDiff = Array.isArray((data as DiffPayload).diff) && Array.isArray((data as DiffPayload).left);
    this.diff = isDiff
      ? {
          left: (data as DiffPayload).left,
          right: (data as DiffPayload).right,
          diff: (data as DiffPayload).diff,
        }
      : null;
    const n = data.names.length;
    const rawValues = isDiff
      ? data.names.map((_, i) => Math.max((data as DiffPayload).left[i] ?? 0, (data as DiffPayload).right[i] ?? 0))
      : ((data as FlamegraphPayload).values ?? []);
    const total = isDiff
      ? rawValues[0] || 1
      : ((data as FlamegraphPayload).total ?? 0) > 0
        ? ((data as FlamegraphPayload).total as number)
        : 1;
    this.total = total;

    if (isDiff) {
      let maxAbs = 0;
      for (let i = 0; i < n; i += 1) {
        const d = Math.abs((data as DiffPayload).diff[i] ?? 0);
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

    const byDepth: number[][] = n > 0 ? [[0]] : [];
    const cursor = new Float64Array(n);
    let frontier = n > 0 ? [0] : [];
    let level = 0;
    while (frontier.length) {
      const next: number[] = [];
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
    this.emitCrumbs();
    this.emitLegend();
    this.startIntro();
    this.draw();
  }

  setSearch(text: string) {
    this.search = text.trim().toLowerCase();
    this.computeMatches();
    this.draw();
  }

  private computeMatches() {
    const { n } = this;
    this.matches = null;
    if (!this.search || !n || !this.data) {
      this.callbacks.onMatch?.("");
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
    this.callbacks.onMatch?.(
      count ? `${count} matching frame${count === 1 ? "" : "s"}` : "no match",
    );
  }

  // ---- geometry --------------------------------------------------------

  private width() {
    return Math.max(320, this.scroll.clientWidth);
  }

  private span() {
    return this.view.x1 - this.view.x0;
  }

  private emitCrumbs() {
    if (!this.data) {
      this.callbacks.onCrumbs?.([]);
      return;
    }
    this.callbacks.onCrumbs?.(
      this.crumbs.map((index) => ({
        index,
        label: this.data!.names[index],
      })),
    );
  }

  private emitLegend() {
    const { data } = this;
    if (!data) {
      this.callbacks.onLegend?.([]);
      return;
    }
    const parts: LegendEntry[] = [];
    if (this.diff) {
      const left = (data as DiffPayload).total_left ?? 0;
      const right = (data as DiffPayload).total_right ?? 0;
      const delta = right - left;
      // A zero baseline has no percentage: dividing by it would print "+0.0%"
      // for a window that did not exist, which reads as "no change at all".
      const ratio = left ? (delta / left) * 100 : null;
      const sign = delta > 0 ? "+" : delta < 0 ? "−" : "";
      const percent = ratio !== null ? ` (${sign}${Math.abs(ratio).toFixed(1)}%)` : " (new)";
      parts.push({
        text: `baseline ${formatValue(left, data.unit)}`,
        swatch: "diff-up",
      });
      parts.push({
        text: `comparison ${formatValue(right, data.unit)}`,
        swatch: "diff-down",
      });
      parts.push({
        text: `delta ${sign}${formatValue(Math.abs(delta), data.unit)}${percent}`,
      });
      parts.push({ text: `${data.names.length} frames` });
    } else {
      const flame = data as FlamegraphPayload;
      parts.push({ text: `total ${formatValue(flame.total, data.unit)}` });
      parts.push({ text: `${flame.samples ?? 0} samples` });
      parts.push({ text: `${data.names.length} frames` });
      if (flame.truncated) parts.push({ text: "truncated — lower max_nodes? see docs" });
    }
    this.callbacks.onLegend?.(parts);
  }

  private color(index: number): string {
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
        color = frameColor(this.data!.names[index], this.depth[index]);
      }
      this.colors[index] = color;
    }
    return color;
  }

  /**
   * `text` cut to fit `room` px, ending in an ellipsis. A hard clip at the bar
   * edge reads as a rendering bug; an ellipsis reads as "there is more", and
   * the tooltip carries the full name either way.
   */
  private fitLabel(text: string, room: number): string {
    const ctx = this.ctx;
    if (ctx.measureText(text).width <= room) return text;
    let lo = 0;
    let hi = text.length;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (ctx.measureText(`${text.slice(0, mid)}…`).width <= room) lo = mid;
      else hi = mid - 1;
    }
    return lo > 0 ? `${text.slice(0, lo)}…` : "";
  }

  // ---- drawing ---------------------------------------------------------

  draw() {
    const width = this.width();
    const rows = this.byDepth.length;
    const height = Math.max(ROW_HEIGHT, rows * ROW_HEIGHT);
    const dpr = window.devicePixelRatio || 1;

    // Advance the viewport transition before anything reads `this.view`.
    const now = performance.now();
    if (this.zoomAnim) {
      const zoom = this.zoomAnim;
      const t = Math.min(1, (now - zoom.start) / zoom.duration);
      const eased = easeInOutCubic(t);
      this.view = {
        x0: lerp(zoom.from.x0, zoom.to.x0, eased),
        x1: lerp(zoom.from.x1, zoom.to.x1, eased),
      };
      if (t >= 1) {
        this.view = { ...zoom.to };
        this.zoomAnim = null;
        this.syncMotion();
      }
    }

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

    if (!this.n || !this.data) {
      // Nothing to animate towards: an empty graph must not stay "moving".
      this.intro = null;
      this.zoomAnim = null;
      this.syncMotion();
      ctx.globalAlpha = 1;
      ctx.fillStyle = "#96a0b2";
      ctx.font = "13px system-ui, sans-serif";
      ctx.textAlign = "center";
      ctx.fillText("No samples in this range.", width / 2, 64);
      ctx.textAlign = "left";
      return;
    }

    const { x, w, byDepth } = this;
    const { x0, x1 } = this.view;
    const scale = width / (x1 - x0);
    const base = this.total > 0 ? this.total : 1;
    const names = this.data.names;

    ctx.textBaseline = "middle";
    ctx.font = FONT;

    // The hovered frame and its ancestors: the chain is what the cursor is
    // actually inside, so highlighting all of it explains where the number
    // comes from without reading the tooltip.
    let chain: Set<number> | null = null;
    if (this.hovered >= 0 && this.hovered < this.n) {
      chain = new Set();
      for (let i = this.hovered; i >= 0 && chain.size <= rows; i = this.parent[i]) {
        chain.add(i);
        if (this.parent[i] < 0) break;
      }
    }

    for (let d = 0; d < rows; d += 1) {
      const nodes = byDepth[d];
      const y = d * ROW_HEIGHT;

      // Bars grow out of the left edge of the row above them, one row behind
      // the next, so the graph assembles top-down instead of blinking in.
      const grow = this.intro ? this.introProgress(d, now) : 1;
      if (grow <= 0) continue;
      ctx.globalAlpha = 0.35 + 0.65 * grow;

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
        const pw = w[i] * scale * grow;
        if (pw < MIN_FRAME_PX) continue;

        const matched = this.matches && this.matches[i] === 1;
        ctx.fillStyle = matched ? "#f8e7c8" : this.color(i);
        ctx.fillRect(px, y + 1, Math.max(pw - 0.6, 0.6), ROW_HEIGHT - 2);

        if (chain && chain.has(i)) {
          // The ancestry is the explanation for the number in the tooltip, so
          // it is lit as one continuous column rather than one bar at a time.
          ctx.fillStyle =
            i === this.hovered ? "rgba(255, 255, 255, 0.16)" : "rgba(255, 255, 255, 0.08)";
          ctx.fillRect(px, y + 1, Math.max(pw - 0.6, 0.6), ROW_HEIGHT - 2);
        }

        if (matched) {
          ctx.strokeStyle = "#ffd489";
          ctx.lineWidth = 1;
          ctx.strokeRect(px + 0.5, y + 1.5, Math.max(pw - 1.6, 0.6), ROW_HEIGHT - 3);
        }

        if (i === this.hovered) {
          ctx.strokeStyle = "rgba(242, 166, 90, 0.95)";
          ctx.lineWidth = 2;
          ctx.strokeRect(px + 1, y + 2, Math.max(pw - 2, 0.6), ROW_HEIGHT - 4);
        }

        if (i === this.selected) {
          // Selection outlives the pointer, so it is drawn on top of the
          // hover ring: a 2px frame in the accent colour plus a tint, which
          // reads on every band colour without adding a shadow or a radius.
          ctx.fillStyle = "rgba(255, 255, 255, 0.14)";
          ctx.fillRect(px, y + 1, Math.max(pw - 0.6, 0.6), ROW_HEIGHT - 2);
          ctx.strokeStyle = "rgba(242, 166, 90, 1)";
          ctx.lineWidth = 2;
          ctx.strokeRect(px + 1, y + 2, Math.max(pw - 2, 0.6), ROW_HEIGHT - 4);
        }

        if (grow >= 1 && pw >= MIN_LABEL_PX) {
          const name = names[i];
          const pct = w[i];
          ctx.save();
          ctx.beginPath();
          ctx.rect(px + 1, y + 1, pw - 2, ROW_HEIGHT - 2);
          ctx.clip();
          ctx.fillStyle = "rgba(12, 10, 8, 0.92)";
          // Fit the name first and add the percentage only when both fit: an
          // ellipsis in the middle of "checkoutValid…37%" hides which part was
          // cut. The name is the identity; the share is a bonus.
          const room = pw - 10;
          let label = this.fitLabel(name, room);
          if (pw > 220) {
            const withPct = `${name}  ${formatPercent(pct, 0)}`;
            if (ctx.measureText(withPct).width <= room) label = withPct;
          }
          if (label) ctx.fillText(label, px + 5, y + ROW_HEIGHT / 2);
          ctx.restore();
        }
      }
    }

    // Hairline separators make deep stacks readable.
    ctx.globalAlpha = 1;
    ctx.fillStyle = "rgba(10, 12, 16, 0.55)";
    for (let d = 1; d < rows; d += 1) ctx.fillRect(0, d * ROW_HEIGHT - 1, width, 1);

    this.emitZoom(base);

    // Keep painting while something is still moving, then let the canvas rest.
    if (this.zoomAnim) {
      this.scheduleFrame();
    } else if (this.intro) {
      if (this.introProgress(rows - 1, now) < 1) this.scheduleFrame();
      else {
        this.intro = null;
        this.syncMotion();
      }
    }
  }

  private emitZoom(_base: number) {
    const fraction = this.view.x0 === 0 && this.view.x1 === 1 ? 1 : this.span();
    this.callbacks.onZoom?.(fraction);
  }

  // ---- interaction -----------------------------------------------------

  private bindPointer() {
    const canvas = this.canvas;
    let moved = 0;
    let lastX = 0;

    canvas.addEventListener("mousemove", (event) => {
      const rect = canvas.getBoundingClientRect();
      const cx = event.clientX - rect.left;
      const cy = event.clientY - rect.top;
      if (this.dragging) {
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
      this.dragging = true;
      moved = 0;
      lastX = event.clientX;
      canvas.style.cursor = "grabbing";
      if (this.hovered !== -1) {
        this.hovered = -1;
        this.draw();
      }
    });

    window.addEventListener("mouseup", this.onWindowMouseUp);

    // Click selects and opens the details panel; double click zooms. Zooming
    // on single click made the details impossible to reach, and the panel
    // carries its own "Zoom in" action for anyone who expects it there.
    canvas.addEventListener("click", (event) => {
      if (moved > 4) return;
      const rect = canvas.getBoundingClientRect();
      const index = this.hitTest(event.clientX - rect.left, event.clientY - rect.top);
      if (index > 0) this.select(index);
      else this.clearSelection();
    });

    canvas.addEventListener("dblclick", (event) => {
      const rect = canvas.getBoundingClientRect();
      const index = this.hitTest(event.clientX - rect.left, event.clientY - rect.top);
      if (index > 0) this.zoomTo(index);
      else this.reset();
    });

    canvas.addEventListener("mouseleave", () => {
      this.callbacks.onHover?.(null, 0, 0);
      if (this.hovered !== -1) {
        this.hovered = -1;
        this.draw();
      }
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

    window.addEventListener("keydown", this.keyHandler);
  }

  private hitTest(cx: number, cy: number): number {
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

  private hover(cx: number, cy: number, event: MouseEvent) {
    const index = this.hitTest(cx, cy);
    if (index !== this.hovered) {
      this.hovered = index;
      this.draw();
    }
    if (index < 0 || !this.data) {
      this.canvas.style.cursor = "default";
      this.callbacks.onHover?.(null, event.clientX, event.clientY);
      return;
    }
    this.canvas.style.cursor = "pointer";
    this.callbacks.onHover?.(this.describe(index), event.clientX, event.clientY);
  }

  private describe(index: number): FlameHover {
    const data = this.data!;
    const total = this.total || 1;
    const value = this.value[index];
    const rows: Array<{ key: string; value: string }> = [];
    if (this.diff) {
      const left = this.diff.left[index] ?? 0;
      const right = this.diff.right[index] ?? 0;
      const delta = right - left;
      const ratio = left ? (delta / left) * 100 : null;
      const sign = delta > 0 ? "+" : delta < 0 ? "−" : "";
      rows.push({ key: "baseline", value: formatValue(left, data.unit) });
      rows.push({ key: "comparison", value: formatValue(right, data.unit) });
      rows.push({
        key: "delta",
        value: `${sign}${formatValue(Math.abs(delta), data.unit)}${
          ratio === null ? "" : ` · ${sign}${Math.abs(ratio).toFixed(1)}%`
        }`,
      });
      rows.push({
        key: "share",
        value: `${formatPercent(value / total)} of max window`,
      });
    } else {
      rows.push({
        key: "total",
        value: `${formatValue(value, data.unit)} · ${formatPercent(value / total)}`,
      });
      rows.push({
        key: "self",
        value: `${formatValue(this.selfValue[index], data.unit)} · ${formatPercent(
          this.selfValue[index] / total,
        )}`,
      });
      const samples = this.sampleCount(index);
      if (samples > 0) {
        rows.push({
          key: "samples",
          value: `${samples} · ${formatPercent(samples / (this.sampleTotal() || 1))}`,
        });
      }
      rows.push({ key: "depth", value: String(this.depth[index]) });
    }
    return { index, title: data.names[index], rows };
  }

  /** Sample rows under a frame; 0 for a payload that predates the field. */
  sampleCount(index: number): number {
    const flame = this.data as FlamegraphPayload | null;
    return flame?.sample_counts?.[index] ?? 0;
  }

  /** Every sample in the window, which is the root's own count. */
  private sampleTotal(): number {
    const flame = this.data as FlamegraphPayload | null;
    if (flame?.sample_counts?.length) return flame.sample_counts[0] ?? 0;
    return flame?.samples ?? 0;
  }

  /**
   * One step in: the selected frame if there is one (it is what the details
   * panel is about), otherwise the middle of the viewport.
   */
  zoomIn() {
    if (!this.data) return;
    if (this.selected >= 0) {
      this.zoomTo(this.selected);
      return;
    }
    this.zoomAt(0.5, 1.5);
  }

  zoomTo(index: number) {
    if (!this.data) return;
    this.crumbs.push(index);
    this.revealFrame(index);
    this.emitCrumbs();
  }

  /** Select a frame: the details panel follows, the viewport does not move. */
  select(index: number) {
    if (!this.data || index < 0) {
      this.clearSelection();
      return;
    }
    if (this.selected === index) return;
    this.selected = index;
    this.draw();
    this.callbacks.onSelectNode?.(this.detail(index));
  }

  clearSelection() {
    if (this.selected === -1) return;
    this.selected = -1;
    this.draw();
    this.callbacks.onSelectNode?.(null);
  }

  /**
   * Everything the panel needs about one frame. The chain is walked from the
   * frame up to the root and then reversed, because the call stack reads
   * outermost-first.
   */
  detail(index: number): FlameNodeDetail {
    const data = this.data!;
    const total = this.total || 1;
    const chain: Array<{ index: number; name: string }> = [];
    for (let at = index; at >= 0; at = this.parent[at]) {
      chain.push({ index: at, name: data.names[at] });
      if (this.parent[at] < 0) break;
    }
    chain.reverse();
    return {
      index,
      name: data.names[index],
      depth: this.depth[index],
      selfValue: this.selfValue[index],
      totalValue: this.value[index],
      selfShare: this.selfValue[index] / total,
      totalShare: this.value[index] / total,
      unit: data.unit,
      samples: this.sampleCount(index),
      sampleShare: this.sampleCount(index) / (this.sampleTotal() || 1),
      chain,
    };
  }

  /** Fly to a frame's extent instead of cutting to it. */
  private revealFrame(index: number) {
    this.hovered = -1;
    this.animateView({ x0: this.x[index], x1: this.x[index] + this.w[index] });
  }

  focusCrumb(position: number) {
    this.crumbs = this.crumbs.slice(0, position + 1);
    this.revealFrame(this.crumbs[position]);
    this.emitCrumbs();
  }

  /** One step out: up the breadcrumb, then the whole graph, then the viewport. */
  zoomOut() {
    if (this.crumbs.length > 1) {
      this.focusCrumb(this.crumbs.length - 2);
      return;
    }
    if (this.crumbs.length === 1) {
      this.reset();
      return;
    }
    this.zoomAt(0.5, 1 / 1.5);
  }

  reset() {
    this.crumbs = [];
    this.emitCrumbs();
    // Slightly slower than a zoom: pulling back to the whole graph reads as a
    // change of context, not a jump within one.
    this.hovered = -1;
    this.animateView({ x0: 0, x1: 1 }, 320);
  }

  private zoomAt(fraction: number, factor: number) {
    this.cancelViewAnimation();
    const span = this.span();
    const nextSpan = Math.min(1, Math.max(1e-6, span / factor));
    const anchor = this.view.x0 + fraction * span;
    let x0 = anchor - fraction * nextSpan;
    x0 = Math.min(Math.max(x0, 0), 1 - nextSpan);
    this.view = { x0, x1: x0 + nextSpan };
    this.draw();
  }

  // ---- export ----------------------------------------------------------

  /**
   * The visible graph as a standalone SVG.
   *
   * Drawn from the same layout arrays as the canvas, so what lands in the file
   * is what is on screen — the current zoom, not the whole profile. Labels are
   * fitted by character count because SVG has no text measuring here; a mono
   * font makes that a good approximation, and the full name is still in the
   * JSON export.
   */
  toSvg(title = ""): string {
    const data = this.data;
    if (!data) return "";
    const width = this.width();
    const rows = Math.max(1, this.byDepth.length);
    const height = rows * ROW_HEIGHT;
    const footer = 30;
    const { x0, x1 } = this.view;
    const scale = width / (x1 - x0);
    const out: string[] = [
      `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height + footer}" ` +
        `viewBox="0 0 ${width} ${height + footer}" ` +
        `font-family="ui-monospace, Menlo, Consolas, monospace">`,
      `<rect width="100%" height="100%" fill="#111318"/>`,
    ];
    for (let d = 0; d < rows; d += 1) {
      const y = d * ROW_HEIGHT;
      for (const i of this.byDepth[d] ?? []) {
        const px = (this.x[i] - x0) * scale;
        const pw = this.w[i] * scale;
        if (pw < MIN_FRAME_PX || px > width || px + pw < 0) continue;
        out.push(
          `<rect x="${px.toFixed(2)}" y="${y + 1}" width="${Math.max(pw - 0.6, 0.6).toFixed(2)}" ` +
            `height="${ROW_HEIGHT - 2}" fill="${this.color(i)}"/>`,
        );
        if (pw < MIN_LABEL_PX) continue;
        const name = data.names[i];
        const room = Math.max(1, Math.floor((pw - 10) / 6.2));
        let label = name.length > room ? `${name.slice(0, Math.max(0, room - 1))}…` : name;
        if (pw > 220) {
          const withPct = `${name}  ${formatPercent(this.w[i], 0)}`;
          if (withPct.length <= room) label = withPct;
        }
        if (!label) continue;
        out.push(
          `<text x="${(px + 5).toFixed(2)}" y="${(y + ROW_HEIGHT / 2).toFixed(2)}" ` +
            `fill="#0c0a08" font-size="11" dominant-baseline="middle">${escapeXml(label)}</text>`,
        );
      }
    }
    const meta = [
      title,
      `total ${formatValue(this.total, data.unit)}`,
      `${this.sampleTotal()} samples`,
      `${data.names.length} frames`,
      `${(this.span() * 100).toFixed(1)}% shown`,
    ]
      .filter(Boolean)
      .join(" · ");
    out.push(
      `<text x="8" y="${height + 19}" fill="#96a0b2" font-size="11">${escapeXml(meta)}</text>`,
    );
    out.push("</svg>");
    return out.join("\n");
  }

  /**
   * The payload plus where the reader is in it. This is what a bug report
   * needs: the graph can be replayed without the server.
   */
  toJson(): string {
    const data = this.data;
    if (!data) return "{}";
    return JSON.stringify(
      {
        view: this.view,
        crumbs: this.crumbs.map((index) => data.names[index]),
        selection: this.selected >= 0 ? data.names[this.selected] : null,
        payload: data,
      },
      null,
      2,
    );
  }

  private pan(deltaPx: number) {
    this.cancelViewAnimation();
    const shift = (-deltaPx / this.width()) * this.span();
    let x0 = this.view.x0 + shift;
    x0 = Math.min(Math.max(x0, 0), 1 - this.span());
    this.view = { x0, x1: x0 + this.span() };
    this.draw();
  }
}
