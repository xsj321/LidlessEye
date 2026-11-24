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
//! The explicit file list comes from an in-memory index of S3 object keys
//! plus a `read_dir` of the local staging dir. No JSON catalog or DuckDB
//! database file is persisted: the Parquet files and S3 objects are the only
//! durable state. Explicit paths avoid a bucket LIST at query bind time.
//! On open we run one best-effort `glob()` of each S3 prefix to rebuild the
//! object index, so a new pod against an existing bucket never hides data.
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
use crate::store::{S3Config, StorageMode};

/// One physical table and the projection used when exposing it through VIEWs.
pub(crate) struct TableDef {
    pub name: &'static str,
    /// Explicit column list — required so an empty `read_parquet` glob still
    /// produces a relation with a stable schema for the UNION.
    pub columns: &'static str,
    /// CREATE TABLE body for the hot tier (`{t}` is the table name).
    pub ddl: &'static str,
    /// Nanosecond column whose epoch hour forms the Parquet partition. Dictionaries
    /// have no time column and are written into the current hour.
    pub hour_column: Option<&'static str>,
}

pub(crate) const TABLES: &[TableDef] = &[
    TableDef {
        name: "batches",
        columns: "batch_id, profile_type, service_name, labels, start_ns, end_ns, total_value, sample_count, bytes, created_ns",
        ddl: "CREATE TABLE {t} (batch_id BIGINT PRIMARY KEY, profile_type VARCHAR NOT NULL, service_name VARCHAR NOT NULL, labels VARCHAR NOT NULL, start_ns BIGINT NOT NULL, end_ns BIGINT NOT NULL, total_value BIGINT NOT NULL, sample_count BIGINT NOT NULL, bytes BIGINT NOT NULL, created_ns BIGINT NOT NULL)",
        hour_column: Some("start_ns"),
    },
    TableDef {
        name: "samples",
        columns: "batch_id, profile_type, service_name, labels, start_ns, end_ns, stack_id, value",
        ddl: "CREATE TABLE {t} (batch_id BIGINT NOT NULL, profile_type VARCHAR NOT NULL, service_name VARCHAR NOT NULL, labels VARCHAR NOT NULL, start_ns BIGINT NOT NULL, end_ns BIGINT NOT NULL, stack_id BIGINT NOT NULL, value BIGINT NOT NULL)",
        hour_column: Some("start_ns"),
    },
    TableDef {
        name: "stacks",
        columns: "stack_id, frames",
        ddl: "CREATE TABLE {t} (stack_id BIGINT PRIMARY KEY, frames BLOB NOT NULL)",
        hour_column: None,
    },
    TableDef {
        name: "frames",
        columns: "frame_id, module, symbol, file, line",
        ddl: "CREATE TABLE {t} (frame_id INTEGER PRIMARY KEY, module VARCHAR NOT NULL, symbol VARCHAR NOT NULL, file VARCHAR NOT NULL, line INTEGER NOT NULL)",
        hour_column: None,
    },
    TableDef {
        name: "net_nodes",
        columns: "bucket_ns, node_id, namespace, workload, service, kind, k8s_node, rx_bytes, tx_bytes, connections, retransmits, pods, ready, instances, containers",
        ddl: "CREATE TABLE {t} (bucket_ns BIGINT NOT NULL, node_id VARCHAR NOT NULL, namespace VARCHAR NOT NULL, workload VARCHAR NOT NULL, service VARCHAR NOT NULL, kind VARCHAR NOT NULL, k8s_node VARCHAR NOT NULL, rx_bytes BIGINT NOT NULL, tx_bytes BIGINT NOT NULL, connections BIGINT NOT NULL, retransmits BIGINT NOT NULL, pods BIGINT DEFAULT 0 NOT NULL, ready BIGINT DEFAULT 0 NOT NULL, instances VARCHAR DEFAULT '[]' NOT NULL, containers VARCHAR DEFAULT '[]' NOT NULL)",
        hour_column: Some("bucket_ns"),
    },
    TableDef {
        name: "net_edges",
        columns: "bucket_ns, client_id, server_id, protocol, client_container, server_container, client_pod, server_pod, connections, retransmits, tx_bytes, rx_bytes, rtt_us_sum, rtt_samples, host, sql",
        ddl: "CREATE TABLE {t} (bucket_ns BIGINT NOT NULL, client_id VARCHAR NOT NULL, server_id VARCHAR NOT NULL, protocol VARCHAR NOT NULL, client_container VARCHAR DEFAULT '' NOT NULL, server_container VARCHAR DEFAULT '' NOT NULL, client_pod VARCHAR DEFAULT '' NOT NULL, server_pod VARCHAR DEFAULT '' NOT NULL, connections BIGINT NOT NULL, retransmits BIGINT NOT NULL, tx_bytes BIGINT NOT NULL, rx_bytes BIGINT NOT NULL, rtt_us_sum BIGINT NOT NULL, rtt_samples BIGINT NOT NULL, host VARCHAR DEFAULT '' NOT NULL, sql VARCHAR DEFAULT '' NOT NULL)",
        hour_column: Some("bucket_ns"),
    },
];

/// In-memory index of the S3 objects that back a table. A VIEW is built from
/// an explicit list of object keys and never has to LIST the bucket at bind
/// time — binding stays fast and works while S3 is temporarily unreachable.
/// The index is rebuilt from the bucket on open, never persisted as JSON.
#[derive(Debug, Clone, Default)]
pub(crate) struct Manifest {
    /// table name -> S3 URIs of the merged objects, oldest first.
    pub objects: std::collections::BTreeMap<String, Vec<String>>,
}

/// Per-store state for the hot tier + Parquet layout.
#[derive(Debug, Clone)]
pub(crate) struct ParquetState {
    /// `<root>/parquet` — local Parquet files, one subdir per table.
    pub dir: PathBuf,
    /// `disk` keeps local finals; `s3` uploads and removes local staging.
    pub mode: StorageMode,
    pub bucket: String,
    /// Full S3 key prefix including the tenant id, no trailing slash.
    pub prefix: String,
    /// How long a local fragment may wait before being merged/uploaded.
    pub local_ttl_ns: u64,
    /// Rebuilt from deterministic object keys on open; no on-disk catalogue.
    manifest: Arc<Mutex<Manifest>>,
    /// When the next S3 archive attempt is allowed, after a failure.
    ///
    /// A failed upload has already paid for a whole-hour merge. Retrying every
    /// ten seconds turned a broken bucket into a permanent full-time scan,
    /// with every dashboard query queued behind it. The backoff is global
    /// because the failure is almost never about one hour: an unreachable
    /// endpoint, a bad credential and a refused multipart upload all fail
    /// every hour the same way. It doubles up to [`ARCHIVE_MAX_BACKOFF`].
    archive_retry: Arc<Mutex<ArchiveRetry>>,
}

impl ParquetState {
    pub fn new(root: &Path, mode: StorageMode, s3: Option<&S3Config>, local_ttl_ns: u64) -> Self {
        let dir = root.join("parquet");
        ParquetState {
            dir,
            mode,
            bucket: s3.map(|s| s.bucket.clone()).unwrap_or_default(),
            prefix: s3
                .map(|s| s.prefix.trim_matches('/').to_string())
                .unwrap_or_default(),
            local_ttl_ns,
            manifest: Arc::new(Mutex::new(Manifest::default())),
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
        self.manifest.lock().clone()
    }

    pub fn save_manifest(&self, manifest: &Manifest) -> Result<()> {
        *self.manifest.lock() = manifest.clone();
        Ok(())
    }

    /// Clear the in-memory object index. Also removes the `manifest.json` left
    /// by releases before the index became in-memory only.
    pub fn clear_manifest(&self) -> Result<()> {
        *self.manifest.lock() = Manifest::default();
        let legacy = self.dir.join("manifest.json");
        match std::fs::remove_file(legacy) {
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
    // A killed flush/merge leaves `.tmp` files that no VIEW references. They
    // are inert, but they would count against the disk cap forever, so clean
    // them up on every open. An old `manifest.json` is likewise obsolete now
    // that the object index lives only in memory.
    let _ = state.clear_manifest();
    for def in TABLES {
        if let Ok(entries) = std::fs::read_dir(state.table_dir(def.name)) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().ends_with(".tmp") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
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

/// A completed hour has one immutable final file. Files written before it are
/// already included in that final and must not be counted a second time —
/// a crash between publishing the final and deleting the fragments would
/// otherwise double every sample in the hour.
fn is_final_file(name: &str) -> bool {
    name.contains("-final") || name.contains("-compact") || name.contains("-merged")
}

/// Local Parquet paths for one table, with superseded hour fragments removed.
///
/// The newest final file of an hour wins; only files modified at the same time
/// or later are still needed. This is a pure read-side rule, so recovery is
/// idempotent whether or not the deleting compaction pass ran.
fn local_parquet_paths(state: &ParquetState, table: &str) -> Vec<String> {
    let mut by_hour: std::collections::BTreeMap<u64, Vec<(PathBuf, SystemTime)>> =
        std::collections::BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(state.table_dir(table)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".parquet") {
                continue;
            }
            // Untagged files come from older releases or one-off imports;
            // give them hour 0 so they stay queryable (and are pruned first).
            let hour = parse_hour_tag(&name).unwrap_or(0);
            let modified = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(UNIX_EPOCH);
            by_hour
                .entry(hour)
                .or_default()
                .push((entry.path(), modified));
        }
    }
    let mut paths = Vec::new();
    for (_, mut files) in by_hour {
        let newest_final = files
            .iter()
            .filter(|(path, _)| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(is_final_file)
            })
            .map(|(_, modified)| *modified)
            .max();
        if let Some(modified) = newest_final {
            files.retain(|(_, at)| *at >= modified);
        }
        for (path, _) in files {
            paths.push(path.to_string_lossy().replace('\\', "/"));
        }
    }
    paths.sort();
    paths
}

/// Collect the explicit Parquet file paths (local + S3) known for a table.
fn parquet_paths(state: &ParquetState, manifest: &Manifest, table: &str) -> Vec<String> {
    let mut paths = local_parquet_paths(state, table);
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
    let mut moved = 0usize;
    // One stamp for the whole flush: the files written for every table belong
    // to the same logical generation. Retention and the disk cap evict by
    // stamp, so per-table stamps could delete `batches` while leaving their
    // `samples` (or vice versa), breaking referential integrity.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for def in TABLES {
        let hot = format!("{}_hot", def.name);
        let dir = state.table_dir(def.name);
        // Partition by the data's own timestamp, not the wall clock at flush
        // time. Backfilled or delayed reports must land in the hour they
        // describe, otherwise retention and the disk cap evict the wrong rows.
        let mut hours: Vec<i64> = Vec::new();
        if let Some(column) = def.hour_column {
            let sql =
                format!("SELECT DISTINCT CAST({column}/1_000_000_000/3600 AS BIGINT) FROM {hot}");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| Error::Db(format!("listing {hot} hours: {e}")))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, i64>(0))
                .map_err(|e| Error::Db(format!("listing {hot} hours: {e}")))?;
            for row in rows.flatten() {
                hours.push(row);
            }
        } else {
            // Dictionaries have no time column; file them under the current
            // hour so they are merged alongside the rows that reference them.
            hours.push(
                hour_tag()
                    .trim_start_matches('h')
                    .parse::<i64>()
                    .unwrap_or(0),
            );
        }
        hours.sort_unstable();
        for hour in hours {
            let filter = def
                .hour_column
                .map(|column| format!("WHERE CAST({column}/1_000_000_000/3600 AS BIGINT) = {hour}"))
                .unwrap_or_default();
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {hot} {filter}"), [], |r| {
                    r.get(0)
                })
                .unwrap_or(0);
            if count <= 0 {
                continue;
            }
            let name = format!("h{hour}-{stamp}-{count}.parquet");
            let file = dir.join(&name);
            let tmp = dir.join(format!("{name}.tmp"));
            let sql = format!(
                "COPY (SELECT {cols} FROM {hot} {filter}) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 122880)",
                tmp.to_string_lossy().replace('\\', "/"),
                cols = def.columns,
            );
            conn.execute_batch(&sql)
                .map_err(|e| Error::Db(format!("flushing {}: {e}", def.name)))?;
            // Publish only a file we have read back. A partial COPY is then a
            // harmless `.tmp` that startup recovery removes, never a VIEW entry.
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
            if rows != count {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::Db(format!(
                    "flushing {}: wrote {rows} rows, expected {count}",
                    def.name
                )));
            }
            std::fs::rename(&tmp, &file).map_err(Error::Io)?;
            moved += count.max(0) as usize;
        }
        // Clear once per table, after every hour's file is safely published.
        conn.execute_batch(&format!("DELETE FROM {hot}"))
            .map_err(|e| Error::Db(format!("clearing hot {}: {e}", def.name)))?;
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
            // Never delete the only copy on a successful-looking upload:
            // read the object back and compare row counts first. Some S3
            // endpoints acknowledge multipart completion before the object is
            // actually readable, and that used to turn a transient failure
            // into permanent data loss.
            let local_rows: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM read_parquet([{list}], \
                         hive_partitioning=false, union_by_name=true)"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(-1);
            let remote_rows: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM read_parquet('{key_url}', \
                         hive_partitioning=false, union_by_name=true)"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(-1);
            if remote_rows < 0 || remote_rows != local_rows {
                let backoff = state.archive_retry.lock().record_failure(
                    now,
                    ARCHIVE_RETRY_AFTER,
                    ARCHIVE_MAX_BACKOFF,
                );
                warn!(
                    table = def.name,
                    hour,
                    local_rows,
                    remote_rows,
                    retry_in_secs = backoff.as_secs(),
                    "uploaded Parquet object failed verification; keeping local copies"
                );
                stats.failed += 1;
                return Ok(stats);
            }
            state.archive_retry.lock().record_success();

            // The verified object is now the durable copy: record it in the
            // in-memory index first, then drop the local files. A crash between
            // the two leaves a harmless extra local file, never a lost object.
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
    // The caller refreshes the canonical VIEWs on the hot connection: this
    // function runs on the maintenance connection, which has no VIEWs.
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
            let target = dir.join(format!("h{hour}-final-{unique}.parquet"));
            let tmp = dir.join(format!("h{hour}-final-{unique}.parquet.tmp"));
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
        if merge_smallest_files(conn, state, def, 16, current_hour)? {
            changed = true;
            stats.files_written += 1;
        }
    }
    let _ = changed; // caller refreshes the read-side VIEWs on the hot connection
    Ok(stats)
}

/// Keep the file count of one table under `budget` by merging the busiest
/// closed hour into a single final file.
///
/// Merging across hours would make an hour's data depend on the time another
/// hour was pruned, so the unit of merging is always one hour. Returns whether
/// anything was rewritten.
fn merge_smallest_files(
    conn: &Connection,
    state: &ParquetState,
    def: &TableDef,
    budget: usize,
    current_hour: u64,
) -> Result<bool> {
    let dir = state.table_dir(def.name);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(false);
    };
    let mut by_hour: std::collections::BTreeMap<u64, Vec<PathBuf>> =
        std::collections::BTreeMap::new();
    let mut total = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".parquet") {
            continue;
        }
        let Some(hour) = parse_hour_tag(&name) else {
            continue;
        };
        if hour >= current_hour {
            continue;
        }
        total += 1;
        by_hour.entry(hour).or_default().push(entry.path());
    }
    if total <= budget || by_hour.is_empty() {
        return Ok(false);
    }
    let Some((hour, chosen)) = by_hour.into_iter().max_by_key(|(_, files)| files.len()) else {
        return Ok(false);
    };
    if chosen.len() < 2 {
        return Ok(false);
    }
    let list = chosen
        .iter()
        .map(|p| format!("'{}'", p.to_string_lossy().replace('\\', "/")))
        .collect::<Vec<_>>()
        .join(", ");
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let target = dir.join(format!("h{hour}-final-{unique}.parquet"));
    let tmp = dir.join(format!("h{hour}-final-{unique}.parquet.tmp"));
    let sql = format!(
        "COPY ({}) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 122880)",
        merge_projection(&list),
        tmp.to_string_lossy().replace('\\', "/"),
    );
    if let Err(err) = conn.execute_batch(&sql) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Db(format!(
            "merging {} hour {hour} down to {budget} files: {err}",
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
    // Publish first, then remove the inputs. The read-side "newest final wins"
    // rule hides the inputs until they are gone, even after a crash.
    std::fs::rename(&tmp, &target).map_err(Error::Io)?;
    for file in &chosen {
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

/// Result of one archive-layer retention/cap pass.
#[derive(Debug, Default, Clone, Copy)]
pub struct ArchivePruneStats {
    pub files_removed: usize,
    pub bytes_reclaimed: u64,
}

/// Enforce retention and the disk cap on committed Parquet files.
///
/// `disk` mode has no database to shrink: deleting rows from the in-memory hot
/// tier cannot reclaim PVC space, so eviction must remove whole hour files.
/// `.tmp` files are never candidates — they are either a partial write or a
/// published file whose deletion is still in flight. The hard disk cap wins
/// over retention, as documented in the deployment guide.
pub(crate) fn prune_archives(
    state: &ParquetState,
    now_ns: u64,
    retention_ns: Option<u64>,
    max_disk_bytes: Option<u64>,
) -> Result<ArchivePruneStats> {
    if state.mode != StorageMode::Disk {
        // In `s3` mode local files are unconfirmed staging copies. Deleting
        // them to meet a cap could destroy the only copy of a batch; bucket
        // lifecycle rules own retention for confirmed objects.
        return Ok(ArchivePruneStats::default());
    }
    struct File {
        hour: u64,
        stamp: Option<String>,
        modified: SystemTime,
        size: u64,
        path: PathBuf,
    }

    let mut files: Vec<File> = Vec::new();
    for def in TABLES {
        let Ok(entries) = std::fs::read_dir(state.table_dir(def.name)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".parquet") {
                continue;
            }
            let Some(hour) = parse_hour_tag(&name) else {
                continue;
            };
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            files.push(File {
                hour,
                stamp: parse_stamp_tag(&name),
                modified: meta.modified().unwrap_or(UNIX_EPOCH),
                size: meta.len(),
                path: entry.path(),
            });
        }
    }
    let mut stats = ArchivePruneStats::default();
    let cutoff_hour =
        retention_ns.map(|retention| now_ns.saturating_sub(retention) / 1_000_000_000 / 3600);
    let remove = |file: &File, stats: &mut ArchivePruneStats| {
        if std::fs::remove_file(&file.path).is_ok() {
            stats.files_removed += 1;
            stats.bytes_reclaimed += file.size;
        }
    };
    files.retain(|file| {
        if cutoff_hour.is_some_and(|cutoff| file.hour < cutoff) {
            remove(file, &mut stats);
            false
        } else {
            true
        }
    });
    if let Some(limit) = max_disk_bytes {
        let total: u64 = files.iter().map(|file| file.size).sum();
        if total > limit {
            // A flush writes one file per table with the same stamp. They must
            // be evicted together: deleting only the `batches` file would
            // leave samples nobody can attribute to a series, and deleting
            // only `samples` would keep empty batches. A compacted hour is
            // one replacement file per table; removing it while the inputs
            // remain would resurrect duplicates through the newest-final-wins
            // read rule, so those hours are grouped as a single unit.
            let final_hours: std::collections::BTreeSet<u64> = files
                .iter()
                .filter(|file| {
                    is_final_file(&file.path.file_name().unwrap_or_default().to_string_lossy())
                })
                .map(|file| file.hour)
                .collect();
            let mut units: std::collections::BTreeMap<String, Vec<File>> =
                std::collections::BTreeMap::new();
            for file in files {
                let key = if final_hours.contains(&file.hour) {
                    format!("hour:{}", file.hour)
                } else {
                    file.stamp
                        .clone()
                        .unwrap_or_else(|| format!("hour:{}", file.hour))
                };
                units.entry(key).or_default().push(file);
            }
            let mut ordered: Vec<(u64, SystemTime, Vec<File>)> = units
                .into_values()
                .map(|unit| {
                    let hour = unit.iter().map(|file| file.hour).min().unwrap_or(0);
                    let modified = unit
                        .iter()
                        .map(|file| file.modified)
                        .max()
                        .unwrap_or(UNIX_EPOCH);
                    (hour, modified, unit)
                })
                .collect();
            ordered.sort_by_key(|(hour, modified, _)| (*hour, *modified));
            let mut excess = total - limit;
            for (_, _, unit) in &ordered {
                if excess == 0 {
                    break;
                }
                for file in unit {
                    remove(file, &mut stats);
                    excess = excess.saturating_sub(file.size);
                }
            }
        }
    }
    Ok(stats)
}

fn parse_hour_tag(name: &str) -> Option<u64> {
    let stem = name.strip_prefix('h')?;
    let end = stem.find('-').unwrap_or(stem.len());
    stem[..end].parse().ok()
}

/// The flush/merge stamp that ties one file per table into one logical write.
/// Older layouts without a stamp are grouped by hour instead.
fn parse_stamp_tag(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".parquet")?;
    let (_, rest) = stem.strip_prefix('h')?.split_once('-')?;
    let parts: Vec<&str> = rest.split('-').collect();
    let first = *parts.first()?;
    if first.is_empty() {
        return None;
    }
    if matches!(first, "final" | "compact" | "merged" | "legacy") {
        let stamp = parts.get(1).copied().unwrap_or("0");
        return Some(format!("{first}-{stamp}"));
    }
    Some(first.to_string())
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

    /// A loopback port that accepts and immediately closes S3 requests, so a
    /// failed archive is fast and deterministic instead of waiting on httpfs's
    /// production timeout.
    fn refusing_s3_endpoint() -> (String, std::net::TcpListener) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        let incoming = listener.try_clone().unwrap();
        std::thread::spawn(move || {
            for stream in incoming.incoming() {
                drop(stream);
            }
        });
        (endpoint, listener)
    }

    fn disk_store(name: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ldx-pq-disk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(StoreConfig::new(&dir)).unwrap();
        (store, dir)
    }

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
    fn queries_see_hot_and_flushed_rows_together() {
        let (store, dir) = disk_store("hot-and-files");
        store
            .append(&report("checkout", 1_000_000_000, 20))
            .unwrap();
        assert!(store.flush_hot().unwrap() > 0);
        store
            .append(&report("checkout", 2_000_000_000, 22))
            .unwrap();

        assert_eq!(store.stats().batch_count, 2);
        assert_eq!(flame_total(&store), 42, "the VIEW must union both tiers");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn final_file_hides_superseded_fragments_after_a_crash() {
        let (store, dir) = disk_store("crash-fragments");
        store
            .append(&report("checkout", 1_000_000_000, 20))
            .unwrap();
        assert!(store.flush_hot().unwrap() > 0);
        store
            .append(&report("checkout", 2_000_000_000, 22))
            .unwrap();
        assert!(store.flush_hot().unwrap() > 0);

        let samples = dir.join("parquet").join("samples");
        let fragments: Vec<_> = std::fs::read_dir(&samples)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect();
        assert_eq!(fragments.len(), 2);
        let backup = dir.join("old-fragment.parquet");
        std::fs::copy(&fragments[0], &backup).unwrap();

        let mut merged = 0;
        for _ in 0..8 {
            let stats = store.compact_all_for_test().unwrap();
            if stats.files_written == 0 {
                break;
            }
            merged += stats.files_merged;
        }
        assert!(merged >= 2, "the hour should have been compacted");

        // Simulate a crash between publishing the final and deleting inputs:
        // an old fragment reappears with an older mtime. The read side must
        // ignore it instead of double-counting the hour.
        let resurrected = samples.join(fragments[0].file_name().unwrap());
        std::fs::copy(&backup, &resurrected).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&resurrected)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH)
            .unwrap();

        let cfg = StoreConfig::new(&dir);
        drop(store);
        let reopened = Store::open(cfg).unwrap();
        assert_eq!(
            flame_total(&reopened),
            42,
            "a stale fragment must stay hidden"
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
    fn failed_s3_archive_keeps_the_only_local_copy() {
        let dir = std::env::temp_dir().join(format!("ldx-pq-s3fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (endpoint, _listener) = refusing_s3_endpoint();
        let s3 = S3Config {
            bucket: "unused".into(),
            prefix: "test".into(),
            // The probe fails before any local file can be treated as confirmed.
            endpoint: Some(endpoint),
            use_ssl: Some(false),
            url_style: Some("path".into()),
            ..Default::default()
        };
        let cfg = StoreConfig {
            root: dir.clone(),
            storage_mode: StorageMode::S3,
            s3: Some(s3),
            parquet_local_ttl_ns: Some(0),
            ..StoreConfig::new(&dir)
        };
        let store = Store::open(cfg).unwrap();
        store
            .append(&report("checkout", 1_000_000_000, 42))
            .unwrap();
        assert!(store.flush_hot().unwrap() > 0);

        let stats = store.archive_local(u64::MAX).unwrap();
        assert!(stats.failed > 0, "the unreachable endpoint must fail");
        assert!(
            store.staged_bytes() > 0,
            "local staging is the only copy and must survive a failed upload"
        );
        assert_eq!(flame_total(&store), 42, "the staged copy stays queryable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn s3_staging_is_not_evicted_by_the_disk_cap() {
        let dir = std::env::temp_dir().join(format!("ldx-pq-s3cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (endpoint, _listener) = refusing_s3_endpoint();
        let s3 = S3Config {
            bucket: "unused".into(),
            prefix: "test".into(),
            endpoint: Some(endpoint),
            use_ssl: Some(false),
            url_style: Some("path".into()),
            ..Default::default()
        };
        let cfg = StoreConfig {
            root: dir.clone(),
            storage_mode: StorageMode::S3,
            s3: Some(s3),
            max_disk_bytes: Some(1),
            ..StoreConfig::new(&dir)
        };
        let store = Store::open(cfg).unwrap();
        store
            .append(&report("checkout", 1_000_000_000, 42))
            .unwrap();
        assert!(store.flush_hot().unwrap() > 0);

        store.prune(crate::now_ns()).unwrap();
        assert!(
            store.staged_bytes() > 0,
            "S3 staging is unconfirmed data and must never be pruned locally"
        );
        assert_eq!(flame_total(&store), 42);
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

        let state = ParquetState::new(&dir, StorageMode::Disk, None, u64::MAX);
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
