//! End-to-end tests of the shipped binary.
//!
//! Everything above this file drives the library. These tests start
//! `lidlesseye` as a real process — argument parsing, tracing setup, signal
//! handling, exit codes and stdout are all part of the product, and none of
//! them are covered by an in-process test.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lidlesseye");

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lidlesseye-cli-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str], stdin: Option<&[u8]>) -> Output {
    use std::io::Write;
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn lidlesseye");
    if let Some(input) = stdin {
        child.stdin.as_mut().unwrap().write_all(input).unwrap();
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    Output {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// A `serve` child process plus the URL it announced.
struct Serving {
    child: Child,
    base_url: String,
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(args: &[&str]) -> Serving {
    serve_env(args, &[])
}

fn serve_env(args: &[&str], env: &[(&str, &str)]) -> Serving {
    let mut command = Command::new(BIN);
    command.arg("serve").args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        // An ephemeral port keeps the test suite parallel-safe; the binary
        // prints the address it actually bound.
        .args([
            "--listen",
            "127.0.0.1:0",
            "--retention",
            "0",
            "--max-disk",
            "0",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn lidlesseye serve");

    let stdout = child.stdout.take().expect("no stdout");
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut base_url = None;
    while Instant::now() < deadline {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if let Some(rest) = line.split("listening on ").nth(1) {
            base_url = Some(rest.trim().to_string());
            break;
        }
    }
    // Keep draining stdout so the child never blocks on a full pipe.
    std::thread::spawn(move || {
        let mut sink = String::new();
        while reader.read_line(&mut sink).unwrap_or(0) > 0 {
            sink.clear();
        }
    });

    let base_url = base_url.unwrap_or_else(|| {
        let _ = child.kill();
        panic!("`serve` never announced its address");
    });
    Serving { child, base_url }
}

#[test]
fn info_reports_the_build() {
    let out = run(&["info"], None);
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert!(out.stdout.contains("lidlesseye"), "{}", out.stdout);
    assert!(out.stdout.contains("LESEG v"), "{}", out.stdout);
    assert!(
        out.stdout.contains("ui assets embedded: true"),
        "the dashboard is not embedded: {}",
        out.stdout
    );
}

#[test]
fn ingest_query_and_fsck_round_trip() {
    let dir = temp_dir("roundtrip");
    let folded = dir.join("stacks.folded");
    std::fs::write(&folded, b"main;serve;handle 750\nmain;serve 250\n").unwrap();
    let data = dir.join("data");

    let out = run(
        &[
            "ingest",
            "--data-dir",
            data.to_str().unwrap(),
            "--format",
            "folded",
            "--service-name",
            "checkout",
            "--tenant",
            "acme",
            folded.to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(
        out.code,
        Some(0),
        "stdout: {}\nstderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(out.stdout.contains("checkout"), "{}", out.stdout);

    let out = run(
        &[
            "query",
            "--data-dir",
            data.to_str().unwrap(),
            "--tenant",
            "acme",
            "--from",
            "1h",
            "--top",
        ],
        None,
    );
    assert_eq!(
        out.code,
        Some(0),
        "stdout: {}\nstderr: {}",
        out.stdout,
        out.stderr
    );
    for frame in ["handle", "serve", "main"] {
        assert!(
            out.stdout.contains(frame),
            "`{frame}` missing:\n{}",
            out.stdout
        );
    }
    assert!(
        out.stdout.contains("1.0µs"),
        "the total should be 1.0µs://n{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("750.0ns") || out.stdout.contains("750ns"),
        "a sub-microsecond value must not be rounded to 0µs://n{}",
        out.stdout
    );

    // The tree view agrees, and the stack is the right way up.
    let out = run(
        &[
            "query",
            "--data-dir",
            data.to_str().unwrap(),
            "--tenant",
            "acme",
            "--from",
            "1h",
            "--tree",
        ],
        None,
    );
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    let tree = out.stdout;
    let main_at = tree.find("main").expect("main missing");
    let serve_at = tree.find("serve").expect("serve missing");
    let handle_at = tree.find("handle").expect("handle missing");
    assert!(main_at < serve_at && serve_at < handle_at, "{tree}");

    let out = run(
        &[
            "fsck",
            "--data-dir",
            data.to_str().unwrap(),
            "--tenant",
            "acme",
        ],
        None,
    );
    assert_eq!(
        out.code,
        Some(0),
        "stdout: {}\nstderr: {}",
        out.stdout,
        out.stderr
    );

    // A fresh tenant has no data and says so instead of exiting 0 silently.
    let out = run(
        &[
            "query",
            "--data-dir",
            data.to_str().unwrap(),
            "--tenant",
            "nobody",
            "--from",
            "1h",
        ],
        None,
    );
    assert_ne!(
        out.code,
        Some(0),
        "an unknown tenant must fail:\n{}",
        out.stdout
    );
    assert!(
        out.stderr.contains("nobody"),
        "the error should name the tenant: {}",
        out.stderr
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ingest_reads_json_from_stdin() {
    let dir = temp_dir("stdin");
    let data = dir.join("data");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    // The JSON wire format is columnar: a string table plus frame refs of
    // [module, symbol, file, line]. Leaf first, like the kernel reports it.
    let report = serde_json::json!({
        "service_name": "stdin-svc",
        "profile_type": "cpu",
        "start_ns": now - 1_000_000,
        "end_ns": now,
        "strings": ["", "leaf", "root"],
        "stacks": [{
            "frames": [[0, 1, 0, 0], [0, 2, 0, 0]],
            "value": 4242,
        }],
    });
    let out = run(
        &["ingest", "--data-dir", data.to_str().unwrap()],
        Some(serde_json::to_vec(&report).unwrap().as_slice()),
    );
    assert_eq!(
        out.code,
        Some(0),
        "stdout: {}\nstderr: {}",
        out.stdout,
        out.stderr
    );

    let out = run(
        &[
            "query",
            "--data-dir",
            data.to_str().unwrap(),
            "--from",
            "1h",
            "--top",
        ],
        None,
    );
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert!(out.stdout.contains("leaf"), "{}", out.stdout);
}

#[test]
fn invalid_input_fails_loudly() {
    let dir = temp_dir("invalid");
    let data = dir.join("data");

    let out = run(
        &[
            "ingest",
            "--data-dir",
            data.to_str().unwrap(),
            "--format",
            "yaml",
        ],
        Some(b"a: b"),
    );
    assert_ne!(out.code, Some(0), "an unknown format must fail");
    assert!(
        out.stderr.contains("format") || out.stderr.contains("yaml"),
        "{out_stderr}",
        out_stderr = out.stderr
    );

    let out = run(
        &[
            "ingest",
            "--data-dir",
            data.to_str().unwrap(),
            "--format",
            "folded",
        ],
        Some(b"main 10\n"),
    );
    assert_ne!(
        out.code,
        Some(0),
        "folded input without a service name must fail"
    );
    assert!(
        out.stderr.contains("service-name") || out.stderr.contains("service_name"),
        "{}",
        out.stderr
    );

    // An unknown subcommand is a clap error, exit code 2.
    let out = run(&["definitely-not-a-command"], None);
    assert_eq!(out.code, Some(2), "{}", out.stderr);
}

#[test]
fn serve_answers_over_a_real_socket() {
    let dir = temp_dir("serve");
    let data = dir.join("data");
    let server = serve(&["--data-dir", data.to_str().unwrap()]);
    let base = server.base_url.clone();
    assert!(base.starts_with("http://127.0.0.1:"), "{base}");

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let health: Value = client
        .get(format!("{base}/api/v1/health"))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(health["status"], "ok", "{health}");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let body = serde_json::json!({
        "service_name": "socket-svc",
        "profile_type": "cpu",
        "start_ns": now - 1_000_000,
        "end_ns": now,
        "strings": ["", "work"],
        "stacks": [{"frames": [[0, 1, 0, 0]], "value": 900}],
    });
    let response = client
        .post(format!("{base}/ingest"))
        .header("X-Scope-OrgID", "acme")
        .json(&body)
        .send()
        .unwrap();
    assert!(
        response.status().is_success(),
        "ingest failed: {}",
        response.text().unwrap()
    );

    let fg: Value = client
        .get(format!("{base}/api/v1/flamegraph?from=now-1h"))
        .header("X-Scope-OrgID", "acme")
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(fg["total"], 900, "{fg}");

    // The dashboard is served from the same socket.
    let index = client
        .get(format!("{base}/"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(index.contains("id=\"main\""), "dashboard missing");

    let metrics = client
        .get(format!("{base}/metrics"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(metrics.contains("lidlesseye_ingest_requests"), "{metrics}");
}

#[test]
fn serve_requires_a_token_when_one_is_configured() {
    let dir = temp_dir("serve-auth");
    let data = dir.join("data");
    let server = serve(&[
        "--data-dir",
        data.to_str().unwrap(),
        "--auth-token",
        "cli-secret",
    ]);
    let base = server.base_url.clone();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let unauthorized = client
        .get(format!("{base}/api/v1/flamegraph?from=now-1h"))
        .send()
        .unwrap();
    assert_eq!(unauthorized.status().as_u16(), 401);

    let authorized = client
        .get(format!("{base}/api/v1/flamegraph?from=now-1h"))
        .bearer_auth("cli-secret")
        .send()
        .unwrap();
    assert!(authorized.status().is_success());

    // Liveness must stay open for orchestrators.
    let health = client.get(format!("{base}/api/v1/health")).send().unwrap();
    assert!(health.status().is_success());
}

#[test]
fn ingest_can_push_to_a_running_server() {
    let dir = temp_dir("push");
    let data = dir.join("data");
    let folded = dir.join("stacks.folded");
    std::fs::write(&folded, b"root;leaf 321\n").unwrap();
    let server = serve(&["--data-dir", data.to_str().unwrap()]);
    let base = server.base_url.clone();

    let out = run(
        &[
            "ingest",
            "--server",
            &base,
            "--format",
            "folded",
            "--service-name",
            "pushed",
            "--tenant",
            "acme",
            folded.to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(
        out.code,
        Some(0),
        "stdout: {}\nstderr: {}",
        out.stdout,
        out.stderr
    );

    let client = reqwest::blocking::Client::new();
    let fg: Value = client
        .get(format!(
            "{base}/api/v1/flamegraph?from=now-1h&service_name=pushed"
        ))
        .header("X-Scope-OrgID", "acme")
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(fg["total"], 321, "{fg}");

    // `--tenant` must not have leaked into the default tenant.
    let other: Value = client
        .get(format!("{base}/api/v1/flamegraph?from=now-1h"))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(
        other["total"], 0,
        "the push went to the wrong tenant: {other}"
    );
}

#[test]
fn tenant_flags_are_wired_up() {
    let dir = temp_dir("tenants");
    let data = dir.join("data");
    // One tenant, one series, one token.
    let server = serve(&[
        "--data-dir",
        data.to_str().unwrap(),
        "--tenant-token",
        "acme-token=acme",
        "--allowed-tenant",
        "acme",
        "--tenant-limit",
        "acme:max_series=1",
        "--tenant-limit",
        "acme:retention=1h",
        "--max-tenants",
        "1",
    ]);
    let base = server.base_url.clone();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    let body = |pod: &str| {
        serde_json::json!({
            "service_name": "api",
            "labels": { "pod": pod },
            "profile_type": "cpu",
            "start_ns": now - 1_000_000,
            "end_ns": now,
            "strings": ["", "work"],
            "stacks": [{ "frames": [[0, 1, 0, 0]], "value": 100 }],
        })
    };

    // No token: the per-tenant token made authentication mandatory.
    let unauthenticated = client
        .post(format!("{base}/ingest"))
        .json(&body("p1"))
        .send()
        .unwrap();
    assert_eq!(unauthenticated.status().as_u16(), 401);

    let first = client
        .post(format!("{base}/ingest"))
        .bearer_auth("acme-token")
        .json(&body("p1"))
        .send()
        .unwrap();
    assert_eq!(first.status().as_u16(), 200, "{}", first.text().unwrap());

    // A second series exceeds max_series=1.
    let second = client
        .post(format!("{base}/ingest"))
        .bearer_auth("acme-token")
        .json(&body("p2"))
        .send()
        .unwrap();
    assert_eq!(
        second.status().as_u16(),
        429,
        "max_series was not applied: {}",
        second.text().unwrap()
    );

    // The allow-list refuses everything else.
    let other = client
        .get(format!("{base}/api/v1/flamegraph?from=now-1h"))
        .bearer_auth("acme-token")
        .header("X-Scope-OrgID", "globex")
        .send()
        .unwrap();
    assert_eq!(other.status().as_u16(), 403);

    // The limits surface through the admin endpoint.
    let tenants: Value = client
        .get(format!("{base}/api/v1/tenants"))
        .bearer_auth("acme-token")
        .send()
        .unwrap()
        .json()
        .unwrap();
    let entry = &tenants["tenants"][0];
    assert_eq!(entry["id"], "acme", "{tenants}");
    assert_eq!(entry["limits"]["max_series"], 1, "{tenants}");
    assert_eq!(entry["limits"]["retention_ns"], 3_600_000_000_000u64);
}

#[test]
fn a_bad_tenant_limit_fails_at_startup() {
    let dir = temp_dir("bad-limit");
    let data = dir.join("data");
    let out = run(
        &[
            "serve",
            "--data-dir",
            data.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--tenant-limit",
            "acme:retentoin=30d",
        ],
        None,
    );
    assert_ne!(out.code, Some(0), "a typo in a limit must not be ignored");
    assert!(
        out.stderr.contains("unknown tenant limit"),
        "{}",
        out.stderr
    );

    let out = run(
        &[
            "serve",
            "--data-dir",
            data.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--tenant-limit",
            "../evil:max_series=5",
        ],
        None,
    );
    assert_ne!(out.code, Some(0), "an invalid tenant id must not start");
}

#[test]
fn a_custom_tenant_header_is_honoured() {
    let dir = temp_dir("tenant-header");
    let data = dir.join("data");
    let server = serve(&[
        "--data-dir",
        data.to_str().unwrap(),
        "--tenant-header",
        "X-Org",
    ]);
    let base = server.base_url.clone();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    let body = serde_json::json!({
        "service_name": "api",
        "profile_type": "cpu",
        "start_ns": now - 1_000_000,
        "end_ns": now,
        "strings": ["", "work"],
        "stacks": [{ "frames": [[0, 1, 0, 0]], "value": 250 }],
    });
    let response = client
        .post(format!("{base}/ingest"))
        .header("X-Org", "Acme")
        .json(&body)
        .send()
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);

    let scoped: Value = client
        .get(format!("{base}/api/v1/flamegraph?from=now-1h"))
        .header("X-Org", "acme")
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(
        scoped["total"], 250,
        "the custom header was ignored: {scoped}"
    );

    // The Pyroscope header is not special any more, so it lands in `default`.
    let other: Value = client
        .get(format!("{base}/api/v1/flamegraph?from=now-1h"))
        .header("X-Scope-OrgID", "acme")
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(other["total"], 0, "{other}");

    let version: Value = client
        .get(format!("{base}/api/v1/version"))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(version["tenant_header"], "X-Org", "{version}");
}

#[test]
fn tenant_tokens_can_come_from_the_environment() {
    let dir = temp_dir("tenant-env");
    let data = dir.join("data");
    // What the Helm chart does: tokens in a Secret, never in the pod spec.
    let server = serve_env(
        &["--data-dir", data.to_str().unwrap()],
        &[("LIDLESSEYE_TENANT_TOKENS", "env-token=acme,other=globex")],
    );
    let base = server.base_url.clone();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let unauthorized = client
        .get(format!("{base}/api/v1/flamegraph?from=now-1h"))
        .send()
        .unwrap();
    assert_eq!(
        unauthorized.status().as_u16(),
        401,
        "an env-provided token must still enable authentication"
    );

    let authorized = client
        .get(format!("{base}/api/v1/flamegraph?from=now-1h"))
        .bearer_auth("env-token")
        .send()
        .unwrap();
    assert!(authorized.status().is_success());
}
