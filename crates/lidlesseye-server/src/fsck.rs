//! Periodic check that the files on disk are still the files the server has.
//!
//! The storage layer trusts its index, and the index is rebuilt from whatever
//! the directory scan finds. That is the right behaviour after a crash, and it
//! is exactly the wrong behaviour when files disappear quietly: the rebuild
//! simply describes a smaller database, the queries answer from it, and nothing
//! anywhere says a week of profiles is gone. This module exists to say so.
//!
//! It judges only what it can prove cheaply: every Parquet file starts and ends
//! with `PAR1`, every hour directory has at least one part, and no file that was
//! here last time has vanished. Anything it cannot check it does not claim.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::Serialize;

/// What one run found for one tenant.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TenantCheck {
    pub tenant: String,
    /// Hour directories that contain at least one file.
    pub hours: usize,
    /// Parquet files the scan found.
    pub files: usize,
    /// Total bytes of those files.
    pub bytes: u64,
    /// Tables with an hour directory that holds no file at all.
    pub empty_hours: Vec<String>,
    /// Files whose header or footer is not `PAR1`.
    pub unreadable: Vec<String>,
    /// Files that were present on the previous run and are not present now.
    pub vanished: Vec<String>,
}

/// The result of one complete pass.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FsckReport {
    pub at_ns: u64,
    pub elapsed_ms: u64,
    pub tenants: Vec<TenantCheck>,
}

impl FsckReport {
    /// True when a human should be told about this run.
    pub fn has_findings(&self) -> bool {
        self.tenants.iter().any(|tenant| {
            !tenant.empty_hours.is_empty()
                || !tenant.unreadable.is_empty()
                || !tenant.vanished.is_empty()
        })
    }

    pub fn files(&self) -> usize {
        self.tenants.iter().map(|tenant| tenant.files).sum()
    }
}

/// The last report, and the file set to compare the next one against.
#[derive(Debug, Default)]
pub struct Fsck {
    last: Mutex<Option<FsckReport>>,
    seen: Mutex<BTreeSet<String>>,
}

impl Fsck {
    pub fn last(&self) -> Option<FsckReport> {
        self.last.lock().clone()
    }

    /// Scan the data directory and remember the result.
    ///
    /// `tenant_root` is one tenant's own directory (`<data>/tenants/<id>`), the
    /// only path the registry exposes; the common parent is derived from it so
    /// every tenant can be walked in one pass.
    pub fn run(&self, tenant_root: &Path, tenants: &[String]) -> FsckReport {
        let base = shared_root(tenant_root);
        let started = std::time::Instant::now();
        let mut report = FsckReport {
            at_ns: lidlesseye_core::now_ns(),
            ..Default::default()
        };
        let mut present = BTreeSet::new();
        for tenant in tenants {
            let check = check_tenant(&base, tenant, &mut present);
            report.tenants.push(check);
        }
        let previous = {
            let mut seen = self.seen.lock();
            std::mem::replace(&mut *seen, present.clone())
        };
        // A file that was here last time and is not here now is the one finding
        // retention cannot explain: retention goes through the index and keeps
        // the file set consistent with itself.
        let gone: BTreeSet<String> = previous.difference(&present).cloned().collect();
        for check in &mut report.tenants {
            let prefix = format!("tenants/{}/", check.tenant);
            check.vanished = gone
                .iter()
                .filter(|path| path.starts_with(&prefix))
                .cloned()
                .collect();
        }
        report.elapsed_ms = started.elapsed().as_millis() as u64;
        *self.last.lock() = Some(report.clone());
        report
    }
}

fn check_tenant(base: &Path, tenant: &str, present: &mut BTreeSet<String>) -> TenantCheck {
    let mut check = TenantCheck {
        tenant: tenant.to_string(),
        ..Default::default()
    };
    // The layout nests the tenant twice: `<data>/tenants/<id>/tenants/<id>/parquet`.
    // The outer directory is the registry's bookkeeping, the inner one is the
    // store's own tree, and only the inner one holds files.
    let root = base
        .join("tenants")
        .join(tenant)
        .join("tenants")
        .join(tenant)
        .join("parquet");
    let tables = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(_) => return check,
    };
    for table in tables.flatten() {
        let table_path = table.path();
        if !table_path.is_dir() {
            continue;
        }
        let hours = match std::fs::read_dir(&table_path) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for hour in hours.flatten() {
            let hour_path = hour.path();
            let name = hour_path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if !hour_path.is_dir() || !name.starts_with("hour=") {
                continue;
            }
            let mut in_hour = 0usize;
            let files = match std::fs::read_dir(&hour_path) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("parquet") {
                    continue;
                }
                in_hour += 1;
                check.files += 1;
                let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
                check.bytes += size;
                if !is_parquet(&path) {
                    check.unreadable.push(relative(&path, base));
                }
                present.insert(relative(&path, base));
            }
            if in_hour == 0 {
                check.empty_hours.push(format!("{}/{}", table_path.file_name().unwrap_or_default().to_string_lossy(), name));
            } else {
                check.hours += 1;
            }
        }
    }
    check
}

/// A Parquet file starts and ends with the four magic bytes.
///
/// This is not a full validation — the footer's metadata is not parsed — but it
/// catches the two things that actually happen: a truncated file after a crash
/// and a file that is not Parquet at all.
fn is_parquet(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut head = [0u8; 4];
    if file.read_exact(&mut head).is_err() || &head != b"PAR1" {
        return false;
    }
    let len = match file.metadata() {
        Ok(meta) => meta.len(),
        Err(_) => return false,
    };
    if len < 12 {
        return false;
    }
    if file.seek(SeekFrom::End(-4)).is_err() {
        return false;
    }
    let mut tail = [0u8; 4];
    file.read_exact(&mut tail).is_ok() && &tail == b"PAR1"
}

fn relative(path: &Path, data_dir: &Path) -> String {
    path.strip_prefix(data_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Group a report by tenant for the endpoint.
pub fn by_tenant(report: &FsckReport) -> BTreeMap<String, TenantCheck> {
    report
        .tenants
        .iter()
        .map(|check| (check.tenant.clone(), check.clone()))
        .collect()
}

/// The parquet tree under a data root, for tests.
pub fn tenant_paths(data_root: &Path, tenant: &str) -> PathBuf {
    data_root.join("tenants").join(tenant).join("tenants").join(tenant).join("parquet")
}

/// Turn the registry root into the directory that holds every tenant.
///
/// The registry root is `<data>/tenants` in the current layout and
/// `<data>/tenants/<id>` in older ones, and a check has to see every tenant to
/// compare runs, so the common parent is derived from whichever it finds.
fn shared_root(tenant_root: &Path) -> PathBuf {
    let named = |path: &Path, name: &str| {
        path.file_name().map(|found| found == name).unwrap_or(false)
    };
    if named(tenant_root, "tenants") {
        return tenant_root.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| tenant_root.to_path_buf());
    }
    if let Some(parent) = tenant_root.parent() {
        if named(parent, "tenants") {
            return parent.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| tenant_root.to_path_buf());
        }
    }
    tenant_root.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_parquet(dir: &Path, name: &str, body: &[u8]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn a_truncated_file_is_reported() {
        let root = std::env::temp_dir().join(format!("fsck-test-{}", lidlesseye_core::now_ns()));
        let hour = tenant_paths(&root, "default").join("samples").join("hour=7");
        write_parquet(&hour, "part-0.parquet", b"PAR1xxPAR1");
        write_parquet(&hour, "part-1.parquet", b"PAR1xx");
        let fsck = Fsck::default();
        let report = fsck.run(&root.join("tenants").join("default"), &["default".to_string()]);
        let check = &report.tenants[0];
        assert_eq!(check.files, 2);
        assert_eq!(check.hours, 1);
        assert_eq!(check.unreadable.len(), 1);
        assert!(report.has_findings());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_that_disappears_between_runs_is_reported() {
        let root = std::env::temp_dir().join(format!("fsck-gone-{}", lidlesseye_core::now_ns()));
        let hour = tenant_paths(&root, "default").join("samples").join("hour=8");
        write_parquet(&hour, "part-0.parquet", b"PAR1xxPAR1");
        let fsck = Fsck::default();
        let first = fsck.run(&root.join("tenants").join("default"), &["default".to_string()]);
        assert!(first.tenants[0].vanished.is_empty());
        std::fs::remove_file(hour.join("part-0.parquet")).unwrap();
        let second = fsck.run(&root.join("tenants").join("default"), &["default".to_string()]);
        assert_eq!(second.tenants[0].vanished.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
