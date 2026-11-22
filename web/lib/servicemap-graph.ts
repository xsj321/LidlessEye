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
import type { LabelFilter, NodeKind, ServiceMapEdge, ServiceMapNode } from "./types";

/** Card geometry, in pixels. */
export const CARD = {
  /** Three lines of text — name, namespace/replicas, traffic — plus the CPU
   *  hairline. 46 fitted two lines; a third needs the extra ten pixels, and
   *  every card shares the height so a column stays a stack. */
  height: 56,
  minWidth: 148,
  maxWidth: 232,
  /** Space taken by the kind bar, the padding and the status glyph. */
  chrome: 40,
  /** Horizontal room between two columns, for the edge label to sit in. */
  gapX: 104,
  /** Vertical room between two cards. */
  gapY: 16,
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
  // The outside world is one node per destination host: `external` itself is
  // the aggregate of everything whose host we could not read, and a host node
  // carries the host in `service`.
  if (node.kind === "external") return node.service || "external";
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

/**
 * Third line of a card: what the node is doing, in two numbers.
 *
 * Name, then identity, then activity: the first two lines answer "what is
 * this" and this one answers "is it busy". Both values are window-scoped, so
 * they read as *measurements of the range on screen* rather than as live
 * gauges — which is the only honest thing a payload fetched once can say.
 *
 * The rate is new connections per second, the same quantity the edge labels
 * carry, because without payload parsing there is no request count to divide.
 * On-CPU time is the total over the window, the same number the node panel's
 * CPU tab headlines.
 */
export function nodeTraffic(node: ServiceMapNode, windowSec: number): string {
  const parts: string[] = [];
  const connections = node.connections ?? 0;
  if (connections > 0 && windowSec > 0) parts.push(formatRate(connections / windowSec));
  if ((node.cpu_ns ?? 0) > 0) parts.push(formatDuration(node.cpu_ns ?? 0));
  return parts.join(" · ") || "—";
}

/** Order of the kinds in a column and in the legend, and the band order. */
export const KIND_ORDER: Record<string, number> = {
  workload: 0,
  service: 1,
  external: 2,
};

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
    nodes.map((node) => ({
      id: node.id,
      connections: node.connections ?? 0,
      group: KIND_ORDER[node.kind ?? "workload"] ?? 9,
    })),
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
  // the class of "why is the first column cut in half" bugs. The top margin
  // leaves room for a zone band's title, which is drawn above its first card.
  let minX = Number.POSITIVE_INFINITY;
  let minY = Number.POSITIVE_INFINITY;
  for (const box of boxes.values()) {
    minX = Math.min(minX, box.x);
    minY = Math.min(minY, box.y);
  }
  if (Number.isFinite(minX)) {
    const dx = 32 - minX;
    const dy = ZONE_PAD.top + 14 - minY;
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

/**
 * A background band around a run of columns that hold the same kind.
 *
 * Colour already distinguishes a workload from a ClusterIP on the card's kind
 * bar, but colour alone is a key you have to memorise. A band says it with
 * area and a title, which survives a screenshot and a projector. Bands are
 * derived from the layout rather than from a hand-written layout mode: the
 * columns are already ranked by dependency depth, so the kind that dominates
 * a column is the kind that region of the graph is about.
 *
 * Rectangles never overlap because each one covers a disjoint run of columns.
 */
export interface MapZone {
  kind: NodeKind;
  x: number;
  y: number;
  w: number;
  h: number;
  count: number;
  columns: number;
}

/** Inset of a band around the cards it contains, and the room for its title. */
const ZONE_PAD = { x: 14, top: 26, bottom: 16 };

export function mapZones(nodes: ServiceMapNode[], boxes: Map<string, Box>): MapZone[] {
  type Column = { x: number; kinds: Map<NodeKind, number>; count: number; boxes: Box[] };
  const columns = new Map<number, Column>();

  for (const node of nodes) {
    const box = boxes.get(node.id);
    if (!box) continue;
    const column: Column =
      columns.get(box.x) ?? { x: box.x, kinds: new Map<NodeKind, number>(), count: 0, boxes: [] };
    const kind = (node.kind ?? "workload") as NodeKind;
    column.kinds.set(kind, (column.kinds.get(kind) ?? 0) + 1);
    column.count += 1;
    column.boxes.push(box);
    columns.set(box.x, column);
  }

  const ordered = [...columns.values()].sort((a, b) => a.x - b.x);
  const dominant = (column: Column): NodeKind => {
    let best: NodeKind = "workload";
    let bestCount = -1;
    for (const [kind, count] of column.kinds) {
      const rank = KIND_ORDER[kind] ?? 9;
      const bestRank = KIND_ORDER[best] ?? 9;
      if (count > bestCount || (count === bestCount && rank < bestRank)) {
        best = kind;
        bestCount = count;
      }
    }
    return best;
  };

  const zones: MapZone[] = [];
  let run: { kind: NodeKind; columns: Column[] } | null = null;
  const flush = () => {
    if (!run) return;
    const { kind, columns: members } = run;
    let minX = Number.POSITIVE_INFINITY;
    let maxX = Number.NEGATIVE_INFINITY;
    let minY = Number.POSITIVE_INFINITY;
    let maxY = Number.NEGATIVE_INFINITY;
    let count = 0;
    for (const column of members) {
      count += column.count;
      for (const box of column.boxes) {
        minX = Math.min(minX, box.x);
        maxX = Math.max(maxX, box.x + box.w);
        minY = Math.min(minY, box.y);
        maxY = Math.max(maxY, box.y + box.h);
      }
    }
    // One column of three cards is a stack, not a region: a band around it
    // would just be a second border around the cards. Two adjacent columns, or
    // one that is tall enough to read as a region, earn a band.
    if (members.length >= 2 || count >= 6) {
      zones.push({
        kind,
        x: minX - ZONE_PAD.x,
        y: minY - ZONE_PAD.top,
        w: maxX - minX + ZONE_PAD.x * 2,
        h: maxY - minY + ZONE_PAD.top + ZONE_PAD.bottom,
        count,
        columns: members.length,
      });
    }
    run = null;
  };

  for (const column of ordered) {
    const kind = dominant(column);
    if (run && run.kind === kind) {
      run.columns.push(column);
    } else {
      flush();
      run = { kind, columns: [column] };
    }
  }
  flush();
  return zones;
}

/**
 * Nodes matching a search query, best first.
 *
 * The map is the only view with no address bar for a specific service: at
 * sixty nodes finding `payments` means reading sixty cards. Ranking prefers an
 * exact id or name over a prefix and a prefix over a substring, and breaks
 * ties by traffic so the busiest `payments` wins when several share a name.
 */
export function searchNodes(
  nodes: ServiceMapNode[],
  query: string,
  limit = 8,
): ServiceMapNode[] {
  const needle = query.trim().toLowerCase();
  if (!needle) return [];

  const scored: { node: ServiceMapNode; score: number }[] = [];
  for (const node of nodes) {
    const label = nodeLabel(node).toLowerCase();
    const id = node.id.toLowerCase();
    const namespace = (node.namespace ?? "").toLowerCase();
    let score = Number.POSITIVE_INFINITY;
    if (label === needle || id === needle) score = 0;
    else if (label.startsWith(needle)) score = 1;
    else if (id.startsWith(needle)) score = 2;
    else if (label.includes(needle)) score = 3;
    else if (id.includes(needle)) score = 4;
    else if (namespace.includes(needle)) score = 5;
    if (Number.isFinite(score)) scored.push({ node, score });
  }

  scored.sort((a, b) => {
    if (a.score !== b.score) return a.score - b.score;
    const traffic = (b.node.connections ?? 0) - (a.node.connections ?? 0);
    if (traffic !== 0) return traffic;
    return nodeLabel(a.node).localeCompare(nodeLabel(b.node));
  });
  return scored.slice(0, limit).map((entry) => entry.node);
}

/**
 * Ink and dash patterns for the four things one line can say.
 *
 * Exported because the legend beside the map has to draw the same ink the map
 * draws; a second copy of these hex values would drift the first time one of
 * them is tuned.
 *
 *   idle      an ordinary hop, between two nodes the reader is not looking at
 *   emphasis  a hop of the selected node, or of the card under the pointer
 *   warn      retransmits on this edge (long dash)
 *   external  the far end is outside the cluster (fine dot)
 */
export const EDGE_STYLE = {
  idle: "#55657c",
  emphasis: "#a9c8f2",
  warn: "#e05c5c",
  external: "#8d94a8",
  /** Label pill: a near-black plate, not a card surface, so a number reads
   *  over a curve without turning into a second card. */
  label: "#0e1014",
  labelBorder: "#232b36",
  labelText: "#a9b4c6",
} as const;

/** Dash patterns, as SVG `stroke-dasharray`. Two meanings, never one style. */
export const EDGE_DASH = { warn: "5 4", external: "1.5 3" } as const;

/** Stroke width for an edge, in world pixels.
 *
 * Traffic drives it on a log scale — a hub edge with a thousand times the rate
 * of a health check must not be a thousand times thicker — and a emphasised
 * hop gets a little extra so it stays ahead of its neighbours.
 */
export function edgeWidth(rate: number, bytes: number, emphasis: boolean): number {
  const base = 0.9 + Math.min(1.7, Math.log10(1 + rate * 20 + bytes / 8192 + 1) * 0.85);
  return base + (emphasis ? 0.35 : 0);
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
export interface EdgeBow {
  side: "left" | "right";
  /** How far out the loop runs. The caller reserves the room. */
  depth?: number;
}

export function edgePath(a: Box, b: Box, bow?: EdgeBow): EdgePath {
  const ay = a.y + a.h / 2;
  const by = b.y + b.h / 2;
  const forward = b.x >= a.x + a.w * 0.5;

  if (forward) {
    const x1 = a.x + a.w;
    const gap = b.x - x1;
    const rise = by - ay;
    // A hop that climbs or falls much further than the column gap is not an
    // S-curve: forcing horizontal tangents at both ends turns 360 px of rise
    // into a near-vertical straight with two short stubs, which is what made
    // "curved" arrows look wrong. Such a hop leaves horizontally, sweeps, and
    // plugs into the card's bottom (or top) edge vertically instead.
    if (Math.abs(rise) > Math.max(48, gap * 2.2)) {
      const down = rise > 0;
      const x2 = b.x + b.w / 2;
      const y2 = down ? b.y - 1 : b.y + b.h + 1;
      const sweep = Math.max(40, Math.min(120, gap * 1.1));
      const hook = Math.max(26, Math.min(90, Math.abs(y2 - ay) * 0.45));
      const c1 = { x: x1 + sweep, y: ay };
      const c2 = { x: x2, y: down ? y2 - hook : y2 + hook };
      return {
        d: `M${x1},${ay} C${c1.x},${c1.y} ${c2.x},${c2.y} ${x2},${y2}`,
        mid: { x: (x1 + 3 * c1.x + 3 * c2.x + x2) / 8, y: (ay + 3 * c1.y + 3 * c2.y + y2) / 8 },
        p0: { x: x1, y: ay },
        c1,
        c2,
        p3: { x: x2, y: y2 },
      };
    }
    // The curve stops one pixel short of the card: the arrowhead is drawn from
    // its endpoint back along the tangent, so the tip lands on the border
    // rather than a head-length away from it.
    const x2 = b.x - 1;
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
  const y2 = down ? b.y - 1 : b.y + b.h + 1;
  const x1 = a.x + a.w / 2;
  const x2 = b.x + b.w / 2;
  // Bow away from the cards, not into them: by default the loop runs on the
  // right, where the next column is, and its depth grows with the distance it
  // has to cover. A caller `bow` sends it out the other way, which is what two
  // cards in the same column need — otherwise the arc lands in the channel
  // where the hops to the selected workload are drawn.
  const depth = bow?.depth ?? Math.max(46, Math.min(140, Math.abs(y2 - y1) * 0.45 + 46));
  const cx = bow?.side === "left" ? Math.min(x1, x2) - depth : Math.max(x1, x2) + depth;
  // The curve used to arrive flat, on a tangent parallel to the card's edge, so
  // the arrowhead pointed sideways along the border while the line visibly
  // dropped into it — a triangle that belonged to nothing. The second control
  // point therefore sits above (or below) the endpoint, which turns the last
  // stretch into a hook and makes the arrival direction vertical, into the
  // card.
  const hook = Math.max(26, Math.min(90, Math.abs(y2 - y1) * 0.6));
  const c2 = { x: x2, y: down ? y2 - hook : y2 + hook };
  return {
    d: `M${x1},${y1} C${cx},${y1} ${c2.x},${c2.y} ${x2},${y2}`,
    mid: {
      x: (x1 + 3 * cx + 3 * c2.x + x2) / 8,
      y: (y1 + 3 * y1 + 3 * c2.y + y2) / 8,
    },
    p0: { x: x1, y: y1 },
    c1: { x: cx, y: y1 },
    c2,
    p3: { x: x2, y: y2 },
  };
}

/**
 * One line per pair.
 *
 * The store keeps an edge per (client, server, protocol, container pair),
 * because that is what the panel's per-container rows need. The map is a
 * service graph: four edges between the same two services draw four identical
 * curves, so the top one hides the others, their labels land on top of each
 * other, and a dotted external hop under a solid one reads as a line that
 * stops in mid-air. Merging here — not in the payload — keeps the panel's
 * detail and gives the drawing one stroke per relationship.
 */
export function mergeParallelEdges(edges: ServiceMapEdge[]): ServiceMapEdge[] {
  const groups = new Map<string, ServiceMapEdge>();
  for (const edge of edges) {
    const key = `${edge.client}\u0000${edge.server}`;
    const current = groups.get(key);
    if (!current) {
      groups.set(key, { ...edge, client_container: "", server_container: "" });
      continue;
    }
    const before = current.connections;
    const w1 = before || 1;
    const w2 = edge.connections || 1;
    // Round-trip time is a mean, so merging means weighting by the
    // connections that carried it — an idle sidecar hop must not drag down a
    // busy app hop.
    current.rtt_us = Math.round(((current.rtt_us ?? 0) * w1 + (edge.rtt_us ?? 0) * w2) / (w1 + w2));
    current.connections = before + edge.connections;
    current.retransmits = (current.retransmits ?? 0) + (edge.retransmits ?? 0);
    current.tx_bytes = (current.tx_bytes ?? 0) + (edge.tx_bytes ?? 0);
    current.rx_bytes = (current.rx_bytes ?? 0) + (edge.rx_bytes ?? 0);
    // The busier protocol names the line; the panel still lists every one.
    if (edge.connections > before) current.protocol = edge.protocol;
    if (!current.host) current.host = edge.host;
    if (!current.sql) current.sql = edge.sql;
  }
  return [...groups.values()];
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

/** Trim trailing zeroes so a number reads as a measurement, not a float. */
function measurement(value: number, digits: number): string {
  const text = value.toFixed(digits);
  return text.includes(".") ? text.replace(/\.?0+$/, "") : text;
}

export function formatRate(rate: number): string {
  if (!Number.isFinite(rate) || rate <= 0) return "0/s";
  if (rate >= 100) return `${formatCount(Math.round(rate))}/s`;
  if (rate >= 10) return `${measurement(rate, 1)}/s`;
  if (rate >= 1) return `${measurement(rate, 1)}/s`;
  if (rate >= 0.01) return `${measurement(rate, 2)}/s`;
  // Below one per second a per-second rate turns into a row of leading zeroes.
  // Per minute keeps the same information at a scale a person can compare.
  const perMinute = rate * 60;
  if (perMinute >= 1) return `${measurement(perMinute, 1)}/min`;
  return `${measurement(perMinute, 2)}/min`;
}

/**
 * Microseconds in, a duration on the edge out.
 *
 * The generic formatter is happy to print `0.005s` when a unit two orders of
 * magnitude smaller says `5ms`. An edge label is measured in pixels, so it
 * picks the unit that leaves exactly one non-zero digit in the ones place —
 * never a leading `0.` and never a four-digit `1000ms` that should have
 * carried into `1s`.
 *
 * The subtlety is rounding: `999.6µs` rounds to `1000µs`, which is the same as
 * `1ms` but reads worse and loses the digit. So the value is rounded first and
 * carried into the next unit while it reaches 1000. Working in nanoseconds as
 * an integer keeps the boundaries exact — floating-point `99.9999` must not
 * decide the unit by itself.
 */
const RTT_UNITS = [
  { ns: 1, suffix: "ns" },
  { ns: 1_000, suffix: "µs" },
  { ns: 1_000_000, suffix: "ms" },
  { ns: 1_000_000_000, suffix: "s" },
] as const;

export function formatRtt(rttUs: number | undefined): string {
  if (rttUs == null || !Number.isFinite(rttUs) || rttUs <= 0) return "—";
  const ns = Math.round(rttUs * 1000);
  if (ns <= 0) return "—";

  // Pick the unit whose value sits in [1, 1000): the ones digit is then always
  // a real number, and the label never starts with `0.`.
  let index = 0;
  for (let i = RTT_UNITS.length - 1; i >= 0; i--) {
    if (ns >= RTT_UNITS[i].ns) {
      index = i;
      break;
    }
  }
  // Round to the precision that fits, then carry: `999.6µs -> 1000µs -> 1ms`.
  // The carry test uses the same digit count the label will print, or a value
  // printed with no decimals (`999.6`) would slip through as `1000µs`.
  // Re-evaluating against the original nanoseconds avoids drifting per carry.
  let value = ns / RTT_UNITS[index].ns;
  let digits = value >= 100 ? 0 : value >= 10 ? 1 : 2;
  while (index < RTT_UNITS.length - 1 && Number(value.toFixed(digits)) >= 1000) {
    index += 1;
    value = ns / RTT_UNITS[index].ns;
    digits = value >= 100 ? 0 : value >= 10 ? 1 : 2;
  }
  return `${measurement(value, digits)}${RTT_UNITS[index].suffix}`;
}

/** What the edge label calls the protocol, including a payload-based hit. */
export function edgeProtocol(edge: ServiceMapEdge): string {
  const name = (edge.protocol || "").trim();
  return name && name !== "tcp" ? name : "";
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
