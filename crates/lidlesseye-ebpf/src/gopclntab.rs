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
//! and reads as 0 — hence the `.text` section address is used instead, which is
//! always in the same coordinate space as `entryOff`.

/// Go 1.20 and newer: `funcID` was added to `_func`.
const MAGIC_GO120: u32 = 0xFFFF_FFF1;
/// Go 1.18 / 1.19: `textStart` was added to the header.
const MAGIC_GO118: u32 = 0xFFFF_FFF0;
/// Go 1.16 / 1.17: no `textStart` in the header.
const MAGIC_GO116: u32 = 0xFFFF_FFFA;

/// A sanity bound: Go's own runtime tops out around 100k functions, and a
/// corrupt header must not make us allocate gigabytes.
const MAX_FUNCTIONS: u32 = 4_000_000;

/// A parsed `.gopclntab`.
pub(crate) struct GoTable {
    /// Address the `entryOff` values are relative to (`.text` vaddr).
    text: u64,
    /// Function ranges, sorted by `start`, offsets relative to `text`.
    funcs: Vec<GoFunc>,
    /// The function name table; `GoFunc::name` indexes into it.
    names: Vec<u8>,
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
    pub(crate) fn parse(blob: &[u8], text_hint: u64) -> Option<Self> {
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
        })
    }

    /// Find the table in a binary whose section headers are gone.
    ///
    /// Packed and section-stripped Go binaries keep the table — the runtime
    /// needs it — but nothing in the ELF points at it any more. The header's
    /// magic is the only handle left, and it is specific enough that a
    /// validation pass over the surrounding bytes rules out the false positives
    /// that a scan through `.text` would otherwise produce.
    pub(crate) fn scan(data: &[u8], text_hint: u64) -> Option<Self> {
        let mut offset = 0usize;
        while let Some(found) = find_magic(&data[offset..]) {
            let start = offset.checked_add(found)?;
            if let Some(table) = GoTable::parse(&data[start..], text_hint) {
                // A real table names its first function with an identifier; a
                // random 4-byte match does not.
                if table.first_name_is_plausible() {
                    return Some(table);
                }
            }
            offset = start.checked_add(4)?;
            if offset >= data.len() {
                break;
            }
        }
        None
    }

    /// Name of the function containing `vaddr`, if any.
    pub(crate) fn lookup(&self, vaddr: u64) -> Option<&str> {
        let off = u32::try_from(vaddr.checked_sub(self.text)?).ok()?;
        let index = self.funcs.partition_point(|func| func.start <= off);
        let func = self.funcs.get(index.checked_sub(1)?)?;
        if off >= func.end {
            return None;
        }
        self.name_at(func.name)
    }

    fn name_at(&self, name: u32) -> Option<&str> {
        let rest = self.names.get(name as usize..)?;
        let len = rest.iter().position(|&byte| byte == 0)?;
        if len == 0 || len > 512 {
            return None;
        }
        std::str::from_utf8(&rest[..len]).ok()
    }

    /// A table with one function, for tests in other modules that need a
    /// module with a Go table but no ELF symbols.
    #[cfg(test)]
    pub(crate) fn for_test(text: u64, start: u32, end: u32, name: &str) -> Self {
        let mut names = name.as_bytes().to_vec();
        names.push(0);
        GoTable {
            text,
            funcs: vec![GoFunc {
                start,
                end,
                name: 0,
            }],
            names,
        }
    }

    fn first_name_is_plausible(&self) -> bool {
        // Go names are `pkg.Func`, `runtime.main`, `type:.eq.[8]uint64`, and
        // occasionally `go:buildid`. Anything with a space or control byte is
        // a misaligned read.
        self.funcs
            .first()
            .and_then(|func| self.name_at(func.name))
            .is_some_and(|name| {
                name.bytes()
                    .all(|byte| byte.is_ascii_graphic() || byte == b' ')
                    && !name.starts_with(' ')
            })
    }
}

/// Offset of the next Go table magic, if any.
fn find_magic(data: &[u8]) -> Option<usize> {
    const MAGICS: [[u8; 4]; 3] = [
        MAGIC_GO120.to_le_bytes(),
        MAGIC_GO118.to_le_bytes(),
        MAGIC_GO116.to_le_bytes(),
    ];
    MAGICS
        .iter()
        .filter_map(|magic| data.windows(4).position(|window| window == magic))
        .min()
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
    use object::{Object, ObjectSection};

    /// Parse real Go binaries from `LIDLESSEYE_GO_FIXTURES` and check that the
    /// table names the functions the program actually has.
    ///
    /// The synthetic table above pins the layout; this pins the *reader* against
    /// whatever the installed toolchain emits — including the PIE and stripped
    /// variants, which are the ones that show up as addresses in production.
    #[test]
    fn real_go_binaries_resolve_their_own_functions() {
        let Ok(dir) = std::env::var("LIDLESSEYE_GO_FIXTURES") else {
            return;
        };
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("fixture dir") {
            let path = entry.expect("dir entry").path();
            if !path.is_file() || path.extension().is_none_or(|ext| ext != "bin") {
                continue;
            }
            let data = std::fs::read(&path).expect("fixture");
            let file = object::File::parse(&*data).expect("elf");
            let text = file
                .section_by_name(".text")
                .map(|section| section.address())
                .unwrap_or(0);
            let table = file
                .section_by_name(".gopclntab")
                .or_else(|| {
                    file.sections().find(|section| {
                        section.name().is_ok_and(|name| name.ends_with("gopclntab"))
                    })
                })
                .and_then(|section| section.data().ok())
                .and_then(|blob| GoTable::parse(blob, text))
                .or_else(|| GoTable::scan(&data, text))
                .unwrap_or_else(|| panic!("{}: no Go table", path.display()));

            let mut names = Vec::new();
            for func in &table.funcs {
                if let Some(name) = table.name_at(func.name) {
                    names.push(name.to_string());
                }
            }
            for expected in ["main.hotA", "main.hotB", "runtime.main"] {
                assert!(
                    names.iter().any(|name| name == expected),
                    "{}: missing {expected} among {} names (text={:x})",
                    path.display(),
                    names.len(),
                    table.text
                );
            }
            // And the range lookup has to agree with the table's own entry.
            let func = table
                .funcs
                .iter()
                .find(|func| table.name_at(func.name) == Some("main.hotA"))
                .expect("main.hotA");
            assert_eq!(
                table.lookup(table.text + u64::from(func.start)),
                Some("main.hotA"),
                "{}: lookup misses its own entry",
                path.display()
            );
            // The build info the agent reports has to name the toolchain the
            // file was built with, or the log cannot answer "is this the Go
            // release we tested".
            let version = crate::module::go_version_of_for_test(&data)
                .unwrap_or_else(|| panic!("{}: no Go version token", path.display()));
            let stem = path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            // `strip-1.22` / `pie-strip-1.24` name the toolchain; the arm64
            // fixtures name the architecture instead and are skipped here.
            if let Some((_, expected)) = stem.rsplit_once('-')
                && expected.starts_with('1')
            {
                assert!(
                    version.contains(expected),
                    "{}: reported {version}, fixture is {expected}",
                    path.display()
                );
            }
            checked += 1;
        }
        assert!(checked > 0, "no fixtures found in {dir}");
    }

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
        let table = GoTable::parse(&blob, 0x401000).expect("table parses");
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
        assert!(GoTable::parse(&[], 0x1000).is_none());
        assert!(GoTable::parse(&[0u8; 32], 0x1000).is_none());
        let mut blob = synthetic();
        blob[0..4].copy_from_slice(&0xdead_beefu32.to_le_bytes());
        assert!(GoTable::parse(&blob, 0x1000).is_none());
        // A truncated table must not panic.
        let blob = synthetic();
        assert!(GoTable::parse(&blob[..80], 0x1000).is_none());
    }

    #[test]
    fn scan_finds_a_table_behind_a_removed_section_header() {
        // A packer leaves the table in place with no section pointing at it.
        let table_bytes = synthetic();
        let offset = 256;
        let mut data = vec![0u8; offset + table_bytes.len()];
        data[offset..].copy_from_slice(&table_bytes);
        let table = GoTable::scan(&data, 0x401000).expect("scan finds the table");
        assert_eq!(table.lookup(0x401010), Some("main.a"));
    }

    #[test]
    fn scan_ignores_a_magic_without_a_table() {
        // The magic bytes appear, but nothing that follows is a functab.
        let mut data = vec![0u8; 1024];
        data[64..68].copy_from_slice(&MAGIC_GO120.to_le_bytes());
        assert!(GoTable::scan(&data, 0x401000).is_none());
    }
}
