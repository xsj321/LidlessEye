"use client";

import { useMemo, useState } from "react";
import { ArrowDown, ArrowUp, ArrowUpDown, Search } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { formatPercent, formatValue } from "@/lib/format";
import type { TopItem, TopPayload } from "@/lib/types";
import { useDashboard } from "@/lib/use-dashboard";
import { cn } from "@/lib/utils";

type SortKey = "name" | "self_value" | "total_value";

function SortHeader({
  label,
  sortKey,
  active,
  ascending,
  onSort,
  className,
}: {
  label: string;
  sortKey: SortKey;
  active: SortKey;
  ascending: boolean;
  onSort: (key: SortKey) => void;
  className?: string;
}) {
  const isActive = active === sortKey;
  const Icon = !isActive ? ArrowUpDown : ascending ? ArrowUp : ArrowDown;
  return (
    <TableHead className={className} aria-sort={isActive ? (ascending ? "ascending" : "descending") : "none"}>
      <button
        type="button"
        onClick={() => onSort(sortKey)}
        className={cn(
          "inline-flex items-center gap-1 rounded-sm px-1 py-0.5 transition-colors hover:text-foreground",
          isActive ? "text-foreground" : "text-muted-foreground",
        )}
      >
        {label}
        <Icon className="size-3 opacity-60" />
      </button>
    </TableHead>
  );
}

function rowsOf(items: TopItem[], filter: string, sortKey: SortKey, ascending: boolean) {
  const needle = filter.trim().toLowerCase();
  const filtered = needle
    ? items.filter(
        (row) =>
          row.name.toLowerCase().includes(needle) ||
          (row.module ?? "").toLowerCase().includes(needle) ||
          (row.file ?? "").toLowerCase().includes(needle),
      )
    : items.slice();

  filtered.sort((a, b) => {
    if (sortKey === "name") {
      const cmp = a.name.localeCompare(b.name);
      return ascending ? cmp : -cmp;
    }
    const cmp = (a[sortKey] ?? 0) - (b[sortKey] ?? 0);
    return ascending ? cmp : -cmp;
  });
  return filtered;
}

/** Sortable, filterable "top functions" table. */
export function TopView() {
  const { top, empty } = useDashboard();
  const [filter, setFilter] = useState("");
  const [sortKey, setSortKey] = useState<SortKey>("self_value");
  const [ascending, setAscending] = useState(false);

  if (empty) return <EmptyState info={empty} />;
  if (!top) return <Skeleton className="h-[420px] w-full rounded-lg" />;

  return (
    <TopTable
      payload={top}
      filter={filter}
      setFilter={setFilter}
      sortKey={sortKey}
      ascending={ascending}
      onSort={(key) => {
        if (key === sortKey) setAscending((value) => !value);
        else {
          setSortKey(key);
          setAscending(key === "name");
        }
      }}
    />
  );
}

function TopTable({
  payload,
  filter,
  setFilter,
  sortKey,
  ascending,
  onSort,
}: {
  payload: TopPayload;
  filter: string;
  setFilter: (value: string) => void;
  sortKey: SortKey;
  ascending: boolean;
  onSort: (key: SortKey) => void;
}) {
  const items = payload.items;
  const rows = useMemo(
    () => rowsOf(items, filter, sortKey, ascending),
    [items, filter, sortKey, ascending],
  );
  const maxSelf = rows.reduce((acc, row) => Math.max(acc, row.self_value ?? 0), 0) || 1;
  const total =
    payload.total_value || items.reduce((acc, row) => acc + (row.self_value ?? 0), 0) || 1;

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-2">
        <div className="relative">
          <Search className="pointer-events-none absolute top-1/2 left-2 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            type="search"
            placeholder="Filter functions…"
            aria-label="Filter functions"
            value={filter}
            onChange={(event) => setFilter(event.target.value)}
            className="h-7 w-64 pl-7 text-xs"
          />
        </div>
        <span className="text-[11px] text-muted-foreground" data-testid="top-count">
          {rows.length} of {items.length} functions
        </span>
        <span className="flex-1" />
        <span className="text-[11px] text-faint">Click a column header to sort.</span>
      </div>

      <div className="rounded-lg border tbl-wrap">
        <Table>
          <TableHeader>
            <TableRow>
              <SortHeader label="Function" sortKey="name" active={sortKey} ascending={ascending} onSort={onSort} />
              <SortHeader label="Self" sortKey="self_value" active={sortKey} ascending={ascending} onSort={onSort} className="text-right" />
              <TableHead className="w-[22%]" aria-label="Share of the hottest function" />
              <SortHeader label="Total" sortKey="total_value" active={sortKey} ascending={ascending} onSort={onSort} className="text-right" />
              <TableHead className="text-right">Total %</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((row) => {
              const location = [row.module, row.file && row.line ? `${row.file}:${row.line}` : row.file]
                .filter(Boolean)
                .join(" · ");
              const width = Math.max(1, Math.round(((row.self_value ?? 0) / maxSelf) * 100));
              return (
                <TableRow key={`${row.name}-${location}`}>
                  <TableCell className="max-w-0">
                    <div className="truncate font-mono text-xs" title={location || row.name}>
                      {row.name}
                    </div>
                    {location ? (
                      <div className="truncate text-[11px] text-muted-foreground">{location}</div>
                    ) : null}
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs whitespace-nowrap">
                    {formatValue(row.self_value, payload.unit)}
                  </TableCell>
                  <TableCell>
                    <div className="h-2 w-full overflow-hidden rounded-sm bg-secondary">
                      <div
                        className="h-full rounded-sm bg-primary/70"
                        style={{ width: `${width}%` }}
                      />
                    </div>
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs whitespace-nowrap">
                    {formatValue(row.total_value, payload.unit)}
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs whitespace-nowrap text-muted-foreground">
                    {formatPercent((row.total_value ?? 0) / total)}
                  </TableCell>
                </TableRow>
              );
            })}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}
