//! JVM heap profiling through the runtime's own event recorder.
//!
//! A JVM has sampled its own allocations since Java 11: the Java Flight
//! Recorder (JFR) emits `jdk.ObjectAllocationSample` for allocations above a
//! threshold, with the allocating stack. Turning that into profiles needs no
//! instrumentation of the target and no change to the application: a recording
//! is started on the *running* process through the attach mechanism, dumped to a
//! temporary file, read back with `jfr`, and stopped again.
//!
//! ## Why this needs `nsenter`
//!
//! The attach handshake is a unix socket the target creates at
//! `/tmp/.java_pid<pid>`, resolved in the *attaching* process's mount
//! namespace. The agent runs on the node, every workload in its own container,
//! so running `jcmd` in the agent's namespaces looks for the socket in the
//! agent's `/tmp` and finds nothing. Entering the target's mount, PID, IPC and
//! UTS namespaces first — `nsenter -t <pid> -m -p -i -u` — makes the path
//! resolve where the JVM actually created it, and `jcmd` names the target by its
//! pid *inside* that namespace, which is the last field of `NSpid`.
//!
//! ## Requirements
//!
//! * `nsenter` (util-linux) in the agent image, and `CAP_SYS_ADMIN`: the agent's
//!   DaemonSet is privileged, so this holds.
//! * A JDK inside the *target* image, because the tools are executed in its
//!   namespaces: `jcmd` and `jfr` ship with a JDK, not with a JRE, and
//!   `jfr print --json` needs JDK 17 or newer. The binaries are located under
//!   the target's `/proc/<pid>/root`, so no assumption about the image is baked
//!   in beyond the handful of usual install prefixes.
//! * A writable `/tmp` in the target for the dump and the settings file.
//!
//! When any of this is missing the reader reports why and stays silent; a
//! cluster of JRE-only images simply produces no `runtime=java` series.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use lidlesseye_core::model::Frame;
use tracing::{debug, info};

/// Name of the recording this reader owns. Fixed so that a restarted agent
/// adopts the recording a previous incarnation left behind instead of starting a
/// second one and doubling the JVM's sampling work.
const RECORDING: &str = "lidlesseye";

/// Settings file written into the target's `/tmp`.
///
/// `settings=profile` alone emits thousands of allocation samples per second,
/// and every one of them carries a full stack: one twelve-second window of a
/// trivial allocation loop measured 3 596 events and 47 MB of JSON. A low
/// throttle keeps the profile statistically useful while bounding both the JVM's
/// own work and what this reader has to parse.
const SETTINGS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<configuration version="2.0" label="LidlessEye" description="Allocation samples for continuous profiling">
  <event name="jdk.ObjectAllocationSample">
    <setting name="enabled">true</setting>
    <setting name="stackTrace">true</setting>
    <setting name="throttle">5/s</setting>
  </event>
  <event name="jdk.OldObjectSample">
    <setting name="enabled">true</setting>
    <setting name="stackTrace">true</setting>
    <setting name="cutoff">1 h</setting>
  </event>
</configuration>
"#;

/// How many distinct allocation stacks to keep per process per interval.
const MAX_STACKS: usize = 4000;

/// Largest `jfr print --json` document this reader will parse, in bytes.
///
/// The document is materialised twice — once as text, once as a value tree — so
/// the real cost is roughly ten times this. The agent's memory limit is a
/// gigabyte and a half shared with the eBPF maps, so a single dump is capped at
/// 24 MiB: at the settings file's throttle that is minutes of events, and a
/// larger document means the throttle was not applied.
const MAX_DUMP_JSON: usize = 24 * 1024 * 1024;

/// Upper bound on one allocation sample's `weight`, in bytes.
///
/// JFR estimates the size of the allocations it did not see individually, and
/// for a thread that allocated continuously since its last sample the estimate
/// is the whole interval's total. Clamping at 2 GiB keeps a single sample from
/// dominating a profile while leaving ordinary samples untouched.
const MAX_EVENT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Where a JDK usually keeps `jcmd`, relative to the target's root.
const BIN_DIRS: &[&str] = &[
    "/opt/java/openjdk/bin",
    "/opt/jdk/bin",
    "/usr/lib/jvm/default-jvm/bin",
    "/usr/local/openjdk-21/bin",
    "/usr/local/openjdk-17/bin",
    "/usr/bin",
    "/usr/local/bin",
];

/// One process's contribution to the memory profiles.
#[derive(Debug, Clone, PartialEq)]
pub struct JvmSample {
    pub tgid: u32,
    /// Frames exactly as the JVM named them (class and method), leaf first.
    pub frames: Vec<Frame>,
    /// Bytes allocated by this stack since the previous scan, as weighted by
    /// the JVM's sampler.
    pub alloc_bytes: u64,
    /// Bytes still held by objects the recorder sampled as survivors.
    pub inuse_bytes: u64,
}

/// Everything needed to talk to one JVM, resolved once per process.
#[derive(Debug, Clone)]
struct Target {
    tgid: u32,
    /// The pid the JVM has inside its own PID namespace.
    inner: String,
    /// `jcmd` as an absolute path valid *inside* the target's namespaces.
    jcmd: PathBuf,
    /// Dump file as an absolute path valid inside the target's namespaces.
    dump: String,
}

/// Discovery and per-process state for the JVM heap reader.
pub struct JvmHeap {
    targets: HashMap<u32, Target>,
    /// Processes whose recording this reader started.
    started: HashSet<u32>,
    /// Set once when the reader cannot exist here at all.
    pub unavailable: Option<&'static str>,
    /// Dumps that produced no events, cumulative.
    pub empty: u64,
    /// Processes that could not be attached to, cumulative.
    pub attach_failed: u64,
    /// Processes left alone because their workload was not switched on.
    pub not_requested: u64,
    /// Why processes were left out: `no jdk`, `nsenter`, `attach`, `readonly`.
    pub skipped: BTreeMap<&'static str, u64>,
    nsenter: Option<PathBuf>,
}

impl Default for JvmHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl JvmHeap {
    pub fn new() -> Self {
        let nsenter = which("nsenter");
        let unavailable = match &nsenter {
            None => Some("nsenter is not on PATH, so container namespaces cannot be entered"),
            Some(_) => None,
        };
        if let Some(reason) = unavailable {
            info!(reason, "JVM heap reader unavailable");
        }
        JvmHeap {
            targets: HashMap::new(),
            started: HashSet::new(),
            unavailable,
            empty: 0,
            attach_failed: 0,
            not_requested: 0,
            skipped: BTreeMap::new(),
            nsenter,
        }
    }

    /// Whether the namespaces this reader needs can be entered at all. A JDK in
    /// the target is discovered per process, because it is the target's image
    /// that has to carry it.
    pub fn available(&self) -> bool {
        self.unavailable.is_none()
    }

    /// Read every JVM whose workload has been switched on, returning what it
    /// allocated since the previous scan.
    ///
    /// `allowed` decides per process: attaching costs the target a pause and
    /// starts a recording it did not ask for, so a JVM nobody asked about is
    /// never touched — not even to check whether it is attachable.
    pub fn scan(
        &mut self,
        exclude: &[u32],
        alive: &mut Vec<u32>,
        allowed: &dyn Fn(u32) -> bool,
    ) -> Vec<JvmSample> {
        alive.clear();
        if self.unavailable.is_some() {
            return Vec::new();
        }
        let mut samples = Vec::new();
        let pids = jvm_pids();
        let found = pids.len();
        for tgid in pids {
            if exclude.contains(&tgid) {
                continue;
            }
            if !allowed(tgid) {
                self.not_requested += 1;
                continue;
            }
            let target = match self.target(tgid) {
                Some(target) => target,
                None => continue,
            };
            match self.scan_one(&target) {
                Ok(mut per_stack) => {
                    // Alive, even when this scan produced nothing: the first
                    // scan only starts the recording, and dropping it here
                    // would restart the recording on every interval and never
                    // dump one.
                    alive.push(tgid);
                    if per_stack.is_empty() {
                        self.empty += 1;
                        continue;
                    }
                    samples.append(&mut per_stack);
                }
                Err(reason) => {
                    *self.skipped.entry(reason).or_default() += 1;
                }
            }
        }
        let live: HashSet<u32> = alive.iter().copied().collect();
        self.targets.retain(|pid, _| live.contains(pid));
        self.started.retain(|pid| live.contains(pid));
        info!(
            found = found,
            alive = alive.len(),
            samples = samples.len(),
            empty = self.empty,
            not_requested = self.not_requested,
            skipped = ?self.skipped,
            "jvm scan"
        );
        samples
    }

    /// Resolve — once — how to reach this process's JDK.
    fn target(&mut self, tgid: u32) -> Option<Target> {
        if let Some(target) = self.targets.get(&tgid) {
            return Some(target.clone());
        }
        let root = PathBuf::from(format!("/proc/{tgid}/root"));
        let mut jcmd = None;
        for dir in BIN_DIRS {
            let candidate = root.join(dir.trim_start_matches('/')).join("jcmd");
            if candidate.is_file() {
                jcmd = Some(PathBuf::from(*dir).join("jcmd"));
                break;
            }
        }
        let Some(jcmd) = jcmd else {
            *self.skipped.entry("no jdk").or_default() += 1;
            return None;
        };
        let inner = inner_pid(tgid).unwrap_or_else(|| tgid.to_string());
        // The dump and the settings file live in the target's own `/tmp`: the
        // agent's root filesystem is read-only, and `jfr` has to read the file
        // from inside the target's mount namespace anyway.
        let target = Target {
            tgid,
            inner,
            jcmd,
            dump: format!("/tmp/lidlesseye-{tgid}.jfr"),
        };
        if !self.ensure_settings(&target) {
            *self.skipped.entry("readonly /tmp").or_default() += 1;
            return None;
        }
        self.targets.insert(tgid, target.clone());
        Some(target)
    }

    /// Write the settings file, once per process. A JDK image has a writable
    /// `/tmp` (the container runtime needs one); a read-only root with a
    /// read-only `/tmp` cannot host the dump either, so it is skipped here
    /// instead of failing on every scan.
    fn ensure_settings(&self, target: &Target) -> bool {
        let path = format!("/tmp/lidlesseye.jfc");
        // Rewritten on every attach rather than kept if present: the file lives
        // in the target's `/tmp`, outlives this agent, and a stale copy from an
        // older release is invisible — it silently keeps the old throttle.
        let Some(nsenter) = self.nsenter.as_ref() else {
            return false;
        };
        let mut child = match Command::new(nsenter)
            .args(["-t", &target.tgid.to_string(), "-m", "-p", "-i", "-u", "--"])
            .args(["/bin/sh", "-c"])
            .arg(format!("cat > {path}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return false,
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(SETTINGS.as_bytes());
        }
        child.wait().map(|s| s.success()).unwrap_or(false)
    }

    /// Dump-and-reset one JVM.
    fn scan_one(&mut self, target: &Target) -> Result<Vec<JvmSample>, &'static str> {
        if !self.started.contains(&target.tgid) {
            return if self.start(target) {
                Ok(Vec::new())
            } else {
                Err("attach")
            };
        }
        if !self.dump(target) {
            // The recording died with a restarted JVM, or something else took
            // it over. Start over rather than dump a missing file forever.
            self.started.remove(&target.tgid);
            return Ok(Vec::new());
        }
        let parsed = self.parse(target)?;
        // Stop *after* reading, and start again, so that the next interval sees
        // the events of that interval only. A recording left running would make
        // every dump cumulative and each stack's total grow without bound.
        let _ = self.jcmd(
            target,
            "JFR.stop",
            &[format!("name={RECORDING}")],
        );
        self.remove_dump(target);
        if !self.start(target) {
            self.started.remove(&target.tgid);
        }
        Ok(parsed)
    }

    fn remove_dump(&self, target: &Target) {
        let Some(nsenter) = self.nsenter.as_ref() else {
            return;
        };
        let _ = Command::new(nsenter)
            .args(["-t", &target.tgid.to_string(), "-m", "-p", "-i", "-u", "--"])
            .args(["rm", "-f", &target.dump])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    fn start(&mut self, target: &Target) -> bool {
        if !self.ensure_settings(target) {
            info!(tgid = target.tgid, "cannot write the JFR settings file");
            self.attach_failed += 1;
            return false;
        }
        // A JVM can accumulate recordings: every agent incarnation that attached
        // before this one left its own behind, they all match `name=lidlesseye`,
        // and the dump then carries all of them at once. One recording is the
        // contract, so every one of them is stopped before ours starts.
        self.stop_all(target);
        let out = self.jcmd(
            target,
            "JFR.start",
            &[
                format!("name={RECORDING}"),
                "settings=/tmp/lidlesseye.jfc".to_string(),
                "disk=false".to_string(),
            ],
        );
        let Some(out) = out else {
            self.attach_failed += 1;
            return false;
        };
        if out.contains("Started recording") {
            self.started.insert(target.tgid);
            return true;
        }
        debug!(tgid = target.tgid, output = %out.trim(), "cannot start a JFR recording");
        info!(tgid = target.tgid, output = %out.trim(), "cannot start a JFR recording");
        self.attach_failed += 1;
        false
    }

    /// Stop every recording this reader owns, not just one.
    ///
    /// `JFR.stop name=…` stops a single recording, so a JVM that accumulated
    /// several leaves the rest running; each of them samples, so the JVM pays
    /// several times the cost and every dump carries them all.
    fn stop_all(&self, target: &Target) {
        for _ in 0..16 {
            let out = self.jcmd(target, "JFR.stop", &[format!("name={RECORDING}")]);
            match out {
                Some(out) if out.contains("Stopped recording") || out.contains("stopped") => {}
                _ => return,
            }
        }
    }

    fn dump(&self, target: &Target) -> bool {
        let out = self.jcmd(
            target,
            "JFR.dump",
            &[
                format!("name={RECORDING}"),
                format!("filename={}", target.dump),
            ],
        );
        matches!(out, Some(out) if out.contains("Dumped recording"))
    }

    /// Read one dump and fold its events into per-stack totals.
    ///
    /// `jfr print --json` is verbose: every event repeats the full stack with
    /// descriptors and type names, and the document is parsed into memory. A
    /// twelve-second window without throttling produced 47 MB, which the agent
    /// cannot afford per scan, so the output is bounded here as well as in the
    /// settings file: anything larger than [`MAX_DUMP_JSON`] is skipped for this
    /// interval instead of parsed.
    fn parse(&self, target: &Target) -> Result<Vec<JvmSample>, &'static str> {
        let jfr = target.jcmd.with_file_name("jfr");
        let out = self.run(
            target,
            &[
                jfr.to_string_lossy().to_string(),
                "print".to_string(),
                "--json".to_string(),
                "--stack-depth".to_string(),
                "24".to_string(),
                "--events".to_string(),
                "jdk.ObjectAllocationSample,jdk.OldObjectSample".to_string(),
                target.dump.clone(),
            ],
        )
        .ok_or("jfr failed")?;
        if out.len() > MAX_DUMP_JSON {
            info!(
                tgid = target.tgid,
                bytes = out.len(),
                "skipping a JFR dump that is too large to parse"
            );
            // The dump is megabytes; leaving it in the target's /tmp would fill
            // it at one file per JVM, every interval.
            self.remove_dump(target);
            return Err("jfr large");
        }
        let value: serde_json::Value = serde_json::from_str(&out).map_err(|_| "jfr json")?;
        Ok(fold(&value, target.tgid))
    }

    /// Run `jcmd` inside the target's namespaces: `jcmd <pid> <command> …`,
    /// where the pid is the plain number the target knows itself by.
    fn jcmd(&self, target: &Target, command: &str, args: &[String]) -> Option<String> {
        let mut argv = vec![
            target.jcmd.to_string_lossy().to_string(),
            target.inner.clone(),
            command.to_string(),
        ];
        argv.extend(args.iter().cloned());
        self.run(target, &argv)
    }

    /// Run a command inside the target's namespaces and capture its output.
    fn run(&self, target: &Target, argv: &[String]) -> Option<String> {
        let nsenter = self.nsenter.as_ref()?;
        let mut cmd = Command::new(nsenter);
        cmd.args(["-t", &target.tgid.to_string(), "-m", "-p", "-i", "-u", "--"]);
        cmd.args(argv);
        // The agent's own environment must not leak into the target: an
        // `LD_PRELOAD` the agent carries is not a library the target has.
        cmd.env_remove("LD_PRELOAD");
        let out = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .ok()?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        if text.is_empty() {
            text = String::from_utf8_lossy(&out.stderr).into_owned();
        }
        Some(text)
    }
}

/// Turn the `jfr print --json` document into per-stack totals.
///
/// The document is `{ recording: { events: [ { type, values } ] } }`; every
/// field is read defensively, because the shape of a frame's `method` object
/// differs between JDK versions and a missing field must drop one event, not
/// void the dump.
fn fold(doc: &serde_json::Value, tgid: u32) -> Vec<JvmSample> {
    let mut by_stack: HashMap<Vec<(String, String, u32)>, (u64, u64)> = HashMap::new();
    let events = doc
        .get("recording")
        .and_then(|r| r.get("events"))
        .and_then(|e| e.as_array());
    let Some(events) = events else {
        return Vec::new();
    };
    for event in events {
        let kind = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let Some(values) = event.get("values") else {
            continue;
        };
        let Some(bytes) = field_bytes(values, &["weight", "allocationSize"]) else {
            continue;
        };
        if bytes == 0 {
            continue;
        }
        // `weight` is an *estimate*, and the sampler reports the whole interval
        // since it last had a chance to sample that thread. A thread in a tight
        // allocation loop therefore reports terabytes "at once" — one sample of
        // a 1 KB/ms loop measured 7 TB — which is not an allocation size and
        // makes the total meaningless. Nothing in one JVM can allocate more than
        // its heap in one step, so a weight above this ceiling is clamped.
        let bytes = bytes.min(MAX_EVENT_BYTES);
        // The recorder reports its own bookkeeping as allocation samples taken
        // on its periodic thread, with the whole interval's bytes attributed to
        // that thread and none to the application. Those samples say nothing
        // about where the memory went, and they dwarf the real ones — one of
        // them measured 7 TB — so they are dropped instead of clamped.
        if let Some(thread) = values
            .get("eventThread")
            .and_then(|t| t.get("javaName").or_else(|| t.get("osName")))
            .and_then(|n| n.as_str())
        {
            if thread.starts_with("JFR ") {
                continue;
            }
        }
        let mut frames: Vec<(String, String, u32)> = Vec::new();
        if let Some(frames_json) = values
            .get("stackTrace")
            .and_then(|s| s.get("frames"))
            .and_then(|f| f.as_array())
        {
            for frame in frames_json {
                let method = frame.get("method");
                let name = method
                    .and_then(|m| m.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let class = method
                    .and_then(|m| m.get("type"))
                    .and_then(|t| t.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let symbol = if class.is_empty() {
                    name.to_string()
                } else {
                    format!("{class}.{name}")
                };
                let line = frame
                    .get("lineNumber")
                    .and_then(|l| l.as_i64())
                    .map(|l| l.max(0) as u32)
                    .unwrap_or(0);
                frames.push((String::new(), symbol, line));
            }
        }
        if frames.is_empty() {
            continue;
        }
        // `jfr print` lists the frames leaf first, which is the order the
        // profile model wants; no reversal.
        let entry = by_stack.entry(frames).or_insert((0, 0));
        if kind.ends_with("OldObjectSample") {
            entry.1 += bytes;
        } else {
            entry.0 += bytes;
        }
    }
    let mut out: Vec<JvmSample> = by_stack
        .into_iter()
        .map(|(stack, (alloc, inuse))| JvmSample {
            tgid,
            frames: stack
                .into_iter()
                .map(|(_, symbol, line)| Frame {
                    module: "java".to_string(),
                    symbol,
                    file: String::new(),
                    line,
                    addr: 0,
                })
                .collect(),
            alloc_bytes: alloc,
            inuse_bytes: inuse,
        })
        .collect();
    if out.len() > MAX_STACKS {
        out.sort_by_key(|s| std::cmp::Reverse(s.alloc_bytes + s.inuse_bytes));
        out.truncate(MAX_STACKS);
    }
    out
}

/// Bytes out of a JFR field that may be a number, a size string, or a `DataAmount`.
fn field_bytes(values: &serde_json::Value, names: &[&str]) -> Option<u64> {
    for name in names {
        let Some(field) = values.get(*name) else {
            continue;
        };
        if let Some(n) = field.as_u64() {
            return Some(n);
        }
        if let Some(s) = field.as_str()
            && let Some(n) = parse_size(s)
        {
            return Some(n);
        }
        if let Some(s) = field.get("value").and_then(|v| v.as_str())
            && let Some(n) = parse_size(s)
        {
            return Some(n);
        }
        if let Some(n) = field.get("value").and_then(|v| v.as_u64()) {
            return Some(n);
        }
    }
    None
}

/// `"8.00 kB"` → 8192. JFR prints sizes with SI suffixes and two decimals in the
/// textual form; the JSON form carries a number, so this is a fallback.
pub(crate) fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let number: f64 = text[..split].parse().ok()?;
    let unit = text[split..].trim();
    let factor = match unit {
        "" | "B" | "bytes" | "byte" => 1.0,
        "kB" | "KB" => 1000.0,
        "KiB" => 1024.0,
        "MB" => 1_000_000.0,
        "MiB" => 1_048_576.0,
        "GB" => 1_000_000_000.0,
        "GiB" => 1_073_741_824.0,
        _ => return None,
    };
    Some((number * factor) as u64)
}

/// The pid a process has inside its own PID namespace: the last field of
/// `NSpid`.
fn inner_pid(tgid: u32) -> Option<String> {
    let status = std::fs::read_to_string(format!("/proc/{tgid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("NSpid:") {
            return rest.split_whitespace().last().map(|s| s.to_string());
        }
    }
    None
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Live JVM processes on this node.
///
/// Matching on the process name is what `/proc` makes cheap: the kernel
/// truncates `comm` to 15 characters and every launcher — `java`,
/// `/usr/lib/jvm/.../bin/java` — writes `java` there. A JVM started from a
/// renamed binary is left alone rather than guessed at.
fn jvm_pids() -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm.trim_end() == "java" {
            out.push(pid);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One event as `jfr print --json` emits it, trimmed to the fields that
    /// matter.
    fn sample(weight: u64, frames: &[(&str, &str, i32)]) -> serde_json::Value {
        let frames: Vec<serde_json::Value> = frames
            .iter()
            .map(|(class, name, line)| {
                serde_json::json!({
                    "method": { "name": name, "type": { "name": class } },
                    "lineNumber": line,
                })
            })
            .collect();
        serde_json::json!({
            "type": "jdk.ObjectAllocationSample",
            "values": {
                "weight": weight,
                "stackTrace": { "truncated": false, "frames": frames },
            }
        })
    }

    #[test]
    fn folds_samples_into_per_stack_totals() {
        let doc = serde_json::json!({
            "recording": {
                "events": [
                    sample(100, &[("Churn", "main", 16), ("java.lang.Thread", "run", 0)]),
                    sample(250, &[("Churn", "main", 16), ("java.lang.Thread", "run", 0)]),
                    sample(7, &[("Other", "work", 3)]),
                ]
            }
        });
        let mut samples = fold(&doc, 42);
        samples.sort_by(|a, b| b.alloc_bytes.cmp(&a.alloc_bytes));
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].alloc_bytes, 350);
        assert_eq!(samples[0].tgid, 42);
        // Leaf first, class-qualified, with the line the event carried.
        assert_eq!(samples[0].frames[0].symbol, "Churn.main");
        assert_eq!(samples[0].frames[0].line, 16);
        assert_eq!(samples[0].frames[1].symbol, "java.lang.Thread.run");
        assert_eq!(samples[1].alloc_bytes, 7);
    }

    #[test]
    fn skips_events_without_a_stack() {
        let doc = serde_json::json!({
            "recording": { "events": [ { "type": "jdk.ObjectAllocationSample",
                "values": { "weight": 10, "stackTrace": { "frames": [] } } } ] }
        });
        assert!(fold(&doc, 1).is_empty());
    }

    #[test]
    fn reads_textual_sizes() {
        assert_eq!(parse_size("8.00 kB"), Some(8000));
        assert_eq!(parse_size("1.5 MiB"), Some(1_572_864));
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("nonsense"), None);
    }
}
