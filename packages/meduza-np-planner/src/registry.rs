//! Package registry (AGENT.md §3.1, §3.2, §3.4, §5.1).
//!
//! Scans `/usr/share/meduza-np/packages/*/manifest.json`. Only packages that are installed, compatible, free of
//! conflicts and with their hard requirements registered become registered. Provider slots resolve to a
//! registered `-pkg` provider first, then to the system binary.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use meduza_np_plugin_sdk::{Platform, ResolvedProvider};
use meduza_np_protocol::digest::canonical_digest;
use meduza_np_protocol::manifest::{packages_conflict, short_name, PackageManifest, PackageRole, SystemDependencyKind};
use meduza_np_protocol::report::{FeatureStatus, PackageStatus, RegistrationState};
use meduza_np_protocol::ui::UiDescriptor;
use meduza_np_protocol::PLANNER_API_VERSION;
use serde::Deserialize;
use serde_json::Value;

/// Service key of planner's own features in the enabled-services map: the node's `networks` (mappings, port
/// forwards, health ports) switch them through the planner manifest's `capabilitySwitches`.
pub const PLANNER_SERVICE: &str = "planner";

use crate::platform;

#[derive(Debug, Clone)]
pub struct Package {
    pub manifest: PackageManifest,
    pub dir: PathBuf,
    pub manifest_digest: String,
    pub ui_digest: Option<String>,
    pub state: RegistrationState,
    pub reason_code: Option<String>,
    pub reason: Option<String>,
}

impl Package {
    pub fn registered(&self) -> bool {
        self.state == RegistrationState::Registered
    }
    pub fn short(&self) -> &str {
        short_name(&self.manifest.package)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Registry {
    pub packages: BTreeMap<String, Package>,
    pub providers: BTreeMap<String, ResolvedProvider>,
    /// Provider problems per slot (pkg installed but binary unusable) — shown, never silently replaced.
    pub provider_errors: BTreeMap<String, String>,
}

/// State file written by preparation tools of download/manual providers (mihomo-pkg, §3.3.2).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedProvider {
    pub state: String,
    pub path: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

pub fn scan(packages_dir: &Path, plat: &Platform) -> Registry {
    let mut packages: BTreeMap<String, Package> = BTreeMap::new();
    let entries = std::fs::read_dir(packages_dir).map(|it| it.filter_map(Result::ok).collect::<Vec<_>>()).unwrap_or_default();
    for entry in entries {
        let dir = entry.path();
        let manifest_path = dir.join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        let parsed: Result<PackageManifest, String> = std::fs::read_to_string(&manifest_path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()));
        let manifest = match parsed {
            Ok(m) => m,
            Err(e) => {
                let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                warn!("package {name}: unreadable manifest: {e}");
                continue;
            }
        };
        let digest = canonical_digest(&manifest).unwrap_or_default();
        let mut pkg = Package {
            manifest,
            dir,
            manifest_digest: digest,
            ui_digest: None,
            state: RegistrationState::Registered,
            reason_code: None,
            reason: None,
        };
        check_self(&mut pkg, plat);
        packages.insert(pkg.manifest.package.clone(), pkg);
    }

    // Conflicts: both sides are refused; an explicit ordered switch is needed (§3.2).
    let names: Vec<String> = packages.keys().cloned().collect();
    for a in &names {
        for b in &names {
            if a < b
                && (packages_conflict(a, b) || packages[a].manifest.conflicts.contains(b) || packages[b].manifest.conflicts.contains(a))
            {
                for (x, y) in [(a, b), (b, a)] {
                    let p = packages.get_mut(x).unwrap();
                    if p.registered() {
                        fail(p, RegistrationState::Conflict, "conflict", format!("conflicts with installed {y}"));
                    }
                }
            }
        }
    }

    // Requirements, iterated to a fixed point (a failed dependency fails its dependents).
    loop {
        let registered: BTreeSet<String> = packages.values().filter(|p| p.registered()).map(|p| p.manifest.package.clone()).collect();
        let mut changed = false;
        for p in packages.values_mut().filter(|p| p.registered()) {
            if let Some(missing) = p.manifest.requires.iter().find(|r| !registered.contains(*r)) {
                let msg = format!("requires {missing}, which is not installed or not registered");
                fail(p, RegistrationState::MissingDependency, "missing-dependency", msg);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut reg = Registry { packages, providers: BTreeMap::new(), provider_errors: BTreeMap::new() };
    reg.resolve_providers();
    reg
}

fn fail(p: &mut Package, state: RegistrationState, code: &str, reason: String) {
    p.state = state;
    p.reason_code = Some(code.into());
    p.reason = Some(reason);
}

fn check_self(p: &mut Package, plat: &Platform) {
    let m = &p.manifest;
    let v = m.validate();
    if !v.is_empty() {
        let msg = v.errors().iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; ");
        return fail(p, RegistrationState::Invalid, "invalid-manifest", msg);
    }
    if !m.planner_api.contains(PLANNER_API_VERSION) {
        let msg = format!("needs planner API {}-{}, this planner speaks {PLANNER_API_VERSION}", m.planner_api.min, m.planner_api.max);
        return fail(p, RegistrationState::Incompatible, "incompatible-api", msg);
    }
    if matches!(m.role, PackageRole::Manager) {
        return fail(p, RegistrationState::Incompatible, "wrong-host", "the manager is not a node package".into());
    }
    let tags = platform::tags(plat);
    if !m.platforms.is_empty() && !m.platforms.iter().any(|t| tags.contains(&t.as_str())) {
        let msg = format!("only for {:?}", m.platforms);
        return fail(p, RegistrationState::Incompatible, "wrong-platform", msg);
    }
    if m.role.is_plugin() {
        if let Some(ep) = &m.entrypoint {
            let local = p.dir.join("bin").join(format!("{}{}", short_name(&m.package), std::env::consts::EXE_SUFFIX));
            if !platform::is_executable(Path::new(ep)) && !platform::is_executable(&local) {
                return fail(p, RegistrationState::Invalid, "entrypoint-missing", format!("plugin executable {ep} not found"));
            }
        }
    }
    if let Some(ui) = &m.ui {
        let path = p.dir.join(&ui.descriptor);
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<UiDescriptor>(&t).map_err(|e| e.to_string()))
        {
            Ok(d) => {
                let errors = d.validate();
                if !errors.is_empty() {
                    return fail(p, RegistrationState::Invalid, "invalid-ui", format!("{:?}", errors.errors()));
                }
                p.ui_digest = canonical_digest(&d).ok();
            }
            Err(e) => fail(p, RegistrationState::Invalid, "invalid-ui", format!("{}: {e}", path.display())),
        }
    }
}

impl Registry {
    fn resolve_providers(&mut self) {
        let mut slots: BTreeSet<String> = BTreeSet::new();
        for p in self.packages.values().filter(|p| p.registered()) {
            slots.extend(p.manifest.providers.iter().map(|d| d.slot.clone()));
            if let Some(s) = &p.manifest.provider_slot {
                slots.insert(s.clone());
            }
            for d in &p.manifest.system_dependencies {
                if let Some(s) = &d.provider_slot {
                    slots.insert(s.clone());
                }
            }
        }
        for slot in slots {
            // 1. registered project provider
            let mut pkg_problem = None;
            for p in self.packages.values().filter(|p| p.registered() && p.manifest.role == PackageRole::BinaryProvider) {
                for decl in p.manifest.providers.iter().filter(|d| d.slot == slot) {
                    match resolve_decl(decl) {
                        Ok(mut rp) => {
                            if rp.source.is_empty() {
                                rp.source = p.manifest.package.clone();
                            }
                            self.providers.insert(slot.clone(), rp);
                        }
                        Err(e) => pkg_problem = Some(format!("{}: {e}", p.manifest.package)),
                    }
                }
            }
            if self.providers.contains_key(&slot) {
                continue;
            }
            if let Some(problem) = pkg_problem {
                // A project provider is installed but not ready: report it and do NOT silently fall back (§3.1).
                self.provider_errors.insert(slot.clone(), problem);
                continue;
            }
            // 2. system binary named after the slot's dependency
            let binary = self
                .packages
                .values()
                .flat_map(|p| p.manifest.system_dependencies.iter())
                .find(|d| d.provider_slot.as_deref() == Some(slot.as_str()) && d.kind == SystemDependencyKind::Binary)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| slot.split('.').next().unwrap_or(&slot).to_string());
            match platform::which(&binary) {
                Some(path) => {
                    self.providers.insert(
                        slot.clone(),
                        ResolvedProvider { path: path.to_string_lossy().into_owned(), source: "system".into(), version: None },
                    );
                }
                None => {
                    self.provider_errors
                        .insert(slot.clone(), format!("system binary {binary} not found and no provider package registered"));
                }
            }
        }
    }

    pub fn get(&self, package: &str) -> Option<&Package> {
        self.packages.get(package)
    }

    pub fn by_short(&self, short: &str) -> Option<&Package> {
        self.packages.get(&format!("{}{short}", meduza_np_protocol::manifest::PACKAGE_PREFIX))
    }

    pub fn registered(&self) -> impl Iterator<Item = &Package> {
        self.packages.values().filter(|p| p.registered())
    }

    pub fn capabilities(&self) -> Vec<String> {
        self.registered().flat_map(|p| p.manifest.provides.iter().cloned()).collect()
    }

    /// Why a registered package cannot provide its features right now (missing system dependency/provider).
    pub fn availability(&self, p: &Package, enabled_caps: &BTreeSet<String>) -> Result<(), (String, String)> {
        if let Some(slot) = &p.manifest.provider_slot {
            if !self.providers.contains_key(slot) {
                let why = self.provider_errors.get(slot).cloned().unwrap_or_else(|| format!("no provider for {slot}"));
                return Err(("provider-missing".into(), why));
            }
        }
        for dep in &p.manifest.system_dependencies {
            if let Some(when) = &dep.when {
                if !enabled_caps.contains(when) {
                    continue;
                }
            }
            let ok = match dep.kind {
                SystemDependencyKind::Binary => dep
                    .provider_slot
                    .as_ref()
                    .map(|s| self.providers.contains_key(s))
                    .unwrap_or_else(|| platform::which(&dep.name).is_some()),
                SystemDependencyKind::KernelModule => kernel_module_available(&dep.name),
                SystemDependencyKind::KernelFeature => kernel_feature_available(&dep.name),
                SystemDependencyKind::Service => service_available(&dep.name),
            };
            if !ok {
                return Err(("system-dependency-missing".into(), format!("{:?} {} is not available", dep.kind, dep.name)));
            }
        }
        Ok(())
    }

    /// Whether the package's missing provider is one a registered provider package prepares at runtime (download or
    /// manual file, e.g. mihomo-pkg): the feature then waits for it instead of making the revision invalid (D34).
    pub fn provider_preparing(&self, p: &Package) -> bool {
        let Some(slot) = &p.manifest.provider_slot else { return false };
        !self.providers.contains_key(slot)
            && self.registered().any(|q| {
                q.manifest.role == PackageRole::BinaryProvider
                    && q.manifest.providers.iter().any(|d| &d.slot == slot && d.requires_preparation)
            })
    }

    pub fn package_statuses(&self) -> Vec<PackageStatus> {
        self.packages
            .values()
            .map(|p| PackageStatus {
                name: p.manifest.package.clone(),
                version: p.manifest.version.clone(),
                role: p.manifest.role,
                manifest_digest: p.manifest_digest.clone(),
                ui_digest: p.ui_digest.clone(),
                registration: p.state,
                reason_code: p.reason_code.clone(),
                reason: p.reason.clone(),
                provides: if p.registered() { p.manifest.provides.clone() } else { Vec::new() },
            })
            .collect()
    }

    /// Feature list derived only from installed packages (§12.1); `enabled_services` are the configurations of
    /// the services the current configuration enables, by service key (planner's own under [`PLANNER_SERVICE`]).
    pub fn features(
        &self,
        enabled_services: &BTreeMap<String, Value>,
        running: &BTreeSet<String>,
        healthy: &BTreeSet<String>,
    ) -> Vec<FeatureStatus> {
        let mut out = Vec::new();
        for p in self.packages.values() {
            let service = enabled_services.get(p.short()).filter(|_| p.registered());
            let console = matches!(p.manifest.role, PackageRole::UiClient | PackageRole::Cli);
            let mut caps = service.map(|s| enabled_capabilities(&p.manifest, s)).unwrap_or_default();
            // Management entry points have no services.* switch. Registration exposes their UI capabilities;
            // other capabilities (including optional service switches) still follow the applied configuration.
            if console && p.registered() {
                caps.extend(p.manifest.provides.iter().filter(|cap| cap.starts_with("ui.")).cloned());
            }
            let avail = if p.registered() {
                self.availability(p, &caps)
            } else {
                Err((p.reason_code.clone().unwrap_or_default(), p.reason.clone().unwrap_or_default()))
            };
            let provider = p.manifest.provider_slot.as_ref().and_then(|s| self.providers.get(s)).map(|rp| rp.source.clone());
            for cap in &p.manifest.provides {
                // A capability switched off in an enabled service is neither enabled nor running/healthy.
                let enabled = caps.contains(cap);
                // A static binary provider (openvpn-pkg, mosdns-pkg) has no process or probe: its slot capability is
                // up while the slot resolves to its own binary. Prepared providers (mihomo-pkg) report their preparer.
                let static_provider = p.manifest.role == PackageRole::BinaryProvider
                    && p.manifest.providers.iter().any(|d| &d.slot == cap && !d.requires_preparation);
                let resolved = static_provider && self.providers.get(cap).is_some_and(|rp| rp.source == p.manifest.package);
                // Planner's own features are in place once their revision applied (a failure rejects the apply).
                let own = p.manifest.role == PackageRole::Controller;
                // CLI/LuCI are not supervised daemons. For these capabilities up means the entry point is
                // available; registration or dependency errors must still prevent a healthy result.
                let entry_point = console && cap.starts_with("ui.") && avail.is_ok();
                out.push(FeatureStatus {
                    id: cap.clone(),
                    package: p.manifest.package.clone(),
                    installed: true,
                    registered: p.registered(),
                    enabled,
                    available: avail.is_ok(),
                    running: enabled && (resolved || own || entry_point || running.contains(p.short())),
                    healthy: enabled && (resolved || own || entry_point || healthy.contains(p.short())),
                    provider: provider.clone(),
                    reason_code: avail.as_ref().err().map(|e| e.0.clone()),
                    reason: avail.as_ref().err().map(|e| e.1.clone()),
                    limitation_code: None,
                    limitation: None,
                });
            }
        }
        out
    }

    /// Marks enabled host-network capabilities of a bridge-network container (§14.3.3): their processes run, but
    /// routes, TPROXY rules, NAT and multicast stay in the container's namespace, so "running" must not read as
    /// "in effect for the host/LAN".
    pub fn mark_network_scope(&self, features: &mut [FeatureStatus], network_mode: Option<&str>) {
        if network_mode != Some("bridge") {
            return;
        }
        for f in features.iter_mut().filter(|f| f.enabled) {
            let host =
                self.packages.values().any(|p| p.manifest.package == f.package && p.manifest.host_network_capabilities.contains(&f.id));
            if host {
                f.limitation_code = Some("bridge-network".into());
                f.limitation = Some(
                    "acts only inside the container's bridge network namespace: host/LAN traffic is affected only when \
                     routed into the container (or use host network mode)"
                        .into(),
                );
            }
        }
    }

    /// Plugin executable path for a registered package.
    pub fn plugin_path(&self, p: &Package) -> Option<PathBuf> {
        let ep = PathBuf::from(p.manifest.entrypoint.as_ref()?);
        if platform::is_executable(&ep) {
            return Some(ep);
        }
        // Packages may also carry the plugin next to their metadata (tests, container package directories).
        let local = p.dir.join("bin").join(format!("{}{}", p.short(), std::env::consts::EXE_SUFFIX));
        platform::is_executable(&local).then_some(local)
    }
}

/// Capabilities an enabled service turns on: every capability of the package except those whose configuration
/// switch is off.
pub fn enabled_capabilities(manifest: &PackageManifest, service: &Value) -> BTreeSet<String> {
    manifest
        .provides
        .iter()
        .filter(|cap| manifest.capability_switches.iter().filter(|s| &s.capability == *cap).all(|s| s.is_on(service)))
        .cloned()
        .collect()
}

fn resolve_decl(decl: &meduza_np_protocol::manifest::ProviderDecl) -> Result<ResolvedProvider, String> {
    if decl.requires_preparation {
        let declared = decl.data_dir.as_deref().ok_or("prepared provider without dataDir")?;
        // Manifests name the generic state root; the node's configured state dir (OpenWrt overlay, container
        // volume) replaces it. The state file there points at the actual (possibly custom) binary location.
        let state_root = meduza_np_runtime::paths::NodePaths::from_env().state;
        let dir = match declared.strip_prefix("/var/lib/meduza-np") {
            Some(rest) => state_root.join(rest.trim_start_matches('/')),
            None => Path::new(declared).to_path_buf(),
        };
        let state: PreparedProvider = meduza_np_runtime::fsutil::read_json(&dir.join("provider.json"), 64 * 1024)
            .map_err(|e| format!("provider.json unreadable: {e}"))?
            .ok_or_else(|| "binary not prepared yet (waiting-binary)".to_string())?;
        if state.state != "registered" {
            return Err(format!("provider state is {}", state.state));
        }
        if !platform::is_executable(Path::new(&state.path)) {
            return Err(format!("{} is missing or not executable", state.path));
        }
        return Ok(ResolvedProvider { path: state.path, source: state.source.unwrap_or_default(), version: state.version });
    }
    if platform::is_executable(Path::new(&decl.binary)) {
        Ok(ResolvedProvider { path: decl.binary.clone(), source: String::new(), version: None })
    } else {
        Err(format!("{} is missing or not executable", decl.binary))
    }
}

fn kernel_module_available(name: &str) -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    Path::new("/sys/module").join(name).exists()
        || std::fs::read_to_string("/proc/modules").map(|m| m.lines().any(|l| l.split_whitespace().next() == Some(name))).unwrap_or(false)
        || module_file_present(name)
}

fn module_file_present(name: &str) -> bool {
    let Ok(release) = std::fs::read_to_string("/proc/sys/kernel/osrelease") else { return false };
    let base = Path::new("/lib/modules").join(release.trim());
    // OpenWrt keeps modules flat in /lib/modules/<release>/; other distros use modules.dep.
    base.join(format!("{name}.ko")).exists()
        || std::fs::read_to_string(base.join("modules.dep")).map(|d| d.contains(&format!("/{name}.ko"))).unwrap_or(false)
}

fn kernel_feature_available(name: &str) -> bool {
    match name {
        "tun" => Path::new("/dev/net/tun").exists(),
        "tproxy" => kernel_module_available("nft_tproxy") || kernel_module_available("xt_TPROXY"),
        "nftables" => platform::which("nft").is_some(),
        _ => false,
    }
}

fn service_available(name: &str) -> bool {
    match name {
        "dbus" => Path::new("/run/dbus/system_bus_socket").exists() || Path::new("/var/run/dbus/system_bus_socket").exists(),
        "openclash" => Path::new("/etc/init.d/openclash").exists(),
        other => Path::new("/etc/init.d").join(other).exists(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_pkg(root: &Path, name: &str, manifest: serde_json::Value) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
    }

    fn base(name: &str, role: &str) -> serde_json::Value {
        json!({"manifestVersion": 1, "package": name, "version": "0.1.0", "role": role, "plannerApi": {"min": 1, "max": 1}})
    }

    #[test]
    fn conflicts_dependencies_and_compat() {
        let root = std::env::temp_dir().join(format!("mnp-registry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_pkg(&root, "planner", base("meduza-np-planner", "controller"));
        let mut dash = base("meduza-np-dashboard", "ui-client");
        dash["requires"] = json!(["meduza-np-planner"]);
        dash["conflicts"] = json!(["meduza-np-dashboard-openwrt"]);
        write_pkg(&root, "dashboard", dash);
        let mut luci = base("meduza-np-dashboard-openwrt", "ui-client");
        luci["requires"] = json!(["meduza-np-planner"]);
        luci["conflicts"] = json!(["meduza-np-dashboard"]);
        write_pkg(&root, "dashboard-openwrt", luci);
        let mut cli = base("meduza-np-cli", "cli");
        cli["requires"] = json!(["meduza-np-planner"]);
        cli["plannerApi"] = json!({"min": 2, "max": 3});
        write_pkg(&root, "cli", cli);
        let mut pkg = base("meduza-np-openvpn-pkg", "binary-provider");
        pkg["requires"] = json!(["meduza-np-planner"]);
        pkg["providers"] = json!([{"slot": "openvpn.binary", "binary": "/nonexistent/openvpn"}]);
        write_pkg(&root, "openvpn-pkg", pkg);

        let reg = scan(&root, &Platform::default());
        assert!(reg.get("meduza-np-planner").unwrap().registered());
        assert_eq!(reg.get("meduza-np-dashboard").unwrap().state, RegistrationState::Conflict);
        assert_eq!(reg.get("meduza-np-dashboard-openwrt").unwrap().state, RegistrationState::Conflict);
        assert_eq!(reg.get("meduza-np-cli").unwrap().state, RegistrationState::Incompatible);
        // Provider package registered but its binary is missing: slot is not silently served by the system.
        assert!(reg.get("meduza-np-openvpn-pkg").unwrap().registered());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn console_features_need_registration_not_a_service_configuration() {
        for dashboard in ["meduza-np-dashboard", "meduza-np-dashboard-openwrt"] {
            let root = std::env::temp_dir().join(format!("mnp-registry-console-{}-{dashboard}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            write_pkg(&root, "planner", base("meduza-np-planner", "controller"));
            write_pkg(&root, "cli", serde_json::from_str(include_str!("../../meduza-np-cli/manifest.json")).unwrap());
            let manifest = if dashboard == "meduza-np-dashboard" {
                include_str!("../../meduza-np-dashboard/manifest.json")
            } else {
                include_str!("../../meduza-np-dashboard-openwrt/manifest.json")
            };
            write_pkg(&root, "dashboard", serde_json::from_str(manifest).unwrap());
            let plat = Platform { openwrt: dashboard == "meduza-np-dashboard-openwrt", ..Platform::default() };
            let mut reg = scan(&root, &plat);
            let features = reg.features(&BTreeMap::new(), &BTreeSet::new(), &BTreeSet::new());
            assert_eq!(features.len(), 2);
            assert!(features.iter().all(|f| f.registered && f.enabled && f.available && f.running && f.healthy), "{features:#?}");
            // A missing runtime dependency is still an error, never a healthy console.
            reg.packages.get_mut(dashboard).unwrap().manifest.system_dependencies = serde_json::from_value(json!([
                {"kind": "binary", "name": "/nonexistent/meduza-np-console-test"}
            ]))
            .unwrap();
            let features = reg.features(&BTreeMap::new(), &BTreeSet::new(), &BTreeSet::new());
            let blocked = features.iter().find(|f| f.package == dashboard).unwrap();
            assert!(blocked.enabled && !blocked.available && !blocked.running && !blocked.healthy);
            assert_eq!(blocked.reason_code.as_deref(), Some("system-dependency-missing"));
            // Incompatible or conflicted packages must not acquire enabled UI capabilities.
            for state in [RegistrationState::Incompatible, RegistrationState::Conflict, RegistrationState::MissingDependency] {
                reg.packages.get_mut(dashboard).unwrap().state = state;
                let features = reg.features(&BTreeMap::new(), &BTreeSet::new(), &BTreeSet::new());
                let blocked = features.iter().find(|f| f.package == dashboard).unwrap();
                assert!(!blocked.registered && !blocked.enabled && !blocked.available && !blocked.running && !blocked.healthy);
            }
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn switched_capability_follows_its_config() {
        let root = std::env::temp_dir().join(format!("mnp-registry3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_pkg(&root, "planner", base("meduza-np-planner", "controller"));
        let mut avahi = base("meduza-np-avahi", "ui-client");
        avahi["requires"] = json!(["meduza-np-planner"]);
        avahi["provides"] = json!(["mdns.avahi", "mdns.bridge"]);
        avahi["capabilitySwitches"] = json!([{"capability": "mdns.bridge", "pointer": "/bridge/enabled"}]);
        write_pkg(&root, "avahi", avahi);
        let reg = scan(&root, &Platform::default());
        assert!(reg.get("meduza-np-avahi").unwrap().registered());
        let up = BTreeSet::from(["avahi".to_string()]);
        let feature =
            |services: &BTreeMap<String, Value>, id: &str| reg.features(services, &up, &up).into_iter().find(|f| f.id == id).unwrap();
        let off = BTreeMap::from([("avahi".to_string(), json!({"bridge": {"enabled": false}}))]);
        let (avahi, bridge) = (feature(&off, "mdns.avahi"), feature(&off, "mdns.bridge"));
        assert!(avahi.enabled && avahi.healthy);
        assert!(!bridge.enabled && !bridge.running && !bridge.healthy);
        let on = BTreeMap::from([("avahi".to_string(), json!({"bridge": {"enabled": true}}))]);
        assert!(feature(&on, "mdns.bridge").healthy);
        assert!(!feature(&BTreeMap::new(), "mdns.avahi").enabled);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn planner_network_features_follow_the_node_networks() {
        let root = std::env::temp_dir().join(format!("mnp-registry6-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // the real planner manifest: its switches and host-network capabilities are what nodes run with
        let manifest: Value = serde_json::from_str(include_str!("../manifest.json")).unwrap();
        write_pkg(&root, "planner", manifest);
        let reg = scan(&root, &Platform::default());
        let none = BTreeSet::new();
        let features = |networks: Value, mode: Option<&str>| {
            let services = BTreeMap::from([(PLANNER_SERVICE.to_string(), networks)]);
            let mut f = reg.features(&services, &none, &none);
            reg.mark_network_scope(&mut f, mode);
            f
        };
        let get = |f: &[FeatureStatus], id: &str| f.iter().find(|x| x.id == id).unwrap().clone();
        let empty = features(json!({"lan": [], "mappings": [], "portForwards": [], "healthPorts": []}), Some("bridge"));
        assert!(get(&empty, "planner.core").running && get(&empty, "planner.core").healthy);
        assert!(!get(&empty, "network.port-forwards").enabled && get(&empty, "network.port-forwards").limitation_code.is_none());
        let forward = json!({"id": "echo", "enabled": true, "proto": "tcp", "listenPort": 8089, "targetAddress": "10.96.0.1",
            "targetPort": 8089, "snat": true});
        let off = features(json!({"portForwards": [{"enabled": false, "id": "x"}]}), Some("bridge"));
        assert!(!get(&off, "network.port-forwards").enabled);
        let on = features(json!({"portForwards": [forward]}), Some("bridge"));
        let pf = get(&on, "network.port-forwards");
        assert!(pf.enabled && pf.running && pf.healthy, "{pf:?}");
        assert_eq!(pf.limitation_code.as_deref(), Some("bridge-network"));
        assert!(!get(&on, "network.mappings").enabled && !get(&on, "network.health-ports").enabled);
        assert!(get(&features(json!({"portForwards": [forward]}), Some("host")), "network.port-forwards").limitation_code.is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn host_network_capabilities_are_limited_in_bridge_mode() {
        let root = std::env::temp_dir().join(format!("mnp-registry5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_pkg(&root, "planner", base("meduza-np-planner", "controller"));
        let mut avahi = base("meduza-np-avahi", "ui-client");
        avahi["requires"] = json!(["meduza-np-planner"]);
        avahi["provides"] = json!(["mdns.avahi", "mdns.bridge"]);
        avahi["hostNetworkCapabilities"] = json!(["mdns.avahi"]);
        write_pkg(&root, "avahi", avahi);
        let reg = scan(&root, &Platform::default());
        let up = BTreeSet::from(["avahi".to_string()]);
        let services = BTreeMap::from([("avahi".to_string(), json!({}))]);
        let marked = |mode: Option<&str>| {
            let mut f = reg.features(&services, &up, &up);
            reg.mark_network_scope(&mut f, mode);
            f
        };
        let bridge = marked(Some("bridge"));
        let avahi = bridge.iter().find(|f| f.id == "mdns.avahi").unwrap();
        // still running and healthy: the limitation is a statement about scope, not a failure
        assert!(avahi.healthy && avahi.limitation_code.as_deref() == Some("bridge-network"), "{avahi:?}");
        assert!(bridge.iter().find(|f| f.id == "mdns.bridge").unwrap().limitation_code.is_none());
        for mode in [Some("host"), Some("macvlan"), None] {
            assert!(marked(mode).iter().all(|f| f.limitation_code.is_none()));
        }
        // a capability that is not enabled carries no limitation
        let mut off = reg.features(&BTreeMap::new(), &up, &up);
        reg.mark_network_scope(&mut off, Some("bridge"));
        assert!(off.iter().all(|f| f.limitation_code.is_none()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn static_provider_is_up_while_its_binary_resolves() {
        let root = std::env::temp_dir().join(format!("mnp-registry4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_pkg(&root, "planner", base("meduza-np-planner", "controller"));
        let mut user = base("meduza-np-mosdns", "ui-client");
        user["requires"] = json!(["meduza-np-planner"]);
        user["providerSlot"] = json!("mosdns.binary");
        write_pkg(&root, "mosdns", user);
        let provider = |binary: &str| {
            let mut pkg = base("meduza-np-mosdns-pkg", "binary-provider");
            pkg["requires"] = json!(["meduza-np-planner"]);
            pkg["provides"] = json!(["mosdns.binary"]);
            pkg["providers"] = json!([{"slot": "mosdns.binary", "binary": binary}]);
            pkg
        };
        // No process and no probe (empty running/healthy sets): the resolved slot alone decides.
        let services = BTreeMap::from([("mosdns-pkg".to_string(), json!({}))]);
        let none = BTreeSet::new();
        let feature = |reg: &Registry| reg.features(&services, &none, &none).into_iter().find(|f| f.id == "mosdns.binary").unwrap();
        write_pkg(&root, "mosdns-pkg", provider(&std::env::current_exe().unwrap().to_string_lossy()));
        let f = feature(&scan(&root, &Platform::default()));
        assert!(f.enabled && f.running && f.healthy, "{f:?}");
        write_pkg(&root, "mosdns-pkg", provider("/nonexistent/mosdns"));
        let f = feature(&scan(&root, &Platform::default()));
        assert!(f.enabled && !f.running && !f.healthy, "{f:?}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_dependency_cascades() {
        let root = std::env::temp_dir().join(format!("mnp-registry2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut cli = base("meduza-np-cli", "cli");
        cli["requires"] = json!(["meduza-np-planner"]);
        write_pkg(&root, "cli", cli);
        let reg = scan(&root, &Platform::default());
        let p = reg.get("meduza-np-cli").unwrap();
        assert_eq!(p.state, RegistrationState::MissingDependency);
        let feats = reg.features(&BTreeMap::new(), &BTreeSet::new(), &BTreeSet::new());
        assert!(feats.iter().all(|f| !f.registered && !f.available));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
