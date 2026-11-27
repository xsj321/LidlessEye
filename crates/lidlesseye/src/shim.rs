//! Turn the bpftime allocator reader's output into profiles.
//!
//! This runs **inside the target pod**, as an ephemeral container attached to
//! the container being profiled. That placement is not a convenience: bpftime
//! talks to the injected agent through POSIX shared memory under `/dev/shm`,
//! and every pod gets its own tmpfs, so a loader running in the DaemonSet can
//! never see the target's segment. An ephemeral container shares the target's
//! PID namespace, `/dev/shm` and libc image, which is exactly what the userspace
//! reader needs (see `docs/change-report-0.1.74-0.1.89-bpftime.zh-CN.md`).
//!
//! The shim starts the loader, attaches it to every process that maps libc,
//! resolves the addresses it prints into frames, and posts the result as
//! folded stacks to `/api/v1/ingest`. Anything that cannot be resolved is
//! dropped rather than guessed: a wrong symbol is worse than a missing one.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::time::{Duration, interval};
use tracing::{debug, info, warn};

/// Where the runtime image keeps the reader; `deploy/docker/Dockerfile` copies
/// the bundle here in its bpftime stage.
const LOADER: &str = "/opt/bpftime/lidlesseye-bpftime";
const OBJECT: &str = "/opt/bpftime/alloc.bpf.o";
const SERVER_LIB: &str = "/opt/bpftime/libbpftime-syscall-server.so";
const ATTACH: &str = "/opt/bpftime/bpftime";

/// One line of the loader's JSON output.
///
/// `{"pid":74,"bytes":1706786816,"count":416696,"stack":"0x5d77…,0x5d78…"}`
/// and, once per interval, `{"hits":true,"pid":74,"calls":1450,"samples":23}`.
/// The hit line carries no bytes, so `bytes == 0` is the "status, not sample"
/// marker. A tick also ends with `{"batch":7,"rows":31}`, which is how the shim
/// proves it received every row the loader produced.
#[derive(Debug, Deserialize)]
struct LoaderSample {
    pid: u32,
    #[serde(default)]
    bytes: u64,
    #[serde(default)]
    count: u64,
    #[serde(default)]
    stack: String,
    /// Present only on the end-of-tick marker.
    #[serde(default)]
    batch: u64,
    #[serde(default)]
    rows: u64,
}

/// Cap on distinct stacks held while the server is unreachable.
///
/// The loader keeps accumulating in shared memory either way; what must not
/// happen is the shim growing until the container is OOM-killed, because then
/// the whole reader disappears instead of just losing history.
const MAX_PENDING_STACKS: usize = 4096;

#[derive(Debug)]
pub struct ShimArgs {
    /// Base URL of the server, e.g. `http://le-disk-lidlesseye:4040`.
    pub server: String,
    /// Tenant the reports belong to (`X-Scope-OrgID`).
    pub tenant: Option<String>,
    /// `service_name` stored with every report; the workload is the useful
    /// answer to "whose allocations are these".
    pub service: String,
    /// How often the loader drains its maps.
    pub interval_secs: u64,
    /// How often to post what has accumulated.
    pub flush_secs: u64,
}

pub async fn run(args: ShimArgs) -> Result<()> {
    if !std::path::Path::new(LOADER).exists() {
        anyhow::bail!("{LOADER} is missing; this image has no bpftime reader");
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut pending: BTreeMap<String, u64> = BTreeMap::new();
    let mut tick = interval(Duration::from_secs(args.flush_secs.max(1)));
    let mut samples: u64 = 0;
    let mut unresolved: u64 = 0;
    let mut received: u64 = 0;
    let mut rows_since_batch: u64 = 0;
    let mut lost: u64 = 0;
    let mut dropped: u64 = 0;
    let mut restarts: u64 = 0;

    // The loader is supervised rather than run once: an ephemeral container
    // never restarts, so if the loader dies and the shim returns, the pod keeps
    // running with no reader and no way to get one back short of deleting it.
    // Restarting the child keeps the failure "a few seconds of samples" instead
    // of "silently nothing until someone notices".
    loop {
        let (mut child, mut lines_rx) = spawn_loader(&args).await?;
        restarts += 1;
        if restarts > 1 {
            warn!(restarts, "restarted the allocator loader");
        }

    loop {
        tokio::select! {
            line = lines_rx.recv() => {
                let Some(line) = line else {
                    warn!("the loader exited; restarting it");
                    break;
                };
                let Ok(sample) = serde_json::from_str::<LoaderSample>(line.trim()) else {
                    debug!(line = %line, "ignoring a line that is not a sample");
                    continue;
                };
                // End-of-tick marker: the loader says how many rows it printed,
                // so a short read becomes visible instead of looking idle.
                if sample.batch > 0 {
                    if rows_since_batch != sample.rows {
                        let delta = sample.rows as i64 - rows_since_batch as i64;
                        lost += delta.unsigned_abs();
                        warn!(
                            batch = sample.batch,
                            expected = sample.rows,
                            seen = rows_since_batch,
                            "the loader printed a different number of rows than the shim read"
                        );
                    }
                    rows_since_batch = 0;
                    continue;
                }
                if !line.trim().starts_with("{\"hits\"") {
                    received += 1;
                    rows_since_batch += 1;
                }
                if sample.bytes == 0 {
                    debug!(pid = sample.pid, count = sample.count, "loader status");
                    continue;
                }
                match folded_stack(sample.pid, &sample.stack) {
                    Some(stack) => {
                        if pending.len() >= MAX_PENDING_STACKS && !pending.contains_key(&stack) {
                            // Drop instead of growing without bound: one
                            // missing stack is a hole in one graph, an OOM-kill
                            // is the end of the reader.
                            dropped += 1;
                            continue;
                        }
                        *pending.entry(stack).or_default() += sample.bytes;
                        samples += 1;
                    }
                    None => unresolved += 1,
                }
            }
            _ = tick.tick() => {
                // Anything that appeared since the last tick (a restarted
                // workload, a forked worker) has to be picked up as well, or
                // the reader would only ever cover the processes that existed
                // when it started.
                let stats = attach_candidates();
                // The server cannot see inside this pod, so it cannot tell
                // "switched on and working" from "switched on and attached to
                // nothing". Send what we know, every tick.
                post_coverage(&client, &args, &stats).await;
                eprintln!(
                    "lidlesseye-shim: tick received={received} stacks={} unresolved={unresolved} lost={lost} dropped={dropped} detected={} attached={}",
                    pending.len(),
                    stats.detected,
                    stats.attached
                );
                if pending.is_empty() {
                    continue;
                }
                let body: String = pending
                    .iter()
                    .map(|(stack, bytes)| format!("{stack} {bytes}\n"))
                    .collect();
                match post(&client, &args, &body).await {
                    Ok(()) => {
                        eprintln!(
                            "lidlesseye-shim: posted stacks={} samples={samples} unresolved={unresolved}",
                            pending.len()
                        );
                        debug!(stacks = pending.len(), samples, unresolved, "posted allocator stacks");
                        pending.clear();
                    }
                    Err(err) => {
                        eprintln!("lidlesseye-shim: post failed: {err}");
                        warn!(error = %err, "posting allocator stacks failed; keeping them")
                    }
                }
            }
        }
    }
        // The child is gone (crash, map error, someone killed it): clean it up
        // and go around again. A restart costs the two seconds below and keeps
        // the reader live; returning here would end the container instead, and
        // Kubernetes does not restart ephemeral containers.
        let _ = child.kill();
        let _ = child.wait();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Start the loader and hand back a line stream from its stdout.
///
/// Split out of [`run`] so the supervisor loop can start a fresh loader after
/// a crash without duplicating the setup, and so the two ordering rules stay in
/// one place: the loader must exist before the first attach, and its stdout
/// must be drained on a thread (the workspace has no tokio `process` feature).
async fn spawn_loader(
    args: &ShimArgs,
) -> Result<(
    std::process::Child,
    tokio::sync::mpsc::UnboundedReceiver<String>,
)> {
    let mut child = Command::new(LOADER)
        .arg(OBJECT)
        .arg(args.interval_secs.to_string())
        .env("LD_PRELOAD", SERVER_LIB)
        // bpftime writes its own diagnostics to `~/.bpftime/runtime.log` by
        // default, and this container's root is read-only: without this the
        // only clue that a probe was never registered is silence.
        .env("BPFTIME_LOG_OUTPUT", "/dev/stderr")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("starting {LOADER}"))?;
    // The loader creates the shared-memory segment the injected agent reports
    // to, so it has to be up before the first attach: attaching first leaves an
    // agent with no server to hand counters to.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    attach_candidates();
    let stdout = child
        .stdout
        .take()
        .context("the loader did not expose stdout")?;
    let (lines_tx, lines_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::Builder::new()
        .name("bpftime-lines".to_string())
        .spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if lines_tx.send(line).is_err() {
                    break;
                }
            }
        })?;
    info!(
        service = %args.service,
        interval = args.interval_secs,
        "the allocator loader is running"
    );
    Ok((child, lines_rx))
}

/// Ask the injected agent to intercept `malloc` in every process that maps libc.
///
/// The loader is attached to processes, not to the pod: a container may hold
/// several, and a sidecar we do not care about would only cost traps. Failures
/// are expected (short-lived processes, static binaries, musl) and are logged
/// at debug level.
///
/// Called again on every tick, because a process that starts after the shim
/// (a restarted workload, a forked worker) is exactly the case a one-shot scan
/// misses. A pid that already carries the injected library is skipped: the
/// maps check is cheaper and more reliable than asking the CLI to re-attach.
/// What one pass over `/proc` found and did.
///
/// The counts are the difference between "the reader is switched on" and "the
/// reader is attached to something". They go back to the server so the memory
/// page can show coverage instead of implying it.
#[derive(Debug, Default)]
struct AttachStats {
    detected: u64,
    attached: u64,
    skipped: std::collections::BTreeMap<String, u64>,
}

impl AttachStats {
    fn skip(&mut self, reason: &str) {
        *self.skipped.entry(reason.to_string()).or_default() += 1;
    }
}

fn attach_candidates() -> AttachStats {
    let me = std::process::id();
    let mut stats = AttachStats::default();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == me || pid == 1 {
            continue;
        }
        let maps = match std::fs::read_to_string(format!("/proc/{pid}/maps")) {
            Ok(maps) => maps,
            Err(_) => continue,
        };
        let musl = maps.contains("libc.musl-");
        if !maps.contains("libc.so") && !musl {
            continue;
        }
        stats.detected += 1;
        if musl {
            // The probe hooks libc's `malloc`; a musl binary never calls it.
            stats.skip("musl");
            continue;
        }
        if maps.contains("libbpftime-agent") {
            stats.attached += 1;
            continue;
        }
        // `--install-location` is not optional for the CLI: without it, and
        // without `HOME` to fall back on, it prints its usage and exits 1
        // before touching the target. An ephemeral container has no HOME, so
        // both are spelled out.
        let status = Command::new(ATTACH)
            .args(["--install-location", "/root/.bpftime", "attach"])
            .arg(pid.to_string())
            .env("HOME", "/root")
            .env("BPFTIME_LOG_OUTPUT", "/dev/stderr")
            .status();
        match status {
            Ok(status) if status.success() => {
                stats.attached += 1;
                eprintln!("lidlesseye-shim: injected pid {pid}");
                info!(pid, "injected the allocator reader");
            }
            Ok(status) => {
                stats.skip("attach-failed");
                eprintln!("lidlesseye-shim: attach failed for pid {pid}: {status}");
                debug!(pid, %status, "could not inject");
            }
            Err(err) => {
                stats.skip("attach-failed");
                eprintln!("lidlesseye-shim: could not run {ATTACH}: {err}");
                debug!(pid, %err, "could not inject");
            }
        }
    }
    stats
}

/// Resolve the loader's address list into a folded stack, or `None`.
///
/// The loader prints frames leaf first; folded stacks are root first, so the
/// list is reversed. A stack whose first frame resolves to nothing is dropped:
/// posting `unknown` frames would put every failed symbolization into one
/// gigantic bogus leaf.
fn folded_stack(pid: u32, stack: &str) -> Option<String> {
    let ips: Vec<u64> = stack
        .split(',')
        .filter_map(|ip| {
            let ip = ip.trim();
            u64::from_str_radix(ip.trim_start_matches("0x"), 16).ok()
        })
        .collect();
    if ips.is_empty() {
        return None;
    }
    let frames = lidlesseye_ebpf::resolve_user_stack(pid, &ips);
    if frames.is_empty() {
        return None;
    }
    let names: Vec<&str> = frames
        .iter()
        .rev()
        .map(|frame| {
            if !frame.symbol.is_empty() {
                frame.symbol.as_str()
            } else if !frame.module.is_empty() {
                frame.module.as_str()
            } else {
                "unknown"
            }
        })
        .collect();
    if names.iter().all(|name| *name == "unknown") {
        return None;
    }
    Some(names.join(";"))
}

/// Tell the server how many of this workload's processes the reader reached.
///
/// Failure is logged and dropped on purpose: coverage is a diagnostic, and a
/// reader that stops sampling because it could not report its own bookkeeping
/// would be worse than one whose coverage line is stale.
async fn post_coverage(client: &reqwest::Client, args: &ShimArgs, stats: &AttachStats) {
    let url = format!("{}api/v1/memory/coverage", server_base(&args.server));
    let (process_count, process_rss, process_pss) = match pod_slice() {
        Some(uid) => {
            let (count, rss, pss) = process_memory(&uid);
            (Some(count), Some(rss), Some(pss))
        }
        None => (None, None, None),
    };
    let body = serde_json::json!({
        "workload": args.service,
        "detected": stats.detected,
        "attached": stats.attached,
        "skipped": stats.skipped,
        "memory_bytes": pod_memory_bytes(),
        "process_count": process_count,
        "process_rss_bytes": process_rss,
        "process_pss_bytes": process_pss,
    });
    let mut request = client.post(&url).json(&body);
    if let Some(tenant) = &args.tenant {
        request = request.header("X-Scope-OrgID", tenant.clone());
    }
    if let Err(err) = request.send().await {
        debug!(error = %err, "posting coverage failed");
    }
    post_capability(client, args, stats).await;
    post_processes(client, args).await;
}

/// Report what the native reader's state is, and why.
///
/// The switchboard says a workload is switched on; only this process knows
/// whether the injection landed, and whether it failed because the target is
/// musl, because ptrace was refused, or because there was nothing to attach
/// to. Reporting the reason is the difference between an operator fixing it and
/// an operator guessing.
async fn post_capability(client: &reqwest::Client, args: &ShimArgs, stats: &AttachStats) {
    let (state, reason) = if stats.detected == 0 {
        ("unsupported", Some("no_target_process"))
    } else if stats.attached > 0 {
        ("active", None)
    } else if stats.skipped.contains_key("musl") {
        ("unsupported", Some("unsupported_libc"))
    } else {
        ("attaching", Some("attach_failed"))
    };
    let body = serde_json::json!([{
        "workload": args.service,
        // The pod name, so coverage can be counted per pod before it is counted
        // per process: "two of three pods" is what an operator needs, and a
        // single process count cannot express it.
        "pod": std::env::var("LIDLESSEYE_POD")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_default(),
        "reader": "native_alloc",
        "state": state,
        "reason": reason,
        "detected": stats.detected,
        "attached": stats.attached,
        "at_ns": now_ns(),
    }]);
    let url = format!("{}api/v1/capabilities", server_base(&args.server));
    let mut request = client.post(&url).json(&body);
    if let Some(tenant) = &args.tenant {
        request = request.header("X-Scope-OrgID", tenant.clone());
    }
    if let Err(err) = request.send().await {
        debug!(error = %err, "posting capabilities failed");
    }
}

/// Wall clock in nanoseconds, the unit every report field uses.
fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// The processes this pod is running, largest resident set first.
///
/// `VmRSS` from `/proc/<pid>/status` is one short read and cheap enough for
/// every process; `smaps_rollup` walks page tables and is therefore read only
/// for the `LIMIT` largest, which is also the only set the UI lists. A process
/// whose files cannot be read is reported with a reason instead of zero: a
/// missing number and a small number are not the same thing.
fn process_rows(limit: usize) -> (Vec<serde_json::Value>, u64) {
    // `starttime` is in clock ticks; 100 is the x86 default. It is used for
    // identity (a restarted process must not inherit the old pid's row), not
    // for arithmetic on wall time, so a host configured differently only
    // shifts it uniformly.
    const HZ_ASSUMED: u64 = 100;
    let mut rows: Vec<(u64, u32, String, u64)> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return (Vec::new(), 0);
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid <= 1 || pid == std::process::id() {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            continue;
        };
        let rss_kb = field_kb(&status, "VmRSS").unwrap_or(0);
        let comm = status
            .lines()
            .find(|line| line.starts_with("Name:"))
            .map(|line| line.trim_start_matches("Name:").trim().to_string())
            .unwrap_or_default();
        let ticks = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|text| {
                let rest = text.rsplit_once(')')?.1;
                rest.split_whitespace().nth(19)?.parse::<u64>().ok()
            })
            .unwrap_or(0);
        rows.push((rss_kb, pid, comm, ticks));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    let omitted = rows.len().saturating_sub(limit) as u64;
    rows.truncate(limit);
    let mut out = Vec::with_capacity(rows.len());
    for (rss_kb, pid, comm, ticks) in rows {
        let executable = std::fs::read_link(format!("/proc/{pid}/exe"))
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default();
        let measured = rollup(pid);
        out.push(serde_json::json!({
            "pid": pid,
            "start_ns": ticks.saturating_mul(1_000_000_000 / HZ_ASSUMED),
            "comm": comm,
            "executable": executable,
            "rss_bytes": rss_kb.saturating_mul(1024),
            "pss_bytes": measured.map(|value| value.0),
            "private_dirty_bytes": measured.map(|value| value.1),
            "swap_bytes": measured.map(|value| value.2),
            "missing": if measured.is_none() { Some("smaps_rollup not readable") } else { None },
        }));
    }
    (out, omitted)
}

/// `Pss`, `Private_Dirty` and `Swap` from `smaps_rollup`, in bytes.
fn rollup(pid: u32) -> Option<(u64, u64, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    let pss = field_kb(&text, "Pss")?.saturating_mul(1024);
    let private_dirty = field_kb(&text, "Private_Dirty").unwrap_or(0).saturating_mul(1024);
    let swap = field_kb(&text, "Swap").unwrap_or(0).saturating_mul(1024);
    Some((pss, private_dirty, swap))
}

/// Send the pod's top processes so the memory page can name them.
async fn post_processes(client: &reqwest::Client, args: &ShimArgs) {
    let (processes, omitted) = process_rows(20);
    if processes.is_empty() {
        return;
    }
    let body = serde_json::json!({
        "workload": args.service,
        "pod": std::env::var("LIDLESSEYE_POD")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_default(),
        "at_ns": now_ns(),
        "processes": processes,
        "omitted": omitted,
    });
    let url = format!("{}api/v1/memory/processes", server_base(&args.server));
    let mut request = client.post(&url).json(&body);
    if let Some(tenant) = &args.tenant {
        request = request.header("X-Scope-OrgID", tenant.clone());
    }
    if let Err(err) = request.send().await {
        debug!(error = %err, "posting processes failed");
    }
}

/// The pod's memory usage in bytes, as its own cgroup reports it.
///
/// cgroup v2 exposes a single `memory.current`; v1 keeps the number one level
/// down. This is the coarsest useful L0 figure — the whole pod, every container
/// — but it is measured, not estimated, and it costs one file read.
fn pod_memory_bytes() -> Option<u64> {
    for path in [
        "/sys/fs/cgroup/memory.current",
        "/sys/fs/cgroup/memory/memory.usage_in_bytes",
    ] {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(bytes) = text.trim().parse::<u64>() {
                return Some(bytes);
            }
        }
    }
    None
}

/// The pod's own slice inside `/proc/<pid>/cgroup`.
///
/// Matching a process to this pod by walking the pod index is not available
/// here (the shim has no Kubernetes client), but it is not needed either: the
/// shim shares the target pod's PID namespace, so every process it can see is
/// in this pod. The slice name is still read because the ephemeral container
/// itself sits in a sibling slice of the same pod.
fn pod_slice() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in text.lines() {
        let Some(path) = line.rsplit(':').next() else {
            continue;
        };
        for segment in path.split('/') {
            // The slice is not always called `pod<uid>.slice`: systemd prefixes
            // it with the accounting hierarchy, as in
            // `kubelet-kubepods-besteffort-pod<uid>.slice`. Take the last `pod`
            // in the segment, because `kubepods` contains the word as well.
            let Some(index) = segment.rfind("pod") else {
                continue;
            };
            // `pod<uid>.slice` in v2, `pod<uid>` in v1, sometimes followed by a
            // container id. The uid itself is what identifies the pod.
            let uid = segment[index + 3..]
                .split(['.', '/'])
                .next()
                .unwrap_or_default();
            if !uid.is_empty() {
                return Some(uid.to_string());
            }
        }
    }
    None
}

/// RSS and PSS summed over the pod's processes.
///
/// `VmRSS` from `/proc/<pid>/status` is cheap and always available; `Pss` needs
/// `/proc/<pid>/smaps_rollup`, which a pod's sidecar within the same PID
/// namespace can read. Pages shared between processes are split in PSS, so it is
/// the number that adds up across a pod without double counting a shared libc.
fn process_memory(uid: &str) -> (u64, u64, u64) {
    let mut count = 0u64;
    let mut rss = 0u64;
    let mut pss = 0u64;
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let cgroup = match std::fs::read_to_string(format!("/proc/{pid}/cgroup")) {
            Ok(text) => text,
            Err(_) => continue,
        };
        if !cgroup.contains(uid) {
            continue;
        }
        count += 1;
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
            if let Some(kb) = field_kb(&status, "VmRSS:") {
                rss = rss.saturating_add(kb.saturating_mul(1024));
            }
        }
        if let Ok(rollup) = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) {
            if let Some(kb) = field_kb(&rollup, "Pss:") {
                pss = pss.saturating_add(kb.saturating_mul(1024));
            }
        }
    }
    (count, rss, pss)
}

/// The kilobytes that follow `key` in a `/proc` file.
fn field_kb(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find(|line| line.starts_with(key))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u64>().ok())
}

async fn post(client: &reqwest::Client, args: &ShimArgs, body: &str) -> Result<()> {    // Stamp the batch with the interval it covers. Without these the server
    // dates a folded upload from its own clock at decode time and the window
    // ends up tens of minutes in the past, so a dashboard asking for the last
    // five minutes sees nothing even while samples flow.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let span = args.flush_secs.saturating_mul(1_000_000_000);
    let from = now.saturating_sub(span);
    // The labels say how the numbers were produced. A reader that samples and
    // scales (1 in 64, multiplied back up) cannot be compared to a count from
    // an unsampled reader unless that is visible where the stack lands.
    let url = format!(
        "{}?format=folded&profile_type=alloc_space&service_name={}&from={}&until={}\
         &label=alloc.reader=bpftime-userspace&label=alloc.sampling=poisson-1%2F64&label=temporality=delta",
        ingest_url(&args.server),
        encode(&args.service),
        from,
        now
    );
    let mut request = client.post(&url).header("content-type", "text/plain").body(body.to_string());
    if let Some(tenant) = &args.tenant {
        request = request.header("X-Scope-OrgID", tenant.clone());
    }
    let response = request.send().await?;
    if !response.status().is_success() {
        anyhow::bail!("ingest answered {}: {}", response.status(), response.text().await?);
    }
    Ok(())
}

/// Build the ingest URL from whatever the agent was told the server is.
///
/// Two wrong guesses have already cost a debugging session each: appending
/// `/api/v1/ingest` to an agent endpoint that was already `/ingest` produced
/// `<host>/ingest/api/v1/ingest`, and appending a bare `/ingest` to a plain
/// server root hit the UI's catch-all, which answers `200 text/html`. A post
/// that "succeeds" while storing nothing is worse than a 404, so the only
/// accepted shorthands are the endpoints we know exist.
fn ingest_url(server: &str) -> String {
    let base = server.trim_end_matches('/');
    // The server serves ingest at `/ingest`; `/api/v1/ingest` is a 404 that the
    // UI's catch-all can still answer with `200 text/html`, so strip any
    // versioned suffix instead of preserving it.
    if let Some(root) = base.strip_suffix("/api/v1/ingest") {
        return format!("{root}/ingest");
    }
    if let Some(root) = base.strip_suffix("/v1/ingest") {
        return format!("{root}/ingest");
    }
    if base.ends_with("/ingest") {
        return base.to_string();
    }
    format!("{base}/ingest")
}

/// The server root with a trailing slash, so a path can be appended.
///
/// The publish endpoint is configured as `http://host:4040/ingest`, not as a
/// bare host, so the ingest path has to come off before anything else is added.
fn server_base(server: &str) -> String {
    let base = ingest_url(server);
    let root = base
        .strip_suffix("/ingest")
        .unwrap_or(server.trim_end_matches('/'));
    format!("{}/", root.trim_end_matches('/'))
}

/// Percent-encode the few characters a service name can contain.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::encode;

    #[test]
    fn encodes_a_workload_name() {
        assert_eq!(encode("disk/jvm-churn"), "disk%2Fjvm-churn");
        assert_eq!(encode("plain"), "plain");
    }
}
