//! Client side of the updater ↔ planner maintenance protocol (§13.1) over `planner.sock`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub trait PlannerApi {
    /// Calls a planner local method. `Err` carries `code: message`; `unavailable` when planner is not running.
    fn call(&self, method: &str, params: Value) -> Result<Value, String>;
}

pub struct UdsPlanner {
    pub socket: PathBuf,
    pub timeout: Duration,
}

#[cfg(unix)]
impl PlannerApi for UdsPlanner {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        let stream = UnixStream::connect(&self.socket).map_err(|e| format!("unavailable: {e}"))?;
        stream.set_read_timeout(Some(self.timeout)).ok();
        stream.set_write_timeout(Some(self.timeout)).ok();
        let req = meduza_np_protocol::local::LocalRequest {
            protocol: meduza_np_protocol::local::LOCAL_PROTOCOL_VERSION,
            id: 1,
            method: method.into(),
            params,
        };
        let mut line = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
        line.push(b'\n');
        (&stream).write_all(&line).map_err(|e| format!("unavailable: {e}"))?;
        let mut buf = String::new();
        BufReader::new(&stream)
            .take(meduza_np_protocol::local::MAX_FRAME as u64 * 4)
            .read_line(&mut buf)
            .map_err(|e| format!("unavailable: {e}"))?;
        let resp: meduza_np_protocol::local::LocalResponse =
            serde_json::from_str(&buf).map_err(|e| format!("unavailable: bad response: {e}"))?;
        match (resp.ok, resp.error) {
            (true, _) => Ok(resp.result),
            (false, Some(e)) => Err(format!("{}: {}", e.code, e.message)),
            (false, None) => Err("internal: error without detail".into()),
        }
    }
}

#[cfg(not(unix))]
impl PlannerApi for UdsPlanner {
    fn call(&self, _method: &str, _params: Value) -> Result<Value, String> {
        Err("unavailable: no unix sockets on this platform".into())
    }
}

#[cfg(unix)]
use std::io::Read as _;

pub fn boot_id(p: &dyn PlannerApi) -> Option<String> {
    p.call("status", Value::Null).ok()?.get("bootId")?.as_str().map(str::to_string)
}

/// Waits until planner answers with a boot id different from `old` (a restart happened) or `old` is `None` and it
/// answers at all.
pub fn wait_for_planner(p: &dyn PlannerApi, old: Option<&str>, timeout: Duration, poll: Duration) -> Result<String, String> {
    let start = Instant::now();
    loop {
        match boot_id(p) {
            Some(b) if old != Some(b.as_str()) => return Ok(b),
            _ if start.elapsed() >= timeout => {
                return Err(format!("planner did not come back within {}s", timeout.as_secs()));
            }
            _ => std::thread::sleep(poll),
        }
    }
}

/// Applied instance ids owned by `package` (from planner's applied state).
pub fn instances_of(p: &dyn PlannerApi, package: &str) -> Vec<String> {
    let Ok(v) = p.call("instances", Value::Null) else { return vec![] };
    let mut out: Vec<String> = v
        .get("applied")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, a)| a.get("package").and_then(Value::as_str) == Some(package))
        .map(|(id, _)| id.clone())
        .collect();
    out.sort();
    out
}

/// Checks that every `(package, version)` is registered by planner at that version.
pub fn verify_packages(p: &dyn PlannerApi, expected: &[(String, String)]) -> Result<(), String> {
    let v = p.call("packages", Value::Null)?;
    // planner answers `{"packages": [PackageStatus...]}`
    let list = v.get("packages").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut problems = Vec::new();
    for (name, version) in expected {
        match list.iter().find(|s| s.get("name").and_then(Value::as_str) == Some(name)) {
            None => problems.push(format!("{name} not found by planner")),
            Some(s) => {
                let got = s.get("version").and_then(Value::as_str).unwrap_or_default();
                let reg = s.get("registration").and_then(Value::as_str).unwrap_or_default();
                if got != version {
                    problems.push(format!("{name} reports {got}, expected {version}"));
                } else if reg != "registered" {
                    let why = s.get("reason").and_then(Value::as_str).unwrap_or_default();
                    problems.push(format!("{name} {version} is {reg}: {why}"));
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

pub fn lock_params(reason: &str) -> Value {
    json!({"reason": reason, "durationSeconds": 900, "pauseApply": true})
}
