"use client";

// A workload in the one-hop map: one card that owns its container rows.
//
// The big map draws a workload and a peer as separate things; the
// neighbourhood answers a different question — "which part of this workload
// holds the connection" — so a container is a row *inside* its workload card,
// under a short spine, never a second free-floating box. One DOM subtree per
// workload means a long name or a third container cannot push a row onto its
// neighbour: the browser stacks them.
//
// Two things come from the browser rather than from JavaScript, following the
// Modern Web Guidance card guide:
//   * semantics and interaction are chosen separately — `<article>` carries
//     the content (`<h3>` + `<ul>`), a single stretched native `<button>` is
//     the whole-card action. Enter/Space and the accessible name are native.
//   * selection only recolours the border and the shadow. Border width,
//     padding and font weight never change, so a card does not jump when it
//     becomes the selected node.
//
// Each container row carries a port on the card border it faces: a caller
// exposes `out` on its right, a callee exposes `in` on its left, and the
// selected workload exposes both. The edge layer anchors lines at those exact
// points, and the layout aligns a peer's port to the row it talks to, so the
// curve between two containers is horizontal and unmistakably container to
// container — not card to card.
//
// The geometry below is shared with neighborhood.tsx: change a constant here
// and both the ports and the edge anchors move with it.

import { Fragment, memo } from "react";

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
  header: 48,
  rowHeight: 28,
  rowGap: 6,
  /** Space between the header and the first container row. */
  padTop: 10,
  padBottom: 11,
  /** How far a container row is indented from the card's left edge. */
  indent: 24,
  rightPad: 10,
  /** Radius of the port circles that sit on the card's left/right borders. */
  portRadius: 4,
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

/** Y of the centre of a container row — the height an edge port sits at. */
export function neighborRowCenter(index: number): number {
  return neighborRowOffset(index) + NEIGHBOR_CARD.rowHeight / 2;
}

export interface NeighborContainer {
  name: string;
  /** Connections per second on all edges that touch this container. */
  rate: number;
  /** Which sides actually carry an edge in this hop. Hides unused ports. */
  ports?: { in?: boolean; out?: boolean };
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
  /**
   * Ports on the card's header edge, used when traffic could not be attributed
   * to a container. Without this the curve would end at empty card space.
   */
  cardPorts?: { in?: boolean; out?: boolean };
}

const TONE_COLOR = {
  ok: "#4fbf8b",
  warn: "#f2a65a",
  muted: "#6b7585",
} as const;

function StatusGlyph({ tone }: { tone: keyof typeof TONE_COLOR }) {
  const color = TONE_COLOR[tone];
  return (
    <svg viewBox="0 0 12 12" className="size-3.5 shrink-0" aria-hidden="true">
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
  cardPorts,
}: NeighborCardProps) {
  const kind = node.kind ?? "workload";
  const palette = KIND[kind] ?? KIND.workload;
  const tone = healthTone(node);
  const heat = cpuMax > 0 ? Math.min(1, Math.sqrt(cpu / cpuMax)) : 0;
  const label = nodeLabel(node);
  const rawMeta = nodeMeta(node);
  // A workload with no namespace would title its second line with a bare
  // dash; the kind is the more useful thing to say there.
  const meta = rawMeta === "—" || rawMeta === "-" ? kind : rawMeta;
  const headerCenter = NEIGHBOR_CARD.header / 2;
  const portStyle = { borderColor: palette.bar, backgroundColor: palette.tint };
  const stubStyle = { backgroundColor: `${palette.bar}73` };

  return (
    <article
      data-testid={testId}
      // The pan surface checks this marker before capturing the pointer:
      // without it a press on a card never reaches its button.
      data-map-card=""
      aria-label={`${label} · ${meta}`}
      className={cn(
        "absolute isolate flex-col rounded-[10px] border border-solid",
        "shadow-[0_1px_0_0_rgba(255,255,255,0.04)_inset,0_8px_20px_-14px_rgba(0,0,0,0.9)]",
        // Selection is colour only: no border-width, padding or font-weight
        // change, so the card and its neighbours never jump.
        "transition-[border-color,box-shadow] duration-150",
        "has-[>button:focus-visible]:ring-2 has-[>button:focus-visible]:ring-ring/70",
        selected
          ? "border-primary/80 shadow-[0_0_0_1px_var(--primary),0_10px_26px_-10px_rgba(0,0,0,0.95)]"
          : "border-white/[0.12] hover:border-white/[0.26]",
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
        className="absolute inset-y-0 left-0 w-[3px] rounded-l-[10px]"
        style={{ backgroundColor: palette.bar }}
      />

      {/* The whole card is one action: a native button stretched over the
          article. Container rows are not interactive, so nothing competes. */}
      <button
        type="button"
        className="absolute inset-0 z-10 cursor-pointer rounded-[10px] focus-visible:outline-none"
        aria-label={`Open the neighbourhood of ${label}`}
        aria-expanded={selected}
        aria-controls="map-panel"
        onClick={() => onSelect?.(node.id)}
      />

      {/* No rule under the header: the container rows below are indented and
          tinted, which already says "these belong to the title". A hairline
          there only drew a second frame inside the card's own frame. */}
      <header
        className="flex items-center gap-2 px-3"
        style={{ height: NEIGHBOR_CARD.header }}
      >
        <hgroup className="min-w-0 flex-1 leading-none">
          <h3 className="neighbor-trim truncate text-[11px] font-semibold text-card-foreground">
            {label}
          </h3>
          <p className="neighbor-trim mt-1.5 truncate font-mono text-[9px] text-muted-foreground">
            {meta}
          </p>
        </hgroup>
        <StatusGlyph tone={tone} />
        {/* Traffic with no container attribution ends on the header edge, so
            the curve still terminates on a visible port. */}
        {cardPorts?.in ? (
          <span
            aria-hidden="true"
            data-card-port="in"
            className="absolute rounded-full border-[1.5px]"
            style={{
              left: -NEIGHBOR_CARD.portRadius,
              top: headerCenter - NEIGHBOR_CARD.portRadius,
              width: NEIGHBOR_CARD.portRadius * 2,
              height: NEIGHBOR_CARD.portRadius * 2,
              ...portStyle,
            }}
          />
        ) : null}
        {cardPorts?.out ? (
          <span
            aria-hidden="true"
            data-card-port="out"
            className="absolute rounded-full border-[1.5px]"
            style={{
              right: -NEIGHBOR_CARD.portRadius,
              top: headerCenter - NEIGHBOR_CARD.portRadius,
              width: NEIGHBOR_CARD.portRadius * 2,
              height: NEIGHBOR_CARD.portRadius * 2,
              ...portStyle,
            }}
          />
        ) : null}
      </header>

      {containers.length > 0 ? (
        <>
          {/* Ports live in their own card-relative layer. They must not sit
              inside the list: `top` is measured from the card's top, and a
              relatively positioned list starts below the header, which would
              push every port down by the header height. */}
          <div aria-hidden="true" className="pointer-events-none absolute inset-0 z-0">
            {containers.map((container, index) => {
              if (container.more) return null;
              const center = neighborRowCenter(index);
              // Strict booleans: a row that only receives must not also show
              // an out port, or a callee's dead end looks like it dials out.
              const hasIn = container.ports?.in === true;
              const hasOut = container.ports?.out === true;
              if (!hasIn && !hasOut) return null;
              return (
                <Fragment key={container.name}>
                  {hasIn ? (
                    <span
                      data-container-port="in"
                      data-node={node.id}
                      data-container={container.name}
                      className="absolute rounded-full border-[1.5px]"
                      style={{
                        left: -NEIGHBOR_CARD.portRadius,
                        top: center - NEIGHBOR_CARD.portRadius,
                        width: NEIGHBOR_CARD.portRadius * 2,
                        height: NEIGHBOR_CARD.portRadius * 2,
                        ...portStyle,
                      }}
                    />
                  ) : null}
                  {hasOut ? (
                    <span
                      data-container-port="out"
                      data-node={node.id}
                      data-container={container.name}
                      className="absolute rounded-full border-[1.5px]"
                      style={{
                        right: -NEIGHBOR_CARD.portRadius,
                        top: center - NEIGHBOR_CARD.portRadius,
                        width: NEIGHBOR_CARD.portRadius * 2,
                        height: NEIGHBOR_CARD.portRadius * 2,
                        ...portStyle,
                      }}
                    />
                  ) : null}
                  {/* A stub joins each live port to its row, so the port reads
                      as this row's terminal rather than a floating dot. */}
                  {hasIn ? (
                    <span
                      className="absolute h-px"
                      style={{ left: 0, top: center, width: NEIGHBOR_CARD.indent, ...stubStyle }}
                    />
                  ) : null}
                  {hasOut ? (
                    <span
                      className="absolute h-px"
                      style={{
                        right: 0,
                        top: center,
                        width: NEIGHBOR_CARD.rightPad,
                        ...stubStyle,
                      }}
                    />
                  ) : null}
                </Fragment>
              );
            })}
          </div>
          <ul
            className="relative m-0 list-none p-0"
            style={{
              paddingTop: NEIGHBOR_CARD.padTop,
              paddingBottom: NEIGHBOR_CARD.padBottom,
              paddingLeft: NEIGHBOR_CARD.indent,
              paddingRight: NEIGHBOR_CARD.rightPad,
            }}
          >
          {/* The spine is inside the card: container rows read as parts of
              the workload, not as neighbours that happen to sit below it. */}
          <span
            aria-hidden="true"
            className="absolute w-px"
            style={{
              left: 13,
              top: NEIGHBOR_CARD.padTop,
              bottom: NEIGHBOR_CARD.padBottom,
              backgroundColor: `${palette.bar}59`,
            }}
          />
          {containers.map((container, index) => {
            if (container.more) {
              return (
                <li
                  key={container.name}
                  className="relative flex items-center rounded-[5px] bg-white/[0.03] px-2 italic text-muted-foreground/80"
                  style={{
                    height: NEIGHBOR_CARD.rowHeight,
                    marginBottom: index < containers.length - 1 ? NEIGHBOR_CARD.rowGap : 0,
                    fontSize: 9,
                  }}
                >
                  {container.name}
                </li>
              );
            }
            return (
              <li
                key={container.name}
                data-testid="map-container-row"
                data-node={node.id}
                data-container={container.name}
                className="relative flex items-center gap-1.5 rounded-[5px] bg-white/[0.045] px-2"
                style={{
                  height: NEIGHBOR_CARD.rowHeight,
                  marginBottom: index < containers.length - 1 ? NEIGHBOR_CARD.rowGap : 0,
                }}
                title={`${label} · ${container.name}`}
              >
                <span
                  aria-hidden="true"
                  className="absolute h-px w-[11px]"
                  style={{
                    left: -11,
                    top: "50%",
                    backgroundColor: `${palette.bar}59`,
                  }}
                />
                <span className="neighbor-trim min-w-0 flex-1 truncate font-mono text-[10px] text-foreground/85">
                  {container.name}
                </span>
                <span className="shrink-0 font-mono text-[9px] tabular-nums text-muted-foreground">
                  {container.rate > 0 ? formatRate(container.rate) : ""}
                </span>
              </li>
            );
          })}
          </ul>
        </>
      ) : null}

      <span
        aria-hidden="true"
        className="absolute inset-x-0 bottom-0 h-[2px] rounded-b-[10px] bg-transparent"
        style={{ backgroundColor: heat > 0 ? "rgba(242,166,90,0.14)" : "transparent" }}
      >
        <span
          className="block h-full rounded-b-[10px]"
          style={{ width: `${Math.round(heat * 100)}%`, backgroundColor: "#f2a65a" }}
        />
      </span>
    </article>
  );
});
