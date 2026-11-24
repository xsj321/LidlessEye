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

// Legacy DuckDB-schema helpers are kept while the migration path is being
// exercised; canonical modes no longer call them.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Instant;

use duckdb::{Connection, InterruptHandle, params};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::{Error, Result};
use crate::model::{Frame, ProfileType, Report, SERVICE_NAME_LABEL};
use crate::parquet::{self, ParquetState};
use crate::selector::{MatchOp, Selector};
use crate::servicemap::{MapEdge, MapHistoryPoint, MapNode, NodeKind, ServiceMapBatch, node_id};
use crate::time::now_ns;

/// Version of the table layout. Bumped when a change needs a migration.
pub const SCHEMA_VERSION: u32 = 2;

/// File name of the database inside a store root.
pub const DB_FILE: &str = "lidlesseye.duckdb";

/// Hard ceiling on rows returned by one [`Store::scan`], so a query over a
/// month of data fails as a truncated answer instead of as an OOM kill.
pub const MAX_SCAN_ROWS: usize = 2_000_000;

/// The counter the service map's time brush plots.
///
/// The type lives with the service map model; it is re-exported here because
/// storage code needs the SQL expression that backs each variant.
pub use lidlesseye_servicemap::NetMetric;

/// SQL expression to sum for a map metric. Constants only — never user input.
fn net_metric_column(metric: NetMetric) -> &'static str {
    match metric {
        NetMetric::Connections => "connections",
        NetMetric::Bytes => "tx_bytes + rx_bytes",
        NetMetric::Retransmits => "retransmits",
    }
}

/// How rows are physically stored.
///
/// Neither canonical mode creates a persistent DuckDB database file. DuckDB is
/// an in-process query engine for the current hot tier; every durable row is a
/// Parquet file (or, in `s3` mode, a verified S3 object).
///
/// * [`StorageMode::Disk`] flushes hot rows to local Parquet fragments, merges
///   each completed hour into one deterministic final file and then removes
///   the fragments. The PVC holds only Parquet data files.
/// * [`StorageMode::S3`] reuses the same flush/merge protocol, uploads the
///   merged file to S3 and removes the local staging copy only after the
///   object has been read back successfully.
///
/// Reads scan the hot tables and all Parquet files through canonical-name
/// VIEWs, so filters/projections are pushed into the Parquet scan and remote
/// data is streamed rather than copied into the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StorageMode {
    /// Legacy DuckDB file mode. Only kept for migration/old fixtures; no new
    /// deployment may select it.
    #[serde(rename = "duckdb")]
    DuckDb,
    /// In-memory DuckDB hot tier, Parquet files on local disk. This is the
    /// default and needs no object store.
    #[serde(rename = "disk")]
    #[default]
    Disk,
    /// In-memory hot tier, local Parquet staging, S3 as the archive.
    #[serde(rename = "s3")]
    S3,
    /// Legacy name for [`StorageMode::S3`]. Accepted with a warning so old
    /// Helm values do not silently start an empty store.
    #[serde(rename = "parquet")]
    Parquet,
}

impl StorageMode {
    /// Whether this mode uses the in-memory hot tier + Parquet layout.
    pub fn uses_parquet(self) -> bool {
        matches!(
            self,
            StorageMode::Disk | StorageMode::S3 | StorageMode::Parquet | StorageMode::DuckDb
        )
    }

    /// Whether the value came from a pre-0.1.16 configuration.
    pub fn is_legacy(self) -> bool {
        matches!(self, StorageMode::DuckDb | StorageMode::Parquet)
    }

    /// Canonical mode used by the new storage layout.
    pub fn canonical(self) -> Self {
        match self {
            StorageMode::Parquet => StorageMode::S3,
            StorageMode::DuckDb => StorageMode::Disk,
            other => other,
        }
    }

    /// Configuration value used by the CLI and Helm chart.
    pub fn as_str(self) -> &'static str {
        match self {
            StorageMode::DuckDb => "duckdb",
            StorageMode::Disk => "disk",
            StorageMode::S3 => "s3",
            StorageMode::Parquet => "parquet",
        }
    }
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
    /// How long a completed Parquet fragment may wait on local disk before it
    /// is merged (and, in `s3` mode, uploaded). `None` uses one hour.
    #[serde(default)]
    pub parquet_local_ttl_ns: Option<u64>,
    /// How often the in-memory hot tier is flushed to a Parquet fragment.
    /// This is the maximum exposure window (RPO) if the process is killed
    /// before a flush. `None` uses ten seconds.
    #[serde(default)]
    pub flush_interval_ns: Option<u64>,
    /// Number of read connections opened onto the same in-memory database.
    ///
    /// Reads used to share one mutex-guarded connection, so a single wide scan
    /// put every other request in a queue that ended in a 408. DuckDB is
    /// MVCC: separate connections can read concurrently while the writer
    /// commits. `None` derives a small pool from the thread count.
    #[serde(default)]
    pub read_connections: Option<usize>,
}

impl StoreConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        StoreConfig {
            root: root.into(),
            retention_ns: None,
            max_disk_bytes: None,
            threads: None,
            duckdb_memory_limit: None,
            storage_mode: StorageMode::Disk,
            s3: None,
            parquet_local_ttl_ns: None,
            flush_interval_ns: None,
            read_connections: None,
        }
    }

    /// Effective number of read connections.
    ///
    /// Four by default: enough that a dashboard's independent panels (map,
    /// histogram, series list, stats) no longer queue behind each other, and
    /// small enough that the extra per-connection DuckDB state stays trivial.
    /// A pod with many cores may raise it; there is no benefit above the
    /// query-concurrency limit the server applies.
    pub fn read_connections(&self) -> usize {
        self.read_connections.unwrap_or(4).clamp(1, 32)
    }

    /// Effective local Parquet TTL: one hour unless overridden.
    pub fn parquet_local_ttl_ns(&self) -> u64 {
        self.parquet_local_ttl_ns.unwrap_or(3_600_000_000_000)
    }

    /// Effective hot-tier flush interval: ten seconds unless overridden.
    pub fn flush_interval_ns(&self) -> u64 {
        self.flush_interval_ns.unwrap_or(10_000_000_000)
    }

    /// Reject configurations that would start an empty store.
    pub fn validate(&self) -> Result<()> {
        if self.storage_mode.canonical() == StorageMode::S3 && self.s3.is_none() {
            return Err(Error::InvalidArgument(
                "storage mode `s3` requires --s3-bucket and credentials".into(),
            ));
        }
        Ok(())
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
    /// Whole Parquet files removed by retention or the disk cap. Disk-mode
    /// eviction cannot delete individual rows, so this is the only removal
    /// signal in that mode.
    pub removed_files: usize,
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

/// Who owns the single store connection, for lock-wait diagnostics.
///
/// Every operation that serialises on `conn` declares itself here. A slow
/// request can then be attributed to the actual holder instead of the
/// catch-all "store busy", which was the reason a rollout with several agents
/// turned into unexplained 408s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ConnUser {
    Idle = 0,
    Read = 1,
    Write = 2,
    Flush = 3,
    Compact = 4,
    Archive = 5,
    Prune = 6,
    Maintenance = 7,
}

impl ConnUser {
    fn label(self) -> &'static str {
        match self {
            ConnUser::Idle => "idle",
            ConnUser::Read => "read",
            ConnUser::Write => "write",
            ConnUser::Flush => "flush",
            ConnUser::Compact => "compact",
            ConnUser::Archive => "archive",
            ConnUser::Prune => "prune",
            ConnUser::Maintenance => "maintenance",
        }
    }

    fn from_byte(value: u8) -> Self {
        match value {
            1 => ConnUser::Read,
            2 => ConnUser::Write,
            3 => ConnUser::Flush,
            4 => ConnUser::Compact,
            5 => ConnUser::Archive,
            6 => ConnUser::Prune,
            7 => ConnUser::Maintenance,
            _ => ConnUser::Idle,
        }
    }
}

/// Atomic counters behind [`Store::lock_stats`].
#[derive(Debug, Default)]
struct LockTelemetry {
    user: AtomicU8,
    acquired_ns: AtomicU64,
    waits: AtomicU64,
    wait_nanos: AtomicU64,
    max_wait_nanos: AtomicU64,
    slow_waits: AtomicU64,
    held_nanos: AtomicU64,
}

/// Observable snapshot of the connection mutex.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StoreLockStats {
    /// Current owner (`idle`, `read`, `write`, ...).
    pub holder: String,
    /// How long the current owner has held the connection, in milliseconds.
    pub held_ms: u64,
    /// Number of times a caller had to wait for the connection.
    pub waits: u64,
    /// Total time spent waiting, in milliseconds.
    pub wait_ms: u64,
    /// Longest single wait, in milliseconds.
    pub max_wait_ms: u64,
    /// Waits longer than 100 ms.
    pub slow_waits: u64,
}

impl LockTelemetry {
    fn holder(&self) -> ConnUser {
        ConnUser::from_byte(self.user.load(Ordering::Acquire))
    }

    fn held_ms(&self) -> u64 {
        let acquired = self.acquired_ns.load(Ordering::Relaxed);
        if acquired == 0 {
            return 0;
        }
        now_ns().saturating_sub(acquired) / 1_000_000
    }

    fn record_wait(&self, requester: ConnUser, waited: std::time::Duration) {
        let nanos = waited.as_nanos().min(u128::from(u64::MAX)) as u64;
        self.waits.fetch_add(1, Ordering::Relaxed);
        self.wait_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.max_wait_nanos.fetch_max(nanos, Ordering::Relaxed);
        if waited >= std::time::Duration::from_millis(100) {
            self.slow_waits.fetch_add(1, Ordering::Relaxed);
            info!(
                stage = "store_lock_wait",
                requester = requester.label(),
                holder = self.holder().label(),
                holder_held_ms = self.held_ms(),
                wait_ms = waited.as_millis(),
                "waited for the store connection"
            );
        }
    }

    fn acquire(&self, user: ConnUser) {
        self.user.store(user as u8, Ordering::Release);
        self.acquired_ns.store(now_ns(), Ordering::Release);
    }

    fn release(&self) {
        let acquired = self.acquired_ns.swap(0, Ordering::AcqRel);
        if acquired != 0 {
            self.held_nanos
                .fetch_add(now_ns().saturating_sub(acquired), Ordering::Relaxed);
        }
        self.user.store(ConnUser::Idle as u8, Ordering::Release);
    }

    fn snapshot(&self) -> StoreLockStats {
        StoreLockStats {
            holder: self.holder().label().to_string(),
            held_ms: self.held_ms(),
            waits: self.waits.load(Ordering::Relaxed),
            wait_ms: self.wait_nanos.load(Ordering::Relaxed) / 1_000_000,
            max_wait_ms: self.max_wait_nanos.load(Ordering::Relaxed) / 1_000_000,
            slow_waits: self.slow_waits.load(Ordering::Relaxed),
        }
    }
}

/// A store-connection guard that publishes its owner and wait time.
struct ConnLock<'a> {
    guard: parking_lot::MutexGuard<'a, Connection>,
    telemetry: &'a LockTelemetry,
    user: ConnUser,
}

impl<'a> ConnLock<'a> {
    fn new(
        guard: parking_lot::MutexGuard<'a, Connection>,
        telemetry: &'a LockTelemetry,
        user: ConnUser,
    ) -> Self {
        telemetry.acquire(user);
        ConnLock {
            guard,
            telemetry,
            user,
        }
    }
}

impl Deref for ConnLock<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl DerefMut for ConnLock<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl Drop for ConnLock<'_> {
    fn drop(&mut self) {
        debug!(
            stage = "store_lock_hold",
            user = self.user.label(),
            held_ms = self.telemetry.held_ms(),
            "released store connection"
        );
        self.telemetry.release();
    }
}

/// Thread-safe profile store.
///
/// DuckDB's Rust binding exposes no separate `Database` handle, but a
/// `Connection` can be cloned onto the same in-memory database. That is what
/// the pool below uses: one writer connection owns the hot tables, and several
/// read connections serve queries concurrently under DuckDB's MVCC. Previously
/// a single mutex-guarded connection serialised everything, so one wide scan
/// queued every other request behind it until the HTTP deadline (a 408 that
/// looked like a broken server).
pub struct Store {
    cfg: StoreConfig,
    /// Writer connection: hot-table inserts, flushes, DDL, maintenance.
    conn: Mutex<Connection>,
    /// Owner/wait telemetry for the writer lock, so a slow ingest or flush is
    /// attributable. Reads are not tracked here: several run at once.
    telemetry: LockTelemetry,
    /// Read connections onto the same in-memory database.
    readers: ReaderPool,
    /// `(module, symbol, file, line)` -> frame id, for the write path only.
    frame_cache: Mutex<HashMap<FrameKey, u32>>,
    ids: Mutex<Ids>,
    /// Layout and lifecycle of the local/S3 Parquet files. Always present:
    /// canonical modes never keep a persistent DuckDB database.
    parquet: ParquetState,
    /// Second in-memory connection used by compaction and S3 upload. It only
    /// opens Parquet files, so a long merge/upload no longer blocks queries
    /// or ingests on the connection that owns the hot tables.
    maintenance_conn: Mutex<Connection>,
    /// Wall-clock instant of the last local Parquet compaction pass. The
    /// compaction tick runs every few seconds; rewriting an hour's files that
    /// often would cost more than it saves.
    last_compact_ns: std::sync::atomic::AtomicU64,
    /// Last successful hot-tier flush, for the configured RPO cadence.
    last_flush_ns: std::sync::atomic::AtomicU64,
    /// Row counts, sums and time bounds of `batches`, carried forward by every
    /// path that changes them.
    ///
    /// Every open dashboard polls `stats`, and in canonical modes the answer
    /// has to read the whole archive: `COUNT(*)` is cheap only while the rows
    /// are hot, and `SUM(total_value)` reads a column's data pages either way.
    /// Recomputing per poll is what made the endpoint time out. So it is read
    /// once, under the writer connection, and then *maintained*: an append adds
    /// its own contribution, a prune recounts. `None` means "not counted yet".
    agg: Mutex<Option<RowAgg>>,
    /// `(computed_at, distinct series)`. A full scan of the label columns with
    /// no exact incremental form, so it runs on a slow clock instead.
    series_cache: Mutex<Option<(u64, usize)>>,
}

/// The row half of [`StoreStats`]: everything that a write path can adjust
/// arithmetically.
#[derive(Debug, Clone, Copy, Default)]
struct RowAgg {
    batch_count: usize,
    total_value: u64,
    logical_bytes: u64,
    earliest_ns: Option<u64>,
    latest_ns: Option<u64>,
}

impl RowAgg {
    /// Account one committed batch.
    fn add(&mut self, value: u64, bytes: u64, start_ns: u64, end_ns: u64) {
        self.batch_count += 1;
        self.total_value = self.total_value.saturating_add(value);
        self.logical_bytes = self.logical_bytes.saturating_add(bytes);
        self.earliest_ns = Some(self.earliest_ns.map_or(start_ns, |at| at.min(start_ns)));
        self.latest_ns = Some(self.latest_ns.map_or(end_ns, |at| at.max(end_ns)));
    }

    fn into_stats(self, total_bytes: u64) -> StoreStats {
        StoreStats {
            batch_count: self.batch_count,
            total_bytes,
            logical_bytes: self.logical_bytes,
            total_value: self.total_value,
            earliest_ns: self.earliest_ns,
            latest_ns: self.latest_ns,
        }
    }
}

/// One read connection onto the shared database.
struct ReaderSlot {
    conn: Mutex<Connection>,
    /// Locked for exactly as long as a query owns this slot. [`ReaderPool::interrupt_all`]
    /// consults it so an interrupt cannot land on an idle connection — and
    /// never on the writer, whose transaction must not be aborted.
    active: Mutex<()>,
    interrupt: Arc<InterruptHandle>,
}

/// A small fixed pool of read connections.
///
/// Slots are handed out from a free list and returned on drop. Waiting is
/// bounded: a caller that cannot get a slot is told the store is busy, which
/// the HTTP layer turns into a retryable response instead of letting the
/// request sit until its own deadline.
struct ReaderPool {
    slots: Vec<ReaderSlot>,
    /// Ids of free slots, most recently used first so a single sequential
    /// client keeps reusing one connection (and its warm caches).
    free: Mutex<Vec<usize>>,
    cv: parking_lot::Condvar,
    /// Wait counters; ownership is not tracked per slot because several reads
    /// legitimately run at once.
    waits: AtomicU64,
    wait_nanos: AtomicU64,
    max_wait_nanos: AtomicU64,
}

impl ReaderPool {
    /// Clone `count` connections off `conn` and give each the session settings
    /// it would otherwise lack.
    ///
    /// DuckDB's `SET` is session-scoped for most options, so a cloned
    /// connection starts with the defaults: no metadata cache, a 30-second
    /// HTTP timeout, one thread. Applying the same tuning here is what makes
    /// the pool actually fast — and what keeps a stalled S3 read from holding
    /// a reader for hours.
    fn new(conn: &Connection, count: usize, cfg: &StoreConfig, temp: &Path) -> Result<Self> {
        let mut slots = Vec::with_capacity(count);
        for _ in 0..count {
            let reader = conn
                .try_clone()
                .map_err(|err| Error::Db(format!("opening a read connection: {err}")))?;
            tune_connection(&reader, temp, cfg)?;
            if let Some(s3) = &cfg.s3 {
                parquet::configure_s3(&reader, s3)?;
            }
            let interrupt = reader.interrupt_handle();
            slots.push(ReaderSlot {
                conn: Mutex::new(reader),
                active: Mutex::new(()),
                interrupt,
            });
        }
        let free = (0..slots.len()).rev().collect();
        Ok(ReaderPool {
            slots,
            free: Mutex::new(free),
            cv: parking_lot::Condvar::new(),
            waits: AtomicU64::new(0),
            wait_nanos: AtomicU64::new(0),
            max_wait_nanos: AtomicU64::new(0),
        })
    }

    fn acquire(&self, timeout: std::time::Duration) -> Option<usize> {
        let started = Instant::now();
        let deadline = started + timeout;
        let mut free = self.free.lock();
        let idx = loop {
            if let Some(idx) = free.pop() {
                break Some(idx);
            }
            let now = Instant::now();
            if now >= deadline {
                break None;
            }
            self.cv.wait_for(&mut free, deadline - now);
        };
        let waited = started.elapsed();
        if waited >= std::time::Duration::from_millis(1) {
            let nanos = waited.as_nanos().min(u128::from(u64::MAX)) as u64;
            self.waits.fetch_add(1, Ordering::Relaxed);
            self.wait_nanos.fetch_add(nanos, Ordering::Relaxed);
            self.max_wait_nanos.fetch_max(nanos, Ordering::Relaxed);
        }
        idx
    }

    fn release(&self, idx: usize) {
        self.free.lock().push(idx);
        self.cv.notify_one();
    }

    /// Interrupt every read that is currently running. Finished slots are
    /// skipped, and the writer is never touched.
    fn interrupt_all(&self) {
        for slot in &self.slots {
            if slot.active.try_lock().is_none() {
                slot.interrupt.interrupt();
            }
        }
    }
}

/// A read's exclusive hold on one pool slot.
///
/// `_active` is declared first so it is released first, keeping the
/// "interrupt may fire" window strictly inside the "connection is held"
/// window.
struct ReadConn<'a> {
    _active: parking_lot::MutexGuard<'a, ()>,
    conn: parking_lot::MutexGuard<'a, Connection>,
    pool: &'a ReaderPool,
    idx: usize,
}

impl Deref for ReadConn<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

impl DerefMut for ReadConn<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.conn
    }
}

impl Drop for ReadConn<'_> {
    fn drop(&mut self) {
        // Return the slot after the guards above are gone. The pool hands the
        // id out again only to a caller that immediately blocks on the slot's
        // own mutex, so this cannot race with the connection still in use.
        self.pool.release(self.idx);
    }
}

/// How long a read waits for the connection before reporting the store busy.
///
/// The connection is single and a wide scan or an archive merge holds it, so
/// "wait forever" is not patience — it is an unbounded queue that eventually
/// turns into a 60 s request timeout. Forty-five seconds leaves a slow but
/// healthy scan time to finish and answer, while still failing clearly before
/// the HTTP layer's 60 s deadline.
const READ_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(45);

/// How long background maintenance waits for the connection before giving up
/// on this tick. Long enough to slip in between two dashboard polls, short
/// enough that a query never notices.
const BACKGROUND_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// How long an ingest waits for the writer before telling the agent to retry.
///
/// Long enough for a flush of the current hot tier, short enough that a wedged
/// writer cannot pin every ingest request in the server until the HTTP deadline.
const WRITE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

impl Store {
    /// Whether this store keeps rows in a DuckDB file or in Parquet/S3.
    pub fn storage_mode(&self) -> StorageMode {
        self.cfg.storage_mode
    }

    /// Take a read connection, or fail with [`Error::Busy`].
    ///
    /// There are several, so a scan no longer queues every other request. The
    /// wait is still bounded: a caller that cannot get one within
    /// [`READ_LOCK_WAIT`] is told the store is busy, which the HTTP layer can
    /// turn into a retryable response instead of a 60 s timeout.
    fn read_conn(&self) -> Result<ReadConn<'_>> {
        let idx = self.readers.acquire(READ_LOCK_WAIT).ok_or_else(|| {
            Error::Busy(format!(
                "the store was busy for {}s; a wide scan is running. Narrow the \
                 time range or retry",
                READ_LOCK_WAIT.as_secs()
            ))
        })?;
        let slot = &self.readers.slots[idx];
        let active = slot.active.lock();
        let conn = slot.conn.lock();
        Ok(ReadConn {
            _active: active,
            conn,
            pool: &self.readers,
            idx,
        })
    }

    /// Take the connection for a write/maintenance operation and label the
    /// owner while it is held. Replaces bare `conn.lock()` calls so a queued
    /// reader can see whether it is waiting behind an ingest, a flush or a
    /// compaction.
    fn lock_conn(&self, user: ConnUser) -> ConnLock<'_> {
        let started = Instant::now();
        let guard = self.conn.lock();
        self.telemetry.record_wait(user, started.elapsed());
        ConnLock::new(guard, &self.telemetry, user)
    }

    /// [`Store::lock_conn`] with a bounded wait for background work.
    fn try_lock_conn(&self, user: ConnUser, timeout: std::time::Duration) -> Option<ConnLock<'_>> {
        let started = Instant::now();
        let guard = self.conn.try_lock_for(timeout);
        // Record the wait whether or not it succeeded: a timeout is exactly the
        // datapoint that explains a burst of `Busy` responses.
        self.telemetry.record_wait(user, started.elapsed());
        let guard = guard?;
        Some(ConnLock::new(guard, &self.telemetry, user))
    }

    /// Take the writer for an ingest, or report [`Error::Busy`].
    ///
    /// Ingest used to wait for the connection indefinitely. When a flush or a
    /// maintenance pass owned it, agent reports piled up inside the server,
    /// each holding a request slot, until the HTTP layer timed them out and the
    /// agents retried — the "report queue full" storm. A bounded wait turns
    /// that into a fast, retryable error while the work that holds the writer
    /// finishes.
    fn write_conn(&self) -> Result<ConnLock<'_>> {
        self.try_lock_conn(ConnUser::Write, WRITE_LOCK_WAIT)
            .ok_or_else(|| {
                Error::Busy(format!(
                    "the store was busy for {}s; a flush or maintenance pass owns the \
                     writer. Retry the report",
                    WRITE_LOCK_WAIT.as_secs()
                ))
            })
    }

    /// Current connection-owner and wait statistics, for diagnostics.
    pub fn lock_stats(&self) -> StoreLockStats {
        let mut stats = self.telemetry.snapshot();
        // Reader waits live in the pool (there is no single owner to report).
        stats.waits += self.readers.waits.load(Ordering::Relaxed);
        stats.wait_ms += self.readers.wait_nanos.load(Ordering::Relaxed) / 1_000_000;
        stats.max_wait_ms = stats
            .max_wait_ms
            .max(self.readers.max_wait_nanos.load(Ordering::Relaxed) / 1_000_000);
        stats
    }

    /// Canonical names are VIEWs (hot ∪ local ∪ S3), so writes always target
    /// the in-memory hot table.
    fn wtable(&self, name: &str) -> String {
        format!("{name}_hot")
    }

    /// `FROM`-clause for a canonical table restricted to a time window.
    ///
    /// Every read uses this instead of naming a VIEW. The relation is the hot
    /// tier plus only the Parquet hours that can overlap `[from_ns, until_ns)`:
    /// a one-hour query over a week of history names one file, where a VIEW
    /// over the whole archive would make DuckDB open every footer in the
    /// retention window before it could return a single row.
    fn span(&self, table: &str, from_ns: u64, until_ns: u64) -> String {
        parquet::table_span(&self.parquet, table, from_ns, until_ns)
    }

    /// [`Store::span`] over the whole dataset, for aggregates with no window.
    fn span_all(&self, table: &str) -> String {
        parquet::table_span_all(&self.parquet, table)
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
        let started = std::time::Instant::now();
        std::fs::create_dir_all(&cfg.root)?;
        info!(
            root = %cfg.root.display(),
            mode = ?cfg.storage_mode,
            elapsed_ms = started.elapsed().as_millis(),
            stage = "directory",
            "opening store"
        );

        cfg.validate()?;
        // Upgrade path: an existing database from an older release must be
        // exported before the in-memory store starts, otherwise it would look
        // like an empty installation and silently hide all history.
        crate::migrate::migrate_legacy(&cfg.root)?;
        let mode = cfg.storage_mode.canonical();
        if cfg.storage_mode.is_legacy() {
            warn!(
                stage = "storage_mode",
                legacy = cfg.storage_mode.as_str(),
                canonical = mode.as_str(),
                "legacy storage mode mapped to its canonical replacement"
            );
        }
        let parquet_state =
            ParquetState::new(&cfg.root, mode, cfg.s3.as_ref(), cfg.parquet_local_ttl_ns());
        std::fs::create_dir_all(&parquet_state.dir).map_err(Error::Io)?;

        let mut duckdb_config = duckdb::Config::default();
        if let Some(threads) = cfg.threads {
            duckdb_config = duckdb_config
                .threads(threads as i64)
                .map_err(|err| Error::Db(format!("setting DuckDB threads: {err}")))?;
        }

        // `open_in_memory_with_flags` — not `open_in_memory` — because the
        // configured thread count has to reach the config object. There is no
        // DuckDB file in either canonical mode; the durable dataset is Parquet.
        let conn = Connection::open_in_memory_with_flags(duckdb_config)
            .map_err(|err| Error::Db(format!("opening in-memory DuckDB: {err}")))?;
        tune_connection(&conn, &parquet_state.dir.join(".duckdb-tmp"), &cfg)?;
        if mode == StorageMode::S3 {
            let s3 = cfg.s3.as_ref().expect("validated above");
            parquet::configure_s3(&conn, s3)?;
        }
        parquet::create_schema(&conn, &parquet_state)?;
        if mode == StorageMode::S3 {
            let s3 = cfg.s3.as_ref().expect("validated above");
            match parquet::discover_s3_objects(&conn, &parquet_state, s3) {
                Ok(0) | Err(_) => {} // nothing archived yet, or S3 unreachable
                Ok(n) => info!(objects = n, "discovered archived S3 objects"),
            }
        }
        info!(
            elapsed_ms = started.elapsed().as_millis(),
            stage = "schema",
            "in-memory store ready"
        );

        // A separate connection for merges/uploads: it opens Parquet files and
        // S3 objects, never the hot tables. A slow upload can no longer make
        // every ingest and query wait on it.
        let extras = std::time::Instant::now();
        let maintenance_conn = Connection::open_in_memory()
            .map_err(|err| Error::Db(format!("opening maintenance DuckDB: {err}")))?;
        if mode == StorageMode::S3 {
            parquet::configure_s3(&maintenance_conn, cfg.s3.as_ref().expect("validated above"))?;
        }
        // Readers share the same in-memory database through cloned
        // connections, so queries no longer serialise on the writer. Each one
        // is tuned again: DuckDB's SET is session-scoped.
        let readers = ReaderPool::new(
            &conn,
            cfg.read_connections(),
            &cfg,
            &parquet_state.dir.join(".duckdb-tmp"),
        )?;
        // Opening connections is not free: each read slot re-runs `LOAD httpfs`
        // and the whole session tuning, so this is worth attributing when a
        // ten-tenant start runs past its probe budget.
        info!(
            stage = "connections",
            readers = cfg.read_connections(),
            elapsed_ms = extras.elapsed().as_millis(),
            "auxiliary connections ready"
        );

        info!(stage = "id_counters", "loading store id counters");
        let ids_started = std::time::Instant::now();
        let ids = {
            // Ids are allocated across the *whole* dataset, so these three
            // lookups must span the hot tier and every Parquet file. They are
            // one `MAX` per table; DuckDB answers each from file statistics
            // without reading any row groups.
            let frames_span = parquet::table_span_all(&parquet_state, "frames");
            let stacks_span = parquet::table_span_all(&parquet_state, "stacks");
            let batches_span = parquet::table_span_all(&parquet_state, "batches");
            let max_frame: i64 = conn
                .query_row(
                    &format!("SELECT COALESCE(MAX(frame_id), 0) FROM {frames_span}"),
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            let max_stack: i64 = conn
                .query_row(
                    &format!("SELECT COALESCE(MAX(stack_id), 0) FROM {stacks_span}"),
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            let max_batch: i64 = conn
                .query_row(
                    &format!("SELECT COALESCE(MAX(batch_id), 0) FROM {batches_span}"),
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

        info!(
            stage = "id_counters",
            elapsed_ms = ids_started.elapsed().as_millis(),
            "id counters loaded"
        );

        let store = Store {
            cfg,
            conn: Mutex::new(conn),
            telemetry: LockTelemetry::default(),
            readers,
            frame_cache: Mutex::new(HashMap::new()),
            ids: Mutex::new(ids),
            parquet: parquet_state,
            maintenance_conn: Mutex::new(maintenance_conn),
            last_compact_ns: std::sync::atomic::AtomicU64::new(0),
            last_flush_ns: std::sync::atomic::AtomicU64::new(now_ns()),
            agg: Mutex::new(None),
            series_cache: Mutex::new(None),
        };
        info!(
            root = %store.cfg.root.display(),
            elapsed_ms = started.elapsed().as_millis(),
            "store ready"
        );
        Ok(store)
    }

    /// Interrupt the read currently running on this store's connection.
    /// Safe to call from any thread; a no-op when no read is in flight. This is
    /// what lets the HTTP layer stop a long scan the moment it is superseded.
    ///
    /// The guard matters: `InterruptHandle` targets *whatever* DuckDB is
    /// executing at that instant. An unguarded late tick used to land during
    /// `append`'s transaction; DuckDB then failed the INSERT, and a second
    /// unlucky tick during `ROLLBACK` stranded the connection in the aborted
    /// state that made every later ingest fail with the same error.
    pub fn interrupt(&self) {
        // Only readers are interrupted. The writer's connection is never
        // signalled: a watchdog tick that lands inside its transaction leaves
        // DuckDB aborted, and every later ingest fails with the same error.
        self.readers.interrupt_all();
    }

    pub fn config(&self) -> &StoreConfig {
        &self.cfg
    }

    pub fn root(&self) -> &Path {
        &self.cfg.root
    }

    /// Directory holding the durable Parquet dataset.
    pub fn path(&self) -> PathBuf {
        self.parquet.dir.clone()
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
    ) -> Result<Vec<Arc<BatchMeta>>> {
        self.query_batches(Some((profile_type, from_ns, until_ns)), None, selector)
    }

    fn query_batches(
        &self,
        window: Option<(&ProfileType, u64, u64)>,
        limit: Option<usize>,
        selector: &Selector,
    ) -> Result<Vec<Arc<BatchMeta>>> {
        let conn = self.read_conn()?;
        let span = match window {
            Some((_, from_ns, until_ns)) => self.span("batches", from_ns, until_ns),
            None => self.span_all("batches"),
        };
        let mut sql = format!(
            "SELECT batch_id, profile_type, service_name, labels, start_ns, end_ns, \
                    total_value, sample_count, bytes, created_ns FROM {span}"
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
            let conn = self.write_conn()?;
            in_transaction(&conn, "COMMIT", |conn| -> Result<()> {
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
            })?;
            // Fold the batch into the catalogue aggregate *before* releasing the
            // writer. A caller that appends and then asks for stats must see its
            // own write, and clearing the cache did that by forcing a full
            // re-scan on every append — which is every few seconds per agent.
            // Doing it here instead keeps the arithmetic exact: a recount (see
            // `Store::row_agg`) can only run while it owns this connection, so
            // it can never see a committed batch that is not counted yet.
            if let Some(agg) = self.agg.lock().as_mut() {
                agg.add(total_value, bytes, report.start_ns, report.end_ns);
            }
        }

        debug!(
            batch = batch_id,
            stacks = pending.samples.len(),
            frames = pending.frames.len(),
            "wrote batch"
        );

        // The catalogue aggregate was updated under the writer above, so a
        // caller that appends and then asks for stats already sees this batch.
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
        let conn = self.read_conn()?;
        let span = self.span("samples", query.from_ns, query.until_ns);
        let types = query.profile_type.source_types();
        let placeholders = vec!["?"; types.len()].join(", ");
        // A selector or a `group_by` needs the series columns; a plain
        // flamegraph does not. Those columns are the bulk of every sample row
        // (a full label map repeated per stack), so reading only
        // `stack_id, value` removes most of the I/O on a wide scan.
        let needs_series = !query.selector.is_empty() || query.group_by.is_some();
        // A gauge is a snapshot, so the window selects *which* snapshot, not how
        // many to add up: only the newest batch of each series inside the window
        // is read. Summing the flushes of `inuse_space` would report a heap five
        // times its size over five flushes.
        let gauge = query.profile_type.is_gauge();
        let series_columns = if needs_series {
            "stack_id, service_name, labels, "
        } else {
            "stack_id, "
        };
        let mut sql = if gauge {
            let inner = vec!["?"; types.len()].join(", ");
            format!(
                "SELECT {series_columns}SUM(value), COUNT(*) FROM {span} \
                 WHERE profile_type IN ({placeholders}) AND end_ns > ? AND start_ns < ? \
                   AND (service_name, labels, end_ns) IN (\
                        SELECT service_name, labels, MAX(end_ns) FROM {span} \
                        WHERE profile_type IN ({inner}) AND end_ns > ? AND start_ns < ? \
                        GROUP BY 1, 2)"
            )
        } else if needs_series {
            format!(
                "SELECT stack_id, service_name, labels, SUM(value), COUNT(*) FROM {span} \
                 WHERE profile_type IN ({placeholders}) AND end_ns > ? AND start_ns < ?"
            )
        } else {
            format!(
                "SELECT stack_id, SUM(value), COUNT(*) FROM {span} \
                 WHERE profile_type IN ({placeholders}) AND end_ns > ? AND start_ns < ?"
            )
        };
        let mut args: Vec<Box<dyn duckdb::ToSql>> = Vec::new();
        for t in &types {
            args.push(Box::new(t.as_str().to_string()));
        }
        args.push(Box::new(ts(query.from_ns)));
        args.push(Box::new(ts(query.until_ns)));
        if gauge {
            // The subquery repeats the type and time filters, so it is bound
            // again rather than referenced by name.
            for t in &types {
                args.push(Box::new(t.as_str().to_string()));
            }
            args.push(Box::new(ts(query.from_ns)));
            args.push(Box::new(ts(query.until_ns)));
        }

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
        // A uniquely named staging table, not a shared one: every read has its
        // own connection onto the same in-memory database, and a shared
        // `lidlesseye_query_ids` would let two concurrent scans delete each
        // other's ids and silently drop stacks.
        let ids_table = next_staging_table();
        conn.execute_batch(&format!("CREATE TEMP TABLE {ids_table}(id BIGINT)"))
            .map_err(db_err)?;
        {
            let mut appender = conn.appender(&ids_table).map_err(db_err)?;
            for &id in ids {
                appender.append_row([id as i64]).map_err(db_err)?;
            }
            appender.flush().map_err(db_err)?;
        }
        let span = self.span_all("stacks");
        let mut out = HashMap::with_capacity(ids.len());
        let mut stmt = conn
            .prepare(&format!(
                "SELECT s.stack_id, s.frames FROM {span} s \
                 JOIN {ids_table} q ON s.stack_id = q.id"
            ))
            .map_err(db_err)?;
        let mut rows = stmt.query([]).map_err(db_err)?;
        while let Some(row) = rows.next().map_err(db_err)? {
            let id = row.get::<_, i64>(0).map_err(db_err)?.max(0) as u64;
            let blob: Vec<u8> = row.get(1).map_err(db_err)?;
            out.insert(id, unpack_frames(&blob));
        }
        drop(rows);
        drop(stmt);
        let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS {ids_table}"));
        Ok(out)
    }

    /// Same bulk-join strategy as [`Store::load_stacks`]: one statement instead
    /// of thousands of `IN (...)` prepares on the hot path of every query.
    fn load_frames(&self, conn: &Connection, ids: &[u32]) -> Result<HashMap<u32, Frame>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let ids_table = next_staging_table();
        conn.execute_batch(&format!("CREATE TEMP TABLE {ids_table}(id BIGINT)"))
            .map_err(db_err)?;
        {
            let mut appender = conn.appender(&ids_table).map_err(db_err)?;
            for &id in ids {
                appender.append_row([id as i64]).map_err(db_err)?;
            }
            appender.flush().map_err(db_err)?;
        }
        let span = self.span_all("frames");
        let mut out = HashMap::with_capacity(ids.len());
        let mut stmt = conn
            .prepare(&format!(
                "SELECT f.frame_id, f.module, f.symbol, f.file, f.line FROM {span} f \
                 JOIN {ids_table} q ON f.frame_id = q.id"
            ))
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
        drop(rows);
        drop(stmt);
        let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS {ids_table}"));
        Ok(out)
    }

    pub fn stats(&self) -> StoreStats {
        self.stats_snapshot()
            .unwrap_or((StoreStats::default(), 0))
            .0
    }

    /// The catalogue numbers, without scanning.
    ///
    /// The tenant list describes every tenant at once. Answering it used to
    /// mean a full scan per tenant on the request path — ten tenants of 1.5 s
    /// each is what made the dashboard's first call time out. The row half is
    /// now arithmetic (see [`Store::append`]); only the distinct-series count
    /// still costs a scan, and it is allowed to be ten minutes old.
    pub fn stats_cached(&self) -> (StoreStats, usize) {
        self.stats_snapshot().unwrap_or_default()
    }

    /// [`Store::stats_snapshot`] without the fallback: a busy store is an
    /// error, not zero rows. Internal callers are happy with the zeros (a log
    /// line, an eviction estimate); the HTTP surface is not — a dashboard told
    /// "0 batches" shows an empty install instead of a busy server.
    pub fn stats_checked(&self) -> Result<(StoreStats, usize)> {
        let (stats, series) = self.stats_snapshot()?;
        Ok((stats, series))
    }

    /// [`Store::stats`] plus the distinct-series count.
    ///
    /// `/api/v1/stats` reports both and the dashboard polls it every few
    /// seconds, so neither may scan. They get there differently:
    ///
    /// The counts, sums and time bounds are a [`RowAgg`], counted once (under
    /// the writer, see [`Store::row_agg`]) and adjusted by every path that
    /// changes the row set, so reading them is a mutex and a clone.
    /// `total_bytes` is a directory walk and is refreshed here, because
    /// flushing and archiving change it without changing a single row.
    ///
    /// The distinct-series count has no exact incremental form, so it is
    /// cached on a slow clock and recomputed only when that expires. One
    /// shared 60-second TTL made a poll pay for it once a minute: measured at
    /// 11 s against a 4-million-row tenant, a visible stall on an otherwise
    /// 40 ms endpoint. Ten minutes is far faster than series appear or
    /// disappear, and the gauge is a cardinality estimate, not a ledger.
    pub fn stats_snapshot(&self) -> Result<(StoreStats, usize)> {
        const SERIES_TTL: u64 = 600_000_000_000;
        let now = now_ns();
        let agg = self.row_agg()?;
        let cached = *self.series_cache.lock();
        if let Some((at, series)) = cached
            && now.saturating_sub(at) < SERIES_TTL
        {
            return Ok((agg.into_stats(self.disk_bytes()), series));
        }

        let conn = self.read_conn()?;
        let span = self.span_all("batches");
        let scan_started = std::time::Instant::now();
        let series = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM \
                     (SELECT DISTINCT service_name, profile_type, labels FROM {span})"
                ),
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count.max(0) as usize)
            .unwrap_or(0);
        *self.series_cache.lock() = Some((now, series));
        let elapsed = scan_started.elapsed();
        if elapsed > std::time::Duration::from_secs(1) {
            info!(
                elapsed_ms = elapsed.as_millis(),
                root = %self.cfg.root.display(),
                "distinct-series scan was slow; it runs once every ten minutes"
            );
        }
        Ok((agg.into_stats(self.disk_bytes()), series))
    }

    /// The row aggregate, counting it the first time it is asked for.
    ///
    /// The recount runs under the *writer* connection, not a read clone, and
    /// [`Store::append`] updates the aggregate before it gives the writer back.
    /// That ordering is what makes the arithmetic exact: there is no instant
    /// in which a batch is committed but not yet folded into the aggregate, so
    /// a recount can never overlap one and lose it.
    fn row_agg(&self) -> Result<RowAgg> {
        if let Some(agg) = *self.agg.lock() {
            return Ok(agg);
        }
        let conn = match self.write_conn() {
            Ok(conn) => conn,
            Err(err) => {
                // A flush or a compaction owns the writer. Answering "0
                // batches" would show an empty install, so fall back to a read
                // snapshot — but do not install it: a batch committing during
                // that scan may be missing from it, and the deltas added later
                // would be added to a number that already lost rows.
                warn!(error = %err, "counted the catalogue without the writer");
                let conn = self.read_conn()?;
                return self.recount(&conn);
            }
        };
        // Nothing can append while this thread owns the writer, so a re-check
        // under the guard is enough — no other thread can be counting.
        if let Some(agg) = *self.agg.lock() {
            return Ok(agg);
        }
        let started = std::time::Instant::now();
        let agg = self.recount(&conn)?;
        // Installed after the scan: the aggregate lock is never held across it.
        *self.agg.lock() = Some(agg);
        info!(
            batches = agg.batch_count,
            elapsed_ms = started.elapsed().as_millis(),
            root = %self.cfg.root.display(),
            "counted the archive for the catalogue"
        );
        Ok(agg)
    }

    /// Recount [`RowAgg`] from the database.
    ///
    /// The caller must own `conn` — the writer, or a maintenance connection
    /// that already holds it — so the recount cannot interleave with an append.
    fn recount(&self, conn: &Connection) -> Result<RowAgg> {
        let span = self.span_all("batches");
        let row = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*), COALESCE(SUM(total_value), 0), COALESCE(SUM(bytes), 0), \
                     MIN(start_ns), MAX(end_ns) FROM {span}"
                ),
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
            )
            .map_err(db_err)?;
        let (count, value, logical, earliest, latest) = row;
        Ok(RowAgg {
            batch_count: count.max(0) as usize,
            total_value: value.max(0) as u64,
            logical_bytes: logical.max(0) as u64,
            earliest_ns: earliest.map(|v| v.max(0) as u64),
            latest_ns: latest.map(|v| v.max(0) as u64),
        })
    }

    /// Physical footprint of the tenant's durable data: the Parquet files on
    /// local disk. The hot tier and DuckDB itself are in memory and disappear
    /// on restart, which is why they are not part of the disk cap.
    fn disk_bytes(&self) -> u64 {
        parquet::staged_bytes(&self.parquet)
    }

    /// Flush the in-memory hot tier to a validated local Parquet fragment.
    pub fn flush_hot(&self) -> Result<usize> {
        let conn = self.lock_conn(ConnUser::Flush);
        let moved = parquet::flush_hot(&conn, &self.parquet)?;
        self.last_flush_ns
            .store(now_ns(), std::sync::atomic::Ordering::Relaxed);
        // A flush moves rows from the hot tier into a file; the row count, the
        // value sum and the time bounds are all unchanged. Dropping the cached
        // catalogue here made every flush invalidate the whole tenant, and the
        // next poll re-scanned every file just to print the same numbers.
        Ok(moved)
    }

    /// [`Store::flush_hot`] that yields to queries.
    ///
    /// Unlike merge/upload, a flush is short and bounds the data-loss window
    /// (`--flush-interval`), so it must not wait for a 60-second "idle" period.
    /// It waits at most [`BACKGROUND_LOCK_WAIT`] and skips this tick if a query
    /// owns the connection; the next tick is seconds away.
    pub fn flush_hot_when_idle(&self) -> Result<usize> {
        let now = now_ns();
        let last = self
            .last_flush_ns
            .load(std::sync::atomic::Ordering::Relaxed);
        if now.saturating_sub(last) < self.cfg.flush_interval_ns() {
            return Ok(0);
        }
        let Some(conn) = self.try_lock_conn(ConnUser::Flush, BACKGROUND_LOCK_WAIT) else {
            return Ok(0);
        };
        let moved = parquet::flush_hot(&conn, &self.parquet)?;
        self.last_flush_ns
            .store(now, std::sync::atomic::Ordering::Relaxed);
        // Row counts survive a flush; see `flush_hot`.
        Ok(moved)
    }

    /// Merge each completed hour's local Parquet files into one final file.
    /// Throttled to once every ten minutes so the tick stays cheap.
    pub fn compact_local(&self) -> Result<parquet::CompactStats> {
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
        // Merging rewrites Parquet files only; it cannot touch the hot tables.
        // Keeping it on the maintenance connection is what stops a busy hour
        // from making every ingest and dashboard query queue up.
        let Some(conn) = self.maintenance_conn.try_lock_for(BACKGROUND_LOCK_WAIT) else {
            return Ok(parquet::CompactStats::default());
        };
        let stats = parquet::compact_local(&conn, &self.parquet, current_hour)?;
        drop(conn);
        // A merge rewrites the same rows into fewer files, so the row counts
        // the catalogue caches are still correct, and the file index was
        // refreshed inside `compact_local`. Invalidate nothing.
        Ok(stats)
    }

    /// Compact every hour, including the current one. Tests only.
    #[cfg(test)]
    pub(crate) fn compact_all_for_test(&self) -> Result<parquet::CompactStats> {
        let conn = self.maintenance_conn.lock();
        parquet::compact_local(&conn, &self.parquet, u64::MAX)
    }

    /// Upload completed local Parquet files to S3 and remove the local copies
    /// only after the object has been verified. Runs entirely on the
    /// maintenance connection; the read index is updated before the local
    /// files are deleted, so no query is left naming a path that is gone.
    pub fn archive_local(&self, now_ns: u64) -> Result<parquet::ArchiveStats> {
        if self.parquet.mode != StorageMode::S3 {
            return Ok(parquet::ArchiveStats::default());
        }
        let Some(s3) = &self.cfg.s3 else {
            return Ok(parquet::ArchiveStats::default());
        };
        let Some(conn) = self.maintenance_conn.try_lock_for(BACKGROUND_LOCK_WAIT) else {
            return Ok(parquet::ArchiveStats::default());
        };
        // Moving an hour from local staging to S3 does not change how many
        // rows exist, so the catalogue's cached counts stay valid.
        parquet::archive_local(&conn, &self.parquet, now_ns, s3)
    }

    /// Total bytes of Parquet files staged on local disk.
    pub fn staged_bytes(&self) -> u64 {
        parquet::staged_bytes(&self.parquet)
    }

    /// How many Parquet files a window would name for one table. Tests only:
    /// the whole point of sharding is that this number stays small.
    #[cfg(test)]
    pub(crate) fn window_file_count(&self, table: &str, from_ns: u64, until_ns: u64) -> usize {
        parquet::window_file_count(&self.parquet, table, from_ns, until_ns)
    }

    /// Rescan the local Parquet tree. Tests only: production code reindexes
    /// after every flush, merge or removal.
    #[cfg(test)]
    pub(crate) fn reindex(&self) {
        parquet::reindex_local(&self.parquet);
    }

    pub fn series(
        &self,
        from_ns: u64,
        until_ns: u64,
        selector: &Selector,
    ) -> Result<Vec<SeriesInfo>> {
        self.series_inner(from_ns, until_ns, selector)
    }

    fn series_inner(
        &self,
        from_ns: u64,
        until_ns: u64,
        selector: &Selector,
    ) -> Result<Vec<SeriesInfo>> {
        let conn = self.read_conn()?;
        let span = self.span("batches", from_ns, until_ns);
        let mut stmt = conn
            .prepare(&format!(
                // A gauge is reported as the newest batch of the series, not as
                // the sum of every batch in the window: `inuse_space` is memory
                // held at one instant, and adding ten instants multiplies it by
                // ten. `arg_max` picks the value that belongs to `MAX(end_ns)`.
                "SELECT service_name, profile_type, labels, \
                        CASE WHEN profile_type IN ('inuse_space', 'inuse_objects') \
                             THEN arg_max(total_value, end_ns) ELSE SUM(total_value) END, \
                        COUNT(*), MIN(start_ns), MAX(end_ns) FROM {span} \
                 WHERE end_ns > ? AND start_ns < ? GROUP BY 1, 2, 3"
            ))
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
    ) -> Result<Vec<(String, u64)>> {
        let mut map: HashMap<String, u64> = HashMap::new();
        for series in self.series(from_ns, until_ns, selector)? {
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
        Ok(out)
    }

    pub fn label_names(
        &self,
        from_ns: u64,
        until_ns: u64,
        selector: &Selector,
    ) -> Result<Vec<String>> {
        let mut set: std::collections::BTreeSet<String> = Default::default();
        for series in self.series(from_ns, until_ns, selector)? {
            set.insert(SERVICE_NAME_LABEL.to_string());
            for key in series.labels.keys() {
                set.insert(key.clone());
            }
        }
        Ok(set.into_iter().collect())
    }

    pub fn series_count(&self) -> usize {
        let Ok(conn) = self.read_conn() else {
            return 0;
        };
        let span = self.span_all("batches");
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM \
                 (SELECT DISTINCT service_name, profile_type, labels FROM {span})"
            ),
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count.max(0) as usize)
        .unwrap_or(0)
    }

    /// Is this exact series already known? Used to let an existing series keep
    /// writing once a tenant has hit its series limit.
    pub fn contains_series(&self, report: &Report) -> bool {
        let Ok(conn) = self.read_conn() else {
            return false;
        };
        let span = self.span_all("batches");
        conn.query_row(
            &format!(
                "SELECT 1 FROM {span} \
                 WHERE service_name = ? AND profile_type = ? AND labels = ? LIMIT 1"
            ),
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
        let archive_bytes: u64;

        let samples_t = self.wtable("samples");
        let batches_t = self.wtable("batches");
        let nodes_t = self.wtable("net_nodes");
        let edges_t = self.wtable("net_edges");
        {
            // Background work yields: if a query holds the connection, skip
            // this pass. Retention and eviction are measured in hours; the
            // next tick is a minute away and a query is what a user sees.
            let Some(conn) = self.try_lock_conn(ConnUser::Prune, BACKGROUND_LOCK_WAIT) else {
                return Ok(stats);
            };
            // The durable rows are Parquet files, not rows in a database file.
            // Retention and the hard disk cap therefore have to remove whole
            // committed files; deleting only from the hot tables reclaims no
            // disk space at all.
            let archives = parquet::prune_archives(
                &self.parquet,
                now_ns,
                self.cfg.retention_ns,
                self.cfg.max_disk_bytes,
            )?;
            archive_bytes = archives.bytes_reclaimed;
            stats.removed_files = archives.files_removed;
            // `prune_archives` already rescanned the file index; there are no
            // views left to rebuild.
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

            // In canonical modes the cap is enforced by removing whole
            // Parquet generations above. Deleting hot rows here cannot shrink
            // the archived dataset, and looping over `SUM(bytes)` would spin
            // forever on data that lives in files (seen as a 100-second prune
            // tick in the S3 staging test).
            if let Some(limit) = self.cfg.max_disk_bytes
                && !self.cfg.storage_mode.canonical().uses_parquet()
            {
                let mut guard = 0;
                let legacy_span = self.span_all("batches");
                loop {
                    let size: i64 = conn
                        .query_row(
                            &format!("SELECT COALESCE(SUM(bytes), 0) FROM {legacy_span}"),
                            [],
                            |row| row.get(0),
                        )
                        .unwrap_or(0);
                    if size.max(0) as u64 <= limit || guard >= 10_000 {
                        break;
                    }
                    guard += 1;
                    let oldest: Option<i64> = conn
                        .query_row(
                            &format!(
                                "SELECT batch_id FROM {legacy_span} \
                                 ORDER BY start_ns ASC, batch_id ASC LIMIT 1"
                            ),
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
            // The counts just changed. Recount while the writer is still held,
            // so both this pass and the next reader observe the pruned state
            // and no append can slip in between.
            *self.agg.lock() = Some(self.recount(&conn)?);
        }

        let after = self.stats();
        // Physical file size stays at its high-water mark after DELETE, so
        // report the logical size actually released instead.
        stats.removed_bytes = before
            .logical_bytes
            .saturating_sub(after.logical_bytes)
            .saturating_add(archive_bytes);
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
            let conn = self.lock_conn(ConnUser::Maintenance);
            conn.execute_batch(&format!(
                "DELETE FROM {samples_t}; DELETE FROM {stacks_t}; DELETE FROM {frames_t}; \
                 DELETE FROM {batches_t}; DELETE FROM {nodes_t}; DELETE FROM {edges_t};"
            ))
            .map_err(db_err)?;
            conn.execute_batch("CHECKPOINT").map_err(db_err)?;
            // Nothing is left to count. Set it under the writer so a reader
            // cannot observe the old totals after the rows are gone.
            *self.agg.lock() = Some(RowAgg::default());
        }
        // In canonical modes the durable rows are Parquet files under
        // `hour=<n>/`, so a clear must remove the whole table tree — a
        // one-level `read_dir` would leave every shard behind. Archived S3
        // objects are left for bucket lifecycle policies; for a full purge the
        // operator clears the S3 prefix.
        for def in parquet::TABLES {
            let dir = self.parquet.table_dir(def.name);
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::create_dir_all(&dir);
        }
        self.parquet.clear_index()?;
        parquet::reindex_local(&self.parquet);
        self.frame_cache.lock().clear();
        *self.series_cache.lock() = None;
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
        let conn = self.lock_conn(ConnUser::Maintenance);
        conn.execute_batch("CHECKPOINT").map_err(db_err)?;

        let samples_span = self.span_all("samples");
        let stacks_span = self.span_all("stacks");
        let orphan_stacks: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {samples_span} s LEFT JOIN {stacks_span} k \
                     USING (stack_id) WHERE k.stack_id IS NULL"
                ),
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
        let conn = self.lock_conn(ConnUser::Maintenance);
        let (broken, _) = self.dangling_stacks(&conn)?;
        // Canonical names are VIEWs; DELETE must target the in-memory hot
        // tables. Immutable Parquet files cannot be repaired in place.
        let samples_t = self.wtable("samples");
        let stacks_t = self.wtable("stacks");
        let removed = in_transaction(&conn, "COMMIT", |conn| -> Result<usize> {
            let mut removed = 0usize;
            for stack_id in broken {
                let id = stack_id as i64;
                removed += conn
                    .execute(
                        &format!("DELETE FROM {samples_t} WHERE stack_id = ?"),
                        params![id],
                    )
                    .map_err(db_err)?;
                removed += conn
                    .execute(
                        &format!("DELETE FROM {stacks_t} WHERE stack_id = ?"),
                        params![id],
                    )
                    .map_err(db_err)?;
            }
            // Samples whose stack is missing entirely (the other half of the
            // invariant checked by `fsck`).
            removed += conn
                .execute(
                    &format!(
                        "DELETE FROM {samples_t} WHERE NOT EXISTS \
                         (SELECT 1 FROM {stacks_t} k WHERE k.stack_id = {samples_t}.stack_id)"
                    ),
                    [],
                )
                .map_err(db_err)?;
            Ok(removed)
        })?;
        // CHECKPOINT after the commit, not inside it: a failed checkpoint must
        // not be mistaken for a failed transaction and trigger a ROLLBACK of
        // work DuckDB already made durable.
        conn.execute_batch("CHECKPOINT").map_err(db_err)?;
        // No aggregate to fix up: repair removes stacks and orphaned samples,
        // never a `batches` row, so the catalogue numbers are still exact.
        Ok(removed)
    }

    /// Stacks that cannot be decoded against the frame dictionary, and the
    /// number of dangling frame references found inside them.
    ///
    /// Blobs are opaque to SQL, so the check is done here: walk every stack and
    /// compare its ids against `frames`. Frames are compact enough to hold in
    /// memory; a store with millions of distinct frames makes this slow.
    fn dangling_stacks(&self, conn: &Connection) -> Result<(HashSet<u64>, usize)> {
        let frames_span = self.span_all("frames");
        let stacks_span = self.span_all("stacks");
        let known: HashSet<i64> = {
            let mut stmt = conn
                .prepare(&format!("SELECT frame_id FROM {frames_span}"))
                .map_err(db_err)?;
            let mut rows = stmt.query([]).map_err(db_err)?;
            let mut set = HashSet::new();
            while let Some(row) = rows.next().map_err(db_err)? {
                set.insert(row.get::<_, i64>(0).map_err(db_err)?);
            }
            set
        };

        let mut stmt = conn
            .prepare(&format!("SELECT stack_id, frames FROM {stacks_span}"))
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
        let conn = self.write_conn()?;
        in_transaction(&conn, "COMMIT", |conn| -> Result<()> {
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
                         client_container, server_container, client_pod, server_pod, \
                         connections, retransmits, tx_bytes, rx_bytes, rtt_us_sum, rtt_samples, \
                         host, sql) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
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
                        edge.client_pod,
                        edge.server_pod,
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
        })?;
        // `total_bytes` is a directory walk, refreshed on every read of the
        // cache; the row counts belong to `batches`, which this does not touch.
        // Clearing the cache here made every service-map window — one per node
        // every ten seconds — force the next dashboard poll to rescan the
        // tenant's whole archive.
        Ok(())
    }

    /// Nodes of the map, busiest first.
    pub fn net_nodes(&self, from_ns: u64, until_ns: u64, limit: usize) -> Result<Vec<MapNode>> {
        let conn = self.read_conn()?;
        let span = self.span("net_nodes", from_ns, until_ns);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT node_id, any_value(namespace), any_value(workload), any_value(service), \
                        any_value(kind), sum(rx_bytes), sum(tx_bytes), sum(connections), \
                        sum(retransmits), \
                        arg_max(pods, (bucket_ns, pods)), arg_max(ready, (bucket_ns, pods)), \
                        arg_max(instances, (bucket_ns, pods)), \
                        arg_max(containers, (bucket_ns, pods)) \
                 FROM {span} WHERE bucket_ns >= ? AND bucket_ns < ? \
                 GROUP BY node_id \
                 ORDER BY sum(connections) + (sum(rx_bytes) + sum(tx_bytes)) / 1024 DESC LIMIT ?"
            ))
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
        let conn = self.read_conn()?;
        let span = self.span("net_edges", from_ns, until_ns);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT client_id, server_id, protocol, client_container, server_container, \
                        client_pod, server_pod, \
                        sum(connections), sum(retransmits), \
                        sum(tx_bytes), sum(rx_bytes), sum(rtt_us_sum), sum(rtt_samples), \
                        arg_max(nullif(host, ''), bucket_ns), \
                        arg_max(nullif(sql, ''), bucket_ns) \
                 FROM {span} WHERE bucket_ns >= ? AND bucket_ns < ? \
                 GROUP BY client_id, server_id, protocol, client_container, server_container, \
                          client_pod, server_pod, \
                          CASE WHEN server_id = 'external' THEN coalesce(host, '') ELSE '' END \
                 HAVING sum(connections) + sum(tx_bytes) + sum(rx_bytes) > 0 \
                 ORDER BY sum(connections) + sum(tx_bytes) / 1024 DESC LIMIT 4000"
            ))
            .map_err(db_err)?;
        let mut rows = stmt
            .query(params![ts(from_ns), ts(until_ns)])
            .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(db_err)? {
            let rtt_us_sum = row.get::<_, i64>(11).unwrap_or(0).max(0) as u64;
            let rtt_samples = row.get::<_, i64>(12).unwrap_or(0).max(0) as u64;
            out.push(MapEdge {
                client: row.get(0).map_err(db_err)?,
                server: row.get(1).map_err(db_err)?,
                protocol: row.get(2).map_err(db_err)?,
                client_container: row.get(3).unwrap_or_default(),
                server_container: row.get(4).unwrap_or_default(),
                // Old files read these as NULL; an empty id means "unknown pod".
                client_pod: row
                    .get::<_, Option<String>>(5)
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                server_pod: row
                    .get::<_, Option<String>>(6)
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                connections: row.get::<_, i64>(7).unwrap_or(0).max(0) as u64,
                retransmits: row.get::<_, i64>(8).unwrap_or(0).max(0) as u64,
                tx_bytes: row.get::<_, i64>(9).unwrap_or(0).max(0) as u64,
                rx_bytes: row.get::<_, i64>(10).unwrap_or(0).max(0) as u64,
                // Mean round-trip time over the connections that closed inside
                // the window; zero when none did. Keep the denominator so the
                // panel can weight edges correctly.
                rtt_us: rtt_us_sum.checked_div(rtt_samples).unwrap_or(0),
                rtt_samples,
                // Old Parquet files predate these columns and read as NULL.
                host: row
                    .get::<_, Option<String>>(13)
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                sql: row
                    .get::<_, Option<String>>(14)
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
        let conn = self.read_conn()?;
        let span = self.span("net_nodes", from_ns, until_ns);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT CAST((bucket_ns - ?) / ? AS BIGINT) AS slot, \
                        sum(rx_bytes), sum(tx_bytes), sum(connections), sum(retransmits), \
                        max(pods), max(ready) \
                 FROM {span} WHERE node_id = ? AND bucket_ns >= ? AND bucket_ns < ? \
                 GROUP BY slot ORDER BY slot"
            ))
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
                rtt_samples: 0,
                pods: row.get::<_, i64>(5).unwrap_or(0).max(0) as u32,
                ready: row.get::<_, i64>(6).unwrap_or(0).max(0) as u32,
            });
        }
        Ok(out)
    }

    /// The same buckets, but for one destination host of the aggregate
    /// `external` node.
    ///
    /// A host node is synthesized from the edges that name it, so it has no
    /// rows of its own in `net_nodes`; its counters *are* the edges. Bytes are
    /// reported from the host's side of the call, which is what
    /// [`Store::net_edge_history`] already does when the node is the server.
    ///
    /// The match is on `lower(host)` because that is how the node id is
    /// built; the stored value keeps whatever case the client typed.
    pub fn net_host_edge_history(
        &self,
        host: &str,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
        protocol: Option<&str>,
    ) -> Result<Vec<MapHistoryPoint>> {
        let step = ts(step_ns.max(1));
        let conn = self.read_conn()?;
        let span = self.span("net_edges", from_ns, until_ns);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT CAST((bucket_ns - ?) / ? AS BIGINT) AS slot, \
                        sum(connections), sum(retransmits), \
                        sum(tx_bytes), sum(rx_bytes), \
                        sum(rtt_us_sum), sum(rtt_samples) \
                 FROM {span} \
                 WHERE server_id = 'external' \
                   AND (lower(host) = ? OR lower(host) = ? || '.') \
                   AND (? IS NULL OR protocol = ?) \
                   AND bucket_ns >= ? AND bucket_ns < ? \
                 GROUP BY slot ORDER BY slot"
            ))
            .map_err(db_err)?;
        let mut rows = stmt
            .query(params![
                ts(from_ns),
                step,
                host,
                host,
                protocol,
                protocol,
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
                // From the host's side: what the clients sent it is its rx.
                rx_bytes: row.get::<_, i64>(3).unwrap_or(0).max(0) as u64,
                tx_bytes: row.get::<_, i64>(4).unwrap_or(0).max(0) as u64,
                connections: row.get::<_, i64>(1).unwrap_or(0).max(0) as u64,
                retransmits: row.get::<_, i64>(2).unwrap_or(0).max(0) as u64,
                rtt_us: rtt_us_sum.checked_div(rtt_samples).unwrap_or(0),
                rtt_samples,
                pods: 0,
                ready: 0,
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
        protocol: Option<&str>,
    ) -> Result<Vec<MapHistoryPoint>> {
        let step = ts(step_ns.max(1));
        let conn = self.read_conn()?;
        let span = self.span("net_edges", from_ns, until_ns);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT CAST((bucket_ns - ?) / ? AS BIGINT) AS slot, \
                        sum(connections), sum(retransmits), \
                        sum(CASE WHEN client_id = ? THEN tx_bytes ELSE rx_bytes END), \
                        sum(CASE WHEN client_id = ? THEN rx_bytes ELSE tx_bytes END), \
                        sum(rtt_us_sum), sum(rtt_samples) \
                 FROM {span} \
                 WHERE (client_id = ? OR server_id = ?) \
                   AND (? IS NULL OR protocol = ?) \
                   AND bucket_ns >= ? AND bucket_ns < ? \
                 GROUP BY slot ORDER BY slot"
            ))
            .map_err(db_err)?;
        let mut rows = stmt
            .query(params![
                ts(from_ns),
                step,
                node,
                node,
                node,
                node,
                protocol,
                protocol,
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
                rtt_samples,
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
        let conn = self.read_conn()?;
        // The span has to cover the lookback too, or a batch that started just
        // before the window would be pruned away by the hour shard.
        let span = self.span("net_edges", from_ns.saturating_sub(LOOKBACK_NS), until_ns);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT bucket_ns, sum({}) FROM {span} \
                 WHERE bucket_ns >= ? AND bucket_ns < ? \
                 GROUP BY bucket_ns ORDER BY bucket_ns",
                net_metric_column(metric)
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
        let conn = self.read_conn()?;
        let span = self.span("samples", from_ns, until_ns);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT service_name, labels, sum(value) FROM {span} \
                 WHERE profile_type = 'cpu' AND start_ns < ? AND end_ns > ? \
                 GROUP BY service_name, labels"
            ))
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

impl lidlesseye_servicemap::ServiceMapStore for Store {
    type Error = Error;

    fn net_nodes(&self, from_ns: u64, until_ns: u64, limit: usize) -> Result<Vec<MapNode>> {
        Store::net_nodes(self, from_ns, until_ns, limit)
    }

    fn net_edges(&self, from_ns: u64, until_ns: u64) -> Result<Vec<MapEdge>> {
        Store::net_edges(self, from_ns, until_ns)
    }

    fn net_node_history(
        &self,
        node: &str,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
    ) -> Result<Vec<MapHistoryPoint>> {
        Store::net_node_history(self, node, from_ns, until_ns, step_ns)
    }

    fn net_edge_history(
        &self,
        node: &str,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
        protocol: Option<&str>,
    ) -> Result<Vec<MapHistoryPoint>> {
        Store::net_edge_history(self, node, from_ns, until_ns, step_ns, protocol)
    }

    fn net_host_edge_history(
        &self,
        host: &str,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
        protocol: Option<&str>,
    ) -> Result<Vec<MapHistoryPoint>> {
        Store::net_host_edge_history(self, host, from_ns, until_ns, step_ns, protocol)
    }

    fn net_histogram_metric(
        &self,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
        buckets: usize,
        metric: NetMetric,
    ) -> Result<Vec<u64>> {
        Store::net_histogram_metric(self, from_ns, until_ns, step_ns, buckets, metric)
    }

    fn cpu_by_service(&self, from_ns: u64, until_ns: u64) -> Result<HashMap<String, u64>> {
        Store::cpu_by_service(self, from_ns, until_ns)
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
    let temp_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".duckdb-tmp");
    clear_stale_spill(&temp_dir);
    let settings: Vec<String> = vec![
        "SET preserve_insertion_order=false".to_string(),
        // DuckDB defaults to 80% of host RAM, which is dangerous in a 2 GiB
        // pod. Cap the hash tables behind long GROUP BYs so wide ranges spill
        // to the temp directory instead of getting the process OOM-killed.
        format!("SET memory_limit='{}'", cfg.duckdb_memory_limit()),
        format!("SET temp_directory='{}'", temp_dir.display()),
        // Keyed by file path, so a query that spans the same local hour files
        // as the previous one reads their footers from memory instead of from
        // disk. Off by default in DuckDB.
        "SET parquet_metadata_cache=true".to_string(),
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

/// Delete spill files left behind by a previous process.
///
/// DuckDB names its spill files after the pid that owns them and never reads
/// one it did not create, so anything already in the directory at open time is
/// dead weight. Nothing else reclaims it: the owner is the only process that
/// deletes its own spill, and by the time the next process starts the owner is
/// gone. One deployment carried 11 GB of it after a single wide scan, on a
/// volume that also holds the tenant's data.
///
/// Only safe because a data directory has exactly one writer — the chart pins
/// `replicaCount: 1` with `strategy: Recreate` for the same reason.
fn clear_stale_spill(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut reclaimed = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("duckdb_temp_") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            reclaimed = reclaimed.saturating_add(meta.len());
        }
        let _ = std::fs::remove_file(entry.path());
    }
    if reclaimed > 0 {
        info!(
            bytes = reclaimed,
            dir = %dir.display(),
            "reclaimed stale DuckDB spill files"
        );
    }
}

fn create_schema(conn: &Connection) -> Result<()> {
    let started = std::time::Instant::now();
    // A current database must stay read-only at the DDL/DML level. The old
    // code ran `CREATE INDEX IF NOT EXISTS` and `INSERT OR REPLACE INTO meta`
    // on *every* open; on a large upgraded file that is a write to DuckDB's
    // catalog and WAL before the first query, and it can hold the tenant's
    // only connection long enough for the dashboard's first request to time
    // out.
    if schema_is_current(conn) {
        info!(
            elapsed_ms = started.elapsed().as_millis(),
            "DuckDB schema is current; skipping migration"
        );
        return Ok(());
    }
    info!("DuckDB schema needs migration; progress will be logged by stage");
    conn.execute_batch(
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
            client_pod VARCHAR DEFAULT '' NOT NULL,
            server_pod VARCHAR DEFAULT '' NOT NULL,
            connections BIGINT NOT NULL,
            retransmits BIGINT NOT NULL,
            tx_bytes BIGINT NOT NULL,
            rx_bytes BIGINT NOT NULL,
            rtt_us_sum BIGINT NOT NULL,
            rtt_samples BIGINT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS meta (key VARCHAR PRIMARY KEY, value VARCHAR NOT NULL);",
    )
    .map_err(db_err)?;
    info!(
        elapsed_ms = started.elapsed().as_millis(),
        stage = "tables",
        "DuckDB migration step complete"
    );
    add_missing_columns(conn)?;
    info!(
        elapsed_ms = started.elapsed().as_millis(),
        stage = "columns",
        "DuckDB migration step complete"
    );
    create_indexes(conn)?;
    info!(
        elapsed_ms = started.elapsed().as_millis(),
        stage = "indexes",
        "DuckDB migration step complete"
    );
    update_schema_version(conn)?;
    info!(
        elapsed_ms = started.elapsed().as_millis(),
        "DuckDB schema is ready"
    );
    Ok(())
}

/// Names of the tables present in the database right now.
fn table_names(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn
        .prepare("SELECT table_name FROM information_schema.tables WHERE table_schema = 'main'")
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(db_err)?;
    rows.collect::<std::result::Result<HashSet<_>, _>>()
        .map_err(db_err)
}

/// Names of DuckDB's indexes. `duckdb_indexes()` is a table function, so a
/// build without it simply takes the migration path rather than failing open.
fn index_names(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn
        .prepare("SELECT index_name FROM duckdb_indexes()")
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(db_err)?;
    rows.collect::<std::result::Result<HashSet<_>, _>>()
        .map_err(db_err)
}

/// Is every object this binary expects already present?
///
/// Checking the catalog before issuing any DDL is what keeps a normal restart
/// cheap. A version stamp alone is not enough: earlier releases added columns
/// without bumping it, so the added columns and indexes are checked too.
fn schema_is_current(conn: &Connection) -> bool {
    let Ok(tables) = table_names(conn) else {
        return false;
    };
    if REQUIRED_TABLES.iter().any(|name| !tables.contains(*name)) {
        return false;
    }
    let Ok(version) = conn.query_row(
        "SELECT value FROM meta WHERE key = 'schema_version'",
        [],
        |row| row.get::<_, String>(0),
    ) else {
        return false;
    };
    if version != SCHEMA_VERSION.to_string() {
        return false;
    }
    let Ok(edges) = existing_columns(conn, "net_edges") else {
        return false;
    };
    let edges: HashSet<&str> = edges.iter().map(String::as_str).collect();
    if NET_EDGE_COLUMNS
        .iter()
        .any(|(column, _)| !edges.contains(*column))
    {
        return false;
    }
    let Ok(nodes) = existing_columns(conn, "net_nodes") else {
        return false;
    };
    let nodes: HashSet<&str> = nodes.iter().map(String::as_str).collect();
    if NET_NODE_COLUMNS
        .iter()
        .any(|(column, _)| !nodes.contains(*column))
    {
        return false;
    }
    let Ok(indexes) = index_names(conn) else {
        return false;
    };
    !INDEXES.iter().any(|(name, _)| !indexes.contains(*name))
}

/// Stamp the schema only after every migration step succeeded. An interrupted
/// migration must be attempted again on the next open.
fn update_schema_version(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "INSERT OR REPLACE INTO meta VALUES ('schema_version', '{SCHEMA_VERSION}');"
    ))
    .map_err(db_err)
}

/// Indexes the columns every read path filters or groups on. DuckDB keeps
/// min/max zone maps per row group, but analyst queries that repeatedly pin a
/// bucket or a node still benefit from an ART index — especially on the wide
/// time ranges that were slow before. Created defensively so an older database
/// or a build without ART support still opens.
/// Indexes every read path relies on, paired with the name needed to rebuild
/// them. Schema migrations must drop these before altering a table: DuckDB
/// refuses `ALTER TABLE` while an index depends on the table.
const INDEXES: &[(&str, &str)] = &[
    (
        "idx_net_edges_bucket",
        "CREATE INDEX IF NOT EXISTS idx_net_edges_bucket ON net_edges(bucket_ns)",
    ),
    (
        "idx_net_edges_node",
        "CREATE INDEX IF NOT EXISTS idx_net_edges_node ON net_edges(client_id, server_id)",
    ),
    (
        "idx_net_nodes_bucket",
        "CREATE INDEX IF NOT EXISTS idx_net_nodes_bucket ON net_nodes(bucket_ns)",
    ),
    (
        "idx_net_nodes_node",
        "CREATE INDEX IF NOT EXISTS idx_net_nodes_node ON net_nodes(node_id)",
    ),
    (
        "idx_samples_time",
        "CREATE INDEX IF NOT EXISTS idx_samples_time ON samples(profile_type, start_ns, end_ns)",
    ),
    (
        "idx_batches_time",
        "CREATE INDEX IF NOT EXISTS idx_batches_time ON batches(profile_type, start_ns, end_ns)",
    ),
];

/// Tables every DuckDB-mode store must have. An old or partially migrated
/// database misses one of these and takes the slow migration path on open.
const REQUIRED_TABLES: &[&str] = &[
    "batches",
    "samples",
    "stacks",
    "frames",
    "net_nodes",
    "net_edges",
    "meta",
];

/// Columns added after the first release of each service-map table. Kept at
/// module scope so the fast-path check and the migration use one definition.
const NET_EDGE_COLUMNS: &[(&str, &str)] = &[
    ("tx_bytes", "BIGINT DEFAULT 0 NOT NULL"),
    ("rx_bytes", "BIGINT DEFAULT 0 NOT NULL"),
    ("rtt_us_sum", "BIGINT DEFAULT 0 NOT NULL"),
    ("rtt_samples", "BIGINT DEFAULT 0 NOT NULL"),
    ("client_container", "VARCHAR DEFAULT '' NOT NULL"),
    ("server_container", "VARCHAR DEFAULT '' NOT NULL"),
    ("client_pod", "VARCHAR DEFAULT '' NOT NULL"),
    ("server_pod", "VARCHAR DEFAULT '' NOT NULL"),
    ("host", "VARCHAR DEFAULT '' NOT NULL"),
    ("sql", "VARCHAR DEFAULT '' NOT NULL"),
];
const NET_NODE_COLUMNS: &[(&str, &str)] = &[
    ("pods", "BIGINT DEFAULT 0 NOT NULL"),
    ("ready", "BIGINT DEFAULT 0 NOT NULL"),
    ("instances", "VARCHAR DEFAULT '[]' NOT NULL"),
    ("containers", "VARCHAR DEFAULT '[]' NOT NULL"),
];

/// Drop the indexes that `create_indexes` owns so a migration can alter their
/// tables. Only called when a column actually needs adding: doing this on
/// every open makes concurrent readers fight over DuckDB's WAL.
fn drop_indexes(conn: &Connection) -> Result<()> {
    for (name, _) in INDEXES {
        conn.execute_batch(&format!("DROP INDEX IF EXISTS {name}"))
            .map_err(db_err)?;
    }
    Ok(())
}

fn create_indexes(conn: &Connection) -> Result<()> {
    let existing = index_names(conn).unwrap_or_default();
    for (name, sql) in INDEXES {
        if existing.contains(*name) {
            continue;
        }
        if let Err(err) = conn.execute_batch(sql) {
            warn!(index = *name, error = %err, "skipping index");
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
    let edges = missing_columns(conn, "net_edges", NET_EDGE_COLUMNS)?;
    let nodes = missing_columns(conn, "net_nodes", NET_NODE_COLUMNS)?;
    // The common case — an already-current database — must stay read-only at
    // the DDL level. Dropping and recreating indexes every time a second
    // connection opens the same file makes DuckDB fight over its WAL.
    if edges.is_empty() && nodes.is_empty() {
        return Ok(());
    }
    let columns: Vec<&str> = edges
        .iter()
        .chain(nodes.iter())
        .map(|(column, _)| *column)
        .collect();
    info!(columns = ?columns, "adding columns to an older DuckDB schema");

    drop_indexes(conn)?;
    add_columns(conn, "net_edges", &edges)?;
    add_columns(conn, "net_nodes", &nodes)?;
    create_indexes(conn)
}

fn existing_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_name = ?",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![table], |row| row.get::<_, String>(0))
        .map_err(db_err)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(db_err)
}

fn missing_columns<'a>(
    conn: &Connection,
    table: &str,
    added: &'a [(&'a str, &'a str)],
) -> Result<Vec<(&'a str, &'a str)>> {
    let existing = existing_columns(conn, table)?;
    Ok(added
        .iter()
        .copied()
        .filter(|(column, _)| !existing.iter().any(|name| name.as_str() == *column))
        .collect())
}

fn add_columns(conn: &Connection, table: &str, added: &[(&str, &str)]) -> Result<()> {
    for (column, definition) in added {
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
/// A unique table name for per-query staging data.
///
/// Reads run on separate connections onto the same in-memory database, so a
/// fixed name would let concurrent scans `DELETE` each other's ids and drop
/// stacks from the result.
fn next_staging_table() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("lidlesseye_stage_{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

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

/// Run `f` as one explicit transaction on the shared connection.
///
/// DuckDB aborts a transaction on *any* statement failure — including a
/// failing `COMMIT` — and afterwards accepts only `ROLLBACK`. Leaving that
/// state behind is why one bad ingest made every later upload fail with
/// `Current transaction is aborted`. This helper clears a stale transaction
/// before `BEGIN` and rolls back on every error path, including commit.
fn in_transaction<T>(
    conn: &Connection,
    commit_sql: &str,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    // `ROLLBACK` on an idle connection is an expected error; it is the only
    // statement DuckDB permits while a transaction is aborted, so it is also
    // the recovery step for a transaction poisoned by an earlier failure.
    let _ = conn.execute_batch("ROLLBACK");
    if conn
        .execute_batch("BEGIN TRANSACTION")
        .map_err(db_err)
        .is_err()
    {
        // The recovery ROLLBACK itself may have been interrupted. One retry is
        // enough; after that the write fails cleanly instead of poisoning the
        // connection for the next caller too.
        let _ = conn.execute_batch("ROLLBACK");
        conn.execute_batch("BEGIN TRANSACTION").map_err(db_err)?;
    }

    let result = (|| -> Result<T> {
        let value = f(conn)?;
        conn.execute_batch(commit_sql).map_err(db_err)?;
        Ok(value)
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
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
    fn lock_stats_expose_the_current_holder() {
        let (store, _dir) = temp_store("lock-stats");
        assert_eq!(store.lock_stats().holder, "idle");

        let guard = store.lock_conn(ConnUser::Compact);
        let held = store.lock_stats();
        assert_eq!(held.holder, "compact");
        let waits = held.waits;
        drop(guard);

        assert_eq!(store.lock_stats().holder, "idle");
        store
            .telemetry
            .record_wait(ConnUser::Read, std::time::Duration::from_millis(25));
        let stats = store.lock_stats();
        assert_eq!(stats.waits, waits + 1);
        assert_eq!(stats.max_wait_ms, 25);
        assert_eq!(stats.slow_waits, 0);
    }

    /// A poll that overlaps an ingest must never report fewer batches than are
    /// already written.
    ///
    /// The catalogue numbers used to be a scan whose result was cached after
    /// the fact, so a batch that committed while that scan was running was
    /// counted by neither side and the dashboard stayed one short until the
    /// cache expired. Counting under the writer, and folding each append in
    /// before the writer is released, is what closes that window: `stats` is
    /// exact after every append, including the very first ones that race the
    /// initial recount.
    #[test]
    fn a_poll_during_ingest_never_undercounts() {
        let (store, dir) = temp_store("stats-race");
        std::thread::scope(|scope| {
            let poller = scope.spawn(|| {
                for _ in 0..100 {
                    let _ = store.stats_snapshot().unwrap();
                }
            });
            for i in 0..100u64 {
                store
                    .append(&report("svc", 1_000 + i, "a", &["hot"], 10 + i))
                    .unwrap();
                assert_eq!(
                    store.stats().batch_count,
                    i as usize + 1,
                    "batch {i} is not visible to the writer that just wrote it"
                );
            }
            poller.join().unwrap();
        });
        let stats = store.stats();
        assert_eq!(stats.batch_count, 100);
        assert_eq!(stats.total_value, (0..100).map(|i| 10 + i).sum::<u64>());
        assert_eq!(stats.earliest_ns, Some(1_000));
        assert_eq!(stats.latest_ns, Some(1_099 + 10_000_000_000));
        std::fs::remove_dir_all(dir).unwrap();
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

        let all = store
            .select(&ProfileType::cpu(), 0, 10_000, &Selector::default())
            .unwrap();
        assert_eq!(all.len(), 3);

        let only_checkout = store
            .select(
                &ProfileType::cpu(),
                0,
                10_000,
                &Selector::parse(r#"{service_name="checkout"}"#).unwrap(),
            )
            .unwrap();
        assert_eq!(only_checkout.len(), 2);

        let narrowed = store
            .select(
                &ProfileType::cpu(),
                0,
                10_000,
                &Selector::parse(r#"{service_name="checkout",pod="b"}"#).unwrap(),
            )
            .unwrap();
        assert_eq!(narrowed.len(), 1);

        // Only flushed rows survive a restart; the hot tier is intentionally
        // in memory and represents the configured RPO window.
        store.flush_hot().unwrap();
        let store2 = Store::open(StoreConfig::new(&dir)).unwrap();
        assert_eq!(store2.stats().batch_count, 3);
        assert_eq!(store2.stats().total_value, 3_000);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_indexed_old_net_edges_table_upgrades_in_place() {
        let dir = std::env::temp_dir().join(format!("lidlesseye-upgrade-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        {
            // An old release's file: the column added later is absent, and
            // indexes depend on the table. The new store must export rows to
            // Parquet rather than try to ALTER a file it will never open.
            let conn = Connection::open(dir.join(DB_FILE)).unwrap();
            conn.execute_batch(
                "CREATE TABLE net_edges (
                    bucket_ns BIGINT NOT NULL,
                    client_id VARCHAR NOT NULL,
                    server_id VARCHAR NOT NULL,
                    protocol VARCHAR NOT NULL,
                    client_container VARCHAR DEFAULT '' NOT NULL,
                    server_container VARCHAR DEFAULT '' NOT NULL,
                    client_pod VARCHAR DEFAULT '' NOT NULL,
                    server_pod VARCHAR DEFAULT '' NOT NULL,
                    connections BIGINT NOT NULL,
                    retransmits BIGINT NOT NULL,
                    tx_bytes BIGINT NOT NULL,
                    rx_bytes BIGINT NOT NULL,
                    rtt_us_sum BIGINT NOT NULL
                 );
                 CREATE INDEX idx_net_edges_bucket ON net_edges(bucket_ns);
                 CREATE INDEX idx_net_edges_node ON net_edges(client_id, server_id);
                 INSERT INTO net_edges VALUES
                   (1000, 'a', 'b', 'tcp', '', '', 'pod-a', 'pod-b', 1, 0, 10, 10, 250);",
            )
            .unwrap();
        }

        let store = Store::open(StoreConfig::new(&dir)).unwrap();
        let edges = store.net_edges(0, u64::MAX).unwrap();
        assert_eq!(edges.len(), 1, "legacy rows must survive the upgrade");
        assert_eq!(edges[0].rtt_samples, 0, "a missing column reads as zero");
        assert!(!dir.join(DB_FILE).exists(), "old DuckDB file was retired");
        let parquet = dir.join("parquet").join("net_edges");
        assert!(
            std::fs::read_dir(&parquet).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".parquet")),
            "legacy rows must be exported as Parquet"
        );
        drop(store);

        // A second open is idempotent and still sees exactly one row.
        let store = Store::open(StoreConfig::new(&dir)).unwrap();
        assert_eq!(store.net_edges(0, u64::MAX).unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Sharding exists so that a query names the hours it asks for. Without
    /// it a one-hour request over a week of history made DuckDB open every
    /// footer in the archive — the 60-second timeouts that started this work.
    #[test]
    fn a_query_window_names_only_the_hours_it_needs() {
        let (store, dir) = temp_store("window-shard");
        const HOUR: u64 = 3_600_000_000_000;
        store
            .append(&report("api", HOUR, "a", &["hot", "main"], 10))
            .unwrap();
        store
            .append(&report("api", 3 * HOUR, "a", &["hot", "main"], 32))
            .unwrap();
        assert!(store.flush_hot().unwrap() > 0);

        let all = store.window_file_count("samples", 0, u64::MAX);
        let one = store.window_file_count("samples", HOUR, HOUR + 1_000_000);
        assert_eq!(all, 2, "both hours are indexed separately");
        assert_eq!(one, 1, "a one-hour window must name one file");

        // Narrowing the files must not lose rows.
        assert_eq!(store.stats().batch_count, 2);
        let rows = store
            .select(&ProfileType::cpu(), 0, u64::MAX, &Selector::default())
            .unwrap();
        let total: u64 = rows.iter().map(|b| b.total_value).sum();
        assert_eq!(total, 42, "both hours are still readable through the span");

        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Reads used to share one mutex-guarded connection, so holding two at
    /// once was impossible and a wide scan queued every other request until
    /// the HTTP deadline. The pool must hand out distinct connections.
    #[test]
    fn several_reads_can_be_held_at_the_same_time() {
        let (store, dir) = temp_store("concurrent-reads");
        store
            .append(&report("api", 1_000, "a", &["hot", "main"], 7))
            .unwrap();

        let first = store.read_conn().expect("first read connection");
        let second = store.read_conn().expect("second read connection");
        let a: i64 = first.query_row("SELECT 42", [], |row| row.get(0)).unwrap();
        let b: i64 = second.query_row("SELECT 42", [], |row| row.get(0)).unwrap();
        assert_eq!((a, b), (42, 42), "both connections must answer");

        drop(first);
        drop(second);
        // The slots are reusable afterwards.
        let again = store.read_conn().expect("a reused read connection");
        let count: i64 = again
            .query_row("SELECT COUNT(*) FROM batches_hot", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        drop(again);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn physical_size_tracks_flushed_parquet_not_the_hot_tier() {
        let (store, dir) = temp_store("disk-bytes");
        store
            .append(&report("checkout", 1_000, "a", &["hot", "main"], 1_000))
            .unwrap();

        // The hot tier is in memory: no DuckDB file, no physical bytes yet.
        let hot = store.stats();
        assert!(hot.logical_bytes > 0, "{hot:?}");
        assert_eq!(hot.total_bytes, 0, "{hot:?}");
        assert!(!dir.join(DB_FILE).exists());

        store.flush_hot().unwrap();
        let flushed = store.stats();
        assert!(flushed.total_bytes > 0, "{flushed:?}");
        assert!(!dir.join(DB_FILE).exists());

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
        // The dictionary rows are written to the hot tier; there are no
        // canonical views, so read the hot table directly.
        let frames: i64 = conn
            .query_row("SELECT COUNT(*) FROM frames_hot", [], |row| row.get(0))
            .unwrap();
        let stacks: i64 = conn
            .query_row("SELECT COUNT(*) FROM stacks_hot", [], |row| row.get(0))
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
        // Persistence is Parquet: the hot tier must be flushed before a
        // retention pass can see (or evict) these rows.
        store.flush_hot().unwrap();
        drop(store);

        let mut cfg = StoreConfig::new(&dir);
        cfg.retention_ns = Some(hour + hour / 2);
        let mut store = Store::open(cfg).unwrap();
        let stats = store.prune(3 * hour).unwrap();
        assert!(stats.removed_bytes > 0);
        assert_eq!(store.stats().batch_count, 1);

        // The hard disk cap is measured on physical Parquet bytes, so a one
        // byte cap evicts every committed file.
        store.cfg.max_disk_bytes = Some(1);
        store
            .append(&report("web", 3 * hour, "b", &["hot"], 10))
            .unwrap();
        store.flush_hot().unwrap();
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

        let series = store.series(0, 10_000, &Selector::default()).unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].service_name, "api");
        assert_eq!(series[0].batch_count, 2);
        assert_eq!(series[0].total_value, 200);

        let pods = store
            .label_values("pod", 0, 10_000, &Selector::default())
            .unwrap();
        assert_eq!(pods.len(), 2);
        assert_eq!(pods[0].0, "a");

        let names = store.label_names(0, 10_000, &Selector::default()).unwrap();
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
                "INSERT INTO samples_hot VALUES (999, 'cpu', 'api', '', 1000, 2000, 999999, 7)",
                [],
            )
            .unwrap();
            // A truncated blob: not a whole number of 4-byte frame ids.
            conn.execute(
                "INSERT INTO stacks_hot VALUES (999998, ?)",
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
        assert!(store.path().is_dir(), "Parquet directory survives");

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

    /// External traffic is one grey blob unless the destination is used. The
    /// host the client asked for is already on the edge, so a request to
    /// `api.example.com` becomes its own node, case-folded, and the aggregate
    /// node keeps only what had no host at all.
    #[test]
    fn external_edges_split_into_a_node_per_host() {
        use crate::servicemap::{EXTERNAL_ID, ServiceEdge, ServiceNode, servicemap};

        let (store, dir) = temp_store("map-external-hosts");
        let start = 1_700_000_000_000_000_000u64;
        let window = 60_000_000_000u64;
        let edge = |host: &str, connections: u64| ServiceEdge {
            client: "shop/api".to_string(),
            server: EXTERNAL_ID.to_string(),
            protocol: "http".to_string(),
            connections,
            host: host.to_string(),
            ..Default::default()
        };
        store
            .append_servicemap(&ServiceMapBatch {
                start_ns: start,
                end_ns: start + window,
                nodes: vec![
                    ServiceNode::new("shop/api", NodeKind::Workload),
                    // The agent reports the aggregate node with the same
                    // counters its edges carry; the split subtracts from it.
                    ServiceNode {
                        connections: 10,
                        ..ServiceNode::new(EXTERNAL_ID, NodeKind::External)
                    },
                ],
                edges: vec![
                    edge("API.Example.com:8443", 4),
                    edge("api.example.com:8443", 1),
                    edge("db.example.com", 3),
                    // No host: TLS without the uprobe, or not HTTP at all.
                    edge("", 2),
                ],
                ..Default::default()
            })
            .unwrap();

        let map = servicemap(&store, start, start + window).unwrap();
        let ids: Vec<&str> = map.nodes.iter().map(|node| node.id.as_str()).collect();
        assert!(ids.contains(&"external/api.example.com:8443"), "{ids:?}");
        assert!(ids.contains(&"external/db.example.com"), "{ids:?}");
        assert!(
            ids.contains(&EXTERNAL_ID),
            "the host-less call keeps the aggregate node: {ids:?}"
        );

        let api = map
            .nodes
            .iter()
            .find(|node| node.id == "external/api.example.com:8443")
            .expect("host node");
        assert_eq!(api.connections, 5, "the two spellings are one host");
        assert_eq!(api.service, "api.example.com:8443");
        assert_eq!(api.kind, NodeKind::External);

        let aggregate = map
            .nodes
            .iter()
            .find(|node| node.id == EXTERNAL_ID)
            .expect("aggregate node");
        assert_eq!(
            aggregate.connections, 2,
            "the aggregate keeps only the host-less traffic, not the total"
        );
        // Every edge still points at a node that exists.
        for edge in &map.edges {
            assert!(ids.contains(&edge.client.as_str()), "{}", edge.client);
            assert!(ids.contains(&edge.server.as_str()), "{}", edge.server);
        }

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
            .net_edge_history("shop/a", start, start + 2 * step, step, None)
            .unwrap();
        assert_eq!(edges.len(), 2);
        // 50us over two samples, doubled because two agents reported it.
        assert_eq!(edges[0].rtt_us, 50);
        assert_eq!(edges[0].rtt_samples, 4);
        assert_eq!(edges[0].tx_bytes, 2 * 5000);
        assert_eq!(edges[0].rx_bytes, 2 * 10000);
        assert_eq!(edges[1].rtt_us, 450);
        assert_eq!(edges[1].rtt_samples, 2);

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

    /// The performance settings must actually take effect — a silently
    /// skipped PRAGMA would leave long scans slow without any signal.
    #[test]
    fn performance_settings_are_applied_and_no_db_file_is_created() {
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

        // Canonical modes have no persistent DuckDB file and no table indexes
        // to migrate: the hot tables are recreated on every open.
        assert!(!dir.join(DB_FILE).exists());

        drop(conn);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The interrupt must stop a running read from another thread — this is
    /// what frees a connection when a client disconnects or a query outlives
    /// its deadline. It must never touch the writer.
    #[test]
    fn interrupt_stops_a_running_query_from_another_thread() {
        let (store, dir) = temp_store("interrupt");
        let store = Arc::new(store);
        let worker = store.clone();
        let thread = std::thread::spawn(move || {
            let conn = worker.read_conn().expect("a read connection");
            // A deliberately long scan (20 billion rows to count) that cannot
            // finish before the interrupt below, but uses O(1) memory.
            conn.execute_batch(
                "SELECT count(*) FROM range(1000000000) t(x) CROSS JOIN range(20) u(y);",
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        store.interrupt();
        let result = thread.join().unwrap();
        assert!(result.is_err(), "the query should be interrupted");

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Parquet mode opens an *in-memory* database, and that path used to call
    /// `Connection::open_in_memory()`, which ignores the config object — so
    /// `--duckdb-threads=4` was silently dropped in exactly the mode that does
    /// the widest scans, and every scan ran on one thread per core instead.
    #[test]
    fn parquet_mode_honours_the_configured_thread_count() {
        let dir = std::env::temp_dir().join(format!("lidlesseye-threads-{}", uuid::Uuid::new_v4()));
        let mut cfg = StoreConfig::new(&dir);
        cfg.storage_mode = StorageMode::Parquet;
        cfg.s3 = Some(S3Config {
            bucket: "b".into(),
            prefix: "p".into(),
            ..Default::default()
        });
        cfg.threads = Some(3);
        let store = Store::open(cfg).expect("open parquet store");
        let conn = store.conn.lock();
        let threads: i64 = conn
            .query_row("SELECT current_setting('threads')::BIGINT", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(threads, 3, "parquet mode ignored the configured threads");
        drop(conn);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Spill files belong to the pid that wrote them; nothing deletes them once
    /// that process is gone. One deployment carried 11 GB of them.
    #[test]
    fn stale_duckdb_spill_is_reclaimed_at_open() {
        let root = std::env::temp_dir().join(format!("lidlesseye-spill-{}", uuid::Uuid::new_v4()));
        let tmp = root.join("parquet").join(".duckdb-tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let stale = tmp.join("duckdb_temp_11_0_storage_DEFAULT-0.tmp");
        std::fs::write(&stale, vec![0u8; 4096]).unwrap();
        // A file that is not ours must survive: only spill files are garbage.
        let keep = tmp.join("notes.txt");
        std::fs::write(&keep, b"keep").unwrap();

        let _store = Store::open(StoreConfig::new(&root)).expect("open store");

        assert!(!stale.exists(), "stale spill file was not removed");
        assert!(keep.exists(), "unrelated files must not be touched");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A failed statement or a failing COMMIT must never leave the shared
    /// connection inside an aborted transaction: DuckDB then rejects every
    /// later statement, which is exactly the upload storm reported in disk mode.
    #[test]
    fn failed_transaction_does_not_poison_later_writes() {
        let (store, dir) = temp_store("tx-failure");
        {
            let conn = store.conn.lock();
            let err = in_transaction(&conn, "COMMIT", |conn| -> Result<()> {
                conn.execute_batch("CREATE TABLE tx_probe (id INTEGER)")
                    .map_err(db_err)?;
                Err(Error::Db("injected failure".into()))
            })
            .unwrap_err();
            assert!(err.to_string().contains("injected failure"));

            // The failed CREATE was rolled back, so it can be created again.
            // The commit string fails *after* COMMIT succeeds; that used to
            // return before ROLLBACK and strand the connection.
            let err = in_transaction(
                &conn,
                "COMMIT; INSERT INTO no_such_tx_table VALUES (1)",
                |conn| {
                    conn.execute_batch("CREATE TABLE tx_probe (id INTEGER)")
                        .map_err(db_err)
                },
            )
            .unwrap_err();
            assert!(!err.to_string().is_empty());
        }
        store
            .append(&report("api", 1_000, "a", &["hot"], 10))
            .expect("store must be writable after failed transactions");
        assert_eq!(store.stats().batch_count, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Recovery must also work when the aborted transaction was left behind by
    /// an earlier process state, not by a failure this helper observed.
    #[test]
    fn stale_aborted_transaction_is_recovered_before_the_next_write() {
        let (store, dir) = temp_store("tx-stale");
        {
            let conn = store.conn.lock();
            conn.execute_batch("BEGIN TRANSACTION").unwrap();
            assert!(
                conn.execute_batch("INSERT INTO no_such_tx_table VALUES (1)")
                    .is_err()
            );
            // DuckDB now refuses even a harmless read until ROLLBACK.
            assert!(conn.execute_batch("SELECT 1").is_err());
        }
        store
            .append(&report("api", 1_000, "a", &["hot"], 10))
            .expect("next write must clear the stale transaction");
        assert_eq!(store.stats().batch_count, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The watchdog handle must interrupt a live read, but be inert once that
    /// read has finished — otherwise a late tick lands on the next ingest.
    #[test]
    fn interrupt_is_scoped_to_a_live_read() {
        let (store, dir) = temp_store("interrupt-scope");
        let store = Arc::new(store);
        let reader = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                let conn = store.read_conn().expect("read slot");
                conn.execute_batch(
                    "SELECT count(*) FROM range(1000000000) t(x) CROSS JOIN range(20) u(y);",
                )
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        store.interrupt();
        assert!(
            reader.join().unwrap().is_err(),
            "a live read must remain interruptible"
        );

        // No read is on the connection now, so the same call must be a no-op.
        store.interrupt();
        store
            .append(&report("api", 1_000, "a", &["hot"], 10))
            .expect("a late interrupt must not poison an ingest");
        assert_eq!(store.stats().batch_count, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The DNS tab asks for its own buckets. Without a protocol filter it
    /// would draw the node's whole traffic, which is a different question.
    #[test]
    fn edge_history_can_filter_by_application_protocol() {
        use crate::servicemap::{ServiceEdge, ServiceNode};

        let (store, dir) = temp_store("edge-history-protocol");
        let start = 1_700_000_000_000_000_000u64;
        let step = 10_000_000_000u64;
        store
            .append_servicemap(&ServiceMapBatch {
                start_ns: start,
                end_ns: start + step,
                nodes: vec![ServiceNode::new("shop/a", NodeKind::Workload)],
                edges: vec![
                    ServiceEdge {
                        client: "shop/a".to_string(),
                        server: "shop/b".to_string(),
                        protocol: "http".to_string(),
                        connections: 5,
                        tx_bytes: 500,
                        rx_bytes: 1_000,
                        rtt_us_sum: 500,
                        rtt_samples: 5,
                        ..Default::default()
                    },
                    ServiceEdge {
                        client: "shop/a".to_string(),
                        server: "dns/53".to_string(),
                        protocol: "dns".to_string(),
                        connections: 3,
                        tx_bytes: 30,
                        rx_bytes: 60,
                        rtt_us_sum: 90,
                        rtt_samples: 3,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            })
            .unwrap();

        let all = store
            .net_edge_history("shop/a", start, start + step, step, None)
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].connections, 8);

        let dns = store
            .net_edge_history("shop/a", start, start + step, step, Some("dns"))
            .unwrap();
        assert_eq!(dns.len(), 1);
        assert_eq!(dns[0].connections, 3);
        assert_eq!(dns[0].rtt_samples, 3);
        assert_eq!(dns[0].rtt_us, 30);

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Reopening an existing store only rebuilds in-memory tables and VIEWs;
    /// it must not rewrite the Parquet dataset or create a database file.
    #[test]
    fn reopening_a_parquet_store_does_not_write_data_files() {
        let (store, dir) = temp_store("schema-current");
        store
            .append(&report("api", 1_000, "a", &["hot"], 10))
            .unwrap();
        store.flush_hot().unwrap();
        drop(store);

        let before: Vec<String> = std::fs::read_dir(dir.join("parquet").join("batches"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let reopened = Store::open(StoreConfig::new(&dir)).unwrap();
        assert_eq!(reopened.stats().batch_count, 1);
        assert!(!dir.join(DB_FILE).exists());
        let after: Vec<String> = std::fs::read_dir(dir.join("parquet").join("batches"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(before, after, "opening a store must not rewrite Parquet");
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
