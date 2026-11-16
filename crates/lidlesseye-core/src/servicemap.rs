//! The service map data model.
//!
//! The map is a time-bucketed graph: agents observe TCP connections inside each
//! pod's network namespace, resolve both endpoints to a service (a workload, a
//! ClusterIP service or "external") and ship *deltas* for one collection
//! window. Storage sums those deltas over whatever window the dashboard asks
//! for, which is what makes the map follow the same time range as the
//! flamegraph.
//!
//! Edges carry only what the kernel exposes without parsing application
//! payloads: new connections, retransmits and the protocol guess that comes
//! from the port. Node CPU is joined from the profile store, so a service that
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
                }
                None => {
                    index.insert(node.id.clone(), self.nodes.len());
                    self.nodes.push(node);
                }
            }
        }
        let mut edge_index: HashMap<(String, String, String), usize> = self
            .edges
            .iter()
            .enumerate()
            .map(|(i, e)| ((e.client.clone(), e.server.clone(), e.protocol.clone()), i))
            .collect();
        for edge in other.edges {
            let key = (
                edge.client.clone(),
                edge.server.clone(),
                edge.protocol.clone(),
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
}

/// An aggregated edge in a query response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MapEdge {
    pub client: String,
    pub server: String,
    pub protocol: String,
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

/// Best-effort protocol name from a port number.
///
/// This is a label, not a promise: telling HTTP from gRPC needs payload
/// inspection, which a connection-level map deliberately does not do.
pub fn protocol_for_port(port: u16) -> &'static str {
    match port {
        80 => "http",
        443 => "https",
        8080 | 8000 | 3000 => "http",
        8443 | 9443 => "https",
        53 => "dns",
        5432 => "postgres",
        3306 => "mysql",
        6379 => "redis",
        27017 => "mongodb",
        9092 => "kafka",
        5672 => "amqp",
        11211 => "memcached",
        9200 => "elasticsearch",
        2181 => "zookeeper",
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

    #[test]
    fn ports_name_the_protocol_they_usually_carry() {
        assert_eq!(crate::servicemap::protocol_for_port(443), "https");
        assert_eq!(crate::servicemap::protocol_for_port(5432), "postgres");
        assert_eq!(crate::servicemap::protocol_for_port(12345), "tcp");
    }
}
