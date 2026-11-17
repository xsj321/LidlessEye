//! Reaching the binary behind a mapping, and reading its symbols.
//!
//! `/proc/<pid>/maps` names files the way the *target* sees them:
//! `/bin/app`, `/app/server`, `/usr/lib/x86_64-linux-gnu/libc.so.6`. The agent
//! runs in its own mount namespace, where none of those paths exist, so a
//! symbolizer that simply calls `fs::read(path)` fails on every frame of every
//! container on the node. That failure is reported as `MissingSyms`, which
//! reads like "this binary has no symbols" and sends the user looking at their
//! build flags instead of at the agent's namespaces.
//!
//! Four routes reach the real file and none works everywhere, so all are tried:
//!
//! 1. `/proc/<pid>/root<path>` — resolves inside the *target's* mount
//!    namespace. Needs only ptrace read permission, not `CAP_SYS_ADMIN`, so it
//!    is the route that keeps working on hardened clusters where the agent runs
//!    unprivileged.
//! 2. `<host root><path>` — an explicit prefix, for the common chart layout
//!    that mounts the host filesystem at `/host`.
//! 3. `path` — only correct when the agent shares the target's namespace.
//! 4. `/proc/<pid>/map_files/<start>-<end>` — the magic link naming the exact
//!    file behind a mapping, deleted files included. Requires
//!    `CAP_SYS_ADMIN`, so it is last.
//!
//! The plain path is the trap, which is why it is third rather than first:
//! `/usr/local/bin/lidlesseye` exists in the agent's own image and in every pod
//! built from the same name, and symbolizing against the agent's copy yields
//! either nothing or plausible-looking names from the wrong build. Candidates
//! are matched by inode, and one that opens a *different* file is discarded
//! rather than used as a last resort.
//!
//! Once a file is in hand, both symbol sources live here so that a process only
//! ever pays for one parse: the ELF symbol table (`.symtab` / `.dynsym`) and,
//! when that came up empty, the Go function table from [`crate::gopclntab`].

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol};

use crate::gopclntab::GoTable;

/// Roots the host filesystem may be mounted at inside the agent, in the order
/// the corresponding charts use them.
const HOST_ROOTS: [&str; 3] = ["/host", "/rootfs", "/run/desktop/mnt/host"];

/// How long a `/proc/<pid>/maps` snapshot is trusted. Go processes that
/// `dlopen` anything are rare, and a stale snapshot can only cause a miss (an
/// address outside every known range), never a wrong name.
const MAPS_TTL: Duration = Duration::from_secs(60);

/// Number of pids whose `/proc/<pid>/maps` snapshots are kept.
const MAX_MAPS: usize = 256;

/// Number of distinct binaries kept parsed. Each one holds a symbol table, so
/// this is the real memory bound of the fallback path: a node runs a hundred
/// different binaries, and re-reading a 10 MB Go function table because it was
/// evicted by an unrelated one would cost more than the memory saved.
const MAX_MODULES: usize = 16;

/// Refuse to read absurd files. A 3 GiB binary is not a profiled service, and
/// the agent must not be the process that finds out.
const MAX_MODULE_BYTES: u64 = 256 * 1024 * 1024;

/// Per-binary cap on symbols. Chromium-sized symbol tables exist; a profiler
/// does not need them and cannot afford them.
const MAX_SYMBOLS: usize = 300_000;

/// Number of distinct unreadable paths reported at warn level. One report per
/// binary is what a user needs to fix their deployment; a report per address
/// would be a log flood.
const MAX_REPORTED: usize = 32;

/// One `PT_LOAD` segment, needed to undo relocation for an address.
#[derive(Clone, Copy)]
pub(crate) struct LoadSegment {
    offset: u64,
    vaddr: u64,
    filesz: u64,
}

/// One `/proc/<pid>/maps` line that has a file behind it.
#[derive(Clone)]
struct Mapping {
    start: u64,
    end: u64,
    /// `r-xp` and friends. An instruction pointer that lands in a mapping
    /// without `x` is not a symbol problem at all: the stack walk produced an
    /// address that never executed.
    perms: [u8; 4],
    offset: u64,
    dev: u64,
    inode: u64,
    path: String,
}

impl Mapping {
    /// Whether the mapping is executable, i.e. whether an instruction pointer
    /// could legitimately land in it.
    fn executable(&self) -> bool {
        self.perms[2] == b'x'
    }

    /// Identity of the file this mapping was made from, as far as `meta` agrees.
    ///
    /// The inode is the only field that survives a mount namespace unchanged.
    /// The device is *not* usable as a primary key: an overlayfs presents the
    /// same inode under a different `st_dev` than the one `/proc/<pid>/maps`
    /// reports, so requiring both rejects the correct file and — worse — lets a
    /// same-named file from the agent's own image win. It is used only to break
    /// ties between candidates that already share an inode.
    fn identity(&self, meta: &std::fs::Metadata) -> Identity {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.ino() != self.inode {
                return Identity::Different;
            }
            let dev = meta.dev();
            let major = libc::major(dev) as u64;
            let minor = libc::minor(dev) as u64;
            if (major << 32 | minor) == self.dev {
                Identity::Exact
            } else {
                Identity::SameInode
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = meta;
            Identity::Different
        }
    }
}

/// How confidently a candidate file is the mapped one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Identity {
    /// Another file entirely: a same-named binary in the agent's own image.
    Different,
    /// Same inode, device reported differently — an overlayfs doing its job.
    SameInode,
    /// Same inode and same device.
    Exact,
}

struct MapsCache {
    at: Instant,
    entries: Arc<Vec<Mapping>>,
}

/// One name range inside a binary.
struct Sym {
    start: u64,
    end: u64,
    name: Box<str>,
}

/// A binary with its symbols, in the file's own address space.
struct Module {
    loads: Vec<LoadSegment>,
    syms: Vec<Sym>,
    go: Option<GoTable>,
    /// `go1.x.y` from the binary's build info, when it carries one. Purely
    /// diagnostic: it is the answer to "is this the Go release we tested".
    go_version: Option<String>,
}

/// Where a name came from, so the collector can report coverage per source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// ELF `.symtab` / `.dynsym`.
    Symbols,
    /// Stripped binary named through its Go function table.
    Go,
}

/// A resolved name plus the file it came from.
pub(crate) struct Resolved {
    pub module: String,
    pub name: String,
    pub source: Source,
}

impl Module {
    /// Parse `data` as an ELF file. Returns `None` for anything that is not a
    /// readable ELF or carries no symbols at all.
    fn parse(data: &[u8], path: &Path) -> Option<Module> {
        let file = object::File::parse(data).ok()?;
        let text = file
            .section_by_name(".text")
            .map(|section| section.address())
            // A binary whose section headers were removed still has program
            // headers, and its code lives in the executable `PT_LOAD`. Without
            // this, a section-stripped PIE has no `text` at all: the header's
            // `textStart` is a relocation placeholder there, so the Go table
            // would parse and then answer nothing.
            .or_else(|| {
                file.segments()
                    .find(|segment| {
                        matches!(
                            segment.flags(),
                            object::SegmentFlags::Elf { p_flags, .. }
                                if p_flags.contains(object::elf::PF_X)
                        )
                    })
                    .map(|segment| segment.address())
            })
            .unwrap_or(0);
        let loads = file
            .segments()
            .map(|segment| {
                let (offset, filesz) = segment.file_range();
                LoadSegment {
                    offset,
                    vaddr: segment.address(),
                    filesz,
                }
            })
            .collect::<Vec<_>>();
        let syms = collect_symbols(&file);
        // A section header is the fast path; a binary whose section headers
        // were removed (or renamed by a packer) still has the table, so fall
        // back to finding it by its magic.
        let go = file
            .section_by_name(".gopclntab")
            .or_else(|| {
                file.sections()
                    .find(|section| section.name().is_ok_and(|name| name.ends_with("gopclntab")))
            })
            .and_then(|section| section.data().ok())
            .and_then(|blob| GoTable::parse(blob, text))
            .or_else(|| GoTable::scan(data, text));
        let go_version = go.as_ref().and_then(|_| {
            file.section_by_name(".go.buildinfo")
                .and_then(|section| section.data().ok())
                .and_then(go_version_of)
                .or_else(|| go_version_of(data))
        });
        if syms.is_empty() && go.is_none() {
            tracing::debug!(
                path = %path.display(),
                loads = loads.len(),
                text,
                "module has no symbol table and no Go function table"
            );
            if !is_interesting(path) {
                // Nothing useful and nothing to report: a kernel-provided vDSO
                // or a resource blob that happens to have an ELF header.
                return None;
            }
        }
        Some(Module {
            loads,
            syms,
            go,
            go_version,
        })
    }

    /// Offset to subtract from a runtime address to land in the file's own
    /// address space.
    ///
    /// `/proc/<pid>/maps` only gives `(start, offset)` per mapping, which is
    /// not enough on its own: for a non-PIE executable `start - offset` is not
    /// zero but the load address, so the naive `addr - start + offset` maps
    /// every address to near zero. The program headers disambiguate: they say
    /// which `p_vaddr` a file offset belongs to.
    fn bias(&self, map_start: u64, map_offset: u64) -> Option<u64> {
        if let Some(segment) = self.loads.iter().find(|segment| {
            map_offset >= segment.offset
                && map_offset < segment.offset.saturating_add(segment.filesz)
        }) {
            let vaddr_at_map_start = segment
                .vaddr
                .checked_add(map_offset.checked_sub(segment.offset)?)?;
            if let Some(bias) = map_start.checked_sub(vaddr_at_map_start) {
                return Some(bias);
            }
        }
        // Every sane ELF keeps `p_vaddr - p_offset` constant, so the mapping
        // itself is a usable answer when the program headers are unreadable.
        map_start.checked_sub(map_offset)
    }

    /// Name of the function containing the file-relative address `vaddr`.
    fn lookup(&self, vaddr: u64) -> Option<(&str, Source)> {
        // No early return here: a stripped Go binary has an empty symbol table
        // and a stripped image has a handful of `.dynsym` entries, and in both
        // cases the address can sit below the first symbol. Bailing out at that
        // point would skip the Go table, which is the only source those
        // binaries have.
        let index = self.syms.partition_point(|sym| sym.start <= vaddr);
        if let Some(sym) = index.checked_sub(1).and_then(|prev| self.syms.get(prev))
            && vaddr < sym.end
        {
            return Some((&sym.name, Source::Symbols));
        }
        self.go
            .as_ref()
            .and_then(|table| table.lookup(vaddr))
            .map(|name| (name, Source::Go))
    }
}

/// Whether a binary is worth a warning when nothing could be read from it.
fn is_interesting(path: &Path) -> bool {
    !path
        .to_str()
        .is_some_and(|path| path.starts_with("/memfd:") || path.starts_with("/SYSV"))
}

fn collect_symbols(file: &object::File<'_>) -> Vec<Sym> {
    let mut entries: Vec<(u64, Box<str>)> = Vec::new();
    for sym in file.symbols().chain(file.dynamic_symbols()) {
        if sym.is_undefined() || sym.address() == 0 {
            continue;
        }
        if !matches!(
            sym.kind(),
            object::SymbolKind::Text | object::SymbolKind::Unknown
        ) {
            continue;
        }
        let Ok(name) = sym.name() else { continue };
        if name.is_empty() {
            continue;
        }
        entries.push((sym.address(), name.into()));
        if entries.len() >= MAX_SYMBOLS {
            break;
        }
    }
    entries.sort_unstable_by_key(|(address, _)| *address);
    entries.dedup_by_key(|(address, _)| *address);
    let mut syms = Vec::with_capacity(entries.len());
    for (index, (start, name)) in entries.iter().enumerate() {
        let end = entries.get(index + 1).map_or(u64::MAX, |(next, _)| *next);
        syms.push(Sym {
            start: *start,
            end,
            name: name.clone(),
        });
    }
    syms
}

/// `go1.x.y` from a Go binary's build info.
///
/// The `.go.buildinfo` section stores either inline length-prefixed strings or
/// two pointers into the image; a PIE relocates the pointers and a packer
/// renames the section, so neither shape is something to rely on. The version
/// string is what answers "is this the Go release the symbolizer was tested
/// with", and scanning for the token finds it in both shapes.
fn go_version_of(data: &[u8]) -> Option<String> {
    const NEEDLE: &[u8] = b"go1.";
    let mut offset = 0usize;
    while offset + NEEDLE.len() <= data.len() {
        let found = data[offset..]
            .windows(NEEDLE.len())
            .position(|window| window == NEEDLE)?;
        let start = offset + found;
        let end = data[start..start + 24.min(data.len() - start)]
            .iter()
            .position(|byte| !(byte.is_ascii_digit() || *byte == b'.' || byte.is_ascii_lowercase()))
            .map(|len| start + len)
            .unwrap_or(data.len());
        let token = &data[start..end];
        if let Ok(text) = std::str::from_utf8(token)
            && text.len() >= 6
            && text.len() <= 24
            && text.as_bytes()[4].is_ascii_digit()
            && !text.ends_with('.')
        {
            return Some(text.to_string());
        }
        offset = start + NEEDLE.len();
    }
    None
}

/// [`go_version_of`], for tests in other modules of this crate.
#[cfg(test)]
pub(crate) fn go_version_of_for_test(data: &[u8]) -> Option<String> {
    go_version_of(data)
}

/// `comm` of a pid, trimmed, for a sample line that survives the process.
fn process_name(tgid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{tgid}/comm"))
        .map(|name| name.trim().to_string())
        .unwrap_or_default()
}

fn read_maps(tgid: u32) -> Option<Vec<Mapping>> {
    let text = std::fs::read_to_string(format!("/proc/{tgid}/maps")).ok()?;
    let mut out = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(range) = fields.next() else { continue };
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
        else {
            continue;
        };
        // perms, then offset, dev, inode, then the path.
        let Some(perms) = fields.next().filter(|perms| perms.len() == 4) else {
            continue;
        };
        let Some(offset) = fields.next().and_then(|v| u64::from_str_radix(v, 16).ok()) else {
            continue;
        };
        let Some(dev) = fields.next().and_then(parse_dev) else {
            continue;
        };
        let Some(inode) = fields.next().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        let path: String = fields.collect::<Vec<_>>().join(" ");
        // `[heap]`, `[stack]`, `[vdso]`, and anonymous mappings have no file.
        if path.is_empty() || path.starts_with('[') || inode == 0 {
            continue;
        }
        let mut flags = [b'-'; 4];
        flags.copy_from_slice(&perms.as_bytes()[..4]);
        out.push(Mapping {
            start,
            end,
            perms: flags,
            offset,
            dev,
            inode,
            path,
        });
    }
    out.sort_unstable_by_key(|mapping| mapping.start);
    Some(out)
}

fn parse_dev(value: &str) -> Option<u64> {
    let (major, minor) = value.split_once(':')?;
    let major = u64::from_str_radix(major, 16).ok()?;
    let minor = u64::from_str_radix(minor, 16).ok()?;
    Some(major << 32 | minor)
}

/// `(path, how)` pairs to try, most authoritative first.
///
/// `/proc/<pid>/root` leads deliberately. The bare path looks cheapest but is
/// the one route that can silently find the *wrong* file: an agent image ships
/// `/usr/local/bin/lidlesseye`, and so does every pod built from the same
/// name — opening the path directly hands back the agent's own binary, whose
/// symbol table then answers nothing for the target's addresses. The target's
/// namespace is tried first so the bare path is only reached when the agent
/// genuinely shares it.
fn candidates(tgid: u32, mapping: &Mapping) -> Vec<(String, &'static str)> {
    // A deleted file still has a live inode; the suffix is not part of the name.
    let path = mapping
        .path
        .strip_suffix(" (deleted)")
        .unwrap_or(&mapping.path);
    let mut out = Vec::with_capacity(2 + HOST_ROOTS.len());
    if path.starts_with('/') {
        out.push((format!("/proc/{tgid}/root{path}"), "proc-root"));
        for root in HOST_ROOTS {
            out.push((format!("{root}{path}"), "host-root"));
        }
    }
    out.push((path.to_string(), "path"));
    out.push((
        format!(
            "/proc/{tgid}/map_files/{:x}-{:x}",
            mapping.start, mapping.end
        ),
        "map-files",
    ));
    out
}

/// Open the file behind `mapping`, preferring one whose identity matches.
///
/// Returns the file, a tag naming the route that worked, and whether the
/// `(dev, inode)` check passed.
fn open_module(tgid: u32, mapping: &Mapping) -> Option<(File, &'static str, Identity)> {
    let mut best: Option<(File, &'static str, Identity)> = None;
    for (path, how) in candidates(tgid, mapping) {
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let Ok(meta) = file.metadata() else { continue };
        if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_MODULE_BYTES {
            continue;
        }
        let identity = mapping.identity(&meta);
        if identity != Identity::Different {
            // Same inode. A device that disagrees is an overlayfs quirk, not a
            // reason to keep looking, and `map_files` cannot do better.
            return Some((file, how, identity));
        }
        // A namespaced route that opens *something* beats nothing, so it is
        // kept as a last resort. A bare path that opens a different file is not
        // a candidate at all: it is almost certainly the agent's own copy.
        if how != "path" && best.is_none() {
            best = Some((file, how, identity));
        }
    }
    best
}

fn read_file(mut file: File, how: &'static str) -> Option<Vec<u8>> {
    let len = file.metadata().ok()?.len();
    let mut data = Vec::with_capacity(len as usize);
    file.read_to_end(&mut data).ok()?;
    if data.is_empty() {
        tracing::debug!(route = how, "module file is empty");
        return None;
    }
    Some(data)
}

/// One cache slot. `module` is `None` for "nothing useful here", which is
/// cached so a symbol-less library is not re-read for every unknown address.
struct CachedModule {
    module: Option<Arc<Module>>,
    /// Monotonic use counter, for eviction.
    used: u64,
    /// Which route opened the file, for diagnostics.
    route: &'static str,
}

/// Why frames from one binary stay unnamed.
///
/// A wall of hex on a dashboard is not a rendering bug: either the file could
/// not be opened at all, or it was read and simply has nothing to say about
/// those addresses (stripped, no Go table, or a stale build). The two need
/// different fixes, and on a cluster the user cannot shell into, the agent has
/// to name the binary and the reason for them.
/// One concrete address that stayed an address, with everything needed to
/// reproduce it on the node: the process, the instruction pointer, and what
/// `/proc/<pid>/maps` said about that pointer.
///
/// A per-binary count says "this file is a problem"; a sample says *which*
/// address, in *which* process, hit *which* mapping — which is the difference
/// between a report one can act on and one that needs another round trip.
#[derive(Clone)]
pub(crate) struct MissSample {
    pub pid: u32,
    /// `/proc/<pid>/comm`, so the line names the process, not just a pid that
    /// may already be gone by the time somebody reads it.
    pub comm: String,
    pub ip: u64,
    /// The mapping `/proc/<pid>/maps` returned for `ip`, or all zeroes when
    /// the address is in no file-backed mapping at all.
    pub perms: [u8; 4],
    pub start: u64,
    pub end: u64,
    pub offset: u64,
    /// `major << 32 | minor`, as `maps` prints it.
    pub dev: u64,
    pub inode: u64,
    /// Address in the file's own space, when a module was parsed.
    pub vaddr: Option<u64>,
    pub cause: &'static str,
    pub path: String,
}

/// What a caller knows about one unnamed address beyond the mapping itself.
struct Miss<'a> {
    module: Option<&'a Module>,
    vaddr: Option<u64>,
    cause: &'static str,
    route: &'static str,
}

/// How many addresses per binary are kept as samples.
const MAX_SAMPLES: usize = 3;

#[derive(Clone)]
pub(crate) struct ModuleIssue {
    pub path: String,
    /// `go1.x.x` when the binary carries build info: the first thing to check
    /// when a Go service still shows addresses.
    pub go_version: Option<String>,
    /// ELF symbols the file offers, and whether it had a Go function table.
    pub symbols: usize,
    pub has_go: bool,
    /// Addresses that landed in this binary and named nothing.
    pub count: u64,
    /// The file was never readable, so `symbols`/`has_go` say nothing.
    pub unreadable: bool,
    /// Route that opened the file, for the readable case.
    pub route: &'static str,
    /// Whether this binary was already named in the log.
    pub reported: bool,
    /// A few concrete unnamed addresses, for reproducing the problem.
    pub samples: Vec<MissSample>,
}

/// How many binaries get an issue entry. A node runs a few hundred binaries and
/// the log has to stay readable.
const MAX_ISSUES: usize = 32;

/// Per-pid map snapshots plus one parsed [`Module`] per binary.
#[derive(Default)]
pub(crate) struct ModuleCache {
    maps: HashMap<u32, MapsCache>,
    /// `(dev, inode)` → module.
    modules: HashMap<(u64, u64), CachedModule>,
    /// Binaries that produced unnamed frames, keyed by path.
    issues: HashMap<String, ModuleIssue>,
    /// Last value handed out by the use counter.
    tick: u64,
    /// Paths already reported as unreadable, so the warning appears once.
    reported: HashSet<String>,
    opened: u64,
    failed: u64,
}

impl ModuleCache {
    pub(crate) fn new() -> Self {
        ModuleCache::default()
    }

    /// Forget processes that are no longer sampled. Their maps are useless once
    /// the pid is gone, and a recycled pid would otherwise inherit the address
    /// ranges of its predecessor.
    pub(crate) fn retain_processes(&mut self, alive: &HashSet<u32>) {
        self.maps.retain(|tgid, _| alive.contains(tgid));
    }

    /// Distinct binaries parsed so far, and mappings whose file could not be
    /// opened by any route. The second number is the one that explains a wall
    /// of `[unknown]` frames.
    pub(crate) fn counts(&self) -> (u64, u64) {
        (self.opened, self.failed)
    }

    /// Resolve `addr` of process `tgid` to a symbol name.
    pub(crate) fn resolve(&mut self, tgid: u32, addr: u64) -> Option<Resolved> {
        let Some(mapping) = self.mapping_for(tgid, addr) else {
            self.record_unmapped(tgid, addr);
            return None;
        };
        let key = (mapping.dev, mapping.inode);
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        if !self.modules.contains_key(&key) {
            let (module, route) = self.load(tgid, &mapping);
            self.insert(key, module.map(Arc::new), tick, route);
        }
        // The entry is cloned out before anything else is recorded: the issue
        // map needs `&mut self` while the module cache holds a borrow.
        let (module, route) = {
            let entry = self.modules.get_mut(&key)?;
            entry.used = tick;
            (entry.module.clone(), entry.route)
        };
        let Some(module) = module else {
            self.record(
                &mapping,
                tgid,
                addr,
                Miss {
                    module: None,
                    vaddr: None,
                    cause: "the file could not be opened through any route",
                    route: "none",
                },
            );
            return None;
        };
        let Some(vaddr) = module
            .bias(mapping.start, mapping.offset)
            .and_then(|bias| addr.checked_sub(bias))
        else {
            self.record(
                &mapping,
                tgid,
                addr,
                Miss {
                    module: None,
                    vaddr: None,
                    cause: "the load bias could not be computed",
                    route,
                },
            );
            return None;
        };
        let Some((name, source)) = module.lookup(vaddr) else {
            let cause = if module.syms.is_empty() && module.go.is_none() {
                "no ELF symbol table and no Go function table"
            } else if module.go.is_some() {
                "the address is outside the Go function table"
            } else {
                "the address is outside every ELF symbol"
            };
            self.record(
                &mapping,
                tgid,
                addr,
                Miss {
                    module: Some(&module),
                    vaddr: Some(vaddr),
                    cause,
                    route,
                },
            );
            return None;
        };
        Some(Resolved {
            module: mapping.path,
            name: name.to_string(),
            source,
        })
    }

    /// Remember one address that produced no name, with its mapping.
    fn record(&mut self, mapping: &Mapping, tgid: u32, addr: u64, miss: Miss<'_>) {
        if self.issues.len() >= MAX_ISSUES && !self.issues.contains_key(&mapping.path) {
            return;
        }
        let sample = MissSample {
            pid: tgid,
            comm: process_name(tgid),
            ip: addr,
            perms: mapping.perms,
            start: mapping.start,
            end: mapping.end,
            offset: mapping.offset,
            dev: mapping.dev,
            inode: mapping.inode,
            vaddr: miss.vaddr,
            // An IP in a mapping without `x` never executed. Saying so on the
            // sample separates "this file has no symbols" (a build problem)
            // from "the stack walk produced this value" (an unwinding problem)
            // — the two look identical in a flamegraph.
            cause: if mapping.executable() {
                miss.cause
            } else {
                "the address is in a mapping that never executes (broken stack walk)"
            },
            path: mapping.path.clone(),
        };
        let issue = self
            .issues
            .entry(mapping.path.clone())
            .or_insert_with(|| ModuleIssue {
                path: mapping.path.clone(),
                go_version: miss.module.and_then(|module| module.go_version.clone()),
                symbols: miss.module.map_or(0, |module| module.syms.len()),
                has_go: miss.module.is_some_and(|module| module.go.is_some()),
                count: 0,
                unreadable: miss.module.is_none(),
                route: miss.route,
                reported: false,
                samples: Vec::new(),
            });
        issue.count += 1;
        if issue.samples.len() < MAX_SAMPLES {
            issue.samples.push(sample);
        }
    }

    /// Remember an address that is in no file-backed mapping at all.
    ///
    /// These never reach a module, so they need a bucket of their own: a
    /// flamegraph full of addresses with no file behind them is a JIT runtime
    /// without a perf map, or a stack that was corrupted.
    fn record_unmapped(&mut self, tgid: u32, addr: u64) {
        let path = "[unmapped]".to_string();
        if self.issues.len() >= MAX_ISSUES && !self.issues.contains_key(&path) {
            return;
        }
        let issue = self
            .issues
            .entry(path.clone())
            .or_insert_with(|| ModuleIssue {
                path: path.clone(),
                go_version: None,
                symbols: 0,
                has_go: false,
                count: 0,
                unreadable: false,
                route: "none",
                reported: false,
                samples: Vec::new(),
            });
        issue.count += 1;
        if issue.samples.len() < MAX_SAMPLES {
            issue.samples.push(MissSample {
                pid: tgid,
                comm: process_name(tgid),
                ip: addr,
                perms: *b"----",
                start: 0,
                end: 0,
                offset: 0,
                dev: 0,
                inode: 0,
                vaddr: None,
                cause: "the address is in no file-backed mapping",
                path,
            });
        }
    }

    /// File backing `addr` in process `tgid`, whether or not it has symbols.
    ///
    /// Used to label frames that could not be symbolized: an address without a
    /// name is still worth attributing to a library.
    pub(crate) fn module_for(&mut self, tgid: u32, addr: u64) -> Option<String> {
        self.mapping_for(tgid, addr).map(|mapping| mapping.path)
    }

    /// Cache `module`, evicting the least recently used entry when full.
    fn insert(
        &mut self,
        key: (u64, u64),
        module: Option<Arc<Module>>,
        used: u64,
        route: &'static str,
    ) {
        if self.modules.len() >= MAX_MODULES {
            let oldest = self
                .modules
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(oldest, _)| *oldest);
            if let Some(oldest) = oldest {
                self.modules.remove(&oldest);
            }
        }
        self.modules.insert(
            key,
            CachedModule {
                module,
                used,
                route,
            },
        );
    }

    fn load(&mut self, tgid: u32, mapping: &Mapping) -> (Option<Module>, &'static str) {
        let Some((file, how, identity)) = open_module(tgid, mapping) else {
            self.report(tgid, mapping);
            return (None, "none");
        };
        let Some(data) = read_file(file, how) else {
            return (None, how);
        };
        let module = Module::parse(&data, Path::new(&mapping.path));
        if identity == Identity::Different {
            tracing::debug!(
                path = %mapping.path,
                route = how,
                "module identity unverified, symbols may belong to a different build"
            );
        }
        if let Some(module) = &module {
            self.opened += 1;
            tracing::debug!(
                path = %mapping.path,
                route = how,
                symbols = module.syms.len(),
                go = module.go.is_some(),
                "module parsed"
            );
        }
        (module, how)
    }

    /// Binaries that produced unnamed frames, worst first.
    pub(crate) fn issues(&self) -> Vec<ModuleIssue> {
        let mut out: Vec<ModuleIssue> = self.issues.values().cloned().collect();
        out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));
        out
    }

    /// The issues that have not been logged yet, marked as reported.
    ///
    /// One line per binary is what makes a remote cluster diagnosable: the user
    /// pastes the agent log and the message already says which file, which Go
    /// release and which cause.
    pub(crate) fn take_new_issues(&mut self, max: usize) -> Vec<ModuleIssue> {
        let mut fresh: Vec<ModuleIssue> = self
            .issues
            .values_mut()
            .filter(|issue| !issue.reported)
            .map(|issue| {
                issue.reported = true;
                issue.clone()
            })
            .collect();
        fresh.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));
        fresh.truncate(max);
        fresh
    }

    /// Name the path and the routes that were tried, once per binary.
    fn report(&mut self, tgid: u32, mapping: &Mapping) {
        self.failed += 1;
        if self.issues.len() < MAX_ISSUES
            && let std::collections::hash_map::Entry::Vacant(slot) =
                self.issues.entry(mapping.path.clone())
        {
            slot.insert(ModuleIssue {
                path: mapping.path.clone(),
                go_version: None,
                symbols: 0,
                has_go: false,
                count: 0,
                unreadable: true,
                route: "none",
                reported: false,
                samples: Vec::new(),
            });
        }
        if self.reported.len() >= MAX_REPORTED || !self.reported.insert(mapping.path.clone()) {
            return;
        }
        let tried = candidates(tgid, mapping)
            .into_iter()
            .map(|(_, how)| how)
            .collect::<Vec<_>>()
            .join(",");
        tracing::warn!(
            path = %mapping.path,
            pid = tgid,
            routes = %tried,
            "cannot read the binary behind a mapping; frames from it stay unnamed"
        );
    }

    fn mapping_for(&mut self, tgid: u32, addr: u64) -> Option<Mapping> {
        let stale = self
            .maps
            .get(&tgid)
            .is_none_or(|cache| cache.at.elapsed() > MAPS_TTL);
        if stale {
            let entries = read_maps(tgid).unwrap_or_default();
            if self.maps.len() >= MAX_MAPS {
                self.maps.clear();
            }
            self.maps.insert(
                tgid,
                MapsCache {
                    at: Instant::now(),
                    entries: Arc::new(entries),
                },
            );
        }
        let cache = self.maps.get(&tgid)?;
        // `/proc/<pid>/maps` is sorted by address.
        let index = cache
            .entries
            .partition_point(|mapping| mapping.start <= addr);
        let mapping = cache.entries.get(index.checked_sub(1)?)?;
        if addr >= mapping.end {
            return None;
        }
        Some(mapping.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_numbers_round_trip() {
        assert_eq!(parse_dev("fd:01"), Some((0xfd << 32) | 0x01));
        assert_eq!(parse_dev("00:00"), Some(0));
        assert_eq!(parse_dev("nope"), None);
    }

    /// A count says "this file is a problem"; the sample says which process and
    /// which instruction pointer, which is what makes it reproducible.
    #[test]
    fn unmapped_addresses_are_recorded_with_pid_and_ip() {
        let mut cache = ModuleCache::new();
        let pid = std::process::id();
        for ip in [0xdead_beef_u64, 0xfeed_face, 0x1000, 0x2000, 0x3000] {
            cache.record_unmapped(pid, ip);
        }
        let issues = cache.issues();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "[unmapped]");
        assert_eq!(issues[0].count, 5, "every address is counted");
        assert_eq!(
            issues[0].samples.len(),
            MAX_SAMPLES,
            "but only a few are kept"
        );
        let sample = &issues[0].samples[0];
        assert_eq!(sample.pid, pid);
        assert_eq!(sample.ip, 0xdead_beef);
        assert!(
            !sample.comm.is_empty(),
            "the process is named, not just a pid"
        );
        assert_eq!(sample.inode, 0, "there is no file behind it");
    }

    /// An address in a read-only data mapping is not an instruction pointer:
    /// the sample has to say so, or a broken stack walk looks like a missing
    /// symbol table.
    #[test]
    fn a_sample_in_a_non_executable_mapping_blames_the_stack_walk() {
        let mut cache = ModuleCache::new();
        let mapping = Mapping {
            start: 0x7f00_0000,
            end: 0x7f01_0000,
            perms: *b"rw-p",
            offset: 0x1000,
            dev: 0x8_0000_0001,
            inode: 42,
            path: "/usr/bin/app".to_string(),
        };
        cache.record(
            &mapping,
            std::process::id(),
            0x7f00_1234,
            Miss {
                module: None,
                vaddr: Some(0x1234),
                cause: "no ELF symbol table and no Go function table",
                route: "proc-root",
            },
        );
        let issues = cache.issues();
        assert_eq!(issues.len(), 1);
        let sample = &issues[0].samples[0];
        assert_eq!(sample.perms, *b"rw-p");
        assert_eq!(sample.offset, 0x1000);
        assert_eq!(sample.inode, 42);
        assert!(
            sample.cause.contains("never executes"),
            "cause was {}",
            sample.cause
        );
    }

    #[test]
    fn reads_a_go_version_token_from_build_info() {
        // Pointer-shaped build info, plus the inline-string shape Go uses when
        // the version is baked in; both contain the token, and that is all the
        // diagnostic needs.
        let mut data = b"\xff Go buildinf:".to_vec();
        data.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        data.extend_from_slice(b"\x07go1.22.3\x00");
        assert_eq!(go_version_of(&data).as_deref(), Some("go1.22.3"));
        // A `go1.` that is not a version never matches.
        assert_eq!(go_version_of(b"go1.txt"), None);
        assert_eq!(go_version_of(b"nothing here"), None);
    }

    #[test]
    fn bias_handles_pie_and_flat_executables() {
        // Non-PIE: the first PT_LOAD maps file offset 0 to vaddr 0x400000 and
        // the kernel maps it at 0x400000, so nothing is relocated.
        let flat = Module {
            loads: vec![LoadSegment {
                offset: 0,
                vaddr: 0x40_0000,
                filesz: 0x10_0000,
            }],
            syms: Vec::new(),
            go: None,
            go_version: None,
        };
        assert_eq!(flat.bias(0x40_0000, 0), Some(0));
        assert_eq!(flat.bias(0x41_2000, 0x1_2000), Some(0));

        // PIE: the same segment starts at vaddr 0 and is mapped at an arbitrary
        // base, so every address is shifted by that base.
        let pie = Module {
            loads: vec![LoadSegment {
                offset: 0,
                vaddr: 0,
                filesz: 0x10_0000,
            }],
            syms: Vec::new(),
            go: None,
            go_version: None,
        };
        let base = 0x55f0_c000_0000;
        assert_eq!(pie.bias(base, 0), Some(base));
        // A later mapping of the same file (offset 0x1000, mapped at base+0x1000).
        assert_eq!(pie.bias(base + 0x1000, 0x1000), Some(base));
    }

    #[test]
    fn bias_falls_back_to_the_mapping_when_segments_are_missing() {
        let module = Module {
            loads: Vec::new(),
            syms: Vec::new(),
            go: None,
            go_version: None,
        };
        assert_eq!(module.bias(0x1000_5000, 0x5000), Some(0x1000_0000));
    }

    #[test]
    fn symbols_are_looked_up_by_range() {
        let module = Module {
            loads: Vec::new(),
            syms: vec![
                Sym {
                    start: 0x100,
                    end: 0x180,
                    name: "a".into(),
                },
                Sym {
                    start: 0x180,
                    end: 0x200,
                    name: "b".into(),
                },
            ],
            go: None,
            go_version: None,
        };
        assert_eq!(module.lookup(0x100).map(|(name, _)| name), Some("a"));
        assert_eq!(module.lookup(0x17f).map(|(name, _)| name), Some("a"));
        assert_eq!(module.lookup(0x180).map(|(name, _)| name), Some("b"));
        assert_eq!(module.lookup(0xff), None);
        assert_eq!(module.lookup(0x1ff).map(|(name, _)| name), Some("b"));
    }

    #[test]
    fn an_empty_symbol_table_falls_through_to_the_go_table() {
        // The shape of every stripped Go binary on a node: no `.symtab`, no
        // `.dynsym`, only the function table. Returning early on "no symbol
        // covers this address" made all of them unresolvable.
        let module = Module {
            loads: Vec::new(),
            syms: Vec::new(),
            go: Some(GoTable::for_test(0x401000, 0, 0x2000, "main.hotA")),
            go_version: None,
        };
        assert_eq!(
            module.lookup(0x401100),
            Some(("main.hotA", Source::Go)),
            "an empty ELF symbol table must not shadow the Go table"
        );
    }

    #[test]
    fn symbols_below_the_first_entry_fall_through_to_the_go_table() {
        // A stripped binary keeps a handful of `.dynsym` entries, all of them
        // above the running code, so a lookup can land before the first one.
        let module = Module {
            loads: Vec::new(),
            syms: vec![Sym {
                start: 0x5000,
                end: 0x5100,
                name: "later".into(),
            }],
            go: Some(GoTable::for_test(0x401000, 0, 0x2000, "main.hotA")),
            go_version: None,
        };
        assert_eq!(module.lookup(0x401100), Some(("main.hotA", Source::Go)));
        assert_eq!(module.lookup(0x5050), Some(("later", Source::Symbols)));
    }

    #[test]
    fn candidates_cover_every_route() {
        let mapping = Mapping {
            start: 0x1000,
            end: 0x2000,
            perms: *b"r-xp",
            offset: 0,
            dev: 1,
            inode: 2,
            path: "/bin/app (deleted)".to_string(),
        };
        let paths = candidates(7, &mapping);
        let routes = paths.iter().map(|(_, how)| *how).collect::<Vec<_>>();
        // The target's own namespace leads: the bare path can resolve to the
        // agent's copy of a same-named binary.
        assert_eq!(routes.first(), Some(&"proc-root"));
        assert!(routes.contains(&"host-root"));
        assert!(routes.contains(&"path"));
        assert_eq!(routes.last(), Some(&"map-files"));
        // The `(deleted)` marker is not part of any path that is opened.
        assert_eq!(paths[0].0, "/proc/7/root/bin/app");
        assert!(routes.iter().position(|how| *how == "path").unwrap() < routes.len() - 1);
        assert!(
            paths
                .last()
                .unwrap()
                .0
                .ends_with("/proc/7/map_files/1000-2000")
        );
    }

    /// Live check against whatever this machine runs.
    ///
    /// The unit tests above prove the arithmetic; this proves the plumbing —
    /// maps, all four routes, load bias — on real processes. It needs root
    /// because it reads `/proc/<pid>/maps` of other processes.
    #[test]
    #[ignore = "requires root and profiled processes on the machine"]
    fn resolves_real_processes() {
        let mut cache = ModuleCache::new();
        let mut checked = 0;
        let mut resolved = 0;
        for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Ok(tgid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(comm) = std::fs::read_to_string(format!("/proc/{tgid}/comm")) else {
                continue;
            };
            let Some(maps) = read_maps(tgid) else {
                continue;
            };
            checked += 1;
            let mut hit = None;
            for mapping in &maps {
                // A file has several mappings; only the executable one covers
                // `.text`, and only its first page proves anything.
                let addr = mapping.start + 0x1000;
                if addr >= mapping.end {
                    continue;
                }
                if let Some(found) = cache.resolve(tgid, addr) {
                    hit = Some(found);
                    break;
                }
            }
            match hit {
                Some(found) => {
                    resolved += 1;
                    println!(
                        "{:>7} {:<20} {} -> {}",
                        tgid,
                        comm.trim(),
                        found.module,
                        found.name
                    );
                }
                None => println!("{:>7} {:<20} (no symbols)", tgid, comm.trim()),
            }
            if checked >= 40 {
                break;
            }
        }
        println!("checked {checked} processes, resolved {resolved}");
        assert!(checked > 0, "no processes with file mappings found");
    }
}
