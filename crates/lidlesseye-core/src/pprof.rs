//! Decoder for the [pprof] protobuf format.
//!
//! Pyroscope's language SDKs, `parca-agent` and every Go service that exposes
//! `net/http/pprof` speak this format, so accepting it is what makes LidlessEye
//! a drop-in backend rather than a closed system. We decode the handful of
//! fields a profile actually needs (`sample_type`, `sample`, `location`,
//! `function`, `string_table`, the two time fields) and ignore the rest —
//! notably `mapping`, which only matters for address symbolization, and that
//! already happened before the profile was written.
//!
//! [pprof]: https://github.com/google/pprof/blob/main/proto/profile.proto

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use crate::error::{Error, Result};
use crate::model::{Frame, MAX_FRAMES_PER_STACK, Report, ReportBuilder};

/// Where the fields pprof does not carry come from.
#[derive(Debug, Clone, Default)]
pub struct PprofOptions {
    /// Service name. Required unless the profile is uploaded with one.
    pub service_name: Option<String>,
    /// Override the profile type instead of deriving it from `sample_type`.
    pub profile_type: Option<String>,
    /// Labels applied to every sample.
    pub labels: BTreeMap<String, String>,
    /// Window start when the profile has no `time_nanos`.
    pub start_ns: Option<u64>,
    /// Window end when the profile has no `duration_nanos`.
    pub end_ns: Option<u64>,
}

/// Refuse absurd inputs before allocating anything.
const MAX_SAMPLES: usize = 2_000_000;
/// pprof samples can carry labels (span ids, thread ids, ...). Turning each
/// distinct label set into its own series explodes cardinality, so beyond this
/// many distinct sets the per-sample labels are dropped.
const MAX_LABEL_SETS: usize = 16;
/// Used when a profile carries neither `time_nanos` nor a duration.
const DEFAULT_WINDOW_NS: u64 = 10_000_000_000;

/// Decompress a gzip member if the payload starts with the gzip magic.
///
/// Some agents send `Content-Encoding: gzip` (handled by the HTTP layer) and
/// some just upload a gzip file; accepting both avoids a class of support
/// tickets.
pub fn maybe_gunzip(data: &[u8]) -> Result<Cow<'_, [u8]>> {
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        let mut decoder = flate2::read::GzDecoder::new(data);
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut out)
            .map_err(|err| Error::Malformed(format!("gzip: {err}")))?;
        return Ok(Cow::Owned(out));
    }
    Ok(Cow::Borrowed(data))
}

/// Decode a pprof profile into one report per distinct label set.
pub fn decode(data: &[u8], opts: &PprofOptions) -> Result<Vec<Report>> {
    let payload = maybe_gunzip(data)?;
    let raw = RawProfile::parse(&payload)?;
    raw.into_reports(opts)
}

// ---------------------------------------------------------------------------
// protobuf plumbing
// ---------------------------------------------------------------------------

const WIRE_VARINT: u8 = 0;
const WIRE_64: u8 = 1;
const WIRE_LEN: u8 = 2;
const WIRE_32: u8 = 5;

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn done(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn varint(&mut self) -> Result<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = *self
                .buf
                .get(self.pos)
                .ok_or_else(|| Error::Malformed("truncated varint".into()))?;
            self.pos += 1;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 64 {
                return Err(Error::Malformed("varint is longer than 64 bits".into()));
            }
        }
    }

    fn tag(&mut self) -> Result<(u32, u8)> {
        let key = self.varint()?;
        Ok(((key >> 3) as u32, (key & 7) as u8))
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= self.buf.len())
            .ok_or_else(|| {
                Error::Malformed("length-delimited field overruns the payload".into())
            })?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.varint()?;
        if len > (self.buf.len() - self.pos) as u64 {
            return Err(Error::Malformed(
                "length-delimited field overruns the payload".into(),
            ));
        }
        self.take(len as usize)
    }

    fn skip(&mut self, wire: u8) -> Result<()> {
        match wire {
            WIRE_VARINT => {
                self.varint()?;
            }
            WIRE_64 => {
                self.take(8)?;
            }
            WIRE_LEN => {
                self.bytes()?;
            }
            WIRE_32 => {
                self.take(4)?;
            }
            other => {
                return Err(Error::Malformed(format!(
                    "unsupported protobuf wire type {other}"
                )));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// profile model
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RawProfile {
    string_table: Vec<String>,
    sample_types: Vec<(i64, i64)>,
    samples: Vec<RawSample>,
    locations: HashMap<u64, RawLocation>,
    functions: HashMap<u64, RawFunction>,
    time_nanos: i64,
    duration_nanos: i64,
}

#[derive(Default)]
struct RawSample {
    locations: Vec<u64>,
    values: Vec<i64>,
    labels: Vec<(i64, i64, i64)>,
}

#[derive(Default)]
struct RawLocation {
    address: u64,
    /// `(function_id, line)` for each inline frame, leaf first.
    lines: Vec<(u64, i64)>,
}

#[derive(Default)]
struct RawFunction {
    name: i64,
    system_name: i64,
    filename: i64,
}

impl RawProfile {
    fn parse(buf: &[u8]) -> Result<Self> {
        let mut profile = RawProfile::default();
        let mut reader = Reader::new(buf);
        while !reader.done() {
            let (field, wire) = reader.tag()?;
            match (field, wire) {
                (1, WIRE_LEN) => profile
                    .sample_types
                    .push(parse_value_type(reader.bytes()?)?),
                (2, WIRE_LEN) => {
                    if profile.samples.len() >= MAX_SAMPLES {
                        return Err(Error::Malformed(format!(
                            "profile has more than {MAX_SAMPLES} samples"
                        )));
                    }
                    profile.samples.push(parse_sample(reader.bytes()?)?);
                }
                (4, WIRE_LEN) => {
                    let (id, location) = parse_location(reader.bytes()?)?;
                    profile.locations.insert(id, location);
                }
                (5, WIRE_LEN) => {
                    let (id, function) = parse_function(reader.bytes()?)?;
                    profile.functions.insert(id, function);
                }
                (6, WIRE_LEN) => {
                    let bytes = reader.bytes()?;
                    let value = std::str::from_utf8(bytes)
                        .map_err(|_| Error::Malformed("string table is not UTF-8".into()))?;
                    profile.string_table.push(value.to_string());
                }
                (9, WIRE_VARINT) => profile.time_nanos = reader.varint()? as i64,
                (10, WIRE_VARINT) => profile.duration_nanos = reader.varint()? as i64,
                _ => reader.skip(wire)?,
            }
        }
        Ok(profile)
    }

    fn string(&self, index: i64) -> &str {
        if index < 0 {
            return "";
        }
        self.string_table
            .get(index as usize)
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    fn into_reports(self, opts: &PprofOptions) -> Result<Vec<Report>> {
        let service_name = opts
            .service_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                Error::Malformed(
                    "pprof upload has no service name; send it as `?service_name=` or \
                     `?name=`"
                        .into(),
                )
            })?;
        let profile_type = match &opts.profile_type {
            Some(explicit) => explicit.clone(),
            None => self.derive_profile_type(),
        };

        let now = crate::time::now_ns();
        let end_ns = if self.duration_nanos > 0 {
            let start = if self.time_nanos > 0 {
                self.time_nanos as u64
            } else {
                now.saturating_sub(self.duration_nanos as u64)
            };
            start.saturating_add(self.duration_nanos as u64)
        } else {
            opts.end_ns.unwrap_or(now)
        };
        let start_ns = if self.duration_nanos > 0 {
            end_ns.saturating_sub(self.duration_nanos as u64)
        } else {
            opts.start_ns
                .unwrap_or_else(|| end_ns.saturating_sub(DEFAULT_WINDOW_NS))
        };

        // Resolve labels first: if the profile uses too many distinct label
        // sets we drop them wholesale rather than explode into thousands of
        // one-sample series.
        let mut label_sets: BTreeMap<BTreeMap<String, String>, Vec<usize>> = BTreeMap::new();
        for (i, sample) in self.samples.iter().enumerate() {
            let mut labels = BTreeMap::new();
            for &(key, str_index, num) in &sample.labels {
                let name = self.string(key).trim();
                if name.is_empty() {
                    continue;
                }
                let value = if str_index > 0 {
                    self.string(str_index).to_string()
                } else {
                    num.to_string()
                };
                labels.insert(name.to_string(), value);
            }
            label_sets.entry(labels).or_default().push(i);
        }
        let keep_sample_labels = !label_sets.is_empty() && label_sets.len() <= MAX_LABEL_SETS;
        if !keep_sample_labels {
            label_sets.clear();
            label_sets.insert(BTreeMap::new(), (0..self.samples.len()).collect());
        }

        let mut reports = Vec::new();
        for (sample_labels, indices) in label_sets {
            let mut builder =
                ReportBuilder::new(&service_name, profile_type.clone(), start_ns, end_ns);
            for (key, value) in &opts.labels {
                builder = builder.label(key.clone(), value.clone());
            }
            if keep_sample_labels {
                for (key, value) in &sample_labels {
                    builder = builder.label(key.clone(), value.clone());
                }
            }
            let mut any = false;
            for index in indices {
                let sample = &self.samples[index];
                let value = sample.values.first().copied().unwrap_or(0);
                if value <= 0 {
                    continue;
                }
                let frames = self.frames_of(sample);
                if frames.is_empty() {
                    continue;
                }
                builder.add_stack(frames, value as u64);
                any = true;
            }
            if any {
                reports.push(builder.finish());
            }
        }
        Ok(reports)
    }

    fn frames_of(&self, sample: &RawSample) -> Vec<Frame> {
        let mut frames = Vec::new();
        for &location_id in &sample.locations {
            if frames.len() >= MAX_FRAMES_PER_STACK {
                break;
            }
            let Some(location) = self.locations.get(&location_id) else {
                continue;
            };
            if location.lines.is_empty() {
                frames.push(Frame::new("", format!("0x{:x}", location.address)));
                continue;
            }
            for &(function_id, line) in location.lines.iter().take(4) {
                if frames.len() >= MAX_FRAMES_PER_STACK {
                    break;
                }
                let Some(function) = self.functions.get(&function_id) else {
                    frames.push(Frame::new("", format!("0x{:x}", location.address)));
                    continue;
                };
                let name = if !self.string(function.name).is_empty() {
                    self.string(function.name)
                } else {
                    self.string(function.system_name)
                };
                let name = if name.is_empty() {
                    format!("0x{:x}", location.address)
                } else {
                    name.to_string()
                };
                let file = self.string(function.filename).to_string();
                let line = if line > 0 { line as u32 } else { 0 };
                let mut frame = Frame::new("", name);
                if !file.is_empty() {
                    frame = frame.with_location(file, line);
                }
                frames.push(frame);
            }
        }
        frames
    }

    fn derive_profile_type(&self) -> String {
        let name = self
            .sample_types
            .first()
            .map(|(type_index, _)| self.string(*type_index))
            .unwrap_or("");
        if name.trim().is_empty() {
            return crate::model::ProfileType::CPU.to_string();
        }
        normalize_profile_type(name)
    }
}

fn parse_value_type(buf: &[u8]) -> Result<(i64, i64)> {
    let mut reader = Reader::new(buf);
    let (mut type_index, mut unit_index) = (0i64, 0i64);
    while !reader.done() {
        let (field, wire) = reader.tag()?;
        match (field, wire) {
            (1, WIRE_VARINT) => type_index = reader.varint()? as i64,
            (2, WIRE_VARINT) => unit_index = reader.varint()? as i64,
            _ => reader.skip(wire)?,
        }
    }
    Ok((type_index, unit_index))
}

fn parse_sample(buf: &[u8]) -> Result<RawSample> {
    let mut reader = Reader::new(buf);
    let mut sample = RawSample::default();
    while !reader.done() {
        let (field, wire) = reader.tag()?;
        match (field, wire) {
            (1, WIRE_VARINT) => sample.locations.push(reader.varint()?),
            // `value` is a packed repeated int64.
            (2, WIRE_VARINT) => sample.values.push(reader.varint()? as i64),
            (2, WIRE_LEN) => {
                let mut packed = Reader::new(reader.bytes()?);
                while !packed.done() {
                    sample.values.push(packed.varint()? as i64);
                }
            }
            (3, WIRE_LEN) => {
                let mut label = Reader::new(reader.bytes()?);
                let (mut key, mut str_index, mut num) = (0i64, 0i64, 0i64);
                while !label.done() {
                    let (field, wire) = label.tag()?;
                    match (field, wire) {
                        (1, WIRE_VARINT) => key = label.varint()? as i64,
                        (2, WIRE_VARINT) => str_index = label.varint()? as i64,
                        (3, WIRE_VARINT) => num = label.varint()? as i64,
                        _ => label.skip(wire)?,
                    }
                }
                sample.labels.push((key, str_index, num));
            }
            _ => reader.skip(wire)?,
        }
    }
    Ok(sample)
}

fn parse_location(buf: &[u8]) -> Result<(u64, RawLocation)> {
    let mut reader = Reader::new(buf);
    let mut location = RawLocation::default();
    let mut id = 0u64;
    while !reader.done() {
        let (field, wire) = reader.tag()?;
        match (field, wire) {
            (1, WIRE_VARINT) => id = reader.varint()?,
            (3, WIRE_VARINT) => location.address = reader.varint()?,
            (4, WIRE_LEN) => {
                let mut line = Reader::new(reader.bytes()?);
                let (mut function_id, mut line_no) = (0u64, 0i64);
                while !line.done() {
                    let (field, wire) = line.tag()?;
                    match (field, wire) {
                        (1, WIRE_VARINT) => function_id = line.varint()?,
                        (2, WIRE_VARINT) => line_no = line.varint()? as i64,
                        _ => line.skip(wire)?,
                    }
                }
                location.lines.push((function_id, line_no));
            }
            _ => reader.skip(wire)?,
        }
    }
    Ok((id, location))
}

fn parse_function(buf: &[u8]) -> Result<(u64, RawFunction)> {
    let mut reader = Reader::new(buf);
    let mut function = RawFunction::default();
    let mut id = 0u64;
    while !reader.done() {
        let (field, wire) = reader.tag()?;
        match (field, wire) {
            (1, WIRE_VARINT) => id = reader.varint()?,
            (2, WIRE_VARINT) => function.name = reader.varint()? as i64,
            (3, WIRE_VARINT) => function.system_name = reader.varint()? as i64,
            (4, WIRE_VARINT) => function.filename = reader.varint()? as i64,
            _ => reader.skip(wire)?,
        }
    }
    Ok((id, function))
}

/// Map the `sample_type` names of the common runtimes onto our profile types.
fn normalize_profile_type(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    let mapped = match lower.as_str() {
        "samples" | "cpu" | "process_cpu" | "cpu_time" | "cpu_nanoseconds" | "oncpu" => "cpu",
        "wall" | "wall_clock" | "wall_time" | "offcpu" | "off_cpu" => "wall",
        "alloc_space" | "alloc_space_bytes" | "alloc_bytes" | "alloc" => "alloc_space",
        "alloc_objects" | "alloc_count" | "objects" => "alloc_objects",
        "inuse_space" | "inuse_space_bytes" | "inuse_bytes" => "inuse_space",
        "inuse_objects" | "inuse_count" => "inuse_objects",
        "goroutine" | "goroutines" => "goroutine",
        "mutex_count" => "mutex_count",
        "mutex_duration" => "mutex_duration",
        "block_count" => "block_count",
        "block_duration" => "block_duration",
        other => {
            // Keep unknown types, but reduce them to a stable, printable key:
            // the value is stored as `profile_type` and is matched exactly by
            // queries, so two spellings must not become two types.
            let slug: String = other
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .take(48)
                .collect();
            return if slug.is_empty() || slug == "_" {
                crate::model::ProfileType::CPU.to_string()
            } else {
                slug
            };
        }
    };
    mapped.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal protobuf encoder used to build fixtures without a codegen
    /// dependency.
    struct Encoder {
        out: Vec<u8>,
    }

    impl Encoder {
        fn new() -> Self {
            Encoder { out: Vec::new() }
        }

        fn varint(&mut self, mut value: u64) {
            while value >= 0x80 {
                self.out.push((value as u8) | 0x80);
                value >>= 7;
            }
            self.out.push(value as u8);
        }

        fn tag(&mut self, field: u32, wire: u8) {
            self.varint(((field as u64) << 3) | wire as u64);
        }

        fn uint(&mut self, field: u32, value: u64) {
            self.tag(field, WIRE_VARINT);
            self.varint(value);
        }

        fn bytes(&mut self, field: u32, value: &[u8]) {
            self.tag(field, WIRE_LEN);
            self.varint(value.len() as u64);
            self.out.extend_from_slice(value);
        }
    }

    fn strings(names: &[&str]) -> Vec<u8> {
        let mut e = Encoder::new();
        for name in names {
            e.bytes(6, name.as_bytes());
        }
        e.out
    }

    #[test]
    fn decodes_a_minimal_profile() {
        let table = [
            "",             // 0
            "cpu",          // 1
            "nanoseconds",  // 2
            "main",         // 3
            "handler",      // 4
            "/app/main.go", // 5
        ];
        let mut e = Encoder::new();
        // sample_type { type = "cpu" unit = "nanoseconds" }
        let mut vt = Encoder::new();
        vt.uint(1, 1);
        vt.uint(2, 2);
        e.bytes(1, &vt.out);

        // function id 1 -> main
        let mut f1 = Encoder::new();
        f1.uint(1, 1);
        f1.uint(2, 3);
        f1.uint(4, 5);
        e.bytes(5, &f1.out);
        // function id 2 -> handler
        let mut f2 = Encoder::new();
        f2.uint(1, 2);
        f2.uint(2, 4);
        f2.uint(4, 5);
        e.bytes(5, &f2.out);

        // location 10 -> line { function_id = 2, line = 42 }
        let mut line = Encoder::new();
        line.uint(1, 2);
        line.uint(2, 42);
        let mut loc = Encoder::new();
        loc.uint(1, 10);
        loc.bytes(4, &line.out);
        e.bytes(4, &loc.out);
        // location 11 -> line { function_id = 1 }
        let mut line = Encoder::new();
        line.uint(1, 1);
        let mut loc = Encoder::new();
        loc.uint(1, 11);
        loc.bytes(4, &line.out);
        e.bytes(4, &loc.out);

        // sample: locations [10, 11], value [1500]
        let mut sample = Encoder::new();
        sample.uint(1, 10);
        sample.uint(1, 11);
        sample.uint(2, 1500);
        e.bytes(2, &sample.out);

        e.uint(9, 1_700_000_000_000_000_000);
        e.uint(10, 10_000_000_000);
        e.out.extend_from_slice(&strings(&table));

        let reports = decode(
            &e.out,
            &PprofOptions {
                service_name: Some("checkout".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(reports.len(), 1);
        let report = &reports[0];
        assert_eq!(report.service_name, "checkout");
        assert_eq!(report.profile_type.as_str(), "cpu");
        assert_eq!(report.total_value(), 1500);
        assert_eq!(report.start_ns, 1_700_000_000_000_000_000);
        assert_eq!(report.end_ns, 1_700_000_010_000_000_000);
        let stack = &report.stacks[0];
        assert_eq!(stack.frames.len(), 2, "leaf first");
        let handler = report.strings[stack.frames[0][1] as usize].clone();
        let main = report.strings[stack.frames[1][1] as usize].clone();
        assert_eq!(handler, "handler");
        assert_eq!(main, "main");
        assert_eq!(stack.frames[0][3], 42);
    }

    #[test]
    fn rejects_truncated_payloads() {
        let mut e = Encoder::new();
        e.tag(2, WIRE_LEN);
        e.varint(64); // claims 64 bytes, provides none
        assert!(decode(&e.out, &PprofOptions::default()).is_err());
    }

    #[test]
    fn requires_a_service_name() {
        let mut e = Encoder::new();
        e.bytes(6, b"cpu");
        e.bytes(6, b"");
        let err = decode(&e.out, &PprofOptions::default()).unwrap_err();
        assert!(err.to_string().contains("service name"), "{err}");
    }

    #[test]
    fn gunzips_when_needed() {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(b"hello").unwrap();
        let compressed = encoder.finish().unwrap();
        assert_eq!(maybe_gunzip(&compressed).unwrap().as_ref(), b"hello");
        assert_eq!(maybe_gunzip(b"plain").unwrap().as_ref(), b"plain");
    }

    #[test]
    fn normalizes_common_sample_types() {
        assert_eq!(normalize_profile_type("samples"), "cpu");
        assert_eq!(normalize_profile_type("wall"), "wall");
        assert_eq!(normalize_profile_type("alloc_space"), "alloc_space");
        assert_eq!(normalize_profile_type("my/weird type"), "my_weird_type");
    }
}
