"use client";

import { ArrowLeftRight } from "lucide-react";

import { FlamegraphCanvas } from "@/components/canvas/flamegraph-canvas";
import { EmptyState } from "@/components/empty-state";
import { Button } from "@/components/ui/button";
import { QueryProgress } from "@/components/ui/query-progress";
import { formatClockRange, formatDuration, formatValue } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { dashboardStore } from "@/lib/store";
import { rangeLabel, resolveCompareWindows } from "@/lib/time";
import type { DiffPayload, FlamegraphPayload } from "@/lib/types";
import { useDashboard } from "@/lib/use-dashboard";

/**
 * The two windows a comparison is between.
 *
 * The brushes choose them, but a brush is a picture, not a label: without this
 * strip the reader has to remember which side of the split is the baseline.
 * Swapping is one button because reading a diff backwards is a normal question.
 */
function CompareHeader() {
  const state = useDashboard();
  const t = useT();
  const payload = state.flamegraph as DiffPayload | null;
  // Same snapshot as the request, so the strip cannot claim "same window"
  // while the API was given two slightly different ones.
  const { window: win, baseline } = resolveCompareWindows(state);
  const left = payload?.total_left ?? 0;
  const right = payload?.total_right ?? 0;
  // Profiles do not all count time: an allocation profile's totals are bytes.
  const unit = payload?.unit ?? "ns";
  const delta = right - left;
  const ratio = left ? (delta / left) * 100 : null;
  const sign = delta > 0 ? "+" : delta < 0 ? "−" : "";

  const side = (label: string, window: { from: number; until: number }, total: number) => (
    <span className="flex items-baseline gap-1.5">
      <span className="text-[10px] font-medium tracking-wide text-muted-foreground uppercase">
        {label}
      </span>
      <span className="font-mono text-[11px]">
        {formatClockRange(window.from, window.until)}
      </span>
      <span className="font-mono text-[11px] text-muted-foreground">
        {formatDuration(window.until - window.from)}
      </span>
      {total > 0 ? (
        <span className="font-mono text-[11px] text-foreground/80">{formatValue(total, unit)}</span>
      ) : (
        // A window with nothing in it is a fact about the comparison, not a
        // missing value: saying so is what explains a diff that is all "new".
        <span className="text-[11px] text-muted-foreground italic">{t("no samples")}</span>
      )}
    </span>
  );

  return (
    <div
      className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-md bg-secondary/40 px-2.5 py-1.5"
      data-testid="compare-summary"
    >
      {side(t("A · Baseline"), baseline, left)}
      <span aria-hidden="true" className="text-muted-foreground">
        ·
      </span>
      {side(t("B · Comparison"), win, right)}
      <span className="font-mono text-[11px] text-foreground/80">
        Δ {sign}
        {formatValue(Math.abs(delta), unit)}
        {ratio === null ? ` (${t("new")})` : ` (${sign}${Math.abs(ratio).toFixed(1)}%)`}
      </span>
      <span className="flex-1" />
      <Button
        variant="ghost"
        size="xs"
        id="compare-swap"
        onClick={() => dashboardStore.swapBaseline()}
      >
        <ArrowLeftRight />
        {t("Swap")}
      </Button>
    </div>
  );
}

/** Flamegraph and compare share this: both payloads flow through one canvas. */
export function FlamegraphView() {
  const state = useDashboard();
  const { flamegraph, empty } = state;

  if (empty) return <EmptyState info={empty} />;
  if (!flamegraph) return <QueryProgress height={420} />;

  const range = state.mode === "relative" ? rangeLabel(state.range) : "custom";
  const stem = `lidlesseye-${state.type}-${range}`.replace(/\s+/g, "-").toLowerCase();
  const diff = flamegraph as DiffPayload;
  const isDiff = Array.isArray(diff.diff) && Array.isArray(diff.left);

  return (
    <FlamegraphCanvas
      payload={flamegraph as FlamegraphPayload | DiffPayload}
      fileStem={isDiff ? `${stem}-compare` : stem}
      header={isDiff ? <CompareHeader /> : null}
    />
  );
}
