//! Ships collected reports to a remote LidlessEye server.
//!
//! Used by the `lidlesseye collect` subcommand and by the Helm agent
//! DaemonSet: profiles are produced on the node and pushed to a central
//! server, which owns storage and the UI.

use std::time::Duration;

use lidlesseye_core::Report;
use tokio::sync::mpsc;

use crate::{Error, Result};

/// How the publisher talks to the server.
#[derive(Debug, Clone)]
pub struct PublisherConfig {
    /// Base URL of the server, e.g. `http://lidlesseye:4040`.
    pub server_url: String,
    /// Bearer token, when the server requires one.
    pub auth_token: Option<String>,
    /// Tenant to write to, sent in [`PublisherConfig::tenant_header`].
    pub tenant: Option<String>,
    /// Header carrying the tenant id. Defaults to `X-Tenant-Id`, the alias the
    /// server accepts whatever `--tenant-header` is set to, so a deployment
    /// that renames the header for a gateway cannot desync the agents.
    pub tenant_header: String,
    /// Maximum number of reports per HTTP request.
    pub max_batch: usize,
    /// Maximum time a report waits before being flushed.
    pub max_delay: Duration,
    /// Per-request timeout.
    pub timeout: Duration,
}

impl PublisherConfig {
    pub fn new(server_url: impl Into<String>) -> Self {
        PublisherConfig {
            server_url: server_url.into().trim_end_matches('/').to_string(),
            auth_token: None,
            tenant: None,
            tenant_header: "X-Tenant-Id".to_string(),
            max_batch: 64,
            max_delay: Duration::from_millis(250),
            timeout: Duration::from_secs(30),
        }
    }
}

/// Handle used to enqueue reports. Cheap to clone.
#[derive(Clone)]
pub struct PublisherHandle {
    tx: mpsc::UnboundedSender<Report>,
    client: reqwest::Client,
    url: String,
}

impl PublisherHandle {
    /// Queue a report for delivery. Never blocks the collector thread.
    pub fn send(&self, report: Report) {
        if self.tx.send(report).is_err() {
            tracing::debug!("publisher stopped, dropping report");
        }
    }

    /// Send one report synchronously, used by `lidlesseye ingest --server`.
    pub async fn send_now(&self, report: Report) -> Result<()> {
        post_batch(&self.client, &self.url, None, vec![report]).await
    }

    /// Poll `/api/v1/health` until the server answers, or `budget` runs out.
    ///
    /// A cluster schedules the agent DaemonSet before the server's Service has
    /// endpoints, and kube-proxy needs a moment to program the rules. Collecting
    /// during that window only produces batches that exhaust their retries and
    /// get dropped, so the caller waits here first. Returns `false` when the
    /// budget expired; the caller starts collecting anyway and the uploader
    /// keeps retrying per batch.
    pub async fn wait_until_ready(&self, budget: Duration) -> bool {
        let health = format!(
            "{}/api/v1/health",
            self.url.trim_end_matches("/ingest").trim_end_matches('/')
        );
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            // A connect to a ClusterIP whose endpoint just disappeared is
            // blackholed rather than refused, and the client's 30s request
            // timeout would swallow the whole budget in one attempt.
            let probe =
                tokio::time::timeout(Duration::from_secs(3), self.client.get(&health).send()).await;
            match probe {
                Ok(Ok(response)) if response.status().is_success() => return true,
                Ok(Ok(response)) => {
                    tracing::debug!(status = %response.status(), "server not ready yet");
                }
                Ok(Err(err)) => tracing::debug!(error = %err, "server not reachable yet"),
                Err(_) => tracing::debug!("server did not answer the health probe in 3s"),
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.url
    }
}

/// Spawn the background uploader.
pub fn start(cfg: PublisherConfig) -> Result<(PublisherHandle, tokio::task::JoinHandle<()>)> {
    let url = format!("{}/ingest", cfg.server_url.trim_end_matches('/'));
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(token) = &cfg.auth_token {
        let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|err| Error::Config(format!("invalid auth token: {err}")))?;
        value.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    if let Some(tenant) = &cfg.tenant {
        let name = reqwest::header::HeaderName::from_bytes(cfg.tenant_header.as_bytes()).map_err(
            |err| {
                Error::Config(format!(
                    "invalid tenant header `{}`: {err}",
                    cfg.tenant_header
                ))
            },
        )?;
        let value = reqwest::header::HeaderValue::from_str(tenant)
            .map_err(|err| Error::Config(format!("invalid tenant id: {err}")))?;
        headers.insert(name, value);
    }
    let client = reqwest::Client::builder()
        // Without a connect timeout a blackholed Service IP keeps the batch in
        // flight for the full request timeout, which stalls every later batch.
        .connect_timeout(Duration::from_secs(5))
        .timeout(cfg.timeout)
        .user_agent(format!("lidlesseye/{}", env!("CARGO_PKG_VERSION")))
        .default_headers(headers)
        .build()
        .map_err(|err| Error::Http(err.to_string()))?;

    let (tx, mut rx) = mpsc::unbounded_channel::<Report>();
    let handle = PublisherHandle {
        tx,
        client: client.clone(),
        url: url.clone(),
    };

    let task = tokio::spawn(async move {
        let max_batch = cfg.max_batch.max(1);
        let mut buffer: Vec<Report> = Vec::with_capacity(max_batch);
        loop {
            // Wait for the first report of a batch, then coalesce whatever
            // arrives within `max_delay` to keep requests chunky.
            let Some(first) = rx.recv().await else {
                break;
            };
            buffer.push(first);
            let deadline = tokio::time::sleep(cfg.max_delay);
            tokio::pin!(deadline);
            while buffer.len() < max_batch {
                tokio::select! {
                    maybe = rx.recv() => {
                        match maybe {
                            Some(report) => buffer.push(report),
                            None => break,
                        }
                    }
                    _ = &mut deadline => break,
                }
            }

            let batch = std::mem::take(&mut buffer);
            let count = batch.len();
            // Retries cover the startup race in a cluster: the agent DaemonSet
            // is usually scheduled before the server Deployment is listening,
            // and dropping the first batches would leave a hole at every
            // rollout. Five backoffs of 0.5–4s ride out ~12s of downtime; past
            // that the server is genuinely unreachable and the batch is
            // dropped rather than buffered without bound.
            const MAX_ATTEMPTS: u32 = 6;
            let mut attempt = 0u32;
            loop {
                match post_batch(&client, &url, None, batch.clone()).await {
                    Ok(()) => {
                        tracing::debug!(reports = count, "uploaded reports");
                        break;
                    }
                    Err(err) => {
                        attempt += 1;
                        if attempt >= MAX_ATTEMPTS {
                            tracing::warn!(
                                reports = count,
                                error = %err,
                                "dropping reports after repeated upload failures"
                            );
                            break;
                        }
                        let backoff = Duration::from_millis(250 * 2u64.pow((attempt - 1).min(4)));
                        tracing::debug!(
                            error = %err,
                            attempt,
                            backoff_ms = backoff.as_millis(),
                            "upload failed, retrying"
                        );
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
            if buffer.capacity() == 0 {
                buffer = Vec::with_capacity(max_batch);
            }
        }
    });

    Ok((handle, task))
}

async fn post_batch(
    client: &reqwest::Client,
    url: &str,
    _token: Option<&str>,
    reports: Vec<Report>,
) -> Result<()> {
    let body = serde_json::json!({ "reports": reports });
    let response = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|err| Error::Http(err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(Error::Http(format!("server returned {status}: {text}")));
    }
    Ok(())
}
