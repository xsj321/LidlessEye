"use client";

// A purpose-built control for choosing a relative time window.
//
// A plain <Select> hides how much time a range really is and buries the
// choices in one long list. This picker groups quick ranges by unit in a
// grid, shows the current value at a glance, and still accepts a custom
// duration — the shape operators expect from a time-series tool.

import { useState } from "react";
import { Check, Clock3 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { useT } from "@/lib/i18n";
import { dashboardStore } from "@/lib/store";
import { RANGES } from "@/lib/time";
import { cn } from "@/lib/utils";
import { useDashboard } from "@/lib/use-dashboard";

/** Split presets into rows: sub-hour, hours, days. */
const GROUPS: ReadonlyArray<{ title: string; match: (value: string) => boolean }> = [
  { title: "Minutes", match: (v) => v.endsWith("m") },
  { title: "Hours", match: (v) => v.endsWith("h") },
  { title: "Days", match: (v) => v.endsWith("d") },
];

/** Short chip label: "15m" instead of "Last 15 minutes". */
function chipLabel(value: string): string {
  return value;
}

export function TimeRangePicker() {
  const state = useDashboard();
  const t = useT();
  const [open, setOpen] = useState(false);
  const [custom, setCustom] = useState("");
  const [error, setError] = useState(false);

  const current = RANGES.find(([value]) => value === state.range);
  const display = current ? t(current[1]) : state.range;

  const pick = (value: string) => {
    dashboardStore.setRelativeRange(value);
    setCustom("");
    setError(false);
    setOpen(false);
  };

  const applyCustom = () => {
    // Accept "45m", "90s" or a bare number (minutes).
    const raw = custom.trim();
    const match = /^(\d+)\s*([smhd])?$/.exec(raw);
    if (!match || Number(match[1]) <= 0) {
      setError(true);
      return;
    }
    pick(`${match[1]}${match[2] ?? "m"}`);
  };

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          id="relative-range"
          data-testid="relative-range"
          variant="outline"
          size="sm"
          className="h-7 gap-1.5 px-2 text-xs"
          aria-label={t("Range")}
          title={t("Choose a relative time range")}
        >
          <Clock3 className="size-3.5" />
          <span className="max-w-28 truncate">{display}</span>
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-72 p-2.5">
        <div className="space-y-2.5">
          {GROUPS.map((group) => {
            const items = RANGES.filter(([value]) => group.match(value));
            if (!items.length) return null;
            return (
              <div key={group.title}>
                <div className="mb-1 px-0.5 text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
                  {t(group.title)}
                </div>
                <div className="grid grid-cols-4 gap-1">
                  {items.map(([value, label]) => {
                    const active = value === state.range;
                    return (
                      <button
                        key={value}
                        type="button"
                        data-range={value}
                        aria-label={t(label)}
                        aria-pressed={active}
                        title={t(label)}
                        onClick={() => pick(value)}
                        className={cn(
                          "flex h-8 items-center justify-center rounded-md border text-xs font-medium transition-colors",
                          "outline-none focus-visible:ring-2 focus-visible:ring-ring/70",
                          active
                            ? "border-primary bg-primary/10 text-foreground"
                            : "border-border bg-background text-muted-foreground hover:bg-accent hover:text-foreground",
                        )}
                      >
                        {chipLabel(value)}
                        {active ? <Check className="ml-1 size-3" /> : null}
                      </button>
                    );
                  })}
                </div>
              </div>
            );
          })}

          <div className="border-t pt-2.5">
            <Label htmlFor="relative-custom" className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
              {t("Custom range")}
            </Label>
            <div className="mt-1 flex items-center gap-1.5">
              <Input
                id="relative-custom"
                value={custom}
                onChange={(event) => {
                  setCustom(event.target.value);
                  setError(false);
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter") applyCustom();
                }}
                placeholder="45m"
                className={cn("h-8 flex-1 text-xs", error && "border-destructive")}
                aria-label={t("Custom range")}
              />
              <Button size="sm" className="h-8" onClick={applyCustom}>
                {t("Apply")}
              </Button>
            </div>
            {error ? (
              <p className="mt-1 text-[10px] text-destructive">{t("Use a value like 45m, 90s or 2h.")}</p>
            ) : null}
          </div>
        </div>
      </PopoverContent>
    </Popover>
  );
}
