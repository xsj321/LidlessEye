//! The service map collector.
//!
//! ## What it reads, and why not eBPF
//!
//! A pod's TCP table is available in `/proc/<pid>/net/tcp` *of any process in
//! its network namespace*, and the kernel resolves that path in that
//! namespace — no `setns`, no `CAP_SYS_ADMIN` beyond the `hostPID` the agent
//! already has. Reading it for one pid per pod yields the complete set of
//! sockets the pod owns, including the ones opened by a sidecar.
//!
//! The alternative — kprobes on `tcp_connect`/`tcp_close` — would give byte
//! counters and RTT, but it also means matching kernel struct layouts with
//! CO-RE and reading `struct sock` fields whose offsets move between releases.
//! The map has to name *both* endpoints of a connection, and `/proc/net/tcp`
//! already has them, so the collector takes the path with no kernel ABI in it.
//! Byte counters come from `/proc/<pid>/net/dev`, which is per namespace and
//! therefore per pod.
//!
//! ## What the map is
//!
//! Every established connection is attributed to a client and a server:
//!
//!   * the side whose local port is in its own listen set is the server,
//!   * the other side is the client.
//!
//! A connection between two pods is counted **once**, by the client's agent.
//! The server side deliberately skips it: both ends are visible on a node, and
//! summing both would double every edge. Connections from outside the cluster
//! are counted by the server side, as `external -> service`.
//!
//! ClusterIP addresses are resolved through the Service index, so
//! `10.96.0.10:443` becomes `default/kubernetes` instead of "external".
//!
//! ## What it deliberately does not do
//!
//! There is no L7 parsing. The protocol on an edge is inferred from the server
//! port (`https`, `postgres`, ...), not from the payload: telling gRPC from
//! HTTP needs either plaintext traffic or a TLS uprobe, and a wrong label is
//! worse than a coarse one. "Requests" would need the same inspection, so edges
//! report new connections and retransmits — the numbers the kernel actually
//! exposes.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lidlesseye_core::servicemap::{
    NodeKind, ServiceEdge, ServiceMapBatch, ServiceNode, protocol_for_port,
};
use lidlesseye_ebpf::k8s::{PodIndex, PodMeta};
use tracing::{debug, warn};

use crate::k8s::ServiceIndex;

/// Where a socket's peer ended up.
enum Peer {
    Pod(PodMeta),
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
    /// How often the network namespaces are scanned. The dashboard sums the
    /// resulting windows, so this only trades freshness for `/proc` reads.
    pub interval: Duration,
    /// Cluster-wide pod index, used to turn a peer address into a service.
    pub pods: Arc<PodIndex>,
    /// ClusterIP -> Service, when the API allowed listing Services.
    pub services: Option<Arc<ServiceIndex>>,
}

impl NetMapConfig {
    pub fn new(pods: Arc<PodIndex>) -> Self {
        NetMapConfig {
            interval: Duration::from_secs(10),
            pods,
            services: None,
        }
    }
}

/// Handle for a running collector.
pub struct NetMapHandle {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl NetMapHandle {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for NetMapHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Start the collector. The callback runs on the collector thread; it must not
/// block on anything that needs the caller's runtime.
pub fn start<F>(config: NetMapConfig, sink: F) -> NetMapHandle
where
    F: Fn(ServiceMapBatch) + Send + 'static,
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
        Ok(join) => NetMapHandle {
            stop,
            join: Some(join),
        },
        Err(err) => {
            warn!(error = %err, "could not start the service map collector");
            NetMapHandle { stop, join: None }
        }
    }
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

            let mut edges: HashMap<(String, String, String), ServiceEdge> = HashMap::new();
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
            out.push(PodSockets {
                meta,
                listen,
                sockets,
                rx_bytes,
                tx_bytes,
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
        let peer = resolve_peer(config, socket.key.remote.0, socket.key.remote.1);
        let this = pod_node(&pod.meta);

        let (client_node, server_node, server_port) = match peer {
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
                peers.entry(id.clone()).or_insert(node);
                (this.id, id, socket.key.remote.1)
            }
            Peer::Service(service) => {
                if local_is_server {
                    return None;
                }
                let node = service_node(&service);
                peers.entry(node.id.clone()).or_insert(node);
                (this.id, service.id(), socket.key.remote.1)
            }
            Peer::External => {
                unnamed.insert((socket.key.remote.0, socket.key.remote.1));
                let id = lidlesseye_core::servicemap::EXTERNAL_ID.to_string();
                peers.entry(id.clone()).or_insert_with(external_node);
                if local_is_server {
                    // Inbound from outside the cluster: the pod is the server.
                    (id, this.id, socket.key.local.1)
                } else {
                    // Outbound to something the map cannot name.
                    (this.id, id, socket.key.remote.1)
                }
            }
        };

        Some(ServiceEdge {
            client: client_node,
            server: server_node,
            protocol: protocol_for_port(server_port).to_string(),
            connections: u64::from(counted),
            retransmits: u64::from(retrans),
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

/// Id a pod contributes to the map. Must match
/// `lidlesseye_core::servicemap::node_id` as used by the CPU join: the
/// workload when the pod has one, its service name otherwise.
fn pod_node(meta: &PodMeta) -> ServiceNode {
    let name = meta
        .workload
        .clone()
        .or_else(|| meta.service_name().map(str::to_string))
        .unwrap_or_else(|| meta.name.clone());
    let mut node = ServiceNode::new(
        lidlesseye_core::servicemap::node_id(&meta.namespace, &name),
        NodeKind::Workload,
    );
    node.namespace = meta.namespace.clone();
    node.workload = meta.workload.clone().unwrap_or_default();
    node.service = meta.service_name().unwrap_or(&name).to_string();
    node
}

fn resolve_peer(config: &NetMapConfig, ip: IpAddr, port: u16) -> Peer {
    if ip.is_loopback() {
        return Peer::External;
    }
    if let Some(meta) = config.pods.lookup_ip(ip, port) {
        return Peer::Pod(meta);
    }
    if let Some(service) = config
        .services
        .as_ref()
        .and_then(|index| index.lookup(ip, port))
    {
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
}
