use std::path::Path;
use std::process::Command;

use meduza_np_protocol::group::{GroupManifest, TrustStore};
use meduza_np_updater::sig;
use serde_json::json;

fn run(args: &[&str]) {
    let result = Command::new(env!("CARGO_BIN_EXE_meduza-np-release"))
        .args(args)
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
}

#[test]
fn real_group_cli_signs_pages_parts_for_changed_and_reused_packages() {
    let root = std::env::temp_dir().join(format!("mnp-pages-group-{}", std::process::id()));
    let dist = root.join("dist");
    let bin = root.join("bin");
    let site = root.join("site");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("meduza-np-planner"), b"fixture executable").unwrap();
    run(&[
        "package",
        "--package",
        "meduza-np-planner",
        "--version",
        "1.0.0",
        "--platform",
        "linux/amd64",
        "--bin-dir",
        bin.to_str().unwrap(),
        "--out",
        dist.to_str().unwrap(),
    ]);
    let matrix = root.join("matrix.json");
    let key = sig::generate_key().unwrap();
    let secret = root.join("release.key");
    std::fs::write(&secret, sig::encode_secret(&key)).unwrap();
    let trust = TrustStore { format_version: 1, threshold: 1, keys: vec![sig::trusted_key(&key.verifying_key(), None)] };
    let mut manifests = Vec::new();
    for sequence in 1..=2 {
        let version = format!("v{sequence}.0.0");
        let record = json!({"meduza-np-planner":{"changed":sequence == 1,"version":"1.0.0","fingerprint":"fixed","reusedFromGroup":if sequence == 1 {None} else {Some("v1.0.0")}}});
        std::fs::write(&matrix, serde_json::to_vec(&record).unwrap()).unwrap();
        let previous = site.join("releases/v1.0.0/manifest.json");
        let count = sequence.to_string();
        let mut args = vec![
            "group",
            "--group-version",
            &version,
            "--sequence",
            &count,
            "--commit",
            "abc",
            "--matrix",
            matrix.to_str().unwrap(),
            "--artifacts",
            dist.to_str().unwrap(),
            "--url-base",
            "https://github.com/o/r/releases/download/v1",
            "--pages-url",
            "https://pages.example/project",
            "--site",
            site.to_str().unwrap(),
            "--platforms",
            "linux/amd64",
            "--key",
            secret.to_str().unwrap(),
        ];
        if sequence == 2 {
            args.extend(["--baseline", previous.to_str().unwrap()]);
        }
        run(&args);
        let path = site.join(format!("releases/{version}/manifest.json"));
        let raw = std::fs::read(&path).unwrap();
        let signature = std::fs::read(path.with_extension("json.sig")).unwrap();
        sig::verify(&raw, &signature, &trust, 0).unwrap();
        let group: GroupManifest = serde_json::from_slice(&raw).unwrap();
        let artifact = &group.packages[0].artifacts[0];
        assert!(artifact.url.starts_with("https://pages.example/project/packages/"));
        assert!(!artifact.chunks.is_empty());
        for part in &artifact.chunks {
            meduza_np_updater::store::verify_file(&site.join(&part.url), part.size, &part.sha256).unwrap();
        }
        manifests.push(group);
    }
    assert_eq!(manifests[0].packages[0].artifacts, manifests[1].packages[0].artifacts);
    assert_eq!(manifests[1].packages[0].reused_from_group.as_deref(), Some("v1.0.0"));
    std::fs::remove_dir_all(root).unwrap();
}
