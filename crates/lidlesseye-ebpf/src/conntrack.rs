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
use aya::maps::{HashMap as AyaHashMap, MapData};
use aya::programs::uprobe::UProbeScope;
use aya::programs::{KProbe, UProbe};
use tracing::{debug, info, warn};

use crate::procfs;

const NET_BPF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/net.bpf.o"));

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
    /// Bit set of application protocols recognised in the payload. Zero when
    /// the socket carried nothing parseable; the map then falls back to the
    /// server port's guess.
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
    protocols: u32,
}

unsafe impl aya::Pod for FlowStats {}

/// L7 detail from the `flow_l7` map, keyed by the same flow. Separate from
/// [`FlowStats`] so the send probes do not carry 240 bytes of buffers on
/// their stack; that alone made the kernel reject them with EINVAL.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FlowL7 {
    host: [u8; 80],
    sql: [u8; 96],
}

unsafe impl aya::Pod for FlowL7 {}

/// Plaintext detail captured by the optional TLS uprobe, keyed by cgroup.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct SslDetail {
    host: [u8; 80],
    sql: [u8; 160],
}

unsafe impl aya::Pod for SslDetail {}

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

/// Read and clear the TLS detail map. A cgroup may have several connections;
/// the newest statement is what the map shows, so one entry per cgroup.
fn drain_ssl_details(ebpf: &mut Ebpf) -> HashMap<u64, (String, String)> {
    let Some(map) = ebpf.map_mut("ssl_details") else {
        return HashMap::new();
    };
    let Ok(mut map) = AyaHashMap::<&mut MapData, u64, SslDetail>::try_from(map) else {
        return HashMap::new();
    };
    let keys: Vec<u64> = map.keys().filter_map(|key| key.ok()).collect();
    let mut out = HashMap::new();
    for key in &keys {
        if let Ok(detail) = map.get(key, 0) {
            out.insert(*key, (fixed_str(&detail.host), fixed_str(&detail.sql)));
        }
    }
    for key in keys {
        let _ = map.remove(&key);
    }
    out
}

/// Read the plaintext L7 detail written by `lidlesseye_tcp_l7`. The map is
/// not cleared: it holds the latest statement per flow, and a window with no
/// new request should still show it.
fn drain_flow_l7(ebpf: &mut Ebpf) -> HashMap<FlowKey, (String, String)> {
    let Some(map) = ebpf.map_mut("flow_l7_map") else {
        return HashMap::new();
    };
    let Ok(mut map) = AyaHashMap::<&mut MapData, FlowKey, FlowL7>::try_from(map) else {
        return HashMap::new();
    };
    let keys: Vec<FlowKey> = map.keys().filter_map(|key| key.ok()).collect();
    let mut out = HashMap::new();
    for key in &keys {
        if let Ok(detail) = map.get(key, 0) {
            out.insert(*key, (fixed_str(&detail.host), fixed_str(&detail.sql)));
        }
    }
    out
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

    let mut ebpf = aya::EbpfLoader::new()
        .load(NET_BPF)
        .map_err(|err| format!("failed to load net.bpf.o: {err}"))?;

    // The data probes are best effort: a kernel that renames or inlines one of
    // these keeps the map, it just cannot report bytes and retransmits. They
    // are attached first so that a failure of the lifecycle probe below still
    // leaves a record of what this kernel does support.
    let mut attached: Vec<&'static str> = Vec::new();
    for (program, symbols) in [
        ("lidlesseye_tcp_send", &["tcp_sendmsg"][..]),
        // `lidlesseye_tcp_l7` is compiled into the object but not attached:
        // its bounded copy loops push the program over what this kernel's
        // verifier accepts on the send path. The SSL uprobe covers encrypted
        // traffic; plaintext L7 detail is the remaining gap.
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
    info!(programs = %attached.join(","), "conntrack programs attached");

    // Optional and best-effort: a host with no readable TLS library keeps the
    // map, it just cannot name the Host or the SQL behind encryption.
    if config.ssl_uprobe {
        let _ = attach_ssl_uprobes(&mut ebpf);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let flows = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&flows);
    let interval = config.flush_interval.max(Duration::from_secs(1));

    let thread = thread::Builder::new()
        .name("lidlesseye-conntrack".to_string())
        .spawn(move || {
            let mut ebpf = ebpf;
            while !flag.load(Ordering::Relaxed) {
                let deadline = std::time::Instant::now() + interval;
                while std::time::Instant::now() < deadline && !flag.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(200));
                }
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                match drain(&mut ebpf) {
                    Ok(batch) => {
                        if !batch.is_empty() {
                            counter.fetch_add(batch.len() as u64, Ordering::Relaxed);
                            debug!(flows = batch.len(), "drained connection window");
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

fn drain(ebpf: &mut Ebpf) -> Result<Vec<Flow>, String> {
    // Read the TLS detail first: it is keyed by cgroup, and the flows below
    // may belong to the cgroups that just wrote a statement.
    let details = drain_ssl_details(ebpf);
    let l7 = drain_flow_l7(ebpf);
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
            // The uprobe has no socket to key on, so it stores detail per
            // cgroup; attaching it here is what ties an encrypted Host/SQL to
            // the connection the cgroup opened.
            // Plaintext detail comes from the flow-keyed map; encrypted
            // detail from the cgroup-keyed uprobe map. The latter wins when
            // both are present, because a TLS flow never writes plaintext.
            if let Some((host, sql)) = l7.get(key) {
                if !host.is_empty() {
                    flow.host = host.clone();
                }
                if !sql.is_empty() {
                    flow.sql = sql.clone();
                }
            }
            if let Some((host, sql)) = details.get(&flow.cgroup) {
                if !host.is_empty() {
                    flow.host = host.clone();
                }
                if !sql.is_empty() {
                    flow.sql = sql.clone();
                }
            }
            out.push(flow);
        }
    }
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
        protocols: stats.protocols,
        // Filled from `flow_l7` / `ssl_details` during the drain.
        host: String::new(),
        sql: String::new(),
    })
}

/// Read a NUL-terminated fixed-size field from the map. The kernel zero-fills
/// the arrays, so a missing value is simply an empty string.
fn fixed_str(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
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

    /// The bit values are duplicated between Rust and C because the kernel
    /// object cannot import the Rust enum. A source contract is the cheapest
    /// way to stop one side drifting; the object is compiled in CI as well.
    #[test]
    fn the_probe_and_the_core_agree_on_protocol_bits() {
        use lidlesseye_core::servicemap::protocol as p;
        let source = include_str!("../bpf/net.bpf.c");
        for (name, value) in [
            ("PROTO_HTTP", p::HTTP),
            ("PROTO_HTTPS", p::HTTPS),
            ("PROTO_POSTGRES", p::POSTGRES),
            ("PROTO_MYSQL", p::MYSQL),
            ("PROTO_MONGODB", p::MONGODB),
            ("PROTO_REDIS", p::REDIS),
            ("PROTO_MEMCACHED", p::MEMCACHED),
            ("PROTO_CASSANDRA", p::CASSANDRA),
            ("PROTO_KAFKA", p::KAFKA),
            ("PROTO_CLICKHOUSE", p::CLICKHOUSE),
            ("PROTO_ZOOKEEPER", p::ZOOKEEPER),
            ("PROTO_FOUNDATIONDB", p::FOUNDATIONDB),
            ("PROTO_HTTP2", p::HTTP2),
            ("PROTO_DNS", p::DNS),
            ("PROTO_GRPC", p::GRPC),
        ] {
            let expected = format!("#define {name:<17} (1U << {})", value.trailing_zeros());
            assert!(
                source.contains(&expected),
                "{name} changed on one side of the ABI"
            );
        }
        assert!(source.contains("read_send_payload"));
        assert!(source.contains("stats->protocols |= detected"));
    }

    #[test]
    fn a_payload_hit_travels_with_the_flow() {
        let flow = to_flow(
            &key([10, 0, 0, 2], [10, 0, 0, 7], 43210, 8080),
            &FlowStats {
                protocols: 0x1001, // HTTP and ClickHouse
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(flow.protocols, 0x1001);
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
