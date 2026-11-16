"use client";

import { RefreshCw } from "lucide-react";

import { Banner } from "@/components/banner";
import { FilterBar } from "@/components/filter-bar";
import { Sidebar } from "@/components/sidebar";
import { TimeBar } from "@/components/time-bar";
import { Toolbar } from "@/components/toolbar";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { FlamegraphView } from "@/components/views/flamegraph-view";
import { LabelsView } from "@/components/views/labels-view";
import { OverviewView } from "@/components/views/overview-view";
import { ServiceMapView } from "@/components/views/service-map-view";
import { TimeseriesView } from "@/components/views/timeseries-view";
import { TopView } from "@/components/views/top-view";
import { dashboardStore } from "@/lib/store";
import type { View } from "@/lib/types";
import { useDashboard } from "@/lib/use-dashboard";

function ViewSwitch({ view }: { view: View }) {
  switch (view) {
    case "flamegraph":
    case "compare":
      return <FlamegraphView />;
    case "top":
      return <TopView />;
    case "timeseries":
      return <TimeseriesView />;
    case "labels":
      return <LabelsView />;
    case "servicemap":
      return <ServiceMapView />;
    case "overview":
      return <OverviewView />;
    default:
      return null;
  }
}

export function Dashboard() {
  const state = useDashboard();

  return (
    <div id="app-shell">
      <Sidebar />

      <div id="app-content" className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-12 shrink-0 items-center gap-3 border-b bg-card px-3">
          <h1 className="truncate text-sm font-medium tracking-tight">LidlessEye</h1>
          <span className="hidden text-[11px] text-muted-foreground sm:inline">
            continuous profiling
          </span>

          <div className="ml-auto flex items-center gap-2">
            <span className="font-mono text-[11px] text-muted-foreground" data-testid="build-info">
              {state.build}
            </span>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="xs"
                  onClick={() => {
                    void dashboardStore.reload();
                    void dashboardStore.loadHistogram();
                  }}
                >
                  <RefreshCw />
                  Refresh
                </Button>
              </TooltipTrigger>
              <TooltipContent>Reload (R)</TooltipContent>
            </Tooltip>
          </div>
        </header>

        <Toolbar />
        <FilterBar />
        <TimeBar />
        <Banner />

        <main
          id="main"
          className="min-h-0 flex-1 overflow-auto px-3 py-2"
          data-loading={state.loading ? "1" : undefined}
        >
          {state.ready ? (
            <ViewSwitch view={state.view} />
          ) : (
            <Skeleton className="h-[420px] w-full rounded-lg" />
          )}
        </main>
      </div>
    </div>
  );
}
