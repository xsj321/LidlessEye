//! Connection-level accounting from eBPF.
//!
//! Loads `bpf/net.bpf.c`, which observes `sock:inet_sock_set_state` and three
//! kernel functions, and hands one [`Flow`] per address pair and window to the
//! caller. The counters are deltas: the kernel-side map is drained and cleared
//! once per interval, so summing them over the dashboard's time range is what
//! makes the map follow the same window as the flamegraph.
//!
//! ## Why this replaces polling `/proc/net/tcp`
//!
//! Three things polling cannot do. It sees only sockets that are open at the
//! instant of the scan. It cannot tell how many bytes a connection carried. And
//! the server side of a same-node pod-to-ClusterIP connection has had its
//! source address rewritten by kube-proxy (hairpin SNAT), so polling cannot
//! name the client of such a connection — the reason a large part of the map
//! used to collapse into `external`. A socket-level probe sees the addresses
//! the application wrote, before NAT.
//!
//! The kernel object is separate from the profiler's so that a kernel which
//! rejects one of these programs only costs the map its byte counters; the
//! caller falls back to polling when [`start`] fails.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use aya::Ebpf;
use aya::maps::perf::{PerfEvent, PerfEventArrayBuffer};
use aya::maps::{HashMap as AyaHashMap, MapData, PerfEventArray};
use aya::programs::uprobe::UProbeScope;
use aya::programs::{KProbe, UProbe};
use tracing::{debug, info, warn};

use crate::l7;
use crate::procfs;

// Read the object from the source tree rather than `OUT_DIR`. The build script
// refreshes it there, and rustc records the file in the crate's dependency
// list, so editing the `.c` recompiles whatever embeds it. With the object in
// `OUT_DIR` cargo reruns the build script but can still consider the crate
// fresh, and the binary keeps the previous bytes — which cost an afternoon of
// chasing a verifier error that the checked-in object did not contain.
const NET_BPF: &[u8] = include_bytes!("../bpf/net.bpf.o");

/// Pages per CPU of the payload ring buffer. Each page is 4 KiB, and the
/// collector drains every `flush_interval`, so this is a burst allowance: a
/// sample is at most 312 bytes, and 64 pages hold roughly 800 of them.
const PERF_PAGES: usize = 64;

/// Ceiling on the cgroup-keyed TLS detail kept between windows. A cgroup with
/// no traffic for this many samples has long stopped being interesting, and the
/// kernel map this mirrors holds 4096 entries.
const CGROUP_CACHE: usize = 4096;

const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;

const ROLE_CLIENT: u32 = 1;
const ROLE_SERVER: u32 = 2;

/// Which end of the connection the observing socket is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The socket opened the connection.
    Client,
    /// The socket accepted it.
    Server,
    /// A transition that did not carry the handshake; kept for byte-only rows.
    Unknown,
}

/// One address pair as the kernel reported it, with the counters that
/// accumulated during the window it was drained from.
#[derive(Debug, Clone)]
pub struct Flow {
    /// Local end of the observing socket: the pod that owns it.
    pub local: (IpAddr, u16),
    /// The peer.
    pub remote: (IpAddr, u16),
    pub role: Role,
    /// Completed handshakes in this window.
    pub opens: u64,
    /// Bytes this pod sent to, and received from, the peer.
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub retransmits: u64,
    /// Sum of the round-trip times of the connections that closed in this
    /// window, and how many of them there were. Kept as a sum so that summing
    /// windows keeps the average exact.
    pub rtt_us_sum: u64,
    pub rtt_samples: u64,
    /// Cgroup id of the task that opened the connection, when the probe saw
    /// the handshake. Zero for a socket that predates the agent, and for a
    /// server-side socket (its handshake runs in softirq, where "current" is
    /// unrelated to the connection). It is what names the container that
    /// dialled, down to the sidecar.
    pub cgroup: u64,
    /// Bit set of application protocols recognised in the payload — by the
    /// agent, from the sampled bytes, not by the kernel. Zero means nothing
    /// parseable was seen and the map falls back to the server port's guess.
    pub protocols: u32,
    /// `Host:` header of the last client request, empty when the flow carried
    /// no HTTP or the client sent none. This is the name the application asked
    /// for, which a ClusterIP or load balancer address cannot tell you.
    pub host: String,
    /// Last SQL text seen from the client (MySQL COM_QUERY or PostgreSQL
    /// Query/Parse), empty when the flow carried no database protocol.
    pub sql: String,
}

impl Flow {
    /// Average round-trip time, when at least one connection closed.
    pub fn avg_rtt_us(&self) -> Option<u32> {
        if self.rtt_samples == 0 {
            return None;
        }
        Some((self.rtt_us_sum / self.rtt_samples).min(u64::from(u32::MAX)) as u32)
    }
}

/// Tunables.
#[derive(Debug, Clone)]
pub struct ConntrackConfig {
    /// How often the kernel map is drained. Matches the service map window.
    pub flush_interval: Duration,
    /// Attach a uprobe to TLS libraries so encrypted HTTP and database
    /// traffic can contribute a Host and a SQL statement. Off by default:
    /// every TLS write pays a user-memory read and the protocol parsers.
    pub ssl_uprobe: bool,
}

impl Default for ConntrackConfig {
    fn default() -> Self {
        ConntrackConfig {
            flush_interval: Duration::from_secs(10),
            ssl_uprobe: false,
        }
    }
}

/// Handle to a running collector.
pub struct ConntrackHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    flows: Arc<AtomicU64>,
}

impl ConntrackHandle {
    /// Total flows observed since start, for a status line.
    pub fn flows(&self) -> u64 {
        self.flows.load(Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ConntrackHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

// SAFETY: `#[repr(C)]`, no padding, no pointers.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
struct FlowKey {
    saddr: [u8; 16],
    daddr: [u8; 16],
    sport: u16,
    dport: u16,
    family: u16,
    pad: u16,
}

unsafe impl aya::Pod for FlowKey {}

impl FlowKey {
    /// Rebuild the key from the 40 raw bytes a ring-buffer sample carries.
    fn from_ne_bytes(bytes: &[u8; 40]) -> Self {
        let mut key = FlowKey::default();
        key.saddr.copy_from_slice(&bytes[0..16]);
        key.daddr.copy_from_slice(&bytes[16..32]);
        key.sport = u16::from_ne_bytes([bytes[32], bytes[33]]);
        key.dport = u16::from_ne_bytes([bytes[34], bytes[35]]);
        key.family = u16::from_ne_bytes([bytes[36], bytes[37]]);
        key.pad = u16::from_ne_bytes([bytes[38], bytes[39]]);
        key
    }
}

// SAFETY: as above; nine 64-bit and two 32-bit counters.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct FlowStats {
    opens: u64,
    tx_bytes: u64,
    rx_bytes: u64,
    retransmits: u64,
    rtt_us_sum: u64,
    rtt_samples: u64,
    cgroup: u64,
    role: u32,
    /// Payload samples already emitted for this flow in the window.
    l7_samples: u32,
}

unsafe impl aya::Pod for FlowStats {}

/// What the probes have told user space about a connection's payloads since
/// the last window.
///
/// Two indexings, because the two probes know different things. A send on a
/// socket names its connection, so the detail is exact. A TLS write has no
/// socket at all, so its detail is keyed by cgroup and applies to whatever
/// that task opened. The kernel map is not consulted for either: the samples
/// arrive as ring-buffer events, and this is where they are accumulated.
#[derive(Default)]
struct L7Cache {
    /// Latest detail per connection. Pruned to the flows of the window being
    /// drained: the kernel counter that anchors a flow is itself per window.
    by_flow: HashMap<FlowKey, l7::Detail>,
    /// Latest detail per cgroup, with the arrival order used to bound it.
    by_cgroup: HashMap<u64, Stamped>,
    seq: u64,
    /// Samples the kernel dropped because the ring buffer was full.
    lost: u64,
    /// Records read from the ring, and how they resolved. Counters rather than
    /// logs: a busy node produces thousands a second, and the ratio is what
    /// tells an operator the probes are alive.
    seen: u64,
    parsed: u64,
    unparsed: u64,
}

struct Stamped {
    detail: l7::Detail,
    seq: u64,
}

impl L7Cache {
    fn apply(&mut self, key: &FlowKey, flow: &mut Flow) {
        if let Some(detail) = self.by_flow.get(key) {
            // The probe only classifies a payload it recognises, so a parse on
            // this path is a second opinion that can only add a bit the port
            // guess would have missed.
            flow.protocols |= detail.protocol;
            copy_detail(flow, detail);
        }
        if let Some(stamped) = self.by_cgroup.get(&flow.cgroup) {
            copy_detail(flow, &stamped.detail);
        }
    }

    /// Keep the map proportional to the flows that still exist.
    fn retain(&mut self, live: &[FlowKey]) {
        let live: HashSet<&FlowKey> = live.iter().collect();
        self.by_flow.retain(|key, _| live.contains(key));
        if self.by_cgroup.len() > CGROUP_CACHE {
            let floor = self.seq.saturating_sub(CGROUP_CACHE as u64 / 2);
            self.by_cgroup.retain(|_, entry| entry.seq > floor);
        }
    }

    fn absorb(&mut self, event: PerfEvent<'_>) {
        match event {
            PerfEvent::Lost { count } => {
                self.lost = self.lost.saturating_add(count);
                warn!(
                    dropped = count,
                    total = self.lost,
                    "payload ring buffer overflow"
                );
            }
            PerfEvent::Sample { head, tail } => self.record(head, tail),
        }
    }

    fn record(&mut self, head: &[u8], tail: &[u8]) {
        self.seen += 1;
        // A sample that straddles the end of the ring arrives in two pieces.
        let mut joined;
        let raw = if tail.is_empty() {
            head
        } else {
            joined = Vec::with_capacity(head.len() + tail.len());
            joined.extend_from_slice(head);
            joined.extend_from_slice(tail);
            &joined
        };
        let Some(sample) = l7::decode(raw) else {
            if self.unparsed < 5 {
                debug!(
                    len = raw.len(),
                    head = ?&raw[..raw.len().min(64)],
                    "undecodable payload sample"
                );
            }
            self.unparsed += 1;
            return;
        };
        let Some(detail) = l7::parse(sample.payload) else {
            if self.unparsed < 5 {
                debug!(
                    payload = ?&sample.payload[..sample.payload.len().min(12)],
                    "unrecognised payload sample"
                );
            }
            self.unparsed += 1;
            return;
        };
        self.parsed += 1;
        match sample.flow {
            Some(bytes) => {
                self.by_flow.insert(FlowKey::from_ne_bytes(&bytes), detail);
            }
            None => {
                // TLS detail is keyed by cgroup, so a sample that parses to
                // nothing useful is still worth seeing once: it means the
                // uprobe fired on bytes the parser does not understand.
                if self.by_cgroup.len() < 32 {
                    debug!(
                        cgroup = sample.cgroup,
                        payload = ?&sample.payload[..sample.payload.len().min(16)],
                        "TLS payload sample"
                    );
                }
                self.seq += 1;
                self.by_cgroup.insert(
                    sample.cgroup,
                    Stamped {
                        detail,
                        seq: self.seq,
                    },
                );
            }
        }
    }
}

fn copy_detail(flow: &mut Flow, detail: &l7::Detail) {
    if let Some(host) = &detail.host {
        flow.host.clone_from(host);
    }
    if let Some(sql) = &detail.sql {
        flow.sql.clone_from(sql);
    }
}

/// The ring buffers, one per CPU, plus the map they are registered in. The
/// array must outlive the buffers: dropping it clears the map entries they
/// are reached through.
struct L7Ring {
    buffers: Vec<PerfEventArrayBuffer>,
    _array: PerfEventArray<MapData>,
}

impl L7Ring {
    fn drain(&mut self, cache: &mut L7Cache) {
        for buffer in &mut self.buffers {
            buffer.for_each(|event| cache.absorb(event));
        }
    }
}

/// Open one ring buffer per CPU. Best effort: a host where `perf_event_open`
/// is denied keeps the map and its counters, it just cannot name Hosts or SQL.
fn open_l7_ring(ebpf: &mut Ebpf) -> Result<L7Ring, String> {
    let Some(map) = ebpf.take_map("l7_events") else {
        return Err("net.bpf.o is missing the `l7_events` map".to_string());
    };
    let mut array =
        PerfEventArray::try_from(map).map_err(|err| format!("invalid `l7_events` map: {err}"))?;
    let cpus = aya::util::online_cpus().map_err(|(_, err)| err.to_string())?;
    let mut buffers = Vec::with_capacity(cpus.len());
    for cpu in cpus {
        let buffer = PerfEventArrayBuffer::open(cpu, PERF_PAGES)
            .map_err(|err| format!("could not open the perf ring for CPU {cpu}: {err}"))?;
        array
            .set(cpu, &buffer)
            .map_err(|err| format!("could not register the perf ring for CPU {cpu}: {err}"))?;
        buffers.push(buffer);
    }
    Ok(L7Ring {
        buffers,
        _array: array,
    })
}

/// Attach `lidlesseye_ssl_write` to every TLS library this host can see.
///
/// There is no single library to target: a node mixes distro packages,
/// statically linked runtimes and containers, and an image may carry
/// `libssl.so.1.1` while the host has `libssl.so.3`. Enumerating the well-known
/// sonames under each root, plus each process's `/proc/<pid>/root`, and trying
/// both plain and `_ex` entry points is the only portable answer. Duplicates
/// are removed by inode so a library bind-mounted into twenty containers is
/// not attached twenty times.
fn attach_ssl_uprobes(ebpf: &mut Ebpf) -> Vec<String> {
    let Some(program) = ebpf.program_mut("lidlesseye_ssl_write") else {
        warn!("net.bpf.o is missing the TLS uprobe program");
        return Vec::new();
    };
    let probe: &mut UProbe = match program.try_into() {
        Ok(probe) => probe,
        Err(err) => {
            warn!(error = %err, "TLS uprobe is not a uprobe program");
            return Vec::new();
        }
    };
    if let Err(err) = probe.load() {
        warn!(error = %err, "could not load the TLS uprobe");
        return Vec::new();
    }

    let mut targets = Vec::new();
    for path in ssl_library_paths() {
        for symbol in ["SSL_write", "SSL_write_ex"] {
            match probe.attach([symbol], path.as_path(), UProbeScope::AllProcesses) {
                Ok(_) => targets.push(format!("{}:{symbol}", path.display())),
                Err(err) => {
                    debug!(path = %path.display(), symbol, error = %err, "TLS uprobe attach failed")
                }
            }
        }
    }
    if !targets.is_empty() {
        info!(targets = targets.len(), "TLS uprobes attached");
    } else {
        warn!("no TLS library could be attached; encrypted flows keep only the port guess");
    }
    targets
}

/// Candidate TLS libraries: the sonames OpenSSL, GnuTLS and NSS have shipped
/// under, in the directories distributions and local installs use.
fn ssl_library_paths() -> Vec<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    const NAMES: &[&str] = &[
        "libssl.so.3",
        "libssl.so.1.1",
        "libssl.so.1.0.2",
        "libssl.so.1.0.0",
        "libssl.so",
        "libgnutls.so.30",
        "libgnutls.so.28",
        "libnss3.so",
    ];
    const ROOTS: &[&str] = &[
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib64",
        "/lib64",
        "/usr/lib",
        "/usr/local/lib",
    ];

    let mut candidates: Vec<PathBuf> = Vec::new();
    for root in ROOTS {
        for name in NAMES {
            candidates.push(Path::new(root).join(name));
        }
    }
    // Container images carry their own copy. `/proc/<pid>/root` resolves it in
    // that process's mount namespace, which is what the uprobe needs.
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let pid = entry.file_name();
            if !pid.to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            for root in ROOTS {
                for name in NAMES {
                    candidates.push(
                        Path::new("/proc")
                            .join(&pid)
                            .join("root")
                            .join(root.trim_start_matches('/'))
                            .join(name),
                    );
                }
            }
        }
    }

    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    candidates
        .into_iter()
        .filter(|path| match path.metadata() {
            Ok(meta) => seen.insert((meta.dev(), meta.ino())),
            Err(_) => false,
        })
        .collect()
}

/// Load the programs and start draining. `on_flows` runs on the collector
/// thread once per `flush_interval`.
///
/// Returns an error — the caller is expected to fall back to polling `/proc` —
/// when the object cannot be loaded or the state tracepoint cannot be attached.
pub fn start<F>(config: ConntrackConfig, on_flows: F) -> Result<ConntrackHandle, String>
where
    F: Fn(Vec<Flow>) + Send + 'static,
{
    if !procfs::is_privileged() {
        return Err("the conntrack collector must run as root (or with CAP_BPF)".to_string());
    }
    procfs::raise_memlock_limit();

    // Tell the send probe where the payload is. The layout moved in 6.16 and
    // the old access turned into an invalid call the verifier rejects, so the
    // offsets come from the kernel's own BTF instead of a CO-RE field name.
    let layout = crate::kernel_btf::iov_layout();
    let mut loader = aya::EbpfLoader::new();
    match &layout {
        Ok(layout) => {
            debug!(?layout, "payload offsets resolved from kernel BTF");
            loader.override_global("msg_iter_offset", &layout.msg_iter, true);
            loader.override_global("iov_ptr_offset", &layout.iov_ptr, true);
        }
        Err(err) => warn!(
            error = %err,
            "kernel BTF did not describe the payload layout; keeping the built-in default"
        ),
    }
    let mut ebpf = loader
        .load(NET_BPF)
        .map_err(|err| format!("failed to load net.bpf.o: {err}"))?;

    // The data probes are best effort: a kernel that renames or inlines one of
    // these keeps the map, it just cannot report bytes and retransmits. They
    // are attached first so that a failure of the lifecycle probe below still
    // leaves a record of what this kernel does support.
    let mut attached: Vec<&'static str> = Vec::new();
    for (program, symbols) in [
        ("lidlesseye_tcp_send", &["tcp_sendmsg"][..]),
        ("lidlesseye_tcp_recv", &["tcp_cleanup_rbuf"][..]),
        ("lidlesseye_tcp_retrans", &["tcp_retransmit_skb"][..]),
    ] {
        match attach_kprobe(&mut ebpf, program, symbols) {
            Some(symbol) => attached.push(symbol),
            None => warn!(
                program,
                "could not attach connection accounting probe; its counters stay zero"
            ),
        }
    }
    // The lifecycle probe is the one that must work: without it there are no
    // connections at all.
    let state = attach_kprobe(
        &mut ebpf,
        "lidlesseye_tcp_state",
        &[
            "tcp_set_state",
            "tcp_set_state.isra.0",
            "tcp_set_state.constprop.0",
        ],
    )
    .ok_or_else(|| "could not attach to tcp_set_state".to_string())?;
    attached.push(state);
    info!(
        programs = %attached.join(","),
        object = env!("LIDLESSEYE_BPF_DIGEST"),
        "conntrack programs attached"
    );

    // Optional and best-effort: a host with no readable TLS library keeps the
    // map, it just cannot name the Host or the SQL behind encryption.
    if config.ssl_uprobe {
        let _ = attach_ssl_uprobes(&mut ebpf);
    }

    // Payload samples arrive out of band from the counters, so the ring has to
    // be drained while waiting too: a burst that fills it loses samples, and
    // the burst is exactly what a dashboard is opened to see.
    let mut ring = match open_l7_ring(&mut ebpf) {
        Ok(ring) => Some(ring),
        Err(err) => {
            warn!(error = %err, "payload capture is off; hosts and SQL stay empty");
            None
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let flows = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&flows);
    let interval = config.flush_interval.max(Duration::from_secs(1));

    let thread = thread::Builder::new()
        .name("lidlesseye-conntrack".to_string())
        .spawn(move || {
            let mut ebpf = ebpf;
            let mut cache = L7Cache::default();
            while !flag.load(Ordering::Relaxed) {
                let deadline = std::time::Instant::now() + interval;
                while std::time::Instant::now() < deadline && !flag.load(Ordering::Relaxed) {
                    if let Some(ring) = ring.as_mut() {
                        ring.drain(&mut cache);
                    }
                    thread::sleep(Duration::from_millis(200));
                }
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(ring) = ring.as_mut() {
                    ring.drain(&mut cache);
                }
                match drain(&mut ebpf, &mut cache) {
                    Ok(batch) => {
                        if !batch.is_empty() {
                            counter.fetch_add(batch.len() as u64, Ordering::Relaxed);
                            debug!(
                                flows = batch.len(),
                                samples = cache.seen,
                                parsed = cache.parsed,
                                unparsed = cache.unparsed,
                                lost = cache.lost,
                                "drained connection window"
                            );
                            on_flows(batch);
                        }
                    }
                    Err(err) => warn!(error = %err, "could not drain connection counters"),
                }
            }
            info!("conntrack collector stopping, detaching eBPF programs");
        })
        .map_err(|err| format!("failed to spawn conntrack thread: {err}"))?;

    Ok(ConntrackHandle {
        stop,
        thread: Some(thread),
        flows,
    })
}

/// Load `program` once and attach it to the first symbol this kernel has.
///
/// Symbol names move between releases and compilers (`tcp_set_state` becomes
/// `tcp_set_state.isra.0` when GCC clones it), so candidates are tried in
/// order. Loading is separate from attaching because aya rejects a second
/// `load()` on a program that is already loaded.
fn attach_kprobe(ebpf: &mut Ebpf, program: &str, symbols: &[&'static str]) -> Option<&'static str> {
    let probe: &mut KProbe = match ebpf.program_mut(program) {
        Some(raw) => match raw.try_into() {
            Ok(probe) => probe,
            Err(err) => {
                debug!(program, error = %err, "not a kprobe program");
                return None;
            }
        },
        None => {
            debug!(program, "program missing from net.bpf.o");
            return None;
        }
    };
    if let Err(err) = probe.load() {
        debug!(program, error = ?err, "could not load kprobe");
        return None;
    }
    for symbol in symbols {
        match probe.attach(symbol, 0) {
            Ok(_) => {
                debug!(program, symbol, "attached kprobe");
                return Some(symbol);
            }
            Err(err) => debug!(program, symbol, error = ?err, "kprobe attach failed"),
        }
    }
    None
}

fn drain(ebpf: &mut Ebpf, cache: &mut L7Cache) -> Result<Vec<Flow>, String> {
    let map = ebpf
        .map_mut("flows")
        .ok_or_else(|| "net.bpf.o is missing the `flows` map".to_string())?;
    let mut map: AyaHashMap<&mut MapData, FlowKey, FlowStats> =
        AyaHashMap::try_from(map).map_err(|err| format!("invalid `flows` map: {err}"))?;

    let keys: Vec<FlowKey> = map.keys().filter_map(|key| key.ok()).collect();
    let mut out = Vec::with_capacity(keys.len());
    for key in &keys {
        if let Ok(stats) = map.get(key, 0)
            && let Some(mut flow) = to_flow(key, &stats)
        {
            cache.apply(key, &mut flow);
            out.push(flow);
        }
    }
    cache.retain(&keys);
    // The counters are per window: clearing them here is what makes the next
    // drain a delta again. An update that lands between the read and the
    // delete is lost, which at this scale is a handful of bytes.
    for key in keys {
        let _ = map.remove(&key);
    }
    Ok(out)
}

fn to_flow(key: &FlowKey, stats: &FlowStats) -> Option<Flow> {
    let local = address(&key.saddr, key.family)?;
    let remote = address(&key.daddr, key.family)?;
    // A socket with no local port has not been bound (yet); a socket with no
    // peer port is a listener. Neither is a connection.
    if key.sport == 0 || key.dport == 0 {
        return None;
    }
    if local.is_loopback() || remote.is_loopback() {
        return None;
    }
    let role = match stats.role {
        ROLE_CLIENT => Role::Client,
        ROLE_SERVER => Role::Server,
        _ => Role::Unknown,
    };
    Some(Flow {
        local: (local, key.sport),
        remote: (remote, key.dport),
        role,
        opens: stats.opens,
        tx_bytes: stats.tx_bytes,
        rx_bytes: stats.rx_bytes,
        retransmits: stats.retransmits,
        rtt_us_sum: stats.rtt_us_sum,
        rtt_samples: stats.rtt_samples,
        cgroup: stats.cgroup,
        // Filled from the payload samples by `L7Cache::apply`; the kernel no
        // longer classifies, so nothing arrives from the map.
        protocols: 0,
        host: String::new(),
        sql: String::new(),
    })
}

fn address(bytes: &[u8; 16], family: u16) -> Option<IpAddr> {
    match family {
        AF_INET => Some(IpAddr::V4(Ipv4Addr::new(
            bytes[0], bytes[1], bytes[2], bytes[3],
        ))),
        AF_INET6 => {
            let ip = Ipv6Addr::from(*bytes);
            // A dual-stack socket reports an IPv4 peer as `::ffff:a.b.c.d`.
            // The pod index, the Service index and `/proc/net/tcp` all know it
            // by its IPv4 form, so leaving it mapped would make every lookup
            // miss on a cluster that binds its sockets that way.
            Some(match ip.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => IpAddr::V6(ip),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(saddr: [u8; 4], daddr: [u8; 4], sport: u16, dport: u16) -> FlowKey {
        let mut key = FlowKey {
            sport,
            dport,
            family: AF_INET,
            ..Default::default()
        };
        key.saddr[..4].copy_from_slice(&saddr);
        key.daddr[..4].copy_from_slice(&daddr);
        key
    }

    #[test]
    fn turns_a_kernel_key_into_addresses() {
        let flow = to_flow(
            &key([10, 0, 0, 2], [10, 0, 0, 7], 43210, 8080),
            &FlowStats {
                opens: 3,
                rtt_us_sum: 1500,
                rtt_samples: 3,
                role: ROLE_CLIENT,
                ..Default::default()
            },
        )
        .expect("a bound client socket is a flow");
        assert_eq!(flow.local, ("10.0.0.2".parse().unwrap(), 43210));
        assert_eq!(flow.remote, ("10.0.0.7".parse().unwrap(), 8080));
        assert_eq!(flow.role, Role::Client);
        assert_eq!(flow.opens, 3);
        assert_eq!(flow.avg_rtt_us(), Some(500));
    }

    #[test]
    fn listeners_and_unbound_sockets_are_not_flows() {
        let stats = FlowStats::default();
        assert!(to_flow(&key([10, 0, 0, 2], [10, 0, 0, 7], 0, 8080), &stats).is_none());
        assert!(to_flow(&key([10, 0, 0, 2], [0, 0, 0, 0], 43210, 0), &stats).is_none());
    }

    #[test]
    fn loopback_traffic_is_dropped() {
        let stats = FlowStats::default();
        assert!(to_flow(&key([127, 0, 0, 1], [127, 0, 0, 1], 40000, 8080), &stats).is_none());
    }

    /// A dual-stack socket reports IPv4 as `::ffff:a.b.c.d`; keeping it mapped
    /// makes every pod and Service lookup miss, and the map draws those
    /// connections as "outside the cluster".
    #[test]
    fn an_ipv4_mapped_address_becomes_ipv4() {
        let mut key = FlowKey {
            sport: 9930,
            dport: 6443,
            family: AF_INET6,
            ..Default::default()
        };
        // ::ffff:172.18.0.4, little-endian words as the kernel writes them.
        key.saddr[10] = 0xff;
        key.saddr[11] = 0xff;
        key.saddr[12..16].copy_from_slice(&[172, 18, 0, 4]);
        key.daddr[10] = 0xff;
        key.daddr[11] = 0xff;
        key.daddr[12..16].copy_from_slice(&[10, 244, 0, 1]);
        let flow = to_flow(
            &key,
            &FlowStats {
                role: ROLE_SERVER,
                ..Default::default()
            },
        )
        .expect("a dual-stack socket is a flow");
        assert_eq!(flow.local, ("172.18.0.4".parse().unwrap(), 9930));
        assert_eq!(flow.remote, ("10.244.0.1".parse().unwrap(), 6443));
    }

    /// The C struct and this mirror must stay byte-compatible: the kernel
    /// writes it, `aya` copies it, and a field added on one side alone would
    /// silently shift every counter that follows.
    #[test]
    fn the_flow_stats_layout_matches_the_probe() {
        let source = include_str!("../bpf/net.bpf.c");
        let struct_start = source
            .find("struct flow_stats {")
            .expect("flow_stats is in the object");
        let struct_end = source[struct_start..]
            .find("};")
            .expect("the struct is closed")
            + struct_start;
        let fields = &source[struct_start..struct_end];
        for field in ["opens", "tx_bytes", "rx_bytes", "retransmits"] {
            assert!(fields.contains(field), "{field} missing from flow_stats");
        }
        assert!(source.contains("read_send_payload"));
        assert!(source.contains("bpf_perf_event_output"));
    }

    #[test]
    fn a_flow_starts_without_a_protocol() {
        // Classification moved to the agent, so nothing arrives from the map;
        // `L7Cache::apply` is what adds the bit.
        let flow = to_flow(
            &key([10, 0, 0, 2], [10, 0, 0, 7], 43210, 8080),
            &FlowStats::default(),
        )
        .unwrap();
        assert_eq!(flow.protocols, 0);
    }

    #[test]
    fn rtt_is_the_average_of_the_sampled_connections() {
        let flow = to_flow(
            &key([10, 0, 0, 2], [10, 0, 0, 7], 43210, 8080),
            &FlowStats {
                rtt_us_sum: 12_000,
                rtt_samples: 4,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(flow.avg_rtt_us(), Some(3_000));
        // No sample yet: unknown, not zero.
        let flow = to_flow(
            &key([10, 0, 0, 2], [10, 0, 0, 7], 43210, 8080),
            &FlowStats::default(),
        )
        .unwrap();
        assert_eq!(flow.avg_rtt_us(), None);
    }
}
