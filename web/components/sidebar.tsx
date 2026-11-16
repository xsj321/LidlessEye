"use client";

// Navigation drawer for the whole application.
//
// These buttons switch the *page*, not a panel: each one is addressable in the
// hash and adds a history entry, so the landmark is a `<nav>` with
// `aria-current="page"`. Keeping `data-view`/`data-state` on the buttons is
// deliberate — the browser smoke test clicks them by name and checks which one
// is lit, exactly as a user would.
//
// Collapsing keeps the same buttons in the DOM with their labels visually
// hidden. Nothing is unmounted: a keyboard user can Tab through the icons and a
// tooltip names each one, while the content pane gains the full width.

import { useEffect, useState } from "react";
import {
  Activity,
  Flame,
  GitCompare,
  LayoutDashboard,
  ListOrdered,
  Network,
  PanelLeftClose,
  PanelLeftOpen,
  Tags,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { SettingsDialog } from "@/components/settings-dialog";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { dashboardStore } from "@/lib/store";
import type { View } from "@/lib/types";
import { cn } from "@/lib/utils";

import { useDashboard } from "@/lib/use-dashboard";

export interface NavItem {
  value: View;
  label: string;
  icon: LucideIcon;
}

export const NAV: ReadonlyArray<NavItem> = [
  { value: "servicemap", label: "Service map", icon: Network },
  { value: "overview", label: "Overview", icon: LayoutDashboard },
  { value: "flamegraph", label: "Flamegraph", icon: Flame },
  { value: "compare", label: "Compare", icon: GitCompare },
  { value: "top", label: "Top functions", icon: ListOrdered },
  { value: "timeseries", label: "Time series", icon: Activity },
  { value: "labels", label: "Labels", icon: Tags },
];

const COLLAPSED_KEY = "lidlesseye.sidebar.collapsed";

function storedCollapsed(): boolean {
  if (typeof window === "undefined") return false;
  try {
    return window.localStorage.getItem(COLLAPSED_KEY) === "1";
  } catch {
    return false;
  }
}

function Brand({ collapsed }: { collapsed: boolean }) {
  return (
    <div className="flex h-12 shrink-0 items-center gap-2 border-b px-3">
      <svg className="size-5 shrink-0 text-primary" viewBox="0 0 32 32" aria-hidden="true">
        <path
          d="M4 16c3.4-7 7.6-7 12 0s8.6 7 12 0"
          fill="none"
          stroke="currentColor"
          strokeWidth={2.6}
          strokeLinecap="round"
        />
        <circle cx="16" cy="16" r="3.4" fill="currentColor" />
      </svg>
      <span className={cn("truncate text-sm font-medium tracking-tight", collapsed && "sr-only")}>
        LidlessEye
      </span>
    </div>
  );
}

export function Sidebar() {
  const state = useDashboard();
  const [collapsed, setCollapsed] = useState(false);

  // Read the preference after hydration so the prerendered HTML and the first
  // client render agree; the drawer then settles into the saved width.
  useEffect(() => {
    setCollapsed(storedCollapsed());
  }, []);

  const toggle = () => {
    setCollapsed((current) => {
      const next = !current;
      try {
        window.localStorage.setItem(COLLAPSED_KEY, next ? "1" : "0");
      } catch {
        /* storage is optional */
      }
      return next;
    });
  };

  return (
    <aside
      id="sidebar"
      data-testid="sidebar"
      data-collapsed={collapsed ? "true" : "false"}
      aria-label="Application navigation"
      className={cn(
        "flex h-full shrink-0 flex-col border-r bg-card",
        "motion-safe:transition-[width] motion-safe:duration-200 motion-safe:ease-out",
        collapsed ? "w-12" : "w-48",
      )}
    >
      <div className="relative">
        <Brand collapsed={collapsed} />
        <Tooltip>
          <TooltipTrigger asChild>
            <Button
              id="sidebar-toggle"
              data-testid="sidebar-toggle"
              variant="ghost"
              size="icon-xs"
              className="absolute top-3 right-2"
              aria-label={collapsed ? "Expand navigation" : "Collapse navigation"}
              aria-expanded={!collapsed}
              aria-controls="tabs"
              onClick={toggle}
            >
              {collapsed ? <PanelLeftOpen /> : <PanelLeftClose />}
            </Button>
          </TooltipTrigger>
          <TooltipContent side="right">
            {collapsed ? "Expand navigation" : "Collapse navigation"}
          </TooltipContent>
        </Tooltip>
      </div>

      <nav id="tabs" aria-label="Dashboard views" className="flex-1 space-y-0.5 overflow-y-auto p-2">
        {NAV.map(({ value, label, icon: Icon }) => {
          const active = state.view === value;
          return (
            <Tooltip key={value}>
              <TooltipTrigger asChild>
                <button
                  type="button"
                  data-view={value}
                  data-state={active ? "active" : "inactive"}
                  aria-current={active ? "page" : undefined}
                  title={collapsed ? label : undefined}
                  onClick={() => dashboardStore.setView(value)}
                  className={cn(
                    "flex h-8 w-full items-center gap-2 rounded-md px-2 text-xs outline-none transition-colors",
                    "hover:bg-secondary/70 hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/70",
                    "text-muted-foreground data-[state=active]:bg-secondary data-[state=active]:text-foreground",
                    collapsed && "justify-center px-0",
                  )}
                >
                  <Icon className="size-4 shrink-0" />
                  <span className={cn("truncate", collapsed && "sr-only")}>{label}</span>
                </button>
              </TooltipTrigger>
              {collapsed ? <TooltipContent side="right">{label}</TooltipContent> : null}
            </Tooltip>
          );
        })}
      </nav>

      <div className="border-t p-2">
        <SettingsDialog collapsed={collapsed} />
      </div>
    </aside>
  );
}
