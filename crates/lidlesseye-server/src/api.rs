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
    AllocatorKind, DiffFlamegraph, DiffOptions, Flamegraph, MapHistogram, MapHistory, ProfileType,
    QueryOptions, Selector, ServiceMap, Store, TenantId, TenantInfo, TimeSeries, TimeSeriesOptions,
    TopResult, now_ns,
};

use crate::error::ApiError;
use crate::params::Params;
use crate::{AppState, Caller, HealthResponse, StatsResponse, VERSION};

/// `?allocator=glibc&mode=self` for a CPU profile.
///
/// The filter runs on the frames that are already stored, so it costs one pass
/// over the distinct frames of the window and works for every window that was
/// ever collected — no second collection path, nothing injected into the target.
fn allocator_filter(params: &Params) -> Result<(Option<AllocatorKind>, bool), ApiError> {
    let Some(raw) = params.get("allocator") else {
        return Ok((None, false));
    };
    let Some(kind) = AllocatorKind::parse(raw) else {
        return Err(ApiError::bad_request(
            "`allocator` must be one of `any`, `glibc`, `jemalloc`, `tcmalloc`, `mimalloc`",
        ));
    };
    let self_only = params
        .get("mode")
        .is_some_and(|mode| mode.eq_ignore_ascii_case("self"));
    Ok((Some(kind), self_only))
}

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
    // A replica started with `--read-only` is a query frontend: it must say so
    // plainly rather than redirect, because there may be no writer behind it at
    // all (that is the whole point of a read-only deployment).
    if state.config().read_only {
        return Err(ApiError::forbidden(
            "this replica is read-only; send writes to the writer deployment",
        ));
    }
    // A standby must not accept writes: two replicas flushing into one data
    // directory is precisely the double-writer this deployment exists to avoid.
    // The caller is told to retry against the active replica instead of being
    // handed a queue that would never drain.
    if !state.inner.lease.is_active() {
        // Redirecting is better than failing: whoever balances writes across
        // both replicas (a Service, a load balancer) then still delivers every
        // batch to the writer in one extra round trip, and the client keeps
        // using its normal retry-free path. 307 preserves method and body.
        let active = state.inner.lease.active_addr();
        if !active.is_empty() {
            let path = format!("/ingest{}", params.query_string());
            let location = format!("http://{active}{path}");
            return Ok((
                axum::http::StatusCode::TEMPORARY_REDIRECT,
                [(axum::http::header::LOCATION, location)],
            )
                .into_response());
        }
        return Err(ApiError::unavailable(format!(
            "this replica is a standby (fence {}); send writes to the active one",
            state.inner.lease.fence()
        )));
    }
    // A replayed batch must not be counted twice: the agent's spool re-sends
    // anything it did not get a 2xx for, and a reply lost on the way back is
    // indistinguishable from a failed request.
    if let Some(batch) = headers
        .get("x-lidlesseye-batch")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        && state.inner.dedupe.seen(batch)
    {
        return Ok((
            StatusCode::OK,
            Json(json!({ "duplicate": true, "batch": batch, "accepted": 0 })),
        )
            .into_response());
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
            // A folded upload that names no window describes *now*, not the last
            // hour. `params.range` falls back to the query default (one hour of
            // history), which parked every sample an hour in the past: the
            // dashboard's five-minute view never looked there, and a freshly
            // ingested batch appeared to be missing while it was on disk.
            let (from, until) = if params.get("from").is_some() || params.get("until").is_some() {
                params.range(now)?
            } else {
                (now.saturating_sub(1_000_000_000), now)
            };
            let text = std::str::from_utf8(&body).map_err(|err| {
                ApiError::bad_request(format!("folded stacks are not UTF-8: {err}"))
            })?;
            let opts = FoldedOptions {
                service_name: service_name.to_string(),
                profile_type: params.profile_type(),
                start_ns: from,
                end_ns: until,
                labels: {
                    // `service_name` is a column, but every dashboard filter
                    // arrives as a label selector (`selector=service_name="x"`),
                    // and a folded upload carried no such label, so its stacks
                    // were invisible to the very filter the UI sends. Mirroring
                    // the column into the labels costs one small string per
                    // report and makes the reader addressable like any other.
                    let mut labels = params.labels();
                    labels
                        .entry("service_name".to_string())
                        .or_insert_with(|| service_name.to_string());
                    labels
                },
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
    // One bad report must not cost the whole upload. An agent retries nine
    // times and then drops the batch, so refusing all sixty-four reports
    // because one counter overflowed is how a single malformed row hid an
    // entire tenant's data. Keep the valid ones and say which were dropped.
    let mut rejected: Vec<serde_json::Value> = Vec::new();
    let mut valid = Vec::with_capacity(reports.len());
    for (index, report) in reports.into_iter().enumerate() {
        match report.validate() {
            Ok(()) => valid.push(report),
            Err(err) => rejected.push(json!({ "index": index, "error": err.to_string() })),
        }
    }
    if valid.is_empty() {
        let first = rejected
            .first()
            .and_then(|entry| entry.get("error"))
            .and_then(|error| error.as_str())
            .unwrap_or("the report is not valid");
        return Err(ApiError::bad_request(format!(
            "no report in the upload was valid: {first}"
        )));
    }
    let reports = valid;

    let count = reports.len();
    let samples: usize = reports.iter().map(|r| r.stacks.len()).sum();
    let want_durable = params
        .get("sync")
        .is_some_and(|value| matches!(value, "1" | "true" | "yes"));
    let outcome = state
        .ingest_many_with(&caller.tenant, reports, want_durable)
        .await?;
    // Only now is the batch really in: a failure above leaves the id unclaimed,
    // so the agent's spool can still deliver it later.
    if let Some(batch) = headers
        .get("x-lidlesseye-batch")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    {
        state.inner.dedupe.remember(batch);
    }
    state
        .metrics()
        .ingest_bytes
        .fetch_add(body.len() as u64, Ordering::Relaxed);

    // `202 Accepted` is the honest status for the asynchronous path: the data
    // is on stable storage (within the group-commit window) but not yet
    // queryable, because the converter has not written its Parquet file. The
    // segment name and `durable` flag let a client tell the two apart.
    let (status, body) = match outcome {
        crate::IngestOutcome::Queued(appended) => (
            StatusCode::ACCEPTED,
            json!({
                "accepted": count,
                "samples": samples,
                "bytes": body.len(),
                "format": format,
                "tenant": caller.tenant.to_string(),
                "queued": true,
                "segment": format!("seg-{:012}", appended.segment),
                "durable": appended.synced,
                "rejected": rejected,
            }),
        ),
        crate::IngestOutcome::Committed(metas) => {
            let batch_ids: Vec<String> = metas
                .iter()
                .map(|m| format!("batch-{}", m.batch_id))
                .collect();
            (
                StatusCode::OK,
                json!({
                    "accepted": count,
                    "batches": batch_ids.len(),
                    "samples": samples,
                    "bytes": body.len(),
                    "format": format,
                    "tenant": caller.tenant.to_string(),
                    "queued": false,
                    "ids": batch_ids,
                    "rejected": rejected,
                }),
            )
        }
    };
    Ok((status, Json(body)).into_response())
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
    let rows = tokio::task::spawn_blocking(move || store.flush_durable())
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
    let (allocator, allocator_self) = allocator_filter(&params)?;
    let opts = QueryOptions::new(profile_type.clone(), from, until, params.selector()?)
        .with_max_nodes(params.usize_param("max_nodes", 1024))
        .with_group_by(params.group_by())
        .with_allocator(allocator, allocator_self);

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
    let (allocator, allocator_self) = allocator_filter(&params)?;
    let opts = QueryOptions::new(profile_type.clone(), from, until, params.selector()?)
        .with_group_by(params.group_by())
        .with_allocator(allocator, allocator_self);
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
    // Ingest health is a backlog, not a queue: the log holds what the converter
    // has not written yet, and `convert_failures` is the only signal that it is
    // stuck rather than busy.
    let mut pending_bytes = 0u64;
    let mut pending_segments = 0u64;
    let mut converted_segments = 0u64;
    let mut converted_reports = 0u64;
    let mut convert_nanos = 0u64;
    let mut convert_failures = 0u64;
    for id in &ids {
        let Some(loaded) = registry.loaded_tenant(id) else {
            continue;
        };
        let (bytes, segments) = loaded.wal_stats();
        pending_bytes += bytes;
        pending_segments += segments as u64;
        let (segments, reports, nanos, failures) = loaded.convert_stats();
        converted_segments += segments;
        converted_reports += reports;
        convert_nanos += nanos;
        convert_failures += failures;
    }
    let cache = state.inner.cache.stats();
    let fsck = state.inner.fsck.last();
    let extra = vec![
        ("lidlesseye_batches", store.batch_count.to_string()),
        ("lidlesseye_storage_bytes", store.total_bytes.to_string()),
        ("lidlesseye_stored_value", store.total_value.to_string()),
        ("lidlesseye_tenants", ids.len().to_string()),
        ("lidlesseye_ingest_log_bytes", pending_bytes.to_string()),
        (
            "lidlesseye_ingest_log_segments",
            pending_segments.to_string(),
        ),
        (
            "lidlesseye_ingest_converted_segments_total",
            converted_segments.to_string(),
        ),
        (
            "lidlesseye_ingest_converted_reports_total",
            converted_reports.to_string(),
        ),
        (
            "lidlesseye_ingest_convert_nanos_total",
            convert_nanos.to_string(),
        ),
        (
            "lidlesseye_ingest_convert_failures_total",
            convert_failures.to_string(),
        ),
        // Replica role and cache/fsck state: the three things an operator
        // cannot otherwise see without opening the JSON endpoints by hand.
        (
            "lidlesseye_replica_active",
            if state.inner.lease.is_active() { "1" } else { "0" }.to_string(),
        ),
        (
            "lidlesseye_lease_fence",
            state.inner.lease.fence().to_string(),
        ),
        (
            "lidlesseye_cache_entries",
            cache.entries.to_string(),
        ),
        ("lidlesseye_cache_hits_total", cache.hits.to_string()),
        ("lidlesseye_cache_misses_total", cache.misses.to_string()),
        (
            "lidlesseye_fsck_findings",
            fsck
                .as_ref()
                .map(|r| {
                    r.tenants
                        .iter()
                        .map(|t| {
                            (t.unreadable.len() + t.vanished.len() + t.empty_hours.len()) as u64
                        })
                        .sum::<u64>()
                })
                .unwrap_or(0)
                .to_string(),
        ),
        (
            "lidlesseye_fsck_elapsed_seconds",
            fsck.as_ref()
                .map(|r| format!("{:.3}", r.elapsed_ms as f64 / 1000.0))
                .unwrap_or_else(|| "0".to_string()),
        ),
        ("lidlesseye_process_rss_bytes", self_metric("VmRSS", 1024)),
        ("lidlesseye_process_threads", self_metric("Threads", 1)),
    ];
    let body = state.metrics().render(&extra, state.uptime().as_secs());
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

/// One number out of `/proc/self/status`, or `0` when it is not there.
///
/// The server is the only process whose memory an operator can see through the
/// dashboard, so it reports its own footprint on the same endpoint as the
/// workload counters. `scale` turns a kB value into bytes.
fn self_metric(key: &str, scale: u64) -> String {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return "0".to_string();
    };
    for line in status.lines() {
        let Some(rest) = line.strip_prefix(key).and_then(|r| r.strip_prefix(':')) else {
            continue;
        };
        let value = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        return (value * scale).to_string();
    }
    "0".to_string()
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
    let selector = params.selector()?;
    let Some(store) = maybe_store(&state, &caller).await else {
        return Ok(Json(ServiceMap {
            from_ns: from,
            until_ns: until,
            ..Default::default()
        }));
    };
    let runtime_source = store.clone();
    let _slot = state.read_slot().await?;
    let started = Instant::now();
    let mut map = state
        .query_guard(store.clone())
        .run(move || lidlesseye_core::servicemap(&store, from, until))
        .await
        .map_err(ApiError::from)?;
    state.record_query(&caller.tenant, started);

    // The map answers "who talks to whom"; the profiler labels say which
    // language was reachable behind each node. Filling it here keeps both the
    // map and the drawer's neighbourhood on one source of truth.
    let languages = runtime_by_workload(&state, runtime_source, selector, from, until).await;
    for node in &mut map.nodes {
        if node.namespace.is_empty() || node.workload.is_empty() {
            continue;
        }
        let key = format!("{}/{}", node.namespace, node.workload);
        if let Some(runtime) = languages.get(&key) {
            node.runtime = runtime.clone();
        }
    }
    Ok(Json(map))
}

/// Which language a workload's reader reported, keyed by `namespace/workload`.
///
/// The label is the evidence: `go-runtime` and `jvm-jfr` come from the runtime
/// reporting for itself, `bpftime-userspace` from the allocator interposer. A
/// workload that only ever produced kernel CPU samples has no entry, and the
/// UI shows a dash rather than guessing from an image name.
async fn runtime_by_workload(
    state: &AppState,
    store: Arc<Store>,
    selector: lidlesseye_core::Selector,
    from: u64,
    until: u64,
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let rows = state
        .query_guard(store.clone())
        .run(move || store.series(from, until, &selector))
        .await;
    let Ok(rows) = rows else {
        return out;
    };
    for row in rows {
        let runtime = match row.labels.get("profile_source").map(String::as_str) {
            Some("go-runtime") => "go",
            Some("jvm-jfr") => "jvm",
            Some("bpftime-userspace") => "native",
            _ => match row.profile_type.as_str() {
                "alloc_space" | "inuse_space" | "inuse_objects" | "alloc_objects" => "native",
                _ => continue,
            },
        };
        out.entry(row.service_name.clone())
            .or_insert_with(|| runtime.to_string());
    }
    out
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

/// `GET /api/v1/profiling/directives`
///
/// Which workloads the operator switched runtime memory profiling on for. The
/// agents poll this; the service map writes it.
pub async fn profiling_directives(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    state.authorize(&headers)?;
    let all = state.inner.directives.list();
    let jvm: Vec<String> = all
        .iter()
        .filter(|(_, kind)| kind == crate::directives::KIND_JVM)
        .map(|(name, _)| name.clone())
        .collect();
    let alloc: Vec<String> = all
        .iter()
        .filter(|(_, kind)| kind == crate::directives::KIND_ALLOC)
        .map(|(name, _)| name.clone())
        .collect();
    let entries: Vec<Value> = all
        .iter()
        .map(|(name, kind)| json!({ "workload": name, "kind": kind }))
        .collect();
    Ok(Json(json!({
        "workloads": jvm,
        "alloc_workloads": alloc,
        "directives": entries,
        // The userspace malloc reader is wired end to end: an ephemeral
        // container is added to the workload's pod, the probe is injected into
        // its glibc processes, and the samples land as `alloc_space` with
        // `profile_source=bpftime-userspace`. Switching it on is still a
        // deliberate act, because the agent writes into another container's
        // processes, so the panel asks for confirmation first.
        "native_memory": {
            "supported": true,
            "requirements": [
                "the target image needs a dynamic glibc (musl and static binaries are skipped)",
                "glibc 2.38 or newer, and libelf.so.1 present in the target image",
                "the agent needs RBAC for pods/ephemeralcontainers in the target namespace",
            ],
            "risk": "the reader injects a library into the target processes; a fragile process can crash",
        },
    })))
}

/// `PUT /api/v1/profiling/directives`
///
/// Switch one workload on or off. The body is `{"workload": "ns/name",
/// "enabled": true}`; the reply is the resulting list, so a caller does not
/// have to guess whether its edit landed.
pub async fn profiling_directive_set(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<DirectiveEdit>,
) -> Result<Json<Value>, ApiError> {
    state.authorize(&headers)?;
    let workload = body.workload.trim().to_string();
    if workload.is_empty() || workload.len() > 256 || workload.matches('/').count() != 1 {
        return Err(ApiError::bad_request(
            "`workload` must be `namespace/name`",
        ));
    }
    let kind = body.kind.unwrap_or_else(|| crate::directives::KIND_JVM.to_string());
    if kind != crate::directives::KIND_JVM && kind != crate::directives::KIND_ALLOC {
        return Err(ApiError::bad_request(
            "`kind` must be `jvm` or `alloc`",
        ));
    }
    let list = state
        .inner
        .directives
        .set(&workload, &kind, body.enabled)
        .map_err(|err| ApiError::internal(format!("saving the directive: {err}")))?;
    let entries: Vec<Value> = list
        .iter()
        .map(|(name, kind)| json!({ "workload": name, "kind": kind }))
        .collect();
    Ok(Json(json!({ "directives": entries })))
}

/// `GET /api/v1/memory/coverage`
///
/// The numbers behind a native allocation figure. A dashboard that shows
/// "12 GB" and nothing else asks to be trusted; this endpoint answers the
/// questions a reader should ask instead: how many samples the number rests on,
/// what interval the reader sampled at, how many allocations that implies per
/// second, and whether the workload is still switched on.
/// Live reports (capabilities, coverage, processes) are posted by readers that
/// talk to the Service, so either replica can end up holding half of the picture
/// and the two would disagree about what is running right now. Rather than keep
/// two copies in sync, both directions of these endpoints go to the writer: the
/// replica holding the lease is the only one allowed to answer, and a 307 keeps
/// the method and body intact for `reqwest`.
///
/// Returns `None` when this replica *is* the writer, or when it is a standby
/// with no writer address to redirect to — the caller then decides whether that
/// means "serve locally" (reads) or "refuse" (writes).
fn writer_redirect(state: &AppState, path: &str, query: Option<String>) -> Option<Response> {
    if state.inner.lease.is_active() {
        return None;
    }
    let active = state.inner.lease.active_addr();
    if active.is_empty() {
        return None;
    }
    // `query` is `Params::query_string()`, which already carries the leading
    // `?` (or is empty when there is nothing to forward).
    let target = match query {
        Some(query) => format!("{path}{query}"),
        None => path.to_string(),
    };
    let location = format!("http://{active}{target}");
    Some(
        (
            axum::http::StatusCode::TEMPORARY_REDIRECT,
            [(axum::http::header::LOCATION, location)],
        )
            .into_response(),
    )
}

pub async fn memory_coverage(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    let caller = state.authorize(&headers)?;
    let now = now_ns();
    let (from, until) = params.range(now)?;
    let selector = params.selector()?;

    let enabled: Vec<String> = state
        .inner
        .directives
        .list()
        .into_iter()
        .filter(|(_, kind)| kind == crate::directives::KIND_ALLOC)
        .map(|(name, _)| name)
        .collect();

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

    // One workload can arrive as several series (the Go runtime reports one per
    // allocation class, and a label change splits a series in two), so the rows
    // are folded by workload before they leave. `batches` is the number of
    // ingested reports, not the number of recorded stacks: the two differ by
    // whatever the reader batched up, and calling it "samples" would overstate
    // what this endpoint knows.
    let mut folded: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    // The row every workload starts from, whichever profile type arrives first:
    // a workload can show up through `inuse_space` before any allocation sample
    // is flushed, and it still belongs on the page.
    let base_row = |name: &str, start_ns: u64, end_ns: u64| -> Value {
        json!({
            "workload": name,
            "enabled": enabled.iter().any(|enabled| enabled == name),
            "requested_alloc_bytes": 0u64,
            "inuse_bytes": 0u64,
            "batches": 0u64,
            "sampling_interval": Value::Null,
            "sampling_label": Value::Null,
            "reader": Value::Null,
            // Which language the reader found. `null` means "not seen yet",
            // which is different from "no runtime": a workload whose only
            // series are CPU samples from the kernel profiler tells us nothing
            // about what runs inside it.
            "runtime": Value::Null,
            "first_seen_ns": start_ns,
            "last_seen_ns": end_ns,
            "coverage": Value::Null,
            "coverage_note": "the reader has not reported how many processes it reached",
        })
    };
    for row in rows {
        // The runtime comes from whichever reader produced the series: the Go
        // heap reader, the JFR reader or the userspace allocator. Kernel CPU
        // samples carry no such label, so they are not evidence of a language.
        let runtime = match row.labels.get("profile_source").map(String::as_str) {
            Some("go-runtime") => Some("go"),
            Some("jvm-jfr") => Some("jvm"),
            Some("bpftime-userspace") => Some("native"),
            _ => match row.profile_type.as_str() {
                "alloc_space" | "inuse_space" | "inuse_objects" | "alloc_objects" => Some("native"),
                _ => None,
            },
        };

        if row.profile_type == "inuse_space" {
            let entry = folded
                .entry(row.service_name.clone())
                .or_insert_with(|| base_row(&row.service_name, row.start_ns, row.end_ns));
            let inuse = entry["inuse_bytes"].as_u64().unwrap_or(0) + row.total_value;
            entry["inuse_bytes"] = json!(inuse);
            entry["first_seen_ns"] = json!(
                entry["first_seen_ns"]
                    .as_u64()
                    .unwrap_or(row.start_ns)
                    .min(row.start_ns)
            );
            entry["last_seen_ns"] = json!(
                entry["last_seen_ns"]
                    .as_u64()
                    .unwrap_or(row.end_ns)
                    .max(row.end_ns)
            );
            if entry["runtime"].is_null()
                && let Some(runtime) = runtime
            {
                entry["runtime"] = json!(runtime);
            }
            continue;
        }

        if row.profile_type != "alloc_space" {
            continue;
        }
        // The reader writes `alloc.sampling=poisson-1/64` and
        // `alloc.reader=bpftime-userspace`. Parsing the interval out of the
        // label keeps the endpoint honest when an older sample arrives without
        // it: the field is then null rather than invented.
        let sampling_label = row.labels.get("alloc.sampling").cloned();
        let sampling_interval = sampling_label
            .as_deref()
            .and_then(|label| label.rsplit_once('/'))
            .and_then(|(_, tail)| tail.parse::<u64>().ok())
            .filter(|interval| *interval > 0);
        let reader = row.labels.get("alloc.reader").cloned();

        let entry = folded
            .entry(row.service_name.clone())
            .or_insert_with(|| base_row(&row.service_name, row.start_ns, row.end_ns));
        if entry["runtime"].is_null()
            && let Some(runtime) = runtime
        {
            entry["runtime"] = json!(runtime);
        }
        let bytes = entry["requested_alloc_bytes"].as_u64().unwrap_or(0) + row.total_value;
        entry["requested_alloc_bytes"] = json!(bytes);
        let batches = entry["batches"].as_u64().unwrap_or(0) + row.batch_count as u64;
        entry["batches"] = json!(batches);
        entry["first_seen_ns"] =
            json!(entry["first_seen_ns"].as_u64().unwrap_or(row.start_ns).min(row.start_ns));
        entry["last_seen_ns"] =
            json!(entry["last_seen_ns"].as_u64().unwrap_or(row.end_ns).max(row.end_ns));
        if entry["sampling_interval"].is_null()
            && let Some(interval) = sampling_interval
        {
            entry["sampling_interval"] = json!(interval);
            entry["sampling_label"] = json!(sampling_label);
        }
        if entry["reader"].is_null()
            && let Some(reader) = reader
        {
            entry["reader"] = json!(reader);
        }
    }

    let mut workloads: Vec<Value> = folded.into_values().collect();
    // A workload the reader reported on but that produced no samples yet still
    // belongs on the page: "switched on, nothing received" is a state an
    // operator needs to see, and it is exactly the state a broken injection
    // leaves behind.
    for (workload, report) in state.inner.coverage.all() {
        if workloads.iter().any(|row| row["workload"] == json!(workload)) {
            continue;
        }
        workloads.push(json!({
            "workload": workload,
            "enabled": enabled.iter().any(|name| *name == workload),
            "requested_alloc_bytes": 0u64,
            "inuse_bytes": 0u64,
            "batches": 0u64,
            "sampling_interval": Value::Null,
            "sampling_label": Value::Null,
            "reader": Value::Null,
            "runtime": Value::Null,
            "first_seen_ns": report.at_ns,
            "last_seen_ns": report.at_ns,
            "coverage": Value::Null,
            "coverage_note": "",
        }));
    }
    // Attach the latest coverage report. The entry is created above with a null
    // `coverage` so its shape is stable; here it is filled from the last report
    // the shim sent, if any.
    for row in workloads.iter_mut() {
        let workload = row["workload"].as_str().unwrap_or_default().to_string();
        let Some(report) = state.inner.coverage.get(&workload) else {
            continue;
        };
        row["coverage"] = json!({
            "detected": report.detected,
            "attached": report.attached,
            "uncovered": report.uncovered(),
            "skipped": report.skipped,
            "memory_bytes": report.memory_bytes,
            "process_count": report.process_count,
            "process_rss_bytes": report.process_rss_bytes,
            "process_pss_bytes": report.process_pss_bytes,
            "at_ns": report.at_ns,
        });
        row["coverage_note"] = json!("");
        let points = state.inner.coverage.history(&workload);
        if !points.is_empty() {
            row["history"] = json!(points);
        }
    }
    workloads.sort_by(|a, b| {
        let left = a["requested_alloc_bytes"].as_u64().unwrap_or(0);
        let right = b["requested_alloc_bytes"].as_u64().unwrap_or(0);
        right.cmp(&left)
    });

    Ok(Json(json!({
        "tenant": caller.tenant.to_string(),
        // The window these totals cover, echoed back so the page and the API
        // cannot disagree about what "requested" means without anyone noticing:
        // an inflated row is either a wide window or a bug, and the reader of
        // the page can tell which from these two numbers.
        "from_ns": from,
        "until_ns": until,
        "workloads": workloads,
        "enabled_workloads": enabled,
    }))
    .into_response())
}

/// `POST /api/v1/memory/coverage`
///
/// The shim inside a target pod reports how many of the workload's processes the
/// native reader actually reached. The server cannot see that from the outside:
/// it knows a workload is switched on, not whether the injection landed on three
/// processes or none. The shim sends this every flush, so the memory page can
/// show the difference.
pub async fn memory_coverage_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<crate::coverage::CoverageBody>,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    if let Some(redirect) = writer_redirect(&state, "/api/v1/memory/coverage", None) {
        return Ok(redirect);
    }
    if !state.inner.lease.is_active() {
        return Err(ApiError::unavailable(
            "this replica is a standby; live reports go to the active one",
        ));
    }
    let workload = body.workload.trim().to_string();
    if workload.is_empty() || workload.len() > 256 || workload.matches('/').count() != 1 {
        return Err(ApiError::bad_request(
            "`workload` must be `namespace/name`",
        ));
    }
    let report = crate::coverage::CoverageReport {
        detected: body.detected,
        attached: body.attached,
        skipped: body.skipped,
        memory_bytes: body.memory_bytes,
        process_count: body.process_count,
        process_rss_bytes: body.process_rss_bytes,
        process_pss_bytes: body.process_pss_bytes,
        at_ns: 0,
    };
    state.inner.coverage.record(&workload, report.clone());
    // The live map answers "what is it now"; the store answers "what was it an
    // hour ago, after the process exited". Recording the same numbers as gauge
    // samples is what makes the second question answerable: the map keeps two
    // hours and dies with the process, Parquet does not.
    let at_ns = if report.at_ns == 0 {
        lidlesseye_core::now_ns()
    } else {
        report.at_ns
    };
    let mut gauges = Vec::new();
    let readings = [
        ("process_rss", report.process_rss_bytes),
        ("process_pss", report.process_pss_bytes),
        ("pod_cgroup_memory", report.memory_bytes),
    ];
    for (kind, value) in readings {
        let Some(value) = value else { continue };
        let opts = FoldedOptions {
            service_name: workload.clone(),
            profile_type: ProfileType::new(kind),
            start_ns: at_ns,
            end_ns: at_ns.saturating_add(1),
            labels: {
                let mut labels = std::collections::BTreeMap::new();
                // Same mirroring as the ingest path: dashboards filter by
                // `service_name` as a label, and a gauge without it would be
                // invisible to the very selector the memory page sends.
                labels.insert("service_name".to_string(), workload.clone());
                labels.insert("temporality".to_string(), "gauge".to_string());
                labels
            },
        };
        // The stack is a label in its own right: these samples have no call
        // chain, and an empty one would be dropped as unparseable.
        let text = format!("{kind};{workload} {value}\n");
        if let Ok(parsed) = lidlesseye_core::decode_folded(&text, &opts) {
            gauges.push(parsed);
        }
    }
    if !gauges.is_empty() {
        let caller = state.authorize(&headers)?;
        // A failed write must not fail the report: the shim retries every tick,
        // and the live map is already updated.
        if let Err(error) = state.ingest_many_with(&caller.tenant, gauges, false).await {
            tracing::debug!(error = %error, workload = %workload, "cannot store the memory gauges");
        }
    }
    Ok(Json(json!({ "ok": true })).into_response())
}

/// `GET /api/v1/capabilities`
///
/// What each workload can profile and whether it currently is. The state and
/// the reason travel together: `unsupported` with `unsupported_libc` tells a
/// user to stop looking, while `disabled` tells them to flip a switch.
pub async fn capabilities(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    let workload = params.get("workload");
    let rows = state.inner.capabilities.list(workload);
    Ok(Json(json!({ "capabilities": rows })).into_response())
}

/// `GET /api/v1/role`
///
/// Which replica may write, and the fencing token that says how many handovers
/// have happened. A dashboard that only reads does not need it; an operator
/// deciding "is this pair actually a pair" does.
pub async fn role(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    let lease = &state.inner.lease;
    Ok(Json(json!({
        "holder": lease.holder(),
        "active": lease.is_active(),
        "fence": lease.fence(),
        "passive": lease.is_passive(),
        "lease_file": lease.path().display().to_string(),
    }))
    .into_response())
}

/// `GET /api/v1/cache`
///
/// What the read cache is holding and how often it answered. It exists because
/// a cache nobody can see is a cache nobody can trust: a sudden drop in hits is
/// the first sign that queries stopped being shaped the way the UI thinks.
pub async fn cache_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    let stats = state.inner.cache.stats();
    Ok(Json(json!({
        "entries": stats.entries,
        "hits": stats.hits,
        "misses": stats.misses,
        "skipped": stats.skipped,
        "ttl_seconds": crate::cache::TTL.as_secs(),
    }))
    .into_response())
}

/// `GET /api/v1/fsck`
///
/// The last on-disk check, or `null` before the first one runs. It answers the
/// question the index cannot: the query layer always describes whatever files
/// it finds, so only a check that remembers the previous run can say a file
/// disappeared.
pub async fn fsck(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    let report = state.inner.fsck.last();
    Ok(Json(json!({ "fsck": report })).into_response())
}

/// `POST /api/v1/fsck`
///
/// Run the check now. It reads directory entries and file headers and never
/// takes a read slot, so it is safe to call while queries are running.
pub async fn fsck_run(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    let registry = state.inner.registry.clone();
    let fsck = state.inner.fsck.clone();
    let data_dir = registry.root().to_path_buf();
    let tenants: Vec<String> = registry
        .tenant_ids()
        .into_iter()
        .map(|tenant| tenant.to_string())
        .collect();
    let report = tokio::task::spawn_blocking(move || fsck.run(&data_dir, &tenants))
        .await
        .map_err(|err| ApiError::internal(format!("fsck task failed: {err}")))?;
    Ok(Json(json!({ "fsck": report })).into_response())
}

/// `POST /api/v1/capabilities`
///
/// Readers report from inside the cluster, where the truth is: whether a
/// process is musl, whether `jcmd` exists, whether the injection landed. The
/// server only knows what the switchboard asked for, so it stores what it is
/// told and never infers a state it cannot see.
pub async fn capabilities_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Vec<crate::capabilities::Capability>>,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    if let Some(redirect) = writer_redirect(&state, "/api/v1/capabilities", None) {
        return Ok(redirect);
    }
    if !state.inner.lease.is_active() {
        return Err(ApiError::unavailable(
            "this replica is a standby; live reports go to the active one",
        ));
    }
    let mut accepted = 0;
    for capability in body {
        if capability.workload.is_empty() || capability.reader.is_empty() {
            continue;
        }
        accepted += state.inner.capabilities.record(vec![capability]);
    }
    Ok(Json(json!({ "accepted": accepted })).into_response())
}
/// `GET /api/v1/memory/processes?workload=&limit=`
///
/// The processes a pod's own `/proc` walk found, merged across the workload's
/// pods and cut to the top `limit` by RSS. Merging happens before truncating,
/// so the second-largest process of a pod is not lost behind a larger sibling.
pub async fn memory_processes(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    let workload = params
        .get("workload")
        .ok_or_else(|| ApiError::bad_request("`workload` is required"))?;
    let limit = params.usize_param("limit", 20).clamp(1, 200);
    let (processes, omitted, pods) = state.inner.processes.top(workload, limit);
    Ok(Json(json!({
        "workload": workload,
        "pods": pods,
        "omitted": omitted,
        "processes": processes,
    }))
    .into_response())
}

/// `POST /api/v1/memory/processes`
///
/// The shim walks the pod's `/proc` every flush and sends the top of the list.
pub async fn memory_processes_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<crate::processes::ProcessReport>,
) -> Result<Response, ApiError> {
    state.authorize(&headers)?;
    if let Some(redirect) = writer_redirect(&state, "/api/v1/memory/processes", None) {
        return Ok(redirect);
    }
    if !state.inner.lease.is_active() {
        return Err(ApiError::unavailable(
            "this replica is a standby; live reports go to the active one",
        ));
    }
    if body.workload.is_empty() {
        return Err(ApiError::bad_request("`workload` is required"));
    }
    let stored = state.inner.processes.record(body);
    Ok(Json(json!({ "stored": stored })).into_response())
}

/// One edit to the profiling switchboard.
#[derive(Debug, Deserialize)]
pub struct DirectiveEdit {
    pub workload: String,
    pub enabled: bool,
    /// `jvm` (read the JVM's recorder) or `alloc` (inject bpftime). Defaults to
    /// `jvm` so a client written before this field existed keeps working.
    #[serde(default)]
    pub kind: Option<String>,
}
