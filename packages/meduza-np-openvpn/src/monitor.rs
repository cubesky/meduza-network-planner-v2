//! Read-only observations independent of the optional native management interface.

use std::io::Read;
use std::path::Path;

fn read_bounded(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path).ok()?.take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

fn owned_process_at(pid_file: &Path, binary: &str, config: &str, proc_root: &Path) -> bool {
    let Some(pid) = read_bounded(pid_file, 32)
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|pid| *pid > 1)
    else {
        return false;
    };
    let Some(command) = read_bounded(&proc_root.join(pid.to_string()).join("cmdline"), 64 * 1024) else { return false };
    let args: Vec<_> = command.split(|b| *b == 0).collect();
    // A reused/stale PID belonging to the host's own VPN or another instance is not ours. Do not
    // infer liveness from a socket file left behind by a terminated daemon.
    args.first().is_some_and(|arg| *arg == binary.as_bytes())
        && args.windows(2).any(|args| args[0] == b"--config" && args[1] == config.as_bytes())
}

pub fn owned_process_running(pid_file: &Path, binary: &str, config: &str) -> bool {
    owned_process_at(pid_file, binary, config, Path::new("/proc"))
}

/// A status file from an older/stopped process must not supply live clients or transfer counters.
pub fn current_status(status_file: &Path, pid_file: &Path, running: bool, now: std::time::SystemTime) -> Option<String> {
    if !running {
        return None;
    }
    let modified = std::fs::metadata(status_file).ok()?.modified().ok()?;
    if now.duration_since(modified).ok()?.as_secs() > 30 {
        return None;
    }
    if std::fs::metadata(pid_file).ok().and_then(|m| m.modified().ok()).is_some_and(|started| modified < started) {
        return None;
    }
    String::from_utf8(read_bounded(status_file, 1024 * 1024)?).ok()
}

/// Single-client/p2p native status uses byte-counter lines instead of a multi-client v3 table.
/// Counters do not prove a current session: persist-tun can keep a device and old traffic on reconnect.
pub fn parse_p2p_counters(text: &str) -> Option<(u64, u64)> {
    let mut rx = None;
    let mut tx = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(',') else { continue };
        match key {
            "TCP/UDP read bytes" => rx = value.trim().parse().ok(),
            "TCP/UDP write bytes" => tx = value.trim().parse().ok(),
            _ => {}
        }
    }
    Some((rx?, tx?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_reused_and_other_instance_pids_are_not_running() {
        let root = std::env::temp_dir().join(format!("mnp-ovpn-observe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let process = root.join("proc/123");
        std::fs::create_dir_all(&process).unwrap();
        let pid = root.join("instance.pid");
        let binary = "/usr/sbin/openvpn";
        let config = "/run/meduza-np/current/openvpn/edenchen/config.ovpn";
        let running = || owned_process_at(&pid, binary, config, &root.join("proc"));
        assert!(!running());
        std::fs::write(&pid, "123\n").unwrap();
        assert!(!running());
        std::fs::write(process.join("cmdline"), format!("{binary}\0--config\0{config}\0")).unwrap();
        assert!(running());
        std::fs::write(process.join("cmdline"), format!("{binary}\0--config\0/etc/openvpn/host.conf\0")).unwrap();
        assert!(!running());
        std::fs::write(process.join("cmdline"), format!("/usr/bin/other\0--config\0{config}\0")).unwrap();
        assert!(!running());
        std::fs::write(process.join("cmdline"), b"").unwrap();
        assert!(!running(), "a zombie is not running");
        for invalid in ["0", "1", "-1", "../123", "123 extra"] {
            std::fs::write(&pid, invalid).unwrap();
            assert!(!running());
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn status_counters_are_bounded_by_process_lifetime_and_freshness() {
        let root = std::env::temp_dir().join(format!("mnp-ovpn-status-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let status = root.join("status");
        let pid = root.join("pid");
        let text = "OpenVPN STATISTICS\nTCP/UDP read bytes,123\nTCP/UDP write bytes,456\nEND\n";
        std::fs::write(&pid, "123").unwrap();
        std::fs::write(&status, text).unwrap();
        let modified = std::fs::metadata(&status).unwrap().modified().unwrap();
        assert_eq!(parse_p2p_counters(text), Some((123, 456)));
        assert_eq!(current_status(&status, &pid, true, modified).as_deref(), Some(text));
        assert!(current_status(&status, &pid, false, modified).is_none());
        assert!(current_status(&status, &pid, true, modified + std::time::Duration::from_secs(31)).is_none());
        let file = std::fs::File::options().write(true).open(&pid).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(modified + std::time::Duration::from_secs(1))).unwrap();
        assert!(current_status(&status, &pid, true, modified).is_none(), "status predating a restart is stale");
        assert!(parse_p2p_counters("TCP/UDP read bytes,invalid\nTCP/UDP write bytes,456").is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
