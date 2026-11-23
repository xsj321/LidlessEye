"use client";

// The seven tabs of the node panel.
//
// Each tab answers one question an operator asks about a service, and each one
// is built from the measurements that actually exist: the map's own counters
// and latency, the profiles the agent collected, and the pod list from the
// cluster. Where a measurement does not exist, the tab says so and says what
// would produce it, instead of drawing an empty chart — a graph that is always
// flat is worse than an honest gap.

import { ArrowUpRight, Flame, HardDrive, Network } from "lucide-react";
import { useEffect, type ReactNode } from "react";

import { FlamegraphCanvas } from "@/components/canvas/flamegraph-canvas";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { formatBytes, formatCount, formatDuration, formatPercent, formatValue } from "@/lib/format";
import {
  edgeRate,
  formatRate,
  formatRtt,
  nodeLabel,
  profileFilters,
} from "@/lib/servicemap-graph";
import { dashboardStore } from "@/lib/store";
import type {
  LabelFilter,
  ServiceInstance,
  ServiceMapEdge,
  ServiceMapNode,
} from "@/lib/types";
import { cn } from "@/lib/utils";

import { RankingChart, TimeChart } from "./charts";
import {
  isCpuType,
  isMemoryType,
  pickMemoryType,
  toTimePoints,
  totalTimePoints,
  type NodeData,
  type ProfileUnit,
} from "./use-node-data";

export interface TabProps {
  node: ServiceMapNode;
  data: NodeData;
  inbound: ServiceMapEdge[];
  outbound: ServiceMapEdge[];
  windowSec: number;
  nodes: Map<string, ServiceMapNode>;
  onSelect: (id: string) => void;
}

const SLO_TARGET = 0.999;
const ERROR_BUDGET = 1 - SLO_TARGET;

/** A sample-weighted mean, with a one-sample floor for legacy edges. */
function weightedRtt(edges: ServiceMapEdge[]): { rtt: number; samples: number } {
  let weighted = 0;
  let samples = 0;
  for (const edge of edges) {
    if (!edge.rtt_us) continue;
    const weight = Math.max(1, edge.rtt_samples ?? 0);
    weighted += edge.rtt_us * weight;
    samples += weight;
  }
  return { rtt: samples ? weighted / samples : 0, samples };
}

/** A native select: the available types are data, not a tab the user hunts for. */
function ProfileSelect({
  units,
  value,
  label,
  testId,
  filter,
  onChange,
}: {
  units: ProfileUnit[];
  value: string | null;
  label: string;
  testId: string;
  filter: (unit: ProfileUnit) => boolean;
  onChange: (name: string) => void;
}) {
  const options = units.filter(filter);
  const current = value && options.some((unit) => unit.name === value) ? value : options[0]?.name ?? "";
  return (
    <label className="flex items-center justify-end gap-1.5 text-[10px] text-muted-foreground">
      <span>{label}</span>
      <select
        value={current}
        disabled={options.length < 2}
        aria-label={label}
        data-testid={testId}
        onChange={(event) => onChange(event.target.value)}
        className="h-6 max-w-[15rem] rounded border bg-background px-1 font-mono text-[10px] text-foreground outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-60"
      >
        {options.length ? (
          options.map((unit) => (
            <option key={unit.name} value={unit.name}>
              {unit.name} · {unit.unit}
            </option>
          ))
        ) : (
          <option value="">none</option>
        )}
      </select>
    </label>
  );
}

/** Literals are not the query shape; keep the statement, lose the secrets. */
function redactSql(sql: string): string {
  return sql
    .replace(/\s+/g, " ")
    .replace(/'(?:''|[^'])*'/g, "'?'")
    .replace(/\b\d+(?:\.\d+)?\b/g, "?")
    .slice(0, 160);
}

function Section({
  title,
  hint,
  children,
}: {
  title: string;
  hint?: string;
  children: ReactNode;
}) {
  return (
    <div className="space-y-1.5">
      <div className="flex items-baseline justify-between gap-2">
        <div className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
          {title}
        </div>
        {hint ? <div className="text-[10px] text-muted-foreground/70">{hint}</div> : null}
      </div>
      {children}
    </div>
  );
}

function Stat({
  label,
  value,
  hint,
  tone,
}: {
  label: string;
  value: ReactNode;
  hint?: string;
  tone?: "ok" | "warn" | "muted";
}) {
  return (
    <div className="rounded-md border bg-background/40 px-2 py-1.5">
      <div className="text-[9px] uppercase tracking-wide text-muted-foreground">{label}</div>
      <div
        className={cn(
          "mt-0.5 truncate font-mono text-[13px] leading-tight",
          tone === "warn" && "text-primary",
          tone === "ok" && "text-[#6fce9b]",
          tone === "muted" && "text-muted-foreground",
        )}
      >
        {value}
      </div>
      {hint ? <div className="mt-0.5 truncate text-[9px] text-muted-foreground/80">{hint}</div> : null}
    </div>
  );
}

function StatRow({ children }: { children: ReactNode }) {
  return <div className="grid grid-cols-3 gap-1.5">{children}</div>;
}

function Note({ children }: { children: ReactNode }) {
  return <p className="text-[10px] leading-relaxed text-muted-foreground/80">{children}</p>;
}

function Missing({ icon, title, hint }: { icon: ReactNode; title: string; hint: string }) {
  return (
    <div
      className="flex flex-col items-center gap-1.5 rounded-md border border-dashed px-4 py-6 text-center"
      data-testid="map-panel-empty"
    >
      <span className="text-muted-foreground/70">{icon}</span>
      <div className="text-[11px] font-medium">{title}</div>
      <p className="max-w-[38ch] text-[10px] leading-relaxed text-muted-foreground">{hint}</p>
    </div>
  );
}

/** One row of a peer list: name, protocol, rate, latency. Clicking walks there. */
function PeerRow({
  peer,
  edge,
  direction,
  windowSec,
  onSelect,
}: {
  peer: ServiceMapNode | undefined;
  edge: ServiceMapEdge;
  direction: "in" | "out";
  windowSec: number;
  onSelect: (id: string) => void;
}) {
  const fallback = direction === "in" ? edge.client : edge.server;
  const peerId = peer?.id ?? fallback;
  const label = peer ? nodeLabel(peer) : fallback;
  const container = direction === "in" ? edge.client_container : edge.server_container;
  const detail = [
    edge.protocol,
    container,
    edge.rtt_samples ? `${formatCount(edge.rtt_samples)} RTT samples` : null,
  ]
    .filter(Boolean)
    .join(" · ");
  return (
    <button
      type="button"
      onClick={() => onSelect(peerId)}
      className="flex w-full items-center gap-2 rounded px-1 py-0.5 text-left text-[11px] hover:bg-secondary/60 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      data-testid="map-peer-row"
      title={`${detail || "no protocol"} · ${formatCount(edge.connections)} connections · ${formatBytes(edge.tx_bytes ?? 0)}`}
    >
      <span className="min-w-0 flex-1">
        <span className="block truncate">{label}</span>
        {detail ? (
          <span className="block truncate text-[9px] text-muted-foreground">{detail}</span>
        ) : null}
      </span>
      <span className="w-10 shrink-0 text-[9px] uppercase text-muted-foreground">{edge.protocol}</span>
      <span className="w-12 shrink-0 text-right font-mono">{formatRate(edgeRate(edge, windowSec))}</span>
      <span className="w-12 shrink-0 text-right font-mono text-[10px] text-muted-foreground">
        {formatRtt(edge.rtt_us)}
      </span>
    </button>
  );
}

// ---- SLO ------------------------------------------------------------------

export function SloTab({ node, data, inbound, outbound, windowSec, nodes, onSelect }: TabProps) {
  const connections = node.connections ?? 0;
  const errors = node.retransmits ?? 0;
  const total = connections + errors;
  const successRate = total ? 1 - errors / total : 1;
  const budget = Math.max(0, Math.min(1, 1 - (1 - successRate) / ERROR_BUDGET));

  // Error rate, not availability: availability sits between 99.5% and 100%,
  // which is a flat line against a y axis that has to start at zero. Error rate
  // puts the interesting variation on screen and makes the objective a
  // threshold the reader can see the line approach.
  const points: Array<[number, number]> = (data.history?.points ?? [])
    .filter((point) => (point.connections ?? 0) + (point.retransmits ?? 0) > 0)
    .map((point) => {
      const ok = point.connections ?? 0;
      const bad = point.retransmits ?? 0;
      return [point.bucket_ns / 1e6, ok + bad ? (bad / (ok + bad)) * 100 : 0];
    });

  const latency = weightedRtt([...inbound, ...outbound]);

  return (
    <div className="space-y-3">
      <StatRow>
        <Stat
          label="Success proxy"
          value={successRate >= 0.99995 ? "100%" : formatPercent(successRate, 2)}
          hint={`target ${formatPercent(SLO_TARGET, 1)}`}
          tone={successRate < SLO_TARGET ? "warn" : "ok"}
        />
        <Stat
          label="Budget left"
          value={formatPercent(budget, 0)}
          hint={`${formatCount(errors)} retx in ${formatCount(total)} conns`}
          tone={budget < 0.2 ? "warn" : "ok"}
        />
        <Stat
          label="Mean RTT"
          value={latency.rtt ? formatRtt(latency.rtt) : "—"}
          hint={latency.rtt ? `${formatCount(latency.samples)} closed connections` : "no closed connection this window"}
        />
      </StatRow>

      <Section title="Error rate over the window" hint="retransmits / connections">
        {points.length ? (
          <TimeChart
            series={[{ name: "error rate", color: "#e05c5c", points, area: true }]}
            format={(value) => `${value.toFixed(2)}%`}
            target={{
              value: ERROR_BUDGET * 100,
              label: `budget ${formatPercent(ERROR_BUDGET, 2)}`,
            }}
            description={`Retransmit ratio per bucket. The dotted line is the ${formatPercent(ERROR_BUDGET, 2)} error budget implied by a ${formatPercent(SLO_TARGET, 1)} success objective.`}
            testId="map-chart-slo"
          />
        ) : (
          <Note>Not enough buckets with traffic to draw a line.</Note>
        )}
      </Section>

      <Section title={`Callers · ${inbound.length}`}>
        {inbound.length ? (
          <div className="space-y-0.5">
            {inbound.slice(0, 6).map((edge) => (
              <PeerRow
                key={`in:${edge.client}:${edge.protocol}`}
                peer={nodes.get(edge.client)}
                edge={edge}
                direction="in"
                windowSec={windowSec}
                onSelect={onSelect}
              />
            ))}
          </div>
        ) : (
          <Note>Nothing calls this node in the selected window.</Note>
        )}
      </Section>

      <Section title={`Callees · ${outbound.length}`}>
        {outbound.length ? (
          <div className="space-y-0.5">
            {outbound.slice(0, 6).map((edge) => (
              <PeerRow
                key={`out:${edge.server}:${edge.protocol}`}
                peer={nodes.get(edge.server)}
                edge={edge}
                direction="out"
                windowSec={windowSec}
                onSelect={onSelect}
              />
            ))}
          </div>
        ) : (
          <Note>This node opens no connections in the selected window.</Note>
        )}
      </Section>

      <Note>
        This objective is derived from TCP: a retransmit stands in for a failed request, because no L7
        status codes are collected. Mean RTT is weighted by closed-connection samples. Set the target
        against your own dashboard when you have one.
      </Note>
    </div>
  );
}

// ---- Instances ------------------------------------------------------------

function InstanceRow({
  instance,
  node,
  profileType,
}: {
  instance: ServiceInstance;
  node: ServiceMapNode;
  profileType: string | null;
}) {
  const openPod = () => {
    const filters: LabelFilter[] = [...(profileFilters(node) ?? [])];
    if (instance.name) {
      filters.push({ label: "k8s_pod", op: "=", value: instance.name });
    }
    dashboardStore.showProfile(filters, profileType ?? "cpu");
  };
  return (
    <button
      type="button"
      onClick={openPod}
      className="flex w-full items-center gap-2 rounded px-1 py-1 text-left text-[11px] hover:bg-secondary/60 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      data-testid="map-instance-row"
      title={`${instance.name}${instance.uid ? ` · uid ${instance.uid}` : ""} — open this pod's profiles`}
    >
      <span
        aria-hidden="true"
        className="size-1.5 shrink-0 rounded-full"
        style={{ background: instance.ready ? "#4fbf8b" : "#f2a65a" }}
      />
      <span className="min-w-0 flex-1 truncate font-mono text-[10px]">{instance.name}</span>
      <span className="w-14 shrink-0 truncate text-[9px] text-muted-foreground">
        {instance.phase || "—"}
      </span>
      <span className="w-10 shrink-0 text-[9px] text-muted-foreground">
        {instance.ready === undefined ? "—" : instance.ready ? "ready" : "pending"}
      </span>
      <span className="w-24 shrink-0 truncate text-right text-[9px] text-muted-foreground" title={instance.node}>
        {instance.node || "—"}
      </span>
    </button>
  );
}

export function InstanceTab({ node, data, inbound, outbound, windowSec, nodes, onSelect }: TabProps) {
  const pods = node.pods ?? 0;
  const ready = Math.min(node.ready ?? 0, pods);
  const instances = node.instances ?? [];

  const desired: Array<[number, number]> = (data.history?.points ?? []).map((point) => [
    point.bucket_ns / 1e6,
    point.pods ?? 0,
  ]);
  const readyPoints: Array<[number, number]> = (data.history?.points ?? []).map((point) => [
    point.bucket_ns / 1e6,
    point.ready ?? 0,
  ]);

  if (node.kind !== "workload") {
    // A service is an address in front of endpoints; the endpoints are their
    // own workload nodes. Showing "Missing" threw away the only instance
    // information the map actually has.
    const endpoints = [
      ...inbound.map((edge) => ({ edge, peer: nodes.get(edge.client), direction: "in" as const })),
      ...outbound.map((edge) => ({ edge, peer: nodes.get(edge.server), direction: "out" as const })),
    ];
    return (
      <div className="space-y-3">
        <StatRow>
          <Stat label="Kind" value={node.kind ?? "service"} hint="no replicas of its own" />
          <Stat label="Peers" value={endpoints.length || "—"} hint="workloads on this edge" />
          <Stat
            label="Traffic"
            value={formatBytes((node.rx_bytes ?? 0) + (node.tx_bytes ?? 0))}
            hint="in the window"
          />
        </StatRow>
        <Section title="Backing endpoints" hint="click to inspect a pod">
          {endpoints.length ? (
            <div className="space-y-0.5" data-testid="map-instances-endpoints">
              {endpoints.slice(0, 24).map(({ edge, peer, direction }) => (
                <PeerRow
                  key={`${direction}:${edge.client}:${edge.server}:${edge.protocol}`}
                  peer={peer}
                  edge={edge}
                  direction={direction}
                  windowSec={windowSec}
                  onSelect={onSelect}
                />
              ))}
            </div>
          ) : (
            <Note>No workload is connected to this address in the selected window.</Note>
          )}
        </Section>
        <Note>
          A ClusterIP service has endpoints rather than pods of its own. Each endpoint is a workload
          node with its own replica list and profiles.
        </Note>
      </div>
    );
  }

  const shown = instances.slice(0, 24);

  return (
    <div className="space-y-3">
      <StatRow>
        <Stat label="Desired" value={pods || "—"} hint="replicas in the cluster" />
        <Stat
          label="Ready"
          value={pods ? `${ready}/${pods}` : "—"}
          tone={pods && ready < pods ? "warn" : "ok"}
          hint={pods ? formatPercent(ready / pods, 0) : ""}
        />
        <Stat
          label="Not ready"
          value={pods ? Math.max(0, pods - ready) : "—"}
          tone={pods - ready > 0 ? "warn" : "muted"}
        />
      </StatRow>

      <Section title="Replicas over the window" hint="ready vs desired">
        {desired.length ? (
          <TimeChart
            series={[
              { name: "desired", color: "#6ba7f7", points: desired, step: true, dashed: true },
              { name: "ready", color: "#4fbf8b", points: readyPoints, step: true },
            ]}
            format={(value) => String(Math.round(value))}
            yMax={Math.max(pods, ready, 1) + 1}
            height={132}
            description="Desired and ready replica counts per bucket. Both are gauges; the newest observation in a bucket wins."
            testId="map-chart-instances"
          />
        ) : (
          <Note>Only one collection window in range, so there is no line to draw.</Note>
        )}
      </Section>

      <Separator />

      <Section title={`Instances · ${instances.length}`} hint="open a pod's profiles">
        {shown.length ? (
          <div className="space-y-0.5">
            <div className="flex items-center gap-2 px-1 pb-0.5 text-[9px] uppercase tracking-wide text-muted-foreground/70">
              <span className="size-1.5" />
              <span className="flex-1">pod</span>
              <span>phase</span>
              <span>state</span>
              <span className="w-24 text-right">node</span>
            </div>
            {shown.map((instance) => (
              <InstanceRow
                key={instance.uid || instance.name}
                instance={instance}
                node={node}
                profileType={data.profileType}
              />
            ))}
          </div>
        ) : (
          <Note>
            The agent reports counts but no pod names for this workload — it started before instances
            were collected, or the cluster API did not answer. The workload-level filters still
            select every pod's profiles.
          </Note>
        )}
      </Section>
      {instances.length > shown.length ? (
        <Note>
          Showing the first 24 of {instances.length} pods. The counts above cover all replicas.
        </Note>
      ) : null}
    </div>
  );
}

// ---- CPU / memory ---------------------------------------------------------

function profileSeries(
  payload: NodeData["profile"],
  transform: (value: number) => number,
): Array<[number, number]> {
  const points = totalTimePoints(payload);
  if (!points.length) return [];
  return points.map(([clock, value]) => [clock, transform(value)]);
}

/** Cores, with enough significant digits to tell 4 mCPU from 9 mCPU. */
function formatCores(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return "0";
  if (value >= 10) return value.toFixed(1);
  if (value >= 1) return value.toFixed(2);
  if (value >= 0.001) return value.toFixed(3);
  // Below a millicore: two significant digits, so 40 µcores does not read as
  // zero next to a peak that is ten times larger.
  return Number(value.toPrecision(2)).toString();
}

/** A stable dependency string: `data.units` is rebuilt on every render. */
function unitNames(units: ProfileUnit[]): string {
  return units.map((unit) => unit.name).join(",");
}

const SERIES_COLORS = ["#f2a65a", "#6ba7f7", "#4fbf8b", "#b48ef2", "#57c7d4", "#e05c5c", "#d4a5a5", "#9fb4cf"];

export function CpuTab({ node, data }: TabProps) {
  const options = data.units.filter((unit) => isCpuType(unit.name));
  const names = unitNames(options);
  const profileType = data.profileType;

  useEffect(() => {
    if (!options.length) return;
    if (!profileType || !options.some((unit) => unit.name === profileType)) {
      data.setProfileType(options[0].name);
    }
    // `names` carries the options; the object identity does not matter.
  }, [names, profileType]);

  if (!options.length) {
    return (
      <Missing
        icon={<Flame className="size-4" />}
        title="No CPU profiles for this node"
        hint={`Nothing matched ${node.namespace ? `${node.namespace}/` : ""}${node.workload || node.service || node.id} in this window. The map's CPU heat comes from the same profiles.`}
      />
    );
  }

  const step = data.profile?.step_ns ?? 0;
  const nanoseconds =
    options.find((unit) => unit.name === profileType)?.unit === "nanoseconds";
  const toCores = (value: number) => (nanoseconds && step ? value / step : value);
  const cores = profileSeries(data.profile, toCores);
  const totalValue = data.profile?.series.reduce(
    (sum, row) => sum + row.points.reduce((inner, value) => inner + value, 0),
    0,
  ) ?? 0;
  const peak = cores.reduce((max, [, value]) => Math.max(max, value), 0);
  const mean = cores.length ? cores.reduce((sum, [, value]) => sum + value, 0) / cores.length : 0;

  const perSeries = (data.profile?.series ?? []).map((row, index) => {
    const points = toTimePoints(data.profile, index).map(
      ([clock, value]) => [clock, toCores(value)] as [number, number],
    );
    return {
      name: row.service_name || `series ${index + 1}`,
      points,
      value: points.length ? points[points.length - 1][1] : 0,
    };
  });

  return (
    <div className="space-y-3">
      <ProfileSelect
        units={data.units}
        value={profileType}
        label="Profile"
        filter={(unit) => isCpuType(unit.name)}
        onChange={data.setProfileType}
        testId="map-cpu-profile-type"
      />
      <StatRow>
        <Stat
          label="CPU time"
          value={totalValue ? (nanoseconds ? formatDuration(totalValue) : formatValue(totalValue, "nanoseconds")) : "—"}
          hint="in the window"
        />
        <Stat label="Mean" value={mean ? `${formatCores(mean)} cores` : "—"} hint={profileType ?? ""} />
        <Stat label="Peak" value={peak ? `${formatCores(peak)} cores` : "—"} hint="single bucket" />
      </StatRow>

      <Section title="Cores over the window" hint={profileType ?? ""}>
        {cores.length ? (
          <TimeChart
            series={[{ name: profileType ?? "cores", color: "#f2a65a", points: cores, area: true }]}
            format={(value) => formatCores(value)}
            description={`${profileType ?? "CPU"} samples converted to cores per bucket, summed across all series behind this node.`}
            testId="map-chart-cpu"
          />
        ) : (
          <Note>This node has no samples for the selected profile type in the window.</Note>
        )}
      </Section>

      <Section title="Series breakdown" hint={`${perSeries.length} series`}>
        {perSeries.length > 1 ? (
          <>
            <TimeChart
              series={perSeries.slice(0, SERIES_COLORS.length).map((series, index) => ({
                name: series.name,
                color: SERIES_COLORS[index % SERIES_COLORS.length],
                points: series.points,
              }))}
              format={(value) => formatCores(value)}
              description={`Per-series ${profileType ?? "CPU"} cores, one line for each labelled series behind this node.`}
              height={132}
              testId="map-chart-cpu-series"
            />
            <ul className="mt-1 space-y-0.5">
              {perSeries.slice(0, 8).map((series, index) => (
                <li key={`${series.name}:${index}`} className="flex items-center gap-2 text-[10px]">
                  <span
                    aria-hidden="true"
                    className="size-1.5 rounded-full"
                    style={{ background: SERIES_COLORS[index % SERIES_COLORS.length] }}
                  />
                  <span className="min-w-0 flex-1 truncate font-mono" title={series.name}>
                    {series.name}
                  </span>
                  <span className="font-mono text-muted-foreground">{formatCores(series.value)}</span>
                </li>
              ))}
            </ul>
            {perSeries.length > 8 ? <Note>Showing the first eight series.</Note> : null}
          </>
        ) : (
          <Note>One series behind this node; the total chart already shows it.</Note>
        )}
      </Section>

      <Note>
        One core equals one second of CPU per second of wall clock. Values are samples scaled by the
        agent&apos;s sampling period, not a kernel counter. CPU quota and throttling are not collected
        by the agent; read those from cgroup or Kubernetes metrics.
      </Note>
    </div>
  );
}

export function MemoryTab({ node, data }: TabProps) {
  const options = data.units.filter(isMemoryType);
  const names = unitNames(options);
  const profileType = data.profileType;

  useEffect(() => {
    if (!options.length) return;
    if (!profileType || !options.some((unit) => unit.name === profileType)) {
      data.setProfileType(pickMemoryType(options) ?? options[0].name);
    }
    // `names` carries the options; the object identity does not matter.
  }, [names, profileType]);

  if (!options.length) {
    return (
      <Missing
        icon={<HardDrive className="size-4" />}
        title="No memory profile in this window"
        hint="The agent reads Go heaps from the runtime and native heaps through the allocator. Neither produced a profile here: the process allocated nothing in the window, or the agent cannot read it."
      />
    );
  }

  const selected = options.find((unit) => unit.name === profileType) ?? options[0];
  const unit = selected.unit ?? "bytes";
  // `inuse_*` is a level and `alloc_*` a rate, so the same three numbers mean
  // different things: for a level, "latest" is what the process holds now and
  // the peak is how high it got; for a rate, both describe one bucket's worth
  // of allocation rather than a running total.
  const isLevel = selected.name.startsWith("inuse");
  const points = profileSeries(data.profile, (value) => value);
  const latest = points.length ? points[points.length - 1][1] : 0;
  const peak = points.reduce((max, [, value]) => Math.max(max, value), 0);
  const format = (value: number) => formatValue(value, unit);

  return (
    <div className="space-y-3">
      <ProfileSelect
        units={options}
        value={selected.name}
        label="Heap"
        filter={isMemoryType}
        onChange={data.setProfileType}
        testId="map-memory-profile-type"
      />
      <StatRow>
        <Stat
          label={isLevel ? "Held now" : "Latest bucket"}
          value={latest ? format(latest) : "—"}
          hint={selected.name}
        />
        <Stat
          label={isLevel ? "Peak held" : "Peak bucket"}
          value={peak ? format(peak) : "—"}
          hint="single bucket"
        />
        <Stat label="Buckets" value={points.length || "—"} hint="with samples" />
      </StatRow>

      <Section title="Memory over the window" hint={selected.name}>
        {points.length ? (
          <TimeChart
            series={[{ name: selected.name, color: "#b48ef2", points, area: true }]}
            format={format}
            description={`${selected.name} in each bucket. ${isLevel ? "This is a snapshot of memory held, not a rate." : "This is the amount allocated in each bucket."}`}
            testId="map-chart-memory"
          />
        ) : data.profileLoading ? (
          <Skeleton className="h-32 w-full rounded-md" data-testid="map-chart-memory-loading" />
        ) : (
          <Note>This node has no memory samples in the selected window.</Note>
        )}
      </Section>

      <Note>
        {isLevel
          ? `${selected.name} is a level, not a rate: bytes (or objects) allocated and not yet freed, read from the newest batch. It does not grow with the window.`
          : `${selected.name} is a cumulative counter: the sum of what was allocated in each window bucket.`}
      </Note>
    </div>
  );
}

// ---- Network --------------------------------------------------------------

export function NetTab({ node, data, inbound, outbound, windowSec, nodes, onSelect }: TabProps) {
  const step = data.history?.step_ns ?? 1;
  const inRate: Array<[number, number]> = (data.history?.points ?? []).map((point) => [
    point.bucket_ns / 1e6,
    ((point.rx_bytes ?? 0) / step) * 1e9,
  ]);
  const outRate: Array<[number, number]> = (data.history?.points ?? []).map((point) => [
    point.bucket_ns / 1e6,
    ((point.tx_bytes ?? 0) / step) * 1e9,
  ]);
  const rtt: Array<[number, number]> = (data.history?.edges ?? [])
    .filter((point) => point.rtt_us)
    .map((point) => [point.bucket_ns / 1e6, (point.rtt_us ?? 0) / 1000]);

  const latency = weightedRtt([...inbound, ...outbound]);
  const rateIn = inbound.reduce((sum, edge) => sum + edgeRate(edge, windowSec), 0);
  const rateOut = outbound.reduce((sum, edge) => sum + edgeRate(edge, windowSec), 0);
  // L7 detail parsed from the client's send buffer. A name that resolves
  // outside the cluster says more than an address that does, so external peers
  // come first; duplicates by value keep the list one line per host.
  const outside = (edge: ServiceMapEdge) =>
    edge.client === "external" || edge.server === "external";
  const hosts = Array.from(
    new Map(
      [...inbound, ...outbound]
        .filter((edge) => edge.host)
        .sort((a, b) => Number(outside(b)) - Number(outside(a)))
        .map((edge) => [edge.host, edge] as const),
    ).values(),
  );
  const statements = [...inbound, ...outbound].filter((edge) => edge.sql);
  const protocols = Array.from(
    [...inbound, ...outbound].reduce((map, edge) => {
      const name = edge.protocol || "tcp";
      map.set(name, (map.get(name) ?? 0) + (edge.connections ?? 0));
      return map;
    }, new Map<string, number>()),
  )
    .map(([name, value]) => ({ name, value }))
    .sort((a, b) => b.value - a.value);

  return (
    <div className="space-y-3">
      <StatRow>
        <Stat
          label="In / out"
          value={formatBytes((node.rx_bytes ?? 0) + (node.tx_bytes ?? 0))}
          hint={`${formatBytes(node.rx_bytes ?? 0)} in · ${formatBytes(node.tx_bytes ?? 0)} out`}
        />
        <Stat
          label="New conn/s"
          value={formatRate(rateIn + rateOut)}
          hint={`${formatRate(rateIn)} in · ${formatRate(rateOut)} out`}
        />
        <Stat
          label="Mean RTT"
          value={latency.rtt ? formatRtt(latency.rtt) : "—"}
          // A dash on its own reads as a broken card. The reason latency is
          // missing is structural — samples arrive when a connection closes —
          // so the hint says that instead of borrowing an unrelated number.
          hint={
            latency.rtt
              ? `${formatCount(node.retransmits ?? 0)} retx · ${formatCount(latency.samples)} samples`
              : "no closed connection this window"
          }
        />
      </StatRow>

      <Section title="Throughput" hint="bytes per second">
        {inRate.length ? (
          <TimeChart
            series={[
              { name: "in", color: "#6ba7f7", points: inRate, area: true },
              { name: "out", color: "#f2a65a", points: outRate, area: true },
            ]}
            format={(value) => formatBytes(value)}
            description="Bytes received and sent by this node in each bucket, as a rate per second."
            testId="map-chart-throughput"
          />
        ) : (
          <Note>No buckets with traffic in this window.</Note>
        )}
      </Section>

      <Section title="Round-trip time" hint="edge samples">
        {rtt.length ? (
          <TimeChart
            series={[{ name: "rtt", color: "#57c7d4", points: rtt }]}
            format={(value) => `${value.toFixed(2)} ms`}
            description="Sample-weighted mean RTT per bucket over every connection incident to this node."
            height={120}
            testId="map-chart-rtt"
          />
        ) : (
          <Note>
            RTT is sampled when a connection closes. Long-lived connections report nothing until they
            do.
          </Note>
        )}
      </Section>

      <Section title="Protocols" hint="new connections">
        {protocols.length ? (
          <RankingChart
            items={protocols}
            format={(value) => formatCount(value)}
            color="#6ba7f7"
            height={Math.max(72, protocols.length * 24)}
            description="Application protocols seen on this node's edges, ranked by new connections."
            testId="map-chart-protocols"
          />
        ) : (
          <Note>No protocol was identified for this node in the window.</Note>
        )}
      </Section>

      <Separator />

      <Section title={`Callers · ${inbound.length}`}>
        {inbound.length ? (
          <div className="space-y-0.5">
            {inbound.slice(0, 6).map((edge) => (
              <PeerRow
                key={`in:${edge.client}:${edge.protocol}`}
                peer={nodes.get(edge.client)}
                edge={edge}
                direction="in"
                windowSec={windowSec}
                onSelect={onSelect}
              />
            ))}
          </div>
        ) : (
          <Note>Nothing calls this node in the selected window.</Note>
        )}
      </Section>

      <Section title={`Callees · ${outbound.length}`}>
        {outbound.length ? (
          <div className="space-y-0.5">
            {outbound.slice(0, 6).map((edge) => (
              <PeerRow
                key={`out:${edge.server}:${edge.protocol}`}
                peer={nodes.get(edge.server)}
                edge={edge}
                direction="out"
                windowSec={windowSec}
                onSelect={onSelect}
              />
            ))}
          </div>
        ) : (
          <Note>This node opens no connections in the selected window.</Note>
        )}
      </Section>

      <Section title="Requested hosts" hint="Host header · external first">
        {hosts.length ? (
          <ul className="space-y-1">
            {hosts.slice(0, 6).map((edge) => (
              <li
                key={`host:${edge.client}:${edge.server}:${edge.host}`}
                className="flex items-baseline gap-2 text-[11px]"
              >
                <span className="min-w-0 flex-1 truncate font-mono" title={edge.host}>
                  {edge.host}
                </span>
                <span className="shrink-0 text-[9px] uppercase text-muted-foreground">
                  {outside(edge) ? "external" : edge.protocol}
                </span>
              </li>
            ))}
          </ul>
        ) : (
          <Note>
            No request named a host in this window. A plaintext HTTP request carries its{" "}
            <code className="font-mono">Host</code> header; set{" "}
            <code className="font-mono">LIDLESSEYE_SSL_UPROBE=1</code> on the agent to read it
            from TLS as well. HTTP/2 keeps its headers compressed and is not covered. Host names are
            client-supplied and are shown verbatim.
          </Note>
        )}
      </Section>

      <Section title="Recent SQL" hint="literals redacted">
        {statements.length ? (
          <ul className="space-y-1">
            {statements.slice(0, 4).map((edge) => (
              <li
                key={`sql:${edge.client}:${edge.server}`}
                className="rounded bg-secondary/40 px-2 py-1"
              >
                <div className="mb-0.5 flex items-center gap-2 text-[9px] uppercase text-muted-foreground">
                  <span>{edge.protocol}</span>
                  <span className="truncate">{edge.server}</span>
                </div>
                <code className="block break-all font-mono text-[10px] leading-snug text-foreground/90">
                  {redactSql(edge.sql ?? "")}
                </code>
              </li>
            ))}
          </ul>
        ) : (
          <Note>
            No plaintext SQL statement in this window. The TLS uprobe also covers MySQL and
            PostgreSQL when the driver negotiates encryption.
          </Note>
        )}
      </Section>
    </div>
  );
}

// ---- DNS ------------------------------------------------------------------

export function DnsTab({ node, data, inbound, outbound, windowSec, nodes, onSelect }: TabProps) {
  const dns = [...inbound, ...outbound].filter((edge) => edge.protocol === "dns");
  const history = data.dnsHistory;
  const step = history?.step_ns ?? 1;
  const lookups = dns.reduce((sum, edge) => sum + (edge.connections ?? 0), 0);
  const lookupsPerSec = lookups / Math.max(1, windowSec);
  const latency = weightedRtt(dns);
  const failures = dns.reduce((sum, edge) => sum + (edge.retransmits ?? 0), 0);

  const lookupPoints: Array<[number, number]> = (history?.points ?? history?.edges ?? []).map(
    (point) => [point.bucket_ns / 1e6, ((point.connections ?? 0) / step) * 1e9],
  );
  const rttPoints: Array<[number, number]> = (history?.edges ?? [])
    .filter((point) => point.rtt_us)
    .map((point) => [point.bucket_ns / 1e6, (point.rtt_us ?? 0) / 1000]);

  if (!dns.length && !lookupPoints.length) {
    return (
      <Missing
        icon={<Network className="size-4" />}
        title="No TCP DNS traffic on this node"
        hint="Lookups over UDP/53 are not captured — the agent reads TCP sockets. A nameserver reached over TCP, or a lookup that fell back to it, appears here."
      />
    );
  }

  const ranked = dns
    .filter((edge) => edge.rtt_us)
    .map((edge) => {
      const direction: "in" | "out" = edge.client === node.id ? "out" : "in";
      const peerId = direction === "in" ? edge.client : edge.server;
      return {
        name: nodes.get(peerId)?.service || peerId,
        value: Math.round((edge.rtt_us ?? 0) / 100) / 10,
      };
    })
    .sort((a, b) => b.value - a.value);

  return (
    <div className="space-y-3">
      <StatRow>
        <Stat label="Lookups/s" value={formatRate(lookupsPerSec)} hint={`${dns.length} peers · ${formatCount(lookups)} total`} />
        <Stat
          label="Mean RTT"
          value={latency.rtt ? formatRtt(latency.rtt) : "—"}
          tone={latency.rtt > 50_000 ? "warn" : undefined}
          hint={latency.rtt ? `${formatCount(latency.samples)} samples · tcp/53` : "no closed connection"}
        />
        <Stat
          label="Retransmits"
          value={formatCount(failures)}
          tone={failures ? "warn" : "ok"}
          hint="timeout proxy"
        />
      </StatRow>

      <Section title="Lookups over the window" hint="TCP/53 connections per second">
        {lookupPoints.length ? (
          <TimeChart
            series={[{ name: "lookups", color: "#6ba7f7", points: lookupPoints, area: true }]}
            format={(value) => formatRate(value)}
            description="DNS connections over TCP port 53 per second, bucketed over the selected window."
            testId="map-chart-dns-rate"
          />
        ) : (
          <Note>Not enough DNS buckets to draw a line.</Note>
        )}
      </Section>

      <Section title="Latency over the window" hint="sample-weighted mean">
        {rttPoints.length ? (
          <TimeChart
            series={[{ name: "rtt", color: "#57c7d4", points: rttPoints }]}
            format={(value) => `${value.toFixed(2)} ms`}
            description="Mean RTT of DNS connections over TCP port 53 per bucket, weighted by closed-connection samples."
            height={120}
            testId="map-chart-dns-rtt"
          />
        ) : (
          <Note>DNS RTT is only reported when a connection closes inside the window.</Note>
        )}
      </Section>

      <Section title="Latency by peer" hint="mean RTT">
        {ranked.length ? (
          <RankingChart
            items={ranked}
            format={(value) => `${value.toFixed(1)} ms`}
            color="#57c7d4"
            height={Math.max(80, ranked.length * 26)}
            description="DNS peers ranked by mean round-trip time in milliseconds."
            testId="map-chart-dns"
          />
        ) : (
          <Note>No closed DNS connection produced an RTT sample.</Note>
        )}
      </Section>

      <Section title="Peers">
        <div className="space-y-0.5">
          {dns.map((edge) => (
            <PeerRow
              key={`dns:${edge.client}:${edge.server}:${edge.protocol}`}
              peer={nodes.get(edge.client === node.id ? edge.server : edge.client)}
              edge={edge}
              direction={edge.client === node.id ? "out" : "in"}
              windowSec={windowSec}
              onSelect={onSelect}
            />
          ))}
        </div>
      </Section>

      <Note>
        Names are resolved by TCP port 53, not by parsing queries: qname, rcode and NXDOMAIN are not
        recorded by the agent, and a resolver that only ever answers over UDP will not appear. The
        window is the one selected above.
      </Note>
    </div>
  );
}

// ---- Profiling ------------------------------------------------------------

export function ProfilingTab({ node, data }: TabProps) {
  const filters = profileFilters(node);
  const items = (data.top?.items ?? []).map((item) => ({
    name: item.module ? `${item.name} (${item.module})` : item.name,
    value: item.self_value,
  }));
  const unit = data.top?.unit ?? "nanoseconds";
  const total = data.top?.total_value ?? 0;

  return (
    <div className="space-y-3">
      <ProfileSelect
        units={data.units}
        value={data.profileType}
        label="Profile"
        filter={() => true}
        onChange={data.setProfileType}
        testId="map-profiling-profile-type"
      />
      <StatRow>
        <Stat label="Profile total" value={total ? formatValue(total, unit) : "—"} hint={unit} />
        <Stat label="Functions" value={items.length || "—"} hint="exclusive time, top 12" />
        <Stat
          label="Series"
          value={data.profile?.series.length ? formatCount(data.profile.series.length) : "—"}
          hint="labelled series in window"
        />
      </StatRow>

      <Section title="Hottest functions" hint={data.profileType ?? "selected profile"}>
        {items.length ? (
          <RankingChart
            items={items}
            format={(value) => formatValue(value, unit)}
            color="#f2a65a"
            height={Math.min(320, Math.max(120, items.length * 22))}
            limit={12}
            description={`Top 12 functions by exclusive ${data.profileType ?? "profile"} value, in ${unit}.`}
            testId="map-chart-top"
          />
        ) : (
          <Missing
            icon={<Flame className="size-4" />}
            title="No stacks matched this node"
            hint="The map and the profiles are joined by labels; if the agent could not resolve a pod to a workload, this tab stays empty while the flamegraph may still have data."
          />
        )}
      </Section>

      <Section
        title="Flamegraph"
        hint={filters ? `this node only · ${data.profileType ?? "no type"}` : "no profiles behind this node"}
      >
        {data.loading ? (
          <Skeleton className="h-48 w-full rounded-md" data-testid="map-profile-flamegraph-loading" />
        ) : data.flamegraph ? (
          // The same canvas the full Flamegraph view uses, so search, zoom and
          // the hover tooltip behave identically — only the selector is scoped
          // to this node. It lives in the tab rather than a new page because
          // the operator is comparing it with the ranking just above.
          <div data-testid="map-profile-flamegraph">
            <FlamegraphCanvas payload={data.flamegraph} />
          </div>
        ) : (
          <Missing
            icon={<Flame className="size-4" />}
            title="No stacks for this node in the window"
            hint="The flamegraph asks for the same profile type and selector as the ranking above. Widen the range or check that the agent resolved this workload's labels."
          />
        )}
      </Section>

      {filters ? (
        <Button
          size="sm"
          className="w-full"
          data-testid="map-profile"
          onClick={() => dashboardStore.showProfile(filters, data.profileType ?? undefined)}
        >
          <ArrowUpRight />
          Open full flamegraph view
        </Button>
      ) : (
        <Note>Traffic to or from outside the cluster has no profiles behind it.</Note>
      )}

      <Separator />
      <Section title="Query context" hint="same as the charts above">
        <div className="rounded bg-secondary/40 px-2 py-1.5 font-mono text-[10px] leading-relaxed break-all">
          {filters ? `${data.profileType ?? "no type"} · ${filters
            .map((filter) => filter.raw ?? `${filter.label}${filter.op ?? "="}"${filter.value ?? ""}"`)
            .join(", ")}` : "external destination · no profile labels"}
        </div>
        <Note>
          The window, selector and profile type are shared by the ranking, the flamegraph and the
          full view. Changing the type here changes all three.
        </Note>
      </Section>
    </div>
  );
}
