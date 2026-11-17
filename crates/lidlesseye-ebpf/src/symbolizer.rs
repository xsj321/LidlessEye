//! Address → symbol resolution, with per-address caching.
//!
//! Symbolization happens in the agent, before the report is shipped. That is a
//! deliberate choice: the storage server then never needs access to the
//! profiled machine's file system, and profiles of processes that have already
//! exited remain readable. It is the same trade-off Pyroscope's eBPF agent
//! makes.
//!
//! Three details decide whether a stack comes out complete or full of holes:
//!
//! * **Return addresses are off by one.** Only the innermost frame holds an
//!   instruction pointer that was actually executing; every frame below it
//!   holds the return address pushed by `call`, i.e. the address of the
//!   *next* instruction. Resolving it verbatim lands on whatever symbol starts
//!   at that boundary — often the wrong function, occasionally past the end of
//!   the last symbol. Such a frame is worse than an unknown one, because it
//!   looks plausible. [`probe_addrs`] subtracts one from every frame but the
//!   innermost.
//! * **DWARF inline information is where most leaves live.** A release build
//!   inlines aggressively: the physical function named by the symbol table is
//!   usually an outer wrapper, and the code actually executing belongs to a
//!   function that only exists in `.debug_info`. Those frames are recovered
//!   from [`blazesym::symbolize::Sym::inlined`] and emitted leaf first.
//! * **"Unknown" has reasons.** A stripped `.so`, an anonymous `JIT` mapping
//!   and an address outside every symbol are three different problems with
//!   three different fixes. [`UnknownReasons`] keeps them apart instead of
//!   collapsing everything into one counter.

use std::collections::HashMap;

use std::time::Instant;

use blazesym::Pid;
use blazesym::symbolize::Reason;
use blazesym::symbolize::source::{Kernel, Process, Source};
use blazesym::symbolize::{CodeInfo, Input, Symbolized, Symbolizer};
use lidlesseye_core::Frame;

use crate::UnknownReasons;
use crate::module::{ModuleCache, Source as ModuleSource};
use crate::perf_map::PerfMapFallback;

/// Hard cap on the symbol cache by entry count.
const MAX_CACHE_ENTRIES: usize = 40_000;

/// Hard cap on the symbol cache by estimated heap size.
///
/// Counting entries alone is not enough: one address expands to one `Frame`
/// per inlined function, and every frame owns three `String`s (module, symbol,
/// source file). A 200k-entry cache with a handful of inlined frames each is
/// several hundred MiB — enough on its own to OOM a 1 GiB agent. Budget the
/// caches in bytes instead and drop them wholesale when the budget is spent:
/// a process restart or a container recycle makes old entries useless anyway.
const MAX_CACHE_BYTES: usize = 48 * 1024 * 1024;

/// How many distinct processes may be symbolized before the resolver starts
/// over with empty caches.
///
/// blazesym's per-process cache is *insert-only*: it has no eviction, and a
/// symbolizer keeps every module it parsed for every process it has seen. That
/// is fine for a single-service agent, but an agent that profiles a whole node
/// — with pods coming and going, and Go services whose symbol tables run to
/// tens of megabytes — would grow until the kernel OOM killer takes it down.
/// Rebasing the caches every few hundred processes bounds it; the cost is one
/// flush of slower symbolization afterwards, because everything is re-read.
///
/// 128 is deliberate: with a 1 GiB container limit the symbol tables of a few
/// hundred processes already dominate RSS, so waiting for 1024 processes made
/// the rebase happen too late to prevent an OOM kill.
const MAX_PROCESSES: usize = 128;

/// Approximate heap cost of a resolved stack. Exact allocation sizes are not
/// observable, but the three owned strings dominate and their lengths are.
fn frames_bytes(frames: &[Frame]) -> usize {
    frames
        .iter()
        .map(|frame| {
            std::mem::size_of::<Frame>()
                + frame.module.len()
                + frame.symbol.len()
                + frame.file.len()
        })
        .sum()
}

impl UnknownReasons {
    fn record(&mut self, reason: &Reason) {
        match reason {
            Reason::Unmapped => self.unmapped += 1,
            Reason::MissingSyms => self.missing_syms += 1,
            Reason::MissingComponent => self.missing_component += 1,
            Reason::UnknownAddr => self.unknown_addr += 1,
            Reason::Unsupported => self.unsupported += 1,
            _ => self.other += 1,
        }
    }
}

/// Running tally of what symbolization produced, accumulated so the collector
/// can publish it without keeping per-address state around.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Tally {
    /// Frames that carry a real symbol name, inlined frames included.
    pub resolved: u64,
    /// Frames that exist only because of DWARF inline information. This is the
    /// number that says "how much did the inline expansion buy us".
    pub inlined: u64,
    /// Frames named by a Go `.gopclntab` after the ELF symbol table came up
    /// empty. Without this fallback every Go binary on a node is a wall of
    /// addresses.
    pub go: u64,
    /// Frames named from an ELF symbol table that `blazesym` could not reach,
    /// because the file is only visible through `/proc/<pid>/root`. See
    /// [`crate::module`].
    pub module: u64,
    /// Frames named by a JIT runtime's perf map.
    pub jit: u64,
    /// Addresses that came back as `Symbolized::Unknown`.
    pub unknown: UnknownReasons,
    /// Batches that failed outright (process gone, unreadable maps, ...).
    pub failed: u64,
    /// Frames dropped because the walk had already left the process's address
    /// space. They are not symbolization failures: the values were never
    /// instruction pointers.
    pub truncated: u64,
}

impl Tally {
    fn merge(&mut self, other: Tally) {
        self.resolved += other.resolved;
        self.inlined += other.inlined;
        self.go += other.go;
        self.module += other.module;
        self.jit += other.jit;
        self.failed += other.failed;
        self.truncated += other.truncated;
        self.unknown.unmapped += other.unknown.unmapped;
        self.unknown.missing_syms += other.unknown.missing_syms;
        self.unknown.missing_component += other.unknown.missing_component;
        self.unknown.unknown_addr += other.unknown.unknown_addr;
        self.unknown.unsupported += other.unknown.unsupported;
        self.unknown.other += other.unknown.other;
    }
}

pub(crate) struct FrameResolver {
    symbolizer: Symbolizer,
    user_cache: HashMap<(u32, u64), Vec<Frame>>,
    kernel_cache: HashMap<u64, Vec<Frame>>,
    /// Estimated heap held by the two caches. Kept in step with every insert,
    /// eviction and clear so pruning can be driven by memory rather than by a
    /// count of addresses that says nothing about the size of a frame.
    user_bytes: usize,
    kernel_bytes: usize,
    /// Processes symbolized since the caches were last rebased. Kept separate
    /// from `user_cache` because it is a bound on *churn*: the entries blazesym
    /// cannot evict are exactly the processes that are already gone.
    processes: std::collections::HashSet<u32>,
    /// Last resort for binaries without any ELF symbols, and for binaries whose
    /// file is only reachable through the target's mount namespace. See
    /// [`crate::module`].
    modules: ModuleCache,
    /// Last resort for code that exists in no file at all. See
    /// [`crate::perf_map`].
    perf_map: PerfMapFallback,
    tally: Tally,
    /// When the coverage summary was last written, so the agent explains
    /// itself once a minute instead of once per stack.
    last_report: Option<Instant>,
    /// `/proc/kallsyms` is checked once: whether it is readable does not change
    /// while the agent runs, and the answer explains every unnamed `[kernel]`
    /// frame at once.
    kernel_symbols_checked: bool,
    /// `uname -r` of the node, read once for the coverage line. "Which kernel
    /// is this" is the first question a symbol report from another cluster
    /// raises, and the answer belongs in the same line as the numbers.
    kernel_release: Option<String>,
    /// Whether to resolve user addresses through `/proc/<pid>/map_files`.
    ///
    /// Those entries are magic links that resolve inside the *target's* mount
    /// namespace, which is the only reliable way to symbolize a process that
    /// lives in another container. They need `CAP_SYS_ADMIN` and are refused by
    /// Docker's default seccomp profile, so the first failure flips this to
    /// `false` and the resolver falls back to the symbolic paths listed in
    /// `/proc/<pid>/maps`.
    map_files: bool,
}

impl FrameResolver {
    pub(crate) fn new() -> Self {
        FrameResolver {
            symbolizer: Symbolizer::new(),
            user_cache: HashMap::new(),
            kernel_cache: HashMap::new(),
            user_bytes: 0,
            kernel_bytes: 0,
            processes: std::collections::HashSet::new(),
            modules: ModuleCache::new(),
            perf_map: PerfMapFallback::new(),
            tally: Tally::default(),
            last_report: None,
            kernel_symbols_checked: false,
            kernel_release: None,
            map_files: true,
        }
    }

    /// Explain, at most once a minute, what could not be named and why.
    ///
    /// A dashboard full of `0x5f3c...` leaves the user guessing whether the
    /// binary is stripped, the kernel is hiding its symbols, or the agent
    /// cannot read the file at all. The three have different fixes, and on a
    /// cluster nobody can shell into, the log is the only place they can be
    /// told apart.
    pub(crate) fn report_diagnostics(&mut self) {
        const INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
        const MAX_LINES: usize = 8;
        if self.last_report.is_some_and(|at| at.elapsed() < INTERVAL) {
            return;
        }
        self.last_report = Some(Instant::now());
        self.check_kernel_symbols();

        let kernel = self
            .kernel_release
            .get_or_insert_with(|| {
                std::fs::read_to_string("/proc/sys/kernel/osrelease")
                    .map(|text| text.trim().to_string())
                    .unwrap_or_else(|_| "unknown".to_string())
            })
            .clone();
        let (opened, failed) = self.modules.counts();
        let issues = self.modules.issues();
        let unnamed: u64 = issues.iter().map(|issue| issue.count).sum();
        if !issues.is_empty() || failed > 0 {
            tracing::info!(
                arch = std::env::consts::ARCH,
                kernel = %kernel,
                named = self.tally.resolved,
                truncated = self.tally.truncated,
                go = self.tally.go,
                elfless = self.tally.module,
                unnamed,
                binaries = issues.len(),
                parsed = opened,
                unreadable = failed,
                "symbol coverage"
            );
        }
        for issue in self.modules.take_new_issues(MAX_LINES) {
            if issue.path == "[unmapped]" {
                tracing::warn!(
                    frames = issue.count,
                    "addresses in no file-backed mapping stay unnamed (a broken frame-pointer \
                     chain: the walk read a string or a constant as a return address)"
                );
                log_samples(&issue);
                continue;
            }
            if issue.path == "[anonymous]" {
                tracing::warn!(
                    frames = issue.count,
                    "addresses in anonymous memory stay unnamed (JIT code with no perf map: \
                     V8, the JVM, and anything else that emits machine code at run time)"
                );
                log_samples(&issue);
                continue;
            }
            let cause = if issue.unreadable {
                "the file could not be opened through any route"
            } else if issue.symbols == 0 && !issue.has_go {
                "no ELF symbol table and no Go function table"
            } else if issue.has_go {
                "the address is outside the Go function table"
            } else {
                "the address is outside every ELF symbol"
            };
            tracing::warn!(
                path = %issue.path,
                go = issue.go_version.as_deref().unwrap_or("-"),
                route = issue.route,
                symbols = issue.symbols,
                go_table = issue.has_go,
                frames = issue.count,
                cause,
                "frames from this binary stay unnamed"
            );
            log_samples(&issue);
        }
    }

    /// Warn once when the kernel's own symbol table is not readable.
    ///
    /// `kptr_restrict=2`, a missing `CAP_SYSLOG` or a masked `/proc/kallsyms`
    /// all leave the kernel half of every stack as an address, and nothing else
    /// in the log would hint at it.
    fn check_kernel_symbols(&mut self) {
        if self.kernel_symbols_checked {
            return;
        }
        self.kernel_symbols_checked = true;
        use std::io::Read as _;
        let mut head = vec![0u8; 4096];
        let read = std::fs::File::open("/proc/kallsyms")
            .and_then(|mut file| file.read(&mut head))
            .unwrap_or(0);
        let text = String::from_utf8_lossy(&head[..read]);
        let lines: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
        if lines.is_empty() {
            tracing::warn!(
                "`/proc/kallsyms` is empty or unreadable: kernel frames will show addresses \
                 (check kptr_restrict and CAP_SYSLOG)"
            );
            return;
        }
        if lines
            .iter()
            .take(8)
            .all(|line| line.starts_with("0000000000000000"))
        {
            tracing::warn!(
                "`/proc/kallsyms` reports zeroed addresses: kernel symbols are hidden \
                 (kptr_restrict=2 or a dropped CAP_SYSLOG), so kernel frames stay unnamed"
            );
        }
    }

    /// Total number of addresses that could not be resolved.
    pub(crate) fn failed_lookups(&self) -> u64 {
        self.tally.failed + self.tally.unknown.total()
    }

    /// Cumulative tally, for publication through the collector stats.
    pub(crate) fn tally(&self) -> Tally {
        self.tally
    }

    /// `(binaries parsed, mappings whose file could not be opened)`.
    ///
    /// The second number is the diagnostic that matters on a node where every
    /// frame is `[unknown]`: it separates "the agent cannot see your files"
    /// from "your binaries really have no symbols".
    pub(crate) fn module_counts(&self) -> (u64, u64) {
        self.modules.counts()
    }

    /// Drop everything cached for processes that are no longer in the batch.
    ///
    /// Two things go wrong without this. The cache grows without bound on a
    /// machine with high process churn, and — worse — a recycled PID inherits
    /// the frames of its predecessor, so a brand new process is reported with
    /// the previous process' symbols. Since a pid only leaves the batch once it
    /// is gone, anything still alive keeps its cache and stays cheap.
    pub(crate) fn retain_processes(&mut self, alive: &[u32]) {
        let alive: std::collections::HashSet<u32> = alive.iter().copied().collect();
        let mut freed = 0usize;
        self.user_cache.retain(|(tgid, _), frames| {
            if alive.contains(tgid) {
                true
            } else {
                freed += frames_bytes(frames);
                false
            }
        });
        self.user_bytes = self.user_bytes.saturating_sub(freed);
        // The fallbacks keep one map per pid too. Without this they hold the
        // `/proc/<pid>/maps` of every process that ever appeared, long after
        // the process is gone and its pid has been reused.
        self.modules.retain_processes(&alive);
        self.perf_map.retain_processes(&alive);
    }

    /// Record that `tgid` is being symbolized, and rebase the caches when too
    /// many processes have passed through since the last time.
    fn note_process(&mut self, tgid: u32) {
        if !self.processes.insert(tgid) {
            return;
        }
        if self.processes.len() > MAX_PROCESSES {
            self.reset();
        }
    }

    /// Drop every cache, including the ones inside blazesym and the fallbacks.
    fn reset(&mut self) {
        tracing::debug!(
            processes = self.processes.len(),
            "rebasing symbol caches after too many processes"
        );
        self.symbolizer = Symbolizer::new();
        self.modules = ModuleCache::new();
        self.perf_map = PerfMapFallback::new();
        self.clear_caches();
        self.processes.clear();
        // glibc keeps freed arena pages in RSS. Without a trim the memory
        // saved by the rebase is invisible to the cgroup limit, and the agent
        // creeps back to OOM even though the caches are empty.
        #[cfg(target_os = "linux")]
        unsafe {
            libc::malloc_trim(0);
        }
    }

    fn clear_caches(&mut self) {
        self.user_cache.clear();
        self.kernel_cache.clear();
        self.user_bytes = 0;
        self.kernel_bytes = 0;
    }

    fn maybe_prune(&mut self) {
        if self.user_cache.len() + self.kernel_cache.len() > MAX_CACHE_ENTRIES
            || self.user_bytes + self.kernel_bytes > MAX_CACHE_BYTES
        {
            self.clear_caches();
        }
    }

    /// Resolve a user space stack of `ips` (leaf first) for process `tgid`.
    pub(crate) fn resolve_user(&mut self, tgid: u32, ips: &[u64]) -> Vec<Frame> {
        if ips.is_empty() {
            return Vec::new();
        }
        // Where the walk stops. A value in no mapping at all, or in one that is
        // not executable, was never an instruction pointer: the frame-pointer
        // chain broke and the unwinder is reading the stack itself (strings,
        // small integers, a saved value). Everything after it is the same
        // garbage, so the stack ends here — `perf` does the same.
        //
        // This runs before symbolization: the frames beyond the break are never
        // stored and never counted as missing symbols, because no symbolizer
        // could have named them. The check uses the original addresses rather
        // than the `ip - 1` probes, since a probe is the call instruction and
        // can fall just outside a mapping whose first byte is the return
        // address.
        let cut = ips
            .iter()
            .position(|ip| self.modules.could_be_code(tgid, *ip) == Some(false));
        let keep = cut.unwrap_or(ips.len());
        if let Some(cut) = cut {
            self.tally.truncated += (ips.len() - cut) as u64;
        }

        let probes = probe_addrs(&ips[..keep]);
        let missing: Vec<u64> = probes
            .iter()
            .filter(|probe| !self.user_cache.contains_key(&(tgid, **probe)))
            .copied()
            .collect();

        if !missing.is_empty() {
            let resolved = self.symbolize_user(tgid, &missing);
            for (probe, frames) in missing.into_iter().zip(resolved) {
                self.user_bytes += frames_bytes(&frames);
                self.user_cache.insert((tgid, probe), frames);
            }
        }

        self.maybe_prune();

        let mut out = Vec::with_capacity(probes.len());
        for probe in &probes {
            if let Some(frames) = self.user_cache.get(&(tgid, *probe)) {
                out.extend(frames.iter().cloned());
            }
        }
        out
    }

    /// Symbolize `addrs` for `tgid`, never failing: unresolved addresses become
    /// `[unknown]` frames so that a stack keeps its shape.
    fn symbolize_user(&mut self, tgid: u32, addrs: &[u64]) -> Vec<Vec<Frame>> {
        self.note_process(tgid);
        loop {
            let mut process = Process::new(Pid::from(tgid));
            process.map_files = self.map_files;
            let src = Source::Process(process);
            match self.symbolizer.symbolize(&src, Input::AbsAddr(addrs)) {
                Ok(symbols) => {
                    // Take ownership so the immutable borrow on `self.symbolizer`
                    // ends here: the Go fallback below needs `&mut self`.
                    let symbols: Vec<Symbolized<'static>> =
                        symbols.into_iter().map(into_owned).collect();
                    let mut tally = Tally::default();
                    let mut out: Vec<Vec<Frame>> = Vec::with_capacity(addrs.len());
                    for (addr, sym) in addrs.iter().zip(symbols.iter()) {
                        out.push(self.frames_for(tgid, *addr, Some(sym), &mut tally));
                    }
                    // A short result is not an error; keep the stack's shape.
                    for addr in addrs.iter().skip(symbols.len()) {
                        out.push(self.frames_for(tgid, *addr, None, &mut tally));
                    }
                    self.tally.merge(tally);
                    return out;
                }
                Err(err) => {
                    if self.map_files {
                        self.map_files = false;
                        tracing::debug!(
                            error = %err,
                            "map_files unusable, falling back to /proc/<pid>/maps paths"
                        );
                        continue;
                    }
                    // `symbolize` is all-or-nothing: one address whose file
                    // cannot be stat'ed fails the whole batch. That is the
                    // normal case for a container's binary — the path exists in
                    // the target's mount namespace and not in the agent's — so
                    // a failed batch must not turn a stack into plain
                    // addresses. Each address is retried on its own, which
                    // isolates the one that poisons the batch and keeps
                    // blazesym's inline information for the rest; whatever
                    // still fails goes through the module cache, the only
                    // source that opens a file in another namespace.
                    tracing::debug!(
                        error = %err,
                        addresses = addrs.len(),
                        "batch symbolization failed, retrying per address"
                    );
                    return self.salvage(tgid, addrs);
                }
            }
        }
    }

    /// Frames for one address: whatever `blazesym` said, plus the sources it
    /// cannot reach.
    ///
    /// `sym` is `None` when the batch answered for fewer addresses than it was
    /// given — a short result, not an error, which still deserves a name from
    /// the module cache rather than a bare address.
    fn frames_for(
        &mut self,
        tgid: u32,
        addr: u64,
        sym: Option<&Symbolized<'static>>,
        tally: &mut Tally,
    ) -> Vec<Frame> {
        match sym {
            Some(sym @ Symbolized::Sym(_)) => frames_from(sym, addr, false, tally),
            Some(Symbolized::Unknown(reason)) => match self.fallback_frame(tgid, addr, tally) {
                Some(frame) => vec![frame],
                None => {
                    tally.unknown.record(reason);
                    vec![unknown_frame(addr, &self.module_name(tgid, addr))]
                }
            },
            None => match self.fallback_frame(tgid, addr, tally) {
                Some(frame) => vec![frame],
                None => {
                    tally.unknown.record(&Reason::IgnoredError);
                    vec![unknown_frame(addr, &self.module_name(tgid, addr))]
                }
            },
        }
    }

    /// Name what can still be named after a batch failure.
    ///
    /// The addresses are asked one at a time so that a single unreadable file
    /// does not condemn the rest of the stack; each answer then runs through
    /// [`Self::frames_for`], which adds the module cache and the JIT perf map.
    fn salvage(&mut self, tgid: u32, addrs: &[u64]) -> Vec<Vec<Frame>> {
        let mut tally = Tally::default();
        let mut out: Vec<Vec<Frame>> = Vec::with_capacity(addrs.len());
        for addr in addrs {
            let single = [*addr];
            let mut process = Process::new(Pid::from(tgid));
            process.map_files = self.map_files;
            let one = self
                .symbolizer
                .symbolize(&Source::Process(process), Input::AbsAddr(&single))
                .ok()
                .and_then(|symbols| symbols.into_iter().next())
                .map(into_owned);
            out.push(self.frames_for(tgid, *addr, one.as_ref(), &mut tally));
        }
        self.tally.merge(tally);
        out
    }

    /// The file an address belongs to, for an `[unknown]` frame that still says
    /// where it came from: "libc has no debug symbols" reads very differently
    /// from "some address".
    fn module_name(&mut self, tgid: u32, addr: u64) -> String {
        self.modules
            .module_for(tgid, addr)
            .unwrap_or_else(|| "[unknown]".to_string())
    }

    /// Try the symbol sources `blazesym` gave up on.
    ///
    /// The module cache comes first: it is the only source that can open a file
    /// living in another mount namespace, and it covers both cases that matter
    /// in practice — a stripped Go binary (function table) and an ordinary
    /// binary whose ELF symbols `blazesym` could not reach. A JIT region is
    /// never inside a mapped file, and a Go binary never publishes a perf map,
    /// so the sources are disjoint and the order only affects cost.
    fn fallback_frame(&mut self, tgid: u32, addr: u64, tally: &mut Tally) -> Option<Frame> {
        if let Some(resolved) = self.modules.resolve(tgid, addr) {
            tally.resolved += 1;
            match resolved.source {
                ModuleSource::Go => tally.go += 1,
                ModuleSource::Symbols => tally.module += 1,
            }
            let mut frame = Frame::new(resolved.module, resolved.name);
            frame.addr = addr;
            return Some(frame);
        }
        if let Some(name) = self.perf_map.resolve(tgid, addr) {
            tally.resolved += 1;
            tally.jit += 1;
            let mut frame = Frame::new("[jit]", name);
            frame.addr = addr;
            return Some(frame);
        }
        None
    }

    /// Resolve a kernel stack of `ips` (leaf first).
    pub(crate) fn resolve_kernel(&mut self, ips: &[u64]) -> Vec<Frame> {
        if ips.is_empty() {
            return Vec::new();
        }
        let probes = probe_addrs(ips);
        let missing: Vec<u64> = probes
            .iter()
            .filter(|probe| !self.kernel_cache.contains_key(*probe))
            .copied()
            .collect();

        if !missing.is_empty() {
            let resolved: Vec<Vec<Frame>> = {
                let src = Source::Kernel(Kernel::default());
                match self.symbolizer.symbolize(&src, Input::AbsAddr(&missing)) {
                    Ok(symbols) => {
                        let mut tally = Tally::default();
                        let mut out = Vec::with_capacity(missing.len());
                        for (index, addr) in missing.iter().enumerate() {
                            match symbols.get(index) {
                                Some(sym) => out.push(frames_from(sym, *addr, true, &mut tally)),
                                None => {
                                    tally.unknown.record(&Reason::IgnoredError);
                                    out.push(vec![unknown_kernel_frame(*addr)]);
                                }
                            }
                        }
                        self.tally.merge(tally);
                        out
                    }
                    Err(_) => {
                        self.tally.failed += missing.len() as u64;
                        missing
                            .iter()
                            .map(|addr| vec![unknown_kernel_frame(*addr)])
                            .collect()
                    }
                }
            };
            for (probe, frames) in missing.into_iter().zip(resolved) {
                self.kernel_bytes += frames_bytes(&frames);
                self.kernel_cache.insert(probe, frames);
            }
        }

        self.maybe_prune();
        let mut out = Vec::with_capacity(ips.len());
        for probe in probes {
            if let Some(frames) = self.kernel_cache.get(&probe) {
                out.extend(frames.iter().cloned());
            }
        }
        out
    }
}

/// One line per sample: pid, process name, the instruction pointer, and exactly
/// what `/proc/<pid>/maps` said about it.
///
/// This is the difference between "some frames are unnamed" and a report
/// somebody can reproduce: the pid and ip can be checked on the node, and the
/// mapping line says whether the file was found, and which one it was.
fn log_samples(issue: &crate::module::ModuleIssue) {
    for sample in &issue.samples {
        tracing::info!(
            path = %sample.path,
            pid = sample.pid,
            comm = %sample.comm,
            ip = format_args!("{:x}", sample.ip),
            perms = %String::from_utf8_lossy(&sample.perms),
            map = format_args!("{:x}-{:x}+{:x}", sample.start, sample.end, sample.offset),
            dev = format_args!("{:x}:{:x}", sample.dev >> 32, sample.dev & 0xffff_ffff),
            inode = sample.inode,
            vaddr = sample
                .vaddr
                .map(|vaddr| format!("{vaddr:x}"))
                .unwrap_or_else(|| "-".to_string()),
            cause = sample.cause,
            "unnamed frame sample"
        );
    }
}

/// Turn a stack of instruction pointers into the addresses that should be
/// looked up.
///
/// The innermost frame is an instruction that really executed, so it is used
/// verbatim. Everything below it is a return address — the address of the
/// instruction *after* the `call` — and must be decremented before lookup, or
/// a call at the very end of a function resolves to the next function. This is
/// the same convention `perf` and `addr2line` use.
fn probe_addrs(ips: &[u64]) -> Vec<u64> {
    ips.iter()
        .enumerate()
        .map(|(index, ip)| {
            if index == 0 {
                *ip
            } else {
                ip.saturating_sub(1)
            }
        })
        .collect()
}

/// Turn a borrowed symbolization result into an owned one, so the borrow on the
/// [`Symbolizer`] can be released before the caches are consulted.
fn into_owned(symbol: Symbolized<'_>) -> Symbolized<'static> {
    match symbol {
        Symbolized::Sym(sym) => Symbolized::Sym(sym.into_owned()),
        Symbolized::Unknown(reason) => Symbolized::Unknown(reason),
    }
}

/// Build every frame one address stands for, leaf first.
///
/// `blazesym` reports a physical symbol plus the inlined functions nested
/// inside it, ordered outside → inside. A profiler needs the opposite order
/// (leaf first) and needs the code locations shifted accordingly: the leaf
/// carries the location inside the innermost inlined function, and each outer
/// frame carries the location of the *call site* that led into the frame below
/// it.
fn frames_from(sym: &Symbolized<'_>, ip: u64, kernel: bool, tally: &mut Tally) -> Vec<Frame> {
    let Symbolized::Sym(sym) = sym else {
        if let Symbolized::Unknown(reason) = sym {
            tally.unknown.record(reason);
        }
        return vec![if kernel {
            unknown_kernel_frame(ip)
        } else {
            unknown_frame(ip, "[unknown]")
        }];
    };

    let module = sym
        .module
        .as_ref()
        .map(|m| m.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            if kernel {
                "[kernel]".to_string()
            } else {
                "[unknown]".to_string()
            }
        });

    let inline = &sym.inlined;
    let mut frames = Vec::with_capacity(inline.len() + 1);
    match inline.len() {
        0 => frames.push(build_frame(
            &module,
            &sym.name,
            sym.code_info.as_deref(),
            ip,
        )),
        n => {
            // Innermost inlined function: this is the code that actually ran.
            frames.push(build_frame(
                &module,
                &inline[n - 1].name,
                sym.code_info.as_deref(),
                ip,
            ));
            // Walk outwards; each frame is located at the call site inside the
            // frame one level further out.
            for k in 1..n {
                frames.push(build_frame(
                    &module,
                    &inline[n - 1 - k].name,
                    inline[n - k].code_info.as_ref(),
                    ip,
                ));
            }
            // The physical symbol that owns the address.
            frames.push(build_frame(
                &module,
                &sym.name,
                inline[0].code_info.as_ref(),
                ip,
            ));
        }
    }

    tally.resolved += frames.len() as u64;
    tally.inlined += inline.len() as u64;
    frames
}

fn build_frame(module: &str, symbol: &str, code_info: Option<&CodeInfo<'_>>, ip: u64) -> Frame {
    let (file, line) = match code_info {
        Some(info) => (
            info.file.to_string_lossy().into_owned(),
            info.line.unwrap_or(0),
        ),
        None => (String::new(), 0),
    };
    let mut frame = Frame::new(module.to_string(), symbol.to_string());
    frame.file = file;
    frame.line = line;
    frame.addr = ip;
    frame
}

fn unknown_frame(addr: u64, module: &str) -> Frame {
    let mut frame = Frame::new(module, format!("0x{addr:x}"));
    frame.addr = addr;
    frame
}

fn unknown_kernel_frame(addr: u64) -> Frame {
    let mut frame = Frame::new("[kernel]", format!("0x{addr:x}"));
    frame.addr = addr;
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_frames_keep_the_address() {
        let f = unknown_frame(0x4011a0, "/usr/lib/libfoo.so");
        assert_eq!(f.symbol, "0x4011a0");
        assert_eq!(f.module, "/usr/lib/libfoo.so");
        assert_eq!(f.addr, 0x4011a0);
    }

    #[test]
    fn only_return_addresses_are_decremented() {
        // 0 is a legitimate "no address" marker and must not wrap to u64::MAX.
        assert_eq!(probe_addrs(&[0x1000, 0x2000, 0]), vec![0x1000, 0x1fff, 0]);
    }

    #[test]
    fn unknown_reasons_add_up() {
        let mut reasons = UnknownReasons::default();
        reasons.record(&Reason::MissingSyms);
        reasons.record(&Reason::MissingSyms);
        reasons.record(&Reason::MissingComponent);
        assert_eq!(reasons.total(), 3);
        assert_eq!(reasons.missing_syms, 2);
        assert_eq!(reasons.missing_component, 1);
        let entries = reasons.entries();
        assert_eq!(entries.iter().map(|(_, n)| n).sum::<u64>(), 3);
    }

    #[inline(never)]
    fn symbolization_marker(x: u64) -> u64 {
        std::hint::black_box(x).wrapping_mul(3)
    }

    #[test]
    fn symbolizes_the_test_process() {
        let mut r = FrameResolver::new();
        let addr = symbolization_marker as *const () as u64;
        let frames = r.resolve_user(std::process::id(), &[addr]);
        assert_eq!(frames.len(), 1, "a leaf address is one frame: {frames:?}");
        assert!(
            frames[0].symbol.contains("symbolization_marker"),
            "expected a real symbol, got {:?}",
            frames[0]
        );
    }

    #[test]
    fn resolver_handles_dead_processes() {
        let mut r = FrameResolver::new();
        // PID 0 is not a process we can read; resolution must not panic.
        let frames = r.resolve_user(0, &[0x1000, 0x2000]);
        assert_eq!(frames.len(), 2);
    }

    /// A failed batch must not turn a whole stack into bare addresses.
    ///
    /// `symbolize` is all-or-nothing, and the usual cause of a batch failure is
    /// a file the agent cannot `stat` — a container's binary that exists in the
    /// target's mount namespace and not in the agent's. This is the entry point
    /// that failure goes through: every address gets its own attempt, and none
    /// of them may be dropped.
    #[test]
    fn salvage_keeps_one_frame_per_address() {
        let mut r = FrameResolver::new();
        // PID 0 has no readable maps, so nothing can be named.
        let addrs = [0x1000_u64, 0x2000, 0x3000];
        let stacks = r.salvage(0, &addrs);
        assert_eq!(stacks.len(), addrs.len(), "no address may be dropped");
        for (frames, addr) in stacks.iter().zip(addrs) {
            assert_eq!(frames.len(), 1, "one address, one frame: {frames:?}");
            assert_eq!(frames[0].addr, addr, "the address has to survive");
        }
        assert!(
            r.failed_lookups() >= addrs.len() as u64,
            "unnamed addresses still count as failed lookups"
        );
    }

    /// The walk stops where it leaves the address space.
    ///
    /// A broken frame-pointer chain hands the unwinder values that were never
    /// instruction pointers — a string, a small integer, a saved register. The
    /// first such value ends the stack: the frames after it are the same
    /// garbage, and storing them is what turns a flamegraph into a wall of
    /// addresses that no symbolizer can ever name.
    #[test]
    fn a_broken_walk_stops_at_the_first_unmapped_address() {
        let mut r = FrameResolver::new();
        let marker = symbolization_marker as *const () as u64;
        let frames = r.resolve_user(std::process::id(), &[marker, 0x1, marker]);
        assert_eq!(
            frames.len(),
            1,
            "the stack ends where the walk left the process: {frames:?}"
        );
        assert!(frames[0].symbol.contains("symbolization_marker"));
        assert_eq!(r.tally.truncated, 2, "both lost frames are counted");
    }

    /// Anonymous memory is where JIT code lives, and it is not a broken walk.
    #[test]
    fn anonymous_code_does_not_end_the_stack() {
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED, "mmap failed");

        let mut r = FrameResolver::new();
        let marker = symbolization_marker as *const () as u64;
        let frames = r.resolve_user(std::process::id(), &[map as u64, marker]);
        unsafe { libc::munmap(map, 4096) };

        assert_eq!(frames.len(), 2, "a JIT frame is a real frame: {frames:?}");
        assert_eq!(r.tally.truncated, 0, "nothing was discarded");
    }

    /// A value in a non-executable mapping is a value, not an instruction.
    ///
    /// This is the other half of a broken walk: the address is inside the
    /// process, so a plain "is it mapped" check would accept it, but nothing
    /// ever executed there.
    #[test]
    fn a_data_address_ends_the_stack() {
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED, "mmap failed");

        let mut r = FrameResolver::new();
        let marker = symbolization_marker as *const () as u64;
        let frames = r.resolve_user(std::process::id(), &[marker, map as u64, marker]);
        unsafe { libc::munmap(map, 4096) };

        assert_eq!(
            frames.len(),
            1,
            "a writable mapping cannot be a caller: {frames:?}"
        );
        assert_eq!(r.tally.truncated, 2);
    }

    /// The same through the public entry point: a stack of unreadable
    /// addresses keeps its shape instead of collapsing to one frame.
    #[test]
    fn an_unresolvable_stack_keeps_its_shape() {
        let mut r = FrameResolver::new();
        let addrs = [0x1000_u64, 0x2000, 0x3000];
        let frames = r.resolve_user(0, &addrs);
        assert_eq!(frames.len(), addrs.len(), "no frame may be dropped");
        // Interior frames are looked up at `ip - 1`, the call instruction, and
        // the frame carries the address it was probed at.
        let probes: Vec<u64> = addrs
            .iter()
            .enumerate()
            .map(|(i, ip)| if i == 0 { *ip } else { ip - 1 })
            .collect();
        for (frame, probe) in frames.iter().zip(probes) {
            assert_eq!(frame.addr, probe, "the address has to survive: {frame:?}");
            assert_eq!(frame.module, "[unknown]", "nothing can be named: {frame:?}");
        }
    }

    /// An unreadable sibling must not cost the rest of the batch its names.
    ///
    /// A mapping whose file was deleted looks exactly like a container binary
    /// from the agent's mount namespace: `/proc/<pid>/maps` names a path that
    /// `stat` rejects. `symbolize` fails the *whole* batch on the first such
    /// address, which used to turn every frame of the stack into an address;
    /// the retry walks the addresses one at a time so the healthy ones keep
    /// their names.
    #[test]
    fn an_unreadable_mapping_does_not_sink_the_batch() {
        use std::io::Write as _;

        let dir = std::env::temp_dir().join(format!("lidlesseye-poison-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("gone.bin");
        // Read+write: `mmap` with PROT_READ is refused on a write-only fd.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("create");
        file.write_all(&[0x90u8; 4096]).expect("write");
        file.sync_all().expect("sync");

        // Map it, then unlink: the mapping stays, the path stops resolving.
        let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_EXEC,
                libc::MAP_PRIVATE,
                fd,
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED, "mmap failed");
        drop(file);
        std::fs::remove_file(&path).expect("unlink");
        let poison = map as u64;
        let good = symbolization_marker as *const () as u64;

        let mut r = FrameResolver::new();
        let frames = r.resolve_user(std::process::id(), &[good, poison]);
        assert_eq!(frames.len(), 2, "both addresses keep a frame");

        unsafe { libc::munmap(map, 4096) };
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            frames[0].symbol.contains("symbolization_marker"),
            "the nameable address lost its name: {:?}",
            frames[0]
        );
    }

    /// An address `blazesym` had no answer for goes through the module cache.
    ///
    /// This is the fallback a failed batch and a short batch result both rely
    /// on: the module cache opens the file through `/proc/<pid>/root`, which is
    /// how a container's binary gets named at all.
    #[test]
    fn a_missing_answer_falls_back_to_the_module() {
        let mut r = FrameResolver::new();
        let addr = symbolization_marker as *const () as u64;
        let mut tally = Tally::default();
        let frames = r.frames_for(std::process::id(), addr, None, &mut tally);
        assert_eq!(frames.len(), 1);
        assert_ne!(
            frames[0].module, "[unknown]",
            "the module cache should name the file: {frames:?}"
        );
    }

    #[test]
    fn rebases_the_caches_after_too_many_processes() {
        let mut r = FrameResolver::new();
        r.user_cache.insert((1, 0x1000), Vec::new());
        for tgid in 1..=MAX_PROCESSES as u32 {
            r.note_process(tgid);
        }
        assert!(!r.user_cache.is_empty(), "cap reached exactly: keep going");

        // One process past the cap. blazesym cannot evict the symbol tables of
        // the processes that are already gone, so the resolver throws its own
        // caches away rather than growing until the OOM killer notices.
        r.note_process(MAX_PROCESSES as u32 + 1);
        assert!(r.user_cache.is_empty());
        assert!(r.kernel_cache.is_empty());
        assert!(r.processes.is_empty());
    }

    #[test]
    fn prunes_the_cache_when_its_byte_budget_is_spent() {
        let mut r = FrameResolver::new();
        r.user_cache.insert(
            (1, 0x1000),
            vec![Frame::new("x".repeat(64), "y".repeat(64))],
        );
        r.user_bytes = MAX_CACHE_BYTES + 1;
        r.maybe_prune();
        assert!(r.user_cache.is_empty());
        assert_eq!(r.user_bytes, 0);
        assert_eq!(
            frames_bytes(&[Frame::new("abcd", "efgh")]),
            std::mem::size_of::<Frame>() + 8
        );
    }

    #[test]
    fn evicting_dead_processes_credits_the_byte_budget() {
        let mut r = FrameResolver::new();
        let frames = vec![Frame::new("mod", "sym")];
        let bytes = frames_bytes(&frames);
        r.user_cache.insert((1, 0x1000), frames.clone());
        r.user_cache.insert((2, 0x2000), frames);
        r.user_bytes = 2 * bytes;
        r.retain_processes(&[1]);
        assert_eq!(r.user_cache.len(), 1);
        assert_eq!(r.user_bytes, bytes);
    }
}
