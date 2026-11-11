//! Server configuration.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use lidlesseye_core::{TenantId, TenantLimits};

/// HTTP server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Address to listen on.
    pub listen: SocketAddr,
    /// Directory holding the profile segments. Each tenant gets a
    /// subdirectory under `<data_dir>/tenants/`.
    pub data_dir: PathBuf,
    /// Delete profiles older than this. Per-tenant overrides live in
    /// [`ServerConfig::tenant_limits`].
    pub retention: Option<Duration>,
    /// Soft cap for the data directory.
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
}
