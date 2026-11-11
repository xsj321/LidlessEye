//! HTTP handlers.

use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Value, json};

use lidlesseye_core::model::{Report, Unit};
use lidlesseye_core::{
    ProfileType, QueryOptions, Selector, TimeSeriesOptions, now_ns,
};

use crate::error::ApiError;
use crate::params::Params;
use crate::{AppState, HealthResponse, StatsResponse, VERSION};

/// Ingestion accepts a single report or a batch.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IngestPayload {
    Batch { reports: Vec<Report> },
    Single(Report),
}

/// `POST /ingest`
///
/// Accepts a [`Report`] or `{"reports": [...]}`. Query parameters can override
/// `service_name`, `profile_type`, `from` and `until`, which makes ad-hoc
/// ingestion with `curl` practical.
pub async fn ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
    body: Bytes,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
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

    let mut reports: Vec<Report> = match serde_json::from_slice::<IngestPayload>(&body) {
        Ok(IngestPayload::Single(report)) => vec![report],
        Ok(IngestPayload::Batch { reports }) => reports,
        Err(err) => {
            state
                .metrics()
                .ingest_errors
                .fetch_add(1, Ordering::Relaxed);
            return Err(ApiError::bad_request(format!("invalid report JSON: {err}")));
        }
    };

    for report in &mut reports {
        if let Some(name) = params.get("service_name").or_else(|| params.get("name")) {
            report.service_name = name.to_string();
        }
        if let Some(raw) = params.get("profile_type").or_else(|| params.get("type")) {
            report.profile_type = ProfileType::new(raw);
        }
        let now = now_ns();
        if params.get("from").is_some() || params.get("until").is_some() {
            let (from, until) = params.range(now)?;
            report.start_ns = from;
            report.end_ns = until;
        }
        report.validate()?;
    }

    let count = reports.len();
    let samples: usize = reports.iter().map(|r| r.stacks.len()).sum();
    let metas = state.ingest_many(reports).await?;
    state
        .metrics()
        .ingest_bytes
        .fetch_add(body.len() as u64, Ordering::Relaxed);

    let segments: Vec<String> = metas
        .iter()
        .map(|m| {
            m.path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();
    Ok(Json(json!({
        "accepted": count,
        "segments": segments.len(),
        "samples": samples,
        "bytes": body.len(),
        "files": segments,
    }))
    .into_response())
}

/// `GET /api/v1/flamegraph`
pub async fn flamegraph(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let opts = QueryOptions::new(
        params.profile_type(),
        from,
        until,
        params.selector()?,
    )
    .with_max_nodes(params.usize_param("max_nodes", 1024));

    let store = state.store().clone();
    let started = Instant::now();
    let result = tokio::task::spawn_blocking(move || lidlesseye_core::flamegraph(&store, &opts))
        .await
        .map_err(|err| ApiError::internal(format!("query task failed: {err}")))?
        .map_err(ApiError::from)?;
    state.record_query(started);

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

/// `GET /api/v1/top`
pub async fn top(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let profile_type = params.profile_type();
    let opts = QueryOptions::new(profile_type.clone(), from, until, params.selector()?);
    let limit = params.usize_param("limit", 50);

    let store = state.store().clone();
    let started = Instant::now();
    let result = tokio::task::spawn_blocking(move || lidlesseye_core::top(&store, &opts, limit))
        .await
        .map_err(|err| ApiError::internal(format!("query task failed: {err}")))?
        .map_err(ApiError::from)?;
    state.record_query(started);

    Ok(Json(json!({
        "unit": profile_type.unit().as_str(),
        "total_value": result.total_value,
        "items": result.items,
    }))
    .into_response())
}

/// `GET /api/v1/timeseries`
pub async fn timeseries(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
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
    };

    let store = state.store().clone();
    let started = Instant::now();
    let series = tokio::task::spawn_blocking(move || lidlesseye_core::timeseries(&store, &opts))
        .await
        .map_err(|err| ApiError::internal(format!("query task failed: {err}")))?
        .map_err(ApiError::from)?;
    state.record_query(started);
    Ok(Json(series).into_response())
}

/// `GET /api/v1/series`
pub async fn series(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let selector = params.selector()?;
    let rows = state.store().series(from, until, &selector);
    Ok(Json(json!({ "series": rows })).into_response())
}

/// `GET /api/v1/label/{name}/values`
pub async fn label_values(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let selector = params.selector()?;
    let values: Vec<Value> = state
        .store()
        .label_values(&name, from, until, &selector)
        .into_iter()
        .map(|(value, total)| json!({ "value": value, "total": total }))
        .collect();
    Ok(Json(json!({ "label": name, "values": values })).into_response())
}

/// `GET /api/v1/profile_types`
pub async fn profile_types(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let selector: Selector = params.selector()?;
    let mut totals: std::collections::BTreeMap<String, (u64, usize)> = Default::default();
    for row in state.store().series(from, until, &selector) {
        let entry = totals
            .entry(row.profile_type.clone())
            .or_insert((0, 0));
        entry.0 += row.total_value;
        entry.1 += row.segment_count;
    }
    let mut out: Vec<Value> = totals
        .iter()
        .map(|(name, (total, segments))| {
            let profile_type = ProfileType::new(name.clone());
            json!({
                "name": name,
                "unit": profile_type.unit().as_str(),
                "total_value": total,
                "segment_count": segments,
            })
        })
        .collect();
    // `wall` is a virtual type, offer it whenever its inputs exist.
    if totals.contains_key(ProfileType::CPU) || totals.contains_key(ProfileType::OFF_CPU) {
        out.push(json!({
            "name": ProfileType::WALL,
            "unit": Unit::Nanoseconds.as_str(),
            "total_value": totals.get(ProfileType::CPU).map(|v| v.0).unwrap_or(0)
                + totals.get(ProfileType::OFF_CPU).map(|v| v.0).unwrap_or(0),
            "segment_count": totals.values().map(|v| v.1).sum::<usize>(),
            "virtual": true,
        }));
    }
    Ok(Json(json!({ "profile_types": out })).into_response())
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
    let stats = state.store().stats();
    if stats.segment_count == 0 {
        return Ok(StatusCode::NO_CONTENT);
    }
    Ok(StatusCode::OK)
}

/// `GET /api/v1/stats`
pub async fn stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    let store = state.store().stats();
    let series = state.store().series(0, u64::MAX, &Selector::default()).len();
    let metrics = state.metrics();
    Ok(Json(StatsResponse {
        version: VERSION,
        uptime_seconds: state.uptime().as_secs(),
        store,
        series,
        ingest_reports: metrics.ingest_reports.load(Ordering::Relaxed),
        ingest_samples: metrics.ingest_samples.load(Ordering::Relaxed),
        queries: metrics.queries.load(Ordering::Relaxed),
    })
    .into_response())
}

/// `GET /api/v1/version`
pub async fn version() -> Json<Value> {
    Json(json!({
        "name": "lidlesseye",
        "version": VERSION,
    }))
}

/// `GET /metrics` — Prometheus text exposition format.
pub async fn metrics(State(state): State<AppState>) -> Response {
    let store = state.store().stats();
    let extra = vec![
        ("lidlesseye_segments", store.segment_count.to_string()),
        ("lidlesseye_storage_bytes", store.total_bytes.to_string()),
        ("lidlesseye_stored_value", store.total_value.to_string()),
    ];
    let body = state
        .metrics()
        .render(&extra, state.uptime().as_secs());
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

impl AppState {
    pub(crate) fn record_query(&self, started: Instant) {
        self.metrics().queries.fetch_add(1, Ordering::Relaxed);
        self.metrics()
            .query_nanos
            .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}
