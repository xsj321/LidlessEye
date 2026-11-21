//! Kernel struct offsets that CO-RE cannot name.
//!
//! `tcp_sendmsg` hands the payload over in `struct msghdr.msg_iter`, and the
//! first iovec pointer lives inside `struct iov_iter`. Kernel 6.16 reworked
//! that struct: the pointer moved and `iov` became `__iov`. A CO-RE access by
//! name resolves on one layout and fails on the other, and a failed relocation
//! is replaced with an invalid call that the verifier rejects — the send probe
//! then fails to load entirely.
//!
//! The two offsets are therefore read once from the running kernel's BTF, the
//! same file aya needs for CO-RE, and patched into the program as constants.
//! Both layouts are handled by one code path, and nothing in the kernel object
//! depends on a field name that a future release may rename.

use std::path::Path;

/// Where the payload's iovec pointer is, relative to the start of the objects
/// the probe is handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IovLayout {
    /// Offset of `msg_iter` inside `struct msghdr`.
    pub msg_iter: u32,
    /// Offset of the iovec pointer inside `struct iov_iter`.
    pub iov_ptr: u32,
}

const BTF_KIND_INT: u32 = 1;
const BTF_KIND_ARRAY: u32 = 3;
const BTF_KIND_STRUCT: u32 = 4;
const BTF_KIND_UNION: u32 = 5;
const BTF_KIND_ENUM: u32 = 6;
const BTF_KIND_FUNC_PROTO: u32 = 13;
const BTF_KIND_VAR: u32 = 14;
const BTF_KIND_DATASEC: u32 = 15;
const BTF_KIND_DECL_TAG: u32 = 17;
const BTF_KIND_ENUM64: u32 = 19;

/// Read the layout from `/sys/kernel/btf/vmlinux`.
///
/// Returns `Err` with a reason the caller can log when the file is missing or
/// the structs are not what this expects; the caller then falls back to the
/// program's defaults.
pub fn iov_layout() -> Result<IovLayout, String> {
    iov_layout_from(Path::new("/sys/kernel/btf/vmlinux"))
}

fn iov_layout_from(path: &Path) -> Result<IovLayout, String> {
    let data = std::fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let table = Table::parse(&data)?;
    let msghdr = table
        .find_struct("msghdr")
        .ok_or_else(|| "the kernel BTF has no `struct msghdr`".to_string())?;
    let msg_iter = table
        .members(msghdr)
        .iter()
        .find(|member| member.name == "msg_iter")
        .map(|member| member.byte_offset())
        .ok_or_else(|| "`struct msghdr` has no `msg_iter`".to_string())?;
    let iov_iter = table
        .find_struct("iov_iter")
        .ok_or_else(|| "the kernel BTF has no `struct iov_iter`".to_string())?;
    let iov_ptr = table
        .find_member(iov_iter, &["__iov", "iov"], 0)
        .ok_or_else(|| "`struct iov_iter` has neither `iov` nor `__iov`".to_string())?;
    Ok(IovLayout { msg_iter, iov_ptr })
}

struct Member<'a> {
    name: &'a str,
    /// Type of the member, used to descend into anonymous structs and unions.
    type_id: u32,
    /// Bit offset from the start of the enclosing struct.
    bit_offset: u32,
}

impl Member<'_> {
    fn byte_offset(&self) -> u32 {
        self.bit_offset / 8
    }
}

struct Entry<'a> {
    name: &'a str,
    kind: u32,
    /// Bit offset of each member, for structs and unions.
    members: Vec<Member<'a>>,
}

/// A minimal BTF reader: the header, the type section and its strings. Only
/// what is needed to walk two structs is decoded.
struct Table<'a> {
    entries: Vec<Entry<'a>>,
}

impl<'a> Table<'a> {
    fn parse(data: &'a [u8]) -> Result<Self, String> {
        if data.len() < 24 {
            return Err("BTF header is truncated".to_string());
        }
        let magic = u16::from_le_bytes([data[0], data[1]]);
        if magic != 0xeb9f {
            return Err(format!("not a BTF blob (magic {magic:#x})"));
        }
        let read_u32 = |at: usize| {
            u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize
        };
        let header_len = read_u32(4);
        let type_off = read_u32(8);
        let type_len = read_u32(12);
        let str_off = read_u32(16);
        let str_len = read_u32(20);
        let types = data
            .get(header_len + type_off..header_len + type_off + type_len)
            .ok_or_else(|| "BTF type section is out of bounds".to_string())?;
        let strings = data
            .get(header_len + str_off..header_len + str_off + str_len)
            .ok_or_else(|| "BTF string section is out of bounds".to_string())?;

        let mut entries = Vec::new();
        let mut at = 0;
        while at + 12 <= types.len() {
            let name_off = u32::from_le_bytes(types[at..at + 4].try_into().unwrap()) as usize;
            let info = u32::from_le_bytes(types[at + 4..at + 8].try_into().unwrap());
            let _value = u32::from_le_bytes(types[at + 8..at + 12].try_into().unwrap());
            let kind = (info >> 24) & 0x1f;
            let count = info & 0xffff;
            let flagged = info >> 31 != 0;
            let name = Self::string(strings, name_off).unwrap_or("");
            at += 12;

            let mut members = Vec::new();
            match kind {
                BTF_KIND_STRUCT | BTF_KIND_UNION => {
                    for _ in 0..count {
                        if at + 12 > types.len() {
                            break;
                        }
                        let member_name =
                            u32::from_le_bytes(types[at..at + 4].try_into().unwrap()) as usize;
                        let member_type =
                            u32::from_le_bytes(types[at + 4..at + 8].try_into().unwrap());
                        let raw = u32::from_le_bytes(types[at + 8..at + 12].try_into().unwrap());
                        at += 12;
                        // A flagged struct packs the bitfield width into the
                        // high byte and the offset into the low three.
                        let bit_offset = if flagged { raw & 0xffffff } else { raw };
                        members.push(Member {
                            name: Self::string(strings, member_name).unwrap_or(""),
                            type_id: member_type,
                            bit_offset,
                        });
                    }
                }
                BTF_KIND_INT => at += 4,
                // An array has one fixed payload; a data section has one entry
                // per variable. Treating them alike silently desynchronised the
                // whole type stream at the first array in the file.
                BTF_KIND_ARRAY => at += 12,
                BTF_KIND_DATASEC => at += 12 * count as usize,
                BTF_KIND_ENUM | BTF_KIND_FUNC_PROTO => at += 8 * count as usize,
                // `btf_enum64` grew a second 32-bit value: name, lo, hi.
                BTF_KIND_ENUM64 => at += 12 * count as usize,
                BTF_KIND_VAR | BTF_KIND_DECL_TAG => at += 4,
                _ => {}
            }
            entries.push(Entry {
                name,
                kind,
                members,
            });
        }
        Ok(Table { entries })
    }

    fn string(strings: &'a [u8], offset: usize) -> Option<&'a str> {
        let tail = strings.get(offset..)?;
        let end = tail.iter().position(|&byte| byte == 0)?;
        std::str::from_utf8(&tail[..end]).ok()
    }

    fn find_struct(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|entry| {
            entry.name == name && (entry.kind == BTF_KIND_STRUCT || entry.kind == BTF_KIND_UNION)
        })
    }

    fn members(&self, index: usize) -> &[Member<'a>] {
        &self.entries[index].members
    }

    /// Search `index` for a member with one of `names`, descending into
    /// anonymous structs and unions (a field can live two levels deep, as
    /// `iov_iter`'s pointer does), and return its byte offset.
    fn find_member(&self, index: usize, names: &[&str], depth: usize) -> Option<u32> {
        // `iov_iter` reaches its pointer through three anonymous levels on
        // 6.16+, so the walk needs a little headroom rather than a tight bound.
        if depth > 6 {
            return None;
        }
        for member in self.members(index) {
            if names.contains(&member.name) {
                return Some(member.bit_offset / 8);
            }
            if member.name.is_empty()
                && let Some(nested) = member.type_id.checked_sub(1)
                && let Some(offset) = self.find_member(nested as usize, names, depth + 1)
            {
                return Some(member.bit_offset / 8 + offset);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_running_kernel_reports_a_usable_layout() {
        let path = Path::new("/sys/kernel/btf/vmlinux");
        if !path.exists() {
            return; // No BTF: nothing to check, and CO-RE would be off too.
        }
        let layout = iov_layout().expect("the kernel BTF should describe msg_iter");
        // `msghdr.msg_iter` has been at 16 since 4.17; the pointer inside
        // `iov_iter` is at 16 on 6.16+ and 24 on older releases.
        assert_eq!(layout.msg_iter, 16);
        assert!(layout.iov_ptr == 16 || layout.iov_ptr == 24, "{layout:?}");
    }

    #[test]
    fn a_truncated_blob_is_rejected_rather_than_misread() {
        assert!(Table::parse(&[0u8; 4]).is_err());
        assert!(Table::parse(&[0u8; 64]).is_err());
    }
}
