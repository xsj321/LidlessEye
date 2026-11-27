//! Native malloc profiling through bpftime.
//!
//! The kernel uprobe reader in `allocator.rs` pays a trap per allocation: on the
//! cluster that measured 79 µs of system time for a single 64-byte `malloc`,
//! which is why `--alloc` is off by default. bpftime moves the same program
//! into the process with an injected shared library (`libbpftime-agent.so`),
//! where a hook costs hundreds of nanoseconds instead.
//!
//! This module owns the other half of that bargain:
//!
//! * it starts `lidlesseye-bpftime` (a libbpf loader running `alloc.bpf.c` on
//!   the userspace runtime) once, with `libbpftime-syscall-server.so` preloaded;
//! * it injects `libbpftime-agent.so` into each process whose workload has been
//!   switched on, and into no others;
//! * it drains the loader's JSON lines and turns the recorded stacks into frames.
//!
//! Only `alloc_space` is produced. `free` is not hooked, so "in use" would be a
//! guess rather than a measurement.
//!
//! Nothing here runs unless a workload is switched on: injection replaces the
//! target's allocator entry point and can, in a bad case, take the target down.
//! The switch is what the user consents to.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;

use tracing::{debug, info, warn};

use lidlesseye_core::model::Frame;

use crate::symbolizer::FrameResolver;

/// Where the image puts the runtime. Both the CLI and the shared libraries have
/// to be here for injection to work at all.
const HOME: &str = "/opt/bpftime";
const CLI: &str = "/opt/bpftime/bpftime";
/// `nsenter` is what puts the loader inside the target pod's mount namespace.
const NSENTER: &str = "/usr/bin/nsenter";
const LOADER: &str = "/opt/bpftime/lidlesseye-bpftime";
const OBJECT: &str = "/opt/bpftime/alloc.bpf.o";
const SYSCALL_SERVER: &str = "/opt/bpftime/libbpftime-syscall-server.so";

/// One allocation stack, already resolved.
pub struct BpftimeSample {
    pub tgid: u32,
    pub frames: Vec<Frame>,
    pub alloc_bytes: u64,
    pub alloc_count: u64,
}

/// State that has to outlive a flush: the loader process, the reader thread, and
/// which pids already have the agent injected.
pub struct Bpftime {
    /// Started on the first scan and kept for the life of the collector.
    loader: Option<Child>,
    /// Lines from the loader, pushed by a thread so a scan never blocks on I/O.
    lines: Option<Receiver<String>>,
    /// Pids the agent has been injected into. A pid that exits is forgotten so
    /// a recycled id is not assumed to carry the agent.
    attached: HashSet<u32>,
    /// Pids seen in the last scan, used to prune `attached`.
    alive: HashSet<u32>,
    /// Pod UID whose `/dev/shm` is currently bound over ours, if any.
    shm_pod: Option<String>,
    pub skipped: HashMap<&'static str, u64>,
    pub attach_failed: u64,
    pub samples: u64,
}

impl Default for Bpftime {
    fn default() -> Self {
        Self::new()
    }
}

impl Bpftime {
    pub fn new() -> Self {
        Self {
            loader: None,
            lines: None,
            attached: HashSet::new(),
            alive: HashSet::new(),
            shm_pod: None,
            skipped: HashMap::new(),
            attach_failed: 0,
            samples: 0,
        }
    }

    /// True when the image carries the runtime. A build without it simply never
    /// reports native memory, which is the same as the switch staying off.
    pub fn available(&self) -> bool {
        let ok = std::path::Path::new(CLI).exists()
            && std::path::Path::new(LOADER).exists()
            && std::path::Path::new(OBJECT).exists()
            && std::path::Path::new(SYSCALL_SERVER).exists();
        if !ok {
            debug!(home = HOME, "bpftime runtime is not installed in this image");
        }
        ok
    }

    /// Bind the target pod's `/dev/shm` over ours, restarting the loader so its
    /// segment is created in the namespace the injected agent will see.
    ///
    /// bpftime's server and agent meet through a POSIX shared memory segment,
    /// and Kubernetes mounts a separate tmpfs on `/dev/shm` in every pod, so a
    /// DaemonSet container cannot otherwise reach a process in another pod.
    /// One pod at a time: the mount belongs to this process, so a second
    /// workload on the node is skipped rather than silently mixed up.
    fn bind_target_shm(&mut self, pod: &str, pid: u32) {
        if self.shm_pod.as_deref() == Some(pod) {
            return;
        }
        if self.shm_pod.is_some() {
            *self.skipped.entry("second_pod").or_insert(0) += 1;
            return;
        }
        // The loader joins the target's mount namespace instead of borrowing its
        // `/dev/shm` (see `ensure_loader`): the runtime locks the pod's tmpfs,
        // and a bind mount on top of a locked mount point fails with `EINVAL`
        // even for root. Joining the namespace needs no privileges and gives the
        // loader the very segment the injected agent writes to.
        self.shm_pod = Some(pod.to_string());
        if let Some(mut child) = self.loader.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.lines = None;
        debug!(pod, pid, "the loader will start in the target pod's namespace");
    }

    /// Forget the target pod and stop its loader.
    fn release_shm(&mut self) {
        if self.shm_pod.is_none() {
            return;
        }
        self.shm_pod = None;
        if let Some(mut child) = self.loader.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.lines = None;
        debug!("stopped the loader: no target pod left");
    }

    /// Start the loader once, with the userspace runtime preloaded.
    ///
    /// The loader runs inside the *target's* mount namespace (`nsenter -m`), so
    /// its `/dev/shm` is the target pod's: that is where the injected agent
    /// creates the shared memory segment they meet on. Binding the target's
    /// `/dev/shm` into our own namespace is rejected with `EINVAL` (the runtime
    /// locks that mount), and joining the namespace sidesteps it entirely.
    ///
    /// Inside that namespace neither our `/opt` nor our `/proc` is reachable, so
    /// the loader is first copied into the target's own `/tmp`, together with
    /// its dynamic linker and libraries: the target image may carry an older
    /// glibc or no libelf at all.
    fn ensure_loader(&mut self, interval_secs: u64, target_pid: Option<u32>) -> bool {
        if self.loader.is_some() {
            return true;
        }
        let mut command = match target_pid {
            Some(pid) => {
                let Some(dir) = install_bundle(pid) else {
                    return false;
                };
                let mut command = Command::new(NSENTER);
                // What the loader will see after `nsenter -m`: report the sizes so
                // a missing or half-copied bundle is visible in the log instead of
                // as a bare "cannot be preloaded".
                let seen = |name: &str| {
                    std::fs::metadata(format!("/proc/{pid}/root{dir}/{name}"))
                        .map(|m| m.len())
                        .unwrap_or(0)
                };
                info!(
                    pid,
                    dir,
                    loader = seen("lidlesseye-bpftime"),
                    object = seen("alloc.bpf.o"),
                    server = seen("libbpftime-syscall-server.so"),
                    linker = seen("ld-linux-x86-64.so.2"),
                    "starting the loader in the target pod"
                );
                command
                    .arg("-t")
                    .arg(pid.to_string())
                    .arg("-m")
                    .arg("--")
                    .arg(format!("{dir}/ld-linux-x86-64.so.2"))
                    .arg("--library-path")
                    .arg(format!("{dir}/lib"))
                    .arg(format!("{dir}/lidlesseye-bpftime"))
                    .arg(format!("{dir}/alloc.bpf.o"))
                    .env(
                        "LD_PRELOAD",
                        format!("{dir}/libbpftime-syscall-server.so"),
                    )
                    // The preloaded runtime has its own dependencies; they are in
                    // the bundle too, and the private linker only finds them here.
                    .env("LD_LIBRARY_PATH", format!("{dir}/lib"));
                command
            }
            None => {
                let mut command = Command::new(LOADER);
                command
                    .arg(OBJECT)
                    .env("LD_PRELOAD", SYSCALL_SERVER)
                    // Our own container libraries are the right ones here, and
                    // the agent may have inherited a search path from its parent.
                    .env("LD_LIBRARY_PATH", "");
                command
            }
        };
        let child = command
            .arg(interval_secs.max(1).to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(err) => {
                warn!(error = %err, "could not start the bpftime loader");
                return false;
            }
        };

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                warn!("the bpftime loader has no stdout");
                let _ = child.kill();
                return false;
            }
        };
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("bpftime-reader".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(line) => {
                            if tx.send(line).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .ok();

        info!(loader = LOADER, interval_secs, "started the bpftime malloc reader");
        self.loader = Some(child);
        self.lines = Some(rx);
        true
    }

    /// Inject the agent into `tgid` once. `bpftime attach` is the only
    /// process-touching step in this reader.
    fn attach(&mut self, tgid: u32) {
        if self.attached.contains(&tgid) {
            return;
        }
        // A process this reader already injected stays injected: `attach` refuses
        // a second time, and the library is visible in its maps, so treat that as
        // success instead of retrying and logging a failure every scan.
        if let Ok(maps) = std::fs::read_to_string(format!("/proc/{tgid}/maps")) {
            if maps.contains("libbpftime-agent") {
                self.attached.insert(tgid);
                debug!(tgid, "the bpftime agent is already injected");
                return;
            }
        }
        let out = Command::new(CLI)
            .arg("attach")
            .arg(tgid.to_string())
            .env_remove("LD_PRELOAD")
            // The CLI writes its log to ~/.bpftime/runtime.log by default, and
            // the agent runs with a read-only root filesystem: the write fails
            // and `attach` exits 1 without a word, before injecting anything.
            .env("BPFTIME_LOG_OUTPUT", "/dev/stderr")
            .output();
        match out {
            Ok(out) if out.status.success() => {
                self.attached.insert(tgid);
                info!(tgid, "injected the bpftime agent");
            }
            Ok(out) => {
                self.attach_failed += 1;
                let stderr = String::from_utf8_lossy(&out.stderr);
                warn!(tgid, stderr = %stderr.trim(), "bpftime attach failed");
            }
            Err(err) => {
                self.attach_failed += 1;
                debug!(tgid, error = %err, "could not run bpftime attach");
            }
        }
    }

    /// Drain what the loader counted and resolve it. `allowed` is the workload
    /// switch: a pid it rejects is never touched.
    pub fn scan(
        &mut self,
        allowed: &dyn Fn(u32) -> bool,
        resolver: &mut FrameResolver,
        interval_secs: u64,
    ) -> Vec<BpftimeSample> {
        // Inject into the processes the server asked for, and only those.
        self.alive.clear();
        let candidates = candidates();
        let found = candidates.len();
        let mut chosen = Vec::new();
        for pid in candidates {
            if !allowed(pid) {
                *self.skipped.entry("not_requested").or_insert(0) += 1;
                continue;
            }
            chosen.push(pid);
        }
        // The runtime's server and the injected agent meet in /dev/shm, and
        // Kubernetes gives every pod its own there, so the loader has to run in
        // the target pod's namespace. One pod at a time: the segment belongs to
        // one namespace.
        let target_pid = chosen.first().copied();
        match target_pid.and_then(pod_of) {
            Some(pod) => {
                debug!(pid = target_pid, pod = %pod, "target pod for bpftime");
                self.bind_target_shm(&pod, target_pid.unwrap_or_default());
            }
            None => {
                // A process outside any pod (a host process): our own /dev/shm
                // is the right place and needs no namespace.
                debug!("no pod for the selected processes; keeping our /dev/shm");
                self.release_shm();
            }
        }
        if !self.ensure_loader(interval_secs, self.shm_pod.as_ref().and_then(|_| target_pid)) {
            return Vec::new();
        }
        for pid in chosen {
            self.alive.insert(pid);
            self.attach(pid);
        }
        self.attached.retain(|pid| self.alive.contains(pid));
        // One line per scan, like the JVM reader: without it, "the switch is on
        // and nothing happens" cannot be told apart from "there is nothing to
        // profile on this node".
        info!(
            found,
            allowed = self.alive.len(),
            attached = self.attached.len(),
            samples = self.samples,
            skipped = ?self.skipped,
            "bpftime scan"
        );

        let Some(lines) = self.lines.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        loop {
            match lines.try_recv() {
                Ok(line) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match parse_line(&line) {
                        Some((tgid, ips, bytes, count)) => {
                            if bytes == 0 {
                                continue;
                            }
                            let frames = resolver.resolve_user(tgid, &ips);
                            if frames.is_empty() {
                                *self.skipped.entry("unsymbolized").or_insert(0) += 1;
                                continue;
                            }
                            self.samples += 1;
                            out.push(BpftimeSample {
                                tgid,
                                frames,
                                alloc_bytes: bytes,
                                alloc_count: count,
                            });
                        }
                        None => {
                            *self.skipped.entry("malformed").or_insert(0) += 1;
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    warn!("the bpftime loader exited; restarting it next scan");
                    self.loader = None;
                    self.lines = None;
                    break;
                }
            }
        }
        out
    }
}

impl Drop for Bpftime {
    fn drop(&mut self) {
        if let Some(child) = self.loader.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// One loader line:
/// `{"pid":1234,"bytes":4096,"count":7,"stack":"0x7f..,0x7f.."}`
fn parse_line(line: &str) -> Option<(u32, Vec<u64>, u64, u64)> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let tgid = value.get("pid")?.as_u64()? as u32;
    let bytes = value.get("bytes")?.as_u64()?;
    let count = value.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    let stack = value.get("stack").and_then(|v| v.as_str()).unwrap_or("");
    let mut ips = Vec::new();
    for part in stack.split(',').filter(|p| !p.is_empty()) {
        let trimmed = part.trim();
        let hex = trimmed.strip_prefix("0x").unwrap_or(trimmed);
        if let Ok(ip) = u64::from_str_radix(hex, 16) {
            ips.push(ip);
        }
    }
    Some((tgid, ips, bytes, count))
}

/// Processes worth injecting into: inside a container (so the workload switch
/// can name them) and mapping libc (so `malloc` exists to hook).
///
/// The agent runs with `hostPID`, so `/proc` here is the node's, not the pod's.
/// Reading `cgroup` first keeps the scan cheap: everything outside a container
/// is rejected before `maps` is opened, which is the expensive read.
fn candidates() -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let cgroup = match std::fs::read_to_string(format!("/proc/{pid}/cgroup")) {
            Ok(cgroup) => cgroup,
            Err(_) => continue,
        };
        if !cgroup.contains("kubepods") {
            continue;
        }
        let maps = match std::fs::read_to_string(format!("/proc/{pid}/maps")) {
            Ok(maps) => maps,
            Err(_) => continue,
        };
        // The symbol is in whichever libc the image ships: glibc (`libc.so.6`),
        // musl (`libc.musl-`), or an explicit path from the allocator family.
        if maps.contains("libc.so") || maps.contains("libc.musl-") {
            out.push(pid);
        }
    }
    out
}

/// The pod a process belongs to, from its cgroup path.
///
/// Only used to decide whose `/dev/shm` to borrow, so the pod UID is enough —
/// the same string appears in every process of the pod. A process outside a
/// pod returns `None`, and then the container's own `/dev/shm` is correct.
fn pod_of(pid: u32) -> Option<String> {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    // The pod segment is `…-pod<uid>.slice`, and "pod" also appears inside
    // "kubepods", so the last occurrence is the one that carries the uid.
    let marker = "pod";
    let start = cgroup.rfind(marker)? + marker.len();
    let rest = &cgroup[start..];
    let uid: String = rest
        .chars()
        .take_while(|c| *c != '.' && *c != '/' && *c != '\n')
        .collect();
    if uid.is_empty() {
        None
    } else {
        Some(uid)
    }
}

/// Where the loader bundle is unpacked inside the target pod.
const TARGET_DIR: &str = "/tmp/.lidlesseye";

/// Copy the loader, its object, the preload and a private dynamic linker with
/// its libraries into `pid`'s pod, and return the directory as *that pod* sees
/// it.
///
/// The loader must run inside the target's mount namespace — only there is the
/// bpftime segment in the same `/dev/shm` as the injected agent — and inside
/// that namespace neither our `/opt` nor our `/proc` exists, so everything the
/// loader needs has to be a plain file in the target's own filesystem. The
/// private linker is what keeps the target image's glibc out of the picture.
fn install_bundle(pid: u32) -> Option<String> {
    let dest = format!("/proc/{pid}/root{TARGET_DIR}");
    if let Err(err) = std::fs::create_dir_all(format!("{dest}/lib")) {
        warn!(pid, error = %err, "could not create the bundle directory in the target pod");
        return None;
    }
    let mut ok = true;
    for (from, name) in [
        (LOADER, "lidlesseye-bpftime"),
        (OBJECT, "alloc.bpf.o"),
        (SYSCALL_SERVER, "libbpftime-syscall-server.so"),
        (
            "/opt/bpftime/bundle/ld-linux-x86-64.so.2",
            "ld-linux-x86-64.so.2",
        ),
    ] {
        let to = format!("{dest}/{name}");
        // The runtime library is 77 MB: recopying it on every scan would rewrite
        // the file the running loader is mapping. Copy once, and only when the
        // sizes differ.
        if same_size(from, &to) {
            continue;
        }
        if let Err(err) = std::fs::copy(from, &to) {
            warn!(pid, from, error = %err, "could not copy the bundle into the target pod");
            ok = false;
        }
    }
    match std::fs::read_dir("/opt/bpftime/bundle/lib") {
        Ok(entries) => {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let to = format!("{dest}/lib/{}", name.to_string_lossy());
                if let Err(err) = std::fs::copy(entry.path(), to) {
                    warn!(pid, error = %err, "could not copy a bundle library");
                    ok = false;
                }
            }
        }
        Err(err) => {
            warn!(pid, error = %err, "the bundle has no libraries to copy");
            ok = false;
        }
    }
    // `0755` for the loader and the linker: `/proc/<pid>/root` keeps the target's
    // permissions, and the target user may not be root.
    let _ = std::fs::set_permissions(
        format!("{dest}/lidlesseye-bpftime"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    );
    let _ = std::fs::set_permissions(
        format!("{dest}/ld-linux-x86-64.so.2"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    );
    ok.then(|| TARGET_DIR.to_string())
}

/// Whether `to` already holds a complete copy of `from`.
fn same_size(from: &str, to: &str) -> bool {
    match (std::fs::metadata(from), std::fs::metadata(to)) {
        (Ok(a), Ok(b)) => a.len() == b.len() && a.len() > 0,
        _ => false,
    }
}

/// The switch, shared with the directives poller in the agent binary.
pub type SharedSwitch = Arc<parking_lot::RwLock<HashMap<String, bool>>>;
