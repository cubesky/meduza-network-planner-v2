//! CycloneDX 1.5 SBOM per package and version (§15.1 step 7). Components come from the same inputs the fingerprint
//! uses, so the SBOM lists exactly what the artifact is built from:
//! - Rust: the resolved normal + build closure for the release targets (registry crates with their Cargo.lock
//!   SHA-256, licenses from the crate metadata, the dependency graph),
//! - npm (manager, dashboard): the importer's `dependencies` with their transitive closure from `pnpm-lock.yaml`
//!   (what ships in node_modules), plus the packages named in `meduzaNp.bundled` of the importer's package.json
//!   (compiled into the browser bundle although they are devDependencies), SHA-512 from the lock's integrity,
//! - provider packages: the pinned upstream sources and toolchain of `upstream.lock.json`,
//! - runtime images (manager, base-container): the digest-pinned base image of the Containerfile's final stage and,
//!   per image platform, every Alpine package installed in the built image (its apk database, extracted by
//!   `tools/release-image.sh`), with apk dependency edges.
//!
//! Output is deterministic for the same inputs and `SOURCE_DATE_EPOCH`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{json, Value};

use crate::fingerprint::{rust_closure, Ctx};

/// Importer directory (relative to the repository) whose npm dependencies are part of the package.
fn npm_importer(package: &str) -> Option<&'static str> {
    match package {
        "meduza-np-manager" => Some("packages/meduza-np-manager"),
        "meduza-np-dashboard" => Some("packages/meduza-np-dashboard/web"),
        _ => None,
    }
}

// ---- pnpm-lock.yaml (the subset pnpm 9+ lockfiles use) ----------------------------------------------------------

#[derive(Debug, Default)]
pub struct PnpmLock {
    /// importer path → section (`dependencies`, `devDependencies`, `optionalDependencies`) → name → version ref
    pub importers: BTreeMap<String, BTreeMap<String, BTreeMap<String, String>>>,
    /// `name@version` → integrity (`sha512-<base64>`)
    pub integrity: BTreeMap<String, String>,
    /// snapshot key `name@version(peers)` → (name, version ref) of its dependencies and optional dependencies
    pub snapshots: BTreeMap<String, Vec<(String, String)>>,
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    s.strip_prefix('\'')
        .and_then(|x| x.strip_suffix('\''))
        .or_else(|| s.strip_prefix('"').and_then(|x| x.strip_suffix('"')))
        .unwrap_or(s)
        .to_string()
}

/// `key: value` / `'key': value` / `key:` → (key, value).
fn split_kv(line: &str) -> (String, String) {
    let t = line.trim();
    if let Some(rest) = t.strip_prefix('\'') {
        if let Some(end) = rest.find('\'') {
            let key = rest[..end].to_string();
            let after = rest[end + 1..].trim_start_matches(':').trim();
            return (key, after.to_string());
        }
    }
    match t.find(':') {
        Some(i) => (unquote(&t[..i]), t[i + 1..].trim().to_string()),
        None => (unquote(t), String::new()),
    }
}

/// Version without the peer suffix: `2.10.6(@babel/core@7.29.7)` → `2.10.6`.
fn plain_version(v: &str) -> &str {
    v.split('(').next().unwrap_or(v)
}

impl PnpmLock {
    pub fn parse(text: &str) -> Self {
        let mut lock = PnpmLock::default();
        let (mut top, mut l1, mut l2, mut l3) = (String::new(), String::new(), String::new(), String::new());
        for raw in text.lines() {
            if raw.trim().is_empty() || raw.trim_start().starts_with('#') {
                continue;
            }
            let indent = raw.len() - raw.trim_start().len();
            let (key, value) = split_kv(raw);
            match indent {
                0 => top = key,
                2 => {
                    l1 = key;
                    l2.clear();
                    if top == "snapshots" {
                        lock.snapshots.entry(l1.clone()).or_default();
                    }
                }
                4 => {
                    l2 = key.clone();
                    if top == "packages" && key == "resolution" {
                        if let Some(i) = value.find("integrity: ") {
                            let v = value[i + 11..].split([',', '}']).next().unwrap_or("").trim().to_string();
                            lock.integrity.insert(l1.clone(), v);
                        }
                    }
                }
                6 => {
                    l3 = key.clone();
                    if top == "snapshots" && (l2 == "dependencies" || l2 == "optionalDependencies") {
                        lock.snapshots.entry(l1.clone()).or_default().push((key, unquote(&value)));
                    }
                }
                8 if top == "importers" && key == "version" => {
                    lock.importers.entry(l1.clone()).or_default().entry(l2.clone()).or_default().insert(l3.clone(), unquote(&value));
                }
                _ => {}
            }
        }
        lock
    }
}

fn npm_purl(name: &str, version: &str) -> String {
    format!("pkg:npm/{}@{version}", name.replace('@', "%40"))
}

/// base64 → lowercase hex (integrity values in the lock).
fn b64_to_hex(s: &str) -> Option<String> {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits = 0u32;
    let mut n = 0;
    let mut out = Vec::new();
    for c in s.bytes().filter(|&c| c != b'=') {
        let v = A.iter().position(|&a| a == c)? as u32;
        bits = (bits << 6) | v;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
            bits &= (1 << n) - 1;
        }
    }
    Some(hex::encode(out))
}

// ---- building the document ---------------------------------------------------------------------------------------

#[derive(Default)]
struct Bom {
    components: BTreeMap<String, Value>,
    deps: BTreeMap<String, BTreeSet<String>>,
}

impl Bom {
    fn add(&mut self, bom_ref: &str, component: Value) {
        self.components.entry(bom_ref.to_string()).or_insert(component);
        self.deps.entry(bom_ref.to_string()).or_default();
    }
    fn edge(&mut self, from: &str, to: &str) {
        self.deps.entry(from.to_string()).or_default().insert(to.to_string());
    }
}

fn licenses(expr: Option<&str>) -> Value {
    match expr {
        Some(e) if !e.is_empty() => json!([{ "expression": e }]),
        _ => json!([]),
    }
}

fn add_rust(bom: &mut Bom, ctx: &Ctx, meta: &Value, package: &str, root_ref: &str) -> Result<(), String> {
    let closure = rust_closure(meta, package)?;
    let refs: BTreeMap<String, String> = closure
        .iter()
        .map(|c| {
            let r = if c.name == package && c.local {
                root_ref.to_string()
            } else if c.source.starts_with("registry+") {
                format!("pkg:cargo/{}@{}", c.name, c.version)
            } else {
                format!("crate:{}@{}", c.name, c.version)
            };
            (c.id.clone(), r)
        })
        .collect();
    for c in &closure {
        let r = &refs[&c.id];
        if r != root_ref {
            let mut comp = json!({
                "type": "library",
                "bom-ref": r,
                "name": c.name,
                "version": c.version,
                "licenses": licenses(c.license.as_deref()),
            });
            if c.source.starts_with("registry+") {
                comp["purl"] = json!(r);
                let sum = ctx.checksum(c);
                if !sum.is_empty() {
                    comp["hashes"] = json!([{ "alg": "SHA-256", "content": sum }]);
                }
            } else if c.local {
                comp["description"] = json!("workspace crate of this repository");
            } else {
                comp["externalReferences"] = json!([{ "type": "vcs", "url": c.source }]);
            }
            bom.add(r, comp);
        }
        for d in &c.deps {
            if let Some(to) = refs.get(d) {
                bom.edge(r, to);
            }
        }
    }
    Ok(())
}

fn add_npm(bom: &mut Bom, repo: &Path, lock: &PnpmLock, importer: &str, root_ref: &str) -> Result<(), String> {
    let pj_path = repo.join(importer).join("package.json");
    let pj: Value = serde_json::from_slice(&std::fs::read(&pj_path).map_err(|e| format!("{}: {e}", pj_path.display()))?)
        .map_err(|e| format!("{}: {e}", pj_path.display()))?;
    let sections = lock.importers.get(importer).ok_or_else(|| format!("{importer} is not an importer in pnpm-lock.yaml"))?;
    // Top level: runtime dependencies (following workspace links), then the declared bundled devDependencies.
    let mut roots: Vec<(String, String)> = Vec::new();
    let mut seen_importers = BTreeSet::new();
    let mut pending = vec![importer.to_string()];
    while let Some(imp) = pending.pop() {
        if !seen_importers.insert(imp.clone()) {
            continue;
        }
        for (name, v) in lock.importers.get(&imp).and_then(|s| s.get("dependencies")).into_iter().flatten() {
            if let Some(rel) = v.strip_prefix("link:") {
                let joined = normalize(&format!("{imp}/{rel}"));
                pending.push(joined);
            } else {
                roots.push((name.clone(), v.clone()));
            }
        }
    }
    for name in pj["meduzaNp"]["bundled"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let v = ["dependencies", "devDependencies"]
            .iter()
            .find_map(|s| sections.get(*s).and_then(|m| m.get(name)))
            .ok_or_else(|| format!("{importer}: bundled package {name} is not a dependency"))?;
        roots.push((name.to_string(), v.clone()));
    }

    let mut stack: Vec<(Option<String>, String, String)> = roots.into_iter().map(|(n, v)| (None, n, v)).collect();
    let mut visited = BTreeSet::new();
    while let Some((parent, name, vref)) = stack.pop() {
        let version = plain_version(&vref).to_string();
        let r = npm_purl(&name, &version);
        bom.edge(parent.as_deref().unwrap_or(root_ref), &r);
        if !visited.insert(format!("{name}@{vref}")) {
            continue;
        }
        let mut comp = json!({ "type": "library", "bom-ref": r, "name": name, "version": version, "purl": r });
        if let Some(hexsum) = lock.integrity.get(&format!("{name}@{version}")).and_then(|i| i.strip_prefix("sha512-")).and_then(b64_to_hex)
        {
            comp["hashes"] = json!([{ "alg": "SHA-512", "content": hexsum }]);
        }
        bom.add(&r, comp);
        for (dn, dv) in lock.snapshots.get(&format!("{name}@{vref}")).into_iter().flatten() {
            stack.push((Some(r.clone()), dn.clone(), dv.clone()));
        }
    }
    Ok(())
}

/// `a/b/../../c` → `c` (paths inside the repository only).
fn normalize(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            x => out.push(x),
        }
    }
    out.join("/")
}

fn github_purl(git: &str, version: &str) -> Option<String> {
    let rest = git.strip_prefix("https://github.com/")?.trim_end_matches(".git");
    Some(format!("pkg:github/{}@{version}", rest.to_lowercase()))
}

fn add_upstream(bom: &mut Bom, lock: &Value, root_ref: &str) {
    for s in lock["sources"].as_array().into_iter().flatten() {
        let name = s["name"].as_str().unwrap_or_default();
        let version = s["version"].as_str().unwrap_or_default();
        let mut refs = Vec::new();
        let purl = if let Some(git) = s["git"].as_str() {
            refs.push(json!({ "type": "vcs", "url": git, "comment": format!("commit {}", s["commit"].as_str().unwrap_or("")) }));
            github_purl(git, version).unwrap_or_else(|| format!("pkg:generic/{name}@{version}"))
        } else {
            if let Some(u) = s["url"].as_str() {
                refs.push(json!({ "type": "distribution", "url": u }));
            }
            format!("pkg:generic/{name}@{version}")
        };
        let mut comp = json!({
            "type": "library",
            "bom-ref": purl,
            "name": name,
            "version": version,
            "purl": purl,
            "licenses": licenses(s["license"].as_str()),
            "externalReferences": refs,
        });
        if let Some(sum) = s["sha256"].as_str() {
            comp["hashes"] = json!([{ "alg": "SHA-256", "content": sum }]);
        }
        bom.add(&purl, comp);
        bom.edge(root_ref, &purl);
    }
    // A toolchain whose runtime is linked into the binary (Go).
    let tc = &lock["toolchain"];
    if tc["name"] == "go" {
        let version = tc["version"].as_str().unwrap_or_default().trim_start_matches("go").to_string();
        let purl = format!("pkg:golang/stdlib@{version}");
        bom.add(
            &purl,
            json!({ "type": "library", "bom-ref": purl, "name": "go-stdlib", "version": version, "purl": purl,
                    "licenses": licenses(tc["license"].as_str()), "description": "Go standard library linked into the binary" }),
        );
        bom.edge(root_ref, &purl);
    }
}

// ---- runtime images: base image + the Alpine packages installed in the built image -------------------------------

/// One package of an apk database (`/lib/apk/db/installed`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApkPackage {
    pub name: String,
    pub version: String,
    pub arch: String,
    pub license: String,
    pub description: String,
    pub url: String,
    pub origin: String,
    pub commit: String,
    pub checksum: String,
    pub depends: Vec<String>,
    pub provides: Vec<String>,
}

/// Paragraphs of `X:value` lines separated by blank lines; `P` is the name, `p` the provides (case matters).
pub fn parse_apk_installed(text: &str) -> Vec<ApkPackage> {
    let mut out = Vec::new();
    let mut cur = ApkPackage::default();
    for line in text.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if !cur.name.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur = ApkPackage::default();
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let words = || v.split_whitespace().map(str::to_string).collect::<Vec<_>>();
        match k {
            "P" => cur.name = v.to_string(),
            "V" => cur.version = v.to_string(),
            "A" => cur.arch = v.to_string(),
            "L" => cur.license = v.to_string(),
            "T" => cur.description = v.to_string(),
            "U" => cur.url = v.to_string(),
            "o" => cur.origin = v.to_string(),
            "c" => cur.commit = v.to_string(),
            "C" => cur.checksum = v.to_string(),
            "D" => cur.depends = words(),
            "p" => cur.provides = words(),
            _ => {}
        }
    }
    out
}

/// Dependency/provides token without version constraint; `None` for conflicts (`!name`).
fn apk_token_name(t: &str) -> Option<&str> {
    if t.starts_with('!') {
        return None;
    }
    let end = t.find(['=', '<', '>', '~']).unwrap_or(t.len());
    Some(&t[..end])
}

/// apk architecture an image platform must contain (packages may also be `noarch`).
fn apk_arch(platform: &str) -> Option<&'static str> {
    match platform {
        "linux/amd64" => Some("x86_64"),
        "linux/arm64" => Some("aarch64"),
        "linux/arm/v7" => Some("armv7"),
        _ => None,
    }
}

/// `KEY=value` from an os-release file.
fn os_release(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=').map(unquote))
}

/// Final-stage base image of a Containerfile, `ARG` defaults substituted: (repository, tag, digest).
pub fn containerfile_base(text: &str) -> Option<(String, String, String)> {
    let mut args: BTreeMap<String, String> = BTreeMap::new();
    let mut base = None;
    for line in text.lines().map(str::trim) {
        if let Some(a) = line.strip_prefix("ARG ") {
            if let Some((k, v)) = a.split_once('=') {
                args.insert(k.trim().to_string(), unquote(v));
            }
        } else if let Some(f) = line.strip_prefix("FROM ") {
            let mut r = f.split_whitespace().next().unwrap_or_default().to_string();
            for (k, v) in &args {
                r = r.replace(&format!("${{{k}}}"), v).replace(&format!("${k}"), v);
            }
            base = Some(r);
        }
    }
    let r = base?;
    let (name, digest) = r.split_once('@')?;
    let (repo, tag) = match name.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => (repo.to_string(), tag.to_string()),
        _ => (name.to_string(), String::new()),
    };
    Some((repo, tag, digest.to_string()))
}

/// Base image and OS packages of a runtime image. `os_dir` holds, per image platform, `<platform-slug>.installed`
/// (the image's `/lib/apk/db/installed`) and `<platform-slug>.os-release` (its `/etc/os-release`), extracted from
/// the built image by `tools/release-image.sh`.
fn add_image(bom: &mut Bom, repo: &Path, package: &str, platforms: &[&str], os_dir: &Path, root_ref: &str) -> Result<(), String> {
    let cf_path = repo.join("packages").join(package).join("Containerfile");
    let cf = std::fs::read_to_string(&cf_path).map_err(|e| format!("{}: {e}", cf_path.display()))?;
    let (base_repo, tag, digest) =
        containerfile_base(&cf).ok_or_else(|| format!("{}: final FROM is not pinned by digest", cf_path.display()))?;
    let base_name = base_repo.rsplit('/').next().unwrap_or(&base_repo).to_string();
    let base_ref = format!("pkg:oci/{base_name}@{}?repository_url={}&tag={tag}", digest.replace(':', "%3A"), base_repo);
    bom.add(
        &base_ref,
        json!({ "type": "container", "bom-ref": base_ref, "name": base_repo, "version": tag, "purl": base_ref,
                "hashes": [{ "alg": "SHA-256", "content": digest.trim_start_matches("sha256:") }],
                "description": "base image of the final stage (pinned by digest)" }),
    );
    bom.edge(root_ref, &base_ref);
    // Node.js of the official node image is not an apk package; its version is the image tag.
    if base_name == "node" {
        let v = tag.split('-').next().unwrap_or_default();
        let purl = format!("pkg:generic/nodejs@{v}");
        bom.add(
            &purl,
            json!({ "type": "framework", "bom-ref": purl, "name": "nodejs", "version": v, "purl": purl,
                    "licenses": licenses(Some("MIT")), "description": "Node.js runtime of the base image" }),
        );
        bom.edge(&base_ref, &purl);
    }

    for platform in platforms {
        let slug = platform.replace('/', "-");
        let read = |ext: &str| {
            let p = os_dir.join(format!("{slug}.{ext}"));
            std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))
        };
        let osr = read("os-release")?;
        if os_release(&osr, "ID").as_deref() != Some("alpine") {
            return Err(format!("{package} {platform}: not an Alpine image (os-release ID)"));
        }
        let distro = format!("alpine-{}", os_release(&osr, "VERSION_ID").unwrap_or_default());
        let pkgs = parse_apk_installed(&read("installed")?);
        if pkgs.is_empty() {
            return Err(format!("{package} {platform}: empty apk database"));
        }
        let want = apk_arch(platform).ok_or_else(|| format!("{platform}: no apk architecture"))?;
        if let Some(p) = pkgs.iter().find(|p| p.arch != want && p.arch != "noarch") {
            return Err(format!("{package} {platform}: apk database is for another platform ({} is {})", p.name, p.arch));
        }
        let pref = |p: &ApkPackage| format!("pkg:apk/alpine/{}@{}?arch={}&distro={distro}", p.name, p.version, p.arch);
        let mut provider: BTreeMap<&str, String> = BTreeMap::new();
        for p in &pkgs {
            provider.insert(p.name.as_str(), pref(p));
            for t in p.provides.iter().filter_map(|t| apk_token_name(t)) {
                provider.entry(t).or_insert_with(|| pref(p));
            }
        }
        for p in &pkgs {
            let r = pref(p);
            let mut props = vec![json!({ "name": "meduza-np:platform", "value": platform })];
            for (k, v) in [("apk:origin", &p.origin), ("apk:commit", &p.commit), ("apk:checksum", &p.checksum)] {
                if !v.is_empty() {
                    props.push(json!({ "name": k, "value": v }));
                }
            }
            let mut comp = json!({
                "type": "library", "bom-ref": r, "name": p.name, "version": p.version, "purl": r,
                "description": p.description, "properties": props,
                // apk license fields are not always valid SPDX expressions
                "licenses": if p.license.is_empty() { json!([]) } else { json!([{ "license": { "name": p.license } }]) },
            });
            if !p.url.is_empty() {
                comp["externalReferences"] = json!([{ "type": "website", "url": p.url }]);
            }
            bom.add(&r, comp);
            bom.edge(root_ref, &r);
            for d in p.depends.iter().filter_map(|d| apk_token_name(d)) {
                if let Some(to) = provider.get(d).filter(|to| **to != r) {
                    bom.edge(&r, to);
                }
            }
        }
    }
    Ok(())
}

/// Deterministic urn:uuid (version 8 layout) from the SBOM subject.
fn serial(package: &str, version: &str, fingerprint: &str) -> String {
    let h = meduza_np_protocol::digest::sha256_hex(format!("{package}\n{version}\n{fingerprint}").as_bytes());
    let mut b = hex::decode(&h[..32]).unwrap_or_default();
    b[6] = (b[6] & 0x0f) | 0x80;
    b[8] = (b[8] & 0x3f) | 0x80;
    let x = hex::encode(b);
    format!("urn:uuid:{}-{}-{}-{}-{}", &x[0..8], &x[8..12], &x[12..16], &x[16..20], &x[20..32])
}

/// `image_os`: OS package databases of the built runtime image (required for runtime image packages, see
/// [`add_image`]).
pub fn build(ctx: &Ctx, package: &str, version: &str, timestamp: &str, image_os: Option<&Path>) -> Result<Value, String> {
    let (fingerprint, _) = ctx.fingerprint(package)?;
    let root_ref = format!("meduza-np:{package}@{version}");
    let mut bom = Bom::default();
    bom.deps.entry(root_ref.clone()).or_default();
    let mut properties = vec![
        json!({ "name": "meduza-np:fingerprint", "value": fingerprint }),
        json!({ "name": "meduza-np:targets", "value": ctx.targets.join(",") }),
    ];
    if let Some(platforms) = crate::oci::image_platforms(package) {
        let dir = image_os.ok_or_else(|| format!("{package} is a runtime image: --image-os DIR (its OS package databases) is required"))?;
        add_image(&mut bom, &ctx.repo, package, platforms, dir, &root_ref)?;
        properties.push(json!({ "name": "meduza-np:image-platforms", "value": platforms.join(",") }));
    }
    if let Some(meta) = &ctx.metadata {
        add_rust(&mut bom, ctx, meta, package, &root_ref)?;
    }
    if let Some(importer) = npm_importer(package) {
        let text = std::fs::read_to_string(ctx.repo.join("pnpm-lock.yaml")).map_err(|e| format!("pnpm-lock.yaml: {e}"))?;
        add_npm(&mut bom, &ctx.repo, &PnpmLock::parse(&text), importer, &root_ref)?;
    }
    let upstream = ctx.repo.join("packages").join(package).join("upstream.lock.json");
    if upstream.is_file() {
        let lock: Value = serde_json::from_slice(&std::fs::read(&upstream).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        add_upstream(&mut bom, &lock, &root_ref);
    }
    let license = std::fs::read_to_string(ctx.repo.join("Cargo.toml"))
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.trim().strip_prefix("license = ").map(|v| v.trim_matches('"').to_string())));
    Ok(json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": serial(package, version, &fingerprint),
        "version": 1,
        "metadata": {
            "timestamp": timestamp,
            "tools": { "components": [{ "type": "application", "name": "meduza-np-release", "version": env!("CARGO_PKG_VERSION") }] },
            "component": {
                "type": "application",
                "bom-ref": root_ref,
                "name": package,
                "version": version,
                "licenses": licenses(license.as_deref()),
                "properties": properties,
            },
        },
        "components": bom.components.into_values().collect::<Vec<_>>(),
        "dependencies": bom.deps.into_iter().map(|(r, d)| json!({ "ref": r, "dependsOn": d.into_iter().collect::<Vec<_>>() })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::cargo_metadata;
    use std::path::PathBuf;

    #[test]
    fn pnpm_lock_subset() {
        let text = "lockfileVersion: '9.0'\n\nimporters:\n\n  packages/app:\n    dependencies:\n      fastify:\n        specifier: 5.1.0\n        version: 5.1.0\n      '@scope/ui':\n        specifier: workspace:*\n        version: link:../../web/ui\n    devDependencies:\n      vite:\n        specifier: 7.0.0\n        version: 7.0.0(@types/node@24.0.0)\n\npackages:\n\n  fastify@5.1.0:\n    resolution: {integrity: sha512-AAEC}\n\nsnapshots:\n\n  fastify@5.1.0:\n    dependencies:\n      '@fastify/error': 4.0.0\n    optionalDependencies:\n      pino: 9.0.0\n\n  '@fastify/error@4.0.0': {}\n";
        let lock = PnpmLock::parse(text);
        let app = &lock.importers["packages/app"];
        assert_eq!(app["dependencies"]["fastify"], "5.1.0");
        assert_eq!(app["dependencies"]["@scope/ui"], "link:../../web/ui");
        assert_eq!(app["devDependencies"]["vite"], "7.0.0(@types/node@24.0.0)");
        assert_eq!(lock.integrity["fastify@5.1.0"], "sha512-AAEC");
        assert_eq!(lock.snapshots["fastify@5.1.0"], vec![("@fastify/error".into(), "4.0.0".into()), ("pino".into(), "9.0.0".into())]);
        assert!(lock.snapshots["@fastify/error@4.0.0"].is_empty());
        assert_eq!(b64_to_hex("AAEC").unwrap(), "000102");
        assert_eq!(npm_purl("@scope/ui", "1.0.0"), "pkg:npm/%40scope/ui@1.0.0");
        assert_eq!(normalize("packages/app/../../web/ui"), "web/ui");
    }

    fn apk_db(arch: &str) -> String {
        let db = r"C:Q1abc=
P:musl
V:1.2.5-r10
A:ARCH
T:the musl c library
U:https://musl.libc.org/
L:MIT
o:musl
c:deadbeef
p:so:libc.musl-ARCH.so.1=1
F:lib
R:ld-musl-ARCH.so.1

P:ca-certificates-bundle
V:20250911-r0
A:noarch
L:MPL-2.0 AND MIT

P:curl
V:8.14.1-r2
A:ARCH
L:curl
D:ca-certificates-bundle so:libc.musl-ARCH.so.1 !curl-legacy so:libmissing.so.1
";
        db.replace("ARCH", arch)
    }

    fn fake_image_os(name: &str, platforms: &[&str]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mnp-release-sbom-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for pl in platforms {
            let slug = pl.replace('/', "-");
            std::fs::write(d.join(format!("{slug}.installed")), apk_db(apk_arch(pl).unwrap())).unwrap();
            std::fs::write(d.join(format!("{slug}.os-release")), "NAME=\"Alpine Linux\"\nID=alpine\nVERSION_ID=3.24.1\n").unwrap();
        }
        d
    }

    #[test]
    fn apk_database_and_containerfile() {
        let pkgs = parse_apk_installed(&apk_db("x86_64"));
        assert_eq!(pkgs.len(), 3);
        assert_eq!(pkgs[0].name, "musl");
        assert_eq!(pkgs[0].provides, vec!["so:libc.musl-x86_64.so.1=1"]);
        assert_eq!(pkgs[0].checksum, "Q1abc=");
        assert_eq!(pkgs[2].depends.len(), 4);
        assert_eq!(apk_token_name("so:libc.musl-x86_64.so.1=1"), Some("so:libc.musl-x86_64.so.1"));
        assert_eq!(apk_token_name("busybox>=1.36"), Some("busybox"));
        assert_eq!(apk_token_name("!curl-legacy"), None);
        let cf = "ARG NODE_IMAGE=docker.io/library/node:24.22.1-alpine3.24@sha256:06d7\nFROM ${NODE_IMAGE} AS build\nRUN x\nFROM ${NODE_IMAGE}\n";
        assert_eq!(containerfile_base(cf), Some(("docker.io/library/node".into(), "24.22.1-alpine3.24".into(), "sha256:06d7".into())));
        assert_eq!(containerfile_base("FROM localhost:5000/alpine@sha256:ab\n").unwrap().0, "localhost:5000/alpine");
        assert_eq!(containerfile_base("FROM alpine:3.24\n"), None, "unpinned base image");
    }

    #[test]
    fn sboms_follow_the_build_inputs() {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let meta = cargo_metadata(&repo, "x86_64-unknown-linux-musl").unwrap();
        let ctx = Ctx::load(&repo, vec!["x86_64-unknown-linux-musl".into()], Some(meta)).unwrap();
        let ts = "2026-01-01T00:00:00Z";
        let names = |b: &Value| -> BTreeSet<String> {
            b["components"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect()
        };

        let planner = build(&ctx, "meduza-np-planner", "0.1.0", ts, None).unwrap();
        assert_eq!(planner["metadata"]["component"]["name"], "meduza-np-planner");
        let hyper = planner["components"].as_array().unwrap().iter().find(|c| c["name"] == "hyper").expect("hyper in planner closure");
        assert!(hyper["purl"].as_str().unwrap().starts_with("pkg:cargo/hyper@"));
        assert_eq!(hyper["hashes"][0]["alg"], "SHA-256");
        assert!(!hyper["licenses"].as_array().unwrap().is_empty());
        assert!(names(&planner).contains("meduza-np-protocol"), "workspace crates listed");
        let root_deps =
            planner["dependencies"].as_array().unwrap().iter().find(|d| d["ref"] == "meduza-np:meduza-np-planner@0.1.0").unwrap();
        assert!(root_deps["dependsOn"].as_array().unwrap().iter().any(|d| d.as_str().unwrap().starts_with("pkg:cargo/tokio@")));
        // every dependency edge points at a listed component (or the root)
        let refs: BTreeSet<&str> = planner["components"].as_array().unwrap().iter().map(|c| c["bom-ref"].as_str().unwrap()).collect();
        for d in planner["dependencies"].as_array().unwrap() {
            for to in d["dependsOn"].as_array().unwrap() {
                assert!(refs.contains(to.as_str().unwrap()), "dangling {to}");
            }
        }
        // deterministic, and closure-scoped like the fingerprint
        assert_eq!(build(&ctx, "meduza-np-planner", "0.1.0", ts, None).unwrap(), planner);
        let wg = build(&ctx, "meduza-np-wireguard", "0.1.0", ts, None).unwrap();
        assert!(!names(&wg).contains("hyper"), "planner-only dependency leaked into the wireguard SBOM");
        assert_ne!(wg["serialNumber"], planner["serialNumber"]);

        // provider: pinned upstream sources and toolchain
        let mosdns = build(&ctx, "meduza-np-mosdns-pkg", "0.1.0", ts, None).unwrap();
        let m = mosdns["components"].as_array().unwrap().iter().find(|c| c["name"] == "mosdns").unwrap();
        assert_eq!(m["purl"], "pkg:github/irinesistiana/mosdns@v5.3.4");
        assert_eq!(m["licenses"][0]["expression"], "GPL-3.0-or-later");
        assert!(names(&mosdns).contains("go-stdlib"));
        let ovpn = build(&ctx, "meduza-np-openvpn-pkg", "0.1.0", ts, None).unwrap();
        let ssl = ovpn["components"].as_array().unwrap().iter().find(|c| c["name"] == "openssl").unwrap();
        assert_eq!(ssl["hashes"][0]["alg"], "SHA-256");

        // npm: runtime closure of the manager, bundled UI packages of the dashboard
        assert!(
            build(&ctx, "meduza-np-manager", "0.1.0", ts, None).unwrap_err().contains("--image-os"),
            "image SBOM needs its OS packages"
        );
        let os = fake_image_os("manager", &["linux/amd64", "linux/arm64"]);
        let manager = build(&ctx, "meduza-np-manager", "0.1.0", ts, Some(&os)).unwrap();
        let mn = names(&manager);
        assert!(mn.contains("fastify") && mn.contains("pino") && mn.contains("ldapts"), "runtime closure");
        assert!(mn.contains("preact") && mn.contains("@simplewebauthn/browser"), "bundled browser packages");
        assert!(!mn.contains("vite") && !mn.contains("typescript"), "build tools are not shipped");
        let fastify = manager["components"].as_array().unwrap().iter().find(|c| c["name"] == "fastify").unwrap();
        assert_eq!(fastify["hashes"][0]["alg"], "SHA-512");
        // image: base image pinned by digest, Node.js of the node image, apk packages per platform with edges
        let comps = manager["components"].as_array().unwrap();
        let base = comps.iter().find(|c| c["type"] == "container").unwrap();
        assert_eq!(base["name"], "docker.io/library/node");
        assert!(base["purl"].as_str().unwrap().starts_with("pkg:oci/node@sha256%3A"));
        assert!(comps.iter().any(|c| c["name"] == "nodejs" && c["version"].as_str().unwrap().starts_with("24.")));
        let musl = "pkg:apk/alpine/musl@1.2.5-r10?arch=aarch64&distro=alpine-3.24.1";
        assert!(comps.iter().any(|c| c["bom-ref"] == musl));
        assert!(comps.iter().any(|c| c["bom-ref"] == "pkg:apk/alpine/curl@8.14.1-r2?arch=x86_64&distro=alpine-3.24.1"));
        let curl = manager["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["ref"] == "pkg:apk/alpine/curl@8.14.1-r2?arch=aarch64&distro=alpine-3.24.1")
            .unwrap();
        assert_eq!(curl["dependsOn"], json!(["pkg:apk/alpine/ca-certificates-bundle@20250911-r0?arch=noarch&distro=alpine-3.24.1", musl]));
        let refs: BTreeSet<&str> = comps.iter().map(|c| c["bom-ref"].as_str().unwrap()).collect();
        for d in manager["dependencies"].as_array().unwrap() {
            for to in d["dependsOn"].as_array().unwrap() {
                assert!(refs.contains(to.as_str().unwrap()), "dangling {to}");
            }
        }
        // a database of the wrong platform or a missing platform is refused
        let wrong = fake_image_os("wrong", &["linux/amd64", "linux/arm64"]);
        std::fs::copy(wrong.join("linux-amd64.installed"), wrong.join("linux-arm64.installed")).unwrap();
        assert!(build(&ctx, "meduza-np-manager", "0.1.0", ts, Some(&wrong)).unwrap_err().contains("another platform"));
        let partial = fake_image_os("partial", &["linux/amd64", "linux/arm64"]);
        assert!(build(&ctx, "meduza-np-base-container", "0.1.0", ts, Some(&partial)).unwrap_err().contains("linux-arm-v7"));
        for d in [os, wrong, partial] {
            std::fs::remove_dir_all(d).ok();
        }
        let dash = build(&ctx, "meduza-np-dashboard", "0.1.0", ts, None).unwrap();
        let dn = names(&dash);
        assert!(dn.contains("preact") && !dn.contains("vite"));
        assert!(dn.contains("hyper"), "dashboard backend crates");
    }
}
