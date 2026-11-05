//! OpenWrt packages (§13.3, §14.2, D29): the same static musl binaries and metadata as the generic archives, laid out
//! for the native package manager. One file per OpenWrt package architecture (`DISTRIB_ARCH`, which is also the
//! updater's `openwrt/<arch>` platform) and format:
//!
//! * `.ipk` (opkg, OpenWrt ≤ 24.10) is written here: a gzipped tar of `debian-binary`, `data.tar.gz` and
//!   `control.tar.gz`, byte-identical for the same inputs.
//! * `.apk` (apk-tools 3, OpenWrt ≥ 25.12) needs the ADB format: this module writes a staging directory (`root/`,
//!   scripts, `mkpkg.sh`) that `tools/openwrt-apk.sh` turns into the package with a pinned `apk mkpkg`, then
//!   `meduza-np-release artifact --stage` records its digest.
//!
//! Packages declare dependencies on other `meduza-np-*` packages only: system software (wireguard-tools, kmods, frr,
//! …) is reported by the planner as a missing dependency instead, so an update can never pull system packages from
//! the feeds (D12). The LuCI app is the exception: it depends on the LuCI pieces it plugs into.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use meduza_np_protocol::group::Artifact;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::archive::{self, walk, Src};

/// Packages built for OpenWrt. The Rust dashboard is not one of them: OpenWrt uses LuCI (§16).
pub const OPENWRT_PACKAGES: &[&str] = &[
    "meduza-np-planner",
    "meduza-np-cli",
    "meduza-np-updater",
    "meduza-np-wireguard",
    "meduza-np-tinc",
    "meduza-np-openvpn",
    "meduza-np-openvpn-pkg",
    "meduza-np-frr",
    "meduza-np-dnsmasq",
    "meduza-np-mosdns",
    "meduza-np-mosdns-pkg",
    "meduza-np-mihomo",
    "meduza-np-mihomo-pkg",
    "meduza-np-mihomo-openwrt",
    "meduza-np-avahi",
    "meduza-np-dashboard-openwrt",
];

/// Architecture-independent OpenWrt packages (opkg `all`, apk `noarch`, group platform `any`).
pub const NOARCH_PACKAGES: &[&str] = &["meduza-np-dashboard-openwrt"];

/// OpenWrt package architectures served by each musl target. The binaries are static, so one build runs on every
/// listed CPU; each architecture still gets its own file because opkg/apk refuse foreign architectures. armv7 is
/// built for VFPv3-D16 hard float, so FPU-less ARM targets are not listed. MIPS has no build target yet.
pub const ARCHES: &[(&str, &[&str])] = &[
    ("x86_64-unknown-linux-musl", &["x86_64"]),
    ("aarch64-unknown-linux-musl", &["aarch64_generic", "aarch64_cortex-a53", "aarch64_cortex-a72", "aarch64_cortex-a76"]),
    ("armv7-unknown-linux-musleabihf", &["arm_cortex-a7_neon-vfpv4", "arm_cortex-a9_vfpv3-d16", "arm_cortex-a15_neon-vfpv4"]),
];

/// Linux platform of a musl target (provider recipes are built per platform, not per Rust target).
pub const TARGET_PLATFORMS: &[(&str, &str)] = &[
    ("x86_64-unknown-linux-musl", "linux/amd64"),
    ("aarch64-unknown-linux-musl", "linux/arm64"),
    ("armv7-unknown-linux-musleabihf", "linux/arm/v7"),
];

pub const FORMATS: &[&str] = &["ipk", "apk"];

/// Marker the updater holds while it runs opkg/apk; package scripts then leave restarts and rescans to it.
pub const UPDATER_MARKER: &str = "/var/run/meduza-np/updater-installing";

pub fn arches_for(target_or_platform: &str) -> Result<&'static [&'static str], String> {
    let target = TARGET_PLATFORMS.iter().find(|(_, p)| *p == target_or_platform).map(|(t, _)| *t).unwrap_or(target_or_platform);
    ARCHES.iter().find(|(t, _)| *t == target).map(|(_, a)| *a).ok_or_else(|| format!("no OpenWrt architectures for {target_or_platform}"))
}

/// Every `openwrt/<arch>` platform of a group built for `targets`.
pub fn platforms(targets: &[String]) -> Vec<String> {
    ARCHES.iter().filter(|(t, _)| targets.iter().any(|x| x == t)).flat_map(|(_, a)| a.iter().map(|a| format!("openwrt/{a}"))).collect()
}

/// Inputs of the OpenWrt layout that are not package files (part of the OpenWrt packages' fingerprints).
pub const LAYOUT_FILES: &[&str] = &["tools/release/src/openwrt.rs", "tools/openwrt-apk.sh"];

/// Release trust store shipped with the OpenWrt updater package (the base image carries its own copy).
pub const TRUST_FILE: &str = "packages/meduza-np-base-container/trust/release-keys.json";

fn noarch(package: &str) -> bool {
    NOARCH_PACKAGES.contains(&package)
}

/// Shell scripts must keep LF endings even from a Windows checkout.
fn script(path: &Path) -> Result<Src, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Src::Bytes(text.replace("\r\n", "\n").into_bytes()))
}

/// Entries of the data part (path without leading `/` → source, executable).
pub fn entries(
    repo: &Path,
    package: &str,
    version: &str,
    bin_dir: Option<&Path>,
    tree: Option<&Path>,
    trust: Option<&Path>,
) -> Result<BTreeMap<String, (Src, bool)>, String> {
    if !OPENWRT_PACKAGES.contains(&package) {
        return Err(format!("{package} is not built for OpenWrt"));
    }
    let dir = repo.join("packages").join(package);
    if package == "meduza-np-dashboard-openwrt" {
        // LuCI layout (as luci.mk installs it): htdocs/ → /www, root/ → /, plus our package metadata.
        let mut out = archive::entries(repo, package, version, None, None)?;
        let htdocs = dir.join("htdocs");
        for rel in walk(&htdocs)? {
            out.insert(format!("www/{}", slash(&rel)), (Src::File(htdocs.join(&rel)), false));
        }
        let root = dir.join("root");
        for rel in walk(&root)? {
            let r = slash(&rel);
            let exec = r.starts_with("usr/libexec/rpcd/");
            out.insert(r, (if exec { script(&root.join(&rel))? } else { Src::File(root.join(&rel)) }, exec));
        }
        return Ok(out);
    }
    let mut out = archive::entries(repo, package, version, bin_dir, tree)?;
    let short = package.trim_start_matches(meduza_np_protocol::manifest::PACKAGE_PREFIX);
    let init = dir.join(format!("packaging/openwrt/{package}.init"));
    if init.is_file() {
        out.insert(format!("etc/init.d/{package}"), (script(&init)?, true));
    }
    if short == "updater" {
        let launcher = dir.join("packaging/updater-run.sh");
        out.insert("usr/lib/meduza-np/updater-run".into(), (script(&launcher)?, true));
        if let Some(t) = trust {
            out.insert("usr/share/meduza-np/trust/release-keys.json".into(), (Src::File(t.to_path_buf()), false));
        }
    }
    Ok(out)
}

fn slash(p: &Path) -> String {
    p.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/")
}

/// Package metadata shared by both formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub name: String,
    pub version: String,
    /// `all`/`noarch` for architecture-independent packages, else the OpenWrt architecture.
    pub arch: String,
    pub depends: Vec<String>,
    pub conflicts: Vec<String>,
    pub description: String,
    pub license: String,
    pub installed_size: u64,
    pub postinst: String,
    pub prerm: String,
}

fn cargo_field(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| l.trim().strip_prefix(&format!("{key} = ")).map(|v| v.trim().trim_matches('"').to_string()))
}

pub fn control(repo: &Path, package: &str, version: &str, arch: &str, data: &BTreeMap<String, (Src, bool)>) -> Result<Control, String> {
    let dir = repo.join("packages").join(package);
    let manifest: Value = serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).map_err(|e| format!("{package}: {e}"))?)
        .map_err(|e| format!("{package}: {e}"))?;
    let list = |k: &str| -> Vec<String> {
        manifest[k]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|p| p.starts_with("meduza-np-"))
            .map(str::to_string)
            .collect()
    };
    let mut depends = list("requires");
    let conflicts = list("conflicts");
    let cargo = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
    let mut description = cargo_field(&cargo, "description").unwrap_or_else(|| format!("Meduza NP {package}"));
    if package == "meduza-np-dashboard-openwrt" {
        // the LuCI app plugs into these (LUCI_DEPENDS of its Makefile)
        depends.extend(["rpcd", "jsonfilter", "luci-base"].map(str::to_string));
        description = "Meduza NP node dashboard for LuCI: status, features, instances, maintenance and updates".into();
    }
    let license = std::fs::read_to_string(repo.join("Cargo.toml")).ok().and_then(|t| cargo_field(&t, "license")).unwrap_or_default();
    let mut installed_size = 0;
    for (src, _) in data.values() {
        installed_size += match src {
            Src::Bytes(b) => b.len() as u64,
            Src::File(p) => std::fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?.len(),
        };
    }
    let role = manifest["role"].as_str().unwrap_or_default();
    let (postinst, prerm) = scripts(package, role);
    Ok(Control {
        name: package.into(),
        version: version.into(),
        arch: arch.into(),
        depends,
        conflicts,
        description,
        license,
        installed_size,
        postinst,
        prerm,
    })
}

/// Maintainer scripts. Upgrades done by the updater (marker present) leave restarts to it: it drains, restarts
/// through the service owner and verifies (D12). A manual `opkg install`/`apk add` gets the usual OpenWrt behaviour:
/// services enabled and started, plugins registered by a planner rescan.
fn scripts(package: &str, role: &str) -> (String, String) {
    let head = format!("#!/bin/sh\n[ -n \"$IPKG_INSTROOT\" ] && exit 0\n[ -e {UPDATER_MARKER} ] && exit 0\n");
    match role {
        "controller" | "updater" => (
            // `start` of a running procd service with an unchanged command line does not restart it
            format!("{head}/etc/init.d/{package} enable\n/etc/init.d/{package} start\nexit 0\n"),
            format!("{head}/etc/init.d/{package} stop\n/etc/init.d/{package} disable\nexit 0\n"),
        ),
        "ui-client" => (
            // luci.mk's postinst: drop LuCI caches, make rpcd load the new ACL and ubus object
            format!("{head}rm -f /tmp/luci-indexcache /tmp/luci-indexcache.*\nrm -rf /tmp/luci-modulecache/\nkillall -HUP rpcd 2>/dev/null\nexit 0\n"),
            String::new(),
        ),
        "cli" => (String::new(), String::new()),
        _ => {
            let rescan = "[ -S /var/run/meduza-np/planner.sock ] && /usr/bin/meduza-np rescan >/dev/null 2>&1\nexit 0\n";
            (format!("{head}{rescan}"), String::new())
        }
    }
}

pub fn file_name(package: &str, version: &str, arch: &str, format: &str) -> String {
    format!("{package}_{version}_{arch}.{format}")
}

/// `arch` as written into the package: opkg says `all`, apk says `noarch`.
pub fn pkg_arch(package: &str, arch: &str, format: &str) -> String {
    match (noarch(package), format) {
        (true, "apk") => "noarch".into(),
        (true, _) => "all".into(),
        _ => arch.into(),
    }
}

pub fn platform(package: &str, arch: &str) -> String {
    if noarch(package) {
        "any".into()
    } else {
        format!("openwrt/{arch}")
    }
}

fn read(src: &Src) -> Result<Vec<u8>, String> {
    match src {
        Src::Bytes(b) => Ok(b.clone()),
        Src::File(p) => std::fs::read(p).map_err(|e| format!("{}: {e}", p.display())),
    }
}

/// Appends an entry under its literal name. The tar crate drops `./` from names it sets, but opkg looks the outer
/// members up as `./control.tar.gz` etc., so names that fit the 100-byte field are written raw (longer ones, only
/// possible deep inside provider trees, fall back to the crate's GNU long-name handling).
fn append_raw<W: std::io::Write>(b: &mut tar::Builder<W>, mut h: tar::Header, name: &str, data: &[u8]) -> Result<(), String> {
    let raw = name.as_bytes();
    if raw.len() >= 100 {
        return b.append_data(&mut h, name, data).map_err(|e| e.to_string());
    }
    let field = &mut h.as_old_mut().name;
    field.fill(0);
    field[..raw.len()].copy_from_slice(raw);
    h.set_cksum();
    b.append(&h, data).map_err(|e| e.to_string())
}

/// Deterministic gzipped tar with `./` paths, directories first (as ipkg-build writes them).
fn tar_gz(files: &BTreeMap<String, (Vec<u8>, u32)>, dirs: bool) -> Result<Vec<u8>, String> {
    let gz = flate2::GzBuilder::new().mtime(0).write(Vec::new(), flate2::Compression::best());
    let mut b = tar::Builder::new(gz);
    b.mode(tar::HeaderMode::Deterministic);
    let header = |kind: tar::EntryType, mode: u32, size: u64| {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_mode(mode);
        h.set_size(size);
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_cksum();
        h
    };
    if dirs {
        let mut all = std::collections::BTreeSet::new();
        for path in files.keys() {
            let parts: Vec<&str> = path.split('/').collect();
            for i in 1..parts.len() {
                all.insert(format!("./{}/", parts[..i].join("/")));
            }
        }
        append_raw(&mut b, header(tar::EntryType::Directory, 0o755, 0), "./", &[])?;
        for d in &all {
            append_raw(&mut b, header(tar::EntryType::Directory, 0o755, 0), d, &[])?;
        }
    }
    for (path, (data, mode)) in files {
        append_raw(&mut b, header(tar::EntryType::Regular, *mode, data.len() as u64), &format!("./{path}"), data)?;
    }
    b.into_inner().map_err(|e| e.to_string())?.finish().map_err(|e| e.to_string())
}

pub fn control_text(c: &Control) -> String {
    let mut s = format!("Package: {}\nVersion: {}\n", c.name, c.version);
    if !c.depends.is_empty() {
        s.push_str(&format!("Depends: {}\n", c.depends.join(", ")));
    }
    if !c.conflicts.is_empty() {
        s.push_str(&format!("Conflicts: {}\n", c.conflicts.join(", ")));
    }
    s.push_str(&format!(
        "Source: packages/{0}\nSourceName: {0}\nLicense: {1}\nSection: net\nArchitecture: {2}\nInstalled-Size: {3}\nMaintainer: Meduza NP\nDescription: {4}\n",
        c.name, c.license, c.arch, c.installed_size, c.description
    ));
    s
}

/// Writes the `.ipk` and returns its artifact descriptor.
pub fn write_ipk(data: &BTreeMap<String, (Src, bool)>, c: &Control, out: &Path, platform: &str) -> Result<Artifact, String> {
    let mut files = BTreeMap::new();
    for (path, (src, exec)) in data {
        files.insert(path.clone(), (read(src)?, if *exec { 0o755 } else { 0o644 }));
    }
    let data_tgz = tar_gz(&files, true)?;
    let mut ctl = BTreeMap::new();
    ctl.insert("control".to_string(), (control_text(c).into_bytes(), 0o644));
    if !c.postinst.is_empty() {
        ctl.insert("postinst".into(), (c.postinst.clone().into_bytes(), 0o755));
    }
    if !c.prerm.is_empty() {
        ctl.insert("prerm".into(), (c.prerm.clone().into_bytes(), 0o755));
    }
    let control_tgz = tar_gz(&ctl, false)?;
    // outer order as ipkg-build: debian-binary, data, control (BTreeMap would sort, so write explicitly)
    let gz = flate2::GzBuilder::new().mtime(0).write(Vec::new(), flate2::Compression::best());
    let mut b = tar::Builder::new(gz);
    b.mode(tar::HeaderMode::Deterministic);
    for (name, bytes) in [("./debian-binary", b"2.0\n".to_vec()), ("./data.tar.gz", data_tgz), ("./control.tar.gz", control_tgz)] {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_size(bytes.len() as u64);
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_cksum();
        append_raw(&mut b, h, name, &bytes)?;
    }
    let bytes = b.into_inner().map_err(|e| e.to_string())?.finish().map_err(|e| e.to_string())?;
    std::fs::write(out, &bytes).map_err(|e| format!("{}: {e}", out.display()))?;
    let (size, sha256) = meduza_np_updater::store::sha256_file(out)?;
    Ok(Artifact { platform: platform.into(), format: "ipk".into(), url: String::new(), size, sha256, chunks: vec![], oci: None })
}

/// What `artifact --stage` needs to describe the package `mkpkg.sh` produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StageInfo {
    pub package: String,
    pub version: String,
    pub file: String,
    pub platform: String,
    pub format: String,
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Writes `<out>/<file>.stage/` with `root/`, the scripts, `mkpkg.sh` (writes `../<file>`) and `stage.json`.
/// File modes and mtimes are fixed so `apk mkpkg` sees the same tree on every runner.
pub fn write_apk_stage(
    data: &BTreeMap<String, (Src, bool)>,
    c: &Control,
    out: &Path,
    file: &str,
    platform: &str,
) -> Result<PathBuf, String> {
    let stage = out.join(format!("{file}.stage"));
    let _ = std::fs::remove_dir_all(&stage);
    let root = stage.join("root");
    std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    for (path, (src, exec)) in data {
        let dest = root.join(path);
        std::fs::create_dir_all(dest.parent().unwrap_or(&root)).map_err(|e| e.to_string())?;
        std::fs::write(&dest, read(src)?).map_err(|e| format!("{}: {e}", dest.display()))?;
        fix_meta(&dest, if *exec { 0o755 } else { 0o644 })?;
    }
    let mut args = vec![
        format!("name:{}", c.name),
        format!("version:{}", c.version),
        format!("arch:{}", c.arch),
        format!("description:{}", c.description),
        format!("license:{}", c.license),
        format!("origin:{}", c.name),
        "maintainer:Meduza NP".to_string(),
    ];
    if !c.depends.is_empty() || !c.conflicts.is_empty() {
        let mut d = c.depends.clone();
        d.extend(c.conflicts.iter().map(|x| format!("!{x}")));
        args.push(format!("depends:{}", d.join(" ")));
    }
    let mut cmd = String::from("#!/bin/sh\n# generated by meduza-np-release openwrt; run inside the stage directory\nset -e\n");
    cmd.push_str("find root -exec touch -h -d @0 {} +\nexec apk mkpkg");
    for a in &args {
        cmd.push_str(&format!(" \\\n  --info {}", sh_quote(a)));
    }
    for (name, kind, body) in [("post-install", "post-install", &c.postinst), ("pre-deinstall", "pre-deinstall", &c.prerm)] {
        if !body.is_empty() {
            let p = stage.join(name);
            std::fs::write(&p, body).map_err(|e| e.to_string())?;
            fix_meta(&p, 0o755)?;
            cmd.push_str(&format!(" \\\n  --script {kind}:{name}"));
        }
    }
    cmd.push_str(&format!(" \\\n  --files root --output {}\n", sh_quote(&format!("../{file}"))));
    std::fs::write(stage.join("mkpkg.sh"), cmd).map_err(|e| e.to_string())?;
    let info = StageInfo {
        package: c.name.clone(),
        version: c.version.clone(),
        file: file.into(),
        platform: platform.into(),
        format: "apk".into(),
    };
    let mut j = serde_json::to_vec_pretty(&info).map_err(|e| e.to_string())?;
    j.push(b'\n');
    std::fs::write(stage.join("stage.json"), j).map_err(|e| e.to_string())?;
    Ok(stage)
}

#[cfg(unix)]
fn fix_meta(p: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).map_err(|e| format!("{}: {e}", p.display()))
}

#[cfg(not(unix))]
fn fix_meta(_p: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn untar(bytes: &[u8]) -> BTreeMap<String, (Vec<u8>, u32)> {
        let mut a = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        let mut out = BTreeMap::new();
        for e in a.entries().unwrap() {
            let mut e = e.unwrap();
            // raw name: `./` prefixes are part of the ipk layout
            let path = String::from_utf8_lossy(&e.path_bytes()).into_owned();
            let mode = e.header().mode().unwrap();
            let mut data = vec![];
            std::io::Read::read_to_end(&mut e, &mut data).unwrap();
            out.insert(path, (data, mode));
        }
        out
    }

    #[test]
    fn arch_table_covers_every_target() {
        for (t, p) in TARGET_PLATFORMS {
            assert!(!arches_for(t).unwrap().is_empty());
            assert_eq!(arches_for(t).unwrap(), arches_for(p).unwrap());
        }
        let all = platforms(&TARGET_PLATFORMS.iter().map(|(t, _)| t.to_string()).collect::<Vec<_>>());
        assert!(all.contains(&"openwrt/x86_64".to_string()) && all.contains(&"openwrt/aarch64_cortex-a53".to_string()));
        assert!(arches_for("mips-unknown-linux-musl").is_err());
    }

    #[test]
    fn planner_ipk_is_deterministic_with_procd_service_and_scripts() {
        let tmp = std::env::temp_dir().join(format!("mnp-release-ipk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let bins = tmp.join("bin");
        std::fs::create_dir_all(&bins).unwrap();
        std::fs::write(bins.join("meduza-np-planner"), b"\x7fELF fake").unwrap();
        let repo = repo_root();
        let data = entries(&repo, "meduza-np-planner", "0.3.0", Some(&bins), None, None).unwrap();
        assert!(data.contains_key("etc/init.d/meduza-np-planner") && data["etc/init.d/meduza-np-planner"].1);
        assert!(data.contains_key("usr/bin/meduza-np-planner"));
        let c = control(&repo, "meduza-np-planner", "0.3.0", "x86_64", &data).unwrap();
        assert!(c.postinst.contains("/etc/init.d/meduza-np-planner enable") && c.postinst.contains(UPDATER_MARKER));
        let a1 = write_ipk(&data, &c, &tmp.join("a.ipk"), "openwrt/x86_64").unwrap();
        let a2 = write_ipk(&data, &c, &tmp.join("b.ipk"), "openwrt/x86_64").unwrap();
        assert_eq!(a1.sha256, a2.sha256, "deterministic");
        let outer = untar(&std::fs::read(tmp.join("a.ipk")).unwrap());
        assert_eq!(outer["./debian-binary"].0, b"2.0\n");
        let ctl = untar(&outer["./control.tar.gz"].0);
        let text = String::from_utf8(ctl["./control"].0.clone()).unwrap();
        assert!(text.contains("Package: meduza-np-planner\nVersion: 0.3.0\n") && text.contains("Architecture: x86_64\n"), "{text}");
        assert_eq!(ctl["./postinst"].1, 0o755);
        let files = untar(&outer["./data.tar.gz"].0);
        assert_eq!(files["./usr/bin/meduza-np-planner"].1, 0o755);
        assert!(!files["./etc/init.d/meduza-np-planner"].0.contains(&b'\r'));
        let manifest: Value = serde_json::from_slice(&files["./usr/share/meduza-np/packages/meduza-np-planner/manifest.json"].0).unwrap();
        assert_eq!(manifest["version"], "0.3.0");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn plugin_depends_on_planner_only_and_luci_app_is_noarch() {
        let tmp = std::env::temp_dir().join(format!("mnp-release-ow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let bins = tmp.join("bin");
        std::fs::create_dir_all(&bins).unwrap();
        std::fs::write(bins.join("meduza-np-wireguard"), b"\x7fELF fake").unwrap();
        let repo = repo_root();
        let data = entries(&repo, "meduza-np-wireguard", "0.3.0", Some(&bins), None, None).unwrap();
        let c = control(&repo, "meduza-np-wireguard", "0.3.0", "aarch64_cortex-a53", &data).unwrap();
        assert_eq!(c.depends, vec!["meduza-np-planner"]);
        assert!(c.postinst.contains("meduza-np rescan") && c.prerm.is_empty());

        let ui = entries(&repo, "meduza-np-dashboard-openwrt", "0.3.0", None, None, None).unwrap();
        assert!(ui.contains_key("www/luci-static/resources/view/meduza-np/overview.js"));
        assert!(ui["usr/libexec/rpcd/meduza-np"].1);
        assert!(ui.contains_key("usr/share/meduza-np/packages/meduza-np-dashboard-openwrt/manifest.json"));
        let c =
            control(&repo, "meduza-np-dashboard-openwrt", "0.3.0", &pkg_arch("meduza-np-dashboard-openwrt", "x86_64", "apk"), &ui).unwrap();
        assert_eq!(c.arch, "noarch");
        assert!(c.depends.contains(&"luci-base".to_string()) && c.conflicts.contains(&"meduza-np-dashboard".to_string()));
        assert_eq!(platform("meduza-np-dashboard-openwrt", "x86_64"), "any");
        let stage = write_apk_stage(&ui, &c, &tmp, "x.apk", "any").unwrap();
        let sh = std::fs::read_to_string(stage.join("mkpkg.sh")).unwrap();
        assert!(
            sh.contains("--info 'arch:noarch'")
                && sh.contains("--info 'depends:meduza-np-planner meduza-np-cli rpcd jsonfilter luci-base !meduza-np-dashboard'"),
            "{sh}"
        );
        assert!(sh.contains("--script post-install:post-install") && sh.contains("--output '../x.apk'"));
        assert!(stage.join("root/www/luci-static/resources/meduza-np/common.js").is_file());
        assert!(entries(&repo, "meduza-np-dashboard", "0.3.0", None, None, None).is_err(), "Rust dashboard is not an OpenWrt package");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
