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
            nodes: vec![ServiceNode::new("shop/a", NodeKind::Workload)],
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
            nodes: vec![ServiceNode::new("shop/a", NodeKind::Workload)],
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
    }

    #[test]
    fn ports_name_the_protocol_they_usually_carry() {
        assert_eq!(crate::servicemap::protocol_for_port(443), "https");
        assert_eq!(crate::servicemap::protocol_for_port(5432), "postgres");
        assert_eq!(crate::servicemap::protocol_for_port(12345), "tcp");
    }
}
