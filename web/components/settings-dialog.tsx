"use client";

// Settings live outside the URL: changing them must not navigate away from the
// screen you are looking at. The dialog is opened from the navigation drawer,
// but it is its own Radix dialog so Escape and focus return behave like any
// other modal surface.

import { useEffect, useState } from "react";
import { RotateCcw, Settings2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog,
  DialogClose,
  DialogContent,
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
import { useT } from "@/lib/i18n";
import { RANGES } from "@/lib/time";
import { dashboardStore } from "@/lib/store";
import {
  BUCKET_OPTIONS,
  MAX_AUTO_REFRESH_MS,
  MAX_NODES_OPTIONS,
  MIN_AUTO_REFRESH_MS,
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

/**
 * Refresh cadence: preset chips for common values plus a free-form seconds
 * input so an operator can dial in exactly 5s / 10s / 15s / … without a menu.
 */
function RefreshInput({
  value,
  onChange,
  t,
}: {
  value: number;
  onChange: (ms: number) => void;
  t: (key: string) => string;
}) {
  const [text, setText] = useState(String(value / 1000));
  useEffect(() => setText(String(value / 1000)), [value]);

  const commit = () => {
    const seconds = Number(text);
    if (Number.isFinite(seconds) && seconds > 0) onChange(seconds * 1000);
    else setText(String(value / 1000));
  };

  return (
    <div className="flex items-center gap-1.5">
      <Input
        id="settings-refresh"
        type="number"
        min={MIN_AUTO_REFRESH_MS / 1000}
        max={MAX_AUTO_REFRESH_MS / 1000}
        step={1}
        value={text}
        onChange={(event) => setText(event.target.value)}
        onBlur={commit}
        onKeyDown={(event) => {
          if (event.key === "Enter") commit();
        }}
        className="h-7 w-20 text-xs"
        aria-label={t("Auto-refresh interval")}
      />
      <span className="text-xs text-muted-foreground">s</span>
    </div>
  );
}

export function SettingsDialog({ collapsed = false }: { collapsed?: boolean }) {
  const settings = useSettings();
  const t = useT();

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
          <span className={collapsed ? "sr-only" : "truncate"}>{t("Settings")}</span>
        </Button>
      </DialogTrigger>
      <DialogContent
        data-testid="settings-dialog"
        className="max-h-[85vh] max-w-lg overflow-y-auto"
      >
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Settings2 className="size-4 text-primary" />
            {t("Settings")}
          </DialogTitle>
        </DialogHeader>

        <div className="space-y-1 py-1">
          <div className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            {t("General")}
          </div>
          <Row label={t("Language")} htmlFor="settings-language">
            <Select
              value={settings.language}
              onValueChange={(value) => update({ language: value as Settings["language"] })}
            >
              <SelectTrigger id="settings-language" className="h-7 w-36 text-xs">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="en" className="text-xs">
                  {t("English")}
                </SelectItem>
                <SelectItem value="zh" className="text-xs">
                  {t("Chinese")}
                </SelectItem>
              </SelectContent>
            </Select>
          </Row>

          <div className="mt-2 text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            {t("Start-up")}
          </div>
          <Row label={t("Default view")}>
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
                    {t(VIEW_LABELS[view] ?? view)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Row>
          <Row label={t("Default range")}>
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
                    {t(label)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Row>

          <Separator className="my-2" />
          <div className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            {t("Refresh")}
          </div>
          <Row label={t("Auto-refresh interval")}>
            <RefreshInput
              value={settings.autoRefreshMs}
              onChange={(value) => update({ autoRefreshMs: value })}
              t={t}
            />
          </Row>

          <Separator className="my-2" />
          <div className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            {t("Service map")}
          </div>
          <Row label={t("Edge labels")}>
            <Switch
              id="settings-edge-labels"
              checked={settings.edgeLabels}
              onCheckedChange={(checked) => update({ edgeLabels: checked })}
              aria-label={t("Edge labels")}
            />
          </Row>
          <Row label={t("Label zoom threshold")}>
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
                aria-label={t("Label zoom threshold")}
              />
              <span className="w-7 shrink-0 text-right font-mono text-[10px] text-muted-foreground">
                {Math.round(settings.labelZoom * 100)}
              </span>
            </div>
          </Row>

          <Separator className="my-2" />
          <div className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            {t("Charts")}
          </div>
          <Row label={t("Flamegraph frame cap")}>
            <NumberSelect
              id="settings-max-nodes"
              value={settings.flamegraphMaxNodes}
              options={MAX_NODES_OPTIONS}
              format={(value) => value.toLocaleString("en-US")}
              onChange={(value) => update({ flamegraphMaxNodes: value })}
            />
          </Row>
          <Row label={t("Histogram buckets")}>
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
            {t("Reset to defaults")}
          </Button>
          <DialogClose asChild>
            <Button size="sm">{t("Done")}</Button>
          </DialogClose>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
