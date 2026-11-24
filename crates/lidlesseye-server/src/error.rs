//! API error type: one place that decides the HTTP status for every failure.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use lidlesseye_core::Error as CoreError;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiError {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl From<CoreError> for ApiError {
    fn from(err: CoreError) -> Self {
        let status = match &err {
            CoreError::InvalidArgument(_)
            | CoreError::InvalidReport(_)
            | CoreError::Selector(_)
            | CoreError::InvalidRange { .. }
            | CoreError::InvalidTenant(_)
            | CoreError::Malformed(_) => StatusCode::BAD_REQUEST,
            // A limit is a temporary state the caller can fix by dropping data
            // or asking an operator to raise it: 429, not 500.
            CoreError::LimitExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
            // The store held its connection for longer than a reader waits.
            // Nothing is wrong with the query, so it must not look like a 500:
            // 503 with a retry hint is what a dashboard can act on.
            CoreError::Busy(_) => StatusCode::SERVICE_UNAVAILABLE,
            CoreError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError::new(status, err.to_string())
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": self.message,
            "status": self.status.as_u16(),
        });
        // A 503 is always transient here (the store was busy), so say so in the
        // one way HTTP clients understand. Agents back off on their own, but a
        // browser or an `curl` user should not be left guessing how long.
        if self.status == StatusCode::SERVICE_UNAVAILABLE {
            return (
                self.status,
                [(axum::http::header::RETRY_AFTER, "2")],
                Json(body),
            )
                .into_response();
        }
        (self.status, Json(body)).into_response()
    }
}
