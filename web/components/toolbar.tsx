"use client";

import { RefreshCw } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { formatClockRange } from "@/lib/format";
import { rangeLabel, resolveWindow } from "@/lib/time";
import { useT } from "@/lib/i18n";
import { dashboardStore } from "@/lib/store";
import { ALLOCATORS, type AllocatorMode } from "@/lib/store";
import { RANGES } from "@/lib/time";
import { useDashboard } from "@/lib/use-dashboard";
import { cn } from "@/lib/utils";

/** Radix Select cannot hold an empty string, so "none" needs a placeholder. */
const NO_GROUP = "__none__";
const CUSTOM_RANGE = "custom";

function Field({
  label,
  htmlFor,
  children,
  className,
}: {
  label: string;
  htmlFor?: string;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <label htmlFor={htmlFor} className={cn("flex items-center gap-2", className)}>
      <span className="text-xs whitespace-nowrap text-muted-foreground">{label}</span>
      {children}
    </label>
  );
}

export function Toolbar() {
  const state = useDashboard();
  const { view, type, profileTypes, groupBy, labelNames, range, mode, auto, filters } = state;
  const { allocator, allocatorMode } = state;
  // A relative range keeps `fromNs`/`untilNs` at zero until the brush moves;
  // the window the query actually ran against is what this line reports.
  const window = resolveWindow(state);
  const t = useT();

  // The service map is a network view: profile type, grouping, auto-refresh and
  // Apply all describe a *profile* query and would only mislead here. Its time
  // range is selected in the time bar, right next to the histogram brush.
  // Rendering an empty bordered strip would look like a broken control row, so
  // the row is omitted altogether on that view.
  if (view === "servicemap") return null;

  const typeOptions = profileTypes.length ? profileTypes : ["cpu"];
  const groupOptions = ["", "service_name", ...labelNames];

  // Controls apply the moment they change, so there is no commit step and no
  // "Apply" button: it would only promise something the app never does. The
  // button that looks like one re-runs the query, which is what a user wants
  // when a range is stale or an earlier query failed.
  const scope = groupBy ? `${t("grouped by")} ${groupBy}` : t("every service");
  const filtered = filters.length
    ? `${filters.length} ${filters.length === 1 ? t("filter") : t("filters")}`
    : t("no filters");

  return (
    <div className="border-b bg-card px-3 py-2">
      <div className="flex flex-wrap items-end gap-x-4 gap-y-2">
        <Field label={t("Profile")}>
        <Select value={type} onValueChange={(value) => dashboardStore.setType(value)}>
          <SelectTrigger size="sm" id="profile-type" className="w-32" aria-label={t("Profile type")}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {typeOptions.map((name) => (
              <SelectItem key={name} value={name}>
                {name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </Field>

      <Field label={t("Group by")}>
        <Select
          value={groupBy || NO_GROUP}
          onValueChange={(value) =>
            dashboardStore.setGroupBy(value === NO_GROUP ? "" : value)
          }
        >
          <SelectTrigger size="sm" id="group-by" className="w-36" aria-label={t("Group by")}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {groupOptions.map((name) => (
              <SelectItem key={name || NO_GROUP} value={name || NO_GROUP}>
                {name || t("none")}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </Field>

      {/* Allocator frames are already in the stored CPU stacks, so this is a
          filter over what was recorded, not a different collection. It stays
          hidden outside the flame graph: self/inclusive only mean something
          when a call path is on screen. */}
      {view === "flamegraph" && state.type === "cpu" ? (
        <>
          <Field label={t("Allocator")} className="gap-1.5">
            <Switch
              checked={Boolean(allocator)}
              onCheckedChange={(checked) =>
                dashboardStore.setAllocator(checked ? allocator || "any" : "")
              }
              aria-label={t("Allocator view")}
              id="allocator-toggle"
            />
          </Field>
          {allocator ? (
            <>
              <Field label={t("Family")}>
                <Select
                  value={allocator}
                  onValueChange={(value) => dashboardStore.setAllocator(value)}
                >
                  <SelectTrigger
                    size="sm"
                    id="allocator-family"
                    className="w-32"
                    aria-label={t("Allocator family")}
                  >
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {ALLOCATORS.map((name) => (
                      <SelectItem key={name} value={name}>
                        {name === "any" ? t("all allocators") : name}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </Field>
              <Field label={t("Mode")}>
                <Select
                  value={allocatorMode}
                  onValueChange={(value) =>
                    dashboardStore.setAllocatorMode(value as AllocatorMode)
                  }
                >
                  <SelectTrigger
                    size="sm"
                    id="allocator-mode"
                    className="w-28"
                    aria-label={t("Allocator mode")}
                  >
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="inclusive">{t("Inclusive")}</SelectItem>
                    <SelectItem value="self">{t("Self")}</SelectItem>
                  </SelectContent>
                </Select>
              </Field>
            </>
          ) : null}
        </>
      ) : null}

      <Field label={t("Range")}>
        <Select
          value={mode === "absolute" ? CUSTOM_RANGE : range}
          onValueChange={(value) => {
            if (value === CUSTOM_RANGE) {
              dashboardStore.setMode("absolute");
              return;
            }
            dashboardStore.setRelativeRange(value);
          }}
        >
          <SelectTrigger size="sm" id="range" className="w-44" aria-label={t("Range")}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {RANGES.map(([value, label]) => (
              <SelectItem key={value} value={value}>
                {t(label)}
              </SelectItem>
            ))}
            <SelectItem value={CUSTOM_RANGE}>{t("Custom range")}</SelectItem>
          </SelectContent>
        </Select>
      </Field>

        <div className="ml-auto flex items-center gap-3">
          <Field label={t("Auto refresh")} className="gap-1.5">
            <Switch
              checked={auto}
              onCheckedChange={(checked) => dashboardStore.setAuto(checked)}
              aria-label={t("Auto refresh")}
            />
          </Field>

          {/* Deliberately quiet: the header already carries a labelled
              Refresh, and controls apply on change, so this is only a retry.
              A second orange button would compete with the real primary. */}
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="outline"
                size="icon-sm"
                id="apply"
                aria-label={t("Re-run the query (R)")}
                onClick={() => void dashboardStore.reload()}
              >
                <RefreshCw className="size-3.5" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>
              {t("Controls apply as you change them; this re-runs the query (R)")}
            </TooltipContent>
          </Tooltip>
        </div>
      </div>

      {/* The summary answers "what am I looking at" without reading three
          selects: the window the server actually resolved, the grouping, and
          how many label filters are in force. */}
      <p
        className="mt-1.5 flex flex-wrap items-baseline gap-x-1.5 font-mono text-[11px] text-muted-foreground"
        data-testid="query-summary"
      >
        <span className="text-foreground/80">{type}</span>
        <span aria-hidden="true">·</span>
        <span>
          {formatClockRange(window.from, window.until)}
          {mode === "relative" ? ` (${t(rangeLabel(range))})` : ""}
        </span>
        <span aria-hidden="true">·</span>
        <span>{scope}</span>
        <span aria-hidden="true">·</span>
        <span>{filtered}</span>
      </p>
    </div>
  );
}
