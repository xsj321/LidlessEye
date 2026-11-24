//! Prometheus-style metrics without a metrics framework.
//!
//! The server exposes a handful of counters that matter for operating a
//! profiling backend (ingest rate, query latency, batch count). Hand-rolling
//! them keeps the dependency tree small and the exposition format explicit.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Metrics {
    pub ingest_requests: AtomicU64,
    pub ingest_reports: AtomicU64,
    pub ingest_samples: AtomicU64,
    pub ingest_bytes: AtomicU64,
    pub ingest_errors: AtomicU64,
    pub queries: AtomicU64,
    pub query_nanos: AtomicU64,
    pub query_errors: AtomicU64,
    pub batches_written: AtomicU64,
    pub batches_pruned: AtomicU64,
    pub parquet_files_pruned: AtomicU64,
}

impl Metrics {
    pub fn render(&self, extra: &[(&str, String)], uptime_secs: u64) -> String {
        let mut out = String::with_capacity(1024);
        let counter =
            |name: &'static str, help: &'static str, value: u64| (name, help, value.to_string());
        let counters = [
            counter(
                "lidlesseye_ingest_requests_total",
                "Profile ingestion requests.",
                self.ingest_requests.load(Ordering::Relaxed),
            ),
            counter(
                "lidlesseye_ingest_reports_total",
                "Profile reports accepted.",
                self.ingest_reports.load(Ordering::Relaxed),
            ),
            counter(
                "lidlesseye_ingest_samples_total",
                "Aggregated stacks accepted.",
                self.ingest_samples.load(Ordering::Relaxed),
            ),
            counter(
                "lidlesseye_ingest_bytes_total",
                "Bytes of report JSON accepted.",
                self.ingest_bytes.load(Ordering::Relaxed),
            ),
            counter(
                "lidlesseye_ingest_errors_total",
                "Rejected ingestion requests.",
                self.ingest_errors.load(Ordering::Relaxed),
            ),
            counter(
                "lidlesseye_queries_total",
                "Profile queries served.",
                self.queries.load(Ordering::Relaxed),
            ),
            (
                "lidlesseye_query_duration_seconds_total",
                "Cumulative query time, in seconds.",
                format!(
                    "{:.6}",
                    self.query_nanos.load(Ordering::Relaxed) as f64 / 1e9
                ),
            ),
            counter(
                "lidlesseye_query_errors_total",
                "Failed profile queries.",
                self.query_errors.load(Ordering::Relaxed),
            ),
            counter(
                "lidlesseye_batches_written_total",
                "Ingest batches persisted (metric name kept for compatibility).",
                self.batches_written.load(Ordering::Relaxed),
            ),
            counter(
                "lidlesseye_batches_pruned_total",
                "Ingest batches dropped by retention or eviction.",
                self.batches_pruned.load(Ordering::Relaxed),
            ),
            counter(
                "lidlesseye_parquet_files_pruned_total",
                "Committed Parquet files removed by retention or the disk cap.",
                self.parquet_files_pruned.load(Ordering::Relaxed),
            ),
        ];
        for (name, help, value) in counters {
            out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
            ));
        }
        out.push_str(&format!(
            "# HELP lidlesseye_uptime_seconds Process uptime.\n# TYPE lidlesseye_uptime_seconds gauge\nlidlesseye_uptime_seconds {uptime_secs}\n"
        ));
        for (name, value) in extra {
            out.push_str(&format!("# TYPE {name} gauge\n{name} {value}\n"));
        }
        out
    }
}
