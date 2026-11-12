//! End-to-end tests of the HTTP surface.
//!
//! The router is exercised directly through `tower::ServiceExt::oneshot`, so
//! the tests cover routing, extraction, auth, tenancy, validation and
//! serialization without opening a socket.

use std::collections::BTreeMap;
use std::path::Path;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use lidlesseye_core::model::ProfileType;
use lidlesseye_core::{Frame, ReportBuilder, TenantLimits, now_ns};
use lidlesseye_server::{LidlessEye, ServerConfig};
use serde_json::{Value, json};
use tower::ServiceExt;

const TOKEN: &str = "s3cret-token";
const TENANT_HEADER: &str = "X-Scope-OrgID";

fn config(dir: &Path) -> ServerConfig {
    ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.to_path_buf(),
        ..Default::default()
    }
}

async fn router(dir: &Path) -> Router {
    LidlessEye::bind(config(dir)).await.unwrap().router()
}

async fn router_with(dir: &Path, cfg: ServerConfig) -> Router {
    LidlessEye::bind(ServerConfig {
        data_dir: dir.to_path_buf(),
        ..cfg
    })
    .await
    .unwrap()
    .router()
}

/// `GET`/`POST` helper returning the status and the decoded JSON body.
async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn call_as(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    tenant: &str,
) -> (StatusCode, Value) {
    call(app, method, uri, body, &[(TENANT_HEADER, tenant)]).await
}

/// Raw-body request, for the pprof and folded formats.
async fn post_raw(
    app: &Router,
    uri: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A two-frame report worth `total` nanoseconds.
fn report(service: &str, profile_type: &str, total: u64) -> Value {
    let now = now_ns();
    let mut builder = ReportBuilder::new(
        service,
        ProfileType::new(profile_type),
        now - 1_000_000,
        now,
    )
    .label("env", "test")
    .label("region", "eu-west")
    .sample_rate_hz(99);
    builder.add_stack(
        vec![
            Frame::new("/app/server", "handle_request"),
            Frame::new("/app/server", "main"),
        ],
        total / 2,
    );
    builder.add_stack(vec![Frame::new("/app/server", "main")], total - total / 2);
    serde_json::to_value(builder.finish()).unwrap()
}

fn report_with_pod(service: &str, pod: &str, total: u64) -> Value {
    let now = now_ns();
    let mut builder =
        ReportBuilder::new(service, ProfileType::cpu(), now - 1_000_000, now).label("pod", pod);
    builder.add_stack(vec![Frame::new("/app", "work")], total);
    serde_json::to_value(builder.finish()).unwrap()
}

#[tokio::test]
async fn ingest_then_query_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let (status, body) = call(
        &app,
        "POST",
        "/ingest",
        Some(report("checkout", "cpu", 1_000_000_000)),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["accepted"], 1);
    assert_eq!(body["segments"], 1);
    assert_eq!(body["samples"], 2);
    assert_eq!(body["tenant"], "default");

    // Flamegraph: root + handle_request + main.
    let (status, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?profile_type=cpu&from=now-1h&max_nodes=100",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{fg}");
    assert_eq!(fg["total"], 1_000_000_000i64);
    assert_eq!(fg["unit"], "nanoseconds");
    let names: Vec<&str> = fg["names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(names.contains(&"handle_request"), "{names:?}");
    assert_eq!(names.len(), fg["levels"].as_array().unwrap().len());

    // Top: the leaf function owns all of its self time.
    let (status, top) = call(
        &app,
        "GET",
        "/api/v1/top?profile_type=cpu&limit=5",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(top["total_value"], 1_000_000_000i64);
    let rows = top["items"].as_array().unwrap();
    let handle = rows
        .iter()
        .find(|row| row["name"] == "handle_request")
        .expect("handle_request is in the table");
    assert_eq!(handle["self_value"], 500_000_000i64);
    assert_eq!(handle["total_value"], 500_000_000i64);
    let main = rows.iter().find(|row| row["name"] == "main").unwrap();
    assert_eq!(main["self_value"], 500_000_000i64, "main is also a leaf");
    assert_eq!(main["total_value"], 1_000_000_000i64);

    // Series carries the labels that were ingested with the report.
    let (status, series) = call(&app, "GET", "/api/v1/series", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    let rows = series["series"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["service_name"], "checkout");
    assert_eq!(rows[0]["labels"]["env"], "test");

    // And the label values endpoint sees them too.
    let (status, values) = call(&app, "GET", "/api/v1/label/env/values", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(values["values"][0]["value"], "test");
}

#[tokio::test]
async fn wall_is_the_sum_of_cpu_and_off_cpu() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    for (kind, value) in [("cpu", 200_000_000u64), ("off_cpu", 800_000_000u64)] {
        let (status, body) = call(
            &app,
            "POST",
            "/ingest",
            Some(report("api", kind, value)),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    let (status, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?profile_type=wall",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{fg}");
    assert_eq!(fg["total"], 1_000_000_000i64);

    let (_, types) = call(&app, "GET", "/api/v1/profile_types", None, &[]).await;
    let names: Vec<&str> = types["profile_types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"cpu") && names.contains(&"off_cpu") && names.contains(&"wall"));
}

#[tokio::test]
async fn selectors_filter_by_label() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    call(&app, "POST", "/ingest", Some(report("a", "cpu", 100)), &[]).await;
    call(&app, "POST", "/ingest", Some(report("b", "cpu", 900)), &[]).await;

    let (status, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?selector=service_name%3D%22a%22",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{fg}");
    assert_eq!(fg["total"], 100);

    // A glob on a label, and a negation.
    let (_, glob) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?selector=%7Bregion%3D~%22eu-*%22%7D",
        None,
        &[],
    )
    .await;
    assert_eq!(glob["total"], 1000);

    let (_, excluded) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?selector=%7Bservice_name!%3D%22a%22%7D",
        None,
        &[],
    )
    .await;
    assert_eq!(excluded["total"], 900);
}

#[tokio::test]
async fn rejects_malformed_requests() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    // Unterminated selector.
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?selector=%7Benv%3D%22a%22",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].is_string());

    // Nonsense timestamp.
    let (status, _) = call(&app, "GET", "/api/v1/top?from=yesterday", None, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Empty body.
    let request = Request::builder()
        .method("POST")
        .uri("/ingest")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // A report with a dangling frame index.
    let mut broken = report("bad", "cpu", 10);
    broken["stacks"][0]["frames"][0][0] = json!(99);
    let (status, body) = call(&app, "POST", "/ingest", Some(broken), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // An unknown format is refused instead of being guessed.
    let (status, body) = post_raw(
        &app,
        "/ingest?format=nonsense",
        "text/plain",
        b"x 1".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Unknown endpoint under /api.
    let (status, _) = call(&app, "GET", "/api/v1/nope", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn auth_is_enforced_everywhere_that_matters() {
    let dir = tempfile::tempdir().unwrap();
    let app = router_with(
        dir.path(),
        ServerConfig {
            auth_token: Some(TOKEN.to_string()),
            ..config(dir.path())
        },
    )
    .await;

    let (status, _) = call(&app, "POST", "/ingest", Some(report("x", "cpu", 10)), &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = call(
        &app,
        "POST",
        "/ingest",
        Some(report("x", "cpu", 10)),
        &[(header::AUTHORIZATION.as_str(), &format!("Bearer {TOKEN}"))],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, _) = call(&app, "GET", "/api/v1/flamegraph", None, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let auth = format!("Bearer {TOKEN}");
    let (status, _) = call(
        &app,
        "GET",
        "/api/v1/flamegraph",
        None,
        &[(header::AUTHORIZATION.as_str(), "Bearer wrong")],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &app,
        "GET",
        "/api/v1/flamegraph",
        None,
        &[(header::AUTHORIZATION.as_str(), &auth)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Health and metrics stay open: probes and scrapers do not carry tokens.
    let (status, _) = call(&app, "GET", "/api/v1/health", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn read_only_servers_reject_ingest_but_serve_queries() {
    let dir = tempfile::tempdir().unwrap();
    // Seed the directory with a normal server first.
    {
        let app = LidlessEye::bind(config(dir.path())).await.unwrap().router();
        call(&app, "POST", "/ingest", Some(report("ro", "cpu", 42)), &[]).await;
    }
    let app = router_with(
        dir.path(),
        ServerConfig {
            read_only: true,
            ..config(dir.path())
        },
    )
    .await;

    let (status, body) = call(&app, "POST", "/ingest", Some(report("ro", "cpu", 42)), &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, fg) = call(&app, "GET", "/api/v1/flamegraph", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fg["total"], 42);
}

#[tokio::test]
async fn metrics_use_the_prometheus_exposition_format() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    call(&app, "POST", "/ingest", Some(report("m", "cpu", 10)), &[]).await;
    call(&app, "GET", "/api/v1/top", None, &[]).await;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();

    for needle in [
        "# TYPE lidlesseye_ingest_reports_total counter",
        "lidlesseye_ingest_reports_total 1",
        "lidlesseye_queries_total 1",
        "lidlesseye_segments 1",
        "lidlesseye_tenants 1",
        "lidlesseye_query_duration_seconds_total",
    ] {
        assert!(body.contains(needle), "missing `{needle}` in:\n{body}");
    }
}

#[tokio::test]
async fn time_series_buckets_a_segment_across_its_window() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    // One 60-second window worth 1s of CPU, asked for in 6 buckets of 10s.
    //
    // The window is absolute rather than `now-60s`..`now` on purpose: a
    // relative window is a few milliseconds wider than the data by the time
    // the request is parsed, and the buckets must divide the *data* window
    // for each one to hold exactly a sixth.
    let end = now_ns() - 120_000_000_000;
    let start = end - 60_000_000_000;
    let mut builder = ReportBuilder::new("ts", ProfileType::cpu(), start, end);
    builder.add_stack(vec![Frame::new("/app", "work")], 1_000_000_000);
    let value = serde_json::to_value(builder.finish()).unwrap();
    let (status, body) = call(&app, "POST", "/ingest", Some(value), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let url = format!("/api/v1/timeseries?from={start}ns&until={end}ns&buckets=6");
    let (status, series) = call(&app, "GET", &url, None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{series}");
    assert_eq!(series["step_ns"], 10_000_000_000i64);
    let points = series["series"][0]["points"].as_array().unwrap();
    assert_eq!(points.len(), 6);
    let total: i64 = points.iter().map(|v| v.as_i64().unwrap()).sum();
    assert!(
        (total - 1_000_000_000).abs() <= 6,
        "the value is spread over the buckets, not duplicated (got {total})"
    );
    for point in points {
        let value = point.as_i64().unwrap();
        let expected = 166_666_666.0;
        assert!(
            (value as f64 - expected).abs() / expected < 0.01,
            "bucket value {value}"
        );
    }
}

#[tokio::test]
async fn the_dashboard_is_embedded() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let response = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(content_type.starts_with("text/html"), "{content_type}");
    let body = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains("LidlessEye"), "the index page is served");

    // An unknown client-side route falls back to the SPA shell.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/flamegraph")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn ingest_accepts_a_batch_and_reports_counts() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let payload = json!({ "reports": [report("a", "cpu", 10), report("b", "cpu", 20)] });
    let (status, body) = call(&app, "POST", "/ingest", Some(payload), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["accepted"], 2);
    assert_eq!(body["segments"], 2);

    let (_, stats) = call(&app, "GET", "/api/v1/stats", None, &[]).await;
    assert_eq!(stats["store"]["segment_count"], 2);
    assert_eq!(stats["series"], 2);
    assert_eq!(stats["ingest_reports"], 2);

    let mut labels = BTreeMap::new();
    labels.insert("env".to_string(), "test".to_string());
}

// ---------------------------------------------------------------------------
// multi-tenancy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tenants_are_isolated_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let (status, body) = call_as(
        &app,
        "POST",
        "/ingest",
        Some(report("api", "cpu", 100)),
        "acme",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tenant"], "acme");

    let (status, body) = call_as(
        &app,
        "POST",
        "/ingest",
        Some(report("api", "cpu", 900)),
        "beta",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, acme) = call_as(&app, "GET", "/api/v1/flamegraph", None, "acme").await;
    assert_eq!(acme["total"], 100);
    let (_, beta) = call_as(&app, "GET", "/api/v1/flamegraph", None, "beta").await;
    assert_eq!(beta["total"], 900);
    // The default tenant never sees the others.
    let (_, default) = call(&app, "GET", "/api/v1/flamegraph", None, &[]).await;
    assert_eq!(default["total"], 0);

    // Series and stats are scoped too.
    let (_, series) = call_as(&app, "GET", "/api/v1/series", None, "acme").await;
    assert_eq!(series["series"].as_array().unwrap().len(), 1);
    let (_, stats) = call_as(&app, "GET", "/api/v1/stats", None, "acme").await;
    assert_eq!(stats["tenant"], "acme");
    assert_eq!(stats["store"]["segment_count"], 1);
    assert_eq!(stats["tenants"], 2);
}

#[tokio::test]
async fn tenant_ids_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    for bad in ["..", "../etc", "a/b", "with space", ""] {
        let (status, body) = call(
            &app,
            "POST",
            "/ingest",
            Some(report("x", "cpu", 1)),
            &[(TENANT_HEADER, bad)],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "`{bad}` -> {body}");
    }
    // No rejected id may have created a directory, and nothing may have
    // escaped the data directory.
    let entries: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        entries,
        vec!["tenants".to_string()],
        "only the registry exists"
    );
    let inner: Vec<String> = std::fs::read_dir(dir.path().join("tenants"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        inner.is_empty(),
        "no tenant directory was created: {inner:?}"
    );
    assert!(!dir.path().parent().unwrap().join("etc").exists());
}

#[tokio::test]
async fn tenant_tokens_pin_the_tenant() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.tenant_tokens
        .insert("acme-token".to_string(), "acme".to_string());
    let app = router_with(dir.path(), cfg).await;
    let auth = |token: &'static str| [(header::AUTHORIZATION.as_str(), token)];

    // No token at all: rejected, because tenant tokens imply auth.
    let (status, _) = call(&app, "GET", "/api/v1/flamegraph", None, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The token writes into its own tenant, with or without the header.
    let (status, body) = call(
        &app,
        "POST",
        "/ingest",
        Some(report("api", "cpu", 7)),
        &[
            (header::AUTHORIZATION.as_str(), "Bearer acme-token"),
            (TENANT_HEADER, "acme"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Asking for another tenant with this token is forbidden, not ignored.
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/flamegraph",
        None,
        &[
            (header::AUTHORIZATION.as_str(), "Bearer acme-token"),
            (TENANT_HEADER, "beta"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // A scoped token sees only its own tenant in the listing.
    let (status, tenants) = call(
        &app,
        "GET",
        "/api/v1/tenants",
        None,
        &[(header::AUTHORIZATION.as_str(), "Bearer acme-token")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let list = tenants["tenants"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], "acme");
    let _ = auth;
}

#[tokio::test]
async fn allowed_tenants_and_the_tenant_cap_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.allowed_tenants = Some(vec!["acme".to_string()]);
    let app = router_with(dir.path(), cfg).await;

    let (status, body) =
        call_as(&app, "POST", "/ingest", Some(report("a", "cpu", 1)), "beta").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _) = call_as(&app, "POST", "/ingest", Some(report("a", "cpu", 1)), "acme").await;
    assert_eq!(status, StatusCode::OK);

    // The cap is hit on the *next* tenant.
    let dir2 = tempfile::tempdir().unwrap();
    let mut cfg = config(dir2.path());
    cfg.max_tenants = 1;
    let app = router_with(dir2.path(), cfg).await;
    let (status, _) = call_as(&app, "POST", "/ingest", Some(report("a", "cpu", 1)), "one").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call_as(&app, "POST", "/ingest", Some(report("a", "cpu", 1)), "two").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
}

#[tokio::test]
async fn the_series_limit_rejects_new_series_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.tenant_limits.insert(
        "acme".to_string(),
        TenantLimits {
            max_series: Some(1),
            ..Default::default()
        },
    );
    let app = router_with(dir.path(), cfg).await;

    let (status, body) = call_as(
        &app,
        "POST",
        "/ingest",
        Some(report("api", "cpu", 5)),
        "acme",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The same series keeps writing.
    let (status, body) = call_as(
        &app,
        "POST",
        "/ingest",
        Some(report("api", "cpu", 5)),
        "acme",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // A new service is refused with 429 and nothing is written.
    let (status, body) = call_as(
        &app,
        "POST",
        "/ingest",
        Some(report("web", "cpu", 5)),
        "acme",
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    let (_, series) = call_as(&app, "GET", "/api/v1/series", None, "acme").await;
    assert_eq!(series["series"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// profile formats
// ---------------------------------------------------------------------------

/// Tiny protobuf writer, so the pprof fixture has no codegen dependency.
struct Proto(Vec<u8>);

impl Proto {
    fn new() -> Self {
        Proto(Vec::new())
    }
    fn varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.0.push((value as u8) | 0x80);
            value >>= 7;
        }
        self.0.push(value as u8);
    }
    fn tag(&mut self, field: u32, wire: u8) {
        self.varint(((field as u64) << 3) | wire as u64);
    }
    fn uint(&mut self, field: u32, value: u64) {
        self.tag(field, 0);
        self.varint(value);
    }
    fn bytes(&mut self, field: u32, value: &[u8]) {
        self.tag(field, 2);
        self.varint(value.len() as u64);
        self.0.extend_from_slice(value);
    }
}

fn pprof_fixture() -> Vec<u8> {
    let table = [
        "",
        "cpu",
        "nanoseconds",
        "main",
        "handle_request",
        "/srv/app.go",
    ];
    let mut p = Proto::new();
    let mut sample_type = Proto::new();
    sample_type.uint(1, 1);
    sample_type.uint(2, 2);
    p.bytes(1, &sample_type.0);

    for (id, name) in [(1u64, 3u64), (2, 4)] {
        let mut function = Proto::new();
        function.uint(1, id);
        function.uint(2, name);
        function.uint(4, 5);
        p.bytes(5, &function.0);
    }
    for (id, function_id, line) in [(10u64, 2u64, 12u64), (11, 1, 0)] {
        let mut line_msg = Proto::new();
        line_msg.uint(1, function_id);
        line_msg.uint(2, line);
        let mut location = Proto::new();
        location.uint(1, id);
        location.bytes(4, &line_msg.0);
        p.bytes(4, &location.0);
    }

    let mut sample = Proto::new();
    sample.uint(1, 10);
    sample.uint(1, 11);
    sample.uint(2, 123_456);
    p.bytes(2, &sample.0);

    let now = now_ns();
    p.uint(9, now - 10_000_000_000);
    p.uint(10, 10_000_000_000);
    for name in table {
        p.bytes(6, name.as_bytes());
    }
    p.0
}

#[tokio::test]
async fn ingest_accepts_pprof_protobuf() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let (status, body) = post_raw(
        &app,
        "/ingest?format=pprof&service_name=checkout",
        "application/vnd.google.protobuf",
        pprof_fixture(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["format"], "pprof");
    assert_eq!(body["samples"], 1);

    let (_, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?profile_type=cpu&from=now-1h",
        None,
        &[],
    )
    .await;
    assert_eq!(fg["total"], 123_456);
    let names: Vec<&str> = fg["names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(names.contains(&"handle_request"), "{names:?}");
    assert!(names.contains(&"main"), "{names:?}");
}

#[tokio::test]
async fn pprof_without_a_service_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    let (status, body) = post_raw(
        &app,
        "/ingest?format=pprof",
        "application/vnd.google.protobuf",
        pprof_fixture(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].as_str().unwrap().contains("service name"));
}

#[tokio::test]
async fn ingest_accepts_folded_stacks() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let payload = b"main;serve;handle 70\nmain;serve;idle 30\n".to_vec();
    let (status, body) = post_raw(
        &app,
        "/ingest?format=folded&service_name=api&from=now-60s&until=now",
        "text/plain",
        payload,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, top) = call(&app, "GET", "/api/v1/top?limit=10", None, &[]).await;
    assert_eq!(top["total_value"], 100);
    let names: Vec<&str> = top["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"handle"), "{names:?}");
}

// ---------------------------------------------------------------------------
// comparison and grouping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn diff_compares_two_windows() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    let now = now_ns();

    let mut before = ReportBuilder::new(
        "api",
        ProfileType::cpu(),
        now - 7_200_000_000_000,
        now - 7_190_000_000_000,
    );
    before.add_stack(vec![Frame::new("/app", "hot")], 100);
    before.add_stack(vec![Frame::new("/app", "cold")], 100);
    let (status, body) = call(
        &app,
        "POST",
        "/ingest",
        Some(serde_json::to_value(before.finish()).unwrap()),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let mut after = ReportBuilder::new(
        "api",
        ProfileType::cpu(),
        now - 600_000_000_000,
        now - 590_000_000_000,
    );
    after.add_stack(vec![Frame::new("/app", "hot")], 400);
    let (status, body) = call(
        &app,
        "POST",
        "/ingest",
        Some(serde_json::to_value(after.finish()).unwrap()),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, diff) = call(
        &app,
        "GET",
        "/api/v1/diff?from=now-1h&until=now&baseline_from=now-2h&baseline_until=now-1h",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{diff}");
    assert_eq!(diff["total_left"], 200);
    assert_eq!(diff["total_right"], 400);
    let names: Vec<&str> = diff["names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let hot = names.iter().position(|n| *n == "hot").unwrap();
    assert_eq!(diff["left"][hot], 100);
    assert_eq!(diff["right"][hot], 400);
    assert_eq!(diff["diff"][hot], 300);
    let cold = names.iter().position(|n| *n == "cold").unwrap();
    assert_eq!(diff["diff"][cold], -100);
}

#[tokio::test]
async fn group_by_adds_one_frame_per_label_value() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    call(
        &app,
        "POST",
        "/ingest",
        Some(report_with_pod("api", "api-1", 10)),
        &[],
    )
    .await;
    call(
        &app,
        "POST",
        "/ingest",
        Some(report_with_pod("api", "api-2", 20)),
        &[],
    )
    .await;

    let (status, fg) = call(&app, "GET", "/api/v1/flamegraph?group_by=pod", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{fg}");
    assert_eq!(fg["total"], 30);
    let groups: Vec<&str> = fg["names"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(i, _)| fg["levels"][*i] == 0)
        .map(|(_, name)| name.as_str().unwrap())
        .collect();
    assert!(
        groups.contains(&"pod=api-1") && groups.contains(&"pod=api-2"),
        "group frames must name the label: {groups:?}"
    );

    let (status, ts) = call(
        &app,
        "GET",
        "/api/v1/timeseries?group_by=pod&buckets=4",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ts}");
    assert_eq!(ts["series"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn label_names_lists_every_key() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    call(&app, "POST", "/ingest", Some(report("api", "cpu", 1)), &[]).await;

    let (status, body) = call(&app, "GET", "/api/v1/label-names", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<&str> = body["label_names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(names.contains(&"service_name"));
    assert!(names.contains(&"env"));
    assert!(names.contains(&"region"));

    // An empty server still answers, so the UI does not have to special-case
    // the first run.
    let dir2 = tempfile::tempdir().unwrap();
    let app2 = router(dir2.path()).await;
    let (status, body) = call(&app2, "GET", "/api/v1/label-names", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["label_names"], json!(["service_name"]));
}

#[tokio::test]
async fn tenants_endpoint_lists_every_tenant() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    call_as(&app, "POST", "/ingest", Some(report("a", "cpu", 1)), "acme").await;
    call_as(&app, "POST", "/ingest", Some(report("b", "cpu", 2)), "beta").await;

    let (status, body) = call(&app, "GET", "/api/v1/tenants", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let list = body["tenants"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    let ids: Vec<&str> = list.iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["acme", "beta"]);
    assert_eq!(list[1]["stats"]["segment_count"], 1);
}
