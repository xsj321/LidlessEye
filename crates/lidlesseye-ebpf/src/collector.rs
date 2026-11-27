//! The eBPF collector: loads the programs, samples the machine and turns the
//! kernel-side aggregates into [`Report`]s once per flush interval.
//!
//! ## Sampling model
//!
//! * **on-CPU** — one software perf event per CPU (`PERF_COUNT_SW_CPU_CLOCK`)
//!   fires `frequency_hz` times per second. Each hit captures the user and
//!   kernel stack of whatever is running and bumps a counter keyed by
//!   `(tgid, user_stack, kernel_stack)`. A count is worth `1s / frequency_hz`
//!   of CPU time, which is how the counter is turned into nanoseconds.
//! * **off-CPU** — `kprobe:schedule` records "this task is about to block"
//!   together with the blocking stack; `kprobe:finish_task_switch` finds the
//!   state of the task that just resumed and adds `now - started` nanoseconds
//!   to that stack. Tasks blocked for less than a millisecond are ignored.
//!
//! Both aggregations happen entirely inside the kernel; user space only reads
//! the maps once per flush interval, so the per-sample cost stays constant no
//! matter how fast samples arrive.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use aya::Ebpf;
use aya::maps::{HashMap as AyaHashMap, MapData, StackTraceMap};
use aya::programs::perf_event::{PerfEventConfig, PerfEventScope, SamplePolicy, SoftwareEvent};
use aya::programs::{KProbe, PerfEvent};
use lidlesseye_core::model::{Frame, ProfileType, Report, ReportBuilder};
use lidlesseye_core::now_ns;
use tracing::{debug, info, warn};

use crate::UnknownReasons;
use crate::allocator::Allocator;
use crate::bpftime::Bpftime;
use crate::goheap::GoHeap;
use crate::jvmheap::JvmHeap;
use crate::procfs;
use crate::symbolizer::FrameResolver;

// From the source tree, not `OUT_DIR`: see the note in `conntrack.rs`. A build
// script that rewrites its output without changing a `.rs` file does not force
// cargo to recompile the crate, so an `OUT_DIR` path can embed stale bytes.
const BPF_OBJECT: &[u8] = include_bytes!("../bpf/profile.bpf.o");

pub(crate) fn bpf_object() -> &'static [u8] {
    BPF_OBJECT
}
const INVALID_STACK_ID: u32 = u32::MAX;

/// Smallest heap that gets a series of its own: 256 KiB, or 256 objects.
///
/// Below that a profile describes an implementation detail rather than memory
/// use, and a node running three hundred processes would otherwise publish a
/// series per process per metric per flush for heaps nobody will look at.
const ALLOC_REPORT_BYTES_FLOOR: u64 = 256 * 1024;
const ALLOC_REPORT_OBJECTS_FLOOR: u64 = 256;

/// Collector configuration.
#[derive(Debug, Clone)]
pub struct CollectorConfig {
    /// Service name attached to every report. When `None`, the process name
    /// (`/proc/<pid>/comm`) is used, which gives one series per process.
    pub service_name: Option<String>,
    /// Extra labels attached to every report.
    pub labels: BTreeMap<String, String>,
    /// Per-CPU sampling frequency.
    pub frequency_hz: u64,
    /// How often the kernel maps are drained into reports.
    pub flush_interval: Duration,
    /// Collect on-CPU profiles.
    pub collect_cpu: bool,
    /// Collect off-CPU (blocked) profiles.
    pub collect_off_cpu: bool,
    /// Collect Go heap profiles by reading the runtime's own bucket chain.
    ///
    /// This is not a sampler of ours: the runtime already samples allocations
    /// (`MemProfileRate`, 512 KiB by default) and keeps the result in
    /// `runtime.mbuckets`. Reading that chain costs one pass per interval and no
    /// instrumentation of the target at all, but it needs `/proc/<pid>/mem`,
    /// i.e. `CAP_SYS_PTRACE`. Processes that cannot be read are counted, not
    /// guessed at.
    pub collect_heap: bool,
    /// Collect JVM heap profiles by reading the recorder the JVM already runs.
    ///
    /// A third kind of reader, and the same bargain as the Go one: a recording is
    /// started on the running JVM through the attach mechanism, dumped to a
    /// temporary file, read back with `jfr`, and stopped again. Nothing is
    /// injected and no application code changes, but it needs `jcmd` and `jfr`
    /// (a JDK, not a JRE) and permission to attach. Reports carry
    /// `profile_source=jvm-jfr` and `runtime=java`.
    pub collect_jvm: bool,
    /// Workloads (`namespace/workload`) the server has switched JVM profiling
    /// on for.
    ///
    /// Reading a JVM is not free: every interval starts a recording, dumps it,
    /// and reads the dump back, which costs the target a pause and the agent a
    /// few megabytes. That is why it is not on for every Java process on the
    /// node. The server keeps the list, the API edits it, and this node polls
    /// it; a workload that is not in here is not attached to at all.
    pub jvm_workloads: Arc<parking_lot::RwLock<HashMap<String, bool>>>,
    /// Workloads (`namespace/workload`) the server has switched *native* memory
    /// profiling on for, by injecting the bpftime agent into their processes.
    ///
    /// Separate from [`Self::jvm_workloads`] because the cost and the risk are
    /// different: reading a JVM asks it for a dump, while this one replaces the
    /// allocator's entry point inside a running process. Nothing is injected
    /// until a workload appears here.
    pub alloc_workloads: Arc<parking_lot::RwLock<HashMap<String, bool>>>,
    /// Collect native memory profiles through bpftime instead of kernel uprobes.
    ///
    /// The kernel uprobe variant ([`Self::collect_alloc`]) measured 79 µs of
    /// system time per 64-byte allocation on this cluster; the userspace runtime
    /// brings that down to hundreds of nanoseconds by running the hook inside
    /// the process. It costs a shared library injected into the target, so it is
    /// gated on a per-workload switch like the JVM reader.
    pub collect_bpftime: bool,
    /// Also store object counts (`alloc_objects`, `inuse_objects`), not only
    /// bytes. Two extra reports per process per interval is real storage, and
    /// the bytes are what a heap question is usually about.
    ///
    /// Applies to both sources: the Go reader and the allocator probes.
    pub heap_objects: bool,
    /// Collect memory profiles for native processes (C, C++, Rust, ...) by
    /// hooking the allocator's `malloc`/`free` family with uprobes.
    ///
    /// Separate from [`Self::collect_heap`] because it is a different
    /// measurement: the Go reader reports what the runtime itself recorded,
    /// while these probes only see allocations made after they were attached.
    /// Their reports carry `profile_source=allocator-uprobe` so the two are
    /// never added together.
    pub collect_alloc: bool,
    /// Process ids to ignore.
    pub exclude_pids: Vec<u32>,
    /// Ignore the collector's own process so that the profiler never profiles
    /// itself. Turn this off when the workload lives in the same process as the
    /// collector (tests, single-process embedded use).
    pub exclude_self: bool,
    /// Derive `k8s_*` labels from the cgroup path of each process.
    pub enrich_kubernetes_labels: bool,
    /// Pod metadata, when something is able to fill it.
    ///
    /// The cgroup path only carries the pod *UID*; naming the series after the
    /// pod's workload needs the API. Whoever can reach it hands the collector a
    /// snapshot through [`crate::k8s::PodIndex`]; leaving this `None` (the
    /// default) keeps the process-name behaviour and is what happens outside a
    /// cluster.
    pub pods: Option<Arc<crate::k8s::PodIndex>>,
    /// Add a `node` label with the host name.
    pub node_label: bool,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        CollectorConfig {
            service_name: None,
            labels: BTreeMap::new(),
            frequency_hz: 99,
            flush_interval: Duration::from_secs(10),
            collect_cpu: true,
            collect_off_cpu: true,
            collect_heap: true,
            collect_jvm: true,
            // Empty means nothing is switched on, which is the safe default: a
            // JVM is not read until someone asks for it on the service map.
            jvm_workloads: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            // Same rule for native memory: no workload, no injection.
            alloc_workloads: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            collect_bpftime: true,
            heap_objects: false,
            collect_alloc: true,
            exclude_pids: Vec::new(),
            exclude_self: true,
            enrich_kubernetes_labels: true,
            pods: None,
            node_label: true,
        }
    }
}

impl CollectorConfig {
    /// Validate the configuration, returning a human readable error.
    pub fn validate(&self) -> Result<(), String> {
        if self.frequency_hz == 0 || self.frequency_hz > 10_000 {
            return Err(format!(
                "frequency_hz must be between 1 and 10000 (got {})",
                self.frequency_hz
            ));
        }
        if self.flush_interval < Duration::from_millis(100) {
            return Err("flush_interval must be at least 100ms".to_string());
        }
        if self.flush_interval > Duration::from_secs(3600) {
            return Err("flush_interval must be at most 1h".to_string());
        }
        if !self.collect_cpu
            && !self.collect_off_cpu
            && !self.collect_heap
            && !self.collect_jvm
            && !self.collect_alloc
        {
            return Err(
                "at least one of collect_cpu / collect_off_cpu / collect_heap / collect_jvm / \
                 collect_alloc must be enabled"
                    .to_string(),
            );
        }
        if let Some(name) = &self.service_name
            && (name.trim().is_empty() || name.len() > 256)
        {
            return Err("service_name must be 1-256 characters".to_string());
        }
        Ok(())
    }
}

/// Live counters, safe to read from another thread.
#[derive(Debug, Default)]
pub struct CollectorStats {
    pub cpu_records: AtomicU64,
    pub offcpu_records: AtomicU64,
    pub reports: AtomicU64,
    pub symbol_failures: AtomicU64,
    pub symbol_frames: AtomicU64,
    pub symbol_inlined: AtomicU64,
    pub symbol_go: AtomicU64,
    pub symbol_jit: AtomicU64,
    /// Frames named through the module cache's own ELF symbol table.
    pub symbol_module: AtomicU64,
    /// Mappings no route could open, cumulative.
    pub symbol_module_failed: AtomicU64,
    pub symbol_unknown: Mutex<UnknownReasons>,
    /// Records dropped because the process no longer exists, cumulatively.
    pub skipped: AtomicU64,
    /// Records whose pid is not visible in the agent's PID namespace. Always
    /// zero when the agent shares the host's PID namespace; non-zero means the
    /// agent is confined to its own and the samples of other containers are
    /// lost.
    pub unattributed: AtomicU64,
    /// Records that were sampled but carried no usable stack at all.
    pub empty_stacks: AtomicU64,
    pub drains: AtomicU64,
    pub errors: AtomicU64,
    /// Heap samples read out of Go processes, cumulative.
    pub heap_samples: AtomicU64,
    /// Reports emitted for those samples, cumulative.
    pub heap_reports: AtomicU64,
    /// Go processes that produced a profile on the last scan.
    pub heap_processes: AtomicU64,
    /// Processes whose memory could not be read, cumulative (missing
    /// `CAP_SYS_PTRACE` shows up here).
    pub heap_unreadable: AtomicU64,
    /// Processes that turned allocation sampling off (`MemProfileRate = 0`).
    pub heap_disabled: AtomicU64,
    /// Buckets that failed validation, cumulative.
    pub heap_invalid: AtomicU64,
    /// Allocation stacks read out of JVMs through Java Flight Recorder,
    /// cumulative.
    pub jvm_samples: AtomicU64,
    /// JVMs that produced a profile on the last scan.
    pub jvm_processes: AtomicU64,
    /// JVMs that could not be attached to, cumulative. A JRE-only image, a
    /// disabled attach mechanism and a recording another tool already owns all
    /// land here.
    pub jvm_attach_failed: AtomicU64,
    /// Allocator samples drained, cumulative.
    pub alloc_samples: AtomicU64,
    /// Reports emitted for them, cumulative.
    pub alloc_reports: AtomicU64,
    /// Uprobes attached across every allocator file, as of the last discovery.
    pub alloc_probes: AtomicU64,
    /// Bytes whose stack could not be captured or was evicted, cumulative.
    pub alloc_lost: AtomicU64,
    /// Allocation stacks drained from the userspace runtime, cumulative.
    pub bpftime_samples: AtomicU64,
    /// Processes the bpftime agent could not be injected into, cumulative.
    pub bpftime_attach_failed: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StatsSnapshot {
    pub cpu_records: u64,
    pub offcpu_records: u64,
    pub reports: u64,
    /// Addresses that produced no symbol name, of every reason. This used to
    /// count only hard failures, which read as "0" on a machine where half the
    /// frames were `[unknown]`; it now counts what a user would call a failure.
    pub symbol_failures: u64,
    /// Frames that carry a real symbol name.
    pub symbol_frames: u64,
    /// Frames recovered from DWARF inline information alone. A leaf that only
    /// exists as an inlined copy shows up here.
    pub symbol_inlined: u64,
    /// Frames named through a Go `.gopclntab`, i.e. stripped Go binaries that
    /// no ELF symbolizer could have resolved.
    pub symbol_go: u64,
    /// Frames named through a JIT runtime's perf map.
    pub symbol_jit: u64,
    /// Frames named from an ELF symbol table the primary symbolizer could not
    /// reach, because the file is only visible through `/proc/<pid>/root`.
    pub symbol_module: u64,
    /// Mappings whose binary could not be opened by any route. Non-zero means
    /// the agent cannot see the files it is profiling, which is the usual
    /// reason a node full of containers reports `[unknown]`.
    pub symbol_module_failures: u64,
    /// Unresolved addresses, split by cause.
    pub symbol_unknown: UnknownReasons,
    /// Samples discarded because the process was gone before it could be
    /// symbolized. Not a symbol failure — a data loss that cannot be undone.
    pub symbol_skipped: u64,
    /// Samples whose pid is not visible in the agent's PID namespace.
    pub unattributed: u64,
    /// Samples that carried no usable stack.
    pub empty_stacks: u64,
    pub drains: u64,
    pub errors: u64,
    pub heap_samples: u64,
    pub heap_reports: u64,
    pub heap_processes: u64,
    pub heap_unreadable: u64,
    pub heap_disabled: u64,
    pub heap_invalid: u64,
    pub jvm_samples: u64,
    pub jvm_processes: u64,
    pub jvm_attach_failed: u64,
    pub alloc_samples: u64,
    pub alloc_reports: u64,
    pub alloc_probes: u64,
    pub alloc_lost: u64,
    pub bpftime_samples: u64,
    pub bpftime_attach_failed: u64,
}

/// Handle to a running collector. Dropping it stops the collector.
pub struct CollectorHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<CollectorStats>,
}

impl CollectorHandle {
    /// Ask the collector to stop and wait for it to detach its eBPF programs.
    pub fn stop(mut self) {
        self.shutdown();
    }

    pub fn stats(&self) -> StatsSnapshot {
        StatsSnapshot {
            cpu_records: self.stats.cpu_records.load(Ordering::Relaxed),
            offcpu_records: self.stats.offcpu_records.load(Ordering::Relaxed),
            reports: self.stats.reports.load(Ordering::Relaxed),
            symbol_failures: self.stats.symbol_failures.load(Ordering::Relaxed),
            symbol_frames: self.stats.symbol_frames.load(Ordering::Relaxed),
            symbol_inlined: self.stats.symbol_inlined.load(Ordering::Relaxed),
            symbol_go: self.stats.symbol_go.load(Ordering::Relaxed),
            symbol_jit: self.stats.symbol_jit.load(Ordering::Relaxed),
            symbol_module: self.stats.symbol_module.load(Ordering::Relaxed),
            symbol_module_failures: self.stats.symbol_module_failed.load(Ordering::Relaxed),
            symbol_unknown: *self
                .stats
                .symbol_unknown
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            symbol_skipped: self.stats.skipped.load(Ordering::Relaxed),
            unattributed: self.stats.unattributed.load(Ordering::Relaxed),
            empty_stacks: self.stats.empty_stacks.load(Ordering::Relaxed),
            drains: self.stats.drains.load(Ordering::Relaxed),
            errors: self.stats.errors.load(Ordering::Relaxed),
            heap_samples: self.stats.heap_samples.load(Ordering::Relaxed),
            heap_reports: self.stats.heap_reports.load(Ordering::Relaxed),
            heap_processes: self.stats.heap_processes.load(Ordering::Relaxed),
            heap_unreadable: self.stats.heap_unreadable.load(Ordering::Relaxed),
            heap_disabled: self.stats.heap_disabled.load(Ordering::Relaxed),
            heap_invalid: self.stats.heap_invalid.load(Ordering::Relaxed),
            jvm_samples: self.stats.jvm_samples.load(Ordering::Relaxed),
            jvm_processes: self.stats.jvm_processes.load(Ordering::Relaxed),
            jvm_attach_failed: self.stats.jvm_attach_failed.load(Ordering::Relaxed),
            alloc_samples: self.stats.alloc_samples.load(Ordering::Relaxed),
            alloc_reports: self.stats.alloc_reports.load(Ordering::Relaxed),
            alloc_probes: self.stats.alloc_probes.load(Ordering::Relaxed),
            alloc_lost: self.stats.alloc_lost.load(Ordering::Relaxed),
            bpftime_samples: self.stats.bpftime_samples.load(Ordering::Relaxed),
            bpftime_attach_failed: self.stats.bpftime_attach_failed.load(Ordering::Relaxed),
        }
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for CollectorHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Start collecting. `on_report` is invoked from the collector thread once per
/// flush interval, for every `(service, labels, profile type)` combination that
/// saw samples.
///
/// Returns an error when the process is not privileged enough to load eBPF
/// programs, or when the kernel rejects the programs.
pub fn start<F>(config: CollectorConfig, on_report: F) -> Result<CollectorHandle, String>
where
    F: Fn(Report) + Send + 'static,
{
    config.validate()?;
    if !procfs::is_privileged() {
        return Err(
            "the eBPF collector must run as root (or with CAP_BPF + CAP_PERFMON + CAP_SYS_ADMIN)"
                .to_string(),
        );
    }
    if let Some(paranoid) = procfs::perf_event_paranoid()
        && paranoid > 2
    {
        warn!(
            perf_event_paranoid = paranoid,
            "perf_event_paranoid is very restrictive; sampling may fail"
        );
    }

    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(CollectorStats::default());
    let thread_stop = stop.clone();
    let thread_stats = stats.clone();

    let handle = thread::Builder::new()
        .name("lidlesseye-ebpf".to_string())
        .spawn(move || {
            if let Err(err) = run(config, on_report, thread_stop, thread_stats.clone()) {
                thread_stats.errors.fetch_add(1, Ordering::Relaxed);
                warn!(error = %err, "collector stopped with an error");
            }
        })
        .map_err(|err| format!("failed to spawn collector thread: {err}"))?;

    Ok(CollectorHandle {
        stop,
        thread: Some(handle),
        stats,
    })
}

/// A key as laid out in the eBPF maps. Keep in sync with `bpf/profile.bpf.c`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
struct StackKey {
    tgid: u32,
    user_stack: u32,
    kernel_stack: u32,
    pad: u32,
}

// SAFETY: `StackKey` is `#[repr(C)]`, has no padding (four `u32` fields) and
// contains no pointers, so it is plain old data.
unsafe impl aya::Pod for StackKey {}

#[derive(Debug, Clone)]
struct RawRecord {
    tgid: u32,
    user_stack: u32,
    kernel_stack: u32,
    value: u64,
}

fn run<F>(
    config: CollectorConfig,
    on_report: F,
    stop: Arc<AtomicBool>,
    stats: Arc<CollectorStats>,
) -> Result<(), String>
where
    F: Fn(Report),
{
    if !procfs::raise_memlock_limit() {
        debug!("could not raise RLIMIT_MEMLOCK, relying on the kernel's default");
    }

    // Tell the programs which PID namespace `/proc` shows, so that the pid
    // they report is one this process can read. A pod with `hostPID: true`
    // shares the node's namespace, which is an ancestor of every other pod's:
    // that is what makes one agent able to profile the whole node.
    let ns_inum = procfs::pid_namespace_inum().unwrap_or(0);
    if ns_inum == 0 {
        warn!(
            "could not read /proc/self/ns/pid; reporting pids from the initial PID namespace, \
             which /proc may not list"
        );
    } else {
        info!(
            pid_namespace = ns_inum,
            "identifying samples by the pid in this PID namespace"
        );
    }
    let mut loader = aya::EbpfLoader::new();
    // `must_exist = false` so that an object built without `agent_ns_inum`
    // still loads (the programs then report initial-namespace pids).
    loader.override_global("agent_ns_inum", &ns_inum, false);
    let mut ebpf = loader
        .load(BPF_OBJECT)
        .map_err(|err| format!("failed to load eBPF: {err}"))?;
    let cpus = procfs::online_cpus();
    info!(
        cpus = cpus.len(),
        frequency_hz = config.frequency_hz,
        "eBPF programs loaded"
    );

    let mut cpu_events = 0usize;
    if config.collect_cpu {
        let program: &mut PerfEvent = ebpf
            .program_mut("lidlesseye_cpu")
            .ok_or_else(|| "eBPF object is missing the `lidlesseye_cpu` program".to_string())?
            .try_into()
            .map_err(|err| format!("`lidlesseye_cpu` is not a perf event program: {err}"))?;
        program
            .load()
            .map_err(|err| format!("failed to load `lidlesseye_cpu`: {err}"))?;

        for &cpu in &cpus {
            match program.attach(
                PerfEventConfig::Software(SoftwareEvent::CpuClock),
                PerfEventScope::AllProcessesOneCpu { cpu },
                SamplePolicy::Frequency(config.frequency_hz),
                true,
            ) {
                Ok(_) => cpu_events += 1,
                Err(err) => warn!(cpu, error = %err, "failed to attach CPU sampler"),
            }
        }
        if cpu_events == 0 {
            return Err("could not attach the CPU sampler to any CPU".to_string());
        }
    }

    let mut offcpu_attached = false;
    if config.collect_off_cpu {
        let start = attach_kprobe(&mut ebpf, "lidlesseye_offcpu_start", &["schedule"]);
        let end = start.and_then(|_| {
            attach_kprobe(
                &mut ebpf,
                "lidlesseye_offcpu_end",
                &[
                    "finish_task_switch",
                    "finish_task_switch.isra.0",
                    "finish_task_switch.constprop.0",
                ],
            )
        });
        offcpu_attached = start.is_some() && end.is_some();
        if !offcpu_attached {
            warn!("could not attach off-CPU probes; continuing with on-CPU only");
        }
    }

    // The perf-event and kprobe programs are the sampled half; the two memory
    // readers do not need them, and running memory-only is a supported setup
    // (it is how a memory question is asked without the sampler's overhead).
    if cpu_events == 0
        && !offcpu_attached
        && !config.collect_heap
        && !config.collect_jvm
        && !config.collect_alloc
    {
        return Err("no eBPF program could be attached".to_string());
    }

    let mut resolver = FrameResolver::new();
    // Heap state lives across flushes: the deltas are differences of cumulative
    // bucket counters, so the previous scan is part of the next one's answer.
    let mut heap = GoHeap::new();
    // The JVM reader keeps the same kind of state: which processes already have
    // a recording, so a scan dumps and restarts instead of accumulating.
    let mut jvm = JvmHeap::new();
    // Allocator probes are process-wide and long-lived; the state they need
    // (which files are instrumented, what each pid is) outlives a flush.
    let mut alloc = Allocator::new();
    // The bpftime reader keeps the loader process and the injected pids.
    let mut bpftime = Bpftime::new();
    let mut last_flush = now_ns();
    let mut ticker = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        // Sleep in short slices so that `stop()` returns promptly.
        let sleep_for = config
            .flush_interval
            .saturating_sub(ticker.elapsed())
            .min(Duration::from_millis(200));
        if sleep_for > Duration::from_millis(1) {
            thread::sleep(sleep_for);
        }
        if ticker.elapsed() < config.flush_interval {
            continue;
        }
        ticker = Instant::now();

        let now = now_ns();
        match flush(
            &mut ebpf,
            &mut resolver,
            &mut heap,
            &mut jvm,
            &mut alloc,
            &mut bpftime,
            &config,
            last_flush,
            now,
            offcpu_attached,
            &stats,
            &on_report,
        ) {
            Ok(()) => {
                stats.drains.fetch_add(1, Ordering::Relaxed);
            }
            Err(err) => {
                stats.errors.fetch_add(1, Ordering::Relaxed);
                warn!(error = %err, "flush failed");
            }
        }
        last_flush = now;
    }

    info!("collector stopping, detaching eBPF programs");
    Ok(())
}

/// Load `program` once and attach it to the first symbol that exists on this
/// kernel. Returns the symbol that worked, or `None`.
///
/// Symbol names move around between kernel versions and compiler settings
/// (`finish_task_switch` becomes `finish_task_switch.isra.0` once GCC decides
/// to clone it), so every candidate has to be tried. Loading is separate from
/// attaching because aya rejects a second `load()` on a program that is
/// already loaded — the very thing that used to make the fallback list fail.
fn attach_kprobe(ebpf: &mut Ebpf, program: &str, symbols: &[&'static str]) -> Option<&'static str> {
    let probe: &mut KProbe = match ebpf.program_mut(program) {
        Some(raw) => match raw.try_into() {
            Ok(probe) => probe,
            Err(err) => {
                debug!(program, error = %err, "not a kprobe program");
                return None;
            }
        },
        None => {
            debug!(program, "program missing from the eBPF object");
            return None;
        }
    };
    if let Err(err) = probe.load() {
        debug!(program, error = %err, "could not load kprobe");
        return None;
    }

    let mut last_error = String::new();
    for symbol in symbols {
        match probe.attach(symbol, 0) {
            Ok(_) => {
                debug!(program, symbol, "attached kprobe");
                return Some(symbol);
            }
            Err(err) => last_error = format!("{symbol}: {err}"),
        }
    }
    debug!(program, error = %last_error, "no candidate symbol could be attached");
    None
}

#[allow(clippy::too_many_arguments)]
fn flush<F>(
    ebpf: &mut Ebpf,
    resolver: &mut FrameResolver,
    heap: &mut GoHeap,
    jvm: &mut crate::jvmheap::JvmHeap,
    alloc: &mut Allocator,
    bpftime: &mut Bpftime,
    config: &CollectorConfig,
    start_ns: u64,
    end_ns: u64,
    offcpu_enabled: bool,
    stats: &CollectorStats,
    on_report: &F,
) -> Result<(), String>
where
    F: Fn(Report),
{
    resolver.report_diagnostics();
    let mut cpu_records = drain_map(ebpf, "cpu_counts")?;
    let mut offcpu_records = if offcpu_enabled {
        drain_map(ebpf, "offcpu_totals")?
    } else {
        Vec::new()
    };

    let mut excluded: HashSet<u32> = config.exclude_pids.iter().copied().collect();
    if config.exclude_self {
        excluded.insert(std::process::id());
    }
    let unattributed = cpu_records
        .iter()
        .chain(offcpu_records.iter())
        .filter(|record| record.tgid == 0)
        .count() as u64;
    stats
        .unattributed
        .fetch_add(unattributed, Ordering::Relaxed);
    cpu_records.retain(|r| !excluded.contains(&r.tgid) && r.tgid != 0);
    offcpu_records.retain(|r| !excluded.contains(&r.tgid) && r.tgid != 0);

    if cpu_records.is_empty()
        && offcpu_records.is_empty()
        && !config.collect_heap
        && !config.collect_jvm
        && !config.collect_alloc
    {
        return Ok(());
    }
    stats
        .cpu_records
        .fetch_add(cpu_records.len() as u64, Ordering::Relaxed);
    stats
        .offcpu_records
        .fetch_add(offcpu_records.len() as u64, Ordering::Relaxed);

    // ---- stacks ---------------------------------------------------------
    let mut needed: Vec<u32> = Vec::new();
    for record in cpu_records.iter().chain(offcpu_records.iter()) {
        if record.user_stack != INVALID_STACK_ID {
            needed.push(record.user_stack);
        }
        if record.kernel_stack != INVALID_STACK_ID {
            needed.push(record.kernel_stack);
        }
    }
    needed.sort_unstable();
    needed.dedup();

    let mut stack_ips: HashMap<u32, Vec<u64>> = HashMap::with_capacity(needed.len());
    {
        let stacks = StackTraceMap::try_from(
            ebpf.map("stacks")
                .ok_or_else(|| "missing `stacks` map".to_string())?,
        )
        .map_err(|err| format!("`stacks` is not a stack trace map: {err}"))?;
        for id in needed {
            match stacks.get(&id, 0) {
                Ok(trace) => {
                    let ips: Vec<u64> = trace.frames().iter().map(|f| f.ip).collect();
                    if !ips.is_empty() {
                        stack_ips.insert(id, ips);
                    }
                }
                Err(_) => {
                    // The entry was evicted between reading the counters and
                    // reading the stack; the sample is simply dropped.
                }
            }
        }
    }

    let period_ns = 1_000_000_000u64 / config.frequency_hz.max(1);
    let mut cpu_builders: HashMap<GroupKey, ReportBuilder> = HashMap::new();
    let mut offcpu_builders: HashMap<GroupKey, ReportBuilder> = HashMap::new();
    // A process that is gone cannot be symbolized at all, so its records are
    // skipped instead of being stored as a wall of `[unknown]` frames.
    let mut groups: HashMap<u32, Option<GroupKey>> = HashMap::new();
    let mut skipped = 0u64;
    let mut empty = 0u64;

    for (records, builders, factor) in [
        (&cpu_records, &mut cpu_builders, period_ns),
        (&offcpu_records, &mut offcpu_builders, 1u64),
    ] {
        for record in records.iter() {
            let group = groups
                .entry(record.tgid)
                .or_insert_with(|| resolve_group(config, record.tgid))
                .clone();
            let Some(group) = group else {
                skipped += 1;
                continue;
            };

            let mut frames: Vec<Frame> = Vec::new();
            if record.kernel_stack != INVALID_STACK_ID
                && let Some(ips) = stack_ips.get(&record.kernel_stack)
            {
                frames.extend(resolver.resolve_kernel(ips));
            }
            if record.user_stack != INVALID_STACK_ID
                && let Some(ips) = stack_ips.get(&record.user_stack)
            {
                frames.extend(resolver.resolve_user(record.tgid, ips));
            }
            if frames.is_empty() {
                empty += 1;
                continue;
            }

            let builder = builders.entry(group.clone()).or_insert_with(|| {
                ReportBuilder::new(group.service.clone(), ProfileType::cpu(), start_ns, end_ns)
                    .sample_rate_hz(config.frequency_hz as u32)
                    .with_labels(group.labels.clone())
            });
            builder.add_stack(frames, record.value.saturating_mul(factor));
        }
    }

    let mut emitted = 0usize;
    for (_group, builder) in cpu_builders {
        if builder.is_empty() {
            continue;
        }
        let mut report = builder.finish();
        report.profile_type = ProfileType::cpu();
        emit(on_report, report, stats, &mut emitted);
    }
    for (_group, builder) in offcpu_builders {
        if builder.is_empty() {
            continue;
        }
        let mut report = builder.finish();
        report.profile_type = ProfileType::off_cpu();
        emit(on_report, report, stats, &mut emitted);
    }

    // ---- Go heaps ---------------------------------------------------------
    //
    // A separate pass, with nothing in common with the perf-event path: no
    // sample to attribute, no kernel counter to drain. Each Go process already
    // keeps its own heap profile, and this reads it.
    let mut heap_alive: Vec<u32> = Vec::new();
    if config.collect_heap {
        let mut skip: Vec<u32> = config.exclude_pids.clone();
        if config.exclude_self {
            skip.push(std::process::id());
        }
        let records = heap.scan(&skip, &mut heap_alive);
        stats
            .heap_samples
            .fetch_add(records.len() as u64, Ordering::Relaxed);
        stats
            .heap_processes
            .store(heap_alive.len() as u64, Ordering::Relaxed);
        stats
            .heap_unreadable
            .store(heap.unreadable, Ordering::Relaxed);
        stats.heap_disabled.store(heap.disabled, Ordering::Relaxed);
        stats
            .heap_invalid
            .store(heap.invalid_buckets, Ordering::Relaxed);
        if !heap.skipped.is_empty() {
            debug!(skipped = ?heap.skipped, "Go heap targets left out");
        }

        let mut builders: HashMap<(GroupKey, String), ReportBuilder> = HashMap::new();
        for record in &records {
            let Some(group) = resolve_group(config, record.tgid) else {
                continue;
            };
            let frames = resolver.resolve_user(record.tgid, &record.stack);
            if frames.is_empty() {
                continue;
            }
            let mut metrics: Vec<(&'static str, u64)> = vec![
                (ProfileType::ALLOC_SPACE, record.alloc_bytes),
                (ProfileType::INUSE_SPACE, record.inuse_bytes),
            ];
            if config.heap_objects {
                metrics.push((ProfileType::ALLOC_OBJECTS, record.alloc_objects));
                metrics.push((ProfileType::INUSE_OBJECTS, record.inuse_objects));
            }
            for (name, value) in metrics {
                if value == 0 {
                    continue;
                }
                let builder = builders
                    .entry((group.clone(), name.to_string()))
                    .or_insert_with(|| {
                        ReportBuilder::new(
                            group.service.clone(),
                            ProfileType::new(name),
                            start_ns,
                            end_ns,
                        )
                        // `with_labels` *replaces* the label set, so it has
                        // to come first or it wipes the ones above.
                        .with_labels(group.labels.clone())
                        // Which reader produced this: a runtime's own sampler
                        // and an allocator uprobe do not cover the same memory,
                        // and the UI must not add them together.
                        .label("profile_source", "go-runtime")
                        .label("sampling", "runtime")
                        // The runtime reports what the process has allocated since
                        // it started, not what happened inside this window. Without
                        // this the same profile type would be summed as if it were
                        // a delta and a long-lived process would report terabytes.
                        .label("temporality", "cumulative")
                        // The same label the allocator path writes, so a
                        // selector on `runtime` covers both readers instead of
                        // silently matching only half the memory series.
                        .label("runtime", "go")
                    });
                builder.add_stack(frames.clone(), value);
            }
        }
        for ((_group, name), builder) in builders {
            if builder.is_empty() {
                continue;
            }
            let mut report = builder.finish();
            report.profile_type = ProfileType::new(name);
            stats.heap_reports.fetch_add(1, Ordering::Relaxed);
            emit(on_report, report, stats, &mut emitted);
        }
    }

    // ---- JVM heaps, through the runtime's own recorder --------------------
    //
    // The third reader, same bargain as the Go one: a JVM samples its own
    // allocations into Java Flight Recorder, and this only asks for a dump and
    // reads it back. Nothing is injected into the target, but it needs a JDK's
    // `jcmd`/`jfr` and permission to attach, so it is skipped silently when the
    // image has neither.
    let mut jvm_alive: Vec<u32> = Vec::new();
    if config.collect_jvm && jvm.available() {
        let skip: Vec<u32> = excluded.iter().copied().collect();
        // JVM reading pauses the target and moves a few megabytes per interval,
        // so it runs only for the workloads the server has switched on.
        let requested = config.jvm_workloads.read();
        let allowed = |tgid: u32| match workload_key(config, tgid) {
            Some(key) => requested.get(&key).copied().unwrap_or(false),
            None => false,
        };
        let records = jvm.scan(&skip, &mut jvm_alive, &allowed);
        stats
            .jvm_samples
            .fetch_add(records.len() as u64, Ordering::Relaxed);
        stats
            .jvm_processes
            .store(jvm_alive.len() as u64, Ordering::Relaxed);
        stats
            .jvm_attach_failed
            .store(jvm.attach_failed, Ordering::Relaxed);
        if jvm.attach_failed > 0 || !jvm.skipped.is_empty() {
            debug!(
                attach_failed = jvm.attach_failed,
                empty = jvm.empty,
                skipped = ?jvm.skipped,
                "JVM targets left out"
            );
        }

        let mut builders: HashMap<(GroupKey, String), ReportBuilder> = HashMap::new();
        for record in &records {
            let Some(group) = resolve_group(config, record.tgid) else {
                continue;
            };
            if record.frames.is_empty() {
                continue;
            }
            let mut metrics: Vec<(&'static str, u64)> = vec![
                (ProfileType::ALLOC_SPACE, record.alloc_bytes),
                (ProfileType::INUSE_SPACE, record.inuse_bytes),
            ];
            if config.heap_objects {
                // JFR samples carry bytes only; objects are not derivable from
                // them without the allocation size of each sample.
            }
            for (name, value) in metrics {
                if value == 0 {
                    continue;
                }
                let builder = builders
                    .entry((group.clone(), name.to_string()))
                    .or_insert_with(|| {
                        ReportBuilder::new(
                            group.service.clone(),
                            ProfileType::new(name),
                            start_ns,
                            end_ns,
                        )
                        .with_labels(group.labels.clone())
                        .label("profile_source", "jvm-jfr")
                        .label("sampling", "runtime")
                        .label("temporality", "cumulative")
                        .label("runtime", "java")
                    });
                builder.add_stack(record.frames.clone(), value);
            }
        }
        for ((_group, name), builder) in builders {
            if builder.is_empty() {
                continue;
            }
            let mut report = builder.finish();
            report.profile_type = ProfileType::new(name);
            emit(on_report, report, stats, &mut emitted);
        }
    }

    // ---- native memory, through the userspace runtime ---------------------
    //
    // The measurement the allocator probes make, with the hook running inside
    // the target instead of in the kernel. A kernel uprobe cost 79 µs of system
    // time per allocation on this cluster; here the same call costs hundreds of
    // nanoseconds. The bargain is a shared library injected into the process,
    // so this too is switched on per workload and stays off otherwise.
    if config.collect_bpftime && bpftime.available() {
        let requested = config.alloc_workloads.read();
        let allowed = |tgid: u32| match workload_key(config, tgid) {
            Some(key) => requested.get(&key).copied().unwrap_or(false),
            None => false,
        };
        let interval = config.flush_interval.as_secs().max(1);
        let records = bpftime.scan(&allowed, resolver, interval);
        stats
            .bpftime_samples
            .fetch_add(records.len() as u64, Ordering::Relaxed);
        stats
            .bpftime_attach_failed
            .store(bpftime.attach_failed, Ordering::Relaxed);
        if bpftime.attach_failed > 0 || !bpftime.skipped.is_empty() {
            debug!(
                attach_failed = bpftime.attach_failed,
                skipped = ?bpftime.skipped,
                "bpftime targets left out"
            );
        }

        let mut builders: HashMap<(GroupKey, String), ReportBuilder> = HashMap::new();
        for record in &records {
            let Some(group) = resolve_group(config, record.tgid) else {
                continue;
            };
            if record.frames.is_empty() {
                continue;
            }
            let mut metrics: Vec<(&'static str, u64)> =
                vec![(ProfileType::ALLOC_SPACE, record.alloc_bytes)];
            if config.heap_objects {
                metrics.push((ProfileType::ALLOC_OBJECTS, record.alloc_count));
            }
            for (name, value) in metrics {
                if value == 0 {
                    continue;
                }
                let builder = builders
                    .entry((group.clone(), name.to_string()))
                    .or_insert_with(|| {
                        ReportBuilder::new(
                            group.service.clone(),
                            ProfileType::new(name),
                            start_ns,
                            end_ns,
                        )
                        .with_labels(group.labels.clone())
                        .label("profile_source", "bpftime-userspace")
                        .label("sampling", "instrumented")
                        .label("runtime", "native")
                    });
                builder.add_stack(record.frames.clone(), value);
            }
        }
        for ((_group, name), builder) in builders {
            if builder.is_empty() {
                continue;
            }
            let mut report = builder.finish();
            report.profile_type = ProfileType::new(name);
            emit(on_report, report, stats, &mut emitted);
        }
    }

    // ---- native heaps, through the allocator ------------------------------
    //
    // The other half of the memory picture, and a different measurement: no
    // runtime keeps a record here, so the bytes are counted as they are handed
    // out by `malloc`. That makes `inuse_*` mean "allocated and not yet freed
    // since the probe was attached" — not "the resident heap" — which is why
    // the reports carry `profile_source=allocator-uprobe`.
    let mut alloc_alive: Vec<u32> = Vec::new();
    if config.collect_alloc {
        // New containers come and go; this is a no-op unless the interval has
        // passed or nothing has been instrumented yet.
        alloc.discover(ebpf, false);
        stats
            .alloc_probes
            .store(alloc.probes as u64, Ordering::Relaxed);
        if !alloc.skipped.is_empty() {
            debug!(skipped = ?alloc.skipped, "allocator targets left out");
        }

        // A Go process's heap is already reported by the runtime reader. The
        // probes are process-wide and cannot tell one language from another,
        // so the classification decides, not the probe.
        let go_pids = heap.go_pids();
        let records = alloc.scan(ebpf, &excluded, &go_pids);
        stats
            .alloc_samples
            .fetch_add(records.len() as u64, Ordering::Relaxed);
        stats.alloc_lost.store(
            alloc.no_capture_bytes + alloc.evicted_bytes,
            Ordering::Relaxed,
        );
        if tracing::enabled!(tracing::Level::DEBUG) {
            // Per-process totals for the window: the first thing to look at
            // when a heap reads as too small, because it separates "the probes
            // did not see it" from "the report was dropped".
            let mut per: BTreeMap<String, (u64, u64)> = BTreeMap::new();
            for record in &records {
                let entry = per
                    .entry(procfs::read_comm(record.tgid).unwrap_or_default())
                    .or_default();
                entry.0 += record.alloc_bytes;
                entry.1 += record.inuse_bytes;
            }
            debug!(rows = records.len(), totals = ?per, "allocator window");
        }
        if alloc.lost_stacks > 0 || alloc.capture_failures > 0 {
            debug!(
                evicted = alloc.lost_stacks,
                evicted_bytes = alloc.evicted_bytes,
                capture_failures = alloc.capture_failures,
                capture_bytes = alloc.no_capture_bytes,
                capture_errnos = ?alloc.capture_errnos,
                "allocation stacks lost"
            );
        }

        let mut builders: HashMap<(GroupKey, String, &'static str, &'static str), ReportBuilder> =
            HashMap::new();
        // Per-report totals, so that a heap too small to be worth a series can
        // be left out. A node runs hundreds of processes and most of them hold
        // a few kilobytes; publishing one series per process per metric per
        // flush buries the handful of real heaps and costs the store more than
        // it is worth.
        let mut totals: HashMap<(GroupKey, String, &'static str, &'static str), u64> =
            HashMap::new();
        for record in &records {
            let Some(group) = resolve_group(config, record.tgid) else {
                continue;
            };
            alloc_alive.push(record.tgid);
            let mut frames = resolver.resolve_user(record.tgid, &record.stack);
            // The stack was captured at the allocator's entry point, so its
            // leaf frame is always the allocator itself — `malloc`,
            // `je_malloc`, `operator new` — which says the same thing for every
            // allocation on the node and nothing about this one. Dropping it
            // leaves the caller's stack, which is what a heap profile is read
            // for.
            //
            // It is dropped only when something remains: a target built without
            // frame pointers unwinds to that one frame, and it is then the
            // allocation site's only evidence. Removing it would report zero
            // bytes for a perfectly healthy program.
            if frames.len() > 1 {
                frames.remove(0);
            }

            let mut metrics: Vec<(&'static str, u64)> = vec![
                (ProfileType::ALLOC_SPACE, record.alloc_bytes),
                (ProfileType::INUSE_SPACE, record.inuse_bytes),
            ];
            if config.heap_objects {
                metrics.push((ProfileType::ALLOC_OBJECTS, record.alloc_objects));
                metrics.push((ProfileType::INUSE_OBJECTS, record.inuse_objects));
            }
            for (name, value) in metrics {
                if value == 0 {
                    continue;
                }
                let key = (
                    group.clone(),
                    name.to_string(),
                    record.runtime,
                    record.allocator,
                );
                let builder = builders.entry(key.clone()).or_insert_with(|| {
                    ReportBuilder::new(
                        group.service.clone(),
                        ProfileType::new(name),
                        start_ns,
                        end_ns,
                    )
                    .with_labels(group.labels.clone())
                    .label("profile_source", "allocator-uprobe")
                    .label("sampling", "uprobe")
                    .label("runtime", record.runtime)
                    .label("allocator", record.allocator)
                });
                builder.add_stack(frames.clone(), value);
                *totals.entry(key).or_default() += value;
            }
        }
        for (key, builder) in builders {
            if builder.is_empty() {
                continue;
            }
            // Below the floor a profile describes an implementation detail
            // rather than memory use: 256 KiB held, or 256 KiB allocated in one
            // interval. The floor is per report, so a process that only ever
            // allocates small amounts in many places still appears once it adds
            // up.
            let floor = if key.1.ends_with("objects") {
                ALLOC_REPORT_OBJECTS_FLOOR
            } else {
                ALLOC_REPORT_BYTES_FLOOR
            };
            if totals.get(&key).copied().unwrap_or(0) < floor {
                continue;
            }
            let mut report = builder.finish();
            report.profile_type = ProfileType::new(key.1.clone());
            stats.alloc_reports.fetch_add(1, Ordering::Relaxed);
            emit(on_report, report, stats, &mut emitted);
        }
    }

    // A sampled pid that is gone by now has no usable symbols left, so its
    // cache entry would only serve a future process that recycles the pid.
    let mut alive: Vec<u32> = cpu_records
        .iter()
        .chain(offcpu_records.iter())
        .map(|record| record.tgid)
        .collect();
    // The heap scan symbolized addresses for processes that may have had no CPU
    // sample this interval; dropping their module cache would re-parse a binary
    // every flush.
    alive.extend(heap_alive);
    // Same for the allocator samples: a heap report can be the only reason a
    // pid's modules were parsed this interval.
    alive.extend(alloc_alive);
    resolver.retain_processes(&alive);

    let tally = resolver.tally();
    stats.symbol_frames.store(tally.resolved, Ordering::Relaxed);
    stats.symbol_inlined.store(tally.inlined, Ordering::Relaxed);
    stats.symbol_go.store(tally.go, Ordering::Relaxed);
    stats.symbol_jit.store(tally.jit, Ordering::Relaxed);
    stats.symbol_module.store(tally.module, Ordering::Relaxed);
    let (_, module_failures) = resolver.module_counts();
    stats
        .symbol_module_failed
        .store(module_failures, Ordering::Relaxed);
    stats.skipped.fetch_add(skipped, Ordering::Relaxed);
    stats.empty_stacks.fetch_add(empty, Ordering::Relaxed);
    stats
        .symbol_failures
        .store(resolver.failed_lookups(), Ordering::Relaxed);
    if let Ok(mut unknown) = stats.symbol_unknown.lock() {
        *unknown = tally.unknown;
    }
    if tracing::enabled!(tracing::Level::DEBUG) {
        let sample: Vec<String> = groups
            .iter()
            .filter_map(|(tgid, group)| {
                group
                    .as_ref()
                    .map(|group| format!("{tgid}:{}", group.service))
            })
            .take(6)
            .collect();
        debug!(targets = ?sample, skipped, "sample targets");
    }
    debug!(
        cpu_records = cpu_records.len(),
        offcpu_records = offcpu_records.len(),
        reports = emitted,
        "flushed profile batch"
    );
    Ok(())
}

fn emit<F>(on_report: &F, report: Report, stats: &CollectorStats, emitted: &mut usize)
where
    F: Fn(Report),
{
    if report.stacks.is_empty() {
        return;
    }
    stats.reports.fetch_add(1, Ordering::Relaxed);
    *emitted += 1;
    on_report(report);
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GroupKey {
    service: String,
    labels: BTreeMap<String, String>,
}

fn resolve_group(config: &CollectorConfig, tgid: u32) -> Option<GroupKey> {
    let comm = procfs::read_comm(tgid)?;
    let mut labels = config.labels.clone();
    if config.node_label {
        labels
            .entry("node".to_string())
            .or_insert_with(procfs::hostname);
    }

    // The cgroup path gives the pod UID, and the UID gives everything else.
    // Both halves are optional: no cgroup path outside a container, and no pod
    // metadata outside a cluster (or without the RBAC to read it).
    let mut pod = None;
    if config.enrich_kubernetes_labels
        && let Some(cgroup) = procfs::read_cgroup(tgid)
    {
        let from_cgroup = procfs::kubernetes_labels(&cgroup);
        if let (Some(index), Some(uid)) = (config.pods.as_ref(), from_cgroup.get("k8s_pod_uid")) {
            pod = index.lookup(uid);
        }
        for (key, value) in from_cgroup {
            labels.entry(key).or_insert(value);
        }
    }
    if let Some(meta) = &pod {
        labels
            .entry("k8s_namespace".to_string())
            .or_insert_with(|| meta.namespace.clone());
        labels
            .entry("k8s_pod".to_string())
            .or_insert_with(|| meta.name.clone());
        if let Some(workload) = &meta.workload {
            labels
                .entry("k8s_workload".to_string())
                .or_insert_with(|| workload.clone());
        }
        if let Some(kind) = &meta.workload_kind {
            labels
                .entry("k8s_workload_kind".to_string())
                .or_insert_with(|| kind.clone());
        }
    }

    // A pod knows what it is; a bare process only knows which binary it runs.
    let service = config
        .service_name
        .clone()
        .or_else(|| {
            pod.as_ref()
                .and_then(|meta| meta.service_name().map(str::to_string))
        })
        .unwrap_or_else(|| comm.clone());
    if service != comm {
        // The service name hides the binary, so keep the binary addressable.
        labels
            .entry("process_name".to_string())
            .or_insert_with(|| comm.clone());
    }
    Some(GroupKey { service, labels })
}

/// `namespace/workload` for the pod this thread belongs to.
///
/// The same identifier the service map draws a card with, so the switch the
/// user flips on a map node names exactly one entry here. A bare pod has no
/// workload, and it is named by its own pod name instead — that number is still
/// stable for the life of the pod, which is all a switch can promise.
fn workload_key(config: &CollectorConfig, tgid: u32) -> Option<String> {
    if !config.enrich_kubernetes_labels {
        return None;
    }
    let cgroup = procfs::read_cgroup(tgid)?;
    let labels = procfs::kubernetes_labels(&cgroup);
    let uid = labels.get("k8s_pod_uid")?;
    let index = config.pods.as_ref()?;
    let meta = index.lookup(uid)?;
    let name = meta.workload.clone().unwrap_or_else(|| meta.name.clone());
    Some(format!("{}/{}", meta.namespace, name))
}

fn drain_map(ebpf: &mut Ebpf, name: &str) -> Result<Vec<RawRecord>, String> {
    let map = ebpf
        .map_mut(name)
        .ok_or_else(|| format!("missing eBPF map `{name}`"))?;
    let mut map: AyaHashMap<&mut MapData, StackKey, u64> =
        AyaHashMap::try_from(map).map_err(|err| format!("invalid map `{name}`: {err}"))?;

    let keys: Vec<StackKey> = map.keys().filter_map(|key| key.ok()).collect();

    let mut records = Vec::with_capacity(keys.len());
    for key in &keys {
        if let Ok(value) = map.get(key, 0) {
            records.push(RawRecord {
                tgid: key.tgid,
                user_stack: key.user_stack,
                kernel_stack: key.kernel_stack,
                value,
            });
        }
    }
    // Reset the counters. An increment that lands between the read and the
    // delete is lost, which at 99Hz is at most a handful of samples.
    for key in keys {
        let _ = map.remove(&key);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    /// The off-CPU fallback has to say which stack it captured.
    ///
    /// User and kernel stacks share one id space (`stacks`), and the resolver
    /// is chosen by the slot an id sits in: a kernel stack recorded as
    /// `user_stack` is handed to the user-space symbolizer, which then reports
    /// every `0xffff...` frame as an address in no mapping. The branch only runs
    /// for a task whose user stack the unwinder cannot follow, which a unit test
    /// cannot provoke, so the contract is checked against the source.
    #[test]
    fn the_offcpu_fallback_records_which_stack_it_captured() {
        let source = include_str!("../bpf/profile.bpf.c");
        assert!(
            source.contains("state.kind = STACK_KIND_KERNEL"),
            "the start probe must mark a kernel fallback as such"
        );
        let end = source
            .split("lidlesseye_offcpu_end")
            .nth(1)
            .expect("the end probe is in the source");
        assert!(
            end.contains("key.kernel_stack = stack_id"),
            "a kernel stack has to land in the kernel slot"
        );
        assert!(
            end.contains("key.user_stack = stack_id"),
            "a user stack has to land in the user slot"
        );
    }
}
