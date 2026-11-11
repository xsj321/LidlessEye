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
    /// Tenant to write to, sent as `X-Scope-OrgID`.
    pub tenant: Option<String>,
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
        let value = reqwest::header::HeaderValue::from_str(tenant)
            .map_err(|err| Error::Config(format!("invalid tenant id: {err}")))?;
        headers.insert("x-scope-orgid", value);
    }
    let client = reqwest::Client::builder()
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
            let mut attempt = 0u32;
            loop {
                match post_batch(&client, &url, None, batch.clone()).await {
                    Ok(()) => {
                        tracing::debug!(reports = count, "uploaded reports");
                        break;
                    }
                    Err(err) => {
                        attempt += 1;
                        if attempt >= 4 {
                            tracing::warn!(
                                reports = count,
                                error = %err,
                                "dropping reports after repeated upload failures"
                            );
                            break;
                        }
                        let backoff = Duration::from_millis(250 * 2u64.pow(attempt));
                        tracing::debug!(error = %err, "upload failed, retrying");
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
