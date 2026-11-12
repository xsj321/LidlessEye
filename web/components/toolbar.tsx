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
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
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
  const { tenant, tenants, type, profileTypes, groupBy, labelNames, range, mode, auto } =
    useDashboard();

  // A tenant-scoped token cannot enumerate tenants: the selector then shows the
  // one it is pinned to instead of pretending there is a choice.
  const showTenant = tenants.length > 1 || Boolean(tenant);
  const typeOptions = profileTypes.length ? profileTypes : ["cpu"];
  const groupOptions = ["", "service_name", ...labelNames];

  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-2 border-b bg-card px-3 py-2">
      {showTenant ? (
        <Field label="Tenant">
          <Select value={tenant} onValueChange={(value) => dashboardStore.setTenantId(value)}>
            <SelectTrigger size="sm" id="tenant" className="w-36" aria-label="Tenant">
              <SelectValue placeholder="default" />
            </SelectTrigger>
            <SelectContent>
              {(tenants.length ? tenants : [tenant || "default"]).map((id) => (
                <SelectItem key={id} value={id}>
                  {id}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </Field>
      ) : null}

      <Field label="Profile">
        <Select value={type} onValueChange={(value) => dashboardStore.setType(value)}>
          <SelectTrigger size="sm" id="profile-type" className="w-32" aria-label="Profile type">
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

      <Field label="Group by">
        <Select
          value={groupBy || NO_GROUP}
          onValueChange={(value) =>
            dashboardStore.setGroupBy(value === NO_GROUP ? "" : value)
          }
        >
          <SelectTrigger size="sm" id="group-by" className="w-36" aria-label="Group by">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {groupOptions.map((name) => (
              <SelectItem key={name || NO_GROUP} value={name || NO_GROUP}>
                {name || "none"}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </Field>

      <Field label="Range">
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
          <SelectTrigger size="sm" id="range" className="w-44" aria-label="Range">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {RANGES.map(([value, label]) => (
              <SelectItem key={value} value={value}>
                {label}
              </SelectItem>
            ))}
            <SelectItem value={CUSTOM_RANGE}>Custom range</SelectItem>
          </SelectContent>
        </Select>
      </Field>

      <Field label="Auto" className="gap-1.5">
        <Switch
          checked={auto}
          onCheckedChange={(checked) => dashboardStore.setAuto(checked)}
          aria-label="Reload every 30 seconds"
        />
      </Field>

      <Tooltip>
        <TooltipTrigger asChild>
          <Button size="sm" id="apply" onClick={() => void dashboardStore.reload()}>
            <Play className="size-3.5" />
            Apply
          </Button>
        </TooltipTrigger>
        <TooltipContent>Re-run the query with the current filters</TooltipContent>
      </Tooltip>
    </div>
  );
}
