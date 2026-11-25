//! HTTP handlers.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Value, json};

use lidlesseye_core::folded::FoldedOptions;
use lidlesseye_core::model::{Report, Unit};
use lidlesseye_core::pprof::PprofOptions;
use lidlesseye_core::servicemap::ServiceMapBatch;
use lidlesseye_core::{
    DiffFlamegraph, DiffOptions, Flamegraph, MapHistogram, MapHistory, ProfileType, QueryOptions,
    Selector, ServiceMap, Store, TenantId, TenantInfo, TimeSeries, TimeSeriesOptions, TopResult,
    now_ns,
};

use crate::error::ApiError;
use crate::params::Params;
use crate::{AppState, Caller, HealthResponse, StatsResponse, VERSION};

/// Ingestion accepts a single report or a batch.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IngestPayload {
    Batch { reports: Vec<Report> },
    Single(Report),
}

/// `POST /ingest`
///
/// Accepts JSON ([`Report`] or `{"reports": [...]}`), `?format=pprof` for the
/// protobuf profiles every Pyroscope SDK and Go `net/http/pprof` produce, and
/// `?format=folded` for the text `stackcollapse` format.
///
/// Query parameters can override `service_name`, `profile_type`, `from` and
/// `until`, which makes ad-hoc ingestion with `curl` practical.
pub async fn ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
    body: Bytes,
) -> Result<Response, ApiError> {
    let caller = state.authorize(&headers)?;
    if state.config().read_only {
        return Err(ApiError::forbidden("server is read-only"));
    }
    state
        .metrics()
        .ingest_requests
        .fetch_add(1, Ordering::Relaxed);
    if body.is_empty() {
        return Err(ApiError::bad_request("empty request body"));
    }

    let format = ingest_format(&headers, &params);
    let now = now_ns();
    let mut reports = match format {
        "json" => match serde_json::from_slice::<IngestPayload>(&body) {
            Ok(IngestPayload::Single(report)) => vec![report],
            Ok(IngestPayload::Batch { reports }) => reports,
            Err(err) => {
                state
                    .metrics()
                    .ingest_errors
                    .fetch_add(1, Ordering::Relaxed);
                return Err(ApiError::bad_request(format!(
                    "invalid report JSON: {err}; send `?format=pprof` for a protobuf profile"
                )));
            }
        },
        "pprof" => {
            let opts = PprofOptions {
                service_name: params
                    .get("service_name")
                    .or_else(|| params.get("name"))
                    .map(|value| value.to_string()),
                profile_type: params
                    .get("profile_type")
                    .or_else(|| params.get("spy-name"))
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| value.to_string()),
                labels: params.labels(),
                start_ns: None,
                end_ns: None,
            };
            lidlesseye_core::decode_pprof(&body, &opts).map_err(bad_payload)?
        }
        "folded" => {
            let service_name = params
                .get("service_name")
                .or_else(|| params.get("name"))
                .ok_or_else(|| {
                    ApiError::bad_request(
                        "folded ingestion needs `?service_name=<name>` (or `?name=`)",
                    )
                })?;
            let (from, until) = params.range(now)?;
            let text = std::str::from_utf8(&body).map_err(|err| {
                ApiError::bad_request(format!("folded stacks are not UTF-8: {err}"))
            })?;
            let opts = FoldedOptions {
                service_name: service_name.to_string(),
                profile_type: params.profile_type(),
                start_ns: from,
                end_ns: until,
                labels: params.labels(),
            };
            vec![lidlesseye_core::decode_folded(text, &opts).map_err(bad_payload)?]
        }
        other => {
            return Err(ApiError::bad_request(format!(
                "unsupported format `{other}`; expected `json`, `pprof` or `folded`"
            )));
        }
    };

    // JSON uploads may be rewritten by query parameters; structured formats
    // already did that through their options.
    if format == "json" {
        for report in &mut reports {
            if let Some(name) = params.get("service_name").or_else(|| params.get("name")) {
                report.service_name = name.to_string();
            }
            if let Some(raw) = params.get("profile_type").or_else(|| params.get("type")) {
                report.profile_type = ProfileType::new(raw);
            }
            if params.get("from").is_some() || params.get("until").is_some() {
                let (from, until) = params.range(now)?;
                report.start_ns = from;
                report.end_ns = until;
            }
        }
    }
    for report in &reports {
        report.validate()?;
    }

    let count = reports.len();
    let samples: usize = reports.iter().map(|r| r.stacks.len()).sum();
    let metas = state.ingest_many(&caller.tenant, reports).await?;
    state
        .metrics()
        .ingest_bytes
        .fetch_add(body.len() as u64, Ordering::Relaxed);

    let batch_ids: Vec<String> = metas
        .iter()
        .map(|m| format!("batch-{}", m.batch_id))
        .collect();
    Ok(Json(json!({
        "accepted": count,
        "batches": batch_ids.len(),
        "samples": samples,
        "bytes": body.len(),
        "format": format,
        "tenant": caller.tenant.to_string(),
        "ids": batch_ids,
    }))
    .into_response())
}

fn bad_payload(err: lidlesseye_core::Error) -> ApiError {
    ApiError::bad_request(err.to_string())
}

/// `POST /api/v1/flush`
///
/// Move the caller tenant's in-memory hot tier to local Parquet before the
/// configured cadence. The hot tier belongs to the running server process, so
/// this must be a server endpoint: opening the data directory in another
/// process would only create a second, empty hot tier.
pub async fn flush(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let caller = state.authorize(&headers)?;
    let store = state.writable_tenant_async(&caller.tenant).await?;
    let mode = store.config().storage_mode.canonical().as_str().to_string();
    let rows = tokio::task::spawn_blocking(move || store.flush_hot())
        .await
        .map_err(|err| ApiError::internal(format!("flush task failed: {err}")))?
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "tenant": caller.tenant.to_string(),
        "rows": rows,
        "mode": mode,
    }))
    .into_response())
}

/// Decide how to read the body: explicit `format=` wins, then the content
/// type, then a weak heuristic.
fn ingest_format(headers: &HeaderMap, params: &Params) -> &'static str {
    if let Some(format) = params.get("format") {
        return match format.to_ascii_lowercase().as_str() {
            "json" => "json",
            "pprof" | "protobuf" | "proto" => "pprof",
            "folded" | "collapsed" | "text" => "folded",
            _ => "unknown",
        };
    }
    match headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
    {
        ct if ct.starts_with("application/vnd.google.protobuf")
            || ct.starts_with("application/x-protobuf") =>
        {
            "pprof"
        }
        ct if ct.starts_with("text/plain") => "folded",
        _ => "json",
    }
}

/// Store of the caller's tenant, or `None` when the tenant has no data yet.
///
/// Tenants are opened on demand, and a first open can spend tens of seconds
/// discovering S3 objects. Doing that on a Tokio worker starves the server:
/// with four workers, four cold requests leave no thread to answer the
/// liveness probe, and Kubernetes kills a perfectly healthy process. The open
/// therefore runs on the blocking pool, where it cannot stall the listener.
async fn maybe_store(state: &AppState, caller: &Caller) -> Option<Arc<Store>> {
    let registry = state.registry().clone();
    let tenant = caller.tenant.clone();
    tokio::task::spawn_blocking(move || registry.tenant(&tenant))
        .await
        .ok()
        .flatten()
}

/// `GET /api/v1/flamegraph`
pub async fn flamegraph(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    let caller = state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let profile_type = params.profile_type();
    let opts = QueryOptions::new(profile_type.clone(), from, until, params.selector()?)
        .with_max_nodes(params.usize_param("max_nodes", 1024))
        .with_group_by(params.group_by());

    let Some(store) = maybe_store(&state, &caller).await else {
        return Ok(Json(Flamegraph::empty(profile_type.unit())).into_response());
    };
    let _slot = state.read_slot().await?;
    let started = Instant::now();
    let result = state
        .query_guard(store.clone())
        .run(move || lidlesseye_core::flamegraph(&store, &opts))
        .await
        .map_err(ApiError::from)?;
    state.record_query(&caller.tenant, started);

    if params.get("format") == Some("tree") {
        return Ok(Json(json!({
            "tree": result.to_tree(),
            "total": result.total,
            "unit": result.unit,
        }))
        .into_response());
    }
    Ok(Json(result).into_response())
}

/// `GET /api/v1/diff`
///
/// `from`/`until` describe the comparison window and `baseline_from`/
/// `baseline_until` the window it is compared against.
pub async fn diff(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    let caller = state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    // Default baseline: the same length, immediately before the window.
    let span = until - from;
    let baseline_until = match params.get("baseline_until") {
        Some(value) => crate::params::parse_time_ns(value, now)?,
        None => from,
    };
    let baseline_from = match params.get("baseline_from") {
        Some(value) => crate::params::parse_time_ns(value, now)?,
        None => baseline_until.saturating_sub(span),
    };
    let profile_type = params.profile_type();
    let opts = DiffOptions {
        profile_type: profile_type.clone(),
        baseline_from_ns: baseline_from,
        baseline_until_ns: baseline_until,
        from_ns: from,
        until_ns: until,
        selector: params.selector()?,
        max_nodes: params.usize_param("max_nodes", 1024).clamp(16, 50_000),
    };

    let Some(store) = maybe_store(&state, &caller).await else {
        return Ok(Json(DiffFlamegraph {
            names: vec!["root".into()],
            levels: vec![-1],
            left: vec![0],
            right: vec![0],
            diff: vec![0],
            unit: profile_type.unit().as_str().to_string(),
            ..Default::default()
        })
        .into_response());
    };
    let _slot = state.read_slot().await?;
    let started = Instant::now();
    let result = state
        .query_guard(store.clone())
        .run(move || lidlesseye_core::diff(&store, &opts))
        .await
        .map_err(ApiError::from)?;
    state.record_query(&caller.tenant, started);
    Ok(Json(result).into_response())
}

/// `GET /api/v1/top`
pub async fn top(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    let caller = state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let profile_type = params.profile_type();
    let opts = QueryOptions::new(profile_type.clone(), from, until, params.selector()?)
        .with_group_by(params.group_by());
    // Clamped: without a ceiling `limit=999999999` would sort and serialize
    // the whole tree, which is a cheap way for one client to stall a replica.
    let limit = params.usize_param("limit", 50).clamp(1, 1_000);

    let Some(store) = maybe_store(&state, &caller).await else {
        return Ok(Json(json!({
            "unit": profile_type.unit().as_str(),
            "total_value": 0,
            "items": [],
        }))
        .into_response());
    };
    let _slot = state.read_slot().await?;
    let started = Instant::now();
    let result = state
        .query_guard(store.clone())
        .run(move || lidlesseye_core::top(&store, &opts, limit))
        .await
        .map_err(ApiError::from)?;
    state.record_query(&caller.tenant, started);
    let TopResult { items, total_value } = result;

    Ok(Json(json!({
        "unit": profile_type.unit().as_str(),
        "total_value": total_value,
        "items": items,
    }))
    .into_response())
}

/// `GET /api/v1/timeseries`
pub async fn timeseries(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    let caller = state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let profile_type = params.profile_type();
    let step_ns = match params.get("step") {
        Some(step) => lidlesseye_core::parse_duration_ns(step)
            .map_err(|err| ApiError::bad_request(format!("invalid step: {err}")))?,
        None => {
            let buckets = params.usize_param("buckets", 60).clamp(1, 1000) as u64;
            ((until - from) / buckets).max(1)
        }
    };
    let opts = TimeSeriesOptions {
        profile_type,
        from_ns: from,
        until_ns: until,
        selector: params.selector()?,
        step_ns,
        group_by: params.group_by(),
    };

    let Some(store) = maybe_store(&state, &caller).await else {
        return Ok(Json(TimeSeries {
            from_ns: from,
            until_ns: until,
            step_ns,
            series: Vec::new(),
        })
        .into_response());
    };
    let _slot = state.read_slot().await?;
    let started = Instant::now();
    let series = state
        .query_guard(store.clone())
        .run(move || lidlesseye_core::timeseries(&store, &opts))
        .await
        .map_err(ApiError::from)?;
    state.record_query(&caller.tenant, started);
    Ok(Json(series).into_response())
}

/// `GET /api/v1/series`
pub async fn series(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    let caller = state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let selector = params.selector()?;
    let rows = match maybe_store(&state, &caller).await {
        Some(store) => {
            let slot = state.read_slot().await?;
            let rows = state
                .query_guard(store.clone())
                .run(move || store.series(from, until, &selector))
                .await;
            drop(slot);
            rows?
        }
        None => Vec::new(),
    };
    Ok(Json(json!({
        "tenant": caller.tenant.to_string(),
        "series": rows,
    }))
    .into_response())
}

/// `GET /api/v1/label-names` (also available as `/api/v1/labels`)
pub async fn label_names(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    let caller = state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let selector = params.selector()?;
    let names = match maybe_store(&state, &caller).await {
        Some(store) => {
            let slot = state.read_slot().await?;
            let names = state
                .query_guard(store.clone())
                .run(move || store.label_names(from, until, &selector))
                .await;
            drop(slot);
            names?
        }
        None => vec![lidlesseye_core::SERVICE_NAME_LABEL.to_string()],
    };
    Ok(Json(json!({ "label_names": names, "labels": names })).into_response())
}

/// `GET /api/v1/label/{name}/values`
pub async fn label_values(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<Response, ApiError> {
    let caller = state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let selector = params.selector()?;
    let values: Vec<Value> = match maybe_store(&state, &caller).await {
        Some(store) => {
            let label = name.clone();
            let slot = state.read_slot().await?;
            let pairs = state
                .query_guard(store.clone())
                .run(move || store.label_values(&label, from, until, &selector))
                .await;
            drop(slot);
            pairs?
                .into_iter()
                .map(|(value, total)| json!({ "value": value, "total": total }))
                .collect()
        }
        None => Vec::new(),
    };
    Ok(Json(json!({ "label": name, "values": values })).into_response())
}

/// `GET /api/v1/profile_types`
pub async fn profile_types(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    let caller = state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let selector: Selector = params.selector()?;
    let rows = match maybe_store(&state, &caller).await {
        Some(store) => {
            let slot = state.read_slot().await?;
            let rows = state
                .query_guard(store.clone())
                .run(move || store.series(from, until, &selector))
                .await;
            drop(slot);
            rows?
        }
        None => Vec::new(),
    };
    let mut totals: BTreeMap<String, (u64, usize)> = Default::default();
    for row in rows {
        let entry = totals.entry(row.profile_type.clone()).or_insert((0, 0));
        entry.0 += row.total_value;
        entry.1 += row.batch_count;
    }
    let mut out: Vec<Value> = totals
        .iter()
        .map(|(name, (total, batches))| {
            let profile_type = ProfileType::new(name.clone());
            json!({
                "name": name,
                "unit": profile_type.unit().as_str(),
                "total_value": total,
                "batch_count": batches,
            })
        })
        .collect();
    // `wall` is a computed type, offer it whenever any of its inputs exist.
    if totals.contains_key(ProfileType::CPU)
        || totals.contains_key(ProfileType::OFF_CPU)
        || totals.contains_key(ProfileType::WALL)
    {
        let sum = |key: &str| totals.get(key).map(|value| value.0).unwrap_or(0);
        out.push(json!({
            "name": ProfileType::WALL,
            "unit": Unit::Nanoseconds.as_str(),
            "total_value": sum(ProfileType::CPU) + sum(ProfileType::OFF_CPU) + sum(ProfileType::WALL),
            "batch_count": totals.values().map(|value| value.1).sum::<usize>(),
            "virtual": true,
        }));
    }
    Ok(Json(json!({ "profile_types": out })).into_response())
}

/// `GET /api/v1/tenants`
///
/// The catalogue of tenants. It answers "which tenants exist", so it must not
/// require the caller to already name a valid one: on a server started with
/// `--allowed-tenant`, the dashboard's first request has no tenant at all, and
/// resolving one anyway made this endpoint answer 403 to the only request that
/// could have told the UI which tenants it may use — an empty picker, forever.
///
/// The tenant header is therefore ignored here; only the token is checked. A
/// tenant-scoped token still sees itself and nothing else, and an allowlist
/// still bounds what an unscoped caller is offered.
pub async fn tenants(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let scope = state.authorize_catalog(&headers)?;
    let allowed = state.inner.allowed_tenants.clone();

    // Every row reads its tenant's store, and a store read is two scans over
    // `batches`. Tenants are cheap to list but expensive to describe, so the
    // description runs off the runtime behind a read slot: as a plain call it
    // pinned a worker for the sum of every tenant's scans, which is the last
    // thing the dashboard's very first request should do.
    let _slot = state.read_slot().await?;
    let rows = tokio::task::spawn_blocking(move || {
        if let Some(scope) = scope {
            return vec![describe_tenant(&state, &scope)];
        }
        let mut out = state.registry().tenants();
        if let Some(allowed) = &allowed {
            out.retain(|tenant| allowed.iter().any(|id| id.to_string() == tenant.id));
            // An allowed tenant that has not been written to yet is still part
            // of the catalogue. Leaving it out would give the dashboard an
            // empty picker on a fresh install, and the first upload would have
            // nowhere to go — the tenant exists as configuration, not as a
            // directory.
            for id in allowed {
                if !out.iter().any(|tenant| tenant.id == id.to_string()) {
                    out.push(describe_tenant(&state, id));
                }
            }
            out.sort_by(|a, b| a.id.cmp(&b.id));
        }
        out
    })
    .await
    .map_err(|err| ApiError::internal(format!("tenant catalogue task failed: {err}")))?;
    Ok(Json(json!({ "tenants": rows })).into_response())
}

/// One row of the tenant catalogue, with empty stats when nothing is stored.
fn describe_tenant(state: &AppState, tenant: &TenantId) -> TenantInfo {
    // From the tenant's stats cache: computed when cold, reused for its TTL.
    // Computing it for every tenant on every poll is what made the dashboard's
    // first request wait on a scan per tenant, and the server also warms these
    // caches in the background at start-up.
    let (stats, series) = state
        .registry()
        .tenant(tenant)
        .map(|store| store.stats_cached())
        .unwrap_or_default();
    TenantInfo {
        id: tenant.to_string(),
        stats,
        series,
        limits: state.registry().limits(tenant),
    }
}

/// `GET /api/v1/health`
pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: VERSION,
        uptime_seconds: state.uptime().as_secs(),
    })
}

/// `GET /api/v1/ready`
pub async fn ready(State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    // Touching the index proves the data directory is still usable.
    if state.registry().tenant_ids().is_empty() {
        return Ok(StatusCode::NO_CONTENT);
    }
    Ok(StatusCode::OK)
}

/// `GET /api/v1/stats`
pub async fn stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let caller = state.authorize(&headers)?;
    let store = maybe_store(&state, &caller).await;
    // Both numbers are full scans of `batches`, and the dashboard polls this
    // every few seconds. They share one read slot and one blocking task.
    let (summary, series_count, locks) = match store {
        Some(store) => {
            let locks = Some(store.lock_stats());
            let _slot = state.read_slot().await?;
            // `stats_checked`, not `stats`: a busy store must be a 503, not
            // "0 batches", which a dashboard renders as an empty install.
            let (stats, series) = state
                .query_guard(store.clone())
                .run(move || store.stats_checked())
                .await?;
            (stats, series, locks)
        }
        None => (Default::default(), 0, None),
    };
    let activity = state.activity(&caller.tenant);
    Ok(Json(StatsResponse {
        version: VERSION,
        tenant: caller.tenant.to_string(),
        uptime_seconds: state.uptime().as_secs(),
        store: summary,
        locks,
        series: series_count,
        tenants: state.registry().tenant_ids().len(),
        // Per tenant, not per process: a token scoped to one tenant must not
        // learn how much another tenant ingests. The process-wide totals stay
        // on `/metrics` for the operator.
        ingest_reports: activity.ingest_reports,
        ingest_samples: activity.ingest_samples,
        queries: activity.queries,
    })
    .into_response())
}

/// `GET /api/v1/version`
pub async fn version(State(state): State<AppState>) -> Json<Value> {
    // Read the header name from the running configuration: a client that
    // discovers it here must be able to trust the answer.
    Json(json!({
        "name": "lidlesseye",
        "version": VERSION,
        "multi_tenant": true,
        "tenant_header": state.config().tenant_header,
    }))
}

/// `GET /metrics` — Prometheus text exposition format.
pub async fn metrics(State(state): State<AppState>) -> Response {
    let registry = state.registry();
    let ids = registry.tenant_ids();
    let store = registry.total_stats();
    let extra = vec![
        ("lidlesseye_batches", store.batch_count.to_string()),
        ("lidlesseye_storage_bytes", store.total_bytes.to_string()),
        ("lidlesseye_stored_value", store.total_value.to_string()),
        ("lidlesseye_tenants", ids.len().to_string()),
    ];
    let body = state.metrics().render(&extra, state.uptime().as_secs());
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

/// Service-map ingestion accepts one window or a batch of them.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ServiceMapPayload {
    Batch { batches: Vec<ServiceMapBatch> },
    Single(ServiceMapBatch),
}

/// `POST /api/v1/servicemap/ingest`
///
/// Agents post one delta per collection window. Windows are append-only and
/// queries sum them, so a retry that lands twice inflates that window; the
/// publisher therefore retries only on connection errors, not on timeouts that
/// may have been processed.
pub async fn servicemap_ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let caller = state.authorize(&headers)?;
    if state.config().read_only {
        return Err(ApiError::forbidden("server is read-only"));
    }
    let payload: ServiceMapPayload = serde_json::from_slice(&body)
        .map_err(|err| ApiError::bad_request(format!("invalid service map JSON: {err}")))?;
    let batches = match payload {
        ServiceMapPayload::Single(batch) => vec![batch],
        ServiceMapPayload::Batch { batches } => batches,
    };
    if batches.len() > 512 {
        return Err(ApiError::bad_request("too many batches in one request"));
    }
    let store = state.writable_tenant_async(&caller.tenant).await?;

    let accepted = batches.len();
    let (mut nodes, mut edges) = (0usize, 0usize);
    for batch in &batches {
        nodes += batch.nodes.len();
        edges += batch.edges.len();
    }
    let result = tokio::task::spawn_blocking(move || -> lidlesseye_core::Result<()> {
        for batch in &batches {
            store.append_servicemap(batch)?;
        }
        Ok(())
    })
    .await
    .map_err(|err| ApiError::internal(format!("ingest task failed: {err}")))?;
    result.map_err(ApiError::from)?;
    Ok(Json(json!({
        "accepted": accepted,
        "nodes": nodes,
        "edges": edges,
    })))
}

/// `GET /api/v1/servicemap`
///
/// Same `from`/`until` parameters as every other query, so the map follows the
/// time range selected for the flamegraph.
pub async fn servicemap(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Json<ServiceMap>, ApiError> {
    let caller = state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let Some(store) = maybe_store(&state, &caller).await else {
        return Ok(Json(ServiceMap {
            from_ns: from,
            until_ns: until,
            ..Default::default()
        }));
    };
    let _slot = state.read_slot().await?;
    let started = Instant::now();
    let map = state
        .query_guard(store.clone())
        .run(move || lidlesseye_core::servicemap(&store, from, until))
        .await
        .map_err(ApiError::from)?;
    state.record_query(&caller.tenant, started);
    Ok(Json(map))
}

/// `GET /api/v1/servicemap/history`
///
/// One node's own counters and its incident edges, bucketed over the same
/// window as the map. The panel behind a click needs a shape to draw, not a
/// single total, and this is the only query that produces one without asking
/// the browser to fetch every bucket of `net_nodes`.
pub async fn servicemap_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Json<MapHistory>, ApiError> {
    let caller = state.authorize(&headers)?;
    let node = match params.get("node").map(str::trim) {
        Some(node) if !node.is_empty() => node.to_string(),
        _ => return Err(ApiError::bad_request("`node` is required")),
    };
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let buckets = params.usize_param("buckets", 60).clamp(1, 600) as u64;
    let step_ns = ((until - from) / buckets).max(1);
    // An application-protocol filter is optional. The DNS tab passes `dns`
    // to get its own series; every other caller sees all incident edges.
    let protocol = params
        .get("protocol")
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 64)
        .map(str::to_string);
    let Some(store) = maybe_store(&state, &caller).await else {
        return Ok(Json(MapHistory {
            node,
            from_ns: from,
            until_ns: until,
            step_ns,
            protocol: protocol.unwrap_or_default(),
            ..Default::default()
        }));
    };
    let _slot = state.read_slot().await?;
    let started = Instant::now();
    let history = state
        .query_guard(store.clone())
        .run(move || {
            lidlesseye_core::servicemap_history(
                &store,
                &node,
                from,
                until,
                step_ns,
                protocol.as_deref(),
            )
        })
        .await
        .map_err(ApiError::from)?;
    state.record_query(&caller.tenant, started);
    Ok(Json(history))
}

/// `GET /api/v1/servicemap/histogram`
///
/// One counter per bucket, summed over every edge in the map. This is what the
/// time brush above the service map draws: traffic density, not the CPU sample
/// density a profile query would return. `metric` picks the counter —
/// `connections` (the default), `bytes` or `retransmits` — and the array is
/// dense so the browser can paint bucket `i` at `from_ns + i * step_ns`.
pub async fn servicemap_histogram(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Json<MapHistogram>, ApiError> {
    let caller = state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let buckets = params.usize_param("buckets", 120).clamp(1, 600);
    let step_ns = ((until.saturating_sub(from)) / buckets as u64).max(1);
    // An unknown name is a client bug, not a reason to fail the whole view:
    // fall back to connections rather than 400 on a stale bookmark.
    let metric = params
        .get("metric")
        .and_then(lidlesseye_core::NetMetric::parse)
        .unwrap_or_default();
    let Some(store) = maybe_store(&state, &caller).await else {
        return Ok(Json(MapHistogram {
            from_ns: from,
            until_ns: until,
            step_ns,
            metric: metric.as_str().to_string(),
            points: vec![0; buckets],
        }));
    };
    let _slot = state.read_slot().await?;
    let started = Instant::now();
    let histogram = state
        .query_guard(store.clone())
        .run(move || lidlesseye_core::servicemap_histogram(&store, from, until, buckets, metric))
        .await
        .map_err(ApiError::from)?;
    state.record_query(&caller.tenant, started);
    Ok(Json(histogram))
}

impl AppState {
    /// Count a query against the tenant that asked for it.
    pub(crate) fn record_query(&self, tenant: &lidlesseye_core::TenantId, started: Instant) {
        if let Ok(mut map) = self.inner.activity.lock() {
            let entry = map.entry(tenant.to_string()).or_default();
            entry.queries += 1;
        }
        self.metrics().queries.fetch_add(1, Ordering::Relaxed);
        self.metrics()
            .query_nanos
            .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}
