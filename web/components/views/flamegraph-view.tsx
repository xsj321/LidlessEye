"use client";

import { FlamegraphCanvas } from "@/components/canvas/flamegraph-canvas";
import { EmptyState } from "@/components/empty-state";
import { Skeleton } from "@/components/ui/skeleton";
import { useDashboard } from "@/lib/use-dashboard";

/** Flamegraph and compare share this: both payloads flow through one canvas. */
export function FlamegraphView() {
  const { flamegraph, empty } = useDashboard();

  if (empty) return <EmptyState info={empty} />;
  if (!flamegraph) return <Skeleton className="h-[420px] w-full rounded-lg" />;
  return <FlamegraphCanvas payload={flamegraph} />;
}
