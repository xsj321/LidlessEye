//! Round 3 — volume, concurrency, retention, integrity and long-run stability.
//!
//! Rounds 1 and 2 answered "does it work" and "can it be abused". This suite
//! answers "does it still work after a lot of it", and it deliberately mixes
//! writers, readers and maintenance on the same store, which is how a real
//! deployment behaves.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use lidlesseye_core::model::{MAX_FRAMES_PER_STACK, ProfileType};
use lidlesseye_core::store::{DB_FILE, StoreConfig};
use lidlesseye_core::{
    Frame, QueryOptions, ReportBuilder, Selector, TenantId, TenantLimits, TenantStore, flamegraph,
    now_ns,
};
use serde_json::json;
use support::*;

/// A report whose every stack is worth exactly `per_stack`.
fn deep_report(service: &str, per_stack: u64, stacks: usize) -> serde_json::Value {
    let now = now_ns();
    let mut builder = ReportBuilder::new(service, ProfileType::cpu(), now - 1_000_000, now);
    for i in 0..stacks {
        builder.add_stack(
            vec![
                Frame::new("/app/busy", format!("leaf{i}")),
                Frame::new("/app/busy", "main"),
            ],
            per_stack,
        );
    }
    serde_json::to_value(builder.finish()).unwrap()
}

// ---------------------------------------------------------------------------
// volume
// ---------------------------------------------------------------------------

/// 2000 individual ingest requests: a year of a 99 Hz sampler on a small fleet.
#[tokio::test]
async fn ingest_throughput_holds_up() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let started = Instant::now();
    let mut accepted = 0usize;
    for i in 0..2_000u64 {
        let (status, body) = call(
            &app,
            "POST",
            "/ingest",
            Some(deep_report("bulk", 10_000 + i, 4)),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "request {i}: {body}");
        accepted += 1;
    }
    let elapsed = started.elapsed();
    println!(
        "ingest: {accepted} requests in {elapsed:?} ({:.0}/s)",
        accepted as f64 / elapsed.as_secs_f64()
    );
    assert_eq!(accepted, 2_000);
    assert!(
        elapsed < Duration::from_secs(60),
        "2000 ingests took {elapsed:?}"
    );

    let started = Instant::now();
    let (status, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h&max_nodes=8192",
        None,
        &[],
    )
    .await;
    let query = started.elapsed();
    println!("flamegraph over 2000 segments: {query:?}");
    assert_eq!(status, StatusCode::OK, "{fg}");
    assert!(fg["total"].as_u64().unwrap() > 0);
    assert!(
        query < Duration::from_secs(30),
        "the flamegraph query took {query:?}"
    );
}

#[tokio::test]
async fn many_series_stay_queryable() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let started = Instant::now();
    for batch in 0..10u64 {
        let reports: Vec<serde_json::Value> = (0..100)
            .map(|i| report_with_pod("fleet", &format!("pod-{batch}-{i:03}"), 1_000))
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
    }
    println!("1000 series ingested in {:?}", started.elapsed());

    let started = Instant::now();
    let (status, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h&group_by=pod&max_nodes=2000",
        None,
        &[],
    )
    .await;
    println!(
        "grouped flamegraph over 1000 series: {:?}",
        started.elapsed()
    );
    assert_eq!(status, StatusCode::OK, "{fg}");
    assert_eq!(fg["total"], 1_000_000);

    let started = Instant::now();
    let (status, ts) = call(
        &app,
        "GET",
        "/api/v1/timeseries?from=now-1h&buckets=1000&group_by=pod",
        None,
        &[],
    )
    .await;
    println!(
        "timeseries with 1000 buckets x 1000 series: {:?}",
        started.elapsed()
    );
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ts["series"].as_array().unwrap().len(), 1_000);
    assert_eq!(
        ts["series"][0]["points"].as_array().unwrap().len(),
        1_000,
        "bucket count was not honoured"
    );
}

/// A stack longer than the format allows is refused, not silently truncated —
/// and a stack exactly at the limit still renders without blowing the stack.
#[tokio::test]
async fn stack_depth_is_bounded_at_both_ends() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    let now = now_ns();

    let mut builder = ReportBuilder::new("deep", ProfileType::cpu(), now - 1_000, now);
    let absurd: Vec<Frame> = (0..50_000)
        .map(|i| Frame::new("/app/deep", format!("fn{i}")))
        .collect();
    builder.add_stack(absurd, 1_000);
    let (status, body) = call(
        &app,
        "POST",
        "/ingest",
        Some(serde_json::to_value(builder.finish()).unwrap()),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("frames"),
        "{body}"
    );

    let mut builder = ReportBuilder::new("deep", ProfileType::cpu(), now - 1_000, now);
    let at_limit: Vec<Frame> = (0..MAX_FRAMES_PER_STACK)
        .map(|i| Frame::new("/app/deep", format!("fn{i}")))
        .collect();
    builder.add_stack(at_limit, 1_000);
    let (status, body) = call(
        &app,
        "POST",
        "/ingest",
        Some(serde_json::to_value(builder.finish()).unwrap()),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let started = Instant::now();
    let (status, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h&max_nodes=50000",
        None,
        &[],
    )
    .await;
    println!(
        "a {MAX_FRAMES_PER_STACK}-frame stack rendered in {:?}",
        started.elapsed()
    );
    assert_eq!(status, StatusCode::OK, "{fg}");
    assert_eq!(fg["total"], 1_000);
    assert!(fg["names"].as_array().unwrap().len() >= MAX_FRAMES_PER_STACK);
}

// ---------------------------------------------------------------------------
// concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn readers_and_writers_can_run_together() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;
    // Seed so queries have something to read from the very first iteration.
    call(
        &app,
        "POST",
        "/ingest",
        Some(deep_report("mixed", 1_000, 3)),
        &[],
    )
    .await;

    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let app = app.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut written = 0u64;
            for _ in 0..200 {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let (status, body) = call(
                    &app,
                    "POST",
                    "/ingest",
                    Some(deep_report("mixed", 100, 3)),
                    &[],
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{body}");
                written += 300;
            }
            written
        })
    };

    let mut readers = Vec::new();
    for _ in 0..4 {
        let app = app.clone();
        let stop = stop.clone();
        readers.push(tokio::spawn(async move {
            let mut queries = 0u64;
            while !stop.load(Ordering::Relaxed) && queries < 100 {
                for uri in [
                    "/api/v1/flamegraph?from=now-1h&max_nodes=256",
                    "/api/v1/top?from=now-1h&limit=10",
                    "/api/v1/series?from=now-1h",
                    "/api/v1/timeseries?from=now-1h&buckets=32",
                ] {
                    let (status, body) = call(&app, "GET", uri, None, &[]).await;
                    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
                    // A flamegraph must never come back internally inconsistent.
                    if let Some(names) = body["names"].as_array() {
                        let values = body["values"].as_array().unwrap();
                        assert_eq!(names.len(), values.len(), "{uri}: {body}");
                        let levels = body["levels"].as_array().unwrap();
                        assert_eq!(names.len(), levels.len(), "{uri}: {body}");
                    }
                }
                queries += 1;
            }
            queries
        }));
    }

    let written = writer.await.unwrap();
    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        assert!(reader.await.unwrap() > 0);
    }

    let (_, fg) = call(
        &app,
        "GET",
        "/api/v1/flamegraph?from=now-1h&max_nodes=512",
        None,
        &[],
    )
    .await;
    assert_eq!(
        fg["total"],
        3_000 + written,
        "concurrent writes lost samples: {fg}"
    );
}

#[tokio::test]
async fn pruning_while_ingesting_keeps_the_index_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(
        TenantStore::open(
            dir.path(),
            StoreConfig {
                root: dir.path().to_path_buf(),
                retention_ns: Some(3_600_000_000_000),
                max_disk_bytes: None,
                threads: None,
            },
        )
        .unwrap(),
    );
    let tenant = TenantId::parse("pruned").unwrap();

    let writer = {
        let registry = registry.clone();
        let tenant = tenant.clone();
        tokio::task::spawn_blocking(move || {
            let store = registry.open_tenant(&tenant).unwrap();
            for i in 0..300u64 {
                let now = now_ns();
                let mut builder = ReportBuilder::new("svc", ProfileType::cpu(), now - 1_000, now);
                builder.add_stack(vec![Frame::new("/app", "work")], 100 + i);
                store.append(&builder.finish()).unwrap();
            }
        })
    };

    let pruner = {
        let registry = registry.clone();
        let tenant = tenant.clone();
        tokio::task::spawn_blocking(move || {
            for _ in 0..50 {
                let store = registry.open_tenant(&tenant).unwrap();
                store.prune(now_ns()).unwrap();
            }
        })
    };

    writer.await.unwrap();
    pruner.await.unwrap();

    let store = registry.open_tenant(&tenant).unwrap();
    // Nothing is older than the retention window, so pruning must not have
    // removed anything, and the index must still match the files on disk.
    assert_eq!(
        store.stats().segment_count,
        300,
        "pruning dropped live segments"
    );
    assert!(store.fsck().iter().all(|(_, result)| result.is_ok()));

    let fg = flamegraph(
        &store,
        &QueryOptions::new("cpu", 0, u64::MAX, Selector::default()),
    )
    .unwrap();
    let expected: i64 = (0..300).map(|i| 100 + i).sum();
    assert_eq!(fg.total, expected);
}

// ---------------------------------------------------------------------------
// retention and eviction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retention_removes_only_expired_segments() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ns();
    let day = 86_400_000_000_000u64;

    let registry = TenantStore::open(
        dir.path(),
        StoreConfig {
            root: dir.path().to_path_buf(),
            retention_ns: Some(7 * day),
            max_disk_bytes: None,
            threads: None,
        },
    )
    .unwrap();
    let tenant = TenantId::parse("acme").unwrap();
    let store = registry.open_tenant(&tenant).unwrap();

    for (age_days, value) in [(10u64, 1_000u64), (8, 2_000), (1, 3_000), (0, 4_000)] {
        let end = now - age_days * day;
        let mut builder = ReportBuilder::new("svc", ProfileType::cpu(), end - 1_000_000, end);
        builder.add_stack(vec![Frame::new("/app", "work")], value);
        store.append(&builder.finish()).unwrap();
    }
    assert_eq!(store.stats().segment_count, 4);

    let pruned = registry.prune(now).unwrap();
    let removed: usize = pruned.values().map(|stats| stats.removed_segments).sum();
    assert_eq!(removed, 2, "8 and 10 day old data must go");
    assert_eq!(store.stats().segment_count, 2);

    let fg = flamegraph(
        &store,
        &QueryOptions::new("cpu", 0, u64::MAX, Selector::default()),
    )
    .unwrap();
    assert_eq!(fg.total, 3_000 + 4_000, "the wrong segments survived");

    // Pruning again is a no-op.
    assert!(
        registry.prune(now).unwrap().is_empty(),
        "a second prune removed more data"
    );
}

#[tokio::test]
async fn the_disk_cap_evicts_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ns();

    {
        let registry = TenantStore::open(dir.path(), StoreConfig::new(dir.path())).unwrap();
        let tenant = TenantId::parse("acme").unwrap();
        let store = registry.open_tenant(&tenant).unwrap();
        // 20 segments, one per minute, growing in size.
        for i in 0..20u64 {
            let end = now - (20 - i) * 60_000_000_000;
            let mut builder = ReportBuilder::new("svc", ProfileType::cpu(), end - 1_000_000, end);
            for s in 0..(i + 1) {
                builder.add_stack(
                    vec![
                        Frame::new("/app", format!("fn{s}")),
                        Frame::new("/app", "main"),
                    ],
                    1_000,
                );
            }
            store.append(&builder.finish()).unwrap();
        }
        assert_eq!(store.stats().segment_count, 20);
        // The cap is applied by the store that opens the directory, so record
        // what a fresh process would see.
        let before = store.stats();
        std::fs::write(
            dir.path().join("cap.txt"),
            before.logical_bytes.to_string().as_bytes(),
        )
        .unwrap();
    }

    let total_bytes: u64 = std::fs::read_to_string(dir.path().join("cap.txt"))
        .unwrap()
        .parse()
        .unwrap();
    let cap = total_bytes / 2;

    // A restart with a per-tenant disk cap.
    let registry = TenantStore::open_with_limits(
        dir.path(),
        StoreConfig::new(dir.path()),
        [(
            TenantId::parse("acme").unwrap(),
            TenantLimits {
                retention_ns: None,
                max_disk_bytes: Some(cap),
                max_series: None,
            },
        )]
        .into_iter()
        .collect(),
        10,
    )
    .unwrap();
    let tenant = TenantId::parse("acme").unwrap();
    let store = registry.open_tenant(&tenant).unwrap();

    let pruned = registry.prune(now).unwrap();
    let removed: usize = pruned.values().map(|stats| stats.removed_segments).sum();
    assert!(removed > 0, "the cap evicted nothing");
    let after = store.stats();
    assert!(
        after.logical_bytes <= cap,
        "retained data is {} bytes, cap is {cap}",
        after.logical_bytes
    );

    // Eviction removes the oldest segments, so the newest one survives.
    let rows = store.series(0, u64::MAX, &Selector::default());
    let earliest = rows.iter().map(|r| r.start_ns).min().unwrap();
    assert!(
        earliest >= now - 20 * 60_000_000_000,
        "the earliest surviving segment is older than the newest one written"
    );
    assert_eq!(
        rows.iter().map(|r| r.segment_count).sum::<usize>(),
        after.segment_count,
        "the index and the series view disagree after eviction"
    );
}

// ---------------------------------------------------------------------------
// integrity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_healthy_store_passes_fsck_and_garbage_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let registry = TenantStore::open(dir.path(), StoreConfig::new(dir.path())).unwrap();
    let tenant = TenantId::parse("acme").unwrap();
    let store = registry.open_tenant(&tenant).unwrap();

    for i in 0..3u64 {
        let now = now_ns();
        let mut builder = ReportBuilder::new("svc", ProfileType::cpu(), now - 1_000, now);
        builder.add_stack(vec![Frame::new("/app", "work")], 100 + i);
        store.append(&builder.finish()).unwrap();
    }

    // A store that was written normally must verify clean.
    let results = registry.fsck();
    assert_eq!(results.len(), 1);
    assert!(
        results[0].2.is_ok(),
        "healthy store failed fsck: {:?}",
        results[0].2
    );

    // Corrupting the file is DuckDB's to detect; what matters for us is that
    // opening garbage is an error, never an empty database that silently
    // reports "no data".
    drop(registry);
    let db = dir.path().join("tenants").join("acme").join(DB_FILE);
    std::fs::write(&db, b"definitely not a duckdb file").unwrap();
    let reopened = TenantStore::open(dir.path(), StoreConfig::new(dir.path())).unwrap();
    assert!(
        reopened.tenant(&tenant).is_none(),
        "a corrupt database must not open as an empty tenant"
    );
    let results = reopened.fsck();
    assert!(
        results.is_empty() || results.iter().all(|(_, _, r)| r.is_err()),
        "fsck must report the corruption"
    );
}

#[tokio::test]
async fn unrelated_files_in_the_data_dir_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    // A data directory accumulates files that are not ours: a deployment that
    // once ran the old binary, a backup, a stray note.
    std::fs::write(dir.path().join("notes.txt"), b"hello").unwrap();
    std::fs::create_dir_all(dir.path().join("scratch")).unwrap();
    std::fs::write(dir.path().join("scratch/half-written.leseg.tmp"), b"junk").unwrap();

    let registry = TenantStore::open(dir.path(), StoreConfig::new(dir.path())).unwrap();
    let tenant = TenantId::parse("acme").unwrap();
    let store = registry.open_tenant(&tenant).unwrap();

    let now = now_ns();
    let mut builder = ReportBuilder::new("svc", ProfileType::cpu(), now - 1_000, now);
    builder.add_stack(vec![Frame::new("/app", "work")], 777);
    store.append(&builder.finish()).unwrap();

    let fg = flamegraph(
        &store,
        &QueryOptions::new("cpu", 0, u64::MAX, Selector::default()),
    )
    .unwrap();
    assert_eq!(fg.total, 777);

    let reopened = TenantStore::open(dir.path(), StoreConfig::new(dir.path())).unwrap();
    let store = reopened.open_tenant(&tenant).unwrap();
    assert_eq!(store.stats().segment_count, 1);
    assert!(store.fsck().iter().all(|(_, r)| r.is_ok()));
}

#[tokio::test]
async fn data_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();

    {
        let app = router(dir.path()).await;
        for tenant in ["acme", "globex"] {
            let (status, body) = call_as(
                &app,
                "POST",
                "/ingest",
                Some(report("api", "cpu", 500)),
                tenant,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        for pod in ["p1", "p2"] {
            call_as(
                &app,
                "POST",
                "/ingest",
                Some(report_with_pod("api", pod, 250)),
                "acme",
            )
            .await;
        }
    }

    // A fresh process, here a fresh router over the same directory.
    let app = router(dir.path()).await;
    for tenant in ["acme", "globex"] {
        let (_, fg) = call_as(
            &app,
            "GET",
            "/api/v1/flamegraph?from=now-1h&profile_type=cpu",
            None,
            tenant,
        )
        .await;
        let expected = if tenant == "acme" { 1_000 } else { 500 };
        assert_eq!(fg["total"], expected, "{tenant} after restart: {fg}");
    }

    let (_, tenants) = call(&app, "GET", "/api/v1/tenants", None, &[]).await;
    assert_eq!(tenants["tenants"].as_array().unwrap().len(), 2, "{tenants}");
}

// ---------------------------------------------------------------------------
// long run
// ---------------------------------------------------------------------------

/// Repeated write/query cycles must not drift the index or double-count.
#[tokio::test]
async fn a_long_run_stays_stable() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(dir.path()).await;

    let mut expected = 0u64;
    for cycle in 0..30u64 {
        let reports: Vec<serde_json::Value> = (0..5)
            .map(|i| report_with_pod("stable", &format!("pod-{i}"), 1_000))
            .collect();
        let (status, body) = call(
            &app,
            "POST",
            "/ingest",
            Some(json!({ "reports": reports })),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "cycle {cycle}: {body}");
        expected += 5_000;

        for uri in [
            "/api/v1/flamegraph?from=now-1h&max_nodes=128",
            "/api/v1/top?from=now-1h",
            "/api/v1/series?from=now-1h",
            "/api/v1/stats",
            "/metrics",
        ] {
            let response = get_raw(&app, uri).await;
            assert_eq!(response.status, StatusCode::OK, "cycle {cycle}: {uri}");
        }

        let (_, stats) = call(&app, "GET", "/api/v1/stats", None, &[]).await;
        assert_eq!(
            stats["store"]["total_value"], expected,
            "cycle {cycle} lost or double-counted values: {stats}"
        );
    }

    let (_, series) = call(&app, "GET", "/api/v1/series?from=now-1h", None, &[]).await;
    assert_eq!(
        series["series"].as_array().unwrap().len(),
        5,
        "series count drifted: {series}"
    );
}
