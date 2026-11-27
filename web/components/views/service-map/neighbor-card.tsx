"use client";

// A workload in the one-hop map: one card that owns its container rows.
//
// The big map draws a workload and a peer as separate things; the
// neighbourhood answers a different question — "which part of this workload
// holds the connection" — so a container is a pane *resting on* its workload
// card, never a second free-floating box. The belonging is carried by depth
// and translucency rather than by linework: the card is a translucent tint,
// each row a lighter translucent pane with a cast shadow, and the gaps between
// them do the grouping a spine and connector hairlines used to do. One DOM
// subtree per workload means a long name or a third container cannot push a
// row onto its neighbour: the browser stacks them.
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
// Each container row carries a port on **its own** left and right edge — not
// on the card's border: a caller exposes `out` beside the container name, a
// callee exposes `in` on the other side, and the selected workload exposes
// both. The edge layer anchors lines at those exact points, so a curve starts
// on the pane that owns the connection and ends on the pane that serves it.
// Traffic the agent could not attribute to a container keeps a port on the
// card's header edge, where the workload's name is.
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
import { RuntimeGlyph } from "./node-card";

/** All sizes in world pixels at scale 1; the view scales the layer, not the card. */
export const NEIGHBOR_CARD = {
  /** Border width. `box-sizing: border-box` makes the card's own border eat
   *  content height, so every offset below adds it back rather than letting
   *  the last container row sit on the bottom border. */
  border: 1,
  header: 48,
  rowHeight: 28,
  rowGap: 8,
  /** Space between the header and the first container row. */
  padTop: 14,
  padBottom: 14,
  /**
   * How far a container row is inset from the card's side edges — and the
   * header's own padding, so the title and the rows share one left edge. This
   * is also where the row's ports sit: the row's edge is the endpoint of a
   * connection, so the dot belongs on the pane, not on the card border.
   */
  indent: 8,
  rightPad: 8,
  /** Radius of the port circles on a container row's left/right edge. */
  portRadius: 4,
} as const;

/** X of a container row's left edge, relative to the card's left edge. */
export function neighborRowLeft(): number {
  return NEIGHBOR_CARD.border + NEIGHBOR_CARD.indent;
}

/** X of a container row's right edge, relative to the card's left edge. */
export function neighborRowRight(cardWidth: number): number {
  return cardWidth - NEIGHBOR_CARD.border - NEIGHBOR_CARD.rightPad;
}

/** Height a card needs for a given number of container rows. */
export function neighborCardHeight(containerCount: number): number {
  const chrome = NEIGHBOR_CARD.border * 2 + NEIGHBOR_CARD.header;
  if (containerCount <= 0) return chrome;
  return (
    chrome +
    NEIGHBOR_CARD.padTop +
    containerCount * NEIGHBOR_CARD.rowHeight +
    (containerCount - 1) * NEIGHBOR_CARD.rowGap +
    NEIGHBOR_CARD.padBottom
  );
}

/** Y of a container row, relative to the top of its card. */
export function neighborRowOffset(index: number): number {
  return (
    NEIGHBOR_CARD.border +
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
  /**
   * Which side of the selected workload this card is on. A node can appear on
   * both sides, and two buttons in one dialog that announce the same name are
   * indistinguishable to a screen reader.
   */
  relation?: "caller" | "callee";
  /** A pod card shows the replica name, but still walks to its workload. */
  title?: string;
  subtitle?: string;
  selectId?: string;
  /** Stable replica id, so the panel can be tested and debugged per pod. */
  podUid?: string;
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
  relation,
  title,
  subtitle,
  selectId,
  podUid,
  onSelect,
  testId,
  cardPorts,
}: NeighborCardProps) {
  const kind = node.kind ?? "workload";
  const palette = KIND[kind] ?? KIND.workload;
  const tone = healthTone(node);
  const heat = cpuMax > 0 ? Math.min(1, Math.sqrt(cpu / cpuMax)) : 0;
  const label = title ?? nodeLabel(node);
  const rawMeta = subtitle ?? nodeMeta(node);
  // A workload with no namespace would title its second line with a bare
  // dash; the kind is the more useful thing to say there.
  const meta = rawMeta === "—" || rawMeta === "-" ? kind : rawMeta;
  const headerCenter = NEIGHBOR_CARD.border + NEIGHBOR_CARD.header / 2;
  const portStyle = { borderColor: palette.bar, backgroundColor: palette.tint };
  // A translucent card, so the canvas reads faintly through it and the rows
  // above it have something to sit on. The tint still carries the kind.
  const cardFill = `color-mix(in srgb, ${palette.tint} 60%, transparent)`;

  return (
    <article
      data-testid={testId}
      data-pod-uid={podUid || undefined}
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
        backgroundColor: cardFill,
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
        aria-label={`Open the neighbourhood of ${label}${
          relation ? ` (${relation})` : ""
        }`}
        aria-expanded={selected}
        aria-controls="map-panel"
        onClick={() => onSelect?.(selectId ?? node.id)}
      />

      {/* No rule under the header: the container rows below are indented and
          tinted, which already says "these belong to the title". A hairline
          there only drew a second frame inside the card's own frame. */}
      <header
        className="flex items-center gap-2 px-2"
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
        {node.runtime ? <RuntimeGlyph runtime={node.runtime} /> : null}
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
              push every port down by the header height. They paint above the
              panes so the dot reads as attached to the row's edge rather than
              hidden under it. */}
          <div aria-hidden="true" className="pointer-events-none absolute inset-0 z-20">
            {containers.map((container, index) => {
              if (container.more) return null;
              const center = neighborRowCenter(index);
              // Strict booleans: a row that only receives must not also show
              // an out port, or a callee's dead end looks like it dials out.
              const hasIn = container.ports?.in === true;
              const hasOut = container.ports?.out === true;
              if (!hasIn && !hasOut) return null;
              // The dot is centred on the pane's own edge, so the edge layer's
              // anchor — and therefore the line — terminates on the container
              // element. No stub joins dot to row: the dot *is* the row's edge.
              return (
                <Fragment key={container.name}>
                  {hasIn ? (
                    <span
                      data-container-port="in"
                      data-node={node.id}
                      data-container={container.name}
                      data-pod-uid={podUid || undefined}
                      className="absolute rounded-full border-[1.5px]"
                      style={{
                        left: neighborRowLeft() - NEIGHBOR_CARD.portRadius,
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
                      data-pod-uid={podUid || undefined}
                      className="absolute rounded-full border-[1.5px]"
                      style={{
                        right:
                          box.w -
                          neighborRowRight(box.w) -
                          NEIGHBOR_CARD.portRadius,
                        top: center - NEIGHBOR_CARD.portRadius,
                        width: NEIGHBOR_CARD.portRadius * 2,
                        height: NEIGHBOR_CARD.portRadius * 2,
                        ...portStyle,
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
          {containers.map((container, index) => {
            if (container.more) {
              return (
                <li
                  key={container.name}
                  className="relative flex items-center rounded-[7px] bg-white/[0.04] px-2 italic text-muted-foreground/80"
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
                data-pod-uid={podUid || undefined}
                // A pane of glass resting on the card, not a box drawn in it:
                // a translucent surface with a shadow underneath. The lift
                // says "part of the workload above", which is more than the
                // old border and spine managed.
                className={cn(
                  "relative flex items-center gap-1.5 rounded-[7px]",
                  "bg-white/[0.10] backdrop-blur-[3px]",
                  // The lift is a cast shadow, not a lit edge: a hairline along
                  // the top of every row would be the same divider this change
                  // is removing, in disguise.
                  "shadow-[0_9px_18px_-10px_rgba(0,0,0,1)]",
                )}
                style={{
                  height: NEIGHBOR_CARD.rowHeight,
                  marginBottom: index < containers.length - 1 ? NEIGHBOR_CARD.rowGap : 0,
                  // A port eats half of the row's padding where it sits, so
                  // the text gets that half back: without it the rate ends up
                  // touching the dot on a busy row.
                  paddingLeft:
                    container.ports?.in === true
                      ? NEIGHBOR_CARD.indent + NEIGHBOR_CARD.portRadius
                      : NEIGHBOR_CARD.indent,
                  paddingRight:
                    container.ports?.out === true
                      ? NEIGHBOR_CARD.rightPad + NEIGHBOR_CARD.portRadius
                      : NEIGHBOR_CARD.rightPad,
                }}
                title={`${label} · ${container.name}`}
              >
                <span className="neighbor-trim min-w-0 flex-1 truncate font-mono text-[10px] text-foreground/90">
                  {container.name}
                </span>
                {/* The container inherits the language of the workload: the
                    profiler resolves a stack to a pod, not to a process tree,
                    so claiming a different language per row would invent it. */}
                {node.runtime ? <RuntimeGlyph runtime={node.runtime} /> : null}
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
