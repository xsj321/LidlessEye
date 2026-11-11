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

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::DefaultBodyLimit;
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
use lidlesseye_core::{SegmentMeta, Store};

pub use config::ServerConfig;
pub use error::ApiError;
pub use metrics::Metrics;

/// Cheaply cloneable server state.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<Store>,
    cfg: ServerConfig,
    metrics: Arc<Metrics>,
    started: Instant,
}

impl AppState {
    pub fn store(&self) -> &Arc<Store> {
        &self.inner.store
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

    /// Check the bearer token, if one is configured.
    fn authorize(&self, headers: &axum::http::HeaderMap) -> Result<(), ApiError> {
        let Some(expected) = &self.inner.cfg.auth_token else {
            return Ok(());
        };
        let header = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let token = header.strip_prefix("Bearer ").unwrap_or_default();
        // Constant-time comparison: the token is a shared secret.
        let ok = token.len() == expected.len()
            && token
                .bytes()
                .zip(expected.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;
        if ok {
            Ok(())
        } else {
            Err(ApiError::unauthorized())
        }
    }
}

impl AppState {
    /// Persist several reports in one blocking file-system task.
    pub async fn ingest_many(
        &self,
        reports: Vec<Report>,
    ) -> Result<Vec<Arc<SegmentMeta>>, ApiError> {
        if self.inner.cfg.read_only {
            return Err(ApiError::forbidden("server is read-only"));
        }
        let store = self.inner.store.clone();
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
        };
        let store = Store::open(store_cfg).map_err(|err| format!("failed to open store: {err}"))?;
        let listener = TcpListener::bind(cfg.listen)
            .await
            .map_err(|err| format!("failed to bind {}: {err}", cfg.listen))?;
        let stats = store.stats();
        info!(
            addr = %listener.local_addr().map(|a| a.to_string()).unwrap_or_default(),
            data_dir = %cfg.data_dir.display(),
            segments = stats.segment_count,
            bytes = stats.total_bytes,
            "store opened"
        );
        Ok(LidlessEye {
            state: AppState {
                inner: Arc::new(Inner {
                    store: Arc::new(store),
                    cfg,
                    metrics: Arc::new(Metrics::default()),
                    started: Instant::now(),
                }),
            },
            listener,
        })
    }

    /// Build a server around an existing store (used for tests and embedding).
    pub fn from_store(store: Arc<Store>, cfg: ServerConfig) -> Result<Self, String> {
        let listener = std::net::TcpListener::bind(cfg.listen)
            .map_err(|err| format!("failed to bind {}: {err}", cfg.listen))?;
        listener
            .set_nonblocking(true)
            .map_err(|err| format!("failed to configure listener: {err}"))?;
        let listener = TcpListener::from_std(listener)
            .map_err(|err| format!("failed to register listener: {err}"))?;
        Ok(LidlessEye {
            state: AppState {
                inner: Arc::new(Inner {
                    store,
                    cfg,
                    metrics: Arc::new(Metrics::default()),
                    started: Instant::now(),
                }),
            },
            listener,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap())
    }

    pub fn state(&self) -> AppState {
        self.state.clone()
    }

    pub fn store(&self) -> Arc<Store> {
        self.state.inner.store.clone()
    }

    /// The HTTP router, without a listener. Useful for tests and for mounting
    /// LidlessEye inside an existing axum application.
    pub fn router(&self) -> Router {
        build_router(self.state.clone())
    }

    /// Persist a report directly, bypassing HTTP. This is what embedded users
    /// call after their own profiler produced a batch of samples.
    pub async fn ingest(&self, report: Report) -> Result<Arc<SegmentMeta>, ApiError> {
        self.ingest_many(vec![report])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::bad_request("no report supplied"))
    }

    /// Persist several reports in one blocking file-system task.
    pub async fn ingest_many(
        &self,
        reports: Vec<Report>,
    ) -> Result<Vec<Arc<SegmentMeta>>, ApiError> {
        self.state.ingest_many(reports).await
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
        let store = self.store();
        let cfg = self.state.inner.cfg.clone();
        let metrics = self.state.inner.metrics.clone();

        let prune = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(cfg.prune_interval.max(Duration::from_secs(1)));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let store = store.clone();
                let metrics = metrics.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    match store.prune(lidlesseye_core::now_ns()) {
                        Ok(stats) if stats.removed_segments > 0 => {
                            metrics
                                .pruned_segments
                                .fetch_add(stats.removed_segments as u64, Ordering::Relaxed);
                            info!(
                                segments = stats.removed_segments,
                                bytes = stats.removed_bytes,
                                "pruned old segments"
                            );
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
        .route("/api/v1/series", get(api::series))
        .route("/api/v1/profile_types", get(api::profile_types))
        .route("/api/v1/label/{name}/values", get(api::label_values))
        .route("/api/v1/flamegraph", get(api::flamegraph))
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
    pub uptime_seconds: u64,
    pub store: StoreStats,
    pub series: usize,
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
