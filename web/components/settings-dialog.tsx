"use client";

// Settings live outside the URL: changing them must not navigate away from the
// screen you are looking at. The dialog is opened from the navigation drawer,
// but it is its own Radix dialog so Escape and focus return behave like any
// other modal surface.

import { RotateCcw, Settings2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Separator } from "@/components/ui/separator";
import { Switch } from "@/components/ui/switch";
import { RANGES } from "@/lib/time";
import { dashboardStore } from "@/lib/store";
import {
  AUTO_REFRESH_OPTIONS,
  BUCKET_OPTIONS,
  MAX_NODES_OPTIONS,
  settingsStore,
  useSettings,
} from "@/lib/settings";
import type { Settings } from "@/lib/settings";
import { VIEWS } from "@/lib/types";

const VIEW_LABELS: Record<string, string> = {
  servicemap: "Service map",
  flamegraph: "Flamegraph",
  compare: "Compare",
  top: "Top functions",
  timeseries: "Time series",
  labels: "Labels",
  overview: "Overview",
};

function Row({
  label,
  hint,
  htmlFor,
  children,
}: {
  label: string;
  hint?: string;
  htmlFor?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="grid grid-cols-[1fr_auto] items-center gap-3 py-1.5">
      <div className="min-w-0">
        <Label htmlFor={htmlFor} className="text-xs font-medium">
          {label}
        </Label>
        {hint ? <p className="mt-0.5 text-[10px] leading-snug text-muted-foreground">{hint}</p> : null}
      </div>
      {children}
    </div>
  );
}

function NumberSelect({
  id,
  value,
  options,
  format,
  onChange,
}: {
  id: string;
  value: number;
  options: readonly number[];
  format: (value: number) => string;
  onChange: (value: number) => void;
}) {
  return (
    <Select value={String(value)} onValueChange={(next) => onChange(Number(next))}>
      <SelectTrigger id={id} className="h-7 w-36 text-xs">
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {options.map((option) => (
          <SelectItem key={option} value={String(option)} className="text-xs">
            {format(option)}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

export function SettingsDialog({ collapsed = false }: { collapsed?: boolean }) {
  const settings = useSettings();

  const update = (patch: Partial<Settings>) => {
    settingsStore.update(patch);
    if (patch.autoRefreshMs !== undefined) dashboardStore.refreshAuto();
  };

  return (
    <Dialog>
      <DialogTrigger asChild>
        <Button
          variant="ghost"
          id="settings"
          data-testid="settings-trigger"
          className="w-full justify-start gap-2 px-2 text-xs text-muted-foreground hover:text-foreground"
        >
          <Settings2 />
          <span className={collapsed ? "sr-only" : "truncate"}>Settings</span>
        </Button>
      </DialogTrigger>
      <DialogContent
        data-testid="settings-dialog"
        className="max-h-[85vh] max-w-lg overflow-y-auto"
      >
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Settings2 className="size-4 text-primary" />
            Settings
          </DialogTitle>
          <DialogDescription>
            Preferences are stored in this browser. They do not travel with a shared link.
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-1 py-1">
          <div className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            Start-up
          </div>
          <Row label="Default view" hint="Used only when the URL does not name a view.">
            <Select
              value={settings.defaultView}
              onValueChange={(value) => update({ defaultView: value as Settings["defaultView"] })}
            >
              <SelectTrigger id="settings-default-view" className="h-7 w-36 text-xs">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {VIEWS.map((view) => (
                  <SelectItem key={view} value={view} className="text-xs">
                    {VIEW_LABELS[view] ?? view}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Row>
          <Row label="Default range" hint="The relative window on a fresh load.">
            <Select
              value={settings.defaultRange}
              onValueChange={(value) => update({ defaultRange: value })}
            >
              <SelectTrigger id="settings-default-range" className="h-7 w-36 text-xs">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {RANGES.map(([value, label]) => (
                  <SelectItem key={value} value={value} className="text-xs">
                    {label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Row>

          <Separator className="my-2" />
          <div className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            Refresh
          </div>
          <Row
            label="Auto-refresh interval"
            hint="How often the current view reloads when auto-refresh is on."
          >
            <NumberSelect
              id="settings-refresh"
              value={settings.autoRefreshMs}
              options={AUTO_REFRESH_OPTIONS}
              format={(value) => (value < 60_000 ? `${value / 1000} s` : `${value / 60_000} min`)}
              onChange={(value) => update({ autoRefreshMs: value })}
            />
          </Row>

          <Separator className="my-2" />
          <div className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            Service map
          </div>
          <Row label="Edge labels" hint="Show connections/s and mean RTT on each edge.">
            <Switch
              id="settings-edge-labels"
              checked={settings.edgeLabels}
              onCheckedChange={(checked) => update({ edgeLabels: checked })}
              aria-label="Show edge labels"
            />
          </Row>
          <Row
            label="Label zoom threshold"
            hint="Labels appear once the map is zoomed past this point."
          >
            <div className="flex w-36 items-center gap-2">
              <input
                id="settings-label-zoom"
                type="range"
                min={0}
                max={1}
                step={0.01}
                value={settings.labelZoom}
                onChange={(event) => update({ labelZoom: Number(event.target.value) })}
                className="h-1.5 w-full cursor-pointer accent-primary"
                aria-label="Label zoom threshold"
              />
              <span className="w-7 shrink-0 text-right font-mono text-[10px] text-muted-foreground">
                {Math.round(settings.labelZoom * 100)}
              </span>
            </div>
          </Row>

          <Separator className="my-2" />
          <div className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            Charts
          </div>
          <Row label="Flamegraph frame cap" hint="Higher caps draw more frames but cost memory.">
            <NumberSelect
              id="settings-max-nodes"
              value={settings.flamegraphMaxNodes}
              options={MAX_NODES_OPTIONS}
              format={(value) => value.toLocaleString("en-US")}
              onChange={(value) => update({ flamegraphMaxNodes: value })}
            />
          </Row>
          <Row label="Histogram buckets" hint="Resolution of the time brush above the views.">
            <NumberSelect
              id="settings-buckets"
              value={settings.histogramBuckets}
              options={BUCKET_OPTIONS}
              format={(value) => String(value)}
              onChange={(value) => update({ histogramBuckets: value })}
            />
          </Row>
        </div>

        <DialogFooter className="gap-2">
          <Button
            variant="ghost"
            size="sm"
            onClick={() => {
              settingsStore.reset();
              dashboardStore.refreshAuto();
            }}
          >
            <RotateCcw />
            Reset to defaults
          </Button>
          <DialogClose asChild>
            <Button size="sm">Done</Button>
          </DialogClose>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
