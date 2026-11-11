//! Address → symbol resolution, with per-address caching.
//!
//! Symbolization happens in the agent, before the report is shipped. That is a
//! deliberate choice: the storage server then never needs access to the
//! profiled machine's file system, and profiles of processes that have already
//! exited remain readable. It is the same trade-off Pyroscope's eBPF agent
//! makes.

use std::collections::HashMap;

use blazesym::Pid;
use blazesym::symbolize::source::{Kernel, Process, Source};
use blazesym::symbolize::{Input, Symbolized, Symbolizer};
use lidlesseye_core::Frame;

/// Hard cap on the symbol cache; when exceeded the cache is dropped wholesale
/// (a process restart or a container recycle makes old entries useless anyway).
const MAX_CACHE_ENTRIES: usize = 200_000;

pub(crate) struct FrameResolver {
    symbolizer: Symbolizer,
    user_cache: HashMap<(u32, u64), Frame>,
    kernel_cache: HashMap<u64, Frame>,
    failed: u64,
}

impl FrameResolver {
    pub(crate) fn new() -> Self {
        FrameResolver {
            symbolizer: Symbolizer::new(),
            user_cache: HashMap::new(),
            kernel_cache: HashMap::new(),
            failed: 0,
        }
    }

    pub(crate) fn failed_lookups(&self) -> u64 {
        self.failed
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
        let mut missing: Vec<u64> = Vec::new();
        for ip in ips {
            if !self.user_cache.contains_key(&(tgid, *ip)) {
                missing.push(*ip);
            }
        }

        if !missing.is_empty() {
            let resolved: Vec<Frame> = {
                let src = Source::Process(Process::new(Pid::from(tgid)));
                match self.symbolizer.symbolize(&src, Input::AbsAddr(&missing)) {
                    Ok(symbols) => {
                        let mut out = Vec::with_capacity(missing.len());
                        for (ip, sym) in missing.iter().zip(symbols.iter()) {
                            out.push(frame_from(sym, *ip, false));
                        }
                        while out.len() < missing.len() {
                            let ip = missing[out.len()];
                            out.push(unknown_frame(ip));
                        }
                        out
                    }
                    Err(_) => {
                        self.failed += missing.len() as u64;
                        missing.iter().map(|ip| unknown_frame(*ip)).collect()
                    }
                }
            };
            for (ip, frame) in missing.into_iter().zip(resolved) {
                self.user_cache.insert((tgid, ip), frame);
            }
        }

        self.maybe_prune();
        ips.iter()
            .filter_map(|ip| self.user_cache.get(&(tgid, *ip)).cloned())
            .collect()
    }

    /// Resolve a kernel stack of `ips` (leaf first).
    pub(crate) fn resolve_kernel(&mut self, ips: &[u64]) -> Vec<Frame> {
        if ips.is_empty() {
            return Vec::new();
        }
        let mut missing: Vec<u64> = Vec::new();
        for ip in ips {
            if !self.kernel_cache.contains_key(ip) {
                missing.push(*ip);
            }
        }

        if !missing.is_empty() {
            let resolved: Vec<Frame> = {
                let src = Source::Kernel(Kernel::default());
                match self.symbolizer.symbolize(&src, Input::AbsAddr(&missing)) {
                    Ok(symbols) => {
                        let mut out = Vec::with_capacity(missing.len());
                        for (ip, sym) in missing.iter().zip(symbols.iter()) {
                            out.push(frame_from(sym, *ip, true));
                        }
                        while out.len() < missing.len() {
                            let ip = missing[out.len()];
                            out.push(unknown_kernel_frame(ip));
                        }
                        out
                    }
                    Err(_) => {
                        self.failed += missing.len() as u64;
                        missing.iter().map(|ip| unknown_kernel_frame(*ip)).collect()
                    }
                }
            };
            for (ip, frame) in missing.into_iter().zip(resolved) {
                self.kernel_cache.insert(ip, frame);
            }
        }

        self.maybe_prune();
        ips.iter()
            .filter_map(|ip| self.kernel_cache.get(ip).cloned())
            .collect()
    }
}

fn frame_from(sym: &Symbolized<'_>, ip: u64, kernel: bool) -> Frame {
    match sym {
        Symbolized::Sym(sym) => {
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
            let (file, line) = match &sym.code_info {
                Some(info) => (
                    info.file.to_string_lossy().into_owned(),
                    info.line.unwrap_or(0),
                ),
                None => (String::new(), 0),
            };
            let mut frame = Frame::new(module, sym.name.to_string());
            frame.file = file;
            frame.line = line;
            // `Sym::addr` is the symbol start, `ip` the exact instruction.
            frame.addr = ip;
            frame
        }
        Symbolized::Unknown(_) => {
            if kernel {
                unknown_kernel_frame(ip)
            } else {
                unknown_frame(ip)
            }
        }
    }
}

fn unknown_frame(addr: u64) -> Frame {
    let mut frame = Frame::new("[unknown]", format!("0x{addr:x}"));
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
        let f = unknown_frame(0x4011a0);
        assert_eq!(f.symbol, "0x4011a0");
        assert_eq!(f.addr, 0x4011a0);
    }

    #[test]
    fn resolver_handles_dead_processes() {
        let mut r = FrameResolver::new();
        // PID 0 is not a process we can read; resolution must not panic.
        let frames = r.resolve_user(0, &[0x1000, 0x2000]);
        assert_eq!(frames.len(), 2);
    }
}
