//! A short-lived response cache in front of the read endpoints.
//!
//! The dashboard is not a stream of distinct queries; it is the same four
//! questions asked at nearly the same instant, several times, by panels that
//! refresh together. Caching the answer for a few seconds turns the second and
//! later copies of a window into a memcpy instead of a Parquet scan, which is
//! the whole win available here without inventing a rollup layer.
//!
//! It is deliberately blunt: a time-to-live, no watermark, no invalidation
//! hooks. That means an answer can be up to [`TTL`] stale. The UI refreshes on
//! the order of tens of seconds, and every endpoint here is an aggregate over a
//! window that ends in the past, so bounded staleness is honest as long as it is
//! visible: every response says whether it was served from here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use parking_lot::Mutex;

use crate::AppState;

/// How long an answer may be reused. Short enough that a dashboard's own
/// refresh outruns it, long enough that one page view reuses its siblings.
pub const TTL: Duration = Duration::from_secs(10);

/// Bounded so a cache cannot become the leak that takes the process down.
const CAPACITY: usize = 512;

/// Largest body worth keeping; a huge flamegraph is cheaper to rebuild than to
/// hold twice.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Endpoints whose answers are aggregate windows and therefore cacheable.
const PREFIXES: &[&str] = &[
    "/api/v1/flamegraph",
    "/api/v1/top",
    "/api/v1/timeseries",
    "/api/v1/servicemap",
];

#[derive(Default)]
pub struct Cache {
    entries: Mutex<HashMap<String, (Instant, Bytes)>>,
    hits: AtomicU64,
    misses: AtomicU64,
    skipped: AtomicU64,
}

/// What `GET /api/v1/cache` reports.
pub struct Stats {
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub skipped: u64,
}

impl Cache {
    pub fn stats(&self) -> Stats {
        Stats {
            entries: self.entries.lock().len(),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
        }
    }

    fn get(&self, key: &str) -> Option<Bytes> {
        let mut entries = self.entries.lock();
        match entries.get(key) {
            Some((at, body)) if at.elapsed() < TTL => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(body.clone())
            }
            Some(_) => {
                entries.remove(key);
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    fn put(&self, key: String, body: Bytes) {
        let mut entries = self.entries.lock();
        if entries.len() >= CAPACITY {
            // Drop everything expired; if that was not enough, drop the oldest
            // half rather than grow without bound.
            entries.retain(|_, (at, _)| at.elapsed() < TTL);
            if entries.len() >= CAPACITY {
                let mut ordered: Vec<(String, Instant)> = entries
                    .iter()
                    .map(|(key, (at, _))| (key.clone(), *at))
                    .collect();
                ordered.sort_by_key(|(_, at)| *at);
                for (key, _) in ordered.into_iter().take(CAPACITY / 2) {
                    entries.remove(&key);
                }
            }
        }
        entries.insert(key, (Instant::now(), body));
    }
}

/// Cache every cacheable GET for [`TTL`].
pub async fn layer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if req.method() != axum::http::Method::GET {
        return next.run(req).await;
    }
    let path = req.uri().path().to_string();
    if !PREFIXES.iter().any(|prefix| path.starts_with(prefix)) {
        return next.run(req).await;
    }
    // A caller that needs the truth now says so; the UI never does.
    if req.uri().query().map(|q| q.contains("fresh=1")).unwrap_or(false) {
        state.inner.cache.skipped.fetch_add(1, Ordering::Relaxed);
        return next.run(req).await;
    }
    // The tenant is part of the identity of every answer: two tenants asking the
    // same window must never see each other's bytes.
    let tenant = req
        .headers()
        .get("x-scope-orgid")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let key = format!(
        "{tenant}\u{1}{}{}",
        path,
        req.uri().query().map(|q| format!("?{q}")).unwrap_or_default()
    );

    if let Some(body) = state.inner.cache.get(&key) {
        return tagged(StatusCode::OK, body, "hit");
    }

    let response = next.run(req).await;
    if response.status() != StatusCode::OK {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "cache: body too large to buffer");
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("response too large"))
                .unwrap_or_else(|_| Response::new(Body::empty()));
        }
    };
    if bytes.len() <= MAX_BODY {
        state.inner.cache.put(key, bytes.clone());
    }
    parts
        .headers
        .insert("x-lidlesseye-cache", HeaderValue::from_static("miss"));
    Response::from_parts(parts, Body::from(bytes))
}

fn tagged(status: StatusCode, body: Bytes, tag: &'static str) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
        .headers_mut()
        .insert("x-lidlesseye-cache", HeaderValue::from_static(tag));
    response
}
