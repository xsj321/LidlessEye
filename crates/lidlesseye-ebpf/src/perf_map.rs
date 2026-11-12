//! Names for just-in-time compiled code.
//!
//! A JVM, a Node.js process or a V8 isolate has no static symbols: the machine
//! code was written to anonymous memory at run time and the profiler sees
//! addresses that belong to no file. The runtimes publish
//! `/tmp/perf-<pid>.map` ("perf map", the same interface `perf report` uses),
//! one line per compiled region:
//!
//! ```text
//! 7f3c1a000000 1a0 LazyCompile:*handle /srv/app/index.js:42
//! 7f3c1a0001a0 90 Builtin:ArgumentsAdaptorTrampoline
//! ```
//!
//! `blazesym` already reads that file, but only at the path *it* can see. When
//! the agent runs on the host (or in a sidecar) and the JIT lives in a
//! container, the file is inside the target's mount namespace and the runtime
//! named it after the pid *inside* the target's pid namespace. Both are one
//! `/proc/<pid>/root` and one `NSpid` lookup away, which is what this module
//! adds.
//!
//! Addresses here are absolute: JIT regions are not relocated, so no load bias
//! applies.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Perf maps grow while the process runs, so a snapshot cannot be cached for
/// long. Five seconds is short enough that a hot new function is named on the
/// next flush and long enough that a 5 Hz flush does not re-read the file.
const TTL: Duration = Duration::from_secs(5);

/// Guard against a corrupt or hostile map file.
const MAX_ENTRIES: usize = 2_000_000;

struct Entry {
    start: u64,
    end: u64,
    name: String,
}

struct Cache {
    at: Instant,
    entries: Arc<Vec<Entry>>,
}

/// Fallback that answers addresses of JIT compiled code.
#[derive(Default)]
pub(crate) struct PerfMapFallback {
    maps: HashMap<u32, Cache>,
}

impl PerfMapFallback {
    pub(crate) fn new() -> Self {
        PerfMapFallback::default()
    }

    /// Name of the JIT region containing `addr`, if the process publishes one.
    pub(crate) fn resolve(&mut self, tgid: u32, addr: u64) -> Option<String> {
        let stale = self
            .maps
            .get(&tgid)
            .is_none_or(|cache| cache.at.elapsed() > TTL);
        if stale {
            let entries = load(tgid).unwrap_or_default();
            if self.maps.len() > 64 {
                self.maps.clear();
            }
            self.maps.insert(
                tgid,
                Cache {
                    at: Instant::now(),
                    entries: Arc::new(entries),
                },
            );
        }
        let cache = self.maps.get(&tgid)?;
        // Entries are sorted by start; overlapping regions do not happen.
        let index = cache.entries.partition_point(|entry| entry.start <= addr);
        let entry = cache.entries.get(index.checked_sub(1)?)?;
        if addr >= entry.end {
            return None;
        }
        Some(entry.name.clone())
    }
}

fn load(tgid: u32) -> Option<Vec<Entry>> {
    let inner = inner_pid(tgid).unwrap_or(tgid);
    let mut candidates = vec![
        format!("/proc/{tgid}/root/tmp/perf-{inner}.map"),
        format!("/tmp/perf-{tgid}.map"),
    ];
    if inner != tgid {
        candidates.push(format!("/tmp/perf-{inner}.map"));
    }
    let data = candidates
        .iter()
        .find_map(|path| std::fs::read(path).ok())?;
    Some(parse(&data))
}

/// The pid the target's own namespace sees.
///
/// A runtime writing `perf-<pid>.map` uses `getpid()`, which inside a container
/// is not the host pid the agent sampled. `NSpid` lists the pid from outermost
/// to innermost, so the last field is the one the file is named after.
fn inner_pid(tgid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{tgid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .and_then(|pids| pids.split_whitespace().last())
        .and_then(|pid| pid.parse().ok())
}

fn parse(data: &[u8]) -> Vec<Entry> {
    let mut entries = Vec::new();
    for line in data.split(|&byte| byte == b'\n') {
        if entries.len() >= MAX_ENTRIES {
            break;
        }
        let line = String::from_utf8_lossy(line);
        let mut fields = line.splitn(3, ' ');
        let (Some(start), Some(size), Some(name)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(start), Ok(size)) = (
            u64::from_str_radix(start.trim(), 16),
            u64::from_str_radix(size.trim(), 16),
        ) else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || size == 0 {
            continue;
        }
        let Some(end) = start.checked_add(size) else {
            continue;
        };
        entries.push(Entry {
            start,
            end,
            name: name.to_string(),
        });
    }
    entries.sort_unstable_by_key(|entry| entry.start);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_perf_map() {
        let data = b"\
7f3c1a000000 1a0 LazyCompile:*handle /srv/app/index.js:42
7f3c1a0001a0 90 Builtin:ArgumentsAdaptorTrampoline
not a line
7f3c1a000230 0 zero-size-region
";
        let entries = parse(data);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].start, 0x7f3c1a000000);
        assert_eq!(entries[0].end, 0x7f3c1a0001a0);
        assert_eq!(entries[0].name, "LazyCompile:*handle /srv/app/index.js:42");
        assert_eq!(entries[1].name, "Builtin:ArgumentsAdaptorTrampoline");
    }

    #[test]
    fn lookup_respects_region_bounds() {
        let mut fallback = PerfMapFallback::default();
        let entries = parse(b"1000 100 main.a\n2000 100 main.b\n");
        fallback.maps.insert(
            1,
            Cache {
                at: Instant::now(),
                entries: Arc::new(entries),
            },
        );
        assert_eq!(fallback.resolve(1, 0x1000).as_deref(), Some("main.a"));
        assert_eq!(fallback.resolve(1, 0x10ff).as_deref(), Some("main.a"));
        assert_eq!(fallback.resolve(1, 0x1100), None);
        assert_eq!(fallback.resolve(1, 0x2000).as_deref(), Some("main.b"));
        assert_eq!(fallback.resolve(1, 0x0fff), None);
        // Unknown pid: no map, no panic.
        assert_eq!(fallback.resolve(999_999, 0x1000), None);
    }

    #[test]
    fn garbage_never_panics() {
        assert!(parse(b"").is_empty());
        assert!(parse(&[0xff, 0xfe, 0x00]).is_empty());
        assert!(parse(b"zzz zzz name").is_empty());
        assert!(parse(b"ffffffffffffffff ffffffffffffffff too-big").is_empty());
    }

    /// Live check against a JIT runtime, if one is running.
    ///
    /// `node --perf-basic-prof` writes `/tmp/perf-<pid>.map`; this asserts that
    /// the file is found through `/proc/<pid>/root` and that an address inside
    /// it comes back with the runtime's name for it.
    #[test]
    #[ignore = "requires root and a JIT process (node --perf-basic-prof)"]
    fn resolves_jit_code_of_a_live_process() {
        let mut fallback = PerfMapFallback::new();
        let mut checked = 0;
        for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Ok(tgid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Some(entries) = load(tgid) else { continue };
            let Some(first) = entries.first() else {
                continue;
            };
            checked += 1;
            let name = fallback.resolve(tgid, first.start);
            println!(
                "{:>7} regions={:<5} {:x} -> {:?}",
                tgid,
                entries.len(),
                first.start,
                name
            );
            assert_eq!(name.as_deref(), Some(first.name.as_str()));
            if checked >= 3 {
                break;
            }
        }
        if checked == 0 {
            println!("no process publishes a perf map; nothing to check");
        }
    }
}
