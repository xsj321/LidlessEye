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
  ProfileTypesPayload,
  SeriesPayload,
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
  const headers = new Headers(init?.headers);
  const token = getToken();
  if (token) headers.set("Authorization", `Bearer ${token}`);
  const tenant = getTenant();
  // `X-Tenant-Id` is the alias the server accepts whatever `--tenant-header`
  // is set to, so the dashboard keeps working when the header is renamed.
  if (tenant) headers.set("X-Tenant-Id", tenant);

  let response: Response;
  try {
    response = await fetch(path + buildQuery(params), { ...init, headers });
  } catch (err) {
    throw new ApiError(`network error: ${(err as Error).message}`, 0);
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

export const api = {
  health: () => request<unknown>("/api/v1/health"),
  version: () => request<VersionPayload>("/api/v1/version"),
  stats: () => request<StatsPayload>("/api/v1/stats"),
  tenants: () => request<TenantsPayload>("/api/v1/tenants"),

  profileTypes: (params?: QueryParams) =>
    request<ProfileTypesPayload>("/api/v1/profile_types", params),
  series: (params?: QueryParams) => request<SeriesPayload>("/api/v1/series", params),
  labelNames: (params?: QueryParams) =>
    request<LabelNamesPayload>("/api/v1/label-names", params),
  labelValues: (name: string, params?: QueryParams) =>
    request<LabelValuesPayload>(`/api/v1/label/${encodeURIComponent(name)}/values`, params),

  // 50k nodes is the server-side ceiling (`QueryOptions::with_max_nodes`); the
  // canvas is happy with that, and it keeps the response in the low megabytes.
  flamegraph: (params: QueryParams) =>
    request<FlamegraphPayload>("/api/v1/flamegraph", { max_nodes: 50000, ...params }),
  diff: (params: QueryParams) =>
    request<DiffPayload>("/api/v1/diff", { max_nodes: 50000, ...params }),
  top: (params: QueryParams) => request<TopPayload>("/api/v1/top", { limit: 100, ...params }),
  timeseries: (params: QueryParams) =>
    request<TimeseriesPayload>("/api/v1/timeseries", { buckets: 90, ...params }),
};
