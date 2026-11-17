"use client";

import { RefreshCw } from "lucide-react";

import { Banner } from "@/components/banner";
import { FilterBar } from "@/components/filter-bar";
import { NAV, Sidebar } from "@/components/sidebar";
import { TimeBar } from "@/components/time-bar";
import { Toolbar } from "@/components/toolbar";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { FlamegraphView } from "@/components/views/flamegraph-view";
import { LabelsView } from "@/components/views/labels-view";
import { OverviewView } from "@/components/views/overview-view";
import { ServiceMapView } from "@/components/views/service-map-view";
import { TimeseriesView } from "@/components/views/timeseries-view";
import { TopView } from "@/components/views/top-view";
import { useT } from "@/lib/i18n";
import { dashboardStore } from "@/lib/store";
import type { View } from "@/lib/types";
import { useDashboard } from "@/lib/use-dashboard";

/** The visible name of a view, reusing the drawer's own labels. */
function viewLabel(view: View): string {
  return NAV.find((item) => item.value === view)?.label ?? "Service map";
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
  const t = useT();

  // A tenant-scoped token cannot enumerate tenants: keep showing the one it is
  // pinned to rather than pretending there is a choice. The control lives in
  // the header now — it changes the whole screen, not just the query row.
  const showTenant = state.tenants.length > 1 || Boolean(state.tenant);

  return (
    <div id="app-shell">
      {/* Keyboard users land here first: the drawer holds seven buttons before
          the content starts, and tabbing through them on every view switch is
          the tax this link refunds. It is off-screen until focused. */}
      <a
        href="#main"
        className="sr-only focus:not-sr-only focus:absolute focus:top-3 focus:left-3 focus:z-50 focus:rounded-md focus:bg-card focus:px-3 focus:py-2 focus:text-xs focus:shadow-lg focus:ring-2 focus:ring-ring"
      >
        {t("Skip to main content")}
      </a>
      <Sidebar />

      <div id="app-content" className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-12 shrink-0 items-center gap-3 border-b bg-card px-3">
          {/* The brand already lives in the navigation drawer; repeating it
              here only pushed the useful controls to the right. */}
          <div className="ml-auto flex items-center gap-2">
            {showTenant ? (
              <Select
                value={state.tenant}
                onValueChange={(value) => dashboardStore.setTenantId(value)}
              >
                <SelectTrigger
                  size="sm"
                  id="tenant"
                  className="w-36"
                  aria-label={t("Tenant")}
                >
                  <SelectValue placeholder={t("default")} />
                </SelectTrigger>
                <SelectContent>
                  {(state.tenants.length ? state.tenants : [state.tenant || "default"]).map(
                    (id) => (
                      <SelectItem key={id} value={id}>
                        {id}
                      </SelectItem>
                    ),
                  )}
                </SelectContent>
              </Select>
            ) : null}
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
                  {t("Refresh")}
                </Button>
              </TooltipTrigger>
              <TooltipContent>{t("Reload (R)")}</TooltipContent>
            </Tooltip>
          </div>
        </header>

        <Toolbar />
        <FilterBar />
        <TimeBar />
        <Banner />

        <main
          id="main"
          // The skip link points here, so the target has to be focusable; -1
          // keeps it out of the tab order while still letting focus land.
          tabIndex={-1}
          className="min-h-0 flex-1 overflow-auto px-3 py-2 outline-none"
          data-loading={state.loading ? "1" : undefined}
        >
          {/* One h1 per page, naming the view that is on screen. The drawer
              already says which view is active visually, so this heading is
              for the document outline rather than for the eye. */}
          <h1 className="sr-only">{t(viewLabel(state.view))}</h1>
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
