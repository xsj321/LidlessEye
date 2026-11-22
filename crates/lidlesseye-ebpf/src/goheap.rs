//! Go heap profiles, read out of the runtime's own bookkeeping.
//!
//! A Go binary already samples its allocations: one in every `MemProfileRate`
//! bytes is recorded in `runtime.mbuckets`, a chain of buckets keyed by
//! `(call stack, size class)`, each carrying how many bytes and objects were
//! allocated and freed on that stack. `runtime/pprof` produces the heap profile
//! by walking that chain — and so can anything else that can read the process's
//! memory. No recompilation, no `[global_allocator]`, no uprobe on every
//! `malloc`: the numbers are the ones the runtime itself would print, sampled by
//! the runtime, for the cost of one pass over the buckets.
//!
//! ## What has to be true to read it
//!
//! * **The binary must say where the chain starts.** `runtime.mbuckets` and
//!   `runtime.MemProfileRate` are ordinary symbols in `.symtab`, so a stripped
//!   Go binary (`-ldflags "-s -w"`) has no entry point and is skipped rather
//!   than guessed at.
//! * **The addresses must be relocated.** The symbol value is a link-time
//!   virtual address; a PIE binary adds a load bias at run time. The bias comes
//!   from the executable's own mapping in `/proc/<pid>/maps`, not from an
//!   assumption about ASLR.
//! * **`/proc/<pid>/mem` must be readable**, which needs `CAP_SYS_PTRACE` (or
//!   `privileged: true`). Reading is passive: nothing is written to the target,
//!   because writing `MemProfileRate` would change the behaviour of a program
//!   whose behaviour is what is being measured.
//!
//! ## Layout
//!
//! `runtime.bucket` has been the same shape since Go 1.18: six pointer-sized
//! fields, then `nstk` program counters, then a `memRecord` whose `active` cycle
//! holds the last flushed counters (`allocs, frees, alloc_bytes, free_bytes`)
//! and whose `future` array holds the three cycles still in flight. The offsets
//! here are derived from that shape rather than from a table of Go versions, and
//! every read is validated — a bucket whose `nstk` is absurd or whose chain does
//! not end is discarded and counted, never turned into frames that never ran.
//!
//! `active` is the field `runtime.MemProfile` reports, which makes the numbers
//! comparable with `go tool pprof` output. It is published at mark termination,
//! so a burst of allocations appears after the next GC rather than instantly. A
//! process that has not collected yet has nothing in `active`; `runtime` then
//! falls back to summing the in-flight cycles, and so does this reader — without
//! that fallback a short-lived workload would look like it never allocated.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};

use object::{Object, ObjectSymbol};
use tracing::debug;

/// The two counters a bucket carries for one profile cycle.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Counters {
    pub allocs: u64,
    pub frees: u64,
    pub alloc_bytes: u64,
    pub free_bytes: u64,
}

impl Counters {
    fn add(&mut self, other: &Counters) {
        self.allocs += other.allocs;
        self.frees += other.frees;
        self.alloc_bytes += other.alloc_bytes;
        self.free_bytes += other.free_bytes;
    }

    fn is_empty(&self) -> bool {
        self.allocs == 0 && self.frees == 0
    }
}

/// A bucket as it sits in the target's heap, before any interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawBucket {
    /// The address of the bucket itself, which is also its identity across
    /// flushes: buckets live in persistent memory and never move.
    pub addr: u64,
    /// Sampled program counters, leaf first.
    pub stack: Vec<u64>,
    /// Size class this bucket covers, in bytes.
    pub size: u64,
    /// Counters published at the last mark termination.
    pub active: Counters,
    /// Cycles not yet flushed. Only used when nothing has been published.
    pub pending: Counters,
}

/// Pointer-sized field offsets inside `runtime.bucket`.
///
/// The struct is `next, allnext, typ, hash, size, nstk`, all pointer-sized on
/// both 32- and 64-bit Go (the `bucketType` field is padded to a word), and the
/// stack array begins right after it. `memRecord` follows the stack: `active`
/// then `future[3]`, four words each.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Layout {
    pub ptr: u64,
}

impl Layout {
    fn header(self) -> u64 {
        6 * self.ptr
    }

    fn size_at(self) -> u64 {
        4 * self.ptr
    }

    fn nstk_at(self) -> u64 {
        5 * self.ptr
    }

    /// Offset of the `memRecord` that trails `nstk` stack entries.
    fn record_at(self, nstk: u64) -> u64 {
        self.header() + nstk * self.ptr
    }

    /// `active` plus `future[3]`, four words each.
    fn record_bytes(self) -> u64 {
        16 * self.ptr
    }
}

/// Where in a process the chain lives, once discovered.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    /// `starttime` from `/proc/<pid>/stat`: a pid alone is recycled, a pid plus
    /// its start time is not.
    pub start_time: u64,
    pub layout: Layout,
    /// Address to read for the head of the bucket list.
    pub buckets: u64,
    /// Address of `runtime.MemProfileRate`.
    pub rate: u64,
    /// Executable name, for the log line that says what is being profiled.
    pub exe: String,
}

/// Reading another process's address space.
pub(crate) trait Mem {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> io::Result<()>;
}

/// `/proc/<pid>/mem`, opened once per scan.
pub(crate) struct ProcessMem {
    file: File,
}

impl ProcessMem {
    pub(crate) fn open(tgid: u32) -> io::Result<Self> {
        Ok(ProcessMem {
            file: File::open(format!("/proc/{tgid}/mem"))?,
        })
    }
}

impl Mem for ProcessMem {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(buf, addr)
    }
}

/// A flat slice of memory for tests: the same walk, without a process.
#[cfg(test)]
pub(crate) struct ImageMem<'a> {
    pub base: u64,
    pub bytes: &'a [u8],
}

#[cfg(test)]
impl Mem for ImageMem<'_> {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> io::Result<()> {
        let Some(offset) = addr.checked_sub(self.base) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "below image"));
        };
        let start = offset as usize;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "overflow"))?;
        if end > self.bytes.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "past image"));
        }
        buf.copy_from_slice(&self.bytes[start..end]);
        Ok(())
    }
}

/// Why a walk stopped, so that a flood of "no data" is explainable.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WalkStats {
    pub buckets: u64,
    /// Buckets whose header did not look like a `runtime.bucket`.
    pub invalid: u64,
    /// The chain hit the safety cap before reaching its end.
    pub truncated: u64,
}

/// Upper bound on buckets per process per scan.
///
/// The real number is one per (stack, size class) ever sampled, so a long-lived
/// service settles in the thousands. The cap exists because a corrupted
/// `mbuckets` pointer would otherwise walk whatever it points at forever.
const MAX_BUCKETS: u64 = 100_000;

/// Upper bound on a bucket's stack depth. `maxStack` is 32 in the runtime; the
/// slack covers a future increase without accepting garbage.
const MAX_STACK: u64 = 128;

/// Walk the bucket chain and return every bucket in it.
///
/// The header is read first so that the record read can be sized: reading a
/// fixed 1 KiB per bucket would fault in pages the walk does not need, and
/// reading field by field would triple the number of syscalls.
pub(crate) fn walk<M: Mem>(
    mem: &mut M,
    head: u64,
    layout: Layout,
    stats: &mut WalkStats,
) -> Vec<RawBucket> {
    let mut out = Vec::new();
    let mut addr = head;
    let mut seen = 0u64;
    let mut header = vec![0u8; layout.header() as usize];
    let mut record = Vec::new();

    while addr != 0 {
        if seen >= MAX_BUCKETS {
            stats.truncated += 1;
            break;
        }
        seen += 1;
        if mem.read(addr, &mut header).is_err() {
            stats.invalid += 1;
            break;
        }
        let word = |buf: &[u8], offset: u64| -> u64 {
            let start = offset as usize;
            let width = layout.ptr as usize;
            let mut bytes = [0u8; 8];
            bytes[..width].copy_from_slice(&buf[start..start + width]);
            u64::from_le_bytes(bytes)
        };

        let next = word(&header, layout.ptr);
        let size = word(&header, layout.size_at());
        let nstk = word(&header, layout.nstk_at());
        // A stack longer than the runtime allows, or a size class larger than
        // any allocation it can make, is a stray pointer: stop rather than turn
        // it into frames.
        if nstk > MAX_STACK || size > (1 << 48) {
            stats.invalid += 1;
            break;
        }

        let record_len = layout.record_at(nstk) + layout.record_bytes();
        record.resize(record_len as usize, 0);
        if mem.read(addr, &mut record).is_err() {
            stats.invalid += 1;
            break;
        }

        let mut stack = Vec::with_capacity(nstk as usize);
        for i in 0..nstk {
            stack.push(word(&record, layout.header() + i * layout.ptr));
        }
        let counters = layout.record_at(nstk);
        let cycle = |index: u64| Counters {
            allocs: word(&record, counters + index * 4 * layout.ptr),
            frees: word(&record, counters + index * 4 * layout.ptr + layout.ptr),
            alloc_bytes: word(&record, counters + index * 4 * layout.ptr + 2 * layout.ptr),
            free_bytes: word(&record, counters + index * 4 * layout.ptr + 3 * layout.ptr),
        };
        let active = cycle(0);
        let mut pending = Counters::default();
        for index in 1..4 {
            pending.add(&cycle(index));
        }

        out.push(RawBucket {
            addr,
            stack,
            size,
            active,
            pending,
        });
        addr = next;
    }

    stats.buckets += out.len() as u64;
    out
}

/// Go's own estimate of the unsampled total, given one bucket's sampled counts.
///
/// Ported from `runtime/pprof`'s `scaleHeapSample`: an allocation is sampled with
/// probability `size/rate`, so the correction is `1/(1-exp(-avgSize/rate))`
/// rather than a plain division. Without it a profile of a service allocating in
/// 4 KiB chunks reads `rate/4096 = 128` times too small; with it the total
/// matches what `go tool pprof` reports for the same process.
pub(crate) fn scale_factor(count: u64, size: u64, rate: u64) -> f64 {
    if count == 0 || size == 0 || rate <= 1 {
        return 1.0;
    }
    // `avg` is the size class of the bucket, because every sample in a bucket
    // has the same size: `AllocBytes / AllocObjects`.
    let avg = size as f64 / count as f64;
    1.0 / (1.0 - (-avg / rate as f64).exp())
}

/// One process's contribution to the memory profiles.
#[derive(Debug, Clone, PartialEq)]
pub struct HeapSample {
    pub tgid: u32,
    pub stack: Vec<u64>,
    /// Bytes allocated since the previous scan, scaled to the unsampled total.
    pub alloc_bytes: u64,
    /// Objects allocated since the previous scan, scaled the same way.
    pub alloc_objects: u64,
    /// Bytes still held, as of the last published profile cycle.
    pub inuse_bytes: u64,
    pub inuse_objects: u64,
}

/// Discovery and per-process state for the Go heap reader.
pub struct GoHeap {
    /// pid -> target, so that a symbol lookup and an ELF parse happen once per
    /// process rather than once per flush.
    targets: HashMap<u32, Target>,
    /// pid -> bucket address -> counters at the previous flush.
    previous: HashMap<u32, HashMap<u64, Counters>>,
    /// Executables already parsed, keyed by inode and size. A node runs a
    /// handful of distinct Go binaries even when it runs a hundred pods.
    binaries: HashMap<(u64, u64), Option<Symbols>>,
    /// Processes whose memory could not be read at all, cumulative.
    pub unreadable: u64,
    /// Scans that found a process with `MemProfileRate = 0`, cumulative.
    pub disabled: u64,
    /// Buckets whose header or chain did not validate, cumulative.
    pub invalid_buckets: u64,
    /// Why processes were left out, so that "no data" can be explained instead
    /// of guessed at: `no symbols` (stripped), `not go`, `no memory` (missing
    /// CAP_SYS_PTRACE), `no mapping` (a binary still being mapped).
    pub skipped: std::collections::BTreeMap<&'static str, u64>,
}

/// The two addresses a Go binary has to publish, plus its word size.
#[derive(Debug, Clone, Copy)]
struct Symbols {
    buckets: u64,
    rate: u64,
    ptr: u64,
}

impl Default for GoHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl GoHeap {
    pub fn new() -> Self {
        GoHeap {
            targets: HashMap::new(),
            previous: HashMap::new(),
            binaries: HashMap::new(),
            unreadable: 0,
            disabled: 0,
            invalid_buckets: 0,
            skipped: std::collections::BTreeMap::new(),
        }
    }

    /// Scan every live Go process and return what changed since the last scan,
    /// together with the memory still held.
    ///
    /// `alive` is filled with the pids that produced data, so that cached state
    /// for exited processes can be dropped by the caller.
    pub fn scan(&mut self, exclude: &[u32], alive: &mut Vec<u32>) -> Vec<HeapSample> {
        alive.clear();
        let mut samples = Vec::new();
        for tgid in pids() {
            if exclude.contains(&tgid) {
                continue;
            }
            let target = match self.target(tgid) {
                Ok(Some(target)) => target,
                Ok(None) => continue,
                Err(reason) => {
                    *self.skipped.entry(reason).or_default() += 1;
                    continue;
                }
            };
            let mut mem = match ProcessMem::open(tgid) {
                Ok(mem) => mem,
                Err(err) => {
                    self.unreadable += 1;
                    debug!(tgid, error = %err, "cannot read process memory");
                    self.forget(tgid);
                    continue;
                }
            };
            let rate = match read_word(&mut mem, target.rate, target.layout.ptr) {
                Ok(rate) => rate,
                Err(_) => {
                    self.unreadable += 1;
                    self.forget(tgid);
                    continue;
                }
            };
            if rate == 0 {
                // `runtime.MemProfileRate = 0` turns allocation sampling off:
                // there is no chain to read and nothing to report.
                self.disabled += 1;
                continue;
            }
            let head = match read_word(&mut mem, target.buckets, target.layout.ptr) {
                Ok(head) => head,
                Err(_) => {
                    self.unreadable += 1;
                    self.forget(tgid);
                    continue;
                }
            };
            let mut stats = WalkStats::default();
            let buckets = walk(&mut mem, head, target.layout, &mut stats);
            self.invalid_buckets += stats.invalid + stats.truncated;
            if !buckets.is_empty() {
                alive.push(tgid);
                self.accumulate(tgid, rate, &buckets, &mut samples);
            }
        }
        // A pid that produced nothing this scan is gone, or its chain was empty;
        // either way its counters must not be carried into a future process.
        let alive_set: HashSet<u32> = alive.iter().copied().collect();
        self.previous.retain(|pid, _| alive_set.contains(pid));
        self.targets.retain(|pid, _| alive_set.contains(pid));
        samples
    }

    fn accumulate(
        &mut self,
        tgid: u32,
        rate: u64,
        buckets: &[RawBucket],
        out: &mut Vec<HeapSample>,
    ) {
        // `runtime.MemProfile` publishes at mark termination and, when nothing
        // has been published yet, falls back to summing the in-flight cycles so
        // that a program which has not collected still has a profile. Mirror
        // that choice, or a workload that stops before its first GC reads as
        // "allocated nothing".
        let published = buckets.iter().any(|bucket| !bucket.active.is_empty());
        let state = self.previous.entry(tgid).or_default();
        for bucket in buckets {
            if bucket.stack.is_empty() {
                continue;
            }
            let mut now = bucket.active;
            if !published {
                now.add(&bucket.pending);
            }
            let before = state.insert(bucket.addr, now).unwrap_or_default();

            // Counters are cumulative for the life of a bucket, so one flush is
            // the difference. A counter that went backwards means the bucket
            // was recycled or wrapped: take the current value as the interval
            // rather than reporting a negative.
            let alloc_bytes = now.alloc_bytes.saturating_sub(before.alloc_bytes);
            let alloc_objects = now.allocs.saturating_sub(before.allocs);
            let factor = scale_factor(now.allocs, now.alloc_bytes, rate);
            let inuse_bytes = now.alloc_bytes.saturating_sub(now.free_bytes);
            let inuse_objects = now.allocs.saturating_sub(now.frees);

            if alloc_bytes == 0 && inuse_bytes == 0 {
                continue;
            }
            out.push(HeapSample {
                tgid,
                stack: bucket.stack.clone(),
                alloc_bytes: (alloc_bytes as f64 * factor) as u64,
                alloc_objects: (alloc_objects as f64 * factor) as u64,
                inuse_bytes: (inuse_bytes as f64 * factor) as u64,
                inuse_objects: (inuse_objects as f64 * factor) as u64,
            });
        }
        if state.len() > 4 * MAX_BUCKETS as usize {
            // A process that allocates in unbounded distinct stacks would
            // otherwise grow this map forever; the deltas of the dropped
            // buckets are lost, which is the price of a bound.
            state.clear();
        }
    }

    fn forget(&mut self, tgid: u32) {
        self.targets.remove(&tgid);
        self.previous.remove(&tgid);
    }

    /// The pid the last scan identified as a Go process.
    ///
    /// The allocator probes are process-wide and cannot tell a Go program from
    /// a C one, so this is what keeps a Go heap from being counted twice.
    pub fn go_pids(&self) -> HashSet<u32> {
        self.targets.keys().copied().collect()
    }

    /// `Ok(None)` means the process is not a Go program (or has no executable
    /// to read); `Err` says why a Go program could not be read.
    fn target(&mut self, tgid: u32) -> Result<Option<Target>, &'static str> {
        let Some(start_time) = start_time(tgid) else {
            return Ok(None);
        };
        if let Some(cached) = self.targets.get(&tgid)
            && cached.start_time == start_time
        {
            return Ok(Some(cached.clone()));
        }
        let target = match self.discover(tgid, start_time) {
            Ok(Some(target)) => target,
            Ok(None) => return Ok(None),
            Err(reason) => return Err(reason),
        };
        self.targets.insert(tgid, target.clone());
        // A new process inherits no counters from the previous owner of the pid.
        self.previous.remove(&tgid);
        debug!(tgid, exe = %target.exe, "Go heap target discovered");
        Ok(Some(target))
    }

    fn discover(&mut self, tgid: u32, start_time: u64) -> Result<Option<Target>, &'static str> {
        // `/proc/<pid>/exe` is a magic link the kernel resolves in the *target's*
        // mount namespace, so it opens the right binary even when the agent runs
        // in a container of its own. The path the link prints does not exist on
        // this side, which is why the file is only ever read through the link.
        let Some(link) = std::fs::read_link(format!("/proc/{tgid}/exe")).ok() else {
            // No executable link: a kernel thread, or a process already gone.
            return Err("no exe");
        };
        let file = File::open(format!("/proc/{tgid}/exe")).map_err(|_| "open exe")?;
        let meta = file.metadata().map_err(|_| "no exe")?;
        let key = (meta.ino(), meta.len());

        let symbols = match self.binaries.get(&key) {
            Some(symbols) => *symbols,
            None => {
                let parsed = match read_executable(&file) {
                    Some(data) => parse(&data),
                    None => {
                        debug!(tgid, "cannot read executable");
                        None
                    }
                };
                if self.binaries.len() > 512 {
                    self.binaries.clear();
                }
                self.binaries.insert(key, parsed);
                parsed
            }
        };
        let Some(symbols) = symbols else {
            return Err("no symbols");
        };

        // The chain and the rate are link-time addresses. A PIE binary is loaded
        // at a bias, and the executable's own mapping says what it is; reading
        // the bare symbol value would land in an unmapped hole and report
        // "unreadable" for a perfectly healthy process.
        let path = link
            .to_string_lossy()
            .trim_end_matches(" (deleted)")
            .to_string();
        let bias = load_bias(tgid, &path, meta.ino(), first_load(&file)).ok_or("no mapping")?;
        Ok(Some(Target {
            start_time,
            layout: Layout { ptr: symbols.ptr },
            buckets: symbols.buckets.wrapping_add(bias),
            rate: symbols.rate.wrapping_add(bias),
            exe: link
                .to_string_lossy()
                .trim_end_matches(" (deleted)")
                .rsplit('/')
                .next()
                .unwrap_or("go")
                .to_string(),
        }))
    }
}

/// Every pid the agent can see. Kernel threads have no `/proc/<pid>/exe` and
/// are dropped by the discovery step.
fn pids() -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in dir.flatten() {
        if let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) {
            out.push(pid);
        }
    }
    out
}

fn read_word<M: Mem>(mem: &mut M, addr: u64, width: u64) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    mem.read(addr, &mut bytes[..width as usize])?;
    Ok(u64::from_le_bytes(bytes))
}

/// `starttime` (field 22) of a process, which distinguishes a pid from the
/// process that used to own it.
fn start_time(tgid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{tgid}/stat")).ok()?;
    // The command name is parenthesised and may contain spaces and `)`; the
    // fields after the last `)` are fixed-width and start at field 3.
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Upper bound on an executable we are willing to read into memory.
const MAX_BINARY: u64 = 1024 * 1024 * 1024;

/// Read the whole executable through the open `/proc/<pid>/exe` handle.
///
/// `object` wants the bytes, and the alternative — a partial reader that touches
/// only `.symtab` — would save a few megabytes once per binary and cost a second
/// set of bounds checks to get wrong. What is cached afterwards is three
/// integers, not the image.
fn read_executable(file: &File) -> Option<Vec<u8>> {
    let len = file.metadata().ok()?.len();
    if len == 0 || len > MAX_BINARY {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    file.read_exact_at(&mut buf, 0).ok()?;
    Some(buf)
}

/// The two symbols the reader needs, from an ELF image.
fn parse(data: &[u8]) -> Option<Symbols> {
    let file = object::File::parse(data).ok()?;
    // `.go.buildinfo` is what makes a binary a Go program; a C binary with a
    // symbol called `runtime.mbuckets` is not one whose chain we can read.
    file.section_by_name(".go.buildinfo")?;
    let mut buckets = None;
    let mut rate = None;
    for symbol in file.symbols().chain(file.dynamic_symbols()) {
        match symbol.name() {
            Ok("runtime.mbuckets") => buckets = Some(symbol.address()),
            Ok("runtime.MemProfileRate") => rate = Some(symbol.address()),
            _ => {}
        }
    }
    let (Some(buckets), Some(rate)) = (buckets, rate) else {
        // The linker drops these symbols when memory profiling is unreachable,
        // which is what a `-ldflags "-s -w"` build looks like from here.
        debug!("Go binary has no heap symbols (stripped?)");
        return None;
    };
    let ptr = if file.is_64() { 8 } else { 4 };
    Some(Symbols { buckets, rate, ptr })
}

/// Load bias of the executable's first mapping.
///
/// The mapping is identified by *inode*: `/proc/<pid>/maps` shows paths from the
/// target's namespace, which need not exist in the agent's. A mapping at file
/// offset 0 gives the bias directly — a static (non-PIE) binary maps at its link
/// address and the subtraction yields zero, a PIE yields its slide.
fn load_bias(tgid: u32, exe: &str, ino: u64, first_load: Option<(u64, u64)>) -> Option<u64> {
    let maps = std::fs::read_to_string(format!("/proc/{tgid}/maps")).ok()?;
    for line in maps.lines() {
        // One unparsable line (a special mapping, a truncated read) must not
        // abort the search: the mapping that matters is somewhere below it.
        let mut fields = line.split_whitespace();
        let Some((start, _)) = fields.next().and_then(|range| range.split_once('-')) else {
            continue;
        };
        let Ok(start) = u64::from_str_radix(start, 16) else {
            continue;
        };
        // maps lines are `address perms offset dev inode pathname`; the offset
        // is the third field, after the permission string.
        let _perms = fields.next();
        let Some(offset) = fields.next().and_then(|v| u64::from_str_radix(v, 16).ok()) else {
            continue;
        };
        let _dev = fields.next();
        let Some(mapped_ino) = fields.next().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        let path = fields.next().unwrap_or_default();
        if offset != 0 {
            continue;
        }
        // Path first, inode second. On overlayfs — which is every container
        // image — the inode `/proc/<pid>/exe` reports is the overlay's, while
        // the one in `maps` is the file's on the underlying device: the two
        // never match, and matching on inode alone means "no Go process found"
        // on exactly the deployments this agent is built for.
        let same_path = path.trim_end_matches(" (deleted)") == exe;
        if !same_path && mapped_ino != ino {
            continue;
        }
        return Some(match first_load {
            Some((p_offset, p_vaddr)) => start
                .wrapping_sub(offset)
                .wrapping_sub(p_vaddr)
                .wrapping_add(p_offset),
            None => start.wrapping_sub(offset),
        });
    }
    None
}

fn first_load(file: &File) -> Option<(u64, u64)> {
    segments(file).first().copied()
}

/// `(p_offset, p_vaddr)` of every `PT_LOAD` segment, sorted by file offset.
///
/// The first one anchors the bias. It is read from the program headers rather
/// than guessed because a static binary links at 0x400000 and a PIE at 0, and
/// only the segment table knows which this is.
fn segments(file: &File) -> Vec<(u64, u64)> {
    let mut header = [0u8; 64];
    if file.read_exact_at(&mut header, 0).is_err() || &header[..4] != b"\x7fELF" {
        return Vec::new();
    }
    let is64 = header[4] == 2;
    let le16 = |range: std::ops::Range<usize>| {
        u16::from_le_bytes(header[range].try_into().unwrap_or_default())
    };
    let le32 = |range: std::ops::Range<usize>| {
        u32::from_le_bytes(header[range].try_into().unwrap_or_default())
    };
    let le64 = |range: std::ops::Range<usize>| {
        u64::from_le_bytes(header[range].try_into().unwrap_or_default())
    };
    let (phoff, phentsize, phnum) = if is64 {
        (le64(32..40), le16(54..56) as u64, le16(56..58) as u64)
    } else {
        (
            le32(28..32) as u64,
            le16(42..44) as u64,
            le16(44..46) as u64,
        )
    };
    if phnum == 0 || phnum > 256 || phentsize == 0 || phentsize > 256 {
        return Vec::new();
    }
    let mut table = vec![0u8; (phnum * phentsize) as usize];
    if file.read_exact_at(&mut table, phoff).is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..phnum as usize {
        let entry = &table[i * phentsize as usize..(i + 1) * phentsize as usize];
        let field = |range: std::ops::Range<usize>, wide: bool| -> u64 {
            if wide {
                u64::from_le_bytes(entry[range].try_into().unwrap_or_default())
            } else {
                u32::from_le_bytes(entry[range].try_into().unwrap_or_default()) as u64
            }
        };
        let (kind, offset, vaddr) = if is64 {
            (field(0..4, false), field(8..16, true), field(16..24, true))
        } else {
            (field(0..4, false), field(4..8, false), field(8..12, false))
        };
        if kind == 1 {
            out.push((offset, vaddr));
        }
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Bucket<'a> {
        next: u64,
        size: u64,
        stack: &'a [u64],
        counters: [Counters; 4],
    }

    /// Serialise one bucket the way the runtime lays it out.
    fn bucket(image: &mut Vec<u8>, base: u64, addr: u64, bucket: &Bucket<'_>) {
        let layout = Layout { ptr: 8 };
        let offset = (addr - base) as usize;
        let total = (layout.record_at(bucket.stack.len() as u64) + layout.record_bytes()) as usize;
        image.resize(offset + total, 0);
        let put = |image: &mut Vec<u8>, at: u64, value: u64| {
            let start = (at - base) as usize;
            image[start..start + 8].copy_from_slice(&value.to_le_bytes());
        };
        put(image, addr, 0); // next, patched by the caller through `next`
        put(image, addr + 8, bucket.next);
        put(image, addr + 16, 1); // typ: memProfile
        put(image, addr + 32, bucket.size);
        put(image, addr + 40, bucket.stack.len() as u64);
        for (i, pc) in bucket.stack.iter().enumerate() {
            put(image, addr + layout.header() + i as u64 * 8, *pc);
        }
        let counters = addr + layout.record_at(bucket.stack.len() as u64);
        for (index, cycle) in bucket.counters.iter().enumerate() {
            let at = counters + index as u64 * 32;
            put(image, at, cycle.allocs);
            put(image, at + 8, cycle.frees);
            put(image, at + 16, cycle.alloc_bytes);
            put(image, at + 24, cycle.free_bytes);
        }
    }

    fn zero() -> Counters {
        Counters::default()
    }

    #[test]
    fn walks_a_bucket_chain_and_reads_its_counters() {
        let base = 0x1000_0000;
        let mut image = Vec::new();
        bucket(
            &mut image,
            base,
            base + 0x100,
            &Bucket {
                next: base + 0x200,
                size: 4096,
                stack: &[0x1111, 0x2222, 0x3333],
                counters: [
                    Counters {
                        allocs: 7,
                        frees: 2,
                        alloc_bytes: 7 * 4096,
                        free_bytes: 2 * 4096,
                    },
                    zero(),
                    zero(),
                    zero(),
                ],
            },
        );
        bucket(
            &mut image,
            base,
            base + 0x200,
            &Bucket {
                next: 0,
                size: 64,
                stack: &[0x4444],
                counters: [
                    Counters {
                        allocs: 3,
                        frees: 3,
                        alloc_bytes: 192,
                        free_bytes: 192,
                    },
                    zero(),
                    zero(),
                    zero(),
                ],
            },
        );
        let mut mem = ImageMem {
            base,
            bytes: &image,
        };
        let mut stats = WalkStats::default();
        let buckets = walk(&mut mem, base + 0x100, Layout { ptr: 8 }, &mut stats);
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].size, 4096);
        assert_eq!(buckets[0].stack, vec![0x1111, 0x2222, 0x3333]);
        assert_eq!(buckets[0].active.alloc_bytes, 7 * 4096);
        assert_eq!(buckets[0].active.free_bytes, 2 * 4096);
        assert_eq!(buckets[1].stack, vec![0x4444]);
        assert_eq!(buckets[1].active.free_bytes, 192);
        assert_eq!(stats.invalid, 0);
        assert_eq!(stats.buckets, 2);
    }

    #[test]
    fn the_in_flight_cycles_are_kept_separately() {
        let base = 0x1000_0000;
        let mut image = Vec::new();
        let in_flight = Counters {
            allocs: 5,
            frees: 0,
            alloc_bytes: 5 * 4096,
            free_bytes: 0,
        };
        bucket(
            &mut image,
            base,
            base + 0x100,
            &Bucket {
                next: 0,
                size: 4096,
                stack: &[0x1],
                // Nothing published yet; the runtime's fallback sums `future`.
                counters: [zero(), in_flight, in_flight, zero()],
            },
        );
        let mut mem = ImageMem {
            base,
            bytes: &image,
        };
        let mut stats = WalkStats::default();
        let buckets = walk(&mut mem, base + 0x100, Layout { ptr: 8 }, &mut stats);
        assert!(buckets[0].active.is_empty());
        assert_eq!(buckets[0].pending.alloc_bytes, 2 * 5 * 4096);
    }

    #[test]
    fn a_broken_chain_stops_instead_of_inventing_buckets() {
        let base = 0x1000_0000;
        let mut image = Vec::new();
        bucket(
            &mut image,
            base,
            base + 0x100,
            &Bucket {
                next: base + 0x9000,
                size: 64,
                stack: &[1],
                counters: [
                    Counters {
                        allocs: 1,
                        frees: 0,
                        alloc_bytes: 64,
                        free_bytes: 0,
                    },
                    zero(),
                    zero(),
                    zero(),
                ],
            },
        );
        let mut mem = ImageMem {
            base,
            bytes: &image,
        };
        let mut stats = WalkStats::default();
        let buckets = walk(&mut mem, base + 0x100, Layout { ptr: 8 }, &mut stats);
        assert_eq!(buckets.len(), 1);
        assert_eq!(stats.invalid, 1);
    }

    #[test]
    fn an_absurd_stack_depth_is_rejected() {
        let base = 0x1000_0000;
        let mut image = Vec::new();
        bucket(
            &mut image,
            base,
            base + 0x100,
            &Bucket {
                next: 0,
                size: 64,
                stack: &[1],
                counters: [zero(), zero(), zero(), zero()],
            },
        );
        image[0x100 + 40..0x100 + 48].copy_from_slice(&10_000u64.to_le_bytes());
        let mut mem = ImageMem {
            base,
            bytes: &image,
        };
        let mut stats = WalkStats::default();
        let buckets = walk(&mut mem, base + 0x100, Layout { ptr: 8 }, &mut stats);
        assert!(buckets.is_empty());
        assert_eq!(stats.invalid, 1);
    }

    #[test]
    fn scaling_is_off_when_the_runtime_samples_everything() {
        // Rate 1 records every allocation, so the sample is the truth.
        assert_eq!(scale_factor(10, 4096, 1), 1.0);
        // A bucket that was never sampled carries no information to scale.
        assert_eq!(scale_factor(0, 4096, 512 * 1024), 1.0);
        assert_eq!(scale_factor(10, 0, 512 * 1024), 1.0);
    }

    #[test]
    fn a_small_bucket_scales_by_the_inverse_sampling_probability() {
        // 512 B allocations at 512 KiB: one in 1024 is kept, and the estimator
        // has to recover that factor, not clamp it to 1.
        let factor = scale_factor(10, 10 * 512, 512 * 1024);
        assert!(
            (factor - 1024.5).abs() < 2.0,
            "expected ~1024, got {factor}"
        );
    }

    #[test]
    fn scaling_undoes_memory_proportional_sampling() {
        // 4 KiB allocations at the default 512 KiB rate: one in 128 is kept, so
        // the estimate has to be about 128x the sampled bytes. The exact factor
        // is 1/(1-e^-1/128), 128.5, a little above the naive ratio.
        let factor = scale_factor(1, 4096, 512 * 1024);
        assert!((factor - 128.5).abs() < 0.5, "expected ~128, got {factor}");
    }
}
