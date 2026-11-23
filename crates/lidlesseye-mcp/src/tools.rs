//! MCP tools exposed to an AI client.
//!
//! Every tool is read-only, rank-limited and returns the same typed envelope:
//! a short summary, the exact query context, a bounded data subset and next
//! suggestions. Tool names are stable so clients can remember them.

use lidlesseye_servicemap::{MapHistogram, MapHistory, NetMetric, ServiceMap};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{Json, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{
    AnalysisResponse, LidlessEyeClient, array, as_i64, as_str, as_u64, envelope, percent,
};
use crate::summary::{
    summarize_diff, summarize_flamegraph, summarize_histogram, summarize_map_history,
    summarize_service_map, summarize_timeseries,
};

fn default_from() -> String {
    "now-1h".to_string()
}

fn default_until() -> String {
    "now".to_string()
}

fn default_profile_type() -> String {
    "cpu".to_string()
}

fn default_limit() -> usize {
    20
}

fn default_max_items() -> usize {
    10
}

fn default_buckets() -> usize {
    60
}

/// Time window and selectors shared by the profile query tools.
#[derive(Debug, Deserialize, JsonSchema)]
struct ProfileQuery {
    /// Start of the window: `now-1h`, RFC3339, or a unix timestamp.
    #[serde(default = "default_from")]
    #[schemars(description = "Start of the window: now-1h, RFC3339, or a unix timestamp.")]
    from: String,
    /// End of the window. Must be later than `from`.
    #[serde(default = "default_until")]
    #[schemars(description = "End of the window. Must be later than from.")]
    until: String,
    /// Profile type such as `cpu`, `off-cpu`, `wall` or `inuse_space`.
    #[serde(default = "default_profile_type")]
    #[schemars(description = "Profile type: cpu, off-cpu, wall, inuse_space, alloc_space, ...")]
    profile_type: String,
    /// PromQL-style selector, for example `{service_name="checkout"}`.
    #[serde(default)]
    selector: Option<String>,
    /// Shorthand for `service_name="..."`.
    #[serde(default)]
    service_name: Option<String>,
    /// Split the result by one label value.
    #[serde(default)]
    group_by: Option<String>,
}

impl ProfileQuery {
    fn params(&self) -> Vec<(String, String)> {
        let mut params = vec![
            ("from".to_string(), self.from.clone()),
            ("until".to_string(), self.until.clone()),
            ("profile_type".to_string(), self.profile_type.clone()),
        ];
        if let Some(selector) = &self.selector {
            params.push(("selector".to_string(), selector.clone()));
        }
        if let Some(service) = &self.service_name {
            params.push(("service_name".to_string(), service.clone()));
        }
        if let Some(group_by) = &self.group_by {
            params.push(("group_by".to_string(), group_by.clone()));
        }
        params
    }
}

/// Time window for the service-map tools.
#[derive(Debug, Deserialize, JsonSchema)]
struct MapQuery {
    /// Start of the window: `now-1h`, RFC3339, or a unix timestamp.
    #[serde(default = "default_from")]
    from: String,
    /// End of the window. Must be later than `from`.
    #[serde(default = "default_until")]
    until: String,
}

impl MapQuery {
    fn params(&self) -> Vec<(String, String)> {
        vec![
            ("from".to_string(), self.from.clone()),
            ("until".to_string(), self.until.clone()),
        ]
    }
}

/// Parameters for a two-window profile comparison.
#[derive(Debug, Deserialize, JsonSchema)]
struct DiffQuery {
    /// Baseline start; defaults to the same length before `from`.
    #[serde(default)]
    baseline_from: Option<String>,
    /// Baseline end; defaults to `from`.
    #[serde(default)]
    baseline_until: Option<String>,
    /// Comparison window and profile selection.
    #[serde(flatten)]
    query: ProfileQuery,
}

/// Parameters for a frame-ranked flamegraph summary.
#[derive(Debug, Deserialize, JsonSchema)]
struct FlamegraphQuery {
    /// Maximum number of frames to return, 1-50.
    #[serde(default = "default_limit")]
    max_frames: usize,
    /// Query window and profile selection.
    #[serde(flatten)]
    query: ProfileQuery,
}

/// Parameters for a ranked top-function table.
#[derive(Debug, Deserialize, JsonSchema)]
struct TopQuery {
    /// Maximum number of functions to return, 1-100.
    #[serde(default = "default_limit")]
    limit: usize,
    /// Query window and profile selection.
    #[serde(flatten)]
    query: ProfileQuery,
}

/// Parameters for a bucketed time series.
#[derive(Debug, Deserialize, JsonSchema)]
struct TimeseriesQuery {
    /// Number of buckets; ignored when `step` is given.
    #[serde(default = "default_buckets")]
    buckets: usize,
    /// Explicit bucket width such as `1m` or `30s`.
    #[serde(default)]
    step: Option<String>,
    /// Maximum number of series to return, 1-50.
    #[serde(default = "default_max_items")]
    max_series: usize,
    /// Query window and profile selection.
    #[serde(flatten)]
    query: ProfileQuery,
}

/// Parameters for one node's service-map history.
#[derive(Debug, Deserialize, JsonSchema)]
struct MapHistoryQuery {
    /// Node id from `get_service_map`, for example `default/checkout`.
    node: String,
    /// Number of buckets in the window, 1-600.
    #[serde(default = "default_buckets")]
    buckets: usize,
    /// Query window.
    #[serde(flatten)]
    query: MapQuery,
}

/// Parameters for a service-map traffic histogram.
#[derive(Debug, Deserialize, JsonSchema)]
struct MapHistogramQuery {
    /// Counter to plot: `connections`, `bytes` or `retransmits`.
    #[serde(default = "default_connections")]
    metric: String,
    /// Number of buckets in the window, 1-600.
    #[serde(default = "default_buckets")]
    buckets: usize,
    /// Query window.
    #[serde(flatten)]
    query: MapQuery,
}

fn default_connections() -> String {
    "connections".to_string()
}

/// Parameters for label value discovery.
#[derive(Debug, Deserialize, JsonSchema)]
struct LabelValuesQuery {
    /// Label name, for example `service_name` or `app.kubernetes.io/name`.
    name: String,
    /// Maximum number of values to return, 1-100.
    #[serde(default = "default_limit")]
    limit: usize,
    /// Query window and profile selection.
    #[serde(flatten)]
    query: ProfileQuery,
}

/// MCP handler holding only an HTTP client and its generated tool router.
#[derive(Clone)]
pub(crate) struct LidlessEyeTools {
    client: LidlessEyeClient,
    tool_router: ToolRouter<Self>,
}

impl LidlessEyeTools {
    pub(crate) fn new(client: LidlessEyeClient) -> Self {
        Self {
            client,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LidlessEyeTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "lidlesseye-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Read-only LidlessEye profiling and service-map queries. Prefer list_tenants and list_series to discover what exists, then use short time windows and narrow selectors. Every result is a summary with explicit units and truncation flags.",
            )
    }
}

#[tool_router(router = tool_router)]
impl LidlessEyeTools {
    /// Check connectivity and learn the server version.
    #[tool(
        name = "get_health",
        description = "Check LidlessEye server connectivity and version before running data queries."
    )]
    pub(crate) async fn get_health(&self) -> Result<Json<AnalysisResponse>, String> {
        let params: Vec<(String, String)> = Vec::new();
        let context = self.client.context("/api/v1/health", &params);
        let raw = self.client.get_json("/api/v1/health", &params).await?;
        let status = as_str(Some(&raw), "status");
        let version = as_str(Some(&raw), "version");
        let summary = if status == "ok" {
            format!("LidlessEye {version} is healthy and reachable at the configured base URL.")
        } else {
            format!("LidlessEye reported status `{status}`; version {version}.")
        };
        Ok(Json(envelope(
            context,
            summary,
            None,
            false,
            raw,
            Vec::new(),
        )))
    }

    /// List tenants known to the server.
    #[tool(
        name = "list_tenants",
        description = "List tenants and their data volume. Use this first when a tenant is unknown or a query says there is no data."
    )]
    pub(crate) async fn list_tenants(&self) -> Result<Json<AnalysisResponse>, String> {
        let params: Vec<(String, String)> = Vec::new();
        let context = self.client.context("/api/v1/tenants", &params);
        let raw = self.client.get_json("/api/v1/tenants", &params).await?;
        let tenants = array(Some(&raw), "tenants");
        let summary = format!(
            "The server reports {} tenant(s); this MCP client is configured for `{}`.",
            tenants.len(),
            self.client.tenant()
        );
        Ok(Json(envelope(
            context,
            summary,
            None,
            false,
            raw,
            Vec::new(),
        )))
    }

    /// Discover profile types that actually have data in a window.
    #[tool(
        name = "list_profile_types",
        description = "List profile types available in a window with their units and totals. Use it before choosing profile_type for flamegraph, top or diff."
    )]
    pub(crate) async fn list_profile_types(
        &self,
        Parameters(query): Parameters<ProfileQuery>,
    ) -> Result<Json<AnalysisResponse>, String> {
        let params = query.params();
        let context = self.client.context("/api/v1/profile_types", &params);
        let raw = self
            .client
            .get_json("/api/v1/profile_types", &params)
            .await?;
        let types = array(Some(&raw), "profile_types").to_vec();
        let summary = if types.is_empty() {
            "No profile types have data in this window. Try a wider window, another tenant or list_tenants.".to_string()
        } else {
            format!(
                "{} profile type(s) have data in this window; units are reported per type.",
                types.len()
            )
        };
        let suggestion = if types.is_empty() {
            vec![
                "Widen `from`/`until` or call list_tenants.".to_string(),
                "Check that an agent is sending profiles with `collect`.".to_string(),
            ]
        } else {
            Vec::new()
        };
        Ok(Json(envelope(
            context,
            summary,
            None,
            false,
            json!({ "profile_types": types }),
            suggestion,
        )))
    }

    /// Get a ranked, bounded service map.
    #[tool(
        name = "get_service_map",
        description = "Get a bounded service-map summary: top nodes and edges by connections/bytes, with RTT in milliseconds. Use node ids from this result with get_service_map_history."
    )]
    pub(crate) async fn get_service_map(
        &self,
        Parameters(query): Parameters<MapQuery>,
    ) -> Result<Json<AnalysisResponse>, String> {
        let params = query.params();
        let context = self.client.context("/api/v1/servicemap", &params);
        let raw = self.client.get_json("/api/v1/servicemap", &params).await?;
        let map: ServiceMap = serde_json::from_value(raw)
            .map_err(|err| format!("cannot parse service map response: {err}"))?;
        let data = summarize_service_map(&map, 10);
        let summary = if map.edges.is_empty() {
            "The service map has no edges in this window. Widen the window, or call list_tenants to confirm the tenant has data.".to_string()
        } else {
            format!(
                "The map has {} node(s) and {} edge(s); the top edges are ranked by connections plus bytes.",
                map.nodes.len(),
                map.edges.len()
            )
        };
        Ok(Json(envelope(
            context,
            summary,
            None,
            map.truncated || as_u64(Some(&data), "node_count") > 10,
            data,
            Vec::new(),
        )))
    }

    /// Get one node's traffic history.
    #[tool(
        name = "get_service_map_history",
        description = "Get one service-map node's own counters and its incident-edge counters over time, including RTT in milliseconds."
    )]
    pub(crate) async fn get_service_map_history(
        &self,
        Parameters(params): Parameters<MapHistoryQuery>,
    ) -> Result<Json<AnalysisResponse>, String> {
        if params.node.trim().is_empty() {
            return Err(
                "`node` is required; call get_service_map and copy an id from top_nodes."
                    .to_string(),
            );
        }
        let buckets = params.buckets.clamp(1, 600);
        let mut query = params.query.params();
        query.push(("node".to_string(), params.node.clone()));
        query.push(("buckets".to_string(), buckets.to_string()));
        let context = self.client.context("/api/v1/servicemap/history", &query);
        let raw = self
            .client
            .get_json("/api/v1/servicemap/history", &query)
            .await?;
        let history: MapHistory = serde_json::from_value(raw)
            .map_err(|err| format!("cannot parse service map history response: {err}"))?;
        let data = summarize_map_history(&history);
        let summary = format!(
            "Node `{}` has {} own bucket(s) and {} edge bucket(s) in this window; bytes are tx+rx and RTT is milliseconds.",
            history.node,
            history.points.len(),
            history.edges.len()
        );
        Ok(Json(envelope(
            context,
            summary,
            Some("connections / bytes / ms".to_string()),
            false,
            data,
            Vec::new(),
        )))
    }

    /// Get a traffic-density histogram.
    #[tool(
        name = "get_service_map_histogram",
        description = "Get total service-map traffic per bucket for connections, bytes or retransmits. Use it to locate a spike before narrowing the time window."
    )]
    pub(crate) async fn get_service_map_histogram(
        &self,
        Parameters(params): Parameters<MapHistogramQuery>,
    ) -> Result<Json<AnalysisResponse>, String> {
        let Some(metric) = NetMetric::parse(&params.metric) else {
            return Err(
                "invalid `metric`; choose one of `connections`, `bytes`, `retransmits`."
                    .to_string(),
            );
        };
        let buckets = params.buckets.clamp(1, 600);
        let mut query = params.query.params();
        query.push(("metric".to_string(), metric.as_str().to_string()));
        query.push(("buckets".to_string(), buckets.to_string()));
        let context = self.client.context("/api/v1/servicemap/histogram", &query);
        let raw = self
            .client
            .get_json("/api/v1/servicemap/histogram", &query)
            .await?;
        let histogram: MapHistogram = serde_json::from_value(raw)
            .map_err(|err| format!("cannot parse service map histogram response: {err}"))?;
        let data = summarize_histogram(&histogram, 32);
        let summary = format!(
            "`{}` totals {} across {} bucket(s); the peak bucket is index {}.",
            histogram.metric,
            histogram.points.iter().sum::<u64>(),
            histogram.points.len(),
            data["peak"]["index"]
        );
        let unit = if metric == NetMetric::Bytes {
            "bytes"
        } else {
            "count"
        };
        Ok(Json(envelope(
            context,
            summary,
            Some(unit.to_string()),
            as_u64(Some(&data), "bucket_count") > 32,
            data,
            Vec::new(),
        )))
    }

    /// Get a flamegraph summary without flooding the model context.
    #[tool(
        name = "get_flamegraph_summary",
        description = "Get a flamegraph's total, sample count and hottest frames. Full frame trees stay in the dashboard; this tool returns only a ranked top-N."
    )]
    pub(crate) async fn get_flamegraph_summary(
        &self,
        Parameters(params): Parameters<FlamegraphQuery>,
    ) -> Result<Json<AnalysisResponse>, String> {
        let max_frames = params.max_frames.clamp(1, 50);
        let mut query = params.query.params();
        query.push(("max_nodes".to_string(), 4096.to_string()));
        let context = self.client.context("/api/v1/flamegraph", &query);
        let raw = self.client.get_json("/api/v1/flamegraph", &query).await?;
        let unit = as_str(Some(&raw), "unit");
        let data = summarize_flamegraph(&raw, max_frames);
        let total = as_i64(Some(&data), "total");
        let summary = if total == 0 {
            "The flamegraph is empty in this window. Check list_profile_types, widen the window or remove selectors.".to_string()
        } else {
            format!(
                "The profile totals {} {} from {} sample(s); top frames are ranked by inclusive value.",
                total,
                unit,
                as_u64(Some(&data), "samples")
            )
        };
        let suggestions = if total == 0 {
            vec![
                "Call list_profile_types to confirm this profile_type has data.".to_string(),
                "Widen the time window or remove the selector.".to_string(),
            ]
        } else {
            Vec::new()
        };
        Ok(Json(envelope(
            context,
            summary,
            Some(unit),
            raw.get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || as_u64(Some(&data), "frame_count") > max_frames as u64,
            data,
            suggestions,
        )))
    }

    /// Get top functions with share-of-total percentages.
    #[tool(
        name = "get_top",
        description = "Get the hottest functions by self value with share-of-total percentages, file and line when known."
    )]
    pub(crate) async fn get_top(
        &self,
        Parameters(params): Parameters<TopQuery>,
    ) -> Result<Json<AnalysisResponse>, String> {
        let limit = params.limit.clamp(1, 100);
        let mut query = params.query.params();
        query.push(("limit".to_string(), limit.to_string()));
        let context = self.client.context("/api/v1/top", &query);
        let raw = self.client.get_json("/api/v1/top", &query).await?;
        let unit = as_str(Some(&raw), "unit");
        let total_value = as_i64(Some(&raw), "total_value");
        let items = array(Some(&raw), "items")
            .iter()
            .map(|item| {
                let self_value = as_i64(Some(item), "self_value");
                json!({
                    "name": as_str(Some(item), "name"),
                    "module": as_str(Some(item), "module"),
                    "file": as_str(Some(item), "file"),
                    "line": as_u64(Some(item), "line"),
                    "self_value": self_value,
                    "total_value": as_i64(Some(item), "total_value"),
                    "self_share_pct": percent(self_value as f64, total_value as f64),
                })
            })
            .collect::<Vec<_>>();
        let summary = if items.is_empty() {
            "No functions have data in this window. Try list_profile_types or a wider window."
                .to_string()
        } else {
            format!(
                "Top {} function(s) by self value; the whole profile totals {} {}.",
                items.len(),
                total_value,
                unit
            )
        };
        Ok(Json(envelope(
            context,
            summary,
            Some(unit),
            items.len() >= limit,
            json!({ "total_value": total_value, "items": items }),
            Vec::new(),
        )))
    }

    /// Compare two windows and rank regressions and improvements.
    #[tool(
        name = "diff_profiles",
        description = "Compare a profile window against a baseline window. Positive deltas are regressions; returns ranked regressions and improvements."
    )]
    pub(crate) async fn diff_profiles(
        &self,
        Parameters(params): Parameters<DiffQuery>,
    ) -> Result<Json<AnalysisResponse>, String> {
        let mut query = params.query.params();
        if let Some(from) = params.baseline_from {
            query.push(("baseline_from".to_string(), from));
        }
        if let Some(until) = params.baseline_until {
            query.push(("baseline_until".to_string(), until));
        }
        query.push(("max_nodes".to_string(), 4096.to_string()));
        let context = self.client.context("/api/v1/diff", &query);
        let raw = self.client.get_json("/api/v1/diff", &query).await?;
        let unit = as_str(Some(&raw), "unit");
        let data = summarize_diff(&raw, 10);
        let summary = format!(
            "Profile changed from {} to {} {} ({}%); positive per-frame deltas are regressions.",
            as_i64(Some(&data), "baseline_total"),
            as_i64(Some(&data), "current_total"),
            unit,
            format_option_pct(data.get("change_pct"))
        );
        Ok(Json(envelope(
            context,
            summary,
            Some(unit),
            raw.get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            data,
            Vec::new(),
        )))
    }

    /// Get bucketed profiling values with per-series totals.
    #[tool(
        name = "get_timeseries",
        description = "Get bucketed profiling values with per-series totals and peaks. Use it to find when a regression starts before calling diff_profiles."
    )]
    pub(crate) async fn get_timeseries(
        &self,
        Parameters(params): Parameters<TimeseriesQuery>,
    ) -> Result<Json<AnalysisResponse>, String> {
        let max_series = params.max_series.clamp(1, 50);
        let buckets = params.buckets.clamp(1, 1000);
        let mut query = params.query.params();
        if let Some(step) = &params.step {
            query.push(("step".to_string(), step.clone()));
        } else {
            query.push(("buckets".to_string(), buckets.to_string()));
        }
        let context = self.client.context("/api/v1/timeseries", &query);
        let raw = self.client.get_json("/api/v1/timeseries", &query).await?;
        let data = summarize_timeseries(&raw, max_series);
        let summary = format!(
            "The time series has {} series over {} bucket(s); series are ranked by total value.",
            as_u64(Some(&data), "series_count"),
            array(Some(&raw), "series")
                .first()
                .map(|entry| array(Some(entry), "points").len())
                .unwrap_or(0)
        );
        Ok(Json(envelope(
            context,
            summary,
            Some(params.query.profile_type.clone()),
            as_u64(Some(&data), "series_count") > max_series as u64,
            data,
            Vec::new(),
        )))
    }

    /// Discover services and label sets that have data.
    #[tool(
        name = "list_series",
        description = "List services, profile types and label sets present in a window. Use this to build a valid selector before querying a flamegraph or a node."
    )]
    pub(crate) async fn list_series(
        &self,
        Parameters(query): Parameters<ProfileQuery>,
    ) -> Result<Json<AnalysisResponse>, String> {
        let params = query.params();
        let context = self.client.context("/api/v1/series", &params);
        let raw = self.client.get_json("/api/v1/series", &params).await?;
        let mut rows = array(Some(&raw), "series").to_vec();
        rows.sort_by(|a, b| {
            as_u64(Some(b), "total_value")
                .cmp(&as_u64(Some(a), "total_value"))
                .then_with(|| as_str(Some(a), "service_name").cmp(&as_str(Some(b), "service_name")))
        });
        let summary = format!(
            "{} series have data in this window; the largest are listed first with their label sets.",
            rows.len()
        );
        let suggestions = if rows.is_empty() {
            vec![
                "Call list_tenants to confirm the tenant has data.".to_string(),
                "Widen the time window.".to_string(),
            ]
        } else {
            Vec::new()
        };
        Ok(Json(envelope(
            context,
            summary,
            None,
            false,
            json!({ "tenant": as_str(Some(&raw), "tenant"), "series": rows }),
            suggestions,
        )))
    }

    /// Discover the values of one label.
    #[tool(
        name = "list_label_values",
        description = "List the most common values of one label in a window. Use it to discover service names, pods and other selector values."
    )]
    pub(crate) async fn list_label_values(
        &self,
        Parameters(params): Parameters<LabelValuesQuery>,
    ) -> Result<Json<AnalysisResponse>, String> {
        let name = params.name.trim();
        if name.is_empty() {
            return Err(
                "`name` is required; call list_series or get_profile_types first.".to_string(),
            );
        }
        let limit = params.limit.clamp(1, 100);
        let mut query = params.query.params();
        query.push(("limit".to_string(), limit.to_string()));
        let endpoint = format!("/api/v1/label/{}/values", encode_path_segment(name));
        let context = self.client.context(&endpoint, &query);
        let raw = self.client.get_json(&endpoint, &query).await?;
        let mut values = array(Some(&raw), "values").to_vec();
        values.sort_by(|a, b| {
            as_u64(Some(b), "total")
                .cmp(&as_u64(Some(a), "total"))
                .then_with(|| as_str(Some(a), "value").cmp(&as_str(Some(b), "value")))
        });
        let truncated = values.len() > limit;
        values.truncate(limit);
        let summary = format!(
            "Label `{}` has {} value(s) in this window; values are ranked by total.",
            name,
            values.len()
        );
        Ok(Json(envelope(
            context,
            summary,
            None,
            truncated,
            json!({ "label": name, "values": values }),
            Vec::new(),
        )))
    }
}

/// Percent formatting that degrades gracefully when the baseline is zero.
fn format_option_pct(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_f64)
        .map(|number| format!("{number:.2}"))
        .unwrap_or_else(|| "n/a".to_string())
}

/// Percent-encode one URL path segment without adding a URL crate to the API.
fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(*byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_names_with_slashes_are_encoded() {
        assert_eq!(
            encode_path_segment("app.kubernetes.io/name"),
            "app.kubernetes.io%2Fname"
        );
    }

    #[test]
    fn profile_params_include_the_selectors() {
        let query = ProfileQuery {
            from: "now-2h".to_string(),
            until: "now".to_string(),
            profile_type: "inuse_space".to_string(),
            selector: Some("{pod=\"a\"}".to_string()),
            service_name: None,
            group_by: Some("pod".to_string()),
        };
        let params = query.params();
        assert!(params.contains(&("profile_type".to_string(), "inuse_space".to_string())));
        assert!(params.contains(&("group_by".to_string(), "pod".to_string())));
    }
}
