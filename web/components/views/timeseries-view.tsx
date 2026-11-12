"use client";

import { TimeseriesCanvas } from "@/components/canvas/timeseries-canvas";
import { EmptyState } from "@/components/empty-state";
import { Skeleton } from "@/components/ui/skeleton";
import { useDashboard } from "@/lib/use-dashboard";

export function TimeseriesView() {
  const { series, unit, empty } = useDashboard();

  if (empty) return <EmptyState info={empty} />;
  if (!series) return <Skeleton className="h-[300px] w-full rounded-lg" />;
  return <TimeseriesCanvas payload={series} unit={unit} />;
}
