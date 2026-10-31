//! Host platform detection and binary lookup.

use std::path::{Path, PathBuf};

use meduza_np_plugin_sdk::Platform;

pub fn detect() -> Platform {
    Platform {
        openwrt: Path::new("/etc/openwrt_release").exists(),
        container: std::env::var_os("MEDUZA_NP_CONTAINER").is_some()
            || Path::new("/run/.containerenv").exists()
            || Path::new("/.dockerenv").exists(),
        arch: std::env::consts::ARCH.to_string(),
        versions: Default::default(),
    }
}

/// Container network mode declared by the deployment (run.sh / Quadlet set `MEDUZA_NP_NETWORK_MODE`); planner cannot
/// tell a bridge namespace from the host's on its own. Only meaningful inside a container.
pub fn network_mode(p: &Platform) -> Option<&'static str> {
    if !p.container {
        return None;
    }
    match std::env::var("MEDUZA_NP_NETWORK_MODE").ok()?.trim() {
        "bridge" => Some("bridge"),
        "host" => Some("host"),
        _ => None,
    }
}

/// Linux capability names by bit number (`include/uapi/linux/capability.h`), without the `CAP_` prefix.
const CAPABILITY_NAMES: [&str; 41] = [
    "CHOWN",
    "DAC_OVERRIDE",
    "DAC_READ_SEARCH",
    "FOWNER",
    "FSETID",
    "KILL",
    "SETGID",
    "SETUID",
    "SETPCAP",
    "LINUX_IMMUTABLE",
    "NET_BIND_SERVICE",
    "NET_BROADCAST",
    "NET_ADMIN",
    "NET_RAW",
    "IPC_LOCK",
    "IPC_OWNER",
    "SYS_MODULE",
    "SYS_RAWIO",
    "SYS_CHROOT",
    "SYS_PTRACE",
    "SYS_PACCT",
    "SYS_ADMIN",
    "SYS_BOOT",
    "SYS_NICE",
    "SYS_RESOURCE",
    "SYS_TIME",
    "SYS_TTY_CONFIG",
    "MKNOD",
    "LEASE",
    "AUDIT_WRITE",
    "AUDIT_CONTROL",
    "SETFCAP",
    "MAC_OVERRIDE",
    "MAC_ADMIN",
    "SYSLOG",
    "WAKE_ALARM",
    "BLOCK_SUSPEND",
    "AUDIT_READ",
    "PERFMON",
    "BPF",
    "CHECKPOINT_RESTORE",
];

/// Effective capabilities of this process inside a container (the deployment picks them per component, §14.3.4; a
/// package the updater installed later may need one the container was not created with). `None` outside containers.
pub fn capabilities(p: &Platform) -> Option<Vec<String>> {
    if !p.container {
        return None;
    }
    effective_capabilities(&std::fs::read_to_string("/proc/self/status").ok()?)
}

/// `CapEff:` of a `/proc/<pid>/status` text → capability names (unknown bits as `CAP_<n>`).
pub fn effective_capabilities(status: &str) -> Option<Vec<String>> {
    let hex = status.lines().find_map(|l| l.strip_prefix("CapEff:"))?.trim();
    let mask = u64::from_str_radix(hex, 16).ok()?;
    Some(
        (0..64)
            .filter(|bit| mask & (1u64 << bit) != 0)
            .map(|bit| CAPABILITY_NAMES.get(bit).map_or_else(|| format!("CAP_{bit}"), |n| n.to_string()))
            .collect(),
    )
}

/// Platform tags matched against a manifest's `platforms` list.
pub fn tags(p: &Platform) -> Vec<&'static str> {
    let mut t = vec!["linux"];
    if p.openwrt {
        t.push("openwrt");
    }
    if p.container {
        t.push("container");
    }
    t
}

/// Finds an executable in PATH (plus sbin dirs that minimal PATHs on OpenWrt/Alpine may omit).
pub fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_executable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let extra = ["/usr/sbin", "/sbin", "/usr/bin", "/bin", "/usr/local/sbin", "/usr/local/bin", "/usr/lib/frr"];
    std::env::split_paths(&path).chain(extra.iter().map(PathBuf::from)).map(|dir| dir.join(name)).find(|p| is_executable(p))
}

#[cfg(unix)]
pub fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
pub fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Random per-process boot id for report ordering (§12.1).
pub fn new_boot_id() -> String {
    let mut b = [0u8; 12];
    let _ = getrandom::getrandom(&mut b);
    format!("boot-{}", hex::encode(b))
}

/// Memory and load figures for the resource summary (Linux /proc; absent elsewhere).
pub fn resources() -> meduza_np_protocol::report::ResourceSummary {
    let mut r = meduza_np_protocol::report::ResourceSummary::default();
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            let mut it = line.split_whitespace();
            match (it.next(), it.next().and_then(|v| v.parse::<u64>().ok())) {
                (Some("MemTotal:"), Some(v)) => r.mem_total_kib = Some(v),
                (Some("MemAvailable:"), Some(v)) => r.mem_available_kib = Some(v),
                _ => {}
            }
        }
    }
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        r.planner_rss_kib =
            status.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse().ok());
    }
    if let Ok(load) = std::fs::read_to_string("/proc/loadavg") {
        r.load1_milli = load.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()).map(|v| (v * 1000.0) as u64);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_capabilities_are_named() {
        // run.sh's planner container with the frr component: NET_ADMIN, NET_RAW, NET_BIND_SERVICE, SYS_ADMIN
        let status = "Name:\tmeduza-np-planner\nCapInh:\t0000000000000000\nCapEff:\t0000000000203400\n";
        assert_eq!(effective_capabilities(status).unwrap(), ["NET_BIND_SERVICE", "NET_ADMIN", "NET_RAW", "SYS_ADMIN"]);
        let all = effective_capabilities("CapEff:\t800001ffffffffff\n").unwrap();
        assert_eq!(all.len(), 42);
        assert_eq!(all[40], "CHECKPOINT_RESTORE");
        assert_eq!(all[41], "CAP_63");
        assert!(effective_capabilities("CapEff:\tzz\n").is_none());
        assert!(effective_capabilities("Name:\tx\n").is_none());
    }
}
