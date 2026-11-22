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

import { useId } from "react";

import {
  edgePath,
  edgeProtocol,
  edgeRate,
  formatRate,
  formatRtt,
  pointOnEdge,
} from "@/lib/servicemap-graph";
import type { Box, EdgeBow, EdgePath } from "@/lib/servicemap-graph";
import type { ServiceMapEdge } from "@/lib/types";

export function edgeKey(edge: ServiceMapEdge): string {
  // Two containers of one pod can carry the same protocol to one peer. Without
  // their names both edges get the same React key and one of them disappears.
  return `${edge.client}->${edge.server}:${edge.protocol}:${edge.client_container ?? ""}:${edge.server_container ?? ""}`;
}

const LINE = {
  /** Unselected, ordinary hop. Brighter than the old #3a4553: on a dark card
   *  surface that value was indistinguishable from the gap between columns. */
  idle: "#55657c",
  /** A hop that leaves the cluster: cooler and dotted, so "this call is not
   *  ours" is visible before reading the label. */
  external: "#8d94a8",
  /** Incident to the selected node. */
  emphasis: "#a9c8f2",
  warn: "#e05c5c",
  label: "#0e1014",
  labelBorder: "#232b36",
  labelText: "#a9b4c6",
};

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
    const rttText = edge.rtt_us ? formatRtt(edge.rtt_us) : "";
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

  // Marker ids are per-instance: the map and the neighbourhood both render an
  // EdgeLayer, and a shared id would make the second one silently adopt the
  // first one's arrow colour and size.
  const uid = useId().replace(/[:#]/g, "");
  const marker = (tone: string) => `${uid}-arrow-${tone}`;

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
      <defs>
        {[
          ["idle", LINE.idle],
          ["emphasis", LINE.emphasis],
          ["external", LINE.external],
          ["warn", LINE.warn],
        ].map(([tone, fill]) => (
          <marker
            key={tone}
            id={marker(tone)}
            viewBox="0 0 8 8"
            refX="6.6"
            refY="4"
            markerWidth="4"
            markerHeight="4"
            orient="auto"
          >
            <path d="M0.4 1 L7 4 L0.4 7 z" fill={fill} />
          </marker>
        ))}
      </defs>

      {drawn.map(({ edge, key, path, rate, external }) => {
        const warn = (edge.retransmits ?? 0) > 0;
        const touched = !selected || edge.client === selected || edge.server === selected;
        const lit = hoveredNode
          ? edge.client === hoveredNode || edge.server === hoveredNode
          : hoveredEdge === key;
        const tone = warn ? "warn" : lit || (selected && touched) ? "emphasis" : external ? "external" : "idle";
        const stroke =
          tone === "warn" ? LINE.warn : tone === "emphasis" ? LINE.emphasis : external ? LINE.external : LINE.idle;
        const emphasis = lit || (selected && touched && !warn);
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
              d={path.d}
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
              strokeWidth={
                muted
                  ? 1.2
                  : 0.9 +
                    Math.min(
                      1.7,
                      Math.log10(1 + rate * 20 + (edge.tx_bytes ?? 0) / 8192 + 1) * 0.85,
                    ) +
                    (emphasis ? 0.35 : 0)
              }
              strokeOpacity={muted ? 0.62 : touched ? (emphasis ? 1 : 0.9) : 0.4}
              strokeLinecap="round"
              markerEnd={
                undirected ? undefined : `url(#${marker(tone === "warn" ? "warn" : emphasis ? "emphasis" : external ? "external" : "idle")})`
              }
              // Two dash patterns, two meanings: a long dash is a retransmit
              // warning, a fine dot says the target is outside the cluster.
              // Selling them as one style would make a healthy outbound call
              // look like a broken one.
              strokeDasharray={warn && !muted ? "5 4" : external && !muted ? "1.5 3" : undefined}
              style={{ transition: "opacity 150ms ease, stroke 150ms ease" }}
            />
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
            opacity={touched ? 1 : 0.1}
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
