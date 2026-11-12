//! Small `/proc` and sysfs helpers used by the collector.

use std::collections::BTreeMap;
use std::path::Path;

/// Process name (as shown by `ps`), truncated by the kernel to 15 characters.
pub fn read_comm(tgid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{tgid}/comm"))
        .ok()
        .map(|s| s.trim_end().to_string())
        .filter(|s| !s.is_empty())
}

/// The cgroup v2 / v1 path of a process, used to derive Kubernetes labels.
pub fn read_cgroup(tgid: u32) -> Option<String> {
    let content = std::fs::read_to_string(format!("/proc/{tgid}/cgroup")).ok()?;
    // Prefer the v2 (unified) hierarchy (`0::/path`), fall back to v1.
    let mut fallback = None;
    for line in content.lines() {
        let mut parts = line.splitn(3, ':');
        let _id = parts.next()?;
        let controllers = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        if controllers.is_empty() {
            return Some(path.to_string());
        }
        if fallback.is_none() {
            fallback = Some(path.to_string());
        }
    }
    fallback
}

/// Extract the Kubernetes pod UID from a cgroup path.
///
/// Handles the common layouts:
///   * cgroup v2: `/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod<uid>.slice/...`
///   * cgroup v1: `/kubepods/burstable/pod<uid>/<container-id>`
///   * containerd: `/kubepods/burstable/pod<uid>/<64 hex chars>`
///
/// The systemd driver replaces the dashes inside the UID with underscores
/// (`pod26fdedc9_3f6d_4bd1_a1c2_e5b7a1f2c3d4.slice`); both spellings map back
/// to the UID the API server uses.
///
/// A pod UID is a UUID and this only returns one. Kubelet names the cgroup of a
/// *static* pod — the control plane: `etcd`, `kube-apiserver`, … — with a
/// 32-character hash instead
/// (`kubelet-kubepods-burstable-pod8d5bd4003a31dbe5a62437d1e90c172b.slice`),
/// and that hash exists nowhere in the API. Reporting it would put an
/// unresolvable id in `k8s_pod_uid` and make every metadata lookup miss for a
/// pod that looks perfectly well identified. Those processes keep their name.
pub fn parse_pod_uid(cgroup: &str) -> Option<String> {
    // Search for the `-pod<uid>`/`/pod<uid>` marker, not the bare string:
    // `kubepods` contains "pod" too and would match much earlier.
    let start = ["-pod", "/pod"]
        .iter()
        .filter_map(|marker| cgroup.rfind(marker))
        .max()?
        + 4;
    let rest = &cgroup[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_hexdigit() || c == '-' || c == '_'))
        .unwrap_or(rest.len());
    let uid = rest[..end].replace('_', "-");
    if !is_uuid(&uid) {
        return None;
    }
    Some(uid)
}

/// `8-4-4-4-12` hexadecimal, the only shape a Kubernetes object UID has.
fn is_uuid(value: &str) -> bool {
    let mut groups = value.split('-');
    for expected in [8, 4, 4, 4, 12] {
        match groups.next() {
            Some(group)
                if group.len() == expected && group.bytes().all(|b| b.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    groups.next().is_none()
}

/// Derive a few low-cardinality Kubernetes labels from a cgroup path.
pub fn kubernetes_labels(cgroup: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    if let Some(uid) = parse_pod_uid(cgroup) {
        labels.insert("k8s_pod_uid".to_string(), uid);
    }
    // `kubepods-burstable-pod<uid>.slice` carries the QoS class.
    for class in ["guaranteed", "burstable", "besteffort"] {
        if cgroup.contains(class) {
            labels.insert("k8s_qos_class".to_string(), class.to_string());
            break;
        }
    }
    if cgroup.contains("/kubepods") || cgroup.contains("kubepods-") {
        labels.insert("k8s_container".to_string(), "true".to_string());
    }
    labels
}

/// Host name, used as the `node` label so that multi-node deployments can tell
/// reports apart.
///
/// Inside a pod the hostname is the *pod* name, which changes on every restart
/// and would add a new `node` value — a new series — per rollout. Kubernetes
/// deployments therefore set `LIDLESSEYE_NODE_NAME` from the Downward API
/// (`spec.nodeName`); it wins over the hostname when present.
pub fn hostname() -> String {
    if let Ok(node) = std::env::var("LIDLESSEYE_NODE_NAME") {
        let node = node.trim().to_string();
        if !node.is_empty() {
            return node;
        }
    }
    if let Ok(host) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let host = host.trim().to_string();
        if !host.is_empty() {
            return host;
        }
    }
    std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string())
}

/// CPU ids that are online and usable by this process.
///
/// Reading `/sys/devices/system/cpu/online` is more accurate than
/// `num_cpus::get()`: inside a container with a cpuset, only a subset of the
/// CPUs can be sampled.
pub fn online_cpus() -> Vec<u32> {
    if let Ok(content) = std::fs::read_to_string("/sys/devices/system/cpu/online") {
        let mut cpus = Vec::new();
        for part in content.trim().split(',') {
            if part.is_empty() {
                continue;
            }
            match part.split_once('-') {
                Some((a, b)) => {
                    if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                        cpus.extend(a..=b.min(a + 4096));
                    }
                }
                None => {
                    if let Ok(cpu) = part.parse::<u32>() {
                        cpus.push(cpu);
                    }
                }
            }
        }
        if !cpus.is_empty() {
            return cpus;
        }
    }
    (0..num_cpus::get() as u32).collect()
}

/// `ns_common.inum` of the agent's PID namespace.
///
/// A pid only names a task together with the namespace it was read in, and
/// `/proc` shows exactly one namespace — the one this process is in. The eBPF
/// programs use this value to pick, for every task, the pid belonging to *this*
/// namespace out of the `task_struct->thread_pid->numbers[]` array, which is
/// the only number `/proc` here can resolve.
///
/// It has to be read from the link target (`pid:[4026532294]`), not from
/// `st_ino`: nsfs hands out anonymous inodes, so since kernel 6.13 the inode
/// number and the namespace id are unrelated numbers.
pub fn pid_namespace_inum() -> Option<u32> {
    let target = std::fs::read_link("/proc/self/ns/pid").ok()?;
    let text = target.to_str()?;
    let inum = text.strip_prefix("pid:[")?.strip_suffix(']')?;
    let inum: u32 = inum.parse().ok()?;
    (inum != 0).then_some(inum)
}

/// Is the process inside a Kubernetes pod?
pub fn in_kubernetes() -> bool {
    Path::new("/var/run/secrets/kubernetes.io/serviceaccount/token").exists()
}

/// Read the current `perf_event_paranoid` value.
pub fn perf_event_paranoid() -> Option<i32> {
    std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Raise `RLIMIT_MEMLOCK` so that the eBPF maps can be created.
pub fn raise_memlock_limit() -> bool {
    unsafe {
        let limit = libc::rlimit {
            rlim_cur: libc::RLIM_INFINITY,
            rlim_max: libc::RLIM_INFINITY,
        };
        libc::setrlimit(libc::RLIMIT_MEMLOCK, &limit) == 0
    }
}

/// Are we running as root (or with enough capabilities to load eBPF)?
pub fn is_privileged() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cgroup_v2_pod_uid() {
        let cg = "/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod9d1b4a7c-8c1e-4f1d-9a3e-1b2c3d4e5f60.slice/cri-containerd-abc.scope";
        assert_eq!(
            parse_pod_uid(cg).as_deref(),
            Some("9d1b4a7c-8c1e-4f1d-9a3e-1b2c3d4e5f60")
        );
        assert_eq!(
            kubernetes_labels(cg)
                .get("k8s_qos_class")
                .map(String::as_str),
            Some("burstable")
        );
    }

    #[test]
    fn parses_cgroup_v1_pod_uid() {
        let cg = "/kubepods/burstable/pod1234abcd-1111-2222-3333-444455556666/0123456789abcdef";
        assert_eq!(
            parse_pod_uid(cg).as_deref(),
            Some("1234abcd-1111-2222-3333-444455556666")
        );
    }

    #[test]
    fn parses_systemd_slice_with_underscored_uid() {
        // The systemd cgroup driver rewrites the UID's dashes as underscores.
        // Stopping at the first `_` used to truncate the label to eight
        // characters, so two containers of the same pod looked unrelated.
        let cg = "/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-pod26fdedc9_3f6d_4bd1_a1c2_e5b7a1f2c3d4.slice/cri-containerd-77b.scope";
        assert_eq!(
            parse_pod_uid(cg).as_deref(),
            Some("26fdedc9-3f6d-4bd1-a1c2-e5b7a1f2c3d4")
        );
        assert_eq!(
            kubernetes_labels(cg)
                .get("k8s_qos_class")
                .map(String::as_str),
            Some("besteffort")
        );
    }

    #[test]
    fn ignores_plain_cgroups() {
        assert_eq!(parse_pod_uid("/user.slice/user-1000.slice"), None);
        assert!(kubernetes_labels("/user.slice").is_empty());
    }

    #[test]
    fn rejects_the_hashed_cgroup_of_a_static_pod() {
        // Verbatim from a kind control-plane node (k8s 1.36): the pod's real
        // UID is a normal UUID, but the cgroup carries a 32-character hash that
        // the API never returns. Labeling the series with it would look
        // correct and resolve to nothing, so the id is dropped and the pod is
        // named after its process instead.
        let cg = "/kubelet.slice/kubelet-kubepods.slice/kubelet-kubepods-burstable.slice/\
                  kubelet-kubepods-burstable-pod8d5bd4003a31dbe5a62437d1e90c172b.slice/\
                  cri-containerd-009d176d3433bad77f9ad74ffcc17d5f7b2bc15080e9dde28c5917d8270efcde.scope";
        assert_eq!(parse_pod_uid(cg), None);
        let labels = kubernetes_labels(cg);
        assert!(!labels.contains_key("k8s_pod_uid"));
        // It is still a container, which is what the other labels say.
        assert_eq!(
            labels.get("k8s_container").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            labels.get("k8s_qos_class").map(String::as_str),
            Some("burstable")
        );
    }

    #[test]
    fn recognises_uuid_shapes() {
        assert!(is_uuid("9d1b4a7c-8c1e-4f1d-9a3e-1b2c3d4e5f60"));
        assert!(!is_uuid("9d1b4a7c-8c1e-4f1d-9a3e-1b2c3d4e5f6"));
        assert!(!is_uuid("9d1b4a7c8c1e4f1d9a3e1b2c3d4e5f60"));
        assert!(!is_uuid("9d1b4a7c-8c1e-4f1d-9a3e-1b2c3d4e5f60-extra"));
        assert!(!is_uuid("9d1b4a7c-8c1e-4f1d-9a3e-1b2c3d4e5f6z"));
    }

    #[test]
    fn online_cpu_list_is_not_empty() {
        assert!(!online_cpus().is_empty());
    }

    #[test]
    fn reads_the_pid_namespace_id() {
        let inum = pid_namespace_inum().expect("/proc/self/ns/pid should be readable");
        // Namespace ids are allocated from a high range and never zero; a small
        // number would mean the link target was parsed wrongly.
        assert!(inum > 1_000_000, "unexpected namespace id {inum}");
        // Everything in this process's `/proc` that shares its namespace must
        // report the same id, which is what the eBPF walk matches on.
        let own = std::fs::read_link("/proc/1/ns/pid").ok();
        if let Some(own) = own.and_then(|link| link.to_str().map(str::to_string))
            && own == format!("pid:[{inum}]")
        {
            assert_eq!(pid_namespace_inum(), Some(inum));
        }
    }
}
