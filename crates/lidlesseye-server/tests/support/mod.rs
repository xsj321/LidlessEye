//! Shared helpers for the integration-test suites (`round1`, `round2`,
//! `round3`).
//!
//! Every suite drives the real `axum` router through
//! `tower::ServiceExt::oneshot`, so routing, extractors, auth, tenancy,
//! validation and serialization all run exactly as they do in production —
//! only the socket is skipped.

#![allow(dead_code)]

use std::path::Path;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use lidlesseye_core::model::ProfileType;
use lidlesseye_core::{Frame, ReportBuilder, now_ns};
use lidlesseye_server::{LidlessEye, ServerConfig};
use serde_json::Value;
use tower::ServiceExt;

/// Header Pyroscope uses for the tenant (org) id.
pub const TENANT_HEADER: &str = "X-Scope-OrgID";
/// Bearer token used by the auth-enabled suites.
pub const TOKEN: &str = "round-trip-secret";

pub fn config(dir: &Path) -> ServerConfig {
    ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.to_path_buf(),
        ..Default::default()
    }
}

pub async fn router(dir: &Path) -> Router {
    LidlessEye::bind(config(dir)).await.unwrap().router()
}

pub async fn router_with(dir: &Path, cfg: ServerConfig) -> Router {
    LidlessEye::bind(ServerConfig {
        data_dir: dir.to_path_buf(),
        ..cfg
    })
    .await
    .unwrap()
    .router()
}

/// A response split into status, headers and raw bytes.
pub struct Raw {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Vec<u8>,
}

impl Raw {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    }
}

/// Send a request with an arbitrary body and headers.
pub async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Vec<u8>>,
    headers: &[(&str, &str)],
) -> Raw {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(body.unwrap_or_default())).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Raw {
        status,
        headers,
        body: body.to_vec(),
    }
}

/// `GET` helper returning status and raw text.
pub async fn get_raw(app: &Router, uri: &str) -> Raw {
    request(app, "GET", uri, None, &[]).await
}

/// JSON request helper returning the status and the decoded body.
pub async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let payload = body.map(|value| serde_json::to_vec(&value).unwrap());
    let mut all = Vec::with_capacity(headers.len() + 1);
    if payload.is_some() {
        all.push((header::CONTENT_TYPE.as_str(), "application/json"));
    }
    all.extend_from_slice(headers);
    let response = request(app, method, uri, payload, &all).await;
    (response.status, response.json())
}

pub async fn call_as(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    tenant: &str,
) -> (StatusCode, Value) {
    call(app, method, uri, body, &[(TENANT_HEADER, tenant)]).await
}

/// POST a raw payload with an explicit content type.
pub async fn post_raw(
    app: &Router,
    uri: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let response = request(
        app,
        "POST",
        uri,
        Some(body),
        &[(header::CONTENT_TYPE.as_str(), content_type)],
    )
    .await;
    (response.status, response.json())
}

/// A two-frame report worth `total` nanoseconds, ending now.
pub fn report(service: &str, profile_type: &str, total: u64) -> Value {
    let now = now_ns();
    report_window(service, profile_type, total, now - 1_000_000, now)
}

/// Same, but with an explicit window.
pub fn report_window(
    service: &str,
    profile_type: &str,
    total: u64,
    start_ns: u64,
    end_ns: u64,
) -> Value {
    let mut builder = ReportBuilder::new(service, ProfileType::new(profile_type), start_ns, end_ns)
        .label("env", "test")
        .label("region", "eu-west")
        .sample_rate_hz(99);
    builder.add_stack(
        vec![
            Frame::new("/app/server", "handle_request"),
            Frame::new("/app/server", "main"),
        ],
        total / 2,
    );
    builder.add_stack(vec![Frame::new("/app/server", "main")], total - total / 2);
    serde_json::to_value(builder.finish()).unwrap()
}

pub fn report_with_pod(service: &str, pod: &str, total: u64) -> Value {
    let now = now_ns();
    let mut builder =
        ReportBuilder::new(service, ProfileType::cpu(), now - 1_000_000, now).label("pod", pod);
    builder.add_stack(vec![Frame::new("/app", "work")], total);
    serde_json::to_value(builder.finish()).unwrap()
}

/// Hand-rolled protobuf writer, so the tests need no codegen.
#[derive(Default)]
pub struct Proto(pub Vec<u8>);

impl Proto {
    pub fn new() -> Self {
        Proto(Vec::new())
    }

    /// Varint field.
    pub fn uint(&mut self, field: u32, mut value: u64) {
        self.0.push((field << 3) as u8);
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                self.0.push(byte);
                break;
            }
            self.0.push(byte | 0x80);
        }
    }

    /// Length-delimited field.
    pub fn bytes(&mut self, field: u32, value: &[u8]) {
        self.0.push(((field << 3) | 2) as u8);
        let mut len = value.len() as u64;
        loop {
            let byte = (len & 0x7f) as u8;
            len >>= 7;
            if len == 0 {
                self.0.push(byte);
                break;
            }
            self.0.push(byte | 0x80);
        }
        self.0.extend_from_slice(value);
    }
}

/// A valid pprof profile: one sample, `main;handle_request`, 123_456 ns.
///
/// Field numbers follow `profile.proto`: `sample_type`=1, `sample`=2,
/// `location`=4, `function`=5, `string_table`=6, `time_nanos`=9,
/// `duration_nanos`=10.
pub fn pprof_fixture() -> Vec<u8> {
    let table = [
        "",
        "cpu",
        "nanoseconds",
        "main",
        "handle_request",
        "/srv/app.go",
    ];
    let mut p = Proto::new();
    let mut sample_type = Proto::new();
    sample_type.uint(1, 1);
    sample_type.uint(2, 2);
    p.bytes(1, &sample_type.0);

    for (id, name) in [(1u64, 3u64), (2, 4)] {
        let mut function = Proto::new();
        function.uint(1, id);
        function.uint(2, name);
        function.uint(4, 5);
        p.bytes(5, &function.0);
    }
    for (id, function_id, line) in [(10u64, 2u64, 12u64), (11, 1, 0)] {
        let mut line_msg = Proto::new();
        line_msg.uint(1, function_id);
        line_msg.uint(2, line);
        let mut location = Proto::new();
        location.uint(1, id);
        location.bytes(4, &line_msg.0);
        p.bytes(4, &location.0);
    }

    let mut sample = Proto::new();
    sample.uint(1, 10);
    sample.uint(1, 11);
    sample.uint(2, 123_456);
    p.bytes(2, &sample.0);

    let now = now_ns();
    p.uint(9, now - 10_000_000_000);
    p.uint(10, 10_000_000_000);
    for name in table {
        p.bytes(6, name.as_bytes());
    }
    p.0
}

/// A folded-text payload: `main;handle_request` plus `main`.
pub fn folded_fixture() -> Vec<u8> {
    b"main;handle_request 600000000\nmain 400000000\n".to_vec()
}

/// The [`pprof_fixture`] profile plus a second sample carrying one pprof
/// `Label{key,str}` — the way Go's `pprof.Labels` and the Pyroscope SDKs tag
/// samples.
pub fn pprof_fixture_labeled(key: &str, value: &str) -> Vec<u8> {
    let mut table = vec![
        String::new(),
        "cpu".into(),
        "nanoseconds".into(),
        "main".into(),
        "handle_request".into(),
        "/srv/app.go".into(),
    ];
    let key_index = table.len();
    table.push(key.to_string());
    let value_index = table.len();
    table.push(value.to_string());

    let mut p = Proto::new();
    let mut sample_type = Proto::new();
    sample_type.uint(1, 1);
    sample_type.uint(2, 2);
    p.bytes(1, &sample_type.0);

    for (id, name) in [(1u64, 3u64), (2, 4)] {
        let mut function = Proto::new();
        function.uint(1, id);
        function.uint(2, name);
        function.uint(4, 5);
        p.bytes(5, &function.0);
    }
    for (id, function_id, line) in [(10u64, 2u64, 12u64), (11, 1, 0)] {
        let mut line_msg = Proto::new();
        line_msg.uint(1, function_id);
        line_msg.uint(2, line);
        let mut location = Proto::new();
        location.uint(1, id);
        location.bytes(4, &line_msg.0);
        p.bytes(4, &location.0);
    }

    // The labelled sample: 500 ns with `key=value`.
    let mut label = Proto::new();
    label.uint(1, key_index as u64);
    label.uint(2, value_index as u64);
    let mut sample = Proto::new();
    sample.uint(1, 10);
    sample.uint(1, 11);
    sample.uint(2, 500);
    sample.bytes(3, &label.0);
    p.bytes(2, &sample.0);

    // An unlabelled sample: 700 ns.
    let mut plain = Proto::new();
    plain.uint(1, 10);
    plain.uint(1, 11);
    plain.uint(2, 700);
    p.bytes(2, &plain.0);

    let now = now_ns();
    p.uint(9, now - 10_000_000_000);
    p.uint(10, 10_000_000_000);
    for entry in &table {
        p.bytes(6, entry.as_bytes());
    }
    p.0
}
