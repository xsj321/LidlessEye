"use client";

import { RefreshCw } from "lucide-react";

import { Banner } from "@/components/banner";
import { FilterBar } from "@/components/filter-bar";
import { TimeBar } from "@/components/time-bar";
import { Toolbar } from "@/components/toolbar";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { FlamegraphView } from "@/components/views/flamegraph-view";
import { LabelsView } from "@/components/views/labels-view";
import { OverviewView } from "@/components/views/overview-view";
import { TimeseriesView } from "@/components/views/timeseries-view";
import { TopView } from "@/components/views/top-view";
import { dashboardStore } from "@/lib/store";
import type { View } from "@/lib/types";
import { useDashboard } from "@/lib/use-dashboard";

const NAV: ReadonlyArray<{ value: View; label: string }> = [
  { value: "flamegraph", label: "Flamegraph" },
  { value: "compare", label: "Compare" },
  { value: "top", label: "Top functions" },
  { value: "timeseries", label: "Time series" },
  { value: "labels", label: "Labels" },
  { value: "overview", label: "Overview" },
];

function Brand() {
  return (
    <div className="flex items-center gap-2">
      <svg className="size-5 text-primary" viewBox="0 0 32 32" aria-hidden="true">
        <path
          d="M4 16c3.4-7 7.6-7 12 0s8.6 7 12 0"
          fill="none"
          stroke="currentColor"
          strokeWidth={2.6}
          strokeLinecap="round"
        />
        <circle cx="16" cy="16" r="3.4" fill="currentColor" />
      </svg>
      <span className="text-sm font-medium tracking-tight">LidlessEye</span>
    </div>
  );
}

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
      <header className="flex h-12 shrink-0 items-center gap-5 border-b bg-card px-3">
        <Brand />

        <Tabs
          id="tabs"
          value={state.view}
          onValueChange={(value) => dashboardStore.setView(value as View)}
        >
          <TabsList className="h-8 gap-0.5 bg-transparent p-0">
            {NAV.map((item) => (
              <TabsTrigger
                key={item.value}
                value={item.value}
                data-view={item.value}
                className="h-7 rounded-md px-2.5 text-xs data-[state=active]:bg-secondary data-[state=active]:shadow-none"
              >
                {item.label}
              </TabsTrigger>
            ))}
          </TabsList>
        </Tabs>

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
  );
}
