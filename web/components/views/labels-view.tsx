"use client";

import { useState } from "react";
import { Search } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { formatCount } from "@/lib/format";
import { dashboardStore } from "@/lib/store";
import { useDashboard } from "@/lib/use-dashboard";
import { cn } from "@/lib/utils";

/**
 * Tag explorer: the label names on the left, the values of the selected label
 * on the right. Clicking a value narrows the selector, which is how you go from
 * "something is slow" to "it is slow on this pod".
 */
export function LabelsView() {
  const { explorerNames, activeLabel, explorerValues, loading } = useDashboard();
  const [search, setSearch] = useState("");

  const needle = search.trim().toLowerCase();
  const filtered = needle
    ? explorerValues.filter((row) => row.value.toLowerCase().includes(needle))
    : explorerValues;
  const max = filtered.reduce((acc, row) => Math.max(acc, row.total ?? 0), 0) || 1;

  if (!explorerNames.length && loading) {
    return <Skeleton className="h-[420px] w-full rounded-lg" />;
  }

  return (
    <div className="flex min-h-0 flex-col gap-2">
      <div className="flex flex-wrap items-center gap-2">
        <div className="relative">
          <Search className="pointer-events-none absolute top-1/2 left-2 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            type="search"
            placeholder="Filter values…"
            aria-label="Filter label values"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            className="h-7 w-56 pl-7 text-xs"
          />
        </div>
        <span className="text-[11px] text-muted-foreground" data-testid="label-count">
          {activeLabel
            ? `${filtered.length} value${filtered.length === 1 ? "" : "s"} for ${activeLabel}`
            : ""}
        </span>
        <span className="flex-1" />
        <span className="text-[11px] text-faint">Click a value to add it to the selector.</span>
      </div>

      <div className="grid min-h-0 gap-2 md:grid-cols-[220px_1fr]">
        <ScrollArea className="label-names h-[420px] rounded-lg border">
          <div className="flex flex-col p-1">
            {explorerNames.map((name) => (
              <button
                key={name}
                type="button"
                data-name={name}
                onClick={() => void dashboardStore.selectLabel(name)}
                className={cn(
                  "truncate rounded-sm px-2 py-1 text-left font-mono text-xs transition-colors hover:bg-accent",
                  name === activeLabel ? "bg-accent text-foreground" : "text-muted-foreground",
                )}
              >
                {name}
              </button>
            ))}
            {!explorerNames.length ? (
              <p className="p-2 text-[11px] text-muted-foreground">No labels in this window.</p>
            ) : null}
          </div>
        </ScrollArea>

        <div className="label-values rounded-lg border">
          <Table className="label-table">
            <TableHeader>
              <TableRow>
                <TableHead>Value</TableHead>
                <TableHead className="w-[30%]" />
                <TableHead className="text-right">Total</TableHead>
                <TableHead className="w-20" />
              </TableRow>
            </TableHeader>
            <TableBody>
              {filtered.map((row) => (
                <TableRow key={row.value}>
                  <TableCell className="max-w-0">
                    <div className="truncate font-mono text-xs" title={row.value}>
                      {row.value}
                    </div>
                  </TableCell>
                  <TableCell>
                    <div className="h-2 w-full overflow-hidden rounded-sm bg-secondary">
                      <div
                        className="h-full rounded-sm bg-primary/70"
                        style={{
                          width: `${Math.max(1, Math.round(((row.total ?? 0) / max) * 100))}%`,
                        }}
                      />
                    </div>
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs">
                    {formatCount(row.total ?? 0)}
                  </TableCell>
                  <TableCell className="text-right">
                    <Button
                      variant="ghost"
                      size="xs"
                      data-value={row.value}
                      onClick={() => {
                        if (!activeLabel) return;
                        dashboardStore.filterByLabel(activeLabel, row.value);
                      }}
                    >
                      Filter
                    </Button>
                  </TableCell>
                </TableRow>
              ))}
              {!filtered.length ? (
                <TableRow>
                  <TableCell colSpan={4} className="h-24 text-center text-xs text-muted-foreground">
                    {activeLabel ? "No values." : "No labels in this window"}
                  </TableCell>
                </TableRow>
              ) : null}
            </TableBody>
          </Table>
        </div>
      </div>
    </div>
  );
}
