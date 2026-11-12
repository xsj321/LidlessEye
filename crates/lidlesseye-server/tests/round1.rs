//! Round 1 — functional regression plus boundary, error-path and concurrency
//! cases.
//!
//! The suite is written from the outside in: every case states what a user (or
//! a machine client) can observe, never how the code is structured. A failure
//! here means the system misbehaves, not that a helper moved.

mod support;

use std::io::Write;

use axum::http::StatusCode;
use lidlesseye_core::model::ProfileType;
use lidlesseye_core::{Frame, ReportBuilder, TenantId, now_ns};
use serde_json::json;
use support::*;

// ---------------------------------------------------------------------------
// tenant identifiers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tenant_id_length_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let at_limit = "a".repeat(lidlesseye_core::MAX_TENANT_LEN);
    let (status, body) = call(
        &app,
        "POST",
        "/ingest",
        Some(report("svc", "cpu", 100)),
        &[(TENANT_HEADER, &at_limit)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let over_limit = "a".repeat(lidlesseye_core::MAX_TENANT_LEN + 1);
    let (status, body) = call(
        &app,
        "POST",
        "/ingest",
        Some(report("svc", "cpu", 100)),
        &[(TENANT_HEADER, &over_limit)],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("invalid tenant"),
        "{body}"
    );
}

#[tokio::test]
async fn tenant_ids_cannot_escape_the_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    for bad in [
        "..", ".", ".hidden", "a/b", "a b", "a:b", "a;b", "a$b", "%2e%2e",
    ] {
        let (status, body) = call(
            &app,
            "POST",
            "/ingest",
            Some(report("svc", "cpu", 100)),
            &[(TENANT_HEADER, bad)],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "tenant `{bad}`: {body}");
    }

    // Nothing may have leaked into the parent of the data directory.
    let parent = dir.path().parent().unwrap();
    assert!(!parent.join("etc").exists());
    assert!(!dir.path().join("svc").exists());
}

#[tokio::test]
async fn blank_tenant_header_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    for blank in ["", "   ", "\t"] {
        let (status, body) = call(
            &app,
            "GET",
            "/api/v1/flamegraph?from=now-1h",
            None,
            &[(TENANT_HEADER, blank)],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "`{blank}`: {body}");
    }
}

/// Tenant ids are case-insensitive, so `Acme` and `acme` are one tenant.
///
/// Anything else would be a cross-tenant data leak on Windows and macOS,
/// where NTFS and APFS are case-insensitive: two distinct `TenantId`s would
/// resolve to the same directory and silently share one store.
#[tokio::test]
async fn tenant_ids_are_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let (status, body) = call_as(
        &app,
        "POST",
        "/ingest",
        Some(report("billing", "cpu", 1_000)),
        "Acme",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tenant"], "acme", "the tenant id was not normalized");

    let (status, _) = call_as(
        &app,
        "POST",
        "/ingest",
        Some(report("billing", "cpu", 7_000)),
        "acme",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Both spellings address the same tenant, and both see all of its data.
    for spelling in ["Acme", "acme", "ACME"] {
        let (_, fg) = call_as(
            &app,
            "GET",
            "/api/v1/flamegraph?profile_type=cpu&from=now-1h",
            None,
            spelling,
        )
        .await;
        assert_eq!(fg["total"], 8_000, "`{spelling}`: {fg}");
    }

    // …and there is exactly one directory for it.
    let entries: Vec<String> = std::fs::read_dir(dir.path().join("tenants"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["acme".to_string()]);
}

// ---------------------------------------------------------------------------
// time ranges
// ---------------------------------------------------------------------------

#[tokio::test]
async fn time_ranges_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now&until=now-1h",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = call(&app, "GET", "/api/v1/flamegraph?from=yesterday", None, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // A window with no data is not an error.
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h&until=now",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 0);
}

// ---------------------------------------------------------------------------
// ingestion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_body_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    let (status, body) = post_raw(&app, "/ingest", "application/json", Vec::new()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap_or_default().contains("empty"),
        "{body}"
    );
}

#[tokio::test]
async fn unknown_format_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    let (status, body) = post_raw(
        &app,
        "/ingest?format=xml",
        "application/xml",
        b"<x/>".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn truncated_pprof_is_rejected_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let full = pprof_fixture();
    for cut in [1usize, 4, full.len() / 3, full.len() / 2, full.len() - 1] {
        let (status, body) = post_raw(
            &app,
            "/ingest?format=pprof&service_name=api",
            "application/vnd.google.protobuf",
            full[..cut].to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "cut at {cut}: {body}");
    }

    // A complete profile still works, so the rejections above were not blanket.
    let (status, body) = post_raw(
        &app,
        "/ingest?format=pprof&service_name=api",
        "application/vnd.google.protobuf",
        full,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn gzipped_pprof_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&pprof_fixture()).unwrap();
    let gz = encoder.finish().unwrap();

    let (status, body) = post_raw(
        &app,
        "/ingest?format=pprof&service_name=api",
        "application/vnd.google.protobuf",
        gz,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["samples"], 1);
}

#[tokio::test]
async fn folded_input_tolerates_crlf_and_comments() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let text = "# a comment\r\nmain;work 500\r\n\r\n   \r\nmain 250\r\nbroken-line\r\n";
    let (status, body) = post_raw(
        &app,
        "/ingest?format=folded&service_name=checkout",
        "text/plain",
        text.as_bytes().to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h&service_name=checkout",
        None,
        &[],
    )
    .await;
    assert_eq!(fg["total"], 750, "{fg}");
}

#[tokio::test]
async fn folded_input_needs_a_service_name() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    let (status, body) = post_raw(
        &app,
        "/ingest?format=folded",
        "text/plain",
        folded_fixture(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn invalid_reports_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    let now = now_ns();

    // start after end
    let mut builder = ReportBuilder::new("svc", ProfileType::cpu(), now, now - 1_000);
    builder.add_stack(vec![Frame::new("/a", "b")], 1);
    let inverted = serde_json::to_value(builder.finish()).unwrap();
    let (status, body) = call(&app, "POST", "/ingest", Some(inverted), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // 65 labels
    let mut builder = ReportBuilder::new("svc", ProfileType::cpu(), now - 1_000, now);
    for i in 0..=lidlesseye_core::MAX_LABELS {
        builder = builder.label(format!("k{i}"), "v");
    }
    builder.add_stack(vec![Frame::new("/a", "b")], 1);
    let crowded = serde_json::to_value(builder.finish()).unwrap();
    let (status, body) = call(&app, "POST", "/ingest", Some(crowded), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // label value over the limit
    let long = "x".repeat(lidlesseye_core::MAX_LABEL_VALUE_LEN + 1);
    let mut builder =
        ReportBuilder::new("svc", ProfileType::cpu(), now - 1_000, now).label("k", &long);
    builder.add_stack(vec![Frame::new("/a", "b")], 1);
    let oversized = serde_json::to_value(builder.finish()).unwrap();
    let (status, body) = call(&app, "POST", "/ingest", Some(oversized), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // An empty service name.
    let mut builder = ReportBuilder::new("", ProfileType::cpu(), now - 1_000, now);
    builder.add_stack(vec![Frame::new("/a", "b")], 1);
    let nameless = serde_json::to_value(builder.finish()).unwrap();
    let (status, body) = call(&app, "POST", "/ingest", Some(nameless), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

/// A batch is all-or-nothing: one bad report must not leave the good ones
/// behind, otherwise a retry duplicates them.
#[tokio::test]
async fn a_batch_with_one_bad_report_stores_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    let now = now_ns();

    let mut bad = ReportBuilder::new("svc", ProfileType::cpu(), now, now - 5);
    bad.add_stack(vec![Frame::new("/a", "b")], 1);
    let payload = json!({
        "reports": [report("svc", "cpu", 1_000), serde_json::to_value(bad.finish()).unwrap()],
    });
    let (status, body) = call(&app, "POST", "/ingest", Some(payload), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (_, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?profile_type=cpu&from=now-1h",
        None,
        &[],
    )
    .await;
    assert_eq!(fg["total"], 0, "a rejected batch left data behind: {fg}");
}

#[tokio::test]
async fn a_batch_is_stored_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let payload = json!({ "reports": [
        report("svc", "cpu", 1_000),
        report("svc", "cpu", 2_000),
        report("other", "cpu", 3_000),
    ]});
    let (status, body) = call(&app, "POST", "/ingest", Some(payload), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["accepted"], 3);

    let (_, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?profile_type=cpu&from=now-1h&max_nodes=64",
        None,
        &[],
    )
    .await;
    assert_eq!(fg["total"], 6_000, "{fg}");
}

// ---------------------------------------------------------------------------
// queries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_parameters_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    call(
        &app,
        "POST",
        "/ingest",
        Some(report("svc", "cpu", 1_000)),
        &[],
    )
    .await;

    // `limit=0` must not error out.
    let (status, body) = call(&app, "GET", "/api/v1/top?from=now-1h&limit=0", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A deep stack is truncated once it exceeds the node budget.
    let now = now_ns();
    let stack: Vec<Frame> = (0..60)
        .rev()
        .map(|i| Frame::new("/app/deep", format!("fn{i}")))
        .collect();
    let mut builder = ReportBuilder::new("deep", ProfileType::cpu(), now - 1_000, now);
    builder.add_stack(stack, 5_000);
    call(
        &app,
        "POST",
        "/ingest",
        Some(serde_json::to_value(builder.finish()).unwrap()),
        &[],
    )
    .await;

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h&service_name=deep&max_nodes=16",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["truncated"], true, "{body}");
    assert!(
        body["names"].as_array().unwrap().len() <= 64,
        "max_nodes was ignored: {} nodes",
        body["names"].as_array().unwrap().len()
    );

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h&max_nodes=99999999999",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn selectors_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h&selector=%7Bservice_name%3D",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn unknown_profile_type_yields_an_empty_result() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    call(
        &app,
        "POST",
        "/ingest",
        Some(report("svc", "cpu", 1_000)),
        &[],
    )
    .await;

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h&profile_type=does-not-exist",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 0);
}

#[tokio::test]
async fn diff_of_an_empty_window_is_empty_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/diff?from=now-1h&until=now&baseline_from=now-2h&baseline_until=now-1h",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total_left"], 0);
    assert_eq!(body["total_right"], 0);
}

#[tokio::test]
async fn label_values_of_an_unknown_label_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    call(
        &app,
        "POST",
        "/ingest",
        Some(report("svc", "cpu", 1_000)),
        &[],
    )
    .await;

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/label/nope/values?from=now-1h",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["values"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// concurrency and volume
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_ingests_of_one_series_lose_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let mut handles = Vec::new();
    for worker in 0..8u64 {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..10u64 {
                let value = 10 + worker * 100 + i;
                let (status, body) = call(
                    &app,
                    "POST",
                    "/ingest",
                    Some(report("hot", "cpu", value)),
                    &[],
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{body}");
            }
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    let (_, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h&profile_type=cpu&max_nodes=4096",
        None,
        &[],
    )
    .await;
    let expected: u64 = (0..8u64)
        .flat_map(|w| (0..10u64).map(move |i| 10 + w * 100 + i))
        .sum();
    assert_eq!(fg["total"], expected, "samples were lost: {fg}");
}

#[tokio::test]
async fn concurrent_tenant_creation_opens_one_store() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || {
                let registry = lidlesseye_core::TenantStore::open(
                    &root,
                    lidlesseye_core::store::StoreConfig::new(&root),
                )
                .unwrap();
                let tenant = TenantId::parse("shared").unwrap();
                registry.open_tenant(&tenant).unwrap().root().to_path_buf()
            })
        })
        .collect();

    let mut paths: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), 1, "concurrent opens produced several stores");
}

#[tokio::test]
async fn many_series_can_be_ingested_in_bulk() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let reports: Vec<serde_json::Value> = (0..300)
        .map(|i| report_with_pod("bulk", &format!("pod-{i:03}"), 1_000))
        .collect();
    let (status, body) = call(
        &app,
        "POST",
        "/ingest",
        Some(json!({ "reports": reports })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["accepted"], 300);

    let (_, series) = call(&app, "GET", "/api/v1/series?from=now-1h", None, &[]).await;
    let rows = series["series"].as_array().unwrap();
    assert_eq!(rows.len(), 300, "expected one series per pod");

    let (_, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h&group_by=pod&max_nodes=4096",
        None,
        &[],
    )
    .await;
    assert_eq!(fg["total"], 300_000, "{fg}");
    let grouped = fg["names"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n.as_str().unwrap_or_default().starts_with("pod="))
        .count();
    assert_eq!(grouped, 300, "group_by did not split every pod");
}

// ---------------------------------------------------------------------------
// static surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dashboard_assets_are_served() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let response = get_raw(&app, "/").await;
    assert_eq!(response.status, StatusCode::OK, "GET /");
    assert!(!response.body.is_empty(), "GET / returned nothing");
    let index = response.text();

    assert!(index.contains("id=\"main\""), "dashboard HTML changed");

    // The shell is a static export, so the chunk names are content-hashed.
    // Follow what the HTML actually references rather than hard-coding a name.
    let script = index
        .split("src=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .find(|src| src.starts_with("/_next/static/") && src.ends_with(".js"))
        .expect("dashboard HTML references no JS chunk");
    let stylesheet = index
        .split("href=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .find(|href| href.starts_with("/_next/static/") && href.ends_with(".css"))
        .expect("dashboard HTML references no stylesheet");

    for path in [script, stylesheet, "/icon.svg"] {
        let asset = get_raw(&app, path).await;
        assert_eq!(asset.status, StatusCode::OK, "GET {path}");
        assert!(!asset.body.is_empty(), "GET {path} returned nothing");
    }

    // Hashed assets are served as immutable; the shell must not be.
    let asset = get_raw(&app, script).await;
    assert_eq!(
        asset
            .headers
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("public, max-age=31536000, immutable"),
        "hashed chunk is not cacheable"
    );
    let shell = get_raw(&app, "/").await;
    assert_eq!(
        shell
            .headers
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-cache"),
        "the shell must be revalidated"
    );
}

#[tokio::test]
async fn unknown_paths_are_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let response = get_raw(&app, "/api/v1/nope").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert!(
        response.json()["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("unknown endpoint"),
        "{}",
        response.text()
    );

    // A client-side route still resolves to the dashboard.
    let spa = get_raw(&app, "/compare").await;
    assert_eq!(spa.status, StatusCode::OK);
    assert!(spa.text().contains("id=\"main\""));
}

#[tokio::test]
async fn metrics_are_prometheus_shaped() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    call(
        &app,
        "POST",
        "/ingest",
        Some(report("svc", "cpu", 1_000)),
        &[],
    )
    .await;

    let response = get_raw(&app, "/metrics").await;
    assert_eq!(response.status, StatusCode::OK);
    let text = response.text();
    for needle in [
        "lidlesseye_ingest_requests",
        "lidlesseye_ingest_samples",
        "lidlesseye_tenants",
    ] {
        assert!(text.contains(needle), "metrics lack `{needle}`:\n{text}");
    }
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        assert!(
            line.split_whitespace().count() == 2,
            "malformed metrics line: `{line}`"
        );
    }
}

// ---------------------------------------------------------------------------
// HTTP surface details
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_oversized_body_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let app = router_with(
        dir.path(),
        lidlesseye_server::ServerConfig {
            max_body_bytes: 1024,
            ..config(dir.path())
        },
    )
    .await;

    let big = serde_json::to_vec(&report("svc", "cpu", 1_000)).unwrap();
    let mut padded = big.clone();
    padded.extend(std::iter::repeat_n(b' ', 4096));
    let response = request(
        &app,
        "POST",
        "/ingest",
        Some(padded),
        &[("content-type", "application/json")],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        response.text()
    );
}

#[tokio::test]
async fn conflicting_tenant_headers_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let response = request(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h",
        None,
        &[(TENANT_HEADER, "acme"), (TENANT_HEADER, "globex")],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "an ambiguous tenant must not be guessed: {}",
        response.text()
    );
}

#[tokio::test]
async fn invalid_step_is_rejected_and_buckets_are_clamped() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    call(
        &app,
        "POST",
        "/ingest",
        Some(report("svc", "cpu", 1_000)),
        &[],
    )
    .await;

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/timeseries?from=now-1h&step=three-minutes",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    for buckets in ["0", "1", "100000"] {
        let (status, body) = call(
            &app,
            "GET",
            &format!("/api/v1/timeseries?from=now-1h&buckets={buckets}"),
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "buckets={buckets}: {body}");
        let points = body["series"][0]["points"].as_array().unwrap();
        assert!(
            !points.is_empty() && points.len() <= 1000,
            "buckets={buckets} produced {} points",
            points.len()
        );
    }
}

#[tokio::test]
async fn the_label_aliases_agree() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    call(
        &app,
        "POST",
        "/ingest",
        Some(report("svc", "cpu", 1_000)),
        &[],
    )
    .await;

    let (_, a) = call(&app, "GET", "/api/v1/label-names?from=now-1h", None, &[]).await;
    let (_, b) = call(&app, "GET", "/api/v1/labels?from=now-1h", None, &[]).await;
    assert_eq!(a, b);
    let names: Vec<&str> = a["label_names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for expected in ["service_name", "env", "region"] {
        assert!(names.contains(&expected), "{names:?} lacks {expected}");
    }
}

#[tokio::test]
async fn liveness_and_readiness_report_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let (status, body) = call(&app, "GET", "/api/v1/health", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");

    let (status, _) = call(&app, "GET", "/api/v1/ready", None, &[]).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "an empty store is still ready"
    );

    call(
        &app,
        "POST",
        "/ingest",
        Some(report("svc", "cpu", 1_000)),
        &[],
    )
    .await;
    let (status, _) = call(&app, "GET", "/api/v1/ready", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "once data exists readiness is 200");

    let (_, version) = call(&app, "GET", "/api/v1/version", None, &[]).await;
    assert_eq!(version["multi_tenant"], true);
    assert_eq!(version["tenant_header"], TENANT_HEADER);
}

#[tokio::test]
async fn tenant_listing_is_scoped_to_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let app = router_with(
        dir.path(),
        lidlesseye_server::ServerConfig {
            auth_token: Some(TOKEN.to_string()),
            tenant_tokens: [("scoped-token".to_string(), "acme".to_string())]
                .into_iter()
                .collect(),
            ..config(dir.path())
        },
    )
    .await;

    for tenant in ["acme", "globex"] {
        let (status, _) = call(
            &app,
            "POST",
            "/ingest",
            Some(report("svc", "cpu", 100)),
            &[
                ("authorization", &format!("Bearer {TOKEN}")),
                (TENANT_HEADER, tenant),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    // A tenant-scoped token may not enumerate or read other tenants.
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/tenants",
        None,
        &[("authorization", "Bearer scoped-token")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["tenants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["acme"], "{body}");

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h",
        None,
        &[
            ("authorization", "Bearer scoped-token"),
            (TENANT_HEADER, "globex"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

// ---------------------------------------------------------------------------
// library-level invariants used by the embedded mode live in
// `crates/lidlesseye/tests/embedded.rs`, because that crate is the facade.
// ---------------------------------------------------------------------------
