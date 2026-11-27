// Thin wrapper over the LidlessEye HTTP API.
//
// Every call goes through `request()`, which attaches the bearer token (kept in
// localStorage) and turns non-2xx responses into `ApiError`s that carry the
// server's own error message.

import type {
  DiffPayload,
  FlamegraphPayload,
  LabelNamesPayload,
  LabelValuesPayload,
  MapHistogramPayload,
  DirectivesPayload,
  MapHistoryPayload,
  CapabilityPayload,
  MemoryCoveragePayload,
  MemoryProcessPayload,
  ProfileTypesPayload,
  SeriesPayload,
  ServiceMapPayload,
  StatsPayload,
  TenantsPayload,
  TimeseriesPayload,
  TopPayload,
  VersionPayload,
} from "./types";

const TOKEN_KEY = "lidlesseye.token";
const TENANT_KEY = "lidlesseye.tenant";

export class ApiError extends Error {
  status: number;

  constructor(message: string, status: number) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

export function getToken(): string {
  try {
    return localStorage.getItem(TOKEN_KEY) ?? "";
  } catch {
    return "";
  }
}

export function setToken(token: string): void {
  try {
    if (token) localStorage.setItem(TOKEN_KEY, token);
    else localStorage.removeItem(TOKEN_KEY);
  } catch {
    /* private mode: the token simply does not persist */
  }
}

/** Tenant the dashboard reads and writes. Empty means "server default". */
export function getTenant(): string {
  try {
    return localStorage.getItem(TENANT_KEY) ?? "";
  } catch {
    return "";
  }
}

export function setTenant(tenant: string): void {
  try {
    if (tenant) localStorage.setItem(TENANT_KEY, tenant);
    else localStorage.removeItem(TENANT_KEY);
  } catch {
    /* ignore */
  }
}

export type QueryParams = Record<string, string | number | undefined | null>;

function buildQuery(params: QueryParams | undefined): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params ?? {})) {
    if (value === undefined || value === null || value === "") continue;
    search.set(key, String(value));
  }
  const query = search.toString();
  return query ? `?${query}` : "";
}

async function request<T>(
  path: string,
  params?: QueryParams,
  init?: RequestInit,
): Promise<T> {
  // An aborted fetch throws an AbortError; callers treat that as a superseded
  // query, not a failure. Let it propagate verbatim.
  const headers = new Headers(init?.headers);
  const token = getToken();
  if (token) headers.set("Authorization", `Bearer ${token}`);
  const tenant = getTenant();
  // `X-Tenant-Id` is the alias the server accepts whatever `--tenant-header`
  // is set to, so the dashboard keeps working when the header is renamed.
  if (tenant) headers.set("X-Tenant-Id", tenant);

  let response: Response;
  const send = async (): Promise<Response> => {
    try {
      return await fetch(path + buildQuery(params), { ...init, headers });
    } catch (err) {
      // A deliberate abort (a superseded query) must stay distinguishable from a
      // real network failure, so callers can ignore it instead of showing an
      // error banner.
      if (err instanceof Error && err.name === "AbortError") throw err;
      throw new ApiError(`Network error: ${(err as Error).message}`, 0);
    }
  };
  response = await send();
  // A token that is scoped to one tenant rejects any tenant named in the
  // header, and this browser persists that name in localStorage. A stale one —
  // say `default`, saved before the token was configured — would keep every
  // request 403 until storage is cleared by hand, with no way out from the UI.
  // The token already decides the tenant, so drop the header and ask again.
  if (response.status === 403 && tenant) {
    const refusal = await response.clone().text();
    if (refusal.includes("is scoped to tenant")) {
      setTenant("");
      headers.delete("X-Tenant-Id");
      response = await send();
    }
  }
  if (!response.ok) {
    let message = `${response.status} ${response.statusText}`;
    try {
      const body = (await response.json()) as { error?: string };
      if (body?.error) message = body.error;
    } catch {
      /* not JSON: keep the status line */
    }
    if (response.status === 401) {
      message = "unauthorized — set an API token with `lidlesseye.token` in localStorage";
    }
    throw new ApiError(message, response.status);
  }
  const type = response.headers.get("content-type") ?? "";
  if (!type.includes("application/json")) {
    return (await response.text()) as unknown as T;
  }
  return (await response.json()) as T;
}

// Every query accepts an optional AbortSignal. When the user switches view or
// moves the time window, the store aborts the previous controller so the
// browser drops the in-flight request and the server can interrupt its query.
export const api = {
  health: (signal?: AbortSignal) => request<unknown>("/api/v1/health", undefined, { signal }),
  version: (signal?: AbortSignal) => request<VersionPayload>("/api/v1/version", undefined, { signal }),
  stats: (signal?: AbortSignal) => request<StatsPayload>("/api/v1/stats", undefined, { signal }),
  tenants: (signal?: AbortSignal) => request<TenantsPayload>("/api/v1/tenants", undefined, { signal }),

  profileTypes: (params?: QueryParams, signal?: AbortSignal) =>
    request<ProfileTypesPayload>("/api/v1/profile_types", params, { signal }),
  series: (params?: QueryParams, signal?: AbortSignal) =>
    request<SeriesPayload>("/api/v1/series", params, { signal }),
  labelNames: (params?: QueryParams, signal?: AbortSignal) =>
    request<LabelNamesPayload>("/api/v1/label-names", params, { signal }),
  labelValues: (name: string, params?: QueryParams, signal?: AbortSignal) =>
    request<LabelValuesPayload>(`/api/v1/label/${encodeURIComponent(name)}/values`, params, { signal }),

  // 50k nodes is the server-side ceiling (`QueryOptions::with_max_nodes`); the
  // canvas is happy with that, and it keeps the response in the low megabytes.
  flamegraph: (params: QueryParams, signal?: AbortSignal) =>
    request<FlamegraphPayload>("/api/v1/flamegraph", { max_nodes: 50000, ...params }, { signal }),
  diff: (params: QueryParams, signal?: AbortSignal) =>
    request<DiffPayload>("/api/v1/diff", { max_nodes: 50000, ...params }, { signal }),
  top: (params: QueryParams, signal?: AbortSignal) =>
    request<TopPayload>("/api/v1/top", { limit: 100, ...params }, { signal }),
  timeseries: (params: QueryParams, signal?: AbortSignal) =>
    request<TimeseriesPayload>("/api/v1/timeseries", { buckets: 90, ...params }, { signal }),

  // The map follows the same from/until window as the flamegraph; profile_type
  // and selector are deliberately not sent, it is a network view.
  serviceMap: (params: QueryParams, signal?: AbortSignal) =>
    request<ServiceMapPayload>("/api/v1/servicemap", params, { signal }),
  // One node's own buckets, for the panel behind a click.
  serviceMapHistory: (params: QueryParams, signal?: AbortSignal) =>
    request<MapHistoryPayload>("/api/v1/servicemap/history", params, { signal }),
  // Whole-map request counts for the histogram brush above the map.
  serviceMapHistogram: (params: QueryParams, signal?: AbortSignal) =>
    request<MapHistogramPayload>("/api/v1/servicemap/histogram", params, { signal }),

  // The numbers behind a native allocation figure: bytes, batches, sampling
  // interval, reader, and whether the workload is still switched on.
  memoryCoverage: (params: QueryParams, signal?: AbortSignal) =>
    request<MemoryCoveragePayload>("/api/v1/memory/coverage", params, { signal }),
  // Workloads whose runtime memory profiling is switched on. The map draws the
  // switch from this list and every agent polls the same answer, so what the
  // panel shows is what the agents are doing.
  profilingDirectives: (signal?: AbortSignal) =>
    request<DirectivesPayload>("/api/v1/profiling/directives", undefined, { signal }),
  capabilities: (signal?: AbortSignal) =>
    request<CapabilityPayload>("/api/v1/capabilities", undefined, { signal }),
  memoryProcesses: (
    params: { workload: string; limit?: string },
    signal?: AbortSignal,
  ) =>
    request<MemoryProcessPayload>(
      `/api/v1/memory/processes?workload=${encodeURIComponent(params.workload)}${
        params.limit ? `&limit=${params.limit}` : ""
      }`,
      undefined,
      { signal },
    ),  setProfilingDirective: (
    workload: string,
    enabled: boolean,
    kind: "jvm" | "alloc" = "jvm",
    signal?: AbortSignal,
  ) =>
    request<DirectivesPayload>("/api/v1/profiling/directives", undefined, {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ workload, enabled, kind }),
      signal,
    }),
};
