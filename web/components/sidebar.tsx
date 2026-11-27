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
  MemoryStick,
  Network,
  PanelLeftClose,
  PanelLeftOpen,
  Tags,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { SettingsDialog } from "@/components/settings-dialog";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useT } from "@/lib/i18n";
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
  { value: "memory", label: "Memory", icon: MemoryStick },
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

function CollapseButton({
  collapsed,
  toggle,
  t,
}: {
  collapsed: boolean;
  toggle: () => void;
  t: (key: string) => string;
}) {
  const label = collapsed ? t("Expand navigation") : t("Collapse navigation");
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          id="sidebar-toggle"
          data-testid="sidebar-toggle"
          variant="ghost"
          size="icon-sm"
          className="shrink-0"
          aria-label={label}
          aria-expanded={!collapsed}
          aria-controls="tabs"
          onClick={toggle}
        >
          {/* Match the size-4 nav icons so the rail looks uniform. */}
          {collapsed ? <PanelLeftOpen className="size-4" /> : <PanelLeftClose className="size-4" />}
        </Button>
      </TooltipTrigger>
      <TooltipContent side="right">{label}</TooltipContent>
    </Tooltip>
  );
}

function Brand({
  collapsed,
  toggle,
  t,
}: {
  collapsed: boolean;
  toggle: () => void;
  t: (key: string) => string;
}) {
  return (
    <div
      className={cn(
        "flex h-12 shrink-0 items-center gap-2 border-b px-3",
        collapsed && "justify-center px-2",
      )}
    >
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
      <span
        className={cn(
          "min-w-0 flex-1 truncate text-sm font-medium tracking-tight",
          collapsed && "sr-only",
        )}
      >
        LidlessEye
      </span>
      {/* Expanded: the button is a flex item beside the wordmark, never an
          absolute overlay on top of it. Collapsed: it is hidden here and
          rendered as the last option of the icon nav below. */}
      {collapsed ? null : <CollapseButton collapsed={false} toggle={toggle} t={t} />}
    </div>
  );
}

export function Sidebar() {
  const state = useDashboard();
  const t = useT();
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
      aria-label={t("Application navigation")}
      className={cn(
        "flex h-full shrink-0 flex-col border-r bg-card",
        "motion-safe:transition-[width] motion-safe:duration-200 motion-safe:ease-out",
        collapsed ? "w-12" : "w-48",
      )}
    >
      <Brand collapsed={collapsed} toggle={toggle} t={t} />

      <nav id="tabs" aria-label={t("Dashboard views")} className="flex-1 space-y-0.5 overflow-y-auto p-2">
        {NAV.map(({ value, label, icon: Icon }) => {
          const active = state.view === value;
          const text = t(label);
          return (
            <Tooltip key={value}>
              <TooltipTrigger asChild>
                <button
                  type="button"
                  data-view={value}
                  data-state={active ? "active" : "inactive"}
                  aria-current={active ? "page" : undefined}
                  title={collapsed ? text : undefined}
                  onClick={() => dashboardStore.setView(value)}
                  className={cn(
                    "flex h-8 w-full items-center gap-2 rounded-md px-2 text-xs outline-none transition-colors",
                    "hover:bg-secondary/70 hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/70",
                    "text-muted-foreground data-[state=active]:bg-secondary data-[state=active]:text-foreground",
                    collapsed && "justify-center px-0",
                  )}
                >
                  <Icon className="size-4 shrink-0" />
                  <span className={cn("truncate", collapsed && "sr-only")}>{text}</span>
                </button>
              </TooltipTrigger>
              {collapsed ? <TooltipContent side="right">{text}</TooltipContent> : null}
            </Tooltip>
          );
        })}
        {/* The expand control is the last option of the collapsed rail, so it
            never sits on top of the brand and stays easy to find by touch. */}
        {collapsed ? (
          <div className="pt-1">
            <CollapseButton collapsed toggle={toggle} t={t} />
          </div>
        ) : null}
      </nav>

      <div className="border-t p-2">
        <SettingsDialog collapsed={collapsed} />
      </div>
    </aside>
  );
}
