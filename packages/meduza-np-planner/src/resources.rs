//! Package accounting: bounded/cached disk walks, fresh procfs RSS, and namespace-local daemon samples.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use meduza_np_protocol::group::InstalledSet;
use meduza_np_protocol::report::{PackageResources, PackageStatus, ProcessResources};
use meduza_np_runtime::resources as measure;

use crate::sync::Planner;

#[derive(Default)]
pub struct Cache {
    key: String,
    at: Option<Instant>,
    disk: BTreeMap<String, (Option<u64>, Option<u64>, String)>,
}

pub fn collect(p: &Arc<Planner>, packages: &[PackageStatus]) -> Vec<PackageResources> {
    let data = std::env::var_os("MEDUZA_NP_DATA_DIR").map(PathBuf::from).unwrap_or_else(|| p.paths.state.clone());
    collect_at(p, packages, &data, Path::new("/proc"), Path::new("/"))
}

fn collect_at(p: &Arc<Planner>, packages: &[PackageStatus], data: &Path, proc_root: &Path, native_root: &Path) -> Vec<PackageResources> {
    let registry = p.registry();
    let store = data.join("store");
    let installed = meduza_np_runtime::fsutil::read_json::<InstalledSet>(&store.join("installed.json"), 4 * 1024 * 1024).ok().flatten();
    let key = format!("{:?}:{:?}:{:?}", packages.iter().map(|p| (&p.name, &p.version)).collect::<Vec<_>>(), installed, registry.providers);
    let now = meduza_np_plugin_sdk::now_rfc3339();
    let mut cache = p.resource_cache.lock().unwrap();
    if cache.key != key || cache.at.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
        cache.disk.clear();
        let start = Instant::now();
        let native = native_files(native_root);
        for pkg in packages {
            let short = meduza_np_protocol::manifest::short_name(&pkg.name);
            let mut roots = vec![p.paths.state.join(short)];
            for provider in
                registry.packages.get(&pkg.name).into_iter().flat_map(|p| &p.manifest.providers).filter(|d| d.requires_preparation)
            {
                let name = provider.slot.trim_end_matches(".binary");
                let dir = p.paths.providers_state().join(name);
                // Include a custom/manual active binary without walking its potentially shared parent directory.
                if let Ok(Some(state)) = meduza_np_runtime::fsutil::read_json::<serde_json::Value>(&dir.join("provider.json"), 64 * 1024) {
                    if let Some(path) = state.get("path").and_then(|p| p.as_str()) {
                        let binary = PathBuf::from(path);
                        roots.push(binary.clone());
                        if let Some(parent) = binary.parent() {
                            roots.push(parent.join("previous"));
                            roots.push(parent.join(".staging"));
                        }
                    }
                }
                roots.push(dir);
            }
            let (disk, data) = if start.elapsed() > Duration::from_millis(500) || pkg.name == "meduza-np-base-container" {
                (None, None)
            } else {
                let disk = if installed
                    .as_ref()
                    .is_some_and(|s| s.backend == "store" && s.packages.get(&pkg.name).is_some_and(|p| p.object.is_some()))
                {
                    // All retained versions belong to this package; views are symlinks and are not recounted.
                    let root = store.join("objects").join(&pkg.name);
                    root.is_dir().then(|| measure::disk_bytes(&[root])).flatten()
                } else if let Some(files) = native.get(&pkg.name) {
                    measure::disk_bytes(files)
                } else if pkg.name == "meduza-np-updater" && Path::new("/usr/lib/meduza-np/image/meduza-np-updater").is_file() {
                    measure::disk_bytes(&[PathBuf::from("/usr/lib/meduza-np/image/meduza-np-updater")])
                } else {
                    None
                };
                (disk, measure::disk_bytes(&roots))
            };
            cache.disk.insert(pkg.name.clone(), (disk, data, now.clone()));
        }
        cache.key = key;
        cache.at = Some(Instant::now());
    }
    let mut result: BTreeMap<_, _> = packages
        .iter()
        .map(|pkg| {
            let (disk, data, observed) = cache.disk.get(&pkg.name).cloned().unwrap_or_default();
            (
                pkg.name.clone(),
                PackageResources {
                    package: pkg.name.clone(),
                    disk_bytes: disk,
                    data_bytes: data,
                    disk_observed_at: observed,
                    memory_observed_at: now.clone(),
                    ..Default::default()
                },
            )
        })
        .collect();
    drop(cache);
    let (all, complete) = measure::processes(proc_root);
    // pid -> (package, process ID, owning instance). Prefer precise executable/instance attribution over planner ancestry.
    let mut owners: BTreeMap<u32, (String, String, Option<String>)> = BTreeMap::new();
    owners.insert(std::process::id(), ("meduza-np-planner".into(), "planner".into(), None));
    let mut executables: BTreeMap<PathBuf, String> = BTreeMap::new();
    for pkg in registry.packages.values() {
        let mut paths: Vec<PathBuf> = pkg.manifest.entrypoint.iter().map(PathBuf::from).collect();
        if matches!(pkg.manifest.package.as_str(), "meduza-np-planner" | "meduza-np-updater" | "meduza-np-dashboard") {
            paths.push(PathBuf::from("/usr/bin").join(&pkg.manifest.package));
        } else if pkg.manifest.package == "meduza-np-cli" {
            paths.push(PathBuf::from("/usr/bin/meduza-np"));
        }
        for path in paths {
            if let Ok(real) = std::fs::canonicalize(path) {
                executables.insert(real, pkg.manifest.package.clone());
            }
        }
    }
    for proc in all.values() {
        if let Some(package) = proc.executable.as_ref().and_then(|e| executables.get(e)) {
            owners.insert(proc.pid, (package.clone(), package.clone(), None));
        }
    }
    let applied = crate::state::AppliedState::load(&p.paths).unwrap_or_default();
    let instance_owners: BTreeMap<_, _> =
        applied.instances.iter().flat_map(|(inst, ai)| ai.processes.keys().map(move |id| (id, (inst, &ai.package)))).collect();
    for s in p.engine.supervisor.statuses() {
        let Some(pid) = s.pid else { continue };
        if let Some((instance, package)) = instance_owners.get(&s.id) {
            owners.insert(pid, ((*package).clone(), s.id, Some((*instance).clone())));
        } else {
            // A new process can be observed before applied.json commits. Its generated ID carries the package.
            let package = format!("meduza-np-{}", s.id.split('/').next().unwrap_or_default());
            if result.contains_key(&package) {
                owners.insert(pid, (package, s.id, None));
            }
        }
    }
    for proc in all.values() {
        if let Some((package, id, instance)) = measure::owner(proc.pid, &all, &owners) {
            if let Some(r) = result.get_mut(package) {
                r.processes.push(ProcessResources {
                    id: if owners.contains_key(&proc.pid) { id.clone() } else { format!("{id}/child-{}", proc.pid) },
                    instance: instance.clone(),
                    pid: proc.pid,
                    scope: "planner".into(),
                    memory_rss_bytes: proc.rss_bytes,
                });
            }
        }
    }
    let boot = std::fs::read_to_string(proc_root.join("sys/kernel/random/boot_id")).unwrap_or_default();
    for r in result.values_mut() {
        let separate = matches!(r.package.as_str(), "meduza-np-updater" | "meduza-np-dashboard");
        let mut measured = complete && owners.iter().filter(|(_, o)| o.0 == r.package).all(|(pid, _)| all.contains_key(pid));
        if separate && r.processes.is_empty() {
            if let Some(sample) = measure::daemon_sample(&p.paths.run, &r.package, boot.trim(), measure::unix_seconds()) {
                measured = sample.complete;
                r.memory_observed_at = meduza_np_plugin_sdk::unix_to_rfc3339(sample.observed_unix);
                r.processes = sample.processes;
            } else {
                measured = false;
            }
        }
        r.memory_rss_bytes = if measured { r.processes.iter().try_fold(0u64, |n, p| n.checked_add(p.memory_rss_bytes?)) } else { None };
        if r.package == "meduza-np-base-container" {
            r.memory_rss_bytes = None;
            r.reason = Some("container-image-shared".into());
        } else if r.disk_bytes.is_none() || r.data_bytes.is_none() || r.memory_rss_bytes.is_none() {
            r.reason = Some("measurement-unavailable".into());
        }
    }
    result.into_values().collect()
}

/// Use native package ownership inventories, never recursively walk shared /usr or /etc directories.
fn native_files(root: &Path) -> BTreeMap<String, Vec<PathBuf>> {
    let mut result = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(root.join("usr/lib/opkg/info")) {
        for e in entries.flatten().take(4096) {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(package) = name.strip_suffix(".list").filter(|s| meduza_np_protocol::manifest::ALL_PACKAGES.contains(s)) else {
                continue;
            };
            if let Some(text) = inventory_text(&e.path()) {
                if text.lines().count() > 50_000 {
                    continue;
                }
                let files = text.lines().map(|p| root.join(p.trim_start_matches('/'))).filter(|p| !p.is_dir()).collect();
                result.insert(package.into(), files);
            }
        }
    }
    if let Some(text) = inventory_text(&root.join("lib/apk/db/installed")) {
        for record in text.split("\n\n") {
            let Some(package) =
                record.lines().find_map(|l| l.strip_prefix("P:")).filter(|s| meduza_np_protocol::manifest::ALL_PACKAGES.contains(s))
            else {
                continue;
            };
            let mut dir = root.to_path_buf();
            let mut files = Vec::new();
            for line in record.lines() {
                if let Some(folder) = line.strip_prefix("F:") {
                    dir = root.join(folder.trim_start_matches('/'));
                }
                if let Some(file) = line.strip_prefix("R:") {
                    files.push(dir.join(file));
                }
            }
            if files.len() <= 50_000 {
                result.insert(package.into(), files);
            }
        }
    }
    result
}

fn inventory_text(path: &Path) -> Option<String> {
    let mut text = String::new();
    std::fs::File::open(path).ok()?.take(8 * 1024 * 1024 + 1).read_to_string(&mut text).ok()?;
    (text.len() <= 8 * 1024 * 1024).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_runtime::paths::NodePaths;
    use serde_json::json;

    fn planner(root: &Path) -> Arc<Planner> {
        let paths = NodePaths::under(root);
        let engine = crate::apply::Engine {
            paths: paths.clone(),
            runner: crate::plugins::PluginRunner::new(1, Duration::from_secs(1)),
            supervisor: crate::supervisor::Supervisor::new(),
            net: Arc::new(crate::netbackend::RecordingBackend::default()),
            keystore: crate::keystore::KeyStore::new(&paths.etc.join("keys")),
            settle: Duration::ZERO,
            settle_deadline: Duration::from_secs(1),
            loaded: Default::default(),
        };
        Planner::new(paths, Default::default(), engine, Default::default())
    }

    fn packages() -> Vec<PackageStatus> {
        ["openvpn", "updater", "openvpn-pkg", "base-container"]
            .into_iter()
            .map(|name| {
                serde_json::from_value(json!({
                    "name": format!("meduza-np-{name}"), "version":"1.0.0", "role":"service-adapter",
                    "manifestDigest":"", "registration":"registered"
                }))
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn store_retained_files_and_daemon_memory_are_reported_without_double_counting_or_stale_samples() {
        let root = std::env::temp_dir().join(format!("mnp-package-usage-{}", std::process::id()));
        let p = planner(&root);
        let object = p.paths.state.join("store/objects/meduza-np-openvpn/version-a");
        std::fs::create_dir_all(&object).unwrap();
        std::fs::write(object.join("adapter"), vec![1u8; 8192]).unwrap();
        let installed: InstalledSet =
            serde_json::from_value(json!({"formatVersion":1,"generation":1,"backend":"store","platform":"linux/amd64",
            "activatedAt":"", "packages":{"meduza-np-openvpn":{"version":"1.0.0","source":"group:test","object":"version-a"}}}))
            .unwrap();
        meduza_np_runtime::fsutil::write_json_atomic(&p.paths.state.join("store/installed.json"), &installed, 0o644).unwrap();
        let proc = root.join("proc");
        std::fs::create_dir_all(proc.join("sys/kernel/random")).unwrap();
        std::fs::write(proc.join("sys/kernel/random/boot_id"), "boot-test\n").unwrap();
        let updater = measure::DaemonSample {
            package: "meduza-np-updater".into(),
            boot_id: "boot-test".into(),
            observed_unix: measure::unix_seconds(),
            complete: true,
            processes: vec![ProcessResources {
                id: "updater".into(),
                instance: None,
                pid: 1,
                scope: "updater".into(),
                memory_rss_bytes: Some(8192),
            }],
        };
        let sample = p.paths.run.join("resources/meduza-np-updater.json");
        meduza_np_runtime::fsutil::write_json_atomic(&sample, &updater, 0o644).unwrap();
        let rows = collect_at(&p, &packages(), &p.paths.state, &proc, &root);
        let get = |name: &str| rows.iter().find(|r| r.package == name).unwrap();
        let initial_disk = get("meduza-np-openvpn").disk_bytes.unwrap();
        assert!(initial_disk > 0);
        assert_eq!(get("meduza-np-openvpn").memory_rss_bytes, Some(0));
        assert_eq!(get("meduza-np-openvpn-pkg").memory_rss_bytes, Some(0), "a provider owns disk, not an extra copy of the service RSS");
        assert_eq!(get("meduza-np-updater").memory_rss_bytes, Some(8192));
        assert_eq!(get("meduza-np-updater").processes[0].scope, "updater");
        assert_eq!(get("meduza-np-base-container").disk_bytes, None, "do not attribute the entire rootfs to the base image");
        std::fs::write(object.join("retained"), vec![1u8; 8192]).unwrap();
        assert_eq!(collect_at(&p, &packages(), &p.paths.state, &proc, &root)[0].disk_bytes, rows[0].disk_bytes, "disk walks are cached");
        p.resource_cache.lock().unwrap().at = None;
        let mut expired = updater;
        expired.observed_unix = expired.observed_unix.saturating_sub(31);
        meduza_np_runtime::fsutil::write_json_atomic(&sample, &expired, 0o644).unwrap();
        let rows = collect_at(&p, &packages(), &p.paths.state, &proc, &root);
        assert!(rows.iter().find(|r| r.package == "meduza-np-openvpn").unwrap().disk_bytes.unwrap() > initial_disk);
        assert_eq!(rows.iter().find(|r| r.package == "meduza-np-updater").unwrap().memory_rss_bytes, None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_inventories_count_owned_files_without_walking_shared_system_directories() {
        let root = std::env::temp_dir().join(format!("mnp-native-usage-{}", std::process::id()));
        for dir in ["usr/lib/opkg/info", "usr/bin", "lib/apk/db"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("usr/bin/tool"), "binary").unwrap();
        std::fs::write(root.join("usr/lib/opkg/info/meduza-np-openvpn.list"), "/usr\n/usr/bin\n/usr/bin/tool\n").unwrap();
        std::fs::write(root.join("lib/apk/db/installed"), "P:meduza-np-mosdns\nF:usr/bin\nR:tool\n\nP:unrelated\nF:usr\nR:other\n")
            .unwrap();
        let inventories = native_files(&root);
        assert_eq!(inventories.len(), 2);
        assert_eq!(inventories["meduza-np-openvpn"], vec![root.join("usr/bin/tool")]);
        assert_eq!(inventories["meduza-np-mosdns"], vec![root.join("usr/bin/tool")]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn live_external_processes_are_measured_under_their_own_instances() {
        let root = std::env::temp_dir().join(format!("mnp-instance-rss-{}", std::process::id()));
        let p = planner(&root);
        let mut applied = crate::state::AppliedState::default();
        for name in ["lemon", "second"] {
            let id = format!("openvpn/{name}");
            p.engine
                .supervisor
                .ensure(crate::supervisor::LaunchSpec {
                    id: id.clone(),
                    program: "/bin/sleep".into(),
                    args: vec!["30".into()],
                    env: Default::default(),
                    restart: meduza_np_protocol::plan::RestartPolicy::Never,
                    startup: Default::default(),
                    stop_timeout: Duration::from_secs(1),
                    restart_digest: "resources".into(),
                    bound_to: None,
                })
                .await;
            assert!(p.engine.supervisor.wait_settled(&id, Duration::from_millis(50), Duration::from_secs(2)).await);
            applied.instances.insert(
                id.clone(),
                crate::state::AppliedInstance {
                    package: "meduza-np-openvpn".into(),
                    processes: BTreeMap::from([(id, "resources".into())]),
                    ..Default::default()
                },
            );
        }
        applied.save(&p.paths).unwrap();
        let rows = collect_at(&p, &packages(), &p.paths.state, Path::new("/proc"), &root);
        p.engine.supervisor.stop_all().await;
        let ovpn = rows.iter().find(|r| r.package == "meduza-np-openvpn").unwrap();
        assert_eq!(ovpn.processes.len(), 2);
        assert_eq!(
            ovpn.processes.iter().filter_map(|p| p.instance.as_deref()).collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from(["openvpn/lemon", "openvpn/second"])
        );
        assert!(ovpn.processes.iter().all(|p| p.memory_rss_bytes.is_some_and(|n| n > 0)));
        assert_eq!(ovpn.memory_rss_bytes, Some(ovpn.processes.iter().map(|p| p.memory_rss_bytes.unwrap()).sum()));
        assert_eq!(rows.iter().find(|r| r.package == "meduza-np-openvpn-pkg").unwrap().memory_rss_bytes, Some(0));
        std::fs::remove_dir_all(root).unwrap();
    }
}
