"use client";

import { Play } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { useT } from "@/lib/i18n";
import { dashboardStore } from "@/lib/store";
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
  const { view, type, profileTypes, groupBy, labelNames, range, mode, auto } = useDashboard();
  const t = useT();

  // The service map is a network view: profile type, grouping, auto-refresh and
  // Apply all describe a *profile* query and would only mislead here. Its time
  // range is selected in the time bar, right next to the histogram brush.
  // Rendering an empty bordered strip would look like a broken control row, so
  // the row is omitted altogether on that view.
  if (view === "servicemap") return null;

  const typeOptions = profileTypes.length ? profileTypes : ["cpu"];
  const groupOptions = ["", "service_name", ...labelNames];

  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-2 border-b bg-card px-3 py-2">
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

      <Field label={t("Auto")} className="gap-1.5">
        <Switch
          checked={auto}
          onCheckedChange={(checked) => dashboardStore.setAuto(checked)}
          aria-label={t("Auto")}
        />
      </Field>

      <Button size="sm" id="apply" onClick={() => void dashboardStore.reload()}>
        <Play className="size-3.5" />
        {t("Apply")}
      </Button>
    </div>
  );
}
