"use client";

import { TimeseriesCanvas } from "@/components/canvas/timeseries-canvas";
import { EmptyState } from "@/components/empty-state";
import { QueryProgress } from "@/components/ui/query-progress";
import { useDashboard } from "@/lib/use-dashboard";

export function TimeseriesView() {
  const { series, unit, empty } = useDashboard();

  if (empty) return <EmptyState info={empty} />;
  if (!series) return <QueryProgress height={300} />;
  return <TimeseriesCanvas payload={series} unit={unit} />;
}
