//! The profile data model.
//!
//! LidlessEye stores *symbolized* profiles: by the time a report reaches the
//! storage layer every stack frame already carries a module, a symbol and an
//! optional source location. This keeps the storage server completely
//! independent from the kernel, the target architecture and the toolchain of
//! the profiled process — a report produced on an aarch64 node with kernel
//! symbols can be read back on an x86_64 laptop.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

/// Conventional label holding the profiled service name (same convention as
/// Pyroscope uses).
pub const SERVICE_NAME_LABEL: &str = "service_name";

/// Maximum number of frames we accept in a single stack. The eBPF collector
/// uses 127 (the kernel's `PERF_MAX_STACK_DEPTH`), leave some head room for
/// native SDKs that walk their own stacks.
pub const MAX_FRAMES_PER_STACK: usize = 2048;

/// Maximum number of distinct strings accepted in one report.
pub const MAX_STRINGS: usize = 4_000_000;

/// Maximum number of stacks accepted in one report.
pub const MAX_STACKS: usize = 2_000_000;

/// Maximum number of labels on one report.
pub const MAX_LABELS: usize = 64;

/// Maximum length of a label name.
pub const MAX_LABEL_KEY_LEN: usize = 128;

/// Maximum length of a label value.
pub const MAX_LABEL_VALUE_LEN: usize = 1024;

/// The unit of a profile's sample values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// Wall-clock / CPU time in nanoseconds.
    Nanoseconds,
    /// Allocated bytes (heap profiling).
    Bytes,
    /// Object / event counts.
    Count,
}

impl Unit {
    pub fn as_str(self) -> &'static str {
        match self {
            Unit::Nanoseconds => "nanoseconds",
            Unit::Bytes => "bytes",
            Unit::Count => "count",
        }
    }
}

/// A profile type, e.g. `cpu`, `off_cpu` or `alloc_space`.
///
/// The type is a string (not an enum) so that reports produced by other
/// profilers — Pyroscope's `goroutine`, `mutex_count`, or a custom in-house
/// type — can be ingested without a schema change.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileType(String);

impl ProfileType {
    pub const CPU: &'static str = "cpu";
    pub const OFF_CPU: &'static str = "off_cpu";
    pub const WALL: &'static str = "wall";
    pub const ALLOC_SPACE: &'static str = "alloc_space";
    pub const ALLOC_OBJECTS: &'static str = "alloc_objects";
    pub const INUSE_SPACE: &'static str = "inuse_space";
    pub const INUSE_OBJECTS: &'static str = "inuse_objects";

    pub fn new(name: impl Into<String>) -> Self {
        ProfileType(name.into())
    }

    pub fn cpu() -> Self {
        ProfileType(Self::CPU.to_string())
    }

    pub fn off_cpu() -> Self {
        ProfileType(Self::OFF_CPU.to_string())
    }

    pub fn wall() -> Self {
        ProfileType(Self::WALL.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `wall` is a *computed* type: a query for it merges every wall-clock
    /// input that exists. It is also a real, storable type, because SDKs ship
    /// wall-clock profiles (pprof `wall`) and rejecting them would lose data.
    pub fn is_virtual(&self) -> bool {
        self.0 == Self::WALL
    }

    /// The concrete types that have to be read to answer a query for `self`.
    ///
    /// For `wall` this is `cpu`, `off_cpu` **and** stored `wall` profiles. A
    /// deployment should pick one source — either the eBPF agent (cpu plus
    /// off_cpu) or an SDK sending wall-clock profiles — otherwise the two are
    /// added together and the numbers double count.
    pub fn source_types(&self) -> Vec<ProfileType> {
        if self.is_virtual() {
            vec![
                ProfileType::cpu(),
                ProfileType::off_cpu(),
                ProfileType::wall(),
            ]
        } else {
            vec![self.clone()]
        }
    }

    pub fn unit(&self) -> Unit {
        match self.0.as_str() {
            Self::ALLOC_SPACE | Self::INUSE_SPACE => Unit::Bytes,
            Self::ALLOC_OBJECTS | Self::INUSE_OBJECTS => Unit::Count,
            _ => Unit::Nanoseconds,
        }
    }
}

impl std::fmt::Display for ProfileType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ProfileType {
    fn from(value: &str) -> Self {
        ProfileType(value.to_string())
    }
}

impl From<String> for ProfileType {
    fn from(value: String) -> Self {
        ProfileType(value)
    }
}

/// A symbolized stack frame.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Frame {
    /// Path of the executable / shared object, `[kernel]`, `[vdso]`, ...
    pub module: String,
    /// Demangled function name.
    pub symbol: String,
    /// Source file, when known.
    pub file: String,
    /// Source line, `0` when unknown.
    pub line: u32,
    /// Instruction pointer the frame was built from, `0` when unknown.
    pub addr: u64,
}

impl Frame {
    pub fn new(module: impl Into<String>, symbol: impl Into<String>) -> Self {
        Frame {
            module: module.into(),
            symbol: symbol.into(),
            file: String::new(),
            line: 0,
            addr: 0,
        }
    }

    pub fn with_location(mut self, file: impl Into<String>, line: u32) -> Self {
        self.file = file.into();
        self.line = line;
        self
    }

    pub fn with_addr(mut self, addr: u64) -> Self {
        self.addr = addr;
        self
    }

    /// Best human readable name for the frame.
    pub fn display_name(&self) -> String {
        if !self.symbol.is_empty() {
            self.symbol.clone()
        } else if self.addr != 0 {
            format!("0x{:x}", self.addr)
        } else {
            "<unknown>".to_string()
        }
    }

    /// Stable key used to merge identical frames coming from different
    /// segments. `addr` is deliberately excluded: with ASLR the same function
    /// has a different address in every process.
    pub fn key(&self) -> (&str, &str, &str, u32) {
        (&self.module, &self.symbol, &self.file, self.line)
    }
}

/// Wire representation of a frame inside a [`Report`]: three indices into
/// `Report::strings` followed by the source line.
///
/// `[module, symbol, file, line]`
pub type FrameRef = [u32; 4];

/// One aggregated stack of a [`Report`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackSample {
    /// Frames ordered **leaf first** (the frame that was executing comes
    /// first), which is the order the kernel returns stacks in.
    pub frames: Vec<FrameRef>,
    /// Aggregated value, in the unit implied by the profile type.
    pub value: u64,
}

/// A batch of profile samples produced by an agent and consumed by the
/// storage server.
///
/// The format is intentionally columnar-friendly: all symbols live in a
/// deduplicated string table, stacks reference it by index. That makes the
/// JSON roughly 10x smaller than an object-per-frame encoding and maps
/// directly onto the `batches`/`samples`/`stacks`/`frames` tables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    /// Name of the profiled service. Also injected into the labels as
    /// `service_name`.
    pub service_name: String,
    /// Additional labels, e.g. `namespace`, `pod`, `node`, `version`.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Profile type, e.g. `cpu`.
    pub profile_type: ProfileType,
    /// Start of the collection window, nanoseconds since the Unix epoch.
    pub start_ns: u64,
    /// End of the collection window, nanoseconds since the Unix epoch.
    pub end_ns: u64,
    /// Sampling frequency in Hertz, informational.
    #[serde(default)]
    pub sample_rate_hz: u32,
    /// Deduplicated string table referenced by [`FrameRef`]s.
    pub strings: Vec<String>,
    /// Aggregated stacks.
    pub stacks: Vec<StackSample>,
}

impl Report {
    /// Validate a report coming from an untrusted source.
    pub fn validate(&self) -> crate::Result<()> {
        use crate::Error;

        if self.service_name.trim().is_empty() {
            return Err(Error::InvalidReport("service_name is empty".into()));
        }
        if self.service_name.len() > 512 {
            return Err(Error::InvalidReport("service_name is too long".into()));
        }
        if self.profile_type.as_str().trim().is_empty() {
            return Err(Error::InvalidReport("profile_type is empty".into()));
        }
        if self.profile_type.as_str().len() > 128 {
            return Err(Error::InvalidReport("profile_type is too long".into()));
        }
        if self.end_ns <= self.start_ns {
            return Err(Error::InvalidReport(format!(
                "end_ns ({}) must be greater than start_ns ({})",
                self.end_ns, self.start_ns
            )));
        }
        if self.strings.len() > MAX_STRINGS {
            return Err(Error::InvalidReport(format!(
                "too many strings ({}, max {MAX_STRINGS})",
                self.strings.len()
            )));
        }
        if self.stacks.len() > MAX_STACKS {
            return Err(Error::InvalidReport(format!(
                "too many stacks ({}, max {MAX_STACKS})",
                self.stacks.len()
            )));
        }
        if self.labels.len() > MAX_LABELS {
            return Err(Error::InvalidReport(format!(
                "too many labels ({}, max {MAX_LABELS})",
                self.labels.len()
            )));
        }
        for (key, value) in &self.labels {
            if key.is_empty() || key.len() > MAX_LABEL_KEY_LEN {
                return Err(Error::InvalidReport(format!(
                    "label name `{key}` must be 1..={MAX_LABEL_KEY_LEN} bytes"
                )));
            }
            if value.len() > MAX_LABEL_VALUE_LEN {
                return Err(Error::InvalidReport(format!(
                    "label `{key}` has a {} byte value, max is {MAX_LABEL_VALUE_LEN}",
                    value.len()
                )));
            }
        }
        let max_index = self.strings.len() as u32;
        for (i, stack) in self.stacks.iter().enumerate() {
            if stack.frames.len() > MAX_FRAMES_PER_STACK {
                return Err(Error::InvalidReport(format!(
                    "stack {i} has {} frames (max {MAX_FRAMES_PER_STACK})",
                    stack.frames.len()
                )));
            }
            for frame in &stack.frames {
                for &idx in &frame[..3] {
                    if idx >= max_index {
                        return Err(Error::InvalidReport(format!(
                            "stack {i} references string {idx}, only {max_index} strings present"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Total value of the report, i.e. the sum of all samples.
    pub fn total_value(&self) -> u64 {
        self.stacks.iter().map(|s| s.value).sum()
    }

    /// Labels including the implicit `service_name`.
    pub fn all_labels(&self) -> BTreeMap<String, String> {
        let mut labels = self.labels.clone();
        labels.insert(SERVICE_NAME_LABEL.to_string(), self.service_name.clone());
        labels
    }

    /// Duration covered by the report in nanoseconds.
    pub fn duration_ns(&self) -> u64 {
        self.end_ns.saturating_sub(self.start_ns)
    }
}

/// Helper that interns strings and stacks while assembling a report.
///
/// ```no_run
/// use lidlesseye_core::model::{Frame, ReportBuilder};
///
/// let mut b = ReportBuilder::new("checkout", "cpu", 0, 10_000_000_000);
/// b.add_stack(
///     [Frame::new("/bin/checkout", "main"), Frame::new("[kernel]", "do_syscall_64")],
///     1_000_000,
/// );
/// let report = b.finish();
/// ```
#[derive(Debug)]
pub struct ReportBuilder {
    service_name: String,
    labels: BTreeMap<String, String>,
    profile_type: ProfileType,
    start_ns: u64,
    end_ns: u64,
    sample_rate_hz: u32,
    strings: Vec<String>,
    string_index: HashMap<String, u32>,
    /// stack (as frame refs) -> value, merged in place.
    stacks: Vec<StackSample>,
    stack_index: HashMap<Vec<FrameRef>, usize>,
}

impl ReportBuilder {
    pub fn new(
        service_name: impl Into<String>,
        profile_type: impl Into<ProfileType>,
        start_ns: u64,
        end_ns: u64,
    ) -> Self {
        ReportBuilder {
            service_name: service_name.into(),
            labels: BTreeMap::new(),
            profile_type: profile_type.into(),
            start_ns,
            end_ns,
            sample_rate_hz: 0,
            strings: Vec::new(),
            string_index: HashMap::new(),
            stacks: Vec::new(),
            stack_index: HashMap::new(),
        }
    }

    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    /// Replace all labels at once (used when labels are computed per process).
    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = labels;
        self
    }

    pub fn sample_rate_hz(mut self, hz: u32) -> Self {
        self.sample_rate_hz = hz;
        self
    }

    fn intern(&mut self, value: &str) -> u32 {
        if let Some(&idx) = self.string_index.get(value) {
            return idx;
        }
        let idx = self.strings.len() as u32;
        self.strings.push(value.to_string());
        self.string_index.insert(value.to_string(), idx);
        idx
    }

    /// Add a stack (leaf first) with an aggregated value.
    pub fn add_stack<I>(&mut self, frames: I, value: u64)
    where
        I: IntoIterator<Item = Frame>,
    {
        let mut refs = Vec::new();
        for frame in frames {
            let module = self.intern(&frame.module);
            let symbol = self.intern(&frame.symbol);
            let file = self.intern(&frame.file);
            refs.push([module, symbol, file, frame.line]);
        }
        self.add_refs(refs, value);
    }

    /// Add an already encoded stack.
    pub fn add_refs(&mut self, refs: Vec<FrameRef>, value: u64) {
        if refs.is_empty() || value == 0 {
            return;
        }
        if let Some(&idx) = self.stack_index.get(&refs) {
            self.stacks[idx].value = self.stacks[idx].value.saturating_add(value);
            return;
        }
        self.stack_index.insert(refs.clone(), self.stacks.len());
        self.stacks.push(StackSample {
            frames: refs,
            value,
        });
    }

    /// Merge another report into this one. Both must describe the same
    /// service / type / time window.
    pub fn merge_report(&mut self, other: &Report) {
        let mut remap = Vec::with_capacity(other.strings.len());
        for s in &other.strings {
            remap.push(self.intern(s));
        }
        for stack in &other.stacks {
            let refs = stack
                .frames
                .iter()
                .map(|f| {
                    [
                        remap[f[0] as usize],
                        remap[f[1] as usize],
                        remap[f[2] as usize],
                        f[3],
                    ]
                })
                .collect::<Vec<_>>();
            self.add_refs(refs, stack.value);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.stacks.is_empty()
    }

    pub fn finish(self) -> Report {
        Report {
            service_name: self.service_name,
            labels: self.labels,
            profile_type: self.profile_type,
            start_ns: self.start_ns,
            end_ns: self.end_ns,
            sample_rate_hz: self.sample_rate_hz,
            strings: self.strings,
            stacks: self.stacks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_merges_duplicate_stacks() {
        let mut b = ReportBuilder::new("svc", "cpu", 0, 1_000);
        b.add_stack([Frame::new("bin", "a"), Frame::new("bin", "b")], 10);
        b.add_stack([Frame::new("bin", "a"), Frame::new("bin", "b")], 5);
        let report = b.finish();
        assert_eq!(report.stacks.len(), 1);
        assert_eq!(report.stacks[0].value, 15);
        // module "bin", symbols "a"/"b" and the empty source file are
        // interned exactly once.
        assert_eq!(report.strings.len(), 4);
    }

    #[test]
    fn validate_rejects_bad_ranges() {
        let report = ReportBuilder::new("svc", "cpu", 10, 10).finish();
        assert!(report.validate().is_err());
    }

    #[test]
    fn wall_expands_to_cpu_and_off_cpu() {
        let types = ProfileType::wall().source_types();
        assert_eq!(
            types,
            vec![
                ProfileType::cpu(),
                ProfileType::off_cpu(),
                ProfileType::wall()
            ]
        );
        assert_eq!(ProfileType::wall().unit(), Unit::Nanoseconds);
    }
}
