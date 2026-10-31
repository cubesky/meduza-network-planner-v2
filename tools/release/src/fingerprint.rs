//! Build-input fingerprints (§15.2). A package's fingerprint covers its own files, the files of every workspace crate
//! in its resolved (normal + build) dependency closure, the exact external crates (name, version, source, checksum,
//! enabled features) of that closure, the toolchain, release profile, targets and the archive layout code. Lockfile
//! entries of crates the package does not use do not change it; test/example code does not either.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::archive::walk;

const SKIP_DIRS: &[&str] = &["target", "node_modules", "dist", "out", "tests", "examples", ".vite"];

/// Canonical path without the Windows verbatim prefix, so paths from `cargo metadata` and ours compare equal.
fn norm(p: &Path) -> PathBuf {
    let c = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let s = c.to_string_lossy();
    PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
}

fn sha(bytes: &[u8]) -> String {
    meduza_np_protocol::digest::sha256_hex(bytes)
}

fn file_lines(repo: &Path, dir: &Path, out: &mut BTreeSet<String>) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    for rel in walk(dir)? {
        if rel.components().any(|c| SKIP_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref())) {
            continue;
        }
        let p = dir.join(&rel);
        let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        // normalize CRLF so Windows checkouts fingerprint like Linux CI
        let norm: Vec<u8> = if bytes.contains(&b'\r') { String::from_utf8_lossy(&bytes).replace("\r\n", "\n").into_bytes() } else { bytes };
        let shown =
            p.strip_prefix(repo).map_err(|_| format!("{} is outside the repository", p.display()))?.to_string_lossy().replace('\\', "/");
        out.insert(format!("file {shown} {}", sha(&norm)));
    }
    Ok(())
}

/// `[profile.*]` sections of the root Cargo.toml (members/dependency tables are covered by the resolve graph).
fn profile_text(cargo_toml: &str) -> String {
    let mut keep = false;
    let mut out = String::new();
    for line in cargo_toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            keep = t.starts_with("[profile");
        }
        if keep {
            out.push_str(t);
            out.push('\n');
        }
    }
    out
}

pub struct Ctx {
    pub repo: PathBuf,
    pub targets: Vec<String>,
    /// `cargo metadata --format-version 1 --locked --filter-platform <first target>`
    pub metadata: Option<Value>,
    pub lock_checksums: BTreeMap<(String, String), String>,
}

impl Ctx {
    pub fn load(repo: &Path, targets: Vec<String>, metadata: Option<Value>) -> Result<Self, String> {
        let lock = std::fs::read_to_string(repo.join("Cargo.lock")).unwrap_or_default();
        let mut lock_checksums = BTreeMap::new();
        let (mut name, mut version) = (String::new(), String::new());
        for line in lock.lines() {
            if let Some(v) = line.strip_prefix("name = ") {
                name = v.trim_matches('"').into();
            } else if let Some(v) = line.strip_prefix("version = ") {
                version = v.trim_matches('"').into();
            } else if let Some(v) = line.strip_prefix("checksum = ") {
                lock_checksums.insert((name.clone(), version.clone()), v.trim_matches('"').into());
            }
        }
        Ok(Self { repo: norm(repo), targets, metadata, lock_checksums })
    }

    fn common_lines(&self, out: &mut BTreeSet<String>) {
        out.insert("format 1".into());
        out.insert(format!("targets {}", self.targets.join(",")));
        let read = |p: &str| std::fs::read_to_string(self.repo.join(p)).unwrap_or_default().replace("\r\n", "\n");
        out.insert(format!("toolchain {}", sha(read("rust-toolchain.toml").as_bytes())));
        out.insert(format!("profile {}", sha(profile_text(&read("Cargo.toml")).as_bytes())));
        out.insert(format!("layout {}", sha(read("tools/release/src/archive.rs").as_bytes())));
        // distribution image layout + the image records in the group (D30)
        out.insert(format!("oci {}", sha(read("tools/release/src/oci.rs").as_bytes())));
    }

    /// Fingerprint + the sorted input lines (for diagnostics / the published change matrix).
    pub fn fingerprint(&self, package: &str) -> Result<(String, Vec<String>), String> {
        let mut lines = BTreeSet::new();
        self.common_lines(&mut lines);
        let dir = self.repo.join("packages").join(package);
        file_lines(&self.repo, &dir, &mut lines)?;
        if matches!(package, "meduza-np-manager" | "meduza-np-dashboard") {
            for shared in ["web/ui-kit", "web/descriptor-renderer", "web/tsconfig.base.json"] {
                let p = self.repo.join(shared);
                if p.is_file() {
                    let b = std::fs::read(&p).map_err(|e| e.to_string())?;
                    lines.insert(format!("file {shared} {}", sha(&b)));
                } else {
                    file_lines(&self.repo, &p, &mut lines)?;
                }
            }
            let lock = std::fs::read(self.repo.join("pnpm-lock.yaml")).unwrap_or_default();
            lines.insert(format!("pnpm-lock {}", sha(&lock)));
        }
        if package == "meduza-np-manager" {
            // the image also carries the generated schemas and every package's manifest/UI/schema (Containerfile)
            file_lines(&self.repo, &self.repo.join("schemas"), &mut lines)?;
            let mut names: Vec<String> = std::fs::read_dir(self.repo.join("packages"))
                .map_err(|e| e.to_string())?
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n != package)
                .collect();
            names.sort();
            for n in names {
                let d = self.repo.join("packages").join(&n);
                if !d.join("manifest.json").is_file() {
                    continue;
                }
                let b = std::fs::read(d.join("manifest.json")).map_err(|e| e.to_string())?;
                let text = String::from_utf8_lossy(&b).replace("\r\n", "\n");
                lines.insert(format!("file packages/{n}/manifest.json {}", sha(text.as_bytes())));
                for sub in ["ui", "schema"] {
                    file_lines(&self.repo, &d.join(sub), &mut lines)?;
                }
            }
        }
        if package == "meduza-np-base-container" {
            // the image embeds the released updater: a new updater version is a new base image (§15.1)
            let (fp, _) = self.fingerprint("meduza-np-updater")?;
            lines.insert(format!("embedded meduza-np-updater {fp}"));
        }
        if crate::openwrt::OPENWRT_PACKAGES.contains(&package) {
            // OpenWrt layout, apk builder (pinned image) and arch table; the updater also ships the trust store
            let mut inputs = crate::openwrt::LAYOUT_FILES.to_vec();
            if package == "meduza-np-updater" {
                inputs.push(crate::openwrt::TRUST_FILE);
            }
            for f in inputs {
                let text = std::fs::read_to_string(self.repo.join(f)).unwrap_or_default().replace("\r\n", "\n");
                lines.insert(format!("openwrt {f} {}", sha(text.as_bytes())));
            }
        }
        if let Some(meta) = &self.metadata {
            self.rust_lines(meta, package, &mut lines)?;
        }
        let text = lines.iter().cloned().collect::<Vec<_>>().join("\n");
        Ok((sha(text.as_bytes()), lines.into_iter().collect()))
    }

    fn rust_lines(&self, meta: &Value, package: &str, out: &mut BTreeSet<String>) -> Result<(), String> {
        for c in rust_closure(meta, package)? {
            if c.local {
                // workspace crate: its sources (the root package's own files are already included)
                if c.name != package {
                    file_lines(&self.repo, &norm(&c.dir), out)?;
                }
                out.insert(format!("crate {} local features={}", c.name, c.features.join(",")));
            } else {
                out.insert(format!("crate {} {} {} {} features={}", c.name, c.version, c.source, self.checksum(&c), c.features.join(",")));
            }
        }
        Ok(())
    }

    /// Cargo.lock checksum of a registry crate (empty for git/path sources).
    pub fn checksum(&self, c: &ClosureCrate) -> String {
        self.lock_checksums.get(&(c.name.clone(), c.version.clone())).cloned().unwrap_or_default()
    }
}

/// One crate of a package's resolved (normal + build) dependency closure.
#[derive(Debug, Clone)]
pub struct ClosureCrate {
    pub id: String,
    pub name: String,
    pub version: String,
    /// `registry+https://…` / `git+…`; empty for workspace crates.
    pub source: String,
    pub local: bool,
    pub dir: PathBuf,
    pub license: Option<String>,
    pub features: Vec<String>,
    /// Ids of the normal/build dependencies.
    pub deps: Vec<String>,
}

/// The resolved closure of `package` (empty when it is not a Rust package), root first, then sorted by id.
pub fn rust_closure(meta: &Value, package: &str) -> Result<Vec<ClosureCrate>, String> {
    let packages = meta["packages"].as_array().cloned().unwrap_or_default();
    let by_id: BTreeMap<String, &Value> = packages.iter().filter_map(|p| p["id"].as_str().map(|id| (id.to_string(), p))).collect();
    let Some(root) = packages.iter().find(|p| p["name"] == package && p["source"].is_null()) else {
        return Ok(Vec::new());
    };
    let nodes: BTreeMap<String, &Value> =
        meta["resolve"]["nodes"].as_array().into_iter().flatten().filter_map(|n| n["id"].as_str().map(|id| (id.to_string(), n))).collect();
    let root_id = root["id"].as_str().unwrap_or_default().to_string();
    let mut found: BTreeMap<String, ClosureCrate> = BTreeMap::new();
    let mut stack = vec![root_id.clone()];
    while let Some(id) = stack.pop() {
        if found.contains_key(&id) {
            continue;
        }
        let node = nodes.get(&id).ok_or_else(|| format!("{id} missing from resolve graph"))?;
        let pkg = by_id.get(&id).ok_or_else(|| format!("{id} missing from packages"))?;
        let deps: Vec<String> = node["deps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| d["dep_kinds"].as_array().into_iter().flatten().any(|k| k["kind"].is_null() || k["kind"] == "build"))
            .filter_map(|d| d["pkg"].as_str().map(str::to_string))
            .collect();
        stack.extend(deps.iter().cloned());
        let manifest = PathBuf::from(pkg["manifest_path"].as_str().unwrap_or_default());
        found.insert(
            id.clone(),
            ClosureCrate {
                id,
                name: pkg["name"].as_str().unwrap_or_default().to_string(),
                version: pkg["version"].as_str().unwrap_or_default().to_string(),
                source: pkg["source"].as_str().unwrap_or_default().to_string(),
                local: pkg["source"].is_null(),
                dir: manifest.parent().unwrap_or(Path::new(".")).to_path_buf(),
                license: pkg["license"].as_str().map(str::to_string),
                features: node["features"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect(),
                deps,
            },
        );
    }
    let root = found.remove(&root_id).expect("root visited");
    Ok(std::iter::once(root).chain(found.into_values()).collect())
}

/// Runs `cargo metadata` for the workspace, resolved for `target`.
pub fn cargo_metadata(repo: &Path, target: &str) -> Result<Value, String> {
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--locked", "--filter-platform", target])
        .current_dir(repo)
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!("cargo metadata: {}", String::from_utf8_lossy(&out.stderr)));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closure_scoped_fingerprints() {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let meta = cargo_metadata(&repo, "x86_64-unknown-linux-musl").unwrap();
        let ctx = Ctx::load(&repo, vec!["x86_64-unknown-linux-musl".into()], Some(meta)).unwrap();
        let (wg, wg_lines) = ctx.fingerprint("meduza-np-wireguard").unwrap();
        let (planner, planner_lines) = ctx.fingerprint("meduza-np-planner").unwrap();
        assert_ne!(wg, planner);
        // shared protocol crate is part of both closures; planner's private deps are not part of wireguard's
        assert!(wg_lines.iter().any(|l| l.starts_with("file crates/protocol/src/lib.rs")));
        assert!(planner_lines.iter().any(|l| l.starts_with("crate hyper ")));
        assert!(!wg_lines.iter().any(|l| l.starts_with("crate hyper ")), "planner-only dependency leaked into wireguard");
        assert!(!wg_lines.iter().any(|l| l.contains("packages/meduza-np-planner/")));
        // tests do not count
        assert!(!planner_lines.iter().any(|l| l.contains("/tests/")));
        // stable across runs
        assert_eq!(ctx.fingerprint("meduza-np-wireguard").unwrap().0, wg);
        // non-Rust package
        let (pkg, lines) = ctx.fingerprint("meduza-np-openvpn-pkg").unwrap();
        assert!(!pkg.is_empty() && lines.iter().any(|l| l.contains("upstream.lock.json")));
        // runtime images: the manager image carries every package's UI metadata, the base image the updater
        let (_, manager) = ctx.fingerprint("meduza-np-manager").unwrap();
        assert!(manager.iter().any(|l| l.starts_with("file packages/meduza-np-wireguard/ui/descriptor.json ")));
        assert!(manager.iter().any(|l| l.starts_with("file schemas/")));
        let (_, base) = ctx.fingerprint("meduza-np-base-container").unwrap();
        let (updater, _) = ctx.fingerprint("meduza-np-updater").unwrap();
        assert!(base.contains(&format!("embedded meduza-np-updater {updater}")));
    }
}
