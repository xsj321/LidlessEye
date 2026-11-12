//! # LidlessEye
//!
//! Continuous, low-overhead profiling for Linux, in one Rust crate.
//!
//! LidlessEye is the profiling backend you embed, not a service you deploy:
//! it starts an HTTP server, ingests profiles over HTTP **or** through a plain
//! Rust call, stores them in an immutable columnar format and serves
//! flamegraphs. The eBPF collector ships in the same crate, so a single
//! `LidlessEye::builder()` gives you "profile this process and show me the
//! flamegraph" with no external infrastructure.
//!
//! ## Embedded: profile yourself
//!
//! ```no_run
//! use lidlesseye::{LidlessEye, Result};
//!
//! # async fn run() -> Result<()> {
//! let eye = LidlessEye::builder()
//!     .data_dir("./profiles")
//!     .listen("127.0.0.1:4040")
//!     .agent(true)
//!     .service_name("my-service")
//!     .build()
//!     .await?;
//!
//! // `eye` now owns the eBPF collector *and* the query API. Dashboard at
//! // http://127.0.0.1:4040
//! eye.run().await
//! # }
//! ```
//!
//! ## Embedded: feed your own profiles
//!
//! ```no_run
//! use lidlesseye::{Frame, LidlessEye, Result};
//! use lidlesseye::core::ReportBuilder;
//!
//! # async fn run() -> Result<()> {
//! let eye = LidlessEye::builder().data_dir("./profiles").build().await?;
//!
//! let mut report = ReportBuilder::new("my-service", "cpu", 0, 10_000_000_000);
//! report.add_stack(
//!     [Frame::new("/usr/bin/app", "handle_request"), Frame::new("/usr/bin/app", "main")],
//!     1_000_000,
//! );
//! eye.ingest(report.finish()).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Standalone
//!
//! The `lidlesseye` binary exposes the same functionality:
//! `lidlesseye serve --data-dir ./data --agent`, `lidlesseye collect --server
//! http://host:4040`, `lidlesseye query --data-dir ./data --top`.

pub mod publisher;

pub use publisher::{PublisherConfig, PublisherHandle};

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lidlesseye_core::Report;
use lidlesseye_server::{RunningServer, ServerConfig};

pub use lidlesseye_core as core;
pub use lidlesseye_ebpf as ebpf;
pub use lidlesseye_server as server;

pub use lidlesseye_core::{
    Frame, ProfileType, QueryOptions, Selector, Store, StoreConfig, TimeSeriesOptions,
};
pub use lidlesseye_server::{LidlessEye as HttpServer, Metrics};

/// Errors returned by the high level API.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("server error: {0}")]
    Server(String),
    #[error("collector error: {0}")]
    Collector(String),
    #[error("http error: {0}")]
    Http(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Core(#[from] lidlesseye_core::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Builder for an embedded LidlessEye instance.
pub struct Builder {
    data_dir: PathBuf,
    listen: String,
    retention: Option<Duration>,
    max_disk_bytes: Option<u64>,
    duckdb_threads: Option<usize>,
    auth_token: Option<String>,
    read_only: bool,
    cors: Option<String>,

    tenant: String,
    tenant_tokens: Vec<(String, String)>,
    allowed_tenants: Option<Vec<String>>,
    tenant_limits: BTreeMap<String, lidlesseye_core::TenantLimits>,
    max_tenants: usize,
    tenant_header: String,

    agent: bool,
    service_name: Option<String>,
    labels: BTreeMap<String, String>,
    frequency_hz: u64,
    flush_interval: Duration,
    collect_cpu: bool,
    collect_off_cpu: bool,
    enrich_kubernetes_labels: bool,
}

impl Default for Builder {
    fn default() -> Self {
        Builder {
            data_dir: PathBuf::from("./lidlesseye-data"),
            // Loopback by default. An embedded server usually has no auth
            // token, and binding every interface would publish the dashboard
            // and the ingest API to the whole network. The `lidlesseye` binary
            // keeps `0.0.0.0` as its own default, where the operator makes
            // that choice explicitly.
            listen: "127.0.0.1:4040".to_string(),
            retention: Some(Duration::from_secs(7 * 24 * 3600)),
            max_disk_bytes: Some(50 * 1024 * 1024 * 1024),
            duckdb_threads: None,
            auth_token: None,
            read_only: false,
            cors: None,
            tenant: lidlesseye_core::TenantId::DEFAULT.to_string(),
            tenant_tokens: Vec::new(),
            allowed_tenants: None,
            tenant_limits: BTreeMap::new(),
            max_tenants: 1000,
            tenant_header: "X-Scope-OrgID".to_string(),
            agent: false,
            service_name: None,
            labels: BTreeMap::new(),
            frequency_hz: 99,
            flush_interval: Duration::from_secs(10),
            collect_cpu: true,
            collect_off_cpu: true,
            enrich_kubernetes_labels: true,
        }
    }
}

impl Builder {
    pub fn new() -> Self {
        Builder::default()
    }

    /// Directory where the tenant databases are stored.
    pub fn data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = dir.into();
        self
    }

    /// Address the HTTP API listens on.
    pub fn listen(mut self, addr: impl Into<String>) -> Self {
        self.listen = addr.into();
        self
    }

    /// Delete profiles older than this.
    pub fn retention(mut self, retention: Option<Duration>) -> Self {
        self.retention = retention;
        self
    }

    /// Soft cap for the estimated size of retained batches.
    pub fn max_disk_bytes(mut self, bytes: Option<u64>) -> Self {
        self.max_disk_bytes = bytes;
        self
    }

    /// Worker threads DuckDB may use per tenant connection.
    /// `None` (the default) lets DuckDB use every core.
    pub fn duckdb_threads(mut self, threads: Option<usize>) -> Self {
        self.duckdb_threads = threads;
        self
    }

    /// Require `Authorization: Bearer <token>` for ingest and API calls.
    pub fn auth_token(mut self, token: Option<String>) -> Self {
        self.auth_token = token;
        self
    }

    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Enable CORS for the given origin (`*` for any).
    pub fn cors(mut self, origin: Option<String>) -> Self {
        self.cors = origin;
        self
    }

    /// Tenant the built-in collector writes to, and the default tenant of the
    /// embedded ingestion API. Defaults to `default`.
    pub fn tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
        self
    }

    /// Bind a bearer token to a tenant. Implies authentication is required.
    pub fn tenant_token(mut self, token: impl Into<String>, tenant: impl Into<String>) -> Self {
        self.tenant_tokens.push((token.into(), tenant.into()));
        self
    }

    /// Restrict the server to these tenants.
    pub fn allowed_tenants(mut self, tenants: Vec<String>) -> Self {
        self.allowed_tenants = Some(tenants);
        self
    }

    /// Per-tenant storage limits.
    pub fn tenant_limits(
        mut self,
        tenant: impl Into<String>,
        limits: lidlesseye_core::TenantLimits,
    ) -> Self {
        self.tenant_limits.insert(tenant.into(), limits);
        self
    }

    /// Request header carrying the tenant id.
    ///
    /// Defaults to `X-Scope-OrgID`, the header Pyroscope and the Grafana
    /// datasource send. Change it when a gateway in front already owns that
    /// name; `x-tenant-id` is always accepted as an alias.
    pub fn tenant_header(mut self, header: impl Into<String>) -> Self {
        self.tenant_header = header.into();
        self
    }

    /// Maximum number of tenants this server will host.
    pub fn max_tenants(mut self, max: usize) -> Self {
        self.max_tenants = max;
        self
    }

    /// Start the built-in eBPF collector. Requires root and Linux.
    pub fn agent(mut self, enabled: bool) -> Self {
        self.agent = enabled;
        self
    }

    /// Service name reported by the collector. Defaults to the process name.
    pub fn service_name(mut self, name: impl Into<String>) -> Self {
        self.service_name = Some(name.into());
        self
    }

    /// Extra labels attached to every collected report.
    pub fn labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = labels;
        self
    }

    /// Sampling frequency of the CPU profiler.
    pub fn frequency_hz(mut self, hz: u64) -> Self {
        self.frequency_hz = hz;
        self
    }

    /// How often the collector flushes to storage.
    pub fn flush_interval(mut self, interval: Duration) -> Self {
        self.flush_interval = interval;
        self
    }

    pub fn collect_cpu(mut self, enabled: bool) -> Self {
        self.collect_cpu = enabled;
        self
    }

    pub fn collect_off_cpu(mut self, enabled: bool) -> Self {
        self.collect_off_cpu = enabled;
        self
    }

    /// Attach the `k8s_*` labels derived from each process's cgroup.
    pub fn enrich_kubernetes_labels(mut self, enabled: bool) -> Self {
        self.enrich_kubernetes_labels = enabled;
        self
    }

    fn server_config(&self) -> Result<ServerConfig> {
        let listen: SocketAddr = self.listen.parse().map_err(|err| {
            Error::Config(format!("invalid listen address `{}`: {err}", self.listen))
        })?;
        let mut tenant_tokens = BTreeMap::new();
        for (token, tenant) in &self.tenant_tokens {
            lidlesseye_core::TenantId::parse(tenant)
                .map_err(|err| Error::Config(err.to_string()))?;
            tenant_tokens.insert(token.clone(), tenant.clone());
        }
        Ok(ServerConfig {
            listen,
            data_dir: self.data_dir.clone(),
            retention: self.retention,
            max_disk_bytes: self.max_disk_bytes,
            duckdb_threads: self.duckdb_threads,
            auth_token: self.auth_token.clone(),
            tenant_tokens,
            allowed_tenants: self.allowed_tenants.clone(),
            tenant_limits: self.tenant_limits.clone(),
            max_tenants: self.max_tenants,
            tenant_header: self.tenant_header.clone(),
            read_only: self.read_only,
            cors_allow_origin: self.cors.clone(),
            ..Default::default()
        })
    }

    /// Bind the HTTP server and, when requested, start the collector.
    ///
    /// This **binds** but does not **serve**: nothing answers requests until
    /// you call [`LidlessEye::run`] (blocking) or [`LidlessEye::spawn`]
    /// (background). `ingest` and `store` work before either, so a caller that
    /// only wants the storage engine can skip serving entirely.
    ///
    /// The default listen address is `127.0.0.1:4040`; pass
    /// [`Builder::listen`] to widen it.
    pub async fn build(self) -> Result<LidlessEye> {
        let cfg = self.server_config()?;
        let tenant = lidlesseye_core::TenantId::parse(&self.tenant)
            .map_err(|err| Error::Config(err.to_string()))?;
        let http = lidlesseye_server::LidlessEye::bind(cfg)
            .await
            .map_err(Error::Server)?;

        let collector = if self.agent {
            let collector_cfg = lidlesseye_ebpf::CollectorConfig {
                service_name: self.service_name.clone(),
                labels: self.labels.clone(),
                frequency_hz: self.frequency_hz,
                flush_interval: self.flush_interval,
                collect_cpu: self.collect_cpu,
                collect_off_cpu: self.collect_off_cpu,
                enrich_kubernetes_labels: self.enrich_kubernetes_labels,
                ..Default::default()
            };
            let state = http.state();
            let tenant = tenant.clone();
            // The collector invokes the callback on its own thread, which has
            // no runtime context: capture the handle before spawning it.
            let runtime = tokio::runtime::Handle::current();
            let handle = lidlesseye_ebpf::start(collector_cfg, move |report: Report| {
                let state = state.clone();
                let tenant = tenant.clone();
                runtime.spawn(async move {
                    if let Err(err) = state.ingest_many(&tenant, vec![report]).await {
                        tracing::warn!(error = %err, "failed to store collected report");
                    }
                });
            })
            .map_err(Error::Collector)?;
            Some(handle)
        } else {
            None
        };

        Ok(LidlessEye {
            http,
            collector,
            data_dir: self.data_dir,
            tenant,
        })
    }
}

/// An embedded LidlessEye instance: HTTP API, storage and (optionally) the
/// eBPF collector.
pub struct LidlessEye {
    http: lidlesseye_server::LidlessEye,
    collector: Option<lidlesseye_ebpf::CollectorHandle>,
    data_dir: PathBuf,
    tenant: lidlesseye_core::TenantId,
}

impl LidlessEye {
    pub fn builder() -> Builder {
        Builder::new()
    }

    /// Start the collector on an already built instance (Linux + root only).
    pub fn start_agent(
        &mut self,
        config: lidlesseye_ebpf::CollectorConfig,
    ) -> Result<lidlesseye_ebpf::StatsSnapshot> {
        if self.collector.is_some() {
            return Err(Error::Collector("collector already running".into()));
        }
        let state = self.http.state();
        let tenant = self.tenant.clone();
        let runtime = tokio::runtime::Handle::current();
        let handle = lidlesseye_ebpf::start(config, move |report: Report| {
            let state = state.clone();
            let tenant = tenant.clone();
            runtime.spawn(async move {
                if let Err(err) = state.ingest_many(&tenant, vec![report]).await {
                    tracing::warn!(error = %err, "failed to store collected report");
                }
            });
        })
        .map_err(Error::Collector)?;
        self.collector = Some(handle);
        Ok(self
            .collector_stats()
            .expect("collector was just installed"))
    }

    pub fn collector_stats(&self) -> Option<lidlesseye_ebpf::StatsSnapshot> {
        self.collector.as_ref().map(|c| c.stats())
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.http.local_addr()
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.local_addr())
    }

    pub fn store(&self) -> std::sync::Arc<Store> {
        self.http.store()
    }

    /// Store of the tenant this instance was built for.
    pub fn tenant(&self) -> &lidlesseye_core::TenantId {
        &self.tenant
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn http(&self) -> &lidlesseye_server::LidlessEye {
        &self.http
    }

    /// Ingest a report directly, without an HTTP round trip.
    pub async fn ingest(&self, report: Report) -> Result<Arc<SegmentMeta>> {
        self.http
            .ingest_for(&self.tenant, report)
            .await
            .map_err(|err| Error::Server(err.to_string()))
    }

    /// Ingest a batch of reports.
    pub async fn ingest_many(&self, reports: Vec<Report>) -> Result<Vec<Arc<SegmentMeta>>> {
        self.http
            .ingest_many(&self.tenant, reports)
            .await
            .map_err(|err| Error::Server(err.to_string()))
    }

    /// Spawn the HTTP server on the current runtime.
    pub fn spawn(self) -> RunningInstance {
        let collector = std::sync::Mutex::new(self.collector);
        let running = self.http.spawn();
        RunningInstance {
            inner: running,
            collector,
        }
    }

    /// Run the HTTP server until the process is asked to stop.
    pub async fn run(self) -> Result<()> {
        let collector = self.collector;
        let result = self.http.serve().await.map_err(Error::Server);
        if let Some(collector) = collector {
            collector.stop();
        }
        result
    }
}

pub use lidlesseye_core::SegmentMeta;

/// A spawned instance. Dropping it stops the HTTP server and the collector.
pub struct RunningInstance {
    inner: RunningServer,
    collector: std::sync::Mutex<Option<lidlesseye_ebpf::CollectorHandle>>,
}

impl RunningInstance {
    pub fn addr(&self) -> SocketAddr {
        self.inner.addr()
    }

    pub fn base_url(&self) -> String {
        self.inner.base_url()
    }

    pub fn collector_stats(&self) -> Option<lidlesseye_ebpf::StatsSnapshot> {
        self.collector
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|c| c.stats()))
    }

    /// Stop the server and the collector.
    pub async fn shutdown(self) {
        if let Some(collector) = self.collector.lock().ok().and_then(|mut g| g.take()) {
            collector.stop();
        }
        self.inner.shutdown().await;
    }
}

/// Initialise tracing with a sane default (`RUST_LOG` honoured).
pub fn init_tracing(default_filter: &str) {
    use tracing_subscriber::EnvFilter;
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
