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

use blazesym::Pid;
use blazesym::symbolize::Reason;
use blazesym::symbolize::source::{Kernel, Process, Source};
use blazesym::symbolize::{CodeInfo, Input, Symbolized, Symbolizer};
use lidlesseye_core::Frame;

use crate::UnknownReasons;
use crate::gopclntab::GoFallback;
use crate::perf_map::PerfMapFallback;

/// Hard cap on the symbol cache; when exceeded the cache is dropped wholesale
/// (a process restart or a container recycle makes old entries useless anyway).
const MAX_CACHE_ENTRIES: usize = 200_000;

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
    /// Frames named by a JIT runtime's perf map.
    pub jit: u64,
    /// Addresses that came back as `Symbolized::Unknown`.
    pub unknown: UnknownReasons,
    /// Batches that failed outright (process gone, unreadable maps, ...).
    pub failed: u64,
}

impl Tally {
    fn merge(&mut self, other: Tally) {
        self.resolved += other.resolved;
        self.inlined += other.inlined;
        self.go += other.go;
        self.jit += other.jit;
        self.failed += other.failed;
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
    /// Last resort for binaries without any ELF symbols. See [`crate::gopclntab`].
    go: GoFallback,
    /// Last resort for code that exists in no file at all. See
    /// [`crate::perf_map`].
    perf_map: PerfMapFallback,
    tally: Tally,
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
            go: GoFallback::new(),
            perf_map: PerfMapFallback::new(),
            tally: Tally::default(),
            map_files: true,
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

    /// Drop everything cached for processes that are no longer in the batch.
    ///
    /// Two things go wrong without this. The cache grows without bound on a
    /// machine with high process churn, and — worse — a recycled PID inherits
    /// the frames of its predecessor, so a brand new process is reported with
    /// the previous process' symbols. Since a pid only leaves the batch once it
    /// is gone, anything still alive keeps its cache and stays cheap.
    pub(crate) fn retain_processes(&mut self, alive: &[u32]) {
        let alive: std::collections::HashSet<u32> = alive.iter().copied().collect();
        self.user_cache.retain(|(tgid, _), _| alive.contains(tgid));
    }

    fn maybe_prune(&mut self) {
        if self.user_cache.len() + self.kernel_cache.len() > MAX_CACHE_ENTRIES {
            self.user_cache.clear();
            self.kernel_cache.clear();
        }
    }

    /// Resolve a user space stack of `ips` (leaf first) for process `tgid`.
    pub(crate) fn resolve_user(&mut self, tgid: u32, ips: &[u64]) -> Vec<Frame> {
        if ips.is_empty() {
            return Vec::new();
        }
        let probes = probe_addrs(ips);
        let missing: Vec<u64> = probes
            .iter()
            .filter(|probe| !self.user_cache.contains_key(&(tgid, **probe)))
            .copied()
            .collect();

        if !missing.is_empty() {
            let resolved = self.symbolize_user(tgid, &missing);
            for (probe, frames) in missing.into_iter().zip(resolved) {
                self.user_cache.insert((tgid, probe), frames);
            }
        }

        self.maybe_prune();
        let mut out = Vec::with_capacity(ips.len());
        for probe in probes {
            if let Some(frames) = self.user_cache.get(&(tgid, probe)) {
                out.extend(frames.iter().cloned());
            }
        }
        out
    }

    /// Symbolize `addrs` for `tgid`, never failing: unresolved addresses become
    /// `[unknown]` frames so that a stack keeps its shape.
    fn symbolize_user(&mut self, tgid: u32, addrs: &[u64]) -> Vec<Vec<Frame>> {
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
                        let frames = match sym {
                            Symbolized::Sym(_) => frames_from(sym, *addr, false, &mut tally),
                            Symbolized::Unknown(reason) => {
                                match self.fallback_frame(tgid, *addr, &mut tally) {
                                    Some(frame) => vec![frame],
                                    None => {
                                        tally.unknown.record(reason);
                                        // Naming the file that has no symbols
                                        // is the difference between "some
                                        // addresses" and "libc has no debug
                                        // info installed".
                                        let module = self
                                            .go
                                            .module_for(tgid, *addr)
                                            .unwrap_or_else(|| "[unknown]".to_string());
                                        vec![unknown_frame(*addr, &module)]
                                    }
                                }
                            }
                        };
                        out.push(frames);
                    }
                    // A short result is not an error; keep the stack's shape.
                    for addr in addrs.iter().skip(symbols.len()) {
                        tally.unknown.record(&Reason::IgnoredError);
                        let module = self
                            .go
                            .module_for(tgid, *addr)
                            .unwrap_or_else(|| "[unknown]".to_string());
                        out.push(vec![unknown_frame(*addr, &module)]);
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
                    tracing::debug!(error = %err, "symbolization failed");
                    self.tally.failed += addrs.len() as u64;
                    return addrs
                        .iter()
                        .map(|addr| vec![unknown_frame(*addr, "[unknown]")])
                        .collect();
                }
            }
        }
    }

    /// Try the symbol sources that do not need an ELF symbol table.
    ///
    /// Order matters only for cost: a Go binary never publishes a perf map, and
    /// a JIT region is never inside a Go function table, so the two are
    /// disjoint. The Go lookup is tried first because its table is parsed once
    /// and cached, while the perf map is re-read as the runtime compiles more
    /// code.
    fn fallback_frame(&mut self, tgid: u32, addr: u64, tally: &mut Tally) -> Option<Frame> {
        if let Some((module, name)) = self.go.resolve(tgid, addr) {
            tally.resolved += 1;
            tally.go += 1;
            let mut frame = Frame::new(module, name);
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
}
