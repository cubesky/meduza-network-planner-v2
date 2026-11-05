//! Deterministic package archives (§13.4, §15.1 step 4): a `usr/` tree with the package's binaries at the paths its
//! manifest names and its metadata under `usr/share/meduza-np/packages/<name>/`, the manifest version set to the
//! release version. Same inputs ⇒ byte-identical archive (sorted entries, fixed mtime/owner/mode, gzip mtime 0).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use meduza_np_protocol::group::Artifact;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// File content source for one archive entry.
#[derive(Debug, Clone)]
pub enum Src {
    File(PathBuf),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BuiltArtifact {
    pub package: String,
    pub version: String,
    /// Archive file name inside the output directory.
    pub file: String,
    pub artifact: Artifact,
}

/// Metadata files shipped next to the manifest (UI, schemas, provider data).
const META_DIRS: &[&str] = &["ui", "schema"];
const META_FILES: &[&str] = &["assets.json", "upstream.lock.json"];

/// Collects the entries of `package` (path inside the archive → source).
pub fn entries(
    repo: &Path,
    package: &str,
    version: &str,
    bin_dir: Option<&Path>,
    tree: Option<&Path>,
) -> Result<BTreeMap<String, (Src, bool)>, String> {
    let dir = repo.join("packages").join(package);
    let raw = std::fs::read(dir.join("manifest.json")).map_err(|e| format!("{package}: manifest: {e}"))?;
    let mut manifest: Value = serde_json::from_slice(&raw).map_err(|e| format!("{package}: {e}"))?;
    manifest["version"] = Value::String(version.to_string());
    let role = manifest["role"].as_str().unwrap_or_default().to_string();
    let mut out: BTreeMap<String, (Src, bool)> = BTreeMap::new();
    let meta = format!("usr/share/meduza-np/packages/{package}");
    let mut text = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    text.push(b'\n');
    out.insert(format!("{meta}/manifest.json"), (Src::Bytes(text), false));
    for d in META_DIRS {
        let base = dir.join(d);
        if base.is_dir() {
            for rel in walk(&base)? {
                out.insert(format!("{meta}/{d}/{}", slash(&rel)), (Src::File(base.join(&rel)), false));
            }
        }
    }
    for f in META_FILES {
        if dir.join(f).is_file() {
            out.insert(format!("{meta}/{f}"), (Src::File(dir.join(f)), false));
        }
    }

    let short = package.trim_start_matches(meduza_np_protocol::manifest::PACKAGE_PREFIX);
    let bin = |name: &str| -> Result<Src, String> {
        let d = bin_dir.ok_or_else(|| format!("{package}: --bin-dir required"))?;
        let p = d.join(name);
        if p.is_file() {
            Ok(Src::File(p))
        } else {
            Err(format!("{package}: binary {} not found", p.display()))
        }
    };
    match role.as_str() {
        "controller" => {
            out.insert("usr/bin/meduza-np-planner".into(), (bin("meduza-np-planner")?, true));
            let pk = dir.join("packaging");
            if pk.is_dir() {
                for rel in walk(&pk)? {
                    out.insert(format!("usr/share/meduza-np/packaging/planner/{}", slash(&rel)), (Src::File(pk.join(&rel)), false));
                }
            }
        }
        "cli" => {
            out.insert("usr/bin/meduza-np".into(), (bin("meduza-np")?, true));
        }
        "updater" => {
            out.insert("usr/bin/meduza-np-updater".into(), (bin("meduza-np-updater")?, true));
            // native launcher (self-update takeover) + service definitions
            out.insert("usr/lib/meduza-np/updater-run".into(), (Src::File(dir.join("packaging/updater-run.sh")), true));
            for rel in walk(&dir.join("packaging"))? {
                let r = slash(&rel);
                if r != "updater-run.sh" {
                    out.insert(format!("usr/share/meduza-np/packaging/updater/{r}"), (Src::File(dir.join("packaging").join(&rel)), false));
                }
            }
        }
        "ui-client" if package == "meduza-np-dashboard" => {
            out.insert("usr/bin/meduza-np-dashboard".into(), (bin("meduza-np-dashboard")?, true));
            // prebuilt Vite output (`pnpm --filter @meduza-np/dashboard-web build`); never sources or node_modules
            let www = dir.join("dist/www");
            if !www.join("index.html").is_file() {
                return Err(format!("{package}: {} missing; build the web UI first", www.display()));
            }
            for rel in walk(&www)? {
                out.insert(format!("usr/share/meduza-np/dashboard/www/{}", slash(&rel)), (Src::File(www.join(&rel)), false));
            }
        }
        _ => {}
    }
    if let Some(ep) = manifest["entrypoint"].as_str() {
        let at = ep.trim_start_matches('/').to_string();
        if !at.starts_with("usr/") {
            return Err(format!("{package}: entrypoint {ep} is outside /usr"));
        }
        out.insert(at, (bin(&format!("meduza-np-{short}"))?, true));
    }
    if package == "meduza-np-openvpn" {
        // Access hook helper; plugins receive `libexecDir` = /usr/libexec/meduza-np.
        out.insert("usr/libexec/meduza-np/openvpn-auth".into(), (bin("openvpn-auth")?, true));
    }
    if let Some(t) = tree {
        // Recipe output (openvpn-pkg/mosdns-pkg): provider binaries, licenses, build-info.
        for rel in walk(t)? {
            let r = slash(&rel);
            if !r.starts_with("usr/") {
                return Err(format!("{package}: tree entry {r} is outside usr/"));
            }
            if r == format!("{meta}/manifest.json") {
                continue; // ours wins (release version)
            }
            let exec = is_exec(&t.join(&rel));
            out.insert(r, (Src::File(t.join(&rel)), exec));
        }
    }
    Ok(out)
}

fn slash(p: &Path) -> String {
    p.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/")
}

#[cfg(unix)]
fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_exec(p: &Path) -> bool {
    p.to_string_lossy().contains("/bin/") || p.to_string_lossy().contains("\\bin\\") || p.to_string_lossy().contains("sbin")
}

pub fn walk(base: &Path) -> Result<Vec<PathBuf>, String> {
    fn go(base: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        let mut items: Vec<_> = std::fs::read_dir(base.join(rel))?.collect::<Result<_, _>>()?;
        items.sort_by_key(|e| e.file_name());
        for e in items {
            let r = rel.join(e.file_name());
            let ft = e.file_type()?;
            if ft.is_dir() {
                go(base, &r, out)?;
            } else if ft.is_file() {
                out.push(r);
            }
        }
        Ok(())
    }
    let mut out = vec![];
    go(base, Path::new(""), &mut out).map_err(|e| format!("{}: {e}", base.display()))?;
    Ok(out)
}

/// Writes the archive and returns its artifact descriptor.
pub fn write(entries: &BTreeMap<String, (Src, bool)>, out: &Path, platform: &str) -> Result<Artifact, String> {
    let f = std::fs::File::create(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let gz = flate2::GzBuilder::new().mtime(0).write(f, flate2::Compression::best());
    let mut b = tar::Builder::new(gz);
    b.mode(tar::HeaderMode::Deterministic);
    // directories first (sorted), then files (sorted)
    let mut dirs = std::collections::BTreeSet::new();
    for path in entries.keys() {
        let mut acc = String::new();
        for seg in path.split('/').collect::<Vec<_>>().split_last().map(|(_, d)| d).unwrap_or(&[]) {
            acc = if acc.is_empty() { seg.to_string() } else { format!("{acc}/{seg}") };
            dirs.insert(format!("{acc}/"));
        }
    }
    for d in &dirs {
        let mut h = tar::Header::new_ustar();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(0o755);
        h.set_size(0);
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_cksum();
        b.append_data(&mut h, d, std::io::empty()).map_err(|e| e.to_string())?;
    }
    for (path, (src, exec)) in entries {
        let data = match src {
            Src::Bytes(b) => b.clone(),
            Src::File(p) => std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?,
        };
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(if *exec { 0o755 } else { 0o644 });
        h.set_size(data.len() as u64);
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_cksum();
        b.append_data(&mut h, path, data.as_slice()).map_err(|e| e.to_string())?;
    }
    b.into_inner().map_err(|e| e.to_string())?.finish().map_err(|e| e.to_string())?;
    let (size, sha256) = meduza_np_updater::store::sha256_file(out)?;
    Ok(Artifact { platform: platform.to_string(), format: "tar.gz".into(), url: String::new(), size, sha256, chunks: vec![], oci: None })
}

pub fn platform_slug(platform: &str) -> String {
    platform.replace('/', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn plugin_archive_is_deterministic_and_installable() {
        let tmp = std::env::temp_dir().join(format!("mnp-release-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let bins = tmp.join("bin");
        std::fs::create_dir_all(&bins).unwrap();
        std::fs::write(bins.join("meduza-np-wireguard"), b"\x7fELF fake").unwrap();
        let e = entries(&repo_root(), "meduza-np-wireguard", "0.2.0", Some(&bins), None).unwrap();
        assert!(e.contains_key("usr/libexec/meduza-np/plugins/wireguard"));
        assert!(e.contains_key("usr/share/meduza-np/packages/meduza-np-wireguard/ui/descriptor.json"));
        let a1 = write(&e, &tmp.join("a.tar.gz"), "linux/amd64").unwrap();
        let a2 = write(&e, &tmp.join("b.tar.gz"), "linux/amd64").unwrap();
        assert_eq!(a1.sha256, a2.sha256, "deterministic");
        // The updater's own extraction + manifest check accepts it.
        let dest = tmp.join("x");
        std::fs::create_dir_all(&dest).unwrap();
        meduza_np_updater::store::extract(&tmp.join("a.tar.gz"), "tar.gz", &dest, 1 << 24).unwrap();
        meduza_np_updater::store::check_package_tree(&dest, "meduza-np-wireguard", "0.2.0").unwrap();
        assert!(entries(&repo_root(), "meduza-np-wireguard", "0.2.0", None, None).is_err(), "binary required");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
