//! Memory profiles for every language that is not a managed runtime, by
//! hooking the allocator.
//!
//! The Go reader in [`crate::goheap`] works because a Go binary carries its own
//! heap profile. Nothing else does: a C, C++ or Rust program hands its
//! allocations to `malloc` and keeps no record of them. What all of those
//! languages *do* share is the allocator interface — `malloc`, `calloc`,
//! `realloc`, `free` and the aligned variants — so profiling that interface
//! profiles all of them at once, without recompiling anything and without
//! knowing which language produced the binary.
//!
//! ## How a sample is produced
//!
//! Entry probes (uprobe) publish the requested size and the allocating stack
//! keyed by thread id; the return probe (uretprobe) pairs that with the pointer
//! the allocator handed back, records `pointer -> (size, stack)` in
//! `alloc_live`, and adds the bytes to two aggregates: a drained per-interval
//! counter (`alloc_*`) and a running balance (`inuse_*`). `free` looks the
//! pointer up, subtracts it from the balance, and forgets it. All of that is in
//! [`crate::collector`]'s eBPF object, section "allocator uprobes".
//!
//! The kernel keys those aggregates by a 64-bit hash of the first four frames
//! rather than by a `BPF_MAP_TYPE_STACK_TRACE` id — those ids carry only
//! `log2(max_entries)` bits and a stolen slot is permanent for the site that
//! loses it, which is enough to make a whole process's heap disappear. The
//! frames themselves are kept beside the hash, and this side resolves them.
//!
//! ## What this side owns
//!
//! Three things the kernel side cannot know:
//!
//! 1. **Where the allocator lives.** A node mixes glibc, musl, jemalloc,
//!    mimalloc and tcmalloc, each in a different file, and a container carries
//!    its own copy rather than using the host's. Candidate files are collected
//!    from every process's `/proc/<pid>/maps` and deduplicated by inode, so one
//!    library bind-mounted into twenty containers is instrumented once.
//! 2. **What each process is.** `runtime` (c / c++ / rust) and `allocator`
//!    labels, so a profile can be read as "the Rust service's heap" rather than
//!    "some pid's heap".
//! 3. **Which balance belongs to whom.** A pid is recycled, and a restarted
//!    container comes back under the same pid in its namespace, so the in-use
//!    balance a dead incarnation left behind has to be dropped before its
//!    successor inherits it.
//!
//! ## The two numbers mean different things
//!
//! `alloc_*` covers allocations made since the probes were attached — a true
//! rate. `inuse_*` covers allocations that were *seen* and not yet freed, so a
//! gigabyte allocated before the agent started is invisible. That is why the
//! reports carry `profile_source=allocator-uprobe`: they are not the same
//! measurement as a runtime's own heap profile, and the two must never be added
//! together.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aya::Ebpf;
use aya::maps::{HashMap as AyaHashMap, MapData};
use aya::programs::uprobe::UProbeScope;
use aya::programs::{ProgramError, UProbe};
use object::{Object, ObjectSymbol};
use tracing::{debug, info, warn};

/// One entry point, and the names it ships under.
///
/// `malloc` is not one symbol: a prefixed jemalloc exports `je_malloc` and
/// nothing else, mimalloc exports `mi_malloc`, and a static musl build keeps it
/// inside the executable. The list is ordered so that the plain name wins when
/// a file publishes both an alias and the real thing — attaching to both would
/// count one allocation twice.
struct Role {
    program: &'static str,
    /// The allocating roles are also probed on return; `free` is not.
    allocating: bool,
    symbols: &'static [&'static str],
}

const ROLES: &[Role] = &[
    Role {
        program: "lidlesseye_malloc",
        allocating: true,
        symbols: &["malloc", "je_malloc", "mi_malloc", "tc_malloc"],
    },
    Role {
        program: "lidlesseye_calloc",
        allocating: true,
        symbols: &["calloc", "je_calloc", "mi_calloc", "tc_calloc"],
    },
    Role {
        program: "lidlesseye_realloc",
        allocating: true,
        symbols: &["realloc", "je_realloc", "mi_realloc", "tc_realloc"],
    },
    Role {
        program: "lidlesseye_free",
        allocating: false,
        symbols: &["free", "je_free", "mi_free", "tc_free"],
    },
    Role {
        program: "lidlesseye_aligned_alloc",
        allocating: true,
        symbols: &[
            "aligned_alloc",
            "memalign",
            "je_aligned_alloc",
            "mi_aligned_alloc",
        ],
    },
    Role {
        program: "lidlesseye_posix_memalign",
        allocating: true,
        symbols: &["posix_memalign", "je_posix_memalign", "mi_posix_memalign"],
    },
];

/// The shared return probe.
const RETURN_PROGRAM: &str = "lidlesseye_alloc_return";

/// How often to look for allocators and new processes again.
///
/// Short on purpose: a rescan that finds only known processes costs one
/// `stat` per pid, because the classification and the mapped files are cached
/// per pid. Anything longer leaves two holes — a container that starts between
/// two lookups is not attached, and a process that restarted looks like the
/// previous owner of its pid, which makes the in-use balance look stale.
const REDISCOVER: Duration = Duration::from_secs(5);

/// One (process, stack) contribution to the memory profiles.
#[derive(Debug, Clone, PartialEq)]
pub struct AllocSample {
    pub tgid: u32,
    /// Allocator family that served it (`glibc`, `musl`, `jemalloc`, ...).
    pub allocator: &'static str,
    /// Runtime the executable is written in, as far as the ELF says.
    pub runtime: &'static str,
    pub stack: Vec<u64>,
    pub alloc_bytes: u64,
    pub alloc_objects: u64,
    pub inuse_bytes: u64,
    pub inuse_objects: u64,
}

/// Key of the per-stack aggregates. Keep in sync with `profile.bpf.c`.
///
/// `pad` is spelled out because `stack` is 64-bit: without it the compiler
/// inserts four bytes of padding that aya would refuse to treat as plain data.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
struct AllocKey {
    tgid: u32,
    pad: u32,
    stack: u64,
}

// SAFETY: two `u32` then a `u64`, no implicit padding, no pointers.
unsafe impl aya::Pod for AllocKey {}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct AllocCounts {
    bytes: u64,
    objects: u64,
}

// SAFETY: two `u64` fields, no padding, no pointers.
unsafe impl aya::Pod for AllocCounts {}

/// Frames behind one allocation stack hash. Keep in sync with `profile.bpf.c`.
const ALLOC_STACK_FRAMES: usize = 16;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct AllocFrames {
    nr: u32,
    pad: u32,
    ips: [u64; ALLOC_STACK_FRAMES],
}

// SAFETY: `u32`, `u32`, then an array of `u64`; no padding, no pointers.
unsafe impl aya::Pod for AllocFrames {}

/// What the agent knows about one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcInfo {
    start_time: u64,
    allocator: &'static str,
    runtime: &'static str,
}

/// Discovery and bookkeeping for the allocator probes.
pub struct Allocator {
    /// Programs are loaded once; `load` on an already loaded program fails.
    ready: bool,
    /// `(dev, inode)` of every file already attempted, so a rescan does not
    /// re-register probes that exist.
    tried: HashSet<(u64, u64)>,
    /// pid -> classification, keyed by start time so a recycled pid is
    /// re-examined instead of inheriting the previous owner's labels.
    procs: HashMap<u32, ProcInfo>,
    /// Executables already classified, by inode, so that a hundred pods running
    /// one image parse it once.
    runtimes: HashMap<(u64, u64), &'static str>,
    last: Option<Instant>,
    /// Allocator files instrumented on the last discovery.
    pub files: usize,
    /// Probes attached, across every file and role.
    pub probes: usize,
    /// Allocating calls whose stack could not be captured at all.
    pub capture_failures: u64,
    /// Bytes those calls accounted for.
    pub no_capture_bytes: u64,
    /// Samples dropped because their stack had been evicted from the stack map.
    pub lost_stacks: u64,
    /// Bytes they accounted for.
    pub evicted_bytes: u64,
    /// Why files were left out, so "no data" can be explained without naming
    /// every path a node has ever run.
    pub skipped: BTreeMap<&'static str, u64>,
    /// `bpf_get_stackid` failures by errno, as the programs last reported them.
    /// A full stack table and an unreadable user stack both lose the row, and
    /// only the errno says which one to fix.
    pub capture_errnos: BTreeMap<u32, u64>,
    /// Pids that came back with a new start time and whose kernel state still
    /// has to be dropped.
    restarted: Vec<u32>,
}

impl Default for Allocator {
    fn default() -> Self {
        Self::new()
    }
}

impl Allocator {
    pub fn new() -> Self {
        Allocator {
            ready: false,
            tried: HashSet::new(),
            procs: HashMap::new(),
            runtimes: HashMap::new(),
            last: None,
            files: 0,
            probes: 0,
            capture_failures: 0,
            no_capture_bytes: 0,
            lost_stacks: 0,
            evicted_bytes: 0,
            skipped: BTreeMap::new(),
            capture_errnos: BTreeMap::new(),
            restarted: Vec::new(),
        }
    }

    /// What the agent thinks a process is, or `None` if it was never seen.
    pub fn info(&self, tgid: u32) -> Option<(&'static str, &'static str)> {
        self.procs
            .get(&tgid)
            .map(|info| (info.allocator, info.runtime))
    }

    /// Attach to any allocator this node has that is not instrumented yet, and
    /// classify the processes seen along the way.
    ///
    /// Cheap to call repeatedly: the per-inode and per-pid caches make a rescan
    /// a directory walk and a few `/proc/<pid>/maps` reads.
    pub fn discover(&mut self, ebpf: &mut Ebpf, force: bool) {
        if !force
            && let Some(last) = self.last
            && last.elapsed() < REDISCOVER
        {
            return;
        }
        self.last = Some(Instant::now());

        let candidates = self.candidates();
        if candidates.is_empty() {
            debug!("no allocator to instrument");
            return;
        }
        if let Err(err) = load_programs(ebpf) {
            warn!(error = %err, "allocator probes could not be loaded");
            *self
                .skipped
                .entry("probes could not be loaded")
                .or_default() += 1;
            return;
        }
        self.ready = true;
        // The programs skip this thread group. Without it the agent's own
        // allocations are captured, hashed and then thrown away at report
        // time — thousands a second, each with a different broken walk, which
        // is enough to fill the frames table and leave real sites without one.
        set_agent_tgid(ebpf);

        for (key, path) in candidates {
            if !self.tried.insert(key) {
                continue;
            }
            self.files += 1;
            let placed = self.attach_file(ebpf, &path);
            if placed == 0 {
                // A static Go binary exports no `malloc`: expected, and not
                // worth a line per process.
                *self.skipped.entry("no allocator symbol").or_default() += 1;
                debug!(path = %path.display(), "no allocator symbol in file");
            } else {
                debug!(path = %path.display(), probes = placed, "allocator instrumented");
            }
        }
        info!(
            files = self.files,
            probes = self.probes,
            processes = self.procs.len(),
            "allocator probes attached"
        );
    }

    /// Attach every role this file actually exports. Returns how many probes
    /// were placed; zero means the file is not an allocator.
    fn attach_file(&mut self, ebpf: &mut Ebpf, path: &Path) -> usize {
        let mut placed = 0;
        for role in ROLES {
            let mut attached: Option<&'static str> = None;
            for symbol in role.symbols {
                match attach(ebpf, role.program, symbol, path) {
                    Ok(()) => {
                        attached = Some(symbol);
                        break;
                    }
                    Err(err) => {
                        // A missing symbol is the normal case: a glibc file has
                        // no `je_malloc`. Only a real failure is worth a line.
                        if !is_missing(&err) {
                            debug!(path = %path.display(), symbol, error = %err, "allocator attach failed");
                        }
                    }
                }
            }
            let Some(symbol) = attached else {
                continue;
            };
            self.probes += 1;
            placed += 1;
            if role.allocating {
                match attach(ebpf, RETURN_PROGRAM, symbol, path) {
                    Ok(()) => self.probes += 1,
                    Err(err) => {
                        warn!(path = %path.display(), symbol, error = %err, "allocation return probe failed");
                    }
                }
            }
        }
        placed
    }

    /// Every allocator file on this node, once per inode, plus the process
    /// classification that the walk turned up.
    ///
    /// Returns `((dev, inode), path)` where `path` goes through
    /// `/proc/<pid>/root` so that the kernel resolves it in that container's
    /// mount namespace — the host path of a container's `libc.so.6` does not
    /// exist on this side.
    fn candidates(&mut self) -> Vec<((u64, u64), PathBuf)> {
        let mut out: Vec<((u64, u64), PathBuf)> = Vec::new();
        let mut seen: HashSet<(u64, u64)> = HashSet::new();
        let mut procs: HashMap<u32, ProcInfo> = HashMap::new();

        for tgid in pids() {
            let Some(start_time) = start_time(tgid) else {
                continue;
            };
            // A known process that has not restarted needs no work: its
            // allocator is already instrumented and its labels are the same.
            // Reading `/proc/<pid>/maps` for every process on every flush is
            // the expensive half of a scan, and it is almost always unnecessary.
            if let Some(known) = self.procs.get(&tgid)
                && known.start_time == start_time
            {
                procs.insert(tgid, *known);
                continue;
            }
            // The pid is back with a new start time: a restart. Its predecessor
            // left a heap balance in the kernel keyed by this very tgid, and
            // nothing in the map says which incarnation it belonged to — so it
            // has to be dropped here, while both start times are in hand.
            // Without this a container that redeploys inherits its own ghost:
            // the new run's objects and bytes are added on top of a balance
            // that was never freed.
            if self.procs.contains_key(&tgid) {
                self.restarted.push(tgid);
            }

            let files = mapped_files(tgid);
            if files.is_empty() {
                continue;
            }

            // Which allocator family this process uses. A prefixed jemalloc is
            // invisible to glibc's `malloc`, so the family decides which file
            // is worth probing; a process that maps two of them uses the one
            // with the higher priority, which is the one it linked against.
            let mut best: Option<(&'static str, u8, (u64, u64))> = None;
            for file in &files {
                let Some((name, priority)) = family_of(&file.path) else {
                    continue;
                };
                if best.is_none_or(|(_, current, _)| priority > current) {
                    best = Some((name, priority, (file.dev, file.ino)));
                }
            }

            // A static binary carries `malloc` inside its own executable, so
            // the executable is the only place a probe can go.
            let allocator = match best {
                Some((name, _, _)) => name,
                None => "static",
            };
            procs.insert(
                tgid,
                ProcInfo {
                    start_time,
                    allocator,
                    runtime: self.runtime_of(tgid),
                },
            );

            for file in &files {
                if family_of(&file.path).is_none() {
                    continue;
                }
                let key = (file.dev, file.ino);
                if !seen.insert(key) {
                    continue;
                }
                let path = PathBuf::from(format!(
                    "/proc/{tgid}/root/{}",
                    file.path.trim_start_matches('/')
                ));
                out.push((key, path));
            }
            if best.is_none()
                && let Some(key) = exe_key(tgid)
                && seen.insert(key)
            {
                out.push((key, PathBuf::from(format!("/proc/{tgid}/exe"))));
            }
        }

        // Processes that exited keep no state, and a pid that reappeared with a
        // new start time is a different process: `scan` compares against these
        // start times to drop a balance it should not inherit.
        self.procs = procs;
        out
    }

    /// `Go` for a Go binary, `Rust`/`C++`/`C` for the rest. Read from the
    /// executable's section and symbol names, which every compiler leaves.
    fn runtime_of(&mut self, tgid: u32) -> &'static str {
        let Ok(meta) = std::fs::metadata(format!("/proc/{tgid}/exe")) else {
            return "unknown";
        };
        // `(dev, ino)`, the same identity `mapped_files` reports: containers
        // each get their own copy of a shared library, and outside a container
        // the inode alone is not unique across filesystems.
        let key = (meta.dev(), meta.ino());
        if let Some(runtime) = self.runtimes.get(&key) {
            return runtime;
        }
        let runtime = classify(Path::new(&format!("/proc/{tgid}/exe")));
        debug!(tgid, runtime, ino = key.1, "process classified");
        if self.runtimes.len() > 1024 {
            self.runtimes.clear();
        }
        self.runtimes.insert(key, runtime);
        runtime
    }

    /// Drop the heap balance the kernel still holds for a pid.
    ///
    /// The aggregates are keyed by tgid alone, and a pid is not an identity:
    /// `docker restart` brings a container back under the same pid in its
    /// namespace, so a redeployed process would begin life already holding its
    /// predecessor's bytes and objects, forever, since nothing ever frees them.
    /// The live-block table is left alone on purpose — it is keyed by pointer,
    /// and a pointer the new incarnation hands out overwrites whatever the old
    /// one had left at that address.
    fn forget(&mut self, ebpf: &mut Ebpf, tgid: u32) {
        let mut dropped = 0usize;
        for name in ["alloc_inuse", "alloc_totals"] {
            let Some(mut map) = hash_map(ebpf, name) else {
                continue;
            };
            let keys: Vec<AllocKey> = map
                .keys()
                .filter_map(|key| key.ok())
                .filter(|key| key.tgid == tgid)
                .collect();
            for key in keys {
                if map.remove(&key).is_ok() {
                    dropped += 1;
                }
            }
        }
        debug!(
            tgid,
            rows = dropped,
            "dropped a restarted process's heap balance"
        );
    }

    /// Read the aggregates and return one entry per `(process, stack)`.
    ///
    /// `go_pids` are left out: a Go process's heap is already reported by the
    /// runtime reader, and the two sources measure different memory — mixing
    /// them into one `inuse_space` series would describe neither.
    pub fn scan(
        &mut self,
        ebpf: &mut Ebpf,
        exclude: &HashSet<u32>,
        go_pids: &HashSet<u32>,
    ) -> Vec<AllocSample> {
        if !self.ready {
            return Vec::new();
        }

        for tgid in std::mem::take(&mut self.restarted) {
            self.forget(ebpf, tgid);
        }

        // ---- allocated in this window -----------------------------------
        let mut allocated: HashMap<(u32, u64), (u64, u64)> = HashMap::new();
        if let Some(mut map) = hash_map(ebpf, "alloc_totals") {
            let keys: Vec<AllocKey> = map.keys().filter_map(|key| key.ok()).collect();
            for key in &keys {
                if let Ok(value) = map.get(key, 0) {
                    allocated.insert((key.tgid, key.stack), (value.bytes, value.objects));
                }
            }
            for key in keys {
                let _ = map.remove(&key);
            }
        }

        // ---- still held, as of now --------------------------------------
        // Read, never cleared: this is a balance the kernel keeps. An entry
        // that has been fully freed is deleted, which keeps a long-lived node
        // from filling the table with dead stacks.
        let mut held: HashMap<(u32, u64), (u64, u64)> = HashMap::new();
        let mut dead: Vec<AllocKey> = Vec::new();
        if let Some(mut map) = hash_map(ebpf, "alloc_inuse") {
            let keys: Vec<AllocKey> = map.keys().filter_map(|key| key.ok()).collect();
            for key in keys {
                let Ok(value) = map.get(&key, 0) else {
                    continue;
                };
                // `free` decrements these counters with `0 - size` in BPF, so a
                // block allocated *before* the probe was attached makes the
                // running total negative. Read as unsigned and one unmatched
                // free of a single object turns into `u64::MAX`, which the
                // server rejects as an implausible value — and it rejects the
                // whole upload, not just that row.
                let bytes = live_counter(value.bytes);
                let objects = live_counter(value.objects);
                if bytes == 0 && objects == 0 {
                    // Nothing held: either a genuinely empty slot or one that
                    // went negative. Keep the map clean either way.
                    dead.push(key);
                    continue;
                }
                // A pid that is gone, or that now names a different process,
                // must not hand its balance to its successor.
                let stale = match self.procs.get(&key.tgid) {
                    Some(info) => start_time(key.tgid) != Some(info.start_time),
                    None => true,
                };
                if stale {
                    dead.push(key);
                    continue;
                }
                held.insert((key.tgid, key.stack), (bytes, objects));
            }
            for key in dead {
                let _ = map.remove(&key);
            }
        }

        // ---- why captures failed -----------------------------------------
        // Read, never cleared: these are running totals in the kernel, and
        // resetting them would hide a failure that only happens at startup.
        if let Some(map) = error_map(ebpf, "alloc_errors") {
            self.capture_errnos.clear();
            for key in map.keys().filter_map(|key| key.ok()) {
                if let Ok(count) = map.get(&key, 0) {
                    self.capture_errnos.insert(key, count);
                }
            }
        }

        if allocated.is_empty() && held.is_empty() {
            return Vec::new();
        }

        let mut rows: HashMap<(u32, u64), (u64, u64, u64, u64)> = HashMap::new();
        for (key, (bytes, objects)) in allocated {
            let row = rows.entry(key).or_default();
            row.0 += bytes;
            row.1 += objects;
        }
        for (key, (bytes, objects)) in held {
            let row = rows.entry(key).or_default();
            row.2 += bytes;
            row.3 += objects;
        }

        let mut stacks: HashMap<u64, Vec<u64>> = HashMap::new();
        if !rows.is_empty()
            && let Some(map) = frames_map(ebpf, "alloc_stacks")
        {
            for (_, stack) in rows.keys() {
                // Zero is the capture-failure marker, not a hash.
                if *stack == 0 || stacks.contains_key(stack) {
                    continue;
                }
                if let Ok(frames) = map.get(stack, 0) {
                    let nr = (frames.nr as usize).min(ALLOC_STACK_FRAMES);
                    let ips = frames.ips[..nr].to_vec();
                    if !ips.is_empty() {
                        stacks.insert(*stack, ips);
                    }
                }
            }
        }

        let mut out = Vec::with_capacity(rows.len());
        for ((tgid, stack_id), (alloc_bytes, alloc_objects, inuse_bytes, inuse_objects)) in rows {
            if exclude.contains(&tgid) || go_pids.contains(&tgid) {
                continue;
            }
            let Some(stack) = stacks.get(&stack_id) else {
                // Either the walk failed at capture time (`stack_id` is zero)
                // or the frames table lost the entry between the counter
                // update and this read. Both lose the whole row, and which one
                // it was decides the fix: a capture that cannot walk the stack
                // versus a table that is too small.
                if stack_id == 0 {
                    self.capture_failures += 1;
                    self.no_capture_bytes += alloc_bytes + inuse_bytes;
                } else {
                    self.lost_stacks += 1;
                    self.evicted_bytes += alloc_bytes + inuse_bytes;
                }
                continue;
            };
            let (allocator, runtime) = match self.procs.get(&tgid) {
                Some(info) => (info.allocator, info.runtime),
                None => continue,
            };
            out.push(AllocSample {
                tgid,
                allocator,
                runtime,
                stack: stack.clone(),
                alloc_bytes,
                alloc_objects,
                inuse_bytes,
                inuse_objects,
            });
        }
        out
    }
}

/// Read a "still live" counter that the kernel decrements.
///
/// `free` subtracts with `0 - size`, so a block allocated before the probe was
/// attached drives the running total negative; as a `u64` that is a number
/// close to `u64::MAX`. Such a value is not a large allocation, it is a broken
/// counter — and the server refuses the whole upload over one implausible
/// value. Clamp at zero, which is what "nothing is held" means.
fn live_counter(raw: u64) -> u64 {
    (raw as i64).max(0) as u64
}

/// Tell the probes which thread group is the agent's own.
fn set_agent_tgid(ebpf: &mut Ebpf) {
    let Some(map) = ebpf.map_mut("agent_tgid") else {
        return;
    };
    let Ok(mut array) = aya::maps::Array::<_, u32>::try_from(map) else {
        return;
    };
    if let Err(err) = array.set(0, &std::process::id(), 0) {
        debug!(error = %err, "could not exclude the agent from allocation profiling");
    }
}

/// Load every allocator program once. `AlreadyLoaded` is success: a rescan
/// loads nothing new.
fn load_programs(ebpf: &mut Ebpf) -> Result<(), String> {
    let names = ROLES
        .iter()
        .map(|role| role.program)
        .chain(std::iter::once(RETURN_PROGRAM));
    for name in names {
        let Some(program) = ebpf.program_mut(name) else {
            return Err(format!("the eBPF object is missing `{name}`"));
        };
        let probe: &mut UProbe = program
            .try_into()
            .map_err(|err| format!("`{name}` is not a uprobe program: {err}"))?;
        match probe.load() {
            Ok(()) => {}
            Err(ProgramError::AlreadyLoaded) => {}
            Err(err) => return Err(format!("could not load `{name}`: {err}")),
        }
    }
    Ok(())
}

fn attach(ebpf: &mut Ebpf, program: &str, symbol: &str, path: &Path) -> Result<(), String> {
    let Some(raw) = ebpf.program_mut(program) else {
        return Err(format!("the eBPF object is missing `{program}`"));
    };
    let probe: &mut UProbe = raw
        .try_into()
        .map_err(|err| format!("`{program}` is not a uprobe program: {err}"))?;
    probe
        .attach([symbol], path, UProbeScope::AllProcesses)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// aya words a missing symbol differently across versions; all of them mean
/// "this file does not export that name", which is expected and not logged.
fn is_missing(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("symbol") || lower.contains("no such") || lower.contains("not found")
}

fn hash_map<'a>(
    ebpf: &'a mut Ebpf,
    name: &str,
) -> Option<AyaHashMap<&'a mut MapData, AllocKey, AllocCounts>> {
    let map = ebpf.map_mut(name)?;
    AyaHashMap::try_from(map).ok()
}

/// The capture-failure table: errno -> times it happened.
fn error_map<'a>(ebpf: &'a mut Ebpf, name: &str) -> Option<AyaHashMap<&'a mut MapData, u32, u64>> {
    let map = ebpf.map_mut(name)?;
    AyaHashMap::try_from(map).ok()
}

/// The frames behind a stack hash: `u64` -> up to `ALLOC_STACK_FRAMES` ips.
fn frames_map<'a>(
    ebpf: &'a mut Ebpf,
    name: &str,
) -> Option<AyaHashMap<&'a mut MapData, u64, AllocFrames>> {
    let map = ebpf.map_mut(name)?;
    AyaHashMap::try_from(map).ok()
}

/// A file visible in a process's address space.
#[derive(Debug, Clone)]
struct Mapped {
    path: String,
    dev: u64,
    ino: u64,
}

/// The files a process has mapped, from `/proc/<pid>/maps`.
///
/// Only lines with an inode and a path are kept: anonymous mappings and
/// `[vdso]`-style pseudo files have neither an allocator nor a stable identity.
fn mapped_files(tgid: u32) -> Vec<Mapped> {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{tgid}/maps")) else {
        return Vec::new();
    };
    text.lines().filter_map(parse_maps_line).collect()
}

/// `address perms offset dev inode path` — the path is the sixth field and may
/// contain spaces, so the first five are split off and the rest is the path.
fn parse_maps_line(line: &str) -> Option<Mapped> {
    let mut rest = line;
    let mut fields = [""; 5];
    for field in fields.iter_mut() {
        let (head, tail) = rest.split_once(char::is_whitespace)?;
        *field = head;
        rest = tail.trim_start();
    }
    let path = rest.trim();
    if path.is_empty() || path.starts_with('[') {
        return None;
    }
    let (dev, ino) = (fields[3], fields[4]);
    let (major, minor) = dev.split_once(':')?;
    let dev = (u64::from_str_radix(major, 16).ok()? << 32) | u64::from_str_radix(minor, 16).ok()?;
    let ino: u64 = ino.parse().ok()?;
    if ino == 0 {
        return None;
    }
    Some(Mapped {
        path: path.trim_end_matches(" (deleted)").to_string(),
        dev,
        ino,
    })
}

/// Which allocator a mapped file is, and how specific that answer is.
///
/// The priority decides which of several mapped allocators a process is
/// credited to: a binary that links both glibc and jemalloc allocates through
/// jemalloc, and reporting it as glibc would name the wrong implementation and
/// drop every allocation (jemalloc's prefixed entry points are not glibc's).
fn family_of(path: &str) -> Option<(&'static str, u8)> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.contains("jemalloc") {
        return Some(("jemalloc", 4));
    }
    if name.contains("mimalloc") {
        return Some(("mimalloc", 3));
    }
    if name.contains("tcmalloc") {
        return Some(("tcmalloc", 2));
    }
    if name.starts_with("libc.musl") || name.starts_with("ld-musl") {
        return Some(("musl", 1));
    }
    // `libc.so.6`, `libc-2.31.so`, and the versionless `libc.so` a musl build
    // sometimes leaves behind.
    if name.starts_with("libc.so") || name.starts_with("libc-") {
        return Some(("glibc", 1));
    }
    None
}

/// The inode of a process's executable, matching the key `mapped_files`
/// produces for it.
fn exe_key(tgid: u32) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(format!("/proc/{tgid}/exe")).ok()?;
    Some((meta.dev(), meta.ino()))
}

/// Name the language from what the compiler left in the file.
///
/// `.go.buildinfo` is definitive for Go. Rust keeps `rust_eh_personality` and
/// the mangled `core`/`alloc` symbols in every binary it links, C++ keeps the
/// Itanium ABI's mangling, and anything else is called C — which is the right
/// answer for a C program and an honest "not one of the three" for Fortran.
fn classify(path: &Path) -> &'static str {
    let Ok(data) = std::fs::read(path) else {
        return "unknown";
    };
    let Ok(file) = object::File::parse(data.as_slice()) else {
        return "unknown";
    };
    if file.section_by_name(".go.buildinfo").is_some() {
        return "go";
    }
    let mut cpp = false;
    for symbol in file.symbols().chain(file.dynamic_symbols()) {
        let Ok(name) = symbol.name() else {
            continue;
        };
        if name.starts_with("rust_eh_personality")
            || name.contains("rust_begin_unwind")
            || name.starts_with("_ZN4core")
            || name.starts_with("_ZN5alloc")
        {
            return "rust";
        }
        // Rust's legacy mangling is Itanium-shaped too, so the C++ test below
        // has to exclude it explicitly; checking Rust first and returning early
        // is not enough because a symbol table is in link order, not ABI order.
        if is_cpp_symbol(name) {
            cpp = true;
        }
    }
    if cpp { "c++" } else { "c" }
}

/// Whether a symbol is one only a C++ compiler emits.
///
/// The obvious markers are wrong in both directions. `_ZTV`/`_ZTI` are absent
/// from any program that never declares a virtual function — a `std::vector`
/// and `std::string` workload has neither — and `__cxa_` is a *C* ABI hook, so
/// `__cxa_finalize` shows up in ordinary C binaries. What a C++ binary always
/// has is name mangling (`_Z…`) and, when exceptions are on, g++'s personality
/// routine.
fn is_cpp_symbol(name: &str) -> bool {
    if name.starts_with("__gxx_personality_v0") || name.starts_with("__cxa_throw") {
        return true;
    }
    let Some(rest) = name.strip_prefix("_Z") else {
        return false;
    };
    // `_ZN4core…17h0011223344556677E` is Rust, not C++: the trailing `17h`
    // plus sixteen hex digits plus `E` is the legacy scheme's hash.
    match rest.rfind("17h") {
        Some(at) => {
            let tail = &rest[at + 3..];
            !(tail.len() == 17
                && tail.ends_with('E')
                && tail[..16].chars().all(|c| c.is_ascii_hexdigit()))
        }
        None => true,
    }
}

/// Every pid the agent can see.
fn pids() -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in dir.flatten() {
        if let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        {
            out.push(pid);
        }
    }
    out
}

/// `starttime` (field 22), which distinguishes a pid from its predecessor.
fn start_time(tgid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{tgid}/stat")).ok()?;
    // The command name is parenthesised and may contain spaces, so counting
    // whitespace-separated fields only works from after the closing paren.
    let (_, rest) = text.rsplit_once(')')?;
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A counter that went negative must not be reported as a huge value.
    ///
    /// `free` decrements with `0 - size`, so an allocation made before the
    /// probe attached shows up as `u64::MAX` once it is freed. The server
    /// rejects a profile whose value exceeds 1e15 and, before it learned to
    /// drop just that report, refused the whole upload — which is how one
    /// unmatched free hid a tenant's data.
    #[test]
    fn a_negative_live_counter_reads_as_zero() {
        assert_eq!(live_counter(u64::MAX), 0, "0 - 1 must not be a value");
        assert_eq!(live_counter(0u64.wrapping_sub(4096)), 0);
        assert_eq!(live_counter(4096), 4096, "a real total is unchanged");
        assert_eq!(live_counter(0), 0);
    }

    #[test]
    fn a_maps_line_yields_the_file_it_maps() {
        let line = "7f1e2c000000-7f1e2c021000 r--p 00000000 08:01 1234567 /usr/lib/x86_64-linux-gnu/libc.so.6";
        let mapped = parse_maps_line(line).expect("a normal mapping parses");
        assert_eq!(mapped.path, "/usr/lib/x86_64-linux-gnu/libc.so.6");
        assert_eq!(mapped.ino, 1_234_567);
        assert_eq!(mapped.dev, (8 << 32) | 1);
    }

    #[test]
    fn anonymous_and_pseudo_mappings_are_not_candidates() {
        assert!(parse_maps_line("55d0-55d1 rw-p 00000000 00:00 0 ").is_none());
        assert!(parse_maps_line("7ffd-7ffe r-xp 00000000 00:00 0 [vdso]").is_none());
    }

    #[test]
    fn a_deleted_file_keeps_its_path() {
        let line = "7f00-7f01 r-xp 00000000 08:01 42 /tmp/libjemalloc.so.2 (deleted)";
        let mapped = parse_maps_line(line).expect("parses");
        assert_eq!(mapped.path, "/tmp/libjemalloc.so.2");
    }

    #[test]
    fn the_allocator_family_is_named_from_the_file() {
        assert_eq!(family_of("/usr/lib/libc.so.6"), Some(("glibc", 1)));
        assert_eq!(family_of("/lib/libc-2.31.so"), Some(("glibc", 1)));
        assert_eq!(family_of("/lib/libjemalloc.so.2"), Some(("jemalloc", 4)));
        assert_eq!(family_of("/usr/lib/libmimalloc.so"), Some(("mimalloc", 3)));
        assert_eq!(
            family_of("/usr/lib/libtcmalloc.so.4"),
            Some(("tcmalloc", 2))
        );
        assert_eq!(family_of("/lib/ld-musl-x86_64.so.1"), Some(("musl", 1)));
        assert_eq!(family_of("/usr/lib/libssl.so.3"), None);
    }

    #[test]
    fn a_prefixed_allocator_outranks_glibc() {
        let jemalloc = family_of("/usr/lib/libjemalloc.so.2").unwrap();
        let glibc = family_of("/usr/lib/libc.so.6").unwrap();
        assert!(
            jemalloc.1 > glibc.1,
            "jemalloc must win when both are mapped"
        );
    }

    #[test]
    fn the_first_symbol_of_a_role_is_the_plain_name() {
        for role in ROLES {
            assert!(
                matches!(
                    role.symbols[0],
                    "malloc" | "calloc" | "realloc" | "free" | "aligned_alloc" | "posix_memalign"
                ),
                "{} must try the unprefixed symbol first",
                role.program
            );
        }
    }

    #[test]
    fn cxx_mangling_marks_a_binary_as_cpp() {
        // What `g++ -std=c++17` actually leaves behind for a
        // `std::vector<std::string>` workload: no vtables, no `__cxa_throw`,
        // because nothing in the standard containers is polymorphic.
        for name in [
            "_ZNSt6vectorIPNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEESaIS6_EE17_M_realloc_insertIJRKS6_EEEvN9__gnu_cxx17__normal_iteratorIPS6_S8_EEDpOT_",
            "_ZSt20__throw_length_errorPKc",
            "_Znam",
            "_Znwm",
            "_ZdlPvm@CXXABI_1.3.9",
            "__gxx_personality_v0@CXXABI_1.3",
        ] {
            assert!(is_cpp_symbol(name), "{name} is C++");
        }
    }

    #[test]
    fn a_c_binary_is_not_mistaken_for_cpp() {
        // `cfix` links these; `__cxa_finalize` is the C ABI's atexit hook, not
        // evidence of C++.
        for name in ["malloc", "__cxa_finalize@GLIBC_2.2.5", "__libc_start_main"] {
            assert!(!is_cpp_symbol(name), "{name} is not C++");
        }
    }

    #[test]
    fn rust_mangling_is_not_mistaken_for_cpp() {
        for name in [
            "_ZN4core3ptr13drop_in_place17h0123456789abcdefE",
            "_ZN5alloc7raw_vec19RawVec$LT$T$C$A$GT$7reserve21do_reserve_and_handle17haabbccddeeff0011E",
        ] {
            assert!(!is_cpp_symbol(name), "{name} is Rust, not C++");
        }
        // A Rust symbol name that happens to be short enough to have no hash
        // still must not flip the answer on its own; the `_ZN4core`/`_ZN5alloc`
        // test in `classify` is what names Rust, this only has to not lie.
        assert!(is_cpp_symbol("_ZN3foo3barEv"));
    }
}
