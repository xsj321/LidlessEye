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
  | "overview";

export const VIEWS: readonly View[] = [
  "flamegraph",
  "compare",
  "top",
  "timeseries",
  "labels",
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

export interface ProfileType {
  name: string;
  unit: string;
  total_value: number;
  segment_count: number;
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
  segment_count: number;
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
    segment_count?: number;
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
