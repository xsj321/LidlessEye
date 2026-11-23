//! The service map data model and query composition.
//!
//! The map is a time-bucketed graph: agents observe TCP connections inside each
//! pod's network namespace, resolve both endpoints to a service (a workload, a
//! ClusterIP service or "external") and ship *deltas* for one collection
//! window. Storage sums those deltas over whatever window the dashboard asks
//! for, which is what makes the map follow the same time range as the
//! flamegraph.
//!
//! This crate owns the shapes and the pure query logic. It deliberately knows
//! nothing about DuckDB, tenants or async runtimes: [`ServiceMapStore`] is the
//! seam through which an implementation supplies rows, and
//! [`lidlesseye_core`](https://docs.rs/lidlesseye-core) implements it for the
//! real store. That keeps the protocol constants shareable with the eBPF
//! collector and makes the map queryable by tools such as the MCP server
//! without pulling in a database.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Node id for traffic that has no endpoint inside the cluster.
pub const EXTERNAL_ID: &str = "external";

/// Prefix of a node that stands for one destination host outside the cluster:
/// `external/api.example.com:8443`.
pub const EXTERNAL_HOST_PREFIX: &str = "external/";

/// How many nodes a single response may contain.
pub const MAX_NODES: usize = 600;

/// How many destination hosts the outside world is split into. Beyond this the
/// traffic stays on the aggregate `external` node rather than pushing the
/// cluster's own services out of the map.
pub const MAX_EXTERNAL_HOSTS: usize = 40;

/// The counter the service map's time brush plots.
///
/// All three live in the same `net_edges` rows, so switching between them is a
/// change of aggregate rather than of query: the brush keeps its buckets and
/// the bars keep their meaning ("how much of this, per bucket"). Requests are
/// TCP connections opened, because without payload parsing there is no request
/// count to divide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetMetric {
    /// TCP connections opened in the window.
    #[default]
    Connections,
    /// Bytes sent plus bytes received.
    Bytes,
    /// Retransmitted segments.
    Retransmits,
}

impl NetMetric {
    /// Parse a metric name from user input, accepting the UI's aliases.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "connections" | "requests" | "request" | "conn" => Some(Self::Connections),
            "bytes" | "traffic" => Some(Self::Bytes),
            "retransmits" | "retrans" => Some(Self::Retransmits),
            _ => None,
        }
    }

    /// Canonical name used in API responses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connections => "connections",
            Self::Bytes => "bytes",
            Self::Retransmits => "retransmits",
        }
    }
}

/// Storage seam used by the service map's query composition.
///
/// The error type is associated instead of fixed so callers keep their own
/// error semantics: the real store returns `lidlesseye_core::Error`, while a
/// test double can return any error it likes.
pub trait ServiceMapStore {
    /// Error returned by the storage implementation.
    type Error;

    /// Nodes seen in `[from_ns, until_ns)`, at most `limit` rows.
    fn net_nodes(
        &self,
        from_ns: u64,
        until_ns: u64,
        limit: usize,
    ) -> Result<Vec<MapNode>, Self::Error>;

    /// Aggregated edges seen in `[from_ns, until_ns)`.
    fn net_edges(&self, from_ns: u64, until_ns: u64) -> Result<Vec<MapEdge>, Self::Error>;

    /// A node's own counters, bucketed by `step_ns`.
    fn net_node_history(
        &self,
        node: &str,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
    ) -> Result<Vec<MapHistoryPoint>, Self::Error>;

    /// Counters of edges incident to a node, bucketed by `step_ns`.
    fn net_edge_history(
        &self,
        node: &str,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
    ) -> Result<Vec<MapHistoryPoint>, Self::Error>;

    /// Counters of edges whose server is an external host, bucketed by `step_ns`.
    fn net_host_edge_history(
        &self,
        host: &str,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
    ) -> Result<Vec<MapHistoryPoint>, Self::Error>;

    /// Dense per-bucket totals of one counter over all edges.
    fn net_histogram_metric(
        &self,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
        buckets: usize,
        metric: NetMetric,
    ) -> Result<Vec<u64>, Self::Error>;

    /// CPU nanoseconds attributed to each service node id.
    fn cpu_by_service(
        &self,
        from_ns: u64,
        until_ns: u64,
    ) -> Result<HashMap<String, u64>, Self::Error>;
}

impl<T: ServiceMapStore + ?Sized> ServiceMapStore for std::sync::Arc<T> {
    type Error = T::Error;

    fn net_nodes(
        &self,
        from_ns: u64,
        until_ns: u64,
        limit: usize,
    ) -> Result<Vec<MapNode>, Self::Error> {
        T::net_nodes(self.as_ref(), from_ns, until_ns, limit)
    }

    fn net_edges(&self, from_ns: u64, until_ns: u64) -> Result<Vec<MapEdge>, Self::Error> {
        T::net_edges(self.as_ref(), from_ns, until_ns)
    }

    fn net_node_history(
        &self,
        node: &str,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
    ) -> Result<Vec<MapHistoryPoint>, Self::Error> {
        T::net_node_history(self.as_ref(), node, from_ns, until_ns, step_ns)
    }

    fn net_edge_history(
        &self,
        node: &str,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
    ) -> Result<Vec<MapHistoryPoint>, Self::Error> {
        T::net_edge_history(self.as_ref(), node, from_ns, until_ns, step_ns)
    }

    fn net_host_edge_history(
        &self,
        host: &str,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
    ) -> Result<Vec<MapHistoryPoint>, Self::Error> {
        T::net_host_edge_history(self.as_ref(), host, from_ns, until_ns, step_ns)
    }

    fn net_histogram_metric(
        &self,
        from_ns: u64,
        until_ns: u64,
        step_ns: u64,
        buckets: usize,
        metric: NetMetric,
    ) -> Result<Vec<u64>, Self::Error> {
        T::net_histogram_metric(self.as_ref(), from_ns, until_ns, step_ns, buckets, metric)
    }

    fn cpu_by_service(
        &self,
        from_ns: u64,
        until_ns: u64,
    ) -> Result<HashMap<String, u64>, Self::Error> {
        T::cpu_by_service(self.as_ref(), from_ns, until_ns)
    }
}

/// Node id for a namespace/workload pair, or just the name outside Kubernetes.
///
/// Keeping this in one place matters: the agent builds ids when it observes a
/// connection and the profile join builds them from labels, and the map only
/// lines up if both spell the same service the same way.
pub fn node_id(namespace: &str, name: &str) -> String {
    let namespace = namespace.trim();
    let name = name.trim();
    if namespace.is_empty() {
        name.to_string()
    } else {
        format!("{namespace}/{name}")
    }
}

/// What a node represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A workload (Deployment, StatefulSet, DaemonSet, ...) running pods.
    Workload,
    /// A ClusterIP Service, i.e. a virtual address with backing pods.
    #[default]
    Service,
    /// Outside the cluster, or inside it but not attributable to a pod.
    External,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Workload => "workload",
            NodeKind::Service => "service",
            NodeKind::External => "external",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "workload" => NodeKind::Workload,
            "external" => NodeKind::External,
            _ => NodeKind::Service,
        }
    }
}

/// One node of the map, as reported by an agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceNode {
    pub id: String,
    #[serde(default)]
    pub namespace: String,
    /// Owning workload (`checkout`), when known.
    #[serde(default)]
    pub workload: String,
    /// `app.kubernetes.io/name` or the workload name; what the UI shows.
    #[serde(default)]
    pub service: String,
    #[serde(default)]
    pub kind: NodeKind,
    /// Node the observation came from. Empty for peers resolved by IP.
    #[serde(default)]
    pub node: String,
    /// Bytes received / sent by this pod during the window.
    #[serde(default)]
    pub rx_bytes: u64,
    #[serde(default)]
    pub tx_bytes: u64,
    /// New TCP connections counted during the window.
    #[serde(default)]
    pub connections: u64,
    /// Retransmitted segments during the window.
    #[serde(default)]
    pub retransmits: u64,
    /// Replicas of this workload the cluster has, and how many of them are
    /// ready. Gauges, not counters: a window reports the newest sample and a
    /// merge keeps the larger one, because every agent reads the same
    /// cluster-wide pod list and would otherwise multiply the count.
    #[serde(default)]
    pub pods: u32,
    #[serde(default)]
    pub ready: u32,
    /// The replicas themselves, capped by the agent: the panel that lists
    /// instances needs names and placements, and the counts alone cannot
    /// answer "which replica is the one that is not ready".
    #[serde(default)]
    pub instances: Vec<InstanceInfo>,
    /// Container names of the workload's pods, deduplicated.
    ///
    /// A pod is not one process: a sidecar (an Istio proxy, a log shipper)
    /// opens its own connections, and a topic that folds them into the pod
    /// hides which of the two is talking. The names are a gauge like the
    /// replica counts — every agent reads the same pod list — and they are
    /// what the panel uses to give each container its own row.
    #[serde(default)]
    pub containers: Vec<String>,
}

/// One replica of a workload, as the cluster reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceInfo {
    pub name: String,
    /// Stable Kubernetes UID, used to line an edge up with this replica.
    #[serde(default)]
    pub uid: String,
    /// Kubernetes node the pod is scheduled on.
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub ready: bool,
    /// `Running`, `Pending`, ... Empty when the cluster did not report one.
    #[serde(default)]
    pub phase: String,
}

impl ServiceNode {
    pub fn new(id: impl Into<String>, kind: NodeKind) -> Self {
        ServiceNode {
            id: id.into(),
            kind,
            ..Default::default()
        }
    }

    /// Fill the display fields from a namespace/name pair.
    pub fn named(mut self, namespace: &str, name: &str) -> Self {
        self.namespace = namespace.to_string();
        self.service = name.to_string();
        self.id = node_id(namespace, name);
        self
    }
}

/// One directed edge: `client` opened connections to `server`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceEdge {
    pub client: String,
    pub server: String,
    /// `http`, `grpc`, `postgres`, `redis`, `dns`, `tcp`, ...
    #[serde(default)]
    pub protocol: String,
    /// Container inside the client pod that opened the sockets, and the one
    /// inside the server pod that accepted them. Both are empty when the agent
    /// could not attribute the traffic — a sidecar-less pod with no declared
    /// ports, or a peer outside the cluster.
    #[serde(default)]
    pub client_container: String,
    #[serde(default)]
    pub server_container: String,
    /// UIDs of the concrete pods that opened/accepted the connections. Empty
    /// when the peer is a service or an external address, or when the data
    /// predates pod-level attribution.
    #[serde(default)]
    pub client_pod: String,
    #[serde(default)]
    pub server_pod: String,
    #[serde(default)]
    pub connections: u64,
    #[serde(default)]
    pub retransmits: u64,
    /// Bytes the client sent to the server during the window.
    ///
    /// Only the eBPF source can report these; the `/proc` fallback leaves them
    /// at zero rather than inventing a number.
    #[serde(default)]
    pub tx_bytes: u64,
    /// Bytes the server sent back.
    #[serde(default)]
    pub rx_bytes: u64,
    /// Sum of the round-trip times sampled when connections closed, in
    /// microseconds, together with how many samples it is the sum of. Stored
    /// this way so that summing windows keeps the average exact.
    #[serde(default)]
    pub rtt_us_sum: u64,
    #[serde(default)]
    pub rtt_samples: u64,
    /// `Host:` header the client asked for, when the connection carried HTTP.
    /// This is often the only name for an external peer behind a load
    /// balancer, where the address is just an ingress IP.
    #[serde(default)]
    pub host: String,
    /// Last SQL statement observed on the edge (MySQL COM_QUERY, PostgreSQL
    /// Query/Parse). Empty for every non-database protocol, and for encrypted
    /// connections unless the optional SSL uprobe is enabled.
    #[serde(default)]
    pub sql: String,
}

/// One collection window, produced by an agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceMapBatch {
    pub start_ns: u64,
    pub end_ns: u64,
    /// Node the agent runs on, for debugging a sparse map.
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub nodes: Vec<ServiceNode>,
    #[serde(default)]
    pub edges: Vec<ServiceEdge>,
}

impl ServiceMapBatch {
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty() && self.nodes.is_empty()
    }

    /// Merge another batch of the same window into this one.
    pub fn merge(&mut self, other: ServiceMapBatch) {
        let mut index: HashMap<String, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| (node.id.clone(), i))
            .collect();
        for node in other.nodes {
            match index.get(&node.id) {
                Some(&i) => {
                    let slot = &mut self.nodes[i];
                    slot.rx_bytes += node.rx_bytes;
                    slot.tx_bytes += node.tx_bytes;
                    slot.connections += node.connections;
                    slot.retransmits += node.retransmits;
                    // Replicas are a gauge sampled from the same cluster-wide
                    // pod list by every agent: summing them would count each
                    // replica once per node that noticed the workload.
                    slot.pods = slot.pods.max(node.pods);
                    slot.ready = slot.ready.max(node.ready);
                    // The instance list is the same gauge: every agent reads
                    // the same cluster-wide pod list, so the longer of the two
                    // is the more complete observation, not a union.
                    if node.instances.len() > slot.instances.len() {
                        slot.instances = node.instances.clone();
                    }
                    if slot.namespace.is_empty() {
                        slot.namespace = node.namespace;
                    }
                    if slot.service.is_empty() {
                        slot.service = node.service;
                    }
                    if slot.workload.is_empty() {
                        slot.workload = node.workload;
                    }
                    // Container names are a gauge too, and every agent reads the
                    // same pod list: the longer list is the more complete
                    // observation, not something to append to.
                    if node.containers.len() > slot.containers.len() {
                        slot.containers = node.containers.clone();
                    }
                }
                None => {
                    index.insert(node.id.clone(), self.nodes.len());
                    self.nodes.push(node);
                }
            }
        }
        let mut edge_index: HashMap<(String, String, String, String, String), usize> = self
            .edges
            .iter()
            .enumerate()
            .map(|(i, e)| {
                (
                    (
                        e.client.clone(),
                        e.server.clone(),
                        e.protocol.clone(),
                        e.client_container.clone(),
                        e.server_container.clone(),
                    ),
                    i,
                )
            })
            .collect();
        for edge in other.edges {
            let key = (
                edge.client.clone(),
                edge.server.clone(),
                edge.protocol.clone(),
                edge.client_container.clone(),
                edge.server_container.clone(),
            );
            match edge_index.get(&key) {
                Some(&i) => {
                    self.edges[i].connections += edge.connections;
                    self.edges[i].retransmits += edge.retransmits;
                    self.edges[i].tx_bytes += edge.tx_bytes;
                    self.edges[i].rx_bytes += edge.rx_bytes;
                    self.edges[i].rtt_us_sum += edge.rtt_us_sum;
                    self.edges[i].rtt_samples += edge.rtt_samples;
                    // The Host and SQL are "latest wins" fields, not counters:
                    // keep a non-empty value instead of blanking what another
                    // agent or window observed.
                    if !edge.host.is_empty() {
                        self.edges[i].host = edge.host;
                    }
                    if !edge.sql.is_empty() {
                        self.edges[i].sql = edge.sql;
                    }
                }
                None => {
                    edge_index.insert(key, self.edges.len());
                    self.edges.push(edge);
                }
            }
        }
    }
}

/// An aggregated node in a query response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MapNode {
    pub id: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub workload: String,
    #[serde(default)]
    pub service: String,
    #[serde(default)]
    pub kind: NodeKind,
    #[serde(default)]
    pub rx_bytes: u64,
    #[serde(default)]
    pub tx_bytes: u64,
    #[serde(default)]
    pub connections: u64,
    #[serde(default)]
    pub retransmits: u64,
    /// On-CPU nanoseconds from the profile store, when the service is profiled.
    #[serde(default)]
    pub cpu_ns: u64,
    /// Replicas behind this node at the end of the window, and how many of
    /// them reported `Ready`. Zero for a ClusterIP service (its endpoints are
    /// pods, and those are their own nodes) and for the outside world.
    #[serde(default)]
    pub pods: u32,
    #[serde(default)]
    pub ready: u32,
    /// The replicas themselves, newest observation.
    #[serde(default)]
    pub instances: Vec<InstanceInfo>,
    /// Container names behind this node, for the per-container rows in the
    /// panel. Empty for the outside world and for a ClusterIP service.
    #[serde(default)]
    pub containers: Vec<String>,
}

/// An aggregated edge in a query response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MapEdge {
    pub client: String,
    pub server: String,
    pub protocol: String,
    /// Container of the client pod that opened the connections, when the agent
    /// could tell one from the socket's owner. Empty on data collected before
    /// containers were tracked, and for a peer that is not a pod.
    #[serde(default)]
    pub client_container: String,
    /// Container of the server pod that accepted them, inferred from the port
    /// the peer declares. Empty when no container claims that port.
    #[serde(default)]
    pub server_container: String,
    /// Pod UID behind each end, when the agent could attribute the traffic.
    #[serde(default)]
    pub client_pod: String,
    #[serde(default)]
    pub server_pod: String,
    pub connections: u64,
    pub retransmits: u64,
    /// Bytes the client sent to the server over the window.
    #[serde(default)]
    pub tx_bytes: u64,
    /// Bytes the server sent back.
    #[serde(default)]
    pub rx_bytes: u64,
    /// Average round-trip time in microseconds, zero when no connection on this
    /// edge closed inside the window (a long-lived connection reports nothing
    /// rather than a stale number).
    #[serde(default)]
    pub rtt_us: u64,
    /// `Host:` header most recently seen on this edge, if it carried HTTP.
    #[serde(default)]
    pub host: String,
    /// Last SQL statement seen on this edge, if it carried MySQL or
    /// PostgreSQL. Encrypted database traffic needs the SSL uprobe.
    #[serde(default)]
    pub sql: String,
}

/// `GET /api/v1/servicemap` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceMap {
    pub from_ns: u64,
    pub until_ns: u64,
    pub nodes: Vec<MapNode>,
    pub edges: Vec<MapEdge>,
    /// True when the node limit clipped the graph.
    #[serde(default)]
    pub truncated: bool,
}

impl ServiceMap {
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }
}

/// Query the map for a window, joining per-service CPU from the profiles.
pub fn servicemap<S: ServiceMapStore>(
    store: &S,
    from_ns: u64,
    until_ns: u64,
) -> Result<ServiceMap, S::Error> {
    let mut nodes = store.net_nodes(from_ns, until_ns, MAX_NODES + 1)?;
    let mut truncated = nodes.len() > MAX_NODES;
    let mut edges = store.net_edges(from_ns, until_ns)?;

    // External traffic is split by the destination the client asked for.
    //
    // One grey "external" blob answers nothing: a page that calls three APIs
    // and a database looks like one dependency, and "which of them is slow" has
    // no node to click. The agent already carries the HTTP `Host:` (and, under
    // the TLS uprobe, the host of an HTTPS request), so the split costs nothing
    // at query time. Traffic with no observed host — encrypted without the
    // uprobe, or not HTTP at all — stays on the aggregate node, which keeps its
    // honest meaning of "outside, destination unknown".
    let mut totals: HashMap<String, (u64, u64, u64, u64)> = HashMap::new();
    for edge in &edges {
        if edge.server != EXTERNAL_ID {
            continue;
        }
        let host = host_node_name(&edge.host);
        if host.is_empty() {
            continue;
        }
        let entry = totals.entry(host).or_insert((0, 0, 0, 0));
        entry.0 += edge.connections;
        entry.1 += edge.retransmits;
        entry.2 += edge.tx_bytes;
        entry.3 += edge.rx_bytes;
    }

    // A crawler can name thousands of hosts. Cap the split at the busiest few,
    // so the outside world cannot push the cluster's own services off a map
    // that only holds `MAX_NODES` — the rest stays on the aggregate node,
    // unsummed and unlabelled, which is exactly what it is.
    let split_hosts: HashMap<String, (u64, u64, u64, u64)> = {
        let mut ranked: Vec<(String, (u64, u64, u64, u64))> = totals.into_iter().collect();
        ranked.sort_by(|a, b| {
            let weight = |t: &(u64, u64, u64, u64)| t.0 + (t.2 + t.3) / 1024;
            weight(&b.1).cmp(&weight(&a.1)).then_with(|| a.0.cmp(&b.0))
        });
        ranked.truncate(MAX_EXTERNAL_HOSTS);
        ranked.into_iter().collect()
    };

    if !split_hosts.is_empty() {
        let (mut split_connections, mut split_retransmits) = (0u64, 0u64);
        let (mut split_tx, mut split_rx) = (0u64, 0u64);
        let mut host_nodes: Vec<MapNode> = Vec::with_capacity(split_hosts.len());
        for (host, (connections, retransmits, tx, rx)) in &split_hosts {
            let id = format!("{EXTERNAL_HOST_PREFIX}{host}");
            split_connections += connections;
            split_retransmits += retransmits;
            split_tx += tx;
            split_rx += rx;
            host_nodes.push(MapNode {
                id,
                service: host.clone(),
                kind: NodeKind::External,
                connections: *connections,
                retransmits: *retransmits,
                tx_bytes: *tx,
                rx_bytes: *rx,
                ..Default::default()
            });
        }
        for edge in &mut edges {
            if edge.server != EXTERNAL_ID {
                continue;
            }
            let host = host_node_name(&edge.host);
            if split_hosts.contains_key(&host) {
                edge.server = format!("{EXTERNAL_HOST_PREFIX}{host}");
            }
        }

        // The aggregate node was summed over *every* external edge, including
        // the ones just split out. Subtracting them leaves it with the traffic
        // that had no host, and drops it entirely when there is none.
        if let Some(external) = nodes.iter_mut().find(|node| node.id == EXTERNAL_ID) {
            external.connections = external.connections.saturating_sub(split_connections);
            external.retransmits = external.retransmits.saturating_sub(split_retransmits);
            external.tx_bytes = external.tx_bytes.saturating_sub(split_tx);
            external.rx_bytes = external.rx_bytes.saturating_sub(split_rx);
        }
        nodes.extend(host_nodes);
        nodes.retain(|node| {
            node.id != EXTERNAL_ID
                || node.connections + node.retransmits + node.tx_bytes + node.rx_bytes > 0
        });
        // Busiest first, the order the limit below is applied in.
        nodes.sort_by(|a, b| {
            let weight = |node: &MapNode| node.connections + (node.tx_bytes + node.rx_bytes) / 1024;
            weight(b).cmp(&weight(a)).then_with(|| a.id.cmp(&b.id))
        });
        if nodes.len() > MAX_NODES {
            nodes.truncate(MAX_NODES);
            truncated = true;
        }
    } else {
        // Nothing was split, so the node list keeps the order and the limit the
        // store already applied.
        nodes.truncate(MAX_NODES);
    }

    let keep: std::collections::HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    // An edge to a node the limit clipped would dangle in the drawing.
    edges.retain(|e| keep.contains(e.client.as_str()) && keep.contains(e.server.as_str()));

    let cpu = store.cpu_by_service(from_ns, until_ns)?;
    for node in &mut nodes {
        node.cpu_ns = cpu.get(&node.id).copied().unwrap_or(0);
    }

    Ok(ServiceMap {
        from_ns,
        until_ns,
        nodes,
        edges,
        truncated,
    })
}

/// Longest host that may become a node name.
const MAX_HOST_LEN: usize = 96;

/// A `Host:` header as a node name.
///
/// The header is client-controlled: it can carry a port, a trailing dot, mixed
/// case, or four kilobytes of garbage. Keep the characters a hostname can
/// actually contain, fold case (DNS is case-insensitive), bound the length, and
/// return an empty string when nothing usable is left — the caller then keeps
/// the traffic on the aggregate external node rather than inventing a node.
fn host_node_name(host: &str) -> String {
    let trimmed = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let mut out = String::with_capacity(trimmed.len().min(MAX_HOST_LEN));
    for ch in trimmed.chars() {
        if out.len() >= MAX_HOST_LEN {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ':') {
            out.push(ch);
        }
    }
    out.trim_matches('.').to_string()
}

/// One bucket of a single node's history.
///
/// The same shape covers two different observations, which is why the fields
/// are all plain counters: `pods`/`ready` are a gauge and are only filled in on
/// the node's own series, `rtt_us` only exists where an edge does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapHistoryPoint {
    pub bucket_ns: u64,
    #[serde(default)]
    pub rx_bytes: u64,
    #[serde(default)]
    pub tx_bytes: u64,
    #[serde(default)]
    pub connections: u64,
    #[serde(default)]
    pub retransmits: u64,
    /// Sample-weighted mean round-trip time over the bucket.
    #[serde(default)]
    pub rtt_us: u64,
    #[serde(default)]
    pub pods: u32,
    #[serde(default)]
    pub ready: u32,
}

/// `GET /api/v1/servicemap/history` response.
///
/// Buckets with no traffic are absent rather than zero-filled: a gap in the
/// agent's reporting is not the same thing as a quiet minute, and the drawing
/// code knows how to leave the former empty.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MapHistory {
    pub node: String,
    pub from_ns: u64,
    pub until_ns: u64,
    pub step_ns: u64,
    /// What the node itself sent and received, plus its replica gauge.
    #[serde(default)]
    pub points: Vec<MapHistoryPoint>,
    /// The same buckets seen through the node's edges, which is the only place
    /// a round-trip time exists.
    #[serde(default)]
    pub edges: Vec<MapHistoryPoint>,
}

/// Per-bucket history of one node over `[from_ns, until_ns)`.
pub fn servicemap_history<S: ServiceMapStore>(
    store: &S,
    node: &str,
    from_ns: u64,
    until_ns: u64,
    step_ns: u64,
) -> Result<MapHistory, S::Error> {
    let step_ns = step_ns.max(1);
    // A host node exists only in the drawing: it is synthesized from the edges
    // that name it, so it has no rows of its own in `net_nodes`. Its counters
    // are those edges, and the same buckets serve as both series.
    if let Some(host) = node.strip_prefix(EXTERNAL_HOST_PREFIX) {
        let points = store.net_host_edge_history(host, from_ns, until_ns, step_ns)?;
        return Ok(MapHistory {
            node: node.to_string(),
            from_ns,
            until_ns,
            step_ns,
            edges: points.clone(),
            points,
        });
    }
    Ok(MapHistory {
        node: node.to_string(),
        from_ns,
        until_ns,
        step_ns,
        points: store.net_node_history(node, from_ns, until_ns, step_ns)?,
        edges: store.net_edge_history(node, from_ns, until_ns, step_ns)?,
    })
}

/// `GET /api/v1/servicemap/histogram` response.
///
/// The time brush above the map shows one counter per bucket — connections
/// opened, bytes moved, or retransmits — rather than CPU samples. `points` is
/// dense: index `i` covers `[from_ns + i*step_ns, … + step_ns)`. `metric` names
/// the counter so the brush can label its bars without trusting its own
/// request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MapHistogram {
    pub from_ns: u64,
    pub until_ns: u64,
    pub step_ns: u64,
    #[serde(default)]
    pub metric: String,
    #[serde(default)]
    pub points: Vec<u64>,
}

/// Per-bucket totals of one counter over the whole map.
pub fn servicemap_histogram<S: ServiceMapStore>(
    store: &S,
    from_ns: u64,
    until_ns: u64,
    buckets: usize,
    metric: NetMetric,
) -> Result<MapHistogram, S::Error> {
    let buckets = buckets.max(1);
    let step_ns = ((until_ns.saturating_sub(from_ns)) / buckets as u64).max(1);
    Ok(MapHistogram {
        from_ns,
        until_ns,
        step_ns,
        metric: metric.as_str().to_string(),
        points: store.net_histogram_metric(from_ns, until_ns, step_ns, buckets, metric)?,
    })
}

/// Application protocols the eBPF payload sniffer can recognise.
///
/// The values are a bit set: one TCP connection can carry more than one
/// protocol over its lifetime, and a batch sums flows rather than replacing
/// them. The same numbers are compiled into `bpf/net.bpf.c`; changing one
/// without the other would mislabel every edge on the map.
pub mod protocol {
    pub const HTTP: u32 = 1 << 0;
    pub const HTTPS: u32 = 1 << 1;
    pub const POSTGRES: u32 = 1 << 2;
    pub const MYSQL: u32 = 1 << 3;
    pub const MONGODB: u32 = 1 << 4;
    pub const REDIS: u32 = 1 << 5;
    pub const MEMCACHED: u32 = 1 << 6;
    pub const CASSANDRA: u32 = 1 << 7;
    pub const KAFKA: u32 = 1 << 8;
    pub const CLICKHOUSE: u32 = 1 << 9;
    pub const ZOOKEEPER: u32 = 1 << 10;
    pub const FOUNDATIONDB: u32 = 1 << 11;
    pub const HTTP2: u32 = 1 << 12;
    pub const DNS: u32 = 1 << 13;
    pub const GRPC: u32 = 1 << 14;
}

use protocol as p;

/// Names of the protocols set in `mask`, in a stable reading order.
pub fn protocol_names(mask: u32) -> Vec<&'static str> {
    const ORDER: &[(u32, &str)] = &[
        (p::HTTP, "http"),
        (p::HTTPS, "https"),
        (p::HTTP2, "http2"),
        (p::GRPC, "grpc"),
        (p::POSTGRES, "postgres"),
        (p::MYSQL, "mysql"),
        (p::MONGODB, "mongodb"),
        (p::REDIS, "redis"),
        (p::MEMCACHED, "memcached"),
        (p::CASSANDRA, "cassandra"),
        (p::KAFKA, "kafka"),
        (p::CLICKHOUSE, "clickhouse"),
        (p::ZOOKEEPER, "zookeeper"),
        (p::FOUNDATIONDB, "foundationdb"),
        (p::DNS, "dns"),
    ];
    ORDER
        .iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

/// Protocol label for an edge: what the payload said, or the port's guess.
pub fn protocol_label(mask: u32, fallback: &str) -> String {
    let names = protocol_names(mask);
    if names.is_empty() {
        fallback.to_string()
    } else {
        names.join("+")
    }
}

/// Best-effort protocol name from a port number.
///
/// Used only when no payload could be classified — a long-lived TLS socket
/// whose plaintext never passes a probed syscall, for example. A payload hit
/// always wins over the port, because a port says what a service usually runs,
/// not what this connection actually carried.
pub fn protocol_for_port(port: u16) -> &'static str {
    match port {
        80 | 3000 | 8000 | 8080 | 9200 => "http",
        443 | 8443 | 9443 => "https",
        50051 => "grpc",
        53 => "dns",
        5432 => "postgres",
        3306 => "mysql",
        6379 => "redis",
        27017 => "mongodb",
        9042 | 9160 => "cassandra",
        9092..=9094 => "kafka",
        8123 => "http",
        9000 | 9009 => "clickhouse",
        5672 => "amqp",
        11211 => "memcached",
        2181 => "zookeeper",
        4500 | 4501 => "foundationdb",
        9090 | 9091 => "prometheus",
        4040 => "lidlesseye",
        _ => "tcp",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_ids_are_namespaced_only_inside_a_cluster() {
        assert_eq!(node_id("shop", "checkout"), "shop/checkout");
        assert_eq!(node_id("", "checkout"), "checkout");
        assert_eq!(node_id("  ", " checkout "), "checkout");
    }

    #[test]
    fn merging_batches_adds_counters_instead_of_duplicating_rows() {
        let mut a = ServiceMapBatch {
            nodes: vec![ServiceNode {
                pods: 3,
                ready: 2,
                ..ServiceNode::new("shop/a", NodeKind::Workload)
            }],
            edges: vec![ServiceEdge {
                client: "shop/a".into(),
                server: "shop/b".into(),
                protocol: "http".into(),
                client_container: "app".into(),
                server_container: "app".into(),
                connections: 2,
                retransmits: 0,
                tx_bytes: 100,
                rx_bytes: 400,
                rtt_us_sum: 900,
                rtt_samples: 1,
                ..Default::default()
            }],
            ..Default::default()
        };
        let b = ServiceMapBatch {
            nodes: vec![ServiceNode {
                // The same rollout a moment later, every replica now ready.
                pods: 3,
                ready: 3,
                ..ServiceNode::new("shop/a", NodeKind::Workload)
            }],
            edges: vec![ServiceEdge {
                client: "shop/a".into(),
                server: "shop/b".into(),
                protocol: "http".into(),
                client_container: "app".into(),
                server_container: "app".into(),
                connections: 3,
                retransmits: 1,
                tx_bytes: 250,
                rx_bytes: 600,
                rtt_us_sum: 2100,
                rtt_samples: 2,
                ..Default::default()
            }],
            ..Default::default()
        };
        a.merge(b);
        assert_eq!(a.nodes.len(), 1);
        assert_eq!(a.edges.len(), 1);
        assert_eq!(a.edges[0].connections, 5);
        assert_eq!(a.edges[0].retransmits, 1);
        // Bytes and RTT are counters too: a merged window must carry the sum,
        // not the last observation.
        assert_eq!(a.edges[0].tx_bytes, 350);
        assert_eq!(a.edges[0].rx_bytes, 1000);
        assert_eq!(a.edges[0].rtt_us_sum, 3000);
        assert_eq!(a.edges[0].rtt_samples, 3);
        // Replicas are a gauge: every agent reads the same cluster-wide pod
        // list, so summing them would multiply the count by the node count.
        // The larger sample wins, because the newer pod list is the one that
        // noticed the last replica becoming ready.
        assert_eq!((a.nodes[0].pods, a.nodes[0].ready), (3, 3));
    }

    /// A pod's sidecar opens its own connections. They must not be folded into
    /// the workload's row: the whole point of tracking containers is that the
    /// panel can say "this traffic came from istio-proxy, not from app".
    #[test]
    fn containers_split_an_edge_instead_of_merging_it() {
        let edge = |container: &str, connections: u64| ServiceEdge {
            client: "shop/a".into(),
            server: "shop/b".into(),
            protocol: "http".into(),
            client_container: container.into(),
            server_container: "app".into(),
            connections,
            ..Default::default()
        };
        let mut batch = ServiceMapBatch {
            edges: vec![edge("app", 3)],
            ..Default::default()
        };
        batch.merge(ServiceMapBatch {
            edges: vec![edge("app", 2), edge("istio-proxy", 7)],
            ..Default::default()
        });
        batch
            .edges
            .sort_by(|a, b| a.client_container.cmp(&b.client_container));
        assert_eq!(batch.edges.len(), 2);
        assert_eq!(batch.edges[0].client_container, "app");
        assert_eq!(batch.edges[0].connections, 5);
        assert_eq!(batch.edges[1].client_container, "istio-proxy");
        assert_eq!(batch.edges[1].connections, 7);
    }

    /// Container names are a gauge like the replica counts, and every agent
    /// reads the same pod list: the merge takes the fuller list rather than
    /// appending, which would multiply each name by the number of agents.
    #[test]
    fn container_lists_merge_as_a_gauge() {
        let mut a = ServiceMapBatch {
            nodes: vec![ServiceNode {
                containers: vec!["app".into()],
                ..ServiceNode::new("shop/a", NodeKind::Workload)
            }],
            ..Default::default()
        };
        a.merge(ServiceMapBatch {
            nodes: vec![ServiceNode {
                containers: vec!["app".into(), "istio-proxy".into()],
                ..ServiceNode::new("shop/a", NodeKind::Workload)
            }],
            ..Default::default()
        });
        assert_eq!(a.nodes[0].containers, ["app", "istio-proxy"]);
    }

    #[test]
    fn ports_name_the_protocol_they_usually_carry() {
        assert_eq!(super::protocol_for_port(443), "https");
        assert_eq!(super::protocol_for_port(5432), "postgres");
        assert_eq!(super::protocol_for_port(9042), "cassandra");
        assert_eq!(super::protocol_for_port(4500), "foundationdb");
        assert_eq!(super::protocol_for_port(12345), "tcp");
    }

    #[test]
    fn payload_hits_beat_the_port_and_keep_their_order() {
        use super::protocol as p;
        assert_eq!(
            super::protocol_label(p::HTTP | p::MYSQL, "postgres"),
            "http+mysql"
        );
        assert_eq!(super::protocol_label(0, "https"), "https");
    }
}
