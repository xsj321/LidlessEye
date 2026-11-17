//! Reads pod metadata from the Kubernetes API.
//!
//! The collector learns *which* pod a stack came from (the UID in the cgroup
//! path) but not what that pod is. This module closes the gap: it lists the
//! pods the agent can see, keeps a snapshot keyed by UID, and hands that
//! snapshot to the collector, which then labels a series with the pod's
//! namespace, name and workload — and names it after the service instead of
//! after the binary.
//!
//! ## Why a poll and not a watch
//!
//! A profile series accumulates over minutes, so a pod that appears 30 s late
//! in the index costs one unnamed flush, not a lost sample: the samples are
//! stored against the UID either way and only the *name* waits. A `list` with a
//! `spec.nodeName` field selector is a few kilobytes per node per interval; a
//! watch would need reconnect logic, resource-version bookkeeping and a
//! resync path, all to win half a minute of naming accuracy.
//!
//! ## Outside a cluster
//!
//! [`ClusterApi::in_cluster`] is the only entry point, and it requires both the
//! `KUBERNETES_SERVICE_HOST` variable and a mounted service account. Without
//! them — a host install, a container, a CI run — no client is built, no
//! request is made, and the collector keeps the process-name behaviour it has
//! always had.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use lidlesseye_ebpf::k8s::{ContainerInfo, PodIndex, PodMeta};
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::{Error, Result};

/// Where a pod's service account is mounted.
const SERVICE_ACCOUNT_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

/// Pod names are not the point of the cache — being *fresh* is. Anything
/// shorter than this would hammer the API server for no gain.
const MIN_REFRESH: Duration = Duration::from_secs(5);

/// Upper bound on pages, so a misbehaving `continue` token cannot loop forever.
const MAX_PAGES: usize = 40;

/// A client for the API server a pod is already allowed to talk to.
pub struct ClusterApi {
    client: reqwest::Client,
    /// `https://<KUBERNETES_SERVICE_HOST>:<KUBERNETES_SERVICE_PORT>`.
    base: String,
    token: PathBuf,
    /// `spec.nodeName` of this agent, used to fetch only the pods that can run
    /// here. `None` means "list the whole cluster", which is correct but larger.
    node: Option<String>,
}

impl ClusterApi {
    /// Build a client from the pod's service account, or `None` when there is
    /// no cluster to talk to.
    ///
    /// Both halves of the test matter. `KUBERNETES_SERVICE_HOST` alone appears
    /// in any pod, including the ones the agent profiles, so it is not proof
    /// that *this* process has credentials; the token file is.
    pub fn in_cluster() -> Option<ClusterApi> {
        let host = std::env::var("KUBERNETES_SERVICE_HOST").ok()?;
        let host = host.trim();
        if host.is_empty() {
            return None;
        }
        let port = std::env::var("KUBERNETES_SERVICE_PORT")
            .ok()
            .filter(|port| !port.trim().is_empty())
            .unwrap_or_else(|| "443".to_string());
        let token = PathBuf::from(SERVICE_ACCOUNT_DIR).join("token");
        if !token.is_file() {
            debug!(
                path = %token.display(),
                "no service account token; not treating this as a cluster"
            );
            return None;
        }

        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("lidlesseye-agent/", env!("CARGO_PKG_VERSION")));
        // The API server presents the cluster's own CA, which no public trust
        // store knows about. Without it every request fails with a certificate
        // error that reads like a network problem.
        let ca = PathBuf::from(SERVICE_ACCOUNT_DIR).join("ca.crt");
        match std::fs::read(&ca) {
            Ok(pem) => match reqwest::Certificate::from_pem(&pem) {
                Ok(cert) => builder = builder.add_root_certificate(cert),
                Err(err) => warn!(path = %ca.display(), %err, "cannot parse the cluster CA"),
            },
            Err(err) => warn!(path = %ca.display(), %err, "cannot read the cluster CA"),
        }
        let client = builder.build().ok()?;

        Some(ClusterApi {
            client,
            base: format!("https://{host}:{port}"),
            token,
            node: std::env::var("LIDLESSEYE_NODE_NAME")
                .ok()
                .map(|node| node.trim().to_string())
                .filter(|node| !node.is_empty()),
        })
    }

    /// Fetch every pod on this node, keyed by UID.
    ///
    /// The token is re-read on each call: a projected service account volume is
    /// rotated in place, and caching the string would start failing an hour
    /// after start-up, which looks exactly like a revoked permission.
    pub async fn list_pods(&self) -> Result<HashMap<String, PodMeta>> {
        self.fetch_pods(true).await
    }

    /// Every pod in the cluster, not just the ones on this node.
    ///
    /// The service map resolves a TCP peer by address, and that peer can be
    /// scheduled anywhere; a node-scoped list would label half of the map
    /// "external". The payload is bigger but the permission is the same one
    /// the ClusterRole already grants.
    pub async fn list_all_pods(&self) -> Result<HashMap<String, PodMeta>> {
        self.fetch_pods(false).await
    }

    async fn fetch_pods(&self, node_scoped: bool) -> Result<HashMap<String, PodMeta>> {
        let token = std::fs::read_to_string(&self.token)
            .map_err(|err| Error::Http(format!("reading {}: {err}", self.token.display())))?;
        let token = token.trim().to_string();

        let mut pods = HashMap::new();
        let mut cont: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut query: Vec<(&str, String)> = vec![("limit", "500".to_string())];
            if let (true, Some(node)) = (node_scoped, &self.node) {
                query.push(("fieldSelector", format!("spec.nodeName={node}")));
            }
            if let Some(token) = &cont {
                query.push(("continue", token.clone()));
            }
            let response = self
                .client
                .get(format!("{}/api/v1/pods", self.base))
                .bearer_auth(&token)
                .query(&query)
                .send()
                .await
                .map_err(|err| Error::Http(format!("listing pods: {err}")))?;
            let status = response.status();
            if !status.is_success() {
                // The body carries the reason (`pods is forbidden: ...`), which
                // is the difference between a wrong RBAC rule and a wrong URL.
                let body = response.text().await.unwrap_or_default();
                let body: String = body.trim().chars().take(400).collect();
                return Err(Error::Http(format!("listing pods: HTTP {status}: {body}")));
            }
            let list: PodList = response
                .json()
                .await
                .map_err(|err| Error::Http(format!("parsing the pod list: {err}")))?;
            for item in list.items {
                if let Some((uid, meta)) = item.into_meta() {
                    pods.insert(uid, meta);
                }
            }
            cont = list
                .metadata
                .continue_token
                .filter(|token| !token.is_empty());
            if cont.is_none() {
                break;
            }
        }
        Ok(pods)
    }

    /// Every Service and the addresses it answers on.
    pub async fn list_services(&self) -> Result<Vec<(IpAddr, Vec<u16>, ServiceRef)>> {
        let token = std::fs::read_to_string(&self.token)
            .map_err(|err| Error::Http(format!("reading {}: {err}", self.token.display())))?;
        let token = token.trim().to_string();

        let mut out = Vec::new();
        let mut cont: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut query: Vec<(&str, String)> = vec![("limit", "500".to_string())];
            if let Some(token) = &cont {
                query.push(("continue", token.clone()));
            }
            let response = self
                .client
                .get(format!("{}/api/v1/services", self.base))
                .bearer_auth(&token)
                .query(&query)
                .send()
                .await
                .map_err(|err| Error::Http(format!("listing services: {err}")))?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                let body: String = body.trim().chars().take(400).collect();
                return Err(Error::Http(format!(
                    "listing services: HTTP {status}: {body}"
                )));
            }
            let list: ServiceList = response
                .json()
                .await
                .map_err(|err| Error::Http(format!("parsing the service list: {err}")))?;
            for item in list.items {
                let namespace = item.metadata.namespace;
                let name = item.metadata.name;
                if namespace.is_empty() || name.is_empty() {
                    continue;
                }
                let mut ips: Vec<IpAddr> = item
                    .spec
                    .cluster_ips
                    .iter()
                    .chain(std::iter::once(&item.spec.cluster_ip))
                    .filter_map(|ip| ip.parse().ok())
                    .collect();
                ips.sort();
                ips.dedup();
                if ips.is_empty() {
                    // Headless Services have no ClusterIP: their traffic goes
                    // straight to pod addresses, which the pod index covers.
                    continue;
                }
                let ports: Vec<u16> = item.spec.ports.iter().map(|p| p.port).collect();
                let service = ServiceRef { namespace, name };
                for ip in ips {
                    out.push((ip, ports.clone(), service.clone()));
                }
            }
            cont = list
                .metadata
                .continue_token
                .filter(|token| !token.is_empty());
            if cont.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// The node the list is filtered by, for log lines.
    pub fn node(&self) -> Option<&str> {
        self.node.as_deref()
    }
}

/// Start refreshing the pod index in the background.
///
/// Returns `None` when there is no cluster to ask, which is the ordinary case
/// for a host install: the collector then labels a series exactly as it did
/// before pod metadata existed.
pub fn start_pod_index(refresh: Duration) -> Option<Arc<PodIndex>> {
    let Some(api) = ClusterApi::in_cluster() else {
        debug!("not running in a cluster: series are named after the process");
        return None;
    };
    let refresh = refresh.max(MIN_REFRESH);
    let index = Arc::new(PodIndex::new());

    let Some(runtime) = tokio::runtime::Handle::try_current().ok() else {
        warn!("no async runtime: pod metadata stays unread");
        return None;
    };
    let shared = Arc::clone(&index);
    runtime.spawn(async move {
        // The first fetch runs immediately: the very first flush then already
        // carries names, instead of falling back for one interval.
        let mut last_error = match api.list_pods().await {
            Ok(pods) => {
                // Install before reporting: the log line describes what a
                // reader of the index will now see, not what the API sent.
                shared.replace(pods);
                report(&api, &shared);
                None
            }
            Err(err) => {
                let err = err.to_string();
                warn!(error = %err, "cannot read pod metadata; series fall back to the process name");
                Some(err)
            }
        };

        loop {
            tokio::time::sleep(refresh).await;
            match api.list_pods().await {
                Ok(pods) => {
                    if let Some(err) = last_error.take() {
                        info!(previous = %err, "pod metadata is readable again");
                    }
                    debug!(pods = pods.len(), "refreshed pod metadata");
                    shared.replace(pods);
                }
                Err(err) => {
                    // Repeating an identical failure every interval is noise; a
                    // *different* failure is news.
                    let text = err.to_string();
                    if last_error.as_deref() != Some(text.as_str()) {
                        warn!(error = %text, "cannot refresh pod metadata; keeping the last snapshot");
                    }
                    last_error = Some(text);
                }
            }
        }
    });
    Some(index)
}

/// Start a cluster-wide pod index, for the service map.
///
/// The profiling index is node-scoped on purpose (it only ever answers "what is
/// UID X"), but a TCP peer has no UID: the map has to resolve an address, and
/// the pod behind it can be on any node. `None` outside a cluster.
pub fn start_cluster_pod_index(refresh: Duration) -> Option<Arc<PodIndex>> {
    let api = ClusterApi::in_cluster()?;
    let refresh = refresh.max(MIN_REFRESH);
    let index = Arc::new(PodIndex::new());
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let shared = Arc::clone(&index);
    runtime.spawn(async move {
        let mut last_error: Option<String> = None;
        loop {
            match api.list_all_pods().await {
                Ok(pods) => {
                    if let Some(err) = last_error.take() {
                        info!(previous = %err, "cluster pod index is readable again");
                    }
                    let count = pods.len();
                    shared.replace(pods);
                    debug!(pods = count, "refreshed cluster-wide pod metadata");
                }
                Err(err) => {
                    let text = err.to_string();
                    if last_error.as_deref() != Some(text.as_str()) {
                        warn!(error = %text, "cannot refresh the cluster pod index");
                    }
                    last_error = Some(text);
                }
            }
            tokio::time::sleep(refresh).await;
        }
    });
    Some(index)
}

/// A ClusterIP Service, as far as the map is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRef {
    pub namespace: String,
    pub name: String,
}

impl ServiceRef {
    pub fn id(&self) -> String {
        lidlesseye_core::servicemap::node_id(&self.namespace, &self.name)
    }
}

impl std::fmt::Display for ServiceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.namespace, self.name)
    }
}

/// ClusterIP -> Service, so `10.96.0.10:443` becomes `default/kubernetes`
/// instead of "external".
///
/// The port matters: one ClusterIP can expose several ports, and Kubernetes
/// uses that to route to different backends. Lookups fall back to the address
/// alone when the port is not one the Service declares (a client connecting to
/// a port the Service does not own is a misconfiguration worth seeing anyway).
#[derive(Debug, Default)]
pub struct ServiceIndex {
    by_addr: RwLock<Arc<HashMap<(IpAddr, u16), ServiceRef>>>,
    by_ip: RwLock<Arc<HashMap<IpAddr, ServiceRef>>>,
}

impl ServiceIndex {
    pub fn new() -> Self {
        ServiceIndex::default()
    }

    /// Install a snapshot, returning how many addresses it covers.
    pub fn replace(&self, services: Vec<(IpAddr, Vec<u16>, ServiceRef)>) -> usize {
        let mut by_addr = HashMap::new();
        let mut by_ip = HashMap::new();
        for (ip, ports, service) in services {
            for port in ports {
                by_addr.insert((ip, port), service.clone());
            }
            by_ip.insert(ip, service);
        }
        let len = by_ip.len();
        *self
            .by_addr
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(by_addr);
        *self
            .by_ip
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(by_ip);
        len
    }

    pub fn lookup(&self, ip: IpAddr, port: u16) -> Option<ServiceRef> {
        let by_addr = self
            .by_addr
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(service) = by_addr.get(&(ip, port)) {
            return Some(service.clone());
        }
        let by_ip = self
            .by_ip
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        by_ip.get(&ip).cloned()
    }
}

/// Start refreshing the Service index. A missing RBAC rule is reported once and
/// degrades to "ClusterIP edges look external"; it never stops the collector.
pub fn start_service_index(refresh: Duration) -> Option<Arc<ServiceIndex>> {
    let api = ClusterApi::in_cluster()?;
    let refresh = refresh.max(MIN_REFRESH);
    let index = Arc::new(ServiceIndex::new());
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let shared = Arc::clone(&index);
    runtime.spawn(async move {
        let mut last_error: Option<String> = None;
        loop {
            match api.list_services().await {
                Ok(services) => {
                    if let Some(err) = last_error.take() {
                        info!(previous = %err, "the Service list is readable again");
                    }
                    let count = services.len();
                    shared.replace(services);
                    debug!(services = count, "refreshed ClusterIP services");
                }
                Err(err) => {
                    let text = err.to_string();
                    if last_error.as_deref() != Some(text.as_str()) {
                        warn!(
                            error = %text,
                            "cannot list Services; ClusterIP edges will look external \
                             (grant `services` `list` to fix it)"
                        );
                    }
                    last_error = Some(text);
                }
            }
            tokio::time::sleep(refresh).await;
        }
    });
    Some(index)
}

/// Log what was indexed, and complain when the filter matched nothing — an
/// agent whose `spec.nodeName` disagrees with the API's idea of the node names
/// would otherwise look like a cluster with no pods.
fn report(api: &ClusterApi, index: &PodIndex) {
    let pods = index.len();
    match api.node() {
        Some(node) if pods == 0 => warn!(
            node,
            "the pod list is empty; check that LIDLESSEYE_NODE_NAME matches the node name"
        ),
        Some(node) => info!(node, pods, "indexed pod metadata for this node"),
        None => info!(pods, "indexed pod metadata for the cluster"),
    }
}

#[derive(Debug, Deserialize)]
struct PodList {
    #[serde(default)]
    items: Vec<PodItem>,
    #[serde(default)]
    metadata: ListMeta,
}

#[derive(Debug, Deserialize)]
struct ServiceList {
    #[serde(default)]
    items: Vec<ServiceItem>,
    #[serde(default)]
    metadata: ListMeta,
}

#[derive(Debug, Deserialize)]
struct ServiceItem {
    metadata: ObjectMeta,
    #[serde(default)]
    spec: ServiceSpec,
}

#[derive(Debug, Default, Deserialize)]
struct ServiceSpec {
    #[serde(default, rename = "clusterIP")]
    cluster_ip: String,
    #[serde(default, rename = "clusterIPs")]
    cluster_ips: Vec<String>,
    #[serde(default)]
    ports: Vec<ServicePort>,
}

#[derive(Debug, Deserialize)]
struct ServicePort {
    #[serde(default)]
    port: u16,
}

#[derive(Debug, Default, Deserialize)]
struct ListMeta {
    #[serde(default, rename = "continue")]
    continue_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PodItem {
    metadata: ObjectMeta,
    #[serde(default)]
    status: PodStatus,
    #[serde(default)]
    spec: PodSpec,
}

/// Only the declared container ports: they are what tells two host-network pods
/// sharing a node address apart on the service map.
#[derive(Debug, Default, Deserialize)]
struct PodSpec {
    #[serde(default)]
    containers: Vec<PodContainer>,
    /// Where the scheduler placed the pod; the instance list shows it.
    #[serde(default, rename = "nodeName")]
    node_name: String,
}

#[derive(Debug, Default, Deserialize)]
struct PodContainer {
    #[serde(default)]
    name: String,
    #[serde(default)]
    ports: Vec<PodContainerPort>,
}

#[derive(Debug, Default, Deserialize)]
struct PodContainerPort {
    #[serde(default, rename = "containerPort")]
    container_port: u16,
}

#[derive(Debug, Default, Deserialize)]
struct PodStatus {
    /// Single-stack clusters only report this one.
    #[serde(default, rename = "podIP")]
    pod_ip: String,
    /// Dual-stack clusters report every address here.
    #[serde(default, rename = "podIPs")]
    pod_ips: Vec<PodIp>,
    #[serde(default)]
    phase: String,
    /// Readiness lives here rather than in `phase`: a pod that is `Running`
    /// with one failing container is up and serving errors, which is a
    /// different thing from a pod that is fine.
    #[serde(default)]
    conditions: Vec<PodCondition>,
    /// The runtime id of every container; with the spec's name it is what
    /// turns a cgroup into `istio-proxy` or `app`.
    #[serde(default, rename = "containerStatuses")]
    container_statuses: Vec<PodContainerStatus>,
}

#[derive(Debug, Default, Deserialize)]
struct PodContainerStatus {
    #[serde(default)]
    name: String,
    /// `containerd://<id>`, `docker://<id>`, `cri-o://<id>`, ...
    #[serde(default, rename = "containerID")]
    container_id: String,
}

#[derive(Debug, Deserialize)]
struct PodCondition {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    status: String,
}

#[derive(Debug, Deserialize)]
struct PodIp {
    ip: String,
}

#[derive(Debug, Default, Deserialize)]
struct ObjectMeta {
    #[serde(default)]
    uid: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default, rename = "ownerReferences")]
    owner_references: Vec<OwnerReference>,
}

#[derive(Debug, Deserialize)]
struct OwnerReference {
    kind: String,
    name: String,
}

impl PodItem {
    /// Reduce one API object to the fields the collector labels a series with.
    fn into_meta(self) -> Option<(String, PodMeta)> {
        let meta = self.metadata;
        if meta.uid.is_empty() {
            return None;
        }
        let owner = meta
            .owner_references
            .first()
            .map(|owner| (owner.kind.as_str(), owner.name.as_str()));
        let template_hash = meta.labels.get("pod-template-hash").map(String::as_str);
        let mut ips: Vec<IpAddr> = self
            .status
            .pod_ips
            .iter()
            .filter_map(|entry| entry.ip.parse().ok())
            .collect();
        if let Ok(ip) = self.status.pod_ip.parse::<IpAddr>() {
            ips.push(ip);
        }
        ips.sort();
        ips.dedup();
        let mut ports: Vec<u16> = self
            .spec
            .containers
            .iter()
            .flat_map(|container| container.ports.iter())
            .map(|port| port.container_port)
            .collect();
        ports.sort_unstable();
        ports.dedup();
        let ready = self
            .status
            .conditions
            .iter()
            .any(|condition| condition.kind == "Ready" && condition.status == "True");
        let phase = self.status.phase.clone();
        let host = self.spec.node_name.clone();
        let containers = self
            .spec
            .containers
            .iter()
            .map(|container| {
                let mut ports: Vec<u16> =
                    container.ports.iter().map(|p| p.container_port).collect();
                ports.sort_unstable();
                ports.dedup();
                // A container without a started id (CrashLoopBackOff, an init
                // container) still belongs in the pod's list: it has a name,
                // and traffic from an old instance of it is still its traffic.
                let id = self
                    .status
                    .container_statuses
                    .iter()
                    .find(|status| status.name == container.name)
                    .map(|status| runtime_id(&status.container_id))
                    .unwrap_or_default();
                ContainerInfo::new(container.name.clone(), id, ports)
            })
            .collect();
        let pod = PodMeta::new(
            meta.uid.as_str(),
            meta.namespace.as_str(),
            meta.name.as_str(),
            &meta.labels,
            owner,
            template_hash,
        )
        .with_ips(ips)
        .with_ports(ports)
        .with_health(phase, ready)
        .with_containers(containers)
        .on_host(host);
        Some((meta.uid, pod))
    }
}

/// The id part of a `containerd://<id>` reference.
///
/// The kubelet prefixes the runtime scheme and nothing else strips it; a
/// cgroup carries the bare id, so leaving the prefix on would make every
/// container lookup miss.
fn runtime_id(reference: &str) -> String {
    reference
        .rsplit_once("://")
        .map(|(_, id)| id)
        .unwrap_or(reference)
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pod as the API returns it, trimmed to the fields that matter.
    fn list_json(labels: &str, owner: &str) -> String {
        format!(
            r#"{{
                "items": [
                    {{
                        "metadata": {{
                            "uid": "8f1c2e64-1f8b-4b1e-9b3a-2d5c0e7a9b11",
                            "name": "checkout-7d9f8b6c4-mn2xk",
                            "namespace": "payments",
                            "labels": {labels},
                            "ownerReferences": {owner}
                        }}
                    }}
                ],
                "metadata": {{}}
            }}"#
        )
    }

    #[test]
    fn maps_a_deployment_pod() {
        let json = list_json(
            r#"{"pod-template-hash": "7d9f8b6c4", "app.kubernetes.io/name": "checkout"}"#,
            r#"[{"kind": "ReplicaSet", "name": "checkout-7d9f8b6c4"}]"#,
        );
        let list: PodList = serde_json::from_str(&json).expect("valid pod list");
        let (uid, pod) = list.items.into_iter().next().unwrap().into_meta().unwrap();
        assert_eq!(uid, "8f1c2e64-1f8b-4b1e-9b3a-2d5c0e7a9b11");
        assert_eq!(pod.namespace, "payments");
        assert_eq!(pod.name, "checkout-7d9f8b6c4-mn2xk");
        assert_eq!(pod.service_name(), Some("checkout"));
        assert_eq!(pod.workload.as_deref(), Some("checkout"));
        assert_eq!(pod.workload_kind.as_deref(), Some("Deployment"));
    }

    #[test]
    fn maps_a_bare_pod_without_owner_references() {
        let json = list_json("{}", "[]");
        let list: PodList = serde_json::from_str(&json).expect("valid pod list");
        let (_, pod) = list.items.into_iter().next().unwrap().into_meta().unwrap();
        assert_eq!(pod.workload, None);
        assert_eq!(pod.service_name(), None);
    }

    #[test]
    fn drops_an_object_without_a_uid() {
        let json = r#"{"items": [{"metadata": {"name": "no-uid"}}], "metadata": {}}"#;
        let list: PodList = serde_json::from_str(json).expect("valid pod list");
        assert!(list.items.into_iter().next().unwrap().into_meta().is_none());
    }

    #[test]
    fn reads_the_pagination_token() {
        let json = r#"{"items": [], "metadata": {"continue": "abc123"}}"#;
        let list: PodList = serde_json::from_str(json).expect("valid pod list");
        assert_eq!(list.metadata.continue_token.as_deref(), Some("abc123"));
    }

    /// The pod's containers, with the runtime id that ties a cgroup to a name.
    /// Without the id the map can say "this pod talks to that one" but not
    /// which container opened the connection, which is the whole point of the
    /// per-container rows.
    #[test]
    fn reads_container_names_ids_and_ports() {
        let id = "009d176d3433bad77f9ad74ffcc17d5f7b2bc15080e9dde28c5917d8270efcde";
        let docker_id = "ab".repeat(32);
        let json = format!(
            r#"{{
                "items": [{{
                    "metadata": {{"uid": "u1", "name": "checkout-1", "namespace": "shop"}},
                    "spec": {{
                        "containers": [
                            {{"name": "app", "ports": [{{"containerPort": 8080}}]}},
                            {{"name": "istio-proxy", "ports": [{{"containerPort": 15006}}]}},
                            {{"name": "not-started"}}
                        ]
                    }},
                    "status": {{
                        "phase": "Running",
                        "containerStatuses": [
                            {{"name": "app", "containerID": "containerd://{id}"}},
                            {{"name": "istio-proxy", "containerID": "docker://{docker_id}"}}
                        ]
                    }}
                }}],
                "metadata": {{}}
            }}"#
        );
        let list: PodList = serde_json::from_str(&json).expect("valid pod list");
        let (_, pod) = list.items.into_iter().next().unwrap().into_meta().unwrap();
        assert_eq!(
            pod.containers
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["app", "istio-proxy", "not-started"]
        );
        // The runtime scheme is stripped: a cgroup carries the bare id.
        assert_eq!(pod.containers[0].id, id);
        assert_eq!(pod.containers[1].id, docker_id);
        assert_eq!(pod.containers[0].ports, [8080]);
        // A container that has not started has no id but keeps its row.
        assert_eq!(pod.containers[2].id, "");
        // The pod-wide port list is still the union, as before.
        assert_eq!(pod.ports, [8080, 15006]);
    }

    /// The one property the whole feature rests on: outside a cluster nothing
    /// is attempted. This is the regression test for "the host install still
    /// works" — a client built here would mean any `KUBERNETES_SERVICE_HOST`
    /// in the environment (a CI runner, a dev shell) changes how series are
    /// named on a plain machine.
    #[test]
    fn outside_a_cluster_there_is_no_client() {
        // When the tests themselves run in a pod, the answer is legitimately
        // "yes, there is a cluster" and there is nothing to prove.
        if PathBuf::from(SERVICE_ACCOUNT_DIR).join("token").exists() {
            eprintln!("skipping: a service account is mounted, this *is* a cluster");
            return;
        }
        assert!(
            ClusterApi::in_cluster().is_none(),
            "no in-cluster client without a service account"
        );
        assert!(start_pod_index(Duration::from_secs(30)).is_none());
    }
}
