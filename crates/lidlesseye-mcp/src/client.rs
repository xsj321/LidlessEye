//! Thin HTTP client for the LidlessEye query API.
//!
//! The MCP process is normally a stdio child of an AI client. Reusing the
//! server's `Store` here would mean linking DuckDB, copying tenant resolution
//! and duplicating query cancellation. Calling the HTTP API keeps the MCP
//! binary small and makes the server the single owner of auth and query
//! semantics.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::{Client, StatusCode, Url};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Echo of the query the model asked for.
///
/// Returning the endpoint, tenant and raw parameters with every answer lets an
/// AI explain where a number came from and reuse the exact same window in a
/// follow-up tool call.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(crate) struct QueryContext {
    /// HTTP path that produced the answer.
    pub endpoint: String,
    /// Tenant sent to the server.
    pub tenant: String,
    /// Raw query parameters, sorted by key.
    pub params: BTreeMap<String, String>,
}

/// Common shape of every tool result.
///
/// `summary` is written for a model and a human reading a transcript; `data`
/// is a bounded, already-ranked subset of the API payload. Nothing here is a
/// full flamegraph: those are available in the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(crate) struct AnalysisResponse {
    /// One or two sentences stating what the data shows and its unit.
    pub summary: String,
    /// Exact request that produced the data.
    pub query_context: QueryContext,
    /// Unit of the numeric fields, when the endpoint reports one.
    pub unit: Option<String>,
    /// True when rows or buckets were left out to protect the model context.
    pub truncated: bool,
    /// Ranked data subset.
    pub data: Value,
    /// Safe next calls when the answer is empty or inconclusive.
    pub suggestions: Vec<String>,
}

/// Build the shared envelope from a tool's summary and data subset.
pub(crate) fn envelope(
    context: QueryContext,
    summary: impl Into<String>,
    unit: Option<String>,
    truncated: bool,
    data: Value,
    suggestions: Vec<String>,
) -> AnalysisResponse {
    AnalysisResponse {
        summary: summary.into(),
        query_context: context,
        unit,
        truncated,
        data,
        suggestions,
    }
}

/// Client for the server's read-only query endpoints.
#[derive(Debug, Clone)]
pub(crate) struct LidlessEyeClient {
    http: Client,
    base_url: String,
    token: Option<String>,
    tenant: String,
    tenant_header: String,
}

impl LidlessEyeClient {
    /// Validate configuration and build the reusable HTTP client.
    pub(crate) fn new(
        base_url: &str,
        token: Option<String>,
        tenant: &str,
        tenant_header: &str,
        timeout: Duration,
    ) -> Result<Self, String> {
        let parsed = Url::parse(base_url)
            .map_err(|err| format!("invalid --base-url `{base_url}`: {err}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(format!(
                "invalid --base-url `{base_url}`: expected http:// or https://"
            ));
        }
        if tenant.trim().is_empty() {
            return Err("--tenant must not be empty".to_string());
        }
        if tenant_header.trim().is_empty() {
            return Err("--tenant-header must not be empty".to_string());
        }
        let http = Client::builder()
            .timeout(timeout)
            .user_agent(concat!("lidlesseye-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| format!("cannot build HTTP client: {err}"))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            tenant: tenant.to_string(),
            tenant_header: tenant_header.to_string(),
        })
    }

    /// Tenant this client sends on every tenant-scoped request.
    pub(crate) fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Query context for a request, derived from the same parameters that are
    /// actually sent to the server.
    pub(crate) fn context(&self, endpoint: &str, params: &[(String, String)]) -> QueryContext {
        QueryContext {
            endpoint: endpoint.to_string(),
            tenant: self.tenant.clone(),
            params: params.iter().cloned().collect(),
        }
    }

    /// GET a JSON document from the server.
    pub(crate) async fn get_json(
        &self,
        endpoint: &str,
        params: &[(String, String)],
    ) -> Result<Value, String> {
        let url = format!("{}{}", self.base_url, endpoint);
        let mut request = self
            .http
            .get(url)
            .query(params)
            .header(&self.tenant_header, &self.tenant);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|err| {
            format!(
                "cannot reach LidlessEye at {} ({err}). Check --base-url, network access and TLS settings.",
                self.base_url
            )
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|err| {
            format!("LidlessEye returned {status} but its body could not be read: {err}")
        })?;
        if !status.is_success() {
            return Err(describe_http_error(status, &body, &self.tenant));
        }
        serde_json::from_str(&body).map_err(|err| {
            format!(
                "LidlessEye returned invalid JSON from {endpoint}: {err}; body starts with `{}`",
                truncate(&body, 160)
            )
        })
    }
}

/// Turn a server error into feedback the model can act on.
fn describe_http_error(status: StatusCode, body: &str, tenant: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| truncate(body.trim(), 240).to_string());
    let suggestion = match status {
        StatusCode::UNAUTHORIZED => {
            "Set --token (or LIDLESSEYE_AUTH_TOKEN) to a valid LidlessEye token."
        }
        StatusCode::FORBIDDEN => {
            "The token cannot access this tenant; change --tenant or use a token scoped to it."
        }
        StatusCode::NOT_FOUND => {
            "This tenant has no data yet. Call list_tenants to see known tenants, or ingest a profile first."
        }
        StatusCode::BAD_REQUEST => {
            "Check the tool arguments: time values accept `now-1h`, RFC3339 or unix timestamps, and `until` must be later than `from`."
        }
        StatusCode::REQUEST_TIMEOUT | StatusCode::SERVICE_UNAVAILABLE => {
            "The server is busy or the query timed out. Narrow the time window and retry."
        }
        _ => "Inspect the server logs at the same timestamp for details.",
    };
    let _ = tenant;
    format!("LidlessEye API returned HTTP {status}: {detail}. {suggestion}")
}

/// Keep error messages useful without echoing an unbounded response body.
fn truncate(text: &str, limit: usize) -> &str {
    if text.chars().count() <= limit {
        return text;
    }
    let end = text
        .char_indices()
        .nth(limit)
        .map_or(text.len(), |(i, _)| i);
    &text[..end]
}

/// Small JSON conveniences shared by the summary helpers.
pub(crate) fn as_u64(value: Option<&Value>, key: &str) -> u64 {
    value
        .and_then(|v| v.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Read an integer that may be negative (profile deltas, diff values).
pub(crate) fn as_i64(value: Option<&Value>, key: &str) -> i64 {
    value
        .and_then(|v| v.get(key))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// Read a string field, falling back to an empty string.
pub(crate) fn as_str(value: Option<&Value>, key: &str) -> String {
    value
        .and_then(|v| v.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Percentage rounded to two decimals; `None` avoids serializing infinities.
pub(crate) fn percent(part: f64, total: f64) -> Option<f64> {
    if !total.is_finite() || total <= 0.0 || !part.is_finite() {
        return None;
    }
    Some((part / total * 10_000.0).round() / 100.0)
}

/// Dense array helper that never panics on unexpected server data.
pub(crate) fn array<'a>(value: Option<&'a Value>, key: &str) -> &'a [Value] {
    value
        .and_then(|v| v.get(key))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}
