use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flate2::read::GzDecoder;
use meduza_np_updater::sig;

fn command(args: &[&str], secret: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_meduza-np-release"));
    command.args(args).current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    if let Some(secret) = secret {
        command.env("TEST_RELEASE_KEY", secret);
    }
    command.output().unwrap()
}

fn trust_from_ipk(path: &Path) -> Vec<u8> {
    let bytes = std::fs::read(path).unwrap();
    let mut outer = tar::Archive::new(GzDecoder::new(bytes.as_slice()));
    for entry in outer.entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.path().unwrap().to_string_lossy().trim_start_matches("./") != "data.tar.gz" {
            continue;
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        let mut inner = tar::Archive::new(GzDecoder::new(data.as_slice()));
        for file in inner.entries().unwrap() {
            let mut file = file.unwrap();
            if file.path().unwrap().to_string_lossy().trim_start_matches("./") == "usr/share/meduza-np/trust/release-keys.json" {
                let mut trust = Vec::new();
                file.read_to_end(&mut trust).unwrap();
                return trust;
            }
        }
    }
    panic!("{} does not contain the updater trust store", path.display());
}

#[test]
fn prepared_public_trust_is_shipped_in_every_armv7_updater_package() {
    let root = std::env::temp_dir().join(format!("mnp-updater-packaging-{}", std::process::id()));
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("bin/meduza-np-updater"), b"test executable").unwrap();
    let public = root.join("trust/release-keys.json");
    let directory = root.join("dist");
    let bin = root.join("bin");
    let args = [
        "openwrt",
        "--package",
        "meduza-np-updater",
        "--version",
        "0.0.6",
        "--target",
        "armv7-unknown-linux-musleabihf",
        "--bin-dir",
        bin.to_str().unwrap(),
        "--trust",
        public.to_str().unwrap(),
        "--out",
        directory.to_str().unwrap(),
    ];
    let missing = command(&args, None);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("trust store"));
    let secret = sig::encode_secret(&sig::generate_key().unwrap());
    let prepared = command(&["prepare-trust", "--key-env", "TEST_RELEASE_KEY", "--out", public.to_str().unwrap()], Some(&secret));
    assert!(prepared.status.success(), "{}", String::from_utf8_lossy(&prepared.stderr));
    assert!(!String::from_utf8_lossy(&prepared.stdout).contains(&secret));
    let expected = std::fs::read(&public).unwrap();
    let packaged = command(&args, None);
    assert!(packaged.status.success(), "{}", String::from_utf8_lossy(&packaged.stderr));
    let entries: Vec<PathBuf> = std::fs::read_dir(&directory).unwrap().map(|entry| entry.unwrap().path()).collect();
    let ipks: Vec<_> = entries.iter().filter(|path| path.extension().is_some_and(|extension| extension == "ipk")).collect();
    let stages: Vec<_> = entries.iter().filter(|path| path.to_string_lossy().ends_with(".apk.stage")).collect();
    assert_eq!(ipks.len(), 3);
    assert_eq!(stages.len(), 3);
    for ipk in ipks {
        assert_eq!(trust_from_ipk(ipk), expected);
    }
    for stage in stages {
        assert_eq!(std::fs::read(stage.join("root/usr/share/meduza-np/trust/release-keys.json")).unwrap(), expected);
    }
    std::fs::remove_dir_all(root).unwrap();
}
