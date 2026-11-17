//! The service map data model.
//!
//! The map is a time-bucketed graph: agents observe TCP connections inside each
//! pod's network namespace, resolve both endpoints to a service (a workload, a
//! ClusterIP service or "external") and ship *deltas* for one collection
//! window. Storage sums those deltas over whatever window the dashboard asks
//! for, which is what makes the map follow the same time range as the
//! flamegraph.
//!
//! Edges carry what the agent can observe: new connections, retransmits and an
//! application protocol. The eBPF collector sniffs the first payload written
//! to a socket and sets a protocol bit; a port-based guess is the fallback for
//! a connection that carried no parseable payload (TLS without an uprobe, for
//! instance). Node CPU is joined from the profile store, so a service that
//! burns CPU but serves little traffic is visible as such.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::store::Store;

/// Node id for traffic that has no endpoint inside the cluster.
pub const EXTERNAL_ID: &str = "external";

/// How many nodes a single response may contain.
pub const MAX_NODES: usize = 600;

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
pub fn servicemap(store: &Store, from_ns: u64, until_ns: u64) -> Result<ServiceMap> {
    let mut nodes = store.net_nodes(from_ns, until_ns, MAX_NODES + 1)?;
    let truncated = nodes.len() > MAX_NODES;
    nodes.truncate(MAX_NODES);

    let keep: std::collections::HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let mut edges = store.net_edges(from_ns, until_ns)?;
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
pub fn servicemap_history(
    store: &Store,
    node: &str,
    from_ns: u64,
    until_ns: u64,
    step_ns: u64,
) -> Result<MapHistory> {
    let step_ns = step_ns.max(1);
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
/// The time brush above the map shows how many connections were opened in each
/// bucket — the map's equivalent of a request count — rather than CPU samples.
/// `connections` is dense: index `i` covers `[from_ns + i*step_ns, … + step_ns)`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MapHistogram {
    pub from_ns: u64,
    pub until_ns: u64,
    pub step_ns: u64,
    #[serde(default)]
    pub connections: Vec<u64>,
}

/// Per-bucket connection totals over the whole map.
pub fn servicemap_histogram(
    store: &Store,
    from_ns: u64,
    until_ns: u64,
    buckets: usize,
) -> Result<MapHistogram> {
    let buckets = buckets.max(1);
    let step_ns = ((until_ns.saturating_sub(from_ns)) / buckets as u64).max(1);
    Ok(MapHistogram {
        from_ns,
        until_ns,
        step_ns,
        connections: store.net_histogram(from_ns, until_ns, step_ns, buckets)?,
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
        assert_eq!(crate::servicemap::protocol_for_port(443), "https");
        assert_eq!(crate::servicemap::protocol_for_port(5432), "postgres");
        assert_eq!(crate::servicemap::protocol_for_port(9042), "cassandra");
        assert_eq!(crate::servicemap::protocol_for_port(4500), "foundationdb");
        assert_eq!(crate::servicemap::protocol_for_port(12345), "tcp");
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
