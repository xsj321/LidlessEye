//! Ships collected reports to a remote LidlessEye server.
//!
//! Used by the `lidlesseye collect` subcommand and by the Helm agent
//! DaemonSet: profiles are produced on the node and pushed to a central
//! server, which owns storage and the UI.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lidlesseye_core::Report;
use lidlesseye_core::servicemap::ServiceMapBatch;
use tokio::sync::mpsc;

use crate::{Error, Result};

/// How many parked batches the spool keeps before the oldest are discarded.
///
/// A batch is one flush window — five minutes of profiles at the default
/// settings — so 720 files is about two days of an outage, which is well past
/// the point where an operator would rather have disk space back.
const SPOOL_KEEP: usize = 720;

/// Batches that could not be uploaded, parked on disk.
///
/// An outage has three possible answers: drop the window (data is gone), keep
/// it in memory (an agent grows until the OOM killer picks an answer), or write
/// it down. The third is the only one that survives both a long outage and a
/// restart of the agent, which is exactly when the server is most likely to be
/// down as well.
///
/// Disabled when `LIDLESSEYE_SPOOL` is unset: a host install has no data
/// directory to own, and silently inventing one is worse than not buffering.
struct Spool {
    dir: std::path::PathBuf,
    counter: AtomicU64,
}

impl Spool {
    fn from_env() -> Option<Arc<Self>> {
        let dir = std::env::var_os("LIDLESSEYE_SPOOL")?;
        let dir = std::path::PathBuf::from(dir);
        if let Err(err) = std::fs::create_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), error = %err, "cannot use the spool directory");
            return None;
        }
        Some(Arc::new(Self {
            dir,
            counter: AtomicU64::new(0),
        }))
    }

    /// Park one batch, replacing the oldest file when the budget is spent.
    fn save(&self, reports: &[Report], batch: &str) -> Result<std::path::PathBuf> {
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        let name = format!("{:020}-{:08}.json", lidlesseye_core::now_ns(), seq);
        let path = self.dir.join(&name);
        let temporary = self.dir.join(format!("{name}.tmp"));
        let body = serde_json::to_vec(&serde_json::json!({
            "batch": batch,
            "reports": reports
        }))
        .map_err(|err| Error::Http(format!("encoding a parked batch: {err}")))?;
        // Write and rename: a half-written file would be read back as a
        // truncated batch and rejected forever.
        std::fs::write(&temporary, &body)
            .and_then(|()| std::fs::rename(&temporary, &path))
            .map_err(|err| Error::Http(format!("parking a batch in {}: {err}", path.display())))?;
        self.trim();
        Ok(path)
    }

    fn pending(&self) -> Vec<std::path::PathBuf> {
        let mut files: Vec<_> = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
                .collect(),
            Err(err) => {
                tracing::warn!(dir = %self.dir.display(), error = %err, "cannot list the spool");
                return Vec::new();
            }
        };
        files.sort();
        files
    }

    fn load(&self, path: &std::path::Path) -> Result<(Vec<Report>, Option<String>)> {
        let body = std::fs::read(path)
            .map_err(|err| Error::Http(format!("reading {}: {err}", path.display())))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|err| Error::Http(format!("parsing {}: {err}", path.display())))?;
        let batch = value
            .get("batch")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let reports = value
            .get("reports")
            .cloned()
            .ok_or_else(|| Error::Http(format!("{} has no reports", path.display())))?;
        let reports = serde_json::from_value(reports)
            .map_err(|err| Error::Http(format!("decoding {}: {err}", path.display())))?;
        Ok((reports, batch))
    }

    fn trim(&self) {
        let files = self.pending();
        if files.len() <= SPOOL_KEEP {
            return;
        }
        let excess = files.len() - SPOOL_KEEP;
        for path in files.into_iter().take(excess) {
            if let Err(err) = std::fs::remove_file(&path) {
                tracing::warn!(path = %path.display(), error = %err, "cannot discard an old batch");
            }
        }
        tracing::warn!(
            discarded = excess,
            keep = SPOOL_KEEP,
            "the spool is full; the oldest parked batches are gone"
        );
    }
}

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
    /// How many profile reports may wait for upload before new ones are
    /// dropped. This is the memory bound on an agent whose server slows down.
    pub report_queue: usize,
    /// Same bound for service-map windows.
    pub map_queue: usize,
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
            report_queue: 256,
            map_queue: 64,
        }
    }
}

/// Emit one warning a minute at most when a queue is full, so an operator can
/// see that data is being dropped without the logs becoming a second outage.
fn note_drop(dropped: &AtomicU64, last_warn: &AtomicU64, what: &str) {
    let count = dropped.fetch_add(1, Ordering::Relaxed) + 1;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let previous = last_warn.swap(now, Ordering::Relaxed);
    if now.saturating_sub(previous) >= 60 {
        tracing::warn!(
            count,
            "{} queue full, dropping new data until the uploader catches up",
            what
        );
    }
}

/// How long one batch keeps retrying while the server is unreachable.
///
/// A rolling upgrade takes far longer than the old ~12 s retry budget: the
/// server stops, its startup probe waits for tenant stores to open, and the
/// Service endpoint only comes back after that. Dropping a batch in that gap
/// left a visible hole in every profile. Five minutes is long enough for a
/// slow restart while still bounding how long one batch can own the uploader.
const UPLOAD_RETRY_BUDGET: Duration = Duration::from_secs(300);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(10);

fn retry_backoff(attempt: u32) -> Duration {
    let shift = (attempt.saturating_sub(1)).min(6);
    Duration::from_millis(250u64.saturating_mul(1u64 << shift)).min(MAX_RETRY_BACKOFF)
}

/// A response that says "this payload is wrong" will not become right by
/// sending it again. Transport failures, 408, 429 and 5xx are worth retrying.
fn retriable_upload_error(err: &Error) -> bool {
    let Error::Http(message) = err else {
        return false;
    };
    let Some(rest) = message.strip_prefix("server returned ") else {
        return true;
    };
    let status = rest
        .split_whitespace()
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    status == 408 || status == 429 || (500..600).contains(&status)
}

/// Handle used to enqueue reports. Cheap to clone.
#[derive(Clone)]
pub struct PublisherHandle {
    tx: mpsc::Sender<Report>,
    map_tx: mpsc::Sender<ServiceMapBatch>,
    client: reqwest::Client,
    url: String,
    /// Headers every request carries: the tenant and, when configured, the
    /// bearer token. Kept so a read like [`PublisherHandle::directives`] uses
    /// the same identity as the uploads.
    headers: reqwest::header::HeaderMap,
    /// Server root, without the ingest path: the reads are built from it.
    base: String,
    dropped_reports: Arc<AtomicU64>,
    dropped_maps: Arc<AtomicU64>,
    report_warned: Arc<AtomicU64>,
    map_warned: Arc<AtomicU64>,
}

impl PublisherHandle {
    /// Queue a report for delivery. Never blocks the collector thread.
    ///
    /// The queue is bounded on purpose. An unbounded queue turns a slow or
    /// unreachable server into an agent OOM: the collectors keep producing on
    /// every flush while the uploader is stuck in retries, so the queue grows
    /// until the container's memory limit kills it. When full, the newest
    /// report is dropped and counted; telemetry may lose a window, but the
    /// agent stays alive and resumes as soon as the uploader drains.
    pub fn send(&self, report: Report) {
        match self.tx.try_send(report) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                note_drop(&self.dropped_reports, &self.report_warned, "report")
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!("publisher stopped, dropping report");
            }
        }
    }

    /// Queue one service-map window. Independent of the profile queue: a slow
    /// profile upload must not delay the map, and vice versa.
    pub fn send_servicemap(&self, batch: ServiceMapBatch) {
        match self.map_tx.try_send(batch) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                note_drop(&self.dropped_maps, &self.map_warned, "service map")
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!("publisher stopped, dropping service map window");
            }
        }
    }

    /// `(dropped_reports, dropped_service_maps)` since this handle was created.
    /// Clones share the counters, so the agent can log them at shutdown.
    pub fn drop_stats(&self) -> (u64, u64) {
        (
            self.dropped_reports.load(Ordering::Relaxed),
            self.dropped_maps.load(Ordering::Relaxed),
        )
    }

    /// Send one report synchronously, used by `lidlesseye ingest --server`.
    pub async fn send_now(&self, report: Report) -> Result<()> {
        post_batch(&self.client, &self.url, None, &[report]).await
    }

    /// Poll `/api/v1/health` until the server answers, or `budget` runs out.
    ///
    /// A cluster schedules the agent DaemonSet before the server's Service has
    /// endpoints, and kube-proxy needs a moment to program the rules. Collecting
    /// during that window only produces batches that exhaust their retries and
    /// get dropped, so the caller waits here first. Returns `false` when the
    /// budget expired; the caller starts collecting anyway and the uploader
    /// keeps retrying per batch.
    /// Report that a reader inside a pod is dead.
    ///
    /// A shim that stopped existing cannot report its own capability, and the
    /// workload would otherwise look switched on and merely quiet. The agent
    /// knows which pod it was and why it went away, so it says so instead.
    pub async fn report_reader_degraded(
        &self,
        workload: &str,
        pod: &str,
        reason: &str,
        at_ns: u64,
    ) -> Result<(), String> {
        let base = self.url.trim_end_matches("/ingest").trim_end_matches('/');
        let body = serde_json::json!([{
            "workload": workload,
            "pod": pod,
            "reader": "native_alloc",
            "state": "degraded",
            "reason": reason,
            "detected": 0,
            "attached": 0,
            "at_ns": at_ns,
        }]);
        let response = self
            .client
            .post(format!("{base}/api/v1/capabilities"))
            .json(&body)
            .send()
            .await
            .map_err(|err| err.to_string())?;
        if !response.status().is_success() {
            return Err(format!("capabilities answered {}", response.status()));
        }
        Ok(())
    }

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

    /// Which workloads the server has switched runtime memory profiling on
    /// for.
    ///
    /// Polled by the agent rather than pushed for a reason: an agent that was
    /// down when the switch flipped must still learn about it, and a poll is
    /// the simplest thing that cannot miss the state — it reads the current
    /// answer, not a stream of changes.
    /// One probe of the server's switchboard.
    ///
    /// Two lists, because the two kinds cost the target differently: `jvm` asks
    /// a runtime for a dump, `alloc` injects a library into the process. A
    /// server that predates the split answers with `workloads` only, which is
    /// read as "JVM" so an old server and a new agent still agree.
    pub async fn directives(&self) -> Result<(Vec<String>, Vec<String>)> {
        let url = format!("{}/api/v1/profiling/directives", self.base);
        let reply = self
            .client
            .get(&url)
            .headers(self.headers.clone())
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|err| Error::Http(err.to_string()))?
            .error_for_status()
            .map_err(|err| Error::Http(err.to_string()))?
            .json::<serde_json::Value>()
            .await
            .map_err(|err| Error::Http(err.to_string()))?;
        let list = |key: &str| -> Vec<String> {
            reply
                .get(key)
                .and_then(|value| value.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        Ok((list("workloads"), list("alloc_workloads")))
    }
}

/// Spawn the background uploader.
pub fn start(cfg: PublisherConfig) -> Result<(PublisherHandle, tokio::task::JoinHandle<()>)> {
    let base = cfg.server_url.trim_end_matches('/').to_string();
    let url = format!("{base}/ingest");
    let map_url = format!("{base}/api/v1/servicemap/ingest");
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
        .default_headers(headers.clone())
        .build()
        .map_err(|err| Error::Http(err.to_string()))?;

    let report_queue = cfg.report_queue.max(1);
    let map_queue = cfg.map_queue.max(1);
    let (tx, mut rx) = mpsc::channel::<Report>(report_queue);
    let (map_tx, mut map_rx) = mpsc::channel::<ServiceMapBatch>(map_queue);
    let dropped_reports = Arc::new(AtomicU64::new(0));
    let dropped_maps = Arc::new(AtomicU64::new(0));
    let handle = PublisherHandle {
        tx,
        map_tx,
        client: client.clone(),
        url: url.clone(),
        headers: headers.clone(),
        base: base.clone(),
        dropped_reports: Arc::clone(&dropped_reports),
        dropped_maps: Arc::clone(&dropped_maps),
        report_warned: Arc::new(AtomicU64::new(0)),
        map_warned: Arc::new(AtomicU64::new(0)),
    };

    // The map runs on its own task: its windows are independent of profile
    // batches, and a map upload that stalls must not hold back profiles.
    {
        let client = client.clone();
        let map_url = map_url.clone();
        let max_delay = cfg.max_delay;
        tokio::spawn(async move {
            let mut buffer: Vec<ServiceMapBatch> = Vec::with_capacity(32);
            loop {
                let Some(first) = map_rx.recv().await else {
                    break;
                };
                buffer.push(first);
                let deadline = tokio::time::sleep(max_delay);
                tokio::pin!(deadline);
                while buffer.len() < 32 {
                    tokio::select! {
                        maybe = map_rx.recv() => match maybe {
                            Some(batch) => buffer.push(batch),
                            None => break,
                        },
                        _ = &mut deadline => break,
                    }
                }
                let batch = std::mem::take(&mut buffer);
                let count = batch.len();
                let mut attempt = 0u32;
                let started = tokio::time::Instant::now();
                loop {
                    match post_service_map(&client, &map_url, &batch).await {
                        Ok(()) => {
                            tracing::debug!(windows = count, "uploaded service map");
                            break;
                        }
                        Err(err) => {
                            if !retriable_upload_error(&err) {
                                tracing::warn!(
                                    windows = count,
                                    error = %err,
                                    "dropping service map windows after a permanent server error"
                                );
                                break;
                            }
                            attempt += 1;
                            if started.elapsed() >= UPLOAD_RETRY_BUDGET {
                                tracing::warn!(
                                    windows = count,
                                    attempts = attempt,
                                    elapsed_ms = started.elapsed().as_millis(),
                                    error = %err,
                                    "dropping service map windows after repeated upload failures"
                                );
                                break;
                            }
                            let backoff = retry_backoff(attempt);
                            tracing::debug!(
                                windows = count,
                                attempt,
                                backoff_ms = backoff.as_millis(),
                                error = %err,
                                "service map upload failed, retrying"
                            );
                            tokio::time::sleep(backoff).await;
                        }
                    }
                }
            }
        });
    }

    // Parked batches drain on their own clock: while the report task is stuck
    // retrying one batch, this task is what turns a recovered server back into
    // a recovered pipeline.
    if let Some(spool) = Spool::from_env() {
        let client = client.clone();
        let url = url.clone();
        tokio::spawn(async move {
            let mut quiet_rounds = 0u32;
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let pending = spool.pending();
                if pending.is_empty() {
                    continue;
                }
                let mut sent = 0usize;
                for path in &pending {
                    let (reports, batch) = match spool.load(path) {
                        Ok(parked) => parked,
                        Err(err) => {
                            // A file that cannot be decoded will never decode:
                            // keeping it would block the queue behind it.
                            tracing::warn!(path = %path.display(), error = %err, "discarding an unreadable parked batch");
                            let _ = std::fs::remove_file(path);
                            continue;
                        }
                    };
                    // Re-send under the id it was parked with, so a delivery
                    // whose reply was lost on the first attempt is not counted
                    // twice now.
                    match post_batch_with(&client, &url, &reports, batch.as_deref()).await {
                        Ok(()) => {
                            let _ = std::fs::remove_file(path);
                            sent += 1;
                        }
                        Err(_) => break,
                    }
                }
                if sent > 0 {
                    quiet_rounds = 0;
                    tracing::info!(
                        parked = pending.len() - sent,
                        delivered = sent,
                        "drained parked reports"
                    );
                } else {
                    quiet_rounds += 1;
                    if quiet_rounds == 40 {
                        tracing::warn!(
                            parked = pending.len(),
                            "the spool has been undeliverable for twenty minutes"
                        );
                    }
                }
            }
        });
    }

    let task = tokio::spawn(async move {
        let max_batch = cfg.max_batch.max(1);
        let spool = Spool::from_env();
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
            // One id for every attempt at this batch, retries included: the
            // server uses it to ignore a delivery whose reply was lost.
            let batch_id = format!("{}-{}", lidlesseye_core::now_ns(), count);
            // A rolling upgrade is not a 12 s blip: the old pod terminates,
            // the new one opens every tenant store before it listens, and the
            // Service endpoint only comes back afterwards. Retry transient
            // failures for up to `UPLOAD_RETRY_BUDGET` so a restart does not
            // punch a hole in every profile, while keeping one batch from
            // owning the uploader forever.
            let mut attempt = 0u32;
            let started = tokio::time::Instant::now();
            loop {
                match post_batch_with(&client, &url, &batch, Some(&batch_id)).await {
                    Ok(()) => {
                        tracing::debug!(reports = count, "uploaded reports");
                        break;
                    }
                    Err(err) => {
                        if !retriable_upload_error(&err) {
                            tracing::warn!(
                                reports = count,
                                error = %err,
                                "dropping reports after a permanent server error"
                            );
                            break;
                        }
                        attempt += 1;
                        if started.elapsed() >= UPLOAD_RETRY_BUDGET {
                            // Giving up on the network must not mean giving up
                            // on the window: park it, and the drain loop will
                            // ship it once the server answers again.
                            match spool.as_ref().map(|spool| spool.save(&batch, &batch_id)) {
                                Some(Ok(path)) => tracing::warn!(
                                    reports = count,
                                    attempts = attempt,
                                    elapsed_ms = started.elapsed().as_millis(),
                                    path = %path.display(),
                                    error = %err,
                                    "parked reports on disk after repeated upload failures"
                                ),
                                Some(Err(save_err)) => tracing::warn!(
                                    reports = count,
                                    error = %err,
                                    save_error = %save_err,
                                    "dropping reports after repeated upload failures"
                                ),
                                None => tracing::warn!(
                                    reports = count,
                                    attempts = attempt,
                                    elapsed_ms = started.elapsed().as_millis(),
                                    error = %err,
                                    "dropping reports after repeated upload failures"
                                ),
                            }
                            break;
                        }
                        let backoff = retry_backoff(attempt);
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
    reports: &[Report],
) -> Result<()> {
    post_batch_with(client, url, reports, None).await
}

/// Post one batch, optionally naming it so the server can drop a replay.
///
/// The id has to survive retries and the spool: it is the only thing that tells
/// the server "you already counted this" when a `2xx` was lost on the way back.
async fn post_batch_with(
    client: &reqwest::Client,
    url: &str,
    reports: &[Report],
    batch: Option<&str>,
) -> Result<()> {
    let body = serde_json::json!({ "reports": reports });
    let mut request = client.post(url).json(&body);
    if let Some(batch) = batch {
        request = request.header("x-lidlesseye-batch", batch);
    }
    let response = request.send().await.map_err(|err| Error::Http(err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(Error::Http(format!("server returned {status}: {text}")));
    }
    Ok(())
}

async fn post_service_map(
    client: &reqwest::Client,
    url: &str,
    batches: &[ServiceMapBatch],
) -> Result<()> {
    let body = serde_json::json!({ "batches": batches });
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

#[cfg(test)]
mod tests {
    use lidlesseye_core::ProfileType;

    use super::*;

    fn report() -> Report {
        Report {
            service_name: "svc".to_string(),
            labels: Default::default(),
            profile_type: ProfileType::cpu(),
            start_ns: 0,
            end_ns: 1,
            sample_rate_hz: 0,
            strings: Vec::new(),
            stacks: Vec::new(),
        }
    }

    /// The receivers are returned so they stay alive: a closed channel reports
    /// `Closed`, not `Full`, which would hide the behaviour under test.
    fn handle(
        capacity: usize,
    ) -> (
        PublisherHandle,
        mpsc::Receiver<Report>,
        mpsc::Receiver<ServiceMapBatch>,
    ) {
        let (tx, rx) = mpsc::channel(capacity);
        let (map_tx, map_rx) = mpsc::channel(capacity);
        let h = PublisherHandle {
            tx,
            map_tx,
            client: reqwest::Client::new(),
            url: "http://127.0.0.1:1/ingest".to_string(),
            dropped_reports: Arc::new(AtomicU64::new(0)),
            dropped_maps: Arc::new(AtomicU64::new(0)),
            report_warned: Arc::new(AtomicU64::new(0)),
            map_warned: Arc::new(AtomicU64::new(0)),
        };
        (h, rx, map_rx)
    }

    #[test]
    fn a_full_report_queue_drops_instead_of_growing() {
        let (h, _rx, _map_rx) = handle(1);
        h.send(report()); // fills the only slot
        h.send(report()); // must be dropped, never queued
        assert_eq!(h.drop_stats().0, 1);
    }

    #[test]
    fn a_full_service_map_queue_drops_instead_of_growing() {
        let (h, _rx, _map_rx) = handle(1);
        h.send_servicemap(ServiceMapBatch::default());
        h.send_servicemap(ServiceMapBatch::default());
        assert_eq!(h.drop_stats().1, 1);
    }

    #[test]
    fn upload_backoff_caps_without_overflowing() {
        assert_eq!(retry_backoff(1), Duration::from_millis(250));
        assert_eq!(retry_backoff(5), Duration::from_millis(4000));
        assert_eq!(retry_backoff(6), Duration::from_millis(8000));
        assert_eq!(retry_backoff(100), MAX_RETRY_BACKOFF);
    }

    #[test]
    fn transport_errors_are_retried_but_bad_requests_are_not() {
        assert!(retriable_upload_error(&Error::Http(
            "error sending request for url (http://lidlesseye:4040/ingest)".into()
        )));
        assert!(retriable_upload_error(&Error::Http(
            "server returned 503 Service Unavailable: busy".into()
        )));
        assert!(retriable_upload_error(&Error::Http(
            "server returned 429 Too Many Requests: retry".into()
        )));
        assert!(!retriable_upload_error(&Error::Http(
            "server returned 400 Bad Request: invalid report JSON".into()
        )));
    }
}
