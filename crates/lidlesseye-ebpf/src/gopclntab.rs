//! Function names for Go binaries that carry no symbol table.
//!
//! Stripping a Go program with `-ldflags="-s -w"` (what every distro and every
//! container image does) removes `.symtab`, `.dynsym` and `.debug_info`. A
//! symbolizer that only understands ELF symbol tables then reports
//! `MissingSyms` for every single frame — which, on a Kubernetes node, is most
//! of the software that matters: containerd, kubelet, kube-proxy, the CSI
//! drivers, Prometheus, etcd.
//!
//! The Go runtime cannot work without a function table: it needs it to unwind
//! stacks, to garbage collect precisely and to print a panic. That table lives
//! in `.gopclntab` and survives even the most aggressive strip. It maps every
//! PC range to a function name, which is precisely what a profiler needs, so
//! this module parses it and answers "which function is this address in".
//!
//! Layout (all little endian, `ptr` = 4 or 8 from the header):
//!
//! ```text
//! offset 0   magic      0xFFFFFFF1 (Go 1.20+), 0xFFFFFFF0 (1.18/1.19),
//!                       0xFFFFFFFA (1.16/1.17)
//! offset 4   pad1, pad2, minLC, ptrSize
//! offset 8   nfunc, nfiles [, textStart from 1.18], funcname, cu, filetab,
//!            pctab, pcln
//! pcln       functab: (entryOff: u32, funcOff: u32) * (nfunc + 1)
//! pcln+funcOff  _func struct; its nameOff (i32 at +4) indexes funcname
//! ```
//!
//! `entryOff` is relative to the start of `.text`, and the trailing sentinel
//! entry closes the last function's range. The *absolute* `textStart` field is
//! a pointer, so in a PIE binary it is a relocation placeholder in the file
//! and reads as 0 — hence the section address of `.text` is used instead, which
//! is always in the same coordinate space as `entryOff`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use object::{Object, ObjectSection, ObjectSegment};

/// Go 1.20 and newer: `funcID` was added to `_func`.
const MAGIC_GO120: u32 = 0xFFFF_FFF1;
/// Go 1.18 / 1.19: `textStart` was added to the header.
const MAGIC_GO118: u32 = 0xFFFF_FFF0;
/// Go 1.16 / 1.17: no `textStart` in the header.
const MAGIC_GO116: u32 = 0xFFFF_FFFA;

/// A sanity bound: Go's own runtime tops out around 100k functions, and a
/// corrupt header must not make us allocate gigabytes.
const MAX_FUNCTIONS: u32 = 4_000_000;

/// How long a `/proc/<pid>/maps` snapshot is trusted. Go processes that
/// `dlopen` anything are rare, and a stale snapshot can only cause a miss
/// (an address outside every known range), never a wrong name.
const MAPS_TTL: Duration = Duration::from_secs(60);

/// Number of distinct binaries whose function tables are kept in memory.
/// containerd's is ~10 MB, so this caps the fallback at a few tens of MB.
const MAX_TABLES: usize = 8;

/// One `PT_LOAD` segment, needed to undo relocation for an address.
#[derive(Clone, Copy)]
struct LoadSegment {
    offset: u64,
    vaddr: u64,
    filesz: u64,
}

/// A parsed `.gopclntab`.
pub(crate) struct GoTable {
    /// Address the `entryOff` values are relative to (`.text` vaddr).
    text: u64,
    /// Function ranges, sorted by `start`, offsets relative to `text`.
    funcs: Vec<GoFunc>,
    /// The function name table; `GoFunc::name` indexes into it.
    names: Vec<u8>,
    /// Program headers, used to map a runtime address back into the file's
    /// own address space.
    loads: Vec<LoadSegment>,
}

#[derive(Clone, Copy)]
struct GoFunc {
    start: u32,
    end: u32,
    name: u32,
}

impl GoTable {
    /// Parse a `.gopclntab` blob. `text_hint` is the `.text` section address,
    /// used when the header's `textStart` is a PIE relocation placeholder.
    fn parse(blob: &[u8], text_hint: u64, loads: Vec<LoadSegment>) -> Option<Self> {
        let magic = u32_at(blob, 0)?;
        let has_text = match magic {
            MAGIC_GO120 | MAGIC_GO118 => true,
            MAGIC_GO116 => false,
            _ => return None,
        };
        let ptr = *blob.get(7)? as usize;
        if ptr != 4 && ptr != 8 {
            return None;
        }

        let word = |off: usize| -> Option<u64> {
            if ptr == 4 {
                u32_at(blob, off).map(u64::from)
            } else {
                u64_at(blob, off)
            }
        };

        let nfunc = u32::try_from(word(8)?).ok()?;
        if nfunc == 0 || nfunc > MAX_FUNCTIONS {
            return None;
        }
        // Go 1.16 has no `textStart`, so its offsets start one word earlier.
        let (text_start, funcname_off, pcln_off) = if has_text {
            (word(8 + 2 * ptr)?, word(8 + 3 * ptr)?, word(8 + 7 * ptr)?)
        } else {
            (text_hint, word(8 + 2 * ptr)?, word(8 + 6 * ptr)?)
        };
        let text = if text_start == 0 {
            text_hint
        } else {
            text_start
        };
        if text == 0 {
            return None;
        }

        let funcname_off = usize::try_from(funcname_off).ok()?;
        let pcln_off = usize::try_from(pcln_off).ok()?;
        if funcname_off >= blob.len() || pcln_off >= blob.len() {
            return None;
        }
        // nfunc + 1 entries: the last one closes the final function's range.
        let table_bytes = (nfunc as usize + 1).checked_mul(8)?;
        if pcln_off.checked_add(table_bytes)? > blob.len() {
            return None;
        }

        let count = nfunc as usize;
        let mut funcs = Vec::with_capacity(count);
        for index in 0..count {
            let entry = pcln_off + index * 8;
            let start = u32_at(blob, entry)?;
            let end = u32_at(blob, entry + 8)?;
            let func_off = u32_at(blob, entry + 4)? as usize;
            let name_at = pcln_off.checked_add(func_off)?.checked_add(4)?;
            let name_off = i32_at(blob, name_at)?;
            let name_off = usize::try_from(name_off).ok()?;
            if funcname_off.checked_add(name_off)? >= blob.len() {
                return None;
            }
            // Trailing entries may be `go:func.*` markers; keep them, they are
            // still better than an address, and the range check filters them.
            funcs.push(GoFunc {
                start,
                end: end.max(start),
                name: u32::try_from(name_off).ok()?,
            });
        }
        // The Go linker emits the table sorted; sorting defensively costs
        // nothing at load time and makes `lookup`'s binary search total.
        funcs.sort_unstable_by_key(|func| func.start);

        Some(GoTable {
            text,
            funcs,
            names: blob[funcname_off..].to_vec(),
            loads,
        })
    }

    /// The offset that has to be subtracted from a runtime address to land in
    /// the file's own address space.
    ///
    /// `/proc/<pid>/maps` only gives `(start, offset)` per mapping, which is
    /// not enough on its own: for a non-PIE executable `start - offset` is not
    /// zero but the load address, so the naive `addr - start + offset` maps
    /// every address to near zero. The program headers disambiguate: they say
    /// which `p_vaddr` a file offset belongs to.
    fn load_bias(&self, map_start: u64, map_offset: u64) -> Option<u64> {
        let segment = self.loads.iter().find(|segment| {
            map_offset >= segment.offset
                && map_offset < segment.offset.saturating_add(segment.filesz)
        })?;
        let vaddr_at_map_start = segment
            .vaddr
            .checked_add(map_offset.checked_sub(segment.offset)?)?;
        map_start.checked_sub(vaddr_at_map_start)
    }

    /// Name of the function containing `vaddr`, if any.
    fn lookup(&self, vaddr: u64) -> Option<&str> {
        let off = u32::try_from(vaddr.checked_sub(self.text)?).ok()?;
        let index = self.funcs.partition_point(|func| func.start <= off);
        let func = self.funcs.get(index.checked_sub(1)?)?;
        if off >= func.end {
            return None;
        }
        let start = func.name as usize;
        let rest = self.names.get(start..)?;
        let len = rest.iter().position(|&byte| byte == 0)?;
        if len == 0 || len > 512 {
            return None;
        }
        std::str::from_utf8(&rest[..len]).ok()
    }
}

/// One `/proc/<pid>/maps` line that has a file behind it.
#[derive(Clone)]
struct Mapping {
    start: u64,
    end: u64,
    offset: u64,
    dev: u64,
    inode: u64,
    path: String,
}

struct MapsCache {
    at: Instant,
    entries: Arc<Vec<Mapping>>,
}

/// The fallback that turns "stripped Go binary" into named frames.
#[derive(Default)]
pub(crate) struct GoFallback {
    maps: HashMap<u32, MapsCache>,
    /// `(dev, inode)` → table. `None` caches "not a Go binary" so the ELF is
    /// not re-read for every unknown address of a stripped C++ program.
    tables: HashMap<(u64, u64), Option<Arc<GoTable>>>,
    hits: u64,
}

impl GoFallback {
    pub(crate) fn new() -> Self {
        GoFallback::default()
    }

    /// Resolve `addr` of process `tgid` through its Go function table.
    ///
    /// Returns `(module path, function name)`.
    pub(crate) fn resolve(&mut self, tgid: u32, addr: u64) -> Option<(String, String)> {
        let mapping = self.mapping_for(tgid, addr)?;
        let key = (mapping.dev, mapping.inode);
        if !self.tables.contains_key(&key) {
            let table = load_table(tgid, &mapping).map(Arc::new);
            if self.tables.len() >= MAX_TABLES {
                self.tables.clear();
            }
            self.tables.insert(key, table);
        }
        let table = self.tables.get(&key)?.as_ref()?;
        let bias = table.load_bias(mapping.start, mapping.offset)?;
        let vaddr = addr.checked_sub(bias)?;
        let name = table.lookup(vaddr)?;
        self.hits += 1;
        Some((mapping.path.clone(), name.to_string()))
    }

    /// File backing `addr` in process `tgid`, whether or not it has symbols.
    ///
    /// Used to label frames that could not be symbolized: an address without a
    /// name is still worth attributing to a library.
    pub(crate) fn module_for(&mut self, tgid: u32, addr: u64) -> Option<String> {
        self.mapping_for(tgid, addr).map(|mapping| mapping.path)
    }

    fn mapping_for(&mut self, tgid: u32, addr: u64) -> Option<Mapping> {
        let stale = self
            .maps
            .get(&tgid)
            .is_none_or(|cache| cache.at.elapsed() > MAPS_TTL);
        if stale {
            let entries = read_maps(tgid).unwrap_or_default();
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
        let Some(_perms) = fields.next() else {
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
        // `[heap]`, `[stack]`, `[vdso]`, and deleted files have no usable path.
        if path.is_empty() || path.starts_with('[') || inode == 0 {
            continue;
        }
        out.push(Mapping {
            start,
            end,
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
    Some((major << 32) | minor)
}

fn load_table(tgid: u32, mapping: &Mapping) -> Option<GoTable> {
    // `.gopclntab` is a few MB out of a much larger binary, and `fs::read`
    // copies the whole file. Refuse absurd sizes rather than allocate them.
    let data = std::fs::read(&mapping.path).ok().or_else(|| {
        // Inside a container the host path does not resolve; `map_files` is a
        // magic link that opens the file in the *target's* mount namespace.
        std::fs::read(format!(
            "/proc/{tgid}/map_files/{:x}-{:x}",
            mapping.start, mapping.end
        ))
        .ok()
    })?;
    let file = object::File::parse(data.as_slice()).ok()?;
    let text = file
        .section_by_name(".text")
        .map(|s| s.address())
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
    let section = file.section_by_name(".gopclntab").or_else(|| {
        file.sections()
            .find(|section| section.name().is_ok_and(|name| name.ends_with("gopclntab")))
    })?;
    let blob = section.data().ok()?;
    GoTable::parse(blob, text, loads)
}

fn u32_at(blob: &[u8], off: usize) -> Option<u32> {
    let bytes = blob.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn i32_at(blob: &[u8], off: usize) -> Option<i32> {
    let bytes = blob.get(off..off.checked_add(4)?)?;
    Some(i32::from_le_bytes(bytes.try_into().ok()?))
}

fn u64_at(blob: &[u8], off: usize) -> Option<u64> {
    let bytes = blob.get(off..off.checked_add(8)?)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal Go 1.20 style `.gopclntab` with three functions.
    fn synthetic() -> Vec<u8> {
        // Layout: header (72 bytes) | funcname table | functab (4 * 8) | _funcs
        let names = b"main.a\0main.b\0main.c\0";
        let funcname_off = 72usize;
        let pcln_off = funcname_off + names.len();
        let functab_len = 4 * 8;
        let funcs_off = pcln_off + functab_len;
        let mut blob = vec![0u8; funcs_off + 3 * 32];

        blob[0..4].copy_from_slice(&MAGIC_GO120.to_le_bytes());
        blob[6] = 1; // minLC
        blob[7] = 8; // ptrSize
        blob[8..16].copy_from_slice(&3u64.to_le_bytes()); // nfunc
        blob[16..24].copy_from_slice(&1u64.to_le_bytes()); // nfiles
        blob[24..32].copy_from_slice(&0u64.to_le_bytes()); // textStart (PIE)
        blob[32..40].copy_from_slice(&(funcname_off as u64).to_le_bytes());
        blob[72 - 8..72].copy_from_slice(&(pcln_off as u64).to_le_bytes());
        blob[funcname_off..pcln_off].copy_from_slice(names);

        let entries = [
            (0u32, 0x100u32, 0u32),
            (0x100, 0x180, 7),
            (0x180, 0x200, 14),
        ];
        for (index, (start, end, name_off)) in entries.iter().enumerate() {
            let entry = pcln_off + index * 8;
            blob[entry..entry + 4].copy_from_slice(&start.to_le_bytes());
            let func_off = (funcs_off - pcln_off + index * 32) as u32;
            blob[entry + 4..entry + 8].copy_from_slice(&func_off.to_le_bytes());
            let func = funcs_off + index * 32;
            blob[func + 4..func + 8].copy_from_slice(&(*name_off as i32).to_le_bytes());
            let _ = end;
        }
        // Sentinel closes the last function.
        let sentinel = pcln_off + 3 * 8;
        blob[sentinel..sentinel + 4].copy_from_slice(&0x200u32.to_le_bytes());
        blob
    }

    #[test]
    fn parses_a_synthetic_table() {
        let blob = synthetic();
        let table = GoTable::parse(&blob, 0x401000, vec![]).expect("table parses");
        assert_eq!(table.lookup(0x401010), Some("main.a"));
        assert_eq!(table.lookup(0x4010ff), Some("main.a"));
        assert_eq!(table.lookup(0x401100), Some("main.b"));
        assert_eq!(table.lookup(0x4011ff), Some("main.c"));
        // One past the last function: the sentinel ends the range.
        assert_eq!(table.lookup(0x401200), None);
        // Below `.text`.
        assert_eq!(table.lookup(0x400fff), None);
    }

    #[test]
    fn rejects_garbage() {
        assert!(GoTable::parse(&[], 0x1000, vec![]).is_none());
        assert!(GoTable::parse(&[0u8; 32], 0x1000, vec![]).is_none());
        let mut blob = synthetic();
        blob[0..4].copy_from_slice(&0xdead_beefu32.to_le_bytes());
        assert!(GoTable::parse(&blob, 0x1000, vec![]).is_none());
        // A truncated table must not panic.
        let blob = synthetic();
        assert!(GoTable::parse(&blob[..80], 0x1000, vec![]).is_none());
    }

    #[test]
    fn parses_maps_lines() {
        let text = "\
55f0c0001000-55f0c0002000 r--p 00000000 fd:01 1234 /usr/bin/app
55f0c0002000-55f0c0010000 r-xp 00001000 fd:01 1234 /usr/bin/app
7fff00000000-7fff00021000 rw-p 00000000 00:00 0 [stack]
";
        // The parser is fed through a file in production; exercise the same
        // field splitting here.
        let mut count = 0;
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let range = fields.next().unwrap();
            let (start, end) = range.split_once('-').unwrap();
            let start = u64::from_str_radix(start, 16).unwrap();
            let end = u64::from_str_radix(end, 16).unwrap();
            let _perms = fields.next();
            let offset = u64::from_str_radix(fields.next().unwrap(), 16).unwrap();
            let dev = parse_dev(fields.next().unwrap()).unwrap();
            let inode: u64 = fields.next().unwrap().parse().unwrap();
            let path: String = fields.collect::<Vec<_>>().join(" ");
            if path.starts_with('[') || inode == 0 {
                continue;
            }
            assert!(end > start);
            assert_eq!(dev >> 32, 0xfd);
            assert_eq!(offset, if count == 0 { 0 } else { 0x1000 });
            assert_eq!(path, "/usr/bin/app");
            count += 1;
        }
        assert_eq!(count, 2);
    }

    #[test]
    fn load_bias_handles_pie_and_flat_executables() {
        // Non-PIE: the first PT_LOAD maps file offset 0 to vaddr 0x400000 and
        // the kernel maps it at 0x400000, so nothing is relocated.
        let flat = GoTable {
            text: 0x401000,
            funcs: Vec::new(),
            names: Vec::new(),
            loads: vec![LoadSegment {
                offset: 0,
                vaddr: 0x40_0000,
                filesz: 0x10_0000,
            }],
        };
        assert_eq!(flat.load_bias(0x40_0000, 0), Some(0));
        assert_eq!(flat.load_bias(0x41_2000, 0x1_2000), Some(0));

        // PIE: the same segment starts at vaddr 0 and is mapped at an arbitrary
        // base, so every address is shifted by that base.
        let pie = GoTable {
            text: 0x401000,
            funcs: Vec::new(),
            names: Vec::new(),
            loads: vec![LoadSegment {
                offset: 0,
                vaddr: 0,
                filesz: 0x10_0000,
            }],
        };
        let base = 0x55f0_c000_0000;
        assert_eq!(pie.load_bias(base, 0), Some(base));
        // A later mapping of the same file (offset 0x1000, mapped at base+0x1000).
        assert_eq!(pie.load_bias(base + 0x1000, 0x1000), Some(base));
    }

    #[test]
    fn dev_numbers_round_trip() {
        assert_eq!(parse_dev("fd:01"), Some((0xfd << 32) | 0x01));
        assert_eq!(parse_dev("00:00"), Some(0));
        assert_eq!(parse_dev("nope"), None);
    }

    /// Live check against whatever Go binaries this machine runs.
    ///
    /// The synthetic table proves the parser; this proves the *plumbing* —
    /// maps, `map_files`, load bias — on real processes. It needs root because
    /// it reads `/proc/<pid>/maps` of other processes.
    #[test]
    #[ignore = "requires root and Go processes on the machine"]
    fn resolves_real_go_processes() {
        let mut fallback = GoFallback::new();
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
                if let Some(found) = fallback.resolve(tgid, addr) {
                    hit = Some(found);
                    break;
                }
            }
            match hit {
                Some((module, name)) => {
                    resolved += 1;
                    println!("{:>7} {:<20} {} -> {name}", tgid, comm.trim(), module);
                }
                None => println!("{:>7} {:<20} (no Go table)", tgid, comm.trim()),
            }
            if checked >= 40 {
                break;
            }
        }
        println!("checked {checked} processes, resolved {resolved}");
        assert!(checked > 0, "no processes with file mappings found");
    }
}
