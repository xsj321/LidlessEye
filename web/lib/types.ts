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
  | "overview";

export const VIEWS: readonly View[] = [
  "flamegraph",
  "compare",
  "top",
  "timeseries",
  "labels",
  "servicemap",
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
   * Sample rows under each node, parallel to `names`. Present on new
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
}

export interface ServiceMapEdge {
  client: string;
  server: string;
  protocol: string;
  /** Container of the client pod that opened the connections, when known. */
  client_container?: string;
  /** Container of the server pod that accepted them, when known. */
  server_container?: string;
  connections: number;
  retransmits?: number;
  /** Bytes the client sent to the server over the window. */
  tx_bytes?: number;
  /** Bytes the server sent back. */
  rx_bytes?: number;
  /** Average round-trip time in microseconds; 0 when nothing closed. */
  rtt_us?: number;
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
  pods?: number;
  ready?: number;
}

export interface MapHistoryPayload {
  node: string;
  from_ns: number;
  until_ns: number;
  step_ns: number;
  points: MapHistoryPoint[];
  edges: MapHistoryPoint[];
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
