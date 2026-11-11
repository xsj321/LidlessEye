//! eBPF profiling for LidlessEye, built on [aya](https://aya-rs.dev).
//!
//! ```no_run
//! use lidlesseye_ebpf::{start, CollectorConfig};
//!
//! let handle = start(CollectorConfig::default(), |report| {
//!     println!(
//!         "{}: {} stacks of {}",
//!         report.service_name,
//!         report.stacks.len(),
//!         report.profile_type
//!     );
//! })?;
//! std::thread::sleep(std::time::Duration::from_secs(30));
//! handle.stop();
//! # Ok::<(), String>(())
//! ```
//!
//! The collector requires root (or `CAP_BPF` + `CAP_PERFMON` +
//! `CAP_SYS_ADMIN`) and a Linux kernel >= 4.17. On other platforms the crate
//! still compiles, but [`start`] returns an error.

#[cfg(target_os = "linux")]
mod collector;
#[cfg(target_os = "linux")]
pub mod procfs;
#[cfg(target_os = "linux")]
mod symbolizer;

#[cfg(target_os = "linux")]
pub use collector::{CollectorConfig, CollectorHandle, StatsSnapshot, start};

/// The compiled eBPF object, exposed for debugging and for `bpftool` based
/// inspection (`lidlesseye` embeds the same bytes).
#[cfg(target_os = "linux")]
pub fn bpf_object() -> &'static [u8] {
    collector::bpf_object()
}

/// Configuration for the eBPF collector.
#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone)]
pub struct CollectorConfig {
    pub service_name: Option<String>,
    pub labels: std::collections::BTreeMap<String, String>,
    pub frequency_hz: u64,
    pub flush_interval: std::time::Duration,
    pub collect_cpu: bool,
    pub collect_off_cpu: bool,
    pub exclude_pids: Vec<u32>,
    pub enrich_kubernetes_labels: bool,
    pub node_label: bool,
}

#[cfg(not(target_os = "linux"))]
impl Default for CollectorConfig {
    fn default() -> Self {
        CollectorConfig {
            service_name: None,
            labels: std::collections::BTreeMap::new(),
            frequency_hz: 99,
            flush_interval: std::time::Duration::from_secs(10),
            collect_cpu: true,
            collect_off_cpu: true,
            exclude_pids: Vec::new(),
            enrich_kubernetes_labels: true,
            node_label: true,
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub struct CollectorHandle;

#[cfg(not(target_os = "linux"))]
impl CollectorHandle {
    pub fn stop(self) {}

    pub fn stats(&self) -> StatsSnapshot {
        StatsSnapshot::default()
    }
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct StatsSnapshot {
    pub cpu_records: u64,
    pub offcpu_records: u64,
    pub reports: u64,
    pub symbol_failures: u64,
    pub drains: u64,
    pub errors: u64,
}

/// Start the eBPF collector. Only supported on Linux.
#[cfg(not(target_os = "linux"))]
pub fn start<F>(_config: CollectorConfig, _on_report: F) -> Result<CollectorHandle, String>
where
    F: Fn(lidlesseye_core::Report) + Send + 'static,
{
    Err("the eBPF collector is only available on Linux".to_string())
}
