//! Plugin side of the planner ↔ plugin protocol (AGENT.md §5.1, docs/DECISIONS.md D2).
//!
//! planner runs `<entrypoint> <operation>`, writes one [`PluginRequest`] to stdin and reads exactly one
//! [`PluginResponse`] from stdout. Plugins implement [`Plugin`] and call [`run`] from `main`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use meduza_np_protocol::config::{NodeConfig, SecretValue};
use meduza_np_protocol::plan::GenerationPlan;
use meduza_np_protocol::report::InstanceStatus;
use meduza_np_protocol::{ValidationError, PLUGIN_PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod dns;
pub mod fetch;
pub mod localhttp;
pub mod text;

/// Largest request planner sends; bigger stdin is rejected.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginRequest {
    pub protocol_version: u32,
    pub request_id: String,
    pub operation: String,
    /// Full node snapshot; plugins only read the paths their manifest declares in `consumes`.
    pub config: NodeConfig,
    /// Restrict the operation to one instance (`probe`, actions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Resolved provider per slot the package uses.
    #[serde(default)]
    pub providers: BTreeMap<String, ResolvedProvider>,
    pub context: PluginContext,
    /// Operation specific parameters (action id + input, probe hints).
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedProvider {
    pub path: String,
    /// `system`, `meduza-np-openvpn-pkg`, `github-release`, `user-provided`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginContext {
    /// Absolute path of the generation directory files will be staged into (`/run/meduza-np/generations/<rev>`).
    pub generation_root: String,
    pub state_dir: String,
    pub run_dir: String,
    pub libexec_dir: String,
    #[serde(default)]
    pub platform: Platform,
    /// Installed packages and the capabilities they registered, for optional cross-package bindings.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Platform {
    #[serde(default)]
    pub openwrt: bool,
    #[serde(default)]
    pub container: bool,
    #[serde(default)]
    pub arch: String,
    /// Detected native software versions (`frr` → `9.1`), for syntax adaptation.
    #[serde(default)]
    pub versions: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginResponse {
    pub protocol_version: u32,
    pub request_id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "camelCase")]
#[error("{code}: {message}")]
pub struct PluginError {
    /// `invalid-config`, `missing-secret`, `unsupported`, `provider-missing`, `probe-failed`, `internal`, ...
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<ValidationError>,
}

impl PluginError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), details: Vec::new() }
    }

    pub fn invalid(details: Vec<ValidationError>) -> Self {
        Self { code: "invalid-config".into(), message: format!("{} validation error(s)", details.len()), details }
    }
}

impl From<Vec<ValidationError>> for PluginError {
    fn from(details: Vec<ValidationError>) -> Self {
        Self::invalid(details)
    }
}

/// `validate` result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidateResult {
    pub errors: Vec<ValidationError>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// `plan` result: one plan per instance, plus removed instances implied by absence.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanResult {
    pub plans: Vec<GenerationPlan>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// `probe` result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    pub instances: Vec<InstanceStatus>,
}

/// Implemented by every service plugin. Default methods report `unsupported`.
pub trait Plugin {
    fn validate(&self, req: &PluginRequest) -> Result<ValidateResult, PluginError>;
    fn plan(&self, req: &PluginRequest) -> Result<PlanResult, PluginError>;
    fn probe(&self, _req: &PluginRequest) -> Result<ProbeResult, PluginError> {
        Err(PluginError::new("unsupported", "probe not implemented"))
    }
    /// Package actions declared in the UI descriptor (`generate-keypair`, ...). `params.action` names it.
    fn action(&self, _req: &PluginRequest) -> Result<Value, PluginError> {
        Err(PluginError::new("unsupported", "no actions"))
    }
}

/// Handles one request already read from stdin. Separated from [`run`] for tests.
pub fn handle(plugin: &dyn Plugin, operation: &str, input: &[u8]) -> PluginResponse {
    let req: PluginRequest = match serde_json::from_slice(input) {
        Ok(r) => r,
        Err(e) => return failure("", PluginError::new("invalid-request", format!("cannot parse request: {e}"))),
    };
    if req.protocol_version != PLUGIN_PROTOCOL_VERSION {
        return failure(&req.request_id, PluginError::new("incompatible", format!("protocol {} not supported", req.protocol_version)));
    }
    if req.operation != operation {
        return failure(&req.request_id, PluginError::new("invalid-request", "operation mismatch between argv and request"));
    }
    let result = match operation {
        "validate" => plugin.validate(&req).and_then(to_value),
        "plan" => plugin.plan(&req).and_then(|r| {
            let mut problems = Vec::new();
            for plan in &r.plans {
                problems.extend(plan.validate().errors().iter().cloned());
            }
            if problems.is_empty() {
                to_value(r)
            } else {
                Err(PluginError { code: "invalid-plan".into(), message: "plugin produced an invalid plan".into(), details: problems })
            }
        }),
        "probe" => plugin.probe(&req).and_then(to_value),
        "action" => plugin.action(&req),
        other => Err(PluginError::new("unsupported", format!("unknown operation {other}"))),
    };
    match result {
        Ok(result) => {
            PluginResponse { protocol_version: PLUGIN_PROTOCOL_VERSION, request_id: req.request_id, ok: true, result, error: None }
        }
        Err(e) => failure(&req.request_id, e),
    }
}

fn to_value<T: Serialize>(v: T) -> Result<Value, PluginError> {
    serde_json::to_value(v).map_err(|e| PluginError::new("internal", e.to_string()))
}

fn failure(request_id: &str, error: PluginError) -> PluginResponse {
    PluginResponse {
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        request_id: request_id.into(),
        ok: false,
        result: Value::Null,
        error: Some(error),
    }
}

/// Entry point for plugin binaries:
/// `fn main() { meduza_np_plugin_sdk::run(&MyPlugin, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")) }`.
pub fn run(plugin: &dyn Plugin, name: &str, version: &str) -> ! {
    let operation = std::env::args().nth(1).unwrap_or_default();
    if operation == "--version" {
        println!("{name} {version} (plugin protocol {PLUGIN_PROTOCOL_VERSION})");
        std::process::exit(0);
    }
    let mut input = Vec::new();
    let read = std::io::stdin().take(MAX_REQUEST_BYTES as u64 + 1).read_to_end(&mut input);
    let response = match read {
        Err(e) => failure("", PluginError::new("invalid-request", format!("stdin: {e}"))),
        Ok(_) if input.len() > MAX_REQUEST_BYTES => failure("", PluginError::new("invalid-request", "request too large")),
        Ok(_) => handle(plugin, &operation, &input),
    };
    let mut out = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, &response);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
    std::process::exit(if response.ok { 0 } else { 1 })
}

impl PluginRequest {
    /// The package's own `services.<name>` entry, deserialized into its typed model. `None` if absent.
    pub fn service<T: serde::de::DeserializeOwned>(&self, name: &str) -> Result<Option<T>, PluginError> {
        match self.config.services.get(name) {
            None => Ok(None),
            Some(v) => serde_json::from_value(v.clone())
                .map(Some)
                .map_err(|e| PluginError::invalid(vec![ValidationError::new(format!("/services/{name}"), e.to_string())])),
        }
    }

    pub fn secret(&self, id: &str) -> Result<&SecretValue, PluginError> {
        self.config.secrets.get(id).ok_or_else(|| PluginError::new("missing-secret", format!("secret {id:?} not delivered to this node")))
    }

    pub fn provider(&self, slot: &str) -> Result<&ResolvedProvider, PluginError> {
        self.providers.get(slot).ok_or_else(|| PluginError::new("provider-missing", format!("no provider for {slot}")))
    }

    pub fn has_capability(&self, cap: &str) -> bool {
        self.context.capabilities.iter().any(|c| c == cap)
    }

    /// Absolute path of a generated file inside the generation root.
    pub fn gen_path(&self, relative: &str) -> String {
        format!("{}/{}", self.context.generation_root.trim_end_matches('/'), relative)
    }
}

/// Output of a bounded read-only command run during `probe`.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Runs a read-only status command (e.g. `wg show <dev> dump`) with a timeout and output cap.
/// Never used to change system state: changes go through plans applied by planner.
pub fn run_status_command(program: &str, args: &[&str], timeout: Duration, max_output: usize) -> std::io::Result<CommandOutput> {
    run_plan_command(program, args, timeout, max_output)
}

/// Bounded native command for a runner explicitly launched and owned by a generation plan.
/// Mutating commands must never be called from plugin validate/plan/probe operations.
pub fn run_plan_command(program: &str, args: &[&str], timeout: Duration, max_output: usize) -> std::io::Result<CommandOutput> {
    let mut child = Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stdout).take(max_output as u64).read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status.code();
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait().ok().and_then(|s| s.code());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = String::from_utf8_lossy(&out_reader.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
    Ok(CommandOutput { status, stdout, stderr, timed_out })
}

/// Linux capability bits (`include/uapi/linux/capability.h`) that plugins check before starting a daemon.
pub const CAP_SYS_CHROOT: u32 = 18;
pub const CAP_SYS_ADMIN: u32 = 21;

/// Whether this process (a plugin inherits planner's capabilities) has capability `bit` in its effective set.
/// `None` when `/proc/self/status` is unreadable (not Linux).
pub fn has_effective_capability(bit: u32) -> Option<bool> {
    capability_in_status(&std::fs::read_to_string("/proc/self/status").ok()?, bit)
}

/// `CapEff:` of a `/proc/<pid>/status` text contains capability `bit`.
pub fn capability_in_status(status: &str, bit: u32) -> Option<bool> {
    let eff = status.lines().find_map(|l| l.strip_prefix("CapEff:"))?;
    let bits = u64::from_str_radix(eff.trim(), 16).ok()?;
    Some(bit < 64 && bits & (1 << bit) != 0)
}

/// Current time as RFC 3339 UTC (second precision), without pulling a date library into every plugin.
pub fn now_rfc3339() -> String {
    unix_to_rfc3339(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
}

/// Formats seconds since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn unix_to_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.fff](Z|+00:00)` (UTC only) into seconds since the Unix epoch.
pub fn rfc3339_to_unix(text: &str) -> Option<u64> {
    let t = text.trim();
    let (date, rest) = t.split_once('T')?;
    let time = rest.trim_end_matches('Z').trim_end_matches("+00:00");
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut hms = time.split(':');
    let (h, mi) = (hms.next()?.parse::<i64>().ok()?, hms.next()?.parse::<i64>().ok()?);
    let s = hms.next()?.split('.').next()?.parse::<i64>().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    // Days-from-civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + s).ok()
}

/// Test helper: a request with the given services JSON and default context.
pub fn test_request(operation: &str, config: Value) -> PluginRequest {
    let mut cfg = serde_json::json!({
        "schemaVersion": 1,
        "revision": "rev-000001",
        "node": {"id": "gw-a"},
    });
    if let (Some(base), Some(extra)) = (cfg.as_object_mut(), config.as_object()) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    PluginRequest {
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        request_id: "req-test".into(),
        operation: operation.into(),
        config: serde_json::from_value(cfg).expect("test config parses"),
        instance: None,
        providers: BTreeMap::new(),
        context: PluginContext {
            generation_root: "/run/meduza-np/generations/rev-000001".into(),
            state_dir: "/var/lib/meduza-np".into(),
            run_dir: "/run/meduza-np".into(),
            libexec_dir: "/usr/libexec/meduza-np".into(),
            platform: Platform::default(),
            capabilities: Vec::new(),
        },
        params: Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_bits_come_from_cap_eff() {
        // NET_BIND_SERVICE, NET_ADMIN, NET_RAW, SYS_ADMIN
        let status = "Name:\tplanner\nCapPrm:\t000001ffffffffff\nCapEff:\t0000000000203400\n";
        assert_eq!(capability_in_status(status, CAP_SYS_ADMIN), Some(true));
        assert_eq!(capability_in_status(status, CAP_SYS_CHROOT), Some(false));
        assert_eq!(capability_in_status(status, 64), Some(false));
        assert_eq!(capability_in_status("CapEff:\tzz\n", CAP_SYS_ADMIN), None);
        assert_eq!(capability_in_status("Name:\tx\n", CAP_SYS_ADMIN), None);
    }

    struct Echo;
    impl Plugin for Echo {
        fn validate(&self, _req: &PluginRequest) -> Result<ValidateResult, PluginError> {
            Ok(ValidateResult::default())
        }
        fn plan(&self, req: &PluginRequest) -> Result<PlanResult, PluginError> {
            let plan: GenerationPlan = serde_json::from_value(serde_json::json!({
                "protocolVersion": 1, "requestId": req.request_id, "instance": "echo/main",
                "files": [{"path": "../escape", "mode": "0644", "content": ""}]
            }))
            .unwrap();
            Ok(PlanResult { plans: vec![plan], warnings: vec![] })
        }
    }

    #[test]
    fn invalid_plans_are_rejected_by_the_sdk() {
        let req = test_request("plan", serde_json::json!({}));
        let resp = handle(&Echo, "plan", &serde_json::to_vec(&req).unwrap());
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "invalid-plan");
    }

    #[test]
    fn operation_mismatch() {
        let req = test_request("validate", serde_json::json!({}));
        let resp = handle(&Echo, "plan", &serde_json::to_vec(&req).unwrap());
        assert!(!resp.ok);
        let resp = handle(&Echo, "validate", &serde_json::to_vec(&req).unwrap());
        assert!(resp.ok);
    }

    #[test]
    fn rfc3339() {
        assert_eq!(unix_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_to_rfc3339(1_792_742_400), "2026-10-23T08:00:00Z");
        assert_eq!(unix_to_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        for t in [0u64, 951_782_400, 1_792_742_400, 4_102_444_799] {
            assert_eq!(rfc3339_to_unix(&unix_to_rfc3339(t)), Some(t));
        }
        assert_eq!(rfc3339_to_unix("2026-10-23T08:00:00.123Z"), Some(1_792_742_400));
        assert_eq!(rfc3339_to_unix("2026-13-23T08:00:00Z"), None);
    }
}
