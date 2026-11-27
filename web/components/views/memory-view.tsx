"use client";

import { Fragment, useCallback, useEffect, useState } from "react";
import { RefreshCw } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { api } from "@/lib/api";
import { formatBytes, formatRelative } from "@/lib/format";
import { iso } from "@/lib/time";
import type {
  CapabilityPayload,
  MemoryCoveragePayload,
  MemoryPoint,
  MemoryProcessRow,
} from "@/lib/types";
import { useDashboard } from "@/lib/use-dashboard";

/**
 * What the native allocation numbers rest on.
 *
 * A page that shows "12 GB allocated" and nothing else is asking to be trusted.
 * This one shows the bytes next to the evidence: how many batches arrived, what
 * interval the reader sampled at, which reader produced it, whether the workload
 * is still switched on, and when the last sample landed. `coverage` — how many
 * processes were seen, injected, or skipped and why — lives in the agent today
 * and is shown as "not reported yet" rather than guessed.
 */
export function MemoryView() {
  const { dataRange } = useDashboard();
  const [data, setData] = useState<MemoryCoveragePayload | null>(null);
  // Reader states come from the agent, not from the switchboard: a workload
  // can be switched on and still have no reader attached, and the page has to
  // say which of those it is looking at.
  const [capabilities, setCapabilities] = useState<CapabilityPayload | null>(null);
  // The trend is behind a click: most of the time the latest numbers are all
  // that is needed, and a chart per row would turn a scan into a read.
  const [open, setOpen] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [nonce, setNonce] = useState(0);

  const capabilityFor = useCallback(
    (workload: string) => {
      const row = capabilities?.capabilities.find(
        (entry) => entry.workload === workload && entry.reader === "native_alloc",
      );
      if (!row?.state) return "unknown";
      // Pods first, then processes: a reader that reached two of three pods is
      // a different problem from one that reached every pod but half of the
      // processes inside them.
      const pods = `${row.pods_active ?? 0}/${row.pods_reported ?? 0} Pods`;
      const procs = `${row.attached ?? 0}/${row.detected ?? 0} Processes`;
      const reason = row.reason ? ` · ${row.reason}` : "";
      return `${row.state} · ${pods} · ${procs}${reason}`;
    },
    [capabilities],
  );
  const from = dataRange?.from;
  const until = dataRange?.until;

  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    const params = from && until ? { from: iso(from), until: iso(until) } : {};
    api
      .capabilities(controller.signal)
      .then((payload) => {
        if (!controller.signal.aborted) setCapabilities(payload);
      })
      .catch(() => undefined);
    api
      .memoryCoverage(params, controller.signal)
      .then((payload) => {
        if (controller.signal.aborted) return;
        setData(payload);
        setError(null);
      })
      .catch((cause: unknown) => {
        if (controller.signal.aborted) return;
        setError(cause instanceof Error ? cause.message : String(cause));
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
  }, [from, until, nonce]);

  const refresh = useCallback(() => setNonce((value) => value + 1), []);
  const rows = data?.workloads ?? [];

  return (
    <div className="flex min-h-0 flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-col gap-0.5">
          <h2 className="text-sm font-medium">Native memory</h2>
          <p className="text-[11px] text-muted-foreground">
            Bytes, batches and sampling interval for every workload that reported
            an allocation profile. Bytes are an estimate from the reader's sampling
            interval, so compare workloads and stacks rather than treating them as
            exact counters.
          </p>
        </div>
        <Button variant="outline" size="sm" onClick={refresh} disabled={loading}>
          <RefreshCw className="size-3.5" />
          {loading ? "Loading" : "Refresh"}
        </Button>
      </div>

      {error ? (
        <p className="text-xs text-destructive" data-testid="memory-error">
          {error}
        </p>
      ) : null}

      {/*
        Three layers, one page: L1 (what the reader says was allocated) is the
        table; L0 (what the processes actually hold) is behind a click; L2
        (allocator-native profiling) is not implemented and says so rather than
        pretending an empty table means zero.
      */}
      <p className="text-[11px] text-muted-foreground" data-testid="memory-summary">
        {rows.length} workload{rows.length === 1 ? "" : "s"} ·{" "}
        {capabilities?.capabilities.reduce((sum, entry) => sum + (entry.pods_reported ?? 0), 0) ?? 0} pods ·{" "}
        {capabilities?.capabilities.reduce((sum, entry) => sum + (entry.attached ?? 0), 0) ?? 0} processes ·{" "}
        {capabilities?.capabilities.filter((entry) => entry.state === "active").length ?? 0} readers active
      </p>

      <h3 className="text-xs font-medium text-muted-foreground">Native allocation (L1)</h3>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Workload</TableHead>
            <TableHead className="text-right">Requested</TableHead>
            <TableHead className="text-right">Batches</TableHead>
            <TableHead>Sampling</TableHead>
            <TableHead>Reader</TableHead>
            <TableHead>Semantics</TableHead>
            <TableHead>Capability</TableHead>
            <TableHead>Coverage</TableHead>
            <TableHead className="text-right">RSS</TableHead>
            <TableHead className="text-right">PSS</TableHead>
            <TableHead className="text-right">Pod cgroup</TableHead>
            <TableHead>State</TableHead>
            <TableHead className="text-right">Last sample</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((row) => (
            <Fragment key={row.workload}>
              <TableRow
                data-testid="memory-row"
                className={row.history?.length ? "cursor-pointer" : undefined}
                onClick={() =>
                  setOpen(open === row.workload ? null : row.workload)
                }
              >
                <TableCell className="font-mono text-xs">{row.workload}</TableCell>
              <TableCell className="text-right tabular-nums">
                {formatBytes(row.requested_alloc_bytes)}
              </TableCell>
              <TableCell className="text-right tabular-nums">{row.batches}</TableCell>
              <TableCell className="text-xs text-muted-foreground">
                {row.sampling_label ?? "unknown"}
              </TableCell>
              <TableCell className="text-xs text-muted-foreground">
                {row.reader ?? "runtime"}
              </TableCell>
              <TableCell className="text-xs text-muted-foreground">
                {row.reader === "bpftime-userspace" ? "delta (this window)" : "cumulative (since start)"}
              </TableCell>
              <TableCell className="text-xs text-muted-foreground">
                {capabilityFor(row.workload)}
              </TableCell>
              <TableCell className="text-xs text-muted-foreground">
                {row.coverage
                  ? `${row.coverage.attached}/${row.coverage.detected} processes${
                      Object.entries(row.coverage.skipped ?? {})
                        .map(([reason, count]) => `, ${count} ${reason}`)
                        .join("") || ""
                    }`
                  : "not reported"}
              </TableCell>
              <TableCell className="text-right tabular-nums">
                {row.coverage?.process_rss_bytes
                  ? formatBytes(row.coverage.process_rss_bytes)
                  : "–"}
              </TableCell>
              <TableCell className="text-right tabular-nums">
                {row.coverage?.process_pss_bytes
                  ? formatBytes(row.coverage.process_pss_bytes)
                  : "–"}
              </TableCell>
              <TableCell className="text-right tabular-nums">
                {row.coverage?.memory_bytes ? formatBytes(row.coverage.memory_bytes) : "–"}
              </TableCell>
              <TableCell className="text-xs">
                {row.enabled ? "reader on" : "reader off"}
              </TableCell>
              <TableCell className="text-right text-xs text-muted-foreground">
                {formatRelative(row.last_seen_ns)}
              </TableCell>
              </TableRow>
              {open === row.workload ? (
                <TableRow data-testid="memory-detail">
                  <TableCell colSpan={11} className="bg-muted/30">
                    <h4 className="mb-1 text-[11px] font-medium text-muted-foreground">
                      Process memory (L0)
                    </h4>
                    {row.history?.length ? <MemoryTrend points={row.history} /> : null}
                    <ProcessTable workload={row.workload} />
                  </TableCell>
                </TableRow>
              ) : null}
            </Fragment>
          ))}
          {!rows.length && !loading ? (
            <TableRow>
              <TableCell colSpan={11} className="text-xs text-muted-foreground">
                No allocation profile in this window. Switch a workload's native
                reader on from the service map, or widen the range.
              </TableCell>
            </TableRow>
          ) : null}
        </TableBody>
      </Table>

      {data?.workloads.length ? (
        <p className="text-[11px] text-muted-foreground" data-testid="memory-coverage-note">
          Coverage is reported by the reader inside the target pod: the dynamic
          processes it found, how many it is attached to, and why it left the
          rest alone. A workload that is switched on but attached to nothing
          shows up here instead of hiding behind a plausible number.
        </p>
      ) : null}
      <div className="rounded-md border border-dashed p-3" data-testid="memory-l2">
        <h3 className="text-xs font-medium">Allocator native profiling (L2)</h3>
        <p className="mt-1 text-[11px] text-muted-foreground">
          jemalloc and tcmalloc keep their own profiles behind their own stats
          endpoints. This build does not read them yet, so the layer is empty on
          purpose — an empty table here would otherwise look like zero allocations.
        </p>
      </div>
    </div>
  );
}
/**
 * The three memory numbers over time.
 *
 * One axis for all three: RSS, PSS and the pod's cgroup total are the same
 * quantity measured at different scopes, so separate axes would invite a
 * comparison that does not hold. The scale is the peak of the window, not the
 * last point — a line that just doubled must not look flat.
 */
function MemoryTrend({ points }: { points: MemoryPoint[] }) {
  if (points.length < 2) {
    return <p className="text-xs text-muted-foreground">Not enough samples yet.</p>;
  }
  const peak = Math.max(
    ...points.map((point) => Math.max(point.rss_bytes, point.pss_bytes, point.cgroup_bytes)),
    1,
  );
  const line = (pick: (point: MemoryPoint) => number) =>
    points
      .map((point, index) => {
        const x = (index / (points.length - 1)) * 100;
        const y = 100 - (pick(point) / peak) * 100;
        return `${x.toFixed(2)},${y.toFixed(2)}`;
      })
      .join(" ");
  const last = points[points.length - 1];
  return (
    <div className="mt-2 space-y-1">
      <svg viewBox="0 0 100 100" preserveAspectRatio="none" className="h-20 w-full">
        <polyline points={line((point) => point.cgroup_bytes)} fill="none" stroke="currentColor" strokeOpacity="0.35" strokeWidth="0.6" />
        <polyline points={line((point) => point.rss_bytes)} fill="none" stroke="currentColor" strokeWidth="1" />
        <polyline points={line((point) => point.pss_bytes)} fill="none" stroke="currentColor" strokeOpacity="0.6" strokeDasharray="2 1" strokeWidth="0.8" />
      </svg>
      <p className="text-[11px] text-muted-foreground">
        RSS {formatBytes(last.rss_bytes)} · PSS {formatBytes(last.pss_bytes)} · cgroup {formatBytes(last.cgroup_bytes)} ·{" "}
        {points.length} points · peak {formatBytes(peak)} · solid RSS, dashed PSS, faint cgroup
      </p>
    </div>
  );
}

/**
 * The processes behind a pod's memory number.
 *
 * The cgroup total says how much the pod uses; only `/proc` can say which
 * process is growing. Rows without PSS carry the reason instead of a zero.
 */
function ProcessTable({ workload }: { workload: string }) {
  const [rows, setRows] = useState<MemoryProcessRow[] | null>(null);
  const [note, setNote] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    api
      .memoryProcesses({ workload, limit: "10" }, controller.signal)
      .then((payload) => {
        if (controller.signal.aborted) return;
        setRows(payload.processes ?? []);
        setNote(payload.omitted ? `${payload.omitted} smaller processes not listed` : null);
      })
      .catch((cause: unknown) => {
        if (controller.signal.aborted) return;
        setNote(cause instanceof Error ? cause.message : String(cause));
      });
    return () => controller.abort();
  }, [workload]);

  if (rows === null) {
    return <p className="mt-3 text-xs text-muted-foreground">Loading processes…</p>;
  }
  if (!rows.length) {
    return (
      <p className="mt-3 text-xs text-muted-foreground">
        This workload&apos;s pods have not reported per-process memory yet.
      </p>
    );
  }
  return (
    <div className="mt-3 space-y-1" data-testid="memory-processes">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>PID</TableHead>
            <TableHead>Process</TableHead>
            <TableHead className="text-right">RSS</TableHead>
            <TableHead className="text-right">PSS</TableHead>
            <TableHead className="text-right">Private dirty</TableHead>
            <TableHead className="text-right">Swap</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((process) => (
            <TableRow key={`${process.pid}-${process.start_ns ?? 0}`}>
              <TableCell className="text-xs tabular-nums">{process.pid}</TableCell>
              <TableCell className="text-xs">
                {process.comm}
                <span className="ml-2 text-muted-foreground">{process.executable}</span>
              </TableCell>
              <TableCell className="text-right text-xs tabular-nums">
                {formatBytes(process.rss_bytes ?? 0)}
              </TableCell>
              <TableCell className="text-right text-xs tabular-nums">
                {process.pss_bytes == null ? (process.missing ?? "—") : formatBytes(process.pss_bytes)}
              </TableCell>
              <TableCell className="text-right text-xs tabular-nums">
                {process.private_dirty_bytes == null
                  ? (process.missing ?? "—")
                  : formatBytes(process.private_dirty_bytes)}
              </TableCell>
              <TableCell className="text-right text-xs tabular-nums">
                {process.swap_bytes == null ? "—" : formatBytes(process.swap_bytes)}
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
      {note ? <p className="text-xs text-muted-foreground">{note}</p> : null}
    </div>
  );
}