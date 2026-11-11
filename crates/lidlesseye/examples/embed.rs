//! Embed LidlessEye in your own binary.
//!
//! This is the whole point of splitting the workspace into crates: adding
//! `lidlesseye = "0.1"` to a Cargo.toml gives you a profiling backend that
//! starts inside *your* process — no sidecar, no agent to deploy, no external
//! database.
//!
//! ```text
//! cargo run --example embed                  # HTTP API and dashboard only
//! sudo -E cargo run --example embed -- agent # also profile this machine
//! ```
//!
//! Open <http://127.0.0.1:4040> afterwards.

use std::collections::BTreeMap;
use std::time::Duration;

use lidlesseye::{LidlessEye, Result};

#[tokio::main]
async fn main() -> Result<()> {
    // `RUST_LOG=debug` for more, `RUST_LOG=warn` for less.
    lidlesseye::init_tracing("lidlesseye=info,lidlesseye_ebpf=info");

    let with_agent = std::env::args().any(|arg| arg == "agent");

    let mut labels = BTreeMap::new();
    labels.insert("env".to_string(), "dev".to_string());
    labels.insert("host".to_string(), "localhost".to_string());

    let eye = LidlessEye::builder()
        .data_dir("./lidlesseye-data")
        .listen("127.0.0.1:4040")
        // Profile every process on the machine. Requires Linux + root
        // (CAP_BPF, CAP_PERFMON, CAP_SYS_ADMIN); without it the HTTP API,
        // storage and query engine still work and accept profiles over
        // `POST /ingest`.
        .agent(with_agent)
        // Aggregate everything under one service name instead of one service
        // per process; the process name stays available as a label.
        .service_name("my-service")
        .labels(labels)
        .frequency_hz(99)
        .flush_interval(Duration::from_secs(5))
        .collect_cpu(true)
        .collect_off_cpu(true)
        // Retention is enforced by a background task.
        .retention(Some(Duration::from_secs(7 * 24 * 3600)))
        .max_disk_bytes(Some(20 * 1024 * 1024 * 1024))
        .build()
        .await?;

    println!(
        "LidlessEye {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        eye.base_url()
    );
    println!("  dashboard: {}/", eye.base_url());
    println!("  data dir : {}", eye.data_dir().display());
    if with_agent {
        println!("  eBPF     : collecting at 99 Hz");
    } else {
        println!("  eBPF     : disabled (pass `agent` to enable)");
    }

    eye.run().await
}
