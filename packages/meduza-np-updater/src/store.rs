//! Store backend (base-container, generic Linux). Layout under `<data>/store`:
//!
//! ```text
//! objects/<package>/<version>-<sha12>/usr/...   verified, extracted package trees (never modified)
//! views/g<generation>/usr/...                   symlink farm: one link per file into objects/
//! current -> views/g<generation>                atomically replaced symlink (rename over)
//! staging/                                      bounded downloads; cleared after each run
//! installed.json  journal.json  state.json
//! ```
//!
//! The system (or the container image) links `/usr/share/meduza-np/packages`, `/usr/libexec/meduza-np` and the
//! binaries into `current/`. Running processes keep their old inode: objects are never overwritten, and the
//! previous view and its objects are kept until the next successful update (one recoverable generation).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use meduza_np_protocol::group::{Artifact, InstalledSet};
use meduza_np_protocol::manifest::PackageManifest;
use sha2::{Digest, Sha256};

pub const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FILES_PER_PACKAGE: usize = 20_000;

pub struct Store {
    pub root: PathBuf,
}

fn io(e: std::io::Error, what: &Path) -> String {
    format!("{}: {e}", what.display())
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn objects(&self) -> PathBuf {
        self.root.join("objects")
    }
    pub fn views(&self) -> PathBuf {
        self.root.join("views")
    }
    pub fn staging(&self) -> PathBuf {
        self.root.join("staging")
    }
    pub fn current_link(&self) -> PathBuf {
        self.root.join("current")
    }
    pub fn installed_path(&self) -> PathBuf {
        self.root.join("installed.json")
    }
    pub fn journal_path(&self) -> PathBuf {
        self.root.join("journal.json")
    }
    pub fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }

    pub fn init(&self) -> Result<(), String> {
        for d in [self.root.clone(), self.objects(), self.views(), self.staging()] {
            meduza_np_runtime::fsutil::ensure_dir(&d, 0o755).map_err(|e| io(e, &d))?;
        }
        Ok(())
    }

    pub fn load_installed(&self) -> Result<Option<InstalledSet>, String> {
        meduza_np_runtime::fsutil::read_json(&self.installed_path(), 1024 * 1024).map_err(|e| io(e, &self.installed_path()))
    }

    pub fn save_installed(&self, s: &InstalledSet) -> Result<(), String> {
        meduza_np_runtime::fsutil::write_json_atomic(&self.installed_path(), s, 0o644).map_err(|e| io(e, &self.installed_path()))
    }

    pub fn object_name(version: &str, sha256: &str) -> String {
        format!("{version}-{}", &sha256[..12.min(sha256.len())])
    }

    pub fn object_dir(&self, package: &str, object: &str) -> PathBuf {
        self.objects().join(package).join(object)
    }

    /// Verifies a downloaded artifact (size + SHA-256) and extracts it into its object directory, unless that
    /// object already exists (identical bits are reused, e.g. an unchanged package or a rollback).
    pub fn install_object(&self, package: &str, version: &str, artifact: &Artifact, file: &Path) -> Result<String, String> {
        let object = Self::object_name(version, &artifact.sha256.to_ascii_lowercase());
        let dest = self.object_dir(package, &object);
        if dest.join("usr").is_dir() {
            return Ok(object);
        }
        verify_file(file, artifact.size, &artifact.sha256)?;
        let tmp = self.objects().join(package).join(format!(".{object}.tmp"));
        let _ = std::fs::remove_dir_all(&tmp);
        meduza_np_runtime::fsutil::ensure_dir(&tmp, 0o755).map_err(|e| io(e, &tmp))?;
        let r = extract(file, &artifact.format, &tmp, artifact.size.saturating_mul(12).clamp(1 << 20, 1 << 30))
            .and_then(|_| check_package_tree(&tmp, package, version));
        if let Err(e) = r {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!("{package} {version}: {e}"));
        }
        std::fs::rename(&tmp, &dest).map_err(|e| io(e, &dest))?;
        let _ = meduza_np_runtime::fsutil::sync_dir(&self.objects().join(package));
        Ok(object)
    }

    /// Builds `views/g<generation>` for `objects` (package → object dir name). Two packages shipping the same file
    /// is an error (ownership must be unambiguous).
    pub fn build_view(&self, generation: u64, objects: &BTreeMap<String, String>) -> Result<String, String> {
        let name = format!("g{generation}");
        let tmp = self.views().join(format!(".{name}.tmp"));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut owners: BTreeMap<PathBuf, String> = BTreeMap::new();
        for (package, object) in objects {
            let base = self.object_dir(package, object);
            let mut files = Vec::new();
            walk(&base, Path::new(""), &mut files).map_err(|e| io(e, &base))?;
            for rel in files {
                if let Some(other) = owners.insert(rel.clone(), package.clone()) {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Err(format!("{} is shipped by both {other} and {package}", rel.display()));
                }
                let link = tmp.join(&rel);
                if let Some(parent) = link.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| io(e, parent))?;
                }
                // Relative target so the volume may be mounted anywhere (planner and updater containers).
                let depth = rel.components().count() + 1; // + views/<name>/ … minus the file itself, plus "views"
                let mut target = PathBuf::new();
                for _ in 0..depth {
                    target.push("..");
                }
                target.push("objects");
                target.push(package);
                target.push(object);
                target.push(&rel);
                link_file(&target, &base.join(&rel), &link).map_err(|e| io(e, &link))?;
            }
        }
        std::fs::create_dir_all(&tmp).map_err(|e| io(e, &tmp))?;
        let dest = self.views().join(&name);
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::rename(&tmp, &dest).map_err(|e| io(e, &dest))?;
        Ok(name)
    }

    pub fn current_view(&self) -> Option<String> {
        read_pointer(&self.current_link())
    }

    /// Atomically points `current` at `view`. Returns the previous view.
    pub fn switch(&self, view: &str) -> Result<Option<String>, String> {
        if !self.views().join(view).is_dir() {
            return Err(format!("view {view} does not exist"));
        }
        let previous = self.current_view();
        let tmp = self.root.join("current.new");
        let _ = remove_pointer(&tmp);
        write_pointer(&Path::new("views").join(view), &tmp).map_err(|e| io(e, &tmp))?;
        std::fs::rename(&tmp, self.current_link()).map_err(|e| io(e, &self.current_link()))?;
        let _ = meduza_np_runtime::fsutil::sync_dir(&self.root);
        Ok(previous)
    }

    /// Resolves a path inside the current view (`usr/bin/meduza-np-planner`).
    pub fn current_path(&self, rel: &str) -> Option<PathBuf> {
        self.current_view().map(|v| self.views().join(v).join(rel))
    }

    /// Removes views not in `keep_views` and objects not referenced by the kept installed sets.
    pub fn gc(&self, keep_views: &[String], keep_objects: &BTreeSet<(String, String)>) {
        if let Ok(rd) = std::fs::read_dir(self.views()) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if !keep_views.contains(&n) {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(self.objects()) {
            for pkg in rd.flatten() {
                let pname = pkg.file_name().to_string_lossy().into_owned();
                if let Ok(objs) = std::fs::read_dir(pkg.path()) {
                    for o in objs.flatten() {
                        let oname = o.file_name().to_string_lossy().into_owned();
                        if !keep_objects.contains(&(pname.clone(), oname)) {
                            let _ = std::fs::remove_dir_all(o.path());
                        }
                    }
                }
                let _ = std::fs::remove_dir(pkg.path()); // only succeeds when empty
            }
        }
        let _ = std::fs::remove_dir_all(self.staging());
        let _ = std::fs::create_dir_all(self.staging());
    }
}

pub fn sha256_file(path: &Path) -> Result<(u64, String), String> {
    let mut f = std::fs::File::open(path).map_err(|e| io(e, path))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        let r = f.read(&mut buf).map_err(|e| io(e, path))?;
        if r == 0 {
            break;
        }
        n += r as u64;
        h.update(&buf[..r]);
    }
    Ok((n, hex::encode(h.finalize())))
}

pub fn verify_file(path: &Path, size: u64, sha256: &str) -> Result<(), String> {
    let (n, got) = sha256_file(path)?;
    if n != size {
        return Err(format!("size {n} != declared {size}"));
    }
    if !got.eq_ignore_ascii_case(sha256) {
        return Err(format!("sha256 {got} != declared {sha256}"));
    }
    Ok(())
}

fn decoder<'a>(file: std::fs::File, format: &str) -> Result<Box<dyn Read + 'a>, String> {
    let r = std::io::BufReader::new(file);
    match format {
        "tar.gz" => Ok(Box::new(flate2::read::GzDecoder::new(r))),
        "tar.zst" => Ok(Box::new(ruzstd::decoding::StreamingDecoder::new(r).map_err(|e| format!("zstd: {e}"))?)),
        other => Err(format!("unsupported archive format {other}")),
    }
}

/// Extracts a package archive. Only regular files and directories under `usr/` are accepted; absolute paths,
/// `..`, links and device nodes are rejected; total size and file count are bounded.
pub fn extract(file: &Path, format: &str, dest: &Path, max_total: u64) -> Result<(), String> {
    let f = std::fs::File::open(file).map_err(|e| io(e, file))?;
    let mut ar = tar::Archive::new(decoder(f, format)?);
    let mut total = 0u64;
    let mut count = 0usize;
    for entry in ar.entries().map_err(|e| format!("archive: {e}"))? {
        let mut entry = entry.map_err(|e| format!("archive: {e}"))?;
        let path = entry.path().map_err(|e| format!("archive path: {e}"))?.into_owned();
        let clean: PathBuf = path.components().filter(|c| !matches!(c, Component::CurDir)).collect();
        if clean.as_os_str().is_empty() {
            continue;
        }
        let safe = clean.components().all(|c| matches!(c, Component::Normal(_)));
        let first_ok = clean.components().next().map(|c| c.as_os_str() == "usr").unwrap_or(false);
        if !safe || !first_ok {
            return Err(format!("archive entry {} is outside usr/", path.display()));
        }
        let out = dest.join(&clean);
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                std::fs::create_dir_all(&out).map_err(|e| io(e, &out))?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                count += 1;
                total += entry.header().size().unwrap_or(0);
                if count > MAX_FILES_PER_PACKAGE || total > max_total {
                    return Err("archive too large".into());
                }
                if let Some(parent) = out.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| io(e, parent))?;
                }
                let mode = entry.header().mode().unwrap_or(0o644);
                let mut w = std::fs::File::create(&out).map_err(|e| io(e, &out))?;
                std::io::copy(&mut (&mut entry).take(max_total), &mut w).map_err(|e| io(e, &out))?;
                w.sync_all().map_err(|e| io(e, &out))?;
                meduza_np_runtime::fsutil::set_mode(&out, if mode & 0o111 != 0 { 0o755 } else { 0o644 }).map_err(|e| io(e, &out))?;
            }
            other => return Err(format!("archive entry {} has unsupported type {other:?}", path.display())),
        }
    }
    Ok(())
}

/// The extracted tree must carry the package's own manifest with the expected name and version.
pub fn check_package_tree(root: &Path, package: &str, version: &str) -> Result<(), String> {
    let mpath = root.join("usr/share/meduza-np/packages").join(package).join("manifest.json");
    let m: PackageManifest = meduza_np_runtime::fsutil::read_json(&mpath, 256 * 1024)
        .map_err(|e| io(e, &mpath))?
        .ok_or_else(|| format!("package has no {}", mpath.strip_prefix(root).unwrap_or(&mpath).display()))?;
    if m.package != package || m.version != version {
        return Err(format!("archive holds {} {}, expected {package} {version}", m.package, m.version));
    }
    let v = m.validate();
    if !v.is_empty() {
        return Err(format!("invalid manifest: {:?}", v.errors()));
    }
    Ok(())
}

fn walk(base: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for e in std::fs::read_dir(base.join(rel))? {
        let e = e?;
        let r = rel.join(e.file_name());
        let ft = e.file_type()?;
        if ft.is_dir() {
            walk(base, &r, out)?;
        } else if ft.is_file() {
            out.push(r);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn link_file(rel_target: &Path, _abs_target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(rel_target, link)
}

#[cfg(not(unix))]
fn link_file(_rel_target: &Path, abs_target: &Path, link: &Path) -> std::io::Result<()> {
    // Development hosts without symlink privileges: hard links keep the same "never copy, never overwrite" model.
    std::fs::hard_link(abs_target, link)
}

#[cfg(unix)]
fn write_pointer(target: &Path, at: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}

#[cfg(not(unix))]
fn write_pointer(target: &Path, at: &Path) -> std::io::Result<()> {
    std::fs::write(at, target.to_string_lossy().replace('\\', "/"))
}

#[cfg(unix)]
fn read_pointer(at: &Path) -> Option<String> {
    let t = std::fs::read_link(at).ok()?;
    t.file_name().map(|n| n.to_string_lossy().into_owned())
}

#[cfg(not(unix))]
fn read_pointer(at: &Path) -> Option<String> {
    let t = std::fs::read_to_string(at).ok()?;
    t.rsplit('/').next().map(str::to_string)
}

fn remove_pointer(at: &Path) -> std::io::Result<()> {
    std::fs::remove_file(at)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a `tar.gz` package archive with the given extra files (path, content, executable).
    pub fn make_package(dir: &Path, name: &str, version: &str, extra: &[(&str, &str, bool)]) -> (PathBuf, Artifact) {
        let manifest = serde_json::json!({
            "manifestVersion": 1, "package": name, "version": version, "role": "cli",
            "plannerApi": {"min": 1, "max": 1}, "requires": [], "conflicts": [], "provides": [format!("{}.test", name.trim_start_matches("meduza-np-"))]
        });
        let path = dir.join(format!("{name}-{version}.tar.gz"));
        let f = std::fs::File::create(&path).unwrap();
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(f, flate2::Compression::fast()));
        let mut add = |p: &str, data: &[u8], mode: u32| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(mode);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            b.append_data(&mut h, p, data).unwrap();
        };
        add(&format!("usr/share/meduza-np/packages/{name}/manifest.json"), manifest.to_string().as_bytes(), 0o644);
        for (p, c, x) in extra {
            add(p, c.as_bytes(), if *x { 0o755 } else { 0o644 });
        }
        b.into_inner().unwrap().finish().unwrap();
        let (size, sha256) = sha256_file(&path).unwrap();
        let artifact = Artifact { platform: "linux/amd64".into(), format: "tar.gz".into(), url: String::new(), size, sha256, oci: None };
        (path, artifact)
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mnp-upd-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn install_view_switch_gc() {
        let d = tmpdir("flow");
        let store = Store::new(d.join("store"));
        store.init().unwrap();
        let (a1, art1) = make_package(&d, "meduza-np-planner", "0.1.0", &[("usr/bin/meduza-np-planner", "v1", true)]);
        let (w1, artw) = make_package(&d, "meduza-np-wireguard", "0.1.0", &[("usr/libexec/meduza-np/plugins/wireguard", "wg", true)]);
        let o1 = store.install_object("meduza-np-planner", "0.1.0", &art1, &a1).unwrap();
        let ow = store.install_object("meduza-np-wireguard", "0.1.0", &artw, &w1).unwrap();
        let mut objs = BTreeMap::from([("meduza-np-planner".to_string(), o1.clone()), ("meduza-np-wireguard".to_string(), ow.clone())]);
        let v1 = store.build_view(1, &objs).unwrap();
        assert_eq!(store.switch(&v1).unwrap(), None);
        let bin = store.current_path("usr/bin/meduza-np-planner").unwrap();
        assert_eq!(std::fs::read_to_string(&bin).unwrap(), "v1");

        let (a2, art2) = make_package(&d, "meduza-np-planner", "0.2.0", &[("usr/bin/meduza-np-planner", "v2", true)]);
        let o2 = store.install_object("meduza-np-planner", "0.2.0", &art2, &a2).unwrap();
        objs.insert("meduza-np-planner".into(), o2.clone());
        let v2 = store.build_view(2, &objs).unwrap();
        assert_eq!(store.switch(&v2).unwrap().as_deref(), Some("g1"));
        assert_eq!(std::fs::read_to_string(store.current_path("usr/bin/meduza-np-planner").unwrap()).unwrap(), "v2");
        // the old object is still there for the running process / rollback
        assert_eq!(std::fs::read_to_string(store.object_dir("meduza-np-planner", &o1).join("usr/bin/meduza-np-planner")).unwrap(), "v1");
        // rollback = switch back
        store.switch("g1").unwrap();
        assert_eq!(std::fs::read_to_string(store.current_path("usr/bin/meduza-np-planner").unwrap()).unwrap(), "v1");
        store.switch("g2").unwrap();

        let keep: BTreeSet<(String, String)> =
            [("meduza-np-planner".to_string(), o2), ("meduza-np-wireguard".to_string(), ow)].into_iter().collect();
        store.gc(&["g2".into()], &keep);
        assert!(!store.object_dir("meduza-np-planner", &o1).exists());
        assert!(!store.views().join("g1").exists());
        assert_eq!(std::fs::read_to_string(store.current_path("usr/libexec/meduza-np/plugins/wireguard").unwrap()).unwrap(), "wg");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rejects_tampering_traversal_wrong_manifest_and_overlap() {
        let d = tmpdir("bad");
        let store = Store::new(d.join("store"));
        store.init().unwrap();
        let (a, mut art) = make_package(&d, "meduza-np-planner", "0.1.0", &[]);
        art.sha256 = "0".repeat(64);
        assert!(store.install_object("meduza-np-planner", "0.1.0", &art, &a).unwrap_err().contains("sha256"));
        let (a, art) = make_package(&d, "meduza-np-planner", "0.1.0", &[]);
        assert!(store.install_object("meduza-np-planner", "0.2.0", &art, &a).unwrap_err().contains("expected"));
        let (a, art) = make_package(&d, "meduza-np-planner", "0.1.0", &[("etc/passwd", "x", false)]);
        assert!(store.install_object("meduza-np-planner", "0.1.0", &art, &a).unwrap_err().contains("outside usr/"));
        // overlapping files between two packages
        let (a, art) = make_package(&d, "meduza-np-frr", "0.1.0", &[("usr/bin/shared", "1", true)]);
        let of = store.install_object("meduza-np-frr", "0.1.0", &art, &a).unwrap();
        let (b, artb) = make_package(&d, "meduza-np-tinc", "0.1.0", &[("usr/bin/shared", "2", true)]);
        let ot = store.install_object("meduza-np-tinc", "0.1.0", &artb, &b).unwrap();
        let objs = BTreeMap::from([("meduza-np-frr".to_string(), of), ("meduza-np-tinc".to_string(), ot)]);
        assert!(store.build_view(5, &objs).unwrap_err().contains("shipped by both"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
