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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lidlesseye_ebpf::k8s::{PodIndex, PodMeta};
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
        let token = std::fs::read_to_string(&self.token)
            .map_err(|err| Error::Http(format!("reading {}: {err}", self.token.display())))?;
        let token = token.trim().to_string();

        let mut pods = HashMap::new();
        let mut cont: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut query: Vec<(&str, String)> = vec![("limit", "500".to_string())];
            if let Some(node) = &self.node {
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

#[derive(Debug, Default, Deserialize)]
struct ListMeta {
    #[serde(default, rename = "continue")]
    continue_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PodItem {
    metadata: ObjectMeta,
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
        let pod = PodMeta::new(
            meta.uid.as_str(),
            meta.namespace.as_str(),
            meta.name.as_str(),
            &meta.labels,
            owner,
            template_hash,
        );
        Some((meta.uid, pod))
    }
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
