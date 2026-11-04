//! Planner's half of mutual upgrades. Only a journaled updater activation is allowed: no caller-supplied program,
//! package name, command or path. The updater daemon keeps its independent service/container lifecycle.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use meduza_np_protocol::api::Job;
use meduza_np_runtime::{fsutil, paths::NodePaths};
use serde::{Deserialize, Serialize};
use serde_json::Value;

type UpdateResult = Result<Value, (String, String)>;

#[derive(Serialize, Deserialize)]
struct Receipt {
    job_id: String,
    result: UpdateResult,
}

/// Acknowledges scheduling, not installation completion. Cache the result before reporting it to Manager so a
/// planner replacement or lost HTTP acknowledgement does not normally repeat an already accepted upgrade.
pub async fn request(paths: &NodePaths, job: &Job) -> UpdateResult {
    let method = match job.kind.as_str() {
        "update-check" => "update.check",
        "update-apply" => "update.apply",
        _ => return Err(("invalid-params".into(), "unsupported update request".into())),
    };
    if job.package.is_some() || job.instance.is_some() || !(job.params.is_null() || job.params.as_object().is_some_and(|p| p.is_empty())) {
        return Err(("invalid-params".into(), "update requests accept no package, instance or parameters".into()));
    }
    let file = paths.state.join("jobs/updater-receipts.json");
    let mut receipts: Vec<Receipt> = fsutil::read_json(&file, 1024 * 1024)
        .map_err(|e| ("internal".into(), format!("reading updater request receipts: {e}")))?
        .unwrap_or_default();
    if let Some(r) = receipts.iter().find(|r| r.job_id == job.job_id) {
        return r.result.clone();
    }
    let result = call_update(&paths.updater_socket(), method, Duration::from_secs(10)).await;
    receipts.push(Receipt { job_id: job.job_id.clone(), result: result.clone() });
    if receipts.len() > 32 {
        receipts.remove(0);
    }
    if let Err(e) = fsutil::write_json_atomic(&file, &receipts, 0o600) {
        // A successful scheduling response remains true even if observation persistence fails.
        warn!("cannot record updater request {}: {e}", job.job_id);
    }
    result
}

#[cfg(unix)]
async fn call_update(socket: &Path, method: &str, timeout: Duration) -> UpdateResult {
    use meduza_np_protocol::local::{LocalRequest, LocalResponse, LOCAL_PROTOCOL_VERSION};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let unavailable = |e: std::io::Error| ("updater-unavailable".into(), e.to_string());
    let work = async {
        let mut stream = tokio::net::UnixStream::connect(socket).await.map_err(unavailable)?;
        let req = LocalRequest { protocol: LOCAL_PROTOCOL_VERSION, id: 1, method: method.into(), params: serde_json::json!({}) };
        let mut line = serde_json::to_vec(&req).map_err(|e| ("internal".into(), e.to_string()))?;
        line.push(b'\n');
        stream.write_all(&line).await.map_err(unavailable)?;
        let mut bytes = Vec::new();
        BufReader::new(stream.take(16 * 1024 + 1)).read_until(b'\n', &mut bytes).await.map_err(unavailable)?;
        if bytes.len() > 16 * 1024 || !bytes.ends_with(b"\n") {
            return Err(("updater-unavailable".into(), "invalid updater response frame".into()));
        }
        let response: LocalResponse =
            serde_json::from_slice(&bytes).map_err(|e| ("updater-unavailable".into(), format!("invalid updater response: {e}")))?;
        if response.id != 1 {
            return Err(("updater-unavailable".into(), "updater response ID mismatch".into()));
        }
        match (response.ok, response.error) {
            (true, _) if response.result["scheduled"] == true => Ok(response.result),
            (false, Some(e)) => Err((e.code, e.message)),
            _ => Err(("updater-unavailable".into(), "updater did not confirm scheduling".into())),
        }
    };
    tokio::time::timeout(timeout, work).await.unwrap_or_else(|_| Err(("updater-unavailable".into(), "updater request timed out".into())))
}

#[cfg(not(unix))]
async fn call_update(_socket: &Path, _method: &str, _timeout: Duration) -> UpdateResult {
    Err(("updater-unavailable".into(), "updater control requires Unix sockets".into()))
}

fn segment(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', ':'])
}

fn helper(store: &Path, journal: &Value) -> Result<PathBuf, String> {
    if journal["phase"] != "self-restart" || !journal["handoff"].is_object() {
        return Err("no staged updater handoff".into());
    }
    match journal["newInstalled"]["backend"].as_str() {
        Some("store") => {
            let object = journal["newInstalled"]["packages"]["meduza-np-updater"]["object"].as_str().ok_or("no staged updater object")?;
            if !segment(object) {
                return Err("invalid staged updater object".into());
            }
            let root = store.join("objects/meduza-np-updater").canonicalize().map_err(|e| e.to_string())?;
            let exe = root.join(object).join("usr/bin/meduza-np-updater").canonicalize().map_err(|e| e.to_string())?;
            if !exe.starts_with(root.join(object)) {
                return Err("staged updater helper escapes its object".into());
            }
            Ok(exe)
        }
        Some("opkg" | "apk") => Ok(PathBuf::from("/usr/bin/meduza-np-updater")),
        _ => Err("unsupported updater backend".into()),
    }
}

pub async fn activate(paths: &NodePaths) -> Result<(), String> {
    let file: Value = fsutil::read_json(&paths.etc.join("updater.json"), 64 * 1024).map_err(|e| e.to_string())?.unwrap_or(Value::Null);
    let data = std::env::var_os("MEDUZA_NP_DATA_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| file["dataDir"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| paths.state.clone());
    let store = data.join("store");
    let journal: Value =
        fsutil::read_json(&store.join("journal.json"), 4 * 1024 * 1024).map_err(|e| e.to_string())?.ok_or("no update journal")?;
    let exe = helper(&store, &journal)?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("activate-updater")
        .env("MEDUZA_NP_DATA_DIR", &data)
        .env("MEDUZA_NP_ETC_DIR", &paths.etc)
        .env("MEDUZA_NP_STATE_DIR", &paths.state)
        .env("MEDUZA_NP_RUN_DIR", &paths.run)
        .env("MEDUZA_NP_UPDATE_BACKEND", journal["newInstalled"]["backend"].as_str().unwrap_or_default())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| format!("updater activation helper: {e}"))?;
    let mut stderr = child.stderr.take().expect("piped helper stderr");
    let mut capture = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut message = Vec::new();
        let mut buffer = [0u8; 1024];
        while let Ok(n) = stderr.read(&mut buffer).await {
            if n == 0 {
                break;
            }
            let keep = n.min(4096usize.saturating_sub(message.len()));
            message.extend_from_slice(&buffer[..keep]);
        }
        String::from_utf8_lossy(&message).trim().to_string()
    });
    let status = match tokio::time::timeout(Duration::from_secs(120), child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            capture.abort();
            return Err(format!("updater activation helper: {e}"));
        }
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                // SAFETY: this child was placed in a new process group with its own PID as PGID. Kill its
                // installer descendants too, so timing out cannot leave an unobserved package switch running.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            capture.abort();
            return Err("updater activation helper timed out".into());
        }
    };
    let detail = match tokio::time::timeout(Duration::from_secs(1), &mut capture).await {
        Ok(Ok(message)) => message,
        _ => {
            capture.abort();
            String::new()
        }
    };
    if status.success() {
        Ok(())
    } else {
        Err(format!("updater activation helper failed: {status}{}", if detail.is_empty() { String::new() } else { format!(": {detail}") }))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn manager_requests_relay_to_updater_and_replayed_jobs_use_durable_receipts() {
        use meduza_np_protocol::local::{LocalRequest, LocalResponse};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let temp = std::env::temp_dir().join(format!("mnp-planner-update-relay-{}", std::process::id()));
        let paths = NodePaths::under(&temp);
        std::fs::create_dir_all(&paths.run).unwrap();
        let listener = tokio::net::UnixListener::bind(paths.updater_socket()).unwrap();
        let worker = tokio::spawn(async move {
            for method in ["update.check", "update.apply"] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let req: LocalRequest = serde_json::from_str(&line).unwrap();
                assert_eq!(req.method, method);
                assert_eq!(req.params, json!({}));
                let response = if method == "update.check" {
                    LocalResponse::ok(req.id, json!({"scheduled":true}))
                } else {
                    LocalResponse::err(req.id, "busy", "updater is already applying")
                };
                let mut line = serde_json::to_vec(&response).unwrap();
                line.push(b'\n');
                stream.get_mut().write_all(&line).await.unwrap();
            }
        });
        let mut job = Job {
            job_id: "job-check".into(),
            kind: "update-check".into(),
            package: None,
            instance: None,
            params: json!({}),
            expires_at: "2099-01-01T00:00:00Z".into(),
        };
        assert_eq!(request(&paths, &job).await.unwrap(), json!({"scheduled":true}));
        assert_eq!(request(&paths, &job).await.unwrap(), json!({"scheduled":true}), "replay cannot schedule the operation twice");
        job.job_id = "job-apply".into();
        job.kind = "update-apply".into();
        assert_eq!(request(&paths, &job).await.unwrap_err().0, "busy");
        worker.await.unwrap();
        std::fs::remove_file(paths.updater_socket()).unwrap();
        assert_eq!(request(&paths, &job).await.unwrap_err().0, "busy", "acknowledgement replays survive the original socket/process");
        job.job_id = "job-new".into();
        assert_eq!(request(&paths, &job).await.unwrap_err().0, "updater-unavailable");
        job.params = json!({"command":"untrusted"});
        assert_eq!(request(&paths, &job).await.unwrap_err().0, "invalid-params");
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[tokio::test]
    async fn only_staged_helper_can_run_and_gets_the_node_paths() {
        use std::os::unix::fs::PermissionsExt;
        let temp = std::env::temp_dir().join(format!("mnp-planner-updater-helper-{}", std::process::id()));
        let paths = NodePaths::under(&temp);
        let store = paths.state.join("store");
        let exe = store.join("objects/meduza-np-updater/v2-test/usr/bin/meduza-np-updater");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"#!/bin/sh\nset -eu\n[ \"$1\" = activate-updater ]\n[ \"$MEDUZA_NP_UPDATE_BACKEND\" = store ]\nprintf activated > \"$MEDUZA_NP_STATE_DIR/helper-called\"\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut journal = json!({"phase":"self-restart", "handoff":{}, "newInstalled":{"backend":"store", "packages":{"meduza-np-updater":{"object":"v2-test"}}}});
        fsutil::write_json_atomic(&store.join("journal.json"), &journal, 0o600).unwrap();
        activate(&paths).await.unwrap();
        assert_eq!(std::fs::read(paths.state.join("helper-called")).unwrap(), b"activated");
        std::fs::remove_file(paths.state.join("helper-called")).unwrap();
        std::fs::write(&exe, b"#!/bin/sh\necho 'store/current.new: Read-only file system' >&2\nexit 1\n").unwrap();
        let error = activate(&paths).await.unwrap_err();
        assert!(
            error.contains("Read-only file system") && error.contains("exit status: 1"),
            "the helper diagnostic must survive the RPC: {error}"
        );
        journal["newInstalled"]["packages"]["meduza-np-updater"]["object"] = json!("../../etc");
        fsutil::write_json_atomic(&store.join("journal.json"), &journal, 0o600).unwrap();
        assert!(activate(&paths).await.unwrap_err().contains("invalid staged updater"));
        journal["phase"] = json!("done");
        fsutil::write_json_atomic(&store.join("journal.json"), &journal, 0o600).unwrap();
        assert!(activate(&paths).await.unwrap_err().contains("no staged updater"));
        assert!(!paths.state.join("helper-called").exists());
        std::fs::remove_dir_all(temp).unwrap();
    }
}
