//! Exercise the real release CLI: production jobs must receive exactly the changed package subset.
use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

fn cli(args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_meduza-np-release"))
        .args(args)
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn write(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

#[test]
fn first_release_single_package_change_and_unchanged_group() {
    let directory = std::env::temp_dir().join(format!("mnp-release-planning-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let fingerprints = directory.join("fingerprints.json");
    let baseline = directory.join("baseline.json");
    let matrix = directory.join("matrix.json");
    let fps = json!({"meduza-np-wireguard": "wg-old", "meduza-np-frr": "frr-old"});
    write(&fingerprints, &fps);
    let first = cli(&["matrix", "--fingerprints", fingerprints.to_str().unwrap(), "--group-version", "v1.0.0"]);
    assert!(first.as_object().unwrap().values().all(|entry| entry["changed"] == true && entry["version"] == "1.0.0"));
    write(
        &baseline,
        &json!({
            "formatVersion": 1, "groupVersion": "v1.0.0", "sequence": 1, "sourceCommit": "old", "publishedAt": "", "expiresAt": "",
            "platforms": [], "compatibility": {"plannerApi": {"min": 1, "max": 1}, "managerApi": {"min": 1, "max": 1}, "configSchema": {"min": 1, "max": 1}},
            "packages": [
                {"name": "meduza-np-wireguard", "version": "1.0.0", "fingerprint": "wg-old", "artifacts": []},
                {"name": "meduza-np-frr", "version": "1.0.0", "fingerprint": "frr-old", "artifacts": []}
            ]
        }),
    );
    let plan = || {
        cli(&[
            "matrix",
            "--fingerprints",
            fingerprints.to_str().unwrap(),
            "--group-version",
            "v2.0.0",
            "--baseline",
            baseline.to_str().unwrap(),
        ])
    };
    write(&matrix, &plan());
    let jobs = || cli(&["classify", "--matrix", matrix.to_str().unwrap()]);
    assert!(jobs().as_object().unwrap().values().all(|packages| packages.as_array().unwrap().is_empty()));

    write(&fingerprints, &json!({"meduza-np-wireguard": "wg-old", "meduza-np-frr": "frr-new"}));
    let changed = plan();
    assert_eq!(changed["meduza-np-frr"]["version"], "2.0.0");
    assert_eq!(changed["meduza-np-wireguard"]["version"], "1.0.0");
    assert_eq!(changed["meduza-np-wireguard"]["reusedFromGroup"], "v1.0.0");
    write(&matrix, &changed);
    let classified = jobs();
    assert_eq!(classified["rust"], json!(["meduza-np-frr"]));
    assert_eq!(classified["linux"], json!(["meduza-np-frr"]));
    assert_eq!(classified["providers"], json!([]));
    assert_eq!(classified["images"], json!([]));
    std::fs::remove_dir_all(directory).unwrap();
}
