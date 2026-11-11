// Thin wrapper over the LidlessEye HTTP API.
//
// Every call goes through `request()`, which attaches the bearer token (kept in
// localStorage) and turns non-2xx responses into `ApiError`s that carry the
// server's own error message.

const TOKEN_KEY = "lidlesseye.token";

export class ApiError extends Error {
  constructor(message, status) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

export function getToken() {
  try {
    return localStorage.getItem(TOKEN_KEY) ?? "";
  } catch {
    return "";
  }
}

export function setToken(token) {
  try {
    if (token) localStorage.setItem(TOKEN_KEY, token);
    else localStorage.removeItem(TOKEN_KEY);
  } catch {
    /* private mode: the token simply does not persist */
  }
}

function buildQuery(params) {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params ?? {})) {
    if (value === undefined || value === null || value === "") continue;
    search.set(key, String(value));
  }
  const query = search.toString();
  return query ? `?${query}` : "";
}

async function request(path, params, init) {
  const headers = new Headers(init?.headers);
  const token = getToken();
  if (token) headers.set("Authorization", `Bearer ${token}`);

  let response;
  try {
    response = await fetch(path + buildQuery(params), { ...init, headers });
  } catch (err) {
    throw new ApiError(`network error: ${err.message}`, 0);
  }
  if (!response.ok) {
    let message = `${response.status} ${response.statusText}`;
    try {
      const body = await response.json();
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
  if (!type.includes("application/json")) return response.text();
  return response.json();
}

export const api = {
  health: () => request("/api/v1/health"),
  version: () => request("/api/v1/version"),
  stats: () => request("/api/v1/stats"),

  profileTypes: (params) => request("/api/v1/profile_types", params),
  series: (params) => request("/api/v1/series", params),
  labelValues: (name, params) =>
    request(`/api/v1/label/${encodeURIComponent(name)}/values`, params),

  flamegraph: (params) =>
    request("/api/v1/flamegraph", { max_nodes: 200000, ...params }),
  top: (params) => request("/api/v1/top", { limit: 100, ...params }),
  timeseries: (params) => request("/api/v1/timeseries", { buckets: 90, ...params }),
};
