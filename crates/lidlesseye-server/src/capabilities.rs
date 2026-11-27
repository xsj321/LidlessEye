//! What each workload can actually profile, and why not.
//!
//! `supported` is a property of the code on the machine; `active` is a
//! property of the last minute. Collapsing them into one boolean is how a
//! dashboard ends up claiming a reader is on while it delivers nothing, so
//! this module keeps the state and the reason apart and lets the UI show
//! both.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The reader a report is about. One workload can have several.
pub const READER_CPU: &str = "cpu";
/// How long a pod's last capability report keeps counting.
///
/// Readers report every 15–30 s, so ten minutes is several missed rounds: the
/// pod is gone, and its old row must not keep inflating "2 of 2 pods".
pub const STALE_AFTER_NS: u64 = 10 * 60 * 1_000_000_000;
pub const READER_GO_HEAP: &str = "go_heap";
pub const READER_JVM_HEAP: &str = "jvm_heap";
pub const READER_NATIVE_ALLOC: &str = "native_alloc";

/// Where a reader is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// The code on this machine could do it, but nobody asked.
    Supported,
    /// Conditions are met and the reader is being set up.
    Attaching,
    /// Samples are arriving.
    Active,
    /// Running but incomplete.
    Degraded,
    /// The machine cannot do it at all.
    Unsupported,
    /// The user turned it off.
    Disabled,
}

/// Why a reader is not active. Kept as a stable string so the UI can map it
/// to a sentence without the server knowing every reason.
pub const REASON_UNSUPPORTED_LIBC: &str = "unsupported_libc";
pub const REASON_STATIC_BINARY: &str = "static_binary";
pub const REASON_MISSING_RUNTIME_TOOL: &str = "missing_runtime_tool";
pub const REASON_MISSING_LIBELF: &str = "missing_libelf";
pub const REASON_PERMISSION_DENIED: &str = "permission_denied";
pub const REASON_NO_TARGET_PROCESS: &str = "no_target_process";
pub const REASON_ATTACH_FAILED: &str = "attach_failed";
pub const REASON_NO_SAMPLES: &str = "no_samples";
pub const REASON_RUNTIME_DEAD: &str = "runtime_dead";

/// One workload's view of one reader.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Capability {
    pub workload: String,
    /// The pod this report came from. Coverage is counted in pods before it is
    /// counted in processes: three pods of six processes is a different
    /// situation from one pod of six.
    #[serde(default)]
    pub pod: String,
    pub reader: String,
    pub state: Option<State>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// How many processes this workload had when the agent last looked.
    #[serde(default)]
    pub detected: u64,
    /// How many of them the reader is attached to.
    #[serde(default)]
    pub attached: u64,
    /// How many pods of this workload reported, and how many of them have a
    /// reader that is actually delivering. Coverage is a pod ratio before it
    /// is a process ratio.
    #[serde(default)]
    pub pods_reported: u64,
    #[serde(default)]
    pub pods_active: u64,
    /// When the agent last reported. The UI hides stale rows instead of
    /// painting them green.
    #[serde(default)]
    pub at_ns: u64,
}

/// Latest report per (workload, reader). Older reports are dropped so a
/// reader that died cannot keep claiming `active`.
#[derive(Default)]
pub struct Capabilities {
    latest: parking_lot::Mutex<BTreeMap<(String, String, String), Capability>>,
    /// The file on the shared volume this replica mirrors `latest` to.
    ///
    /// Reports arrive at whichever replica the Service happened to pick, so a
    /// purely in-memory map means each replica sees half the pods and the two
    /// disagree about what is running. Mirroring to the data directory the
    /// replicas already share makes the picture whole without a second service
    /// to keep in sync.
    path: parking_lot::Mutex<Option<std::path::PathBuf>>,
}

impl Capabilities {
    pub fn new() -> Self {
        Self::default()
    }

    /// Share the reports through `path`, reusing whatever is already there.
    pub fn with_path(path: impl Into<std::path::PathBuf>) -> Self {
        let this = Self::default();
        *this.path.lock() = Some(path.into());
        this.reload();
        this
    }

    fn reload(&self) {
        // The path lock is taken and released before `latest` is locked, and the
        // saving side does the same, so the two locks never invert.
        let path = self.path.lock().clone();
        let Some(path) = path else { return };
        let Ok(text) = std::fs::read(&path) else { return };
        let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&text) else {
            return;
        };
        if doc.get("version").and_then(|v| v.as_u64()) != Some(1) {
            return;
        }
        let Some(rows) = doc.get("capabilities").cloned() else {
            return;
        };
        let Ok(rows) = serde_json::from_value::<Vec<Capability>>(rows) else {
            return;
        };
        let mut latest = self.latest.lock();
        for row in rows {
            if row.workload.is_empty() || row.reader.is_empty() {
                continue;
            }
            let key = (row.workload.clone(), row.reader.clone(), row.pod.clone());
            let newer = latest
                .get(&key)
                .map(|current| row.at_ns > current.at_ns)
                .unwrap_or(true);
            if newer {
                latest.insert(key, row);
            }
        }
    }

    fn save(&self, path: Option<&std::path::Path>, latest: &BTreeMap<(String, String, String), Capability>) {
        let Some(path) = path else { return };
        let rows: Vec<&Capability> = latest.values().collect();
        let body = serde_json::json!({ "version": 1, "capabilities": rows });
        let Ok(text) = serde_json::to_vec(&body) else {
            return;
        };
        // Write beside the target and rename, so a reader never sees half a
        // document and a crash never leaves a truncated one in place.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_err() {
            return;
        }
        let _ = std::fs::rename(&tmp, path);
    }

    /// Store a batch of reports, ignoring any that are older than what we
    /// already hold for that pod and reader.
    pub fn record(&self, reports: Vec<Capability>) -> usize {
        self.reload();
        let path = self.path.lock().clone();
        let mut latest = self.latest.lock();
        let mut stored = 0;
        for report in reports {
            if report.workload.is_empty() || report.reader.is_empty() {
                continue;
            }
            let key = (
                report.workload.clone(),
                report.reader.clone(),
                report.pod.clone(),
            );
            let fresher = latest
                .get(&key)
                .map(|current| report.at_ns >= current.at_ns)
                .unwrap_or(true);
            if fresher {
                latest.insert(key, report);
                stored += 1;
            }
        }
        if stored > 0 {
            self.save(path.as_deref(), &latest);
        }
        stored
    }

    /// Every known capability, folded to one row per workload and reader.
    ///
    /// Per-pod reports are counted, not listed: what an operator needs to know
    /// is "two of three pods are instrumented", and one row per pod would push
    /// that arithmetic into the reader's head. The pod count travels with the
    /// row so the UI can write `2/3 Pods · 3/6 Processes`.
    pub fn list(&self, workload: Option<&str>) -> Vec<Capability> {
        // Pick up whatever the other replica recorded since the last read.
        self.reload();
        let latest = self.latest.lock();
        // A pod that is gone stops reporting. Counting its last report forever
        // would keep saying "2 of 2 pods" after one was deleted, so a report
        // older than the grace window no longer counts.
        let now = lidlesseye_core::now_ns();
        let horizon = STALE_AFTER_NS;
        let mut folded: BTreeMap<(String, String), Capability> = BTreeMap::new();
        for capability in latest.values() {
            if capability.at_ns != 0 && now.saturating_sub(capability.at_ns) > horizon {
                continue;
            }
            if let Some(want) = workload {
                if capability.workload != want {
                    continue;
                }
            }
            let key = (capability.workload.clone(), capability.reader.clone());
            let entry = folded.entry(key).or_insert_with(|| Capability {
                workload: capability.workload.clone(),
                reader: capability.reader.clone(),
                state: Some(State::Disabled),
                ..Default::default()
            });
            entry.detected += capability.detected;
            entry.attached += capability.attached;
            entry.at_ns = entry.at_ns.max(capability.at_ns);
            entry.pods_reported += 1;
            let is_active = capability.state == Some(State::Active);
            if is_active {
                entry.pods_active += 1;
            }
            // A single active pod outranks an unsupported sibling: the reader
            // works for this workload, the sibling just cannot be reached.
            let rank = |state: Option<State>| match state {
                Some(State::Active) => 5,
                Some(State::Degraded) => 4,
                Some(State::Attaching) => 3,
                Some(State::Supported) => 2,
                Some(State::Unsupported) => 1,
                Some(State::Disabled) => 0,
                None => 0,
            };
            if rank(capability.state) > rank(entry.state) {
                entry.state = capability.state;
                entry.reason = capability.reason.clone();
            }
        }
        folded.into_values().collect()
    }
}

/// The state to report for a reader the agent just tried to use.
///
/// `attached` is the number of processes actually instrumented, `detected`
/// the number it found to instrument. A reader with zero of both has a
/// reason, and the reason is what the user needs to read.
pub fn from_attach(
    workload: &str,
    pod: &str,
    detected: u64,
    attached: u64,
    skipped: &BTreeMap<String, u64>,
    at_ns: u64,
) -> Capability {
    let (state, reason) = if detected == 0 {
        (State::Unsupported, Some(REASON_NO_TARGET_PROCESS.to_string()))
    } else if attached > 0 {
        (State::Active, None)
    } else if skipped.contains_key("musl") {
        (State::Unsupported, Some(REASON_UNSUPPORTED_LIBC.to_string()))
    } else {
        (State::Attaching, Some(REASON_ATTACH_FAILED.to_string()))
    };
    Capability {
        workload: workload.to_string(),
        pod: pod.to_string(),
        reader: READER_NATIVE_ALLOC.to_string(),
        state: Some(state),
        reason,
        detected,
        attached,
        pods_reported: 1,
        pods_active: u64::from(state == State::Active),
        at_ns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_report_replaces_an_older_one() {
        let capabilities = Capabilities::new();
        let mut first = Capability {
            workload: "disk/app".into(),
            reader: READER_NATIVE_ALLOC.into(),
            state: Some(State::Attaching),
            at_ns: 10,
            ..Default::default()
        };
        capabilities.record(vec![first.clone()]);
        first.state = Some(State::Active);
        first.at_ns = 20;
        capabilities.record(vec![first.clone()]);
        let listed = capabilities.list(None);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, Some(State::Active));
    }

    #[test]
    fn a_stale_report_is_ignored() {
        let capabilities = Capabilities::new();
        capabilities.record(vec![Capability {
            workload: "disk/app".into(),
            reader: READER_NATIVE_ALLOC.into(),
            state: Some(State::Active),
            at_ns: 30,
            ..Default::default()
        }]);
        capabilities.record(vec![Capability {
            workload: "disk/app".into(),
            reader: READER_NATIVE_ALLOC.into(),
            state: Some(State::Degraded),
            at_ns: 20,
            ..Default::default()
        }]);
        assert_eq!(capabilities.list(None)[0].state, Some(State::Active));
    }

    #[test]
    fn a_musl_target_is_unsupported_not_failed() {
        let mut skipped = BTreeMap::new();
        skipped.insert("musl".to_string(), 2);
        let capability = from_attach("disk/app", "app-1", 2, 0, &skipped, 1);
        assert_eq!(capability.state, Some(State::Unsupported));
        assert_eq!(capability.reason.as_deref(), Some(REASON_UNSUPPORTED_LIBC));
    }

    #[test]
    fn no_processes_is_reported_as_such() {
        let capability = from_attach("disk/app", "app-1", 0, 0, &BTreeMap::new(), 1);
        assert_eq!(capability.state, Some(State::Unsupported));
        assert_eq!(capability.reason.as_deref(), Some(REASON_NO_TARGET_PROCESS));
    }
}
