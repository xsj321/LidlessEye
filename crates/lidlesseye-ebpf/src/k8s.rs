//! What a pod UID means.
//!
//! A cgroup path says *which* pod a stack was sampled in — its UID — but not
//! *what* that pod is. The name, the namespace and the workload that owns it
//! only exist in the Kubernetes API, and the UID is not something a human can
//! read in a dashboard.
//!
//! This module is the half of that lookup which touches no network: a snapshot
//! of pod metadata keyed by UID, which the collector consults while resolving a
//! process. Whoever can reach the API server fills it (the `lidlesseye` binary,
//! see `lidlesseye::k8s`); the collector only reads it. Keeping the HTTP client
//! out of this crate is what makes the standalone case uninteresting: outside a
//! cluster nothing ever fills the index, every lookup misses, and the labels are
//! exactly the ones the cgroup path already provided.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

/// The pod labels that name a service, in the order Kubernetes itself
/// recommends.
///
/// `app.kubernetes.io/name` is the current convention; `app` and `k8s-app` are
/// what most charts in the wild still use, and reading them costs one lookup.
const SERVICE_LABELS: [&str; 3] = ["app.kubernetes.io/name", "app", "k8s-app"];

/// Metadata for one pod, reduced to what labels a profile series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodMeta {
    /// UID of the pod, as it appears in the cgroup path.
    pub uid: String,
    pub namespace: String,
    pub name: String,
    /// Workload that owns the pod: the Deployment for the pods of a Deployment,
    /// the StatefulSet / DaemonSet / Job itself for the others. `None` for a
    /// bare pod, which is exactly the case where a rollout-stable name does not
    /// exist.
    pub workload: Option<String>,
    /// Kind of [`PodMeta::workload`]: `Deployment`, `StatefulSet`, `Job`, ...
    pub workload_kind: Option<String>,
    /// Value of the first label in [`SERVICE_LABELS`] the pod carries.
    pub service: Option<String>,
}

impl PodMeta {
    /// Build the metadata from the fields the API returns.
    ///
    /// `owner` is the pod's first owner reference as `(kind, name)`, and
    /// `template_hash` the `pod-template-hash` label, which is what turns a
    /// ReplicaSet into the Deployment that created it.
    pub fn new(
        uid: impl Into<String>,
        namespace: impl Into<String>,
        name: impl Into<String>,
        labels: &BTreeMap<String, String>,
        owner: Option<(&str, &str)>,
        template_hash: Option<&str>,
    ) -> Self {
        let service = SERVICE_LABELS
            .iter()
            .find_map(|key| labels.get(*key).map(|value| value.trim()))
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let (workload, workload_kind) = match owner {
            Some((kind, name)) => workload_of(kind, name, template_hash),
            None => (None, None),
        };
        PodMeta {
            uid: uid.into(),
            namespace: namespace.into(),
            name: name.into(),
            workload,
            workload_kind,
            service,
        }
    }

    /// The name a series should use, if the pod offers a better one than the
    /// process name.
    ///
    /// A service label wins over the workload because one Deployment can run
    /// several containers that belong to the same service, while a workload
    /// name is still far better than `python3` or `sh`.
    pub fn service_name(&self) -> Option<&str> {
        self.service
            .as_deref()
            .or(self.workload.as_deref())
            .filter(|name| !name.is_empty())
    }
}

/// `(workload, kind)` for a pod owned by `kind`/`name`.
///
/// A Deployment's pods are owned by a ReplicaSet called
/// `<deployment>-<pod-template-hash>`, and the hash changes on every rollout.
/// Left alone it would make each rollout look like a new service, so it is
/// stripped when the pod's own `pod-template-hash` label confirms the suffix.
/// The reported kind then names what the name actually refers to.
///
/// A ReplicaSet created by hand has no such suffix and is left as it is; so is
/// a pod whose hash label was removed, because guessing would be worse than the
/// honest, slightly noisier name.
fn workload_of(
    kind: &str,
    name: &str,
    template_hash: Option<&str>,
) -> (Option<String>, Option<String>) {
    if kind == "ReplicaSet"
        && let Some(hash) = template_hash
        && let Some(deployment) = name.strip_suffix(&format!("-{hash}"))
        && !deployment.is_empty()
    {
        return (Some(deployment.to_string()), Some("Deployment".to_string()));
    }
    (Some(name.to_string()), Some(kind.to_string()))
}

/// A snapshot of the cluster's pods, replaceable while the collector reads it.
///
/// Reads are far more frequent than refreshes (once per process per flush
/// interval), so the snapshot is an `Arc` swapped as a whole: a reader clones
/// the pointer under a short lock and greps it without holding anything.
#[derive(Debug, Default)]
pub struct PodIndex {
    pods: RwLock<Arc<HashMap<String, PodMeta>>>,
}

impl PodIndex {
    pub fn new() -> Self {
        PodIndex::default()
    }

    /// Install a fresh snapshot, returning how many pods it holds.
    ///
    /// The previous snapshot is dropped as a whole, which is what removes the
    /// pods that have been deleted — a merge would keep them forever.
    pub fn replace(&self, pods: HashMap<String, PodMeta>) -> usize {
        let len = pods.len();
        let mut slot = self
            .pods
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Arc::new(pods);
        len
    }

    /// Metadata for `uid`, or `None` when the pod is not in the snapshot.
    ///
    /// A miss is normal: the pod may have started after the last refresh, or
    /// the agent may be running outside a cluster.
    pub fn lookup(&self, uid: &str) -> Option<PodMeta> {
        let snapshot = self
            .pods
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        snapshot.get(uid).cloned()
    }

    pub fn len(&self) -> usize {
        self.pods
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn prefers_the_standard_service_label() {
        let meta = PodMeta::new(
            "uid",
            "payments",
            "checkout-7d9f-abc",
            &labels(&[
                ("app", "legacy-name"),
                ("app.kubernetes.io/name", "checkout"),
            ]),
            None,
            None,
        );
        assert_eq!(meta.service.as_deref(), Some("checkout"));
        assert_eq!(meta.service_name(), Some("checkout"));
    }

    #[test]
    fn falls_back_to_older_service_labels() {
        let meta = PodMeta::new(
            "uid",
            "ns",
            "p",
            &labels(&[("k8s-app", "coredns")]),
            None,
            None,
        );
        assert_eq!(meta.service_name(), Some("coredns"));
    }

    #[test]
    fn ignores_an_empty_label_value() {
        let meta = PodMeta::new(
            "uid",
            "ns",
            "p",
            &labels(&[("app", "  ")]),
            Some(("DaemonSet", "node-exporter")),
            None,
        );
        assert_eq!(meta.service, None);
        assert_eq!(meta.service_name(), Some("node-exporter"));
    }

    #[test]
    fn resolves_a_deployment_through_its_replicaset_hash() {
        // What a Deployment actually looks like from the pod's side.
        let meta = PodMeta::new(
            "uid",
            "default",
            "cluster-spin-7d9f8b6c4-mn2xk",
            &labels(&[("pod-template-hash", "7d9f8b6c4")]),
            Some(("ReplicaSet", "cluster-spin-7d9f8b6c4")),
            Some("7d9f8b6c4"),
        );
        assert_eq!(meta.workload.as_deref(), Some("cluster-spin"));
        assert_eq!(meta.workload_kind.as_deref(), Some("Deployment"));
        assert_eq!(meta.service_name(), Some("cluster-spin"));
    }

    #[test]
    fn keeps_a_hand_made_replicaset_whole() {
        // A bare ReplicaSet is named `foo`; its hash is not a suffix of that
        // name, so nothing is stripped.
        let meta = PodMeta::new(
            "uid",
            "ns",
            "foo-abcde",
            &labels(&[("pod-template-hash", "9d4b7c8f1")]),
            Some(("ReplicaSet", "foo")),
            Some("9d4b7c8f1"),
        );
        assert_eq!(meta.workload.as_deref(), Some("foo"));
        assert_eq!(meta.workload_kind.as_deref(), Some("ReplicaSet"));
    }

    #[test]
    fn a_bare_pod_has_no_workload() {
        let meta = PodMeta::new("uid", "ns", "debug", &labels(&[]), None, None);
        assert_eq!(meta.workload, None);
        assert_eq!(meta.service_name(), None);
    }

    #[test]
    fn statefulset_pods_keep_their_ordinal_out_of_the_workload() {
        let meta = PodMeta::new(
            "uid",
            "ns",
            "kafka-2",
            &labels(&[]),
            Some(("StatefulSet", "kafka")),
            None,
        );
        assert_eq!(meta.workload.as_deref(), Some("kafka"));
        assert_eq!(meta.workload_kind.as_deref(), Some("StatefulSet"));
        // The pod is still resolvable by name, which is where the ordinal goes.
        assert_eq!(meta.name, "kafka-2");
    }

    #[test]
    fn index_answers_by_uid_and_forgets_deleted_pods() {
        let index = PodIndex::new();
        assert!(index.is_empty());
        assert_eq!(index.lookup("a"), None);

        let mut first = HashMap::new();
        first.insert(
            "a".to_string(),
            PodMeta::new("a", "ns", "one", &labels(&[("app", "svc")]), None, None),
        );
        assert_eq!(index.replace(first), 1);
        assert_eq!(
            index
                .lookup("a")
                .and_then(|m| m.service.map(|s| s))
                .as_deref(),
            Some("svc")
        );

        // A refresh that no longer contains `a` — the pod was deleted — must not
        // leave its name behind for a recycled uid.
        index.replace(HashMap::new());
        assert_eq!(index.lookup("a"), None);
        assert!(index.is_empty());
    }
}
