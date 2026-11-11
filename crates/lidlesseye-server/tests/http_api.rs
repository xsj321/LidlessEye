//! End-to-end tests of the HTTP surface.
//!
//! The router is exercised directly through `tower::ServiceExt::oneshot`, so
//! the tests cover routing, extraction, auth, validation and serialization
//! without opening a socket.

use std::collections::BTreeMap;
use std::path::Path;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use lidlesseye_core::model::ProfileType;
use lidlesseye_core::{Frame, ReportBuilder, now_ns};
use lidlesseye_server::{LidlessEye, ServerConfig};
use serde_json::{Value, json};
use tower::ServiceExt;

const TOKEN: &str = "s3cret-token";

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

async fn authed_router(dir: &Path) -> Router {
    let cfg = ServerConfig {
        auth_token: Some(TOKEN.to_string()),
        ..config(dir)
    };
    LidlessEye::bind(cfg).await.unwrap().router()
}

/// `GET`/`POST` helper returning the status and the decoded JSON body.
async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
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

/// A two-frame report worth `total` nanoseconds.
fn report(service: &str, profile_type: &str, total: u64) -> Value {
    let now = now_ns();
    let mut builder = ReportBuilder::new(service, ProfileType::new(profile_type), now - 1_000_000, now)
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
    builder.add_stack(
        vec![Frame::new("/app/server", "main")],
        total - total / 2,
    );
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
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["accepted"], 1);
    assert_eq!(body["segments"], 1);
    assert_eq!(body["samples"], 2);

    // Flamegraph: root + handle_request + main.
    let (status, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?profile_type=cpu&from=now-1h&max_nodes=100",
        None,
        None,
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
    let (status, top) = call(&app, "GET", "/api/v1/top?profile_type=cpu&limit=5", None, None).await;
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
    let (status, series) = call(&app, "GET", "/api/v1/series", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = series["series"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["service_name"], "checkout");
    assert_eq!(rows[0]["labels"]["env"], "test");

    // And the label values endpoint sees them too.
    let (status, values) = call(&app, "GET", "/api/v1/label/env/values", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(values["values"][0]["value"], "test");
}

#[tokio::test]
async fn wall_is_the_sum_of_cpu_and_off_cpu() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    for (kind, value) in [("cpu", 200_000_000u64), ("off_cpu", 800_000_000u64)] {
        let (status, body) = call(&app, "POST", "/ingest", Some(report("api", kind, value)), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    let (status, fg) = call(&app, "GET", "/api/v1/flamegraph?profile_type=wall", None, None).await;
    assert_eq!(status, StatusCode::OK, "{fg}");
    assert_eq!(fg["total"], 1_000_000_000i64);

    let (_, types) = call(&app, "GET", "/api/v1/profile_types", None, None).await;
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
    call(&app, "POST", "/ingest", Some(report("a", "cpu", 100)), None).await;
    call(&app, "POST", "/ingest", Some(report("b", "cpu", 900)), None).await;

    let (status, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?selector=service_name%3D%22a%22",
        None,
        None,
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
        None,
    )
    .await;
    assert_eq!(glob["total"], 1000);

    let (_, excluded) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?selector=%7Bservice_name!%3D%22a%22%7D",
        None,
        None,
    )
    .await;
    assert_eq!(excluded["total"], 900);
}

#[tokio::test]
async fn rejects_malformed_requests() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    // Unterminated selector.
    let (status, body) = call(&app, "GET", "/api/v1/flamegraph?selector=%7Benv%3D%22a%22", None, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].is_string());

    // Nonsense timestamp.
    let (status, _) = call(&app, "GET", "/api/v1/top?from=yesterday", None, None).await;
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
    let (status, body) = call(&app, "POST", "/ingest", Some(broken), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // `wall` cannot be ingested: it is computed at query time.
    let (status, _) = call(&app, "POST", "/ingest", Some(report("w", "wall", 10)), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Unknown endpoint under /api.
    let (status, _) = call(&app, "GET", "/api/v1/nope", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn auth_is_enforced_everywhere_that_matters() {
    let dir = tempfile::tempdir().unwrap();
    let app = authed_router(dir.path()).await;

    let (status, _) = call(&app, "POST", "/ingest", Some(report("x", "cpu", 10)), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = call(
        &app,
        "POST",
        "/ingest",
        Some(report("x", "cpu", 10)),
        Some(TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, _) = call(&app, "GET", "/api/v1/flamegraph", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(&app, "GET", "/api/v1/flamegraph", None, Some("wrong")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(&app, "GET", "/api/v1/flamegraph", None, Some(TOKEN)).await;
    assert_eq!(status, StatusCode::OK);

    // Health and metrics stay open: probes and scrapers do not carry tokens.
    let (status, _) = call(&app, "GET", "/api/v1/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let response = app
        .clone()
        .oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn read_only_servers_reject_ingest_but_serve_queries() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = ServerConfig {
        read_only: true,
        ..config(dir.path())
    };
    // Seed the directory with a normal server first.
    {
        let app = LidlessEye::bind(config(dir.path())).await.unwrap().router();
        call(&app, "POST", "/ingest", Some(report("ro", "cpu", 42)), None).await;
    }
    let app = LidlessEye::bind(cfg).await.unwrap().router();

    let (status, body) = call(&app, "POST", "/ingest", Some(report("ro", "cpu", 42)), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, fg) = call(&app, "GET", "/api/v1/flamegraph", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fg["total"], 42);
}

#[tokio::test]
async fn metrics_use_the_prometheus_exposition_format() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    call(&app, "POST", "/ingest", Some(report("m", "cpu", 10)), None).await;
    call(&app, "GET", "/api/v1/top", None, None).await;

    let response = app
        .oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(
        response.into_body().collect().await.unwrap().to_bytes().to_vec(),
    )
    .unwrap();

    for needle in [
        "# TYPE lidlesseye_ingest_reports_total counter",
        "lidlesseye_ingest_reports_total 1",
        "lidlesseye_queries_total 1",
        "lidlesseye_segments 1",
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
    let now = now_ns();
    let mut builder = ReportBuilder::new("ts", ProfileType::cpu(), now - 60_000_000_000, now);
    builder.add_stack(vec![Frame::new("/app", "work")], 1_000_000_000);
    let value = serde_json::to_value(builder.finish()).unwrap();
    let (status, body) = call(&app, "POST", "/ingest", Some(value), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, series) = call(
        &app,
        "GET",
        "/api/v1/timeseries?from=now-60s&until=now&buckets=6",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{series}");
    assert_eq!(series["step_ns"], 10_000_000_000i64);
    let points = series["series"][0]["points"].as_array().unwrap();
    assert_eq!(points.len(), 6);
    let total: i64 = points.iter().map(|v| v.as_i64().unwrap()).sum();
    assert!(
        (total - 1_000_000_000).abs() <= 6,
        "the value is spread over the buckets, not duplicated (got {total})"
    );
    // Each bucket got a sixth of the total, i.e. the segment was not attributed
    // to a single bucket. The tolerance absorbs the drift between the `now` used
    // at ingest and the one used by the query.
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
        response.into_body().collect().await.unwrap().to_bytes().to_vec(),
    )
    .unwrap();
    assert!(body.contains("LidlessEye"), "the index page is served");

    // An unknown client-side route falls back to the SPA shell.
    let response = app
        .oneshot(Request::builder().uri("/flamegraph").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn ingest_accepts_a_batch_and_reports_counts() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let payload = json!({ "reports": [report("a", "cpu", 10), report("b", "cpu", 20)] });
    let (status, body) = call(&app, "POST", "/ingest", Some(payload), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["accepted"], 2);
    assert_eq!(body["segments"], 2);

    let (_, stats) = call(&app, "GET", "/api/v1/stats", None, None).await;
    assert_eq!(stats["store"]["segment_count"], 2);
    assert_eq!(stats["series"], 2);

    let mut labels = BTreeMap::new();
    labels.insert("env".to_string(), "test".to_string());
    assert_eq!(stats["ingest_reports"], 2);
}
