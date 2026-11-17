"use client";

// One node of the map, as a real `Card` rather than a shape on a canvas.
//
// Three signals share the card without fighting for it:
//
//   colour   the left bar and a faint tint say what the thing is — a workload,
//            a ClusterIP service, or the world outside the cluster;
//   shape    the glyph on the right says how it is doing — filled circle for
//            all replicas ready, triangle for degraded, hollow ring when the
//            cluster did not report any;
//   position the bottom hairline is CPU: its length is the share of the
//            busiest node on the map, so cost reads as a bar instead of as a
//            colour that would have to compete with the two above.
//
// The card is positioned absolutely inside the map's transformed layer, so its
// coordinates are world pixels; the view scales the layer, not the card.

import { memo } from "react";

import { Card } from "@/components/ui/card";
import { healthTone, KIND, nodeLabel, nodeMeta } from "@/lib/servicemap-graph";
import type { Box } from "@/lib/servicemap-graph";
import type { ServiceMapNode } from "@/lib/types";
import { cn } from "@/lib/utils";

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
      {tone === "warn" ? (
        <path d="M6 1.3 11.2 10.5H0.8z" fill={color} />
      ) : null}
      {tone === "muted" ? (
        <circle cx="6" cy="6" r="3.5" fill="none" stroke={color} strokeWidth="1.3" />
      ) : null}
    </svg>
  );
}

export interface NodeCardProps {
  node: ServiceMapNode;
  box: Box;
  /** This node's on-CPU time, and the busiest node's, for the heat bar. */
  cpu: number;
  cpuMax: number;
  selected?: boolean;
  /** Dimmed because something else is selected. */
  dimmed?: boolean;
  compact?: boolean;
  onSelect?: (id: string) => void;
  testId?: string;
}

export const NodeCard = memo(function NodeCard({
  node,
  box,
  cpu,
  cpuMax,
  selected = false,
  dimmed = false,
  compact = false,
  onSelect,
  testId,
}: NodeCardProps) {
  const kind = node.kind ?? "workload";
  const palette = KIND[kind] ?? KIND.workload;
  const tone = healthTone(node);
  const heat = cpuMax > 0 ? Math.min(1, Math.sqrt(cpu / cpuMax)) : 0;
  const label = nodeLabel(node);
  const meta = nodeMeta(node);

  return (
    <Card
      data-testid={testId}
      // Every clickable card carries this marker, on the big map and in the
      // neighbourhood alike. A pan surface must not capture the pointer on a
      // card press, or the card's own click never fires — the popup used the
      // big map's test id for this check and silently lost its node jumps.
      data-map-card=""
      title={`${label}\n${node.id}${meta ? `\n${meta}` : ""}`}
      className={cn(
        "absolute select-none flex-row items-center gap-0 overflow-hidden rounded-md border py-0 px-0",
        "shadow-sm transition-[border-color,box-shadow,opacity] duration-150",
        // Focus is drawn on the card, not on the invisible button: the ring
        // must trace the card a sighted keyboard user is actually looking at.
        "has-[>button:focus-visible]:ring-2 has-[>button:focus-visible]:ring-ring/70",
        selected
          ? "border-primary/80 shadow-[0_0_0_1px_var(--primary),0_6px_20px_-8px_rgba(0,0,0,0.9)]"
          : "border-white/10 hover:border-white/25",
        dimmed && !selected && "opacity-35",
      )}
      style={{
        left: box.x,
        top: box.y,
        width: box.w,
        height: box.h,
        backgroundColor: palette.tint,
      }}
    >
      {/* A native button stretched over the card, exactly as the neighbour
          card does it. `div role="button"` would mean re-implementing Enter,
          Space-on-keyup, and the accessible name by hand; the browser already
          does all three, and the card is a disclosure rather than a toggle —
          it opens the panel, so `aria-expanded` is the state to report. */}
      <button
        type="button"
        className="absolute inset-0 z-10 cursor-pointer rounded-md focus-visible:outline-none"
        aria-label={`Open the neighbourhood of ${label}`}
        aria-expanded={selected}
        aria-controls="map-panel"
        onClick={(event) => {
          event.stopPropagation();
          onSelect?.(node.id);
        }}
      />
      {/* The kind bar: the one colour on the card that never changes with
          state, so "what is this" is answerable before "how is it". */}
      <span
        aria-hidden="true"
        className="h-full w-[3px] shrink-0"
        style={{ backgroundColor: palette.bar }}
      />
      <div className="flex min-w-0 flex-1 items-center gap-1.5 px-2">
        <div className="min-w-0 flex-1 leading-none">
          <div
            className={cn(
              "truncate font-medium text-card-foreground",
              compact ? "text-[10px]" : "text-[11px]",
            )}
          >
            {label}
          </div>
          <div
            className={cn(
              "mt-1 truncate font-mono text-muted-foreground",
              compact ? "text-[8.5px]" : "text-[9px]",
            )}
          >
            {meta}
          </div>
        </div>
        <StatusGlyph tone={tone} />
      </div>
      {/* CPU share, as a bar: a tint would have to fight the kind colour for
          the same surface, and a bar can be read at a glance. */}
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
