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
mod gopclntab;
#[cfg(target_os = "linux")]
mod perf_map;
#[cfg(target_os = "linux")]
pub mod procfs;
#[cfg(target_os = "linux")]
mod symbolizer;

#[cfg(target_os = "linux")]
pub use collector::{CollectorConfig, CollectorHandle, StatsSnapshot, start};

/// Why addresses could not be symbolized, split by cause.
///
/// The split is what makes a coverage report actionable: `missing_syms` means
/// "install debug info or stop stripping the binary", `missing_component`
/// means "this is a JIT or anonymous mapping with no file behind it", and
/// `unmapped` means the address is not in the process at all (a corrupt stack,
/// or a pid that was recycled between sampling and flushing).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UnknownReasons {
    /// The address is not inside any mapping of the process.
    pub unmapped: u64,
    /// The mapping exists but carries no symbols (stripped binary).
    pub missing_syms: u64,
    /// The mapping has no file behind it: anonymous memory, `[heap]`, or JIT
    /// code that did not publish a perf map.
    pub missing_component: u64,
    /// The file has symbols, but none covers this address.
    pub unknown_addr: u64,
    /// The mapping is of a kind the symbolizer cannot read (e.g. a device).
    pub unsupported: u64,
    /// Any other reason.
    pub other: u64,
}

impl UnknownReasons {
    pub fn total(&self) -> u64 {
        self.unmapped
            + self.missing_syms
            + self.missing_component
            + self.unknown_addr
            + self.unsupported
            + self.other
    }

    /// `(name, count)` pairs, in a stable order suitable for logs and metrics.
    pub fn entries(&self) -> [(&'static str, u64); 6] {
        [
            ("unmapped", self.unmapped),
            ("missing_syms", self.missing_syms),
            ("missing_component", self.missing_component),
            ("unknown_addr", self.unknown_addr),
            ("unsupported", self.unsupported),
            ("other", self.other),
        ]
    }
}

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
    /// Addresses that produced no symbol name, of every reason.
    pub symbol_failures: u64,
    /// Frames that carry a real symbol name.
    pub symbol_frames: u64,
    /// Frames recovered from DWARF inline information alone.
    pub symbol_inlined: u64,
    /// Frames named through a Go `.gopclntab`.
    pub symbol_go: u64,
    /// Frames named through a JIT runtime's perf map.
    pub symbol_jit: u64,
    /// Unresolved addresses, split by cause. Always zero off Linux.
    pub symbol_unknown: UnknownReasons,
    /// Samples discarded because the process was gone. Always zero off Linux.
    pub symbol_skipped: u64,
    /// Samples whose pid is not visible in the agent's PID namespace.
    pub unattributed: u64,
    /// Samples that carried no usable stack.
    pub empty_stacks: u64,
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
