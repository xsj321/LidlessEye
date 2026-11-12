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
use tracing::{info, warn};

use lidlesseye_core::model::Report;
use lidlesseye_core::store::{StoreConfig, StoreStats};
use lidlesseye_core::{SegmentMeta, Store, TenantId, TenantStore};

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
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default()
            .trim();

        let mut bound: Option<TenantId> = None;
        if cfg.auth_token.is_none() && cfg.tenant_tokens.is_empty() {
            // Open server.
        } else {
            for (candidate, tenant) in &cfg.tenant_tokens {
                if constant_time_eq(token, candidate) {
                    bound =
                        Some(TenantId::parse(tenant).map_err(|err| {
                            ApiError::internal(format!("bad tenant token: {err}"))
                        })?);
                }
            }
            let global = cfg
                .auth_token
                .as_deref()
                .is_some_and(|expected| constant_time_eq(token, expected));
            if bound.is_none() && !global {
                return Err(ApiError::unauthorized());
            }
        }

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

    /// Store of a tenant that must already exist (read path).
    pub fn tenant_store(&self, tenant: &TenantId) -> Result<Arc<Store>, ApiError> {
        self.inner.registry.tenant(tenant).ok_or_else(|| {
            ApiError::not_found(format!(
                "tenant `{tenant}` has no data; `GET /api/v1/tenants` lists the known ones"
            ))
        })
    }

    /// Store of a tenant, creating it when needed (write path).
    pub fn writable_tenant(&self, tenant: &TenantId) -> Result<Arc<Store>, ApiError> {
        if self.inner.cfg.read_only {
            return Err(ApiError::forbidden("server is read-only"));
        }
        Ok(self.inner.registry.open_tenant(tenant)?)
    }
}

impl AppState {
    /// Persist several reports of one tenant in a single blocking task.
    pub async fn ingest_many(
        &self,
        tenant: &TenantId,
        reports: Vec<Report>,
    ) -> Result<Vec<Arc<SegmentMeta>>, ApiError> {
        if self.inner.cfg.read_only {
            return Err(ApiError::forbidden("server is read-only"));
        }
        let store = self.writable_tenant(tenant)?;
        let limits = self.inner.registry.limits(tenant);

        // Enforce the per-tenant series limit before writing anything, so a
        // rejected batch leaves no partial state behind.
        if let Some(max_series) = limits.max_series {
            let current = store.series_count();
            let fresh = distinct_new_series(&store, &reports);
            if current.saturating_add(fresh) > max_series {
                return Err(ApiError::new(
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    format!(
                        "tenant `{tenant}` is at its series limit ({max_series}); \
                         raise `tenant_limits.{tenant}.max_series` to accept more"
                    ),
                ));
            }
        }

        let metrics = self.inner.metrics.clone();
        let samples: u64 = reports.iter().map(|r| r.stacks.len() as u64).sum();
        let result = tokio::task::spawn_blocking(move || {
            let mut metas = Vec::with_capacity(reports.len());
            let mut errors = Vec::new();
            for report in &reports {
                match store.append(report) {
                    Ok(meta) => {
                        metrics.ingest_reports.fetch_add(1, Ordering::Relaxed);
                        metas.push(meta);
                    }
                    Err(err) => errors.push(err.to_string()),
                }
            }
            (metas, errors)
        })
        .await
        .map_err(|err| ApiError::internal(format!("ingest task failed: {err}")))?;

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
            .segments_written
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
        };
        let registry = TenantStore::open_with_limits(
            &cfg.data_dir,
            store_cfg,
            cfg.parsed_tenant_limits(),
            cfg.max_tenants,
        )
        .map_err(|err| format!("failed to open tenant registry: {err}"))?;
        let listener = TcpListener::bind(cfg.listen)
            .await
            .map_err(|err| format!("failed to bind {}: {err}", cfg.listen))?;
        let stats = registry.total_stats();
        info!(
            addr = %listener.local_addr().map(|a| a.to_string()).unwrap_or_default(),
            data_dir = %cfg.data_dir.display(),
            tenants = registry.tenant_ids().len(),
            batches = stats.segment_count,
            bytes = stats.total_bytes,
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
        LidlessEye {
            state: AppState {
                inner: Arc::new(Inner {
                    registry,
                    cfg,
                    metrics: Arc::new(Metrics::default()),
                    started: Instant::now(),
                    allowed_tenants,
                    activity: Mutex::new(BTreeMap::new()),
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

    /// Persist a report for the default tenant, bypassing HTTP.
    pub async fn ingest(&self, report: Report) -> Result<Arc<SegmentMeta>, ApiError> {
        self.ingest_for(&TenantId::default_tenant(), report).await
    }

    /// Persist a report for one tenant, bypassing HTTP.
    pub async fn ingest_for(
        &self,
        tenant: &TenantId,
        report: Report,
    ) -> Result<Arc<SegmentMeta>, ApiError> {
        self.ingest_many(tenant, vec![report])
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
    ) -> Result<Vec<Arc<SegmentMeta>>, ApiError> {
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

        let prune = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(cfg.prune_interval.max(Duration::from_secs(1)));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let registry = registry.clone();
                let metrics = metrics.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    match registry.prune(lidlesseye_core::now_ns()) {
                        Ok(per_tenant) if !per_tenant.is_empty() => {
                            for (tenant, stats) in per_tenant {
                                metrics
                                    .pruned_segments
                                    .fetch_add(stats.removed_segments as u64, Ordering::Relaxed);
                                info!(
                                    tenant = %tenant,
                                    batches = stats.removed_segments,
                                    bytes = stats.removed_bytes,
                                    "pruned old batches"
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

        info!(addr = %addr, "LidlessEye listening");
        let result = axum::serve(self.listener, app)
            .with_graceful_shutdown(shutdown)
            .await;
        prune.abort();
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
    let cors_origin = state.inner.cfg.cors_allow_origin.clone();
    let mut router = Router::new()
        .route("/", get(ui::index))
        .route("/ingest", post(api::ingest))
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
        .route("/api/v1/version", get(api::version))
        .route("/metrics", get(api::metrics))
        .fallback(ui::fallback)
        .with_state(state)
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
