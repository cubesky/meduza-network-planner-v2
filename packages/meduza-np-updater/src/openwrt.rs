//! OpenWrt backend (§13.3, §14.2): the detected native package manager installs our verified `.ipk`/`.apk` files.
//! This is *not* an atomic group switch: updater is taken over first, then packages are installed one by one and
//! a failure triggers a best-effort rollback by reinstalling the cached previous files. Every package this updater
//! installed successfully is kept in `<store>/openwrt/cache/` so it can be reinstalled; packages that were
//! installed by other means have no cached file, which the plan reports as "rollback unavailable".
//! Only `meduza-np-*` packages are touched — never the kernel, firmware or other system packages.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::Backend;

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
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
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
        _ => ("apk", vec!["add".into(), "--allow-untrusted".into(), f]),
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
}
