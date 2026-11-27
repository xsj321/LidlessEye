//! Which workloads the operator has switched memory profiling on for.
//!
//! Reading a JVM (or any other runtime that only exposes itself on request)
//! costs the target a pause and the agent a dump per interval. Injection costs
//! more: the bpftime agent replaces the allocator entry point inside a running
//! process. Neither is something to run everywhere, so the choice lives here: a
//! small map of `namespace/workload` to *kind*, edited through the API and
//! polled by every agent.
//!
//! Kinds:
//!
//! * `jvm`   — read the JVM's own Flight Recorder. Nothing is injected.
//! * `alloc` — inject the bpftime agent and hook `malloc` in userspace.
//!
//! A workload appears at most once; switching one kind on replaces the other.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// The two runtimes a switch can name.
pub const KIND_JVM: &str = "jvm";
pub const KIND_ALLOC: &str = "alloc";

/// The file's contents. A version number because the shape may grow.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    version: u32,
    /// `namespace/workload` -> kind. Version 1 stored a plain list, which is
    /// read as "all jvm" by [`Directives::load`].
    #[serde(default)]
    workloads: serde_json::Value,
}

const VERSION: u32 = 2;

/// The switchboard, kept in memory and mirrored to one file.
pub struct Directives {
    path: PathBuf,
    workloads: RwLock<BTreeMap<String, String>>,
}

impl Directives {
    /// Load whatever the last server wrote. A missing or unreadable file is an
    /// empty set, not an error: refusing to start because a preference file is
    /// corrupt would trade a nuisance for an outage.
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("profiling-directives.json");
        let workloads = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Stored>(&bytes).ok())
            .filter(|stored| stored.version == 1 || stored.version == VERSION)
            .map(|stored| match stored.workloads {
                // Version 1: a list of names, all JVM.
                serde_json::Value::Array(items) => items
                    .into_iter()
                    .filter_map(|item| item.as_str().map(|s| (s.to_string(), KIND_JVM.to_string())))
                    .collect(),
                // Version 2: name -> kind.
                serde_json::Value::Object(map) => map
                    .into_iter()
                    .filter_map(|(name, kind)| {
                        kind.as_str().map(|kind| (name, kind.to_string()))
                    })
                    .collect(),
                _ => BTreeMap::new(),
            })
            .unwrap_or_default();
        Directives {
            path,
            workloads: RwLock::new(workloads),
        }
    }

    /// Every switched-on workload with its kind.
    pub fn list(&self) -> Vec<(String, String)> {
        self.workloads
            .read()
            .iter()
            .map(|(name, kind)| (name.clone(), kind.clone()))
            .collect()
    }

    /// The workloads switched on for one kind.
    pub fn list_kind(&self, kind: &str) -> Vec<String> {
        self.workloads
            .read()
            .iter()
            .filter(|(_, value)| value.as_str() == kind)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Whether this workload is switched on for `kind`.
    pub fn enabled(&self, workload: &str, kind: &str) -> bool {
        self.workloads
            .read()
            .get(workload)
            .is_some_and(|value| value == kind)
    }

    /// Switch one workload on (for `kind`) or off, returning the resulting map.
    ///
    /// The file is rewritten in full each time: the set is tiny, and a
    /// write-then-rename keeps a crash from leaving half a JSON document that
    /// the next start would silently discard.
    pub fn set(
        &self,
        workload: &str,
        kind: &str,
        enabled: bool,
    ) -> std::io::Result<Vec<(String, String)>> {
        let snapshot = {
            let mut guard = self.workloads.write();
            if enabled {
                guard.insert(workload.to_string(), kind.to_string());
            } else {
                guard.remove(workload);
            }
            guard
                .iter()
                .map(|(name, kind)| (name.clone(), kind.clone()))
                .collect::<Vec<_>>()
        };
        let mut map = serde_json::Map::new();
        for (name, kind) in &snapshot {
            map.insert(name.clone(), serde_json::Value::String(kind.clone()));
        }
        let stored = Stored {
            version: VERSION,
            workloads: serde_json::Value::Object(map),
        };
        let bytes = serde_json::to_vec_pretty(&stored)?;
        let tmp = self.path.with_extension("json.tmp");
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        Ok(snapshot)
    }
}

/// What the API returns and the agent parses.
#[derive(Debug, Serialize)]
pub struct Payload<'a> {
    /// JVM workloads, kept as a flat list for the first version of the agent.
    pub workloads: Vec<String>,
    /// Native-memory workloads (bpftime injection).
    pub alloc_workloads: Vec<String>,
    /// Every entry with its kind.
    pub directives: Vec<Entry<'a>>,
}

#[derive(Debug, Serialize)]
pub struct Entry<'a> {
    pub workload: &'a str,
    pub kind: &'a str,
}
