//! Base image replacement support (§13.5, §14.3.2). The host helper `deploy/podman/base-image.sh` replaces the two
//! base-container containers; the updater only supplies what it can verify:
//!
//! * `base-image`: the base image of the verified (signed) package group for this platform — the only image
//!   reference the helper deploys, besides one it recorded as running before.
//! * `store-check`: run by the *candidate* image against the store volume mounted read-only. Its baseline updater is
//!   the recovery fallback after the replacement, so it must understand the installed set and the journal, and no
//!   update may be half done. The same check guards a rollback to an older image.

use std::collections::BTreeMap;

use meduza_np_protocol::group::GroupManifest;
use serde::Serialize;
use serde_json::Value;

use crate::engine::Phase;
use crate::store::Store;

pub const BASE_IMAGE_PACKAGE: &str = "meduza-np-base-container";
/// Format versions of `installed.json` and `journal.json` this updater reads and writes.
pub const INSTALLED_FORMAT_VERSION: u64 = 1;
pub const JOURNAL_FORMAT_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaseImageTarget {
    pub group_version: String,
    pub version: String,
    /// `<repo>@sha256:<image index digest>` — the multi-platform reference deployments pin (D30).
    pub reference: String,
    /// Image manifest digest for this platform inside that index (`sha256:…`).
    pub platform_digest: String,
    pub platform: String,
    /// Version of the updater the image embeds (= the group's updater, D30); checked against the image label.
    pub updater_version: Option<String>,
}

/// The base image the verified group pins for `platform`.
pub fn base_image_target(group: &GroupManifest, platform: &str) -> Result<BaseImageTarget, String> {
    let gv = &group.group_version;
    let pkg = group.package(BASE_IMAGE_PACKAGE).ok_or_else(|| format!("group {gv} has no {BASE_IMAGE_PACKAGE}"))?;
    let art = pkg
        .artifacts
        .iter()
        .find(|a| a.format == "oci" && a.platform == platform)
        .ok_or_else(|| format!("group {gv}: {BASE_IMAGE_PACKAGE} {} has no image for {platform}", pkg.version))?;
    let reference = art.oci.clone().ok_or_else(|| format!("group {gv}: {BASE_IMAGE_PACKAGE} image for {platform} is not pinned"))?;
    if !pinned_reference(&reference) {
        return Err(format!("group {gv}: {BASE_IMAGE_PACKAGE} reference {reference:?} is not <repository>@sha256:<digest>"));
    }
    let digest = art.sha256.to_ascii_lowercase();
    if !is_sha256_hex(&digest) {
        return Err(format!("group {gv}: {BASE_IMAGE_PACKAGE} {platform}: invalid manifest digest"));
    }
    Ok(BaseImageTarget {
        group_version: gv.clone(),
        version: pkg.version.clone(),
        reference,
        platform_digest: format!("sha256:{digest}"),
        platform: platform.into(),
        updater_version: group.package("meduza-np-updater").map(|p| p.version.clone()),
    })
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// `<registry>/<path>/meduza-np-base-container@sha256:<64 hex>`: a digest reference to this project's image.
pub fn pinned_reference(r: &str) -> bool {
    let Some((repo, digest)) = r.split_once("@sha256:") else { return false };
    let name_ok = repo.rsplit('/').next() == Some(BASE_IMAGE_PACKAGE)
        && repo.contains('/')
        && repo.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_' | b'/' | b':'));
    name_ok && is_sha256_hex(digest)
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreCheck {
    pub compatible: bool,
    /// Version of this (baseline) updater, from the image metadata.
    pub updater_version: Option<String>,
    /// Updater version persisted in the store (the entrypoint prefers it while it is healthy).
    pub store_updater_version: Option<String>,
    pub group_version: Option<String>,
    pub generation: Option<u64>,
    pub packages: BTreeMap<String, String>,
    pub problems: Vec<String>,
    pub warnings: Vec<String>,
}

fn read_raw(path: &std::path::Path, limit: u64) -> Result<Option<Value>, String> {
    meduza_np_runtime::fsutil::read_json::<Value>(path, limit).map_err(|e| format!("{}: {e}", path.display()))
}

/// Read-only check that this updater can take over `store` on `platform` (never writes, the store is mounted
/// read-only for it).
pub fn store_check(store: &Store, platform: &str, updater_version: Option<&str>) -> StoreCheck {
    let mut c = StoreCheck { updater_version: updater_version.map(str::to_string), ..Default::default() };
    match read_raw(&store.installed_path(), 1024 * 1024) {
        Err(e) => c.problems.push(format!("installed set unreadable: {e}")),
        Ok(None) => c.warnings.push("no installed set yet (blank store): the image's updater bootstraps it".into()),
        Ok(Some(v)) => {
            let fv = v.get("formatVersion").and_then(Value::as_u64).unwrap_or(0);
            if fv != INSTALLED_FORMAT_VERSION {
                c.problems.push(format!("installed set format {fv}, this updater reads {INSTALLED_FORMAT_VERSION}"));
            } else {
                match serde_json::from_value::<meduza_np_protocol::group::InstalledSet>(v) {
                    Err(e) => c.problems.push(format!("installed set: {e}")),
                    Ok(s) => {
                        if s.backend != "store" {
                            c.problems.push(format!("installed set belongs to backend {:?}, not the store", s.backend));
                        }
                        if s.platform != platform {
                            c.problems.push(format!("store holds {} packages, this image runs {platform}", s.platform));
                        }
                        c.group_version = s.group_version.clone();
                        c.generation = Some(s.generation);
                        c.store_updater_version = s.packages.get("meduza-np-updater").map(|p| p.version.clone());
                        c.packages = s.packages.iter().map(|(k, p)| (k.clone(), p.version.clone())).collect();
                        if !store.current_link().join("usr").is_dir() {
                            c.problems.push("store has an installed set but no current view".into());
                        }
                    }
                }
            }
        }
    }
    match read_raw(&store.journal_path(), 4 * 1024 * 1024) {
        Err(e) => c.problems.push(format!("journal unreadable: {e}")),
        Ok(None) => {}
        Ok(Some(v)) => {
            let fv = v.get("formatVersion").and_then(Value::as_u64).unwrap_or(0);
            let phase = v.get("phase").cloned().map(serde_json::from_value::<Phase>);
            let group = v.get("groupVersion").and_then(Value::as_str).unwrap_or("?");
            match phase {
                _ if fv != JOURNAL_FORMAT_VERSION => {
                    c.problems.push(format!("update journal format {fv}, this updater reads {JOURNAL_FORMAT_VERSION}"))
                }
                Some(Ok(Phase::Done | Phase::RolledBack | Phase::Failed)) => {}
                Some(Ok(p)) => c.problems.push(format!(
                    "package update {group} is in phase {}; wait until it finishes or rolls back",
                    serde_json::to_value(p).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
                )),
                _ => c.problems.push("update journal has an unknown phase".into()),
            }
        }
    }
    if let Err(e) = read_raw(&store.state_path(), 256 * 1024) {
        c.warnings.push(format!("updater state unreadable, it is rebuilt: {e}"));
    }
    if updater_version.is_none() {
        c.warnings.push("image does not declare its updater version".into());
    }
    c.compatible = c.problems.is_empty();
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_protocol::group::{Artifact, GroupPackage};
    use serde_json::json;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mnp-image-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn pkg(name: &str, version: &str, artifacts: Vec<Artifact>) -> GroupPackage {
        GroupPackage {
            name: name.into(),
            version: version.into(),
            fingerprint: "f".into(),
            reused_from_group: None,
            requires: vec![],
            conflicts: vec![],
            artifacts,
            upstream: None,
            migration: None,
        }
    }

    fn oci(platform: &str, digest: char, reference: Option<&str>) -> Artifact {
        Artifact {
            platform: platform.into(),
            format: "oci".into(),
            url: "https://ghcr.io/v2/x/manifests/sha256:0".into(),
            size: 1,
            sha256: digest.to_string().repeat(64),
            chunks: vec![],
            oci: reference.map(str::to_string),
        }
    }

    #[test]
    fn target_comes_from_the_group_for_this_platform() {
        let r = format!("ghcr.io/meduza-np/meduza-np-base-container@sha256:{}", "c".repeat(64));
        let g = crate::resolve::tests::group(
            "v0.9.0",
            vec![
                pkg(BASE_IMAGE_PACKAGE, "0.4.0", vec![oci("linux/amd64", 'a', Some(&r)), oci("linux/arm64", 'b', Some(&r))]),
                pkg("meduza-np-updater", "0.3.1", vec![]),
            ],
        );
        let t = base_image_target(&g, "linux/arm64").unwrap();
        assert_eq!(t.reference, r);
        assert_eq!(t.platform_digest, format!("sha256:{}", "b".repeat(64)));
        assert_eq!((t.version.as_str(), t.updater_version.as_deref()), ("0.4.0", Some("0.3.1")));
        assert!(base_image_target(&g, "linux/arm/v7").unwrap_err().contains("no image for linux/arm/v7"));

        let tag = crate::resolve::tests::group(
            "v0.9.0",
            vec![pkg(
                BASE_IMAGE_PACKAGE,
                "0.4.0",
                vec![oci("linux/amd64", 'a', Some("ghcr.io/meduza-np/meduza-np-base-container:stable"))],
            )],
        );
        assert!(base_image_target(&tag, "linux/amd64").unwrap_err().contains("is not <repository>@sha256"));
        let none = crate::resolve::tests::group("v0.9.0", vec![pkg(BASE_IMAGE_PACKAGE, "0.4.0", vec![oci("linux/amd64", 'a', None)])]);
        assert!(base_image_target(&none, "linux/amd64").unwrap_err().contains("not pinned"));
        assert!(base_image_target(&crate::resolve::tests::group("v0.9.0", vec![]), "linux/amd64").is_err());
    }

    #[test]
    fn pinned_references() {
        let d = "e".repeat(64);
        assert!(pinned_reference(&format!("ghcr.io/meduza-np/meduza-np-base-container@sha256:{d}")));
        assert!(pinned_reference(&format!("localhost:5000/lab/meduza-np-base-container@sha256:{d}")));
        assert!(!pinned_reference(&format!("ghcr.io/meduza-np/meduza-np-manager@sha256:{d}")));
        assert!(!pinned_reference(&format!("meduza-np-base-container@sha256:{d}")));
        assert!(!pinned_reference(&format!("ghcr.io/x/meduza-np-base-container@sha256:{}", "E".repeat(64))));
        assert!(!pinned_reference("ghcr.io/x/meduza-np-base-container@sha256:abc"));
        assert!(!pinned_reference(&format!("ghcr.io/x/meduza-np-base-container@sha256:{d};rm")));
    }

    #[test]
    fn store_check_accepts_a_settled_store_and_refuses_what_it_cannot_take_over() {
        let dir = tmp("check");
        let store = Store::new(dir.clone());
        let c = store_check(&store, "linux/amd64", Some("0.3.1"));
        assert!(c.compatible, "blank store: {:?}", c.problems);

        let installed = json!({
            "formatVersion": 1, "generation": 4, "groupVersion": "v0.8.0", "backend": "store", "platform": "linux/amd64",
            "packages": {
                "meduza-np-planner": {"version": "0.8.6", "sha256": "0".repeat(64), "source": "group:v0.8.0", "object": "o"},
                "meduza-np-updater": {"version": "0.3.2", "sha256": "1".repeat(64), "source": "group:v0.8.0", "object": "u"}
            },
            "activatedAt": "2026-10-24T00:00:00Z"
        });
        std::fs::write(store.installed_path(), installed.to_string()).unwrap();
        let c = store_check(&store, "linux/amd64", Some("0.3.1"));
        assert!(!c.compatible);
        assert!(c.problems[0].contains("no current view"));
        std::fs::create_dir_all(dir.join("views/g4/usr")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("views/g4", store.current_link()).unwrap();
        #[cfg(not(unix))]
        std::fs::create_dir_all(store.current_link().join("usr")).unwrap();
        let c = store_check(&store, "linux/amd64", Some("0.3.1"));
        assert!(c.compatible, "{:?}", c.problems);
        assert_eq!(c.group_version.as_deref(), Some("v0.8.0"));
        assert_eq!(c.store_updater_version.as_deref(), Some("0.3.2"));
        assert_eq!(c.packages.get("meduza-np-planner").map(String::as_str), Some("0.8.6"));

        // an image for another architecture
        let c = store_check(&store, "linux/arm64", Some("0.3.1"));
        assert!(c.problems.iter().any(|p| p.contains("this image runs linux/arm64")));

        // a newer installed-set format written by a newer updater (rollback to an old image)
        let mut newer = installed.clone();
        newer["formatVersion"] = json!(2);
        std::fs::write(store.installed_path(), newer.to_string()).unwrap();
        assert!(store_check(&store, "linux/amd64", None).problems.iter().any(|p| p.contains("format 2")));
        std::fs::write(store.installed_path(), installed.to_string()).unwrap();

        // an update in progress, then finished
        let journal = |phase: &str, fv: u32| {
            json!({"formatVersion": fv, "groupVersion": "v0.9.0", "phase": phase, "startedAt": "t", "updatedAt": "t"}).to_string()
        };
        std::fs::write(store.journal_path(), journal("switched", 1)).unwrap();
        let c = store_check(&store, "linux/amd64", None);
        assert!(c.problems.iter().any(|p| p.contains("v0.9.0 is in phase switched")), "{:?}", c.problems);
        std::fs::write(store.journal_path(), journal("rolled-back", 1)).unwrap();
        assert!(store_check(&store, "linux/amd64", None).compatible);
        std::fs::write(store.journal_path(), journal("done", 3)).unwrap();
        assert!(store_check(&store, "linux/amd64", None).problems.iter().any(|p| p.contains("journal format 3")));
        std::fs::write(store.journal_path(), journal("teleporting", 1)).unwrap();
        assert!(!store_check(&store, "linux/amd64", None).compatible);
        std::fs::write(store.journal_path(), "{not json").unwrap();
        assert!(store_check(&store, "linux/amd64", None).problems.iter().any(|p| p.contains("journal unreadable")));
        std::fs::remove_dir_all(&dir).ok();
    }
}
