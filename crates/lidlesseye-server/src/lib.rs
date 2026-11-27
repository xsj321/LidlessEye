//! LidlessEye server: ingest API, query API and the embedded web UI.
//!
//! ```no_run
//! use lidlesseye_server::ServerConfig;
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let server = lidlesseye_server::LidlessEye::bind(ServerConfig::new("./data")).await?;
//! println!("listening on {}", server.local_addr());
//! server.serve().await?;
//! # Ok(())
//! # }
//! ```

mod api;
mod config;
mod capabilities;
mod cache;
mod lease;
mod dedupe;
mod fsck;
mod processes;
mod coverage;
mod directives;
mod error;
mod metrics;
mod params;
mod ui;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderMap, header};
use axum::routing::{get, post};
use serde::Serialize;
use tokio::net::TcpListener;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::decompression::RequestDecompressionLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing::{debug, info, warn};

use lidlesseye_core::model::Report;
use lidlesseye_core::store::{StorageMode, StoreConfig, StoreLockStats, StoreStats};
use lidlesseye_core::{BatchMeta, Store, TenantId, TenantStore};

pub use config::ServerConfig;
pub use error::ApiError;
pub use metrics::Metrics;

/// Cheaply cloneable server state.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    registry: Arc<TenantStore>,
    /// Workloads whose runtime memory profiling the operator switched on.
    directives: Arc<directives::Directives>,
    /// The last coverage report each workload's native reader sent in: how many
    /// processes it saw, how many it is attached to, and what it skipped.
    coverage: Arc<coverage::Coverage>,
    /// The last capability report each workload's readers sent in. A reader
    /// that is enabled but not active says so here, with a reason.
    capabilities: Arc<capabilities::Capabilities>,
    /// Batch ids already accepted, so a replayed delivery does not count twice.
    dedupe: Arc<dedupe::Dedupe>,
    /// The last on-disk check, and the file set it compared against.
    fsck: Arc<fsck::Fsck>,
    /// Short-lived answers for the read endpoints.
    pub cache: Arc<cache::Cache>,
    /// Which replica may write. See [`lease`].
    pub lease: Arc<lease::Lease>,
    /// Latest per-process memory report per (workload, pod).
    processes: Arc<processes::Processes>,
    cfg: ServerConfig,
    metrics: Arc<Metrics>,
    started: Instant,
    allowed_tenants: Option<Vec<TenantId>>,
    /// Per-tenant counters for `/api/v1/stats`.
    ///
    /// [`Metrics`] stays process-global because `/metrics` is scraped by the
    /// operator, not by a tenant. A tenant-scoped caller asking for its own
    /// stats, however, must not be handed another tenant's ingest volume, so
    /// the numbers it sees are kept per tenant.
    activity: Mutex<BTreeMap<String, TenantActivity>>,
    /// Caps how many store reads are running (or waiting to run) at once.
    ///
    /// `None` disables the cap. See [`ServerConfig::query_concurrency`] for
    /// why a cap exists at all.
    reads: Option<Arc<tokio::sync::Semaphore>>,
}

/// Ingestion and query counters for a single tenant.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct TenantActivity {
    pub ingest_reports: u64,
    pub ingest_samples: u64,
    pub queries: u64,
}

/// Who is calling, and on which tenant.
#[derive(Debug, Clone)]
pub struct Caller {
    /// Tenant the request is scoped to.
    pub tenant: TenantId,
    /// True when the credential itself pins the tenant (a per-tenant token),
    /// in which case cross-tenant administration is not allowed.
    pub scoped: bool,
}

impl AppState {
    pub fn registry(&self) -> &Arc<TenantStore> {
        &self.inner.registry
    }

    /// Counters for one tenant; unknown tenants read as zero.
    pub fn activity(&self, tenant: &TenantId) -> TenantActivity {
        self.inner
            .activity
            .lock()
            .ok()
            .and_then(|map| map.get(tenant.as_str()).copied())
            .unwrap_or_default()
    }

    fn record_ingest(&self, tenant: &TenantId, reports: u64, samples: u64) {
        if let Ok(mut map) = self.inner.activity.lock() {
            let entry = map.entry(tenant.to_string()).or_default();
            entry.ingest_reports += reports;
            entry.ingest_samples += samples;
        }
    }

    pub fn config(&self) -> &ServerConfig {
        &self.inner.cfg
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.inner.metrics
    }

    pub fn uptime(&self) -> Duration {
        self.inner.started.elapsed()
    }

    /// Authenticate a request and resolve the tenant it may touch.
    ///
    /// * No authentication configured: open access, tenant from the header.
    /// * Global bearer token: any tenant, chosen by the header.
    /// * Per-tenant token: the token pins the tenant and a conflicting header
    ///   is rejected, so a tenant can never read another one.
    pub fn authorize(&self, headers: &HeaderMap) -> Result<Caller, ApiError> {
        let cfg = &self.inner.cfg;
        let bound = self.bound_tenant(headers)?;

        // An explicitly empty header is a malformed request, not "use the
        // default tenant": silently writing into `default` is exactly the kind
        // of cross-tenant surprise this feature exists to prevent.
        //
        // Repeating the header with different values is refused rather than
        // resolved: a proxy that appends `X-Scope-OrgID` would otherwise let
        // whichever value the map happens to return decide the tenant.
        let mut seen: Vec<&str> = Vec::new();
        for name in [cfg.tenant_header.as_str(), "x-tenant-id"] {
            for value in headers.get_all(name).iter() {
                let value = value
                    .to_str()
                    .map_err(|_| ApiError::bad_request(format!("`{name}` must be ASCII")))?;
                let value = value.trim();
                if value.is_empty() {
                    return Err(ApiError::bad_request(format!(
                        "`{name}` is present but empty"
                    )));
                }
                if !seen.contains(&value) {
                    seen.push(value);
                }
            }
        }
        let requested = match seen.len() {
            0 => None,
            1 => Some(seen[0].to_string()),
            _ => {
                return Err(ApiError::bad_request(format!(
                    "conflicting tenant headers: {}",
                    seen.join(", ")
                )));
            }
        };

        let tenant = match (&bound, requested) {
            (Some(bound), Some(requested)) => {
                let requested = TenantId::parse(&requested)?;
                if &requested != bound {
                    return Err(ApiError::forbidden(format!(
                        "this token is scoped to tenant `{bound}`"
                    )));
                }
                bound.clone()
            }
            (Some(bound), None) => bound.clone(),
            (None, Some(requested)) => TenantId::parse(&requested)?,
            (None, None) => TenantId::default_tenant(),
        };

        if let Some(allowed) = &self.inner.allowed_tenants
            && !allowed.contains(&tenant)
        {
            return Err(ApiError::forbidden(format!(
                "tenant `{tenant}` is not allowed on this server"
            )));
        }

        Ok(Caller {
            tenant,
            scoped: bound.is_some(),
        })
    }

    /// Authenticate without resolving a tenant.
    ///
    /// Returns the tenant a bearer token pins the caller to, if any, and does
    /// nothing else: no tenant header is read and `--allowed-tenant` is not
    /// applied. This exists for the one endpoint that is about *which* tenants
    /// exist — a catalogue that demanded the caller name a valid tenant first
    /// could not be read at all on a server whose allowlist does not contain
    /// the caller's default, which is exactly the state the dashboard starts in.
    pub fn authorize_catalog(&self, headers: &HeaderMap) -> Result<Option<TenantId>, ApiError> {
        self.bound_tenant(headers)
    }

    /// The token check alone: `None` for an open or globally-tokened server,
    /// `Some(tenant)` when a per-tenant token matched.
    fn bound_tenant(&self, headers: &HeaderMap) -> Result<Option<TenantId>, ApiError> {
        let cfg = &self.inner.cfg;
        if cfg.auth_token.is_none() && cfg.tenant_tokens.is_empty() {
            return Ok(None);
        }
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default()
            .trim();

        for (candidate, tenant) in &cfg.tenant_tokens {
            if constant_time_eq(token, candidate) {
                let tenant = TenantId::parse(tenant)
                    .map_err(|err| ApiError::internal(format!("bad tenant token: {err}")))?;
                return Ok(Some(tenant));
            }
        }
        let global = cfg
            .auth_token
            .as_deref()
            .is_some_and(|expected| constant_time_eq(token, expected));
        if global {
            Ok(None)
        } else {
            Err(ApiError::unauthorized())
        }
    }

    /// Store of a tenant that must already exist (read path).
    pub fn tenant_store(&self, tenant: &TenantId) -> Result<Arc<Store>, ApiError> {
        self.inner.registry.tenant(tenant).ok_or_else(|| {
            ApiError::not_found(format!(
                "tenant `{tenant}` has no data; `GET /api/v1/tenants` lists the known ones"
            ))
        })
    }

    /// Store of a tenant, creating it when needed (write path).
    ///
    /// Blocking: opening a cold tenant in `s3` mode lists its prefix and reads
    /// every archive footer, which is tens of seconds. Only call this off an
    /// async handler — see [`Self::writable_tenant_async`].
    pub fn writable_tenant(&self, tenant: &TenantId) -> Result<Arc<Store>, ApiError> {
        if self.inner.cfg.read_only {
            return Err(ApiError::forbidden("server is read-only"));
        }
        Ok(self.inner.registry.open_tenant(tenant)?)
    }

    /// [`Self::writable_tenant`] for async handlers, run on the blocking pool.
    ///
    /// The first write to a cold tenant opens its store. Doing that inline
    /// parks the Tokio worker for the whole open; a restarted agent sends a
    /// burst of ingests, so every worker can end up parked and the liveness
    /// probe times out — Kubernetes then kills a process that is only slow.
    /// The open is pure blocking work with no reactor use, so it belongs on the
    /// blocking pool, where concurrent callers queue on the tenant's open lock
    /// instead of on the runtime.
    pub async fn writable_tenant_async(&self, tenant: &TenantId) -> Result<Arc<Store>, ApiError> {
        if self.inner.cfg.read_only {
            return Err(ApiError::forbidden("server is read-only"));
        }
        let registry = self.inner.registry.clone();
        let tenant = tenant.clone();
        tokio::task::spawn_blocking(move || registry.open_tenant(&tenant))
            .await
            .map_err(|err| ApiError::internal(format!("opening tenant: {err}")))?
            .map_err(ApiError::from)
    }
}

/// Watchdog for one read query.
///
/// The handler holds this for the lifetime of the request; if the query is
/// still running after [`QueryGuard::run`]'s timeout, DuckDB is interrupted so
/// a single stuck scan cannot pin a read slot until the HTTP deadline.
///
/// It deliberately does *not* interrupt when the handler is dropped. That used
/// to be the "the client went away, stop scanning" path, but an interrupt is
/// per store, not per request: it aborts every reader on that tenant. Several
/// controls legitimately query the same tenant at once (map, histogram,
/// stats), and aborting one request — a browser cancel, a re-render, a
/// superseded fetch — killed the sibling that had just started. The dashboard
/// showed `INTERRUPT Error: Interrupted!` instead of a flamegraph. A query
/// nobody is waiting for now runs to completion, or to the timeout below.
pub struct QueryGuard {
    store: Arc<Store>,
}

impl QueryGuard {
    /// Run a blocking query, interrupting it if it exceeds the safety timeout.
    pub async fn run<T, F>(&self, query: F) -> T
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

        let watchdog_store = self.store.clone();
        let watchdog = tokio::spawn(async move {
            tokio::time::sleep(QUERY_TIMEOUT).await;
            // Only the queries that have already run longer than any request
            // will wait for. Interrupting the whole pool here used to cancel the
            // map and the histogram because the flamegraph beside them was slow,
            // and the browser reported that as `INTERRUPT Error: Interrupted!`
            // on a panel that had done nothing wrong.
            debug!("query exceeded timeout; interrupting reads older than the timeout");
            watchdog_store.interrupt_older_than(QUERY_TIMEOUT);
        });
        let started = Instant::now();
        let result = tokio::task::spawn_blocking(query).await;
        let elapsed = started.elapsed();
        watchdog.abort();
        if elapsed >= Duration::from_secs(1) {
            info!(
                stage = "query_exec",
                elapsed_ms = elapsed.as_millis(),
                "blocking query finished"
            );
        }
        result.unwrap_or_else(|err| std::panic::resume_unwind(err.into_panic()))
    }
}

/// What an ingest call did.
#[derive(Debug, Clone)]
pub enum IngestOutcome {
    /// Written to the segment log; the converter will turn it into Parquet.
    Queued(lidlesseye_core::wal::Appended),
    /// Committed to DuckDB before returning.
    Committed(Vec<Arc<BatchMeta>>),
}

impl AppState {
    /// Create a cancellation guard tied to one store connection. Hold it in
    /// the handler until the query result has been produced.
    pub fn query_guard(&self, store: Arc<Store>) -> QueryGuard {
        QueryGuard { store }
    }

    /// Take one of the slots a store read is allowed to occupy.
    ///
    /// The guard must be held until the read returns. Waiting happens in async
    /// land, so a queue of callers costs no threads; without the cap, each
    /// caller took a blocking thread with it and the queue became thousands of
    /// parked threads all contending for the single DuckDB connection.
    ///
    /// Bounded waiting is deliberate. A caller that has queued behind a slow
    /// scan for longer than [`READ_QUEUE_TIMEOUT`] is better served by a 503 it
    /// can retry than by the 60 s request timeout, which looks exactly like a
    /// broken server from the browser.
    pub async fn read_slot(&self) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, ApiError> {
        const READ_QUEUE_TIMEOUT: Duration = Duration::from_secs(15);
        let Some(semaphore) = &self.inner.reads else {
            return Ok(None);
        };
        let started = Instant::now();
        let outcome =
            tokio::time::timeout(READ_QUEUE_TIMEOUT, semaphore.clone().acquire_owned()).await;
        let waited = started.elapsed();
        if waited >= Duration::from_millis(100) {
            info!(
                stage = "read_slot_wait",
                wait_ms = waited.as_millis(),
                "waited for a query read slot"
            );
        }
        match outcome {
            Ok(Ok(permit)) => Ok(Some(permit)),
            Ok(Err(_)) => Err(ApiError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "server is shutting down",
            )),
            Err(_) => Err(ApiError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "server is busy: every read slot was in use for {}s. Narrow the time \
                     range, or raise `--query-concurrency` if this deployment has the CPU \
                     and memory for more parallel scans.",
                    READ_QUEUE_TIMEOUT.as_secs()
                ),
            )),
        }
    }

    /// Persist several reports of one tenant.
    ///
    /// In [`IngestMode::Async`] the upload goes to the segment log and the
    /// answer is `Queued` the moment it is written — no DuckDB, no queue, no
    /// writer latency. In [`IngestMode::Sync`] it is committed before the
    /// answer, which is what tests and ad-hoc `curl` uploads need.
    pub async fn ingest_many(
        &self,
        tenant: &TenantId,
        reports: Vec<Report>,
    ) -> Result<IngestOutcome, ApiError> {
        self.ingest_many_with(tenant, reports, false).await
    }

    /// [`AppState::ingest_many`], optionally waiting for the `fsync`.
    ///
    /// `durable` is what `?sync=1` asks for: the upload is already accepted
    /// either way, but a caller that is about to kill the process — a test, a
    /// script, a pprof push — can ask for the bytes to be on the platter first.
    pub async fn ingest_many_with(
        &self,
        tenant: &TenantId,
        reports: Vec<Report>,
        durable: bool,
    ) -> Result<IngestOutcome, ApiError> {
        if self.inner.cfg.read_only {
            return Err(ApiError::forbidden("server is read-only"));
        }
        let store = self.writable_tenant_async(tenant).await?;
        if store.config().ingest_mode == lidlesseye_core::IngestMode::Async {
            // One encoded append; the fsync happens on its own thread. Only the
            // log is touched, so a flush or a merge can no longer delay an
            // agent's upload.
            let appended = tokio::task::spawn_blocking(move || {
                let appended = store.append_durable(&reports)?;
                if durable {
                    let appended = lidlesseye_core::wal::Appended {
                        synced: store.await_durable(&appended, std::time::Duration::from_secs(5)),
                        ..appended
                    };
                    return Ok::<_, lidlesseye_core::Error>(appended);
                }
                Ok(appended)
            })
            .await
            .map_err(|err| ApiError::internal(format!("ingest task failed: {err}")))??;
            self.inner
                .metrics
                .ingest_reports
                .fetch_add(appended.reports as u64, Ordering::Relaxed);
            return Ok(IngestOutcome::Queued(appended));
        }
        self.ingest_committed_with(store, tenant, reports)
            .await
            .map(IngestOutcome::Committed)
    }

    /// Commit several reports before returning. Used by the synchronous mode
    /// and by callers that bypass HTTP and need a batch id back.
    pub async fn ingest_committed(
        &self,
        tenant: &TenantId,
        reports: Vec<Report>,
    ) -> Result<Vec<Arc<BatchMeta>>, ApiError> {
        let store = self.writable_tenant_async(tenant).await?;
        self.ingest_committed_with(store, tenant, reports).await
    }

    async fn ingest_committed_with(
        &self,
        store: Arc<Store>,
        tenant: &TenantId,
        reports: Vec<Report>,
    ) -> Result<Vec<Arc<BatchMeta>>, ApiError> {
        if self.inner.cfg.read_only {
            return Err(ApiError::forbidden("server is read-only"));
        }
        // In S3 mode local Parquet files are unconfirmed staging copies. If
        // the archive cannot drain and the staging area exceeds the hard cap,
        // refuse new data instead of deleting the only copy to make room.
        if store.config().storage_mode.canonical() == StorageMode::S3
            && let Some(limit) = store.config().max_disk_bytes
            && store.staged_bytes() > limit
        {
            return Err(ApiError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "S3 staging is full ({} > {limit} bytes); \
                     wait for the archive backlog to drain",
                    store.staged_bytes()
                ),
            ));
        }
        let limits = self.inner.registry.limits(tenant);
        let max_series = limits.max_series;
        let tenant_label = tenant.to_string();

        let metrics = self.inner.metrics.clone();
        let samples: u64 = reports.iter().map(|r| r.stacks.len() as u64).sum();
        let result = tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
            // Enforce the per-tenant series limit before writing anything, so a
            // rejected batch leaves no partial state behind. This lives in the
            // blocking task with the writes: `contains_series` is one query per
            // report, and running it on the handler's thread put a batch of
            // fifty reports between the runtime and its next poll.
            if let Some(max_series) = max_series {
                let current = store.series_count();
                let fresh = distinct_new_series(&store, &reports);
                if current.saturating_add(fresh) > max_series {
                    return Err(ApiError::new(
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        format!(
                            "tenant `{tenant_label}` is at its series limit ({max_series}); \
                             raise `tenant_limits.{tenant_label}.max_series` to accept more"
                        ),
                    ));
                }
            }
            let mut metas = Vec::with_capacity(reports.len());
            let mut errors = Vec::new();
            // One transaction for the whole upload: a few hundred reports used
            // to mean a few hundred commits, and the writer never caught up.
            for result in store.append_owned_batch(reports) {
                match result {
                    Ok(meta) => {
                        metrics.ingest_reports.fetch_add(1, Ordering::Relaxed);
                        metas.push(meta);
                    }
                    Err(err) => errors.push(err.to_string()),
                }
            }
            Ok((metas, errors))
        })
        .await
        .map_err(|err| ApiError::internal(format!("ingest task failed: {err}")))??;

        let (metas, errors) = result;
        if !metas.is_empty() {
            self.record_ingest(tenant, metas.len() as u64, samples);
        }
        self.inner
            .metrics
            .ingest_samples
            .fetch_add(samples, Ordering::Relaxed);
        self.inner
            .metrics
            .batches_written
            .fetch_add(metas.len() as u64, Ordering::Relaxed);
        if !errors.is_empty() {
            self.inner
                .metrics
                .ingest_errors
                .fetch_add(errors.len() as u64, Ordering::Relaxed);
            if metas.is_empty() {
                return Err(ApiError::bad_request(errors.join("; ")));
            }
            warn!(
                rejected = errors.len(),
                accepted = metas.len(),
                "some reports were rejected"
            );
        }
        Ok(metas)
    }

    /// Persist one service-map window for the embedded agent.
    ///
    /// The in-process agent has no HTTP round trip to make; this is the same
    /// write the ingest endpoint performs, minus the serialisation.
    pub async fn ingest_servicemap(
        &self,
        tenant: &TenantId,
        batch: lidlesseye_core::ServiceMapBatch,
    ) -> Result<(), ApiError> {
        if self.inner.cfg.read_only {
            return Err(ApiError::forbidden("server is read-only"));
        }
        let store = self.writable_tenant_async(tenant).await?;
        tokio::task::spawn_blocking(move || store.append_servicemap(&batch))
            .await
            .map_err(|err| ApiError::internal(format!("service map task failed: {err}")))?
            .map_err(ApiError::from)
    }
}

/// Number of reports that would introduce a series the store has never seen.
fn distinct_new_series(store: &Store, reports: &[Report]) -> usize {
    let mut seen: Vec<(
        &str,
        &lidlesseye_core::ProfileType,
        BTreeMap<String, String>,
    )> = Vec::new();
    let mut fresh = 0usize;
    for report in reports {
        if store.contains_series(report) {
            continue;
        }
        let key = (
            report.service_name.as_str(),
            &report.profile_type,
            report.all_labels(),
        );
        if seen
            .iter()
            .any(|existing| existing.0 == key.0 && existing.1 == key.1 && existing.2 == key.2)
        {
            continue;
        }
        fresh += 1;
        seen.push((key.0, key.1, key.2));
    }
    fresh
}

/// Compare two secrets without leaking their length or content through timing.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut diff = (a.len() ^ b.len()) as u64;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as u64;
    }
    diff == 0
}

/// A running (or ready to run) LidlessEye HTTP server.
pub struct LidlessEye {
    state: AppState,
    listener: TcpListener,
}

impl LidlessEye {
    /// Open the data directory and bind the listener.
    pub async fn bind(cfg: ServerConfig) -> Result<Self, String> {
        let store_cfg = StoreConfig {
            root: cfg.data_dir.clone(),
            retention_ns: cfg.retention.map(|d| d.as_nanos() as u64),
            max_disk_bytes: cfg.max_disk_bytes,
            threads: cfg.duckdb_threads,
            duckdb_memory_limit: cfg.duckdb_memory_limit.clone(),
            storage_mode: cfg.storage_mode.unwrap_or_default(),
            s3: cfg.s3.clone(),
            parquet_local_ttl_ns: cfg.parquet_local_ttl_ns,
            flush_interval_ns: cfg.storage_flush_interval_ns,
            // Match the pool to the read-slot cap: a larger pool would just
            // queue on the semaphore instead, a smaller one would make the
            // semaphore pointless.
            read_connections: cfg.query_concurrency(),
            ingest_mode: cfg.ingest_mode,
            wal_segment_bytes: cfg.wal_segment_bytes,
            wal_sync_interval_ms: cfg.wal_sync_interval_ms,
            wal_max_pending_bytes: cfg.wal_max_pending_bytes,
            wal_segment_age_ms: cfg.wal_segment_age_ms,
            // The per-tenant cap moves into the store so the converter can
            // enforce it in one batch instead of one query per report.
            max_series: None,
            wal_converters: cfg.wal_converters,
        };
        let registry = TenantStore::open_with_limits(
            &cfg.data_dir,
            store_cfg,
            cfg.parsed_tenant_limits(),
            cfg.max_tenants,
        )
        .map_err(|err| format!("failed to open tenant registry: {err}"))?;
        // Do *not* compute every tenant's totals before binding. That is a
        // full scan of each tenant's `batches` (and, in canonical modes, of
        // every Parquet footer) and it runs before the socket can answer a
        // liveness probe: a ten-tenant installation spent the better part of a
        // minute here, so kubelet killed the container while it was still
        // starting. The catalogue warms itself on first request instead, and
        // `serve` kicks that off in the background.
        let tenants = registry.tenant_ids().len();
        let listener = TcpListener::bind(cfg.listen)
            .await
            .map_err(|err| format!("failed to bind {}: {err}", cfg.listen))?;
        info!(
            addr = %listener.local_addr().map(|a| a.to_string()).unwrap_or_default(),
            data_dir = %cfg.data_dir.display(),
            tenants,
            "store opened"
        );
        Ok(Self::from_registry_inner(Arc::new(registry), cfg, listener))
    }

    /// Build a server around an existing tenant registry (tests, embedding).
    pub fn from_registry(registry: Arc<TenantStore>, cfg: ServerConfig) -> Result<Self, String> {
        let listener = std::net::TcpListener::bind(cfg.listen)
            .map_err(|err| format!("failed to bind {}: {err}", cfg.listen))?;
        listener
            .set_nonblocking(true)
            .map_err(|err| format!("failed to configure listener: {err}"))?;
        let listener = TcpListener::from_std(listener)
            .map_err(|err| format!("failed to register listener: {err}"))?;
        Ok(Self::from_registry_inner(registry, cfg, listener))
    }

    fn from_registry_inner(
        registry: Arc<TenantStore>,
        cfg: ServerConfig,
        listener: TcpListener,
    ) -> Self {
        let allowed_tenants = cfg.parsed_allowed_tenants();
        let reads = cfg
            .query_concurrency()
            .map(|limit| Arc::new(tokio::sync::Semaphore::new(limit)));
        LidlessEye {
            state: AppState {
                inner: Arc::new(Inner {
                    registry,
                    directives: Arc::new(directives::Directives::load(&cfg.data_dir)),
                    coverage: Arc::new(coverage::Coverage::with_path(
                        cfg.data_dir.join("coverage.json"),
                    )),
            capabilities: Arc::new(capabilities::Capabilities::with_path(
                cfg.data_dir.join("capabilities.json"),
            )),
            dedupe: dedupe::Dedupe::new(),
            fsck: Arc::new(fsck::Fsck::default()),
            cache: Arc::new(cache::Cache::default()),
            lease: if cfg.read_only {
                // A replica that has decided not to write must not compete for
                // the lease: holding it while refusing writes would redirect
                // every write to itself.
                lease::Lease::passive(
                    std::env::var("LIDLESSEYE_POD_NAME")
                        .unwrap_or_else(|_| format!("lidlesseye-{}", std::process::id())),
                )
            } else {
                lease::Lease::open_with_addr(
                    &cfg.data_dir,
                    std::env::var("LIDLESSEYE_POD_NAME")
                        .unwrap_or_else(|_| format!("lidlesseye-{}", std::process::id())),
                    // The address writers are redirected to. Without a pod IP the
                    // record stays empty and a standby falls back to a plain 503.
                    std::env::var("LIDLESSEYE_POD_IP")
                        .map(|ip| format!("{ip}:{}", cfg.listen.port()))
                        .unwrap_or_default(),
                )
            },
            processes: Arc::new(processes::Processes::with_path(
                cfg.data_dir.join("processes.json"),
            )),
                    cfg,
                    metrics: Arc::new(Metrics::default()),
                    started: Instant::now(),
                    allowed_tenants,
                    activity: Mutex::new(BTreeMap::new()),
                    reads,
                }),
            },
            listener,
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap())
    }

    pub fn state(&self) -> AppState {
        self.state.clone()
    }

    /// Registry of every tenant.
    pub fn registry(&self) -> Arc<TenantStore> {
        self.state.inner.registry.clone()
    }

    /// Store of the default tenant, for callers that do not care about
    /// multi-tenancy.
    pub fn store(&self) -> Arc<Store> {
        self.state
            .writable_tenant(&TenantId::default_tenant())
            .expect("default tenant store")
    }

    /// The HTTP router, without a listener. Useful for tests and for mounting
    /// LidlessEye inside an existing axum application.
    pub fn router(&self) -> Router {
        build_router(self.state.clone())
    }

    /// Persist several reports, waiting for the commit.
    pub async fn ingest_committed(
        &self,
        tenant: &TenantId,
        reports: Vec<Report>,
    ) -> Result<Vec<Arc<BatchMeta>>, ApiError> {
        self.state.ingest_committed(tenant, reports).await
    }

    /// Persist a report for the default tenant, bypassing HTTP.
    pub async fn ingest(&self, report: Report) -> Result<Arc<BatchMeta>, ApiError> {
        self.ingest_for(&TenantId::default_tenant(), report).await
    }

    /// Persist a report for one tenant, bypassing HTTP.
    pub async fn ingest_for(
        &self,
        tenant: &TenantId,
        report: Report,
    ) -> Result<Arc<BatchMeta>, ApiError> {
        self.ingest_committed(tenant, vec![report])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::bad_request("no report supplied"))
    }

    /// Persist several reports for one tenant.
    pub async fn ingest_many(
        &self,
        tenant: &TenantId,
        reports: Vec<Report>,
    ) -> Result<IngestOutcome, ApiError> {
        self.state.ingest_many(tenant, reports).await
    }

    /// Run until the process is terminated.
    pub async fn serve(self) -> Result<(), String> {
        self.serve_with_shutdown(shutdown_signal()).await
    }

    /// Run until `shutdown` resolves.
    pub async fn serve_with_shutdown<F>(self, shutdown: F) -> Result<(), String>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let app = self.router();
        let addr = self.local_addr();
        let registry = self.state.inner.registry.clone();
        let cfg = self.state.inner.cfg.clone();
        let metrics = self.state.inner.metrics.clone();

        // The first request for an idle tenant pays for opening it: its archive
        // index, its connections and its id counters. On a tenant with real
        // history that is minutes of work, and it happens inside whichever
        // HTTP request arrives first, which then runs into the request timeout.
        // Open the tenants that already have directories in the background
        // instead — one at a time, so their connections never arrive as the
        // storm that on-demand loading exists to prevent, and strictly after
        // the listener is up, so a slow archive never delays a restart.
        let warm_registry = registry.clone();
        let inner_lease = self.state.inner.lease.clone();
        let _warm = tokio::spawn(async move {
            let ids = warm_registry.tenant_ids();
            if ids.is_empty() {
                return;
            }
            info!(tenants = ids.len(), "warming tenant stores in the background");
            for id in ids {
                let name = id.to_string();
                let registry = warm_registry.clone();
                let started = std::time::Instant::now();
                match tokio::task::spawn_blocking(move || registry.open_tenant(&id)).await {
                    Ok(Ok(_)) => info!(
                        tenant = %name,
                        elapsed_ms = started.elapsed().as_millis(),
                        "tenant store warm"
                    ),
                    Ok(Err(err)) => {
                        warn!(tenant = %name, error = %err, "could not warm a tenant store")
                    }
                    Err(err) => warn!(tenant = %name, error = %err, "tenant warm task failed"),
                }
            }
            info!("tenant stores warmed");
        });

        // This loop moves the hot tier to Parquet and, past the local TTL,
        // merges/uploads files. Its cadence must be at least as frequent as
        // the configured flush interval, otherwise the RPO is silently larger
        // than the value the operator chose.
        //
        // Tenants are opened on demand. A start-up warm pass would reopen all
        // of them — and with it their writers, readers and maintenance
        // connections — which is the connection storm on-demand loading exists
        // to avoid. Counting a tenant's archive happens on its first real use
        // instead, on the blocking pool rather than holding up the listener.
        // A replica that is not writing never refreshes its Parquet index: the
        // writer's index advances as a side effect of every flush, but a standby
        // (and a read-only query frontend) opens the tree once and would then
        // answer from a snapshot that never moves. That is a silent wrong answer,
        // not a slow one — two replicas behind one Service would disagree about
        // the same query. Re-scan on a short timer whenever this replica is not
        // the writer; the gate is inside the loop because the role can change.
        {
            let refresh_registry = registry.clone();
            let refresh_lease = inner_lease.clone();
            let _refresh = tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    if refresh_lease.is_active() {
                        continue;
                    }
                    for id in refresh_registry.tenant_ids() {
                        if let Some(store) = refresh_registry.loaded_tenant(&id) {
                            store.refresh_parquet_index();
                        }
                    }
                }
            });
        }

        let lease_tick = inner_lease.clone();
        let coverage_for_lease = self.state.inner.coverage.clone();
        let _lease = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(lease::TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let changed = lease_tick.tick();
                // Only the writer rewrites the shared trend file: a standby that
                // saved its own smaller map would erase the writer's points.
                coverage_for_lease.set_persistent(lease_tick.is_active());
                if changed {
                    if lease_tick.is_active() {
                        info!(
                            holder = lease_tick.holder(),
                            fence = lease_tick.fence(),
                            "this replica is now the writer"
                        );
                    } else {
                        warn!(
                            holder = lease_tick.holder(),
                            fence = lease_tick.fence(),
                            "another replica holds the lease; serving reads only"
                        );
                    }
                }
            }
        });

        let compact_registry = registry.clone();
        // Only the lease holder maintains storage; a standby keeps its readers
        // warm and refuses writes, so the two can never rotate the same files.
        let lease_for_tasks = inner_lease.clone();
        let flush_interval = cfg
            .storage_flush_interval_ns
            .map(Duration::from_nanos)
            .unwrap_or(Duration::from_secs(10));
        let compact_interval = cfg
            .prune_interval
            .min(Duration::from_secs(10))
            .min(flush_interval);
        let compact = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(compact_interval.max(Duration::from_secs(1)));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if !lease_for_tasks.is_active() {
                    continue;
                }
                let registry = compact_registry.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let now = lidlesseye_core::now_ns();
                    for (tenant, flushed, uploaded, merged) in registry.compaction_tick(now) {
                        info!(
                            tenant = %tenant,
                            flushed,
                            uploaded,
                            merged,
                            "parquet compaction"
                        );
                    }
                })
                .await;
            }
        });

        let lease_for_prune = inner_lease.clone();
        let prune = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(cfg.prune_interval.max(Duration::from_secs(1)));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if !lease_for_prune.is_active() {
                    continue;
                }
                let registry = registry.clone();
                let metrics = metrics.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    match registry.prune(lidlesseye_core::now_ns()) {
                        Ok(per_tenant) if !per_tenant.is_empty() => {
                            for (tenant, stats) in per_tenant {
                                metrics
                                    .batches_pruned
                                    .fetch_add(stats.removed_batches as u64, Ordering::Relaxed);
                                metrics
                                    .parquet_files_pruned
                                    .fetch_add(stats.removed_files as u64, Ordering::Relaxed);
                                info!(
                                    tenant = %tenant,
                                    batches = stats.removed_batches,
                                    files = stats.removed_files,
                                    bytes = stats.removed_bytes,
                                    "pruned old data"
                                );
                            }
                        }
                        Ok(_) => {}
                        Err(err) => warn!(error = %err, "prune failed"),
                    }
                })
                .await;
            }
        });

        // The on-disk check runs on its own clock and never blocks a query: it
        // only reads directory entries and file headers.
        let fsck_task = {
            let fsck = self.state.inner.fsck.clone();
            // The prune task below takes the local `registry`, so this one takes
            // its own handle from the state.
            let registry = self.state.inner.registry.clone();
            let data_dir = registry.root().to_path_buf();
            let every = cfg.fsck_interval.max(Duration::from_secs(60));
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(every);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    let tenants: Vec<String> = registry
                        .tenant_ids()
                        .into_iter()
                        .map(|tenant| tenant.to_string())
                        .collect();
                    if tenants.is_empty() {
                        continue;
                    }
                    let fsck = fsck.clone();
                    let data_dir = data_dir.clone();
                    let ran = tokio::task::spawn_blocking(move || fsck.run(&data_dir, &tenants))
                        .await;
                    match ran {
                        Ok(report) if report.has_findings() => {
                            for tenant in &report.tenants {
                                if tenant.empty_hours.is_empty()
                                    && tenant.unreadable.is_empty()
                                    && tenant.vanished.is_empty()
                                {
                                    continue;
                                }
                                warn!(
                                    tenant = %tenant.tenant,
                                    files = tenant.files,
                                    vanished = tenant.vanished.len(),
                                    unreadable = tenant.unreadable.len(),
                                    empty_hours = tenant.empty_hours.len(),
                                    "on-disk check found a problem"
                                );
                            }
                        }
                        Ok(_) => {}
                        Err(err) => warn!(error = %err, "on-disk check failed"),
                    }
                }
            })
        };

        info!(addr = %addr, "LidlessEye listening");
        let result = axum::serve(self.listener, app)
            .with_graceful_shutdown(shutdown)
            .await;
        prune.abort();
        compact.abort();
        fsck_task.abort();
        result.map_err(|err| format!("server error: {err}"))
    }

    /// Spawn the server on the current runtime.
    pub fn spawn(self) -> RunningServer {
        let addr = self.local_addr();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let join = tokio::spawn(async move {
            let _ = self
                .serve_with_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        RunningServer {
            addr,
            shutdown: Some(tx),
            join,
        }
    }
}

/// Handle for a server spawned with [`LidlessEye::spawn`].
pub struct RunningServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl RunningServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stop accepting new requests and wait for the server to exit.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.join.await;
    }
}

fn build_router(state: AppState) -> Router {
    let max_body = state.inner.cfg.max_body_bytes;
    let cache_state = state.clone();
    let cors_origin = state.inner.cfg.cors_allow_origin.clone();
    let mut router = Router::new()
        .route("/", get(ui::index))
        .route("/ingest", post(api::ingest))
        .route("/api/v1/flush", post(api::flush))
        .route("/api/v1/health", get(api::health))
        .route("/api/v1/ready", get(api::ready))
        .route("/api/v1/stats", get(api::stats))
        .route("/api/v1/tenants", get(api::tenants))
        .route("/api/v1/series", get(api::series))
        .route("/api/v1/profile_types", get(api::profile_types))
        .route("/api/v1/label-names", get(api::label_names))
        .route("/api/v1/labels", get(api::label_names))
        .route("/api/v1/label/{name}/values", get(api::label_values))
        .route("/api/v1/flamegraph", get(api::flamegraph))
        .route("/api/v1/diff", get(api::diff))
        .route("/api/v1/top", get(api::top))
        .route("/api/v1/timeseries", get(api::timeseries))
        .route("/api/v1/servicemap", get(api::servicemap))
        .route("/api/v1/servicemap/history", get(api::servicemap_history))
        .route(
            "/api/v1/servicemap/histogram",
            get(api::servicemap_histogram),
        )
        .route("/api/v1/servicemap/ingest", post(api::servicemap_ingest))
        .route(
            "/api/v1/profiling/directives",
            get(api::profiling_directives).put(api::profiling_directive_set),
        )
        .route(
            "/api/v1/memory/coverage",
            get(api::memory_coverage).post(api::memory_coverage_report),
        )
        .route(
            "/api/v1/capabilities",
            get(api::capabilities).post(api::capabilities_report),
        )
        .route(
            "/api/v1/memory/processes",
            get(api::memory_processes).post(api::memory_processes_report),
        )
        .route(
            "/api/v1/fsck",
            get(api::fsck).post(api::fsck_run),
        )
        .route("/api/v1/cache", get(api::cache_stats))
        .route("/api/v1/role", get(api::role))
        .route("/api/v1/version", get(api::version))
        .route("/metrics", get(api::metrics))
        .fallback(ui::fallback)
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(cache_state, crate::cache::layer))
        .layer(DefaultBodyLimit::max(max_body))
        .layer(RequestDecompressionLayer::new())
        .layer(CompressionLayer::new())
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(60),
        ))
        .layer(TraceLayer::new_for_http());

    if let Some(origin) = &cors_origin {
        let origin = if origin == "*" {
            tower_http::cors::AllowOrigin::any()
        } else {
            tower_http::cors::AllowOrigin::exact(
                origin
                    .parse()
                    .unwrap_or(axum::http::HeaderValue::from_static("*")),
            )
        };
        router = router.layer(
            CorsLayer::new()
                .allow_origin(origin)
                .allow_headers(tower_http::cors::AllowHeaders::mirror_request())
                .allow_methods([axum::http::Method::GET, axum::http::Method::POST]),
        );
    }
    router
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
    pub uptime_seconds: u64,
}

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub version: &'static str,
    pub tenant: String,
    pub uptime_seconds: u64,
    pub store: StoreStats,
    /// Connection-lock telemetry for the selected tenant, so a slow request
    /// can be attributed to a reader, ingest, or a specific background job.
    pub locks: Option<StoreLockStats>,
    pub series: usize,
    pub tenants: usize,
    pub ingest_reports: u64,
    pub ingest_samples: u64,
    pub queries: u64,
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Names of the UI assets compiled into the binary.
pub fn ui_assets() -> Vec<String> {
    ui::embedded_assets()
}

/// Parse a timestamp the same way the HTTP API does (RFC3339, unix seconds,
/// `ms`/`us`/`ns` suffixes and `now-1h`).
pub fn parse_timestamp(input: &str, now_ns: u64) -> Result<u64, String> {
    params::parse_time_ns(input, now_ns).map_err(|err| err.to_string())
}

/// Wait for Ctrl-C or SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}
