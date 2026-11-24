//! Multi-tenancy.
//!
//! A tenant is a hard isolation boundary: it owns a database file, a retention
//! policy and a set of limits. Nothing in a query path can reach across
//! tenants, because a query only ever holds the [`Store`] of the tenant it was
//! resolved for.
//!
//! Layout:
//!
//! ```text
//! <data-dir>/tenants/<tenant>/lidlesseye.duckdb
//! ```
//!
//! One database per tenant. Data written before multi-tenancy existed lived
//! in that same file directly under `<data-dir>`; [`TenantStore::open`] moves
//! it under `tenants/default/` once, so an upgrade does not lose history.
//!
//! Tenant ids follow the same rule as Pyroscope's `X-Scope-OrgID`: a short,
//! URL-safe token. We are stricter than Pyroscope on purpose, because the id
//! becomes a path component — `../` would otherwise walk out of the data
//! directory.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::{Error, Result};
use crate::store::{PruneStats, S3Config, Store, StoreConfig, StoreStats};

/// Maximum accepted tenant id length.
pub const MAX_TENANT_LEN: usize = 64;

/// Serializes `Store::open` per tenant directory.
///
/// A `TenantStore` guards its own map with a write lock, but a process can hold
/// more than one registry over the same data directory (tests do, and a future
/// hot-reload path could too). Without this lock two registries can initialize
/// the same DuckDB file at once; the loser then sees a half-written header and
/// reports "not a valid DuckDB database file".
static OPEN_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn open_lock(dir: &Path) -> Arc<Mutex<()>> {
    let mut locks = OPEN_LOCKS.get_or_init(|| Mutex::new(HashMap::new())).lock();
    locks.entry(dir.to_path_buf()).or_default().clone()
}

/// Identifier of a tenant.
///
/// Ordered and hashable so it can key maps; the inner string is always valid
/// as a single path component.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TenantId(String);

impl TenantId {
    /// Tenant used when a request does not name one.
    pub const DEFAULT: &'static str = "default";

    /// Validate and normalize a tenant id.
    ///
    /// Ids are case-insensitive: they are lowercased before use. That is not
    /// cosmetic. The id becomes a directory name, and both NTFS (Windows) and
    /// APFS (macOS, default) are case-insensitive, so keeping `Acme` and `acme`
    /// distinct in memory would point both at the *same* directory on disk —
    /// two tenants silently sharing one store. Normalizing at the boundary
    /// makes the in-memory model agree with the filesystem on every platform.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(Error::InvalidTenant("tenant id is empty".into()));
        }
        if raw.len() > MAX_TENANT_LEN {
            return Err(Error::InvalidTenant(format!(
                "tenant id is longer than {MAX_TENANT_LEN} bytes"
            )));
        }
        if raw == "." || raw == ".." || raw.starts_with('.') {
            return Err(Error::InvalidTenant(format!(
                "`{raw}` is not a usable tenant id"
            )));
        }
        if !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(Error::InvalidTenant(format!(
                "tenant id `{raw}` may only contain ASCII letters, digits, `-`, `_` and `.`"
            )));
        }
        Ok(TenantId(raw.to_ascii_lowercase()))
    }

    pub fn default_tenant() -> Self {
        TenantId(Self::DEFAULT.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_default(&self) -> bool {
        self.0 == Self::DEFAULT
    }
}

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for TenantId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        TenantId::parse(s)
    }
}

/// Per-tenant storage policy.
///
/// `None` means "inherit the server default", so a deployment can set one
/// global retention and override it for a single noisy tenant.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantLimits {
    /// Delete batches older than this.
    pub retention_ns: Option<u64>,
    /// Soft cap for the tenant's retained data (not the database file size).
    pub max_disk_bytes: Option<u64>,
    /// Maximum number of distinct series. Ingestion of a new series beyond
    /// this limit is rejected; existing series keep working.
    pub max_series: Option<usize>,
}

impl TenantLimits {
    pub fn is_empty(&self) -> bool {
        self.retention_ns.is_none() && self.max_disk_bytes.is_none() && self.max_series.is_none()
    }
}

/// A tenant plus the numbers the API exposes about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantInfo {
    pub id: String,
    pub stats: StoreStats,
    pub series: usize,
    pub limits: TenantLimits,
}

/// Registry of per-tenant stores.
#[derive(Debug)]
pub struct TenantStore {
    root: PathBuf,
    base: StoreConfig,
    limits: RwLock<BTreeMap<TenantId, TenantLimits>>,
    stores: RwLock<HashMap<TenantId, Arc<Store>>>,
    max_tenants: usize,
}

impl TenantStore {
    /// Open the tenant registry rooted at `data_dir`.
    ///
    /// The `base` config supplies the directory-independent defaults
    /// (retention, disk cap); its `root` field is ignored.
    pub fn open(data_dir: impl Into<PathBuf>, base: StoreConfig) -> Result<Self> {
        Self::open_with_limits(data_dir, base, BTreeMap::new(), 1000)
    }

    pub fn open_with_limits(
        data_dir: impl Into<PathBuf>,
        base: StoreConfig,
        limits: BTreeMap<TenantId, TenantLimits>,
        max_tenants: usize,
    ) -> Result<Self> {
        let data_dir = data_dir.into();
        std::fs::create_dir_all(&data_dir)?;
        let root = data_dir.join("tenants");
        migrate_legacy_layout(&data_dir, &root)?;
        std::fs::create_dir_all(&root)?;
        let registry = TenantStore {
            root,
            base,
            limits: RwLock::new(limits),
            stores: RwLock::new(HashMap::new()),
            max_tenants: max_tenants.max(1),
        };
        // Index what is already on disk so the first request is not slow and
        // so a corrupt directory is reported at startup rather than at query
        // time. Each store logs its own phases; these lines give the overall
        // progress so an upgrade on a large volume is not a silent wait.
        let tenants = registry.tenant_ids();
        let started = std::time::Instant::now();
        info!(
            count = tenants.len(),
            stage = "tenants",
            "opening tenant stores"
        );
        for (index, id) in tenants.iter().enumerate() {
            info!(
                tenant = %id,
                index = index + 1,
                total = tenants.len(),
                stage = "tenant_open",
                "opening tenant store"
            );
            if let Err(err) = registry.open_tenant(id) {
                warn!(tenant = %id, error = %err, "failed to open tenant");
            }
        }
        info!(
            elapsed_ms = started.elapsed().as_millis(),
            stage = "tenants",
            "all tenant stores opened"
        );
        Ok(registry)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory of one tenant (created on demand by [`Self::open_tenant`]).
    pub fn tenant_dir(&self, tenant: &TenantId) -> PathBuf {
        self.root.join(tenant.as_str())
    }

    /// Limits configured for a tenant, falling back to the global defaults.
    pub fn limits(&self, tenant: &TenantId) -> TenantLimits {
        let overrides = self.limits.read().get(tenant).cloned().unwrap_or_default();
        TenantLimits {
            retention_ns: overrides.retention_ns.or(self.base.retention_ns),
            max_disk_bytes: overrides.max_disk_bytes.or(self.base.max_disk_bytes),
            max_series: overrides.max_series,
        }
    }

    /// Override the limits of a tenant that may already be open.
    ///
    /// The change applies to pruning immediately; the per-tenant store keeps
    /// the disk cap it was opened with, so a shrinking cap takes effect on the
    /// next process start (documented in `docs/multi-tenancy.md`).
    pub fn set_limits(&self, tenant: TenantId, limits: TenantLimits) {
        self.limits.write().insert(tenant, limits);
    }

    /// Open (creating the directory if needed) the store of a tenant.
    pub fn open_tenant(&self, tenant: &TenantId) -> Result<Arc<Store>> {
        if let Some(store) = self.stores.read().get(tenant) {
            return Ok(store.clone());
        }
        let mut stores = self.stores.write();
        // Re-check under the write lock: another thread may have won the race
        // while we were waiting.
        if let Some(store) = stores.get(tenant) {
            return Ok(store.clone());
        }
        let existing = self.tenant_ids().len();
        if existing >= self.max_tenants && !self.tenant_dir(tenant).is_dir() {
            return Err(Error::LimitExceeded {
                tenant: tenant.to_string(),
                reason: format!("the server already hosts {existing} tenants"),
            });
        }
        let limits = self.limits(tenant);
        // The S3 prefix is namespaced per tenant so archives never mix. The
        // storage mode and connection details come from the process-wide base
        // config; only the on-disk root and retention are tenant-specific.
        let s3 = self.base.s3.as_ref().map(|base| S3Config {
            prefix: format!("{}/{}", base.prefix.trim_end_matches('/'), tenant.as_str()),
            ..base.clone()
        });
        let cfg = StoreConfig {
            root: self.tenant_dir(tenant),
            retention_ns: limits.retention_ns,
            max_disk_bytes: limits.max_disk_bytes,
            // A process-wide budget, not a per-tenant one: DuckDB's thread
            // pool is per connection, and letting one tenant size its own
            // would let a noisy tenant starve the others.
            threads: self.base.threads,
            duckdb_memory_limit: self.base.duckdb_memory_limit.clone(),
            storage_mode: self.base.storage_mode,
            s3,
            parquet_local_ttl_ns: self.base.parquet_local_ttl_ns,
            flush_interval_ns: self.base.flush_interval_ns,
            // Per-tenant read connections, sized process-wide: the pool is
            // what lets one slow panel stop blocking the others.
            read_connections: self.base.read_connections,
        };
        // Another registry over the same directory may be opening this tenant
        // right now. DuckDB writes its file header during open, so the two
        // initializations must not overlap — even though each registry has its
        // own in-memory lock. Canonicalizing makes `a/tenants/x` and
        // `a/../a/tenants/x` share one lock.
        let dir = cfg.root.clone();
        std::fs::create_dir_all(&dir)?;
        let canonical = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        let mutex = open_lock(&canonical);
        let _open_guard = mutex.lock();
        let opened = std::time::Instant::now();
        info!(tenant = %tenant, dir = %dir.display(), "tenant store open started");
        let store = Arc::new(Store::open(cfg)?);
        stores.insert(tenant.clone(), store.clone());
        info!(
            tenant = %tenant,
            elapsed_ms = opened.elapsed().as_millis(),
            "tenant store open finished"
        );
        Ok(store)
    }

    /// The store of a tenant, if it exists. Never creates one: a read of an
    /// unknown tenant is a 404, not a new empty directory.
    pub fn tenant(&self, tenant: &TenantId) -> Option<Arc<Store>> {
        if let Some(store) = self.stores.read().get(tenant) {
            return Some(store.clone());
        }
        if !self.tenant_dir(tenant).is_dir() {
            return None;
        }
        self.open_tenant(tenant).ok()
    }

    /// Tenant ids present on disk, sorted.
    pub fn tenant_ids(&self) -> Vec<TenantId> {
        let mut ids = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(_) => return ids,
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            match TenantId::parse(&name) {
                Ok(id) => ids.push(id),
                Err(err) => warn!(dir = %name, error = %err, "ignoring unusable tenant directory"),
            }
        }
        ids.sort();
        ids
    }

    /// Stats for every tenant on disk.
    ///
    /// Each entry comes from the tenant's stats cache, which is computed on
    /// demand when cold and then reused for the TTL. Computing it here for
    /// every tenant on every poll is what turned the dashboard's first request
    /// into a fifteen-second wait; the server also warms the cache in the
    /// background at start-up so that first poll usually finds it full.
    pub fn tenants(&self) -> Vec<TenantInfo> {
        self.tenant_ids()
            .into_iter()
            .map(|id| {
                let (stats, series) = self
                    .tenant(&id)
                    .map(|store| store.stats_cached())
                    .unwrap_or_default();
                TenantInfo {
                    id: id.to_string(),
                    stats,
                    series,
                    limits: self.limits(&id),
                }
            })
            .collect()
    }

    /// Aggregate stats over all tenants, from the per-tenant cached numbers.
    ///
    /// The only caller is `/metrics`, which Prometheus scrapes every few
    /// seconds. Summing `stats()` per tenant there meant a full scan of every
    /// tenant on every scrape — a self-inflicted load that competed with the
    /// dashboard for the same connections. A scrape may be up to one cache
    /// window stale, which is irrelevant for a gauge.
    pub fn total_stats(&self) -> StoreStats {
        let mut total = StoreStats::default();
        for id in self.tenant_ids() {
            if let Some(store) = self.tenant(&id) {
                let (stats, _) = store.stats_cached();
                total.batch_count += stats.batch_count;
                total.total_bytes += stats.total_bytes;
                total.logical_bytes += stats.logical_bytes;
                total.total_value += stats.total_value;
                total.earliest_ns = match (total.earliest_ns, stats.earliest_ns) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                total.latest_ns = match (total.latest_ns, stats.latest_ns) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
            }
        }
        total
    }

    /// Run one Parquet compaction pass across all tenants: flush each hot tier
    /// to local Parquet, merge completed hours, then archive files past the
    /// local TTL to S3. Returns `(tenant, rows_flushed, files_uploaded,
    /// files_merged)`. No-op when not in Parquet mode.
    pub fn compaction_tick(&self, now_ns: u64) -> Vec<(TenantId, usize, usize, usize)> {
        let mut out = Vec::new();
        if !self.base.storage_mode.uses_parquet() {
            return out;
        }
        for id in self.tenant_ids() {
            let Some(store) = self.tenant(&id) else {
                continue;
            };
            let flushed = match store.flush_hot_when_idle() {
                Ok(n) => n,
                Err(err) => {
                    warn!(tenant = %id, error = %err, "parquet hot flush failed");
                    continue;
                }
            };
            let uploaded = match store.archive_local(now_ns) {
                Ok(stats) => stats.files_uploaded,
                Err(err) => {
                    warn!(tenant = %id, error = %err, "parquet S3 archive failed");
                    0
                }
            };
            // Merge each finished hour into one file per table. Without this a
            // five-minute ingest cadence leaves twelve files an hour, and every
            // query pays to open all of their footers.
            let merged = match store.compact_local() {
                Ok(stats) => stats.files_merged,
                Err(err) => {
                    warn!(tenant = %id, error = %err, "parquet local compaction failed");
                    0
                }
            };
            if flushed > 0 || uploaded > 0 || merged > 0 {
                out.push((id, flushed, uploaded, merged));
            }
        }
        out
    }

    /// Prune every tenant; returns what was removed, per tenant.
    pub fn prune(&self, now_ns: u64) -> Result<BTreeMap<TenantId, PruneStats>> {
        let mut out = BTreeMap::new();
        for id in self.tenant_ids() {
            let Some(store) = self.tenant(&id) else {
                continue;
            };
            match store.prune(now_ns) {
                Ok(stats) if stats.removed_batches > 0 || stats.removed_files > 0 => {
                    out.insert(id, stats);
                }
                Ok(_) => {}
                Err(err) => warn!(tenant = %id, error = %err, "prune failed"),
            }
        }
        Ok(out)
    }

    /// Verify every tenant database.
    pub fn fsck(&self) -> Vec<(TenantId, PathBuf, Result<()>)> {
        let mut out = Vec::new();
        for id in self.tenant_ids() {
            let Some(store) = self.tenant(&id) else {
                continue;
            };
            for (path, result) in store.fsck() {
                out.push((id.clone(), path, result));
            }
        }
        out
    }
}

/// Move a pre-multi-tenancy data directory under `tenants/default/`.
///
/// The heuristic is deliberately conservative: only directories whose
/// children are all directories are moved, which is exactly the shape of
/// `<type>/<service>/`, and never anything named `tenants`.
fn migrate_legacy_layout(data_dir: &Path, tenants_dir: &Path) -> Result<()> {
    if tenants_dir.exists() {
        return Ok(());
    }
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(Error::Io(err)),
    };

    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "tenants" {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir {
            // A pre-multi-tenancy DuckDB store is a single file next to its
            // write-ahead log; both belong to the default tenant.
            if name.ends_with(".duckdb") || name.ends_with(".duckdb.wal") {
                candidates.push(path);
            }
            continue;
        }
        let children = match std::fs::read_dir(&path) {
            Ok(children) => children,
            Err(_) => continue,
        };
        let mut all_dirs = true;
        let mut count = 0usize;
        for child in children.flatten() {
            count += 1;
            if !child.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                all_dirs = false;
                break;
            }
        }
        if count > 0 && all_dirs {
            candidates.push(path);
        }
    }

    if candidates.is_empty() {
        return Ok(());
    }

    let target = tenants_dir.join(TenantId::DEFAULT);
    std::fs::create_dir_all(&target)?;
    for path in candidates {
        let Some(name) = path.file_name() else {
            continue;
        };
        let destination = target.join(name);
        match std::fs::rename(&path, &destination) {
            Ok(()) => {
                debug!(from = %path.display(), to = %destination.display(), "migrated legacy data")
            }
            Err(err) => warn!(
                from = %path.display(),
                error = %err,
                "could not migrate legacy data; leaving it in place"
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Frame, ProfileType, ReportBuilder};
    use crate::selector::Selector;
    use crate::store::DB_FILE;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("lidlesseye-tenant-{name}-{}", uuid::Uuid::new_v4()))
    }

    fn report(service: &str, start: u64) -> crate::model::Report {
        let mut b = ReportBuilder::new(service, "cpu", start, start + 1_000);
        b.add_stack([Frame::new("/bin/app", "main")], 1_000);
        b.finish()
    }

    #[test]
    fn tenant_ids_are_validated() {
        assert!(TenantId::parse("acme").is_ok());
        assert!(TenantId::parse("acme-prod_1.2").is_ok());
        assert!(TenantId::parse("default").unwrap().is_default());

        for bad in ["", "..", ".", ".hidden", "a/b", "a b", "a\\b", "é"] {
            assert!(TenantId::parse(bad).is_err(), "`{bad}` should be rejected");
        }
        assert!(TenantId::parse(&"a".repeat(MAX_TENANT_LEN + 1)).is_err());
    }

    /// Ids differing only by case must land in the same tenant, because that
    /// is what the filesystem does. Keeping them separate in memory would mean
    /// two tenants sharing one directory on Windows and macOS.
    #[test]
    fn tenant_ids_are_case_insensitive() {
        let lower = TenantId::parse("acme").unwrap();
        for spelling in ["Acme", "ACME", "aCmE", "  Acme  "] {
            assert_eq!(
                TenantId::parse(spelling).unwrap(),
                lower,
                "`{spelling}` did not normalize to `acme`"
            );
        }

        let dir = temp_dir("case");
        let registry = TenantStore::open(&dir, StoreConfig::new(&dir)).unwrap();
        let upper = TenantId::parse("Acme").unwrap();
        registry.open_tenant(&upper).unwrap();
        assert_eq!(
            registry.tenant_dir(&upper),
            registry.tenant_dir(&lower),
            "one tenant must have exactly one directory"
        );
        let names: Vec<String> = std::fs::read_dir(registry.root())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["acme".to_string()]);
    }

    #[test]
    fn tenants_are_isolated() {
        let dir = temp_dir("isolation");
        let registry = TenantStore::open(&dir, StoreConfig::new(&dir)).unwrap();
        let acme = TenantId::parse("acme").unwrap();
        let beta = TenantId::parse("beta").unwrap();

        registry
            .open_tenant(&acme)
            .unwrap()
            .append(&report("api", 1_000))
            .unwrap();
        registry
            .open_tenant(&beta)
            .unwrap()
            .append(&report("api", 1_000))
            .unwrap();
        registry
            .open_tenant(&beta)
            .unwrap()
            .append(&report("api", 2_000))
            .unwrap();

        assert_eq!(registry.tenant(&acme).unwrap().stats().batch_count, 1);
        assert_eq!(registry.tenant(&beta).unwrap().stats().batch_count, 2);

        let fg = crate::flamegraph(
            &registry.tenant(&acme).unwrap(),
            &crate::QueryOptions::new("cpu", 0, 10_000, Selector::default()),
        )
        .unwrap();
        assert_eq!(fg.total, 1_000, "queries never cross tenants");

        // A tenant directory holds nothing but its own data.
        assert!(!registry.tenant_dir(&acme).join("tenants").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unknown_tenant_is_absent_until_written() {
        let dir = temp_dir("absent");
        let registry = TenantStore::open(&dir, StoreConfig::new(&dir)).unwrap();
        let ghost = TenantId::parse("ghost").unwrap();
        assert!(registry.tenant(&ghost).is_none());
        assert!(
            !registry.tenant_dir(&ghost).exists(),
            "reads must not create dirs"
        );
        registry.open_tenant(&ghost).unwrap();
        assert!(registry.tenant(&ghost).is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_layout_is_migrated_once() {
        let dir = temp_dir("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        // A store written before multi-tenancy: one DuckDB file in the data
        // dir, with no per-tenant directory and no Parquet files.
        let conn = duckdb::Connection::open(dir.join(DB_FILE)).unwrap();
        conn.execute_batch(
            "CREATE TABLE batches (
                batch_id BIGINT PRIMARY KEY, profile_type VARCHAR NOT NULL,
                service_name VARCHAR NOT NULL, labels VARCHAR NOT NULL,
                start_ns BIGINT NOT NULL, end_ns BIGINT NOT NULL,
                total_value BIGINT NOT NULL, sample_count BIGINT NOT NULL,
                bytes BIGINT NOT NULL, created_ns BIGINT NOT NULL);
             INSERT INTO batches VALUES
                (1, 'cpu', 'api', '', 1000, 2000, 10, 1, 128, 1500);",
        )
        .unwrap();
        drop(conn);

        let registry = TenantStore::open(&dir, StoreConfig::new(&dir)).unwrap();
        let default = TenantId::default_tenant();
        let store = registry.tenant(&default).unwrap();
        assert_eq!(store.stats().batch_count, 1);
        assert_eq!(
            store
                .select(&ProfileType::cpu(), 0, 10_000, &Selector::default())
                .unwrap()
                .len(),
            1
        );
        assert!(!dir.join(DB_FILE).exists(), "the old root file is gone");
        let tenant_dir = registry.tenant_dir(&default);
        assert!(!tenant_dir.join(DB_FILE).exists(), "no DuckDB file on disk");
        assert!(
            std::fs::read_dir(tenant_dir.join("parquet").join("batches"))
                .unwrap()
                .any(|e| e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".parquet")),
            "tenants must keep their data as Parquet"
        );

        // Re-opening is idempotent.
        drop(registry);
        let registry = TenantStore::open(&dir, StoreConfig::new(&dir)).unwrap();
        assert_eq!(registry.tenant(&default).unwrap().stats().batch_count, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn series_limit_only_blocks_new_series() {
        let dir = temp_dir("series-limit");
        let mut limits = BTreeMap::new();
        let acme = TenantId::parse("acme").unwrap();
        limits.insert(
            acme.clone(),
            TenantLimits {
                max_series: Some(1),
                ..Default::default()
            },
        );
        let registry =
            TenantStore::open_with_limits(&dir, StoreConfig::new(&dir), limits, 4).unwrap();
        assert_eq!(registry.limits(&acme).max_series, Some(1));

        let store = registry.open_tenant(&acme).unwrap();
        store.append(&report("api", 1_000)).unwrap();
        assert!(store.contains_series(&report("api", 2_000)));
        assert!(!store.contains_series(&report("web", 2_000)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The server's process-wide thread budget must survive the tenant
    /// registry: `open_tenant` used to hardcode `None` and silently drop it.
    #[test]
    fn thread_budget_reaches_every_tenant() {
        let dir = temp_dir("threads");
        let base = StoreConfig {
            threads: Some(2),
            ..StoreConfig::new(&dir)
        };
        let registry = TenantStore::open(&dir, base).unwrap();
        let acme = TenantId::parse("acme").unwrap();
        let store = registry.open_tenant(&acme).unwrap();
        assert_eq!(store.config().threads, Some(2));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tenant_count_is_capped() {
        let dir = temp_dir("cap");
        let registry =
            TenantStore::open_with_limits(&dir, StoreConfig::new(&dir), BTreeMap::new(), 1)
                .unwrap();
        registry
            .open_tenant(&TenantId::parse("one").unwrap())
            .unwrap();
        let err = registry
            .open_tenant(&TenantId::parse("two").unwrap())
            .unwrap_err();
        assert!(matches!(err, Error::LimitExceeded { .. }), "{err:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
