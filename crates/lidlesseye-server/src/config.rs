//! Server configuration.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// HTTP server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Address to listen on.
    pub listen: SocketAddr,
    /// Directory holding the profile segments.
    pub data_dir: PathBuf,
    /// Delete profiles older than this.
    pub retention: Option<Duration>,
    /// Soft cap for the data directory.
    pub max_disk_bytes: Option<u64>,
    /// Maximum accepted request body, in bytes.
    pub max_body_bytes: usize,
    /// When set, `/ingest` and `/api` require `Authorization: Bearer <token>`.
    pub auth_token: Option<String>,
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
}
