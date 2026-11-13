//! The embedded single-page UI.
//!
//! `web/dist` is compiled into the binary, so a LidlessEye release is a single
//! static executable: no Node runtime, no asset directory to ship, and the UI
//! is always the same version as the API it talks to.
//!
//! The directory itself is generated — `make ui` runs `next build` and copies
//! the static export in — and is not committed. `build.rs` creates an empty
//! placeholder so a checkout without Node still compiles; until a real build is
//! present `index()` answers 503 rather than serving a blank page.

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../web/dist/"]
struct Assets;

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

fn serve(path: &str) -> Option<Response> {
    let asset = Assets::get(path)?;
    let cache = if path.starts_with("_next/static/") || path.starts_with("assets/") {
        // Next.js emits content-hashed filenames under `_next/static/`, so a
        // given URL never changes meaning; `index.html` must be revalidated or
        // a new agent would serve an old asset graph.
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Some(
        (
            [
                (header::CONTENT_TYPE, mime_for(path)),
                (header::CACHE_CONTROL, cache),
            ],
            asset.data.into_owned(),
        )
            .into_response(),
    )
}

/// `GET /`
pub async fn index() -> Response {
    serve("index.html").unwrap_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "LidlessEye UI is not embedded in this build",
        )
            .into_response()
    })
}

/// SPA fallback: serve the requested asset when it exists, otherwise hand
/// `index.html` to the router so client-side routes survive a refresh.
pub async fn fallback(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") || path == "ingest" || path == "metrics" {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": format!("unknown endpoint /{path}"),
            })),
        )
            .into_response();
    }
    if !path.is_empty()
        && let Some(response) = serve(path)
    {
        return response;
    }
    index().await
}

/// List the embedded assets, used by `lidlesseye ui --list`.
pub fn embedded_assets() -> Vec<String> {
    Assets::iter().map(|name| name.to_string()).collect()
}
