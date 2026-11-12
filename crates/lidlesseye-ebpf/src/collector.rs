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
use crate::procfs;
use crate::symbolizer::FrameResolver;

const BPF_OBJECT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/profile.bpf.o"));

pub(crate) fn bpf_object() -> &'static [u8] {
    BPF_OBJECT
}
const INVALID_STACK_ID: u32 = u32::MAX;

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
    /// Process ids to ignore.
    pub exclude_pids: Vec<u32>,
    /// Ignore the collector's own process so that the profiler never profiles
    /// itself. Turn this off when the workload lives in the same process as the
    /// collector (tests, single-process embedded use).
    pub exclude_self: bool,
    /// Derive `k8s_*` labels from the cgroup path of each process.
    pub enrich_kubernetes_labels: bool,
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
            exclude_pids: Vec::new(),
            exclude_self: true,
            enrich_kubernetes_labels: true,
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
        if !self.collect_cpu && !self.collect_off_cpu {
            return Err(
                "at least one of collect_cpu / collect_off_cpu must be enabled".to_string(),
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
    pub symbol_unknown: Mutex<UnknownReasons>,
    /// Records dropped because the process no longer exists, cumulatively.
    pub skipped: AtomicU64,
    /// Records whose pid could not be translated into the agent's PID
    /// namespace. Non-zero means the samples of other containers are invisible.
    pub unattributed: AtomicU64,
    /// Records that were sampled but carried no usable stack at all.
    pub empty_stacks: AtomicU64,
    pub drains: AtomicU64,
    pub errors: AtomicU64,
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

/// Must match `struct ns_config` in `profile.bpf.c`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct NsConfig {
    dev: u64,
    ino: u64,
}

// SAFETY: two `u64` fields, no padding, no pointers.
unsafe impl aya::Pod for NsConfig {}

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

    // Translate the kernel-global pids reported by the helpers into the agent's
    // PID namespace, so that they line up with `/proc`. The value has to outlive
    // the loader because `override_global` keeps a borrow of it until `load`.
    let ns_cfg = procfs::pid_namespace().map(|(dev, ino)| NsConfig { dev, ino });
    if let Some(cfg) = &ns_cfg {
        debug!(
            dev = cfg.dev,
            ino = cfg.ino,
            "using PID namespace translation"
        );
    }
    let mut loader = aya::EbpfLoader::new();
    if let Some(cfg) = ns_cfg.as_ref() {
        // `must_exist = false` so that an object built without `ns_cfg` still
        // loads (the pids then stay in the initial namespace).
        loader.override_global("ns_cfg", cfg, false);
    }
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

    if cpu_events == 0 && !offcpu_attached {
        return Err("no eBPF program could be attached".to_string());
    }

    let mut resolver = FrameResolver::new();
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

    if cpu_records.is_empty() && offcpu_records.is_empty() {
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

    // A sampled pid that is gone by now has no usable symbols left, so its
    // cache entry would only serve a future process that recycles the pid.
    let alive: Vec<u32> = cpu_records
        .iter()
        .chain(offcpu_records.iter())
        .map(|record| record.tgid)
        .collect();
    resolver.retain_processes(&alive);

    let tally = resolver.tally();
    stats.symbol_frames.store(tally.resolved, Ordering::Relaxed);
    stats.symbol_inlined.store(tally.inlined, Ordering::Relaxed);
    stats.symbol_go.store(tally.go, Ordering::Relaxed);
    stats.symbol_jit.store(tally.jit, Ordering::Relaxed);
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
    let service = config.service_name.clone().unwrap_or_else(|| comm.clone());
    let mut labels = config.labels.clone();
    if config.node_label {
        labels
            .entry("node".to_string())
            .or_insert_with(procfs::hostname);
    }
    if config.service_name.is_some() {
        // When everything is aggregated under one service name, keep the
        // process name so that individual binaries remain distinguishable.
        labels
            .entry("process_name".to_string())
            .or_insert(comm.clone());
    }
    if config.enrich_kubernetes_labels
        && let Some(cgroup) = procfs::read_cgroup(tgid)
    {
        for (key, value) in procfs::kubernetes_labels(&cgroup) {
            labels.entry(key).or_insert(value);
        }
    }
    Some(GroupKey { service, labels })
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
