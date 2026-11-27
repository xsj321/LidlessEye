//! Per-process memory, as the process itself reports it.
//!
//! The pod's cgroup answers "how much memory does this pod use" and nothing
//! else. That number cannot say which of the six processes inside is the one
//! growing, so the agent walks `/proc` and sends the top of the list. Reading
//! `smaps_rollup` is not free — it walks page tables — so only the largest few
//! processes get that treatment and the rest keep their `/proc/<pid>/status`
//! RSS, which is one line read.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One process in one pod, as of the last report.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProcessRow {
    pub pid: u32,
    /// Start time in nanoseconds since the epoch, so a restarted process with a
    /// recycled pid is a different row.
    #[serde(default)]
    pub start_ns: u64,
    #[serde(default)]
    pub comm: String,
    #[serde(default)]
    pub executable: String,
    #[serde(default)]
    pub rss_bytes: u64,
    /// Resident minus shared; filled from `smaps_rollup` for the processes the
    /// agent chose to look at closely.
    #[serde(default)]
    pub pss_bytes: Option<u64>,
    #[serde(default)]
    pub private_dirty_bytes: Option<u64>,
    #[serde(default)]
    pub swap_bytes: Option<u64>,
    /// Why some numbers are missing. A zero would be a lie; this is the truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
}

/// Everything one pod reported in one tick.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProcessReport {
    pub workload: String,
    pub pod: String,
    #[serde(default)]
    pub at_ns: u64,
    #[serde(default)]
    pub processes: Vec<ProcessRow>,
    /// Processes seen but not listed because the report was truncated to the
    /// top N. A count, not a guess: the reader knows how many it skipped.
    #[serde(default)]
    pub omitted: u64,
}

impl ProcessReport {
    pub fn total_rss(&self) -> u64 {
        self.processes.iter().map(|process| process.rss_bytes).sum()
    }
}

/// Latest report per (workload, pod).
#[derive(Default)]
pub struct Processes {
    latest: parking_lot::Mutex<BTreeMap<(String, String), ProcessReport>>,
    /// Shared-volume mirror, for the same reason `capabilities` has one: the
    /// pod that read `/proc` is whichever replica the Service picked, and a
    /// per-replica map would show half the pods on each.
    path: parking_lot::Mutex<Option<std::path::PathBuf>>,
}

impl Processes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_path(path: impl Into<std::path::PathBuf>) -> Self {
        let this = Self::default();
        *this.path.lock() = Some(path.into());
        this.reload();
        this
    }

    fn reload(&self) {
        let path = self.path.lock().clone();
        let Some(path) = path else { return };
        let Ok(text) = std::fs::read(&path) else { return };
        let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&text) else {
            return;
        };
        if doc.get("version").and_then(|v| v.as_u64()) != Some(1) {
            return;
        }
        let Some(rows) = doc.get("processes").cloned() else {
            return;
        };
        let Ok(rows) = serde_json::from_value::<Vec<ProcessReport>>(rows) else {
            return;
        };
        let mut latest = self.latest.lock();
        for row in rows {
            if row.workload.is_empty() {
                continue;
            }
            let key = (row.workload.clone(), row.pod.clone());
            let newer = latest
                .get(&key)
                .map(|current| row.at_ns > current.at_ns)
                .unwrap_or(true);
            if newer {
                latest.insert(key, row);
            }
        }
    }

    fn save(
        &self,
        path: Option<&std::path::Path>,
        latest: &BTreeMap<(String, String), ProcessReport>,
    ) {
        let Some(path) = path else { return };
        let rows: Vec<&ProcessReport> = latest.values().collect();
        let body = serde_json::json!({ "version": 1, "processes": rows });
        let Ok(text) = serde_json::to_vec(&body) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_err() {
            return;
        }
        let _ = std::fs::rename(&tmp, path);
    }

    pub fn record(&self, report: ProcessReport) -> bool {
        if report.workload.is_empty() {
            return false;
        }
        self.reload();
        let path = self.path.lock().clone();
        let mut latest = self.latest.lock();
        let key = (report.workload.clone(), report.pod.clone());
        match latest.get(&key) {
            Some(current) if report.at_ns < current.at_ns => false,
            _ => {
                latest.insert(key, report);
                self.save(path.as_deref(), &latest);
                true
            }
        }
    }

    /// The top processes of one workload, merged across its pods.
    ///
    /// `limit` is applied after merging: taking the top of each pod and then
    /// the top of that would drop the second-largest process of every pod.
    pub fn top(&self, workload: &str, limit: usize) -> (Vec<ProcessRow>, u64, u64) {
        self.reload();
        let latest = self.latest.lock();
        let mut merged: Vec<ProcessRow> = Vec::new();
        let mut omitted = 0;
        let mut pods = 0;
        for ((name, _), report) in latest.iter() {
            if name != workload {
                continue;
            }
            pods += 1;
            omitted += report.omitted;
            merged.extend(report.processes.iter().cloned());
        }
        merged.sort_by(|a, b| b.rss_bytes.cmp(&a.rss_bytes));
        merged.truncate(limit.max(1));
        (merged, omitted, pods)
    }

    /// Which workloads have reported at least once, with their pod counts.
    pub fn workloads(&self) -> Vec<(String, u64, u64)> {
        let latest = self.latest.lock();
        let mut counts: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for ((name, _), report) in latest.iter() {
            let entry = counts.entry(name.clone()).or_insert((0, 0));
            entry.0 += 1;
            entry.1 = entry.1.max(report.at_ns);
        }
        counts
            .into_iter()
            .map(|(name, (pods, at_ns))| (name, pods, at_ns))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, rss: u64) -> ProcessRow {
        ProcessRow {
            pid,
            rss_bytes: rss,
            ..Default::default()
        }
    }

    fn report(pod: &str, at_ns: u64, rows: Vec<ProcessRow>) -> ProcessReport {
        ProcessReport {
            workload: "disk/app".into(),
            pod: pod.into(),
            at_ns,
            processes: rows,
            omitted: 0,
        }
    }

    #[test]
    fn merges_pods_before_truncating() {
        let processes = Processes::new();
        processes.record(report("a", 10, vec![row(1, 900), row(2, 100)]));
        processes.record(report("b", 10, vec![row(3, 800), row(4, 200)]));
        let (top, _, pods) = processes.top("disk/app", 2);
        assert_eq!(pods, 2);
        assert_eq!(top.len(), 2);
        // The two largest overall, not the two largest of pod "a".
        assert_eq!(top[0].pid, 1);
        assert_eq!(top[1].pid, 3);
    }

    #[test]
    fn a_stale_report_does_not_overwrite_a_newer_one() {
        let processes = Processes::new();
        assert!(processes.record(report("a", 20, vec![row(1, 10)])));
        assert!(!processes.record(report("a", 10, vec![row(2, 20)])));
        let (top, _, _) = processes.top("disk/app", 10);
        assert_eq!(top[0].pid, 1);
    }
}
