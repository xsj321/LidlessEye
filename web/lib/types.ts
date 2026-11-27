// Wire types for the LidlessEye HTTP API.
//
// The shapes mirror what the Rust handlers serialise; fields the dashboard does
// not read are left out on purpose, and anything the server may omit is
// optional so the drawing code has to handle it explicitly.

export type View =
  | "flamegraph"
  | "compare"
  | "top"
  | "timeseries"
  | "labels"
  | "servicemap"
  | "memory"
  | "overview";

export const VIEWS: readonly View[] = [
  "flamegraph",
  "compare",
  "top",
  "timeseries",
  "labels",
  "servicemap",
  "memory",
  "overview",
];

/**
 * One row of the label filter bar.
 *
 * `raw` is a matcher the UI could not break into dropdowns (pasted from a deep
 * link); it is kept verbatim so the filter still applies.
 */
export interface LabelFilter {
  label?: string;
  op?: string;
  value?: string;
  raw?: string;
}

export interface FlamegraphPayload {
  names: string[];
  levels: number[];
  values?: number[];
  /**
   * Sample rows under each node, parallel to 
ames`. Present on new
   * responses; older servers omit it, and the UI then hides the column
   * instead of pretending the count is zero.
   */
  sample_counts?: number[];
  total?: number;
  samples?: number;
  unit: string;
  truncated?: boolean;
}

export interface DiffPayload {
  names: string[];
  levels: number[];
  left: number[];
  right: number[];
  diff: number[];
  total_left: number;
  total_right: number;
  unit: string;
  truncated?: boolean;
}

export interface TopItem {
  name: string;
  module?: string;
  file?: string;
  line?: number;
  self_value: number;
  total_value: number;
}

export interface TopPayload {
  items: TopItem[];
  unit: string;
  total_value: number;
}

export interface TimeseriesSeries {
  service_name: string;
  labels?: Record<string, string>;
  points: number[];
}

export interface TimeseriesPayload {
  from_ns: number;
  until_ns: number;
  step_ns: number;
  series: TimeseriesSeries[];
}

export type NodeKind = "workload" | "service" | "external";

/** One replica behind a workload, as the cluster reports it. */
export interface ServiceInstance {
  name: string;
  /** Stable Kubernetes UID, used to match an edge to this replica. */
  uid?: string;
  /** Kubernetes node the pod runs on; empty when the API did not say. */
  node?: string;
  ready?: boolean;
  /** `Running`, `Pending`, ... */
  phase?: string;
}

export interface ServiceMapNode {
  id: string;
  namespace?: string;
  workload?: string;
  service?: string;
  kind?: NodeKind;
  rx_bytes?: number;
  tx_bytes?: number;
  connections?: number;
  retransmits?: number;
  cpu_ns?: number;
  /** Replicas of this workload at the end of the window, and how many are ready. */
  pods?: number;
  ready?: number;
  /** The replicas themselves, newest observation, bounded by the agent. */
  instances?: ServiceInstance[];
  /** Container names of the workload's pods, for the per-container rows. */
  containers?: string[];
  /** Language the profiler reached: `go`, `jvm`, `native`, or absent. */
  runtime?: string;
}

export interface ServiceMapEdge {
  client: string;
  server: string;
  protocol: string;
  /** Container of the client pod that opened the connections, when known. */
  client_container?: string;
  /** Container of the server pod that accepted them, when known. */
  server_container?: string;
  /** UIDs of the concrete pods on each end; empty for old data or non-pods. */
  client_pod?: string;
  server_pod?: string;
  connections: number;
  retransmits?: number;
  /** Bytes the client sent to the server over the window. */
  tx_bytes?: number;
  /** Bytes the server sent back. */
  rx_bytes?: number;
  /** Average round-trip time in microseconds; 0 when nothing closed. */
  rtt_us?: number;
  /** Number of closed connections behind `rtt_us`, for correct averaging. */
  rtt_samples?: number;
  /** `Host:` header of the last HTTP request on this edge. */
  host?: string;
  /** Last MySQL/PostgreSQL statement on this edge. */
  sql?: string;
}

export interface ServiceMapPayload {
  from_ns: number;
  until_ns: number;
  nodes: ServiceMapNode[];
  edges: ServiceMapEdge[];
  truncated?: boolean;
}

/**
 * One bucket of a single node's history.
 *
 * `points` come from the node's own counters (and carry the replica gauge),
 * `edges` from the connections around it (and carry the round-trip time).
 * Buckets with no traffic are simply absent.
 */
export interface MapHistoryPoint {
  bucket_ns: number;
  rx_bytes?: number;
  tx_bytes?: number;
  connections?: number;
  retransmits?: number;
  rtt_us?: number;
  /** How many closed connections produced `rtt_us`. */
  rtt_samples?: number;
  pods?: number;
  ready?: number;
}

export interface MapHistoryPayload {
  node: string;
  from_ns: number;
  until_ns: number;
  step_ns: number;
  /** Optional application-protocol filter echoed by the server. */
  protocol?: string;
  points: MapHistoryPoint[];
  edges: MapHistoryPoint[];
  /**
   * Pods behind this node, read only when a panel asks for one node.
   *
   * The map payload carries counts and nothing else, so a pod name is not
   * repeated in every bucket of every node the map draws.
   */
  instances?: ServiceInstance[];
  /** Container names behind this node, for the per-container rows. */
  containers?: string[];
  /** Language the profiler reached: `go`, `jvm`, `native`, or absent. */
  runtime?: string;
}

/**
 * Whole-map request counts over the brush domain.
 *
 * `points` is the counter named by `metric` in each bucket, summed across
 * every edge — the map's closest equivalent of a request rate. It is returned
 * as a dense array so the brush can draw bucket index i at time from + i*step.
 */
export type MapMetric = "connections" | "bytes" | "retransmits";

export interface MapHistogramPayload {
  from_ns: number;
  until_ns: number;
  step_ns: number;
  /** Echoes the requested counter, so the brush labels bars from the answer. */
  metric?: MapMetric;
  points: number[];
}

export interface ProfileType {  name: string;
  unit: string;
  total_value: number;
  batch_count: number;
  virtual?: boolean;
}

export interface ProfileTypesPayload {
  profile_types: ProfileType[];
}

export interface SeriesRow {
  service_name: string;
  profile_type: string;
  labels?: Record<string, string>;
  total_value: number;
  batch_count: number;
  start_ns: number;
  end_ns: number;
}

export interface SeriesPayload {
  series: SeriesRow[];
}

export interface StatsPayload {
  version?: string;
  uptime_seconds?: number;
  series?: number;
  ingest_samples?: number;
  ingest_reports?: number;
  queries?: number;
  store?: {
    batch_count?: number;
    total_bytes?: number;
    earliest_ns?: number;
    latest_ns?: number;
  };
}

export interface TenantRow {
  id: string;
}

export interface TenantsPayload {
  tenants: TenantRow[];
}

export interface LabelNamesPayload {
  label_names: string[];
}

export interface LabelValueRow {
  value: string;
  total?: number;
}

export interface LabelValuesPayload {
  values: LabelValueRow[];
}

export interface VersionPayload {
  version: string;
}

/**
 * The workloads whose runtime memory profiling is switched on.
 *
 * Names are 
amespace/workload`, the same identifier a map node carries, so a
 * switch flipped on a node names exactly one entry.
 */
export interface DirectivesPayload {
  /** Workloads whose JVM Flight Recorder is read. */
  workloads: string[];
  /** Workloads whose processes get the bpftime agent injected. */
  alloc_workloads?: string[];
  /** Every entry with its kind. */
  directives?: { workload: string; kind: "jvm" | "alloc" }[];
  /**
   * Whether the native reader behind `alloc_workloads` can actually report
   * samples. When it cannot, the panel keeps its switch hidden.
   */
  native_memory?: { supported: boolean; reason?: string };
}

/** One workload's allocation profile and the evidence behind it. */
/**
 * The language a reader found inside a workload.
 *
 * It is a fact about what the profiler reached, not about the image: a Go
 * binary that also reports eBPF CPU samples still shows `go` here only because
 * the Go heap reader answered for it.
 */
export type MemoryRuntime = "go" | "jvm" | "native";

export interface MemoryCoverageRow {
  workload: string;
  /** Whether the operator still has this workload's reader switched on. */
  enabled: boolean;
  /** Estimated bytes allocated in the window (samples × interval, summed). */
  requested_alloc_bytes: number;
  /** Bytes the heap still holds right now: the last `inuse_space` reading. */
  inuse_bytes: number;
  /** Language the reader found inside the workload, inferred from its series. */
  runtime: MemoryRuntime | null;
  /** Ingested reports, not recorded stacks: the reader batches those up. */
  batches: number;
  sampling_interval: number | null;
  sampling_label: string | null;
  reader: string | null;
  first_seen_ns: number;
  last_seen_ns: number;
  /** Process-level coverage; null until the agent reports it back. */
  coverage: MemoryCoverage | null;
  coverage_note?: string;
  /** Process memory over time, oldest first. */
  history?: MemoryPoint[];
}

/** One point on the process memory trend. */
export interface MemoryPoint {
  at_ns: number;
  rss_bytes: number;
  pss_bytes: number;
  cgroup_bytes: number;
}

/** What the reader in the pod reported about its own reach. */
export interface MemoryCoverage {
  detected: number;
  attached: number;
  uncovered: number;
  skipped: Record<string, number>;
  /** The pod's cgroup memory usage in bytes, when the shim could read it. */
  memory_bytes: number | null;
  /** Processes summed for the RSS/PSS figures. */
  process_count: number | null;
  /** Resident set summed over the pod's processes, in bytes. */
  process_rss_bytes: number | null;
  /** Proportional set summed over the pod's processes, in bytes. */
  process_pss_bytes: number | null;
  at_ns: number;
}

export interface MemoryCoveragePayload {
  tenant: string;
  /** Window the totals cover, echoed by the server so the page can show it. */
  from_ns?: number;
  until_ns?: number;
  enabled_workloads: string[];
  workloads: MemoryCoverageRow[];
}

/** One reader's state for one workload, as the agent last reported it. */
export type CapabilityRow = {
  workload: string;
  reader: string;
  state?: "supported" | "attaching" | "active" | "degraded" | "unsupported" | "disabled";
  reason?: string;
  pod?: string;
  detected?: number;
  attached?: number;
  pods_reported?: number;
  pods_active?: number;
  at_ns?: number;
};

export type CapabilityPayload = { capabilities: CapabilityRow[] };
/** One process as the target pod's own /proc reported it. */
export type MemoryProcessRow = {
  pid: number;
  start_ns?: number;
  comm?: string;
  executable?: string;
  rss_bytes?: number;
  pss_bytes?: number | null;
  private_dirty_bytes?: number | null;
  swap_bytes?: number | null;
  missing?: string;
};

export type MemoryProcessPayload = {
  workload: string;
  pods: number;
  omitted: number;
  processes: MemoryProcessRow[];
};