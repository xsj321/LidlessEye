"use client";

// The seven tabs of the node panel.
//
// Each tab answers one question an operator asks about a service, and each one
// is built from the measurements that actually exist: the map's own counters
// and latency, the profiles the agent collected, and the pod list from the
// cluster. Where a measurement does not exist, the tab says so and says what
// would produce it, instead of drawing an empty chart — a graph that is always
// flat is worse than an honest gap.

import { ArrowDownLeft, ArrowUpRight, Flame, Gauge, HardDrive, Network, Server } from "lucide-react";
import type { ReactNode } from "react";

import { FlamegraphCanvas } from "@/components/canvas/flamegraph-canvas";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { formatBytes, formatCount, formatDuration, formatPercent, formatValue } from "@/lib/format";
import { edgeRate, edgeWeight, formatRate, formatRtt, profileFilters } from "@/lib/servicemap-graph";
import { dashboardStore } from "@/lib/store";
import type { ServiceInstance, ServiceMapEdge, ServiceMapNode } from "@/lib/types";
import { cn } from "@/lib/utils";

import { RankingChart, TimeChart } from "./charts";
import { totalTimePoints, type NodeData } from "./use-node-data";

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
    <div className="flex flex-col items-center gap-1.5 rounded-md border border-dashed px-4 py-6 text-center">
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
  windowSec,
  onSelect,
}: {
  peer: ServiceMapNode | undefined;
  edge: ServiceMapEdge;
  windowSec: number;
  onSelect: (id: string) => void;
}) {
  const label = peer?.service || peer?.workload || peer?.id || edge.client;
  return (
    <button
      type="button"
      onClick={() => onSelect(peer?.id ?? edge.server)}
      className="flex w-full items-center gap-2 rounded px-1 py-0.5 text-left text-[11px] hover:bg-secondary/60"
      data-testid="map-peer-row"
      title={`${edge.protocol} · ${formatCount(edge.connections)} connections · ${formatBytes(edge.tx_bytes ?? 0)}`}
    >
      <span className="min-w-0 flex-1 truncate">{label}</span>
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
  const availability = total ? 1 - errors / total : 1;
  const budget = Math.max(0, Math.min(1, 1 - (1 - availability) / (1 - SLO_TARGET)));

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

  const sampled = [...inbound, ...outbound].filter((edge) => edge.rtt_us);
  const latency = sampled.length
    ? sampled.reduce((sum, edge) => sum + (edge.rtt_us ?? 0), 0) / sampled.length
    : 0;

  return (
    <div className="space-y-3">
      <StatRow>
        <Stat
          label="Availability"
          value={availability >= 0.99995 ? "100%" : formatPercent(availability, 2)}
          hint={`target ${formatPercent(SLO_TARGET, 1)}`}
          tone={availability < SLO_TARGET ? "warn" : "ok"}
        />
        <Stat
          label="Budget left"
          value={formatPercent(budget, 0)}
          hint={`${formatCount(errors)} retx in ${formatCount(total)} conns`}
          tone={budget < 0.2 ? "warn" : "ok"}
        />
        <Stat
          label="Mean RTT"
          value={latency ? formatRtt(latency) : "—"}
          hint={latency ? "over this node's edges" : "no closed connection this window"}
        />
      </StatRow>

      <Section title="Error rate over the window" hint="retransmits / connections">
        {points.length > 1 ? (
          <TimeChart
            series={[{ name: "error rate", color: "#e05c5c", points, area: true }]}
            format={(value) => `${value.toFixed(2)}%`}
            target={{ value: (1 - SLO_TARGET) * 100, label: "budget 0.10%" }}
            testId="map-chart-slo"
          />
        ) : (
          <Note>Not enough buckets with traffic to draw a line.</Note>
        )}
      </Section>

      <Section title="Callers">
        {inbound.length ? (
          <div className="space-y-0.5">
            {inbound.slice(0, 5).map((edge) => (
              <PeerRow
                key={`in:${edge.client}:${edge.protocol}`}
                peer={nodes.get(edge.client)}
                edge={edge}
                windowSec={windowSec}
                onSelect={onSelect}
              />
            ))}
          </div>
        ) : (
          <Note>Nothing calls this node in the selected window.</Note>
        )}
      </Section>

      <Note>
        This objective is derived from TCP: a retransmit stands in for a failed request, because no L7
        status codes are collected. Set it against your own dashboard when you have one.
      </Note>
    </div>
  );
}

// ---- Instances ------------------------------------------------------------

function InstanceRow({ instance }: { instance: ServiceInstance }) {
  return (
    <div className="flex items-center gap-2 px-1 py-0.5 text-[11px]">
      <span
        className="size-1.5 shrink-0 rounded-full"
        style={{ background: instance.ready ? "#4fbf8b" : "#f2a65a" }}
      />
      <span className="min-w-0 flex-1 truncate font-mono text-[10px]" title={instance.name}>
        {instance.name}
      </span>
      <span className="shrink-0 text-[9px] text-muted-foreground">{instance.phase || "—"}</span>
      <span className="w-24 shrink-0 truncate text-right text-[9px] text-muted-foreground" title={instance.node}>
        {instance.node || "—"}
      </span>
    </div>
  );
}

export function InstanceTab({ node, data }: TabProps) {
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
    return (
      <Missing
        icon={<Server className="size-4" />}
        title="Replicas belong to workloads"
        hint="A ClusterIP service has endpoints rather than pods of its own; the pods behind it are their own nodes on the map."
      />
    );
  }

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
        {desired.length > 1 ? (
          <TimeChart
            series={[
              { name: "desired", color: "#6ba7f7", points: desired, step: true, dashed: true },
              { name: "ready", color: "#4fbf8b", points: readyPoints, step: true },
            ]}
            format={(value) => String(Math.round(value))}
            yMax={Math.max(pods, ready, 1) + 1}
            height={132}
            testId="map-chart-instances"
          />
        ) : (
          <Note>Only one collection window in range, so there is no line to draw.</Note>
        )}
      </Section>

      <Separator />

      <Section title={`Instances · ${instances.length}`} hint="sample">
        {instances.length ? (
          <div className="space-y-0.5">
            <div className="flex items-center gap-2 px-1 pb-0.5 text-[9px] uppercase tracking-wide text-muted-foreground/70">
              <span className="size-1.5" />
              <span className="flex-1">pod</span>
              <span>phase</span>
              <span className="w-24 text-right">node</span>
            </div>
            {instances.map((instance) => (
              <InstanceRow key={instance.name} instance={instance} />
            ))}
          </div>
        ) : (
          <Note>
            The agent reports counts but no pod names for this workload — it started before instances
            were collected, or the cluster API did not answer.
          </Note>
        )}
      </Section>
      {instances.length >= 24 ? (
        <Note>Lists at most 24 replicas per workload.</Note>
      ) : null}
    </div>
  );
}

// ---- CPU / memory ---------------------------------------------------------

function profileSeries(
  payload: NodeData["cpu"],
  transform: (value: number) => number,
): Array<[number, number]> {
  if (!payload) return [];
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

export function CpuTab({ node, data }: TabProps) {
  const step = data.cpu?.step_ns ?? 0;
  const cores = profileSeries(data.cpu, (value) => (step ? value / step : 0));
  const total = cores.reduce((sum, [, value]) => sum + value * (step / 1e9), 0);
  const peak = cores.reduce((max, [, value]) => Math.max(max, value), 0);
  const mean = cores.length ? cores.reduce((sum, [, value]) => sum + value, 0) / cores.length : 0;

  return (
    <div className="space-y-3">
      <StatRow>
        <Stat label="CPU time" value={total ? formatDuration(total * 1e9) : "—"} hint="in the window" />
        <Stat label="Mean" value={mean ? `${formatCores(mean)} cores` : "—"} hint="sampled on-CPU" />
        <Stat label="Peak" value={peak ? `${formatCores(peak)} cores` : "—"} hint="single bucket" />
      </StatRow>

      <Section title="Cores over the window" hint="on-CPU samples">
        {cores.length ? (
          <TimeChart
            series={[{ name: "cores", color: "#f2a65a", points: cores, area: true }]}
            format={(value) => formatCores(value)}
            testId="map-chart-cpu"
          />
        ) : (
          <Missing
            icon={<Flame className="size-4" />}
            title="No CPU profiles for this node"
            hint={`Nothing matched ${node.namespace ? `${node.namespace}/` : ""}${node.workload || node.service || node.id} in this window. The map's CPU heat comes from the same profiles.`}
          />
        )}
      </Section>

      <Note>
        One core equals one second of CPU per second of wall clock. Values are samples scaled by the
        agent&apos;s sampling period, not a kernel counter.
      </Note>
    </div>
  );
}

export function MemoryTab({ node, data }: TabProps) {
  if (!data.memoryType) {
    return (
      <Missing
        icon={<HardDrive className="size-4" />}
        title="No memory profile type in this window"
        hint="The agent collects on-CPU and off-CPU. Send heap profiles with profile_type=memory (pprof upload, an SDK, or a folded file) and this tab fills in."
      />
    );
  }

  const unit = data.units.find((entry) => entry.name === data.memoryType)?.unit ?? "bytes";
  const points = profileSeries(data.memory, (value) => value);
  const latest = points.length ? points[points.length - 1][1] : 0;
  const peak = points.reduce((max, [, value]) => Math.max(max, value), 0);
  const format = (value: number) => formatValue(value, unit);

  return (
    <div className="space-y-3">
      <StatRow>
        <Stat label="Latest" value={latest ? format(latest) : "—"} hint={data.memoryType} />
        <Stat label="Peak" value={peak ? format(peak) : "—"} hint="single bucket" />
        <Stat label="Buckets" value={points.length || "—"} hint="with samples" />
      </StatRow>

      <Section title="Memory over the window" hint={data.memoryType}>
        {points.length ? (
          <TimeChart
            series={[{ name: data.memoryType, color: "#b48ef2", points, area: true }]}
            format={format}
            testId="map-chart-memory"
          />
        ) : (
          <Note>This node has no memory samples in the selected window.</Note>
        )}
      </Section>

      <Note>
        A heap profile type is whatever the emitter chose ({data.memoryType}); the unit comes from the
        profile metadata, so an allocation-count type would read as a count rather than as bytes.
      </Note>
      <Note>{node.id}</Note>
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

  const sampled = [...inbound, ...outbound].filter((edge) => edge.rtt_us);
  const meanRtt = sampled.length
    ? sampled.reduce((sum, edge) => sum + (edge.rtt_us ?? 0) * (edgeWeight(edge) || 1), 0) /
      sampled.reduce((sum, edge) => sum + (edgeWeight(edge) || 1), 0)
    : 0;
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
          value={meanRtt ? formatRtt(meanRtt) : "—"}
          // A dash on its own reads as a broken card. The reason latency is
          // missing is structural — samples arrive when a connection closes —
          // so the hint says that instead of borrowing an unrelated number.
          hint={
            meanRtt
              ? `${formatCount(node.retransmits ?? 0)} retransmits`
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
            testId="map-chart-throughput"
          />
        ) : (
          <Note>No buckets with traffic in this window.</Note>
        )}
      </Section>

      <Section title="Round-trip time" hint="edge samples">
        {rtt.length > 1 ? (
          <TimeChart
            series={[{ name: "rtt", color: "#57c7d4", points: rtt }]}
            format={(value) => `${value.toFixed(2)} ms`}
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

      <Separator />

      <Section title={`Callers · ${inbound.length}`}>
        {inbound.length ? (
          <div className="space-y-0.5">
            {inbound.slice(0, 6).map((edge) => (
              <PeerRow
                key={`in:${edge.client}:${edge.protocol}`}
                peer={nodes.get(edge.client)}
                edge={edge}
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
            from TLS as well. HTTP/2 keeps its headers compressed and is not covered.
          </Note>
        )}
      </Section>

      <Section title="Recent SQL" hint="MySQL / PostgreSQL">
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
                  {edge.sql}
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
  const lookups = dns.reduce((sum, edge) => sum + edgeRate(edge, windowSec), 0);
  const sampled = dns.filter((edge) => edge.rtt_us);
  const meanRtt = sampled.length
    ? sampled.reduce((sum, edge) => sum + (edge.rtt_us ?? 0), 0) / sampled.length
    : 0;
  const failures = dns.reduce((sum, edge) => sum + (edge.retransmits ?? 0), 0);

  if (!dns.length) {
    return (
      <Missing
        icon={<Network className="size-4" />}
        title="No TCP DNS traffic on this node"
        hint="Lookups over UDP/53 are not captured — the agent reads TCP sockets. A nameserver reached over TCP, or a lookup that fell back to it, appears here."
      />
    );
  }

  return (
    <div className="space-y-3">
      <StatRow>
        <Stat label="Lookups/s" value={formatRate(lookups)} hint={`${dns.length} peers`} />
        <Stat
          label="Mean RTT"
          value={meanRtt ? formatRtt(meanRtt) : "—"}
          tone={meanRtt > 50_000 ? "warn" : undefined}
          hint="tcp/53"
        />
        <Stat
          label="Retransmits"
          value={formatCount(failures)}
          tone={failures ? "warn" : "ok"}
          hint="timeout proxy"
        />
      </StatRow>

      <Section title="Latency by peer" hint="mean RTT">
        <RankingChart
          items={sampled.map((edge) => ({
            name: nodes.get(edge.client === node.id ? edge.server : edge.client)?.service ?? edge.server,
            value: Math.round((edge.rtt_us ?? 0) / 100) / 10,
          }))}
          format={(value) => `${value.toFixed(1)} ms`}
          color="#57c7d4"
          height={Math.max(80, sampled.length * 26)}
          testId="map-chart-dns"
        />
      </Section>

      <Section title="Peers">
        <div className="space-y-0.5">
          {dns.map((edge) => {
            const peerId = edge.client === node.id ? edge.server : edge.client;
            return (
              <PeerRow
                key={`dns:${edge.client}:${edge.server}`}
                peer={nodes.get(peerId)}
                edge={edge}
                windowSec={windowSec}
                onSelect={onSelect}
              />
            );
          })}
        </div>
      </Section>

      <Note>
        Names are resolved by TCP port 53, not by parsing queries: a resolver that only ever answers
        over UDP will not appear. The window is the one selected above.
      </Note>
      {data.history?.edges.length ? null : (
        <Note>
          No per-bucket DNS series: latency is stored per edge, not per protocol, so the chart above
          ranks peers rather than drawing a line.
        </Note>
      )}
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
      <StatRow>
        <Stat label="Profile total" value={total ? formatValue(total, unit) : "—"} hint={unit} />
        <Stat label="Functions" value={items.length || "—"} hint="self time, top 12" />
        <Stat
          label="Samples"
          value={data.cpu?.series?.length ? formatCount(data.cpu.series.length) : "—"}
          hint="series in window"
        />
      </StatRow>

      <Section title="Hottest functions" hint="exclusive time">
        {items.length ? (
          <RankingChart
            items={items}
            format={(value) => formatValue(value, unit)}
            color="#f2a65a"
            height={Math.min(260, Math.max(120, items.length * 22))}
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
        hint={filters ? 'this node only · on-CPU' : 'no profiles behind this node'}
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
          onClick={() => dashboardStore.showProfile(filters)}
        >
          <ArrowUpRight />
          Open full flamegraph view
        </Button>
      ) : (
        <Note>Traffic to or from outside the cluster has no profiles behind it.</Note>
      )}

      <div className="flex items-center gap-2 text-[10px] text-muted-foreground">
        <Gauge className="size-3" />
        Same window and selector as the flamegraph; switching profile type there keeps the filters.
      </div>
      <Separator />
      <Section title="Direction" hint="from the map">
        <div className="flex items-center justify-between text-[10px] text-muted-foreground">
          <span className="flex items-center gap-1">
            <ArrowDownLeft className="size-3" />
            {formatCount(node.connections ?? 0)} new connections in
          </span>
          <Badge variant="secondary" className="font-mono text-[9px]">
            {formatBytes(node.rx_bytes ?? 0)} in
          </Badge>
        </div>
      </Section>
    </div>
  );
}
