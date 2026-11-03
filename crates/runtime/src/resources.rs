//! Bounded Linux process measurements and filesystem footprints. Never reads process command lines or file contents.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use meduza_np_protocol::report::ProcessResources;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct Process {
    pub pid: u32,
    pub parent: u32,
    pub start_ticks: u64,
    pub rss_bytes: Option<u64>,
    pub executable: Option<PathBuf>,
}

/// Procfs status uses KiB on every Linux architecture; unlike statm it needs no guessed page size.
pub fn process(root: &Path, pid: u32) -> Option<Process> {
    let dir = root.join(pid.to_string());
    let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(") ")?.1.split_whitespace().collect();
    let parent = fields.get(1)?.parse().ok()?;
    let start_ticks = fields.get(19)?.parse().ok()?;
    let rss_bytes = std::fs::read_to_string(dir.join("status")).ok().and_then(|s| {
        s.lines().find_map(|l| l.strip_prefix("VmRSS:").and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok()?.checked_mul(1024)))
    });
    // Reject a process which exited and whose PID was reused between the two procfs reads.
    let again = std::fs::read_to_string(dir.join("stat")).ok()?;
    if again.rsplit_once(") ")?.1.split_whitespace().nth(19)?.parse::<u64>().ok()? != start_ticks {
        return None;
    }
    Some(Process { pid, parent, start_ticks, rss_bytes, executable: std::fs::read_link(dir.join("exe")).ok() })
}

pub fn processes(root: &Path) -> (BTreeMap<u32, Process>, bool) {
    let mut result = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(root) else { return (result, false) };
    let start = Instant::now();
    let mut count = 0;
    for e in entries {
        if start.elapsed() > Duration::from_millis(100) || count >= 4096 {
            return (result, false);
        }
        let Ok(e) = e else { return (result, false) };
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse().ok()) else { continue };
        count += 1;
        match process(root, pid) {
            Some(p) => {
                result.insert(pid, p);
            }
            None if root.join(pid.to_string()).exists() => return (result, false),
            None => {}
        }
    }
    (result, true)
}

/// The closest explicitly owned ancestor wins: nested supervised processes are not counted twice.
pub fn owner<'a, T>(pid: u32, all: &BTreeMap<u32, Process>, owners: &'a BTreeMap<u32, T>) -> Option<&'a T> {
    let mut cursor = pid;
    for _ in 0..64 {
        if let Some(o) = owners.get(&cursor) {
            return Some(o);
        }
        let p = all.get(&cursor)?;
        if p.parent == cursor || p.parent == 0 {
            return None;
        }
        cursor = p.parent;
    }
    None
}

/// Symlinks are counted but never followed (including root paths).
/// A shared visited set deduplicates overlapping roots and hard links. Missing roots occupy zero bytes;
/// permission failures, entry/time limits or I/O failures make the entire measurement unknown.
pub fn disk_bytes(roots: &[PathBuf]) -> Option<u64> {
    let mut pending = roots.to_vec();
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    let start = Instant::now();
    let mut count = 0;
    while let Some(path) = pending.pop() {
        count += 1;
        if count > 50_000 || start.elapsed() > Duration::from_millis(200) {
            return None;
        }
        let m = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        #[cfg(unix)]
        let (key, size) = {
            use std::os::unix::fs::MetadataExt;
            ((m.dev(), m.ino()), m.blocks().checked_mul(512)?)
        };
        #[cfg(not(unix))]
        let (key, size) = (path.clone(), m.len());
        if !seen.insert(key) {
            continue;
        }
        total = total.checked_add(size)?;
        if m.is_dir() {
            for e in std::fs::read_dir(&path).ok()? {
                pending.push(e.ok()?.path());
            }
        }
    }
    Some(total)
}

pub fn unix_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSample {
    pub package: String,
    pub boot_id: String,
    pub observed_unix: u64,
    pub processes: Vec<ProcessResources>,
    pub complete: bool,
}

/// Independent containers need their own /proc reader; publish a tiny sample in the shared runtime volume.
pub fn publish_daemon(run: PathBuf, package: &'static str) {
    std::thread::spawn(move || loop {
        let (all, complete) = processes(Path::new("/proc"));
        let owners = BTreeMap::from([(std::process::id(), ())]);
        let sample = DaemonSample {
            package: package.into(),
            boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default().trim().into(),
            observed_unix: unix_seconds(),
            complete,
            processes: all
                .values()
                .filter(|p| owner(p.pid, &all, &owners).is_some())
                .map(|p| ProcessResources {
                    id: if p.pid == std::process::id() { package.into() } else { format!("{package}/child-{}", p.pid) },
                    instance: None,
                    pid: p.pid,
                    scope: package.trim_start_matches("meduza-np-").into(),
                    memory_rss_bytes: p.rss_bytes,
                })
                .collect(),
        };
        let _ = crate::fsutil::write_json_atomic(&run.join(format!("resources/{package}.json")), &sample, 0o644);
        std::thread::sleep(Duration::from_secs(10));
    });
}

pub fn daemon_sample(run: &Path, package: &str, boot_id: &str, now: u64) -> Option<DaemonSample> {
    let s: DaemonSample = crate::fsutil::read_json(&run.join(format!("resources/{package}.json")), 128 * 1024).ok()??;
    if s.package != package
        || s.boot_id.is_empty()
        || s.boot_id != boot_id
        || s.observed_unix > now.saturating_add(5)
        || now.saturating_sub(s.observed_unix) > 30
        || s.processes.is_empty()
    {
        return None;
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mnp-resources-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn procfs_parses_spaces_in_names_and_rss_without_assuming_a_page_size() {
        let root = dir("proc");
        let pid = root.join("42");
        std::fs::create_dir_all(&pid).unwrap();
        let mut fields = vec!["0"; 22];
        fields[0] = "S";
        fields[1] = "10";
        fields[19] = "9876";
        std::fs::write(pid.join("stat"), format!("42 (name with ) spaces) {}", fields.join(" "))).unwrap();
        std::fs::write(pid.join("status"), "Name: test\nVmRSS:\t123 kB\n").unwrap();
        let p = process(&root, 42).unwrap();
        assert_eq!((p.parent, p.start_ticks, p.rss_bytes), (10, 9876, Some(123 * 1024)));
        std::fs::write(pid.join("status"), "Name: test\n").unwrap();
        assert_eq!(process(&root, 42).unwrap().rss_bytes, None, "missing RSS is never invented as zero");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn descendants_belong_to_the_closest_instance_and_cycles_are_bounded() {
        let all = [(1, 0), (10, 1), (11, 10), (12, 11), (20, 1), (21, 20), (99, 99)]
            .into_iter()
            .map(|(pid, parent)| (pid, Process { pid, parent, start_ticks: 1, rss_bytes: Some(1024), executable: None }))
            .collect();
        let owners = BTreeMap::from([(1, "planner"), (10, "openvpn/lemon"), (20, "openvpn/other")]);
        assert_eq!(owner(12, &all, &owners), Some(&"openvpn/lemon"));
        assert_eq!(owner(21, &all, &owners), Some(&"openvpn/other"));
        assert_eq!(owner(99, &all, &owners), None);
    }

    #[cfg(unix)]
    #[test]
    fn disk_deduplicates_hardlinks_and_overlaps_without_following_symlinks() {
        let root = dir("disk");
        let inside = root.join("inside");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(inside.join("file"), vec![1u8; 8192]).unwrap();
        let before = disk_bytes(std::slice::from_ref(&inside)).unwrap();
        std::fs::hard_link(inside.join("file"), inside.join("alias")).unwrap();
        assert_eq!(disk_bytes(&[inside.clone(), inside.join("file")]), Some(before));
        std::os::unix::fs::symlink(&root, inside.join("loop")).unwrap();
        let after = disk_bytes(std::slice::from_ref(&inside)).unwrap();
        std::fs::write(root.join("outside"), vec![1u8; 1024 * 1024]).unwrap();
        assert_eq!(disk_bytes(&[inside]), Some(after), "symlink escapes are not included");
        assert_eq!(disk_bytes(&[root.join("missing")]), Some(0));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn separate_daemon_samples_expire_and_cannot_cross_boots_or_packages() {
        let run = dir("daemon");
        let package = "meduza-np-updater";
        let sample = DaemonSample {
            package: package.into(),
            boot_id: "boot-a".into(),
            observed_unix: 100,
            complete: true,
            processes: vec![ProcessResources {
                id: "updater".into(),
                instance: None,
                pid: 1,
                scope: "updater".into(),
                memory_rss_bytes: Some(2048),
            }],
        };
        crate::fsutil::write_json_atomic(&run.join(format!("resources/{package}.json")), &sample, 0o644).unwrap();
        assert_eq!(daemon_sample(&run, package, "boot-a", 125).unwrap().processes[0].memory_rss_bytes, Some(2048));
        assert!(daemon_sample(&run, package, "boot-a", 131).is_none());
        assert!(daemon_sample(&run, package, "boot-b", 125).is_none());
        assert!(daemon_sample(&run, package, "boot-a", 94).is_none());
        let mut wrong = sample;
        wrong.package = "meduza-np-dashboard".into();
        crate::fsutil::write_json_atomic(&run.join(format!("resources/{package}.json")), &wrong, 0o644).unwrap();
        assert!(daemon_sample(&run, package, "boot-a", 125).is_none());
        std::fs::remove_dir_all(run).unwrap();
    }
}
