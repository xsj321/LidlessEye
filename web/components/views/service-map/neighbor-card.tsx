"use client";

// A workload as it appears in the one-hop map: one card that owns its
// containers.
//
// The big map draws a workload and a peer as separate things; the
// neighbourhood has a different job. There the question is "which part of
// this workload holds the connection", so a container is a row *inside* the
// workload card, under a short spine, not a second free-floating box next to
// it. Keeping the row in the card's own DOM subtree is what makes overlap
// impossible by construction: the browser stacks them, there is no second
// set of absolute coordinates that can drift out of sync with the layout.
//
// The geometry below is shared with the layout in neighborhood.tsx. Both
// import it: if a padding changes here, the edge anchors move with it.

import { memo } from "react";

import { Card } from "@/components/ui/card";
import {
  KIND,
  formatRate,
  healthTone,
  nodeLabel,
  nodeMeta,
  type Box,
} from "@/lib/servicemap-graph";
import type { ServiceMapNode } from "@/lib/types";
import { cn } from "@/lib/utils";

/** All sizes in world pixels at scale 1; the view scales the layer, not the card. */
export const NEIGHBOR_CARD = {
  header: 44,
  rowHeight: 26,
  rowGap: 5,
  /** Space between the header and the first container row. */
  padTop: 8,
  padBottom: 9,
  /** How far a container row is indented from the card's left edge. */
  indent: 22,
  rightPad: 8,
} as const;

/** Height a card needs for a given number of container rows. */
export function neighborCardHeight(containerCount: number): number {
  if (containerCount <= 0) return NEIGHBOR_CARD.header;
  return (
    NEIGHBOR_CARD.header +
    NEIGHBOR_CARD.padTop +
    containerCount * NEIGHBOR_CARD.rowHeight +
    (containerCount - 1) * NEIGHBOR_CARD.rowGap +
    NEIGHBOR_CARD.padBottom
  );
}

/** Y of a container row, relative to the top of its card. */
export function neighborRowOffset(index: number): number {
  return (
    NEIGHBOR_CARD.header +
    NEIGHBOR_CARD.padTop +
    index * (NEIGHBOR_CARD.rowHeight + NEIGHBOR_CARD.rowGap)
  );
}

export interface NeighborContainer {
  name: string;
  /** Connections per second on all edges that touch this container. */
  rate: number;
  /** Set when more containers exist than the card can draw. */
  more?: boolean;
}

export interface NeighborCardProps {
  node: ServiceMapNode;
  box: Box;
  containers: NeighborContainer[];
  cpu: number;
  cpuMax: number;
  selected?: boolean;
  onSelect?: (id: string) => void;
  testId?: string;
}

const TONE_COLOR = {
  ok: "#4fbf8b",
  warn: "#f2a65a",
  muted: "#6b7585",
} as const;

function StatusGlyph({ tone }: { tone: keyof typeof TONE_COLOR }) {
  const color = TONE_COLOR[tone];
  return (
    <svg viewBox="0 0 12 12" className="size-3 shrink-0" aria-hidden="true">
      {tone === "ok" ? <circle cx="6" cy="6" r="4" fill={color} /> : null}
      {tone === "warn" ? <path d="M6 1.3 11.2 10.5H0.8z" fill={color} /> : null}
      {tone === "muted" ? (
        <circle cx="6" cy="6" r="3.5" fill="none" stroke={color} strokeWidth="1.3" />
      ) : null}
    </svg>
  );
}

export const NeighborCard = memo(function NeighborCard({
  node,
  box,
  containers,
  cpu,
  cpuMax,
  selected = false,
  onSelect,
  testId,
}: NeighborCardProps) {
  const kind = node.kind ?? "workload";
  const palette = KIND[kind] ?? KIND.workload;
  const tone = healthTone(node);
  const heat = cpuMax > 0 ? Math.min(1, Math.sqrt(cpu / cpuMax)) : 0;
  const label = nodeLabel(node);
  const meta = nodeMeta(node);

  return (
    <Card
      role="button"
      tabIndex={0}
      aria-expanded={selected}
      aria-controls="map-panel"
      data-testid={testId}
      // The pan surface checks this marker before capturing the pointer:
      // without it a press on a card never reaches this onClick.
      data-map-card=""
      title={`${label}\n${node.id}${meta ? `\n${meta}` : ""}`}
      onClick={(event) => {
        event.stopPropagation();
        onSelect?.(node.id);
      }}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          onSelect?.(node.id);
        }
      }}
      className={cn(
        // `Card` brings py-6 and gap-6. Both would push the rows past the
        // height the layout reserved for them and get clipped, so zero them
        // out: this card's geometry is the shared constants above.
        "absolute flex-col gap-0 select-none overflow-hidden rounded-md border px-0 py-0 shadow-sm",
        "cursor-pointer outline-none focus-visible:ring-2 focus-visible:ring-ring/70",
        "transition-[border-color,box-shadow] duration-150",
        selected
          ? "border-primary/80 shadow-[0_0_0_1px_var(--primary),0_6px_20px_-8px_rgba(0,0,0,0.9)]"
          : "border-white/10 hover:border-white/25",
      )}
      style={{
        left: box.x,
        top: box.y,
        width: box.w,
        height: box.h,
        backgroundColor: palette.tint,
      }}
    >
      <span
        aria-hidden="true"
        className="absolute left-0 top-0 h-full w-[3px]"
        style={{ backgroundColor: palette.bar }}
      />

      <div
        className="flex items-center gap-1.5 px-2"
        style={{ height: NEIGHBOR_CARD.header }}
      >
        <div className="min-w-0 flex-1 leading-none">
          <div className="truncate text-[10px] font-medium text-card-foreground">{label}</div>
          <div className="mt-1 truncate font-mono text-[8.5px] text-muted-foreground">{meta}</div>
        </div>
        <StatusGlyph tone={tone} />
      </div>

      {containers.length > 0 ? (
        <div
          className="relative"
          style={{
            paddingTop: NEIGHBOR_CARD.padTop,
            paddingBottom: NEIGHBOR_CARD.padBottom,
            paddingLeft: NEIGHBOR_CARD.indent,
            paddingRight: NEIGHBOR_CARD.rightPad,
          }}
        >
          {/* The spine is inside the card: container rows read as parts of the
              workload, not as neighbours that happen to sit underneath. */}
          <span
            aria-hidden="true"
            className="absolute top-0 w-px"
            style={{
              left: 11,
              top: NEIGHBOR_CARD.padTop,
              bottom: NEIGHBOR_CARD.padBottom,
              backgroundColor: `${palette.bar}66`,
            }}
          />
          {containers.map((container, index) => (
            <div
              key={container.name}
              data-testid={container.more ? undefined : "map-container-row"}
              data-node={container.more ? undefined : node.id}
              data-container={container.more ? undefined : container.name}
              className="relative flex items-center gap-1 rounded border border-white/5 bg-white/[0.05] px-1.5"
              style={{
                height: NEIGHBOR_CARD.rowHeight,
                marginBottom: index < containers.length - 1 ? NEIGHBOR_CARD.rowGap : 0,
              }}
              title={container.more ? undefined : `${label} · ${container.name}`}
            >
              <span
                aria-hidden="true"
                className="absolute h-px w-[10px]"
                style={{ left: -11, top: "50%", backgroundColor: `${palette.bar}66` }}
              />
          <span
            className={cn(
              "min-w-0 flex-1 truncate font-mono",
              container.more
                ? "text-[8.5px] italic text-muted-foreground/80"
                : "text-[9px] text-foreground/80",
            )}
          >
            {container.name}
          </span>
          {!container.more && container.rate > 0 ? (
            <span className="shrink-0 font-mono text-[8px] text-muted-foreground">
              {formatRate(container.rate)}
            </span>
          ) : null}
            </div>
          ))}
        </div>
      ) : null}

      <span
        aria-hidden="true"
        className="absolute inset-x-0 bottom-0 h-[2px] bg-transparent"
        style={{ backgroundColor: heat > 0 ? "rgba(242,166,90,0.16)" : "transparent" }}
      >
        <span
          className="block h-full"
          style={{ width: `${Math.round(heat * 100)}%`, backgroundColor: "#f2a65a" }}
        />
      </span>
    </Card>
  );
});
