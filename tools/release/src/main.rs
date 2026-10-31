//! `meduza-np-release` — release tooling used by `.github/workflows/release.yml` (§15). Never installed on nodes.
//!
//! ```text
//! fingerprint --targets T1,T2 [--package P]...              → {package: fingerprint} (all distributable packages)
//! matrix --fingerprints F --group-version V [--baseline M]  → {package: {fingerprint, changed, version, reusedFromGroup}}
//! prepare-trust --key-env ENV --out FILE                    → public trust roots; validate an existing store without replacing it
//! package --package P --version V --platform PL --out DIR [--bin-dir D] [--tree D]
//!                                                           → DIR/<P>_<V>_<platform>.tar.gz + .json (BuiltArtifact)
//! group --group-version V --sequence N --commit C --matrix M --artifacts DIR --url-base U --site DIR
//!       [--baseline M] [--channel stable] [--expires-days 30] [--platforms a,b] [--notes TEXT] --key KEYFILE...
//!                                                           → DIR/releases/V/manifest.json(.sig), DIR/channels/<ch>.json(.sig)
//! sign-pointer --site DIR --channel C --group-version V --sequence N [--expires-days 30] --key KEYFILE...
//!                                                           → re-sign the channel pointer (expiry refresh, §13.4)
//! sbom --package P --version V --targets T1,T2 [--out FILE] [--image-os DIR]
//!                                                           → CycloneDX 1.5 JSON (stdout or FILE); runtime images need
//!                                                             DIR/<platform-slug>.{installed,os-release} of the built image
//! openwrt --package P --version V (--target T | --platform PL | --noarch) --out DIR [--bin-dir D] [--tree D]
//!         [--trust FILE] [--formats ipk,apk]                → DIR/<P>_<V>_<arch>.ipk(.json) + .apk.stage/ per arch (D29)
//! artifact --stage DIR                                      → describes the .apk that tools/openwrt-apk.sh built
//! classify --matrix M                                       → changed packages by build job (rust/providers/linux/openwrt/noarch/images)
//! oci-layout --package P --artifacts DIR --image REPO --out LAYOUT [--describe FILE] [--source URL]
//!                                                           → distribution image (OCI layout) of P's linux archives + OciImage JSON
//! oci-describe --package P --version V --image REPO --raw INDEX [--out FILE]
//!                                                           → OciImage JSON of a pushed runtime image (raw index from skopeo)
//! oci-check --manifest M --ref REPO@DIGEST --raw INDEX      → the pushed index matches what the group pins (D30)
//! mihomo-assets --tag vX.Y.Z [--release FILE] [--work DIR] [--manifest FILE] [--keep N]
//!                                                           → downloads, checks and pins that Mihomo release in assets.json (D31)
//! ```
//!
//! `group` also takes `--oci DIR` (`*.oci.json` records applied to the packages) and `--require-oci 1` (every generic
//! archive and every runtime image platform must be pinned to an OCI image, §15.1 step 6).

mod archive;
mod fingerprint;
mod mihomo_assets;
mod oci;
mod openwrt;
mod sbom;
mod trust;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use meduza_np_protocol::group::{ChannelPointer, Compatibility, GroupManifest, GroupPackage, GROUP_FORMAT_VERSION};
use meduza_np_protocol::manifest::{ApiRange, PackageManifest, ALL_PACKAGES};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Packages distributed as node package archives. manager and base-container ship as runtime OCI images (`oci.rs`);
/// dashboard-openwrt ships as an OpenWrt package. The dashboard archive needs its prebuilt web UI (dist/www).
pub const ARCHIVE_PACKAGES: &[&str] = &[
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
    "meduza-np-avahi",
    "meduza-np-dashboard",
];

pub const PLATFORMS: &[&str] = &["linux/amd64", "linux/arm64", "linux/arm/v7"];

/// Packages recipes build (not cargo).
pub const PROVIDER_RECIPES: &[&str] = &["meduza-np-openvpn-pkg", "meduza-np-mosdns-pkg"];

/// Every package with a release artifact: generic archives, OpenWrt packages and runtime images.
pub fn distributed() -> Vec<&'static str> {
    let mut v: Vec<&str> =
        ARCHIVE_PACKAGES.iter().chain(openwrt::OPENWRT_PACKAGES).copied().chain(oci::IMAGE_PACKAGES.iter().map(|(p, _)| *p)).collect();
    v.sort();
    v.dedup();
    v
}

/// Whether `package` must have an artifact for the group platform `platform`.
pub fn applies(package: &str, platform: &str) -> bool {
    if platform.starts_with("openwrt/") {
        openwrt::OPENWRT_PACKAGES.contains(&package)
    } else if let Some(pl) = oci::image_platforms(package) {
        pl.contains(&platform)
    } else {
        ARCHIVE_PACKAGES.contains(&package)
    }
}

/// Artifact formats a package needs on a platform (OpenWrt: both package managers; runtime images: the image).
fn formats_for(package: &str, platform: &str) -> &'static [&'static str] {
    if platform.starts_with("openwrt/") {
        openwrt::FORMATS
    } else if oci::image_platforms(package).is_some() {
        &["oci"]
    } else {
        &["tar.gz"]
    }
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("meduza-np-release: {msg}");
    std::process::exit(1)
}

struct Args(BTreeMap<String, Vec<String>>);

impl Args {
    fn parse(raw: &[String]) -> Self {
        let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut it = raw.iter();
        while let Some(k) = it.next() {
            let Some(k) = k.strip_prefix("--") else { fail(format!("unexpected argument {k}")) };
            let v = it.next().unwrap_or_else(|| fail(format!("--{k} needs a value")));
            m.entry(k.to_string()).or_default().push(v.clone());
        }
        Self(m)
    }
    fn opt(&self, k: &str) -> Option<&str> {
        self.0.get(k).and_then(|v| v.last()).map(String::as_str)
    }
    fn req(&self, k: &str) -> &str {
        self.opt(k).unwrap_or_else(|| fail(format!("--{k} is required")))
    }
    fn all(&self, k: &str) -> Vec<String> {
        self.0.get(k).cloned().unwrap_or_default()
    }
}

fn repo_root() -> PathBuf {
    std::env::var_os("MEDUZA_NP_REPO").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn read_json<T: serde::de::DeserializeOwned>(p: &str) -> T {
    let raw = std::fs::read(p).unwrap_or_else(|e| fail(format!("{p}: {e}")));
    serde_json::from_slice(&raw).unwrap_or_else(|e| fail(format!("{p}: {e}")))
}

fn write_json(p: &Path, v: &impl Serialize) -> Vec<u8> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).unwrap_or_else(|e| fail(format!("{}: {e}", d.display())));
    }
    let mut bytes = serde_json::to_vec_pretty(v).unwrap_or_else(|e| fail(e));
    bytes.push(b'\n');
    std::fs::write(p, &bytes).unwrap_or_else(|e| fail(format!("{}: {e}", p.display())));
    bytes
}

fn now_unix() -> u64 {
    if let Some(v) = std::env::var("SOURCE_DATE_EPOCH").ok().and_then(|v| v.parse().ok()) {
        return v;
    }
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn sign_file(path: &Path, bytes: &[u8], keys: &[String]) {
    if keys.is_empty() {
        fail("at least one --key is required to sign");
    }
    let keys: Vec<_> = keys
        .iter()
        .map(|f| {
            let text = std::fs::read_to_string(f).unwrap_or_else(|e| fail(format!("{f}: {e}")));
            meduza_np_updater::sig::decode_secret(&text).unwrap_or_else(|e| fail(e))
        })
        .collect();
    let sig = meduza_np_updater::sig::sign(bytes, &keys);
    write_json(&PathBuf::from(format!("{}.sig", path.display())), &sig);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatrixEntry {
    fingerprint: String,
    changed: bool,
    version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reused_from_group: Option<String>,
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = raw.first().cloned() else { fail("usage: see the header of tools/release/src/main.rs") };
    let a = Args::parse(&raw[1..]);
    let repo = repo_root();
    match cmd.as_str() {
        "prepare-trust" => {
            let variable = a.req("key-env");
            let secret = std::env::var(variable).unwrap_or_default();
            let ids = trust::prepare(&secret, Path::new(a.req("out")), now_unix()).unwrap_or_else(|e| fail(e));
            println!("public release trust ready: {} (signer {ids})", a.req("out"));
        }
        "fingerprint" => {
            let targets: Vec<String> = a.req("targets").split(',').map(str::to_string).collect();
            let meta = fingerprint::cargo_metadata(&repo, &targets[0]).unwrap_or_else(|e| fail(e));
            let ctx = fingerprint::Ctx::load(&repo, targets, Some(meta)).unwrap_or_else(|e| fail(e));
            let selected = a.all("package");
            let list: Vec<&str> = if selected.is_empty() { distributed() } else { selected.iter().map(String::as_str).collect() };
            let mut out = BTreeMap::new();
            for p in list {
                let (fp, lines) = ctx.fingerprint(p).unwrap_or_else(|e| fail(e));
                if a.opt("verbose").is_some() {
                    eprintln!("== {p}\n{}", lines.join("\n"));
                }
                out.insert(p.to_string(), fp);
            }
            println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        }
        "matrix" => {
            let fps: BTreeMap<String, String> = read_json(a.req("fingerprints"));
            let gv = a.req("group-version");
            let baseline: Option<GroupManifest> = a.opt("baseline").map(read_json);
            let mut out = BTreeMap::new();
            for (p, fp) in fps {
                let prev = baseline.as_ref().and_then(|b| b.package(&p));
                let entry = match prev {
                    Some(bp) if bp.fingerprint == fp => MatrixEntry {
                        fingerprint: fp,
                        changed: false,
                        version: bp.version.clone(),
                        reused_from_group: Some(
                            bp.reused_from_group.clone().unwrap_or_else(|| baseline.as_ref().unwrap().group_version.clone()),
                        ),
                    },
                    _ => MatrixEntry {
                        fingerprint: fp,
                        changed: true,
                        version: gv.trim_start_matches('v').to_string(),
                        reused_from_group: None,
                    },
                };
                out.insert(p, entry);
            }
            println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        }
        "package" => {
            let p = a.req("package");
            if !ALL_PACKAGES.contains(&p) {
                fail(format!("unknown package {p}"));
            }
            let version = a.req("version");
            let platform = a.req("platform");
            let out_dir = PathBuf::from(a.req("out"));
            std::fs::create_dir_all(&out_dir).unwrap_or_else(|e| fail(e));
            let entries = archive::entries(&repo, p, version, a.opt("bin-dir").map(Path::new), a.opt("tree").map(Path::new))
                .unwrap_or_else(|e| fail(e));
            let file = format!("{p}_{version}_{}.tar.gz", archive::platform_slug(platform));
            let artifact = archive::write(&entries, &out_dir.join(&file), platform).unwrap_or_else(|e| fail(e));
            let built = archive::BuiltArtifact { package: p.into(), version: version.into(), file: file.clone(), artifact };
            write_json(&out_dir.join(format!("{file}.json")), &built);
            println!("{}", out_dir.join(file).display());
        }
        "group" => group(&a, &repo),
        "openwrt" => openwrt_cmd(&a, &repo),
        "artifact" => {
            let stage = PathBuf::from(a.req("stage"));
            let info: openwrt::StageInfo = read_json(&stage.join("stage.json").to_string_lossy());
            let out_dir = stage.parent().map(Path::to_path_buf).unwrap_or_default();
            let path = out_dir.join(&info.file);
            let (size, sha256) = meduza_np_updater::store::sha256_file(&path).unwrap_or_else(|e| fail(e));
            let artifact = meduza_np_protocol::group::Artifact {
                platform: info.platform.clone(),
                format: info.format.clone(),
                url: String::new(),
                size,
                sha256,
                oci: None,
            };
            let built = archive::BuiltArtifact { package: info.package, version: info.version, file: info.file.clone(), artifact };
            write_json(&out_dir.join(format!("{}.json", info.file)), &built);
            println!("{}", path.display());
        }
        "classify" => {
            let matrix: BTreeMap<String, MatrixEntry> = read_json(a.req("matrix"));
            let changed: Vec<&str> = matrix.iter().filter(|(_, m)| m.changed).map(|(p, _)| p.as_str()).collect();
            let pick = |f: &dyn Fn(&str) -> bool| changed.iter().copied().filter(|p| f(p)).collect::<Vec<_>>();
            let has_crate = |p: &str| repo.join("packages").join(p).join("Cargo.toml").is_file();
            let out = json!({
                "rust": pick(&|p| has_crate(p) && !PROVIDER_RECIPES.contains(&p)),
                "providers": pick(&|p| PROVIDER_RECIPES.contains(&p)),
                "linux": pick(&|p| ARCHIVE_PACKAGES.contains(&p)),
                "openwrt": pick(&|p| openwrt::OPENWRT_PACKAGES.contains(&p) && !openwrt::NOARCH_PACKAGES.contains(&p)),
                "noarch": pick(&|p| openwrt::NOARCH_PACKAGES.contains(&p)),
                "images": pick(&|p| oci::image_platforms(p).is_some()),
            });
            println!("{out}");
        }
        "sbom" => {
            let targets: Vec<String> = a.req("targets").split(',').map(str::to_string).collect();
            let meta = fingerprint::cargo_metadata(&repo, &targets[0]).unwrap_or_else(|e| fail(e));
            let ctx = fingerprint::Ctx::load(&repo, targets, Some(meta)).unwrap_or_else(|e| fail(e));
            let ts = meduza_np_plugin_sdk::unix_to_rfc3339(now_unix());
            let bom =
                sbom::build(&ctx, a.req("package"), a.req("version"), &ts, a.opt("image-os").map(Path::new)).unwrap_or_else(|e| fail(e));
            match a.opt("out") {
                Some(f) => {
                    write_json(Path::new(f), &bom);
                }
                None => println!("{}", serde_json::to_string_pretty(&bom).unwrap_or_default()),
            }
        }
        "oci-layout" => {
            let p = a.req("package");
            if !ARCHIVE_PACKAGES.contains(&p) {
                fail(format!("{p} has no distribution image (runtime images are built with Podman)"));
            }
            let img = oci::write_distribution(p, a.req("image"), Path::new(a.req("artifacts")), Path::new(a.req("out")), a.opt("source"))
                .unwrap_or_else(|e| fail(e));
            if let Some(f) = a.opt("describe") {
                write_json(Path::new(f), &img);
            }
            println!("{}", serde_json::to_string_pretty(&img).unwrap_or_default());
        }
        "oci-describe" => {
            let raw = std::fs::read(a.req("raw")).unwrap_or_else(|e| fail(format!("{}: {e}", a.req("raw"))));
            let img = oci::describe(a.req("package"), a.req("version"), a.req("image"), &raw).unwrap_or_else(|e| fail(e));
            if let Some(pl) = oci::image_platforms(&img.package) {
                let missing: Vec<&&str> = pl.iter().filter(|p| !img.manifests.contains_key(**p)).collect();
                if !missing.is_empty() {
                    fail(format!("{}: the pushed index has no image for {missing:?}", img.reference()));
                }
            }
            match a.opt("out") {
                Some(f) => {
                    write_json(Path::new(f), &img);
                }
                None => println!("{}", serde_json::to_string_pretty(&img).unwrap_or_default()),
            }
        }
        "oci-check" => {
            let group: GroupManifest = read_json(a.req("manifest"));
            let raw = std::fs::read(a.req("raw")).unwrap_or_else(|e| fail(format!("{}: {e}", a.req("raw"))));
            let problems = oci::check(&group, a.req("ref"), &raw);
            if !problems.is_empty() {
                fail(problems.join("\n  "));
            }
            println!("ok {}", a.req("ref"));
        }
        "mihomo-assets" => mihomo_assets_cmd(&a, &repo),
        "sign-pointer" => {
            let site = PathBuf::from(a.req("site"));
            let gv = a.req("group-version");
            let rel = format!("releases/{gv}/manifest.json");
            let bytes = std::fs::read(site.join(&rel)).unwrap_or_else(|e| fail(format!("{rel}: {e}")));
            let days: u64 = a.opt("expires-days").and_then(|v| v.parse().ok()).unwrap_or(30);
            let now = now_unix();
            let ptr = ChannelPointer {
                format_version: GROUP_FORMAT_VERSION,
                channel: a.req("channel").into(),
                group_version: gv.into(),
                sequence: a.req("sequence").parse().unwrap_or_else(|_| fail("bad --sequence")),
                manifest: rel,
                manifest_sha256: meduza_np_protocol::digest::sha256_hex(&bytes),
                expires_at: meduza_np_plugin_sdk::unix_to_rfc3339(now + days * 86_400),
                published_at: meduza_np_plugin_sdk::unix_to_rfc3339(now),
            };
            let path = site.join(format!("channels/{}.json", ptr.channel));
            let bytes = write_json(&path, &ptr);
            sign_file(&path, &bytes, &a.all("key"));
            println!("{}", path.display());
        }
        other => fail(format!("unknown command {other}")),
    }
}

fn group(a: &Args, repo: &Path) {
    let gv = a.req("group-version");
    if !meduza_np_updater::config::valid_version_label(gv) {
        fail(format!("bad group version {gv}"));
    }
    let sequence: u64 = a.req("sequence").parse().unwrap_or_else(|_| fail("bad --sequence"));
    let matrix: BTreeMap<String, MatrixEntry> = read_json(a.req("matrix"));
    let baseline: Option<GroupManifest> = a.opt("baseline").map(read_json);
    if let Some(b) = &baseline {
        if sequence <= b.sequence {
            fail(format!("sequence {sequence} must be greater than the baseline's {}", b.sequence));
        }
    }
    let url_base = a.req("url-base").trim_end_matches('/').to_string();
    let site = PathBuf::from(a.req("site"));
    let channel = a.opt("channel").unwrap_or("stable");
    let platforms: Vec<String> = a.opt("platforms").map(|p| p.split(',').map(str::to_string).collect()).unwrap_or_else(|| {
        let targets: Vec<String> = openwrt::TARGET_PLATFORMS.iter().map(|(t, _)| t.to_string()).collect();
        PLATFORMS.iter().map(|s| s.to_string()).chain(openwrt::platforms(&targets)).collect()
    });

    // built artifacts of changed packages
    let mut built: BTreeMap<String, Vec<archive::BuiltArtifact>> = BTreeMap::new();
    let art_dir = a.req("artifacts");
    for e in std::fs::read_dir(art_dir).unwrap_or_else(|e| fail(format!("{art_dir}: {e}"))).flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if [".tar.gz.json", ".ipk.json", ".apk.json"].iter().any(|s| name.ends_with(s)) {
            let b: archive::BuiltArtifact = read_json(&e.path().to_string_lossy());
            // re-verify the archive we are about to publish
            meduza_np_updater::store::verify_file(&Path::new(art_dir).join(&b.file), b.artifact.size, &b.artifact.sha256)
                .unwrap_or_else(|err| fail(format!("{}: {err}", b.file)));
            built.entry(b.package.clone()).or_default().push(b);
        }
    }

    // OCI images (distribution + runtime) of this run
    let mut images: BTreeMap<String, oci::OciImage> = BTreeMap::new();
    if let Some(dir) = a.opt("oci") {
        for e in std::fs::read_dir(dir).unwrap_or_else(|e| fail(format!("{dir}: {e}"))).flatten() {
            if e.file_name().to_string_lossy().ends_with(".oci.json") {
                let img: oci::OciImage = read_json(&e.path().to_string_lossy());
                if let Some(prev) = images.insert(img.package.clone(), img) {
                    fail(format!("{}: two OCI records", prev.package));
                }
            }
        }
    }
    let require_oci = a.opt("require-oci").is_some();

    let mut packages = Vec::new();
    let mut problems = Vec::new();
    for (name, m) in &matrix {
        let manifest: PackageManifest = read_json(&repo.join("packages").join(name).join("manifest.json").to_string_lossy());
        let gp = if m.changed {
            let arts = built.get(name).cloned().unwrap_or_default();
            let mut artifacts = vec![];
            for b in arts {
                if b.version != m.version {
                    problems.push(format!("{name}: built {} but the matrix says {}", b.version, m.version));
                }
                let mut art = b.artifact.clone();
                art.url = format!("{url_base}/{}", b.file);
                artifacts.push(art);
            }
            artifacts.sort_by(|x, y| x.platform.cmp(&y.platform));
            GroupPackage {
                name: name.clone(),
                version: m.version.clone(),
                fingerprint: m.fingerprint.clone(),
                reused_from_group: None,
                requires: manifest.requires.clone(),
                conflicts: manifest.conflicts.clone(),
                artifacts,
                upstream: upstream_metadata(repo, name),
                migration: None,
            }
        } else {
            let Some(prev) = baseline.as_ref().and_then(|b| b.package(name)) else {
                problems.push(format!("{name}: marked reused but not in the baseline"));
                continue;
            };
            GroupPackage { reused_from_group: m.reused_from_group.clone(), ..prev.clone() }
        };
        let mut gp = gp;
        if let Some(img) = images.remove(name) {
            if let Err(e) = oci::apply(&mut gp, &img) {
                problems.push(e);
            }
        }
        if require_oci {
            problems.extend(oci::completeness(&gp));
        }
        for p in platforms.iter().filter(|p| applies(name, p)) {
            for f in formats_for(name, p) {
                if !gp.artifacts.iter().any(|x| x.format == *f && (&x.platform == p || x.platform == "any")) {
                    problems.push(format!("{name} {}: no {f} artifact for {p}", gp.version));
                }
            }
        }
        packages.push(gp);
    }
    problems.extend(images.keys().map(|p| format!("{p}: OCI record for a package outside the matrix")));
    if !problems.is_empty() {
        fail(format!("group incomplete (nothing published):\n  {}", problems.join("\n  ")));
    }
    let now = now_unix();
    let days: u64 = a.opt("expires-days").and_then(|v| v.parse().ok()).unwrap_or(30);
    let api = ApiRange { min: meduza_np_protocol::PLANNER_API_VERSION, max: meduza_np_protocol::PLANNER_API_VERSION };
    let manifest = GroupManifest {
        format_version: GROUP_FORMAT_VERSION,
        group_version: gv.into(),
        sequence,
        source_commit: a.req("commit").into(),
        published_at: meduza_np_plugin_sdk::unix_to_rfc3339(now),
        expires_at: meduza_np_plugin_sdk::unix_to_rfc3339(now + days * 86_400),
        platforms,
        compatibility: Compatibility {
            planner_api: api,
            manager_api: ApiRange { min: 1, max: 1 },
            config_schema: ApiRange { min: 1, max: 1 },
        },
        packages,
        notes: a.opt("notes").map(str::to_string),
    };
    let rel = format!("releases/{gv}/manifest.json");
    let mpath = site.join(&rel);
    if mpath.exists() {
        let existing = std::fs::read(&mpath).unwrap_or_default();
        let old: Value = serde_json::from_slice(&existing).unwrap_or(json!({}));
        if old["sequence"] != json!(sequence) || old["sourceCommit"] != json!(manifest.source_commit) {
            fail(format!("{rel} already published with different content; immutable releases are never overwritten"));
        }
    }
    let bytes = write_json(&mpath, &manifest);
    let keys = a.all("key");
    sign_file(&mpath, &bytes, &keys);
    // The channel pointer is written last: only now does the complete group become visible (§15.1 step 9).
    let ptr = ChannelPointer {
        format_version: GROUP_FORMAT_VERSION,
        channel: channel.into(),
        group_version: gv.into(),
        sequence,
        manifest: rel,
        manifest_sha256: meduza_np_protocol::digest::sha256_hex(&bytes),
        expires_at: meduza_np_plugin_sdk::unix_to_rfc3339(now + days * 86_400),
        published_at: meduza_np_plugin_sdk::unix_to_rfc3339(now),
    };
    let ppath = site.join(format!("channels/{channel}.json"));
    let pbytes = write_json(&ppath, &ptr);
    sign_file(&ppath, &pbytes, &keys);
    println!("{}\n{}", mpath.display(), ppath.display());
}

/// OpenWrt packages of one package for every architecture of a target (or once, architecture-independent).
fn openwrt_cmd(a: &Args, repo: &Path) {
    let p = a.req("package");
    let version = a.req("version");
    let out_dir = PathBuf::from(a.req("out"));
    std::fs::create_dir_all(&out_dir).unwrap_or_else(|e| fail(e));
    let arches: Vec<&str> = if a.opt("noarch").is_some() {
        if !openwrt::NOARCH_PACKAGES.contains(&p) {
            fail(format!("{p} is architecture-specific"));
        }
        vec!["noarch"]
    } else {
        let t = a.opt("target").or(a.opt("platform")).unwrap_or_else(|| fail("--target, --platform or --noarch is required"));
        openwrt::arches_for(t).unwrap_or_else(|e| fail(e)).to_vec()
    };
    let formats: Vec<String> = a.opt("formats").unwrap_or("ipk,apk").split(',').map(str::to_string).collect();
    let trust = a.opt("trust").map(PathBuf::from);
    if p == "meduza-np-updater" {
        let Some(path) = trust.as_deref() else {
            fail("meduza-np-updater needs --trust <release-keys.json> (the OpenWrt package carries the trust store)");
        };
        if !path.is_file() {
            fail(format!("trust store {} is missing; run prepare-trust before packaging meduza-np-updater", path.display()));
        }
    }
    let data = openwrt::entries(repo, p, version, a.opt("bin-dir").map(Path::new), a.opt("tree").map(Path::new), trust.as_deref())
        .unwrap_or_else(|e| fail(e));
    for arch in arches {
        let platform = openwrt::platform(p, arch);
        for f in &formats {
            let pkg_arch = openwrt::pkg_arch(p, arch, f);
            let file = openwrt::file_name(p, version, &pkg_arch, f);
            let c = openwrt::control(repo, p, version, &pkg_arch, &data).unwrap_or_else(|e| fail(e));
            match f.as_str() {
                "ipk" => {
                    let artifact = openwrt::write_ipk(&data, &c, &out_dir.join(&file), &platform).unwrap_or_else(|e| fail(e));
                    let built = archive::BuiltArtifact { package: p.into(), version: version.into(), file: file.clone(), artifact };
                    write_json(&out_dir.join(format!("{file}.json")), &built);
                    println!("{}", out_dir.join(&file).display());
                }
                "apk" => {
                    let stage = openwrt::write_apk_stage(&data, &c, &out_dir, &file, &platform).unwrap_or_else(|e| fail(e));
                    println!("{}", stage.display());
                }
                other => fail(format!("unknown OpenWrt format {other}")),
            }
        }
    }
}

/// Pins one Mihomo release in mihomo-pkg's assets.json (`mihomo_assets.rs`). `--release` takes a saved
/// `releases/tags/<tag>` document instead of asking the GitHub API (offline re-runs, rate limits).
fn mihomo_assets_cmd(a: &Args, repo: &Path) {
    use meduza_np_plugin_sdk::fetch::{fetch_to_file, FetchOptions};
    let tag = a.req("tag");
    let manifest_path = a.opt("manifest").map(PathBuf::from).unwrap_or_else(|| repo.join("packages/meduza-np-mihomo-pkg/assets.json"));
    let manifest: Value = read_json(&manifest_path.to_string_lossy());
    let gh = mihomo_assets::github_repo(manifest["upstream"].as_str().unwrap_or_default()).unwrap_or_else(|e| fail(e));
    let work = a.opt("work").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join(format!("meduza-np-mihomo-{tag}")));
    std::fs::create_dir_all(&work).unwrap_or_else(|e| fail(format!("{}: {e}", work.display())));
    let release: Value = match a.opt("release") {
        Some(f) => read_json(f),
        None => {
            let dest = work.join("release.json");
            let url = format!("https://api.github.com/repos/{gh}/releases/tags/{tag}");
            let opts =
                FetchOptions { timeout_seconds: 60, max_bytes: 8 << 20, proxy: None, header: Some("Accept: application/vnd.github+json") };
            fetch_to_file(&url, &dest, &opts).unwrap_or_else(|e| fail(format!("{url}: {e}")));
            read_json(&dest.to_string_lossy())
        }
    };
    let planned = mihomo_assets::plan(&manifest, &release, &gh, tag).unwrap_or_else(|e| fail(e));
    let mut assets = Vec::new();
    for p in &planned {
        let gz = work.join(&p.name);
        if std::fs::metadata(&gz).map(|m| m.len()).ok() != Some(p.size) {
            let opts = FetchOptions { timeout_seconds: 600, max_bytes: p.size, proxy: None, header: None };
            fetch_to_file(&p.url, &gz, &opts).unwrap_or_else(|e| fail(format!("{}: {e}", p.url)));
        }
        let probe = meduza_np_mihomo_pkg::probe_version;
        let probe: Option<mihomo_assets::Probe<'_>> =
            if meduza_np_mihomo_pkg::host_platform() == Some(p.platform.as_str()) { Some(&probe) } else { None };
        let asset = mihomo_assets::verify(p, tag, &gz, probe).unwrap_or_else(|e| fail(e));
        eprintln!(
            "{} {} {} bytes sha256 {}{}{}",
            p.platform,
            p.name,
            asset.size,
            asset.sha256,
            if p.digest.is_some() { ", matches the release digest" } else { ", release has no digest" },
            if probe.is_some() { ", version probe ok" } else { "" }
        );
        assets.push(asset);
    }
    let keep = a.opt("keep").map(|k| k.parse().unwrap_or_else(|_| fail("bad --keep")));
    let entry = meduza_np_mihomo_pkg::VersionAssets { tag: tag.into(), assets };
    let out = mihomo_assets::merge(&manifest, entry, keep).unwrap_or_else(|e| fail(e));
    write_json(&manifest_path, &out);
    println!("{}", manifest_path.display());
}

/// mihomo-pkg: tested upstream versions/assets travel with the group (not as our artifacts, §13.2).
fn upstream_metadata(repo: &Path, name: &str) -> Option<Value> {
    if name != "meduza-np-mihomo-pkg" {
        return None;
    }
    std::fs::read(repo.join("packages/meduza-np-mihomo-pkg/assets.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())
}
