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
    instances.extend(binary_provider_statuses(&registry));

    let network_mode = crate::platform::network_mode(&p.platform);
    let mut features = registry.features(&enabled, &running, &healthy);
    registry.mark_network_scope(&mut features, network_mode);

    let mut resources = crate::platform::resources();
    resources.supervised_processes = p.engine.supervisor.ids().len() as u32;
    resources.state_disk_free_kib = meduza_np_runtime::fsutil::free_space(&p.paths.state).ok().map(|b| b / 1024);
    let packages = packages(p);
    resources.packages = crate::resources::collect(p, &packages);

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
            recovery_error: st.recovery_error.clone(),
            backoff,
        },
        packages,
        features,
        instances,
        resources: Some(resources),
        maintenance: st.maintenance.clone(),
        update: meduza_np_runtime::update::snapshot(
            &std::env::var_os("MEDUZA_NP_DATA_DIR").map(std::path::PathBuf::from).unwrap_or_else(|| p.paths.state.clone()).join("store"),
            &p.paths.run,
        )
        .map(|mut value| {
            // This planner can relay authenticated Manager update jobs to the independent updater socket.
            value["remoteControl"] = Value::Bool(true);
            value
        }),
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

/// Static providers have no daemon/plugin probe. Report their actual slot resolution as a resource of its own.
fn binary_provider_statuses(registry: &crate::registry::Registry) -> Vec<InstanceStatus> {
    let mut result = Vec::new();
    for package in registry.packages.values().filter(|p| p.manifest.role == PackageRole::BinaryProvider) {
        for declaration in package.manifest.providers.iter().filter(|d| !d.requires_preparation) {
            let resolved = registry.providers.get(&declaration.slot);
            let active = resolved.is_some_and(|r| r.source == package.manifest.package);
            let upstream: Option<Value> =
                std::fs::read(package.dir.join("upstream.lock.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
            let binary_name = declaration.slot.trim_end_matches(".binary");
            let version = upstream
                .as_ref()
                .and_then(|u| u.get("sources"))
                .and_then(Value::as_array)
                .and_then(|sources| sources.iter().find(|s| s.get("name").and_then(Value::as_str) == Some(binary_name)))
                .and_then(|s| s.get("version"))
                .cloned()
                .unwrap_or(Value::Null);
            let reason = package
                .reason
                .clone()
                .or_else(|| registry.provider_errors.get(&declaration.slot).cloned())
                .or_else(|| (!active).then(|| "this package is not the active binary provider".into()));
            result.push(InstanceStatus {
                id: format!("{}/main", package.short()),
                kind: format!("{binary_name}-provider"),
                state: if active && package.registered() { ServiceState::Up } else { ServiceState::Down },
                observed_at: Some(meduza_np_plugin_sdk::now_rfc3339()),
                reason,
                detail: BTreeMap::from([
                    ("slot".into(), Value::String(declaration.slot.clone())),
                    ("packageVersion".into(), Value::String(package.manifest.version.clone())),
                    ("upstreamVersion".into(), version),
                    ("activeSource".into(), resolved.map(|p| Value::String(p.source.clone())).unwrap_or(Value::Null)),
                    ("path".into(), Value::String(declaration.binary.clone())),
                ]),
            });
        }
    }
    result
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
    add_image_base(
        &mut packages,
        Path::new("/usr/lib/meduza-np/image"),
        Path::new("/usr/lib/meduza-np/container/entrypoint.sh"),
        p.platform.container,
    );
    packages
}

/// The running base image is not a store-installed package. Its immutable marker records its own version,
/// independently of the updater baked into it. Older images can still be recognized by their runtime files.
fn add_image_base(packages: &mut Vec<PackageStatus>, image_dir: &Path, entrypoint: &Path, container: bool) {
    if !container
        || packages.iter().any(|p| p.name == "meduza-np-base-container")
        || !image_dir.join("meduza-np-updater").is_file()
        || !entrypoint.is_file()
    {
        return;
    }
    let marker = meduza_np_runtime::fsutil::read_json::<Value>(&image_dir.join("base-container.json"), 64 * 1024).ok().flatten();
    let version = marker
        .as_ref()
        .filter(|m| m.get("package").and_then(Value::as_str) == Some("meduza-np-base-container"))
        .and_then(|m| m.get("version"))
        .and_then(Value::as_str)
        .filter(|v| valid_image_version(v))
        .unwrap_or_default();
    packages.push(PackageStatus {
        name: "meduza-np-base-container".into(),
        version: version.into(),
        role: PackageRole::ContainerBase,
        manifest_digest: String::new(),
        ui_digest: None,
        registration: RegistrationState::Registered,
        reason_code: version.is_empty().then(|| "image-version-unknown".into()),
        reason: version.is_empty().then(|| "The base-container runtime is present; this image does not record its own version.".into()),
        provides: vec!["container.runtime".into()],
    });
    packages.sort_by(|a, b| a.name.cmp(&b.name));
}

fn valid_image_version(value: &str) -> bool {
    // Bounded release labels; never derive the base image version from the embedded updater's version.
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.bytes().all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c))
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
    fn base_image_is_reported_independently_of_the_updater_and_legacy_versions_stay_unknown() {
        let dir = std::env::temp_dir().join(format!("mnp-report-base-image-{}", std::process::id()));
        let image = dir.join("image");
        let entrypoint = dir.join("entrypoint.sh");
        std::fs::create_dir_all(&image).unwrap();
        std::fs::write(image.join("meduza-np-updater"), "embedded updater").unwrap();
        let mut packages = Vec::new();
        add_image_base(&mut packages, &image, &entrypoint, true);
        assert!(packages.is_empty(), "an updater alone does not identify this base image");
        std::fs::write(&entrypoint, "base container entrypoint").unwrap();
        add_image_base(&mut packages, &image, &entrypoint, false);
        assert!(packages.is_empty(), "do not advertise a base image on native hosts");
        add_image_base(&mut packages, &image, &entrypoint, true);
        assert_eq!(packages[0].name, "meduza-np-base-container");
        assert_eq!(packages[0].registration, RegistrationState::Registered);
        assert_eq!(packages[0].version, "", "legacy images are installed but have no known version");
        assert_eq!(packages[0].reason_code.as_deref(), Some("image-version-unknown"));
        let marker = image.join("base-container.json");
        std::fs::write(&marker, r#"{"package":"meduza-np-base-container","version":"0.3.0","updaterVersion":"0.2.0"}"#).unwrap();
        packages.clear();
        add_image_base(&mut packages, &image, &entrypoint, true);
        add_image_base(&mut packages, &image, &entrypoint, true);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].role, PackageRole::ContainerBase);
        assert_eq!(packages[0].version, "0.3.0", "the base image and updater versions can differ");
        assert!(packages[0].reason.is_none());
        for invalid in [
            r#"{"package":"another-image","version":"9.9.9"}"#,
            r#"{"package":"meduza-np-base-container","updaterVersion":"0.2.0"}"#,
            "malformed",
        ] {
            std::fs::write(&marker, invalid).unwrap();
            packages.clear();
            add_image_base(&mut packages, &image, &entrypoint, true);
            assert_eq!(packages[0].version, "", "do not invent the image version from other package metadata");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn static_provider_resources_resolve_without_an_adapter_and_report_real_status() {
        let dir = std::env::temp_dir().join(format!("mnp-report-static-providers-{}", std::process::id()));
        let binary = std::env::current_exe().unwrap();
        for (name, version) in [("mosdns", "v5.3.4"), ("openvpn", "2.7.8")] {
            let package = format!("meduza-np-{name}-pkg");
            let root = dir.join(&package);
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                root.join("manifest.json"),
                serde_json::json!({
                    "manifestVersion":1,"package":package,"version":"0.2.0","role":"binary-provider",
                    "plannerApi":{"min":1,"max":1},"provides":[format!("{name}.binary")],
                    "providers":[{"slot":format!("{name}.binary"),"binary":binary}]
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(root.join("upstream.lock.json"), serde_json::json!({"sources":[{"name":name,"version":version}]}).to_string())
                .unwrap();
        }
        let mut registry = crate::registry::scan(&dir, &meduza_np_plugin_sdk::Platform::default());
        assert_eq!(registry.providers["mosdns.binary"].source, "meduza-np-mosdns-pkg", "standalone resources register their own slots");
        assert_eq!(registry.providers["openvpn.binary"].source, "meduza-np-openvpn-pkg");
        let statuses = binary_provider_statuses(&registry);
        assert_eq!(statuses.len(), 2);
        let mosdns = statuses.iter().find(|s| s.kind == "mosdns-provider").unwrap();
        assert_eq!(mosdns.state, ServiceState::Up);
        assert_eq!(mosdns.detail["packageVersion"], "0.2.0");
        assert_eq!(mosdns.detail["upstreamVersion"], "v5.3.4");
        assert_eq!(mosdns.detail["activeSource"], "meduza-np-mosdns-pkg");
        registry.providers.remove("mosdns.binary");
        registry.provider_errors.insert("mosdns.binary".into(), "packaged binary is missing".into());
        let statuses = binary_provider_statuses(&registry);
        let mosdns = statuses.iter().find(|s| s.kind == "mosdns-provider").unwrap();
        assert_eq!(mosdns.state, ServiceState::Down);
        assert!(mosdns.reason.as_ref().unwrap().contains("missing"));
        assert_eq!(mosdns.detail["activeSource"], Value::Null);
        std::fs::remove_dir_all(dir).unwrap();
    }

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
