//! Storage backed by DuckDB 2.0.
//!
//! There is no storage engine of our own here, on purpose: transactional
//! writes, compression, zone maps, checkpointing and recovery are DuckDB's
//! problem, and each one of them was a thing that had to be re-proved whenever
//! it changed.
//!
//! What is left on our side is the part that is actually specific to
//! profiling — a small schema:
//!
//! ```sql
//! batches(batch_id, profile_type, service_name, labels, start_ns, end_ns,
//!         total_value, sample_count, bytes, created_ns)
//! samples(batch_id, profile_type, service_name, labels, start_ns, end_ns,
//!         stack_id, value)
//! stacks (stack_id, frames BLOB)        -- packed little-endian u32 frame ids
//! frames (frame_id, module, symbol, file, line)
//! ```
//!
//! `samples` repeats the series columns instead of joining `batches` on every
//! query. Profiles are append-only and a batch is one (service, type, label
//! set, time window), so the repeated values are constant over a contiguous
//! run of rows and DuckDB's run-length compression makes them nearly free.
//! Paying that on write buys a single-table scan with no join on the read path,
//! which is the only part that has to be fast.
//!
//! Stacks are stored as packed ids rather than as a delimited string: a stack
//! is walked on every flamegraph query, and turning text into frame ids at
//! query time would move work from ingest (once) to query (every time).
//! `frames` is a global dictionary, so the same function appears once no matter
//! how many batches reference it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use duckdb::{Connection, InterruptHandle, params};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::{Error, Result};
use crate::model::{Frame, ProfileType, Report, SERVICE_NAME_LABEL};
use crate::parquet::{self, ParquetState};
use crate::selector::{MatchOp, Selector};
use crate::servicemap::{MapEdge, MapHistoryPoint, MapNode, NodeKind, ServiceMapBatch, node_id};
use crate::time::now_ns;

/// Version of the table layout. Bumped when a change needs a migration.
pub const SCHEMA_VERSION: u32 = 1;

/// File name of the database inside a store root.
pub const DB_FILE: &str = "lidlesseye.duckdb";

/// Hard ceiling on rows returned by one [`Store::scan`], so a query over a
/// month of data fails as a truncated answer instead of as an OOM kill.
pub const MAX_SCAN_ROWS: usize = 2_000_000;

/// The counter the service map's time brush plots.
///
/// All three live in the same `net_edges` rows, so switching between them is a
/// change of aggregate rather than of query: the brush keeps its buckets and
/// the bars keep their meaning ("how much of this, per bucket"). Requests are
/// TCP connections opened, because without payload parsing there is no request
/// count to divide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetMetric {
    #[default]
    Connections,
    Bytes,
    Retransmits,
}

impl NetMetric {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "connections" | "requests" | "request" | "conn" => Some(Self::Connections),
            "bytes" | "traffic" => Some(Self::Bytes),
            "retransmits" | "retrans" => Some(Self::Retransmits),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connections => "connections",
            Self::Bytes => "bytes",
            Self::Retransmits => "retransmits",
        }
    }

    /// SQL expression to sum. Constants only — never user input.
    fn column(self) -> &'static str {
        match self {
            Self::Connections => "connections",
            Self::Bytes => "tx_bytes + rx_bytes",
            Self::Retransmits => "retransmits",
        }
    }
}

/// How rows are physically stored.
///
/// * [`StorageMode::DuckDb`] keeps everything in one `lidlesseye.duckdb` file.
///   This is the original, self-contained mode.
/// * [`StorageMode::Parquet`] keeps a small in-memory "hot" DuckDB for recent
///   writes, continuously flushes it to Parquet files on local disk, merges
///   each completed hour into one large Parquet object in S3, and removes the
///   local copy once the upload succeeds. Reads scan the hot tables and all
///   Parquet files (local + S3) through canonical-name VIEWs, so DuckDB pushes
///   filters/projections into the Parquet scans and never copies remote data
///   into the database ("zero-copy" reads).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageMode {
    #[default]
    DuckDb,
    Parquet,
}

/// Connection details for the S3-compatible object store used by
/// [`StorageMode::Parquet`]. Any S3-compatible service (AWS S3, MinIO, ...)
/// works; DuckDB's `httpfs` extension performs the actual uploads/reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct S3Config {
    /// Bucket that holds the Parquet archive.
    pub bucket: String,
    /// Key prefix inside the bucket. The tenant id is appended by
    /// `TenantStore`, so tenants never share objects.
    #[serde(default)]
    pub prefix: String,
    /// Endpoint host:port for S3-compatible stores such as MinIO. `None`
    /// means AWS S3's default endpoint.
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub access_key_id: Option<String>,
    #[serde(default)]
    pub secret_access_key: Option<String>,
    /// Whether to use HTTPS for the endpoint. MinIO test setups use `false`.
    #[serde(default)]
    pub use_ssl: Option<bool>,
    /// `path` for MinIO, `host` for AWS virtual-hosted buckets.
    #[serde(default)]
    pub url_style: Option<String>,
}

/// Tunables for the storage engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreConfig {
    /// Directory holding the database file.
    pub root: PathBuf,
    /// Delete data whose window ended longer ago than this. `None` keeps
    /// everything.
    pub retention_ns: Option<u64>,
    /// Soft cap for the estimated size of the retained batches
    /// (`SUM(batches.bytes)`), not the file size — DuckDB does not shrink the
    /// file on `DELETE`. Oldest batches are evicted first.
    pub max_disk_bytes: Option<u64>,
    /// Worker threads DuckDB may use. `None` lets it pick.
    pub threads: Option<usize>,
    /// DuckDB `memory_limit` for query hash tables. Spills to
    /// `temp_directory` beyond this instead of growing until the container
    /// OOM-kills the process. `None` uses a conservative 1 GiB default that
    /// fits alongside the server in a small pod.
    pub duckdb_memory_limit: Option<String>,
    /// Physical storage backend. Defaults to the original DuckDB file mode.
    #[serde(default)]
    pub storage_mode: StorageMode,
    /// S3 connection used only in [`StorageMode::Parquet`] mode.
    #[serde(default)]
    pub s3: Option<S3Config>,
    /// How long compacted Parquet stays on local disk before being uploaded
    /// to S3 and removed. `None` uses one hour. This is the "hot on disk"
    /// window: queries inside it read local files, older ranges read S3.
    #[serde(default)]
    pub parquet_local_ttl_ns: Option<u64>,
}

impl StoreConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        StoreConfig {
            root: root.into(),
            retention_ns: None,
            max_disk_bytes: None,
            threads: None,
            duckdb_memory_limit: None,
            storage_mode: StorageMode::DuckDb,
            s3: None,
            parquet_local_ttl_ns: None,
        }
    }

    /// Effective local Parquet TTL: one hour unless overridden.
    pub fn parquet_local_ttl_ns(&self) -> u64 {
        self.parquet_local_ttl_ns.unwrap_or(3_600_000_000_000)
    }

    /// Effective DuckDB memory limit: the configured value or a conservative
    /// default sized for a container with a ~2 GiB ceiling.
    pub fn duckdb_memory_limit(&self) -> &str {
        self.duckdb_memory_limit.as_deref().unwrap_or("1GB")
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct StoreStats {
    /// Number of ingested batches, i.e. rows in `batches`.
    pub batch_count: usize,
    /// Physical size of the database file.
    pub total_bytes: u64,
    /// Size the retained batches are estimated to occupy. This is what the
    /// disk cap is measured against: DuckDB marks deleted rows free inside
    /// existing row groups instead of shrinking the file, so the physical size
    /// is not a usable eviction signal.
    pub logical_bytes: u64,
    pub total_value: u64,
    pub earliest_ns: Option<u64>,
    pub latest_ns: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PruneStats {
    pub removed_batches: usize,
    pub removed_bytes: u64,
}

/// One ingested batch: one accepted report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchMeta {
    pub batch_id: u64,
    pub profile_type: ProfileType,
    pub service_name: String,
    pub labels: BTreeMap<String, String>,
    pub start_ns: u64,
    pub end_ns: u64,
    pub total_value: u64,
    pub sample_count: u32,
    /// Approximate on-disk contribution, used for tenant size accounting.
    pub bytes: u64,
    pub created_ns: u64,
}

impl BatchMeta {
    pub fn overlaps(&self, from_ns: u64, until_ns: u64) -> bool {
        self.end_ns > from_ns && self.start_ns < until_ns
    }

    pub fn label(&self, key: &str) -> Option<&str> {
        if key == SERVICE_NAME_LABEL {
            Some(&self.service_name)
        } else {
            self.labels.get(key).map(|s| s.as_str())
        }
    }
}

/// Aggregated information about one series (service + label set + type).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeriesInfo {
    pub service_name: String,
    pub profile_type: String,
    pub labels: BTreeMap<String, String>,
    pub total_value: u64,
    pub batch_count: usize,
    pub start_ns: u64,
    pub end_ns: u64,
}

/// One `(stack, value)` pair of a scan, already summed over the whole window.
#[derive(Debug, Clone)]
pub struct SampleRow {
    pub stack_id: u64,
    pub value: u64,
    /// How many sample rows were folded into this row's `value`. The value
    /// alone cannot answer "how often was this frame seen", because a sample
    /// can weigh more than one unit (a 1 ms CPU interval, say).
    pub samples: u64,
    /// Value of the group-by label, decoded from the batch labels.
    pub group: Option<String>,
}

/// Everything a query needs from storage, in one round trip per table.
pub struct ScanResult {
    pub rows: Vec<SampleRow>,
    /// `stack_id` -> frame ids, leaf first.
    pub stacks: HashMap<u64, Vec<u32>>,
    pub frames: HashMap<u32, Frame>,
    /// True when the query hit [`MAX_SCAN_ROWS`] or the row cap of the query.
    pub truncated: bool,
}

/// Parameters of a [`Store::scan`].
pub struct ScanQuery<'a> {
    pub profile_type: &'a ProfileType,
    pub from_ns: u64,
    pub until_ns: u64,
    pub selector: &'a Selector,
    pub group_by: Option<&'a str>,
}

impl<'a> ScanQuery<'a> {
    pub fn new(
        profile_type: &'a ProfileType,
        from_ns: u64,
        until_ns: u64,
        selector: &'a Selector,
    ) -> Self {
        ScanQuery {
            profile_type,
            from_ns,
            until_ns,
            selector,
            group_by: None,
        }
    }

    pub fn with_group_by(mut self, label: Option<&'a str>) -> Self {
        self.group_by = label.filter(|label| !label.trim().is_empty());
        self
    }
}

/// Frame identity used to deduplicate across the frames that arrive in one
/// batch. Values are owned because the cache outlives a single report.
type FrameKey = (String, String, String, u32);

/// The rows one append will insert, staged before the transaction opens.
struct PendingBatch {
    /// `(frame_id, frame)` for frames the database does not have yet.
    frames: Vec<(u32, Frame)>,
    /// `(stack_id, packed frame ids)`.
    stacks: Vec<(u64, Vec<u32>)>,
    /// `(stack_id, value)`.
    samples: Vec<(u64, u64)>,
}

#[derive(Default)]
struct Ids {
    next_frame: u32,
    next_stack: u64,
    next_batch: u64,
}

/// Thread-safe profile store.
///
/// DuckDB's Rust binding has no separate `Database` handle: a `Connection`
/// owns the engine, and opening the same file twice in one process is a lock
/// conflict rather than a second reader. Every operation therefore goes
/// through one connection behind a mutex. Queries are still parallel *inside*
/// DuckDB (`threads`), which is where a large scan actually spends its time;
/// what is serialized is the short per-query setup.
pub struct Store {
    cfg: StoreConfig,
    conn: Mutex<Connection>,
    /// Cloneable handle that interrupts whatever query is currently running on
    /// `conn` from another thread. Lets the HTTP layer cancel a long scan as
    /// soon as the client goes away or a newer query supersedes it, instead of
    /// holding the single connection hostage until it finishes.
    interrupt: Arc<InterruptHandle>,
    /// `(module, symbol, file, line)` -> frame id, for the write path only.
    frame_cache: Mutex<HashMap<FrameKey, u32>>,
    ids: Mutex<Ids>,
    /// Present only in Parquet mode: layout + lifecycle of the local/S3 files.
    parquet: Option<ParquetState>,
    /// Wall-clock instant of the last local Parquet compaction pass. The
    /// compaction tick runs every few seconds; rewriting an hour's files that
    /// often would cost more than it saves.
    last_compact_ns: std::sync::atomic::AtomicU64,
    /// Cached [`StoreStats`] and when it was computed. Every open dashboard
    /// polls `stats`, and in Parquet mode that query opens every file's footer
    /// just to count rows.
    stats_cache: Mutex<Option<(u64, StoreStats)>>,
}

impl Store {
    /// Whether this store keeps rows in a DuckDB file or in Parquet/S3.
    pub fn storage_mode(&self) -> StorageMode {
        self.cfg.storage_mode
    }

    /// Name of the table writes must go to. In Parquet mode the canonical
    /// names are VIEWs (hot ∪ local ∪ S3), so writes target the hot table.
    fn wtable(&self, name: &str) -> String {
        match self.parquet {
            Some(_) => format!("{name}_hot"),
            None => name.to_string(),
        }
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("root", &self.cfg.root)
            .field("db", &self.path())
            .finish()
    }
}

impl Store {
    /// Open (and create if needed) the store under `cfg.root`.
    pub fn open(cfg: StoreConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.root)?;

        let parquet_state = if cfg.storage_mode == StorageMode::Parquet {
            let s3 = cfg.s3.as_ref().ok_or_else(|| {
                Error::InvalidArgument("parquet storage mode requires S3 configuration".into())
            })?;
            Some(ParquetState::new(&cfg.root, s3, cfg.parquet_local_ttl_ns()))
        } else {
            None
        };

        let mut duckdb_config = duckdb::Config::default();
        if let Some(threads) = cfg.threads {
            duckdb_config = duckdb_config
                .threads(threads as i64)
                .map_err(|err| Error::Db(format!("setting DuckDB threads: {err}")))?;
        }

        let (conn, path) = if let Some(state) = &parquet_state {
            // Hot tier only needs an in-memory database; durable rows live in
            // Parquet files. This is the key to "no .duckdb file on disk".
            let conn = Connection::open_in_memory()
                .map_err(|err| Error::Db(format!("opening in-memory DuckDB: {err}")))?;
            let s3 = cfg.s3.as_ref().expect("checked above");
            parquet::configure_s3(&conn, s3)?;
            tune_connection(&conn, &cfg.root.join("parquet.meta.duckdb"), &cfg)?;
            parquet::create_schema(&conn, state)?;
            // S3 is the source of truth; the local manifest is only a cache.
            // Re-discover archived objects so a lost manifest (new node / PVC
            // rebuild / first start against an existing bucket) never hides
            // data. Best-effort: an unreachable S3 must not block startup.
            match parquet::discover_s3_objects(&conn, state, s3) {
                Ok(0) | Err(_) => {} // nothing archived yet, or S3 unreachable: both fine
                Ok(n) => eprintln!("lidlesseye: discovered {n} archived S3 object(s)"),
            }
            (conn, cfg.root.join(DB_FILE))
        } else {
            let path = cfg.root.join(DB_FILE);
            let conn = Connection::open_with_flags(&path, duckdb_config)
                .map_err(|err| Error::Db(format!("opening {}: {err}", path.display())))?;
            tune_connection(&conn, &path, &cfg)?;
            create_schema(&conn)?;
            (conn, path)
        };
        let _ = &path; // path kept for the DuckDB `disk_bytes` helper
        let interrupt = conn.interrupt_handle();

        let ids = {
            // These SELECTs hit canonical names: real tables in DuckDB mode,
            // VIEWs over (hot ∪ local ∪ S3) in Parquet mode.
            let max_frame: i64 = conn
                .query_row("SELECT COALESCE(MAX(frame_id), 0) FROM frames", [], |row| {
                    row.get(0)
                })
                .unwrap_or(0);
            let max_stack: i64 = conn
                .query_row("SELECT COALESCE(MAX(stack_id), 0) FROM stacks", [], |row| {
                    row.get(0)
                })
                .unwrap_or(0);
            let max_batch: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(batch_id), 0) FROM batches",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            // Counters resume *after* the highest id in use; taking `max`
            // verbatim would hand the next append an id that already exists.
            // frame ids are u32 on the wire, so the counter may not wrap.
            Ids {
                next_frame: u32::try_from(max_frame.saturating_add(1)).unwrap_or(u32::MAX),
                next_stack: max_stack.max(0) as u64 + 1,
                next_batch: max_batch.max(0) as u64 + 1,
            }
        };

        let store = Store {
            cfg,
            conn: Mutex::new(conn),
            interrupt,
            frame_cache: Mutex::new(HashMap::new()),
            ids: Mutex::new(ids),
            parquet: parquet_state,
            last_compact_ns: std::sync::atomic::AtomicU64::new(0),
            stats_cache: Mutex::new(None),
        };
        Ok(store)
    }

    /// Interrupt the query currently running on this store's connection.
    /// Safe to call from any thread; a no-op when nothing is in flight. This is
    /// what lets the HTTP layer stop a long scan the moment it is superseded.
    pub fn interrupt(&self) {
        self.interrupt.interrupt();
    }

    pub fn config(&self) -> &StoreConfig {
        &self.cfg
    }

    pub fn root(&self) -> &Path {
        &self.cfg.root
    }

    /// Path of the database file.
    pub fn path(&self) -> PathBuf {
        self.cfg.root.join(DB_FILE)
    }

    /// Kept for API compatibility; the schema is created on open.
    pub fn reload(&self) -> Result<usize> {
        Ok(self.stats().batch_count)
    }

    /// Every batch, oldest first.
    pub fn batches(&self) -> Vec<Arc<BatchMeta>> {
        self.query_batches(None, None, &Selector::default())
            .unwrap_or_default()
    }

    /// Batches of `profile_type` overlapping the window that satisfy the
    /// selector.
    pub fn select(
        &self,
        profile_type: &ProfileType,
        from_ns: u64,
        until_ns: u64,
        selector: &Selector,
    ) -> Vec<Arc<BatchMeta>> {
        self.query_batches(Some((profile_type, from_ns, until_ns)), None, selector)
            .unwrap_or_default()
    }

    fn query_batches(
        &self,
        window: Option<(&ProfileType, u64, u64)>,
        limit: Option<usize>,
        selector: &Selector,
    ) -> Result<Vec<Arc<BatchMeta>>> {
        let conn = self.conn.lock();
        let mut sql = String::from(
            "SELECT batch_id, profile_type, service_name, labels, start_ns, end_ns, \
                    total_value, sample_count, bytes, created_ns FROM batches",
        );
        let mut clauses: Vec<String> = Vec::new();
        let mut args: Vec<Box<dyn duckdb::ToSql>> = Vec::new();

        if let Some((profile_type, from_ns, until_ns)) = window {
            let types = profile_type.source_types();
            let placeholders = vec!["?"; types.len()].join(", ");
            clauses.push(format!("profile_type IN ({placeholders})"));
            for t in types {
                args.push(Box::new(t.as_str().to_string()));
            }
            clauses.push("end_ns > ?".into());
            args.push(Box::new(ts(from_ns)));
            clauses.push("start_ns < ?".into());
            args.push(Box::new(ts(until_ns)));
        }
        push_selector_sql(selector, &mut clauses, &mut args);

        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY start_ns ASC, batch_id ASC");
        if let Some(limit) = limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }

        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let refs: Vec<&dyn duckdb::ToSql> = args.iter().map(|a| a.as_ref()).collect();
        let mut rows = stmt.query(refs.as_slice()).map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(db_err)? {
            let labels: String = row.get(3).map_err(db_err)?;
            let meta = BatchMeta {
                batch_id: row.get::<_, i64>(0).map_err(db_err)?.max(0) as u64,
                profile_type: ProfileType::new(row.get::<_, String>(1).map_err(db_err)?),
                service_name: row.get(2).map_err(db_err)?,
                labels: decode_labels(&labels),
                start_ns: row.get::<_, i64>(4).map_err(db_err)?.max(0) as u64,
                end_ns: row.get::<_, i64>(5).map_err(db_err)?.max(0) as u64,
                total_value: row.get::<_, i64>(6).map_err(db_err)?.max(0) as u64,
                sample_count: row.get::<_, i64>(7).map_err(db_err)?.max(0) as u32,
                bytes: row.get::<_, i64>(8).map_err(db_err)?.max(0) as u64,
                created_ns: row.get::<_, i64>(9).map_err(db_err)?.max(0) as u64,
            };
            // SQL already rejected the profile type and the window; the label
            // matchers run here, where glob semantics live.
            if selector.matches_meta(&meta.service_name, &meta.labels) {
                out.push(Arc::new(meta));
            }
        }
        Ok(out)
    }

    /// Persist a report as one batch.
    pub fn append(&self, report: &Report) -> Result<Arc<BatchMeta>> {
        report.validate()?;

        let labels_text = encode_labels(&report.all_labels());
        let batch_id = {
            let mut ids = self.ids.lock();
            let id = ids.next_batch;
            ids.next_batch = ids.next_batch.saturating_add(1);
            id
        };

        // Intern frames and stacks before opening the transaction so a bad
        // report cannot leave a half-written batch behind.
        let pending = self.prepare_rows(report)?;
        let bytes = pending.samples.len() as u64 * 24
            + pending
                .stacks
                .iter()
                .map(|(_, frames)| frames.len() as u64 * 4)
                .sum::<u64>()
            + pending.frames.len() as u64 * 48;

        let created_ns = now_ns();
        let total_value: u64 = report.stacks.iter().map(|s| s.value).sum();

        let frames_t = self.wtable("frames");
        let stacks_t = self.wtable("stacks");
        let samples_t = self.wtable("samples");
        let batches_t = self.wtable("batches");
        {
            let conn = self.conn.lock();
            conn.execute_batch("BEGIN TRANSACTION").map_err(db_err)?;
            let result = (|| -> Result<()> {
                if !pending.frames.is_empty() {
                    let mut stmt = conn
                        .prepare(&format!(
                            "INSERT INTO {frames_t} (frame_id, module, symbol, file, line) \
                             VALUES (?, ?, ?, ?, ?)"
                        ))
                        .map_err(db_err)?;
                    for (id, frame) in &pending.frames {
                        stmt.execute(params![
                            *id as i64,
                            frame.module,
                            frame.symbol,
                            frame.file,
                            frame.line as i64
                        ])
                        .map_err(db_err)?;
                    }
                }
                if !pending.stacks.is_empty() {
                    let mut stmt = conn
                        .prepare(&format!(
                            "INSERT INTO {stacks_t} (stack_id, frames) VALUES (?, ?)"
                        ))
                        .map_err(db_err)?;
                    for (id, frames) in &pending.stacks {
                        stmt.execute(params![*id as i64, pack_frames(frames)])
                            .map_err(db_err)?;
                    }
                }
                let mut stmt = conn
                    .prepare(&format!(
                        "INSERT INTO {samples_t} (batch_id, profile_type, service_name, labels, \
                         start_ns, end_ns, stack_id, value) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
                    ))
                    .map_err(db_err)?;
                for (stack_id, value) in &pending.samples {
                    stmt.execute(params![
                        batch_id as i64,
                        report.profile_type.as_str(),
                        report.service_name,
                        labels_text,
                        ts(report.start_ns),
                        ts(report.end_ns),
                        *stack_id as i64,
                        *value as i64
                    ])
                    .map_err(db_err)?;
                }
                conn.execute(
                    &format!(
                        "INSERT INTO {batches_t} (batch_id, profile_type, service_name, labels, start_ns, \
                         end_ns, total_value, sample_count, bytes, created_ns) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                    ),
                    params![
                        batch_id as i64,
                        report.profile_type.as_str(),
                        report.service_name,
                        labels_text,
                        ts(report.start_ns),
                        ts(report.end_ns),
                        total_value as i64,
                        pending.samples.len() as i64,
                        bytes as i64,
                        ts(created_ns)
                    ],
                )
                .map_err(db_err)?;
                Ok(())
            })();

            match result {
                Ok(()) => {
                    conn.execute_batch("COMMIT").map_err(db_err)?;
                }
                Err(err) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(err);
                }
            }
        }

        debug!(
            batch = batch_id,
            stacks = pending.samples.len(),
            frames = pending.frames.len(),
            "wrote batch"
        );

        // A caller that appends and then asks for stats must see its own write.
        // The cache only short-circuits polling between writes.
        *self.stats_cache.lock() = None;
        Ok(Arc::new(BatchMeta {
            batch_id,
            profile_type: report.profile_type.clone(),
            service_name: report.service_name.clone(),
            labels: report.all_labels(),
            start_ns: report.start_ns,
            end_ns: report.end_ns,
            total_value,
            sample_count: pending.samples.len() as u32,
            bytes,
            created_ns,
        }))
    }

    /// Turn a report into the three row sets the transaction will insert.
    ///
    /// Deduplication happens twice and for two different reasons: frames are
    /// deduplicated against the whole database (so a function is stored once),
    /// stacks only inside the batch (a repeated stack inside one profile would
    /// otherwise be stored twice, while cross-batch deduplication would need a
    /// lookup of every stack on the ingest path).
    fn prepare_rows(&self, report: &Report) -> Result<PendingBatch> {
        let mut frame_rows: Vec<(u32, Frame)> = Vec::new();
        // Keyed by the whole frame reference `[module, symbol, file, line]`,
        // not by the module's string index: two functions in one binary share
        // a module string, and keying on it would merge them.
        let mut frame_ids: HashMap<[u32; 4], u32> = HashMap::new();
        {
            let mut cache = self.frame_cache.lock();
            let mut ids = self.ids.lock();
            for stack in &report.stacks {
                for frame_ref in &stack.frames {
                    if frame_ids.contains_key(frame_ref) {
                        continue;
                    }
                    let string = |i: u32| -> &str {
                        report
                            .strings
                            .get(i as usize)
                            .map(String::as_str)
                            .unwrap_or("")
                    };
                    let frame = Frame {
                        module: string(frame_ref[0]).to_string(),
                        symbol: string(frame_ref[1]).to_string(),
                        file: string(frame_ref[2]).to_string(),
                        line: frame_ref[3],
                        addr: 0,
                    };
                    let key: FrameKey = (
                        frame.module.clone(),
                        frame.symbol.clone(),
                        frame.file.clone(),
                        frame.line,
                    );
                    let id = match cache.get(&key) {
                        Some(&id) => id,
                        None => {
                            let id = ids.next_frame;
                            ids.next_frame = ids.next_frame.saturating_add(1);
                            cache.insert(key, id);
                            frame_rows.push((id, frame));
                            id
                        }
                    };
                    frame_ids.insert(*frame_ref, id);
                }
            }
        }

        let mut stack_rows: Vec<(u64, Vec<u32>)> = Vec::new();
        let mut stack_index: HashMap<Vec<u32>, u64> = HashMap::new();
        let mut sample_rows: Vec<(u64, u64)> = Vec::with_capacity(report.stacks.len());
        {
            let mut ids = self.ids.lock();
            for stack in &report.stacks {
                let frames: Vec<u32> = stack
                    .frames
                    .iter()
                    .filter_map(|frame_ref| frame_ids.get(frame_ref).copied())
                    .collect();
                let stack_id = match stack_index.get(&frames) {
                    Some(&id) => id,
                    None => {
                        let id = ids.next_stack;
                        ids.next_stack = ids.next_stack.saturating_add(1);
                        stack_index.insert(frames.clone(), id);
                        stack_rows.push((id, frames));
                        id
                    }
                };
                sample_rows.push((stack_id, stack.value));
            }
        }
        Ok(PendingBatch {
            frames: frame_rows,
            stacks: stack_rows,
            samples: sample_rows,
        })
    }

    /// Every `(stack, value)` pair matching the query, plus the frames they
    /// reference.
    pub fn scan(&self, query: &ScanQuery<'_>) -> Result<ScanResult> {
        let conn = self.conn.lock();
        let types = query.profile_type.source_types();
        let placeholders = vec!["?"; types.len()].join(", ");
        // A selector or a `group_by` needs the series columns; a plain
        // flamegraph does not. Those columns are the bulk of every sample row
        // (a full label map repeated per stack), so reading only
        // `stack_id, value` removes most of the I/O on a wide scan.
        let needs_series = !query.selector.is_empty() || query.group_by.is_some();
        let mut sql = if needs_series {
            format!(
                "SELECT stack_id, service_name, labels, SUM(value), COUNT(*) FROM samples \
                 WHERE profile_type IN ({placeholders}) AND end_ns > ? AND start_ns < ?"
            )
        } else {
            format!(
                "SELECT stack_id, SUM(value), COUNT(*) FROM samples \
                 WHERE profile_type IN ({placeholders}) AND end_ns > ? AND start_ns < ?"
            )
        };
        let mut args: Vec<Box<dyn duckdb::ToSql>> = Vec::new();
        for t in &types {
            args.push(Box::new(t.as_str().to_string()));
        }
        args.push(Box::new(ts(query.from_ns)));
        args.push(Box::new(ts(query.until_ns)));

        let mut clauses: Vec<String> = Vec::new();
        push_selector_sql(query.selector, &mut clauses, &mut args);
        if !clauses.is_empty() {
            sql.push_str(" AND ");
            sql.push_str(&clauses.join(" AND "));
        }
        // Grouping by the series as well as the stack keeps the label string
        // available for the exact matchers, and lets the same stack appear
        // once per series (which is what `group_by` needs).
        if needs_series {
            sql.push_str(" GROUP BY stack_id, service_name, labels");
        } else {
            sql.push_str(" GROUP BY stack_id");
        }

        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let refs: Vec<&dyn duckdb::ToSql> = args.iter().map(|a| a.as_ref()).collect();
        let mut raw = stmt.query(refs.as_slice()).map_err(db_err)?;

        let mut rows: Vec<SampleRow> = Vec::new();
        let mut truncated = false;
        // A wide scan returns one row per (stack, series) — over a million
        // rows in practice — but those rows share only a handful of distinct
        // label sets. Decoding each one was measured at ~8s of a ~9s scan;
        // memoising by the encoded string turns that into a few hundred
        // decodes. Matchers and grouping below are pure reads of this value.
        let mut label_cache: HashMap<String, BTreeMap<String, String>> = HashMap::new();
        while let Some(row) = raw.next().map_err(db_err)? {
            if rows.len() >= MAX_SCAN_ROWS {
                truncated = true;
                break;
            }
            let stack_id = row.get::<_, i64>(0).map_err(db_err)?.max(0) as u64;
            // No selector and no grouping: the query selected only
            // `stack_id, value`, and there is no series to keep.
            if !needs_series {
                let value = row.get::<_, i64>(1).map_err(db_err)?.max(0) as u64;
                let samples = row.get::<_, i64>(2).map_err(db_err)?.max(0) as u64;
                rows.push(SampleRow {
                    stack_id,
                    value,
                    samples,
                    group: None,
                });
                continue;
            }
            let service_name: String = row.get(1).map_err(db_err)?;
            let labels_text: String = row.get(2).map_err(db_err)?;
            let value = row.get::<_, i64>(3).map_err(db_err)?.max(0) as u64;
            let samples = row.get::<_, i64>(4).map_err(db_err)?.max(0) as u64;

            // Only the encoded payload is the cache key: `matches_meta` and
            // `group_by` both take service_name separately, so the decoded
            // view never needs it injected. `String: Borrow<str>` lets us look
            // up with `&str` and only allocate on a cache miss.
            if !label_cache.contains_key(labels_text.as_str()) {
                label_cache.insert(labels_text.clone(), decode_labels(&labels_text));
            }
            let labels = &label_cache[labels_text.as_str()];
            // The label matchers (glob included) run here; SQL only pruned on
            // `service_name`. Dropping a whole (stack, series) group is safe
            // because the group is atomic: every row in it shares the series.
            if !query.selector.matches_meta(&service_name, labels) {
                continue;
            }

            let group = query.group_by.and_then(|label| {
                let value = if label == SERVICE_NAME_LABEL {
                    Some(service_name.clone())
                } else {
                    labels.get(label).cloned()
                };
                value.map(|value| format!("{label}={value}"))
            });
            rows.push(SampleRow {
                stack_id,
                value,
                samples,
                group,
            });
        }
        drop(raw);
        drop(stmt);

        let stack_ids: Vec<u64> = rows.iter().map(|r| r.stack_id).collect();
        let stacks = self.load_stacks(&conn, &stack_ids)?;
        let frame_ids: Vec<u32> = {
            let mut seen: std::collections::HashSet<u32> = Default::default();
            let mut out = Vec::new();
            for frames in stacks.values() {
                for &id in frames {
                    if seen.insert(id) {
                        out.push(id);
                    }
                }
            }
            out
        };
        let frames = self.load_frames(&conn, &frame_ids)?;

        Ok(ScanResult {
            rows,
            stacks,
            frames,
            truncated,
        })
    }

    /// Load stack frames for many ids in ONE query.
    ///
    /// The old implementation issued `ids.len()/1000` separate `IN (...)`
    /// queries; with a wide time range that is well over a thousand statement
    /// prepares and dominated total query time (16s of an 17s scan). Loading
    /// the ids into a temp table with DuckDB's `Appender` and joining once
    /// brings the same work down to well under a second.
    fn load_stacks(&self, conn: &Connection, ids: &[u64]) -> Result<HashMap<u64, Vec<u32>>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS lidlesseye_query_ids(id BIGINT); \
             DELETE FROM lidlesseye_query_ids;",
        )
        .map_err(db_err)?;
        {
            let mut appender = conn.appender("lidlesseye_query_ids").map_err(db_err)?;
            for &id in ids {
                appender.append_row([id as i64]).map_err(db_err)?;
            }
            appender.flush().map_err(db_err)?;
        }
        let mut out = HashMap::with_capacity(ids.len());
        let mut stmt = conn
            .prepare(
                "SELECT s.stack_id, s.frames FROM stacks s \
                 JOIN lidlesseye_query_ids q ON s.stack_id = q.id",
            )
            .map_err(db_err)?;
        let mut rows = stmt.query([]).map_err(db_err)?;
        while let Some(row) = rows.next().map_err(db_err)? {
            let id = row.get::<_, i64>(0).map_err(db_err)?.max(0) as u64;
            let blob: Vec<u8> = row.get(1).map_err(db_err)?;
            out.insert(id, unpack_frames(&blob));
        }
        Ok(out)
    }

    /// Same bulk-join strategy as [`Store::load_stacks`]: one statement instead
    /// of thousands of `IN (...)` prepares on the hot path of every query.
    fn load_frames(&self, conn: &Connection, ids: &[u32]) -> Result<HashMap<u32, Frame>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS lidlesseye_query_ids(id BIGINT); \
             DELETE FROM lidlesseye_query_ids;",
        )
        .map_err(db_err)?;
        {
            let mut appender = conn.appender("lidlesseye_query_ids").map_err(db_err)?;
            for &id in ids {
                appender.append_row([id as i64]).map_err(db_err)?;
            }
            appender.flush().map_err(db_err)?;
        }
        let mut out = HashMap::with_capacity(ids.len());
        let mut stmt = conn
            .prepare(
                "SELECT f.frame_id, f.module, f.symbol, f.file, f.line FROM frames f \
                 JOIN lidlesseye_query_ids q ON f.frame_id = q.id",
            )
            .map_err(db_err)?;
        let mut rows = stmt.query([]).map_err(db_err)?;
        while let Some(row) = rows.next().map_err(db_err)? {
            let id = row.get::<_, i64>(0).map_err(db_err)?.max(0) as u32;
            out.insert(
                id,
                Frame {
                    module: row.get(1).map_err(db_err)?,
                    symbol: row.get(2).map_err(db_err)?,
                    file: row.get(3).map_err(db_err)?,
                    line: row.get::<_, i64>(4).map_err(db_err)?.max(0) as u32,
                    addr: 0,
                },
            );
        }
        Ok(out)
    }

    pub fn stats(&self) -> StoreStats {
        let now = now_ns();
        // Dashboards poll this every few seconds. In Parquet mode the query
        // reads every file's footer, so a short cache removes a surprising
        // amount of metadata I/O without making the number stale to the eye.
        if let Some((at, cached)) = *self.stats_cache.lock()
            && now.saturating_sub(at) < 30_000_000_000
        {
            return cached;
        }
        let fallback = StoreStats::default();
        let conn = self.conn.lock();
        let row = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(total_value), 0), COALESCE(SUM(bytes), 0), \
                    MIN(start_ns), MAX(end_ns) FROM batches",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0).unwrap_or(0),
                    row.get::<_, i64>(1).unwrap_or(0),
                    row.get::<_, i64>(2).unwrap_or(0),
                    row.get::<_, Option<i64>>(3).ok().flatten(),
                    row.get::<_, Option<i64>>(4).ok().flatten(),
                ))
            },
        );
        let (count, value, logical, earliest, latest) = match row {
            Ok(v) => v,
            Err(err) => {
                warn!(error = %err, "stats query failed");
                return fallback;
            }
        };
        let stats = StoreStats {
            batch_count: count.max(0) as usize,
            total_bytes: self.disk_bytes(),
            logical_bytes: logical.max(0) as u64,
            total_value: value.max(0) as u64,
            earliest_ns: earliest.map(|v| v.max(0) as u64),
            latest_ns: latest.map(|v| v.max(0) as u64),
        };
        *self.stats_cache.lock() = Some((now, stats));
        stats
    }

    /// Physical footprint of the tenant's database: the file DuckDB keeps its
    /// blocks in, plus the write-ahead log.
    ///
    /// `pragma_database_size()` is the tempting source, but its
    /// `database_size` only covers what has been checkpointed — right after a
    /// burst of ingest it reports 0 while the WAL holds every byte — so the
    /// number shown next to `logical_bytes` would look broken. The filesystem
    /// does not guess, and it matches what an operator sees with `du`.
    fn disk_bytes(&self) -> u64 {
        if let Some(state) = &self.parquet {
            // Local staged Parquet is the physical footprint we control; the
            // hot tier is in-memory and the archive lives in S3.
            return parquet::staged_bytes(state);
        }
        let path = self.path();
        let main = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut wal = path.into_os_string();
        wal.push(".wal");
        let wal = std::fs::metadata(Path::new(&wal))
            .map(|m| m.len())
            .unwrap_or(0);
        main.saturating_add(wal)
    }

    /// Flush the in-memory hot tier to a local Parquet file and return the
    /// number of rows moved. No-op in DuckDB mode. Safe to call periodically.
    pub fn flush_hot(&self) -> Result<usize> {
        let Some(state) = &self.parquet else {
            return Ok(0);
        };
        let conn = self.conn.lock();
        parquet::flush_hot(&conn, state)
    }

    /// Merge each completed hour's local Parquet files into one per table.
    /// No-op in DuckDB mode, and throttled to once every ten minutes so the
    /// ten-second compaction tick stays cheap.
    pub fn compact_local(&self) -> Result<parquet::CompactStats> {
        let Some(state) = &self.parquet else {
            return Ok(parquet::CompactStats::default());
        };
        let now = now_ns();
        const INTERVAL_NS: u64 = 600_000_000_000;
        let last = self
            .last_compact_ns
            .load(std::sync::atomic::Ordering::Relaxed);
        if now.saturating_sub(last) < INTERVAL_NS {
            return Ok(parquet::CompactStats::default());
        }
        self.last_compact_ns
            .store(now, std::sync::atomic::Ordering::Relaxed);
        let current_hour = parquet::hour_tag()
            .trim_start_matches('h')
            .parse::<u64>()
            .unwrap_or(u64::MAX);
        let conn = self.conn.lock();
        parquet::compact_local(&conn, state, current_hour)
    }

    /// Compact every hour, including the current one. Tests only: the public
    /// method deliberately leaves the hour being written alone.
    #[cfg(test)]
    pub(crate) fn compact_all_for_test(&self) -> Result<parquet::CompactStats> {
        let Some(state) = &self.parquet else {
            return Ok(parquet::CompactStats::default());
        };
        let conn = self.conn.lock();
        parquet::compact_local(&conn, state, u64::MAX)
    }

    /// Merge local Parquet files older than the local TTL into one object per
    /// table/hour, upload it to S3, and remove the local copies. No-op in
    /// DuckDB mode. `now_ns` is supplied by the caller for deterministic TTL.
    pub fn archive_local(&self, now_ns: u64) -> Result<parquet::ArchiveStats> {
        let Some(state) = &self.parquet else {
            return Ok(parquet::ArchiveStats::default());
        };
        let Some(s3) = &self.cfg.s3 else {
            return Ok(parquet::ArchiveStats::default());
        };
        let conn = self.conn.lock();
        parquet::archive_local(&conn, state, now_ns, s3)
    }

    /// Total bytes staged locally in Parquet mode (0 in DuckDB mode).
    pub fn staged_bytes(&self) -> u64 {
        self.parquet
            .as_ref()
            .map(parquet::staged_bytes)
            .unwrap_or(0)
    }

    pub fn series(&self, from_ns: u64, until_ns: u64, selector: &Selector) -> Vec<SeriesInfo> {
        self.series_inner(from_ns, until_ns, selector)
            .unwrap_or_default()
    }

    fn series_inner(
        &self,
        from_ns: u64,
        until_ns: u64,
        selector: &Selector,
    ) -> Result<Vec<SeriesInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT service_name, profile_type, labels, SUM(total_value), COUNT(*), \
                        MIN(start_ns), MAX(end_ns) FROM batches \
                 WHERE end_ns > ? AND start_ns < ? GROUP BY 1, 2, 3",
            )
            .map_err(db_err)?;
        let mut rows = stmt
            .query(params![ts(from_ns), ts(until_ns)])
            .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(db_err)? {
            let service_name: String = row.get(0).map_err(db_err)?;
            let labels = decode_labels(&row.get::<_, String>(2).map_err(db_err)?);
            if !selector.matches_meta(&service_name, &labels) {
                continue;
            }
            out.push(SeriesInfo {
                profile_type: row.get(1).map_err(db_err)?,
                total_value: row.get::<_, i64>(3).map_err(db_err)?.max(0) as u64,
                batch_count: row.get::<_, i64>(4).map_err(db_err)?.max(0) as usize,
                start_ns: row.get::<_, i64>(5).map_err(db_err)?.max(0) as u64,
                end_ns: row.get::<_, i64>(6).map_err(db_err)?.max(0) as u64,
                service_name,
                labels,
            });
        }
        out.sort_unstable_by(|a, b| {
            b.total_value
                .cmp(&a.total_value)
                .then_with(|| a.service_name.cmp(&b.service_name))
        });
        Ok(out)
    }

    pub fn label_values(
        &self,
        label: &str,
        from_ns: u64,
        until_ns: u64,
        selector: &Selector,
    ) -> Vec<(String, u64)> {
        let mut map: HashMap<String, u64> = HashMap::new();
        for series in self.series(from_ns, until_ns, selector) {
            let value = if label == SERVICE_NAME_LABEL {
                Some(series.service_name.as_str())
            } else {
                series.labels.get(label).map(String::as_str)
            };
            if let Some(value) = value {
                *map.entry(value.to_string()).or_default() += series.total_value;
            }
        }
        let mut out: Vec<(String, u64)> = map.into_iter().collect();
        out.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    pub fn label_names(&self, from_ns: u64, until_ns: u64, selector: &Selector) -> Vec<String> {
        let mut set: std::collections::BTreeSet<String> = Default::default();
        for series in self.series(from_ns, until_ns, selector) {
            set.insert(SERVICE_NAME_LABEL.to_string());
            for key in series.labels.keys() {
                set.insert(key.clone());
            }
        }
        set.into_iter().collect()
    }

    pub fn series_count(&self) -> usize {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM (SELECT DISTINCT service_name, profile_type, labels FROM batches)",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count.max(0) as usize)
        .unwrap_or(0)
    }

    /// Is this exact series already known? Used to let an existing series keep
    /// writing once a tenant has hit its series limit.
    pub fn contains_series(&self, report: &Report) -> bool {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT 1 FROM batches WHERE service_name = ? AND profile_type = ? AND labels = ? LIMIT 1",
            params![
                report.service_name,
                report.profile_type.as_str(),
                encode_labels(&report.all_labels())
            ],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Drop data older than the retention window and, if configured, evict the
    /// oldest batches until the database fits in `max_disk_bytes`.
    pub fn prune(&self, now_ns: u64) -> Result<PruneStats> {
        let mut stats = PruneStats::default();
        let before = self.stats();

        let samples_t = self.wtable("samples");
        let batches_t = self.wtable("batches");
        let nodes_t = self.wtable("net_nodes");
        let edges_t = self.wtable("net_edges");
        {
            let conn = self.conn.lock();
            if let Some(retention) = self.cfg.retention_ns {
                let cutoff = now_ns.saturating_sub(retention);
                let removed: i64 = conn
                    .query_row(
                        &format!("SELECT COUNT(*) FROM {batches_t} WHERE end_ns < ?"),
                        params![ts(cutoff)],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                conn.execute(
                    &format!("DELETE FROM {samples_t} WHERE end_ns < ?"),
                    params![ts(cutoff)],
                )
                .map_err(db_err)?;
                conn.execute(
                    &format!("DELETE FROM {batches_t} WHERE end_ns < ?"),
                    params![ts(cutoff)],
                )
                .map_err(db_err)?;
                // The map is a second, independent table family: it has no
                // batches, so it needs its own cutoff.
                conn.execute(
                    &format!("DELETE FROM {nodes_t} WHERE bucket_ns < ?"),
                    params![ts(cutoff)],
                )
                .map_err(db_err)?;
                conn.execute(
                    &format!("DELETE FROM {edges_t} WHERE bucket_ns < ?"),
                    params![ts(cutoff)],
                )
                .map_err(db_err)?;
                stats.removed_batches += removed.max(0) as usize;
            }

            if let Some(limit) = self.cfg.max_disk_bytes {
                // Delete whole batches, oldest first, until the *retained*
                // data fits. DuckDB does not return freed row-group space to
                // the filesystem, so the file size is not an eviction signal:
                // using it would evict everything and still never reach the
                // limit. `SUM(batches.bytes)` is, and the loop is bounded.
                let mut guard = 0;
                loop {
                    let size: i64 = conn
                        .query_row("SELECT COALESCE(SUM(bytes), 0) FROM batches", [], |row| {
                            row.get(0)
                        })
                        .unwrap_or(0);
                    if size.max(0) as u64 <= limit || guard >= 10_000 {
                        break;
                    }
                    guard += 1;
                    let oldest: Option<i64> = conn
                        .query_row(
                            "SELECT batch_id FROM batches ORDER BY start_ns ASC, batch_id ASC LIMIT 1",
                            [],
                            |row| row.get(0),
                        )
                        .ok();
                    let Some(batch_id) = oldest else { break };
                    conn.execute(
                        &format!("DELETE FROM {samples_t} WHERE batch_id = ?"),
                        params![batch_id],
                    )
                    .map_err(db_err)?;
                    conn.execute(
                        &format!("DELETE FROM {batches_t} WHERE batch_id = ?"),
                        params![batch_id],
                    )
                    .map_err(db_err)?;
                    stats.removed_batches += 1;
                }
            }

            conn.execute_batch("CHECKPOINT").map_err(db_err)?;
        }

        // The counts just changed; drop the value `before` cached so both this
        // pass and the caller observe the pruned state.
        *self.stats_cache.lock() = None;
        let after = self.stats();
        // Physical file size stays at its high-water mark after DELETE, so
        // report the logical size actually released instead.
        stats.removed_bytes = before.logical_bytes.saturating_sub(after.logical_bytes);
        Ok(stats)
    }

    /// Delete every row (used by tests and `fsck --fix`).
    pub fn clear(&self) -> Result<()> {
        let samples_t = self.wtable("samples");
        let stacks_t = self.wtable("stacks");
        let frames_t = self.wtable("frames");
        let batches_t = self.wtable("batches");
        let nodes_t = self.wtable("net_nodes");
        let edges_t = self.wtable("net_edges");
        {
            let conn = self.conn.lock();
            conn.execute_batch(&format!(
                "DELETE FROM {samples_t}; DELETE FROM {stacks_t}; DELETE FROM {frames_t}; \
                 DELETE FROM {batches_t}; DELETE FROM {nodes_t}; DELETE FROM {edges_t};"
            ))
            .map_err(db_err)?;
            conn.execute_batch("CHECKPOINT").map_err(db_err)?;
        }
        // In Parquet mode the canonical tables are VIEWs; also drop the local
        // files they read so a clear really empties the store. Archived S3
        // objects are left for bucket lifecycle policies; queries still see
        // nothing here because the view over this tenant's prefix is empty
        // after its local files are removed. For a full purge the operator
        // clears the S3 prefix.
        if let Some(state) = &self.parquet {
            for def in parquet::TABLES {
                let dir = state.table_dir(def.name);
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
            state.clear_manifest()?;
            let conn = self.conn.lock();
            parquet::refresh_views(&conn, state)?;
        }
        self.frame_cache.lock().clear();
        *self.stats_cache.lock() = None;
        let mut ids = self.ids.lock();
        ids.next_frame = 0;
        ids.next_stack = 0;
        ids.next_batch = 0;
        Ok(())
    }

    /// Check that the database is readable and internally consistent.
    ///
    /// A real integrity check of the storage is DuckDB's job (`CHECKPOINT`
    /// fails loudly on a damaged file), so this verifies the invariants only
    /// this schema can violate: samples pointing at a missing stack, and stack
    /// blobs pointing at a missing frame.
    pub fn fsck(&self) -> Vec<(PathBuf, Result<()>)> {
        vec![(self.path(), self.fsck_inner())]
    }

    fn fsck_inner(&self) -> Result<()> {
        let path = self.path();
        let conn = self.conn.lock();
        conn.execute_batch("CHECKPOINT").map_err(db_err)?;

        let orphan_stacks: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM samples s LEFT JOIN stacks k USING (stack_id) \
                 WHERE k.stack_id IS NULL",
                [],
                |row| row.get(0),
            )
            .map_err(db_err)?;
        if orphan_stacks > 0 {
            return Err(Error::corrupt(
                &path,
                format!("{orphan_stacks} samples reference a missing stack"),
            ));
        }

        let (broken, orphan_frames) = self.dangling_stacks(&conn)?;
        if orphan_frames > 0 || !broken.is_empty() {
            return Err(Error::corrupt(
                &path,
                format!(
                    "{orphan_frames} stack frames reference a missing frame id; \
                     {} stack blobs cannot be decoded",
                    broken.len()
                ),
            ));
        }
        Ok(())
    }

    /// Remove rows that break the schema's referential invariants.
    ///
    /// A tenant is a *single* database, so a repair cannot throw the file away
    /// the way a per-file store could: it would destroy the whole tenant.
    /// Returns the number of rows removed.
    pub fn repair(&self) -> Result<usize> {
        let conn = self.conn.lock();
        let (broken, _) = self.dangling_stacks(&conn)?;
        conn.execute_batch("BEGIN TRANSACTION").map_err(db_err)?;

        let deleted = (|| -> Result<usize> {
            let mut removed = 0usize;
            for stack_id in broken {
                let id = stack_id as i64;
                removed += conn
                    .execute("DELETE FROM samples WHERE stack_id = ?", params![id])
                    .map_err(db_err)?;
                removed += conn
                    .execute("DELETE FROM stacks WHERE stack_id = ?", params![id])
                    .map_err(db_err)?;
            }
            // Samples whose stack is missing entirely (the other half of the
            // invariant checked by `fsck`).
            removed += conn
                .execute(
                    "DELETE FROM samples WHERE NOT EXISTS \
                     (SELECT 1 FROM stacks k WHERE k.stack_id = samples.stack_id)",
                    [],
                )
                .map_err(db_err)?;
            Ok(removed)
        })();

        match deleted {
            Ok(removed) => {
                conn.execute_batch("COMMIT; CHECKPOINT").map_err(db_err)?;
                Ok(removed)
            }
            Err(err) => {
                // Never leave the shared connection inside an open transaction.
                let _ = conn.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }

    /// Stacks that cannot be decoded against the frame dictionary, and the
    /// number of dangling frame references found inside them.
    ///
    /// Blobs are opaque to SQL, so the check is done here: walk every stack and
    /// compare its ids against `frames`. Frames are compact enough to hold in
    /// memory; a store with millions of distinct frames makes this slow.
    fn dangling_stacks(&self, conn: &Connection) -> Result<(HashSet<u64>, usize)> {
        let known: HashSet<i64> = {
            let mut stmt = conn
                .prepare("SELECT frame_id FROM frames")
                .map_err(db_err)?;
            let mut rows = stmt.query([]).map_err(db_err)?;
            let mut set = HashSet::new();
            while let Some(row) = rows.next().map_err(db_err)? {
                set.insert(row.get::<_, i64>(0).map_err(db_err)?);
            }
            set
        };

        let mut stmt = conn
            .prepare("SELECT stack_id, frames FROM stacks")
            .map_err(db_err)?;
        let mut rows = stmt.query([]).map_err(db_err)?;
        let mut broken = HashSet::new();
        let mut orphans = 0usize;
        while let Some(row) = rows.next().map_err(db_err)? {
            let stack_id: i64 = row.get(0).map_err(db_err)?;
            let blob: Vec<u8> = row.get(1).map_err(db_err)?;
            // Frame ids are 4 bytes each; a truncated blob cannot be trusted.
            let mut bad = !blob.len().is_multiple_of(4);
            for id in unpack_frames(&blob) {
                if !known.contains(&(id as i64)) {
                    orphans += 1;
                    bad = true;
                }
            }
            if bad {
                broken.insert(stack_id as u64);
            }
        }
        Ok((broken, orphans))
    }
}

/// One service-map window written by an agent.
impl Store {
    /// Persist a service-map delta. Nodes and edges of one window go in one
    /// transaction: a half-written window would draw edges to nothing.
    pub fn append_servicemap(&self, batch: &ServiceMapBatch) -> Result<()> {
        if batch.nodes.is_empty() && batch.edges.is_empty() {
            return Ok(());
        }
        let bucket = ts(batch.start_ns);
        let nodes_t = self.wtable("net_nodes");
        let edges_t = self.wtable("net_edges");
        let conn = self.conn.lock();
        conn.execute_batch("BEGIN TRANSACTION").map_err(db_err)?;
        let result = (|| -> Result<()> {
            if !batch.nodes.is_empty() {
                let mut stmt = conn
                    .prepare(&format!(
                        "INSERT INTO {nodes_t} (bucket_ns, node_id, namespace, workload, service, \
                         kind, k8s_node, rx_bytes, tx_bytes, connections, retransmits, pods, ready, \
                         instances, containers) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                    ))
                    .map_err(db_err)?;
                for node in &batch.nodes {
                    stmt.execute(params![
                        bucket,
                        node.id,
                        node.namespace,
                        node.workload,
                        node.service,
                        node.kind.as_str(),
                        node.node,
                        node.rx_bytes as i64,
                        node.tx_bytes as i64,
                        node.connections as i64,
                        node.retransmits as i64,
                        node.pods as i64,
                        node.ready as i64,
                        serde_json::to_string(&node.instances).unwrap_or_else(|_| "[]".into()),
                        serde_json::to_string(&node.containers).unwrap_or_else(|_| "[]".into()),
                    ])
                    .map_err(db_err)?;
                }
            }
            if !batch.edges.is_empty() {
                let mut stmt = conn
                    .prepare(&format!(
                        "INSERT INTO {edges_t} (bucket_ns, client_id, server_id, protocol, \
                         client_container, server_container, connections, retransmits, \
                         tx_bytes, rx_bytes, rtt_us_sum, rtt_samples, host, sql) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                    ))
                    .map_err(db_err)?;
                for edge in &batch.edges {
                    stmt.execute(params![
                        bucket,
                        edge.client,
                        edge.server,
                        edge.protocol,
                        edge.client_container,
                        edge.server_container,
                        edge.connections as i64,
                        edge.retransmits as i64,
                        edge.tx_bytes as i64,
                        edge.rx_bytes as i64,
                        edge.rtt_us_sum as i64,
                        edge.rtt_samples as i64,
                        edge.host,
                        edge.sql,
                    ])
                    .map_err(db_err)?;
                }
            }
            Ok(())
        })();

        match result {
            Ok(()) => {
                conn.execute_batch("COMMIT").map_err(db_err)?;
                // The map contributes to `total_bytes`; a cached stats value
                // from before this window would undercount it.
                *self.stats_cache.lock() = None;
                Ok(())
            }
            Err(err) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }

    /// Nodes of the map, busiest first.
    pub fn net_nodes(&self, from_ns: u64, until_ns: u64, limit: usize) -> Result<Vec<MapNode>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT node_id, any_value(namespace), any_value(workload), any_value(service), \
                        any_value(kind), sum(rx_bytes), sum(tx_bytes), sum(connections), \
                        sum(retransmits), \
                        arg_max(pods, (bucket_ns, pods)), arg_max(ready, (bucket_ns, pods)), \
                        arg_max(instances, (bucket_ns, pods)), \
                        arg_max(containers, (bucket_ns, pods)) \
                 FROM net_nodes WHERE bucket_ns >= ? AND bucket_ns < ? \
                 GROUP BY node_id \
                 ORDER BY sum(connections) + (sum(rx_bytes) + sum(tx_bytes)) / 1024 DESC LIMIT ?",
            )
            .map_err(db_err)?;
        let mut rows = stmt
            .query(params![ts(from_ns), ts(until_ns), limit as i64])
            .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(db_err)? {
            let kind: String = row.get(4).map_err(db_err)?;
            out.push(MapNode {
                id: row.get(0).map_err(db_err)?,
                namespace: row.get(1).map_err(db_err)?,
                workload: row.get(2).map_err(db_err)?,
                service: row.get(3).map_err(db_err)?,
                kind: NodeKind::parse(&kind),
                rx_bytes: row.get::<_, i64>(5).unwrap_or(0).max(0) as u64,
                tx_bytes: row.get::<_, i64>(6).unwrap_or(0).max(0) as u64,
                connections: row.get::<_, i64>(7).unwrap_or(0).max(0) as u64,
                retransmits: row.get::<_, i64>(8).unwrap_or(0).max(0) as u64,
                cpu_ns: 0,
                // The newest sample in the window, not the largest: a workload
                // that scaled down must not keep reporting its peak replica
                // count. Both columns come from the same row, so the pair is
                // always consistent.
                pods: row.get::<_, i64>(9).unwrap_or(0).max(0) as u32,
                ready: row.get::<_, i64>(10).unwrap_or(0).max(0) as u32,
                instances: row
                    .get::<_, String>(11)
                    .ok()
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default(),
                containers: row
                    .get::<_, String>(12)
                    .ok()
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// Edges of the map, aggregated over the window.
    ///
    /// The `HAVING` keeps an edge that carried traffic but opened no new
    /// connection in the window: a busy long-lived connection has
    /// `connections = 0` and a lot of bytes, and dropping it would make the
    /// map flicker as connections stop being re-established.
    pub fn net_edges(&self, from_ns: u64, until_ns: u64) -> Result<Vec<MapEdge>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT client_id, server_id, protocol, client_container, server_container, \
                        sum(connections), sum(retransmits), \
                        sum(tx_bytes), sum(rx_bytes), sum(rtt_us_sum), sum(rtt_samples), \
                        arg_max(nullif(host, ''), bucket_ns), \
                        arg_max(nullif(sql, ''), bucket_ns) \
                 FROM net_edges WHERE bucket_ns >= ? AND bucket_ns < ? \
                 GROUP BY client_id, server_id, protocol, client_container, server_container \
                 HAVING sum(connections) + sum(tx_bytes) + sum(rx_bytes) > 0 \
                 ORDER BY sum(connections) + sum(tx_bytes) / 1024 DESC LIMIT 4000",
            )
            .map_err(db_err)?;
        let mut rows = stmt
            .query(params![ts(from_ns), ts(until_ns)])
            .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(db_err)? {
            let rtt_us_sum = row.get::<_, i64>(9).unwrap_or(0).max(0) as u64;
            let rtt_samples = row.get::<_, i64>(10).unwrap_or(0).max(0) as u64;
            out.push(MapEdge {
                client: row.get(0).map_err(db_err)?,
                server: row.get(1).map_err(db_err)?,
                protocol: row.get(2).map_err(db_err)?,
                client_container: row.get(3).unwrap_or_default(),
                server_container: row.get(4).unwrap_or_default(),
                connections: row.get::<_, i64>(5).unwrap_or(0).max(0) as u64,
                retransmits: row.get::<_, i64>(6).unwrap_or(0).max(0) as u64,
                tx_bytes: row.get::<_, i64>(7).unwrap_or(0).max(0) as u64,
                rx_bytes: row.get::<_, i64>(8).unwrap_or(0).max(0) as u64,
                // Mean round-trip time over the connections that closed inside
                // the window; zero when none did.
                rtt_us: rtt_us_sum.checked_div(rtt_samples).unwrap_or(0),
                // Old Parquet files predate these columns and read as NULL.
                host: row
                    .get::<_, Option<String>>(11)
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                sql: row
                    .get::<_, Option<String>>(12)
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// One row per bucket of a single node's own counters.
    ///
    /// Slots are computed relative to `from_ns`, not as absolute epoch
    /// multiples: `bucket_ns` is ~1.7e18, which is past the exact integer range
    /// of a double, and DuckDB's `/` returns one. Subtracting `from_ns` first
    /// keeps the numerator inside the window (hours, not decades) so the
    /// division is exact.
    ///
    /// Replicas are a gauge shared by every agent, so a bucket reports the
    /// largest count seen rather than the sum — the same rule [`ServiceNode`]
    /// merging uses.
    pub fn net_node_history(
        &self,
        node: &str,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
    ) -> Result<Vec<MapHistoryPoint>> {
        let step = ts(step_ns.max(1));
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT CAST((bucket_ns - ?) / ? AS BIGINT) AS slot, \
                        sum(rx_bytes), sum(tx_bytes), sum(connections), sum(retransmits), \
                        max(pods), max(ready) \
                 FROM net_nodes WHERE node_id = ? AND bucket_ns >= ? AND bucket_ns < ? \
                 GROUP BY slot ORDER BY slot",
            )
            .map_err(db_err)?;
        let mut rows = stmt
            .query(params![ts(from_ns), step, node, ts(from_ns), ts(until_ns)])
            .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(db_err)? {
            let slot: i64 = row.get(0).map_err(db_err)?;
            out.push(MapHistoryPoint {
                bucket_ns: from_ns
                    .saturating_add((slot.max(0) as u64).saturating_mul(step_ns.max(1))),
                rx_bytes: row.get::<_, i64>(1).unwrap_or(0).max(0) as u64,
                tx_bytes: row.get::<_, i64>(2).unwrap_or(0).max(0) as u64,
                connections: row.get::<_, i64>(3).unwrap_or(0).max(0) as u64,
                retransmits: row.get::<_, i64>(4).unwrap_or(0).max(0) as u64,
                rtt_us: 0,
                pods: row.get::<_, i64>(5).unwrap_or(0).max(0) as u32,
                ready: row.get::<_, i64>(6).unwrap_or(0).max(0) as u32,
            });
        }
        Ok(out)
    }

    /// The same buckets as [`Store::net_node_history`], but seen through the
    /// edges incident to the node.
    ///
    /// `rtt_us` is the sample-weighted mean of the bucket rather than the mean
    /// of the per-edge means: a busy edge and an idle one would otherwise
    /// count the same. Bytes are split by who sent them, which is what makes
    /// "in" and "out" of a node mean the same thing on both sides of an edge.
    pub fn net_edge_history(
        &self,
        node: &str,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
    ) -> Result<Vec<MapHistoryPoint>> {
        let step = ts(step_ns.max(1));
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT CAST((bucket_ns - ?) / ? AS BIGINT) AS slot, \
                        sum(connections), sum(retransmits), \
                        sum(CASE WHEN client_id = ? THEN tx_bytes ELSE rx_bytes END), \
                        sum(CASE WHEN client_id = ? THEN rx_bytes ELSE tx_bytes END), \
                        sum(rtt_us_sum), sum(rtt_samples) \
                 FROM net_edges \
                 WHERE (client_id = ? OR server_id = ?) AND bucket_ns >= ? AND bucket_ns < ? \
                 GROUP BY slot ORDER BY slot",
            )
            .map_err(db_err)?;
        let mut rows = stmt
            .query(params![
                ts(from_ns),
                step,
                node,
                node,
                node,
                node,
                ts(from_ns),
                ts(until_ns)
            ])
            .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(db_err)? {
            let slot: i64 = row.get(0).map_err(db_err)?;
            let rtt_us_sum = row.get::<_, i64>(5).unwrap_or(0).max(0) as u64;
            let rtt_samples = row.get::<_, i64>(6).unwrap_or(0).max(0) as u64;
            out.push(MapHistoryPoint {
                bucket_ns: from_ns
                    .saturating_add((slot.max(0) as u64).saturating_mul(step_ns.max(1))),
                rx_bytes: row.get::<_, i64>(4).unwrap_or(0).max(0) as u64,
                tx_bytes: row.get::<_, i64>(3).unwrap_or(0).max(0) as u64,
                connections: row.get::<_, i64>(1).unwrap_or(0).max(0) as u64,
                retransmits: row.get::<_, i64>(2).unwrap_or(0).max(0) as u64,
                rtt_us: rtt_us_sum.checked_div(rtt_samples).unwrap_or(0),
                pods: 0,
                ready: 0,
            });
        }
        Ok(out)
    }

    /// New connections per bucket across the whole map.
    ///
    /// Unlike the per-node history this is a dense array: the time brush draws
    /// bucket index `i` at `from_ns + i * step_ns`, so an absent bucket must be
    /// a zero rather than a missing row. Buckets are relative to `from_ns` for
    /// the same reason as [`Store::net_node_history`] — the absolute ns value
    /// is far past the exact integer range of a double.
    /// A batch describes a window, not an instant: its counters belong to the
    /// whole `[batch start, next batch start)`. Putting the total on the single
    /// bucket that happens to contain `batch start` makes a five-minute sample
    /// rate look like a spike train once the brush zooms below five minutes —
    /// the 1h view has 30-second buckets and would show ten empty slots between
    /// every bar. Spread the batch over every bucket it overlaps instead, and
    /// let the newest batch run to `until_ns` so the right edge of the brush is
    /// not permanently blank.
    pub fn net_histogram(
        &self,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
        buckets: usize,
    ) -> Result<Vec<u64>> {
        self.net_histogram_metric(from_ns, until_ns, step_ns, buckets, NetMetric::Connections)
    }

    /// The same dense bucket array for a chosen counter.
    ///
    /// The time brush offers the three quantities the map already stores:
    /// how many connections were opened, how many bytes moved, and how many
    /// segments had to be retransmitted. They are summed by the same spreading
    /// rule as [`Store::net_histogram`], so a bar's height means the same span
    /// of time whichever metric is drawn.
    pub fn net_histogram_metric(
        &self,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
        buckets: usize,
        metric: NetMetric,
    ) -> Result<Vec<u64>> {
        let buckets = buckets.max(1);
        let mut out = vec![0u64; buckets];
        if until_ns <= from_ns {
            return Ok(out);
        }
        let step = step_ns.max(1);

        // A batch that started just before `from_ns` still covers the left
        // edge of the window. Look one hour back so that batch is included;
        // the spread below clips it to the visible range.
        const LOOKBACK_NS: u64 = 3_600_000_000_000;
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT bucket_ns, sum({}) FROM net_edges \
                 WHERE bucket_ns >= ? AND bucket_ns < ? \
                 GROUP BY bucket_ns ORDER BY bucket_ns",
                metric.column()
            ))
            .map_err(db_err)?;
        let mut rows = stmt
            .query(params![
                ts(from_ns.saturating_sub(LOOKBACK_NS)),
                ts(until_ns)
            ])
            .map_err(db_err)?;
        let mut batches: Vec<(u64, u64)> = Vec::new();
        while let Some(row) = rows.next().map_err(db_err)? {
            let start: i64 = row.get(0).map_err(db_err)?;
            if start < 0 {
                continue;
            }
            let value = row.get::<_, i64>(1).unwrap_or(0).max(0) as u64;
            batches.push((start as u64, value));
        }
        if batches.is_empty() {
            return Ok(out);
        }

        // A gap much larger than the usual reporting cadence means the agents
        // were down, not that the last batch carried hours of traffic. Cap the
        // spread at twice the median gap so an outage stays visible as zeros.
        let mut gaps: Vec<u64> = batches
            .windows(2)
            .map(|pair| pair[1].0.saturating_sub(pair[0].0))
            .filter(|gap| *gap > 0)
            .collect();
        gaps.sort_unstable();
        // With a single batch there is no cadence to learn from; treat it as
        // covering the requested range so a short query still gets a shape.
        let max_span = match gaps.get(gaps.len() / 2).copied() {
            Some(median) => median.saturating_mul(2).max(step),
            None => batches
                .first()
                .map(|&(start, _)| until_ns.saturating_sub(start))
                .unwrap_or(step)
                .max(step),
        };

        for (index, &(start, value)) in batches.iter().enumerate() {
            if value == 0 {
                continue;
            }
            let next = batches
                .get(index + 1)
                .map(|&(at, _)| at)
                .unwrap_or(until_ns);
            let end = next.min(start.saturating_add(max_span)).min(until_ns);
            let visible_start = start.max(from_ns);
            if end <= visible_start {
                continue;
            }
            // Only the part of the batch that falls inside the query window is
            // counted; the look-back batch may start before it.
            let span = end - start;
            let visible_span = end - visible_start;
            let visible_value = (value as u128 * visible_span as u128 / span as u128) as u64;
            let mut assigned = 0u64;
            let mut cursor = visible_start;
            while cursor < end {
                let slot = ((cursor - from_ns) / step) as usize;
                let slot_end = from_ns + (slot as u64 + 1) * step;
                let segment_end = end.min(slot_end);
                let overlap = segment_end - cursor;
                let mut share = if segment_end == end {
                    visible_value - assigned
                } else {
                    (visible_value as u128 * overlap as u128 / visible_span as u128) as u64
                };
                share = share.min(visible_value - assigned);
                assigned += share;
                if let Some(cell) = out.get_mut(slot) {
                    *cell += share;
                }
                cursor = segment_end;
            }
            // Integer division loses a few connections; give them to the last
            // bucket instead of dropping them from the total.
            if assigned < visible_value {
                let last = ((end - 1 - from_ns) / step) as usize;
                if let Some(cell) = out.get_mut(last) {
                    *cell += visible_value - assigned;
                }
            }
        }
        Ok(out)
    }

    /// On-CPU nanoseconds per map node, so the map can be coloured by cost.
    ///
    /// The join is by label, not by table key: a profile series carries
    /// `k8s_namespace`/`k8s_workload`, which is exactly what [`node_id`]
    /// builds. Series without those labels (host processes) fall back to
    /// `service_name` and land on a node with the same id the agent would use.
    pub fn cpu_by_service(&self, from_ns: u64, until_ns: u64) -> Result<HashMap<String, u64>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT service_name, labels, sum(value) FROM samples \
                 WHERE profile_type = 'cpu' AND start_ns < ? AND end_ns > ? \
                 GROUP BY service_name, labels",
            )
            .map_err(db_err)?;
        let mut rows = stmt
            .query(params![ts(until_ns), ts(from_ns)])
            .map_err(db_err)?;
        let mut out: HashMap<String, u64> = HashMap::new();
        while let Some(row) = rows.next().map_err(db_err)? {
            let service: String = row.get(0).map_err(db_err)?;
            let labels: String = row.get(1).map_err(db_err)?;
            let value: i64 = row.get(2).unwrap_or(0);
            let labels = decode_labels(&labels);
            let namespace = labels
                .get("k8s_namespace")
                .map(String::as_str)
                .unwrap_or("");
            let name = labels
                .get("k8s_workload")
                .or_else(|| labels.get(SERVICE_NAME_LABEL))
                .map(String::as_str)
                .unwrap_or(service.as_str());
            *out.entry(node_id(namespace, name)).or_insert(0) += value.max(0) as u64;
        }
        Ok(out)
    }
}

/// Apply connection-level settings that make large scans faster and bounded.
///
/// Each statement is applied independently: an unavailable PRAGMA on a
/// particular DuckDB build is logged and skipped rather than failing to open
/// the database. The settings that matter most here:
///
/// * `preserve_insertion_order=false` lets DuckDB reorder grouped aggregates
///   and drop the final sort — a large win for the scan/GROUP BY queries that
///   back flamegraphs and the service map.
/// * `memory_limit` caps the hash tables behind long GROUP BYs so a wide range
///   spills to disk instead of growing until the process is OOM-killed.
/// * `temp_directory` gives those spills a known scratch location.
fn tune_connection(conn: &Connection, path: &Path, cfg: &StoreConfig) -> Result<()> {
    let settings: Vec<String> = vec![
        "SET preserve_insertion_order=false".to_string(),
        // DuckDB defaults to 80% of host RAM, which is dangerous in a 2 GiB
        // pod. Cap the hash tables behind long GROUP BYs so wide ranges spill
        // to the temp directory instead of getting the process OOM-killed.
        format!("SET memory_limit='{}'", cfg.duckdb_memory_limit()),
        format!(
            "SET temp_directory='{}'",
            path.parent()
                .unwrap_or_else(|| Path::new("."))
                .join(".duckdb-tmp")
                .display()
        ),
    ];
    // Thread count is pinned through duckdb::Config at open time (see
    // Store::open); no need to issue a redundant SET here.
    for sql in settings {
        if let Err(err) = conn.execute_batch(&sql) {
            warn!("skipping DuckDB setting `{sql}`: {err}");
        }
    }
    Ok(())
}

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS batches (
            batch_id BIGINT PRIMARY KEY,
            profile_type VARCHAR NOT NULL,
            service_name VARCHAR NOT NULL,
            labels VARCHAR NOT NULL,
            start_ns BIGINT NOT NULL,
            end_ns BIGINT NOT NULL,
            total_value BIGINT NOT NULL,
            sample_count BIGINT NOT NULL,
            bytes BIGINT NOT NULL,
            created_ns BIGINT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS samples (
            batch_id BIGINT NOT NULL,
            profile_type VARCHAR NOT NULL,
            service_name VARCHAR NOT NULL,
            labels VARCHAR NOT NULL,
            start_ns BIGINT NOT NULL,
            end_ns BIGINT NOT NULL,
            stack_id BIGINT NOT NULL,
            value BIGINT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS stacks (
            stack_id BIGINT PRIMARY KEY,
            frames BLOB NOT NULL
        );
        CREATE TABLE IF NOT EXISTS frames (
            frame_id INTEGER PRIMARY KEY,
            module VARCHAR NOT NULL,
            symbol VARCHAR NOT NULL,
            file VARCHAR NOT NULL,
            line INTEGER NOT NULL
        );
        /* Service map: one row per observation window. Agents ship deltas, so
         * a query is a plain SUM over the buckets the window overlaps. */
        CREATE TABLE IF NOT EXISTS net_nodes (
            bucket_ns BIGINT NOT NULL,
            node_id VARCHAR NOT NULL,
            namespace VARCHAR NOT NULL,
            workload VARCHAR NOT NULL,
            service VARCHAR NOT NULL,
            kind VARCHAR NOT NULL,
            k8s_node VARCHAR NOT NULL,
            rx_bytes BIGINT NOT NULL,
            tx_bytes BIGINT NOT NULL,
            connections BIGINT NOT NULL,
            retransmits BIGINT NOT NULL,
            pods BIGINT DEFAULT 0 NOT NULL,
            ready BIGINT DEFAULT 0 NOT NULL,
            /* The replicas themselves, as a JSON array of name / node / ready /
             * phase objects. Bounded by the agent; the map only needs it for
             * the panel behind a click. */
            instances VARCHAR DEFAULT '[]' NOT NULL,
            /* Container names of the workload's pods, as a JSON array. The
             * panel gives each container its own row, so a sidecar's traffic
             * does not disappear into the pod's total. */
            containers VARCHAR DEFAULT '[]' NOT NULL
        );
        CREATE TABLE IF NOT EXISTS net_edges (
            bucket_ns BIGINT NOT NULL,
            client_id VARCHAR NOT NULL,
            server_id VARCHAR NOT NULL,
            protocol VARCHAR NOT NULL,
            client_container VARCHAR DEFAULT '' NOT NULL,
            server_container VARCHAR DEFAULT '' NOT NULL,
            connections BIGINT NOT NULL,
            retransmits BIGINT NOT NULL,
            tx_bytes BIGINT NOT NULL,
            rx_bytes BIGINT NOT NULL,
            rtt_us_sum BIGINT NOT NULL,
            rtt_samples BIGINT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS meta (key VARCHAR PRIMARY KEY, value VARCHAR NOT NULL);
        INSERT OR REPLACE INTO meta VALUES ('schema_version', '{SCHEMA_VERSION}');"
    ))
    .map_err(db_err)?;
    add_missing_columns(conn)?;
    create_indexes(conn)
}

/// Indexes the columns every read path filters or groups on. DuckDB keeps
/// min/max zone maps per row group, but analyst queries that repeatedly pin a
/// bucket or a node still benefit from an ART index — especially on the wide
/// time ranges that were slow before. Created defensively so an older database
/// or a build without ART support still opens.
fn create_indexes(conn: &Connection) -> Result<()> {
    let indexes = [
        "CREATE INDEX IF NOT EXISTS idx_net_edges_bucket ON net_edges(bucket_ns)",
        "CREATE INDEX IF NOT EXISTS idx_net_edges_node ON net_edges(client_id, server_id)",
        "CREATE INDEX IF NOT EXISTS idx_net_nodes_bucket ON net_nodes(bucket_ns)",
        "CREATE INDEX IF NOT EXISTS idx_net_nodes_node ON net_nodes(node_id)",
        "CREATE INDEX IF NOT EXISTS idx_samples_time ON samples(profile_type, start_ns, end_ns)",
        "CREATE INDEX IF NOT EXISTS idx_batches_time ON batches(profile_type, start_ns, end_ns)",
    ];
    for sql in indexes {
        if let Err(err) = conn.execute_batch(sql) {
            warn!("skipping index `{sql}`: {err}");
        }
    }
    Ok(())
}

/// Add the columns a store written by an older release does not have yet.
///
/// `CREATE TABLE IF NOT EXISTS` leaves an existing table alone, so a database
/// that predates a column keeps missing it until it is added here. The catalog
/// is asked rather than relying on `ALTER TABLE ... ADD COLUMN IF NOT EXISTS`,
/// whose availability depends on the DuckDB build the agent was linked with.
fn add_missing_columns(conn: &Connection) -> Result<()> {
    add_columns(
        conn,
        "net_edges",
        &[
            ("tx_bytes", "BIGINT DEFAULT 0 NOT NULL"),
            ("rx_bytes", "BIGINT DEFAULT 0 NOT NULL"),
            ("rtt_us_sum", "BIGINT DEFAULT 0 NOT NULL"),
            ("rtt_samples", "BIGINT DEFAULT 0 NOT NULL"),
            ("client_container", "VARCHAR DEFAULT '' NOT NULL"),
            ("server_container", "VARCHAR DEFAULT '' NOT NULL"),
            ("host", "VARCHAR DEFAULT '' NOT NULL"),
            ("sql", "VARCHAR DEFAULT '' NOT NULL"),
        ],
    )?;
    add_columns(
        conn,
        "net_nodes",
        &[
            ("pods", "BIGINT DEFAULT 0 NOT NULL"),
            ("ready", "BIGINT DEFAULT 0 NOT NULL"),
            ("instances", "VARCHAR DEFAULT '[]' NOT NULL"),
            ("containers", "VARCHAR DEFAULT '[]' NOT NULL"),
        ],
    )
}

fn add_columns(conn: &Connection, table: &str, added: &[(&str, &str)]) -> Result<()> {
    let mut existing: Vec<String> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_name = ?",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![table], |row| row.get::<_, String>(0))
            .map_err(db_err)?;
        for row in rows {
            existing.push(row.map_err(db_err)?);
        }
    }
    for (column, definition) in added {
        if existing.iter().any(|name| name == column) {
            continue;
        }
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))
        .map_err(db_err)?;
    }
    Ok(())
}

/// Render the parts of a selector that SQL can express.
///
/// Only `service_name` has a column of its own; the other matchers would need
/// substring matching on the encoded label string, which cannot express
/// `label != value` correctly (the label may simply be absent). Those run in
/// Rust after the scan. `service_name` is included because a dashboard almost
/// always filters on it and it is the column with the most distinct values.
fn push_selector_sql(
    selector: &Selector,
    clauses: &mut Vec<String>,
    args: &mut Vec<Box<dyn duckdb::ToSql>>,
) {
    for matcher in &selector.matchers {
        if matcher.label != SERVICE_NAME_LABEL {
            continue;
        }
        match matcher.op {
            MatchOp::Eq => {
                clauses.push("service_name = ?".into());
                args.push(Box::new(matcher.value.clone()));
            }
            MatchOp::NotEq => {
                clauses.push("service_name <> ?".into());
                args.push(Box::new(matcher.value.clone()));
            }
            MatchOp::Glob => {
                clauses.push("service_name LIKE ?".into());
                args.push(Box::new(glob_to_like(&matcher.value)));
            }
            MatchOp::NotGlob => {
                clauses.push("service_name NOT LIKE ?".into());
                args.push(Box::new(glob_to_like(&matcher.value)));
            }
        }
    }
}

/// `*` -> `%`, `?` -> `_`, with LIKE metacharacters escaped.
fn glob_to_like(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    for ch in pattern.chars() {
        match ch {
            '*' => out.push('%'),
            '?' => out.push('_'),
            '%' => out.push_str("\\%"),
            '_' => out.push_str("\\_"),
            c => out.push(c),
        }
    }
    out
}

fn db_err(err: duckdb::Error) -> Error {
    Error::Db(err.to_string())
}

/// Clamp a nanosecond timestamp into DuckDB's `BIGINT`.
///
/// A query written as `until = u64::MAX` ("everything") would otherwise
/// become `-1` and match nothing at all.
fn ts(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

/// Version of the linked DuckDB engine, as the library reports it.
///
/// Asked of the library rather than hard-coded, so `lidlesseye info` cannot
/// claim a version the binary does not actually link.
pub fn duckdb_version() -> String {
    // SAFETY: `duckdb_library_version` returns a pointer to a static,
    // NUL-terminated string owned by the library.
    let ptr = unsafe { duckdb::ffi::duckdb_library_version() };
    if ptr.is_null() {
        return "unknown".to_string();
    }
    unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Pack frame ids into a little-endian blob.
fn pack_frames(frames: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frames.len() * 4);
    for id in frames {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}

fn unpack_frames(blob: &[u8]) -> Vec<u32> {
    blob.as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_le_bytes(*chunk))
        .collect()
}

/// Encode a label set as `k=v,k2=v2`, sorted by key.
///
/// Values may contain `,`, `=` or `\`, so those are escaped. Encoding is
/// deterministic, which matters because the encoded string is part of the
/// series identity used by `contains_series` and by tenant series limits.
pub fn encode_labels(labels: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (i, (key, value)) in labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        escape_into(&mut out, key);
        out.push('=');
        escape_into(&mut out, value);
    }
    out
}

fn escape_into(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ',' => out.push_str("\\,"),
            '=' => out.push_str("\\="),
            c => out.push(c),
        }
    }
}

/// Inverse of [`encode_labels`].
pub fn decode_labels(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut key = String::new();
    let mut value = String::new();
    let mut in_value = false;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            if in_value {
                value.push(ch);
            } else {
                key.push(ch);
            }
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '=' if !in_value => in_value = true,
            ',' => {
                if !key.is_empty() {
                    out.insert(std::mem::take(&mut key), std::mem::take(&mut value));
                }
                in_value = false;
            }
            c => {
                if in_value {
                    value.push(c);
                } else {
                    key.push(c);
                }
            }
        }
    }
    if !key.is_empty() {
        out.insert(key, value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Frame, ProfileType, ReportBuilder};

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("lidlesseye-store-{name}-{}", uuid::Uuid::new_v4()));
        let store = Store::open(StoreConfig::new(&dir)).unwrap();
        (store, dir)
    }

    fn report(service: &str, start: u64, pod: &str, frames: &[&str], value: u64) -> Report {
        let mut b =
            ReportBuilder::new(service, "cpu", start, start + 10_000_000_000).label("pod", pod);
        b.add_stack(
            frames.iter().map(|name| Frame::new("/bin/app", *name)),
            value,
        );
        b.finish()
    }

    #[test]
    fn labels_round_trip_through_the_encoding() {
        let mut labels = BTreeMap::new();
        labels.insert("pod".to_string(), "a,b=c".to_string());
        labels.insert("path".to_string(), "C://x//y".to_string());
        labels.insert("empty".to_string(), String::new());

        let encoded = encode_labels(&labels);
        // Sorting by key makes the encoding a stable series identity.
        assert!(encoded.starts_with("empty=,path="), "{encoded}");
        assert_eq!(decode_labels(&encoded), labels);

        assert_eq!(decode_labels(""), BTreeMap::new());
        assert_eq!(decode_labels("a=1").get("a").map(String::as_str), Some("1"));
    }

    #[test]
    fn append_and_select() {
        let (store, dir) = temp_store("append");
        store
            .append(&report("checkout", 1_000, "a", &["hot", "main"], 1_000))
            .unwrap();
        store
            .append(&report("checkout", 2_000, "b", &["hot", "main"], 1_000))
            .unwrap();
        store
            .append(&report("cart", 3_000, "c", &["hot", "main"], 1_000))
            .unwrap();

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

        // Reopening reads the same rows out of the database file.
        let store2 = Store::open(StoreConfig::new(&dir)).unwrap();
        assert_eq!(store2.stats().batch_count, 3);
        assert_eq!(store2.stats().total_value, 3_000);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn physical_size_is_reported_while_the_data_sits_in_the_wal() {
        let (store, dir) = temp_store("disk-bytes");
        // Nothing has checkpointed or pruned yet, so every byte is in the
        // write-ahead log — the window in which `pragma_database_size()`
        // reported 0 and the dashboard showed "0 B" next to a non-zero
        // logical size.
        store
            .append(&report("checkout", 1_000, "a", &["hot", "main"], 1_000))
            .unwrap();

        let stats = store.stats();
        assert!(stats.logical_bytes > 0, "{stats:?}");
        assert!(stats.total_bytes > 0, "{stats:?}");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scan_applies_label_matchers_not_just_service_name() {
        // The SQL pass can only filter on `service_name`; the label matchers
        // have to run afterwards or a `pod` filter would silently return
        // every pod of the service.
        let (store, dir) = temp_store("scan-labels");
        store
            .append(&report("api", 1_000, "a", &["work", "main"], 100))
            .unwrap();
        store
            .append(&report("api", 2_000, "b", &["work", "main"], 200))
            .unwrap();

        let selector = Selector::parse(r#"{pod="a"}"#).unwrap();
        let scan = store
            .scan(&ScanQuery::new(&ProfileType::cpu(), 0, 10_000, &selector))
            .unwrap();
        let total: u64 = scan.rows.iter().map(|r| r.value).sum();
        assert_eq!(total, 100, "only pod a should contribute");

        // A glob must be honoured too, and `!=` must exclude.
        let glob = Selector::parse(r#"{pod=~"b*"}"#).unwrap();
        let scan = store
            .scan(&ScanQuery::new(&ProfileType::cpu(), 0, 10_000, &glob))
            .unwrap();
        assert_eq!(scan.rows.iter().map(|r| r.value).sum::<u64>(), 200);

        let not_a = Selector::parse(r#"{pod!="a"}"#).unwrap();
        let scan = store
            .scan(&ScanQuery::new(&ProfileType::cpu(), 0, 10_000, &not_a))
            .unwrap();
        assert_eq!(scan.rows.iter().map(|r| r.value).sum::<u64>(), 200);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scan_reports_the_group_label() {
        let (store, dir) = temp_store("scan-group");
        store
            .append(&report("api", 1_000, "a", &["work", "main"], 100))
            .unwrap();
        store
            .append(&report("api", 2_000, "b", &["work", "main"], 200))
            .unwrap();

        let scan = store
            .scan(
                &ScanQuery::new(&ProfileType::cpu(), 0, 10_000, &Selector::default())
                    .with_group_by(Some("pod")),
            )
            .unwrap();
        let mut groups: Vec<_> = scan.rows.iter().filter_map(|r| r.group.clone()).collect();
        groups.sort();
        assert_eq!(groups, vec!["pod=a", "pod=b"]);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn identical_frames_are_stored_once() {
        let (store, dir) = temp_store("frames");
        // Two functions in the same binary, and the same functions again in a
        // second batch: the dictionary must hold three frames, not six.
        store
            .append(&report("api", 1_000, "a", &["hot", "main"], 10))
            .unwrap();
        store
            .append(&report("api", 2_000, "a", &["hot", "main"], 10))
            .unwrap();

        let conn = store.conn.lock();
        let frames: i64 = conn
            .query_row("SELECT COUNT(*) FROM frames", [], |row| row.get(0))
            .unwrap();
        let stacks: i64 = conn
            .query_row("SELECT COUNT(*) FROM stacks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(frames, 2, "a frame must be stored once per database");
        // Stacks are deduplicated per batch, so two batches hold two stacks.
        assert_eq!(stacks, 2);
        drop(conn);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retention_and_the_disk_cap_remove_old_batches() {
        let hour = 3_600_000_000_000u64;
        let (store, dir) = temp_store("retention");
        store
            .append(&report("api", 1_000, "a", &["hot"], 10))
            .unwrap();
        store
            .append(&report("api", hour, "a", &["hot"], 10))
            .unwrap();
        // DuckDB locks the file, so a second `Store` on the same path is an
        // error rather than a second reader: reopen only after dropping.
        drop(store);

        let mut cfg = StoreConfig::new(&dir);
        cfg.retention_ns = Some(hour + hour / 2);
        let store = Store::open(cfg).unwrap();
        let stats = store.prune(2 * hour).unwrap();
        assert_eq!(stats.removed_batches, 1);
        assert_eq!(store.stats().batch_count, 1);
        drop(store);

        // The cap is measured on retained data, not on the file, which DuckDB
        // does not shrink.
        let mut cfg = StoreConfig::new(&dir);
        cfg.max_disk_bytes = Some(1);
        let store = Store::open(cfg).unwrap();
        store
            .append(&report("web", 3 * hour, "b", &["hot"], 10))
            .unwrap();
        assert!(store.stats().batch_count >= 1);
        store.prune(3 * hour).unwrap();
        assert_eq!(
            store.stats().batch_count,
            0,
            "everything exceeds a 1 byte cap"
        );

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn series_and_label_values_aggregate() {
        let (store, dir) = temp_store("series");
        store
            .append(&report("api", 1_000, "a", &["hot"], 100))
            .unwrap();
        store
            .append(&report("api", 2_000, "a", &["hot"], 100))
            .unwrap();
        store
            .append(&report("web", 3_000, "b", &["hot"], 100))
            .unwrap();

        let series = store.series(0, 10_000, &Selector::default());
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].service_name, "api");
        assert_eq!(series[0].batch_count, 2);
        assert_eq!(series[0].total_value, 200);

        let pods = store.label_values("pod", 0, 10_000, &Selector::default());
        assert_eq!(pods.len(), 2);
        assert_eq!(pods[0].0, "a");

        let names = store.label_names(0, 10_000, &Selector::default());
        assert!(names.contains(&"pod".to_string()));
        assert!(names.contains(&SERVICE_NAME_LABEL.to_string()));

        assert_eq!(store.series_count(), 2);
        assert!(store.contains_series(&report("api", 5_000, "a", &["hot"], 1)));
        assert!(!store.contains_series(&report("nope", 5_000, "a", &["hot"], 1)));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn glob_patterns_become_like_patterns() {
        assert_eq!(glob_to_like("checkout-*"), "checkout-%");
        assert_eq!(glob_to_like("a?c"), "a_c");
        // LIKE metacharacters must not leak through as wildcards.
        assert_eq!(glob_to_like("100%"), "100\\%");
        assert_eq!(glob_to_like("a_b"), "a\\_b");
    }

    #[test]
    fn timestamps_do_not_wrap_at_the_top_of_u64() {
        assert_eq!(ts(0), 0);
        assert_eq!(ts(1_700_000_000_000_000_000), 1_700_000_000_000_000_000);
        assert_eq!(ts(u64::MAX), i64::MAX);
    }

    #[test]
    fn clearing_the_store_empties_every_table() {
        let (store, dir) = temp_store("clear");
        store
            .append(&report("api", 1_000, "a", &["hot"], 10))
            .unwrap();
        store.clear().unwrap();
        let stats = store.stats();
        assert_eq!(stats.batch_count, 0);
        assert_eq!(stats.total_value, 0);
        assert!(
            store
                .scan(&ScanQuery::new(
                    &ProfileType::cpu(),
                    0,
                    10_000,
                    &Selector::default()
                ))
                .unwrap()
                .rows
                .is_empty()
        );
        assert!(store.fsck().iter().all(|(_, r)| r.is_ok()));

        std::fs::remove_dir_all(dir).unwrap();
    }

    /// `--fix` must drop the bad rows, not the file: there is one database per
    /// tenant, and deleting it would delete everything.
    #[test]
    fn repair_removes_dangling_rows_and_keeps_good_batches() {
        let (store, dir) = temp_store("repair");
        store
            .append(&report("api", 1_000, "a", &["hot"], 10))
            .unwrap();
        let before = store.stats().batch_count;

        {
            let conn = store.conn.lock();
            conn.execute(
                "INSERT INTO samples VALUES (999, 'cpu', 'api', '', 1000, 2000, 999999, 7)",
                [],
            )
            .unwrap();
            // A truncated blob: not a whole number of 4-byte frame ids.
            conn.execute(
                "INSERT INTO stacks VALUES (999998, ?)",
                params![vec![1u8, 2, 3]],
            )
            .unwrap();
        }

        assert!(
            store.fsck()[0].1.is_err(),
            "fsck must report the dangling sample"
        );
        let removed = store.repair().unwrap();
        assert!(
            removed >= 2,
            "expected at least two rows, removed {removed}"
        );
        assert!(store.fsck().iter().all(|(_, r)| r.is_ok()));
        assert_eq!(store.stats().batch_count, before, "good batch survives");
        assert!(store.path().is_file(), "database file survives");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn map_replicas_come_from_the_newest_window_not_the_peak() {
        use crate::servicemap::ServiceNode;

        let (store, dir) = temp_store("net-replicas");
        let start = 1_700_000_000_000_000_000u64;
        let window = |start_ns: u64, pods: u32, ready: u32| ServiceMapBatch {
            start_ns,
            end_ns: start_ns + 10_000_000_000,
            nodes: vec![ServiceNode {
                pods,
                ready,
                ..ServiceNode::new("shop/a", NodeKind::Workload)
            }],
            ..Default::default()
        };
        // A rollout that scaled down: three replicas, then one. Summing the
        // gauges would report six, and taking the maximum would keep the peak.
        store.append_servicemap(&window(start, 3, 3)).unwrap();
        store
            .append_servicemap(&window(start + 10_000_000_000, 1, 1))
            .unwrap();
        // Two agents write the same window, and one of them has not learned
        // the workload yet: the tie must not report "no replicas" because the
        // empty row happened to be read last.
        store
            .append_servicemap(&window(start + 20_000_000_000, 1, 1))
            .unwrap();
        store
            .append_servicemap(&window(start + 20_000_000_000, 0, 0))
            .unwrap();

        let nodes = store.net_nodes(start, start + 60_000_000_000, 10).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!((nodes[0].pods, nodes[0].ready), (1, 1));

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Both container columns have to survive the store: the panel groups by
    /// them, and a column that is written but never read back would silently
    /// collapse a sidecar into its workload.
    #[test]
    fn map_containers_round_trip_through_the_store() {
        use crate::servicemap::{ServiceEdge, ServiceNode};

        let (store, dir) = temp_store("net-containers");
        let start = 1_700_000_000_000_000_000u64;
        store
            .append_servicemap(&ServiceMapBatch {
                start_ns: start,
                end_ns: start + 10_000_000_000,
                nodes: vec![
                    ServiceNode {
                        containers: vec!["app".into(), "istio-proxy".into()],
                        ..ServiceNode::new("shop/a", NodeKind::Workload)
                    },
                    // An edge whose peer is not a node would be dropped by the
                    // query, and this test is about the edge.
                    ServiceNode::new("shop/b", NodeKind::Workload),
                ],
                edges: vec![ServiceEdge {
                    client: "shop/a".into(),
                    server: "shop/b".into(),
                    protocol: "http".into(),
                    client_container: "istio-proxy".into(),
                    server_container: "app".into(),
                    connections: 4,
                    ..Default::default()
                }],
                ..Default::default()
            })
            .unwrap();

        let map = crate::servicemap(&store, start, start + 10_000_000_000).unwrap();
        let a = map
            .nodes
            .iter()
            .find(|node| node.id == "shop/a")
            .expect("shop/a is on the map");
        assert_eq!(a.containers, ["app", "istio-proxy"]);
        assert_eq!(map.edges.len(), 1);
        assert_eq!(map.edges[0].client_container, "istio-proxy");
        assert_eq!(map.edges[0].server_container, "app");

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A payload sample lands in one window; the next report for the same edge
    /// carries nothing, because not every window catches a request. Taking the
    /// newest row's value would blank the host one window later, which is what
    /// made a captured host disappear from the map.
    #[test]
    fn map_detail_survives_a_window_that_carried_no_sample() {
        use crate::servicemap::{ServiceEdge, ServiceNode};

        let (store, dir) = temp_store("net-detail-window");
        let start = 1_700_000_000_000_000_000u64;
        let report = |start_ns: u64, host: &str, sql: &str| ServiceMapBatch {
            start_ns,
            end_ns: start_ns + 10_000_000_000,
            nodes: vec![
                ServiceNode::new("shop/a", NodeKind::Workload),
                ServiceNode::new("shop/b", NodeKind::Workload),
            ],
            edges: vec![ServiceEdge {
                client: "shop/a".into(),
                server: "shop/b".into(),
                protocol: "http".into(),
                connections: 1,
                host: host.into(),
                sql: sql.into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        store
            .append_servicemap(&report(start, "checkout.example.com", "select 1"))
            .unwrap();
        store
            .append_servicemap(&report(start + 10_000_000_000, "", ""))
            .unwrap();

        let edges = store
            .net_edges(start, start + 20_000_000_000)
            .expect("the window is readable");
        assert_eq!(edges.len(), 1, "one edge, two windows");
        assert_eq!(edges[0].host, "checkout.example.com");
        assert_eq!(edges[0].sql, "select 1");

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A panel that draws a shape needs buckets, and the two series it draws
    /// come from different tables: counters from `net_nodes`, latency from
    /// `net_edges`. This pins down both the bucketing and the two aggregation
    /// rules that are easy to get wrong — counters add, a gauge does not, and
    /// latency is weighted by sample count rather than averaged per edge.
    #[test]
    fn node_history_buckets_counters_and_weights_latency_by_samples() {
        use crate::servicemap::{ServiceEdge, ServiceNode};

        let (store, dir) = temp_store("net-history");
        let start = 1_700_000_000_000_000_000u64;
        let step = 10_000_000_000u64;
        let batch = |offset: u64, pods: u32, ready: u32, connections: u64, rtt: (u64, u64)| {
            ServiceMapBatch {
                start_ns: start + offset,
                end_ns: start + offset + step,
                nodes: vec![ServiceNode {
                    connections,
                    pods,
                    ready,
                    ..ServiceNode::new("shop/a", NodeKind::Workload)
                }],
                edges: vec![ServiceEdge {
                    client: "shop/a".to_string(),
                    server: "shop/b".to_string(),
                    protocol: "http".to_string(),
                    connections,
                    tx_bytes: 1000 * connections,
                    rx_bytes: 2000 * connections,
                    rtt_us_sum: rtt.0,
                    rtt_samples: rtt.1,
                    ..Default::default()
                }],
                ..Default::default()
            }
        };
        // Two agents report the same window: counters double, replicas do not.
        for _ in 0..2 {
            store
                .append_servicemap(&batch(0, 3, 3, 5, (100, 2)))
                .unwrap();
        }
        // A later bucket, after a scale-down.
        store
            .append_servicemap(&batch(step, 1, 0, 1, (900, 2)))
            .unwrap();

        let points = store
            .net_node_history("shop/a", start, start + 2 * step, step)
            .unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].bucket_ns, start);
        assert_eq!(points[0].connections, 10, "counters add across agents");
        assert_eq!(
            (points[0].pods, points[0].ready),
            (3, 3),
            "a gauge must not be summed per agent"
        );
        assert_eq!(points[1].bucket_ns, start + step);
        assert_eq!((points[1].pods, points[1].ready), (1, 0));

        let edges = store
            .net_edge_history("shop/a", start, start + 2 * step, step)
            .unwrap();
        assert_eq!(edges.len(), 2);
        // 50us over two samples, doubled because two agents reported it.
        assert_eq!(edges[0].rtt_us, 50);
        assert_eq!(edges[0].tx_bytes, 2 * 5000);
        assert_eq!(edges[0].rx_bytes, 2 * 10000);
        assert_eq!(edges[1].rtt_us, 450);

        // Asking for a narrower step re-buckets the same rows.
        let coarse = store
            .net_node_history("shop/a", start, start + 2 * step, 2 * step)
            .unwrap();
        assert_eq!(coarse.len(), 1);
        assert_eq!(coarse[0].connections, 11);
        assert_eq!(
            (coarse[0].pods, coarse[0].ready),
            (3, 3),
            "a gauge inside a bucket reports its peak, not its sum"
        );

        // The whole-map histogram the time brush draws: dense, and summing the
        // same edge counters the per-node history showed.
        let histogram = store
            .net_histogram(start, start + 2 * step, step, 2)
            .unwrap();
        assert_eq!(histogram.len(), 2, "the brush gets one entry per bucket");
        assert_eq!(histogram[0], edges[0].connections);
        assert_eq!(histogram[1], edges[1].connections);

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A batch describes a window, so a five-minute sample must light up every
    /// sub-bucket it covers. Bucketing by `batch start` alone leaves the 1h
    /// brush (30-second buckets) with ten empty slots between bars.
    #[test]
    fn histogram_spreads_a_batch_over_the_buckets_it_covers() {
        use crate::servicemap::ServiceEdge;

        let (store, dir) = temp_store("net-histogram-spread");
        let start = 1_700_000_000_000_000_000u64;
        let batch_ns = 60_000_000_000u64;
        store
            .append_servicemap(&ServiceMapBatch {
                start_ns: start,
                end_ns: start + batch_ns,
                edges: vec![ServiceEdge {
                    client: "shop/a".to_string(),
                    server: "shop/b".to_string(),
                    protocol: "http".to_string(),
                    connections: 600,
                    ..Default::default()
                }],
                ..Default::default()
            })
            .unwrap();

        // 120 buckets over a one-minute batch = 500ms each.
        let histogram = store
            .net_histogram(start, start + batch_ns, batch_ns / 120, 120)
            .unwrap();
        assert_eq!(histogram.len(), 120);
        assert!(
            histogram.iter().all(|value| *value > 0),
            "every bucket inside the batch must carry traffic: {histogram:?}"
        );
        assert_eq!(
            histogram.iter().sum::<u64>(),
            600,
            "spreading must conserve the batch total"
        );

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A query that starts in the middle of a batch must still see that
    /// batch's share of the window: the brush's left edge is a time, not a
    /// batch boundary.
    #[test]
    fn histogram_clips_a_batch_that_started_before_the_window() {
        use crate::servicemap::ServiceEdge;

        let (store, dir) = temp_store("net-histogram-clip");
        let start = 1_700_000_000_000_000_000u64;
        let batch_ns = 60_000_000_000u64;
        store
            .append_servicemap(&ServiceMapBatch {
                start_ns: start,
                end_ns: start + batch_ns,
                edges: vec![ServiceEdge {
                    client: "shop/a".to_string(),
                    server: "shop/b".to_string(),
                    protocol: "http".to_string(),
                    connections: 600,
                    ..Default::default()
                }],
                ..Default::default()
            })
            .unwrap();

        // Second half of the batch, 30 one-second buckets.
        let histogram = store
            .net_histogram(start + batch_ns / 2, start + batch_ns, 1_000_000_000, 30)
            .unwrap();
        assert!(
            histogram.iter().all(|value| *value > 0),
            "the half-open left edge must still carry traffic: {histogram:?}"
        );
        assert_eq!(
            histogram.iter().sum::<u64>(),
            300,
            "only the visible half of the batch counts"
        );

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The brush's metric switch reads the same batches through a different
    /// aggregate. Bytes count both directions; retransmits are their own
    /// counter. Both must spread over the window exactly like connections do,
    /// or the three views would disagree about when the traffic happened.
    #[test]
    fn histogram_offers_bytes_and_retransmits() {
        use crate::servicemap::ServiceEdge;

        let (store, dir) = temp_store("net-histogram-metrics");
        let start = 1_700_000_000_000_000_000u64;
        let batch_ns = 60_000_000_000u64;
        store
            .append_servicemap(&ServiceMapBatch {
                start_ns: start,
                end_ns: start + batch_ns,
                edges: vec![ServiceEdge {
                    client: "shop/a".to_string(),
                    server: "shop/b".to_string(),
                    protocol: "http".to_string(),
                    connections: 60,
                    tx_bytes: 1_200,
                    rx_bytes: 3_600,
                    retransmits: 6,
                    ..Default::default()
                }],
                ..Default::default()
            })
            .unwrap();

        let bytes = store
            .net_histogram_metric(start, start + batch_ns, batch_ns / 12, 12, NetMetric::Bytes)
            .unwrap();
        assert_eq!(bytes.len(), 12);
        assert_eq!(
            bytes.iter().sum::<u64>(),
            4_800,
            "bytes are both directions, spread over the batch"
        );

        let retransmits = store
            .net_histogram_metric(
                start,
                start + batch_ns,
                batch_ns / 12,
                12,
                NetMetric::Retransmits,
            )
            .unwrap();
        assert_eq!(retransmits.iter().sum::<u64>(), 6);

        // An unknown metric name must not silently mean "bytes": parse is the
        // one place that decides, and the API falls back to connections.
        assert_eq!(NetMetric::parse("requests"), Some(NetMetric::Connections));
        assert_eq!(NetMetric::parse("nope"), None);

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The performance settings and indexes must actually take effect — a
    /// silently skipped PRAGMA would leave long scans slow without any signal.
    #[test]
    fn performance_settings_and_indexes_are_applied() {
        let (store, dir) = temp_store("tuning");
        let conn = store.conn.lock();

        // DuckDB exposes this setting as a BOOLEAN, not a string.
        let insertion_order: bool = conn
            .query_row(
                "SELECT current_setting('preserve_insertion_order')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!insertion_order, "reordering must be enabled");

        let memory_limit: String = conn
            .query_row("SELECT current_setting('memory_limit')", [], |row| {
                row.get(0)
            })
            .unwrap();
        // DuckDB normalises "1GB" to ~953.6 MiB (10^9 bytes). Parse the number
        // and assert it is a small absolute cap, not the 80%-of-host default.
        let mib: f64 = memory_limit
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        assert!(
            mib > 0.0 && mib < 1500.0,
            "a conservative memory cap must be set, got {memory_limit}"
        );

        let mut stmt = conn
            .prepare("SELECT index_name FROM duckdb_indexes() WHERE index_name LIKE 'idx_%'")
            .unwrap();
        let indexes: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|name| name.unwrap())
            .collect();
        for expected in [
            "idx_net_edges_bucket",
            "idx_net_nodes_bucket",
            "idx_samples_time",
        ] {
            assert!(
                indexes.iter().any(|name| name == expected),
                "missing {expected}"
            );
        }

        drop(conn);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The interrupt handle must be able to stop a running query from another
    /// thread — this is what frees the single connection when a newer query
    /// supersedes an old long scan.
    #[test]
    fn interrupt_stops_a_running_query_from_another_thread() {
        let (store, dir) = temp_store("interrupt");
        let caller = store.interrupt.clone();
        let thread = std::thread::spawn(move || {
            let conn = store.conn.lock();
            // A deliberately long scan (20 billion rows to count) that cannot
            // finish before the interrupt below, but uses O(1) memory.
            conn.execute_batch(
                "SELECT count(*) FROM range(1000000000) t(x) CROSS JOIN range(20) u(y);",
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        caller.interrupt();
        let result = thread.join().unwrap();
        assert!(result.is_err(), "the query should be interrupted");

        // `store` was moved into the thread and dropped when it finished.
        std::fs::remove_dir_all(dir).unwrap();
    }
}
