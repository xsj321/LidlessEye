//! Server configuration.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use lidlesseye_core::{S3Config, StorageMode, TenantId, TenantLimits};

/// HTTP server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Address to listen on.
    pub listen: SocketAddr,
    /// Directory holding the databases. Each tenant gets one
    /// `lidlesseye.duckdb` under `<data_dir>/tenants/`.
    pub data_dir: PathBuf,
    /// Delete profiles older than this. Per-tenant overrides live in
    /// [`ServerConfig::tenant_limits`].
    pub retention: Option<Duration>,
    /// Soft cap for the estimated size of retained batches.
    pub max_disk_bytes: Option<u64>,
    /// Maximum accepted request body, in bytes.
    pub max_body_bytes: usize,
    /// When set, `/ingest` and `/api` require `Authorization: Bearer <token>`.
    pub auth_token: Option<String>,
    /// Per-tenant bearer tokens. When non-empty, authentication is mandatory
    /// and the token decides which tenant the request belongs to.
    pub tenant_tokens: BTreeMap<String, String>,
    /// Request header carrying the tenant id (`X-Scope-OrgID` by default, the
    /// same one Pyroscope and Grafana's datasource send).
    pub tenant_header: String,
    /// When set, only these tenants may be used. `None` lets the first write
    /// create any valid tenant id.
    pub allowed_tenants: Option<Vec<String>>,
    /// Maximum number of tenants on this server.
    pub max_tenants: usize,
    /// Per-tenant storage limits, keyed by tenant id.
    pub tenant_limits: BTreeMap<String, TenantLimits>,
    /// Reject writes (useful for read-only replicas).
    pub read_only: bool,
    /// Value of `Access-Control-Allow-Origin`; `None` disables CORS.
    pub cors_allow_origin: Option<String>,
    /// How often retention/eviction runs.
    pub prune_interval: Duration,
    /// How often the on-disk check walks the Parquet tree. Defaults to an hour:
    /// it exists to notice a slow disappearance, and it competes with nothing.
    pub fsck_interval: Duration,
    /// Worker threads DuckDB may use per connection (i.e. per tenant).
    /// Defaults to 4: on hosts with many cores DuckDB's thread coordination
    /// costs more than it saves on the GROUP BY scans behind flamegraphs and
    /// the service map, and 4 threads kept the same query ~6x faster. Set to
    /// `null` in config to let DuckDB pick every core.
    #[serde(default = "default_duckdb_threads")]
    pub duckdb_threads: Option<usize>,
    /// DuckDB memory limit per tenant connection, e.g. "1GB", "512MB".
    /// Caps the hash tables behind long scans so wide ranges spill to disk
    /// instead of OOM-killing the pod. `None` uses a conservative 1 GiB.
    #[serde(default)]
    pub duckdb_memory_limit: Option<String>,
    /// How many store reads may be in flight at once.
    ///
    /// Reads are handed to blocking threads, so an unbounded number of them
    /// costs a blocking thread each while they queue on the one DuckDB
    /// connection, and the pile answers nothing. Bounding the number keeps the
    /// queue in async land, where waiting is nearly free and the runtime stays
    /// able to accept connections and answer `/api/v1/health` — the probe
    /// kubelet kills the process over when it cannot be served.
    #[serde(default)]
    pub query_concurrency: Option<usize>,
    /// Physical storage backend: `disk` (local Parquet, the default) or `s3`
    /// (verified S3 archive). `None` means the canonical `disk` mode.
    #[serde(default)]
    pub storage_mode: Option<StorageMode>,
    /// S3-compatible object store configuration for Parquet mode.
    #[serde(default)]
    pub s3: Option<S3Config>,
    /// How long compacted Parquet stays on local disk before S3 upload/removal.
    #[serde(default)]
    pub parquet_local_ttl_ns: Option<u64>,
    /// How often the in-memory hot tier is flushed to Parquet (nanoseconds).
    /// This is the maximum data-loss window on an unclean shutdown.
    #[serde(default)]
    pub storage_flush_interval_ns: Option<u64>,
    /// Acknowledge an ingest before it reaches DuckDB (default) or after.
    ///
    /// `async` appends the upload to the segment log and answers 202; the
    /// converter turns segments into Parquet within a second or two. `sync`
    /// waits for the commit, which is what tests and `curl` scripts want.
    #[serde(default)]
    pub ingest_mode: lidlesseye_core::IngestMode,
    /// Roll the active segment file at this size (bytes).
    #[serde(default)]
    pub wal_segment_bytes: Option<u64>,
    /// Segment log group-commit interval in milliseconds. This is the maximum
    /// data loss on an unclean shutdown.
    #[serde(default)]
    pub wal_sync_interval_ms: Option<u64>,
    /// Refuse uploads above this many bytes of unconverted segment log.
    #[serde(default)]
    pub wal_max_pending_bytes: Option<u64>,
    /// Seal the active segment once it is this old, so a quiet tenant's upload
    /// becomes queryable without waiting for the next one.
    #[serde(default)]
    pub wal_segment_age_ms: Option<u64>,
    /// Converter threads per tenant. DuckDB documents that concurrent appends
    /// never conflict, so segments can be decoded and inserted in parallel;
    /// `None` derives a small number from the available cores.
    #[serde(default)]
    pub wal_converters: Option<usize>,
}

fn default_duckdb_threads() -> Option<usize> {
    // Four threads scan a 30-day, 40M-row Parquet set about twice as fast as
    // two. It needs a 4 GiB pod: each thread keeps its own decode buffers.
    // Small deployments can lower it with `--duckdb-threads=2`.
    Some(4)
}

fn default_query_concurrency() -> Option<usize> {
    // A DuckDB connection is single-writer and serialises its own readers, so
    // more than a handful of concurrent reads buys nothing but queue depth.
    // The dashboard asks for about six things at once; four lets the cheap
    // ones through while the slow one runs.
    Some(4)
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            listen: "0.0.0.0:4040".parse().expect("valid default address"),
            data_dir: PathBuf::from("./lidlesseye-data"),
            retention: None,
            max_disk_bytes: None,
            max_body_bytes: 64 * 1024 * 1024,
            auth_token: None,
            tenant_tokens: BTreeMap::new(),
            tenant_header: "X-Scope-OrgID".to_string(),
            allowed_tenants: None,
            max_tenants: 1000,
            tenant_limits: BTreeMap::new(),
            read_only: false,
            cors_allow_origin: None,
            prune_interval: Duration::from_secs(60),
            fsck_interval: Duration::from_secs(3600),
            duckdb_threads: default_duckdb_threads(),
            duckdb_memory_limit: None,
            query_concurrency: default_query_concurrency(),
            storage_mode: None,
            s3: None,
            parquet_local_ttl_ns: None,
            storage_flush_interval_ns: None,
            ingest_mode: lidlesseye_core::IngestMode::Async,
            wal_segment_bytes: None,
            wal_sync_interval_ms: None,
            wal_max_pending_bytes: None,
            wal_segment_age_ms: None,
            wal_converters: None,
        }
    }
}

impl ServerConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        ServerConfig {
            data_dir: data_dir.into(),
            ..Default::default()
        }
    }

    pub fn with_listen(mut self, listen: impl Into<String>) -> Result<Self, String> {
        self.listen = listen
            .into()
            .parse()
            .map_err(|err| format!("invalid listen address: {err}"))?;
        Ok(self)
    }

    /// Choose how an ingest is acknowledged.
    pub fn ingest_mode(mut self, mode: lidlesseye_core::IngestMode) -> Self {
        self.ingest_mode = mode;
        self
    }

    /// Segment log size, group-commit interval and backlog cap.
    pub fn wal(
        mut self,
        segment_bytes: Option<u64>,
        sync_interval_ms: Option<u64>,
        max_pending_bytes: Option<u64>,
    ) -> Self {
        self.wal_segment_bytes = segment_bytes;
        self.wal_sync_interval_ms = sync_interval_ms;
        self.wal_max_pending_bytes = max_pending_bytes;
        self
    }

    /// How long a segment may stay open before the converter picks it up.
    pub fn wal_segment_age(mut self, millis: u64) -> Self {
        self.wal_segment_age_ms = Some(millis);
        self
    }

    /// Number of converter threads per tenant.
    pub fn wal_converters(mut self, count: Option<usize>) -> Self {
        self.wal_converters = count;
        self
    }

    pub fn with_retention(mut self, retention: Duration) -> Self {
        self.retention = Some(retention);
        self
    }

    /// Bind a token to a tenant. Implies authentication is required.
    pub fn with_tenant_token(mut self, token: impl Into<String>, tenant: &str) -> Self {
        self.tenant_tokens.insert(token.into(), tenant.to_string());
        self
    }

    /// Parsed tenant limits, skipping ids that are not valid.
    pub fn parsed_tenant_limits(&self) -> BTreeMap<TenantId, TenantLimits> {
        self.tenant_limits
            .iter()
            .filter_map(|(id, limits)| TenantId::parse(id).ok().map(|id| (id, limits.clone())))
            .collect()
    }

    /// Parsed allow-list, if one is configured.
    pub fn parsed_allowed_tenants(&self) -> Option<Vec<TenantId>> {
        self.allowed_tenants.as_ref().map(|list| {
            list.iter()
                .filter_map(|id| TenantId::parse(id).ok())
                .collect()
        })
    }

    /// Number of store reads allowed to run at once.
    ///
    /// `0` is treated as "no limit" rather than "no reads": a zero-sized
    /// semaphore would reject every query, which is never what an operator
    /// means by disabling the cap.
    pub fn query_concurrency(&self) -> Option<usize> {
        self.query_concurrency.filter(|value| *value > 0)
    }
}
