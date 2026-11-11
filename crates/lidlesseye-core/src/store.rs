//! On-disk storage: a directory of immutable segments plus an in-memory
//! index of their headers.
//!
//! Layout:
//!
//! ```text
//! <root>/cpu/checkout/1712345678000000000-<uuid>.leseg
//! <root>/cpu/checkout/1712345688000000000-<uuid>.leseg
//! <root>/off_cpu/checkout/...
//! ```
//!
//! Keeping the profile type and the service in the path means the common
//! dashboard query ("`cpu` of `checkout` in the last hour") can be answered
//! from the directory listing alone; the remaining label matchers are checked
//! against the 128 byte segment header before a single sample is touched.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::{Error, Result};
use crate::model::Report;
use crate::segment::{self, SegmentMeta};

/// Tunables for the storage engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreConfig {
    /// Directory holding the segments.
    pub root: PathBuf,
    /// Delete segments older than this. `None` keeps everything.
    pub retention_ns: Option<u64>,
    /// Soft cap for the on-disk size. Oldest segments are evicted first.
    pub max_disk_bytes: Option<u64>,
}

impl StoreConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        StoreConfig {
            root: root.into(),
            retention_ns: None,
            max_disk_bytes: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct StoreStats {
    pub segment_count: usize,
    pub total_bytes: u64,
    pub total_value: u64,
    pub earliest_ns: Option<u64>,
    pub latest_ns: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PruneStats {
    pub removed_segments: usize,
    pub removed_bytes: u64,
}

/// Aggregated information about one series (service + label set + type),
/// computed from segment headers only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeriesInfo {
    pub service_name: String,
    pub profile_type: String,
    pub labels: BTreeMap<String, String>,
    pub total_value: u64,
    pub segment_count: usize,
    pub start_ns: u64,
    pub end_ns: u64,
}

#[derive(Default)]
struct Index {
    segments: Vec<Arc<SegmentMeta>>,
    bytes: u64,
}

/// Thread-safe segment store.
pub struct Store {
    cfg: StoreConfig,
    index: RwLock<Index>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let index = self.index.read();
        f.debug_struct("Store")
            .field("root", &self.cfg.root)
            .field("segments", &index.segments.len())
            .field("bytes", &index.bytes)
            .finish()
    }
}

impl Store {
    /// Open (and index) an existing data directory.
    pub fn open(cfg: StoreConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.root)?;
        let store = Store {
            cfg,
            index: RwLock::new(Index::default()),
        };
        store.reload()?;
        Ok(store)
    }

    pub fn config(&self) -> &StoreConfig {
        &self.cfg
    }

    pub fn root(&self) -> &Path {
        &self.cfg.root
    }

    /// Rebuild the in-memory index from disk.
    pub fn reload(&self) -> Result<usize> {
        let mut metas = Vec::new();
        let mut bytes = 0u64;
        let mut visited = 0usize;
        for type_dir in read_dir(&self.cfg.root)? {
            if !type_dir.is_dir() {
                continue;
            }
            for service_dir in read_dir(&type_dir)? {
                if !service_dir.is_dir() {
                    continue;
                }
                for entry in read_dir(&service_dir)? {
                    let name = entry
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if !name.ends_with(".leseg") {
                        continue;
                    }
                    visited += 1;
                    match segment::read_segment_meta(&entry) {
                        Ok(meta) => {
                            bytes += meta.size_bytes;
                            metas.push(Arc::new(meta));
                        }
                        Err(err) => warn!(path = %entry.display(), error = %err, "skipping unreadable segment"),
                    }
                }
            }
        }
        metas.sort_unstable_by_key(|m| (m.start_ns, m.path.clone()));
        let count = metas.len();
        *self.index.write() = Index {
            segments: metas,
            bytes,
        };
        debug!(segments = count, scanned = visited, "store index rebuilt");
        Ok(count)
    }

    /// Snapshot of the index, newest segment last.
    pub fn segments(&self) -> Vec<Arc<SegmentMeta>> {
        self.index.read().segments.clone()
    }

    /// Persist a report as a new segment and index it.
    ///
    /// This performs blocking file I/O; async callers should use
    /// `spawn_blocking`.
    pub fn append(&self, report: &Report) -> Result<Arc<SegmentMeta>> {
        let dir = self
            .cfg
            .root
            .join(report.profile_type.as_str())
            .join(segment::service_slug(&report.service_name));
        let meta = Arc::new(segment::write_segment(&dir, report)?);

        let mut index = self.index.write();
        let pos = index
            .segments
            .partition_point(|m| (m.start_ns, &m.path) <= (meta.start_ns, &meta.path));
        index.segments.insert(pos, meta.clone());
        index.bytes += meta.size_bytes;
        Ok(meta)
    }

    /// Segments of `profile_type` overlapping `[from_ns, until_ns)` that
    /// satisfy the selector.
    pub fn select(
        &self,
        profile_type: &crate::model::ProfileType,
        from_ns: u64,
        until_ns: u64,
        selector: &crate::selector::Selector,
    ) -> Vec<Arc<SegmentMeta>> {
        let index = self.index.read();
        index
            .segments
            .iter()
            .filter(|meta| {
                profile_type.source_types().iter().any(|t| t == &meta.profile_type)
                    && meta.overlaps(from_ns, until_ns)
                    && selector.matches_meta(&meta.service_name, &meta.labels)
            })
            .cloned()
            .collect()
    }

    pub fn stats(&self) -> StoreStats {
        let index = self.index.read();
        StoreStats {
            segment_count: index.segments.len(),
            total_bytes: index.bytes,
            total_value: index.segments.iter().map(|m| m.total_value).sum(),
            earliest_ns: index.segments.first().map(|m| m.start_ns),
            latest_ns: index.segments.last().map(|m| m.end_ns),
        }
    }

    /// Aggregate known series, optionally restricted to a time window.
    pub fn series(&self, from_ns: u64, until_ns: u64, selector: &crate::selector::Selector) -> Vec<SeriesInfo> {
        let index = self.index.read();
        let mut map: HashMap<(String, String, BTreeMap<String, String>), SeriesInfo> = HashMap::new();
        for meta in &index.segments {
            if !meta.overlaps(from_ns, until_ns) {
                continue;
            }
            if !selector.matches_meta(&meta.service_name, &meta.labels) {
                continue;
            }
            let key = (
                meta.service_name.clone(),
                meta.profile_type.as_str().to_string(),
                meta.labels.clone(),
            );
            let entry = map.entry(key).or_insert_with(|| SeriesInfo {
                service_name: meta.service_name.clone(),
                profile_type: meta.profile_type.as_str().to_string(),
                labels: meta.labels.clone(),
                total_value: 0,
                segment_count: 0,
                start_ns: u64::MAX,
                end_ns: 0,
            });
            entry.total_value += meta.total_value;
            entry.segment_count += 1;
            entry.start_ns = entry.start_ns.min(meta.start_ns);
            entry.end_ns = entry.end_ns.max(meta.end_ns);
        }
        let mut out: Vec<SeriesInfo> = map.into_values().collect();
        out.sort_unstable_by(|a, b| {
            b.total_value
                .cmp(&a.total_value)
                .then_with(|| a.service_name.cmp(&b.service_name))
        });
        out
    }

    /// Distinct values of a label with their aggregated totals.
    pub fn label_values(
        &self,
        label: &str,
        from_ns: u64,
        until_ns: u64,
        selector: &crate::selector::Selector,
    ) -> Vec<(String, u64)> {
        let index = self.index.read();
        let mut map: HashMap<String, u64> = HashMap::new();
        for meta in &index.segments {
            if !meta.overlaps(from_ns, until_ns) {
                continue;
            }
            if !selector.matches_meta(&meta.service_name, &meta.labels) {
                continue;
            }
            if let Some(value) = meta.label(label) {
                *map.entry(value.to_string()).or_default() += meta.total_value;
            }
        }
        let mut out: Vec<(String, u64)> = map.into_iter().collect();
        out.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// Drop segments older than the retention window and, if configured,
    /// evict the oldest segments until the store fits in `max_disk_bytes`.
    pub fn prune(&self, now_ns: u64) -> Result<PruneStats> {
        let mut stats = PruneStats::default();
        let mut victims: Vec<PathBuf> = Vec::new();

        {
            let index = self.index.read();
            if let Some(retention) = self.cfg.retention_ns {
                let cutoff = now_ns.saturating_sub(retention);
                for meta in &index.segments {
                    if meta.end_ns < cutoff {
                        victims.push(meta.path.clone());
                    }
                }
            }
            if let Some(limit) = self.cfg.max_disk_bytes {
                let mut bytes = index.bytes
                    - index
                        .segments
                        .iter()
                        .filter(|m| victims.contains(&m.path))
                        .map(|m| m.size_bytes)
                        .sum::<u64>();
                for meta in &index.segments {
                    if bytes <= limit {
                        break;
                    }
                    if victims.contains(&meta.path) {
                        continue;
                    }
                    victims.push(meta.path.clone());
                    bytes = bytes.saturating_sub(meta.size_bytes);
                }
            }
        }

        if victims.is_empty() {
            return Ok(stats);
        }

        for path in &victims {
            match std::fs::remove_file(path) {
                Ok(()) => {
                    stats.removed_segments += 1;
                    if let Ok(md) = std::fs::metadata(path) {
                        stats.removed_bytes += md.len();
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => warn!(path = %path.display(), error = %err, "failed to remove segment"),
            }
        }

        let victim_set: std::collections::HashSet<&PathBuf> = victims.iter().collect();
        let mut index = self.index.write();
        index.segments.retain(|m| !victim_set.contains(&m.path));
        index.bytes = index.segments.iter().map(|m| m.size_bytes).sum();
        Ok(stats)
    }

    /// Delete every segment (used by tests and `fsck --fix`).
    pub fn clear(&self) -> Result<()> {
        let paths: Vec<PathBuf> = self.index.read().segments.iter().map(|m| m.path.clone()).collect();
        for path in paths {
            let _ = std::fs::remove_file(path);
        }
        self.reload()?;
        Ok(())
    }

    /// Verify every segment's checksum.
    pub fn fsck(&self) -> Vec<(PathBuf, Result<()>)> {
        self.segments()
            .into_iter()
            .map(|meta| {
                let result = segment::Segment::open(&meta.path).and_then(|s| s.verify());
                (meta.path.clone(), result)
            })
            .collect()
    }
}

fn read_dir(path: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => {
            return Err(Error::Io(err));
        }
    };
    for entry in entries {
        let entry = entry?;
        out.push(entry.path());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Frame, ProfileType, ReportBuilder};
    use crate::selector::Selector;

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "lidlesseye-store-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        let store = Store::open(StoreConfig::new(&dir)).unwrap();
        (store, dir)
    }

    fn report(service: &str, start: u64, pod: &str) -> Report {
        let mut b = ReportBuilder::new(service, "cpu", start, start + 10_000_000_000)
            .label("pod", pod);
        b.add_stack(
            [Frame::new("/bin/app", "hot"), Frame::new("/bin/app", "main")],
            1_000,
        );
        b.finish()
    }

    #[test]
    fn append_and_select() {
        let (store, dir) = temp_store("append");
        store.append(&report("checkout", 1_000, "a")).unwrap();
        store.append(&report("checkout", 2_000, "b")).unwrap();
        store.append(&report("cart", 3_000, "c")).unwrap();

        let all = store.select(&ProfileType::cpu(), 0, 10_000, &Selector::default());
        assert_eq!(all.len(), 3);

        let only_checkout = store.select(
            &ProfileType::cpu(),
            0,
            10_000,
            &Selector::parse(r#"{service_name="checkout"}"#).unwrap(),
        );
        assert_eq!(only_checkout.len(), 2);

        let narrowed = store.select(
            &ProfileType::cpu(),
            0,
            10_000,
            &Selector::parse(r#"{service_name="checkout",pod="b"}"#).unwrap(),
        );
        assert_eq!(narrowed.len(), 1);

        // Reopening rebuilds the index from disk.
        let store2 = Store::open(StoreConfig::new(&dir)).unwrap();
        assert_eq!(store2.stats().segment_count, 3);
        assert_eq!(store2.stats().total_value, 3_000);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retention_prunes_old_segments() {
        let hour = 3_600_000_000_000u64;
        let (store, dir) = temp_store("retention");
        let mut cfg = store.config().clone();
        cfg.retention_ns = Some(hour + hour / 2);
        let store = Store::open(cfg).unwrap();

        store.append(&report("api", 1_000, "a")).unwrap();
        store.append(&report("api", hour, "b")).unwrap();
        let stats = store.prune(2 * hour).unwrap();
        assert_eq!(stats.removed_segments, 1);
        assert_eq!(store.stats().segment_count, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn series_and_label_values() {
        let (store, dir) = temp_store("series");
        store.append(&report("api", 1_000, "a")).unwrap();
        store.append(&report("api", 2_000, "a")).unwrap();
        store.append(&report("web", 3_000, "b")).unwrap();

        let series = store.series(0, 10_000, &Selector::default());
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].service_name, "api");
        assert_eq!(series[0].segment_count, 2);

        let pods = store.label_values("pod", 0, 10_000, &Selector::default());
        assert_eq!(pods.len(), 2);
        assert_eq!(pods[0].0, "a");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
