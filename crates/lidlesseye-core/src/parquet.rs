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
    /// Shard by the hour of the samples that reference the rows, not by a
    /// column of the row itself.
    ///
    /// `stacks` is a dictionary: it has no timestamp, yet a row is only ever
    /// referenced by samples from the same flush (stack ids are allocated per
    /// batch, not deduplicated across batches). Filing those rows under the hour
    /// of the samples that name them is therefore exact, and it is what turns
    /// the flamegraph's dictionary read from "every object since install" into
    /// "this hour's object". A global dictionary file made every flamegraph read
    /// 57 MB over S3 to render one hour of data.
    pub hour_from_samples: bool,
}

pub(crate) const TABLES: &[TableDef] = &[
    // Dictionaries first: a stack row must be durable before the sample that
    // names it, so a crash between the two writes loses nothing that a reader
    // would try to resolve.
    TableDef {
        name: "stacks",
        columns: "stack_id, frames",
        ddl: "CREATE TABLE {t} (stack_id BIGINT PRIMARY KEY, frames BLOB NOT NULL)",
        hour_column: None,
        hour_from_samples: true,
    },
    TableDef {
        name: "frames",
        columns: "frame_id, module, symbol, file, line",
        ddl: "CREATE TABLE {t} (frame_id INTEGER PRIMARY KEY, module VARCHAR NOT NULL, symbol VARCHAR NOT NULL, file VARCHAR NOT NULL, line INTEGER NOT NULL)",
        hour_column: None,
        hour_from_samples: false,
    },
    TableDef {
        name: "batches",
        columns: "batch_id, profile_type, service_name, labels, start_ns, end_ns, total_value, sample_count, bytes, created_ns",
        ddl: "CREATE TABLE {t} (batch_id BIGINT PRIMARY KEY, profile_type VARCHAR NOT NULL, service_name VARCHAR NOT NULL, labels VARCHAR NOT NULL, start_ns BIGINT NOT NULL, end_ns BIGINT NOT NULL, total_value BIGINT NOT NULL, sample_count BIGINT NOT NULL, bytes BIGINT NOT NULL, created_ns BIGINT NOT NULL)",
        hour_column: Some("start_ns"),
        hour_from_samples: false,
    },
    TableDef {
        name: "samples",
        columns: "batch_id, profile_type, service_name, labels, start_ns, end_ns, stack_id, value",
        ddl: "CREATE TABLE {t} (batch_id BIGINT NOT NULL, profile_type VARCHAR NOT NULL, service_name VARCHAR NOT NULL, labels VARCHAR NOT NULL, start_ns BIGINT NOT NULL, end_ns BIGINT NOT NULL, stack_id BIGINT NOT NULL, value BIGINT NOT NULL)",
        hour_column: Some("start_ns"),
        hour_from_samples: false,
    },
    TableDef {
        name: "net_nodes",
        columns: "bucket_ns, node_id, namespace, workload, service, kind, k8s_node, rx_bytes, tx_bytes, connections, retransmits, pods, ready, instances, containers",
        ddl: "CREATE TABLE {t} (bucket_ns BIGINT NOT NULL, node_id VARCHAR NOT NULL, namespace VARCHAR NOT NULL, workload VARCHAR NOT NULL, service VARCHAR NOT NULL, kind VARCHAR NOT NULL, k8s_node VARCHAR NOT NULL, rx_bytes BIGINT NOT NULL, tx_bytes BIGINT NOT NULL, connections BIGINT NOT NULL, retransmits BIGINT NOT NULL, pods BIGINT DEFAULT 0 NOT NULL, ready BIGINT DEFAULT 0 NOT NULL, instances VARCHAR DEFAULT '[]' NOT NULL, containers VARCHAR DEFAULT '[]' NOT NULL, PRIMARY KEY (bucket_ns, node_id))",
        hour_column: Some("bucket_ns"),
        hour_from_samples: false,
    },
    TableDef {
        name: "net_edges",
        columns: "bucket_ns, client_id, server_id, protocol, client_container, server_container, client_pod, server_pod, connections, retransmits, tx_bytes, rx_bytes, rtt_us_sum, rtt_samples, host, sql",
        ddl: "CREATE TABLE {t} (bucket_ns BIGINT NOT NULL, client_id VARCHAR NOT NULL, server_id VARCHAR NOT NULL, protocol VARCHAR NOT NULL, client_container VARCHAR DEFAULT '' NOT NULL, server_container VARCHAR DEFAULT '' NOT NULL, client_pod VARCHAR DEFAULT '' NOT NULL, server_pod VARCHAR DEFAULT '' NOT NULL, connections BIGINT NOT NULL, retransmits BIGINT NOT NULL, tx_bytes BIGINT NOT NULL, rx_bytes BIGINT NOT NULL, rtt_us_sum BIGINT NOT NULL, rtt_samples BIGINT NOT NULL, host VARCHAR DEFAULT '' NOT NULL, sql VARCHAR DEFAULT '' NOT NULL, PRIMARY KEY (bucket_ns, client_id, server_id, protocol, client_pod, server_pod, host, sql))",
        hour_column: Some("bucket_ns"),
        hour_from_samples: false,
    },
];

/// The Parquet files backing one table, grouped by the epoch hour of the data
/// they hold.
///
/// Grouping is what makes a query cheap. `read_parquet([...])` has to open the
/// footer of every path it is given — including files in S3, over the network
/// — to learn the schema and the row groups. A VIEW that lists the whole
/// retention window therefore pays for the entire history on every dashboard
/// poll, regardless of the requested range. Naming only the hours inside the
/// requested window turns that into one or two objects.
#[derive(Debug, Clone, Default)]
pub(crate) struct TableFiles {
    /// Epoch hour -> paths, oldest hour first.
    by_hour: std::collections::BTreeMap<u64, Vec<String>>,
    /// Files whose name carries no hour (one-off imports from older
    /// releases). They are mounted for *every* window: sharding must never
    /// hide data it cannot attribute to an hour.
    unknown: Vec<String>,
    /// At least one path comes from the pre-0.1.19 layout, where the hour was
    /// taken from the wall clock at flush/archive time rather than from the
    /// rows. Such a file can hold rows from the *previous* hour: the flush that
    /// straddled a boundary tagged 14:00 data that ended at 13:59:58. Windows
    /// over legacy files therefore grow by one hour on each side.
    legacy: bool,
}

impl TableFiles {
    fn push(&mut self, hour: Option<u64>, path: String) {
        self.legacy |= is_legacy_path(&path);
        match hour {
            Some(hour) => self.by_hour.entry(hour).or_default().push(path),
            None => self.unknown.push(path),
        }
    }

    fn extend(&mut self, hour: Option<u64>, paths: impl IntoIterator<Item = String>) {
        match hour {
            Some(hour) => {
                let bucket = self.by_hour.entry(hour).or_default();
                for path in paths {
                    self.legacy |= is_legacy_path(&path);
                    bucket.push(path);
                }
            }
            None => {
                for path in paths {
                    self.legacy |= is_legacy_path(&path);
                    self.unknown.push(path);
                }
            }
        }
    }

    /// Paths for the inclusive hour range `[lo, hi]`, plus every untagged file.
    /// Remove one path from every bucket.
    fn forget(&mut self, path: &Path) {
        let needle = path.to_string_lossy().replace('\\', "/");
        self.unknown.retain(|entry| entry != &needle);
        for files in self.by_hour.values_mut() {
            files.retain(|entry| entry != &needle);
        }
        self.by_hour.retain(|_, files| !files.is_empty());
    }

    fn window(&self, lo: u64, hi: u64) -> Vec<String> {
        let mut out = self.unknown.clone();
        if lo <= hi {
            for files in self.by_hour.range(lo..=hi).map(|(_, files)| files) {
                out.extend(files.iter().cloned());
            }
        }
        out
    }

    fn all(&self) -> Vec<String> {
        let mut out = self.unknown.clone();
        for files in self.by_hour.values() {
            out.extend(files.iter().cloned());
        }
        out
    }

    /// Paths that can hold the highest id in the table.
    ///
    /// Ids are allocated in arrival order and archived by hour, so the newest
    /// hour holds the newest ids. Untagged files are included because their
    /// contents cannot be placed in time. This is for `MAX(id)` at open time:
    /// opening every footer in a large archive just to find a counter costs
    /// tens of seconds per tenant.
    fn newest(&self) -> Vec<String> {
        let mut out = self.unknown.clone();
        if let Some((_, files)) = self.by_hour.last_key_value() {
            out.extend(files.iter().cloned());
        }
        out
    }
}

/// Whether a path predates the `hour=<n>` layout.
///
/// Current files say `hour=<n>` in a directory component (local) or carry the
/// `-part-` marker in a flat key (S3). Everything else — `hour=<n>.parquet`
/// and `h<n>-<stamp>.parquet` — was named after the wall clock at write time,
/// which is indistinguishable from a data hour by parsing alone.
fn is_legacy_path(path: &str) -> bool {
    let sharded = path.split('/').any(|segment| {
        // httpfs hands back URL-escaped keys, so `hour=7` can arrive as
        // `hour%3D7`. A legacy flat key is `hour=<n>.parquet`, which has a
        // suffix and is therefore not a partition *component*.
        let segment = segment.replace("%3D", "=");
        segment
            .strip_prefix("hour=")
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
    });
    if sharded {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.strip_prefix("hour=") {
        // `hour=<n>-part-<stamp>.parquet` is this release's archive key.
        Some(rest) => !rest.contains("-part-"),
        None => true,
    }
}

/// Local and remote file placement for every table, sharded by data hour.
///
/// This replaces the flat "explicit list of every object" manifest: the S3
/// bucket and the local staging directory remain the only durable state, and
/// the index is rebuilt at open. An on-open `glob()` per prefix re-discovers
/// archived objects (a fresh pod against an existing bucket must not hide
/// history), while local files are read straight off the directory tree.
#[derive(Debug, Clone, Default)]
pub(crate) struct FileIndex {
    local: std::collections::BTreeMap<String, TableFiles>,
    s3: std::collections::BTreeMap<String, TableFiles>,
}

impl FileIndex {
    fn s3_mut(&mut self, table: &str) -> &mut TableFiles {
        self.s3.entry(table.to_string()).or_default()
    }
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
    /// Rebuilt from the directory tree and one bucket glob on open; no
    /// on-disk catalogue.
    index: Arc<Mutex<FileIndex>>,
    /// When the next S3 archive attempt is allowed, after a failure.
    ///
    /// A failed upload has already paid for a whole-hour merge. Retrying every
    /// ten seconds turned a broken bucket into a permanent full-time scan,
    /// with every dashboard query queued behind it. The backoff is global
    /// because the failure is almost never about one hour: an unreachable
    /// endpoint, a bad credential and a refused multipart upload all fail
    /// every hour the same way. It doubles up to [`ARCHIVE_MAX_BACKOFF`].
    archive_retry: Arc<Mutex<ArchiveRetry>>,
    /// Serialises "publish a new file" against "rewrite an hour".
    ///
    /// A merge lists the files of an hour, copies them, then publishes the
    /// result and retires the inputs. A flush that lands inside that window
    /// writes a part the merge never read, and because that part is older than
    /// the published final the read-side "newest final wins" rule hides it —
    /// its rows would vanish from every query without anything failing. A
    /// shared lock on the write side and an exclusive lock on the merge side
    /// removes the window instead of narrowing it, which is what makes it safe
    /// to compact the *live* hour (the hour every dashboard asks for, and the
    /// one a ten-second flush cadence shreds into hundreds of files).
    merge_gate: Arc<parking_lot::RwLock<()>>,
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
            index: Arc::new(Mutex::new(FileIndex::default())),
            archive_retry: Arc::new(Mutex::new(ArchiveRetry::default())),
            merge_gate: Arc::new(parking_lot::RwLock::new(())),
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

    pub fn load_index(&self) -> FileIndex {
        self.index.lock().clone()
    }

    /// Drop one path from the local half of the in-memory index.
    ///
    /// Called before a retired file is moved out of the tree. The index is what
    /// every query builds its statement from, so removing the entry first means
    /// the worst case is a query that misses a row for one refresh — never a
    /// query that names a file which is already gone.
    fn forget_local_path(&self, path: &Path) {
        let mut index = self.index.lock();
        for files in index.local.values_mut() {
            files.forget(path);
        }
    }

    pub fn save_index(&self, index: &FileIndex) -> Result<()> {
        *self.index.lock() = index.clone();
        Ok(())
    }

    /// Replace the local half of the index with a fresh scan of the tree.
    fn reindex_local(&self, index: &mut FileIndex) {
        for def in TABLES {
            let files = scan_local_table(&self.table_dir(def.name));
            index.local.insert(def.name.to_string(), files);
        }
    }

    /// Re-scan the local tree and publish the result.
    ///
    /// A writer refreshes its index as part of every flush, so it always knows
    /// about the files it just wrote. A read-only replica never flushes, so
    /// without this it would answer from the file list captured when it opened —
    /// i.e. it would serve a snapshot that gets older every hour. This is the
    /// "short delay buffer" a query frontend needs: it sees what the writer has
    /// published, at most one scan interval late.
    pub fn refresh_local_index(&self) {
        let mut index = self.index.lock();
        let mut next = index.clone();
        self.reindex_local(&mut next);
        *index = next;
    }

    /// Clear the index. Also removes the `manifest.json` written by releases
    /// before the object index became in-memory only.
    pub fn clear_index(&self) -> Result<()> {
        *self.index.lock() = FileIndex::default();
        let legacy = self.dir.join("manifest.json");
        match std::fs::remove_file(legacy) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }
}

/// Epoch hour of a nanosecond timestamp.
pub(crate) fn hour_of(ns: u64) -> u64 {
    ns / 1_000_000_000 / 3600
}

/// Inclusive hour range covering `[from_ns, until_ns)`.
///
/// An empty or inverted window is widened to `[0, u64::MAX]` because callers
/// use `from = 0, until = u64::MAX` to mean "everything": returning no files
/// for that would silently report an empty tenant.
pub(crate) fn hour_window(from_ns: u64, until_ns: u64) -> (u64, u64) {
    if from_ns == 0 && until_ns == u64::MAX {
        return (0, u64::MAX);
    }
    let lo = hour_of(from_ns);
    // `until` is exclusive: a window ending exactly on an hour boundary must
    // not pull in the next hour's files.
    let hi = hour_of(until_ns.saturating_sub(1).max(from_ns));
    (lo, hi.max(lo))
}

/// The hour a file belongs to, from either layout.
///
/// Current releases write `<table>/hour=<n>/part-<stamp>-<rows>.parquet`; the
/// pre-0.1.19 layout was `<table>/h<n>-<stamp>-<rows>.parquet`. Both are read,
/// so upgrading needs no rewrite of the archive.
fn file_hour(path: &Path, name: &str) -> Option<u64> {
    for component in path.components().rev().skip(1) {
        let text = component.as_os_str().to_string_lossy();
        if let Some(value) = text.strip_prefix("hour=")
            && let Ok(hour) = value.parse()
        {
            return Some(hour);
        }
    }
    hour_from_name(name)
}

/// Hour encoded in a bare file name: `hour=<n>.parquet`, `hour=<n>-...`, or
/// the older `h<n>-...`.
fn hour_from_name(name: &str) -> Option<u64> {
    if let Some(rest) = name
        .strip_prefix("hour=")
        .or_else(|| name.strip_prefix("hour%3D"))
    {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(hour) = digits.parse() {
            return Some(hour);
        }
    }
    parse_hour_tag(name)
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
    // `http_timeout` is in **seconds**, not milliseconds. This used to be set
    // to `15000`, i.e. four hours: one half-open keep-alive connection could
    // then hold a reader for the whole retention window, and the query
    // watchdog's `interrupt()` cannot break a thread parked in `recv()`.
    // Fifteen seconds is longer than any healthy object read and short enough
    // that a wedged socket surfaces as a retryable error instead of a hang.
    // Best-effort: an older httpfs may not expose these.
    let _ = conn.execute_batch("SET http_timeout=15");
    let _ = conn.execute_batch("SET http_retries=2");
    // Metadata caching is off by default in DuckDB. Without it every query
    // re-reads the footer (and the HEAD) of every Parquet object it touches —
    // the same object was observed being fetched 151 times in one minute. Both
    // caches are keyed by path, so they survive across queries and make a
    // repeated scan of the same files nearly free.
    let _ = conn.execute_batch("SET enable_http_metadata_cache=true");
    let _ = conn.execute_batch("SET parquet_metadata_cache=true");
    Ok(())
}

/// Create the hot tables and rebuild the local file index for a Parquet store.
///
/// There are deliberately no canonical VIEWs. A VIEW has to name every file to
/// be useful, and that set changes on every flush, so it costs a DDL on the
/// catalog each time — which serialises against every concurrent read. Read
/// paths instead build their `FROM` clause per query from [`table_span`], so a
/// query names exactly the hours it needs and nothing has to be rebuilt.
pub(crate) fn create_schema(conn: &Connection, state: &ParquetState) -> Result<()> {
    std::fs::create_dir_all(&state.dir).map_err(Error::Io)?;
    // A killed flush/merge leaves `.tmp` files that nothing references. They
    // are inert, but they would count against the disk cap forever, so clean
    // them up on every open. An old `manifest.json` is likewise obsolete now
    // that the object index lives only in memory.
    let _ = state.clear_index();
    for def in TABLES {
        remove_tmp_files(&state.table_dir(def.name));
    }
    for def in TABLES {
        let ddl = def.ddl.replace("{t}", &format!("{}_hot", def.name));
        conn.execute_batch(&ddl)
            .map_err(|e| Error::Db(format!("creating hot table {}: {e}", def.name)))?;
        std::fs::create_dir_all(state.table_dir(def.name)).map_err(Error::Io)?;
    }
    // DuckDB does not support `ADD COLUMN IF NOT EXISTS` with a NOT NULL
    // default, so ask the catalogue which columns are already there.
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
    reindex_local(state);
    Ok(())
}

/// Remove leftover `.tmp` files from a killed write, recursively.
fn remove_tmp_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            remove_tmp_files(&path);
        } else if path.to_string_lossy().ends_with(".tmp") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Best-effort rebuild of the remote half of the file index at store-open time.
///
/// The bucket is the source of truth for archived objects. If the in-memory
/// index is lost (a new pod, a rebuilt PVC) we would otherwise hide every
/// archived hour, so a `glob()` over the tenant prefix re-discovers them.
/// Query binding never issues a LIST: it names files from the index.
///
/// One pattern, not one per table: a LIST against an object store costs a
/// round trip, and this ran twelve of them per tenant at every start. Six times
/// fewer round trips is the difference between a ready pod and one the startup
/// probe kills. `*` per level rather than `**` for the same reason — a glob is
/// expanded one directory at a time, so a recursive pattern over an archive
/// with an `hour=<n>/` directory per hour is one LIST per hour.
///
/// Returns the number of newly discovered objects. Any error is swallowed by
/// the caller so an unreachable/misconfigured S3 never blocks startup.
pub(crate) fn discover_s3_objects(
    conn: &Connection,
    state: &ParquetState,
    s3: &S3Config,
) -> Result<usize> {
    let base = state.s3_base(); // s3://bucket[/prefix]
    let mut old = state.load_index();
    // The bucket, not the cached index, is the source of truth. Rebuild the
    // remote half from the LIST result: appending to the old one kept objects
    // that retention had already deleted, and a query naming one of those
    // failed the whole panel with a 404 instead of skipping a stale row.
    // Local files stay untouched — they are scanned from the directory tree.
    let mut discovered: std::collections::BTreeMap<String, TableFiles> =
        std::collections::BTreeMap::new();
    let mut found = 0usize;

    // Every layout this reads is flat under the table:
    // `<table>/hour=<n>-part-<stamp>.parquet` (current),
    // `<table>/hour=<n>.parquet` (0.1.16–0.1.18 archive) and
    // `<table>/h<n>-*.parquet` (a pre-0.1.19 local tree copied into a bucket).
    // One pattern covers all three: a LIST for the prefix, one per table.
    let pattern = format!("{base}/*/*.parquet");
    {
        // DuckDB's glob() returns a `file` column of fully-qualified s3://
        // URIs (URL-escaped keys). Bind the pattern so a quote in a prefix
        // can never break the SQL.
        let mut stmt = conn
            .prepare("SELECT file FROM glob(?)")
            .map_err(|e| Error::Db(format!("preparing S3 discovery for {pattern}: {e}")))?;
        let rows = stmt
            .query_map([pattern.as_str()], |r| r.get::<_, String>(0))
            .map_err(|e| Error::Db(format!("listing S3 objects for {pattern}: {e}")))?;
        for row in rows.flatten() {
            let uri = normalize_s3_uri(&row, s3);
            let Some(table) = table_of_key(&uri, &base) else {
                continue;
            };
            let hour = hour_from_key(&uri);
            discovered
                .entry(table.to_string())
                .or_default()
                .push(hour, uri);
            found += 1;
        }
    }

    // A LIST that returns nothing must not be treated as proof that the bucket
    // was emptied: a transient S3 outage must keep the last known index rather
    // than hide the whole archive. Any non-empty result is authoritative and
    // replaces the remote half, including entries that have since vanished.
    if found > 0 {
        for files in discovered.values_mut() {
            for group in files.by_hour.values_mut() {
                group.sort();
            }
            files.unknown.sort();
        }
        old.s3 = discovered;
        state.save_index(&old)?;
    }
    Ok(found)
}

/// The table an object belongs to: the first key segment after the tenant
/// prefix, which is the same position in both layouts.
fn table_of_key<'a>(uri: &'a str, base: &str) -> Option<&'a str> {
    let rest = uri.strip_prefix(base)?.trim_start_matches('/');
    let table = rest.split('/').next()?;
    TABLES
        .iter()
        .find(|def| def.name == table)
        .map(|def| def.name)
}

/// The hour an S3 key belongs to, from either layout.
fn hour_from_key(key: &str) -> Option<u64> {
    for segment in key.split('/') {
        // httpfs returns URL-escaped keys, so `hour=7` may arrive as
        // `hour%3D7`.
        let segment = segment.replace("%3D", "=");
        if let Some(value) = segment.strip_prefix("hour=")
            && let Ok(hour) = value.parse::<u64>()
        {
            return Some(hour);
        }
    }
    // Old flat layout: the *file* is named `hour=<n>.parquet`.
    hour_from_name(key.rsplit('/').next().unwrap_or(key))
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
    name.contains("final") || name.contains("compact") || name.contains("merged")
}

/// One file found on local disk, with everything the layout rules need.
struct LocalFile {
    path: PathBuf,
    modified: SystemTime,
    final_file: bool,
    hour: Option<u64>,
    size: u64,
    /// Flush/merge stamp, which ties one file per table into one logical
    /// write. Files from earlier layouts have none.
    stamp: Option<String>,
}

/// Recursively collect Parquet files under one table directory. The tree is
/// at most two levels deep (`<table>/hour=<n>/<file>`), but a bounded walk
/// also copes with any nesting a future layout introduces.
fn collect_local_files(dir: &Path, depth: usize, out: &mut Vec<LocalFile>) {
    const MAX_DEPTH: usize = 4;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_local_files(&path, depth + 1, out);
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // `.tmp` files are partial writes and must never be published.
        if !name.ends_with(".parquet") {
            continue;
        }
        let meta = entry.metadata().ok();
        out.push(LocalFile {
            modified: meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .unwrap_or(UNIX_EPOCH),
            size: meta.map(|m| m.len()).unwrap_or(0),
            final_file: is_final_file(&name),
            hour: file_hour(&path, &name),
            stamp: parse_stamp_tag(&name),
            path,
        });
    }
}

/// Every Parquet file of one table, wherever in the tree it lives.
fn local_files(state: &ParquetState, table: &str) -> Vec<LocalFile> {
    let mut out = Vec::new();
    collect_local_files(&state.table_dir(table), 0, &mut out);
    out
}

/// Scan one table's local directory and group its files by data hour.
///
/// Within an hour, the newest file whose name marks it as a merged final
/// supersedes everything older. The merge deletes its inputs after publishing,
/// so a crash between the two steps would otherwise let the read path count
/// every row twice; applying the rule here makes recovery idempotent whether or
/// not the deleting pass ever ran.
fn scan_local_table(dir: &Path) -> TableFiles {
    let mut entries = Vec::new();
    collect_local_files(dir, 0, &mut entries);
    let mut by_hour: std::collections::BTreeMap<u64, Vec<(PathBuf, SystemTime, bool)>> =
        std::collections::BTreeMap::new();
    let mut files = TableFiles::default();
    for file in entries {
        match file.hour {
            Some(hour) => {
                by_hour
                    .entry(hour)
                    .or_default()
                    .push((file.path, file.modified, file.final_file))
            }
            None => files.push(None, file.path.to_string_lossy().replace('\\', "/")),
        }
    }
    for (hour, mut group) in by_hour {
        let newest_final = group
            .iter()
            .filter(|(_, _, is_final)| *is_final)
            .map(|(_, modified, _)| *modified)
            .max();
        if let Some(modified) = newest_final {
            group.retain(|(_, at, _)| *at >= modified);
        }
        group.sort_by(|(a, ..), (b, ..)| a.cmp(b));
        files.extend(
            Some(hour),
            group
                .into_iter()
                .map(|(path, ..)| path.to_string_lossy().replace('\\', "/")),
        );
    }
    files
}

/// Rescan the local tree into the index. Cheap (a few `read_dir` calls) and
/// called after every flush/merge/removal, so the hot query path never has to
/// touch the filesystem beyond the window it is asked for.
pub(crate) fn reindex_local(state: &ParquetState) {
    let mut index = state.load_index();
    state.reindex_local(&mut index);
    // Saving cannot fail (it is an in-memory swap); ignore the Result shape.
    let _ = state.save_index(&index);
}

/// The oldest hour a dictionary was sharded into, if it ever was.
///
/// `hour=0` holds dictionary rows written before sharding, which any sample can
/// name. Once a sharded file exists, a window that begins after that hour no
/// longer needs the unsharded ones.
fn first_sharded_hour(index: &FileIndex, table: &str) -> Option<u64> {
    [index.local.get(table), index.s3.get(table)]
        .into_iter()
        .flatten()
        .filter_map(|files| files.by_hour.range(1..).next().map(|(hour, _)| *hour))
        .min()
}

/// SQL for the relation a query should read for `table`, restricted to the
/// hours that can possibly hold data in `[from_ns, until_ns)`.
///
/// The result is a parenthesised subquery — `SELECT * FROM {span}`, `{span} s
/// JOIN ...` — built from the hot tier plus only the matching Parquet files.
/// Naming the files explicitly keeps DuckDB from listing a bucket, and the hour
/// range keeps it from opening the footer of every object in the retention
/// window just to discover that none of them overlap the query.
///
/// `union_by_name` is required, not cosmetic: an upgrade adds columns, and the
/// archive still holds files written before they existed. A positional union
/// would make the binder reject those files and every query that touches them.
pub(crate) fn table_span(state: &ParquetState, table: &str, from_ns: u64, until_ns: u64) -> String {
    let index = state.load_index();
    let def = TABLES.iter().find(|def| def.name == table);
    let sharded = def
        .map(|def| def.hour_column.is_some() || def.hour_from_samples)
        .unwrap_or(false);
    // A dictionary sharded by the samples that reference it has no time column
    // of its own, but its files are still named by an hour, so a window can
    // name only the hours it needs — that is the whole point of sharding it.
    let by_samples = def.map(|def| def.hour_from_samples).unwrap_or(false);
    let (lo, hi) = if sharded {
        hour_window(from_ns, until_ns)
    } else {
        // Dictionaries have no time column; they are always mounted whole.
        (0, u64::MAX)
    };
    // Files written before dictionaries were sharded sit in `hour=0` and can be
    // named by a sample from any hour. Mount them until the window is entirely
    // past the hour the first sharded file was written: from then on, every
    // sample's dictionary rows live in the same hour as the sample itself.
    let switch = if by_samples {
        first_sharded_hour(&index, table)
    } else {
        None
    };
    let mut paths: Vec<String> = Vec::new();
    for files in [index.local.get(table), index.s3.get(table)]
        .into_iter()
        .flatten()
    {
        if !sharded {
            paths.extend(files.all());
            continue;
        }
        // A pre-0.1.19 file was tagged with the wall clock at write time, so
        // the rows it holds may belong to the hour before. Widen by one hour on
        // each side while such a file is still in the table: the cost is two
        // more objects, and the alternative is silently hiding rows exactly at
        // the hour boundary after an upgrade. New files are sharded by the data
        // hour, so this ends with the last legacy object's retention.
        let (lo, hi) = if files.legacy {
            (lo.saturating_sub(1), hi.saturating_add(1))
        } else {
            (lo, hi)
        };
        paths.extend(files.window(lo, hi));
        let needs_unsharded = match switch {
            Some(switch) => lo <= switch,
            None => true,
        };
        if by_samples && needs_unsharded {
            paths.extend(files.by_hour.get(&0).cloned().unwrap_or_default());
        }
    }
    // A local file can be retired between the moment the index named it and the
    // moment this statement runs — a merge or an upload finishing while a
    // dashboard refreshes. Naming a file that is already gone fails the whole
    // panel with "Cannot open file", so the ones that no longer exist are
    // dropped here. Remote objects are left alone: they cannot be `stat`ed, and
    // the archive drops them from the index before deleting the local copy.
    paths.retain(|path| !path.starts_with('/') || std::path::Path::new(path).exists());
    if paths.is_empty() {
        return format!("{table}_hot");
    }
    paths.sort();
    paths.dedup();
    let list = paths
        .iter()
        .map(|p| format!("'{}'", esc(p)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "(SELECT * FROM {table}_hot \
         UNION ALL BY NAME \
         SELECT * FROM read_parquet([{list}], hive_partitioning=false, union_by_name=true))"
    )
}

/// [`table_span`] over the whole dataset, for catalogue-wide aggregates that
/// have no time window of their own.
pub(crate) fn table_span_all(state: &ParquetState, table: &str) -> String {
    table_span(state, table, 0, u64::MAX)
}

/// Relation for finding the highest id in `table` at open time.
///
/// Ids are monotonic and archived by hour, so the highest one lives in the
/// newest shard (or still in the hot tier). Opening every Parquet footer in a
/// tenant with hundreds of objects just to evaluate `MAX(id)` dominated cold
/// starts; naming the newest shard keeps the counter correct while touching a
/// handful of files. Untagged files are included because they cannot be placed
/// in time, and dictionaries that live entirely in `hour=0` get that directory.
pub(crate) fn table_span_newest(state: &ParquetState, table: &str) -> String {
    let index = state.load_index();
    let mut paths: Vec<String> = Vec::new();
    for files in [index.local.get(table), index.s3.get(table)]
        .into_iter()
        .flatten()
    {
        paths.extend(files.newest());
    }
    if paths.is_empty() {
        return format!("{table}_hot");
    }
    paths.sort();
    paths.dedup();
    let list = paths
        .iter()
        .map(|p| format!("'{}'", esc(p)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "(SELECT * FROM {table}_hot \
         UNION ALL BY NAME \
         SELECT * FROM read_parquet([{list}], hive_partitioning=false, union_by_name=true))"
    )
}

/// How many Parquet objects a window would name, for diagnostics and tests.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn window_file_count(
    state: &ParquetState,
    table: &str,
    from_ns: u64,
    until_ns: u64,
) -> usize {
    let index = state.load_index();
    let (lo, hi) = hour_window(from_ns, until_ns);
    let mut total = 0usize;
    for files in [index.local.get(table), index.s3.get(table)]
        .into_iter()
        .flatten()
    {
        total += files.window(lo, hi).len();
    }
    total
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

/// Rows one flush job may move before it hands the writer back.
///
/// Ingest and flushing share one writer connection, so the size of one flush
/// decides how long a report waits. A fraction of a second of `COPY` keeps the
/// transient tier small; an all-or-nothing flush of a backlog holds the writer
/// for minutes, and every report queued behind it fails its timeout.
///
/// The budget is counted in **samples**, not batches. A batch is one report and
/// a report carries many samples, so a budget of N batches is N × (samples per
/// report) rows: 200 000 batches was over ten million samples, which is not a
/// slice but minutes of `COPY`. Counting the table that dominates the cost is
/// what makes the slice actually bounded.
pub(crate) const FLUSH_CHUNK_ROWS: usize = 250_000;

/// Copy every row currently in the hot tables into one Parquet file per table
/// and hour, then clear them.
///
/// The caller owns the writer connection, so no reader observes a partially
/// flushed state.
pub(crate) fn flush_hot(conn: &Connection, state: &ParquetState) -> Result<usize> {
    flush_tables(conn, state, None, None)
}

/// Move at most `budget` rows out of the hot tier, oldest hours first.
///
/// A flush used to be all-or-nothing: it copied every hot row and cleared the
/// tables, which meant it needed the writer for the whole archive at once. On a
/// tenant taking a steady stream of reports that window never arrived — ingest
/// always had the writer — so the tier grew to millions of rows and every query
/// had to scan them. This moves the oldest hours that fit the budget instead:
/// one pass is a fraction of a second, the writer comes straight back, and the
/// backlog drains a slice per tick.
///
/// `stacks`/`frames` deliberately stay in the hot tier while a backlog remains.
/// They are dictionaries keyed by id, so deduplication bounds them, and
/// deleting one while a published Parquet sample still names it would lose a
/// symbol. The final, untruncated pass flushes them whole.
pub(crate) fn flush_hot_bounded(
    conn: &Connection,
    state: &ParquetState,
    budget: usize,
) -> Result<usize> {
    let budget = budget.max(1) as i64;
    // `samples` is the table every query scans, and it is one to two orders of
    // magnitude larger than `batches`. Measuring the budget on batches made a
    // "slice" ten hours wide and held the writer for the whole of it, which is
    // exactly the failure the bounded flush exists to prevent — so gate and
    // slice on the expensive table.
    let samples = hot_rows(conn, "samples");
    let batches = hot_rows(conn, "batches");
    if samples <= budget && batches <= budget {
        return flush_hot(conn, state);
    }
    // Plan the slice on whichever table is actually over budget. `samples` is
    // normally the one, but a tier can hold batches whose samples already went
    // to Parquet (or a profile with no samples at all). Asking an empty table
    // for its hours yields no hours, and the slice then moves nothing — a
    // backlog that never drains while every pass reports success.
    let plan = if samples > budget {
        "samples"
    } else {
        "batches"
    };
    // `batches` and `samples` move together: one hour of one without the other
    // would leave a Parquet sample whose batch row is still hot, or a batch
    // whose samples are still hot, and neither is a state a query expects.
    let hours = oldest_hours(conn, plan, budget)?;
    // `stacks` rides along: its rows are filed under the hour of the samples
    // that name them, so a slice has to publish the dictionary rows for the
    // same hours it publishes samples for.
    let mut moved = flush_tables(
        conn,
        state,
        Some(&["stacks", "batches", "samples"]),
        Some(&hours),
    )?;
    // The map tables carry no batches and have their own clock, so they get a
    // budget of their own instead of waiting for the report backlog to clear.
    for name in ["net_nodes", "net_edges"] {
        moved += flush_table_bounded(conn, state, name, budget)?;
    }
    Ok(moved)
}

/// Rows in one table's hot tier. A missing table is zero, not an error: the
/// hot tables are created on open, and a partially migrated store may not
/// have all of them yet.
fn hot_rows(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}_hot"), [], |row| {
        row.get::<_, i64>(0)
    })
    .unwrap_or(0)
}

/// The oldest hours of `table` whose rows fit in `budget`, never fewer than one.
///
/// Grouping in SQL rather than fetching rows keeps this to one scan of a column
/// and returns at most one row per archived hour.
fn oldest_hours(conn: &Connection, table: &str, budget: i64) -> Result<Vec<i64>> {
    let column = TABLES
        .iter()
        .find(|def| def.name == table)
        .and_then(|def| def.hour_column)
        .ok_or_else(|| Error::InvalidArgument(format!("{table} has no hour column")))?;
    let sql = format!(
        "SELECT ({column} // 1_000_000_000 // 3600) AS h, COUNT(*) AS n \
         FROM {table}_hot GROUP BY 1 ORDER BY 1"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| Error::Db(format!("listing {table} hours: {e}")))?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
        .map_err(|e| Error::Db(format!("listing {table} hours: {e}")))?;
    let mut picked = Vec::new();
    let mut total = 0i64;
    for row in rows.flatten() {
        picked.push(row.0);
        total += row.1;
        if total >= budget {
            break;
        }
    }
    Ok(picked)
}

/// [`flush_tables`] for one table, truncated to `budget` rows if it is over.
fn flush_table_bounded(
    conn: &Connection,
    state: &ParquetState,
    name: &str,
    budget: i64,
) -> Result<usize> {
    if hot_rows(conn, name) <= budget {
        return flush_tables(conn, state, Some(&[name]), None);
    }
    let hours = oldest_hours(conn, name, budget)?;
    flush_tables(conn, state, Some(&[name]), Some(&hours))
}

/// Flush `only` (or every table) restricted to `hours` (or every hour present).
fn flush_tables(
    conn: &Connection,
    state: &ParquetState,
    only: Option<&[&str]>,
    hours: Option<&[i64]>,
) -> Result<usize> {
    let mut moved = 0usize;
    // One stamp for the whole pass: the files written for every table belong to
    // the same logical generation. Retention and the disk cap evict by stamp,
    // so per-table stamps could delete `batches` while leaving their `samples`
    // (or vice versa), breaking referential integrity.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // The hour plan for dictionary-sharded tables has to be taken *before* the
    // samples leave the hot tier, because that is the only place the mapping
    // from a stack id to an hour exists.
    let sample_hours: Vec<i64> = if TABLES.iter().any(|def| def.hour_from_samples) {
        match hours {
            Some(hours) => hours.to_vec(),
            None => all_hours(conn, "samples")?,
        }
    } else {
        Vec::new()
    };
    for def in TABLES {
        if let Some(only) = only
            && !only.contains(&def.name)
        {
            continue;
        }
        moved += flush_one(conn, state, def, hours, &sample_hours, stamp)?;
    }
    if moved > 0 {
        reindex_local(state);
    }
    Ok(moved)
}

/// One table, one pass.
///
/// Partition by the data's own timestamp, not the wall clock at flush time:
/// backfilled or delayed reports must land in the hour they describe, or
/// retention and the disk cap evict the wrong rows.
fn flush_one(
    conn: &Connection,
    state: &ParquetState,
    def: &TableDef,
    only: Option<&[i64]>,
    sample_hours: &[i64],
    stamp: u128,
) -> Result<usize> {
    let hot = format!("{}_hot", def.name);
    // A dictionary-sharded table owns no time column: its files are named after
    // the hour of the samples that reference the rows. A slice writes the same
    // hours it writes samples for, and the rows stay hot until nothing hot
    // references them any more.
    if def.hour_from_samples {
        return flush_by_sample_hours(conn, state, def, &hot, sample_hours, only, stamp);
    }
    let mut hours: Vec<i64> = Vec::new();
    match (def.hour_column, only) {
        (Some(column), Some(only)) => {
            // `//` — integer division. `/` in DuckDB is floating point, and
            // `CAST(double AS BIGINT)` rounds to nearest, so an event at 13:43
            // became hour 14 and every query whose window ended at "now" missed
            // it. Half of every hour's data was filed under the next hour.
            let sql = format!(
                "SELECT DISTINCT ({column} // 1_000_000_000 // 3600) FROM {hot} \
                 WHERE ({column} // 1_000_000_000 // 3600) IN ({})",
                hour_list(only)
            );
            collect_hours(conn, &sql, &hot, &mut hours)?;
        }
        (Some(column), None) => {
            let sql = format!("SELECT DISTINCT ({column} // 1_000_000_000 // 3600) FROM {hot}");
            collect_hours(conn, &sql, &hot, &mut hours)?;
        }
        (None, Some(_)) => {
            // A dictionary cannot be sliced by hour. Refusing here is what keeps
            // a truncated pass from deleting a frame that a Parquet sample still
            // references.
            return Ok(0);
        }
        (None, _) => {
            // Dictionaries have no time column, so they cannot be sharded by
            // data hour. They all go into `hour=0`: every scan mounts that
            // single directory, and compaction can merge its files. Sharding
            // them by wall-clock hour instead (the previous behaviour) would
            // add a handful of tiny files every flush and make the *unsharded*
            // dictionary scan grow without bound.
            hours.push(0);
        }
    }
    hours.sort_unstable();

    let mut moved = 0usize;
    for hour in &hours {
        let hour = *hour;
        let filter = def
            .hour_column
            .map(|column| format!("WHERE ({column} // 1_000_000_000 // 3600) = {hour}"))
            .unwrap_or_default();
        moved += write_hour_file(conn, state, def, &hot, hour, &filter, stamp)?;
    }
    if moved == 0 {
        return Ok(0);
    }
    // Clear exactly what was written, after every hour's file is published.
    let filter = match (def.hour_column, only) {
        (Some(column), Some(_)) => format!(
            "WHERE ({column} // 1_000_000_000 // 3600) IN ({})",
            hour_list(&hours)
        ),
        _ => String::new(),
    };
    if filter.is_empty() {
        // The whole table is being drained. Replacing it beats deleting its
        // rows — see [`reset_hot`].
        reset_hot(conn, def)?;
    } else {
        conn.execute_batch(&format!("DELETE FROM {hot} {filter}"))
            .map_err(|e| Error::Db(format!("clearing hot {}: {e}", def.name)))?;
    }
    Ok(moved)
}

/// Empty a hot table by replacing it, never by deleting its live rows.
///
/// DuckDB keeps a deleted row as a tombstone inside its row group until a
/// checkpoint rewrites that group. The hot tier is an in-memory database, and an
/// in-memory database never checkpoints, so `DELETE FROM {t}_hot` left one
/// tombstone per ingested row for the lifetime of the process. Every read mounts
/// `(SELECT * FROM {t}_hot UNION ALL BY NAME SELECT * FROM read_parquet(…))`,
/// so after an hour of ingest the scans walked more dead rows than live ones and
/// each query got slower the longer the server had been up. Dropping the table
/// frees the storage outright; recreating it from the same DDL `create_schema`
/// uses is a catalogue operation, not a copy.
fn reset_hot(conn: &Connection, def: &TableDef) -> Result<()> {
    let hot = format!("{}_hot", def.name);
    let ddl = def.ddl.replace("{t}", &hot);
    // One transaction, so a reader that prepares a statement in between never
    // sees the table missing. DuckDB invalidates a prepared statement whose
    // table changed underneath it, and that is a retryable error — the reads
    // that matter here already retry.
    conn.execute_batch(&format!("BEGIN TRANSACTION; DROP TABLE IF EXISTS {hot}; {ddl}; COMMIT;"))
        .map_err(|e| Error::Db(format!("clearing hot {}: {e}", def.name)))?;
    Ok(())
}

/// Write the rows matching `filter` into `hour=<hour>/` and return how many.
///
/// Sharding on disk by the hour the rows describe is what lets a query name
/// only the hours it asks for.
fn write_hour_file(
    conn: &Connection,
    state: &ParquetState,
    def: &TableDef,
    hot: &str,
    hour: i64,
    filter: &str,
    stamp: u128,
) -> Result<usize> {
    // Held for the whole write so a merge of the live hour can never list the
    // directory, copy it, and publish a final that predates this part — see
    // [`ParquetState::merge_gate`].
    let _gate = state.merge_gate.read();
    let count: i64 = conn
        .query_row(&format!("SELECT COUNT(*) FROM {hot} {filter}"), [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    if count <= 0 {
        return Ok(0);
    }
    let dir = state.table_dir(def.name).join(format!("hour={hour}"));
    std::fs::create_dir_all(&dir).map_err(Error::Io)?;
    let name = format!("part-{stamp}-{count}.parquet");
    let file = dir.join(&name);
    let tmp = dir.join(format!("{name}.tmp"));
    let sql = format!(
        "COPY (SELECT {cols} FROM {hot} {filter}) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 122880)",
        tmp.to_string_lossy().replace('\\', "/"),
        cols = def.columns,
    );
    conn.execute_batch(&sql)
        .map_err(|e| Error::Db(format!("flushing {}: {e}", def.name)))?;
    // Publish only a file we have read back. A partial COPY is then a harmless
    // `.tmp` that startup recovery removes, never a VIEW entry.
    //
    // The row count comes from the Parquet footer, not from a second scan of
    // the data: `COUNT(*)` over `read_parquet` re-reads every column it was
    // just given, which doubled the write cost of every flush and held the
    // tenant's only write connection for seconds. A footer read still proves
    // the file is complete and parseable — the failure this guards against, a
    // COPY that did not finish, leaves no readable footer at all.
    let path = tmp.to_string_lossy().replace('\\', "/");
    let rows: i64 = conn
        .query_row(
            &format!("SELECT sum(num_rows) FROM parquet_file_metadata('{path}')"),
            [],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map_err(|e| Error::Db(format!("flushing {}: {e}", def.name)))?
        .unwrap_or(0);
    // More rows than the count means the hot table grew between the count and
    // the COPY — a converter appending while this flush runs — and that file is
    // still a consistent snapshot of everything up to that point. Only fewer
    // rows mean the COPY itself lost data, which is the case worth parking a
    // segment for. Insisting on equality turned that benign race into a parked
    // segment and a retry that rewrote the same rows.
    if rows < count {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Db(format!(
            "flushing {}: wrote {rows} rows, expected {count}",
            def.name
        )));
    }
    std::fs::rename(&tmp, &file).map_err(Error::Io)?;
    Ok(count.max(0) as usize)
}

/// Write a dictionary's rows into the hours of the samples that name them.
///
/// Only the rows a given hour's samples reference go into that hour's file:
/// writing the whole dictionary into every hour would multiply the archive by
/// the number of archived hours. Rows are cleared from the hot tier only when
/// the pass is draining everything (`only` is `None`), because a slice leaves
/// later hours still needing to resolve the same ids.
fn flush_by_sample_hours(
    conn: &Connection,
    state: &ParquetState,
    def: &TableDef,
    hot: &str,
    sample_hours: &[i64],
    only: Option<&[i64]>,
    stamp: u128,
) -> Result<usize> {
    let draining = only.is_none();
    if sample_hours.is_empty() {
        // Nothing hot references these rows: either they were published by an
        // earlier slice, or the store has no samples at all.
        if draining && hot_rows(conn, def.name) > 0 {
            reset_hot(conn, def)?;
        }
        return Ok(0);
    }
    let mut moved = 0usize;
    for hour in sample_hours {
        // Stack ids are allocated per batch rather than deduplicated across
        // batches, so a stack row is only ever named by samples from the same
        // flush generation. That is what makes this filter exact.
        let filter = format!(
            "WHERE stack_id IN (SELECT DISTINCT stack_id FROM samples_hot \
             WHERE (start_ns // 1_000_000_000 // 3600) = {hour})"
        );
        moved += write_hour_file(conn, state, def, hot, *hour, &filter, stamp)?;
    }
    if draining && hot_rows(conn, def.name) > 0 {
        reset_hot(conn, def)?;
    }
    Ok(moved)
}

/// Every hour present in a table's hot tier.
fn all_hours(conn: &Connection, table: &str) -> Result<Vec<i64>> {
    let column = TABLES
        .iter()
        .find(|def| def.name == table)
        .and_then(|def| def.hour_column)
        .ok_or_else(|| Error::InvalidArgument(format!("{table} has no hour column")))?;
    let mut hours = Vec::new();
    let sql = format!("SELECT DISTINCT ({column} // 1_000_000_000 // 3600) FROM {table}_hot");
    collect_hours(conn, &sql, &format!("{table}_hot"), &mut hours)?;
    hours.sort_unstable();
    Ok(hours)
}

fn hour_list(hours: &[i64]) -> String {
    hours
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn collect_hours(conn: &Connection, sql: &str, hot: &str, out: &mut Vec<i64>) -> Result<()> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| Error::Db(format!("listing {hot} hours: {e}")))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(|e| Error::Db(format!("listing {hot} hours: {e}")))?;
    for row in rows.flatten() {
        out.push(row);
    }
    Ok(())
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
    let mut index = state.load_index();
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
        // Read the tree rather than the index: the index is a read-side cache
        // and may be a tick behind a just-published local file.
        let local = scan_local_table(&state.table_dir(def.name));
        let mut by_hour: Vec<(u64, Vec<String>)> = local
            .by_hour
            .into_iter()
            .filter(|(hour, files)| *hour < cutoff_hour && !files.is_empty())
            .collect();
        by_hour.sort_by_key(|(hour, _)| *hour);

        for (hour, files) in by_hour {
            // Merge into one large Parquet object directly on S3. DuckDB's
            // httpfs streams the upload; nothing is staged a second time.
            let list = files
                .iter()
                .map(|p| format!("'{}'", esc(p)))
                .collect::<Vec<_>>()
                .join(", ");
            // Flat key with the hour in the name, not an `hour=<n>/` prefix.
            //
            // The hour still has to be in the key, so a windowed query can name
            // the archived hours it needs instead of every object in the
            // bucket. What must *not* be there is a directory level: a `glob()`
            // resolves one directory at a time, so `<table>/hour=<n>/<file>`
            // cost one LIST per archived hour at start-up. With a flat key the
            // whole tenant prefix is six LISTs, which is the difference between
            // a ready pod and one the startup probe kills.
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let key_url = format!(
                "{}/{}/hour={}-part-{stamp}.parquet",
                state.s3_base(),
                def.name,
                hour
            );
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
            // the two leaves a harmless extra local file, never a lost object;
            // and a crash before the index is saved is repaired by the
            // open-time glob.
            index.s3_mut(def.name).push(Some(hour), key_url.clone());

            for file in &files {
                if let Ok(meta) = std::fs::metadata(file) {
                    stats.bytes_reclaimed += meta.len() as usize;
                }
                retire_file(state, Path::new(file));
                stats.files_uploaded += 1;
            }
        }
    }
    // Rescan the local half *before* publishing it: the files this pass
    // retired are gone from the tree, and saving the pre-retire snapshot would
    // put them back in the index for a moment — long enough for a query to
    // build a statement around a path that no longer exists.
    state.reindex_local(&mut index);
    state.save_index(&index)?;
    // Remote entries changed; refresh the local half one more time so the
    // in-memory index matches the tree exactly.
    reindex_local(state);
    // Credentials were applied to the connection at open time.
    let _ = s3;
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
/// Bytes on disk of a group of Parquet files. Used to decide whether rewriting
/// a live hour is cheap enough to hold the merge gate for.
fn total_bytes(files: &[String]) -> u64 {
    files
        .iter()
        .filter_map(|file| std::fs::metadata(file).ok())
        .map(|meta| meta.len())
        .sum()
}

pub(crate) fn compact_local(
    conn: &Connection,
    state: &ParquetState,
    current_hour: u64,
) -> Result<CompactStats> {
    // A first run against months of history would otherwise rewrite thousands
    // of hours in one tick, holding the connection and spiking memory. Bound
    // each pass; the tick runs again in ten minutes and works through the rest.
    //
    // One merge per table per pass, not one per pass: a pass that merges a
    // single hour leaves the other five tables' fragmentation in place until
    // the next tick, and the tick only comes round in ten minutes. Six small
    // merges hold the connection no longer than one big one, and each of them
    // is bounded by the files that accumulated in a single hour.
    const MAX_MERGES_PER_PASS: usize = 2 * TABLES.len();
    // The live hour needs compaction like any other: a ten-second flush cadence
    // drops six files a minute into it, three hundred and sixty an hour, and a
    // query on the last hour opens every one of them. Merging on each tick
    // would rewrite that hour for a handful of files, so wait until the parts
    // are worth the copy, and stop rewriting it once it is big enough that the
    // copy would stall ingest — the closed-hour merge cleans it up after the
    // boundary anyway.
    const OPEN_HOUR_MIN_FILES: usize = 8;
    const OPEN_HOUR_MAX_BYTES: u64 = 1 << 30;
    let mut stats = CompactStats::default();
    'tables: for def in TABLES {
        // The on-disk tree, not the read index: compaction must see a file the
        // index has not caught up with, or the two would disagree about which
        // files exist and the merge could drop rows.
        let local = scan_local_table(&state.table_dir(def.name));
        for (hour, files) in local.by_hour {
            let live = hour >= current_hour;
            let wanted = if live {
                files.len() >= OPEN_HOUR_MIN_FILES && total_bytes(&files) <= OPEN_HOUR_MAX_BYTES
            } else {
                files.len() >= 2
            };
            if !wanted {
                continue;
            }
            if stats.files_written >= MAX_MERGES_PER_PASS {
                break 'tables;
            }
            // A merge lists the hour's files, copies them, then publishes and
            // retires the inputs. A flush landing in that window writes a part
            // the merge never read, and because that part's stamp is older than
            // the published final the read-side "newest final wins" rule hides
            // it: rows gone, nothing failing. Completed hours cannot grow, so
            // only the live hour needs the gate — and it is taken before the
            // list, then the hour is re-listed underneath it, so nothing can
            // slip into the copy.
            let _gate = live.then(|| state.merge_gate.write());
            let again = scan_local_table(&state.table_dir(def.name));
            let Some(files) = again.by_hour.get(&hour).cloned() else {
                continue;
            };
            if files.len() < 2 {
                continue;
            }
            let list = files
                .iter()
                .map(|p| format!("'{}'", esc(p)))
                .collect::<Vec<_>>()
                .join(", ");
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let dir = state.table_dir(def.name).join(format!("hour={hour}"));
            std::fs::create_dir_all(&dir).map_err(Error::Io)?;
            let target = dir.join(format!("final-{unique}.parquet"));
            let tmp = dir.join(format!("final-{unique}.parquet.tmp"));
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
            // Footer-only, for the same reason as the flush path: a merge
            // rewrites a whole hour, and re-reading that hour just to count it
            // doubled the cost of every compaction.
            let path = tmp.to_string_lossy().replace('\\', "/");
            let rows: i64 = conn
                .query_row(
                    &format!("SELECT sum(num_rows) FROM parquet_file_metadata('{path}')"),
                    [],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .map_err(|e| Error::Db(format!("compacting {} hour {hour}: {e}", def.name)))?
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
                retire_file(state, Path::new(file));
            }
            stats.files_merged += files.len();
            stats.files_written += 1;
        }
    }
    for def in TABLES {
        if merge_smallest_files(conn, state, def, 16, current_hour)? {
            stats.files_written += 1;
        }
    }
    // New `final-*` files replaced their inputs on disk; the next windowed
    // query must name the replacements.
    reindex_local(state);
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
    let local = scan_local_table(&state.table_dir(def.name));
    let mut total = 0usize;
    let mut by_hour: std::collections::BTreeMap<u64, Vec<String>> =
        std::collections::BTreeMap::new();
    for (hour, files) in local.by_hour {
        if hour >= current_hour {
            continue;
        }
        total += files.len();
        by_hour.insert(hour, files);
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
        .map(|p| format!("'{}'", esc(p)))
        .collect::<Vec<_>>()
        .join(", ");
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = state.table_dir(def.name).join(format!("hour={hour}"));
    std::fs::create_dir_all(&dir).map_err(Error::Io)?;
    let target = dir.join(format!("final-{unique}.parquet"));
    let tmp = dir.join(format!("final-{unique}.parquet.tmp"));
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
        retire_file(state, Path::new(file));
    }
    Ok(true)
}

/// Size on local disk of the staged Parquet files.
pub(crate) fn staged_bytes(state: &ParquetState) -> u64 {
    let mut total = 0u64;
    for def in TABLES {
        for file in local_files(state, def.name) {
            total = total.saturating_add(file.size);
        }
    }
    // Files waiting out their grace period still occupy the volume, and the
    // disk cap exists to bound the volume. Counting only what the index can
    // name would let a busy tenant run five minutes ahead of its cap.
    if let Ok(entries) = std::fs::read_dir(trash_dir(state)) {
        for entry in entries.flatten() {
            total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
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

/// Directory holding files that have been published-and-retired but not yet
/// unlinked.
///
/// A query builds its SQL from the in-memory index and then opens the paths it
/// named. Deleting a file the moment it is merged or uploaded therefore fails
/// whatever query was in flight — a 500 that says "No such file", on a file the
/// index itself just promised. Retiring is a rename into this directory; the
/// janitor removes entries once no query can still be holding them.
const TRASH_DIR: &str = ".trash";
/// How long a retired file is kept before it is unlinked. Longer than the
/// longest query a client is allowed to run.
pub(crate) const TRASH_TTL: std::time::Duration = std::time::Duration::from_secs(300);

fn trash_dir(state: &ParquetState) -> PathBuf {
    state.dir.join(TRASH_DIR)
}

/// Move a published file out of the read path without unlinking it yet.
pub(crate) fn retire_file(state: &ParquetState, path: &Path) {
    // Make the read path stop naming it first, then move it. The other order
    // leaves a window where the index still promises a file that is already
    // gone, and a query that lands in it fails with "Cannot open file".
    state.forget_local_path(path);
    let dir = trash_dir(state);
    if std::fs::create_dir_all(&dir).is_err() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let target = dir.join(format!("{stamp}-{name}"));
    if std::fs::rename(path, &target).is_err() {
        let _ = std::fs::remove_file(path);
    }
}

/// Delete retired files that are old enough to be unreachable.
pub(crate) fn purge_trash(state: &ParquetState, min_age: std::time::Duration) -> usize {
    let dir = trash_dir(state);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(min_age.max(std::time::Duration::from_millis(1)))
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|modified| modified < cutoff)
            .unwrap_or(false);
        if old && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
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
    let mut files: Vec<LocalFile> = Vec::new();
    for def in TABLES {
        let dictionary = def.hour_column.is_none();
        files.extend(local_files(state, def.name).into_iter().map(|mut file| {
            // `hour=0` is the sentinel for an unsharded dictionary (`frames`,
            // and `stacks` written before they were sharded by sample hour),
            // not a timestamp. Left as an hour it is 1970, so every retention
            // pass deleted the whole frame dictionary and the flamegraph
            // collapsed to a root node with no children while the samples that
            // referenced it survived.
            if dictionary && file.hour == Some(0) {
                file.hour = None;
            }
            file
        }));
    }
    // Over the cap, retired files are the cheapest thing to reclaim: they are
    // already unreachable from the index, so nothing is lost by deleting them
    // now instead of after their grace period.
    if let Some(cap) = max_disk_bytes
        && staged_bytes(state) > cap
    {
        purge_trash(state, std::time::Duration::ZERO);
    }
    let mut stats = ArchivePruneStats::default();
    let retention_hours = retention_ns.map(|retention| retention / 1_000_000_000 / 3600);
    let wall_cutoff = retention_ns.map(|retention| now_ns.saturating_sub(retention) / 1_000_000_000 / 3600);
    // Retention is measured from the wall clock, and the wall clock is the one
    // input we do not control. A suspended VM resumes with a clock that can be
    // hours or days ahead, and a retention pass in that window deletes data that
    // is only minutes old — silently, because the files are the only copy and
    // the cron keeps running. That is exactly the shape of a tenant losing nine
    // hours of archives while every other signal looked healthy.
    //
    // So a file has to be old relative to *both* clocks: the wall clock and the
    // newest data in the tenant. When they agree (the normal case) nothing
    // changes; when the clock jumps forward, the data anchors the cutoff and
    // only genuinely expired hours go. When a tenant stops ingesting, its newest
    // hour goes stale with it, so the wall clock takes over again and the old
    // hours still expire.
    let newest_hour = files.iter().filter_map(|file| file.hour).max();
    let cutoff_hour = match (wall_cutoff, newest_hour, retention_hours) {
        (Some(wall), Some(newest), Some(span)) => Some(wall.min(newest.saturating_sub(span))),
        (Some(wall), None, Some(_)) | (Some(wall), _, None) => Some(wall),
        _ => None,
    };
    if let (Some(wall), Some(anchored)) = (wall_cutoff, cutoff_hour)
        && wall.saturating_sub(anchored) > 24
    {
        warn!(
            wall_cutoff_hour = wall,
            anchored_cutoff_hour = anchored,
            "the clock is far ahead of the newest archive; retention is anchored to the data"
        );
    }
    let remove = |file: &LocalFile, stats: &mut ArchivePruneStats| {
        retire_file(state, &file.path);
        if !file.path.exists() {
            stats.files_removed += 1;
            stats.bytes_reclaimed += file.size;
        }
    };
    // A file with no hour tag cannot be older than anything: keep it rather
    // than guess, so retention never deletes data it cannot date.
    files.retain(|file| {
        if let (Some(cutoff), Some(hour)) = (cutoff_hour, file.hour)
            && hour < cutoff
        {
            remove(file, &mut stats);
            return false;
        }
        true
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
            //
            // A file with no hour cannot be grouped that way, so it is its own
            // unit and sorts last: eviction takes dated data first.
            let mut units: std::collections::BTreeMap<String, Vec<LocalFile>> =
                std::collections::BTreeMap::new();
            for file in files {
                let hour = file.hour.unwrap_or(u64::MAX);
                let key = if file.final_file {
                    format!("hour:{hour}")
                } else {
                    file.stamp.clone().unwrap_or_else(|| format!("hour:{hour}"))
                };
                units.entry(key).or_default().push(file);
            }
            let mut ordered: Vec<(u64, SystemTime, Vec<LocalFile>)> = units
                .into_values()
                .map(|unit| {
                    let hour = unit
                        .iter()
                        .filter_map(|file| file.hour)
                        .min()
                        .unwrap_or(u64::MAX);
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
    // Whole hour files disappeared; a windowed query must stop naming them.
    reindex_local(state);
    // Under cap pressure the grace period cannot be honoured: the files this
    // pass just retired are already unreachable from the index, and keeping
    // them for five minutes would mean the cap is not a cap. A query that was
    // mid-flight on one of them re-reads the index and retries.
    if let Some(cap) = max_disk_bytes
        && staged_bytes(state) > cap
    {
        let purged = purge_trash(state, std::time::Duration::ZERO);
        stats.files_removed += purged;
    }
    Ok(stats)
}

fn parse_hour_tag(name: &str) -> Option<u64> {
    let stem = name.strip_prefix('h')?;
    let end = stem.find('-').unwrap_or(stem.len());
    stem[..end].parse().ok()
}

/// The flush/merge stamp that ties one file per table into one logical write.
///
/// Current layout: `part-<stamp>-<rows>.parquet` and `final-<stamp>.parquet`.
/// The pre-sharded layout was `h<hour>-<stamp>-<rows>.parquet` and
/// `h<hour>-final-<stamp>.parquet`; both are still read, so an upgrade needs
/// no rewrite. Files without a stamp are grouped by hour instead.
fn parse_stamp_tag(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".parquet")?;
    if let Some(rest) = stem.strip_prefix("part-") {
        let stamp = rest.split('-').next()?;
        if !stamp.is_empty() {
            return Some(stamp.to_string());
        }
    }
    if let Some(rest) = stem.strip_prefix("final-")
        && !rest.is_empty()
    {
        return Some(format!("final-{rest}"));
    }
    let stem = stem.strip_prefix('h')?;
    let (_, rest) = stem.split_once('-')?;
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

    /// Every Parquet file under a table directory, including the `hour=<n>`
    /// shard subdirectories that a plain `read_dir` does not see.
    fn parquet_files(dir: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        collect_local_files(dir, 0, &mut files);
        files.into_iter().map(|file| file.path).collect()
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

    /// A flush must be able to move part of the hot tier.
    ///
    /// The old flush was all-or-nothing, which is why a background pass could
    /// never afford to start on a tenant under steady ingest: it needed the
    /// writer for the entire backlog. A slice has to move the oldest hour and
    /// leave everything else readable, or the fix does not buy anything.
    #[test]
    fn a_flush_slice_moves_only_the_oldest_hour() {
        let (store, dir) = parquet_store("slice", u64::MAX);
        const HOUR: u64 = 3_600_000_000_000;
        let base = 5_000 * HOUR; // an hour boundary, so the shard names are exact
        let rows = [
            (base, 1u64),
            (base + 1, 1),
            (base + HOUR, 1),
            (base + 2 * HOUR, 1),
        ];
        for (start, value) in rows {
            store.append(&report("checkout", start, value)).unwrap();
        }
        assert_eq!(store.stats().batch_count, 4);

        let moved = store.flush_slice(2).unwrap();
        // `moved` counts rows across tables: two batches, their two samples and
        // the two stack rows that now travel with them. The hour directories
        // below are what prove the slice boundary.
        assert_eq!(moved, 6, "the two rows of the oldest hour, and only those");

        let stacks = dir.join("parquet").join("stacks");
        assert!(
            stacks
                .join(format!("hour={}", base / HOUR))
                .read_dir()
                .unwrap()
                .next()
                .is_some(),
            "the stack rows follow the hour of the samples that name them"
        );

        let batches = dir.join("parquet").join("batches");
        let oldest = batches.join(format!("hour={}", base / HOUR));
        assert!(
            oldest.read_dir().unwrap().next().is_some(),
            "the oldest hour became a file"
        );
        assert!(
            !batches.join(format!("hour={}", base / HOUR + 1)).exists(),
            "a newer hour must still be hot: one slice is not a full flush"
        );
        assert_eq!(
            store.stats().batch_count,
            4,
            "moving rows between tiers does not change the count"
        );

        // The canonical span has to union the file and the remaining hot rows,
        // otherwise slicing would hide data instead of relocating it.
        let q = crate::QueryOptions::new(
            ProfileType::cpu(),
            base,
            base + 3 * HOUR,
            Selector::default(),
        );
        assert_eq!(crate::flamegraph(&store, &q).unwrap().total as u64, 4);

        // The flushed hour on its own must resolve its symbols through the
        // hour's dictionary file: a sharded stack that a query cannot find is
        // the same as a missing symbol on the flamegraph.
        let q =
            crate::QueryOptions::new(ProfileType::cpu(), base, base + HOUR, Selector::default());
        let flushed = crate::flamegraph(&store, &q).unwrap();
        assert_eq!(flushed.total as u64, 2);
        assert!(
            flushed.names.iter().any(|name| name == "main"),
            "the frame dictionary resolved from the hour the samples belong to"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Upgrading must not lose symbols.
    ///
    /// Before dictionaries were sharded by sample hour, every `stacks` file was
    /// written into `hour=0` regardless of the data it described. Those files
    /// are still referenced by samples from any hour, so a window that predates
    /// the first sharded file has to mount them — a query that cannot find a
    /// stack renders a flamegraph full of address placeholders.
    #[test]
    fn an_unsharded_stack_file_stays_visible_after_the_switch() {
        let (store, dir) = parquet_store("stack-legacy", u64::MAX);
        const HOUR: u64 = 3_600_000_000_000;
        let base = 6_000 * HOUR;
        store.append(&report("checkout", base, 3)).unwrap();
        store.append(&report("checkout", base + HOUR, 5)).unwrap();
        assert!(store.flush_slice(1).unwrap() > 0);

        // Rewrite the first hour's dictionary as the old release would have:
        // one file, in `hour=0`, for samples that live in `hour=6000`.
        let stacks = dir.join("parquet").join("stacks");
        let shard = stacks.join(format!("hour={}", base / HOUR));
        let mut moved = 0;
        for entry in std::fs::read_dir(&shard).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            std::fs::rename(entry.path(), stacks.join(format!("hour=0-{name}"))).unwrap();
            moved += 1;
        }
        assert!(moved > 0, "the hour had a dictionary file to move");
        std::fs::remove_dir_all(&shard).unwrap();
        store.reindex();

        let q =
            crate::QueryOptions::new(ProfileType::cpu(), base, base + HOUR, Selector::default());
        let fg = crate::flamegraph(&store, &q).unwrap();
        assert_eq!(fg.total as u64, 3);
        assert!(
            fg.names.iter().any(|name| name == "main"),
            "the unsharded dictionary file is still mounted for old windows"
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
        let fragments = parquet_files(&samples);
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
        // Restore it at the *same* path, hour shard included: a file dropped
        // in the table root no longer carries an hour and would be mounted for
        // every window, which is a different (and not intended) situation.
        let resurrected = fragments[0].clone();
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

    /// DuckDB's `/` is floating point, and `CAST(double AS BIGINT)` rounds to
    /// the nearest integer. Partitioning with it put every event after half
    /// past into the *next* hour's file: a query whose window ended at "now"
    /// then named hours up to `now`'s hour and found nothing, for half of every
    /// hour. Caught by the CLI ingest/query round-trip, which happens to run at
    /// an arbitrary minute of the hour.
    #[test]
    fn a_flush_files_rows_under_the_hour_they_describe() {
        const HOUR: u64 = 3_600_000_000_000;
        // 13:43 of an arbitrary hour: the second half, where rounding up bites.
        let start = 498_758 * HOUR + 43 * 60 * 1_000_000_000;
        assert_eq!(hour_of(start), 498_758, "sanity: still the same hour");

        let (store, dir) = parquet_store("hour-rounding", u64::MAX);
        store.append(&report("checkout", start, 7)).unwrap();
        assert!(store.flush_hot().unwrap() > 0);

        assert!(
            dir.join("parquet")
                .join("samples")
                .join("hour=498758")
                .is_dir(),
            "the file must be filed under the hour the rows describe"
        );
        let count = store.window_file_count("samples", start - HOUR, start + 1);
        assert_eq!(count, 1, "a window ending at the event must name its hour");
        let rows = store
            .select(
                &ProfileType::cpu(),
                start - HOUR,
                start + 1,
                &Selector::default(),
            )
            .unwrap();
        let total: u64 = rows.iter().map(|batch| batch.total_value).sum();
        assert_eq!(total, 7, "the flushed row is readable through that window");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Before 0.1.19 the hour in the file name came from the wall clock at
    /// flush time, so a flush that straddled a boundary tagged 14:00 rows that
    /// ended at 13:59. Once queries began naming only the hours in their
    /// window, such a file had to stay reachable from the hour its *rows*
    /// describe, or an upgrade would silently lose the last minute of an hour.
    #[test]
    fn an_upgraded_legacy_file_stays_visible_beside_its_hour() {
        const HOUR: u64 = 3_600_000_000_000;
        let late = 498_757 * HOUR + 59 * 60 * 1_000_000_000; // 13:59
        let (store, dir) = parquet_store("legacy-shift", u64::MAX);
        store.append(&report("checkout", late, 7)).unwrap();
        assert!(store.flush_hot().unwrap() > 0);

        // Rewrite the fragment in the old layout with the hour the *clock*
        // would have given it, one hour ahead of the rows it holds.
        let table = dir.join("parquet").join("samples");
        let shard = table.join("hour=498757");
        let original = std::fs::read_dir(&shard)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name();
        let original = original.to_string_lossy();
        let legacy = format!("h498758-{}", original.trim_start_matches("part-"));
        std::fs::rename(shard.join(original.as_ref()), table.join(&legacy)).unwrap();
        std::fs::remove_dir(&shard).unwrap();
        store.reindex();

        let rows = store
            .select(
                &ProfileType::cpu(),
                late - 60 * 1_000_000_000,
                late + 1_000_000_000,
                &Selector::default(),
            )
            .unwrap();
        let total: u64 = rows.iter().map(|batch| batch.total_value).sum();
        assert_eq!(total, 7, "the legacy file is still reachable");
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

        let files = parquet_files(&dir.join("parquet").join("samples")).len();
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
            parquet_files(&samples).len(),
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

        let span = table_span_all(&state, "net_nodes");
        let rows: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {span}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 1, "the archived row still reads");
        let containers: Option<String> = conn
            .query_row(&format!("SELECT containers FROM {span}"), [], |row| {
                row.get(0)
            })
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
