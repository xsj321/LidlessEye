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
        .find(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
        .unwrap_or(rest.len());
    let uid = &rest[..end];
    if uid.len() < 8 {
        return None;
    }
    Some(uid.to_string())
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
pub fn hostname() -> String {
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

/// `(st_dev, st_ino)` of the agent's PID namespace.
///
/// The loader feeds these into the eBPF programs so that the pids stored in the
/// maps are relative to the namespace the agent can actually read from `/proc`.
pub fn pid_namespace() -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata("/proc/self/ns/pid").ok()?;
    Some((metadata.dev(), metadata.ino()))
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
            kubernetes_labels(cg).get("k8s_qos_class").map(String::as_str),
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
    fn ignores_plain_cgroups() {
        assert_eq!(parse_pod_uid("/user.slice/user-1000.slice"), None);
        assert!(kubernetes_labels("/user.slice").is_empty());
    }

    #[test]
    fn online_cpu_list_is_not_empty() {
        assert!(!online_cpus().is_empty());
    }
}
