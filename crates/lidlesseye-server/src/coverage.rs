//! Process-level coverage of the native allocator reader.
//!
//! The server cannot see inside a target pod, so it cannot know how many of a
//! workload's processes the reader actually reached. The shim that runs in the
//! pod does know, and posts it here; this module keeps the last report per
//! workload so the memory page can say "3 of 4 processes, 1 skipped (musl)"
//! instead of leaving the reader's reach to the imagination.
//!
//! A report is a snapshot, not a series: the reader re-attaches as pods restart,
//! and what an operator needs is the current state, not the history of attempts.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// One workload's most recent coverage report.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoverageReport {
    /// Dynamic processes the reader considered (a `libc.so` is mapped).
    pub detected: u64,
    /// Processes the agent is currently attached to.
    pub attached: u64,
    /// Processes it looked at and left alone, and why.
    #[serde(default)]
    pub skipped: BTreeMap<String, u64>,
    /// The pod's memory usage in bytes, as its cgroup reports it. This is the
    /// coarsest useful number for L0: it covers every container in the pod, not
    /// one process, but it is measured rather than estimated.
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    /// Processes in the pod that were summed for the two numbers below.
    #[serde(default)]
    pub process_count: Option<u64>,
    /// Resident set size summed over the pod's processes, in bytes.
    #[serde(default)]
    pub process_rss_bytes: Option<u64>,
    /// Proportional set size summed over the pod's processes, in bytes.
    ///
    /// PSS is the number to compare pods against each other: shared pages are
    /// divided between the processes that map them, so ten forks of one binary
    /// do not each claim the whole text segment.
    #[serde(default)]
    pub process_pss_bytes: Option<u64>,
    /// When the shim sent this, in nanoseconds since the epoch.
    #[serde(default)]
    pub at_ns: u64,
}

impl CoverageReport {
    /// Processes the reader does not cover: detected minus attached.
    pub fn uncovered(&self) -> u64 {
        self.detected.saturating_sub(self.attached)
    }
}

/// One point on the memory trend.
///
/// The three numbers move for different reasons, so they are kept separately:
/// RSS follows the working set, PSS follows it after shared pages are split, and
/// the cgroup also counts page cache, which can double while the process memory
/// does not move at all. A single "memory" line would hide exactly the case an
/// operator is hunting for.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MemoryPoint {
    pub at_ns: u64,
    pub rss_bytes: u64,
    pub pss_bytes: u64,
    pub cgroup_bytes: u64,
}

/// The last report for a workload, plus the trend behind it.
#[derive(Debug, Default, Clone)]
struct Entry {
    report: CoverageReport,
    points: VecDeque<MemoryPoint>,
}

/// Points kept per workload: one per reader flush (15 s), about two hours.
const MAX_POINTS: usize = 480;

/// The last report for every workload.
#[derive(Debug)]
pub struct Coverage {
    reports: Mutex<BTreeMap<String, Entry>>,
    /// Where the trend is kept between restarts, when the caller wants that.
    ///
    /// Without a path the module is pure memory, which is what the tests want
    /// and what a server without a data directory can do. With a path the whole
    /// map is rewritten after every report: 480 points per workload, a few
    /// hundred bytes each, a handful of workloads — a rewrite costs less than
    /// keeping a journal of appends, and it cannot end up half-applied.
    path: Option<PathBuf>,
    /// Whether this replica is the writer. Only the writer may rewrite the file:
    /// the replicas share one volume, and a standby saving its own (smaller) map
    /// would delete the writer's trend.
    persist: std::sync::atomic::AtomicBool,
}

impl Default for Coverage {
    fn default() -> Self {
        Self {
            reports: Mutex::new(BTreeMap::new()),
            path: None,
            persist: std::sync::atomic::AtomicBool::new(true),
        }
    }
}

impl Coverage {
    pub fn new() -> Self {
        Self::default()
    }

    /// A coverage map that survives a restart, loaded from `path` if it exists.
    pub fn with_path(path: PathBuf) -> Self {
        let reports = load(&path).unwrap_or_default();
        Self {
            reports: Mutex::new(reports),
            path: Some(path),
            persist: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Say whether this replica may rewrite the trend file. A standby keeps the
    /// trend in memory (and shows it) but never overwrites the writer's copy.
    pub fn set_persistent(&self, persistent: bool) {
        self.persist
            .store(persistent, std::sync::atomic::Ordering::Relaxed);
        // Taking over means the file is now ours to refresh, so start from what
        // is there instead of from whatever this replica happened to see.
        if persistent {
            self.reload();
        }
    }

    /// Pull the writer's newer entries in, so a standby answers with the same
    /// numbers as the writer instead of the snapshot it booted with.
    ///
    /// Entries are merged, not replaced: only the writer writes the file, so
    /// everything read here is either equal to or newer than what we hold.
    pub fn reload(&self) {
        let Some(path) = &self.path else { return };
        let Some(loaded) = load_quiet(path, false) else { return };
        let mut reports = self.reports.lock().expect("coverage reports poisoned");
        for (workload, incoming) in loaded {
            match reports.get_mut(&workload) {
                Some(current) if current.report.at_ns >= incoming.report.at_ns => {}
                _ => {
                    reports.insert(workload, incoming);
                }
            }
        }
    }

    /// Record a report. Older reports are ignored: the shim retries, and a
    /// retry that arrives late must not overwrite a fresher state.
    pub fn record(&self, workload: &str, mut report: CoverageReport) {
        if report.at_ns == 0 {
            report.at_ns = now_ns();
        }
        let mut reports = self.reports.lock().expect("coverage reports poisoned");
        let entry = reports.entry(workload.to_string()).or_default();
        if entry.report.at_ns != 0 && report.at_ns < entry.report.at_ns {
            return;
        }
        // Only keep a trend point when the reader actually measured something;
        // a report without process memory would draw a line to zero.
        if let (Some(rss), Some(pss)) = (report.process_rss_bytes, report.process_pss_bytes) {
            entry.points.push_back(MemoryPoint {
                at_ns: report.at_ns,
                rss_bytes: rss,
                pss_bytes: pss,
                cgroup_bytes: report.memory_bytes.unwrap_or(0),
            });
            while entry.points.len() > MAX_POINTS {
                entry.points.pop_front();
            }
        }
        entry.report = report;
        if let Some(path) = &self.path {
            if self.persist.load(std::sync::atomic::Ordering::Relaxed) {
                if let Err(error) = save(path, &reports) {
                    // A failed write must not stop the reader from reporting: the
                    // next report tries again, and the in-memory trend stays right.
                    warn!("cannot save the coverage trend to {}: {error}", path.display());
                }
            }
        }
    }

    /// The report for one workload, if the reader ever sent one.
    pub fn get(&self, workload: &str) -> Option<CoverageReport> {
        self.reports
            .lock()
            .expect("coverage reports poisoned")
            .get(workload)
            .map(|entry| entry.report.clone())
    }

    /// The trend for one workload, oldest first.
    pub fn history(&self, workload: &str) -> Vec<MemoryPoint> {
        self.reload_if_standby();
        self.reports
            .lock()
            .expect("coverage reports poisoned")
            .get(workload)
            .map(|entry| entry.points.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Every workload that has reported, oldest first by name.
    pub fn all(&self) -> Vec<(String, CoverageReport)> {
        self.reload_if_standby();
        self.reports
            .lock()
            .expect("coverage reports poisoned")
            .iter()
            .map(|(name, entry)| (name.clone(), entry.report.clone()))
            .collect()
    }

    /// A standby answers from the writer's file; the writer answers from its own
    /// memory, which is what it just wrote there.
    fn reload_if_standby(&self) {
        if !self.persist.load(std::sync::atomic::Ordering::Relaxed) {
            self.reload();
        }
    }
}

/// The body the shim posts.
#[derive(Debug, Deserialize)]
pub struct CoverageBody {
    /// `namespace/workload`, the same key the directives use.
    pub workload: String,
    pub detected: u64,
    pub attached: u64,
    #[serde(default)]
    pub skipped: BTreeMap<String, u64>,
    /// The pod's cgroup memory usage, when the shim could read it.
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    /// Processes summed for the RSS/PSS figures.
    #[serde(default)]
    pub process_count: Option<u64>,
    #[serde(default)]
    pub process_rss_bytes: Option<u64>,
    #[serde(default)]
    pub process_pss_bytes: Option<u64>,
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0)
}

/// What the trend file holds. A version field so a future format can be
/// recognised instead of silently misread.
#[derive(Serialize, Deserialize)]
struct Saved {
    version: u32,
    entries: Vec<SavedEntry>,
}

#[derive(Serialize, Deserialize)]
struct SavedEntry {
    workload: String,
    report: CoverageReport,
    points: Vec<MemoryPoint>,
}

fn load(path: &Path) -> Option<BTreeMap<String, Entry>> {
    load_quiet(path, true)
}

/// `announce` is false for the read-through a standby does before answering a
/// query: that happens on every request, and logging it each time would bury
/// the one line at boot that actually tells an operator the trend was restored.
fn load_quiet(path: &Path, announce: bool) -> Option<BTreeMap<String, Entry>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            warn!("cannot read the coverage trend from {}: {error}", path.display());
            return None;
        }
    };
    let saved: Saved = match serde_json::from_str(&text) {
        Ok(saved) => saved,
        Err(error) => {
            warn!("ignoring the unreadable coverage trend in {}: {error}", path.display());
            return None;
        }
    };
    if saved.version != 1 {
        warn!("ignoring coverage trend version {} in {}", saved.version, path.display());
        return None;
    }
    let mut reports = BTreeMap::new();
    for entry in saved.entries {
        reports.insert(
            entry.workload,
            Entry {
                report: entry.report,
                points: entry.points.into(),
            },
        );
    }
    if announce {
        info!(
            "restored the memory trend for {} workloads from {}",
            reports.len(),
            path.display()
        );
    }
    Some(reports)
}

fn save(path: &Path, reports: &BTreeMap<String, Entry>) -> std::io::Result<()> {
    let saved = Saved {
        version: 1,
        entries: reports
            .iter()
            .map(|(workload, entry)| SavedEntry {
                workload: workload.clone(),
                report: entry.report.clone(),
                points: entry.points.iter().copied().collect(),
            })
            .collect(),
    };
    let text = serde_json::to_string(&saved)
        .map_err(|error| std::io::Error::other(format!("cannot serialise the trend: {error}")))?;
    // Write beside the target and rename, so a crash mid-write leaves the old
    // file rather than a truncated one that would be ignored on the next boot.
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_report_does_not_replace_a_newer_one() {
        let coverage = Coverage::new();
        coverage.record(
            "disk/app",
            CoverageReport {
                detected: 4,
                attached: 3,
                skipped: [("musl".to_string(), 1)].into_iter().collect(),
                at_ns: 20,
            },
        );
        coverage.record(
            "disk/app",
            CoverageReport {
                detected: 1,
                attached: 0,
                skipped: Default::default(),
                at_ns: 10,
            },
        );
        let report = coverage.get("disk/app").expect("a report");
        assert_eq!(report.detected, 4);
        assert_eq!(report.uncovered(), 1);
    }
}
