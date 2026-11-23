"use client";

// The edges of the map, drawn as one SVG.
//
// A path is cheap and scales with the transform without re-measuring, which is
// what makes it a better fit here than anything canvas-drawn: the cards are
// DOM, so the edges have to live in the same coordinate space as them, and an
// SVG nested in the transformed layer does exactly that with no scale maths.
//
// Every edge carries its own numbers — new connections per second and the mean
// round-trip time — because that is the question a map is asked ("is this hop
// slow, or is it busy?"), and a tooltip answers it only for one hop at a time.
//
// Four channels say what a line is, in the order a reader needs them:
//
//   direction   an arrowhead on the target end, so a call graph reads as a
//               flow rather than as "these two are connected";
//   weight      stroke width grows with traffic, so a main call chain is a
//               thick line and a health check is a hairline;
//   colour      emphasis blue for the selected node's hops, red for
//               retransmits, cool grey for a dependency outside the cluster;
//   opacity     everything unrelated to the selection drops back to a hint.
//
// Labels are placed, not just dropped at the midpoint: in a graph with a hub,
// twenty midpoints land within a few dozen pixels of each other and the text
// turns into overlapping noise. Each label is therefore tried at five points
// along its own curve and five offsets from each, and takes the first spot that
// hits neither a card nor another label. A label with nowhere to go is skipped
// rather than drawn on top of its neighbour — but hovering the line still
// reveals it, in full, above everything else.
//
// Paths are drawn before labels, in a separate pass: a label's whole reason to
// exist is being read, and an edge that happens to be drawn later would strike
// a line straight through the number.


import {
  EDGE_DASH,
  EDGE_STYLE,
  edgePath,
  edgeProtocol,
  edgeRate,
  formatEdgeRtt,
  formatRate,
  edgeWidth,
  pointOnEdge,
} from "@/lib/servicemap-graph";
import type { Box, EdgeBow, EdgePath } from "@/lib/servicemap-graph";
import type { ServiceMapEdge } from "@/lib/types";

export function edgeKey(edge: ServiceMapEdge): string {
  // Two containers of one pod can carry the same protocol to one peer. Without
  // their names both edges get the same React key and one of them disappears.
  return `${edge.client}->${edge.server}:${edge.protocol}:${edge.client_container ?? ""}:${edge.server_container ?? ""}:${edge.client_pod ?? ""}:${edge.server_pod ?? ""}`;
}

// Colours and dash patterns live in the shared module so the legend beside the
// map is drawing the same ink as the map, not a second copy that can drift.
const LINE = EDGE_STYLE;

// The label has to fit in the channel between two card columns, and that
// channel is what decides its size: 9px monospace with 10px of padding leaves
// the pill narrow enough to clear the cards on both sides, which is what lets
// every edge keep its own number instead of half of them being dropped.
const LABEL = { height: 15, font: 9, advance: 4.9, pad: 10 };

interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

function overlaps(a: Rect, b: Rect, gap = 2): boolean {
  const dx = Math.min(a.x + a.w, b.x + b.w) - Math.max(a.x, b.x);
  const dy = Math.min(a.y + a.h, b.y + b.h) - Math.max(a.y, b.y);
  return dx > -gap && dy > -gap;
}

/** First spot along the curve that is free of cards and other labels. */
function placeLabel(
  path: EdgePath,
  width: number,
  cards: Rect[],
  taken: Rect[],
): Rect | null {
  const stops = [0.5, 0.6, 0.4, 0.7, 0.3, 0.8, 0.2, 0.9, 0.1];
  const offsets = [0, -14, 14, -28, 28, -42, 42];
  for (const t of stops) {
    const point = pointOnEdge(path, t);
    for (const dy of offsets) {
      const rect: Rect = {
        x: point.x - width / 2,
        y: point.y + dy - LABEL.height / 2,
        w: width,
        h: LABEL.height,
      };
      // 4px of clearance, not 0: a card's shadow reaches outside its box, and a
      // label tucked under it reads as a clipped one. Eight was safe and
      // expensive — in a graph whose columns are 104px apart it left two pixels
      // of channel to spare, and half the edges lost their number.
      if (cards.some((card) => overlaps(rect, card, 4))) continue;
      if (taken.some((other) => overlaps(rect, other, 2))) continue;
      return rect;
    }
  }
  return null;
}

export interface EdgeLayerProps {
  edges: ServiceMapEdge[];
  boxes: Map<string, Box>;
  /** Window length in seconds: connections divided by it is the rate. */
  windowSec: number;
  /** Node whose incident edges should stand out, if any. */
  selected?: string | null;
  showLabels?: boolean;
  /**
   * Rectangles a label must not cover. Edges may end on a sub-row inside a
   * card, so the endpoint boxes and the things that can hide a label are not
   * necessarily the same set: the neighbourhood passes its row anchors as
   * `boxes` and its workload cards here.
   */
  avoid?: Box[];
  /**
   * Context, not the subject: thinner, dimmer curves. Used for the mesh
   * between neighbours, which says "these two talk" and would otherwise
   * compete with the hops to the selected workload.
   */
  muted?: boolean;
  /**
   * No arrowhead. A mesh curve stands for both directions of one
   * relationship, so pointing it one way would be a lie.
   */
  undirected?: boolean;
  /**
   * Which way a same-column curve bows. Two cards in one column need the arc
   * on the outside of that column; the default (right) would drop it into the
   * channel where the hops to the selected workload are drawn.
   */
  bow?: (edge: ServiceMapEdge) => EdgeBow | undefined;
  /**
   * Draw the curves above the cards instead of behind them. The neighbourhood
   * anchors a hop on a container row's edge, which sits a few pixels inside the
   * card's border: behind the card, that last stretch would be swallowed by the
   * card's fill and the line would look like it stopped at the card after all.
   */
  above?: boolean;
  /**
   * Kind of every node, so an edge to something outside the cluster can be
   * drawn differently. Absent in the neighbourhood, where every hop is internal
   * by construction.
   */
  kinds?: Map<string, string>;
  /** Edge under the pointer, keyed by `edgeKey`. */
  hoveredEdge?: string | null;
  /** Node under the pointer; its hops brighten even when no line is hovered. */
  hoveredNode?: string | null;
  onHoverEdge?: (key: string | null) => void;
  /**
   * World rectangle currently on screen. Labels are placed only for edges
   * whose midpoint falls inside it (plus a margin): a ten-thousand-pixel map
   * has two hundred labels, and the off-screen ones cost a text node each
   * while being invisible. Paths are always drawn — the shape is what makes
   * the map.
   */
  labelArea?: { x: number; y: number; w: number; h: number };
  width: number;
  height: number;
  testId?: string;
}

/**
 * Where the head goes, and where the line has to stop so it does.
 *
 * SVG markers can do this, but only with a `strokeWidth`-relative box, they
 * cannot inherit `stroke-opacity` (the fill stays opaque while the line fades,
 * which is what made the two halves look glued on), and they are placed at the
 * path's end rather than aimed at a card.
 *
 * This head is a **dart**: the back edge is notched so the line runs *into* the
 * shape instead of butting against a flat base. A plain triangle shows its base
 * as a straight cut across the stroke; a notch has no such edge, and the head
 * reads as the line tapering to a point.
 */
function arrowHead(path: EdgePath, width: number) {
  // Direction from the point one head-length back, not from the endpoint's
  // tangent. On a curve the two differ: the tangent is the direction at the
  // last pixel, while the reader's eye takes the direction of the stretch the
  // head covers. Aiming down the tangent made the head look rotated against
  // its own line wherever the curve was still bending.
  const back = cubicBack(path, Math.min(12, width * 4 + 2));
  const dx = path.p3.x - back.x;
  const dy = path.p3.y - back.y;
  const norm = Math.hypot(dx, dy) || 1;
  const ux = dx / norm;
  const uy = dy / norm;
  // Longer than it is wide, or it reads as a blob: about 2.2 : 1.
  const length = Math.min(10, width * 4 + 2);
  const half = width * 1.4;
  const notch = length * 0.35;
  const base = { x: path.p3.x - ux * length, y: path.p3.y - uy * length };
  const waist = { x: base.x + ux * notch, y: base.y + uy * notch };
  const nx = -uy;
  const ny = ux;
  return {
    // The line ends at the notch, inside the dart, and the last control point
    // moves back with it so the arrival angle survives the trim.
    d: `M${path.p0.x},${path.p0.y} C${path.c1.x},${path.c1.y} ${path.c2.x - ux * (length - notch) * 0.5},${path.c2.y - uy * (length - notch) * 0.5} ${waist.x + ux * 0.5},${waist.y + uy * 0.5}`,
    // From behind the notch to inside the head: a solid joint the dashes of an
    // external or retransmitting edge cannot break.
    // Long enough to span a dash gap (4 px on both dashed styles) no matter
    // where the phase lands, so the head is never orphaned.
    neck: `M${waist.x - ux * Math.max(12, length)},${waist.y - uy * Math.max(12, length)} L${waist.x + ux},${waist.y + uy}`,
    head: `M${path.p3.x},${path.p3.y} L${base.x + nx * half},${base.y + ny * half} L${waist.x},${waist.y} L${base.x - nx * half},${base.y - ny * half} Z`,
  };
}

/** The point on the curve a given distance before its end. */
function cubicBack(path: EdgePath, distance: number) {
  const at = (t: number) => {
    const u = 1 - t;
    const a = u * u * u;
    const b = 3 * u * u * t;
    const c = 3 * u * t * t;
    const d = t * t * t;
    return {
      x: a * path.p0.x + b * path.c1.x + c * path.c2.x + d * path.p3.x,
      y: a * path.p0.y + b * path.c1.y + c * path.c2.y + d * path.p3.y,
    };
  };
  // Bisect on the chord length: monotonic in t, so eight steps land within a
  // tenth of a pixel and cost less than measuring the path in the DOM.
  let low = 0;
  let high = 1;
  for (let step = 0; step < 8; step += 1) {
    const mid = (low + high) / 2;
    const point = at(mid);
    const chord = Math.hypot(path.p3.x - point.x, path.p3.y - point.y);
    if (chord > distance) low = mid;
    else high = mid;
  }
  return at((low + high) / 2);
}

export function EdgeLayer({
  edges,
  boxes,
  windowSec,
  selected,
  showLabels = true,
  avoid,
  muted = false,
  undirected = false,
  bow,
  above = false,
  kinds,
  hoveredEdge,
  hoveredNode,
  onHoverEdge,
  labelArea,
  width,
  height,
  testId,
}: EdgeLayerProps) {
  const cards: Rect[] = [];
  for (const box of avoid ?? [...boxes.values()]) {
    cards.push({ x: box.x, y: box.y, w: box.w, h: box.h });
  }

  const taken: Rect[] = [];
  const drawn = edges.flatMap((edge) => {
    const from = boxes.get(edge.client);
    const to = boxes.get(edge.server);
    if (!from || !to) return [];
    const path = edgePath(from, to, bow?.(edge));
    const rate = edgeRate(edge, windowSec);
    const rateText = formatRate(rate);
    const protocol = edgeProtocol(edge);
    const rttText = formatEdgeRtt(edge.rtt_us);
    const fullText = [protocol, rateText, rttText].filter(Boolean).join(" · ");
    const withoutRtt = [protocol, rateText].filter(Boolean).join(" · ");
    const widthOf = (text: string) => Math.round(text.length * LABEL.advance + LABEL.pad);

    // A label gives up detail in order, never all of it at once: latency first
    // (the panel lists it anyway), then the rate. The protocol is kept as long
    // as a spot exists because it is the only thing on the wire that says what
    // the hop actually carries rather than which port it happens to use.
    const onScreen =
      !labelArea ||
      (path.mid.x >= labelArea.x &&
        path.mid.x <= labelArea.x + labelArea.w &&
        path.mid.y >= labelArea.y &&
        path.mid.y <= labelArea.y + labelArea.h);
    let text = fullText;
    let spot = showLabels && onScreen ? placeLabel(path, widthOf(text), cards, taken) : null;
    for (const candidate of [withoutRtt, protocol, rateText]) {
      if (spot || !showLabels || !onScreen || candidate === text || !candidate) continue;
      text = candidate;
      spot = placeLabel(path, widthOf(candidate), cards, taken);
    }
    if (spot) taken.push(spot);

    const external = kinds?.get(edge.server) === "external" || kinds?.get(edge.client) === "external";
    const key = edgeKey(edge);
    return [{ edge, key, path, text, spot, rate, protocol, fullText, width: widthOf(fullText), external }];
  });

  const hot = hoveredEdge ? drawn.find((entry) => entry.key === hoveredEdge) : undefined;

  return (
    <svg
      className={
        above
          ? "pointer-events-none absolute left-0 top-0 z-30 overflow-visible"
          : "pointer-events-none absolute left-0 top-0 overflow-visible"
      }
      width={width}
      height={height}
      data-testid={testId}
      aria-hidden="true"
    >
      {drawn.map(({ edge, key, path, rate, external }) => {
        const warn = (edge.retransmits ?? 0) > 0;
        const touched = !selected || edge.client === selected || edge.server === selected;
        const lit = hoveredNode
          ? edge.client === hoveredNode || edge.server === hoveredNode
          : hoveredEdge === key;
        // With a card under the pointer, every line that does not touch it
        // steps back: the map is a hairball at overview zoom, and "what does
        // this node talk to" is a question the pointer can ask without
        // selecting anything.
        const context = hoveredNode ? lit : true;
        const tone = warn ? "warn" : lit || (selected && touched) ? "emphasis" : external ? "external" : "idle";
        const stroke =
          tone === "warn" ? LINE.warn : tone === "emphasis" ? LINE.emphasis : external ? LINE.external : LINE.idle;
        const emphasis = Boolean(lit || (selected && touched && !warn));
        const width = muted ? 1.2 : edgeWidth(rate, edge.tx_bytes ?? 0, emphasis);
        const alpha = muted ? 0.62 : !context ? 0.16 : touched ? (emphasis ? 1 : 0.9) : 0.4;
        const head = undirected ? null : arrowHead(path, width);
        return (
          <g key={key}>
            {/* A wide transparent twin of the curve: a 1px line is impossible
                to hover on purpose, and the map needs "which hop is this"
                to be answerable from the drawing itself. It sits in the same
                layer as the path, so a pointer over a card still reaches the
                card. */}
            {onHoverEdge && !muted ? (
              <path
                d={path.d}
                fill="none"
                stroke="transparent"
                strokeWidth={14}
                style={{ pointerEvents: "stroke" }}
                onPointerEnter={() => onHoverEdge(key)}
                onPointerLeave={() => onHoverEdge(null)}
              />
            ) : null}
            <path
              d={head ? head.d : path.d}
              data-protocol={edge.protocol || "tcp"}
              data-from={edge.client}
              data-to={edge.server}
              data-tone={tone}
              fill="none"
              opacity={touched ? 1 : 0.1}
              stroke={stroke}
              // 0.9 to 2.6: an edge is a hint at the shape of the traffic, and
              // a fat line competes with the card it points at. The floor is
              // thick enough to see at fit zoom on a dark surface.
              strokeWidth={width}
              strokeOpacity={alpha}
              // Butt, not round: the stroke stops at the head's base, and a
              // round cap would bulge out past it and read as a second shape.
              strokeLinecap="butt"
              // Two dash patterns, two meanings: a long dash is a retransmit
              // warning, a fine dot says the target is outside the cluster.
              // Selling them as one style would make a healthy outbound call
              // look like a broken one.
              strokeDasharray={
                warn && !muted
                  ? EDGE_DASH.warn
                  : external && !muted
                    ? EDGE_DASH.external
                    : undefined
              }
              style={{ transition: "opacity 150ms ease, stroke 150ms ease" }}
            />
            {head ? (
              <path
                d={head.neck}
                fill="none"
                stroke={stroke}
                strokeWidth={width}
                strokeOpacity={alpha}
                strokeLinecap="butt"
                style={{ transition: "opacity 150ms ease, stroke 150ms ease" }}
              />
            ) : null}
            {head ? (
              <path
                d={head.head}
                data-head={key}
                fill={stroke}
                fillOpacity={alpha}
                stroke="none"
                style={{ transition: "fill-opacity 150ms ease, fill 150ms ease" }}
              />
            ) : null}
          </g>
        );
      })}

      {drawn.map(({ edge, key, text, spot, protocol }) => {
        if (!spot) return null;
        const warn = (edge.retransmits ?? 0) > 0;
        const touched = !selected || edge.client === selected || edge.server === selected;
        const lit = hoveredNode
          ? edge.client === hoveredNode || edge.server === hoveredNode
          : hoveredEdge === key;
        return (
          <g
            key={key}
            data-protocol={protocol || edge.protocol || "tcp"}
            opacity={hoveredNode && edge.client !== hoveredNode && edge.server !== hoveredNode ? 0.12 : touched ? 1 : 0.1}
            style={{ transition: "opacity 150ms ease" }}
            transform={`translate(${spot.x + spot.w / 2},${spot.y + spot.h / 2})`}
          >
            <rect
              x={-spot.w / 2}
              y={-LABEL.height / 2}
              width={spot.w}
              height={LABEL.height}
              rx={3}
              fill={LINE.label}
              fillOpacity={lit ? 1 : 0.92}
              stroke={lit ? LINE.emphasis : warn ? "#5a3030" : LINE.labelBorder}
              strokeWidth={0.75}
            />
            <text
              x={0}
              y={0.5}
              textAnchor="middle"
              dominantBaseline="middle"
              fontSize={LABEL.font}
              fontFamily="ui-monospace, SFMono-Regular, Menlo, monospace"
              fill={warn ? "#f0a1a1" : lit ? "#dbe8fb" : LINE.labelText}
            >
              {text}
            </text>
          </g>
        );
      })}

      {/* The hovered hop's full numbers, above every label: the resting label
          may have dropped the RTT or the protocol to fit, and the one line the
          pointer is on is exactly the line the reader wants complete. */}
      {hot ? (
        <g
          transform={`translate(${hot.path.mid.x},${hot.path.mid.y - 16})`}
          style={{ pointerEvents: "none" }}
        >
          <rect
            x={-hot.width / 2}
            y={-LABEL.height / 2}
            width={hot.width}
            height={LABEL.height}
            rx={3}
            fill="#101722"
            stroke={LINE.emphasis}
            strokeWidth={0.9}
          />
          <text
            x={0}
            y={0.5}
            textAnchor="middle"
            dominantBaseline="middle"
            fontSize={LABEL.font}
            fontFamily="ui-monospace, SFMono-Regular, Menlo, monospace"
            fill="#e6eefc"
          >
            {hot.fullText}
          </text>
        </g>
      ) : null}
    </svg>
  );
}
