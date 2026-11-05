//! OCI images of a package group (§15.1 step 6, §15.4, D30).
//!
//! - Distribution images of the generic archive packages are written by this tool as an OCI image layout: one image
//!   per `linux/*` platform whose single layer holds the package archive (`meduza-np/<package>/<file>`) and its
//!   artifact descriptor, tied together by an image index. No timestamps, sorted entries, gzip mtime 0 ⇒ the same
//!   archives always give the same digests, so re-running a tag pushes identical blobs.
//! - Runtime images (manager, base-container) are built by Podman in release.yml; `describe` turns the pushed image
//!   index into the same [`OciImage`] record.
//! - The group manifest pins every image by its index digest (`<repo>@sha256:…` in `Artifact::oci`). Runtime images
//!   additionally get one `oci` format artifact per platform describing that platform's manifest.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use meduza_np_protocol::group::{Artifact, GroupManifest, GroupPackage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::archive::BuiltArtifact;

pub const INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
pub const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";
const LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

/// Packages shipped as runtime images (built with Podman) and the platforms each one is published for. The manager
/// is a server application: amd64 + arm64 (its Node.js base image has no tested armv7 build).
pub const IMAGE_PACKAGES: &[(&str, &[&str])] = &[
    ("meduza-np-base-container", &["linux/amd64", "linux/arm64", "linux/arm/v7"]),
    ("meduza-np-manager", &["linux/amd64", "linux/arm64"]),
];

pub fn image_platforms(package: &str) -> Option<&'static [&'static str]> {
    IMAGE_PACKAGES.iter().find(|(p, _)| *p == package).map(|(_, pl)| *pl)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub digest: String,
    pub size: u64,
}

/// One published (or about to be published) multi-platform image.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OciImage {
    pub package: String,
    pub version: String,
    /// Repository without tag or digest, e.g. `ghcr.io/owner/meduza-np-wireguard`.
    pub image: String,
    /// Digest of the image index (what deployments and the group pin).
    pub index: String,
    /// Group platform (`linux/arm/v7`) → that platform's image manifest.
    pub manifests: BTreeMap<String, Descriptor>,
}

impl OciImage {
    /// `<repo>@sha256:…`
    pub fn reference(&self) -> String {
        format!("{}@{}", self.image, self.index)
    }
}

fn sha(bytes: &[u8]) -> String {
    format!("sha256:{}", meduza_np_protocol::digest::sha256_hex(bytes))
}

/// Group platform → OCI (architecture, variant).
pub fn oci_platform(platform: &str) -> Result<(&'static str, Option<&'static str>), String> {
    match platform {
        "linux/amd64" => Ok(("amd64", None)),
        "linux/arm64" => Ok(("arm64", None)),
        "linux/arm/v7" => Ok(("arm", Some("v7"))),
        other => Err(format!("no OCI platform for {other}")),
    }
}

/// OCI platform object → group platform (`arm64/v8` is plain `linux/arm64`); `None` for entries that are not images
/// of a supported platform (attestation manifests have `unknown/unknown`).
fn group_platform(p: &Value) -> Option<String> {
    let os = p["os"].as_str()?;
    let arch = p["architecture"].as_str()?;
    let variant = p["variant"].as_str().unwrap_or("");
    let s = match (arch, variant) {
        ("arm64", "" | "v8") => format!("{os}/arm64"),
        (a, "") => format!("{os}/{a}"),
        (a, v) => format!("{os}/{a}/{v}"),
    };
    oci_platform(&s).ok().map(|_| s)
}

/// Repository name rules of the distribution spec (lowercase path components, optional registry host).
pub fn valid_repository(image: &str) -> bool {
    let mut parts = image.split('/');
    let Some(host) = parts.next() else { return false };
    let rest: Vec<&str> = parts.collect();
    let comp = |c: &str| {
        !c.is_empty()
            && c.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
            && c.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
    };
    !host.is_empty()
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || b".:-".contains(&b))
        && !rest.is_empty()
        && rest.into_iter().all(comp)
}

fn layer(package: &str, file: &str, archive: &[u8], descriptor: &[u8]) -> Result<(Vec<u8>, String), String> {
    let mut tar_bytes = Vec::new();
    {
        let mut b = tar::Builder::new(&mut tar_bytes);
        b.mode(tar::HeaderMode::Deterministic);
        let dir = |b: &mut tar::Builder<&mut Vec<u8>>, name: &str| -> Result<(), String> {
            let mut h = tar::Header::new_ustar();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_mode(0o755);
            h.set_size(0);
            h.set_mtime(0);
            h.set_uid(0);
            h.set_gid(0);
            h.set_cksum();
            b.append_data(&mut h, name, std::io::empty()).map_err(|e| e.to_string())
        };
        dir(&mut b, "meduza-np/")?;
        dir(&mut b, &format!("meduza-np/{package}/"))?;
        let mut files = vec![(format!("meduza-np/{package}/{file}"), archive), (format!("meduza-np/{package}/artifact.json"), descriptor)];
        files.sort();
        for (name, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_mode(0o644);
            h.set_size(data.len() as u64);
            h.set_mtime(0);
            h.set_uid(0);
            h.set_gid(0);
            h.set_cksum();
            b.append_data(&mut h, &name, data).map_err(|e| e.to_string())?;
        }
        b.finish().map_err(|e| e.to_string())?;
    }
    let diff_id = sha(&tar_bytes);
    let mut gz = flate2::GzBuilder::new().mtime(0).write(Vec::new(), flate2::Compression::best());
    std::io::Write::write_all(&mut gz, &tar_bytes).map_err(|e| e.to_string())?;
    Ok((gz.finish().map_err(|e| e.to_string())?, diff_id))
}

fn put_blob(layout: &Path, bytes: &[u8]) -> Result<Descriptor, String> {
    let digest = sha(bytes);
    let p = layout.join("blobs/sha256").join(digest.trim_start_matches("sha256:"));
    std::fs::create_dir_all(p.parent().unwrap_or(layout)).map_err(|e| e.to_string())?;
    std::fs::write(&p, bytes).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(Descriptor { digest, size: bytes.len() as u64 })
}

fn to_bytes(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

/// Writes the distribution image of `package` (its `tar.gz` artifacts in `artifacts`, one per `linux/*` platform)
/// as an OCI image layout at `layout`, tagged `version` in the layout's `index.json`. `source` (the repository URL)
/// becomes `org.opencontainers.image.source`, which links a GHCR package to its repository.
pub fn write_distribution(package: &str, image: &str, artifacts: &Path, layout: &Path, source: Option<&str>) -> Result<OciImage, String> {
    if !valid_repository(image) {
        return Err(format!("{image} is not a valid lowercase repository name"));
    }
    let mut built: Vec<(BuiltArtifact, PathBuf)> = vec![];
    for e in std::fs::read_dir(artifacts).map_err(|e| format!("{}: {e}", artifacts.display()))?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".tar.gz.json") {
            continue;
        }
        let raw = std::fs::read(e.path()).map_err(|e| e.to_string())?;
        let b: BuiltArtifact = serde_json::from_slice(&raw).map_err(|err| format!("{name}: {err}"))?;
        if b.package == package && b.artifact.platform.starts_with("linux/") {
            let file = artifacts.join(&b.file);
            meduza_np_updater::store::verify_file(&file, b.artifact.size, &b.artifact.sha256)
                .map_err(|err| format!("{}: {err}", b.file))?;
            built.push((b, file));
        }
    }
    if built.is_empty() {
        return Err(format!("{package}: no linux tar.gz artifacts in {}", artifacts.display()));
    }
    built.sort_by(|a, b| a.0.artifact.platform.cmp(&b.0.artifact.platform));
    let version = built[0].0.version.clone();
    if let Some((b, _)) = built.iter().find(|(b, _)| b.version != version) {
        return Err(format!("{package}: artifacts of different versions ({version}, {})", b.version));
    }
    let _ = std::fs::remove_dir_all(layout);
    std::fs::create_dir_all(layout).map_err(|e| format!("{}: {e}", layout.display()))?;
    let mut annotations = json!({
        "org.opencontainers.image.title": package,
        "org.opencontainers.image.version": version,
        "org.opencontainers.image.description": format!("Meduza NP package {package} {version} (distribution image: package archive + descriptor)"),
        "io.meduza-np.package": package,
    });
    if let Some(s) = source {
        annotations["org.opencontainers.image.source"] = json!(s);
    }
    let mut manifests = BTreeMap::new();
    let mut index_entries = vec![];
    for (b, file) in &built {
        let platform = &b.artifact.platform;
        let (arch, variant) = oci_platform(platform)?;
        let archive = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let mut descr = b.clone();
        descr.artifact.url = String::new();
        descr.artifact.oci = None;
        let mut descr_bytes = serde_json::to_vec_pretty(&descr).map_err(|e| e.to_string())?;
        descr_bytes.push(b'\n');
        let (gz, diff_id) = layer(package, &b.file, &archive, &descr_bytes)?;
        let layer_d = put_blob(layout, &gz)?;
        let mut plat = json!({"architecture": arch, "os": "linux"});
        let mut config = json!({
            "architecture": arch,
            "os": "linux",
            "config": {"Labels": annotations.clone()},
            "rootfs": {"type": "layers", "diff_ids": [diff_id]},
        });
        if let Some(v) = variant {
            plat["variant"] = json!(v);
            config["variant"] = json!(v);
        }
        let config_d = put_blob(layout, &to_bytes(&config))?;
        let mut ann = annotations.clone();
        ann["io.meduza-np.artifact.sha256"] = json!(b.artifact.sha256);
        let manifest = json!({
            "schemaVersion": 2,
            "mediaType": MANIFEST_MEDIA_TYPE,
            "config": {"mediaType": CONFIG_MEDIA_TYPE, "digest": config_d.digest, "size": config_d.size},
            "layers": [{"mediaType": LAYER_MEDIA_TYPE, "digest": layer_d.digest, "size": layer_d.size}],
            "annotations": ann,
        });
        let m = put_blob(layout, &to_bytes(&manifest))?;
        index_entries.push(json!({"mediaType": MANIFEST_MEDIA_TYPE, "digest": m.digest, "size": m.size, "platform": plat}));
        manifests.insert(platform.clone(), m);
    }
    let index = json!({"schemaVersion": 2, "mediaType": INDEX_MEDIA_TYPE, "manifests": index_entries, "annotations": annotations});
    let index_d = put_blob(layout, &to_bytes(&index))?;
    // layout top level: one tagged reference to our image index
    let top = json!({
        "schemaVersion": 2,
        "mediaType": INDEX_MEDIA_TYPE,
        "manifests": [{
            "mediaType": INDEX_MEDIA_TYPE, "digest": index_d.digest, "size": index_d.size,
            "annotations": {"org.opencontainers.image.ref.name": version},
        }],
    });
    std::fs::write(layout.join("index.json"), to_bytes(&top)).map_err(|e| e.to_string())?;
    std::fs::write(layout.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#).map_err(|e| e.to_string())?;
    Ok(OciImage { package: package.into(), version, image: image.into(), index: index_d.digest, manifests })
}

/// Describes a pushed image from the raw bytes of its image index (`skopeo inspect --raw`).
pub fn describe(package: &str, version: &str, image: &str, raw_index: &[u8]) -> Result<OciImage, String> {
    if !valid_repository(image) {
        return Err(format!("{image} is not a valid lowercase repository name"));
    }
    let v: Value = serde_json::from_slice(raw_index).map_err(|e| format!("image index: {e}"))?;
    let mt = v["mediaType"].as_str().unwrap_or_default();
    if mt != INDEX_MEDIA_TYPE && mt != "application/vnd.docker.distribution.manifest.list.v2+json" {
        return Err(format!("{image}: expected a multi-platform image index, got {mt:?}"));
    }
    let mut manifests = BTreeMap::new();
    for m in v["manifests"].as_array().into_iter().flatten() {
        let Some(platform) = group_platform(&m["platform"]) else { continue };
        let digest = m["digest"].as_str().ok_or("index entry without digest")?.to_string();
        let size = m["size"].as_u64().ok_or("index entry without size")?;
        if manifests.insert(platform.clone(), Descriptor { digest, size }).is_some() {
            return Err(format!("{image}: two manifests for {platform}"));
        }
    }
    Ok(OciImage { package: package.into(), version: version.into(), image: image.into(), index: sha(raw_index), manifests })
}

/// Registry API URL of a manifest (`https://<host>/v2/<path>/manifests/<digest>`).
fn manifest_url(image: &str, digest: &str) -> String {
    let (host, path) = image.split_once('/').unwrap_or((image, ""));
    format!("https://{host}/v2/{path}/manifests/{digest}")
}

/// Records `img` in the group package: archive artifacts get the image reference, runtime image packages get one
/// `oci` artifact per platform.
pub fn apply(gp: &mut GroupPackage, img: &OciImage) -> Result<(), String> {
    if gp.name != img.package || gp.version != img.version {
        return Err(format!("{} {}: image describes {} {}", gp.name, gp.version, img.package, img.version));
    }
    let reference = img.reference();
    if image_platforms(&gp.name).is_some() {
        gp.artifacts.retain(|a| a.format != "oci");
        for (platform, d) in &img.manifests {
            gp.artifacts.push(Artifact {
                platform: platform.clone(),
                format: "oci".into(),
                url: manifest_url(&img.image, &d.digest),
                size: d.size,
                sha256: d.digest.trim_start_matches("sha256:").to_string(),
                chunks: vec![],
                oci: Some(reference.clone()),
            });
        }
        gp.artifacts.sort_by(|x, y| x.platform.cmp(&y.platform));
    } else {
        for a in gp.artifacts.iter_mut().filter(|a| a.format == "tar.gz") {
            if img.manifests.contains_key(&a.platform) {
                a.oci = Some(reference.clone());
            }
        }
    }
    Ok(())
}

/// Problems with the OCI side of a group package (§15.1 step 8 before publishing): archive packages need an image
/// reference on every generic archive, and one package names one image. (Runtime images always need their `oci`
/// artifacts; the group's per-platform format check covers that.)
pub fn completeness(gp: &GroupPackage) -> Vec<String> {
    let mut out = vec![];
    if image_platforms(&gp.name).is_none() {
        for a in gp.artifacts.iter().filter(|a| a.format == "tar.gz" && a.platform.starts_with("linux/")) {
            if a.oci.is_none() {
                out.push(format!("{} {}: no OCI image reference on the {} archive", gp.name, gp.version, a.platform));
            }
        }
    }
    let mut refs: Vec<&String> = gp.artifacts.iter().filter_map(|a| a.oci.as_ref()).collect();
    refs.sort();
    refs.dedup();
    if refs.len() > 1 {
        out.push(format!("{} {}: artifacts name different images ({})", gp.name, gp.version, refs.len()));
    }
    out
}

/// Checks a pushed image against the published group: the raw index has the pinned digest and lists a manifest for
/// every platform the group says it has (and the same platform manifest for runtime images).
pub fn check(group: &GroupManifest, reference: &str, raw_index: &[u8]) -> Vec<String> {
    let mut out = vec![];
    let Some((image, digest)) = reference.split_once('@') else { return vec![format!("{reference}: not pinned by digest")] };
    if sha(raw_index) != digest {
        out.push(format!("{reference}: registry returned an index with digest {}", sha(raw_index)));
        return out;
    }
    let img = match describe("", "", image, raw_index) {
        Ok(i) => i,
        Err(e) => return vec![e],
    };
    for gp in &group.packages {
        for a in gp.artifacts.iter().filter(|a| a.oci.as_deref() == Some(reference)) {
            match img.manifests.get(&a.platform) {
                None => out.push(format!("{} {}: {reference} has no {} image", gp.name, gp.version, a.platform)),
                Some(d) if a.format == "oci" && d.digest.trim_start_matches("sha256:") != a.sha256 => {
                    out.push(format!("{} {}: {} manifest is {} in the registry", gp.name, gp.version, a.platform, d.digest))
                }
                Some(_) => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_protocol::group::Artifact;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mnp-release-oci-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_artifacts(dir: &Path, package: &str, version: &str) {
        for platform in crate::PLATFORMS {
            let file = format!("{package}_{version}_{}.tar.gz", crate::archive::platform_slug(platform));
            let body = format!("archive of {package} for {platform}");
            std::fs::write(dir.join(&file), &body).unwrap();
            let (size, sha256) = meduza_np_updater::store::sha256_file(&dir.join(&file)).unwrap();
            let b = BuiltArtifact {
                package: package.into(),
                version: version.into(),
                file: file.clone(),
                artifact: Artifact {
                    platform: platform.to_string(),
                    format: "tar.gz".into(),
                    url: String::new(),
                    size,
                    sha256,
                    chunks: vec![],
                    oci: None,
                },
            };
            std::fs::write(dir.join(format!("{file}.json")), serde_json::to_vec(&b).unwrap()).unwrap();
        }
    }

    fn blob(layout: &Path, digest: &str) -> Vec<u8> {
        std::fs::read(layout.join("blobs/sha256").join(digest.trim_start_matches("sha256:"))).unwrap()
    }

    #[test]
    fn distribution_image_is_deterministic_and_well_formed() {
        let d = tmp("dist");
        fake_artifacts(&d, "meduza-np-wireguard", "0.3.0");
        let a = write_distribution("meduza-np-wireguard", "ghcr.io/owner/meduza-np-wireguard", &d, &d.join("l1"), None).unwrap();
        let b = write_distribution("meduza-np-wireguard", "ghcr.io/owner/meduza-np-wireguard", &d, &d.join("l2"), None).unwrap();
        assert_eq!(a, b, "same archives ⇒ same digests");
        assert_eq!(a.version, "0.3.0");
        assert_eq!(a.manifests.keys().cloned().collect::<Vec<_>>(), vec!["linux/amd64", "linux/arm/v7", "linux/arm64"]);

        let l = d.join("l1");
        let top: Value = serde_json::from_slice(&std::fs::read(l.join("index.json")).unwrap()).unwrap();
        assert_eq!(top["manifests"][0]["digest"], json!(a.index));
        assert_eq!(top["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"], json!("0.3.0"));
        let raw_index = blob(&l, &a.index);
        // describing the index the registry would return gives the same record
        assert_eq!(describe("meduza-np-wireguard", "0.3.0", "ghcr.io/owner/meduza-np-wireguard", &raw_index).unwrap(), a);
        let index: Value = serde_json::from_slice(&raw_index).unwrap();
        let arm = index["manifests"].as_array().unwrap().iter().find(|m| m["platform"]["architecture"] == "arm").unwrap();
        assert_eq!(arm["platform"]["variant"], json!("v7"));

        // the layer of the amd64 image holds exactly the archive and its descriptor, byte for byte
        let m: Value = serde_json::from_slice(&blob(&l, &a.manifests["linux/amd64"].digest)).unwrap();
        let cfg: Value = serde_json::from_slice(&blob(&l, m["config"]["digest"].as_str().unwrap())).unwrap();
        assert_eq!(cfg["architecture"], json!("amd64"));
        let gz = blob(&l, m["layers"][0]["digest"].as_str().unwrap());
        let mut tar_bytes = vec![];
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(gz.as_slice()), &mut tar_bytes).unwrap();
        assert_eq!(cfg["rootfs"]["diff_ids"][0], json!(sha(&tar_bytes)));
        let mut ar = tar::Archive::new(tar_bytes.as_slice());
        let mut names = vec![];
        for e in ar.entries().unwrap() {
            let mut e = e.unwrap();
            let name = e.path().unwrap().to_string_lossy().into_owned();
            if name.ends_with(".tar.gz") {
                let mut body = String::new();
                std::io::Read::read_to_string(&mut e, &mut body).unwrap();
                assert_eq!(body, "archive of meduza-np-wireguard for linux/amd64");
            }
            names.push(name);
        }
        assert_eq!(
            names,
            vec![
                "meduza-np/",
                "meduza-np/meduza-np-wireguard/",
                "meduza-np/meduza-np-wireguard/artifact.json",
                "meduza-np/meduza-np-wireguard/meduza-np-wireguard_0.3.0_linux-amd64.tar.gz",
            ]
        );
        assert!(write_distribution("meduza-np-wireguard", "ghcr.io/Owner/x", &d, &d.join("l3"), None).is_err(), "uppercase owner");
        assert!(write_distribution("meduza-np-tinc", "ghcr.io/owner/meduza-np-tinc", &d, &d.join("l4"), None).is_err(), "no artifacts");
        let _ = std::fs::remove_dir_all(&d);
    }

    fn gp(name: &str, artifacts: Vec<Artifact>) -> GroupPackage {
        GroupPackage {
            name: name.into(),
            version: "0.3.0".into(),
            fingerprint: "f".into(),
            reused_from_group: None,
            requires: vec![],
            conflicts: vec![],
            artifacts,
            upstream: None,
            migration: None,
        }
    }

    #[test]
    fn runtime_image_index_from_podman_and_group_checks() {
        // what `podman manifest push` + `skopeo inspect --raw` returns (with an attestation-like entry to ignore)
        let raw = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": INDEX_MEDIA_TYPE,
            "manifests": [
                {"mediaType": MANIFEST_MEDIA_TYPE, "digest": format!("sha256:{}", "a".repeat(64)), "size": 500, "platform": {"architecture": "amd64", "os": "linux"}},
                {"mediaType": MANIFEST_MEDIA_TYPE, "digest": format!("sha256:{}", "b".repeat(64)), "size": 501, "platform": {"architecture": "arm64", "os": "linux", "variant": "v8"}},
                {"mediaType": MANIFEST_MEDIA_TYPE, "digest": format!("sha256:{}", "d".repeat(64)), "size": 9, "platform": {"architecture": "unknown", "os": "unknown"}},
            ],
        }))
        .unwrap();
        let img = describe("meduza-np-manager", "0.3.0", "ghcr.io/owner/meduza-np-manager", &raw).unwrap();
        assert_eq!(img.manifests.keys().cloned().collect::<Vec<_>>(), vec!["linux/amd64", "linux/arm64"]);

        let mut p = gp("meduza-np-manager", vec![]);
        apply(&mut p, &img).unwrap();
        assert!(completeness(&p).is_empty(), "{:?}", completeness(&p));
        assert_eq!(p.artifacts[0].url, format!("https://ghcr.io/v2/owner/meduza-np-manager/manifests/sha256:{}", "a".repeat(64)));
        assert_eq!(p.artifacts[1].sha256, "b".repeat(64));
        assert_eq!(p.artifacts[0].oci.as_deref(), Some(img.reference().as_str()));

        let mut w = gp(
            "meduza-np-wireguard",
            vec![Artifact {
                platform: "linux/amd64".into(),
                format: "tar.gz".into(),
                url: "u".into(),
                size: 1,
                sha256: "s".into(),
                chunks: vec![],
                oci: None,
            }],
        );
        assert_eq!(completeness(&w).len(), 1);
        assert!(apply(&mut w, &OciImage { version: "9".into(), ..img.clone() }).is_err(), "other package/version");

        let group = GroupManifest {
            format_version: 1,
            group_version: "v0.3.0".into(),
            sequence: 1,
            source_commit: "c".into(),
            published_at: "x".into(),
            expires_at: "y".into(),
            platforms: vec![],
            compatibility: meduza_np_protocol::group::Compatibility {
                planner_api: meduza_np_protocol::manifest::ApiRange { min: 1, max: 1 },
                manager_api: meduza_np_protocol::manifest::ApiRange { min: 1, max: 1 },
                config_schema: meduza_np_protocol::manifest::ApiRange { min: 1, max: 1 },
            },
            packages: vec![p.clone()],
            notes: None,
        };
        assert!(check(&group, &img.reference(), &raw).is_empty());
        let mut other: Value = serde_json::from_slice(&raw).unwrap();
        other["manifests"].as_array_mut().unwrap().remove(1);
        let other = serde_json::to_vec(&other).unwrap();
        assert!(!check(&group, &img.reference(), &other).is_empty(), "digest mismatch");
        let moved = format!("ghcr.io/owner/meduza-np-manager@{}", sha(&other));
        let mut g2 = group.clone();
        for a in &mut g2.packages[0].artifacts {
            a.oci = Some(moved.clone());
        }
        let probs = check(&g2, &moved, &other);
        assert_eq!(probs.len(), 1, "{probs:?}");
        assert!(probs[0].contains("linux/arm64"));
    }

    #[test]
    fn repository_names() {
        assert!(valid_repository("ghcr.io/owner/meduza-np-planner"));
        assert!(valid_repository("localhost:5000/a/b_c.d"));
        assert!(!valid_repository("ghcr.io"));
        assert!(!valid_repository("ghcr.io/Owner/x"));
        assert!(!valid_repository("ghcr.io/owner/-x"));
    }
}
