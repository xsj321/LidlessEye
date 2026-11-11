//! The `LESEG` columnar segment format.
//!
//! ## Why a custom format?
//!
//! A profiling workload is append-mostly and read-mostly: agents push small
//! batches every few seconds, dashboards read aggregated views over a time
//! range. That shape is a poor fit for a row store and an even worse fit for
//! an LSM tree that rewrites data on every flush.
//!
//! `LESEG` files are immutable, memory-mappable columnar segments:
//!
//! ```text
//! ┌────────┬────────┬─────────────┬─────────────┬───────────────┬─────────┐
//! │ header │ strtab │   frames    │ stack_frames│     stacks    │ samples │
//! │ 128 B  │ UTF-8  │ 16 B/frame  │  4 B/entry  │   8 B/stack   │ 16 B/ea │
//! └────────┴────────┴─────────────┴─────────────┴───────────────┴─────────┘
//! ```
//!
//! * every symbol is written once per segment (dictionary encoding),
//! * a stack is a `(offset, len)` slice into one flat `u32` array, so a
//!   repeated stack costs 8 bytes instead of a `Vec`,
//! * samples are sorted by value descending, which makes "top N functions"
//!   and truncated flamegraphs cheap: the scan can stop early,
//! * reads are zero-copy: the query engine mmaps the file and walks the
//!   arrays in place, no deserialization step, no per-sample allocation.
//!
//! This is the same design axis DuckDB/Parquet optimise for (columnar,
//! dictionary encoded, immutable files) without pulling a C++ engine into the
//! server process. Each segment also carries its own series metadata, so the
//! query planner can reject a segment from its 128 byte header alone.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{Frame, ProfileType, Report, SERVICE_NAME_LABEL};

/// `LESEG\0\0\x01`
pub const MAGIC: [u8; 8] = *b"LESEG\x00\x00\x01";
pub const FORMAT_VERSION: u32 = 1;
pub const HEADER_SIZE: usize = 128;
pub const FRAME_SIZE: usize = 16;
pub const STACK_SIZE: usize = 8;
pub const SAMPLE_SIZE: usize = 16;
/// Hard limit for a single segment file (256 MiB).
pub const MAX_SEGMENT_BYTES: u64 = 256 * 1024 * 1024;

const FLAG_SORTED_BY_VALUE: u32 = 1 << 0;

/// Metadata of a segment, cheap to obtain (128 byte header + labels).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentMeta {
    pub path: PathBuf,
    pub profile_type: ProfileType,
    pub service_name: String,
    pub labels: BTreeMap<String, String>,
    pub start_ns: u64,
    pub end_ns: u64,
    pub total_value: u64,
    pub sample_count: u32,
    pub size_bytes: u64,
    pub created_ns: u64,
}

impl SegmentMeta {
    pub fn overlaps(&self, from_ns: u64, until_ns: u64) -> bool {
        self.end_ns > from_ns && self.start_ns < until_ns
    }

    pub fn label(&self, key: &str) -> Option<&str> {
        if key == SERVICE_NAME_LABEL {
            Some(&self.service_name)
        } else {
            self.labels.get(key).map(|s| s.as_str())
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Header {
    start_ns: u64,
    end_ns: u64,
    total_value: u64,
    sample_count: u32,
    flags: u32,
    type_off: u32,
    service_off: u32,
    labels_off: u32,
    labels_len: u32,
    strtab_off: u32,
    strtab_len: u32,
    frames_off: u32,
    frames_count: u32,
    stack_frames_off: u32,
    stack_frames_count: u32,
    stacks_off: u32,
    stacks_count: u32,
    samples_off: u32,
    samples_count: u32,
    created_ns: u64,
    crc32: u32,
}

impl Header {
    fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..8].copy_from_slice(&MAGIC);
        buf[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf[12..16].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
        buf[16..24].copy_from_slice(&self.start_ns.to_le_bytes());
        buf[24..32].copy_from_slice(&self.end_ns.to_le_bytes());
        buf[32..40].copy_from_slice(&self.total_value.to_le_bytes());
        buf[40..44].copy_from_slice(&self.sample_count.to_le_bytes());
        buf[44..48].copy_from_slice(&self.flags.to_le_bytes());
        buf[48..52].copy_from_slice(&self.type_off.to_le_bytes());
        buf[52..56].copy_from_slice(&self.service_off.to_le_bytes());
        buf[56..60].copy_from_slice(&self.labels_off.to_le_bytes());
        buf[60..64].copy_from_slice(&self.labels_len.to_le_bytes());
        buf[64..68].copy_from_slice(&self.strtab_off.to_le_bytes());
        buf[68..72].copy_from_slice(&self.strtab_len.to_le_bytes());
        buf[72..76].copy_from_slice(&self.frames_off.to_le_bytes());
        buf[76..80].copy_from_slice(&self.frames_count.to_le_bytes());
        buf[80..84].copy_from_slice(&self.stack_frames_off.to_le_bytes());
        buf[84..88].copy_from_slice(&self.stack_frames_count.to_le_bytes());
        buf[88..92].copy_from_slice(&self.stacks_off.to_le_bytes());
        buf[92..96].copy_from_slice(&self.stacks_count.to_le_bytes());
        buf[96..100].copy_from_slice(&self.samples_off.to_le_bytes());
        buf[100..104].copy_from_slice(&self.samples_count.to_le_bytes());
        buf[104..112].copy_from_slice(&self.created_ns.to_le_bytes());
        buf[112..116].copy_from_slice(&self.crc32.to_le_bytes());
        buf
    }

    fn decode(buf: &[u8], path: &Path) -> Result<Self> {
        if buf.len() < HEADER_SIZE {
            return Err(Error::corrupt(path, "file is shorter than the header"));
        }
        if buf[0..8] != MAGIC {
            return Err(Error::corrupt(path, "bad magic"));
        }
        let version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(Error::UnsupportedVersion {
                version,
                path: path.to_path_buf(),
            });
        }
        let header_size = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
        if header_size != HEADER_SIZE {
            return Err(Error::corrupt(path, "unexpected header size"));
        }
        macro_rules! u32_at {
            ($o:expr) => {
                u32::from_le_bytes(buf[$o..$o + 4].try_into().unwrap())
            };
        }
        macro_rules! u64_at {
            ($o:expr) => {
                u64::from_le_bytes(buf[$o..$o + 8].try_into().unwrap())
            };
        }
        Ok(Header {
            start_ns: u64_at!(16),
            end_ns: u64_at!(24),
            total_value: u64_at!(32),
            sample_count: u32_at!(40),
            flags: u32_at!(44),
            type_off: u32_at!(48),
            service_off: u32_at!(52),
            labels_off: u32_at!(56),
            labels_len: u32_at!(60),
            strtab_off: u32_at!(64),
            strtab_len: u32_at!(68),
            frames_off: u32_at!(72),
            frames_count: u32_at!(76),
            stack_frames_off: u32_at!(80),
            stack_frames_count: u32_at!(84),
            stacks_off: u32_at!(88),
            stacks_count: u32_at!(92),
            samples_off: u32_at!(96),
            samples_count: u32_at!(100),
            created_ns: u64_at!(104),
            crc32: u32_at!(112),
        })
    }

    fn sections(&self) -> [(u32, u64); 5] {
        [
            (self.strtab_off, self.strtab_len as u64),
            (self.frames_off, self.frames_count as u64 * FRAME_SIZE as u64),
            (
                self.stack_frames_off,
                self.stack_frames_count as u64 * 4,
            ),
            (self.stacks_off, self.stacks_count as u64 * STACK_SIZE as u64),
            (self.samples_off, self.samples_count as u64 * SAMPLE_SIZE as u64),
        ]
    }

    fn validate_layout(&self, len: u64, path: &Path) -> Result<()> {
        for (off, size) in self.sections() {
            let end = off as u64 + size;
            if end > len {
                return Err(Error::corrupt(
                    path,
                    format!("section [{off}..{end}] exceeds file size {len}"),
                ));
            }
        }
        if self.type_off >= self.strtab_len || self.service_off >= self.strtab_len {
            return Err(Error::corrupt(path, "string table references are out of range"));
        }
        if self.labels_off as u64 + self.labels_len as u64 > self.strtab_len as u64 {
            return Err(Error::corrupt(path, "labels blob is out of range"));
        }
        Ok(())
    }
}

fn now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Interned string table used while writing a segment.
#[derive(Default)]
struct StringTable {
    values: Vec<String>,
    index: HashMap<String, u32>,
}

impl StringTable {
    fn intern(&mut self, value: &str) -> u32 {
        if let Some(&idx) = self.index.get(value) {
            return idx;
        }
        let idx = self.values.len() as u32;
        self.values.push(value.to_string());
        self.index.insert(value.to_string(), idx);
        idx
    }
}

/// Turn a service name into something safe for a directory name.
pub fn service_slug(service_name: &str) -> String {
    let mut slug: String = service_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    slug.truncate(96);
    if slug.is_empty() {
        slug.push_str("unnamed");
    }
    slug
}

/// Write a report as a new immutable segment.
///
/// The file is written to a temporary name and atomically renamed, so a
/// crashing writer can never leave a half-written segment behind.
pub fn write_segment(dir: &Path, report: &Report) -> Result<SegmentMeta> {
    report.validate()?;
    std::fs::create_dir_all(dir)?;

    // --- dictionary -------------------------------------------------------
    let mut dict = StringTable::default();
    let type_off = dict.intern(report.profile_type.as_str());
    let service_off = dict.intern(&report.service_name);

    // labels are stored as a compact json blob inside the string table
    let labels = report.all_labels();
    let labels_json = serde_json::to_string(&labels)?;
    let labels_off = dict.intern(&labels_json);
    let labels_len = labels_json.len() as u32;

    // --- frames -----------------------------------------------------------
    let mut frames: Vec<(u32, u32, u32, u32)> = Vec::new();
    let mut frame_idx: HashMap<(u32, u32, u32, u32), u32> = HashMap::new();

    // --- stacks -----------------------------------------------------------
    let mut stack_frames: Vec<u32> = Vec::new();
    let mut stacks: Vec<(u32, u32)> = Vec::new();
    let mut stack_idx: HashMap<Vec<u32>, u32> = HashMap::new();
    let mut samples: Vec<(u32, u32, u64)> = Vec::with_capacity(report.stacks.len());
    let mut total_value: u64 = 0;

    for stack in &report.stacks {
        if stack.value == 0 || stack.frames.is_empty() {
            continue;
        }
        let mut frame_ids = Vec::with_capacity(stack.frames.len());
        for frame in &stack.frames {
            let module = report.strings.get(frame[0] as usize).map(String::as_str).unwrap_or("");
            let symbol = report.strings.get(frame[1] as usize).map(String::as_str).unwrap_or("");
            let file = report.strings.get(frame[2] as usize).map(String::as_str).unwrap_or("");
            let line = frame[3];
            let key = (
                dict.intern(module),
                dict.intern(symbol),
                dict.intern(file),
                line,
            );
            let id = *frame_idx.entry(key).or_insert_with(|| {
                frames.push(key);
                (frames.len() - 1) as u32
            });
            frame_ids.push(id);
        }

        let stack_id = match stack_idx.get(&frame_ids) {
            Some(&id) => id,
            None => {
                let off = stack_frames.len() as u32;
                stack_frames.extend_from_slice(&frame_ids);
                stacks.push((off, frame_ids.len() as u32));
                let id = (stacks.len() - 1) as u32;
                stack_idx.insert(frame_ids, id);
                id
            }
        };
        total_value = total_value.saturating_add(stack.value);
        samples.push((stack_id, 0, stack.value));
    }

    // Sorted by value: enables early exit for top-N and truncated flamegraphs.
    samples.sort_unstable_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));

    // --- layout -----------------------------------------------------------
    let strtab_size: usize = dict.values.iter().map(|s| s.len() + 1).sum();
    let frames_size = frames.len() * FRAME_SIZE;
    let stack_frames_size = stack_frames.len() * 4;
    let stacks_size = stacks.len() * STACK_SIZE;
    let samples_size = samples.len() * SAMPLE_SIZE;
    let total_size = HEADER_SIZE
        + strtab_size
        + frames_size
        + stack_frames_size
        + stacks_size
        + samples_size;
    if total_size as u64 > MAX_SEGMENT_BYTES {
        return Err(Error::SegmentTooLarge {
            size: total_size as u64,
            limit: MAX_SEGMENT_BYTES,
        });
    }

    let mut buf = Vec::<u8>::with_capacity(total_size);
    buf.resize(HEADER_SIZE, 0);

    let strtab_off = buf.len() as u32;
    let mut str_offsets: Vec<u32> = Vec::with_capacity(dict.values.len());
    for s in &dict.values {
        str_offsets.push((buf.len() - strtab_off as usize) as u32);
        buf.extend_from_slice(s.as_bytes());
        buf.push(0);
    }
    // Re-map the dictionaries now that the string table offsets are known.
    let remap = |off: u32| str_offsets[off as usize];

    let frames_off = buf.len() as u32;
    for (m, s, f, line) in &frames {
        buf.extend_from_slice(&remap(*m).to_le_bytes());
        buf.extend_from_slice(&remap(*s).to_le_bytes());
        buf.extend_from_slice(&remap(*f).to_le_bytes());
        buf.extend_from_slice(&line.to_le_bytes());
    }

    let stack_frames_off = buf.len() as u32;
    for id in &stack_frames {
        buf.extend_from_slice(&id.to_le_bytes());
    }

    let stacks_off = buf.len() as u32;
    for (off, len) in &stacks {
        buf.extend_from_slice(&off.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
    }

    let samples_off = buf.len() as u32;
    for (stack, pad, value) in &samples {
        buf.extend_from_slice(&stack.to_le_bytes());
        buf.extend_from_slice(&pad.to_le_bytes());
        buf.extend_from_slice(&value.to_le_bytes());
    }

    let payload = &buf[HEADER_SIZE..];
    let header = Header {
        start_ns: report.start_ns,
        end_ns: report.end_ns,
        total_value,
        sample_count: samples.len() as u32,
        flags: FLAG_SORTED_BY_VALUE,
        type_off: remap(type_off),
        service_off: remap(service_off),
        labels_off: remap(labels_off),
        labels_len,
        strtab_off,
        strtab_len: strtab_size as u32,
        frames_off,
        frames_count: frames.len() as u32,
        stack_frames_off,
        stack_frames_count: stack_frames.len() as u32,
        stacks_off,
        stacks_count: stacks.len() as u32,
        samples_off,
        samples_count: samples.len() as u32,
        created_ns: now_ns(),
        crc32: crc32fast::hash(payload),
    };
    buf[..HEADER_SIZE].copy_from_slice(&header.encode());

    if samples.is_empty() {
        return Err(Error::InvalidReport("report contains no samples".into()));
    }

    let file_name = format!("{}-{}.leseg", report.start_ns, uuid::Uuid::new_v4());
    let final_path = dir.join(&file_name);
    let tmp_path = dir.join(format!(".{file_name}.tmp"));
    {
        let mut file = File::create(&tmp_path)?;
        file.write_all(&buf)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp_path, &final_path)?;

    Ok(SegmentMeta {
        path: final_path,
        profile_type: report.profile_type.clone(),
        service_name: report.service_name.clone(),
        labels,
        start_ns: report.start_ns,
        end_ns: report.end_ns,
        total_value,
        sample_count: samples.len() as u32,
        size_bytes: total_size as u64,
        created_ns: header.created_ns,
    })
}

/// Read a segment's metadata without mapping the whole file.
pub fn read_segment_meta(path: &Path) -> Result<SegmentMeta> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut head = [0u8; HEADER_SIZE];
    file.read_exact(&mut head)?;
    let header = Header::decode(&head, path)?;
    header.validate_layout(file_len, path)?;

    let mut strtab = vec![0u8; header.strtab_len as usize];
    file.seek(SeekFrom::Start(header.strtab_off as u64))?;
    file.read_exact(&mut strtab)?;

    let read_str = |off: u32| -> String {
        let start = off as usize;
        if start >= strtab.len() {
            return String::new();
        }
        let tail = &strtab[start..];
        let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
        String::from_utf8_lossy(&tail[..end]).into_owned()
    };

    let profile_type = read_str(header.type_off);
    let service_name = read_str(header.service_off);
    let labels_json = read_str(header.labels_off);
    let labels: BTreeMap<String, String> = serde_json::from_str(&labels_json).unwrap_or_default();

    Ok(SegmentMeta {
        path: path.to_path_buf(),
        profile_type: ProfileType::new(profile_type),
        service_name,
        labels,
        start_ns: header.start_ns,
        end_ns: header.end_ns,
        total_value: header.total_value,
        sample_count: header.sample_count,
        size_bytes: file_len,
        created_ns: header.created_ns,
    })
}

/// A memory-mapped segment, ready for zero-copy iteration.
pub struct Segment {
    path: PathBuf,
    mmap: Mmap,
    header: Header,
    labels: BTreeMap<String, String>,
    profile_type: ProfileType,
    service_name: String,
}

impl Segment {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let file_len = file.metadata()?.len();
        if file_len < HEADER_SIZE as u64 {
            return Err(Error::corrupt(&path, "file is shorter than the header"));
        }
        // SAFETY: the file is immutable by construction (segments are only
        // ever created via atomic rename and never written in place).
        let mmap = unsafe { Mmap::map(&file)? };
        let header = Header::decode(&mmap[..HEADER_SIZE], &path)?;
        header.validate_layout(mmap.len() as u64, &path)?;

        let strtab = &mmap[header.strtab_off as usize
            ..header.strtab_off as usize + header.strtab_len as usize];
        let read_str = |off: u32| -> String {
            if off as usize >= strtab.len() {
                return String::new();
            }
            let tail = &strtab[off as usize..];
            let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
            String::from_utf8_lossy(&tail[..end]).into_owned()
        };

        let profile_type = ProfileType::new(read_str(header.type_off));
        let service_name = read_str(header.service_off);
        let labels_json = read_str(header.labels_off);
        let labels: BTreeMap<String, String> =
            serde_json::from_str(&labels_json).unwrap_or_default();

        Ok(Segment {
            path,
            mmap,
            header,
            labels,
            profile_type,
            service_name,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn meta(&self) -> SegmentMeta {
        SegmentMeta {
            path: self.path.clone(),
            profile_type: self.profile_type.clone(),
            service_name: self.service_name.clone(),
            labels: self.labels.clone(),
            start_ns: self.header.start_ns,
            end_ns: self.header.end_ns,
            total_value: self.header.total_value,
            sample_count: self.header.sample_count,
            size_bytes: self.mmap.len() as u64,
            created_ns: self.header.created_ns,
        }
    }

    pub fn profile_type(&self) -> &ProfileType {
        &self.profile_type
    }

    pub fn service_name(&self) -> &str {
        &self.service_name
    }

    pub fn labels(&self) -> &BTreeMap<String, String> {
        &self.labels
    }

    pub fn start_ns(&self) -> u64 {
        self.header.start_ns
    }

    pub fn end_ns(&self) -> u64 {
        self.header.end_ns
    }

    pub fn total_value(&self) -> u64 {
        self.header.total_value
    }

    pub fn sample_count(&self) -> usize {
        self.header.samples_count as usize
    }

    pub fn frame_count(&self) -> usize {
        self.header.frames_count as usize
    }

    pub fn is_sorted(&self) -> bool {
        self.header.flags & FLAG_SORTED_BY_VALUE != 0
    }

    /// `(module, symbol, file, line)` string table offsets of a frame.
    pub fn frame_at(&self, id: u32) -> Option<(u32, u32, u32, u32)> {
        if id >= self.header.frames_count {
            return None;
        }
        let off = self.header.frames_off as usize + id as usize * FRAME_SIZE;
        let b = &self.mmap[off..off + FRAME_SIZE];
        Some((
            u32::from_le_bytes(b[0..4].try_into().unwrap()),
            u32::from_le_bytes(b[4..8].try_into().unwrap()),
            u32::from_le_bytes(b[8..12].try_into().unwrap()),
            u32::from_le_bytes(b[12..16].try_into().unwrap()),
        ))
    }

    /// Resolve a string table offset.
    pub fn string_at(&self, off: u32) -> &str {
        let base = self.header.strtab_off as usize;
        let len = self.header.strtab_len as usize;
        if off as usize >= len {
            return "";
        }
        let tail = &self.mmap[base + off as usize..base + len];
        let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
        // SAFETY: the string table is validated as UTF-8 at write time; a
        // corrupted file may produce invalid UTF-8, so fall back gracefully.
        std::str::from_utf8(&tail[..end]).unwrap_or("")
    }

    /// Byte range of a stack's frame ids, `None` when the id is out of range.
    fn stack_range(&self, stack_id: u32) -> Option<std::ops::Range<usize>> {
        if stack_id >= self.header.stacks_count {
            return None;
        }
        let off = self.header.stacks_off as usize + stack_id as usize * STACK_SIZE;
        let b = &self.mmap[off..off + STACK_SIZE];
        let s_off = u32::from_le_bytes(b[0..4].try_into().unwrap()) as usize;
        let s_len = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
        if s_off + s_len > self.header.stack_frames_count as usize {
            return None;
        }
        let base = self.header.stack_frames_off as usize;
        Some(base + s_off * 4..base + (s_off + s_len) * 4)
    }

    /// Call `f` for every frame id of a stack, leaf first.
    ///
    /// Sections are not guaranteed to be 4-byte aligned inside the file, so
    /// words are decoded explicitly instead of being cast to `&[u32]`.
    pub fn for_each_frame_id(&self, stack_id: u32, mut f: impl FnMut(u32)) {
        if let Some(range) = self.stack_range(stack_id) {
            for chunk in self.mmap[range].chunks_exact(4) {
                f(u32::from_le_bytes(chunk.try_into().unwrap()));
            }
        }
    }

    /// Frame ids of a stack, leaf first (convenience, allocates).
    pub fn stack_frame_ids(&self, stack_id: u32) -> Vec<u32> {
        let mut out = Vec::new();
        self.for_each_frame_id(stack_id, |id| out.push(id));
        out
    }

    /// Number of frames in a stack.
    pub fn stack_depth(&self, stack_id: u32) -> usize {
        self.stack_range(stack_id).map(|r| r.len() / 4).unwrap_or(0)
    }

    /// `(stack_id, value)` of sample `i`.
    pub fn sample_at(&self, i: usize) -> Option<(u32, u64)> {
        if i >= self.header.samples_count as usize {
            return None;
        }
        let off = self.header.samples_off as usize + i * SAMPLE_SIZE;
        let b = &self.mmap[off..off + SAMPLE_SIZE];
        Some((
            u32::from_le_bytes(b[0..4].try_into().unwrap()),
            u64::from_le_bytes(b[8..16].try_into().unwrap()),
        ))
    }

    /// Materialized frames of a stack, leaf first.
    pub fn stack_frames_materialized(&self, stack_id: u32) -> Vec<Frame> {
        let mut frames = Vec::with_capacity(self.stack_depth(stack_id));
        let mut push = |fid: u32| {
            if let Some((m, s, f, line)) = self.frame_at(fid) {
                let mut frame = Frame::new(self.string_at(m), self.string_at(s));
                frame.file = self.string_at(f).to_string();
                frame.line = line;
                frames.push(frame);
            }
        };
        self.for_each_frame_id(stack_id, &mut push);
        frames
    }

    /// Verify the payload checksum. Used by `lidlesseye fsck`.
    pub fn verify(&self) -> Result<()> {
        let actual = crc32fast::hash(&self.mmap[HEADER_SIZE..]);
        if actual != self.header.crc32 {
            return Err(Error::corrupt(
                &self.path,
                format!(
                    "crc mismatch: stored {:#010x}, computed {actual:#010x}",
                    self.header.crc32
                ),
            ));
        }
        if self.is_sorted() {
            let mut prev = u64::MAX;
            for i in 0..self.sample_count() {
                let (_, v) = self.sample_at(i).unwrap();
                if v > prev {
                    return Err(Error::corrupt(&self.path, "samples not sorted by value"));
                }
                prev = v;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Frame, ReportBuilder};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lidlesseye-seg-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_report() -> Report {
        let mut b = ReportBuilder::new("checkout", "cpu", 1_000, 2_000).label("pod", "checkout-0");
        b.add_stack(
            [
                Frame::new("/bin/checkout", "process_order"),
                Frame::new("/bin/checkout", "main"),
            ],
            700,
        );
        b.add_stack(
            [
                Frame::new("/bin/checkout", "render"),
                Frame::new("/bin/checkout", "main"),
            ],
            300,
        );
        b.finish()
    }

    #[test]
    fn roundtrip() {
        let dir = temp_dir("roundtrip");
        let meta = write_segment(&dir, &sample_report()).unwrap();
        let seg = Segment::open(&meta.path).unwrap();
        seg.verify().unwrap();
        assert_eq!(seg.service_name(), "checkout");
        assert_eq!(seg.total_value(), 1000);
        assert_eq!(seg.sample_count(), 2);
        assert_eq!(seg.labels().get("pod").unwrap(), "checkout-0");
        assert_eq!(seg.labels().get("service_name").unwrap(), "checkout");

        // sorted descending by value
        let (s0, v0) = seg.sample_at(0).unwrap();
        assert_eq!(v0, 700);
        let frames = seg.stack_frames_materialized(s0);
        assert_eq!(frames[0].symbol, "process_order");
        assert_eq!(frames[1].symbol, "main");

        let meta2 = read_segment_meta(&meta.path).unwrap();
        assert_eq!(meta2.total_value, 1000);
        assert_eq!(meta2.profile_type, ProfileType::cpu());
        assert_eq!(meta2.labels.get("pod").unwrap(), "checkout-0");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_magic_is_rejected() {
        let dir = temp_dir("corrupt");
        let meta = write_segment(&dir, &sample_report()).unwrap();
        let mut data = std::fs::read(&meta.path).unwrap();
        data[0] = b'X';
        std::fs::write(&meta.path, &data).unwrap();
        assert!(Segment::open(&meta.path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
