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
// Labels are placed, not just dropped at the midpoint: in a graph with a hub,
// twenty midpoints land within a few dozen pixels of each other and the text
// turns into overlapping noise. Each label is therefore tried at five points
// along its own curve and five offsets from each, and takes the first spot that
// hits neither a card nor another label. A label with nowhere to go is skipped
// rather than drawn on top of its neighbour.
//
// Paths are drawn before labels, in a separate pass: a label's whole reason to
// exist is being read, and an edge that happens to be drawn later would strike
// a line straight through the number.

import { edgePath, edgeRate, formatRate, formatRtt, pointOnEdge } from "@/lib/servicemap-graph";
import type { Box, EdgePath } from "@/lib/servicemap-graph";
import type { ServiceMapEdge } from "@/lib/types";

export function edgeKey(edge: ServiceMapEdge): string {
  return `${edge.client}->${edge.server}:${edge.protocol}`;
}

const LINE = {
  idle: "#3a4553",
  warn: "#e05c5c",
  focused: "#8fa3bd",
  label: "#0e1014",
  labelBorder: "#232b36",
  labelText: "#9aa5b6",
};

// The label has to fit in the channel between two card columns, and that
// channel is what decides its size: 9px monospace with 10px of padding leaves
// the pill narrow enough to clear the cards on both sides, which is what lets
// every edge keep its own number instead of half of them being dropped.
const LABEL = { height: 14, font: 9, advance: 4.9, pad: 10 };

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
  const offsets = [0, -13, 13, -26, 26, -39, 39];
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
  width,
  height,
  testId,
}: EdgeLayerProps) {
  const cards: Rect[] = [];
  for (const box of boxes.values()) cards.push({ x: box.x, y: box.y, w: box.w, h: box.h });

  const taken: Rect[] = [];
  const drawn = edges.flatMap((edge) => {
    const from = boxes.get(edge.client);
    const to = boxes.get(edge.server);
    if (!from || !to) return [];
    const path = edgePath(from, to);
    const rate = edgeRate(edge, windowSec);
    const rateText = formatRate(rate);
    const fullText = edge.rtt_us ? `${rateText} · ${formatRtt(edge.rtt_us)}` : rateText;
    const widthOf = (text: string) => Math.round(text.length * LABEL.advance + LABEL.pad);

    // The number that matters most is the rate, so a crowded wire keeps it and
    // gives up the latency rather than giving up both: the panel lists the RTT
    // of every hop anyway, and a missing rate is an edge with nothing to say.
    let text = fullText;
    let labelWidth = widthOf(fullText);
    let spot = showLabels ? placeLabel(path, labelWidth, cards, taken) : null;
    if (showLabels && !spot && fullText !== rateText) {
      text = rateText;
      labelWidth = widthOf(rateText);
      spot = placeLabel(path, labelWidth, cards, taken);
    }
    if (spot) taken.push(spot);

    return [{ edge, path, text, labelWidth, spot, rate }];
  });

  return (
    <svg
      className="pointer-events-none absolute left-0 top-0 overflow-visible"
      width={width}
      height={height}
      data-testid={testId}
      aria-hidden="true"
    >
      <defs>
        <marker
          id="se-arrow"
          viewBox="0 0 8 8"
          refX="7"
          refY="4"
          markerWidth="5"
          markerHeight="5"
          orient="auto"
        >
          <path d="M0 0.8 L7 4 L0 7.2 z" fill="#5b6679" />
        </marker>
        <marker
          id="se-arrow-warn"
          viewBox="0 0 8 8"
          refX="7"
          refY="4"
          markerWidth="5"
          markerHeight="5"
          orient="auto"
        >
          <path d="M0 0.8 L7 4 L0 7.2 z" fill={LINE.warn} />
        </marker>
      </defs>

      {drawn.map(({ edge, path, rate }) => {
        const warn = (edge.retransmits ?? 0) > 0;
        const touched = !selected || edge.client === selected || edge.server === selected;
        return (
          <path
            key={edgeKey(edge)}
            d={path.d}
            fill="none"
            opacity={touched ? 1 : 0.16}
            stroke={warn ? LINE.warn : touched && selected ? LINE.focused : LINE.idle}
            strokeWidth={1 + Math.min(3.2, Math.log10(1 + rate * 20 + (edge.tx_bytes ?? 0) / 8192 + 1) * 1.4)}
            strokeOpacity={touched ? 0.9 : 0.5}
            strokeLinecap="round"
            markerEnd={warn ? "url(#se-arrow-warn)" : "url(#se-arrow)"}
            strokeDasharray={warn ? "5 4" : undefined}
            style={{ transition: "opacity 150ms ease" }}
          />
        );
      })}

      {drawn.map(({ edge, text, spot }) => {
        if (!spot) return null;
        const warn = (edge.retransmits ?? 0) > 0;
        const touched = !selected || edge.client === selected || edge.server === selected;
        return (
          <g
            key={edgeKey(edge)}
            opacity={touched ? 1 : 0.16}
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
              fillOpacity={0.92}
              stroke={LINE.labelBorder}
              strokeWidth={0.75}
            />
            <text
              x={0}
              y={0.5}
              textAnchor="middle"
              dominantBaseline="middle"
              fontSize={LABEL.font}
              fontFamily="ui-monospace, SFMono-Regular, Menlo, monospace"
              fill={warn ? "#f0a1a1" : LINE.labelText}
            >
              {text}
            </text>
          </g>
        );
      })}
    </svg>
  );
}
