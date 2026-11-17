// Geometry for the service map.
//
// The map is drawn as DOM: a card per node, an SVG path per edge, both inside
// one transformed layer so pan and zoom are a single CSS transform. That is a
// different trade from the canvas the map used to be — a few dozen cards are
// cheap to lay out and can hold real components (a shadcn `Card`, a badge, a
// status glyph), which a canvas cannot without re-implementing text layout.
//
// Everything here works in world pixels at scale 1: a card is exactly as tall
// as `CARD.height`, and the view scales the whole layer instead of each part.
// Sizes therefore have to be decided before layout, not by the renderer:
// columns are as wide as the widest card in them, which is what keeps a column
// reading as a stack instead of a ragged edge.

import { layeredLayout } from "./servicemap-layout";
import { formatCount, formatDuration } from "./format";
import type { LabelFilter, ServiceMapEdge, ServiceMapNode } from "./types";

/** Card geometry, in pixels. */
export const CARD = {
  height: 46,
  minWidth: 138,
  maxWidth: 214,
  /** Space taken by the kind bar, the padding and the status glyph. */
  chrome: 40,
  /** Horizontal room between two columns, for the edge label to sit in. */
  gapX: 104,
  /** Vertical room between two cards. */
  gapY: 14,
};

export interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface Point {
  x: number;
  y: number;
}

/** What a card shows as its title.
 *
 * A workload is titled with its own name, not with an app label: the label is
 * what a chart chose to call the whole release, and two Deployments of one
 * release would read as the same thing. The namespace belongs on the second
 * line, where it qualifies the name instead of replacing it.
 */
export function nodeLabel(node: ServiceMapNode | undefined): string {
  if (!node) return "—";
  if (node.kind === "external") return "external";
  if (node.kind === "workload") return node.workload || node.service || node.id;
  return node.service || node.workload || node.id;
}

/** Fraction of the replicas that are ready, or null when the node has none. */
export function readyRatio(node: ServiceMapNode): number | null {
  const pods = node.pods ?? 0;
  if (!pods) return null;
  return Math.min(node.ready ?? 0, pods) / pods;
}

export type HealthTone = "ok" | "warn" | "muted";

export function healthTone(node: ServiceMapNode): HealthTone {
  const ratio = readyRatio(node);
  if (ratio === null) return "muted";
  return ratio >= 1 ? "ok" : "warn";
}

/** Second line of a card: where the node lives, and how much of it is up. */
export function nodeMeta(node: ServiceMapNode): string {
  if (node.kind === "external") return "outside the cluster";
  if (node.kind === "service") return node.namespace || "—";
  const pods = node.pods ?? 0;
  const where = node.namespace || "—";
  if (!pods) return where;
  return `${where} · ${Math.min(node.ready ?? 0, pods)}/${pods}`;
}

/** Colours of a node by kind. The left bar and the card tint read from these.
 *
 * The tint is a mix of the card surface and the bar colour at ~8%: enough to
 * group the kinds at a glance, faint enough that the text keeps its contrast
 * and the CPU heat bar stays the loudest thing on the card. */
export const KIND = {
  workload: { bar: "#6ba7f7", tint: "#19222e" },
  service: { bar: "#b48ef2", tint: "#1e1c2c" },
  external: { bar: "#6b7585", tint: "#1a1d23" },
} as const;

const ADVANCE_NAME = 6.3;

/** Width a card needs to show its two lines without clipping. */
function cardWidth(node: ServiceMapNode): number {
  const name = nodeLabel(node).length * ADVANCE_NAME;
  const meta = nodeMeta(node).length * 5.2;
  const needed = Math.max(name, meta) + CARD.chrome;
  return Math.round(Math.min(CARD.maxWidth, Math.max(CARD.minWidth, needed)));
}

/**
 * Place every node, keyed by id, as a top-left box in world pixels.
 *
 * Cards in one column share a width, so the column reads as a list; the width
 * is decided after the layout because the layout has to know how many columns
 * there are before it can pick a gap.
 */
export function placeNodes(
  nodes: ServiceMapNode[],
  edges: ServiceMapEdge[],
  viewport: { width: number; height: number },
): Map<string, Box> {
  const widths = new Map(nodes.map((node) => [node.id, cardWidth(node)]));
  const widest = Math.max(CARD.minWidth, ...nodes.map((node) => widths.get(node.id) ?? 0));

  const positions = layeredLayout(
    nodes.map((node) => ({ id: node.id, connections: node.connections ?? 0 })),
    edges.map((edge) => ({ client: edge.client, server: edge.server })),
    {
      width: Math.max(520, viewport.width - 120),
      height: Math.max(320, viewport.height - 100),
      minColumnGap: widest + CARD.gapX,
      minRowGap: CARD.height + CARD.gapY,
      maxRowGap: CARD.height + CARD.gapY + 34,
    },
  );

  // Column membership is the shared x the layout assigned. Making every card
  // in it as wide as its widest member keeps the edges straight and the
  // labels aligned.
  const widestPerColumn = new Map<number, number>();
  for (const node of nodes) {
    const point = positions.get(node.id);
    if (!point) continue;
    const width = widths.get(node.id) ?? CARD.minWidth;
    widestPerColumn.set(point.x, Math.max(widestPerColumn.get(point.x) ?? 0, width));
  }

  const boxes = new Map<string, Box>();
  for (const node of nodes) {
    const point = positions.get(node.id);
    if (!point) continue;
    const w = widestPerColumn.get(point.x) ?? CARD.minWidth;
    boxes.set(node.id, {
      x: Math.round(point.x - w / 2),
      y: Math.round(point.y - CARD.height / 2),
      w,
      h: CARD.height,
    });
  }

  // The layout puts the centre of the first column at x = 0, so the leftmost
  // cards start at a negative coordinate and would be clipped by the canvas.
  // Shifting the whole drawing into positive space costs one pass and removes
  // the class of "why is the first column cut in half" bugs.
  let minX = Number.POSITIVE_INFINITY;
  let minY = Number.POSITIVE_INFINITY;
  for (const box of boxes.values()) {
    minX = Math.min(minX, box.x);
    minY = Math.min(minY, box.y);
  }
  if (Number.isFinite(minX)) {
    const dx = 32 - minX;
    const dy = 24 - minY;
    for (const [id, box] of boxes) {
      boxes.set(id, { ...box, x: box.x + dx, y: box.y + dy });
    }
  }
  return boxes;
}

/** Bounding box of the drawing, with room for the edge labels outside it. */
export function worldBounds(boxes: Iterable<Box>, pad = 56): { width: number; height: number } {
  let maxX = 0;
  let maxY = 0;
  for (const box of boxes) {
    maxX = Math.max(maxX, box.x + box.w);
    maxY = Math.max(maxY, box.y + box.h);
  }
  return { width: maxX + pad, height: maxY + pad };
}

export interface EdgePath {
  d: string;
  /** Where the label goes: the midpoint of the drawn curve. */
  mid: Point;
  /** Control points, so a caller can evaluate the curve at any parameter. */
  p0: Point;
  c1: Point;
  c2: Point;
  p3: Point;
}

/** A point on the cubic at `t` in [0, 1]. */
export function pointOnEdge(path: EdgePath, t: number): Point {
  const u = 1 - t;
  const a = u * u * u;
  const b = 3 * u * u * t;
  const c = 3 * u * t * t;
  const d = t * t * t;
  return {
    x: a * path.p0.x + b * path.c1.x + c * path.c2.x + d * path.p3.x,
    y: a * path.p0.y + b * path.c1.y + c * path.c2.y + d * path.p3.y,
  };
}

/**
 * A cubic curve from `a` to `b`, anchored on the side that faces the peer.
 *
 * Forward edges (left column calls right column) leave the right edge and
 * arrive on the left edge; the curve is nearly straight, which is what makes a
 * layered graph readable. A back edge — the target sits above or left of the
 * source, which a retry or a callback produces — leaves the bottom and arrives
 * at the top, bowing sideways so it does not cut through either card.
 */
export function edgePath(a: Box, b: Box): EdgePath {
  const ay = a.y + a.h / 2;
  const by = b.y + b.h / 2;
  const forward = b.x >= a.x + a.w * 0.5;

  if (forward) {
    const x1 = a.x + a.w;
    // The arrowhead would otherwise touch the card it points at, so the curve
    // stops three pixels short and the marker fills the gap.
    const x2 = b.x - 3;
    const pull = Math.max(28, (x2 - x1) * 0.5);
    return {
      d: `M${x1},${ay} C${x1 + pull},${ay} ${x2 - pull},${by} ${x2},${by}`,
      mid: { x: (x1 + x2) / 2, y: (ay + by) / 2 },
      p0: { x: x1, y: ay },
      c1: { x: x1 + pull, y: ay },
      c2: { x: x2 - pull, y: by },
      p3: { x: x2, y: by },
    };
  }

  const down = by >= ay;
  const y1 = down ? a.y + a.h : a.y;
  const y2 = down ? b.y - 3 : b.y + b.h + 3;
  const x1 = a.x + a.w / 2;
  const x2 = b.x + b.w / 2;
  // Bow away from the cards, not into them: the loop runs on the right, where
  // the next column is, and its depth grows with the distance it has to cover.
  const bow = Math.max(46, Math.min(140, Math.abs(y2 - y1) * 0.45 + 46));
  const cx = Math.max(x1, x2) + bow;
  return {
    d: `M${x1},${y1} C${cx},${y1} ${cx},${y2} ${x2},${y2}`,
    mid: {
      x: (x1 + 3 * cx + 3 * cx + x2) / 8,
      y: (y1 + 3 * y1 + 3 * y2 + y2) / 8,
    },
    p0: { x: x1, y: y1 },
    c1: { x: cx, y: y1 },
    c2: { x: cx, y: y2 },
    p3: { x: x2, y: y2 },
  };
}

/** New connections per second, the rate the edges are labelled with.
 *
 * The agent counts *new* TCP connections, not requests: without parsing
 * payloads there is no request count to divide. It is the same number the map
 * has always shown as "connections", expressed per second so two different
 * time windows stay comparable.
 */
export function edgeRate(edge: ServiceMapEdge, windowSec: number): number {
  if (!(windowSec > 0)) return 0;
  return (edge.connections ?? 0) / windowSec;
}

export function formatRate(rate: number): string {
  if (!Number.isFinite(rate) || rate <= 0) return "0/s";
  if (rate >= 100) return `${formatCount(Math.round(rate))}/s`;
  if (rate >= 10) return `${rate.toFixed(1)}/s`;
  if (rate >= 0.01) return `${rate.toFixed(2)}/s`;
  // A quiet hour is still a number: 12 connections over 3600s is 0.003/s, and
  // rounding it to 0.00 would make every idle edge look identical to a dead one.
  const precise = rate.toFixed(3);
  return precise === "0.000" ? "<0.001/s" : `${precise}/s`;
}

/** Microseconds in, human duration out. */
export function formatRtt(rttUs: number | undefined): string {
  if (!rttUs) return "—";
  return formatDuration(rttUs * 1000);
}

/** How heavy an edge is, for ordering and stroke width. */
export function edgeWeight(edge: ServiceMapEdge): number {
  return (edge.connections ?? 0) + (edge.tx_bytes ?? 0) / 4096;
}

/**
 * The label matchers a node's profiles carry.
 *
 * A workload's series are labelled `k8s_namespace` + `k8s_workload`; a
 * ClusterIP's are labelled `service_name`. This mirrors how the agent builds a
 * node id, so the two views agree on what "this service" means.
 */
export function profileFilters(node: ServiceMapNode): LabelFilter[] | null {
  if (node.kind === "external") return null;
  if (node.kind === "service") {
    return node.service ? [{ label: "service_name", op: "=", value: node.service }] : null;
  }
  const name = node.workload || node.service;
  if (!name) return null;
  const filters: LabelFilter[] = [];
  if (node.namespace) filters.push({ label: "k8s_namespace", op: "=", value: node.namespace });
  filters.push({ label: "k8s_workload", op: "=", value: name });
  return filters;
}
