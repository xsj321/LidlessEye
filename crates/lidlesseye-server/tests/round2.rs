//! Round 2 — security, tenant isolation, wire-format compatibility and quotas.
//!
//! Round 1 proved the happy paths and the obvious boundaries. This suite goes
//! after the ways a multi-tenant profiler can hurt you: a token that reaches
//! another tenant, a header that walks out of the data directory, a quota that
//! is bypassed by a second request, and the two ingestion formats Pyroscope
//! clients actually speak.

mod support;

use std::collections::BTreeMap;

use axum::http::StatusCode;
use lidlesseye_core::model::ProfileType;
use lidlesseye_core::{Frame, ReportBuilder, TenantLimits, now_ns};
use serde_json::json;
use support::*;

fn authed(
    dir: &std::path::Path,
    tenant_tokens: &[(&str, &str)],
) -> lidlesseye_server::ServerConfig {
    lidlesseye_server::ServerConfig {
        auth_token: Some(TOKEN.to_string()),
        tenant_tokens: tenant_tokens
            .iter()
            .map(|(token, tenant)| (token.to_string(), tenant.to_string()))
            .collect(),
        ..config(dir)
    }
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

// ---------------------------------------------------------------------------
// authentication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn authentication_is_mandatory_once_a_token_is_configured() {
    let dir = tempfile::tempdir().unwrap();
    let app = router_with(dir.path(), authed(dir.path(), &[])).await;

    for uri in [
        "/api/v1/flamegraph?from=now-1h",
        "/api/v1/tenants",
        "/api/v1/stats",
    ] {
        let (status, body) = call(&app, "GET", uri, None, &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "GET {uri}: {body}");
    }
    let (status, body) = call(&app, "POST", "/ingest", Some(report("api", "cpu", 1)), &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "POST /ingest: {body}");

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h",
        None,
        &[("authorization", &bearer("wrong"))],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h",
        None,
        &[("authorization", &bearer(TOKEN))],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Liveness, readiness, version and metrics stay open so probes and
    // Prometheus work without distributing the token.
    for uri in [
        "/api/v1/health",
        "/api/v1/ready",
        "/api/v1/version",
        "/metrics",
    ] {
        let response = get_raw(&app, uri).await;
        assert!(
            response.status.is_success(),
            "{uri} needs a token: {}",
            response.status
        );
    }
}

#[tokio::test]
async fn the_authorization_prefix_must_be_exact() {
    let dir = tempfile::tempdir().unwrap();
    let app = router_with(dir.path(), authed(dir.path(), &[])).await;

    for header in [
        format!("bearer {TOKEN}"),
        format!("Token {TOKEN}"),
        TOKEN.to_string(),
        format!("Basic {TOKEN}"),
    ] {
        let (status, _) = call(
            &app,
            "GET",
            "/api/v1/flamegraph?from=now-1h",
            None,
            &[("authorization", &header)],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "`{header}` should not authenticate"
        );
    }
}

#[tokio::test]
async fn a_scoped_token_cannot_reach_another_tenant() {
    let dir = tempfile::tempdir().unwrap();
    let app = router_with(dir.path(), authed(dir.path(), &[("acme-token", "acme")])).await;

    let (status, body) = call(
        &app,
        "POST",
        "/ingest",
        Some(report("api", "cpu", 1_000)),
        &[("authorization", &bearer("acme-token"))],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tenant"], "acme");

    // Naming another tenant is refused, not silently rewritten.
    for tenant in ["globex", "default", "ACME.OTHER"] {
        let (status, body) = call(
            &app,
            "POST",
            "/ingest",
            Some(report("api", "cpu", 1_000)),
            &[
                ("authorization", &bearer("acme-token")),
                (TENANT_HEADER, tenant),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{tenant}: {body}");

        let (status, body) = call(
            &app,
            "GET",
            "/api/v1/flamegraph?from=now-1h",
            None,
            &[
                ("authorization", &bearer("acme-token")),
                (TENANT_HEADER, tenant),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{tenant}: {body}");
    }

    // The same tenant in a different case is the same tenant, so it is fine.
    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h",
        None,
        &[
            ("authorization", &bearer("acme-token")),
            (TENANT_HEADER, "ACME"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 1_000);
}

#[tokio::test]
async fn the_allow_list_is_enforced_on_reads_and_writes() {
    let dir = tempfile::tempdir().unwrap();
    let app = router_with(
        dir.path(),
        lidlesseye_server::ServerConfig {
            allowed_tenants: Some(vec!["acme".to_string()]),
            ..config(dir.path())
        },
    )
    .await;

    let (status, _) = call_as(
        &app,
        "POST",
        "/ingest",
        Some(report("api", "cpu", 100)),
        "acme",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for tenant in ["globex", "default"] {
        let (status, body) = call_as(
            &app,
            "POST",
            "/ingest",
            Some(report("api", "cpu", 100)),
            tenant,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{tenant}: {body}");

        let (status, body) =
            call_as(&app, "GET", "/api/v1/flamegraph?from=now-1h", None, tenant).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{tenant}: {body}");
    }

    // The list is matched after normalization, so `ACME` is allowed.
    let (status, _) = call_as(&app, "GET", "/api/v1/tenants", None, "ACME").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_read_only_server_refuses_writes_but_answers_queries() {
    let dir = tempfile::tempdir().unwrap();
    let writable = router(dir.path()).await;
    call(
        &writable,
        "POST",
        "/ingest",
        Some(report("api", "cpu", 700)),
        &[],
    )
    .await;
    drop(writable);

    let app = router_with(
        dir.path(),
        lidlesseye_server::ServerConfig {
            read_only: true,
            ..config(dir.path())
        },
    )
    .await;

    let (status, body) = call(
        &app,
        "POST",
        "/ingest",
        Some(report("api", "cpu", 700)),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = call(&app, "GET", "/api/v1/flamegraph?from=now-1h", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 700);
}

// ---------------------------------------------------------------------------
// isolation and path safety
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hostile_service_names_cannot_escape_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let hostile = [
        "../../../etc/passwd",
        "..\\..\\windows",
        "a/b/c",
        "con",
        "aux.",
        "",
        "   ",
        "\u{202e}evil",
    ];
    for name in hostile {
        let now = now_ns();
        let mut builder = ReportBuilder::new(name, ProfileType::cpu(), now - 1_000, now);
        builder.add_stack(vec![Frame::new("/bin/app", "main")], 100);
        let (status, body) = call(
            &app,
            "POST",
            "/ingest",
            Some(serde_json::to_value(builder.finish()).unwrap()),
            &[],
        )
        .await;
        // Either refused, or stored under a slug — never written outside.
        assert!(
            status == StatusCode::OK || status == StatusCode::BAD_REQUEST,
            "`{name}` produced {status}: {body}"
        );
    }

    // Whatever happened, the data directory holds only its own subdirectories.
    let root = dir.path().join("tenants").join("default");
    let mut stack = vec![root];
    while let Some(path) = stack.pop() {
        if !path.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !name.contains("..") && !name.contains('/') && !name.contains('\\'),
                "unsafe path component `{name}` under {}",
                path.display()
            );
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            }
        }
    }
    assert!(!std::path::Path::new("/etc/passwd.leseg").exists());
}

#[tokio::test]
async fn hostile_profile_types_become_slugs() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    for profile_type in ["../../evil", "..", "a/b", ""] {
        let now = now_ns();
        let mut builder = ReportBuilder::new("svc", ProfileType::new(profile_type), now - 1, now);
        builder.add_stack(vec![Frame::new("/bin/app", "main")], 5);
        let (status, body) = call(
            &app,
            "POST",
            "/ingest",
            Some(serde_json::to_value(builder.finish()).unwrap()),
            &[],
        )
        .await;
        assert!(
            status == StatusCode::OK || status == StatusCode::BAD_REQUEST,
            "`{profile_type}` produced {status}: {body}"
        );
    }

    let types = std::fs::read_dir(dir.path().join("tenants").join("default"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    for name in &types {
        assert!(
            !name.contains("..") && !name.contains('/') && !name.contains('\\'),
            "unsafe profile type directory `{name}`"
        );
    }
}

#[tokio::test]
async fn one_tenant_cannot_see_another_through_any_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    call_as(
        &app,
        "POST",
        "/ingest",
        Some(report("secret", "cpu", 9_999)),
        "alpha",
    )
    .await;
    call_as(
        &app,
        "POST",
        "/ingest",
        Some(report("public", "cpu", 1)),
        "beta",
    )
    .await;

    let endpoints = [
        "/api/v1/flamegraph?from=now-1h",
        "/api/v1/top?from=now-1h",
        "/api/v1/timeseries?from=now-1h&buckets=4",
        "/api/v1/series?from=now-1h",
        "/api/v1/label-names?from=now-1h",
        "/api/v1/label/service_name/values?from=now-1h",
        "/api/v1/profile_types?from=now-1h",
        "/api/v1/diff?from=now-1h&until=now",
        "/api/v1/stats",
        "/metrics",
    ];

    for endpoint in endpoints {
        let response = request(&app, "GET", endpoint, None, &[(TENANT_HEADER, "beta")]).await;
        assert_eq!(response.status, StatusCode::OK, "{endpoint}");
        let text = response.text();
        assert!(
            !text.contains("secret"),
            "{endpoint} leaked another tenant's service name: {text}"
        );
        assert!(
            !text.contains("9999"),
            "{endpoint} leaked another tenant's values: {text}"
        );
    }
}

#[tokio::test]
async fn metrics_do_not_name_tenants() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    call_as(
        &app,
        "POST",
        "/ingest",
        Some(report("api", "cpu", 100)),
        "customer-acme",
    )
    .await;

    let text = get_raw(&app, "/metrics").await.text();
    assert!(
        !text.contains("customer-acme"),
        "metrics must not disclose tenant ids:\n{text}"
    );
    assert!(text.contains("lidlesseye_tenants"));
}

#[tokio::test]
async fn cors_allows_the_tenant_header() {
    let dir = tempfile::tempdir().unwrap();
    let app = router_with(
        dir.path(),
        lidlesseye_server::ServerConfig {
            cors_allow_origin: Some("https://grafana.example".to_string()),
            ..config(dir.path())
        },
    )
    .await;

    let response = request(
        &app,
        "OPTIONS",
        "/api/v1/flamegraph",
        None,
        &[
            ("origin", "https://grafana.example"),
            ("access-control-request-method", "GET"),
            (
                "access-control-request-headers",
                "x-scope-orgid,authorization",
            ),
        ],
    )
    .await;
    assert!(
        response.status.is_success(),
        "preflight failed: {}",
        response.status
    );
    let allowed = response
        .header("access-control-allow-headers")
        .to_ascii_lowercase();
    assert!(
        allowed.contains("x-scope-orgid"),
        "the tenant header is not allowed by CORS: `{allowed}`"
    );
    assert!(
        allowed.contains("authorization"),
        "the auth header is not allowed by CORS: `{allowed}`"
    );
}

// ---------------------------------------------------------------------------
// quotas
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_tenant_cap_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let app = router_with(
        dir.path(),
        lidlesseye_server::ServerConfig {
            max_tenants: 2,
            ..config(dir.path())
        },
    )
    .await;

    for tenant in ["one", "two"] {
        let (status, body) = call_as(
            &app,
            "POST",
            "/ingest",
            Some(report("api", "cpu", 10)),
            tenant,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{tenant}: {body}");
    }

    let (status, body) = call_as(
        &app,
        "POST",
        "/ingest",
        Some(report("api", "cpu", 10)),
        "three",
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

    // Existing tenants keep working.
    let (status, body) = call_as(
        &app,
        "POST",
        "/ingest",
        Some(report("api", "cpu", 10)),
        "one",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn the_series_quota_blocks_only_new_series() {
    let dir = tempfile::tempdir().unwrap();
    let mut limits = BTreeMap::new();
    limits.insert(
        "acme".to_string(),
        TenantLimits {
            max_series: Some(2),
            ..Default::default()
        },
    );
    let app = router_with(
        dir.path(),
        lidlesseye_server::ServerConfig {
            tenant_limits: limits,
            ..config(dir.path())
        },
    )
    .await;

    for pod in ["p1", "p2"] {
        let (status, body) = call_as(
            &app,
            "POST",
            "/ingest",
            Some(report_with_pod("api", pod, 100)),
            "acme",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{pod}: {body}");
    }

    let (status, body) = call_as(
        &app,
        "POST",
        "/ingest",
        Some(report_with_pod("api", "p3", 100)),
        "acme",
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

    // Writing to an existing series is still allowed.
    let (status, body) = call_as(
        &app,
        "POST",
        "/ingest",
        Some(report_with_pod("api", "p1", 100)),
        "acme",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Another tenant is unaffected.
    let (status, body) = call_as(
        &app,
        "POST",
        "/ingest",
        Some(report_with_pod("api", "p3", 100)),
        "globex",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_rejected_batch_leaves_no_partial_series() {
    let dir = tempfile::tempdir().unwrap();
    let mut limits = BTreeMap::new();
    limits.insert(
        "acme".to_string(),
        TenantLimits {
            max_series: Some(1),
            ..Default::default()
        },
    );
    let app = router_with(
        dir.path(),
        lidlesseye_server::ServerConfig {
            tenant_limits: limits,
            ..config(dir.path())
        },
    )
    .await;

    let payload = json!({ "reports": [
        report_with_pod("api", "p1", 100),
        report_with_pod("api", "p2", 100),
        report_with_pod("api", "p3", 100),
    ]});
    let (status, body) = call_as(&app, "POST", "/ingest", Some(payload), "acme").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

    let (_, series) = call_as(&app, "GET", "/api/v1/series?from=now-1h", None, "acme").await;
    assert_eq!(
        series["series"].as_array().unwrap().len(),
        0,
        "a rejected batch created series: {series}"
    );
}

// ---------------------------------------------------------------------------
// wire-format compatibility
// ---------------------------------------------------------------------------

/// pprof lists frames leaf first. A backend that gets this backwards produces
/// an upside-down flamegraph, which is the classic pprof-interop bug.
#[tokio::test]
async fn pprof_stack_direction_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let (status, body) = post_raw(
        &app,
        "/ingest?format=pprof&name=checkout",
        "application/vnd.google.protobuf",
        pprof_fixture(),
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
    assert_eq!(fg["total"], 123_456, "{fg}");

    let names: Vec<String> = fg["names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let levels: Vec<i64> = fg["levels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    let depth = |name: &str| -> i64 {
        names
            .iter()
            .position(|n| n == name)
            .map(|i| levels[i])
            .unwrap_or(-99)
    };
    assert_eq!(
        depth("main"),
        0,
        "`main` must be the outermost frame: {names:?}"
    );
    assert_eq!(
        depth("handle_request"),
        1,
        "`handle_request` must sit under `main`: {names:?}"
    );
}

#[tokio::test]
async fn pprof_profile_types_are_mapped() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    // No `sample_type` mapping is needed: the query parameter wins.
    let (status, _) = post_raw(
        &app,
        "/ingest?format=pprof&service_name=explicit&profile_type=alloc_space",
        "application/vnd.google.protobuf",
        pprof_fixture(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, types) = call(&app, "GET", "/api/v1/profile_types?from=now-1h", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{types}");
    let names: Vec<&str> = types["profile_types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"alloc_space"), "{names:?}");
}

#[tokio::test]
async fn content_type_selects_the_decoder_when_format_is_absent() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let (status, body) = post_raw(
        &app,
        "/ingest?service_name=ct-proto",
        "application/vnd.google.protobuf",
        pprof_fixture(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["format"], "pprof");

    let (status, body) = post_raw(
        &app,
        "/ingest?service_name=ct-folded",
        "text/plain; charset=utf-8",
        folded_fixture(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["format"], "folded");

    // An explicit `format` overrides a misleading content type.
    let (status, body) = post_raw(
        &app,
        "/ingest?format=folded&service_name=ct-explicit",
        "application/json",
        folded_fixture(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["format"], "folded");
}

#[tokio::test]
async fn pprof_sample_labels_become_series_labels() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let (status, body) = post_raw(
        &app,
        "/ingest?format=pprof&service_name=labeled",
        "application/vnd.google.protobuf",
        pprof_fixture_labeled("pod", "api-1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // One sample carried a pprof label, the other did not, so the decoder must
    // emit two series rather than one.
    let (_, series) = call(
        &app,
        "GET",
        "/api/v1/series?from=now-1h&service_name=labeled",
        None,
        &[],
    )
    .await;
    let rows = series["series"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        2,
        "pprof sample labels did not split series: {series}"
    );

    let (_, names) = call(
        &app,
        "GET",
        "/api/v1/label-names?from=now-1h&service_name=labeled",
        None,
        &[],
    )
    .await;
    let keys: Vec<&str> = names["label_names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        keys.contains(&"pod"),
        "pprof sample labels were dropped: {keys:?}"
    );

    let (_, values) = call(
        &app,
        "GET",
        "/api/v1/label/pod/values?from=now-1h",
        None,
        &[],
    )
    .await;
    assert_eq!(values["values"][0]["value"], "api-1", "{values}");
}

#[tokio::test]
async fn folded_frames_are_root_first() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let (status, _) = post_raw(
        &app,
        "/ingest?format=folded&service_name=folded",
        "text/plain",
        b"main;serve;handle 900\nmain;serve 100\n".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h&service_name=folded",
        None,
        &[],
    )
    .await;
    let names: Vec<String> = fg["names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let levels: Vec<i64> = fg["levels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    let depth = |name: &str| -> i64 {
        names
            .iter()
            .position(|n| n == name)
            .map(|i| levels[i])
            .unwrap_or(-99)
    };
    assert_eq!(depth("main"), 0, "{names:?}");
    assert_eq!(depth("serve"), 1, "{names:?}");
    assert_eq!(depth("handle"), 2, "{names:?}");
}

#[tokio::test]
async fn pprof_without_a_service_name_is_refused_even_with_a_label() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let (status, body) = post_raw(
        &app,
        "/ingest?format=pprof&label=pod=x",
        "application/vnd.google.protobuf",
        pprof_fixture(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("service name"),
        "{body}"
    );
}

#[tokio::test]
async fn documents_sent_with_gzip_content_encoding_are_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder
        .write_all(&serde_json::to_vec(&report("gz", "cpu", 321)).unwrap())
        .unwrap();
    let gz = encoder.finish().unwrap();

    let response = request(
        &app,
        "POST",
        "/ingest",
        Some(gz),
        &[
            ("content-type", "application/json"),
            ("content-encoding", "gzip"),
        ],
    )
    .await;
    assert!(
        response.status.is_success(),
        "gzip request body failed: {}",
        response.text()
    );

    let (_, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h&service_name=gz",
        None,
        &[],
    )
    .await;
    assert_eq!(fg["total"], 321, "{fg}");
}

#[tokio::test]
async fn gzip_responses_are_negotiated() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    call(&app, "POST", "/ingest", Some(report("gz", "cpu", 42)), &[]).await;

    let response = request(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h",
        None,
        &[("accept-encoding", "gzip")],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.header("content-encoding"),
        "gzip",
        "responses are not compressed for gzip-capable clients"
    );
}

/// `/api/v1/stats` must describe the caller's tenant, not the process.
///
/// The numbers used to come straight from the process-wide atomics, so a
/// tenant-scoped token could read how much every other tenant ingests, and the
/// dashboard showed a total that did not match the series listed below it.
#[tokio::test]
async fn stats_are_scoped_to_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    // Two reports with two stacks each for acme, one report with one for zeta.
    let batch = serde_json::to_vec(&json!({
        "reports": [report("acme-api", "cpu", 10), report("acme-web", "cpu", 20)],
    }))
    .unwrap();
    let accepted = request(
        &app,
        "POST",
        "/ingest",
        Some(batch),
        &[("x-scope-orgid", "acme")],
    )
    .await;
    assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.text());
    let (status, _) = call_as(
        &app,
        "POST",
        "/ingest",
        Some(report("zeta-api", "cpu", 30)),
        "zeta",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A query made by acme only counts against acme.
    let (status, _) = call_as(&app, "GET", "/api/v1/flamegraph?from=now-1h", None, "acme").await;
    assert_eq!(status, StatusCode::OK);

    let (status, acme) = call_as(&app, "GET", "/api/v1/stats", None, "acme").await;
    assert_eq!(status, StatusCode::OK, "{acme}");
    assert_eq!(acme["tenant"], "acme");
    assert_eq!(acme["ingest_reports"], 2, "{acme}");
    assert_eq!(acme["ingest_samples"], 4, "{acme}");
    assert_eq!(acme["queries"], 1, "{acme}");
    assert_eq!(acme["series"], 2, "{acme}");

    let (status, zeta) = call_as(&app, "GET", "/api/v1/stats", None, "zeta").await;
    assert_eq!(status, StatusCode::OK, "{zeta}");
    assert_eq!(
        zeta["ingest_reports"], 1,
        "zeta sees another tenant's ingest count: {zeta}"
    );
    assert_eq!(zeta["ingest_samples"], 2, "{zeta}");
    assert_eq!(
        zeta["queries"], 0,
        "zeta sees another tenant's query count: {zeta}"
    );

    // The process-wide totals are still on /metrics, where the operator looks.
    let metrics = get_raw(&app, "/metrics").await;
    let text = metrics.text();
    assert!(
        text.contains("lidlesseye_ingest_reports_total 3"),
        "the operator lost the global counter: {text}"
    );
}
