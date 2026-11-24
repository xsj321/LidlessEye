//! Parquet + S3 storage backend (DuckDB 2.0 "zero-copy" file scanning).
//!
//! This module does not reimplement a query engine. DuckDB already reads
//! Parquet directly, pushing filters and projections into the scan and using
//! per-row-group zonemaps/page indexes to skip data — including objects in S3,
//! streamed on demand rather than copied into the database. Our job is only to
//! lay the files out well and keep the catalogue of files current.
//!
//! # Layout
//!
//! For each tenant (`root` is already per-tenant) the hot tier lives in a
//! small in-memory DuckDB. Every write goes to a `<table>_hot` table. The
//! canonical table names (`samples`, `batches`, ...) are VIEWs defined as:
//!
//! ```text
//! SELECT <cols> FROM <table>_hot
//! UNION ALL SELECT <cols> FROM read_parquet([<explicit local + S3 file list>], hive_partitioning=false)
//! ```
//!
//! The explicit file list comes from a small local `manifest.json` (S3 objects)
//! plus a `read_dir` of the local staging dir. Explicit paths avoid a bucket
//! LIST at query bind time — binding stays fast and works even when S3 is
//! temporarily unreachable. On open we *do* run one best-effort `glob()` of
//! each S3 prefix to rebuild the manifest, so a lost local manifest (PVC
//! rebuild, new node) never hides already-archived data.
//!
//! So every existing read query sees the whole dataset — hot rows, local
//! Parquet and archived S3 Parquet — without any change to its SQL.
//!
//! # Lifecycle
//!
//! 1. `flush_hot` copies hot rows to a small local Parquet file (partitioned by
//!    hour in the file name) and empties the hot table.
//! 2. Once a local file is older than the local TTL (default one hour),
//!    `archive_local` merges the hour's files into one large Parquet object,
//!    uploads it to S3 with `COPY ... TO 's3://...'`, verifies the object exists,
//!    then removes the local copies. Queries inside the local window read
//!    local files; older windows read S3.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use duckdb::Connection;
use tracing::warn;

use crate::error::{Error, Result};
use crate::store::S3Config;

/// One physical table and the projection used when exposing it through VIEWs.
pub(crate) struct TableDef {
    pub name: &'static str,
    /// Explicit column list — required so an empty `read_parquet` glob still
    /// produces a relation with a stable schema for the UNION.
    pub columns: &'static str,
    /// CREATE TABLE body for the hot tier (`{t}` is the table name).
    pub ddl: &'static str,
}

pub(crate) const TABLES: &[TableDef] = &[
    TableDef {
        name: "batches",
        columns: "batch_id, profile_type, service_name, labels, start_ns, end_ns, total_value, sample_count, bytes, created_ns",
        ddl: "CREATE TABLE {t} (batch_id BIGINT PRIMARY KEY, profile_type VARCHAR NOT NULL, service_name VARCHAR NOT NULL, labels VARCHAR NOT NULL, start_ns BIGINT NOT NULL, end_ns BIGINT NOT NULL, total_value BIGINT NOT NULL, sample_count BIGINT NOT NULL, bytes BIGINT NOT NULL, created_ns BIGINT NOT NULL)",
    },
    TableDef {
        name: "samples",
        columns: "batch_id, profile_type, service_name, labels, start_ns, end_ns, stack_id, value",
        ddl: "CREATE TABLE {t} (batch_id BIGINT NOT NULL, profile_type VARCHAR NOT NULL, service_name VARCHAR NOT NULL, labels VARCHAR NOT NULL, start_ns BIGINT NOT NULL, end_ns BIGINT NOT NULL, stack_id BIGINT NOT NULL, value BIGINT NOT NULL)",
    },
    TableDef {
        name: "stacks",
        columns: "stack_id, frames",
        ddl: "CREATE TABLE {t} (stack_id BIGINT PRIMARY KEY, frames BLOB NOT NULL)",
    },
    TableDef {
        name: "frames",
        columns: "frame_id, module, symbol, file, line",
        ddl: "CREATE TABLE {t} (frame_id INTEGER PRIMARY KEY, module VARCHAR NOT NULL, symbol VARCHAR NOT NULL, file VARCHAR NOT NULL, line INTEGER NOT NULL)",
    },
    TableDef {
        name: "net_nodes",
        columns: "bucket_ns, node_id, namespace, workload, service, kind, k8s_node, rx_bytes, tx_bytes, connections, retransmits, pods, ready, instances, containers",
        ddl: "CREATE TABLE {t} (bucket_ns BIGINT NOT NULL, node_id VARCHAR NOT NULL, namespace VARCHAR NOT NULL, workload VARCHAR NOT NULL, service VARCHAR NOT NULL, kind VARCHAR NOT NULL, k8s_node VARCHAR NOT NULL, rx_bytes BIGINT NOT NULL, tx_bytes BIGINT NOT NULL, connections BIGINT NOT NULL, retransmits BIGINT NOT NULL, pods BIGINT DEFAULT 0 NOT NULL, ready BIGINT DEFAULT 0 NOT NULL, instances VARCHAR DEFAULT '[]' NOT NULL, containers VARCHAR DEFAULT '[]' NOT NULL)",
    },
    TableDef {
        name: "net_edges",
        columns: "bucket_ns, client_id, server_id, protocol, client_container, server_container, client_pod, server_pod, connections, retransmits, tx_bytes, rx_bytes, rtt_us_sum, rtt_samples, host, sql",
        ddl: "CREATE TABLE {t} (bucket_ns BIGINT NOT NULL, client_id VARCHAR NOT NULL, server_id VARCHAR NOT NULL, protocol VARCHAR NOT NULL, client_container VARCHAR DEFAULT '' NOT NULL, server_container VARCHAR DEFAULT '' NOT NULL, client_pod VARCHAR DEFAULT '' NOT NULL, server_pod VARCHAR DEFAULT '' NOT NULL, connections BIGINT NOT NULL, retransmits BIGINT NOT NULL, tx_bytes BIGINT NOT NULL, rx_bytes BIGINT NOT NULL, rtt_us_sum BIGINT NOT NULL, rtt_samples BIGINT NOT NULL, host VARCHAR DEFAULT '' NOT NULL, sql VARCHAR DEFAULT '' NOT NULL)",
    },
];

/// A manifest of the S3 objects that back a table. Keeping this locally means
/// a VIEW is built from an explicit list of object keys and never has to LIST
/// the bucket at bind time — which is both faster and makes an initially empty
/// (or temporarily unreachable) bucket a non-event. `serde` derives let us
/// persist it as JSON so the catalogue survives restarts.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Manifest {
    /// table name -> S3 URIs of the merged objects, oldest first.
    pub objects: std::collections::BTreeMap<String, Vec<String>>,
}

/// Per-store state that only the Parquet backend uses.
#[derive(Debug, Clone)]
pub(crate) struct ParquetState {
    /// `<root>/parquet` — local staged files, one subdir per table.
    pub dir: PathBuf,
    pub bucket: String,
    /// Full S3 key prefix including the tenant id, no trailing slash.
    pub prefix: String,
    pub local_ttl_ns: u64,
    /// Path of the on-disk manifest JSON.
    manifest_path: PathBuf,
    /// When the next S3 archive attempt is allowed, after a failure.
    ///
    /// An archive attempt is not cheap and it is not local: it re-reads and
    /// re-compresses a whole hour and holds the tenant's only DuckDB
    /// connection while it does. Retrying on the next tick — ten seconds
    /// later — turned a bucket that rejects multipart uploads into a permanent
    /// full-time scan: the pass never succeeded, so the local files were never
    /// reclaimed, so the next pass had even more to read. Worse, every query
    /// queued behind the connection it was holding, so the dashboard saw 408s
    /// while the server was busy failing at something else.
    ///
    /// The backoff is global rather than per hour because a failure is almost
    /// never about *one* hour: an unreachable endpoint, a bad credential and a
    /// refused multipart upload all fail every hour the same way.
    ///
    /// It doubles on every consecutive failure up to [`ARCHIVE_MAX_BACKOFF`],
    /// so a bucket that is broken for a day costs one failed merge every hour
    /// rather than twelve every minute, and a bucket that was merely rate
    /// limited is retried within minutes.
    archive_retry: Arc<Mutex<ArchiveRetry>>,
}

impl ParquetState {
    pub fn new(root: &Path, s3: &S3Config, local_ttl_ns: u64) -> Self {
        let dir = root.join("parquet");
        ParquetState {
            dir: dir.clone(),
            bucket: s3.bucket.clone(),
            prefix: s3.prefix.trim_matches('/').to_string(),
            local_ttl_ns,
            manifest_path: dir.join("manifest.json"),
            archive_retry: Arc::new(Mutex::new(ArchiveRetry::default())),
        }
    }

    pub fn table_dir(&self, table: &str) -> PathBuf {
        self.dir.join(table)
    }

    pub fn s3_base(&self) -> String {
        if self.prefix.is_empty() {
            format!("s3://{}", self.bucket)
        } else {
            format!("s3://{}/{}", self.bucket, self.prefix)
        }
    }

    pub fn load_manifest(&self) -> Manifest {
        std::fs::read(&self.manifest_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save_manifest(&self, manifest: &Manifest) -> Result<()> {
        let bytes = serde_json::to_vec(manifest).map_err(Error::Json)?;
        // Write atomically so a crash never leaves a half-written catalogue.
        let tmp = self.manifest_path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes).map_err(Error::Io)?;
        std::fs::rename(&tmp, &self.manifest_path).map_err(Error::Io)?;
        Ok(())
    }

    pub fn clear_manifest(&self) -> Result<()> {
        match std::fs::remove_file(&self.manifest_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }
}

/// Set up httpfs and S3 credentials on a connection. Idempotent.
pub(crate) fn configure_s3(conn: &Connection, s3: &S3Config) -> Result<()> {
    // LOAD first (fast path when the extension is already installed). Only if
    // that fails do we try INSTALL, which needs network on first use.
    if conn.execute_batch("LOAD httpfs").is_err() {
        conn.execute_batch("INSTALL httpfs")
            .map_err(|e| Error::Db(format!("INSTALL httpfs: {e}")))?;
        conn.execute_batch("LOAD httpfs")
            .map_err(|e| Error::Db(format!("LOAD httpfs: {e}")))?;
    }
    let set = |sql: String| -> Result<()> {
        conn.execute_batch(&sql)
            .map_err(|e| Error::Db(format!("S3 setting failed: {e}")))
    };
    // The bucket is part of every `s3://bucket/...` URI, not a global setting.
    if let Some(endpoint) = &s3.endpoint {
        set(format!("SET s3_endpoint='{}'", esc(endpoint)))?;
    }
    if let Some(region) = &s3.region {
        set(format!("SET s3_region='{}'", esc(region)))?;
    }
    if let Some(key) = &s3.access_key_id {
        set(format!("SET s3_access_key_id='{}'", esc(key)))?;
    }
    if let Some(secret) = &s3.secret_access_key {
        set(format!("SET s3_secret_access_key='{}'", esc(secret)))?;
    }
    let ssl = s3.use_ssl.unwrap_or(false);
    set(format!(
        "SET s3_use_ssl={}",
        if ssl { "true" } else { "false" }
    ))?;
    let style = s3.url_style.as_deref().unwrap_or("path");
    set(format!("SET s3_url_style='{}'", esc(style)))?;
    // Without a bound, one stalled object read holds the tenant's only
    // connection forever: the query never finishes, so it never releases the
    // mutex, so every request after it is refused. DuckDB's own default is
    // longer than any dashboard is willing to wait, and a request that errors
    // in fifteen seconds is recoverable while a wedged connection is not.
    // Best-effort: an older httpfs may not expose these.
    let _ = conn.execute_batch("SET http_timeout=15000");
    let _ = conn.execute_batch("SET http_retries=2");
    Ok(())
}

/// Create the hot tables and the canonical VIEWs for a fresh Parquet store.
pub(crate) fn create_schema(conn: &Connection, state: &ParquetState) -> Result<()> {
    std::fs::create_dir_all(&state.dir).map_err(Error::Io)?;
    for def in TABLES {
        let ddl = def.ddl.replace("{t}", &format!("{}_hot", def.name));
        conn.execute_batch(&ddl)
            .map_err(|e| Error::Db(format!("creating hot table {}: {e}", def.name)))?;
        std::fs::create_dir_all(state.table_dir(def.name)).map_err(Error::Io)?;
    }
    // Columns added after a release: `CREATE TABLE` above is a no-op on an
    // existing database, so the hot tier needs an explicit migration. Old
    // Parquet files simply lack the column and `union_by_name` reads NULL.
    // The canonical VIEWs depend on the hot tables, and DuckDB refuses an
    // ALTER while a dependent VIEW exists, so drop them first and rebuild
    // them below. On a fresh store this is a no-op.
    for def in TABLES {
        conn.execute_batch(&format!("DROP VIEW IF EXISTS {}", def.name))
            .map_err(|e| Error::Db(format!("dropping view {}: {e}", def.name)))?;
    }
    // DuckDB does not support `ADD COLUMN IF NOT EXISTS` with a NOT NULL
    // default, so ask the catalogue which columns are already there. This was
    // previously a swallowed error: a fresh store tried to re-add every column
    // and silently kept only the ones the DDL already provided.
    let mut existing = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_name = 'net_edges_hot'",
            )
            .map_err(|e| Error::Db(format!("reading net_edges_hot columns: {e}")))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| Error::Db(format!("reading net_edges_hot columns: {e}")))?;
        for row in rows {
            existing.push(row.map_err(|e| Error::Db(format!("reading column name: {e}")))?);
        }
    }
    for column in ["client_pod", "server_pod", "host", "sql"] {
        if existing.iter().any(|name| name == column) {
            continue;
        }
        conn.execute_batch(&format!(
            "ALTER TABLE net_edges_hot ADD COLUMN {column} VARCHAR DEFAULT '' NOT NULL"
        ))
        .map_err(|e| Error::Db(format!("migrating net_edges_hot.{column}: {e}")))?;
    }
    refresh_views(conn, state)
}

/// Best-effort rebuild of the local S3 manifest at store-open time.
///
/// The manifest is a *cache* of which objects back each VIEW; the S3 bucket is
/// the source of truth. If the local manifest is lost (PVC rebuilt, node
/// replaced, first start against an existing bucket) we would otherwise hide
/// every archived object. One `glob()` per table at open time re-discovers
/// them. This is deliberately done here — not in `refresh_views` — so the hot
/// query path never issues a bucket LIST and still binds while S3 is down.
///
/// Returns the number of newly discovered objects. Any error is swallowed by
/// the caller so an unreachable/misconfigured S3 never blocks startup.
pub(crate) fn discover_s3_objects(
    conn: &Connection,
    state: &ParquetState,
    s3: &S3Config,
) -> Result<usize> {
    let base = state.s3_base(); // s3://bucket[/prefix]
    let mut manifest = state.load_manifest();
    let mut added = 0usize;

    for def in TABLES {
        // Archived objects live at <base>/<table>/hour=<n>.parquet. A single
        // level glob is enough and avoids scanning unrelated table prefixes.
        let pattern = format!("{base}/{}/*.parquet", def.name);
        // DuckDB's glob() returns a `file` column of fully-qualified s3://
        // URIs (URL-escaped keys). Bind the pattern so a quote in a prefix
        // can never break the SQL.
        let mut stmt = conn
            .prepare("SELECT file FROM glob(?)")
            .map_err(|e| Error::Db(format!("preparing S3 discovery for {}: {e}", def.name)))?;
        let rows = stmt
            .query_map([pattern.as_str()], |r| r.get::<_, String>(0))
            .map_err(|e| Error::Db(format!("listing S3 objects for {}: {e}", def.name)))?;

        let entry = manifest.objects.entry(def.name.to_string()).or_default();
        for row in rows.flatten() {
            let uri = normalize_s3_uri(&row, s3);
            if !entry.contains(&uri) {
                entry.push(uri);
                added += 1;
            }
        }
        entry.sort();
    }

    if added > 0 {
        state.save_manifest(&manifest)?;
        refresh_views(conn, state)?;
    }
    Ok(added)
}

/// `glob()` returns fully-qualified URIs for path-style buckets; normalise the
/// few shapes seen across httpfs versions so keys stay consistent with the
/// ones written by `archive_local` (URL-escaped, `s3://bucket/...`).
fn normalize_s3_uri(file: &str, s3: &S3Config) -> String {
    let f = file.trim();
    if f.starts_with("s3://") {
        return f.to_string();
    }
    // Some httpfs builds return a bucket-relative path; prepend the bucket.
    if f.starts_with(&format!("{}/", s3.bucket)) || f == s3.bucket {
        return format!("s3://{}", f.trim_start_matches('/'));
    }
    format!("s3://{}/{}", s3.bucket, f.trim_start_matches('/'))
}

/// Collect the explicit Parquet file paths (local + S3) known for a table.
fn parquet_paths(state: &ParquetState, manifest: &Manifest, table: &str) -> Vec<String> {
    let mut paths = Vec::new();
    if let Ok(entries) = std::fs::read_dir(state.table_dir(table)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".parquet") {
                paths.push(entry.path().to_string_lossy().replace('\\', "/"));
            }
        }
    }
    paths.sort();
    if let Some(objects) = manifest.objects.get(table) {
        paths.extend(objects.iter().cloned());
    }
    paths
}

/// (Re)build every canonical VIEW from the hot table + the exact set of known
/// Parquet files. No bucket LIST happens here: S3 objects come from the local
/// manifest, so binding works even before any upload and when S3 is down.
/// When there are no Parquet files the VIEW is just the hot table, which is
/// always a valid, correctly-typed relation.
///
/// The two halves are joined `BY NAME`, not positionally. An upgrade adds
/// columns (the container names on a service map edge, say), and the archive
/// still holds files written before they existed: listing the columns
/// explicitly makes the binder reject such a file, which turns every write into
/// a 500 until the first new file appears — a deadlock the deployment cannot
/// recover from on its own. With `BY NAME` the missing column reads as NULL,
/// which the consumers already treat as "not recorded" (an empty container, an
/// empty list).
pub(crate) fn refresh_views(conn: &Connection, state: &ParquetState) -> Result<()> {
    let manifest = state.load_manifest();
    for def in TABLES {
        let hot = format!("{}_hot", def.name);
        let paths = parquet_paths(state, &manifest, def.name);
        let sql = if paths.is_empty() {
            format!("CREATE OR REPLACE VIEW {} AS SELECT * FROM {hot}", def.name)
        } else {
            let list = paths
                .iter()
                .map(|p| format!("'{}'", esc(p)))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "CREATE OR REPLACE VIEW {name} AS \
                 SELECT * FROM {hot} \
                 UNION ALL BY NAME \
                 SELECT * FROM read_parquet([{list}], hive_partitioning=false, union_by_name=true)",
                name = def.name,
            )
        };
        conn.execute_batch(&sql)
            .map_err(|e| Error::Db(format!("creating view {}: {e}", def.name)))?;
    }
    Ok(())
}

/// The projection used when merging a group of local Parquet files, shared by
/// the archive write and its test. `union_by_name` lets files written before a
/// column was added merge with files written after it; naming columns here
/// would instead make the binder reject any file that predates one of them.
fn merge_projection(list: &str) -> String {
    format!("SELECT * FROM read_parquet([{list}], hive_partitioning=false, union_by_name=true)")
}

/// Free the current wall-clock hour as `h<epoch-hour>` for file naming.
pub(crate) fn hour_tag() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Seconds since epoch / 3600 gives a monotonically increasing hour number;
    // that is enough to group files by hour for compaction and TTL.
    format!("h{}", secs / 3600)
}

/// Copy all rows currently in the hot tables into one small Parquet file per
/// table and clear them. Called on a cadence; holds the connection lock at the
/// call site so no reader observes a partially flushed state.
pub(crate) fn flush_hot(conn: &Connection, state: &ParquetState) -> Result<usize> {
    let tag = hour_tag();
    let mut moved = 0usize;
    for def in TABLES {
        let hot = format!("{}_hot", def.name);
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {hot}"), [], |r| r.get(0))
            .unwrap_or(0);
        if count <= 0 {
            continue;
        }
        // `count` is not unique within an hour: two flushes of the same number
        // of rows used to write the same file name, so the second COPY
        // silently overwrote the first and those rows vanished from every
        // view. Use the wall clock at nanosecond resolution instead — the
        // hour prefix (used for TTL/archival) stays intact, and two flushes
        // in the same hour can no longer collide.
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file = state
            .table_dir(def.name)
            .join(format!("{tag}-{stamp}-{count}.parquet"));
        let sql = format!(
            "COPY (SELECT {cols} FROM {hot}) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 122880)",
            file.to_string_lossy().replace('\\', "/"),
            cols = def.columns,
        );
        conn.execute_batch(&sql)
            .map_err(|e| Error::Db(format!("flushing {}: {e}", def.name)))?;
        conn.execute_batch(&format!("DELETE FROM {hot}"))
            .map_err(|e| Error::Db(format!("clearing hot {}: {e}", def.name)))?;
        moved += count.max(0) as usize;
    }
    if moved > 0 {
        refresh_views(conn, state)?;
    }
    Ok(moved)
}

/// Result of one archival pass, useful for logging/metrics.
#[derive(Debug, Default, Clone, Copy)]
pub struct ArchiveStats {
    pub files_uploaded: usize,
    pub bytes_reclaimed: usize,
    /// Hours whose upload failed on this pass. They are skipped for
    /// [`ARCHIVE_RETRY_AFTER`] instead of being retried on the next tick.
    pub failed: usize,
}

/// First backoff after a failed archive attempt.
///
/// Ten minutes, not ten seconds: an attempt that fails has already paid for a
/// full-hour merge, and that merge holds the tenant's only connection. Retrying
/// eagerly cannot succeed — the failure is the endpoint, not the hour — and it
/// costs the dashboard every query that arrives meanwhile.
const ARCHIVE_RETRY_AFTER: Duration = Duration::from_secs(600);
/// Ceiling for that backoff, so a permanently broken bucket still gets retried.
const ARCHIVE_MAX_BACKOFF: Duration = Duration::from_secs(3_600);

/// Circuit-breaker state for the S3 archive.
#[derive(Debug, Default)]
struct ArchiveRetry {
    /// Earliest instant the next attempt may run.
    until: Option<Instant>,
    /// Duration applied by the most recent failure, doubled on the next one.
    backoff: Duration,
}

impl ArchiveRetry {
    fn blocked(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now < until)
    }

    fn record_failure(&mut self, now: Instant, first: Duration, cap: Duration) -> Duration {
        self.backoff = if self.backoff.is_zero() {
            first
        } else {
            (self.backoff * 2).min(cap)
        };
        self.until = Some(now + self.backoff);
        self.backoff
    }

    fn record_success(&mut self) {
        self.backoff = Duration::ZERO;
        self.until = None;
    }
}

/// Write a single-row Parquet object to the archive prefix.
///
/// Cheap enough to run on every archival pass and decisive: it exercises the
/// same code path as the merge (`COPY ... TO 's3://...'`, multipart upload and
/// all) without reading or compressing anything worth waiting for. An endpoint
/// that is unreachable, denies the credentials, or refuses multipart uploads
/// fails here in milliseconds instead of after a full-hour merge.
fn probe_archive_target(conn: &Connection, state: &ParquetState) -> Result<()> {
    // No `.parquet` suffix: the object is a write probe, not a table file, and
    // nothing should ever glob it back in as one.
    let key = format!("{}/.archive-probe", state.s3_base());
    conn.execute_batch(&format!(
        "COPY (SELECT 1::BIGINT AS ok) TO '{key}' (FORMAT PARQUET)"
    ))
    .map_err(|e| Error::Db(format!("archive probe to {key}: {e}")))
}

/// Archive local Parquet files that are older than the local TTL: merge the
/// hour's files into one large object, upload to S3, verify, then delete the
/// local copies. `now` is supplied by the caller (nanoseconds) so TTL decisions
/// are deterministic in tests.
pub(crate) fn archive_local(
    conn: &Connection,
    state: &ParquetState,
    now_ns: u64,
    s3: &S3Config,
) -> Result<ArchiveStats> {
    let cutoff_hour = (now_ns.saturating_sub(state.local_ttl_ns) / 1_000_000_000) / 3600;
    let mut stats = ArchiveStats::default();
    let mut manifest = state.load_manifest();
    let now = Instant::now();
    // One failed endpoint must not be retried per table per hour: see the
    // field comment. Skipping the whole pass keeps the connection free for
    // queries, which is the only thing the user can actually see.
    if state.archive_retry.lock().blocked(now) {
        return Ok(stats);
    }
    // Ask the bucket whether it accepts writes before merging an hour for it.
    // The probe is one row: milliseconds when the endpoint is healthy, and it
    // fails immediately when it is not — which is the whole point, because the
    // merge it guards re-reads and re-compresses tens of megabytes while
    // holding the connection every query is queued on. Without it, a bucket
    // that rejects multipart uploads costs a full merge per retry, and the
    // only visible symptom is queries that time out.
    if let Err(err) = probe_archive_target(conn, state) {
        let backoff = state.archive_retry.lock().record_failure(
            now,
            ARCHIVE_RETRY_AFTER,
            ARCHIVE_MAX_BACKOFF,
        );
        warn!(
            error = %err,
            retry_in_secs = backoff.as_secs(),
            "parquet archive target is not writable; pausing archival"
        );
        stats.failed += 1;
        return Ok(stats);
    }

    for def in TABLES {
        let dir = state.table_dir(def.name);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        // Group local files by their hour tag.
        let mut by_hour: std::collections::BTreeMap<u64, Vec<PathBuf>> =
            std::collections::BTreeMap::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(hour) = parse_hour_tag(&name) else {
                continue;
            };
            if hour >= cutoff_hour {
                continue;
            }
            by_hour.entry(hour).or_default().push(entry.path());
        }

        for (hour, files) in by_hour {
            // Merge into one large Parquet object directly on S3. DuckDB's
            // httpfs streams the upload; nothing is staged a second time.
            let list = files
                .iter()
                .map(|p| format!("'{}'", p.to_string_lossy().replace('\\', "/")))
                .collect::<Vec<_>>()
                .join(", ");
            let key_url = format!("{}/{}/hour={}.parquet", state.s3_base(), def.name, hour);
            // `SELECT *` with `union_by_name` is deliberate here, for the same
            // reason the VIEWs use it: an hour's local files can span an
            // upgrade, and the older ones predate columns such as `containers`.
            // Naming those columns explicitly makes the binder reject the whole
            // merge, so the archive never uploads and the local copies are
            // never reclaimed. A missing column reads as NULL instead, which
            // is how the read path already treats it.
            let sql = format!(
                "COPY ({}) TO '{key_url}' (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 122880)",
                merge_projection(&list),
            );
            // One failing hour must not stop the rest of the pass: an hour of
            // `samples` that S3 refuses used to abort before `stacks` was even
            // looked at, so nothing else was ever archived either.
            if let Err(err) = conn.execute_batch(&sql) {
                let backoff = state.archive_retry.lock().record_failure(
                    now,
                    ARCHIVE_RETRY_AFTER,
                    ARCHIVE_MAX_BACKOFF,
                );
                warn!(
                    table = def.name,
                    hour,
                    error = %err,
                    retry_in_secs = backoff.as_secs(),
                    "parquet archive attempt failed; pausing archival"
                );
                stats.failed += 1;
                // Nothing else in this pass can succeed either, and each
                // attempt costs a full merge on the connection every query is
                // waiting for.
                return Ok(stats);
            }
            state.archive_retry.lock().record_success();

            // A successful COPY is our commit marker: record the object in the
            // manifest first, then drop the local copies. Order matters — a
            // crash between the two leaves a harmless extra local file, never
            // a lost object.
            manifest
                .objects
                .entry(def.name.to_string())
                .or_default()
                .push(key_url.clone());
            state.save_manifest(&manifest)?;

            for file in &files {
                if let Ok(meta) = std::fs::metadata(file) {
                    stats.bytes_reclaimed += meta.len() as usize;
                }
                let _ = std::fs::remove_file(file);
                stats.files_uploaded += 1;
            }
        }
    }
    if stats.files_uploaded > 0 {
        // Build views from the fresh manifest (now includes the S3 objects).
        refresh_views(conn, state)?;
    }
    let _ = s3; // credentials already applied to the connection at open time
    Ok(stats)
}

/// Result of one local compaction pass.
#[derive(Debug, Default, Clone, Copy)]
pub struct CompactStats {
    /// Small files replaced by a merged file.
    pub files_merged: usize,
    /// Merged files written.
    pub files_written: usize,
    /// Bytes of the small files removed.
    pub bytes_reclaimed: u64,
}

/// Merge the local Parquet files of each completed hour into one file per
/// table, without touching any rows.
///
/// A five-minute ingest cadence flushes twelve files an hour per table, and
/// every query opens all of them just to read their footers. The metadata cost
/// therefore grows with the ingest rate rather than with the amount of data.
/// Rewriting the hour once keeps the file count bounded.
///
/// `current_hour` is skipped: it is still being appended to, and rewriting it
/// on every tick would cost more than it saves. Pass [`u64::MAX`] in tests.
pub(crate) fn compact_local(
    conn: &Connection,
    state: &ParquetState,
    current_hour: u64,
) -> Result<CompactStats> {
    // A first run against months of history would otherwise rewrite thousands
    // of hours in one tick, holding the connection and spiking memory. Bound
    // each pass; the tick runs again in ten minutes and works through the rest.
    //
    // One, not forty-eight: each merge rewrites a whole hour of Parquet while
    // holding the tenant's only connection, and queries wait behind it. A pass
    // that runs for minutes is indistinguishable from a hung server, and the
    // tick comes round again in ten minutes.
    const MAX_MERGES_PER_PASS: usize = 1;
    let mut stats = CompactStats::default();
    let mut changed = false;
    'tables: for def in TABLES {
        let dir = state.table_dir(def.name);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut by_hour: std::collections::BTreeMap<u64, Vec<PathBuf>> =
            std::collections::BTreeMap::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(hour) = parse_hour_tag(&name) else {
                continue;
            };
            if hour >= current_hour {
                continue;
            }
            by_hour.entry(hour).or_default().push(entry.path());
        }
        for (hour, files) in by_hour {
            if files.len() < 2 {
                continue;
            }
            if stats.files_written >= MAX_MERGES_PER_PASS {
                break 'tables;
            }
            let list = files
                .iter()
                .map(|p| format!("'{}'", p.to_string_lossy().replace('\\', "/")))
                .collect::<Vec<_>>()
                .join(", ");
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let target = dir.join(format!("h{hour}-compact-{unique}.parquet"));
            let tmp = dir.join(format!("h{hour}-compact-{unique}.parquet.tmp"));
            let sql = format!(
                "COPY ({}) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 122880)",
                merge_projection(&list),
                tmp.to_string_lossy().replace('\\', "/"),
            );
            if let Err(err) = conn.execute_batch(&sql) {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::Db(format!(
                    "compacting {} hour {hour}: {err}",
                    def.name
                )));
            }
            let rows: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM read_parquet('{}')",
                        tmp.to_string_lossy().replace('\\', "/")
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            if rows <= 0 {
                let _ = std::fs::remove_file(&tmp);
                continue;
            }
            // Publish the merged file before removing the parts. If the rename
            // fails the old files are still there; if a remove fails the merged
            // file is a superset, so a duplicate is the worst case, never a gap.
            std::fs::rename(&tmp, &target).map_err(Error::Io)?;
            for file in &files {
                if let Ok(meta) = std::fs::metadata(file) {
                    stats.bytes_reclaimed += meta.len();
                }
                let _ = std::fs::remove_file(file);
            }
            stats.files_merged += files.len();
            stats.files_written += 1;
            changed = true;
        }
    }
    // Hourly compaction still leaves one file per busy hour. Every query opens
    // all of them for their footers, so a one-hour request pays for the whole
    // retention window. Fold the smallest files into one until the table is
    // back under budget; already-large merged files are left alone, so the
    // oldest data is rewritten at most once.
    for def in TABLES {
        if merge_smallest_files(conn, state, def, 16)? {
            changed = true;
        }
    }
    if changed {
        refresh_views(conn, state)?;
    }
    Ok(stats)
}

/// Merge the smallest local files of one table until it holds `budget` files.
/// Returns whether anything was rewritten.
fn merge_smallest_files(
    conn: &Connection,
    state: &ParquetState,
    def: &TableDef,
    budget: usize,
) -> Result<bool> {
    let dir = state.table_dir(def.name);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(false);
    };
    let mut files: Vec<(u64, u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(hour) = parse_hour_tag(&name) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        files.push((meta.len(), hour, entry.path()));
    }
    if files.len() <= budget {
        return Ok(false);
    }
    files.sort_by_key(|(size, _, _)| *size);
    let take = files.len() - budget + 1;
    let chosen: Vec<(u64, u64, PathBuf)> = files.into_iter().take(take).collect();

    let list = chosen
        .iter()
        .map(|(_, _, p)| format!("'{}'", p.to_string_lossy().replace('\\', "/")))
        .collect::<Vec<_>>()
        .join(", ");
    let hour = chosen.iter().map(|(_, hour, _)| *hour).min().unwrap_or(0);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let target = dir.join(format!("h{hour}-merged-{unique}.parquet"));
    let tmp = dir.join(format!("h{hour}-merged-{unique}.parquet.tmp"));
    let sql = format!(
        "COPY ({}) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 122880)",
        merge_projection(&list),
        tmp.to_string_lossy().replace('\\', "/"),
    );
    if let Err(err) = conn.execute_batch(&sql) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Db(format!(
            "merging {} down to {budget} files: {err}",
            def.name
        )));
    }
    let rows: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM read_parquet('{}')",
                tmp.to_string_lossy().replace('\\', "/")
            ),
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if rows <= 0 {
        let _ = std::fs::remove_file(&tmp);
        return Ok(false);
    }
    std::fs::rename(&tmp, &target).map_err(Error::Io)?;
    for (_, _, file) in &chosen {
        let _ = std::fs::remove_file(file);
    }
    Ok(true)
}

/// Size on local disk of the staged Parquet files.
pub(crate) fn staged_bytes(state: &ParquetState) -> u64 {
    let mut total = 0u64;
    for def in TABLES {
        if let Ok(entries) = std::fs::read_dir(state.table_dir(def.name)) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    total = total.saturating_add(meta.len());
                }
            }
        }
    }
    total
}

fn parse_hour_tag(name: &str) -> Option<u64> {
    let stem = name.strip_prefix('h')?;
    let end = stem.find('-').unwrap_or(stem.len());
    stem[..end].parse().ok()
}

fn esc(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Frame, ProfileType, ReportBuilder};
    use crate::selector::Selector;
    use crate::store::{StorageMode, Store, StoreConfig};

    fn parquet_store(name: &str, ttl_ns: u64) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ldx-pq-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s3 = S3Config {
            // Bucket is never contacted in these tests: TTL is huge so no
            // archive runs. LOAD httpfs is local and needs no network.
            bucket: "unused".into(),
            prefix: "test".into(),
            endpoint: Some("127.0.0.1:9000".into()),
            use_ssl: Some(false),
            url_style: Some("path".into()),
            ..Default::default()
        };
        let cfg = StoreConfig {
            root: dir.clone(),
            storage_mode: StorageMode::Parquet,
            s3: Some(s3),
            parquet_local_ttl_ns: Some(ttl_ns),
            ..StoreConfig::new(&dir)
        };
        let store = Store::open(cfg).unwrap();
        (store, dir)
    }

    fn report(service: &str, start: u64, value: u64) -> crate::model::Report {
        let mut b = ReportBuilder::new(service, "cpu", start, start + 1_000_000_000);
        b.add_stack([Frame::new("/bin/app", "main")], value);
        b.finish()
    }

    fn flame_total(store: &Store) -> u64 {
        let q = crate::QueryOptions::new(
            ProfileType::cpu(),
            1_000_000_000,
            4_000_000_000,
            Selector::default(),
        );
        crate::flamegraph(store, &q).unwrap().total as u64
    }

    #[test]
    fn parquet_roundtrip_hot_then_flushed() {
        let (store, dir) = parquet_store("roundtrip", u64::MAX);
        assert_eq!(store.storage_mode(), StorageMode::Parquet);
        // No duckdb main file should be created for the hot tier.
        assert!(!dir.join("lidlesseye.duckdb").exists());

        store
            .append(&report("checkout", 1_000_000_000, 42))
            .unwrap();
        assert_eq!(store.stats().batch_count, 1, "batch visible while hot");
        assert_eq!(flame_total(&store), 42, "hot rows visible through the view");

        // Flush to local Parquet and confirm data is still queryable.
        let moved = store.flush_hot().unwrap();
        assert!(moved >= 1, "hot rows moved to parquet");
        assert!(
            dir.join("parquet")
                .join("samples")
                .read_dir()
                .unwrap()
                .next()
                .is_some()
        );
        assert_eq!(
            store.stats().batch_count,
            1,
            "batch still visible after flush"
        );
        assert_eq!(
            flame_total(&store),
            42,
            "flushed rows readable zero-copy from parquet"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_equal_flushes_in_one_hour_do_not_overwrite_each_other() {
        let (store, dir) = parquet_store("collision", u64::MAX);
        // Each flush moves exactly one batch. Both land in the same hour with
        // the same row count, which used to produce one file name and lose
        // the first batch.
        store
            .append(&report("checkout", 1_000_000_000, 20))
            .unwrap();
        assert!(store.flush_hot().unwrap() > 0);
        store
            .append(&report("checkout", 2_000_000_000, 22))
            .unwrap();
        assert!(store.flush_hot().unwrap() > 0);

        let files = dir
            .join("parquet")
            .join("samples")
            .read_dir()
            .unwrap()
            .count();
        assert_eq!(files, 2, "two flushes must leave two distinct files");
        assert_eq!(store.stats().batch_count, 2, "both batches survive");
        assert_eq!(flame_total(&store), 42, "both values queryable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_merges_same_hour_files_and_keeps_rows() {
        let (store, dir) = parquet_store("compact", u64::MAX);
        store
            .append(&report("checkout", 1_000_000_000, 20))
            .unwrap();
        assert!(store.flush_hot().unwrap() > 0);
        store
            .append(&report("checkout", 2_000_000_000, 22))
            .unwrap();
        assert!(store.flush_hot().unwrap() > 0);

        let samples = dir.join("parquet").join("samples");
        assert_eq!(
            samples.read_dir().unwrap().count(),
            2,
            "two flushes in one hour leave two files"
        );

        // A pass merges one hour of one table and stops: the work is done on
        // the same connection queries use, so the tick is deliberately small.
        // Run passes until the backlog is gone.
        let mut written = 0;
        let mut merged = 0;
        for _ in 0..8 {
            let stats = store.compact_all_for_test().unwrap();
            if stats.files_written == 0 {
                break;
            }
            written += stats.files_written;
            merged += stats.files_merged;
        }
        // samples, batches and stacks each had two parts; frames is a shared
        // dictionary and may already be a single file, so assert the lower
        // bound rather than a table count that can change with the schema.
        assert!(written >= 3, "one merged file per multi-part table");
        assert!(merged >= 6);
        assert_eq!(
            samples.read_dir().unwrap().count(),
            1,
            "the parts are replaced by the merged file"
        );
        assert_eq!(store.stats().batch_count, 2, "both batches survive");
        assert_eq!(flame_total(&store), 42, "merged rows stay queryable");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parquet_empty_views_are_queryable() {
        let (store, dir) = parquet_store("empty", u64::MAX);
        assert_eq!(store.stats().batch_count, 0);
        // A flamegraph over an empty parquet store must not error.
        assert_eq!(flame_total(&store), 0);
        // Flushing an empty hot tier is a no-op.
        assert_eq!(store.flush_hot().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_view_reads_an_archive_file_written_before_a_column_existed() {
        // The upgrade that added container names to the map left files behind
        // that predate the column. Listing the columns explicitly made the
        // binder reject them, and because the views are rebuilt on every write
        // the deployment could not get past it — every ingest returned 500.
        let dir = std::env::temp_dir().join(format!("ldx-pq-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("parquet").join("net_nodes")).unwrap();
        let file = dir.join("parquet").join("net_nodes").join("old.parquet");

        let conn = Connection::open_in_memory().unwrap();
        // `net_nodes` as the release before containers wrote it.
        conn.execute_batch(&format!(
            "COPY (SELECT 1::BIGINT AS bucket_ns, 'shop/a' AS node_id, 'shop' AS namespace, \
             'a' AS workload, 'a' AS service, 'workload' AS kind, 'node' AS k8s_node, \
             0::BIGINT AS rx_bytes, 0::BIGINT AS tx_bytes, 0::BIGINT AS connections, \
             0::BIGINT AS retransmits, 1::BIGINT AS pods, 1::BIGINT AS ready, \
             '[]' AS instances) TO '{}' (FORMAT PARQUET)",
            file.to_string_lossy().replace('\\', "/")
        ))
        .unwrap();

        let state = ParquetState {
            dir: dir.join("parquet"),
            bucket: "unused".into(),
            prefix: "test".into(),
            local_ttl_ns: u64::MAX,
            manifest_path: dir.join("parquet").join("manifest.json"),
            archive_retry: Arc::new(Mutex::new(ArchiveRetry::default())),
        };
        state.save_manifest(&Manifest::default()).unwrap();
        create_schema(&conn, &state).unwrap();

        let rows: i64 = conn
            .query_row("SELECT count(*) FROM net_nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "the archived row still reads");
        let containers: Option<String> = conn
            .query_row("SELECT containers FROM net_nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(containers, None, "a column the file predates reads as NULL");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archiving_a_hour_that_spans_an_upgrade_succeeds() {
        // The S3 failure seen after adding container columns: an hour's local
        // files include one written before `containers` existed. The archive
        // merge must read it (missing column -> NULL), not reject it.
        let dir = std::env::temp_dir().join(format!("ldx-pq-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.parquet");
        let new = dir.join("new.parquet");
        let merged = dir.join("merged.parquet");

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT 1::BIGINT AS bucket_ns, 'a' AS node_id, 7::BIGINT AS connections) \
             TO '{}' (FORMAT PARQUET)",
            old.to_string_lossy().replace('\\', "/")
        ))
        .unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT 2::BIGINT AS bucket_ns, 'b' AS node_id, 9::BIGINT AS connections, \
             '[\"app\",\"proxy\"]' AS containers) TO '{}' (FORMAT PARQUET)",
            new.to_string_lossy().replace('\\', "/")
        ))
        .unwrap();

        let list = [&old, &new]
            .iter()
            .map(|p| format!("'{}'", p.to_string_lossy().replace('\\', "/")))
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute_batch(&format!(
            "COPY ({}) TO '{}' (FORMAT PARQUET)",
            merge_projection(&list),
            merged.to_string_lossy().replace('\\', "/")
        ))
        .unwrap();

        let rows: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM read_parquet('{}', union_by_name=true)",
                    merged.to_string_lossy().replace('\\', "/")
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2, "both the legacy and the modern row are merged");
        let nulls: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM read_parquet('{}', union_by_name=true) \
                     WHERE containers IS NULL",
                    merged.to_string_lossy().replace('\\', "/")
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            nulls, 1,
            "the pre-upgrade row keeps a NULL containers column"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parquet_service_map_roundtrip() {
        use crate::servicemap::{NodeKind, ServiceEdge, ServiceMapBatch, ServiceNode};
        let (store, dir) = parquet_store("netmap", u64::MAX);
        let batch = ServiceMapBatch {
            start_ns: 1_000_000_000,
            end_ns: 2_000_000_000,
            node: "host".into(),
            nodes: vec![ServiceNode {
                id: "svc".into(),
                service: "svc".into(),
                kind: NodeKind::Service,
                ..Default::default()
            }],
            edges: vec![ServiceEdge {
                client: "ext".into(),
                server: "svc".into(),
                protocol: "http".into(),
                connections: 5,
                ..Default::default()
            }],
        };
        store.append_servicemap(&batch).unwrap();
        let map = crate::servicemap(&store, 1_000_000_000, 3_000_000_000).unwrap();
        assert!(!map.nodes.is_empty(), "net node visible while hot");

        store.flush_hot().unwrap();
        let map2 = crate::servicemap(&store, 1_000_000_000, 3_000_000_000).unwrap();
        assert!(
            !map2.nodes.is_empty(),
            "net node visible after parquet flush"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
