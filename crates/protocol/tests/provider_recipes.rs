//! Binary provider packages that ship a built binary (openvpn-pkg, mosdns-pkg) carry a complete, pinned build
//! recipe (§4, §14.1): every upstream input locked by digest or commit, a digest-pinned builder image that the
//! Containerfile actually uses, license files named, and a build script that installs exactly the binary path
//! the manifest registers. Download/manual providers (mihomo-pkg) are excluded: they never embed the binary.

use std::path::PathBuf;

use meduza_np_protocol::manifest::{PackageManifest, PackageRole, ALL_PACKAGES};
use serde_json::Value;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn str_field<'a>(v: &'a Value, key: &str, ctx: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_else(|| panic!("{ctx}: missing string `{key}`"))
}

fn built_providers() -> Vec<(PathBuf, PackageManifest)> {
    ALL_PACKAGES
        .iter()
        .filter_map(|name| {
            let dir = repo_root().join("packages").join(name);
            let m: PackageManifest = serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
            let built = m.role == PackageRole::BinaryProvider && m.providers.iter().all(|p| !p.requires_preparation);
            built.then_some((dir, m))
        })
        .collect()
}

#[test]
fn built_provider_packages_exist() {
    let mut names: Vec<String> = built_providers().into_iter().map(|(_, m)| m.package).collect();
    names.sort();
    assert_eq!(names, vec!["meduza-np-mosdns-pkg".to_string(), "meduza-np-openvpn-pkg".to_string()]);
}

#[test]
fn recipes_are_pinned_and_consistent() {
    for (dir, m) in built_providers() {
        let ctx = m.package.as_str();
        let lock: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("upstream.lock.json")).unwrap_or_else(|e| panic!("{ctx}: upstream.lock.json: {e}")),
        )
        .unwrap_or_else(|e| panic!("{ctx}: {e}"));
        assert_eq!(lock["lockVersion"], 1, "{ctx}");
        assert_eq!(str_field(&lock, "package", ctx), ctx);

        // Builder image pinned by digest, and the Containerfile builds from exactly that image.
        let image = str_field(&lock["builder"], "image", ctx);
        let digest = image.rsplit_once("@sha256:").map(|(_, d)| d).unwrap_or_else(|| panic!("{ctx}: builder not pinned"));
        assert!(is_hex(digest, 64), "{ctx}: bad builder digest");
        let containerfile = std::fs::read_to_string(dir.join("build/Containerfile")).unwrap_or_else(|e| panic!("{ctx}: {e}"));
        let froms: Vec<&str> = containerfile.lines().filter(|l| l.starts_with("FROM ")).collect();
        assert!(!froms.is_empty(), "{ctx}: no FROM");
        for from in froms {
            let image_ref = from.split_whitespace().skip(1).find(|w| !w.starts_with("--")).unwrap_or_else(|| panic!("{ctx}: {from}"));
            assert!(image_ref == "scratch" || image_ref == image, "{ctx}: `{from}` does not use the locked builder");
        }
        assert!(containerfile.contains("COPY upstream.lock.json"), "{ctx}: Containerfile must feed the lock to the build");

        // Every source pinned: tarball by sha256 or git by full commit; licenses declared.
        let sources = lock["sources"].as_array().unwrap_or_else(|| panic!("{ctx}: sources"));
        assert!(!sources.is_empty(), "{ctx}");
        for s in sources {
            let name = str_field(s, "name", ctx);
            let sctx = format!("{ctx}/{name}");
            str_field(s, "version", &sctx);
            str_field(s, "license", &sctx);
            match (s.get("url"), s.get("git")) {
                (Some(url), None) => {
                    assert!(url.as_str().unwrap().starts_with("https://"), "{sctx}: https only");
                    assert!(is_hex(str_field(s, "sha256", &sctx), 64), "{sctx}: sha256");
                }
                (None, Some(git)) => {
                    assert!(git.as_str().unwrap().starts_with("https://"), "{sctx}: https only");
                    str_field(s, "tag", &sctx);
                    assert!(is_hex(str_field(s, "commit", &sctx), 40), "{sctx}: full commit id");
                }
                _ => panic!("{sctx}: exactly one of url/git"),
            }
            let lic = s["licenseFiles"].as_array().unwrap_or_else(|| panic!("{sctx}: licenseFiles"));
            assert!(!lic.is_empty(), "{sctx}: licenseFiles");
        }
        if let Some(tc) = lock.get("toolchain") {
            for (arch, a) in tc["archives"].as_object().unwrap_or_else(|| panic!("{ctx}: toolchain archives")) {
                assert!(str_field(a, "url", ctx).starts_with("https://"), "{ctx}/{arch}");
                assert!(is_hex(str_field(a, "sha256", ctx), 64), "{ctx}/{arch}: sha256");
            }
        }

        // The build installs the binary at the path the manifest registers and refuses dynamic ELF.
        let script = std::fs::read_to_string(dir.join("build/build.sh")).unwrap_or_else(|e| panic!("{ctx}: {e}"));
        for p in &m.providers {
            assert!(p.binary.starts_with("/usr/lib/meduza-np/providers/"), "{ctx}: provider outside the provider dir");
            assert!(script.contains(&format!("\"$out{}\"", p.binary)), "{ctx}: build.sh does not install {}", p.binary);
        }
        assert!(script.contains("Requesting program interpreter") && script.contains("(NEEDED)"), "{ctx}: static ELF gate");
        assert!(script.contains("sha256sum -c") || lock.get("toolchain").is_none(), "{ctx}: downloads must be verified");
    }
}
