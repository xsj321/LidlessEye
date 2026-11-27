//! Append-only segment log for ingest.
//!
//! Ingest used to be a synchronous round trip through one writer thread: the
//! HTTP task queued a report batch and waited for DuckDB to commit it, with a
//! deep queue in between. That shape has a hard ceiling — every upload pays the
//! writer's service time, so a cluster with enough agents fills the queue and
//! every request starts timing out at thirty seconds.
//!
//! This module is the other shape. An upload is encoded and appended to a
//! segment file, and the caller is answered as soon as the bytes are in the
//! file (and, at most once per [`WalOptions::sync_interval`], on the platter).
//! A background converter turns closed segments into Parquet. Ingest therefore
//! costs one sequential write, and its throughput is disk bandwidth rather than
//! one core's transaction rate.
//!
//! Segment layout:
//!
//! ```text
//! [ 8-byte file header ][ record ][ record ] ...
//!
//! file header:  "LEPW" | version u8 | reserved [u8; 3]
//! record:       len u32 | crc32 u32 | payload [len]   (bincode of one Report)
//! ```
//!
//! A crash can only tear the last record. Reading validates every checksum and
//! truncates a torn tail instead of failing the whole segment, so a killed
//! process loses the in-flight record and nothing else.

use parking_lot::{Condvar, Mutex};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::model::Report;
use crate::{Error, Result};

/// Identifies a LidlessEye segment file: "LEPW" (LidlessEye Profile WAL).
const MAGIC: [u8; 4] = *b"LEPW";
/// Bumped when the record payload encoding changes. Segments carry their own
/// version so an upgrade can refuse to guess rather than misread.
const VERSION: u8 = 1;
const FILE_HEADER_LEN: u64 = 8;
const RECORD_HEADER_LEN: u64 = 8;
/// Suffix of a segment a converter is working on. Claiming is a `rename`, so
/// exactly one of N converters can win a segment without a lock.
const CLAIM_SUFFIX: &str = ".work";
/// Suffix of a segment whose rows are already in the hot tier but whose flush
/// failed. Retrying must only flush, never append again.
const FLUSH_SUFFIX: &str = ".flush";

/// A single encoded report larger than this is refused rather than trusted:
/// one corrupt length field must not make a reader allocate a gigabyte.
const MAX_RECORD_BYTES: u32 = 256 * 1024 * 1024;

/// Knobs for the segment log.
#[derive(Debug, Clone)]
pub struct WalOptions {
    /// Seal the active segment once it reaches this size.
    pub segment_bytes: u64,
    /// Seal the active segment once it is this old, so a trickle of uploads
    /// still becomes queryable promptly.
    pub segment_age: Duration,
    /// At most one `fsync` per interval, shared by every upload that arrived
    /// inside it. Zero fsyncs on every append.
    pub sync_interval: Duration,
    /// Refuse new uploads once this many bytes are waiting to be converted.
    /// Backpressure is a byte count, not a queue length: a job is an upload and
    /// its size varies by two orders of magnitude.
    pub max_pending_bytes: u64,
}

impl Default for WalOptions {
    fn default() -> Self {
        WalOptions {
            segment_bytes: 8 * 1024 * 1024,
            segment_age: Duration::from_secs(2),
            sync_interval: Duration::from_millis(100),
            max_pending_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}

/// What one append did, for the HTTP response.
#[derive(Debug, Clone, Copy, Default)]
pub struct Appended {
    pub reports: usize,
    pub bytes: u64,
    /// Sequence number of the segment the records landed in.
    pub segment: u64,
    /// Byte offset just past the last record written. Together with `segment`
    /// this names a durable position, which is what `?sync=1` waits for.
    pub offset: u64,
    /// Whether this append also forced the file to stable storage. `false`
    /// still means "queued in the kernel"; a crash may lose up to one
    /// [`WalOptions::sync_interval`] of data, never more.
    pub synced: bool,
}

/// Result of converting one batch of segments.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConvertStats {
    pub segments: usize,
    pub reports: usize,
    pub rejected: usize,
    pub flushed_rows: usize,
}

struct Active {
    /// Shared so the sync thread can hold the descriptor without holding the
    /// append lock: a `fsync` of tens of megabytes takes hundreds of
    /// milliseconds, and every upload queued behind it pays for it.
    file: std::sync::Arc<File>,
    seq: u64,
    bytes: u64,
    created: Instant,
}

/// The highest `(segment, offset)` known to be on stable storage.
#[derive(Default, Clone, Copy)]
struct Durable {
    seq: u64,
    offset: u64,
}

pub struct SegmentLog {
    dir: PathBuf,
    opts: WalOptions,
    active: Mutex<Option<Active>>,
    /// Wakes appenders waiting for `sync=1`, and the sync thread itself.
    durable: Mutex<Durable>,
    synced: Condvar,
    /// Wakes the converter when a segment is sealed.
    sealed: Condvar,
    next_seq: AtomicU64,
    pending_bytes: AtomicU64,
}

impl SegmentLog {
    /// Open (or create) the segment directory.
    ///
    /// Files left by a previous process are *not* converted here: the converter
    /// owns that, and doing it at open would put the cost back on the startup
    /// path. The sequence counter resumes past the highest file name so a new
    /// segment can never overwrite an unconverted one.
    pub fn open(dir: impl Into<PathBuf>, opts: WalOptions) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(Error::Io)?;
        let mut highest = 0u64;
        let mut pending = 0u64;
        for entry in std::fs::read_dir(&dir).map_err(Error::Io)? {
            let entry = entry.map_err(Error::Io)?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(seq) = seq_of(&name, ".log") {
                highest = highest.max(seq);
                pending = pending.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
            } else if let Some(seq) = seq_of(&name, ".applied") {
                // A segment that was durable in Parquet but not yet unlinked
                // when the process died. Its rows are already in the archive.
                highest = highest.max(seq);
                let _ = std::fs::remove_file(entry.path());
            } else if let Some(seq) = seq_of(&name, FLUSH_SUFFIX) {
                highest = highest.max(seq);
                let back = dir.join(format!("seg-{seq:012}.log"));
                if std::fs::rename(entry.path(), &back).is_ok() {
                    pending = pending
                        .saturating_add(std::fs::metadata(&back).map(|m| m.len()).unwrap_or(0));
                }
            } else if let Some(seq) = seq_of(&name, CLAIM_SUFFIX) {
                // A converter claimed this segment and then the process died.
                // Nothing was published for it (publishing renames to
                // `.applied` first), so it goes back to the queue.
                highest = highest.max(seq);
                let back = dir.join(format!("seg-{seq:012}.log"));
                if std::fs::rename(entry.path(), &back).is_ok() {
                    pending = pending
                        .saturating_add(std::fs::metadata(&back).map(|m| m.len()).unwrap_or(0));
                }
            }
        }
        Ok(SegmentLog {
            dir,
            opts,
            active: Mutex::new(None),
            durable: Mutex::new(Durable::default()),
            synced: Condvar::new(),
            sealed: Condvar::new(),
            next_seq: AtomicU64::new(highest + 1),
            pending_bytes: AtomicU64::new(pending),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Bytes written to segments that the converter has not drained yet.
    pub fn pending_bytes(&self) -> u64 {
        self.pending_bytes.load(Ordering::Relaxed)
    }

    /// Whether an upload of `bytes` would exceed the pending cap.
    pub fn would_exceed(&self, bytes: u64) -> bool {
        self.pending_bytes().saturating_add(bytes) > self.opts.max_pending_bytes
    }

    /// Encode and append every report, returning once the bytes are written.
    pub fn append(&self, reports: &[Report]) -> Result<Appended> {
        if reports.is_empty() {
            return Ok(Appended::default());
        }
        let mut encoded: Vec<Vec<u8>> = Vec::with_capacity(reports.len());
        let mut payload = 0u64;
        for report in reports {
            let bytes = bincode::serialize(report)
                .map_err(|err| Error::Db(format!("encoding a report for the log: {err}")))?;
            payload = payload.saturating_add(bytes.len() as u64 + RECORD_HEADER_LEN);
            encoded.push(bytes);
        }
        // Backpressure is a byte budget, not a queue length: a report can be
        // tens of kilobytes or a few megabytes, so "N entries" says nothing
        // about how far behind the converter is.
        if self.would_exceed(payload) {
            return Err(Error::Busy(format!(
                "the ingest log holds {} bytes waiting to be converted (limit {}); \
                 the converter cannot keep up with this write rate",
                self.pending_bytes(),
                self.opts.max_pending_bytes
            )));
        }
        let mut guard = self.active.lock();
        // Seal first, so the append that crosses the threshold lands in a fresh
        // segment and the closed one becomes visible to the converter.
        let rotate = match guard.as_ref() {
            Some(active) => {
                active.bytes + payload > self.opts.segment_bytes
                    || active.created.elapsed() >= self.opts.segment_age
            }
            None => false,
        };
        if rotate && let Some(active) = guard.take() {
            let _ = active.file.sync_data();
            self.advance_durable(active.seq, active.bytes);
            debug!(segment = active.seq, bytes = active.bytes, "sealed segment");
            self.sealed.notify_all();
        }
        if guard.is_none() {
            let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
            *guard = Some(Active {
                file: std::sync::Arc::new(self.create_segment(seq)?),
                seq,
                bytes: 0,
                created: Instant::now(),
            });
        }
        let active = guard.as_mut().expect("opened above");
        for bytes in &encoded {
            let len = u32::try_from(bytes.len())
                .map_err(|_| Error::InvalidReport("a report is too large for the log".into()))?;
            let mut header = [0u8; RECORD_HEADER_LEN as usize];
            header[..4].copy_from_slice(&len.to_le_bytes());
            header[4..].copy_from_slice(&crc32fast::hash(bytes).to_le_bytes());
            active.file.write_all(&header).map_err(Error::Io)?;
            active.file.write_all(bytes).map_err(Error::Io)?;
            active.bytes += RECORD_HEADER_LEN + bytes.len() as u64;
        }
        let segment = active.seq;
        let offset = active.bytes;
        self.pending_bytes.fetch_add(payload, Ordering::Relaxed);
        // Nothing is fsynced here on purpose. A `fsync` of a few hundred
        // milliseconds — which is what a burst of uploads produces — used to
        // run on whichever caller happened to cross the interval, while holding
        // the append lock. That single decision is what turned a 32-client burst
        // into a 900 ms p50: the flush thread now does it out of band, and an
        // append costs a `write` and nothing else.
        let synced = if self.opts.sync_interval.is_zero() {
            // Configured for "every append is durable before it returns".
            active.file.sync_data().map_err(Error::Io)?;
            self.advance_durable(segment, offset);
            true
        } else {
            let durable = *self.durable.lock();
            durable.seq > segment || (durable.seq == segment && durable.offset >= offset)
        };
        Ok(Appended {
            reports: encoded.len(),
            bytes: payload,
            segment,
            offset,
            synced,
        })
    }

    /// Sequence numbers of closed segments, oldest first.
    ///
    /// The active segment is excluded: it may still be growing.
    pub fn closed_segments(&self) -> Vec<u64> {
        let active_seq = self.active.lock().as_ref().map(|a| a.seq);
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(seq) = seq_of(&name, ".log")
                && Some(seq) != active_seq
            {
                out.push(seq);
            }
        }
        out.sort_unstable();
        out
    }

    /// Close the active segment now. Used on shutdown and by tests that need
    /// determinism rather than a wall-clock threshold.
    pub fn seal(&self) {
        let mut guard = self.active.lock();
        if let Some(active) = guard.take() {
            let _ = active.file.sync_data();
            self.advance_durable(active.seq, active.bytes);
            debug!(segment = active.seq, bytes = active.bytes, "sealed segment");
            self.sealed.notify_all();
        }
    }

    /// Seal the active segment if it has reached its age limit.
    ///
    /// Called by the converter's tick, so a quiet tenant's last few uploads do
    /// not sit unqueryable until the next one arrives.
    pub fn seal_if_due(&self) {
        let mut guard = self.active.lock();
        let due = guard
            .as_ref()
            .is_some_and(|active| active.created.elapsed() >= self.opts.segment_age);
        if due && let Some(active) = guard.take() {
            let _ = active.file.sync_data();
            debug!(
                segment = active.seq,
                bytes = active.bytes,
                "sealed segment on age"
            );
            self.sealed.notify_all();
        }
    }

    /// Flush the active segment's bytes to stable storage.
    ///
    /// Runs on its own thread. The descriptor is cloned out of the append lock
    /// first, so a slow disk delays durability — never uploads. The position is
    /// read *before* the `fsync`, which makes the resulting claim conservative:
    /// a write that raced the flush is simply not counted as durable yet.
    pub fn sync_active(&self) -> Result<bool> {
        let (file, seq, offset) = {
            let guard = self.active.lock();
            match guard.as_ref() {
                Some(active) if active.bytes > 0 => (active.file.clone(), active.seq, active.bytes),
                _ => return Ok(false),
            }
        };
        {
            let durable = self.durable.lock();
            if durable.seq == seq && durable.offset >= offset {
                return Ok(false);
            }
        }
        file.sync_data().map_err(Error::Io)?;
        self.advance_durable(seq, offset);
        Ok(true)
    }

    /// Publish the highest durable position and wake `sync=1` waiters.
    fn advance_durable(&self, seq: u64, offset: u64) {
        let mut durable = self.durable.lock();
        if durable.seq > seq || (durable.seq == seq && durable.offset >= offset) {
            return;
        }
        *durable = Durable { seq, offset };
        self.synced.notify_all();
    }

    /// Wait until `(seq, offset)` is durable, or the timeout expires.
    pub fn await_durable(&self, seq: u64, offset: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut durable = self.durable.lock();
        loop {
            if durable.seq > seq || (durable.seq == seq && durable.offset >= offset) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            self.synced.wait_for(&mut durable, deadline - now);
        }
    }

    /// Block until a segment is sealed or the timeout expires.
    pub fn wait_for_segment(&self, timeout: Duration) {
        let mut guard = self.active.lock();
        self.sealed.wait_for(&mut guard, timeout);
    }

    /// Wake the converter immediately (used on shutdown and in tests).
    pub fn wake(&self) {
        self.sealed.notify_all();
    }

    /// Decode every report in one closed segment.
    ///
    /// Returns `Ok(None)` when the file vanished (another converter won the
    /// race), and truncates a torn tail so the next read starts clean.
    pub fn read_segment(&self, seq: u64) -> Result<Option<Vec<Report>>> {
        self.read_at(seq, &self.path_of(seq))
    }

    /// Decode a segment this converter has claimed.
    pub fn read_claimed(&self, seq: u64) -> Result<Option<Vec<Report>>> {
        let path = self.dir.join(format!("seg-{seq:012}{CLAIM_SUFFIX}"));
        self.read_at(seq, &path)
    }

    fn read_at(&self, seq: u64, path: &Path) -> Result<Option<Vec<Report>>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(Error::Io(err)),
        };
        let mut header = [0u8; FILE_HEADER_LEN as usize];
        let mut reader = BufReader::new(file);
        if reader.read_exact(&mut header).is_err() || header[..4] != MAGIC {
            warn!(segment = seq, "segment has no header; discarding it");
            let _ = std::fs::remove_file(path);
            self.forget(seq);
            return Ok(None);
        }
        if header[4] != VERSION {
            return Err(Error::Db(format!(
                "segment {seq} was written by log format {}, this build reads {VERSION}",
                header[4]
            )));
        }
        let mut reports = Vec::new();
        let mut offset = FILE_HEADER_LEN;
        let mut record = [0u8; RECORD_HEADER_LEN as usize];
        loop {
            match reader.read_exact(&mut record) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(err) => return Err(Error::Io(err)),
            }
            let len = u32::from_le_bytes(record[..4].try_into().expect("4 bytes"));
            let crc = u32::from_le_bytes(record[4..].try_into().expect("4 bytes"));
            if len == 0 || len > MAX_RECORD_BYTES {
                warn!(
                    segment = seq,
                    len, "implausible record length; truncating segment"
                );
                break;
            }
            let mut payload = vec![0u8; len as usize];
            match reader.read_exact(&mut payload) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(err) => return Err(Error::Io(err)),
            }
            if crc32fast::hash(&payload) != crc {
                warn!(segment = seq, "checksum mismatch; truncating segment");
                break;
            }
            match bincode::deserialize::<Report>(&payload) {
                Ok(report) => reports.push(report),
                Err(err) => warn!(segment = seq, error = %err, "unreadable record; skipping it"),
            }
            offset += RECORD_HEADER_LEN + len as u64;
        }
        // A crash leaves at most one torn record; cut it off so the segment can
        // be marked applied without leaving unreadable bytes behind.
        if let Ok(file) = OpenOptions::new().write(true).open(path) {
            let _ = file.set_len(offset);
        }
        Ok(Some(reports))
    }

    /// Take one closed segment for conversion, or `None` when there is none.
    ///
    /// Claiming is a rename to `.work`: on one filesystem exactly one caller
    /// can win it, which is what lets several converters run without a lock and
    /// without ever publishing the same segment twice.
    pub fn claim_next(&self) -> Option<u64> {
        let active_seq = self.active.lock().as_ref().map(|active| active.seq);
        let entries = std::fs::read_dir(&self.dir).ok()?;
        let mut candidates: Vec<u64> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(seq) = seq_of(&name, ".log")
                && Some(seq) != active_seq
            {
                candidates.push(seq);
            }
        }
        candidates.sort_unstable();
        for seq in candidates {
            let from = self.path_of(seq);
            let to = self.dir.join(format!("seg-{seq:012}{CLAIM_SUFFIX}"));
            if std::fs::rename(&from, &to).is_ok() {
                return Some(seq);
            }
            // Lost the race (another converter claimed it); try the next one.
        }
        None
    }

    /// Return a claimed segment to the queue after a failed conversion.
    pub fn release_claim(&self, seq: u64) {
        let from = self.dir.join(format!("seg-{seq:012}{CLAIM_SUFFIX}"));
        let to = self.path_of(seq);
        if let Err(err) = std::fs::rename(&from, &to) {
            warn!(segment = seq, error = %err, "could not release a claimed segment");
        }
    }

    /// Publish a converted segment and drop it.
    ///
    /// The `.applied` rename is the durable claim: a crash between "rows are in
    /// Parquet" and "file is unlinked" leaves a marker that [`open`] deletes
    /// without reprocessing. Anything else would double-count the last segment
    /// of every crash.
    pub fn finish_claim(&self, seq: u64) {
        let claimed = self.dir.join(format!("seg-{seq:012}{CLAIM_SUFFIX}"));
        let applied = self.dir.join(format!("seg-{seq:012}.applied"));
        let size = std::fs::metadata(&claimed).map(|m| m.len()).unwrap_or(0);
        match std::fs::rename(&claimed, &applied) {
            Ok(()) => {
                let _ = std::fs::remove_file(&applied);
                self.pending_bytes
                    .fetch_sub(size.min(self.pending_bytes()), Ordering::Relaxed);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => warn!(segment = seq, error = %err, "could not retire a segment"),
        }
    }

    /// Record that a segment's rows reached the hot tier but the flush failed.
    pub fn mark_flushing(&self, seq: u64) {
        let from = self.dir.join(format!("seg-{seq:012}{CLAIM_SUFFIX}"));
        let to = self.dir.join(format!("seg-{seq:012}{FLUSH_SUFFIX}"));
        if let Err(err) = std::fs::rename(&from, &to) {
            warn!(segment = seq, error = %err, "could not record a failed flush");
        }
    }

    /// Segments waiting for a retried flush, oldest first.
    pub fn pending_flush(&self) -> Vec<u64> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(seq) = seq_of(&name, FLUSH_SUFFIX) {
                out.push(seq);
            }
        }
        out.sort_unstable();
        out
    }

    /// Retire a segment whose rows are now in Parquet.
    pub fn finish_flush(&self, seq: u64) {
        let path = self.dir.join(format!("seg-{seq:012}{FLUSH_SUFFIX}"));
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                self.pending_bytes
                    .fetch_sub(size.min(self.pending_bytes()), Ordering::Relaxed);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => warn!(segment = seq, error = %err, "could not retire a segment"),
        }
    }

    /// Mark a segment converted: rename, then unlink.
    pub fn mark_applied(&self, seq: u64) {
        let path = self.path_of(seq);
        let applied = self.dir.join(format!("seg-{seq:012}.applied"));
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        match std::fs::rename(&path, &applied) {
            Ok(()) => {
                let _ = std::fs::remove_file(&applied);
                self.pending_bytes
                    .fetch_sub(size.min(self.pending_bytes()), Ordering::Relaxed);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => warn!(segment = seq, error = %err, "could not retire a segment"),
        }
    }

    fn forget(&self, seq: u64) {
        let size = std::fs::metadata(self.path_of(seq))
            .map(|m| m.len())
            .unwrap_or(0);
        self.pending_bytes
            .fetch_sub(size.min(self.pending_bytes()), Ordering::Relaxed);
    }

    fn path_of(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("seg-{seq:012}.log"))
    }

    fn create_segment(&self, seq: u64) -> Result<File> {
        let path = self.path_of(seq);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(Error::Io)?;
        let mut header = [0u8; FILE_HEADER_LEN as usize];
        header[..4].copy_from_slice(&MAGIC);
        header[4] = VERSION;
        file.write_all(&header).map_err(Error::Io)?;
        file.sync_data().map_err(Error::Io)?;
        self.pending_bytes
            .fetch_add(FILE_HEADER_LEN, Ordering::Relaxed);
        Ok(file)
    }
}

/// Parse `seg-000000000042.log` -> 42.
fn seq_of(name: &str, suffix: &str) -> Option<u64> {
    let rest = name.strip_prefix("seg-")?.strip_suffix(suffix)?;
    rest.parse().ok()
}

/// Simulate a kill mid-write: cut `bytes` off the tail of a segment.
#[cfg(test)]
fn truncate_to(dir: &Path, seq: u64, bytes: u64) {
    let path = dir.join(format!("seg-{seq:012}.log"));
    if let Ok(file) = OpenOptions::new().write(true).open(path) {
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = file.set_len(len.saturating_sub(bytes));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Frame, ProfileType, ReportBuilder};

    fn report(name: &str, value: u64) -> Report {
        let mut builder = ReportBuilder::new(name, ProfileType::cpu(), 1_000, 2_000);
        builder.add_stack([Frame::new("/bin/app", "main")], value);
        builder.finish()
    }

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("le-wal-{tag}-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn an_append_round_trips_every_report() {
        let root = dir("roundtrip");
        let log = SegmentLog::open(root.join("wal"), WalOptions::default()).unwrap();
        let reports: Vec<Report> = (0..5).map(|i| report("api", 1_000 + i)).collect();
        let appended = log.append(&reports).unwrap();
        assert_eq!(appended.reports, 5);
        assert!(appended.bytes > 0);
        // The active segment is not handed out until it is sealed.
        assert!(log.closed_segments().is_empty());
        log.seal();
        let seqs = log.closed_segments();
        assert_eq!(seqs.len(), 1);
        assert_eq!(log.read_segment(seqs[0]).unwrap().unwrap().len(), 5);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_closed_segment_reads_back_the_same_reports() {
        let root = dir("closed");
        let log = SegmentLog::open(
            root.join("wal"),
            WalOptions {
                segment_bytes: 1,
                ..Default::default()
            },
        )
        .unwrap();
        log.append(&[report("api", 7)]).unwrap();
        // Second append crosses the threshold and seals the first.
        log.append(&[report("api", 8)]).unwrap();
        let seqs = log.closed_segments();
        assert_eq!(seqs.len(), 1, "exactly the first segment is closed");
        let reports = log.read_segment(seqs[0]).unwrap().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].service_name, "api");
        log.mark_applied(seqs[0]);
        assert!(log.closed_segments().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_torn_record_is_dropped_not_fatal() {
        let root = dir("torn");
        let log = SegmentLog::open(
            root.join("wal"),
            WalOptions {
                segment_bytes: 1,
                ..Default::default()
            },
        )
        .unwrap();
        log.append(&[report("api", 1), report("api", 2)]).unwrap();
        log.append(&[report("api", 3)]).unwrap();
        let seq = log.closed_segments()[0];
        // Cut the file in the middle of the last record, as a kill would.
        truncate_to(log.dir(), seq, 5);
        let reports = log.read_segment(seq).unwrap().unwrap();
        assert_eq!(reports.len(), 1, "only the intact record survives");
        assert_eq!(reports[0].service_name, "api");
        // Reading again is stable: the tail was truncated, not failed on.
        assert_eq!(log.read_segment(seq).unwrap().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Backpressure is a byte budget: once the converter is behind by more
    /// than the cap, an append is refused instead of growing without bound.
    #[test]
    fn an_over_full_log_refuses_new_appends() {
        let root = dir("full");
        let log = SegmentLog::open(
            root.join("wal"),
            WalOptions {
                max_pending_bytes: 256,
                ..Default::default()
            },
        )
        .unwrap();
        // The first small append fits.
        log.append(&[report("api", 1)]).unwrap();
        // A wide report pushes past the cap.
        let mut builder = ReportBuilder::new("api", ProfileType::cpu(), 1_000, 2_000);
        for i in 0..200 {
            builder.add_stack(
                [Frame::new("/bin/app", format!("frame-{i}"))],
                1_000 + i as u64,
            );
        }
        let wide = builder.finish();
        let err = log.append(&[wide]).expect_err("the cap must refuse this");
        assert!(
            matches!(err, Error::Busy(_)),
            "expected a retryable busy error, got {err}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pending_bytes_are_accounted_for() {
        let root = dir("pending");
        let log = SegmentLog::open(
            root.join("wal"),
            WalOptions {
                segment_bytes: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let appended = log.append(&[report("api", 1)]).unwrap();
        assert_eq!(log.pending_bytes(), appended.bytes + FILE_HEADER_LEN);
        let _ = std::fs::remove_dir_all(root);
    }
}
