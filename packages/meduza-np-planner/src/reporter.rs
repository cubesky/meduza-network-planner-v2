//! Builds the node report (§12.1, §12.2): sync state with backoff, packages, features derived from installed
//! packages only, instances (processes + plugin probes), resources and maintenance.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use meduza_np_protocol::group::InstalledSet;
use meduza_np_protocol::manifest::PackageRole;
use meduza_np_protocol::report::{InstanceStatus, NodeReport, PackageStatus, RegistrationState, ServiceState, SyncStatus, SystemInfo};
use meduza_np_protocol::REPORT_VERSION;
use serde_json::Value;

use crate::supervisor::ProcState;
use crate::sync::Planner;

pub fn build(p: &Arc<Planner>) -> NodeReport {
    let registry = p.registry();
    let st = p.status.lock().unwrap().clone();
    let backoff = p.client().map(|c| c.gate.lock().unwrap().status(Instant::now())).unwrap_or_default();
    let node_id = p.client().map(|c| c.node_id().to_string()).unwrap_or_default();
    let (running, healthy) = p.running_packages();
    let enabled = p.enabled_services();
    // Owning instance of each supervised process: the console restarts a process through it.
    let owner: BTreeMap<String, String> = crate::state::AppliedState::load(&p.paths)
        .map(|a| a.instances.iter().flat_map(|(inst, ai)| ai.processes.keys().map(move |proc| (proc.clone(), inst.clone()))).collect())
        .unwrap_or_default();

    let mut instances: Vec<InstanceStatus> = p
        .engine
        .supervisor
        .statuses()
        .into_iter()
        .map(|s| {
            let mut detail = BTreeMap::new();
            detail.insert("restarts".into(), Value::from(s.restarts));
            if let Some(pid) = s.pid {
                detail.insert("pid".into(), Value::from(pid));
            }
            if let Some(inst) = owner.get(&s.id) {
                detail.insert("instance".into(), Value::from(inst.clone()));
            }
            InstanceStatus {
                id: format!("{}/process", s.id),
                kind: "process".into(),
                state: match s.state {
                    ProcState::Running => ServiceState::Up,
                    ProcState::Starting => ServiceState::Starting,
                    ProcState::Backoff => ServiceState::Degraded,
                    ProcState::Failed | ProcState::Exited => ServiceState::Down,
                    ProcState::Stopped => ServiceState::Disabled,
                },
                observed_at: Some(meduza_np_plugin_sdk::now_rfc3339()),
                reason: s.last_exit,
                detail,
            }
        })
        .collect();
    instances.extend(st.probes.iter().cloned());

    let network_mode = crate::platform::network_mode(&p.platform);
    let mut features = registry.features(&enabled, &running, &healthy);
    registry.mark_network_scope(&mut features, network_mode);

    let mut resources = crate::platform::resources();
    resources.supervised_processes = p.engine.supervisor.ids().len() as u32;
    resources.state_disk_free_kib = meduza_np_runtime::fsutil::free_space(&p.paths.state).ok().map(|b| b / 1024);

    NodeReport {
        report_version: REPORT_VERSION,
        node_id,
        boot_id: p.boot_id.clone(),
        sequence: p.next_sequence(),
        observed_at: meduza_np_plugin_sdk::now_rfc3339(),
        sync: SyncStatus {
            mode: st.mode,
            transport: st.transport,
            behavior: st.behavior,
            state: st.state,
            revisions: st.pointers.clone(),
            degraded_reason: st.degraded_reason.clone(),
            event_cursor: st.event_cursor.clone(),
            apply_stage: st.apply_stage.clone(),
            last_error: st.last_error.clone(),
            backoff,
        },
        packages: packages(p),
        features,
        instances,
        resources: Some(resources),
        maintenance: st.maintenance.clone(),
        update: None,
        system: Some(SystemInfo {
            arch: std::env::consts::ARCH.into(),
            os: std::env::consts::OS.into(),
            os_release: std::fs::read_to_string("/etc/os-release").ok().and_then(|t| {
                t.lines()
                    .find(|l| l.starts_with("PRETTY_NAME="))
                    .map(|l| l.trim_start_matches("PRETTY_NAME=").trim_matches('"').to_string())
            }),
            kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease").ok().map(|s| s.trim().to_string()),
            openwrt: p.platform.openwrt,
            container: p.platform.container,
            network_mode: network_mode.map(String::from),
            capabilities: crate::platform::capabilities(&p.platform),
        }),
    }
}

pub fn packages(p: &Arc<Planner>) -> Vec<PackageStatus> {
    let mut packages = p.registry().package_statuses();
    let data_dir = std::env::var_os("MEDUZA_NP_DATA_DIR").map(std::path::PathBuf::from).unwrap_or_else(|| p.paths.state.clone());
    add_image_updater(
        &mut packages,
        &data_dir.join("store/installed.json"),
        Path::new("/usr/lib/meduza-np/image/meduza-np-updater"),
        std::env::var("MEDUZA_NP_IMAGE_UPDATER_VERSION").ok().as_deref(),
    );
    packages
}

// The base image's updater has no store object/manifest. Include its verified installation record so the
// manager does not mistake this container for a node without an updater and ask for manual installation.
fn add_image_updater(packages: &mut Vec<PackageStatus>, installed_path: &Path, binary: &Path, image_version: Option<&str>) {
    if packages.iter().any(|p| p.name == "meduza-np-updater") || !binary.is_file() {
        return;
    }
    let Some(image_version) = image_version.filter(|v| !v.is_empty()) else { return };
    let Ok(Some(installed)) = meduza_np_runtime::fsutil::read_json::<InstalledSet>(installed_path, 4 * 1024 * 1024) else { return };
    let Some(updater) = installed.packages.get("meduza-np-updater") else { return };
    if installed.backend != "store" || updater.source != "image" || updater.version != image_version {
        return;
    }
    packages.push(PackageStatus {
        name: "meduza-np-updater".into(),
        version: updater.version.clone(),
        role: PackageRole::Updater,
        manifest_digest: String::new(),
        ui_digest: None,
        registration: RegistrationState::Registered,
        reason_code: None,
        reason: None,
        provides: vec!["updater.auto-update".into()],
    });
    packages.sort_by(|a, b| a.name.cmp(&b.name));
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_protocol::group::InstalledPackage;

    #[test]
    fn image_updater_is_reported_only_when_installed_and_present() {
        let dir = std::env::temp_dir().join(format!("mnp-report-image-updater-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let record = dir.join("installed.json");
        let binary = dir.join("meduza-np-updater");
        std::fs::write(&binary, b"image updater").unwrap();
        let installed = InstalledSet {
            format_version: 1,
            backend: "store".into(),
            packages: BTreeMap::from([(
                "meduza-np-updater".into(),
                InstalledPackage { version: "0.0.8".into(), sha256: String::new(), source: "image".into(), object: None },
            )]),
            ..Default::default()
        };
        meduza_np_runtime::fsutil::write_json_atomic(&record, &installed, 0o600).unwrap();
        let mut packages = Vec::new();
        add_image_updater(&mut packages, &record, &binary, None);
        add_image_updater(&mut packages, &record, &binary, Some("0.0.7"));
        add_image_updater(&mut packages, &record, &dir.join("missing"), Some("0.0.8"));
        assert!(packages.is_empty(), "do not advertise absent or mismatched image installations");
        add_image_updater(&mut packages, &record, &binary, Some("0.0.8"));
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "meduza-np-updater");
        assert_eq!(packages[0].version, "0.0.8");
        assert_eq!(packages[0].registration, RegistrationState::Registered);
        packages[0].version = "0.0.9".into();
        add_image_updater(&mut packages, &record, &binary, Some("0.0.8"));
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version, "0.0.9", "a store-installed updater takes precedence over the image baseline");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
