"use client";

import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { formatBytes, formatCount, formatLabels, formatRelative, formatTimestamp, formatValue } from "@/lib/format";
import { useDashboard } from "@/lib/use-dashboard";

function formatUptime(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return "–";
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (days) return `${days}d ${hours}h`;
  if (hours) return `${hours}h ${minutes}m`;
  return `${minutes}m`;
}

/** `1 report ingested` / `2 reports ingested`, without an "s" on the first. */
function plural(count: number, one: string, many: string) {
  return `${formatCount(count)} ${count === 1 ? one : many}`;
}

export function OverviewView() {
  const { overview } = useDashboard();
  if (!overview) return <Skeleton className="h-[420px] w-full rounded-lg" />;

  const { stats, types, series } = overview;
  const store = stats.store ?? {};

  const cards: Array<{ k: string; v: string; s: string }> = [
    {
      k: "Uptime",
      v: formatUptime(stats.uptime_seconds ?? 0),
      s: `version ${stats.version ?? "?"}`,
    },
    { k: "Batches", v: formatCount(store.segment_count ?? 0), s: formatBytes(store.total_bytes ?? 0) },
    {
      k: "Series",
      v: formatCount(stats.series ?? 0),
      s: plural(stats.ingest_reports ?? 0, "report ingested", "reports ingested"),
    },
    {
      k: "Samples ingested",
      v: formatCount(stats.ingest_samples ?? 0),
      s: plural(stats.queries ?? 0, "query served", "queries served"),
    },
    {
      k: "Oldest data",
      v: store.earliest_ns ? formatRelative(store.earliest_ns) : "–",
      s: store.latest_ns ? `newest ${formatRelative(store.latest_ns)}` : "",
    },
  ];

  return (
    <div className="space-y-3">
      <div className="grid gap-2 sm:grid-cols-2 lg:grid-cols-5">
        {cards.map((card) => (
          <Card key={card.k} data-testid="overview-card" className="gap-1 py-3">
            <CardHeader className="px-3">
              <CardTitle className="text-[11px] font-normal tracking-wide text-muted-foreground uppercase">
                {card.k}
              </CardTitle>
            </CardHeader>
            <CardContent className="px-3">
              <div className="font-mono text-lg leading-tight">{card.v}</div>
              <div className="mt-0.5 text-[11px] text-muted-foreground">{card.s}</div>
            </CardContent>
          </Card>
        ))}
      </div>

      {types.length ? (
        <div className="grid gap-2 sm:grid-cols-2 lg:grid-cols-4">
          {types.map((type) => (
            <Card key={type.name} className="gap-1 py-3">
              <CardHeader className="px-3">
                <CardTitle className="flex items-center gap-1.5 text-[11px] font-normal tracking-wide text-muted-foreground uppercase">
                  {type.name}
                  {type.virtual ? (
                    <Badge variant="secondary" className="normal-case tracking-normal">
                      virtual
                    </Badge>
                  ) : null}
                </CardTitle>
              </CardHeader>
              <CardContent className="px-3">
                <div className="font-mono text-lg leading-tight">
                  {formatValue(type.total_value, type.unit)}
                </div>
                <div className="mt-0.5 text-[11px] text-muted-foreground">
                  {type.segment_count} batches · {type.unit}
                </div>
              </CardContent>
            </Card>
          ))}
        </div>
      ) : null}

      <div className="rounded-lg border">
        <div className="flex items-center gap-2 border-b px-3 py-2">
          <h2 className="text-xs font-medium">Series</h2>
          <span className="flex-1" />
          <span className="text-[11px] text-faint">
            {series.length} combinations in the selected range
          </span>
        </div>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Service</TableHead>
              <TableHead>Profile</TableHead>
              <TableHead>Labels</TableHead>
              <TableHead className="text-right">Value</TableHead>
              <TableHead className="text-right">Batches</TableHead>
              <TableHead>Range</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {series.map((row, index) => (
              <TableRow key={`${row.service_name}-${row.profile_type}-${index}`}>
                <TableCell className="text-xs">{row.service_name}</TableCell>
                <TableCell className="font-mono text-xs">{row.profile_type}</TableCell>
                <TableCell className="max-w-[280px] truncate font-mono text-[11px] text-muted-foreground">
                  {formatLabels(row.labels) || "–"}
                </TableCell>
                <TableCell className="text-right font-mono text-xs">
                  {formatCount(row.total_value)}
                </TableCell>
                <TableCell className="text-right font-mono text-xs">
                  {formatCount(row.segment_count)}
                </TableCell>
                <TableCell className="text-[11px] whitespace-nowrap text-muted-foreground">
                  {formatTimestamp(row.start_ns)} → {formatTimestamp(row.end_ns)}
                </TableCell>
              </TableRow>
            ))}
            {!series.length ? (
              <TableRow>
                <TableCell colSpan={6} className="h-24 text-center text-xs text-muted-foreground">
                  No series yet.
                </TableCell>
              </TableRow>
            ) : null}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}
