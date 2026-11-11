//! Embedded-mode tests: the facade crate has to be usable as a library
//! without the binary, so its ingest/query path is exercised directly — first
//! in-process, then through a real socket (the integration suite other than
//! this file uses `oneshot` and never proves the listener works).

use std::collections::BTreeMap;

use lidlesseye_core::model::ProfileType;
use lidlesseye_core::{Frame, ReportBuilder, Selector, now_ns};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lidlesseye-test-{tag}-{}-{}",
        std::process::id(),
        now_ns()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn report(total: u64) -> lidlesseye_core::Report {
    let now = now_ns();
    let mut builder = ReportBuilder::new("embedded", ProfileType::cpu(), now - 1_000, now);
    builder.add_stack(vec![Frame::new("/srv", "work")], total);
    builder.finish()
}

#[tokio::test]
async fn embedded_ingest_and_query_agree() {
    let dir = temp_dir("embedded");
    let eye = lidlesseye::LidlessEye::builder()
        .data_dir(&dir)
        .listen("127.0.0.1:0")
        .agent(false)
        .build()
        .await
        .unwrap();

    eye.ingest(report(4_000)).await.unwrap();

    // The library path and the storage path must agree.
    let mut totals = BTreeMap::new();
    for row in eye.store().series(0, u64::MAX, &Selector::default()) {
        totals.insert(row.service_name.clone(), row.total_value);
    }
    assert_eq!(totals.get("embedded").copied(), Some(4_000));

    // And the same data is visible over a real socket.
    let running = eye.spawn();
    let base = running.base_url();
    let body: serde_json::Value = reqwest::get(format!(
        "{base}/api/v1/flamegraph?from=now-1h&profile_type=cpu"
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(body["total"], 4_000, "{body}");
    running.shutdown().await;

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn embedded_tenant_is_honoured() {
    let dir = temp_dir("tenant");
    let eye = lidlesseye::LidlessEye::builder()
        .data_dir(&dir)
        .listen("127.0.0.1:0")
        .agent(false)
        .tenant("acme")
        .build()
        .await
        .unwrap();
    assert_eq!(eye.tenant().as_str(), "acme");
    eye.ingest(report(500)).await.unwrap();

    let running = eye.spawn();
    let base = running.base_url();
    let client = reqwest::Client::new();

    let scoped: serde_json::Value = client
        .get(format!("{base}/api/v1/flamegraph?from=now-1h"))
        .header("X-Scope-OrgID", "acme")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(scoped["total"], 500, "{scoped}");

    let other: serde_json::Value = client
        .get(format!("{base}/api/v1/flamegraph?from=now-1h"))
        .header("X-Scope-OrgID", "other")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(other["total"], 0, "data leaked into another tenant");

    running.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two embedded instances must be able to coexist: the default listen address
/// is loopback with a fixed port, so this only holds when the caller picks an
/// ephemeral port — which is exactly what the docs recommend for tests.
#[tokio::test]
async fn two_instances_can_run_side_by_side() {
    let dir_a = temp_dir("side-a");
    let dir_b = temp_dir("side-b");
    let a = lidlesseye::LidlessEye::builder()
        .data_dir(&dir_a)
        .listen("127.0.0.1:0")
        .agent(false)
        .build()
        .await
        .unwrap()
        .spawn();
    let b = lidlesseye::LidlessEye::builder()
        .data_dir(&dir_b)
        .listen("127.0.0.1:0")
        .agent(false)
        .build()
        .await
        .unwrap()
        .spawn();
    assert_ne!(a.addr(), b.addr());
    assert_ne!(a.base_url(), b.base_url());
    a.shutdown().await;
    b.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

/// The library default must not publish an unauthenticated API to the network.
#[tokio::test]
async fn the_default_listen_address_is_loopback() {
    let dir = temp_dir("loopback");
    let eye = lidlesseye::LidlessEye::builder()
        .data_dir(&dir)
        .agent(false)
        .build()
        .await
        .unwrap();
    let addr = eye.local_addr();
    assert!(
        addr.ip().is_loopback(),
        "an unauthenticated embedded server bound {addr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
