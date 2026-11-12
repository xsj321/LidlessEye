"use client";

import { useEffect, useState } from "react";
import { ChevronsUpDown, Plus, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { composeSelector, OPS } from "@/lib/selector";
import { dashboardStore } from "@/lib/store";
import type { LabelFilter } from "@/lib/types";
import { useDashboard } from "@/lib/use-dashboard";
import { cn } from "@/lib/utils";

/**
 * Label value picker. The list comes from the server scoped by the other
 * filters, so it is loaded per row and per label — a plain Select would have
 * to render thousands of options with no way to search them.
 */
function ValueCombobox({
  label,
  value,
  onChange,
}: {
  label: string | undefined;
  value: string | undefined;
  onChange: (value: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [options, setOptions] = useState<string[]>([]);
  const [note, setNote] = useState("");

  useEffect(() => {
    if (!label) {
      setOptions([]);
      return;
    }
    let cancelled = false;
    void dashboardStore.filterValues(label).then((result) => {
      if (cancelled) return;
      setOptions(result.values);
      setNote(result.note);
    });
    return () => {
      cancelled = true;
    };
  }, [label]);

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          variant="outline"
          size="sm"
          role="combobox"
          data-role="value"
          aria-expanded={open}
          disabled={!label}
          title={note || "Label value"}
          className="w-44 justify-between font-normal"
        >
          <span className={cn("truncate", !value && "text-muted-foreground")}>
            {value || "value…"}
          </span>
          <ChevronsUpDown className="size-3.5 shrink-0 opacity-50" />
        </Button>
      </PopoverTrigger>
      <PopoverContent className="w-64 p-0" align="start">
        <Command>
          <CommandInput placeholder="Search values…" />
          <CommandList>
            <CommandEmpty>No values found.</CommandEmpty>
            <CommandGroup>
              {options.map((option) => (
                <CommandItem
                  key={option}
                  value={option}
                  data-checked={option === value}
                  onSelect={() => {
                    onChange(option);
                    setOpen(false);
                  }}
                >
                  <span className="truncate font-mono text-xs">{option}</span>
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
        {note ? <p className="border-t px-2 py-1.5 text-[11px] text-muted-foreground">{note}</p> : null}
      </PopoverContent>
    </Popover>
  );
}

/** One matcher row: label, operator, value. */
function FilterRow({ filter, index }: { filter: LabelFilter; index: number }) {
  const { filters, labelNames } = useDashboard();
  const names = filter.label && !labelNames.includes(filter.label)
    ? [filter.label, ...labelNames]
    : labelNames;

  const update = (patch: Partial<LabelFilter>) => {
    const next = filters.map((row, i) => (i === index ? { ...row, ...patch } : row));
    dashboardStore.setFilters(next);
  };

  const remove = () => {
    dashboardStore.setFilters(filters.filter((_, i) => i !== index));
  };

  if (filter.raw !== undefined) {
    // Pasted matcher: show it, let it apply, but do not pretend it is editable
    // with dropdowns it cannot round-trip through.
    return (
      <span className="flex items-center gap-1 rounded-md border border-dashed bg-secondary/40 py-0.5 pr-0.5 pl-2 filter-row">
        <span
          className="font-mono text-xs text-muted-foreground filter-raw"
          title="Selector expression kept as written"
        >
          {filter.raw}
        </span>
        <Button
          variant="ghost"
          size="icon-xs"
          data-role="remove"
          onClick={remove}
          title="Remove this filter"
          aria-label="Remove this filter"
        >
          <X />
        </Button>
      </span>
    );
  }

  return (
    <span className="flex items-center gap-1 rounded-md border bg-secondary/40 p-0.5 filter-row">
      <Select
        value={filter.label ?? ""}
        onValueChange={(value) => update({ label: value, value: "" })}
      >
        <SelectTrigger
          size="sm"
          data-role="label"
          className="h-7 w-40 border-0 bg-transparent shadow-none"
          aria-label="Label name"
        >
          <SelectValue placeholder="label" />
        </SelectTrigger>
        <SelectContent>
          {names.length ? (
            names.map((name) => (
              <SelectItem key={name} value={name}>
                {name}
              </SelectItem>
            ))
          ) : (
            <SelectItem value="service_name">service_name</SelectItem>
          )}
        </SelectContent>
      </Select>

      <Select value={filter.op ?? "="} onValueChange={(value) => update({ op: value })}>
        <SelectTrigger
          size="sm"
          data-role="op"
          className="h-7 w-28 border-0 bg-transparent shadow-none"
          aria-label="Operator"
        >
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {OPS.map(([value, label]) => (
            <SelectItem key={value} value={value}>
              {label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>

      <ValueCombobox
        label={filter.label}
        value={filter.value}
        onChange={(value) => update({ value })}
      />

      <Button
        variant="ghost"
        size="icon-xs"
        data-role="remove"
        onClick={remove}
        title="Remove this filter"
        aria-label="Remove this filter"
      >
        <X />
      </Button>
    </span>
  );
}

export function FilterBar() {
  const { filters, labelNames } = useDashboard();
  const selector = composeSelector(filters);

  const addFilter = () => {
    const used = new Set(filters.map((filter) => filter.label));
    const name = labelNames.find((candidate) => !used.has(candidate)) ?? labelNames[0] ?? "service_name";
    dashboardStore.setFilters([...filters, { label: name, op: "=", value: "" }]);
  };

  return (
    <div
      className="flex flex-wrap items-center gap-2 border-b px-3 py-1.5"
      id="filters"
      data-selector={selector}
    >
      <Tooltip>
        <TooltipTrigger asChild>
          <Button variant="ghost" size="xs" className="filter-add" onClick={addFilter}>
            <Plus />
            Filter
          </Button>
        </TooltipTrigger>
        <TooltipContent>Add a label matcher to the selector</TooltipContent>
      </Tooltip>

      {filters.map((filter, index) => (
        <FilterRow key={`${index}-${filter.label ?? filter.raw ?? ""}`} filter={filter} index={index} />
      ))}

      {selector ? (
        <code
          className="ml-auto truncate font-mono text-[11px] text-muted-foreground"
          data-testid="selector-preview"
        >
          {selector}
        </code>
      ) : null}
    </div>
  );
}
