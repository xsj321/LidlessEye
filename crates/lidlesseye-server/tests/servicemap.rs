//! The service map: ingest, window aggregation and the join with profiles.

mod support;

use axum::http::StatusCode;
use serde_json::{Value, json};
use support::{call, router};

fn batch(start: u64, checkout_connections: u64) -> Value {
    json!({
        "start_ns": start,
        "end_ns": start + 10_000_000_000u64,
        "node": "node-a",
        "nodes": [
            {
                "id": "shop/checkout",
                "namespace": "shop",
                "workload": "checkout",
                "service": "checkout",
                "kind": "workload",
                "rx_bytes": 1000,
                "tx_bytes": 2000,
                "connections": checkout_connections,
                "retransmits": 1
            },
            {
                "id": "shop/payments",
                "namespace": "shop",
                "workload": "payments",
                "service": "payments",
                "kind": "workload",
                "connections": 0
            }
        ],
        "edges": [{
            "client": "shop/checkout",
            "server": "shop/payments",
            "protocol": "postgres",
            "connections": checkout_connections,
            "retransmits": 1
        }]
    })
}

#[tokio::test]
async fn service_map_round_trips_and_sums_windows() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    let now = lidlesseye_core::now_ns();
    let start = now - 60_000_000_000;

    for offset in [0u64, 10_000_000_000] {
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/servicemap/ingest",
            Some(json!({ "batches": [batch(start + offset, 3)] })),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["accepted"], 1);
    }

    let (status, body) = call(&app, "GET", "/api/v1/servicemap?from=now-5m", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let nodes = body["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 2, "{body}");
    let checkout = nodes
        .iter()
        .find(|node| node["id"] == "shop/checkout")
        .unwrap();
    assert_eq!(checkout["connections"], 6, "windows are summed");
    assert_eq!(checkout["tx_bytes"], 4000);
    assert_eq!(checkout["kind"], "workload");

    let edges = body["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["client"], "shop/checkout");
    assert_eq!(edges[0]["protocol"], "postgres");
    assert_eq!(edges[0]["connections"], 6);
}

#[tokio::test]
async fn service_map_ignores_windows_outside_the_range() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    let now = lidlesseye_core::now_ns();
    // Two hours old: outside the default one-hour range.
    let start = now - 2 * 3_600_000_000_000;

    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/servicemap/ingest",
        Some(json!({ "batches": [batch(start, 1)] })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(&app, "GET", "/api/v1/servicemap", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["nodes"].as_array().unwrap().is_empty(), "{body}");

    let (status, body) = call(
        &app,
        "GET",
        "/api/v1/servicemap?from=now-3h&until=now-90m",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["nodes"].as_array().unwrap().len(), 2, "{body}");
}

#[tokio::test]
async fn nodes_carry_the_cpu_of_the_profiles_behind_them() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    let now = lidlesseye_core::now_ns();

    let report = json!({
        "service_name": "checkout",
        "labels": { "k8s_namespace": "shop", "k8s_workload": "checkout" },
        "profile_type": "cpu",
        "start_ns": now - 60_000_000_000u64,
        "end_ns": now - 50_000_000_000u64,
        "strings": ["/bin/checkout", "main", ""],
        "stacks": [{ "frames": [[0, 1, 2, 0]], "value": 2_000_000_000u64 }]
    });
    let (status, body) = call(&app, "POST", "/ingest", Some(report), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/servicemap/ingest",
        Some(json!({ "batches": [batch(now - 60_000_000_000, 1)] })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(&app, "GET", "/api/v1/servicemap?from=now-5m", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let checkout = body["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == "shop/checkout")
        .unwrap();
    assert_eq!(
        checkout["cpu_ns"], 2_000_000_000u64,
        "the workload label is the join key: {body}"
    );
}
