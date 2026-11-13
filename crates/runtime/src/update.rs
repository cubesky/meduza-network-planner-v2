//! Read-only updater progress shared by planner reports and the updater's local API.
use serde_json::{json, Value};
use std::path::Path;

/// Stable, per-store archive directory shared by native updater helpers and progress readers.
pub fn native_download_dir(store: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let root = std::fs::canonicalize(store)?;
    let hash = meduza_np_protocol::digest::sha256_hex(root.to_string_lossy().as_bytes());
    let base = if cfg!(target_os = "linux") { std::path::PathBuf::from("/tmp") } else { std::env::temp_dir() };
    Ok(base.join(format!("meduza-np-updater-{}", &hash[..16])))
}
fn read(path: &Path) -> Value {
    crate::fsutil::read_json(path, 4 * 1024 * 1024).ok().flatten().unwrap_or(Value::Null)
}

pub fn snapshot(store: &Path, run: &Path) -> Option<Value> {
    let installed = read(&store.join("installed.json"));
    let state = read(&store.join("state.json"));
    let journal = read(&store.join("journal.json"));
    let last = read(&store.join("journal.last.json"));
    if installed.is_null() && state.is_null() && journal.is_null() && last.is_null() {
        return None;
    }
    let active = !journal.is_null();
    let job = if active { &journal } else { &last };
    let activity = read(&store.join("activity.json"));
    let busy = activity.get("busy").and_then(Value::as_str);
    let control = read(&store.join("control.json"));
    let requested = read(&run.join("wanted-components.json"));
    let mut wanted: Vec<String> = requested
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|n| if n.starts_with("meduza-np-") { n.to_string() } else { format!("meduza-np-{n}") })
        .collect();
    wanted.extend(installed["selected"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string));
    wanted.sort();
    wanted.dedup();
    let pending: Vec<_> = wanted.iter().filter(|n| installed["packages"].get(n.as_str()).is_none()).cloned().collect();
    let rolled_back = matches!(state["lastResult"]["outcome"].as_str(), Some("rolledback" | "rolled-back"));
    let failed = rolled_back || state["lastResult"]["outcome"].as_str() == Some("failed");
    let error = if active || busy.is_some() {
        Value::Null
    } else if !state["lastCheckError"].is_null() {
        state["lastCheckError"].clone()
    } else if failed {
        state["lastResult"]["detail"].clone()
    } else {
        Value::Null
    };
    let mut phase = if active {
        journal["phase"].as_str().unwrap_or("installing")
    } else if busy.is_some() {
        "checking"
    } else if control["paused"].as_bool() == Some(true) {
        "paused"
    } else if !error.is_null() || failed {
        if rolled_back {
            "rolled-back"
        } else {
            "failed"
        }
    } else if !pending.is_empty() {
        "waiting"
    } else if !installed.is_null() {
        "done"
    } else {
        "idle"
    };
    let download = read(&store.join("download-progress.json"));
    // A retry of the same group must not inherit the old attempt's byte counters.
    let matching = !job["startedAt"].is_null() && download["startedAt"] == job["startedAt"];
    let progress = if matching { download } else { Value::Null };
    if active && progress["native"].as_bool() == Some(true) && matches!(phase, "locked" | "downloading") {
        phase = if progress["step"].as_str() == Some("installing") { "installing" } else { "downloading" };
    }
    if phase == "downloading" && progress["step"].as_str() == Some("installing") {
        phase = "installing";
    }
    if phase == "downloading" && progress["retryAtUnix"].as_u64().is_some() {
        phase = "retrying";
    }
    let current_total = progress["packageBytesTotal"].as_u64().unwrap_or(0);
    let current_bytes = if matches!(phase, "downloading" | "retrying") {
        progress["file"]
            .as_str()
            .filter(|s| !s.is_empty() && !s.contains(['/', '\\']) && !matches!(*s, "." | ".."))
            .and_then(|name| {
                let dir = if progress["native"].as_bool() == Some(true) { native_download_dir(store).ok()? } else { store.join("staging") };
                std::fs::metadata(dir.join(name)).ok()
            })
            .map(|m| m.len().min(current_total))
            .unwrap_or(0)
    } else {
        0
    };
    let total = progress["bytesTotal"].as_u64();
    let downloaded = progress["completedBytes"].as_u64().unwrap_or(0).saturating_add(current_bytes).min(total.unwrap_or(u64::MAX));
    Some(json!({
        "phase":phase, "active":active || busy.is_some(),
        "groupVersion":if active || failed { job.get("groupVersion") } else { installed.get("groupVersion") },
        "installedGroupVersion":installed.get("groupVersion"),
        "availableGroupVersion":state["available"].get("groupVersion"),
        "installedAt":installed.get("activatedAt"),
        "lastCheck":state.get("lastCheck"), "lastResult":state.get("lastResult"),
        "currentPackage":progress.get("currentPackage"),
        "packages":progress.get("packages").or_else(|| job.get("changed")),
        "pendingPackages":pending, "completedPackages":progress["completedPackages"].as_u64().unwrap_or(0),
        "totalPackages":progress.get("totalPackages"),
        "downloadedBytes":if total.is_some() { json!(downloaded) } else { Value::Null }, "bytesTotal":total,
        "packageDownloadedBytes":current_bytes, "packageBytesTotal":current_total,
        "error":error, "nextCheckUnix":state.get("nextCheckUnix"),
        "attempt":progress.get("attempt"), "maxAttempts":progress.get("maxAttempts"),
        "retryAtUnix":progress.get("retryAtUnix"), "retryError":progress.get("retryError"),
        "updatedAt":job.get("updatedAt").or_else(|| state.get("lastCheck")),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_bytes_retries_and_failures() {
        let root = std::env::temp_dir().join(format!("mnp-update-progress-{}", std::process::id()));
        let store = root.join("store");
        let run = root.join("run");
        std::fs::create_dir_all(store.join("staging")).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        let write = |name: &str, v: Value| crate::fsutil::write_json_atomic(&store.join(name), &v, 0o644).unwrap();
        assert!(snapshot(&store, &run).is_none());
        write("installed.json", json!({"packages":{},"selected":[]}));
        std::fs::write(run.join("wanted-components.json"), br#"["tinc","frr"]"#).unwrap();
        write("journal.json", json!({"phase":"downloading","startedAt":"a"}));
        write(
            "download-progress.json",
            json!({"startedAt":"a","currentPackage":"meduza-np-frr","file":"frr.tar.gz",
            "bytesTotal":1000,"completedBytes":200,"packageBytesTotal":800,"completedPackages":1,"totalPackages":2}),
        );
        std::fs::write(store.join("staging/frr.tar.gz"), [0u8; 300]).unwrap();
        let p = snapshot(&store, &run).unwrap();
        assert_eq!(p["downloadedBytes"], 500);
        assert_eq!(p["pendingPackages"], json!(["meduza-np-frr", "meduza-np-tinc"]));
        write(
            "download-progress.json",
            json!({"startedAt":"a","currentPackage":"meduza-np-frr","bytesTotal":1000,
            "completedBytes":200,"retryAtUnix":456,"retryError":"TLS EOF","attempt":1,"maxAttempts":4}),
        );
        let retry = snapshot(&store, &run).unwrap();
        assert_eq!(retry["phase"], "retrying");
        assert_eq!(retry["downloadedBytes"], 200);
        assert_eq!(retry["retryError"], "TLS EOF");
        assert_eq!(retry["maxAttempts"], 4);
        write(
            "download-progress.json",
            json!({"startedAt":"a","file":"frr.tar.gz","bytesTotal":1000,
            "completedBytes":200,"packageBytesTotal":800,"retryAtUnix":456}),
        );
        assert_eq!(snapshot(&store, &run).unwrap()["downloadedBytes"], 500, "verified assembled chunks remain visible during retry");
        write("download-progress.json", json!({"startedAt":"a","step":"installing","completedBytes":1000,"bytesTotal":1000}));
        let p = snapshot(&store, &run).unwrap();
        assert_eq!(p["phase"], "installing", "downloaded bytes reaching 100% is not installation completion");
        assert_eq!(p["downloadedBytes"], 1000);
        write("journal.json", json!({"phase":"downloading","startedAt":"b"}));
        assert!(snapshot(&store, &run).unwrap()["downloadedBytes"].is_null());
        std::fs::rename(store.join("journal.json"), store.join("journal.last.json")).unwrap();
        write("state.json", json!({"lastResult":{"outcome":"failed","detail":"TLS EOF"},"nextCheckUnix":123}));
        let p = snapshot(&store, &run).unwrap();
        assert_eq!(p["phase"], "failed");
        assert_eq!(p["error"], "TLS EOF");
        assert_eq!(p["nextCheckUnix"], 123);
        write("state.json", json!({"lastResult":{"outcome":"rolledback","detail":"previous version restored"}}));
        let p = snapshot(&store, &run).unwrap();
        assert_eq!(p["phase"], "rolled-back");
        assert_eq!(p["lastResult"]["outcome"], "rolledback");
        write("activity.json", json!({"busy":"checking"}));
        let p = snapshot(&store, &run).unwrap();
        assert_eq!(p["phase"], "checking");
        assert!(p["error"].is_null());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_progress_reads_tmp_bytes_and_keeps_installation_distinct() {
        let root = std::env::temp_dir().join(format!("mnp-native-progress-{}", std::process::id()));
        let store = root.join("store");
        let run = root.join("run");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        let tmp = native_download_dir(&store).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();
        let file = "meduza-np-planner_1.0.0.ipk";
        std::fs::write(tmp.join(file), [0u8; 300]).unwrap();
        crate::fsutil::write_json_atomic(&store.join("journal.json"), &json!({"phase":"locked","startedAt":"native"}), 0o600).unwrap();
        let mut progress = json!({"startedAt":"native","native":true,"step":"downloading","file":file,"packageBytesTotal":800,"completedBytes":200,"bytesTotal":1000});
        let write = |v: &Value| crate::fsutil::write_json_atomic(&store.join("download-progress.json"), v, 0o600).unwrap();
        write(&progress);
        let p = snapshot(&store, &run).unwrap();
        assert_eq!(p["phase"], "downloading");
        assert_eq!(p["downloadedBytes"], 500);
        progress["retryAtUnix"] = json!(123);
        write(&progress);
        assert_eq!(snapshot(&store, &run).unwrap()["phase"], "retrying");
        progress["retryAtUnix"] = Value::Null;
        progress["step"] = json!("installing");
        write(&progress);
        assert_eq!(snapshot(&store, &run).unwrap()["phase"], "installing");
        progress["step"] = json!("downloading");
        progress["file"] = json!("../escape.ipk");
        write(&progress);
        assert_eq!(snapshot(&store, &run).unwrap()["packageDownloadedBytes"], 0);
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn completed_upgrade_retains_versions_bytes_and_result() {
        let root = std::env::temp_dir().join(format!("mnp-update-complete-{}", std::process::id()));
        let store = root.join("store");
        let run = root.join("run");
        std::fs::create_dir_all(&store).unwrap();
        let write = |name: &str, v: Value| crate::fsutil::write_json_atomic(&store.join(name), &v, 0o644).unwrap();
        write("installed.json", json!({"groupVersion":"v0.1.5","activatedAt":"2026-11-03T06:57:17Z","packages":{}}));
        write("journal.last.json", json!({"phase":"done","groupVersion":"v0.1.5","startedAt":"start","updatedAt":"2026-11-03T06:57:20Z"}));
        write(
            "download-progress.json",
            json!({"startedAt":"start","completedBytes":14514061,"bytesTotal":14514061,"completedPackages":14,"totalPackages":14}),
        );
        write(
            "state.json",
            json!({"available":{"groupVersion":"v0.1.6"},"lastCheck":"2026-11-03T09:02:47Z",
            "lastResult":{"groupVersion":"v0.1.5","outcome":"done","at":"2026-11-03T06:57:20Z"}}),
        );
        let progress = snapshot(&store, &run).unwrap();
        assert_eq!(progress["active"], false);
        assert_eq!(progress["phase"], "done");
        assert_eq!(progress["installedGroupVersion"], "v0.1.5");
        assert_eq!(progress["availableGroupVersion"], "v0.1.6");
        assert_eq!(progress["downloadedBytes"], 14514061);
        assert_eq!(progress["completedPackages"], 14);
        assert_eq!(progress["lastCheck"], "2026-11-03T09:02:47Z");
        assert_eq!(progress["lastResult"]["outcome"], "done");
        std::fs::remove_dir_all(root).unwrap();
    }
}
