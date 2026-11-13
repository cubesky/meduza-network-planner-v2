//! OpenWrt backend (§13.3, §14.2): the detected native package manager installs our verified `.ipk`/`.apk` files.
//! This is *not* an atomic group switch: updater is taken over first, then packages are installed one by one and
//! a failure triggers a best-effort rollback by fetching previously verified artifacts again. Archives live only
//! in an owned /tmp directory, one at a time; persistent storage holds bounded artifact metadata, never new archives.
//! Only `meduza-np-*` packages are touched — never the kernel, firmware or other system packages.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::Backend;
use crate::meta::Source;
use crate::resolve::Plan;
use crate::store::Store;
use meduza_np_protocol::group::Artifact;
use serde::{Deserialize, Serialize};

/// Journaled before touching native packages; absent in journals from the former bulk-download backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    pub plan: Plan,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub attempted: Vec<String>,
    #[serde(default)]
    pub completed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedArtifact {
    pub package: String,
    pub version: String,
    pub base_url: String,
    pub artifact: Artifact,
}

pub fn artifacts(store: &Store) -> Result<Vec<SavedArtifact>, String> {
    meduza_np_runtime::fsutil::read_json(&store.root.join("openwrt/artifacts.json"), 1024 * 1024)
        .map(|v| v.unwrap_or_default())
        .map_err(|e| e.to_string())
}

pub fn remember(store: &Store, saved: SavedArtifact) -> Result<(), String> {
    let mut all = artifacts(store)?;
    if all.contains(&saved) {
        return Ok(());
    }
    all.retain(|a| a.package != saved.package || a.version != saved.version);
    all.push(saved);
    if all.len() > 256 {
        return Err("too many native artifact records; cleanup required".into());
    }
    save_artifacts(store, &all)
}

pub fn save_artifacts(store: &Store, all: &[SavedArtifact]) -> Result<(), String> {
    if artifacts(store)? == all {
        return Ok(());
    }
    meduza_np_runtime::fsutil::write_json_atomic(&store.root.join("openwrt/artifacts.json"), &all, 0o600).map_err(|e| e.to_string())
}

/// Stable across the updater's helper/restart, isolated per persistent store; never follow an attacker-owned /tmp link.
pub fn workspace(store: &Store) -> Result<PathBuf, String> {
    let dir = meduza_np_runtime::update::native_download_dir(&store.root).map_err(|e| e.to_string())?;
    match std::fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.to_string()),
    }
    let meta = std::fs::symlink_metadata(&dir).map_err(|e| e.to_string())?;
    if !meta.is_dir() {
        return Err("native download directory is not a real directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments or memory safety preconditions.
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err("native download directory has another owner".into());
        }
    }
    meduza_np_runtime::fsutil::set_mode(&dir, 0o700).map_err(|e| e.to_string())?;
    Ok(dir)
}

pub fn archive(store: &Store, package: &str, version: &str, format: &str) -> Result<PathBuf, String> {
    if !meduza_np_protocol::manifest::ALL_PACKAGES.contains(&package)
        || version.is_empty()
        || !version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
        || !matches!(format, "ipk" | "apk")
    {
        return Err("invalid native artifact filename".into());
    }
    Ok(workspace(store)?.join(format!("{package}_{version}.{format}")))
}

/// Delete only our known archives, never recurse or follow symlinks. Called under operation.lock.
pub fn clean_downloads(store: &Store) -> Result<(), String> {
    let dir = workspace(store)?;
    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if (name.ends_with(".ipk") || name.ends_with(".apk"))
            && meduza_np_protocol::manifest::ALL_PACKAGES.iter().any(|p| name.starts_with(&format!("{p}_")))
        {
            std::fs::remove_file(entry.path()).map_err(|e| e.to_string())?;
        }
    }
    let _ = std::fs::remove_dir(&dir); // empty directories only; preserve unknown files.
    Ok(())
}

/// The guard outlives the package-manager invocation and releases successful/partial downloads on every error path.
pub struct Download(pub PathBuf);
impl Drop for Download {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn fetch(store: &Store, saved: &SavedArtifact, notify: impl FnMut(crate::retry::Attempt)) -> Result<Download, String> {
    if saved.artifact.size > crate::store::MAX_ARTIFACT_BYTES {
        return Err("native artifact exceeds size limit".into());
    }
    let file = Download(archive(store, &saved.package, &saved.version, &saved.artifact.format)?);
    // Remove stale partial files or links before opening the destination in our private directory.
    let _ = std::fs::remove_file(&file.0);
    check_tmp_space(store, saved.artifact.size)?;
    let source = Source::new(&saved.base_url).map_err(|e| e.to_string())?;
    let url = source.resolve(&saved.artifact.url).map_err(|e| e.to_string())?;
    source.fetch_to_with_progress(&url, &file.0, saved.artifact.size, 900, notify).map_err(|e| e.to_string())?;
    crate::store::verify_file(&file.0, saved.artifact.size, &saved.artifact.sha256)?;
    Ok(file)
}

pub fn check_tmp_space(store: &Store, size: u64) -> Result<(), String> {
    let need = size.saturating_add(8 * 1024 * 1024);
    let dir = workspace(store)?;
    let free = meduza_np_runtime::fsutil::free_space(&dir).map_err(|e| e.to_string())?;
    if free < need {
        return Err(format!("insufficient space in {}: {free} bytes free, {need} needed", dir.display()));
    }
    #[cfg(target_os = "linux")]
    if let Some(available) = std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("MemAvailable:")?.split_whitespace().next()?.parse::<u64>().ok().map(|v| v.saturating_mul(1024)))
    }) {
        if available < need {
            return Err(format!("insufficient memory for native download: {available} bytes available, {need} needed"));
        }
    }
    Ok(())
}

/// The install target is the system overlay even when metadata lives on an external volume.
pub fn check_install_space(store: &Store, size: u64) -> Result<(), String> {
    let root = if Path::new("/etc/openwrt_release").exists() { Path::new("/usr") } else { store.root.as_path() };
    let need = size.saturating_mul(3).saturating_add(1024 * 1024);
    let free = meduza_np_runtime::fsutil::free_space(root).map_err(|e| e.to_string())?;
    if free < need {
        return Err(format!("insufficient native installation space in {}: {free} free, {need} needed", root.display()));
    }
    Ok(())
}

pub trait Runner {
    fn run(&self, program: &str, args: &[String]) -> Result<String, String>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
        let out = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("{program}: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.status.success() {
            Ok(text)
        } else {
            let err: String = String::from_utf8_lossy(&out.stderr).chars().take(600).collect();
            Err(format!("{program} {}: {}", args.join(" "), err.trim()))
        }
    }
}

/// `openwrt/<DISTRIB_ARCH>` from `/etc/openwrt_release`, e.g. `openwrt/aarch64_cortex-a53`.
pub fn platform() -> Option<String> {
    let text = std::fs::read_to_string("/etc/openwrt_release").ok()?;
    parse_release_arch(&text).map(|a| format!("openwrt/{a}"))
}

pub fn parse_release_arch(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let v = l.strip_prefix("DISTRIB_ARCH=")?.trim().trim_matches(['\'', '"']);
        (!v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then(|| v.to_string())
    })
}

/// Installed `meduza-np-*` packages and versions as reported by the package manager.
pub fn installed(runner: &dyn Runner, backend: Backend) -> Result<BTreeMap<String, String>, String> {
    match backend {
        Backend::Opkg => Ok(parse_opkg_list(&runner.run("opkg", &["list-installed".into()])?)),
        Backend::Apk => Ok(parse_apk_list(&runner.run("apk", &["list".into(), "--installed".into()])?)),
        Backend::Store => Err("not an OpenWrt backend".into()),
    }
}

/// `name - version` per line.
pub fn parse_opkg_list(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let (name, version) = l.split_once(" - ")?;
            let version = version.split(" - ").next()?.trim();
            name.starts_with("meduza-np-").then(|| (name.trim().to_string(), version.to_string()))
        })
        .collect()
}

/// `meduza-np-planner-0.2.0-r1 aarch64_cortex-a53 {meduza-np-planner} (Apache-2.0) [installed]`
pub fn parse_apk_list(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let first = l.split_whitespace().next()?;
            let origin = l.split_once('{').and_then(|(_, r)| r.split_once('}')).map(|(o, _)| o.trim());
            // name-version-rN: the version starts at the first '-' followed by a digit.
            let idx = first.char_indices().find(|(i, c)| *c == '-' && first[i + 1..].starts_with(|d: char| d.is_ascii_digit()))?.0;
            let name = origin.filter(|o| first.starts_with(&format!("{o}-"))).unwrap_or(&first[..idx]);
            let version = &first[name.len() + 1..];
            let version =
                version.rsplit_once("-r").filter(|(_, r)| r.chars().all(|c| c.is_ascii_digit())).map(|(v, _)| v).unwrap_or(version);
            name.starts_with("meduza-np-").then(|| (name.to_string(), version.to_string()))
        })
        .collect()
}

pub struct Cache {
    pub dir: PathBuf,
    pub ext: &'static str,
}

impl Cache {
    pub fn new(store_root: &Path, backend: Backend) -> Self {
        Self { dir: store_root.join("openwrt/cache"), ext: if backend == Backend::Apk { "apk" } else { "ipk" } }
    }
    pub fn file(&self, package: &str, version: &str) -> PathBuf {
        self.dir.join(format!("{package}_{version}.{}", self.ext))
    }
    pub fn find(&self, package: &str, version: &str) -> Option<PathBuf> {
        let f = self.file(package, version);
        f.exists().then_some(f)
    }
    /// Keeps the files of `keep` (package → version) and deletes every other cached file.
    pub fn prune(&self, keep: &BTreeMap<String, Vec<String>>) {
        if [&self.dir, self.dir.parent().unwrap_or(&self.dir)].iter().any(|p| !std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())) {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if !(n.ends_with(".apk") || n.ends_with(".ipk"))
                || !meduza_np_protocol::manifest::ALL_PACKAGES.iter().any(|p| n.starts_with(&format!("{p}_")))
            {
                continue;
            }
            let wanted = keep.iter().any(|(p, vs)| vs.iter().any(|v| n == format!("{p}_{v}.{}", self.ext)));
            if !wanted {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// File in the run directory that exists while this updater runs opkg/apk. Our packages' maintainer scripts then skip
/// their own service start / planner rescan: the updater drains, restarts through the owner and verifies (D12, D29).
pub const INSTALL_MARKER: &str = "updater-installing";

pub struct InstallMarker(PathBuf);

impl InstallMarker {
    pub fn hold(run_dir: &Path) -> Self {
        let p = run_dir.join(INSTALL_MARKER);
        let _ = std::fs::create_dir_all(run_dir);
        if let Err(e) = std::fs::write(&p, std::process::id().to_string()) {
            eprintln!("warning: {}: {e} (package scripts may restart services themselves)", p.display());
        }
        Self(p)
    }

    /// A marker left by a crashed updater (the run directory is tmpfs, so only within one boot).
    pub fn clear_stale(run_dir: &Path) {
        let _ = std::fs::remove_file(run_dir.join(INSTALL_MARKER));
    }
}

impl Drop for InstallMarker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn install_args(backend: Backend, file: &Path) -> (&'static str, Vec<String>) {
    let f = file.to_string_lossy().into_owned();
    match backend {
        // --force-downgrade so a rollback (or a pinned older group) can reinstall the previous version.
        Backend::Opkg => ("opkg", vec!["install".into(), "--force-downgrade".into(), f]),
        // Our own signature/digest checks replace the repository key check for local files.
        _ => ("apk", vec!["add".into(), "--allow-untrusted".into(), "--no-cache".into(), f]),
    }
}

pub fn remove_args(backend: Backend, package: &str) -> (&'static str, Vec<String>) {
    match backend {
        Backend::Opkg => ("opkg", vec!["remove".into(), package.into()]),
        _ => ("apk", vec!["del".into(), package.into()]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn private_tmp_directory_rejects_links_and_cleans_only_known_archives() {
        let root = std::env::temp_dir().join(format!("mnp-native-private-{}", std::process::id()));
        let store = Store::new(root.join("store"));
        store.init().unwrap();
        let outside = root.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("preserve"), b"user data").unwrap();
        let dir = meduza_np_runtime::update::native_download_dir(&store.root).unwrap();
        std::os::unix::fs::symlink(&outside, &dir).unwrap();
        assert!(workspace(&store).is_err());
        assert!(clean_downloads(&store).is_err());
        assert_eq!(std::fs::read(outside.join("preserve")).unwrap(), b"user data");
        std::fs::remove_file(&dir).unwrap();
        workspace(&store).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::write(dir.join("meduza-np-planner_0.1.0.ipk"), b"orphan").unwrap();
        std::fs::write(dir.join("notes"), b"unknown").unwrap();
        clean_downloads(&store).unwrap();
        assert!(!dir.join("meduza-np-planner_0.1.0.ipk").exists());
        assert!(dir.join("notes").exists());
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_marker_exists_while_held() {
        let d = std::env::temp_dir().join(format!("mnp-marker-{}", std::process::id()));
        {
            let _m = InstallMarker::hold(&d);
            assert!(d.join(INSTALL_MARKER).is_file());
        }
        assert!(!d.join(INSTALL_MARKER).exists());
        std::fs::write(d.join(INSTALL_MARKER), "1").unwrap();
        InstallMarker::clear_stale(&d);
        assert!(!d.join(INSTALL_MARKER).exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn parse_lists() {
        let opkg = "busybox - 1.36.1-1\nmeduza-np-planner - 0.2.0\nmeduza-np-wireguard - 0.1.0 - extra\n";
        let m = parse_opkg_list(opkg);
        assert_eq!(m.len(), 2);
        assert_eq!(m["meduza-np-planner"], "0.2.0");
        assert_eq!(m["meduza-np-wireguard"], "0.1.0");
        let apk = "meduza-np-planner-0.2.0-r1 aarch64_cortex-a53 {meduza-np-planner} (Apache-2.0) [installed]\n\
                   meduza-np-mihomo-pkg-0.3.1-r0 aarch64_cortex-a53 {meduza-np-mihomo-pkg} (Apache-2.0) [installed]\n\
                   dnsmasq-2.90-r4 aarch64_cortex-a53 {dnsmasq} (GPL-2.0) [installed]\n";
        let m = parse_apk_list(apk);
        assert_eq!(m.len(), 2);
        assert_eq!(m["meduza-np-planner"], "0.2.0");
        assert_eq!(m["meduza-np-mihomo-pkg"], "0.3.1");
    }

    #[test]
    fn release_arch() {
        let t = "DISTRIB_ID='OpenWrt'\nDISTRIB_RELEASE='24.10.2'\nDISTRIB_ARCH='aarch64_cortex-a53'\n";
        assert_eq!(parse_release_arch(t).as_deref(), Some("aarch64_cortex-a53"));
        assert_eq!(parse_release_arch("DISTRIB_ARCH='x; rm -rf /'\n"), None);
    }

    #[test]
    fn cache_retains_only_selected_rollback_versions_and_leaves_unknown_files() {
        let root = std::env::temp_dir().join(format!("mnp-native-cache-gc-{}", std::process::id()));
        for backend in [Backend::Opkg, Backend::Apk] {
            let cache = Cache::new(&root, backend);
            std::fs::create_dir_all(&cache.dir).unwrap();
            for version in ["0.1.0", "0.2.0", "0.3.0"] {
                std::fs::write(cache.file("meduza-np-planner", version), b"archive").unwrap();
            }
            std::fs::write(cache.dir.join("user-notes"), b"preserve").unwrap();
            cache.prune(&BTreeMap::from([("meduza-np-planner".into(), vec!["0.2.0".into(), "0.3.0".into()])]));
            assert!(!cache.file("meduza-np-planner", "0.1.0").exists());
            assert!(cache.file("meduza-np-planner", "0.2.0").exists());
            assert!(cache.file("meduza-np-planner", "0.3.0").exists());
            assert!(cache.dir.join("user-notes").exists());
            std::fs::remove_dir_all(&root).unwrap();
        }
    }
}
