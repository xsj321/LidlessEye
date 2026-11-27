//! Storage backed by DuckDB 2.0.
//!
//! There is no storage engine of our own here, on purpose: transactional
//! writes, compression, zone maps, checkpointing and recovery are DuckDB's
//! problem, and each one of them was a thing that had to be re-proved whenever
//! it changed.
//!
//! What is left on our side is the part that is actually specific to
//! profiling —a small schema:
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
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::Instant;

use duckdb::{Connection, InterruptHandle, params};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::{Error, Result};
use crate::model::{Frame, ProfileType, Report, SERVICE_NAME_LABEL};
use crate::parquet::{self, ParquetState};
use crate::selector::{MatchOp, Selector};
use crate::servicemap::{
    InstanceInfo, MapEdge, MapHistoryPoint, MapNode, NodeKind, ServiceMapBatch, node_id,
};
use crate::time::now_ns;

/// Version of the table layout. Bumped when a change needs a migration.
pub const SCHEMA_VERSION: u32 = 2;

/// File name of the database inside a store root.
pub const DB_FILE: &str = "lidlesseye.duckdb";

/// Hard ceiling on rows returned by one [`Store::scan`], so a query over a
/// month of data fails as a truncated answer instead of as an OOM kill.
pub const MAX_SCAN_ROWS: usize = 2_000_000;

/// Ceiling on cached dictionary rows (`stack_id` -> frames, `frame_id` -> row).
///
/// The cache turns a dashboard that polls the same window into one dictionary
/// read instead of one per poll. It is a cache, not an index: when it fills the
/// process simply stops adding rows and falls back to reading Parquet, so the
/// worst case is the old latency and never an OOM. Two million stacks is around
/// a hundred megabytes, which is the same order as the ingest-side frame cache
/// and well under the default DuckDB memory limit.
const DICT_CACHE_ROWS: usize = 2_000_000;

/// How many prepared statements each read connection keeps.
///
/// Read statements embed the Parquet file list, so their text changes as flushes
/// land; a dashboard polling one window still repeats the same text, and
/// re-planning `read_parquet([...])` over hundreds of paths is a large part of a
/// read's fixed cost. Sixty-four plans is a few megabytes at most.
const PREPARE_CACHE: usize = 64;
/// File name of the on-disk dictionary snapshot, inside `<root>/cache/`.
const DICT_CACHE_FILE: &str = "dictionaries.bin";

/// Bumped when [`DictCache`] changes shape; a mismatch drops the file instead of
/// trying to read it.
const DICT_CACHE_VERSION: u32 = 1;

/// How often the dictionaries may be rewritten even when only a few rows were
/// added.
const DICT_SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How many new rows force a rewrite regardless of [`DICT_SAVE_INTERVAL`].
const DICT_SAVE_MIN_ROWS: usize = 8_192;

/// On-disk snapshot of the frame and stack dictionaries.
///
/// Reading a dictionary out of Parquet costs a full scan plus one copy per row
/// through the C API; deserializing the same rows is orders of magnitude
/// cheaper. Entries are `id -> contents` and never change, so the file only ever
/// grows, which is what makes it safe to load without checking it against the
/// stored data: the watermarks say how far it is complete and everything past
/// them is still fetched from Parquet.
#[derive(serde::Serialize, serde::Deserialize)]
struct DictCache {
    version: u32,
    /// Highest stack id the file is complete up to.
    stacks_upto: u64,
    /// Highest frame id the file is complete up to.
    frames_upto: u32,
    stacks: Vec<(u64, Vec<u32>)>,
    frames: Vec<(u32, Frame)>,
}

/// The counter the service map's time brush plots.
///
/// The type lives with the service map model; it is re-exported here because
/// storage code needs the SQL expression that backs each variant.
pub use lidlesseye_servicemap::NetMetric;

/// SQL expression to sum for a map metric. Constants only —never user input.
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

/// How an ingest is acknowledged.
///
/// `Async` is the production path: the upload is appended to the segment log
/// and answered immediately, and a background converter turns it into Parquet.
/// `Sync` keeps the old behaviour —the caller waits for the DuckDB commit —/// which tests, `curl` scripts and the pprof push path rely on for immediate
/// query visibility.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IngestMode {
    #[default]
    Async,
    Sync,
}

impl IngestMode {
    pub fn as_str(self) -> &'static str {
        match self {
            IngestMode::Async => "async",
            IngestMode::Sync => "sync",
        }
    }
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
    /// (`SUM(batches.bytes)`), not the file size —DuckDB does not shrink the
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
    /// Acknowledge an ingest before it reaches DuckDB (the default) or after.
    #[serde(default)]
    pub ingest_mode: IngestMode,
    /// Roll the active segment log file at this size.
    #[serde(default)]
    pub wal_segment_bytes: Option<u64>,
    /// Group-commit interval for the segment log: at most one `fsync` per
    /// window, shared by every upload that arrives inside it. This is also the
    /// maximum data loss on an unclean shutdown, and it is two orders of
    /// magnitude smaller than the old "everything in the hot tier" window.
    #[serde(default)]
    pub wal_sync_interval_ms: Option<u64>,
    /// Refuse uploads once this many bytes are waiting to be converted.
    /// Backpressure is measured in bytes, not in queue entries.
    #[serde(default)]
    pub wal_max_pending_bytes: Option<u64>,
    /// Seal the active segment once it is this old. Bounds how long a quiet
    /// tenant's last upload waits before it becomes queryable.
    #[serde(default)]
    pub wal_segment_age_ms: Option<u64>,
    /// Per-tenant cap on distinct series. Enforced by the converter, which can
    /// batch the check, rather than on the ingest path where it would cost one
    /// query per report.
    #[serde(default)]
    pub max_series: Option<usize>,
    /// Converter threads per tenant.
    ///
    /// DuckDB documents that concurrent appends never conflict, even on the
    /// same table, so converting N segments in parallel is safe; the flush
    /// (`COPY` + `DELETE`) is what must stay serialised, and it is. `None`
    /// derives a small number from the available cores.
    #[serde(default)]
    pub wal_converters: Option<usize>,
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
            ingest_mode: IngestMode::Async,
            wal_segment_bytes: None,
            wal_sync_interval_ms: None,
            wal_max_pending_bytes: None,
            wal_segment_age_ms: None,
            max_series: None,
            wal_converters: None,
        }
    }

    /// Effective number of converter threads for one tenant.
    pub fn converters(&self) -> usize {
        self.wal_converters.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| (n.get() / 2).clamp(1, 4))
                .unwrap_or(1)
        })
    }

    /// Effective segment log configuration.
    pub fn wal_options(&self) -> crate::wal::WalOptions {
        let mut opts = crate::wal::WalOptions::default();
        if let Some(bytes) = self.wal_segment_bytes {
            opts.segment_bytes = bytes.max(64 * 1024);
        }
        if let Some(ms) = self.wal_sync_interval_ms {
            opts.sync_interval = std::time::Duration::from_millis(ms);
        }
        if let Some(bytes) = self.wal_max_pending_bytes {
            opts.max_pending_bytes = bytes.max(1024 * 1024);
        }
        if let Some(ms) = self.wal_segment_age_ms {
            opts.segment_age = std::time::Duration::from_millis(ms.max(10));
        }
        opts
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

/// One report prepared for the batch transaction.
///
/// Everything that does not need the writer connection has already happened:
/// ids are allocated, frames and stacks are interned, labels are encoded. The
/// transaction then only inserts rows, which is what makes one commit for a
/// whole upload possible.
struct PreparedBatch<'a> {
    index: usize,
    report: &'a Report,
    batch_id: u64,
    labels: String,
    rows: PendingBatch,
    bytes: u64,
    total_value: u64,
    created_ns: u64,
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

/// One unit of work for the store's writer thread.
///
/// DuckDB serialises write transactions on a database anyway, so the choice was
/// never "concurrent writes or not" —only who waits. Doing it with a mutex
/// meant every caller held a thread and a request slot while it queued, and a
/// background pass could not get in at all: flushing gave up after 500 ms, so on
/// a tenant under steady ingest the hot tier never drained and grew until every
/// query scanned millions of rows. A single owning thread with one FIFO queue
/// removes the lock from the caller's path entirely: work is *ordered*, and the
/// background passes are in the same queue, so they take their turn instead of
/// starving.
enum WriteJob {
    Append {
        report: Report,
        reply: SyncSender<Result<Arc<BatchMeta>>>,
    },
    /// One upload's worth of reports, committed in a single transaction. The
    /// reply carries one result per report, in upload order.
    AppendBatch {
        reports: Vec<Report>,
        reply: SyncSender<Result<Vec<Result<Arc<BatchMeta>>>>>,
    },
    ServiceMap {
        batch: ServiceMapBatch,
        reply: SyncSender<Result<()>>,
    },
    /// Run something that needs the writer to itself (flush, prune). The
    /// closure runs on the writer thread, outside the connection mutex, and
    /// reports through the channel it captured.
    Exclusive(Box<dyn FnOnce(&Store) + Send + 'static>),
}

impl WriteJob {
    /// Short stable name, for the slow-job log line.
    fn kind(&self) -> &'static str {
        match self {
            WriteJob::Append { .. } => "append",
            WriteJob::AppendBatch { .. } => "append_batch",
            WriteJob::ServiceMap { .. } => "servicemap",
            WriteJob::Exclusive(_) => "exclusive",
        }
    }
}

/// Handle to the writer thread. Cheap to clone; `Store` holds one.
struct WriterHandle {
    tx: SyncSender<WriteJob>,
}

/// How long a caller queues behind other writes before giving up.
///
/// This is not "wait for a slow flush": every job in the queue is bounded, so
/// the wait is a queue position, and a queue that cannot drain in this window is
/// genuinely ill. The HTTP layer turns the resulting [`Error::Busy`] into a 503
/// with `Retry-After`, which an agent retries; a request that waited 30 s and
/// then timed out at the 60 s ceiling helps nobody.
const WRITE_QUEUE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a background pass queues behind other writes before giving up.
///
/// Longer than [`WRITE_QUEUE_WAIT`] on purpose, because a request and a
/// maintenance pass want opposite things from a busy writer. A caller holding
/// an HTTP connection is better served by a 503 it can retry than by waiting;
/// a flush or a prune that gives up is work that never happens, and the tier it
/// was supposed to drain keeps growing until every query pays for it.
///
/// Two independent producers —the compaction loop and the prune loop —share
/// this queue, so a pass regularly finds the other one already running. With
/// the request timeout they timed each other out forever: flush never ran,
/// the hot tier reached millions of rows, and each dashboard query scanned all
/// of them. Waiting is the correct behaviour here; nothing is lost but ticks,
/// and the loop re-arms in seconds.
const MAINTENANCE_QUEUE_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// Jobs allowed to queue before producers are made to wait.
///
/// Deep enough to absorb a burst from a dozen agents, shallow enough that the
/// memory held by queued reports stays bounded (a report is the pre-encoded
/// profile, tens of kilobytes).
const WRITE_QUEUE_DEPTH: usize = 512;

/// A write job slower than this gets a log line naming its kind.
///
/// The threshold sits above what a normal report costs (a few milliseconds) and
/// below anything a caller would notice as a stall, so the log stays quiet in
/// steady state and points straight at the job that hogged the writer.
const WRITE_JOB_SLOW: std::time::Duration = std::time::Duration::from_millis(250);

/// Segments one converter pass takes before yielding.
///
/// Small enough that a pass ends in well under a second (an upload should not
/// wait for a 250 MB transaction to commit), large enough that the per-segment
/// transaction overhead stays amortised.
const CONVERT_SEGMENTS_PER_PASS: usize = 8;

impl WriterHandle {
    /// Queue a job, waiting for room at most [`WRITE_QUEUE_WAIT`].
    fn submit(&self, mut job: WriteJob) -> Result<()> {
        let deadline = Instant::now() + WRITE_QUEUE_WAIT;
        loop {
            match self.tx.try_send(job) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(back)) => {
                    job = back;
                    if Instant::now() >= deadline {
                        return Err(Error::Busy(format!(
                            "the write queue was full for {}s; the disk cannot keep up with \
                             ingest. Retry the report",
                            WRITE_QUEUE_WAIT.as_secs()
                        )));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(Error::Db("the store's writer thread has stopped".into()));
                }
            }
        }
    }
}

/// Body of the writer thread.
///
/// Holds a `Weak`, not an `Arc`: the store owns the sender, so an `Arc` here
/// would keep the store —and this thread —alive forever.
fn writer_loop(store: Weak<Store>, jobs: Receiver<WriteJob>) {
    while let Ok(job) = jobs.recv() {
        let Some(store) = store.upgrade() else {
            break;
        };
        // Every write is serialised through this thread, so one slow job is
        // every caller's latency. Timing each one is the only way to tell a
        // big archive pass from a report that should have been microseconds —        // and, when the queue starts timing out, which job was holding it.
        let kind = job.kind();
        let started = Instant::now();
        match job {
            WriteJob::Append { report, reply } => {
                let result = run_guarded(|| store.append_now(&report));
                let _ = reply.send(result);
            }
            WriteJob::AppendBatch { reports, reply } => {
                let count = reports.len();
                let results =
                    run_guarded(|| Ok(store.append_many_now(&reports))).unwrap_or_else(|err| {
                        let message = err.to_string();
                        (0..count)
                            .map(|_| Err(Error::Db(message.clone())))
                            .collect()
                    });
                let _ = reply.send(Ok(results));
            }
            WriteJob::ServiceMap { batch, reply } => {
                let result = run_guarded(|| store.append_servicemap_now(&batch));
                let _ = reply.send(result);
            }
            WriteJob::Exclusive(task) => {
                let store = &*store;
                run_guarded(|| {
                    task(store);
                    Ok(())
                })
                .ok();
            }
        }
        let elapsed = started.elapsed();
        if elapsed >= WRITE_JOB_SLOW {
            info!(
                stage = "write_job",
                kind,
                elapsed_ms = elapsed.as_millis() as u64,
                "slow write job"
            );
        }
    }
    debug!("writer thread stopped");
}

/// Whether an error is the "the file moved under us" failure.
///
/// Compaction and the S3 archive retire published files; a statement that was
/// built a moment earlier still names them. DuckDB reports it as an IO error
/// naming the path, and the fix is to rebuild the statement, not to fail.
fn is_retired_file(err: &Error) -> bool {
    let message = err.to_string();
    message.contains("Cannot open file") || message.contains("No such file or directory")
}

/// Run a write job, turning a panic into an error for its caller.
///
/// Without this one poisoned report would take the writer thread down and every
/// later write would fail with "the writer thread has stopped", which is a much
/// worse failure than the single request that deserved it.
fn run_guarded<T>(job: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)) {
        Ok(result) => result,
        Err(_) => {
            warn!("a write job panicked; the store keeps serving");
            Err(Error::Db("the write job panicked".into()))
        }
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
    /// Set by [`Store::start_writer`]. While it is set, all writes go through
    /// that thread's queue and this mutex is only ever taken by the thread
    /// itself —callers never touch it, so a slow background pass cannot make
    /// an agent wait behind a lock it is holding.
    writer: OnceLock<WriterHandle>,
    /// Owner/wait telemetry for the writer lock, so a slow ingest or flush is
    /// attributable. Reads are not tracked here: several run at once.
    telemetry: LockTelemetry,
    /// Read connections onto the same in-memory database.
    readers: ReaderPool,
    /// `(module, symbol, file, line)` -> frame id, for the write path only.
    frame_cache: Mutex<HashMap<FrameKey, u32>>,
    /// `stack_id` -> frames, for the read path.
    ///
    /// A dashboard polls the same window over and over, and every poll used to
    /// re-read the whole stack dictionary out of Parquet: half a million rows
    /// for a week of data, at a couple of microseconds per row through the C
    /// API, which is seconds of a flamegraph. Dictionary rows are immutable
    /// once written, so they can be cached by id and never invalidated.
    stacks_by_id: parking_lot::RwLock<HashMap<u64, Arc<Vec<u32>>>>,
    /// `frame_id` -> frame. Same reasoning as [`Store::stacks_by_id`], and the
    /// frame dictionary is shared by every tenant's queries.
    frames_by_id: parking_lot::RwLock<HashMap<u32, Arc<Frame>>>,
    /// Highest `stack_id` up to which this cache is complete.
    ///
    /// Ids at or below this are either cached or genuinely absent from the
    /// dictionary, so looking for one in Parquet is pointless: that retry is
    /// what made every later query re-read the whole dictionary for a handful
    /// of ids that were never written.
    stacks_max_id: std::sync::atomic::AtomicU64,
    /// Highest `frame_id` up to which this cache is complete, for the same
    /// reason as [`Store::stacks_max_id`].
    frames_max_id: std::sync::atomic::AtomicU32,
    /// When the dictionaries were last written to disk, so a burst of flushes
    /// cannot rewrite the file on every scan.
    dict_saved_at: parking_lot::Mutex<Option<std::time::Instant>>,
    ids: Mutex<Ids>,
    /// Layout and lifecycle of the local/S3 Parquet files. Always present:
    /// canonical modes never keep a persistent DuckDB database.
    parquet: ParquetState,
    /// Append-only ingest log. The HTTP path writes here and returns; a
    /// converter thread turns closed segments into Parquet. This is what keeps
    /// a slow DuckDB commit out of an agent's request latency.
    wal: crate::wal::SegmentLog,
    /// Converter telemetry: segments and reports moved from the log into
    /// Parquet, and how long that took. Exposed per tenant on `/metrics`, which
    /// is the only way to see a converter falling behind before the backlog
    /// reaches its cap.
    converted_segments: std::sync::atomic::AtomicU64,
    converted_reports: std::sync::atomic::AtomicU64,
    convert_nanos: std::sync::atomic::AtomicU64,
    convert_failures: std::sync::atomic::AtomicU64,
    /// Only one converter pass runs at a time.
    ///
    /// The background thread and an explicit `/flush` both call
    /// `convert_pending`; without this the same segment could be decoded twice
    /// and its rows inserted twice, which double-counts a window. Taking the
    /// lock *before* listing segments is what makes the list authoritative.
    convert_lock: parking_lot::Mutex<()>,
    /// Gate between appends and flushes.
    ///
    /// A flush is `COPY` followed by `DELETE`, two statements over the same
    /// rows. Running it next to an append loses data: a row committed between
    /// the copy and the delete is removed without ever being written to
    /// Parquet. DuckDB lets appends run in parallel (they never conflict), so
    /// the gate is a read lock for appends and a write lock for flushes —    /// appends stay concurrent, flushes stay exclusive.
    flush_gate: parking_lot::RwLock<()>,
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
    /// How many times [`Store::recount`] has run. A recount reads the whole
    /// archive under the writer, so "how often" is a property worth pinning:
    /// doing it on a prune that removed nothing starved ingest into 503s.
    #[cfg(test)]
    recounts: std::sync::atomic::AtomicUsize,
    /// How many batch commits this store has run. Test-only: the point of the
    /// batch path is that an upload costs one commit, which nothing outside
    /// can observe.
    #[cfg(test)]
    commits: std::sync::atomic::AtomicUsize,
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
    /// consults it so an interrupt cannot land on an idle connection —and
    /// never on the writer, whose transaction must not be aborted.
    active: Mutex<()>,
    /// When the query currently holding this slot started, or 0 when idle.
    /// A watchdog needs it to interrupt *its own* runaway query instead of
    /// every query in the pool: the dashboard runs several at once, and killing
    /// a healthy sibling because another one ran long is what surfaced as
    /// `INTERRUPT Error: Interrupted!` in the browser.
    started_ns: AtomicU64,
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
    /// Clone `count` connections off the already-tuned writer connection.
    ///
    /// `try_clone` shares the DuckDB database instance, so global settings
    /// (`memory_limit`, `threads`, `temp_directory`, `preserve_insertion_order`)
    /// and the loaded `httpfs` extension come along. The `s3_*` credentials,
    /// however, are connection-scoped in httpfs: a cloned reader without them
    /// sends unauthenticated requests and gets 404s for every archived object.
    /// Re-running the whole `tune_connection` was redundant; re-applying just
    /// the S3 settings is not.
    fn new(conn: &Connection, count: usize, s3: Option<&S3Config>) -> Result<Self> {
        let mut slots = Vec::with_capacity(count);
        for _ in 0..count {
            let reader = conn
                .try_clone()
                .map_err(|err| Error::Db(format!("opening a read connection: {err}")))?;
            // Read statements embed the Parquet file list, so their text changes
            // whenever a flush lands. Caching the prepared plan still pays off:
            // parsing and planning `read_parquet([...])` over a few hundred
            // paths is most of a read's fixed cost, and a dashboard that polls
            // the same window keeps re-planning byte-identical SQL. The cache
            // lives on the connection, so each pooled reader needs its own.
            reader.set_prepared_statement_cache_capacity(PREPARE_CACHE);
            if let Some(s3) = s3 {
                parquet::configure_s3(&reader, s3)?;
            }
            let interrupt = reader.interrupt_handle();
            slots.push(ReaderSlot {
                conn: Mutex::new(reader),
                active: Mutex::new(()),
                started_ns: AtomicU64::new(0),
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

    /// Interrupt only reads that have been running longer than `age_ns`.
    ///
    /// A watchdog belongs to one query, but an `InterruptHandle` addresses a
    /// connection, and interrupting the pool indiscriminately cancelled healthy
    /// queries that merely happened to run at the same time —the dashboard
    /// loads the map, the histogram and the flamegraph together, so one slow
    /// panel used to break the other two. Since a query older than the timeout
    /// is one the request layer is about to abandon anyway, this is the oldest
    /// query that can still be usefully stopped and never a younger one.
    fn interrupt_older_than(&self, age_ns: u64) {
        let now = now_ns();
        for slot in &self.slots {
            // Held `active` means a query owns this slot.
            if slot.active.try_lock().is_some() {
                continue;
            }
            let started = slot.started_ns.load(Ordering::Acquire);
            if started != 0 && now.saturating_sub(started) >= age_ns {
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
    /// When the slot was handed out. A store call holds this for exactly the
    /// life of its queries, so the hold time is the query time —the number a
    /// slow dashboard is actually made of.
    started: Instant,
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
        // Mark idle *before* the guards drop, so a watchdog that sees the slot
        // as still busy reads a timestamp that is about to be irrelevant rather
        // than one from the query that starts next.
        self.pool.slots[self.idx]
            .started_ns
            .store(0, Ordering::Release);
        // Return the slot after the guards above are gone. The pool hands the
        // id out again only to a caller that immediately blocks on the slot's
        // own mutex, so this cannot race with the connection still in use.
        self.pool.release(self.idx);
        let elapsed = self.started.elapsed();
        if elapsed >= READ_SLOW {
            debug!(
                stage = "read_conn_held",
                slot = self.idx,
                elapsed_ms = elapsed.as_millis() as u64,
                "slow read"
            );
        }
    }
}

/// A store read slower than this is worth a log line.
///
/// Every dashboard panel is one or two of these, so a `read_conn_held` line
/// names the exact store call behind a slow page —without it the only visible
/// number is the whole request, which cannot tell a slow query from five fast
/// ones and a slow S3 round trip.
const READ_SLOW: std::time::Duration = std::time::Duration::from_millis(250);

/// How long a read waits for the connection before reporting the store busy.
///
/// The connection is single and a wide scan or an archive merge holds it, so
/// "wait forever" is not patience —it is an unbounded queue that eventually
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

/// How long the one-off catalogue recount waits for the writer.
///
/// A recount reads the whole archive, so this is not a case for patience: if
/// the writer is busy with a flush, answer this poll from a read snapshot
/// instead and let the background warm install the exact number.
const AGG_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

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
        slot.started_ns.store(now_ns(), Ordering::Release);
        Ok(ReadConn {
            _active: active,
            conn,
            pool: &self.readers,
            idx,
            started: Instant::now(),
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
    /// agents retried —the "report queue full" storm. A bounded wait turns
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

    /// Canonical names are VIEWs (hot ?local ?S3), so writes always target
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

        // `open_in_memory_with_flags` —not `open_in_memory` —because the
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
        // connections, so queries no longer serialise on the writer. The
        // settings above are database-wide and come along with the clone.
        let readers = ReaderPool::new(&conn, cfg.read_connections(), cfg.s3.as_ref())?;
        info!(
            stage = "connections",
            readers = cfg.read_connections(),
            elapsed_ms = extras.elapsed().as_millis(),
            "auxiliary connections ready"
        );

        info!(stage = "id_counters", "loading store id counters");
        let ids_started = std::time::Instant::now();
        let ids = {
            // Ids are monotonic and archived by hour, so the newest shard plus
            // the hot tier already contains the maximum. Mounting every file
            // in a large archive for three `MAX` lookups used to cost tens of
            // seconds on the first request to a cold tenant.
            let frames_span = parquet::table_span_newest(&parquet_state, "frames");
            let stacks_span = parquet::table_span_newest(&parquet_state, "stacks");
            let batches_span = parquet::table_span_newest(&parquet_state, "batches");
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

        let wal = crate::wal::SegmentLog::open(cfg.root.join("wal"), cfg.wal_options())?;
        let store = Store {
            cfg,
            wal,
            converted_segments: std::sync::atomic::AtomicU64::new(0),
            converted_reports: std::sync::atomic::AtomicU64::new(0),
            convert_nanos: std::sync::atomic::AtomicU64::new(0),
            convert_failures: std::sync::atomic::AtomicU64::new(0),
            convert_lock: parking_lot::Mutex::new(()),
            flush_gate: parking_lot::RwLock::new(()),
            conn: Mutex::new(conn),
            writer: OnceLock::new(),
            telemetry: LockTelemetry::default(),
            readers,
            frame_cache: Mutex::new(HashMap::new()),
            stacks_by_id: parking_lot::RwLock::new(HashMap::new()),
            frames_by_id: parking_lot::RwLock::new(HashMap::new()),
            stacks_max_id: std::sync::atomic::AtomicU64::new(0),
            frames_max_id: std::sync::atomic::AtomicU32::new(0),
            dict_saved_at: parking_lot::Mutex::new(None),
            ids: Mutex::new(ids),
            parquet: parquet_state,
            maintenance_conn: Mutex::new(maintenance_conn),
            last_compact_ns: std::sync::atomic::AtomicU64::new(0),
            last_flush_ns: std::sync::atomic::AtomicU64::new(now_ns()),
            agg: Mutex::new(None),
            series_cache: Mutex::new(None),
            #[cfg(test)]
            recounts: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            commits: std::sync::atomic::AtomicUsize::new(0),
        };
        info!(
            root = %store.cfg.root.display(),
            elapsed_ms = started.elapsed().as_millis(),
            "store ready"
        );
        store.restore_dict_cache();
        Ok(store)
    }

    /// Open a store and start its writer thread.
    ///
    /// This is the form every long-lived process uses. [`Store::open`] alone
    /// leaves writes on the direct, mutex-guarded path, which is what the unit
    /// tests want —they assert on a single-threaded timeline and do not need a
    /// second thread's worth of scheduling noise.
    pub fn open_shared(cfg: StoreConfig) -> Result<Arc<Self>> {
        let store = Arc::new(Self::open(cfg)?);
        store.start_writer();
        // Anything the previous process left in the log is recovered *before*
        // the store is handed out. Without this, the first request after a
        // crash opens the tenant, races the converter and answers with the
        // empty dataset it saw a moment earlier —which reads as data loss and
        // is the one thing the log exists to prevent.
        let (bytes, segments) = store.wal_stats();
        if segments > 0 {
            info!(
                root = %store.cfg.root.display(),
                segments,
                bytes,
                "converting the ingest log left by the previous process"
            );
            if let Err(err) = store.convert_pending() {
                warn!(error = %err, "could not recover the ingest log; the converter will retry");
            }
        }
        store.start_converter();
        Ok(store)
    }

    /// Hand all writes to one dedicated thread.
    ///
    /// Idempotent and safe to call once per store. After this returns, `append`,
    /// `append_servicemap`, `flush` and `prune` queue instead of locking, and
    /// the writer connection is only ever touched by the thread.
    pub fn start_writer(self: &Arc<Self>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(WRITE_QUEUE_DEPTH);
        let weak = Arc::downgrade(self);
        let handle = WriterHandle { tx };
        if self.writer.set(handle).is_err() {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name(format!("store-writer-{}", self.cfg.root.display()))
            .spawn(move || writer_loop(weak, rx));
        if let Err(err) = spawned {
            // A store without its thread is still correct, just slower under
            // contention: writes fall back to the mutex path.
            warn!(error = %err, "could not start the store writer thread");
        }
    }

    /// Queue a job on the writer thread and wait for its answer.
    fn queue<T: Send + 'static>(&self, job: WriteJob, answer: Receiver<Result<T>>) -> Result<T> {
        let Some(writer) = self.writer.get() else {
            return Err(Error::Db("this store has no writer thread".into()));
        };
        writer.submit(job)?;
        match answer.recv_timeout(WRITE_QUEUE_WAIT) {
            Ok(result) => result,
            Err(_) => Err(Error::Busy(format!(
                "a write job did not finish within {}s; retry",
                WRITE_QUEUE_WAIT.as_secs()
            ))),
        }
    }

    /// Run `task` on the writer thread, with the writer to itself.
    ///
    /// Background passes use this instead of trying to take a lock with a
    /// deadline: as a queued job they are guaranteed a turn, and as short jobs
    /// they do not hold ingest up long enough to matter. That combination is
    /// what replaced a flush that gave up after 500 ms and therefore never ran.
    fn run_exclusive<T, F>(&self, task: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Store) -> Result<T> + Send + 'static,
    {
        let Some(writer) = self.writer.get() else {
            return task(self);
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        writer.submit(WriteJob::Exclusive(Box::new(move |store| {
            let _ = tx.send(task(store));
        })))?;
        match rx.recv_timeout(MAINTENANCE_QUEUE_WAIT) {
            Ok(result) => result,
            Err(_) => Err(Error::Busy(format!(
                "a background pass did not finish within {}s; retry",
                MAINTENANCE_QUEUE_WAIT.as_secs()
            ))),
        }
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

    /// Interrupt reads that have been running for at least `age`.
    ///
    /// This is what a per-request watchdog should call. [`Store::interrupt`]
    /// stops every query in the pool, which turns one slow request into failures
    /// for whatever else the page was loading at the time.
    pub fn interrupt_older_than(&self, age: std::time::Duration) {
        let age_ns = age.as_nanos().min(u128::from(u64::MAX)) as u64;
        self.readers.interrupt_older_than(age_ns);
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

    /// Re-scan the Parquet tree so a read-only replica picks up what the writer
    /// has published since it opened. A writer does not need this: every flush
    /// refreshes its own index.
    pub fn refresh_parquet_index(&self) {
        self.parquet.refresh_local_index();
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
        let started = std::time::Instant::now();
        let conn = self.read_conn()?;
        let span_started = std::time::Instant::now();
        let span = match window {
            Some((_, from_ns, until_ns)) => self.span("batches", from_ns, until_ns),
            None => self.span_all("batches"),
        };
        let span_ms = span_started.elapsed().as_millis() as u64;
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

        let prepare_started = std::time::Instant::now();
        let mut stmt = conn.prepare_cached(&sql).map_err(db_err)?;
        let prepare_ms = prepare_started.elapsed().as_millis() as u64;
        let query_started = std::time::Instant::now();
        let refs: Vec<&dyn duckdb::ToSql> = args.iter().map(|a| a.as_ref()).collect();
        let mut rows = stmt.query(refs.as_slice()).map_err(db_err)?;
        let query_ms = query_started.elapsed().as_millis() as u64;
        let rows_started = std::time::Instant::now();
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
        let elapsed = started.elapsed();
        if elapsed.as_millis() >= 300 {
            info!(
                stage = "query_batches",
                span_ms,
                prepare_ms,
                query_ms,
                rows_ms = rows_started.elapsed().as_millis() as u64,
                rows = out.len(),
                elapsed_ms = elapsed.as_millis() as u64,
                "slow batch read"
            );
        }
        Ok(out)
    }

    /// Persist a report as one batch.
    ///
    /// When the store has a writer thread this queues the report and waits for
    /// that thread to commit it —no lock on the caller's path, and the wait is
    /// a queue position rather than "however long a background pass holds the
    /// connection". Validation happens here so an invalid report is rejected
    /// before it occupies a queue slot.
    pub fn append(&self, report: &Report) -> Result<Arc<BatchMeta>> {
        report.validate()?;
        if self.writer.get().is_none() {
            return self.append_now(report);
        }
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        self.queue(
            WriteJob::Append {
                report: report.clone(),
                reply,
            },
            answer,
        )
    }

    /// [`Store::append`] taking ownership, so the queued job does not clone a
    /// profile that the caller is done with. Ingest uses this.
    pub fn append_owned(&self, report: Report) -> Result<Arc<BatchMeta>> {
        report.validate()?;
        if self.writer.get().is_none() {
            return self.append_now(&report);
        }
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        self.queue(WriteJob::Append { report, reply }, answer)
    }

    /// Persist an upload's reports in a single transaction.
    ///
    /// This is the ingest path: one HTTP request, one commit, one queued job.
    /// Results come back in upload order, so a report that fails validation or
    /// cannot be prepared costs only itself.
    pub fn append_owned_batch(&self, reports: Vec<Report>) -> Vec<Result<Arc<BatchMeta>>> {
        if reports.is_empty() {
            return Vec::new();
        }
        if self.writer.get().is_none() {
            return self.append_many_now(&reports);
        }
        let count = reports.len();
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        match self.queue(WriteJob::AppendBatch { reports, reply }, answer) {
            Ok(results) => results,
            Err(err) => {
                let message = err.to_string();
                (0..count)
                    .map(|_| Err(Error::Db(message.clone())))
                    .collect()
            }
        }
    }

    /// Append an upload to the segment log and return immediately.
    ///
    /// This is the ingest path in [`IngestMode::Async`]. The cost is one
    /// encoded append plus, at most once per `sync_interval`, one `fsync`
    /// shared with every other upload in that window. Nothing here waits for
    /// DuckDB, so a flush, a merge or a slow disk can no longer turn an agent's
    /// upload into a thirty-second timeout.
    pub fn append_durable(&self, reports: &[Report]) -> Result<crate::wal::Appended> {
        self.wal.append(reports)
    }

    /// Wait until an append is on stable storage.
    ///
    /// Only `?sync=1` callers pay this; the default answer is "written", which
    /// is what the 202 means.
    pub fn await_durable(
        &self,
        appended: &crate::wal::Appended,
        timeout: std::time::Duration,
    ) -> bool {
        self.wal
            .await_durable(appended.segment, appended.offset, timeout)
    }

    /// Pending segment-log bytes and how many closed segments are waiting.
    pub fn wal_stats(&self) -> (u64, usize) {
        (self.wal.pending_bytes(), self.wal.closed_segments().len())
    }

    /// Converter counters: `(segments, reports, total_nanos, failures)`.
    pub fn convert_stats(&self) -> (u64, u64, u64, u64) {
        (
            self.converted_segments.load(Ordering::Relaxed),
            self.converted_reports.load(Ordering::Relaxed),
            self.convert_nanos.load(Ordering::Relaxed),
            self.convert_failures.load(Ordering::Relaxed),
        )
    }

    /// How many series in `reports` the store has never seen.
    ///
    /// Deduplicated by key first, so a batch of two hundred reports over ten
    /// series costs ten lookups rather than two hundred.
    fn new_series_count(&self, reports: &[Report]) -> usize {
        let mut seen: Vec<(String, String, String)> = Vec::new();
        let mut fresh = 0usize;
        for report in reports {
            let key = (
                report.service_name.clone(),
                report.profile_type.as_str().to_string(),
                encode_labels(&report.all_labels()),
            );
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            if !self.contains_series(report) {
                fresh += 1;
            }
        }
        fresh
    }

    /// Convert every closed segment into Parquet and retire it.
    ///
    /// Single-threaded entry point used by `/flush` and by tests. The
    /// converters call [`Store::convert_ready_on`] with their own connection,
    /// and segment claiming keeps the two from ever picking the same file.
    pub fn convert_pending(&self) -> Result<crate::wal::ConvertStats> {
        let conn = self.lock_conn(ConnUser::Flush);
        self.convert_ready_on(&conn, usize::MAX)
    }

    /// Convert up to `max_segments` claimed segments on `conn`.
    ///
    /// Rows are appended first (one transaction per segment, so a bad report
    /// costs only itself), then the hot tier is flushed before the segment is
    /// retired: a segment may only disappear once its rows are a Parquet file.
    /// If the flush fails the segment is parked in the `.flush` state and the
    /// next pass retries only the flush —re-appending would double-count.
    fn convert_ready_on(
        &self,
        conn: &Connection,
        max_segments: usize,
    ) -> Result<crate::wal::ConvertStats> {
        let mut stats = crate::wal::ConvertStats::default();
        let started = Instant::now();

        // A parked flush means rows are already in the hot tier. Flushing now
        // retires every parked segment, so they are handled before new claims.
        let parked = self.wal.pending_flush();
        if !parked.is_empty() {
            match self.flush_all_hot_on(conn) {
                Ok(rows) => {
                    stats.flushed_rows += rows;
                    for seq in &parked {
                        self.wal.finish_flush(*seq);
                    }
                    stats.segments += parked.len();
                }
                Err(err) => {
                    self.convert_failures.fetch_add(1, Ordering::Relaxed);
                    return Err(err);
                }
            }
        }

        for _ in 0..max_segments {
            let Some(seq) = self.wal.claim_next() else {
                break;
            };
            let reports = match self.wal.read_claimed(seq) {
                Ok(Some(reports)) => reports,
                Ok(None) => continue,
                Err(err) => {
                    // Unreadable: leave it out of the queue so it cannot block
                    // everything behind it, and say so once.
                    warn!(segment = seq, error = %err, "skipping an unreadable segment");
                    self.wal.finish_claim(seq);
                    continue;
                }
            };
            let count = reports.len();
            if let Some(max) = self.cfg.max_series
                && self
                    .series_count()
                    .saturating_add(self.new_series_count(&reports))
                    > max
            {
                warn!(
                    tenant = %self.cfg.root.display(),
                    series = max,
                    reports = count,
                    "rejecting a converted batch: the tenant is at its series limit"
                );
                self.wal.finish_claim(seq);
                stats.segments += 1;
                stats.reports += count;
                stats.rejected += count;
                continue;
            }

            let results = self.append_many_on(conn, &reports);
            let rejected = results.iter().filter(|result| result.is_err()).count();
            if count > 0 && rejected == count {
                // The insert itself failed (not a per-report rejection): put the
                // segment back and try again on the next pass.
                self.convert_failures.fetch_add(1, Ordering::Relaxed);
                self.wal.release_claim(seq);
                continue;
            }
            match self.flush_all_hot_on(conn) {
                Ok(rows) => {
                    self.wal.finish_claim(seq);
                    stats.segments += 1;
                    stats.reports += count;
                    stats.rejected += rejected;
                    stats.flushed_rows += rows;
                }
                Err(err) => {
                    self.wal.mark_flushing(seq);
                    self.convert_failures.fetch_add(1, Ordering::Relaxed);
                    warn!(segment = seq, error = %err, "flush failed; the segment is parked");
                    return Err(err);
                }
            }
        }

        if stats.segments > 0 {
            self.converted_segments
                .fetch_add(stats.segments as u64, Ordering::Relaxed);
            self.converted_reports
                .fetch_add(stats.reports as u64, Ordering::Relaxed);
            self.convert_nanos
                .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
            debug!(
                stage = "wal_convert",
                segments = stats.segments,
                reports = stats.reports,
                rejected = stats.rejected,
                rows = stats.flushed_rows,
                "converted segments"
            );
        }
        Ok(stats)
    }

    /// Move the whole hot tier to Parquet, however many slices that takes.
    ///
    /// The converter needs the rows of a segment on disk before it may delete
    /// the segment, so unlike the cadence-driven flush this must actually
    /// finish rather than move one bounded slice and return.
    fn flush_all_hot(&self) -> Result<usize> {
        const MAX_SLICES: usize = 64;
        let conn = self.lock_conn(ConnUser::Flush);
        let mut total = 0usize;
        for _ in 0..MAX_SLICES {
            let moved = self.flush_chunk_on(&conn)?;
            total += moved;
            if moved < parquet::FLUSH_CHUNK_ROWS {
                break;
            }
        }
        Ok(total)
    }

    /// [`Store::flush_all_hot`] on a connection the caller owns.
    fn flush_all_hot_on(&self, conn: &Connection) -> Result<usize> {
        const MAX_SLICES: usize = 64;
        let mut total = 0usize;
        for _ in 0..MAX_SLICES {
            let moved = self.flush_chunk_on(conn)?;
            total += moved;
            if moved < parquet::FLUSH_CHUNK_ROWS {
                break;
            }
        }
        Ok(total)
    }

    /// Start the segment converter.
    ///
    /// One thread per store: decoding is CPU work that must not sit on the
    /// writer, and the insert it hands over is already batched into a single
    /// transaction, so a second decoder would only contend for the same writer.
    pub fn start_converter(self: &Arc<Self>) {
        // Durability runs on its own thread, like a database's WAL writer.
        // Doing the `fsync` inside `append` meant the unluckiest upload of each
        // interval paid for flushing every other upload's bytes —measured at
        // 873 ms p50 under a 32-client burst.
        let sync_interval = self.cfg.wal_options().sync_interval;
        if !sync_interval.is_zero() {
            let weak = Arc::downgrade(self);
            let spawned = std::thread::Builder::new()
                .name(format!("store-walsync-{}", self.cfg.root.display()))
                .spawn(move || {
                    let tick = (sync_interval / 2).max(std::time::Duration::from_millis(5));
                    loop {
                        let Some(store) = weak.upgrade() else {
                            return;
                        };
                        if let Err(err) = store.wal.sync_active() {
                            warn!(error = %err, "fsync of the ingest log failed");
                        }
                        drop(store);
                        std::thread::sleep(tick);
                    }
                });
            if let Err(err) = spawned {
                warn!(error = %err, "could not start the ingest log sync thread");
            }
        }

        // One converter per worker. Each gets its own cloned connection: DuckDB
        // appends do not conflict, so the only shared state between them is the
        // segment directory, where claiming is a rename and therefore atomic.
        let converters = self.cfg.converters();
        let clones = {
            let conn = self.conn.lock();
            let mut clones = Vec::with_capacity(converters);
            for _ in 0..converters {
                match conn.try_clone() {
                    Ok(clone) => {
                        if let Some(s3) = self.cfg.s3.as_ref()
                            && let Err(err) = parquet::configure_s3(&clone, s3)
                        {
                            warn!(error = %err, "configuring a converter connection");
                        }
                        clones.push(clone);
                    }
                    Err(err) => {
                        warn!(error = %err, "cloning a converter connection");
                        break;
                    }
                }
            }
            clones
        };
        if clones.is_empty() {
            warn!("no converter connection could be opened; the ingest log will not drain");
            return;
        }
        for (index, conn) in clones.into_iter().enumerate() {
            let weak = Arc::downgrade(self);
            let spawned = std::thread::Builder::new()
                .name(format!("store-convert{index}-{}", self.cfg.root.display()))
                .spawn(move || {
                    loop {
                        let Some(store) = weak.upgrade() else {
                            return;
                        };
                        // A trickle of uploads must not sit unqueryable just
                        // because the segment never reached its size limit.
                        store.wal.seal_if_due();
                        let mut did_work = false;
                        match store.convert_ready_on(&conn, CONVERT_SEGMENTS_PER_PASS) {
                            Ok(stats) if stats.segments > 0 => did_work = true,
                            Ok(_) => {}
                            Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
                        }
                        if did_work {
                            continue;
                        }
                        drop(store);
                        if let Some(store) = weak.upgrade() {
                            store
                                .wal
                                .wait_for_segment(std::time::Duration::from_millis(200));
                        } else {
                            return;
                        }
                    }
                });
            if let Err(err) = spawned {
                warn!(error = %err, "could not start a segment converter");
            }
        }
    }

    /// Commit one batch on whichever thread called this.
    fn append_now(&self, report: &Report) -> Result<Arc<BatchMeta>> {
        match self.append_many_now(std::slice::from_ref(report)).pop() {
            Some(result) => result,
            None => Err(Error::Db("the writer returned no result".into())),
        }
    }

    /// Commit several reports in one transaction, on the calling thread.
    ///
    /// Each report used to get its own job and its own `BEGIN`/`COMMIT`. An
    /// agent uploads a few hundred reports per flush, so the writer spent its
    /// time on commits and the queue never drained; ingest then timed out
    /// against a writer that was busy doing bookkeeping. Preparing every report
    /// first —which touches only in-process state —also means one bad report
    /// costs only itself instead of the whole commit.
    fn append_many_now(&self, reports: &[Report]) -> Vec<Result<Arc<BatchMeta>>> {
        match self.write_conn() {
            Ok(conn) => self.append_many_on(&conn, reports),
            Err(err) => {
                let message = err.to_string();
                (0..reports.len())
                    .map(|_| Err(Error::Db(message.clone())))
                    .collect()
            }
        }
    }

    /// [`Store::append_many_now`] on a connection the caller owns.
    ///
    /// Converters each hold a cloned connection: DuckDB documents that
    /// concurrent appends never conflict, even on the same table, so the
    /// conversion of N segments can proceed in parallel instead of queueing on
    /// one writer.
    fn append_many_on(&self, conn: &Connection, reports: &[Report]) -> Vec<Result<Arc<BatchMeta>>> {
        // Held for the whole transaction: a flush that copies and deletes the
        // same rows must not interleave with this insert. Other appends only
        // take the read side, so they still run in parallel.
        let _no_flush_in_flight = self.flush_gate.read();
        let mut out: Vec<Option<Result<Arc<BatchMeta>>>> =
            (0..reports.len()).map(|_| None).collect();
        let mut prepared: Vec<PreparedBatch<'_>> = Vec::with_capacity(reports.len());

        for (index, report) in reports.iter().enumerate() {
            // Validate here rather than trusting the caller: `append_owned`
            // always did, and a batch is only safe to share a transaction if
            // one bad report can still be refused on its own.
            if let Err(err) = report.validate() {
                out[index] = Some(Err(err));
                continue;
            }
            let labels = encode_labels(&report.all_labels());
            let batch_id = {
                let mut ids = self.ids.lock();
                let id = ids.next_batch;
                ids.next_batch = ids.next_batch.saturating_add(1);
                id
            };
            // Interning frames and stacks before the transaction means a bad
            // report is rejected on its own, without a half-written batch.
            match self.prepare_rows(report) {
                Ok(rows) => {
                    let bytes = rows.samples.len() as u64 * 24
                        + rows
                            .stacks
                            .iter()
                            .map(|(_, frames)| frames.len() as u64 * 4)
                            .sum::<u64>()
                        + rows.frames.len() as u64 * 48;
                    prepared.push(PreparedBatch {
                        index,
                        report,
                        batch_id,
                        labels,
                        rows,
                        bytes,
                        total_value: report.total_value(),
                        created_ns: now_ns(),
                    });
                }
                Err(err) => out[index] = Some(Err(err)),
            }
        }

        if !prepared.is_empty() {
            #[cfg(test)]
            self.commits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let frames_t = self.wtable("frames");
            let stacks_t = self.wtable("stacks");
            let samples_t = self.wtable("samples");
            let batches_t = self.wtable("batches");
            let written = (|| -> Result<Vec<Arc<BatchMeta>>> {
                in_transaction(conn, "COMMIT", |conn| -> Result<()> {
                    // `Appender` instead of prepared statements: the same rows
                    // go through the engine's bulk path. Measured on this
                    // build, a prepared `execute` costs ~0.9 ms per row and
                    // `append_row` ~0.002 ms, which was the difference between
                    // a 300-report upload taking 4.4 s and a few milliseconds.
                    {
                        let mut appender = conn.appender(&frames_t).map_err(db_err)?;
                        for batch in &prepared {
                            for (id, frame) in &batch.rows.frames {
                                appender
                                    .append_row(params![
                                        *id as i32,
                                        frame.module.as_str(),
                                        frame.symbol.as_str(),
                                        frame.file.as_str(),
                                        frame.line as i32
                                    ])
                                    .map_err(db_err)?;
                            }
                        }
                        appender.flush().map_err(db_err)?;
                    }
                    {
                        let mut appender = conn.appender(&stacks_t).map_err(db_err)?;
                        for batch in &prepared {
                            for (id, frames) in &batch.rows.stacks {
                                appender
                                    .append_row(params![*id as i64, pack_frames(frames)])
                                    .map_err(db_err)?;
                            }
                        }
                        appender.flush().map_err(db_err)?;
                    }
                    {
                        let mut appender = conn.appender(&samples_t).map_err(db_err)?;
                        for batch in &prepared {
                            for (stack_id, value) in &batch.rows.samples {
                                appender
                                    .append_row(params![
                                        batch.batch_id as i64,
                                        batch.report.profile_type.as_str(),
                                        batch.report.service_name.as_str(),
                                        batch.labels.as_str(),
                                        ts(batch.report.start_ns),
                                        ts(batch.report.end_ns),
                                        *stack_id as i64,
                                        *value as i64
                                    ])
                                    .map_err(db_err)?;
                            }
                        }
                        appender.flush().map_err(db_err)?;
                    }
                    {
                        let mut appender = conn.appender(&batches_t).map_err(db_err)?;
                        for batch in &prepared {
                            appender
                                .append_row(params![
                                    batch.batch_id as i64,
                                    batch.report.profile_type.as_str(),
                                    batch.report.service_name.as_str(),
                                    batch.labels.as_str(),
                                    ts(batch.report.start_ns),
                                    ts(batch.report.end_ns),
                                    batch.total_value as i64,
                                    batch.rows.samples.len() as i64,
                                    batch.bytes as i64,
                                    ts(batch.created_ns)
                                ])
                                .map_err(db_err)?;
                        }
                        appender.flush().map_err(db_err)?;
                    }
                    Ok(())
                })?;
                // Fold every batch into the catalogue aggregate before releasing
                // the writer. A caller that appends and then asks for stats must
                // see its own write, and a recount can only run while it owns
                // this connection.
                if let Some(agg) = self.agg.lock().as_mut() {
                    for batch in &prepared {
                        agg.add(
                            batch.total_value,
                            batch.bytes,
                            batch.report.start_ns,
                            batch.report.end_ns,
                        );
                    }
                }
                Ok(prepared
                    .iter()
                    .map(|batch| {
                        Arc::new(BatchMeta {
                            batch_id: batch.batch_id,
                            profile_type: batch.report.profile_type.clone(),
                            service_name: batch.report.service_name.clone(),
                            labels: batch.report.all_labels(),
                            start_ns: batch.report.start_ns,
                            end_ns: batch.report.end_ns,
                            total_value: batch.total_value,
                            sample_count: batch.rows.samples.len() as u32,
                            bytes: batch.bytes,
                            created_ns: batch.created_ns,
                        })
                    })
                    .collect())
            })();

            match written {
                Ok(metas) => {
                    for (batch, meta) in prepared.iter().zip(metas) {
                        debug!(
                            batch = batch.batch_id,
                            stacks = batch.rows.samples.len(),
                            frames = batch.rows.frames.len(),
                            "wrote batch"
                        );
                        out[batch.index] = Some(Ok(meta));
                    }
                }
                Err(err) => {
                    // One transaction, one outcome: a commit that failed wrote
                    // nothing, so every report in it failed.
                    let message = format!("the batch was not committed: {err}");
                    for batch in &prepared {
                        out[batch.index] = Some(Err(Error::Db(message.clone())));
                    }
                }
            }
        }

        out.into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| Err(Error::Db("the writer returned no result".into())))
            })
            .collect()
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
        // A query builds its SQL from the in-memory file index and only then
        // opens the paths it named. A merge or an upload can retire one of them
        // in between —the file is gone, the index that named it was correct a
        // millisecond earlier, and the panel fails with "Cannot open file".
        // Rebuilding the statement reads a fresh index and names the file that
        // superseded it, so one retry turns a visible 500 into a slower answer.
        match self.scan_once(query) {
            Err(err) if is_retired_file(&err) => {
                debug!(error = %err, "a Parquet file was retired mid-query; retrying");
                self.scan_once(query)
            }
            other => other,
        }
    }

    fn scan_once(&self, query: &ScanQuery<'_>) -> Result<ScanResult> {
        let conn = self.read_conn()?;
        let started = Instant::now();
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

        let prepare_started = std::time::Instant::now();
        let mut stmt = conn.prepare_cached(&sql).map_err(db_err)?;
        let prepare_ms = prepare_started.elapsed().as_millis() as u64;
        let query_started = std::time::Instant::now();
        let refs: Vec<&dyn duckdb::ToSql> = args.iter().map(|a| a.as_ref()).collect();
        let mut raw = stmt.query(refs.as_slice()).map_err(db_err)?;
        let query_ms = query_started.elapsed().as_millis() as u64;
        let mut rows: Vec<SampleRow> = Vec::new();
        let mut truncated = false;
        // A wide scan returns one row per (stack, series) —over a million
        // rows in practice —but those rows share only a handful of distinct
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
        let samples_done = started.elapsed();

        let stack_ids: Vec<u64> = rows.iter().map(|r| r.stack_id).collect();
        let stacks = self.load_stacks(&conn, &stack_ids, query.from_ns, query.until_ns)?;
        let stacks_done = started.elapsed();
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
        let elapsed = started.elapsed();
        // One line per slow scan, with the split that decides what to fix next:
        // a scan that is slow in `samples_ms` needs less data read, one that is
        // slow in `stacks_ms` needs a better dictionary layout. Silent below
        // the threshold, because a healthy scan is a few milliseconds and this
        // runs on every dashboard poll.
        if elapsed >= std::time::Duration::from_secs(1) {
            info!(
                stage = "scan",
                prepare_ms,
                query_ms,
                samples_ms = samples_done.as_millis() as u64,
                stacks_ms = (stacks_done - samples_done).as_millis() as u64,
                frames_ms = (elapsed - stacks_done).as_millis() as u64,
                total_ms = elapsed.as_millis() as u64,
                rows = rows.len(),
                "slow scan"
            );
        }

        Ok(ScanResult {
            rows,
            stacks,
            frames,
            truncated,
        })
    }

    /// Load stack frames for the ids a scan produced.
    ///
    /// The dictionary is mounted for the query's own hours, not for the whole
    /// retention window. A stack row is written into the hour of the samples
    /// that name it ([`parquet::flush_by_sample_hours`]), so the sample span
    /// the scan just read is also the only set of files that can hold these
    /// ids. Mounting the entire dictionary made a one-hour query open every
    /// stacks file on disk —325 files and ~2.5 s of a ~2.8 s scan —and the
    /// cost grew with retention instead of with the window.
    fn load_stacks(
        &self,
        conn: &Connection,
        ids: &[u64],
        from_ns: u64,
        until_ns: u64,
    ) -> Result<HashMap<u64, Vec<u32>>> {
        let wanted: HashSet<u64> = ids.iter().copied().collect();
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let mut out = self.load_stacks_from(conn, &self.span("stacks", from_ns, until_ns), ids)?;
        if out.len() < wanted.len() {
            // A row can legitimately sit outside the window: files written
            // before dictionaries were sharded live in `hour=0`, and a stack
            // written after the warm pass may not be in the current window yet.
            // Only ids above the warm watermark can still be found this way;
            // anything at or below it was already in the snapshot the cache
            // holds, so retrying is a full dictionary read that cannot succeed
            // —which is what kept every later query at the old cost.
            let watermark = self.stacks_max_id.load(Ordering::Acquire);
            let missing: Vec<u64> = wanted
                .iter()
                .copied()
                .filter(|id| !out.contains_key(id) && *id > watermark)
                .collect();
            if !missing.is_empty() {
                out.extend(self.load_stacks_from(conn, &self.span_all("stacks"), &missing)?);
            }
        }
        Ok(out)
    }

    /// One bulk read of the stacks table behind `span`, keeping only `ids`.
    ///
    /// This used to load the ids into a temp table and join. The join looked
    /// cheap and measured expensive: a six-hour flamegraph spent five seconds
    /// in this one statement for sixty thousand ids while the same window's
    /// samples scan (an order of magnitude more rows) took half a second. The
    /// dictionary is small —a few megabytes for a week —so reading it whole
    /// and filtering in Rust beats paying DuckDB's staging-table machinery on
    /// every query.
    fn load_stacks_from(
        &self,
        conn: &Connection,
        span: &str,
        ids: &[u64],
    ) -> Result<HashMap<u64, Vec<u32>>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        // The dictionary is read on demand and cached by id. Warming it up
        // front held a reader connection for the length of a full dictionary
        // read —a minute on a real tenant, which timed out the first query
        // after a restart —so the cache fills from the scans that need it.
        let started = std::time::Instant::now();
        let mut out = HashMap::with_capacity(ids.len());
        let mut seen = 0u64;
        let mut missing: Vec<u64> = Vec::with_capacity(ids.len());
        {
            let cache = self.stacks_by_id.read();
            for &id in ids {
                match cache.get(&id) {
                    Some(frames) => {
                        out.insert(id, (**frames).clone());
                    }
                    None => missing.push(id),
                }
            }
        }
        if missing.is_empty() {
            return Ok(out);
        }
        // Filter in Rust, not in SQL. An `IN` list with tens of thousands of
        // ids makes DuckDB build an enormous expression and plan around it,
        // which measured twenty seconds of a thirty-second query; and the
        // per-row extraction through the C API is what actually costs, at a
        // couple of microseconds a row. Read the dictionary and keep only the
        // ids this scan asked about.
        let wanted: HashSet<u64> = missing.iter().copied().collect();
        // `stacks_max_id` marks how far the cache is *complete*: every stack row
        // with an id at or below it is in the cache, so an id below it that
        // missed above does not exist at all and is answered without reading
        // anything. Without that distinction a single id that was never cached
        // re-read the whole dictionary on every poll
        // (`scanned=641971 wanted=76194`).
        let cached_upto = self.stacks_max_id.load(Ordering::Acquire);
        if missing.iter().all(|&id| id <= cached_upto) {
            return Ok(out);
        }
        // `stack_id` is allocated monotonically, so anything newer than the
        // cache lives in the last row groups and DuckDB's zone maps skip the
        // rest. Ask for the tail only when every missing id is newer than
        // everything already known.
        let filter = if cached_upto > 0 && missing.iter().all(|&id| id > cached_upto) {
            format!(" WHERE stack_id > {cached_upto}")
        } else {
            String::new()
        };
        let mut stmt = conn
            .prepare_cached(&format!("SELECT stack_id, frames FROM {span}{filter}"))
            .map_err(db_err)?;
        let mut rows = stmt.query([]).map_err(db_err)?;
        // Every row read is cached, not just the ones this query asked for:
        // only then does `stacks_max_id` mean what it says. Rows are buffered
        // so the cache write lock is taken once, after the scan.
        let budget = DICT_CACHE_ROWS.saturating_sub(self.stacks_by_id.read().len());
        let mut all: Vec<(u64, Vec<u32>)> = Vec::new();
        let mut max_seen = 0u64;
        while let Some(row) = rows.next().map_err(db_err)? {
            seen += 1;
            let id = row.get::<_, i64>(0).map_err(db_err)?.max(0) as u64;
            max_seen = max_seen.max(id);
            let blob: Vec<u8> = row.get(1).map_err(db_err)?;
            let frames = unpack_frames(&blob);
            if wanted.contains(&id) {
                out.insert(id, frames.clone());
            }
            if all.len() < budget {
                all.push((id, frames));
            }
        }
        drop(rows);
        drop(stmt);
        // The cache is complete up to `max_seen` only when nothing had to be
        // dropped on the floor; once the budget is exhausted the watermark
        // stops moving and misses fall back to a scan, which is correct.
        let complete = all.len() == seen as usize;
        let inserted = all.len();
        if !all.is_empty() {
            let mut cache = self.stacks_by_id.write();
            for (id, frames) in all {
                cache.insert(id, std::sync::Arc::new(frames));
            }
        }
        if complete && max_seen > cached_upto {
            self.stacks_max_id.fetch_max(max_seen, Ordering::AcqRel);
        }
        self.maybe_save_dict_cache(inserted);
        let elapsed = started.elapsed();
        if elapsed.as_millis() >= 300 {
            info!(
                stage = "load_stacks",
                scanned = seen,
                wanted = ids.len(),
                found = out.len(),
                elapsed_ms = elapsed.as_millis() as u64,
                "slow dictionary read"
            );
        }
        Ok(out)
    }

    /// Same bulk-join strategy as [`Store::load_stacks`]: one statement instead
    /// of thousands of `IN (...)` prepares on the hot path of every query.
    fn load_frames(&self, conn: &Connection, ids: &[u32]) -> Result<HashMap<u32, Frame>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        // The dictionary is read on demand and cached by id. Warming it up
        // front held a reader connection for the length of a full dictionary
        // read —a minute on a real tenant, which timed out the first query
        // after a restart —so the cache fills from the scans that need it.
        let started = std::time::Instant::now();
        let span = self.span_all("frames");
        let mut out = HashMap::with_capacity(ids.len());
        let mut seen = 0u64;
        let mut missing: Vec<u32> = Vec::with_capacity(ids.len());
        {
            let cache = self.frames_by_id.read();
            for &id in ids {
                match cache.get(&id) {
                    Some(frame) => {
                        out.insert(id, (**frame).clone());
                    }
                    None => missing.push(id),
                }
            }
        }
        if missing.is_empty() {
            return Ok(out);
        }
        let wanted: HashSet<u32> = missing.iter().copied().collect();
        // Same contract as [`Store::stacks_max_id`]: the cache holds every
        // frame with an id at or below the watermark, so a miss below it means
        // the frame does not exist.
        let cached_upto = self.frames_max_id.load(Ordering::Acquire);
        if missing.iter().all(|&id| id <= cached_upto) {
            return Ok(out);
        }
        let filter = if cached_upto > 0 && missing.iter().all(|&id| id > cached_upto) {
            format!(" WHERE frame_id > {cached_upto}")
        } else {
            String::new()
        };
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT frame_id, module, symbol, file, line FROM {span}{filter}"
            ))
            .map_err(db_err)?;
        let mut rows = stmt.query([]).map_err(db_err)?;
        let budget = DICT_CACHE_ROWS.saturating_sub(self.frames_by_id.read().len());
        let mut all: Vec<(u32, Frame)> = Vec::new();
        let mut max_seen = 0u32;
        while let Some(row) = rows.next().map_err(db_err)? {
            seen += 1;
            let id = row.get::<_, i64>(0).map_err(db_err)?.max(0) as u32;
            max_seen = max_seen.max(id);
            let frame = Frame {
                module: row.get(1).map_err(db_err)?,
                symbol: row.get(2).map_err(db_err)?,
                file: row.get(3).map_err(db_err)?,
                line: row.get::<_, i64>(4).map_err(db_err)?.max(0) as u32,
                addr: 0,
            };
            if wanted.contains(&id) {
                out.insert(id, frame.clone());
            }
            if all.len() < budget {
                all.push((id, frame));
            }
        }
        drop(rows);
        drop(stmt);
        let complete = all.len() == seen as usize;
        let inserted = all.len();
        if !all.is_empty() {
            let mut cache = self.frames_by_id.write();
            for (id, frame) in all {
                cache.insert(id, std::sync::Arc::new(frame));
            }
        }
        if complete && max_seen > cached_upto {
            self.frames_max_id.fetch_max(max_seen, Ordering::AcqRel);
        }
        self.maybe_save_dict_cache(inserted);
        let elapsed = started.elapsed();
        if elapsed.as_millis() >= 300 {
            info!(
                stage = "load_frames",
                scanned = seen,
                wanted = wanted.len(),
                found = out.len(),
                elapsed_ms = elapsed.as_millis() as u64,
                "slow dictionary read"
            );
        }
        Ok(out)
    }

    /// Where the dictionary snapshot lives. Kept outside `parquet/` so the file
    /// index, the compactor and retention never mistake it for a table.
    fn dict_cache_path(&self) -> std::path::PathBuf {
        self.cfg.root.join("cache").join(DICT_CACHE_FILE)
    }

    /// Fill both dictionary caches from disk.
    ///
    /// Reading a dictionary out of Parquet is the single most expensive thing a
    /// cold query does: one full scan plus a per-row copy through the C API,
    /// which measured 9.5 s for 71 657 stacks and 15.4 s for 140 627 frames
    /// inside a container. Deserializing the same rows takes tens of
    /// milliseconds. The file is only a cache —an id map that never shrinks and
    /// carries its own completeness watermarks —so a stale or missing file only
    /// means more ids are fetched from Parquet.
    fn restore_dict_cache(&self) {
        let path = self.dict_cache_path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => return,
        };
        let cache: DictCache = match bincode::deserialize(&bytes) {
            Ok(cache) => cache,
            Err(err) => {
                debug!(path = %path.display(), error = %err, "ignoring unreadable dictionary cache");
                return;
            }
        };
        if cache.version != DICT_CACHE_VERSION {
            debug!(version = cache.version, "ignoring dictionary cache from another layout");
            return;
        }
        let started = std::time::Instant::now();
        let (stacks, frames) = (cache.stacks.len(), cache.frames.len());
        {
            let mut map = self.stacks_by_id.write();
            map.reserve(stacks);
            for (id, frames) in cache.stacks {
                map.insert(id, std::sync::Arc::new(frames));
            }
        }
        {
            let mut map = self.frames_by_id.write();
            map.reserve(frames);
            for (id, frame) in cache.frames {
                map.insert(id, std::sync::Arc::new(frame));
            }
        }
        // Only claim the watermarks the file actually carried: entries past
        // them were never written down, so they must still be fetched.
        self.stacks_max_id
            .fetch_max(cache.stacks_upto, Ordering::AcqRel);
        self.frames_max_id
            .fetch_max(cache.frames_upto, Ordering::AcqRel);
        // The counters were seeded from the stored tables, but a cache entry can
        // outlive the rows that named it (retention drops whole hours), and
        // handing that id to a new frame would alias the stale entry.
        {
            let mut ids = self.ids.lock();
            ids.next_frame = ids.next_frame.max(cache.frames_upto.saturating_add(1));
            ids.next_stack = ids.next_stack.max(cache.stacks_upto.saturating_add(1));
        }
        *self.dict_saved_at.lock() = Some(std::time::Instant::now());
        info!(
            stage = "dict_cache",
            path = %path.display(),
            stacks,
            frames,
            stacks_upto = cache.stacks_upto,
            frames_upto = cache.frames_upto,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "dictionaries restored from disk"
        );
    }

    /// Write both dictionary caches to disk.
    ///
    /// Called after a scan has grown them; a crash between the temp write and
    /// the rename leaves the previous file intact, which is why the write goes
    /// through a temp file at all.
    fn save_dict_cache(&self) {
        let started = std::time::Instant::now();
        let cache = DictCache {
            version: DICT_CACHE_VERSION,
            stacks_upto: self.stacks_max_id.load(Ordering::Acquire),
            frames_upto: self.frames_max_id.load(Ordering::Acquire),
            stacks: self
                .stacks_by_id
                .read()
                .iter()
                .map(|(id, frames)| (*id, (**frames).clone()))
                .collect(),
            frames: self
                .frames_by_id
                .read()
                .iter()
                .map(|(id, frame)| (*id, (**frame).clone()))
                .collect(),
        };
        let dir = self.cfg.root.join("cache");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join(DICT_CACHE_FILE);
        let tmp = dir.join(format!("{DICT_CACHE_FILE}.tmp"));
        let bytes = match bincode::serialize(&cache) {
            Ok(bytes) => bytes,
            Err(err) => {
                debug!(error = %err, "could not serialize dictionary cache");
                return;
            }
        };
        if let Err(err) = std::fs::write(&tmp, &bytes) {
            debug!(error = %err, "could not write dictionary cache");
            return;
        }
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        *self.dict_saved_at.lock() = Some(std::time::Instant::now());
        info!(
            stage = "dict_cache",
            path = %path.display(),
            bytes = bytes.len(),
            stacks = cache.stacks.len(),
            frames = cache.frames.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "dictionaries saved to disk"
        );
    }

    /// Persist the dictionaries if the scan that just ran moved them, at most
    /// once a minute: a flush every ten seconds would otherwise rewrite the
    /// file for a handful of new ids.
    fn maybe_save_dict_cache(&self, inserted: usize) {
        if inserted == 0 {
            return;
        }
        let due = match *self.dict_saved_at.lock() {
            Some(at) => at.elapsed() >= DICT_SAVE_INTERVAL,
            None => true,
        };
        if due || inserted >= DICT_SAVE_MIN_ROWS {
            self.save_dict_cache();
        }
    }

    pub fn stats(&self) -> StoreStats {
        self.stats_snapshot()
            .unwrap_or((StoreStats::default(), 0))
            .0
    }

    /// The catalogue numbers, without scanning.
    ///
    /// The tenant list describes every tenant at once. Answering it used to
    /// mean a full scan per tenant on the request path —ten tenants of 1.5 s
    /// each is what made the dashboard's first call time out. The row half is
    /// now arithmetic (see [`Store::append`]); only the distinct-series count
    /// still costs a scan, and it is allowed to be ten minutes old.
    pub fn stats_cached(&self) -> (StoreStats, usize) {
        self.stats_snapshot().unwrap_or_default()
    }

    /// [`Store::stats_snapshot`] without the fallback: a busy store is an
    /// error, not zero rows. Internal callers are happy with the zeros (a log
    /// line, an eviction estimate); the HTTP surface is not —a dashboard told
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
        // Wait briefly, not for the full write budget: a recount reads the whole
        // archive, so queueing a dashboard poll behind a flush for 30 s is
        // worse than answering this one poll approximately.
        let Some(conn) = self.try_lock_conn(ConnUser::Write, AGG_LOCK_WAIT) else {
            // Somebody else holds the writer. Answer from a read snapshot —            // a consistent one —but do not install it: a batch committing
            // during that scan may be missing, and the deltas added later would
            // be added to a number that already lost rows. The background warm
            // (and the next poll) will count it properly.
            debug!("counting the catalogue from a read snapshot; the writer is busy");
            let conn = self.read_conn()?;
            return self.recount(&conn);
        };
        // Nothing can append while this thread owns the writer, so a re-check
        // under the guard is enough —no other thread can be counting.
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
    /// The caller must own `conn` —the writer, or a maintenance connection
    /// that already holds it —so the recount cannot interleave with an append.
    fn recount(&self, conn: &Connection) -> Result<RowAgg> {
        #[cfg(test)]
        self.recounts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

    /// Make everything accepted so far queryable.
    ///
    /// `/api/v1/flush` promises that a caller can query its own write right
    /// after the response. In the asynchronous ingest mode the upload may still
    /// be sitting in a segment, so a flush has to convert first, then drain the
    /// hot tier the converter produced.
    pub fn flush_durable(&self) -> Result<usize> {
        self.wal.seal();
        // A converter may already have claimed one of the segments this flush
        // wants to publish. Waiting for the log to empty is what makes the
        // endpoint's promise true: after `/flush`, everything accepted before
        // the call is queryable —not "everything except what another thread
        // happens to be holding".
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let mut total = 0usize;
        loop {
            total += self.convert_pending()?.flushed_rows;
            total += self.flush_all_hot()?;
            let (bytes, _) = self.wal_stats();
            if bytes == 0 {
                return Ok(total);
            }
            if Instant::now() >= deadline {
                return Err(Error::Busy(
                    "the ingest log did not drain in time; retry the flush".into(),
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
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

    /// Move one bounded slice of the hot tier to Parquet.
    ///
    /// The slice is what makes the queue work: a job that moves 200k rows hands
    /// the writer back in a fraction of a second, so the reports queued behind
    /// it still commit promptly. An all-or-nothing flush had the opposite
    /// problem —on a tenant with a backlog it would own the writer for minutes,
    /// which is why the pass that ran on a timer could never afford to start.
    ///
    /// Rows move between tiers; the catalogue's counts, sums and time bounds do
    /// not change, so nothing invalidates.
    fn flush_chunk(&self) -> Result<usize> {
        let conn = self.lock_conn(ConnUser::Flush);
        let moved = self.flush_chunk_on(&conn)?;
        Ok(moved)
    }

    /// One bounded flush slice on a connection the caller owns.
    ///
    /// The lock matters because a flush is a `COPY` followed by a `DELETE`:
    /// two of those at once would fight over the same rows. Appends may run in
    /// parallel, deletions may not.
    fn flush_chunk_on(&self, conn: &Connection) -> Result<usize> {
        let _no_append_in_flight = self.flush_gate.write();
        let moved = parquet::flush_hot_bounded(conn, &self.parquet, parquet::FLUSH_CHUNK_ROWS)?;
        self.last_flush_ns.store(now_ns(), Ordering::Relaxed);
        Ok(moved)
    }

    /// Flush on the configured cadence, on the writer thread's queue.
    ///
    /// This used to try the writer lock for 500 ms and skip the tick, to stay
    /// out of the way of queries. Under steady ingest the writer is never free
    /// for 500 ms —every append is a writer transaction —so the pass never
    /// ran at all, the hot tier grew to millions of rows, and each query paid to
    /// scan them. As a queued job it always gets a turn, and because it is now
    /// sliced it does not monopolise that turn.
    pub fn flush_hot_when_idle(&self) -> Result<usize> {
        let now = now_ns();
        let last = self.last_flush_ns.load(Ordering::Relaxed);
        if now.saturating_sub(last) < self.cfg.flush_interval_ns() {
            return Ok(0);
        }
        self.flush_until_caught_up()
    }

    /// [`Store::flush_hot_when_idle`], repeated until the backlog is gone.
    ///
    /// One pass per tick would leave a multi-million-row backlog draining for
    /// half an hour, and until it is gone every query scans it. Each iteration
    /// is a separate job, so appends interleave between slices instead of
    /// waiting behind the whole drain. The cadence check lives in the caller:
    /// once a drain has started it must not stop after one slice because the
    /// clock says the next flush is ten seconds away.
    pub fn flush_until_caught_up(&self) -> Result<usize> {
        const MAX_SLICES: usize = 8;
        let mut total = 0usize;
        let mut slices = 0usize;
        for _ in 0..MAX_SLICES {
            let moved = self.run_exclusive(Store::flush_chunk)?;
            total += moved;
            slices += 1;
            if moved < parquet::FLUSH_CHUNK_ROWS {
                break;
            }
        }
        // A backlog that never drains is invisible without this: the pass
        // reports success every tick, and the only symptom is that every query
        // keeps scanning the tier it was supposed to shrink.
        if total > 0 {
            info!(
                stage = "flush_drain",
                slices,
                moved = total,
                "moved hot rows to Parquet"
            );
        }
        Ok(total)
    }

    /// Run one bounded flush slice.
    ///
    /// Tests only. Production reaches this through the writer thread, which is
    /// also what makes the slice size observable: a slice is the unit of time an
    /// ingest can be made to wait.
    #[cfg(test)]
    pub(crate) fn flush_slice(&self, budget: usize) -> Result<usize> {
        let conn = self.lock_conn(ConnUser::Flush);
        parquet::flush_hot_bounded(&conn, &self.parquet, budget)
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
            .prepare_cached(&format!(
                // A gauge is reported as the newest batch of the series, not as
                // the sum of every batch in the window: `inuse_space` is memory
                // held at one instant, and adding ten instants multiplies it by
                // ten. `arg_max` picks the value that belongs to `MAX(end_ns)`.
                //
                // `alloc_space` is a gauge too when a runtime reports it: the Go
                // reader and JFR both hand over the bytes allocated since the
                // process started, so summing twelve hundred of those batches
                // turned 6 GB into 72 TB. Only the injected reader sends
                // per-interval deltas, and it says so by carrying
                // `alloc.reader=` in its labels — that is the one case that
                // stays a sum.
                "SELECT service_name, profile_type, labels, \
                        CASE WHEN instr(labels, 'temporality=cumulative') > 0 \
                                  OR instr(labels, 'temporality=gauge') > 0 \
                                  OR profile_type IN ('inuse_space', 'inuse_objects', 'memory_rss', 'memory_pss', 'cgroup_memory', 'process_rss', 'process_pss', 'pod_cgroup_memory') \
                                  OR (profile_type = 'alloc_space' AND instr(labels, 'alloc.reader=') = 0) \
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
    ///
    /// Runs on the writer thread's queue: pruning deletes rows and recounts the
    /// catalogue, so it needs the writer to itself, and queueing is what gives
    /// it a turn without a deadline it can miss.
    pub fn prune(&self, now_ns: u64) -> Result<PruneStats> {
        self.run_exclusive(move |store| store.prune_now(now_ns))
    }

    fn prune_now(&self, now_ns: u64) -> Result<PruneStats> {
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
            // Retired files are kept for a grace period so a query that already
            // named one can still open it. Collect the ones past it here rather
            // than at the moment they are published: unlinking then fails the
            // query that was already in flight.
            let purged = parquet::purge_trash(&self.parquet, parquet::TRASH_TTL);
            if purged > 0 {
                debug!(files = purged, "collected retired Parquet files");
            }
            // `prune_archives` already rescanned the file index; there are no
            // views left to rebuild.
            // What this pass deletes, so the catalogue aggregate can be adjusted
            // by subtraction. Recounting the whole archive instead is O(all
            // data) on every pass, and a retention pass runs every interval: a
            // catch-up deleted rows on each one and spent the writer on an
            // 8-second scan each time, which is why ingest started answering
            // 503. Subtracting is O(what is going away).
            let mut deleted = (0usize, 0u64, 0u64);
            if let Some(retention) = self.cfg.retention_ns {
                let cutoff = now_ns.saturating_sub(retention);
                // One pass removes at most `PRUNE_BATCH_ROWS` batches, oldest
                // first, and the rest goes on the next tick.
                //
                // The delete used to be the whole expired range in one
                // statement. On a database whose hot tier had grown to millions
                // of rows —which is exactly what happens after a while of
                // failed flushes —that single delete dirtied enough of the
                // file that the CHECKPOINT below rewrote all of it: minutes
                // inside one queued job. Every other producer timed out behind
                // it, including the flush that would have shrunk the tier, so
                // the state that caused the long delete was the state it
                // preserved. A bounded delete converges in a few ticks instead
                // and never costs more than one tick's worth of work.
                //
                // `<=` is deliberate. With `<` the boundary row itself stayed
                // behind, so a tier whose last expired row was also its newest
                // row deleted nothing on every subsequent pass: retention would
                // stall one row short of done, forever.
                if let Some(pass_cutoff) =
                    expired_cutoff(&conn, &batches_t, cutoff, PRUNE_BATCH_ROWS)?
                {
                    let row = conn
                        .query_row(
                            &format!(
                                "SELECT COUNT(*), COALESCE(SUM(total_value), 0), \
                                 COALESCE(SUM(bytes), 0) FROM {batches_t} WHERE end_ns <= ?"
                            ),
                            params![ts(pass_cutoff)],
                            |row| {
                                Ok((
                                    row.get::<_, i64>(0).unwrap_or(0),
                                    row.get::<_, i64>(1).unwrap_or(0),
                                    row.get::<_, i64>(2).unwrap_or(0),
                                ))
                            },
                        )
                        .unwrap_or((0, 0, 0));
                    let removed = row.0.max(0);
                    deleted = (removed as usize, row.1.max(0) as u64, row.2.max(0) as u64);
                    conn.execute(
                        &format!("DELETE FROM {samples_t} WHERE end_ns <= ?"),
                        params![ts(pass_cutoff)],
                    )
                    .map_err(db_err)?;
                    conn.execute(
                        &format!("DELETE FROM {batches_t} WHERE end_ns <= ?"),
                        params![ts(pass_cutoff)],
                    )
                    .map_err(db_err)?;
                    // The map is a second, independent table family: it has
                    // no batches, so it needs its own cutoff.
                    conn.execute(
                        &format!("DELETE FROM {nodes_t} WHERE bucket_ns <= ?"),
                        params![ts(pass_cutoff)],
                    )
                    .map_err(db_err)?;
                    conn.execute(
                        &format!("DELETE FROM {edges_t} WHERE bucket_ns <= ?"),
                        params![ts(pass_cutoff)],
                    )
                    .map_err(db_err)?;
                    stats.removed_batches += removed as usize;
                }
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
                    let oldest: Option<(i64, i64, i64)> = conn
                        .query_row(
                            &format!(
                                "SELECT batch_id, total_value, bytes FROM {legacy_span} \
                                 ORDER BY start_ns ASC, batch_id ASC LIMIT 1"
                            ),
                            [],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )
                        .ok();
                    let Some((batch_id, value, bytes)) = oldest else {
                        break;
                    };
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
                    deleted.0 += 1;
                    deleted.1 = deleted.1.saturating_add(value.max(0) as u64);
                    deleted.2 = deleted.2.saturating_add(bytes.max(0) as u64);
                }
            }

            // Only rewrite the database file when this pass actually removed
            // rows from it. Retention fires on its own interval whether or not
            // there is anything to do, and an unconditional `CHECKPOINT` made
            // every quiet pass rewrite the whole file —seconds of writer time
            // to reclaim nothing, which is time the flush behind it waits for.
            if deleted.0 > 0 {
                conn.execute_batch("CHECKPOINT").map_err(db_err)?;
            }
            // Two ways to keep the catalogue honest after a prune, and the
            // choice is about cost, not accuracy:
            //
            // - Rows deleted from the hot tables were counted by the aggregate,
            //   and this pass already knows how many and how much, so subtract.
            //   That is O(what is going away) and it is exact.
            // - Whole Parquet files deleted by retention or the disk cap were
            //   counted too, but their rows are not readable any more, so the
            //   only exact answer is a recount. That is O(the whole archive) and
            //   it runs while holding the writer, so it is confined to the case
            //   that needs it.
            //
            // Recounting unconditionally —the obvious version —made every
            // retention pass read 4 million batches under the writer, eight
            // seconds at a time, and ingest answered 503 throughout.
            if stats.removed_files > 0 {
                *self.agg.lock() = Some(self.recount(&conn)?);
            } else if deleted.0 > 0 {
                let mut slot = self.agg.lock();
                if let Some(agg) = slot.as_mut() {
                    agg.batch_count = agg.batch_count.saturating_sub(deleted.0);
                    agg.total_value = agg.total_value.saturating_sub(deleted.1);
                    agg.logical_bytes = agg.logical_bytes.saturating_sub(deleted.2);
                    // The time bounds are left alone. They only move when the
                    // oldest rows leave, and when they do it is retention that
                    // took them —which also deletes their file, so the branch
                    // above fixes the bounds in the same pass.
                }
            }
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
        // `hour=<n>/`, so a clear must remove the whole table tree —a
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
        if self.writer.get().is_none() {
            return self.append_servicemap_now(batch);
        }
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        self.queue(
            WriteJob::ServiceMap {
                batch: batch.clone(),
                reply,
            },
            answer,
        )
    }

    fn append_servicemap_now(&self, batch: &ServiceMapBatch) -> Result<()> {
        // Reports arrive every few seconds, but a map query sums across a
        // window of an hour or more: at one row per report per node, a single
        // hour is thousands of rows for a couple of dozen nodes, and the scan
        // —not the bytes —is what the map waits on. Flooring the bucket to a
        // minute and folding the new counters into the row that is already
        // there keeps every existing query exact (they all sum buckets) while
        // cutting the rows such a query walks by the reporting rate.
        let bucket = ts(batch.start_ns) / 60_000_000_000 * 60_000_000_000;
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
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                         ON CONFLICT (bucket_ns, node_id) DO UPDATE SET \
                         namespace = excluded.namespace, workload = excluded.workload, \
                         service = excluded.service, kind = excluded.kind, \
                         k8s_node = excluded.k8s_node, \
                         rx_bytes = {nodes_t}.rx_bytes + excluded.rx_bytes, \
                         tx_bytes = {nodes_t}.tx_bytes + excluded.tx_bytes, \
                         connections = {nodes_t}.connections + excluded.connections, \
                         retransmits = {nodes_t}.retransmits + excluded.retransmits, \
                         pods = excluded.pods, ready = excluded.ready, \
                         instances = excluded.instances, containers = excluded.containers"
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
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                         ON CONFLICT (bucket_ns, client_id, server_id, protocol, client_pod, \
                         server_pod, host, sql) DO UPDATE SET \
                         client_container = excluded.client_container, \
                         server_container = excluded.server_container, \
                         connections = {edges_t}.connections + excluded.connections, \
                         retransmits = {edges_t}.retransmits + excluded.retransmits, \
                         tx_bytes = {edges_t}.tx_bytes + excluded.tx_bytes, \
                         rx_bytes = {edges_t}.rx_bytes + excluded.rx_bytes, \
                         rtt_us_sum = {edges_t}.rtt_us_sum + excluded.rtt_us_sum, \
                         rtt_samples = {edges_t}.rtt_samples + excluded.rtt_samples"
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
        // Clearing the cache here made every service-map window —one per node
        // every ten seconds —force the next dashboard poll to rescan the
        // tenant's whole archive.
        Ok(())
    }

    /// Nodes of the map, busiest first.
    pub fn net_nodes(&self, from_ns: u64, until_ns: u64, limit: usize) -> Result<Vec<MapNode>> {
        // Same race as [`Store::scan`]: the index named a file that a merge
        // retired before DuckDB opened it. Rebuilding the statement reads a
        // fresh index, so one retry turns a 500 into a slower answer.
        let started = std::time::Instant::now();
        let out = match self.net_nodes_once(from_ns, until_ns, limit) {
            Err(err) if is_retired_file(&err) => {
                debug!(error = %err, "a Parquet file was retired mid-query; retrying");
                self.net_nodes_once(from_ns, until_ns, limit)
            }
            other => other,
        };
        let elapsed = started.elapsed();
        if elapsed.as_millis() >= 300 {
            info!(
                stage = "net_nodes",
                rows = out.as_ref().map(Vec::len).unwrap_or(0),
                elapsed_ms = elapsed.as_millis() as u64,
                "slow service map read"
            );
        }
        out
    }

    fn net_nodes_once(&self, from_ns: u64, until_ns: u64, limit: usize) -> Result<Vec<MapNode>> {
        let started = Instant::now();
        let conn = self.read_conn()?;
        let span = self.span("net_nodes", from_ns, until_ns);
        let prepare_started = Instant::now();
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT node_id, any_value(namespace), any_value(workload), any_value(service), \
                        any_value(kind), sum(rx_bytes), sum(tx_bytes), sum(connections), \
                        sum(retransmits), \
                        arg_max(pods, (bucket_ns, pods)), arg_max(ready, (bucket_ns, pods)) \
                 FROM {span} WHERE bucket_ns >= ? AND bucket_ns < ? \
                 GROUP BY node_id \
                 ORDER BY sum(connections) + (sum(rx_bytes) + sum(tx_bytes)) / 1024 DESC LIMIT ?"
            ))
            .map_err(db_err)?;
        let prepare_ms = prepare_started.elapsed().as_millis() as u64;
        let query_started = Instant::now();
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
                // The pod and container lists are detail behind a click, so
                // they are not aggregated here: carrying them through
                // `arg_max` made every bucket of every node drag two JSON
                // blobs through the aggregate. `net_node_detail` reads them
                // for the one node whose panel was opened.
                instances: Vec::new(),
                containers: Vec::new(),
            });
        }
        let query_ms = query_started.elapsed().as_millis() as u64;
        if started.elapsed().as_millis() >= 300 {
            tracing::info!(
                stage = "net_nodes_once",
                prepare_ms,
                query_ms,
                rows = out.len() as u64,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "slow service map read"
            );
        }
        Ok(out)
    }

    /// The pods and containers behind one node, as of the newest bucket before
    /// `until_ns`.
    ///
    /// This is the detail the map deliberately leaves out: it is one row (plus
    /// a short index walk) for one node, so it costs nothing until a panel is
    /// opened, while the map stays a pure set of counters.
    pub fn net_node_detail(
        &self,
        node: &str,
        until_ns: u64,
    ) -> Result<(Vec<InstanceInfo>, Vec<String>)> {
        let conn = self.read_conn()?;
        // An hour covers the newest bucket of a live map as well as the last
        // hour of a historical window, so a panel opened on yesterday's data
        // still names its pods. This runs only for the node whose panel was
        // opened, never for the whole map.
        let from_ns = until_ns.saturating_sub(3_600_000_000_000);
        let span = self.span("net_nodes", from_ns, until_ns);
        // An agent reports the replica list only while it can see it, so the
        // very last bucket of a window can be empty for a workload that still
        // had pods a minute earlier. Read the newest bucket that *does* name its
        // pods, and fall back to the plain newest row so a genuinely empty
        // workload still answers "no pods" rather than "no data".
        let read = |filter: &str| -> Result<Option<(String, String)>> {
            let mut stmt = conn
                .prepare_cached(&format!(
                    "SELECT instances, containers FROM {span} \
                     WHERE node_id = ? AND bucket_ns >= ? AND bucket_ns < ? {filter} \
                     ORDER BY bucket_ns DESC LIMIT 1"
                ))
                .map_err(db_err)?;
            let mut rows = stmt
                .query(params![node, ts(from_ns), ts(until_ns)])
                .map_err(db_err)?;
            let Some(row) = rows.next().map_err(db_err)? else {
                return Ok(None);
            };
            Ok(Some((
                row.get::<_, String>(0).unwrap_or_default(),
                row.get::<_, String>(1).unwrap_or_default(),
            )))
        };
        let row = match read("AND (instances <> '[]' OR containers <> '[]')") {
            Ok(Some(pair)) => Some(pair),
            Ok(None) => match read("") {
                Ok(pair) => pair,
                Err(err) => {
                    info!(error = %err, node, "node detail query failed");
                    return Ok((Vec::new(), Vec::new()));
                }
            },
            Err(err) => {
                info!(error = %err, node, "node detail query failed");
                return Ok((Vec::new(), Vec::new()));
            }
        };
        let Some((instances, containers)) = row else {
            info!(node, "node detail: no row in the window");
            return Ok((Vec::new(), Vec::new()));
        };
        let raw_instances = instances.len();
        let raw_containers = containers.len();
        let instances: Vec<InstanceInfo> = serde_json::from_str(&instances).unwrap_or_default();
        let containers: Vec<String> = serde_json::from_str(&containers).unwrap_or_default();
        if instances.is_empty() && containers.is_empty() {
            info!(
                node,
                raw_instances, raw_containers, "node detail: the newest row names no pods"
            );
        }
        Ok((instances, containers))
    }

    /// Edges of the map, aggregated over the window.
    ///
    /// The `HAVING` keeps an edge that carried traffic but opened no new
    /// connection in the window: a busy long-lived connection has
    /// `connections = 0` and a lot of bytes, and dropping it would make the
    /// map flicker as connections stop being re-established.
    pub fn net_edges(&self, from_ns: u64, until_ns: u64) -> Result<Vec<MapEdge>> {
        let started = std::time::Instant::now();
        let out = match self.net_edges_once(from_ns, until_ns) {
            Err(err) if is_retired_file(&err) => {
                debug!(error = %err, "a Parquet file was retired mid-query; retrying");
                self.net_edges_once(from_ns, until_ns)
            }
            other => other,
        };
        let elapsed = started.elapsed();
        if elapsed.as_millis() >= 300 {
            info!(
                stage = "net_edges",
                rows = out.as_ref().map(Vec::len).unwrap_or(0),
                elapsed_ms = elapsed.as_millis() as u64,
                "slow service map read"
            );
        }
        out
    }

    fn net_edges_once(&self, from_ns: u64, until_ns: u64) -> Result<Vec<MapEdge>> {
        let conn = self.read_conn()?;
        let span = self.span("net_edges", from_ns, until_ns);
        let mut stmt = conn
            .prepare_cached(&format!(
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
    /// largest count seen rather than the sum —the same rule [`ServiceNode`]
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
            .prepare_cached(&format!(
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
            .prepare_cached(&format!(
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
            .prepare_cached(&format!(
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
    /// the same reason as [`Store::net_node_history`] —the absolute ns value
    /// is far past the exact integer range of a double.
    /// A batch describes a window, not an instant: its counters belong to the
    /// whole `[batch start, next batch start)`. Putting the total on the single
    /// bucket that happens to contain `batch start` makes a five-minute sample
    /// rate look like a spike train once the brush zooms below five minutes —    /// the 1h view has 30-second buckets and would show ten empty slots between
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
            .prepare_cached(&format!(
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
            .prepare_cached(&format!(
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

    fn net_node_detail(
        &self,
        node: &str,
        until_ns: u64,
    ) -> Result<(Vec<InstanceInfo>, Vec<String>)> {
        Store::net_node_detail(self, node, until_ns)
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
///   and drop the final sort —a large win for the scan/GROUP BY queries that
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
/// Only safe because a data directory has exactly one writer —the chart pins
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
/// bucket or a node still benefit from an ART index —especially on the wide
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
    // The common case —an already-current database —must stay read-only at
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

/// Batches one retention pass may delete from the hot tier.
///
/// Retention is expressed in days, but a pass is a unit of work the writer has
/// to fit between two reports. Catching up after a backlog is spread over as
/// many ticks as it takes instead of landing as one delete that rewrites the
/// whole database file.
const PRUNE_BATCH_ROWS: i64 = 100_000;

/// The effective `end_ns` boundary for one retention pass.
///
/// `None` means nothing is older than `cutoff`. Otherwise the returned value is
/// the largest `end_ns` of the `limit` oldest expired rows, so deleting
/// everything strictly below it removes at most that many batches. `end_ns` is
/// the same column every hot table is cut on, so one boundary keeps the tiers
/// consistent.
fn expired_cutoff(
    conn: &Connection,
    batches: &str,
    cutoff: u64,
    limit: i64,
) -> Result<Option<u64>> {
    let sql = format!(
        "SELECT max(end_ns) FROM (\
         SELECT end_ns FROM {batches} WHERE end_ns < ? ORDER BY end_ns LIMIT ?)"
    );
    let value: Option<i64> = conn
        .query_row(&sql, params![ts(cutoff), limit.max(1)], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .map_err(db_err)?;
    Ok(value.map(|ns| ns.max(0) as u64))
}

/// Run `f` as one explicit transaction on the shared connection.
///
/// DuckDB aborts a transaction on *any* statement failure —including a
/// failing `COMMIT` —and afterwards accepts only `ROLLBACK`. Leaving that
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

    /// A prune that removes nothing must not re-read the archive.
    ///
    /// Retention pruning runs on a timer against a window that is usually still
    /// inside retention, so most passes find nothing to delete. Recounting
    /// unconditionally made every pass read the whole archive *while holding the
    /// writer* —on a 4-million-batch tenant that is half a minute of the writer
    /// per pass, and ingest answered 503 the whole time.
    #[test]
    fn a_prune_that_removes_nothing_does_not_recount() {
        let (store, dir) = temp_store("prune-no-recount");
        for i in 0..3u64 {
            store
                .append(&report("svc", 1_000 + i, "a", &["hot"], 10))
                .unwrap();
        }
        // The first read counts the archive; after that it is arithmetic.
        assert_eq!(store.stats().batch_count, 3);
        let recounts = store.recounts.load(std::sync::atomic::Ordering::Relaxed);

        for _ in 0..5 {
            let pruned = store.prune(now_ns()).unwrap();
            assert_eq!(pruned.removed_batches, 0, "nothing is older than retention");
        }
        assert_eq!(
            store.recounts.load(std::sync::atomic::Ordering::Relaxed),
            recounts,
            "pruning deleted nothing, so nothing had to be recounted"
        );
        assert_eq!(store.stats().batch_count, 3);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A retention catch-up subtracts what it deleted; it does not rescan.
    ///
    /// When retention is shortened (or a pod was down long enough to fall
    /// behind), every prune pass deletes rows. Recounting the archive on each
    /// pass meant the writer was reading 4 million batches more often than not,
    /// and ingest timed out against it —503s for the whole catch-up.
    #[test]
    fn a_prune_catch_up_subtracts_instead_of_rescanning() {
        let dir =
            std::env::temp_dir().join(format!("lidlesseye-store-catchup-{}", uuid::Uuid::new_v4()));
        let mut cfg = StoreConfig::new(&dir);
        // Everything is already older than retention, so each pass has work.
        cfg.retention_ns = Some(1);
        let store = Store::open(cfg).unwrap();

        for i in 0..2u64 {
            store
                .append(&report("svc", 1_000 + i, "a", &["hot"], 10 + i))
                .unwrap();
        }
        assert_eq!(store.stats().batch_count, 2);
        let counted = store.recounts.load(std::sync::atomic::Ordering::Relaxed);

        let pruned = store.prune(now_ns()).unwrap();
        assert_eq!(pruned.removed_batches, 2, "both batches are past retention");
        // Exact, from what the pass deleted —not from a second scan.
        assert_eq!(store.stats().batch_count, 0);
        assert_eq!(store.stats().total_value, 0);
        assert_eq!(
            store.recounts.load(std::sync::atomic::Ordering::Relaxed),
            counted,
            "deleting rows must not re-read the archive"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// One upload must cost one commit, not one per report.
    ///
    /// A few hundred reports per flush used to mean a few hundred transactions
    /// on the single writer; the queue never drained and ingest timed out
    /// against a writer that was busy with bookkeeping.
    #[test]
    fn one_upload_is_one_commit() {
        let (store, dir) = temp_store("batch-commit");
        let before = store.commits.load(std::sync::atomic::Ordering::Relaxed);
        let reports: Vec<Report> = (0..50)
            .map(|i| report("api", 1_000 + i, "a", &["leaf", "mid", "root"], 10 + i))
            .collect();

        let results = store.append_owned_batch(reports);
        assert_eq!(results.len(), 50, "one result per report");
        assert!(results.iter().all(|result| result.is_ok()));

        let commits = store.commits.load(std::sync::atomic::Ordering::Relaxed) - before;
        assert_eq!(commits, 1, "the upload was committed {commits} times");

        let mut ids: Vec<u64> = results
            .into_iter()
            .map(|result| result.unwrap().batch_id)
            .collect();
        assert_eq!(store.stats().batch_count, 50);
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 50, "batch ids must be unique");

        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A report the store refuses must not take the rest of the upload down.
    #[test]
    fn one_invalid_report_does_not_fail_the_upload() {
        let (store, dir) = temp_store("batch-invalid");
        let good = report("api", 1_000, "a", &["leaf"], 10);

        // A value the server's own validation rejects: this is what a counter
        // that went negative on the kernel side used to produce.
        let mut bad = report("api", 2_000, "a", &["leaf"], 10);
        bad.stacks[0].value = u64::MAX;

        let results = store.append_owned_batch(vec![good, bad]);
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok(), "the valid report was refused");
        assert!(results[1].is_err(), "the invalid report was accepted");
        assert_eq!(store.stats().batch_count, 1);

        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A file retired by a merge or an upload must not fail the next query.
    ///
    /// The index is read when a query starts and the paths are opened when it
    /// runs; compaction and the S3 archive can publish and retire a file in
    /// between. Naming a file that no longer exists fails the whole panel with
    /// "Cannot open file", which is what a dashboard showed as a 500.
    #[test]
    fn a_query_survives_a_file_retired_between_index_and_execution() {
        let (store, dir) = temp_store("retired-file");
        let start = crate::time::now_ns() - 60_000_000_000;
        store
            .append(&report("api", start, "a", &["leaf", "mid"], 5))
            .unwrap();
        store.flush_hot().unwrap();

        // Everything the query would name is on disk at this point.
        let samples_dir = dir.join("parquet").join("samples");
        let mut victims = Vec::new();
        for entry in walk_files(&samples_dir) {
            victims.push(entry);
        }
        assert!(!victims.is_empty(), "the flush wrote no sample file");
        // Retire them exactly as compaction/archive does, leaving the in-memory
        // index untouched —that stale index is what used to break the query.
        for path in &victims {
            crate::parquet::retire_file(&store.parquet, path);
        }

        let scanned = store.scan(&crate::store::ScanQuery::new(
            &ProfileType::cpu(),
            start,
            start + 1_000,
            &crate::Selector::default(),
        ));
        assert!(
            scanned.is_ok(),
            "a retired Parquet file failed the query: {}",
            scanned.err().map(|e| e.to_string()).unwrap_or_default()
        );

        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn walk_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk_files(&path));
            } else if path.extension().is_some_and(|ext| ext == "parquet") {
                out.push(path);
            }
        }
        out
    }

    /// A flush must never run between the copy and the delete of another
    /// flush's rows.
    ///
    /// `flush_hot` is `COPY` then `DELETE`, two statements. An append that
    /// commits between them is deleted without ever reaching Parquet —silent
    /// data loss that only shows up as a missing window. The append/flush gate
    /// is what makes this test pass; without it the row count comes up short.
    #[test]
    fn concurrent_appends_and_flushes_lose_nothing() {
        let dir =
            std::env::temp_dir().join(format!("lidlesseye-store-gate-{}", uuid::Uuid::new_v4()));
        let mut cfg = StoreConfig::new(&dir);
        cfg.wal_sync_interval_ms = Some(0);
        let store = std::sync::Arc::new(Store::open(cfg).unwrap());
        let start = crate::time::now_ns() - 60_000_000_000;

        const BATCHES: u64 = 120;
        let conn_append = store.conn.lock().try_clone().unwrap();
        let conn_flush = store.conn.lock().try_clone().unwrap();
        let appender = store.clone();
        let flusher = store.clone();

        std::thread::scope(|scope| {
            let a = scope.spawn(move || {
                for i in 0..BATCHES {
                    let report = report("api", start + i, "a", &["leaf", "mid"], 1);
                    let results = appender.append_many_on(&conn_append, &[report]);
                    assert!(results.iter().all(|r| r.is_ok()));
                }
            });
            let b = scope.spawn(move || {
                for _ in 0..BATCHES {
                    flusher.flush_all_hot_on(&conn_flush).unwrap();
                }
            });
            a.join().unwrap();
            b.join().unwrap();
        });

        store.flush_all_hot().unwrap();
        assert_eq!(
            store.stats().batch_count,
            BATCHES as usize,
            "an append was flushed away without being copied"
        );
        assert_eq!(store.stats().total_value, BATCHES);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Two converters may run at once: DuckDB appends do not conflict, and
    /// claiming a segment is a rename, so no segment is ever published twice.
    #[test]
    fn two_converters_count_every_segment_exactly_once() {
        let dir = std::env::temp_dir().join(format!(
            "lidlesseye-store-parallel-{}",
            uuid::Uuid::new_v4()
        ));
        let mut cfg = StoreConfig::new(&dir);
        // One small segment per 4 reports, so there is real work to share.
        cfg.wal_segment_bytes = Some(64 * 1024);
        cfg.wal_sync_interval_ms = Some(0);
        let store = std::sync::Arc::new(Store::open(cfg).unwrap());
        let start = crate::time::now_ns() - 60_000_000_000;

        const BATCHES: u64 = 40;
        for i in 0..BATCHES {
            store
                .append_durable(&[report(
                    "api",
                    start + i,
                    "a",
                    &["leaf", "mid", "root"],
                    10 + i,
                )])
                .unwrap();
            // Seal every upload so there are 40 independent segments to share,
            // rather than one file sized above the floor.
            store.wal.seal();
        }
        assert_eq!(
            store.wal_stats().1,
            BATCHES as usize,
            "one segment per sealed upload"
        );

        let conn_a = store.conn.lock().try_clone().unwrap();
        let conn_b = store.conn.lock().try_clone().unwrap();
        let one = store.clone();
        let two = store.clone();
        // `Connection` is `Send` but not `Sync`, so each thread owns its clone —        // which is exactly how the converters are wired.
        std::thread::scope(|scope| {
            let a = scope.spawn(move || one.convert_ready_on(&conn_a, 100));
            let b = scope.spawn(move || two.convert_ready_on(&conn_b, 100));
            for result in [a.join().unwrap(), b.join().unwrap()] {
                result.expect("a converter failed");
            }
        });

        // Every report landed exactly once: the row count and the value sum are
        // the invariant that a double publish would break.
        let expected_value: u64 = (0..BATCHES).map(|i| 10 + i).sum();
        assert_eq!(store.stats().batch_count, BATCHES as usize);
        assert_eq!(store.stats().total_value, expected_value);
        assert_eq!(store.wal_stats().1, 0, "segments left in the queue");
        assert_eq!(store.wal_stats().0, 0, "bytes left in the log");

        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A segment claimed by a converter that then died must be replayed.
    #[test]
    fn a_claimed_segment_returns_to_the_queue_after_a_crash() {
        let dir =
            std::env::temp_dir().join(format!("lidlesseye-store-claim-{}", uuid::Uuid::new_v4()));
        let mut cfg = StoreConfig::new(&dir);
        cfg.wal_segment_bytes = Some(64 * 1024);
        cfg.wal_sync_interval_ms = Some(0);
        let start = crate::time::now_ns() - 60_000_000_000;

        let store = Store::open(cfg.clone()).unwrap();
        store
            .append_durable(&[report("api", start, "a", &["leaf"], 77)])
            .unwrap();
        store.wal.seal();
        let claimed = store.wal.claim_next().expect("a segment to claim");
        drop(store);

        let reopened = Store::open(cfg).unwrap();
        assert_eq!(reopened.wal_stats().1, 1, "the claim was not recovered");
        let stats = reopened.convert_pending().unwrap();
        assert_eq!(stats.segments, 1);
        assert_eq!(reopened.stats().batch_count, 1);
        assert_eq!(reopened.stats().total_value, 77);
        let _ = claimed;
        drop(reopened);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An upload accepted into the log must survive a restart.
    ///
    /// This is the case the old in-memory hot tier could not do at all: kill
    /// the process between two flushes and the samples were simply gone. The
    /// segment is the record, and a reopened store converts it before it is
    /// ever served.
    #[test]
    fn a_segment_written_before_a_restart_is_converted_after_it() {
        let dir =
            std::env::temp_dir().join(format!("lidlesseye-store-wal-{}", uuid::Uuid::new_v4()));
        let mut cfg = StoreConfig::new(&dir);
        cfg.wal_sync_interval_ms = Some(0);
        let start = crate::time::now_ns() - 60_000_000_000;

        let store = Store::open(cfg.clone()).unwrap();
        let appended = store
            .append_durable(&[report("api", start, "a", &["leaf", "mid"], 42)])
            .unwrap();
        assert_eq!(appended.reports, 1);
        // No converter and no flush: this is the crash window.
        drop(store);
        assert!(dir.join("wal").read_dir().unwrap().count() > 0);

        let reopened = Store::open(cfg).unwrap();
        assert_eq!(reopened.stats().batch_count, 0, "nothing converted yet");
        let converted = reopened.convert_pending().unwrap();
        assert_eq!(converted.segments, 1);
        assert_eq!(converted.reports, 1);
        assert_eq!(reopened.stats().batch_count, 1);
        let fg = crate::query::flamegraph(
            &reopened,
            &crate::QueryOptions::new(
                ProfileType::cpu(),
                start,
                start + 1_000,
                crate::Selector::default(),
            ),
        )
        .unwrap();
        assert_eq!(fg.total, 42, "the replayed upload is queryable");
        assert!(reopened.wal_stats().1 == 0, "the segment was retired");
        drop(reopened);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Retention must not delete the unsharded frame dictionary.
    ///
    /// `frames` has no time column, so its files are filed under the `hour=0`
    /// sentinel. Read as an hour that is 1970, and the first retention pass
    /// deleted the whole dictionary while the samples that referenced it
    /// stayed —the flamegraph collapsed to a root node with no children and
    /// `top` returned nothing, which looks like a broken query rather than
    /// missing data.
    #[test]
    fn retention_keeps_the_unsharded_frame_dictionary() {
        let dir = std::env::temp_dir().join(format!(
            "lidlesseye-store-frames-retention-{}",
            uuid::Uuid::new_v4()
        ));
        let mut cfg = StoreConfig::new(&dir);
        cfg.retention_ns = Some(3_600_000_000_000);
        let store = Store::open(cfg).unwrap();

        let start = crate::time::now_ns() - 60_000_000_000;
        store
            .append(&report("api", start, "a", &["leaf", "mid", "root"], 10))
            .unwrap();
        store.flush_hot().unwrap();

        let frames_dir = dir.join("parquet").join("frames").join("hour=0");
        let archived = || {
            std::fs::read_dir(&frames_dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|entry| entry.path().to_string_lossy().ends_with(".parquet"))
                        .count()
                })
                .unwrap_or(0)
        };
        assert!(archived() > 0, "the flush archived no frame file");

        store.prune(crate::time::now_ns()).unwrap();
        assert!(
            archived() > 0,
            "retention deleted the unsharded frame dictionary"
        );

        // The archived stack must still resolve to its frames.
        let opts = crate::query::QueryOptions::new(
            "cpu",
            start,
            start + 60_000_000_000,
            crate::selector::Selector::service("api"),
        );
        let graph = crate::query::flamegraph(&store, &opts).unwrap();
        assert!(graph.total > 0, "the archived stack no longer resolves");
        assert!(
            graph.names.iter().any(|name| name == "leaf"),
            "frames were not joined back onto the stack: {:?}",
            graph.names
        );

        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A retention pass must cut the tier into bounded pieces.
    ///
    /// Deleting everything past retention in one statement is what turned a
    /// catch-up into a minute-long job inside the writer queue: the delete
    /// dirtied enough of the database file that the checkpoint after it rewrote
    /// all of it, and every other producer —including the flush that would
    /// have made room —timed out behind it. The boundary has to advance the
    /// retained edge by at most `limit` rows per pass, and leave the rest for
    /// the next tick.
    #[test]
    fn a_retention_pass_cuts_the_tier_into_bounded_pieces() {
        let (store, dir) = temp_store("prune-bounded");
        for i in 0..5u64 {
            store
                .append(&report("svc", 1_000 + i, "a", &["hot"], 10 + i))
                .unwrap();
        }
        let conn = store.lock_conn(ConnUser::Maintenance);

        // Nothing is expired below the oldest row: nothing to cut.
        let none = expired_cutoff(&conn, "batches_hot", 1_000, 2).unwrap();
        assert_eq!(none, None, "no row is older than the cutoff");

        // Everything is expired: the boundary is the second-oldest `end_ns`,
        // and rows *at* the boundary are included —that is what lets the last
        // pass finish instead of stalling on the newest expired row.
        let boundary = expired_cutoff(&conn, "batches_hot", u64::MAX, 2)
            .unwrap()
            .expect("rows are expired");
        let oldest: Vec<i64> = conn
            .prepare("SELECT end_ns FROM batches_hot ORDER BY end_ns LIMIT 2")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(
            boundary, oldest[1] as u64,
            "the boundary is the limit-th oldest row"
        );

        let removed = conn
            .execute(
                "DELETE FROM batches_hot WHERE end_ns <= ?",
                params![ts(boundary)],
            )
            .unwrap();
        assert_eq!(removed, 2, "one pass removes at most `limit` rows");
        // A second pass over what is left must find a boundary again: a stale
        // one would mean retention never finishes.
        assert!(
            expired_cutoff(&conn, "batches_hot", u64::MAX, 2)
                .unwrap()
                .is_some(),
            "the remaining rows are still expired"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The writer thread is the only thing that touches the writer connection,
    /// and every caller that queues gets an answer. If that hand-off lost a job,
    /// or answered before the commit, an agent would be told "stored" about a
    /// batch that is not there —the exact failure a queue has to avoid.
    #[test]
    fn the_writer_thread_commits_every_queued_report() {
        let dir =
            std::env::temp_dir().join(format!("lidlesseye-store-writer-{}", uuid::Uuid::new_v4()));
        // Flush on every tick: the cadence guard would otherwise answer "not
        // due" and the test would never see the queued pass run.
        let mut cfg = StoreConfig::new(&dir);
        cfg.flush_interval_ns = Some(1);
        let store = Store::open_shared(cfg).unwrap();

        let threads: Vec<_> = (0..4)
            .map(|_worker| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for i in 0..10u64 {
                        let start = 1_000_000_000 + i * 100_000_000;
                        store
                            .append_owned(report("svc", start, "a", &["hot"], 3))
                            .unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(store.stats().batch_count, 40, "every queued batch landed");

        // A background pass is a job in the same queue, so it cannot starve and
        // cannot be starved: it runs to completion between two ingests.
        let moved = store.flush_hot_when_idle().unwrap();
        assert!(moved > 0, "the queued flush ran");
        assert_eq!(
            store.stats().batch_count,
            40,
            "a flush moves rows between tiers; nothing is lost"
        );

        drop(store);
        let _ = std::fs::remove_dir_all(dir);
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
    /// footer in the archive —the 60-second timeouts that started this work.
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
    /// rules that are easy to get wrong —counters add, a gauge does not, and
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

    /// The performance settings must actually take effect —a silently
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

    /// The interrupt must stop a running read from another thread —this is
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
    /// `Connection::open_in_memory()`, which ignores the config object —so
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
    /// read has finished —otherwise a late tick lands on the next ingest.
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

    /// Ids must keep increasing after a tenant with several archived hours is
    /// reopened. Finding the counter by opening only the newest shard is what
    /// keeps a cold open fast; this test makes sure that shortcut is exact.
    #[test]
    fn ids_keep_increasing_after_a_multi_hour_reopen() {
        let (store, dir) = temp_store("ids-multi-hour");
        let hour = 3_600_000_000_000u64;
        let first = store
            .append(&report("api", 1_000, "a", &["hot"], 10))
            .unwrap();
        let second = store
            .append(&report("api", 1_000 + hour, "a", &["hot"], 10))
            .unwrap();
        store.flush_hot().unwrap();
        assert!(first.batch_id < second.batch_id);
        drop(store);

        let reopened = Store::open(StoreConfig::new(&dir)).unwrap();
        assert_eq!(reopened.stats().batch_count, 2);
        let third = reopened
            .append(&report("api", 1_000 + 2 * hour, "a", &["hot"], 10))
            .unwrap();
        assert!(
            third.batch_id > second.batch_id,
            "the reopened store reused an archived batch id"
        );
        drop(reopened);
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
