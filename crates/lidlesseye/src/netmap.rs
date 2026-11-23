//! The service map collector.
//!
//! ## Two sources, one rule set
//!
//! The map is built from connections, and there are two ways to see them:
//!
//!   * **eBPF** (preferred) — `bpf/net.bpf.c` hooks `tcp_set_state`, so every
//!     completed handshake is seen no matter how briefly the socket lives, and
//!     the addresses are the ones the application wrote, before kube-proxy
//!     rewrites them. It also counts bytes and retransmits per connection and
//!     samples the round-trip time. The direction is read off the handshake
//!     itself (`SYN_SENT` -> established is a client, `SYN_RECV` ->
//!     established is a server), so no listen-set guessing is needed.
//!   * **`/proc/<pid>/net/tcp`** (fallback) — a pod's TCP table is available
//!     in that process's network namespace, so one `hostPID` agent can read
//!     every pod's table without `setns`. Nothing needs to be loaded into the
//!     kernel, which is what makes this work on a kernel without BTF or when
//!     the agent is not allowed to load programs.
//!
//! Both feed the same attribution. Direction comes from the source: the eBPF
//! probe knows it from the handshake, `/proc` infers it because the side whose
//! local port is in its own listen set is the server. A connection between two
//! pods is counted **once**, by the client's agent. ClusterIP addresses resolve
//! through the Service index, so `10.96.0.10:443` becomes `default/kubernetes`
//! instead of "external".
//!
//! ## What it deliberately does not do
//!
//! There is no L7 parsing. The protocol on an edge is inferred from the server
//! port (`https`, `postgres`, ...), not from the payload: telling gRPC from
//! HTTP needs either plaintext traffic or a TLS uprobe, and a wrong label is
//! worse than a coarse one. "Requests" would need the same inspection, so edges
//! report connections, bytes, retransmits and RTT — the numbers the kernel
//! actually exposes.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lidlesseye_core::servicemap::{
    EXTERNAL_ID, InstanceInfo, NodeKind, ServiceEdge, ServiceMapBatch, ServiceNode,
    protocol_for_port, protocol_label,
};
use lidlesseye_ebpf::conntrack::{self, ConntrackConfig, ConntrackHandle, Flow, Role};
use lidlesseye_ebpf::k8s::{PodIndex, PodMeta};
use tracing::{debug, warn};

use crate::k8s::ServiceIndex;

/// Where a socket's peer ended up.
///
/// Boxed because a [`PodMeta`] is the big variant by an order of magnitude —
/// it carries the pod's container and port lists — and every resolution would
/// otherwise move that payload around by value.
enum Peer {
    Pod(Box<PodMeta>),
    Service(crate::k8s::ServiceRef),
    External,
}

/// A connection as `/proc/net/tcp` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SocketKey {
    local: (IpAddr, u16),
    remote: (IpAddr, u16),
}

#[derive(Debug, Clone, Copy)]
struct Socket {
    key: SocketKey,
    state: u8,
    retrans: u32,
}

const TCP_ESTABLISHED: u8 = 0x01;
const TCP_LISTEN: u8 = 0x0a;

/// States that still describe a connection worth counting.
///
/// Polling `/proc/net/tcp` only sees what is open at the instant of the scan,
/// so a request/response pair that opens and closes between two scans would be
/// invisible — which is most of the traffic in a cluster (scrapes, health
/// checks, API calls). The closing states linger for a minute or more after the
/// peer is gone, so counting them recovers those connections without a second
/// eBPF program. `SYN_SENT`/`SYN_RECV` are left out: a refused or unanswered
/// connect is not a connection the map should draw.
fn counted_state(state: u8) -> bool {
    matches!(
        state,
        TCP_ESTABLISHED // 0x01
            | 0x04 // FIN_WAIT1
            | 0x05 // FIN_WAIT2
            | 0x06 // TIME_WAIT
            | 0x07 // CLOSE
            | 0x08 // CLOSE_WAIT
            | 0x09 // LAST_ACK
            | 0x0b // CLOSING
    )
}

/// Tunables for the collector.
#[derive(Debug, Clone)]
pub struct NetMapConfig {
    /// How often the connection tables are read. The dashboard sums the
    /// resulting windows, so this only trades freshness for work.
    pub interval: Duration,
    /// Cluster-wide pod index, used to turn a peer address into a service.
    pub pods: Arc<PodIndex>,
    /// ClusterIP -> Service, when the API allowed listing Services.
    pub services: Option<Arc<ServiceIndex>>,
    /// Prefer the eBPF source, falling back to `/proc` when it cannot attach.
    pub ebpf: bool,
    /// Attach TLS uprobes to read the plaintext Host/SQL behind encryption.
    /// Off by default: every TLS write pays a user-memory read and parsing.
    pub ssl_uprobe: bool,
}

impl NetMapConfig {
    pub fn new(pods: Arc<PodIndex>) -> Self {
        NetMapConfig {
            interval: Duration::from_secs(10),
            pods,
            services: None,
            ebpf: true,
            // Opt-in because the uprobe runs on every TLS write. The DaemonSet
            // enables it with `LIDLESSEYE_SSL_UPROBE=1` rather than a CLI flag,
            // so an embedded agent does not grow an argument it cannot use.
            ssl_uprobe: std::env::var("LIDLESSEYE_SSL_UPROBE")
                .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
                .unwrap_or(false),
        }
    }
}

/// Handle for a running collector.
pub struct NetMapHandle {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    /// Set when the eBPF source is in use; stopping the scanner thread would
    /// not reach it.
    conntrack: Option<ConntrackHandle>,
}

impl NetMapHandle {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        if let Some(mut conntrack) = self.conntrack.take() {
            conntrack.stop();
        }
    }
}

impl Drop for NetMapHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the collector. The callback runs on the collector thread; it must not
/// block on anything that needs the caller's runtime.
pub fn start<F>(config: NetMapConfig, sink: F) -> NetMapHandle
where
    F: Fn(ServiceMapBatch) + Send + Sync + 'static,
{
    let sink = Arc::new(sink);
    if config.ebpf {
        match start_ebpf(&config, Arc::clone(&sink)) {
            Ok(conntrack) => {
                return NetMapHandle {
                    stop: Arc::new(AtomicBool::new(false)),
                    join: None,
                    conntrack: Some(conntrack),
                };
            }
            Err(err) => warn!(
                error = %err,
                "eBPF connection accounting unavailable; falling back to /proc scanning"
            ),
        }
    }
    let (handle_stop, join) = start_scan(config, sink);
    NetMapHandle {
        stop: handle_stop,
        join,
        conntrack: None,
    }
}

/// Drain the eBPF counters and turn them into map windows.
fn start_ebpf<F>(config: &NetMapConfig, sink: Arc<F>) -> Result<ConntrackHandle, String>
where
    F: Fn(ServiceMapBatch) + Send + Sync + 'static,
{
    let mapper = FlowMapper {
        pods: Arc::clone(&config.pods),
        services: config.services.clone(),
        node: lidlesseye_ebpf::procfs::hostname(),
    };
    conntrack::start(
        ConntrackConfig {
            flush_interval: config.interval,
            ssl_uprobe: config.ssl_uprobe,
        },
        move |flows| {
            let batch = mapper.batch(flows, lidlesseye_core::now_ns());
            if !batch.is_empty() {
                sink(batch);
            }
        },
    )
}

/// Scan every pod's network namespace for open sockets.
fn start_scan<F>(
    config: NetMapConfig,
    sink: Arc<F>,
) -> (Arc<AtomicBool>, Option<std::thread::JoinHandle<()>>)
where
    F: Fn(ServiceMapBatch) + Send + Sync + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let interval = config.interval.max(Duration::from_secs(1));
    let join = std::thread::Builder::new()
        .name("lidlesseye-netmap".to_string())
        .spawn(move || {
            let mut scanner = Scanner::default();
            let node = lidlesseye_ebpf::procfs::hostname();
            guard_memlock();
            while !flag.load(Ordering::Relaxed) {
                let started = lidlesseye_core::now_ns();
                match scanner.scan(&config, &node, started) {
                    Ok(batch) => {
                        if !batch.is_empty() {
                            debug!(
                                nodes = batch.nodes.len(),
                                edges = batch.edges.len(),
                                "service map window"
                            );
                            sink(batch);
                        }
                    }
                    Err(err) => warn!(error = %err, "service map scan failed"),
                }
                // Sleep in slices so `stop()` does not wait a whole interval.
                let deadline = std::time::Instant::now() + interval;
                while std::time::Instant::now() < deadline && !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        });
    match join {
        Ok(join) => (stop, Some(join)),
        Err(err) => {
            warn!(error = %err, "could not start the service map collector");
            (stop, None)
        }
    }
}

/// Turns drained eBPF flows into one map window.
///
/// The observer of a flow is a pod, so the pod's own address is enough to
/// identify it — no pid lookups, and therefore no race with a process exiting
/// between the event and the drain.
struct FlowMapper {
    pods: Arc<PodIndex>,
    services: Option<Arc<ServiceIndex>>,
    node: String,
}

impl FlowMapper {
    fn batch(&self, flows: Vec<Flow>, start_ns: u64) -> ServiceMapBatch {
        let mut batch = ServiceMapBatch {
            start_ns,
            end_ns: lidlesseye_core::now_ns(),
            node: self.node.clone(),
            ..Default::default()
        };
        let mut nodes: HashMap<String, ServiceNode> = HashMap::new();
        let mut edges: HashMap<(String, String, String, String, String), ServiceEdge> =
            HashMap::new();
        let containers = ContainerMap::scan(&self.pods);
        let mut skipped = 0usize;
        let mut hairpin = 0usize;
        let mut ambiguous = 0usize;

        for flow in flows {
            if flow.opens == 0 && flow.tx_bytes == 0 && flow.rx_bytes == 0 && flow.retransmits == 0
            {
                continue;
            }
            // A socket that belongs to no pod is the node's own traffic (the
            // kubelet, the agent itself). It has no place in a service map.
            let Some(meta) = self.pods.lookup_ip(flow.local.0, flow.local.1) else {
                skipped += 1;
                continue;
            };
            let local_node = pod_node(&meta);
            let peer = resolve_peer(
                &self.pods,
                self.services.as_deref(),
                flow.remote.0,
                flow.remote.1,
            );

            let role = match flow.role {
                Role::Client | Role::Server => flow.role,
                Role::Unknown => match infer_role(&flow, &meta, &peer) {
                    Some(role) => role,
                    None => {
                        ambiguous += 1;
                        continue;
                    }
                },
            };

            let (client, server, server_port, swapped) = match (role, &peer) {
                (Role::Client, Peer::Pod(other)) => {
                    let node = pod_node(other);
                    if node.id == local_node.id {
                        // A pod talking to itself is not an edge.
                        continue;
                    }
                    (local_node.id.clone(), node.id.clone(), flow.remote.1, false)
                }
                (Role::Client, Peer::Service(service)) => {
                    (local_node.id.clone(), service.id(), flow.remote.1, false)
                }
                (Role::Client, Peer::External) => (
                    local_node.id.clone(),
                    EXTERNAL_ID.to_string(),
                    flow.remote.1,
                    false,
                ),
                // The client's own agent records a pod-to-pod edge, so the
                // server side only contributes the inbound-from-outside case
                // — otherwise every internal edge would be counted twice.
                (Role::Server, Peer::External) => {
                    if is_cluster_private(flow.remote.0) {
                        // kube-proxy rewrites the source of a same-node
                        // pod-to-ClusterIP connection to the node's bridge
                        // address (hairpin SNAT), so the server's socket
                        // reports a private peer that belongs to no pod. The
                        // client's agent names that connection; drawing this
                        // view too would count the traffic twice and blame an
                        // outside client that does not exist.
                        hairpin += 1;
                        continue;
                    }
                    (
                        EXTERNAL_ID.to_string(),
                        local_node.id.clone(),
                        flow.local.1,
                        true,
                    )
                }
                (Role::Server, _) | (Role::Unknown, _) => continue,
            };

            // A payload hit names what this connection actually carried; the
            // port is only the fallback for a socket we could not sniff (TLS
            // without a user-space uprobe, a connection that predates the
            // agent, or an opaque protocol).
            let protocol = protocol_label(flow.protocols, protocol_for_port(server_port));
            // The kernel parsed these off the client's send buffer; clone them
            // before the edge map takes ownership of the slot.
            let host = flow.host.clone();
            let sql = flow.sql.clone();
            let (tx, rx) = if swapped {
                (flow.rx_bytes, flow.tx_bytes)
            } else {
                (flow.tx_bytes, flow.rx_bytes)
            };

            // The observing socket belongs to one container of the pod, and the
            // peer may be a pod whose port names one on its side. Both are
            // empty when the kernel and the specs cannot say.
            let local_container = if role == Role::Client {
                // The connecting task is the exact answer; the socket itself
                // has usually closed by the time /proc is scanned.
                containers
                    .by_handshake(flow.cgroup)
                    .map(str::to_string)
                    .unwrap_or_else(|| containers.container_of(flow.local))
            } else {
                // An accepted socket: its owner is the container that declared
                // the port, because the accept ran in softirq, where the
                // handshake's cgroup says nothing about the connection.
                meta.container_by_port(flow.local.1)
                    .map(|info| info.name.clone())
                    .unwrap_or_else(|| containers.container_of(flow.local))
            };
            let (client_container, server_container) = if swapped {
                (String::new(), local_container)
            } else {
                let peer_container = match &peer {
                    Peer::Pod(other) => other
                        .container_by_port(flow.remote.1)
                        .map(|info| info.name.clone())
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                (local_container, peer_container)
            };

            let this = nodes
                .entry(local_node.id.clone())
                .or_insert_with(|| local_node.clone());
            this.tx_bytes += flow.tx_bytes;
            this.rx_bytes += flow.rx_bytes;
            this.retransmits += flow.retransmits;
            this.connections += flow.opens;

            let peer_node = match &peer {
                Peer::Pod(other) => Some(pod_node(other)),
                Peer::Service(service) => Some(service_node(service)),
                Peer::External => Some(external_node()),
            };
            if let Some(peer_node) = peer_node.filter(|node| node.id != local_node.id) {
                let slot = nodes.entry(peer_node.id.clone()).or_insert(peer_node);
                slot.tx_bytes += tx;
                slot.rx_bytes += rx;
                slot.connections += flow.opens;
            }

            let key = (
                client.clone(),
                server.clone(),
                protocol.clone(),
                client_container.clone(),
                server_container.clone(),
            );
            let edge = edges.entry(key).or_insert_with(|| ServiceEdge {
                client,
                server,
                protocol,
                client_container,
                server_container,
                ..Default::default()
            });
            edge.connections += flow.opens;
            edge.retransmits += flow.retransmits;
            edge.tx_bytes += tx;
            edge.rx_bytes += rx;
            edge.rtt_us_sum += flow.rtt_us_sum;
            edge.rtt_samples += flow.rtt_samples;
            // Several flows collapse into one edge; keep the newest non-empty
            // detail rather than blanking a value a quieter flow just set.
            if !host.is_empty() {
                edge.host = host;
            }
            if !sql.is_empty() {
                edge.sql = sql;
            }
        }

        if skipped > 0 {
            debug!(
                skipped,
                "connections of processes that belong to no pod were left out"
            );
        }
        if ambiguous > 0 {
            debug!(
                ambiguous,
                "connections whose handshake predates the agent and whose direction cannot be read from the pod specs were left out"
            );
        }
        if hairpin > 0 {
            debug!(
                hairpin,
                "inbound connections whose client was rewritten by kube-proxy were left to the client's agent"
            );
        }
        if !nodes.is_empty() {
            attach_health(&mut nodes, &health_by_node(&self.pods));
        }
        batch.nodes = nodes.into_values().collect();
        batch.edges = edges.into_values().collect();
        batch.end_ns = lidlesseye_core::now_ns();
        batch
    }
}

/// Which container owns which local socket, for the pods in one window of eBPF
/// flows.
///
/// The probe sees a 5-tuple, not a cgroup: the local address is the pod and the
/// local port is the socket. Tying that socket back to a container means
/// walking `/proc`, so the walk runs once per batch — and it keys on the
/// *address*, not on the pod identity, because a `hostNetwork` pod and the
/// control plane share the node's address: resolving the flow to a pod first
/// would pick whichever of them the pod list happens to return and then look
/// for the socket in the wrong process list.
#[derive(Default)]
struct ContainerMap {
    /// Cgroup id from the handshake -> container name. Exact, and it survives
    /// a connection that closed before the scan ran — which is most of them.
    by_cgroup: HashMap<u64, String>,
    /// Local endpoint of every socket whose owner the pod list can name. The
    /// fallback for a socket whose handshake predates the agent.
    owned: HashMap<(IpAddr, u16), String>,
}

impl ContainerMap {
    fn scan(pods: &PodIndex) -> Self {
        // Container id -> name. The id is the only link between a cgroup and a
        // human-readable container: a static pod's cgroup carries a kubelet
        // hash instead of the object's UID, so the UID is not an option.
        let snapshot = pods.snapshot();
        let mut owners: HashMap<&str, &str> = HashMap::new();
        for meta in snapshot.values() {
            for container in &meta.containers {
                if !container.id.is_empty() {
                    owners
                        .entry(container.id.as_str())
                        .or_insert(container.name.as_str());
                }
            }
        }

        let mut by_cgroup = HashMap::new();
        let mut owned = HashMap::new();
        // The TCP table belongs to the network namespace, and a pod's
        // containers share one: reading it per pod beats reading it per pid.
        let mut tables: HashMap<Option<u64>, HashMap<u64, (IpAddr, u16)>> = HashMap::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Self::default();
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Some(cgroup) = lidlesseye_ebpf::procfs::read_cgroup(pid) else {
                continue;
            };
            let Some(id) = lidlesseye_ebpf::procfs::parse_container_id(&cgroup) else {
                continue;
            };
            let Some(name) = owners.get(id.as_str()) else {
                continue;
            };
            if let Some(group) = cgroup_id(&cgroup) {
                by_cgroup.insert(group, (*name).to_string());
            }
            let namespace = netns_of(pid);
            let table = tables
                .entry(namespace)
                .or_insert_with(|| socket_endpoints(pid));
            for inode in own_socket_inodes(pid) {
                if let Some(local) = table.get(&inode) {
                    owned.insert(*local, (*name).to_string());
                }
            }
        }
        ContainerMap { by_cgroup, owned }
    }

    /// Container that owns the socket at `local`, or an empty string.
    fn container_of(&self, local: (IpAddr, u16)) -> String {
        self.owned.get(&local).cloned().unwrap_or_default()
    }

    /// Container that ran the task which completed the handshake.
    fn by_handshake(&self, cgroup: u64) -> Option<&str> {
        if cgroup == 0 {
            // No handshake in this window (a keep-alive that predates the
            // agent), or a context that has no cgroup of its own.
            return None;
        }
        self.by_cgroup.get(&cgroup).map(String::as_str)
    }
}

/// Kernel cgroup id of a cgroup path — what `bpf_get_current_cgroup_id`
/// returns for a task in it.
///
/// The id is the kernfs inode of the cgroup directory. The agent runs with
/// `hostPID`, so pid 1 is the host's init and `/proc/1/root` is the host's root
/// filesystem, which is where the full hierarchy lives; the agent's own
/// `/sys/fs/cgroup` is only its container's subtree and is tried as a fallback
/// for a host install.
fn cgroup_id(path: &str) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    for root in ["/proc/1/root/sys/fs/cgroup", "/sys/fs/cgroup"] {
        if let Ok(meta) = std::fs::metadata(format!("{root}{path}")) {
            return Some(meta.ino());
        }
    }
    None
}

/// Reading another process's `/proc/<pid>/net` needs ptrace-level access; the
/// agent runs privileged, this only makes a missing capability visible.
fn guard_memlock() {
    if !lidlesseye_ebpf::procfs::is_privileged() {
        warn!("not privileged: /proc/<pid>/net may be unreadable for other pods");
    }
}

struct PodSockets {
    meta: PodMeta,
    /// Ports this pod listens on, from the same `/proc/net/tcp` snapshot.
    listen: HashSet<u16>,
    sockets: Vec<Socket>,
    rx_bytes: u64,
    tx_bytes: u64,
    /// Local port -> container name, for the sockets the pod's processes own.
    /// A port with no entry belongs to a process the scan could not attribute.
    containers: HashMap<u16, String>,
}

#[derive(Default)]
struct Scanner {
    /// Last seen retransmit counter per socket, so a window reports a delta.
    known: HashMap<String, HashMap<SocketKey, u32>>,
    /// Last cumulative interface counters per pod.
    traffic: HashMap<String, (u64, u64)>,
}

impl Scanner {
    fn scan(
        &mut self,
        config: &NetMapConfig,
        node: &str,
        started_ns: u64,
    ) -> Result<ServiceMapBatch, String> {
        let pods = self.local_pods(config);
        let mut batch = ServiceMapBatch {
            start_ns: started_ns,
            end_ns: lidlesseye_core::now_ns(),
            node: node.to_string(),
            ..Default::default()
        };
        if pods.is_empty() {
            return Ok(batch);
        }

        // Node rows by id: several pods of one Deployment collapse into one.
        let mut nodes: HashMap<String, ServiceNode> = HashMap::new();
        // Peers named by an edge, materialised only if an edge references them.
        let mut peers: HashMap<String, ServiceNode> = HashMap::new();

        // Sockets are compared against the previous scan only, and the new map
        // is rebuilt from what this scan sees: a closed connection must not
        // keep a slot forever.
        let previous = std::mem::take(&mut self.known);
        let mut live: HashSet<String> = HashSet::new();
        // A connection involves two nodes, so both of them see it: the edge is
        // counted once (client side), the node counter is per node.
        let mut node_connections: HashMap<String, (u64, u64)> = HashMap::new();
        // Peers the map cannot name, for the debug log below: an unresolved
        // address is either genuinely outside the cluster or a gap in the pod
        // index, and only the address itself tells the two apart.
        let mut unnamed: HashSet<(IpAddr, u16)> = HashSet::new();

        for pod in &pods {
            live.insert(pod.meta.uid.clone());
            let (rx, tx) = self.traffic_delta(&pod.meta.uid, pod.rx_bytes, pod.tx_bytes);
            let node = pod_node(&pod.meta);
            let slot = nodes.entry(node.id.clone()).or_insert(node);
            slot.rx_bytes += rx;
            slot.tx_bytes += tx;

            let mut edges: HashMap<(String, String, String, String, String), ServiceEdge> =
                HashMap::new();
            for socket in &pod.sockets {
                if !counted_state(socket.state) {
                    continue;
                }
                let Some(edge) =
                    self.attribute(config, pod, socket, &previous, &mut peers, &mut unnamed)
                else {
                    continue;
                };
                let key = (
                    edge.client.clone(),
                    edge.server.clone(),
                    edge.protocol.clone(),
                    edge.client_container.clone(),
                    edge.server_container.clone(),
                );
                edges
                    .entry(key)
                    .and_modify(|existing| {
                        existing.connections += edge.connections;
                        existing.retransmits += edge.retransmits;
                    })
                    .or_insert(edge);
            }
            for edge in edges.values() {
                let counts = (edge.connections, edge.retransmits);
                let client = node_connections
                    .entry(edge.client.clone())
                    .or_insert((0, 0));
                client.0 += counts.0;
                client.1 += counts.1;
                if edge.server != edge.client {
                    let server = node_connections
                        .entry(edge.server.clone())
                        .or_insert((0, 0));
                    server.0 += counts.0;
                    server.1 += counts.1;
                }
            }
            batch.edges.extend(edges.into_values());
        }

        for (id, peer) in peers {
            nodes.entry(id).or_insert(peer);
        }
        for (id, (connections, retransmits)) in node_connections {
            if let Some(slot) = nodes.get_mut(&id) {
                slot.connections += connections;
                slot.retransmits += retransmits;
            }
        }
        self.traffic.retain(|uid, _| live.contains(uid));
        if !unnamed.is_empty() {
            let mut sample: Vec<String> = unnamed
                .iter()
                .take(8)
                .map(|(ip, port)| format!("{ip}:{port}"))
                .collect();
            sample.sort();
            debug!(
                peers = unnamed.len(),
                sample = %sample.join(" "),
                "service map peers with no name"
            );
        }
        if !nodes.is_empty() {
            attach_health(&mut nodes, &health_by_node(&config.pods));
        }
        batch.nodes = nodes.into_values().collect();
        batch.end_ns = lidlesseye_core::now_ns();
        Ok(batch)
    }

    /// One representative pid per pod, with the pod's metadata.
    fn local_pods(&self, config: &NetMapConfig) -> Vec<PodSockets> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return out;
        };
        // A pod with `hostNetwork: true` shares this namespace, so its
        // `/proc/<pid>/net` is the node's: every socket on the machine and the
        // node's interface counters. Reading it as if it were the pod's would
        // credit the whole node to kube-proxy, kindnet and node-exporter alike
        // (their byte counters come out identical). Such pods are identified by
        // comparing network namespaces against pid 1 of the host PID namespace,
        // and only the sockets they actually hold open are kept.
        let host_netns = netns_of(1);
        let mut seen: HashSet<String> = HashSet::new();
        // Pods already in `out`, so a second process of the same pod can still
        // contribute its container attribution without a second socket table.
        let mut placed: HashMap<String, usize> = HashMap::new();
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Some(cgroup) = lidlesseye_ebpf::procfs::read_cgroup(pid) else {
                continue;
            };
            let Some(uid) = lidlesseye_ebpf::procfs::parse_pod_uid(&cgroup) else {
                continue;
            };
            if let Some(&index) = placed.get(&uid) {
                // Every container of a pod shares its network namespace, so the
                // sockets are the pod's either way; what the extra process adds
                // is the link from a socket to the container that owns it.
                if let Some(name) = container_name(config, &uid, &cgroup)
                    && let Some(pod) = out.get_mut(index)
                {
                    for port in ports_of_process(pid) {
                        pod.containers.insert(port, name.clone());
                    }
                }
                continue;
            }
            if !seen.insert(uid.clone()) {
                continue;
            }
            let Some(meta) = config.pods.lookup(&uid) else {
                // The pod started after the last index refresh; its traffic is
                // picked up on the next one instead of being mislabelled now.
                continue;
            };
            let shared = host_netns.is_some() && netns_of(pid) == host_netns;
            let owned = shared.then(|| own_socket_inodes(pid));
            let (mut sockets, listen) = read_sockets(pid, owned.as_ref());
            if sockets.is_empty() && listen.is_empty() {
                continue;
            }
            sockets.retain(|socket| !socket.key.remote.0.is_loopback());
            // The interface counters belong to the node in a shared namespace,
            // and there is no per-pod byte count to substitute: report none
            // rather than the node's totals under every host-network pod.
            let (rx_bytes, tx_bytes) = if shared { (0, 0) } else { read_netdev(pid) };
            let mut containers = HashMap::new();
            if let Some(name) = container_name(config, &uid, &cgroup) {
                for port in ports_of_process(pid) {
                    containers.insert(port, name.clone());
                }
            }
            placed.insert(uid, out.len());
            out.push(PodSockets {
                meta,
                listen,
                sockets,
                rx_bytes,
                tx_bytes,
                containers,
            });
        }
        out
    }

    /// Interface counters are cumulative: the window reports the difference.
    /// A pod seen for the first time reports zero, because its counters start
    /// at boot, not at this window.
    fn traffic_delta(&mut self, uid: &str, rx: u64, tx: u64) -> (u64, u64) {
        let previous = self.traffic.insert(uid.to_string(), (rx, tx));
        match previous {
            Some((prev_rx, prev_tx)) => (rx.saturating_sub(prev_rx), tx.saturating_sub(prev_tx)),
            None => (0, 0),
        }
    }

    /// Turn one socket into an edge, or `None` when it must not be counted
    /// here (a server-side view of a pod-to-pod connection, a self connection,
    /// or a peer on a network the map does not name).
    fn attribute(
        &mut self,
        config: &NetMapConfig,
        pod: &PodSockets,
        socket: &Socket,
        previous: &HashMap<String, HashMap<SocketKey, u32>>,
        peers: &mut HashMap<String, ServiceNode>,
        unnamed: &mut HashSet<(IpAddr, u16)>,
    ) -> Option<ServiceEdge> {
        let (counted, retrans) = self.new_connection(previous, pod, socket);
        if !counted && retrans == 0 {
            return None;
        }

        let local_is_server = pod.listen.contains(&socket.key.local.1);
        let peer = resolve_peer(
            config.pods.as_ref(),
            config.services.as_deref(),
            socket.key.remote.0,
            socket.key.remote.1,
        );
        let this = pod_node(&pod.meta);
        let local_container = pod
            .containers
            .get(&socket.key.local.1)
            .cloned()
            .unwrap_or_default();

        let (client_node, server_node, server_port, server_container, server_pod) = match peer {
            Peer::Pod(meta) => {
                let node = pod_node(&meta);
                if local_is_server {
                    // The client's own agent records this edge.
                    return None;
                }
                if node.id == this.id {
                    return None;
                }
                let id = node.id.clone();
                let container = meta
                    .container_by_port(socket.key.remote.1)
                    .map(|info| info.name.clone())
                    .unwrap_or_default();
                peers.entry(id.clone()).or_insert(node);
                (
                    this.id,
                    id,
                    socket.key.remote.1,
                    container,
                    meta.uid.clone(),
                )
            }
            Peer::Service(service) => {
                if local_is_server {
                    return None;
                }
                let node = service_node(&service);
                peers.entry(node.id.clone()).or_insert(node);
                (
                    this.id,
                    service.id(),
                    socket.key.remote.1,
                    String::new(),
                    String::new(),
                )
            }
            Peer::External => {
                unnamed.insert((socket.key.remote.0, socket.key.remote.1));
                let id = lidlesseye_core::servicemap::EXTERNAL_ID.to_string();
                peers.entry(id.clone()).or_insert_with(external_node);
                if local_is_server {
                    // Inbound from outside the cluster: the pod is the server.
                    (
                        id,
                        this.id,
                        socket.key.local.1,
                        String::new(),
                        String::new(),
                    )
                } else {
                    // Outbound to something the map cannot name.
                    (
                        this.id,
                        id,
                        socket.key.remote.1,
                        String::new(),
                        String::new(),
                    )
                }
            }
        };

        // The edge's ends are known now: whichever side this pod is on gets
        // the container the socket belongs to.
        let (client_container, server_container) = if local_is_server {
            (String::new(), local_container)
        } else {
            (local_container, server_container)
        };
        // The local pod is the only one this agent can identify without a
        // cluster-wide lookup. A peer pod was resolved by the socket address,
        // so its UID travels with the edge too; services and the outside world
        // have no pod to name.
        let (client_pod, server_pod) = if local_is_server {
            (String::new(), pod.meta.uid.clone())
        } else {
            (pod.meta.uid.clone(), server_pod)
        };

        Some(ServiceEdge {
            client: client_node,
            server: server_node,
            protocol: protocol_for_port(server_port).to_string(),
            client_container,
            server_container,
            client_pod,
            server_pod,
            connections: u64::from(counted),
            retransmits: u64::from(retrans),
            ..Default::default()
        })
    }

    /// Is this socket new to the window, and how many retransmits did it add?
    fn new_connection(
        &mut self,
        previous: &HashMap<String, HashMap<SocketKey, u32>>,
        pod: &PodSockets,
        socket: &Socket,
    ) -> (bool, u32) {
        let before = previous
            .get(&pod.meta.uid)
            .and_then(|sockets| sockets.get(&socket.key))
            .copied();
        self.known
            .entry(pod.meta.uid.clone())
            .or_default()
            .insert(socket.key, socket.retrans);
        match before {
            Some(previous) => (false, socket.retrans.saturating_sub(previous)),
            None => (true, 0),
        }
    }
}

/// The direction of a connection whose handshake happened before this agent
/// was running.
///
/// The socket state alone does not say which end opened it, but the pod specs
/// do: a pod that listens on its local port is the server, and a peer that
/// listens on the remote port means this pod is the client. Anything else —
/// both sides on undeclared ports — is unknowable, and guessing would draw
/// half the edges backwards.
fn infer_role(flow: &Flow, meta: &PodMeta, peer: &Peer) -> Option<Role> {
    if meta.ports.contains(&flow.local.1) {
        return Some(Role::Server);
    }
    match peer {
        Peer::Pod(other) if other.ports.contains(&flow.remote.1) => Some(Role::Client),
        Peer::Pod(_) => None,
        // A virtual address has no listening socket of its own, so the pod is
        // the one that dialled it.
        Peer::Service(_) | Peer::External => Some(Role::Client),
    }
}

/// Whether an address is inside a range a cluster uses for itself.
///
/// A peer in one of these ranges that resolves to no pod and no Service is not
/// an outside client: it is the node's own bridge, after kube-proxy rewrote the
/// source of a hairpin connection. Public addresses are the ones that can only
/// come from outside.
fn is_cluster_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_link_local()
                || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]) // CGNAT
        }
        IpAddr::V6(v6) => {
            v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_private())
        }
    }
}

/// A ClusterIP Service node. The id is the selector the UI offers: there is no
/// workload to profile behind a virtual address.
fn service_node(service: &crate::k8s::ServiceRef) -> ServiceNode {
    let mut node = ServiceNode::new(service.id(), NodeKind::Service);
    node.namespace = service.namespace.clone();
    node.service = service.name.clone();
    node
}

fn external_node() -> ServiceNode {
    let mut node = ServiceNode::new(
        lidlesseye_core::servicemap::EXTERNAL_ID.to_string(),
        NodeKind::External,
    );
    node.service = "external".to_string();
    node
}

/// The name a pod contributes to the map.
///
/// Must match `lidlesseye_core::servicemap::node_id` as used by the CPU join:
/// the workload when the pod has one, its service name otherwise, and its own
/// name for a bare pod, which has neither.
fn map_name_of(meta: &PodMeta) -> String {
    meta.workload
        .clone()
        .or_else(|| meta.service_name().map(str::to_string))
        .unwrap_or_else(|| meta.name.clone())
}

fn pod_node(meta: &PodMeta) -> ServiceNode {
    let name = map_name_of(meta);
    let mut node = ServiceNode::new(
        lidlesseye_core::servicemap::node_id(&meta.namespace, &name),
        NodeKind::Workload,
    );
    node.namespace = meta.namespace.clone();
    node.workload = meta.workload.clone().unwrap_or_default();
    node.service = meta.service_name().unwrap_or(&name).to_string();
    node.containers = meta
        .containers
        .iter()
        .map(|container| container.name.clone())
        .collect();
    node
}

/// Replica count, ready count and the replicas themselves, per map node.
///
/// The counts say whether the workload is healthy; the list says which pod is
/// the sick one, and on which node it sits. Every agent reads the same
/// cluster-wide list, so the numbers agree across nodes.
fn health_by_node(pods: &PodIndex) -> HashMap<String, NodeHealth> {
    let mut out: HashMap<String, NodeHealth> = HashMap::new();
    for meta in pods.snapshot().values() {
        let id = lidlesseye_core::servicemap::node_id(&meta.namespace, &map_name_of(meta));
        let slot = out.entry(id).or_default();
        slot.pods += 1;
        for container in &meta.containers {
            if !slot.containers.contains(&container.name) {
                slot.containers.push(container.name.clone());
            }
        }
        if meta.ready {
            slot.ready += 1;
        }
        // A bounded list: the panel that shows it scrolls, and a 500-replica
        // deployment must not turn every map payload into megabytes.
        if slot.instances.len() < MAX_INSTANCES {
            slot.instances.push(InstanceInfo {
                name: meta.name.clone(),
                uid: meta.uid.clone(),
                node: meta.host.clone(),
                ready: meta.ready,
                phase: meta.phase.clone(),
            });
        }
    }
    for health in out.values_mut() {
        // Not-ready first, then by name: the exception belongs at the top of
        // the list, and a stable order keeps the panel from reshuffling.
        health
            .instances
            .sort_by(|a, b| a.ready.cmp(&b.ready).then_with(|| a.name.cmp(&b.name)));
        // Containers are a set, and their order is the spec's, which is the
        // order every other tool shows them in.
        health.containers.sort();
    }
    out
}

/// Replica gauges for one map node.
#[derive(Default)]
struct NodeHealth {
    pods: u32,
    ready: u32,
    instances: Vec<InstanceInfo>,
    containers: Vec<String>,
}

/// How many replicas of one workload a map payload carries.
const MAX_INSTANCES: usize = 24;

/// Stamp the replica gauges onto the nodes of a window.
///
/// Nodes the pod list does not know (a ClusterIP service, the outside world, a
/// workload whose pods all went away mid-window) keep zero, which the UI reads
/// as "no replicas to report" rather than "all replicas are down".
fn attach_health(nodes: &mut HashMap<String, ServiceNode>, health: &HashMap<String, NodeHealth>) {
    for node in nodes.values_mut() {
        if let Some(known) = health.get(&node.id) {
            node.pods = known.pods;
            node.ready = known.ready;
            node.instances = known.instances.clone();
            node.containers = known.containers.clone();
        }
    }
}

fn resolve_peer(pods: &PodIndex, services: Option<&ServiceIndex>, ip: IpAddr, port: u16) -> Peer {
    if ip.is_loopback() {
        return Peer::External;
    }
    if let Some(meta) = pods.lookup_ip(ip, port) {
        return Peer::Pod(Box::new(meta));
    }
    if let Some(service) = services.and_then(|index| index.lookup(ip, port)) {
        return Peer::Service(service);
    }
    Peer::External
}

/// The network namespace a process runs in, as `net:[4026532388]` -> inode.
fn netns_of(pid: u32) -> Option<u64> {
    let link = std::fs::read_link(format!("/proc/{pid}/ns/net")).ok()?;
    let text = link.to_str()?;
    text.strip_prefix("net:[")?.strip_suffix(']')?.parse().ok()
}

/// Inodes of the sockets a process holds open, from `socket:[1234]` fd links.
fn own_socket_inodes(pid: u32) -> HashSet<u64> {
    let mut out = HashSet::new();
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let Some(text) = target.to_str() else {
            continue;
        };
        let Some(rest) = text.strip_prefix("socket:[") else {
            continue;
        };
        let Some(inode) = rest.strip_suffix(']').and_then(|s| s.parse().ok()) else {
            continue;
        };
        out.insert(inode);
    }
    out
}

/// Sockets and listening ports of one process's network namespace.
fn read_sockets(pid: u32, only: Option<&HashSet<u64>>) -> (Vec<Socket>, HashSet<u16>) {
    let mut sockets = Vec::new();
    let mut listen = HashSet::new();
    for (path, v6) in [
        (format!("/proc/{pid}/net/tcp"), false),
        (format!("/proc/{pid}/net/tcp6"), true),
    ] {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 7 {
                continue;
            }
            if let Some(only) = only {
                // A shared network namespace lists every socket on the node;
                // the inode says which of them this process opened.
                let Some(inode) = fields.get(9).and_then(|f| f.parse::<u64>().ok()) else {
                    continue;
                };
                if !only.contains(&inode) {
                    continue;
                }
            }
            let Some(local) = parse_endpoint(fields[1], v6) else {
                continue;
            };
            let Some(remote) = parse_endpoint(fields[2], v6) else {
                continue;
            };
            let state = u8::from_str_radix(fields[3], 16).unwrap_or(0);
            if state == TCP_LISTEN {
                listen.insert(local.1);
                continue;
            }
            if remote.1 == 0 || remote.0.is_unspecified() {
                continue;
            }
            let retrans = u32::from_str_radix(fields[6], 16).unwrap_or(0);
            sockets.push(Socket {
                key: SocketKey { local, remote },
                state,
                retrans,
            });
        }
    }
    (sockets, listen)
}

/// The container a process belongs to, named as the Kubernetes API names it.
///
/// The cgroup only carries the runtime id, which nobody can read; the pod
/// index is what turns it into `app` or `istio-proxy`. `None` for a process
/// the index does not describe — a stopped container whose id outlived the
/// status entry, or a runtime whose cgroup layout is not recognised.
fn container_name(config: &NetMapConfig, uid: &str, cgroup: &str) -> Option<String> {
    let id = lidlesseye_ebpf::procfs::parse_container_id(cgroup)?;
    let meta = config.pods.lookup(uid)?;
    meta.container_by_id(&id).map(|info| info.name.clone())
}

/// Local ports of the sockets this process holds open.
///
/// `/proc/<pid>/net/tcp` belongs to the network *namespace*, which every
/// container of a pod shares: it lists the pods' sockets, not this process's.
/// The fd table is what makes the difference, so the inode of each open socket
/// is looked up in the namespace's table and translated to its local port.
fn ports_of_process(pid: u32) -> Vec<u16> {
    let endpoints = socket_endpoints(pid);
    let mut out: Vec<u16> = own_socket_inodes(pid)
        .iter()
        .filter_map(|inode| endpoints.get(inode).map(|(_, port)| *port))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Socket inode -> local endpoint, for every TCP socket in a namespace.
///
/// The local address is read as well as the port: a `hostNetwork` pod shares
/// the node's address, so "the socket on port 41234" is only one socket if the
/// address is part of the key.
fn socket_endpoints(pid: u32) -> HashMap<u64, (IpAddr, u16)> {
    let mut out = HashMap::new();
    for (path, v6) in [
        (format!("/proc/{pid}/net/tcp"), false),
        (format!("/proc/{pid}/net/tcp6"), true),
    ] {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 {
                continue;
            }
            let Some(inode) = fields[9].parse::<u64>().ok() else {
                continue;
            };
            let Some(local) = parse_endpoint(fields[1], v6) else {
                continue;
            };
            out.insert(inode, local);
        }
    }
    out
}

/// `0100007F:1F90` -> `127.0.0.1:8080`. The kernel prints each 32-bit word in
/// host order, so a little-endian host needs the bytes reversed.
fn parse_endpoint(field: &str, v6: bool) -> Option<(IpAddr, u16)> {
    let (addr, port) = field.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    if v6 {
        if addr.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 16];
        for word in 0..4 {
            let chunk = &addr[word * 8..word * 8 + 8];
            let value = u32::from_str_radix(chunk, 16).ok()?;
            bytes[word * 4..word * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        let ip = Ipv6Addr::from(bytes);
        // A dual-stack socket reports an IPv4 peer as ::ffff:a.b.c.d; the pod
        // index knows it by the IPv4 form.
        let ip = match ip.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(ip),
        };
        Some((ip, port))
    } else {
        let value = u32::from_str_radix(addr, 16).ok()?;
        Some((IpAddr::V4(Ipv4Addr::from(value.swap_bytes())), port))
    }
}

/// Cumulative receive/transmit bytes across the pod's interfaces, loopback
/// excluded: localhost traffic is not a network edge.
fn read_netdev(pid: u32) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/net/dev")) else {
        return (0, 0);
    };
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name == "lo" || name.is_empty() {
            continue;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 9 {
            continue;
        }
        rx += fields[0].parse::<u64>().unwrap_or(0);
        tx += fields[8].parse::<u64>().unwrap_or(0);
    }
    (rx, tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lidlesseye_ebpf::k8s::{ContainerInfo, PodIndex};
    use std::collections::BTreeMap;

    /// A pod index with `(uid, namespace, name, ip)` entries and no labels, so
    /// the node id falls back to the pod name.
    fn pod_index(entries: &[(&str, &str, &str, &str)]) -> Arc<PodIndex> {
        let mut pods = HashMap::new();
        for &(uid, namespace, name, ip) in entries {
            let meta = PodMeta::new(uid, namespace, name, &BTreeMap::new(), None, None)
                .with_ips(vec![ip.parse().unwrap()]);
            pods.insert(uid.to_string(), meta);
        }
        let index = PodIndex::new();
        index.replace(pods);
        Arc::new(index)
    }

    fn mapper(entries: &[(&str, &str, &str, &str)]) -> FlowMapper {
        FlowMapper {
            pods: pod_index(entries),
            services: None,
            node: "test-node".to_string(),
        }
    }

    /// `(container name, ports)` pairs of one stand-in pod.
    type PodContainers<'a> = &'a [(&'a str, &'a [u16])];

    /// A pod index whose pods carry named containers with their ports.
    fn mapper_with_containers(
        entries: &[(&str, &str, &str, &str, PodContainers<'_>)],
    ) -> FlowMapper {
        let mut pods = HashMap::new();
        for &(uid, namespace, name, ip, containers) in entries {
            let containers = containers
                .iter()
                .map(|(name, ports)| ContainerInfo::new(*name, "", ports.to_vec()))
                .collect();
            let meta = PodMeta::new(uid, namespace, name, &BTreeMap::new(), None, None)
                .with_ips(vec![ip.parse().unwrap()])
                .with_containers(containers);
            pods.insert(uid.to_string(), meta);
        }
        let index = PodIndex::new();
        index.replace(pods);
        FlowMapper {
            pods: Arc::new(index),
            services: None,
            node: "test-node".to_string(),
        }
    }

    /// A pod index whose pods declare the given container ports.
    fn mapper_with_ports(entries: &[(&str, &str, &str, &str, &[u16])]) -> FlowMapper {
        let mut pods = HashMap::new();
        for &(uid, namespace, name, ip, ports) in entries {
            let meta = PodMeta::new(uid, namespace, name, &BTreeMap::new(), None, None)
                .with_ips(vec![ip.parse().unwrap()])
                .with_ports(ports.to_vec());
            pods.insert(uid.to_string(), meta);
        }
        let index = PodIndex::new();
        index.replace(pods);
        FlowMapper {
            pods: Arc::new(index),
            services: None,
            node: "test-node".to_string(),
        }
    }

    fn flow(local: (&str, u16), remote: (&str, u16), role: Role, tx: u64, rx: u64) -> Flow {
        Flow {
            local: (local.0.parse().unwrap(), local.1),
            remote: (remote.0.parse().unwrap(), remote.1),
            role,
            opens: 1,
            tx_bytes: tx,
            rx_bytes: rx,
            retransmits: 0,
            rtt_us_sum: 0,
            rtt_samples: 0,
            cgroup: 0,
            protocols: 0,
            host: String::new(),
            sql: String::new(),
        }
    }

    #[test]
    fn a_client_flow_becomes_one_edge_with_both_directions() {
        let mapper = mapper(&[
            ("u1", "shop", "checkout", "10.0.0.2"),
            ("u2", "shop", "cart", "10.0.0.7"),
        ]);
        let batch = mapper.batch(
            vec![flow(
                ("10.0.0.2", 43210),
                ("10.0.0.7", 8080),
                Role::Client,
                900,
                2000,
            )],
            1,
        );
        assert_eq!(batch.edges.len(), 1);
        let edge = &batch.edges[0];
        assert_eq!(edge.client, "shop/checkout");
        assert_eq!(edge.server, "shop/cart");
        assert_eq!(edge.protocol, "http");
        assert_eq!(edge.connections, 1);
        assert_eq!(edge.tx_bytes, 900);
        assert_eq!(edge.rx_bytes, 2000);

        let checkout = batch
            .nodes
            .iter()
            .find(|n| n.id == "shop/checkout")
            .unwrap();
        assert_eq!(checkout.tx_bytes, 900);
        assert_eq!(checkout.rx_bytes, 2000);
        let cart = batch.nodes.iter().find(|n| n.id == "shop/cart").unwrap();
        // The server's counters come from the client's view of the same
        // connection, which is why the server-side flow is dropped.
        assert_eq!(cart.rx_bytes, 2000);
        assert_eq!(cart.tx_bytes, 900);
    }

    /// The server's container is inferred from the port the peer declares; the
    /// client's comes from `/proc`, which cannot see these stand-in pods, so it
    /// stays empty rather than being guessed.
    #[test]
    fn an_edge_names_the_container_that_owns_the_server_port() {
        let mapper = mapper_with_containers(&[
            ("u1", "shop", "checkout", "10.0.0.2", &[("app", &[8080])]),
            ("u2", "shop", "payments", "10.0.0.7", &[("app", &[5432])]),
        ]);
        let batch = mapper.batch(
            vec![flow(
                ("10.0.0.2", 43210),
                ("10.0.0.7", 5432),
                Role::Client,
                900,
                2000,
            )],
            1,
        );
        assert_eq!(batch.edges.len(), 1);
        assert_eq!(batch.edges[0].server_container, "app");
        assert_eq!(batch.edges[0].client_container, "");
        // The container list travels with the node, so the panel can draw a row
        // for a container that opened no connection in this window.
        let payments = batch
            .nodes
            .iter()
            .find(|node| node.id == "shop/payments")
            .expect("the peer is on the map");
        assert_eq!(payments.containers, ["app"]);
    }

    /// The same workload, two containers: the edges must not merge, or the
    /// sidecar's traffic would be indistinguishable from the app's. The split
    /// itself is covered by `containers_split_an_edge_instead_of_merging_it`
    /// in the core crate; what this pins down is that both flows survive with
    /// their own connection count.
    #[test]
    fn two_containers_of_one_pod_do_not_double_count() {
        let mapper = mapper_with_containers(&[
            ("u1", "shop", "checkout", "10.0.0.2", &[("app", &[8080])]),
            ("u2", "shop", "payments", "10.0.0.7", &[("app", &[5432])]),
        ]);
        let batch = mapper.batch(
            vec![
                flow(
                    ("10.0.0.2", 43210),
                    ("10.0.0.7", 5432),
                    Role::Client,
                    900,
                    2000,
                ),
                flow(
                    ("10.0.0.2", 43211),
                    ("10.0.0.7", 5432),
                    Role::Client,
                    100,
                    300,
                ),
            ],
            1,
        );
        let total: u64 = batch.edges.iter().map(|edge| edge.connections).sum();
        assert_eq!(total, 2);
        assert!(
            batch
                .edges
                .iter()
                .all(|edge| edge.server == "shop/payments")
        );
    }

    #[test]
    fn the_server_side_of_an_internal_connection_is_not_counted_again() {
        let mapper = mapper(&[
            ("u1", "shop", "checkout", "10.0.0.2"),
            ("u2", "shop", "cart", "10.0.0.7"),
        ]);
        let batch = mapper.batch(
            vec![flow(
                ("10.0.0.7", 8080),
                ("10.0.0.2", 43210),
                Role::Server,
                2000,
                900,
            )],
            1,
        );
        assert!(batch.edges.is_empty(), "the client's agent owns this edge");
        assert!(batch.nodes.is_empty(), "and no counter is recorded twice");
    }

    #[test]
    fn an_inbound_connection_from_outside_is_attributed_to_external() {
        let mapper = mapper(&[("u1", "shop", "checkout", "10.0.0.2")]);
        let batch = mapper.batch(
            vec![flow(
                ("10.0.0.2", 8080),
                ("203.0.113.9", 51515),
                Role::Server,
                700,
                120,
            )],
            1,
        );
        assert_eq!(batch.edges.len(), 1);
        let edge = &batch.edges[0];
        assert_eq!(edge.client, EXTERNAL_ID);
        assert_eq!(edge.server, "shop/checkout");
        // The observer is the server: what it received is the client's tx.
        assert_eq!(edge.tx_bytes, 120);
        assert_eq!(edge.rx_bytes, 700);
    }

    #[test]
    fn a_hairpin_rewritten_client_is_not_an_outside_client() {
        // kube-proxy masquerades a same-node pod-to-ClusterIP connection to the
        // node's bridge address, so the server's socket sees a private peer
        // that is not a pod. The client's agent already names this connection.
        let mapper = mapper(&[("u1", "shop", "checkout", "10.0.0.2")]);
        let batch = mapper.batch(
            vec![flow(
                ("10.0.0.2", 8080),
                ("10.244.0.1", 51515),
                Role::Server,
                700,
                120,
            )],
            1,
        );
        assert!(batch.edges.is_empty());
        assert!(batch.nodes.is_empty());
    }

    #[test]
    fn outbound_to_an_unknown_address_is_kept_as_a_client_edge() {
        let mapper = mapper(&[("u1", "shop", "checkout", "10.0.0.2")]);
        let batch = mapper.batch(
            vec![flow(
                ("10.0.0.2", 43210),
                ("203.0.113.9", 443),
                Role::Client,
                120,
                700,
            )],
            1,
        );
        assert_eq!(batch.edges.len(), 1);
        assert_eq!(batch.edges[0].client, "shop/checkout");
        assert_eq!(batch.edges[0].server, EXTERNAL_ID);
        assert_eq!(batch.edges[0].protocol, "https");
    }

    #[test]
    fn a_payload_hit_names_the_edge_instead_of_the_port() {
        // Port 9200 usually means the HTTP API. Here the sniffer recognised a
        // native ClickHouse frame on the same socket, so that is the label.
        let mut sample = flow(
            ("10.0.0.2", 43210),
            ("10.0.0.7", 9200),
            Role::Client,
            120,
            700,
        );
        sample.protocols = lidlesseye_core::servicemap::protocol::CLICKHOUSE;
        let mapper = mapper(&[
            ("u1", "shop", "checkout", "10.0.0.2"),
            ("u2", "shop", "stats", "10.0.0.7"),
        ]);
        let batch = mapper.batch(vec![sample], 1);
        assert_eq!(batch.edges[0].protocol, "clickhouse");
    }

    #[test]
    fn an_edge_can_carry_more_than_one_application_protocol() {
        let mut sample = flow(
            ("10.0.0.2", 43210),
            ("10.0.0.7", 5432),
            Role::Client,
            120,
            700,
        );
        sample.protocols = lidlesseye_core::servicemap::protocol::POSTGRES
            | lidlesseye_core::servicemap::protocol::REDIS;
        let mapper = mapper(&[
            ("u1", "shop", "checkout", "10.0.0.2"),
            ("u2", "shop", "payments", "10.0.0.7"),
        ]);
        let batch = mapper.batch(vec![sample], 1);
        assert_eq!(batch.edges[0].protocol, "postgres+redis");
    }

    #[test]
    fn traffic_of_a_process_outside_any_pod_is_dropped() {
        let mapper = mapper(&[("u1", "shop", "checkout", "10.0.0.2")]);
        // 127.0.0.1 and the node's own address belong to no pod.
        let batch = mapper.batch(
            vec![
                flow(("127.0.0.1", 43210), ("10.0.0.7", 80), Role::Client, 10, 10),
                flow(
                    ("172.18.0.2", 43210),
                    ("10.0.0.7", 80),
                    Role::Client,
                    10,
                    10,
                ),
            ],
            1,
        );
        assert!(batch.edges.is_empty());
        assert!(batch.nodes.is_empty());
    }

    #[test]
    fn a_connection_whose_direction_cannot_be_read_is_skipped() {
        // Its handshake predates the agent and neither pod declares a port, so
        // which end opened it is unknowable; guessing would draw half the
        // edges backwards.
        let mapper = mapper(&[
            ("u1", "shop", "checkout", "10.0.0.2"),
            ("u2", "shop", "cart", "10.0.0.7"),
        ]);
        let batch = mapper.batch(
            vec![flow(
                ("10.0.0.2", 43210),
                ("10.0.0.7", 8080),
                Role::Unknown,
                10,
                10,
            )],
            1,
        );
        assert!(batch.edges.is_empty());
    }

    #[test]
    fn a_connection_that_predates_the_agent_is_directed_by_the_pod_specs() {
        // Long-lived keep-alive connections never re-handshake: without this,
        // the busiest edges of a cluster would be invisible until the client
        // reconnects. The declared ports say who is the server.
        let mapper = mapper_with_ports(&[
            ("u1", "monitoring", "prometheus", "10.0.0.2", &[9090]),
            ("u2", "monitoring", "node-exporter", "10.0.0.7", &[9100]),
        ]);
        let batch = mapper.batch(
            vec![
                // The scrape, from prometheus's side.
                flow(
                    ("10.0.0.2", 43210),
                    ("10.0.0.7", 9100),
                    Role::Unknown,
                    900,
                    2000,
                ),
                // The same connection from node-exporter's side: it listens on
                // 9100, so it is the server and must not be counted again.
                flow(
                    ("10.0.0.7", 9100),
                    ("10.0.0.2", 43210),
                    Role::Unknown,
                    2000,
                    900,
                ),
            ],
            1,
        );
        assert_eq!(batch.edges.len(), 1);
        assert_eq!(batch.edges[0].client, "monitoring/prometheus");
        assert_eq!(batch.edges[0].server, "monitoring/node-exporter");
        assert_eq!(batch.edges[0].tx_bytes, 900);
        assert_eq!(batch.edges[0].rx_bytes, 2000);
    }

    #[test]
    fn parses_ipv4_endpoints_in_kernel_byte_order() {
        let (ip, port) = parse_endpoint("0100007F:1F90", false).unwrap();
        assert_eq!(ip, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(port, 8080);
        let (ip, port) = parse_endpoint("0A00000A:1538", false).unwrap();
        assert_eq!(ip, "10.0.0.10".parse::<IpAddr>().unwrap());
        assert_eq!(port, 0x1538);
    }

    #[test]
    fn parses_ipv6_endpoints_and_unmaps_v4_peers() {
        // ::ffff:10.0.0.10 in the kernel's per-word little-endian layout.
        let (ip, port) = parse_endpoint("0000000000000000FFFF00000A00000A:1538", true).unwrap();
        assert_eq!(ip, "10.0.0.10".parse::<IpAddr>().unwrap());
        assert_eq!(port, 0x1538);
    }

    #[test]
    fn a_listening_port_is_not_an_established_socket() {
        let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1
";
        assert!(text.contains("0A"));
        let fields: Vec<&str> = text.lines().nth(1).unwrap().split_whitespace().collect();
        assert_eq!(fields[3], "0A");
        assert_eq!(u8::from_str_radix(fields[3], 16).unwrap(), TCP_LISTEN);
    }

    #[test]
    fn a_workload_card_carries_its_replica_count() {
        // Two replicas of one Deployment, the second failing its readiness
        // probe: the map has one card for the workload and it must say 1/2.
        let mut pods = HashMap::new();
        for (uid, name, ip, ready) in [
            ("u1", "checkout-1", "10.0.0.2", true),
            ("u2", "checkout-2", "10.0.0.3", false),
        ] {
            let meta = PodMeta::new(
                uid,
                "shop",
                name,
                &BTreeMap::new(),
                Some(("Deployment", "checkout")),
                None,
            )
            .with_ips(vec![ip.parse().unwrap()])
            .with_health("Running", ready);
            pods.insert(uid.to_string(), meta);
        }
        let cart = PodMeta::new(
            "u3",
            "shop",
            "cart",
            &BTreeMap::new(),
            Some(("Deployment", "cart")),
            None,
        )
        .with_ips(vec!["10.0.0.7".parse().unwrap()])
        .with_health("Running", true);
        pods.insert("u3".to_string(), cart);
        let index = PodIndex::new();
        index.replace(pods);
        let mapper = FlowMapper {
            pods: Arc::new(index),
            services: None,
            node: "test-node".to_string(),
        };

        let batch = mapper.batch(
            vec![flow(
                ("10.0.0.2", 43210),
                ("10.0.0.7", 8080),
                Role::Client,
                900,
                2000,
            )],
            1,
        );

        let checkout = batch
            .nodes
            .iter()
            .find(|node| node.id == "shop/checkout")
            .expect("the client's workload is a node");
        assert_eq!((checkout.pods, checkout.ready), (2, 1));
        let cart = batch
            .nodes
            .iter()
            .find(|node| node.id == "shop/cart")
            .expect("the peer's workload is a node");
        assert_eq!((cart.pods, cart.ready), (1, 1));
    }
}
