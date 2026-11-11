//! `meduza-np` — node CLI (AGENT.md §12.3). A short-lived client of the planner (and updater) local API; it
//! keeps no state of its own. `--json` prints the raw, stable API result.

use std::path::PathBuf;

use meduza_np_runtime::paths::NodePaths;
use serde_json::{json, Value};

const USAGE: &str = "usage: meduza-np [--json] <command>

commands:
  status                         sync state, revisions, backoff, features summary
  features                       features of installed packages
  packages                       installed packages, registration and providers
  enroll --url URL --token TOKEN [--ca-file PEM] [--insecure]
  sync                           sync with the manager now
  validate                       validate the current configuration against installed packages
  plan                           show the generation plan for the current configuration
  logs [-n LINES]                recent planner log lines
  doctor                         dependency, provider and connectivity checks
  support-bundle [-o FILE]       redacted support bundle (status, checks, config, logs) as JSON
  maintenance status|stop
  maintenance start --reason TEXT [--minutes N] [--pause-apply] [--stop INSTANCE]...
  config                         last successfully applied configuration (secrets redacted)
  mihomo-panel                   current native panel connection (privileged; contains API secret)
  mihomo-panel-status            native panel availability (no credentials)
  instances                      applied instances, their package and supervised processes
  instance start|stop|restart INSTANCE
  rescan                         re-scan installed packages
  update status|check|plan|apply|pause|resume   (requires meduza-np-updater)
  dashboard-link [--url URL]     one-time local sign-in link for meduza-np-dashboard (root; valid 10 minutes)
  version";

struct Args {
    json: bool,
    rest: Vec<String>,
}

fn parse_args() -> Args {
    let mut json = false;
    let rest = std::env::args()
        .skip(1)
        .filter(|a| {
            if a == "--json" {
                json = true;
                false
            } else {
                true
            }
        })
        .collect();
    Args { json, rest }
}

fn flag(rest: &[String], name: &str) -> Option<String> {
    rest.iter().position(|a| a == name).and_then(|i| rest.get(i + 1)).cloned()
}

fn flags(rest: &[String], name: &str) -> Vec<String> {
    rest.windows(2).filter(|w| w[0] == name).map(|w| w[1].clone()).collect()
}

fn fail(msg: &str) -> ! {
    eprintln!("meduza-np: {msg}");
    std::process::exit(1)
}

/// `println!` to stdout, except that a reader which went away (`meduza-np --json status | head -c 100`) ends the
/// CLI quietly with status 0 instead of println!'s "failed printing to stdout" panic.
macro_rules! out {
    ($($arg:tt)*) => { emit(format_args!("{}
", format_args!($($arg)*))) };
}

fn emit(args: std::fmt::Arguments) {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    match stdout.write_fmt(args).and_then(|_| stdout.flush()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => fail(&format!("cannot write to stdout: {e}")),
    }
}

#[cfg(unix)]
fn call(socket: &PathBuf, method: &str, params: Value) -> Result<Value, String> {
    use meduza_np_protocol::local::{LocalRequest, LocalResponse, LOCAL_PROTOCOL_VERSION};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    let stream =
        UnixStream::connect(socket).map_err(|e| format!("cannot connect to {}: {e} (is the service running?)", socket.display()))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(120))).ok();
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let req = LocalRequest { protocol: LOCAL_PROTOCOL_VERSION, id: 1, method: method.into(), params };
    let mut line = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
    line.push(b'\n');
    writer.write_all(&line).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).map_err(|e| e.to_string())?;
    let resp: LocalResponse = serde_json::from_str(&resp).map_err(|e| format!("bad response: {e}"))?;
    match (resp.ok, resp.error) {
        (true, _) => Ok(resp.result),
        (false, Some(e)) => Err(format!("{}: {}", e.code, e.message)),
        (false, None) => Err("request failed".into()),
    }
}

#[cfg(not(unix))]
fn call(_socket: &PathBuf, _method: &str, _params: Value) -> Result<Value, String> {
    Err("the local API uses Unix sockets and is only available on Linux nodes".into())
}

fn s(v: &Value, ptr: &str) -> String {
    match v.pointer(ptr) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => "-".into(),
        Some(other) => other.to_string(),
    }
}

fn print_status(v: &Value) {
    out!("node        {}", s(v, "/nodeId"));
    out!("sync        {} via {} ({}, {})", s(v, "/sync/state"), s(v, "/sync/transport"), s(v, "/sync/mode"), s(v, "/sync/behavior"));
    out!(
        "revisions   desired {} · received {} · applied {} · last ok {}",
        s(v, "/sync/desiredRevision"),
        s(v, "/sync/receivedRevision"),
        s(v, "/sync/appliedRevision"),
        s(v, "/sync/lastSuccessfulRevision")
    );
    if let Some(stage) = v.pointer("/sync/applyStage").and_then(Value::as_str) {
        out!("apply       {stage}");
    }
    if let Some(r) = v.pointer("/sync/degradedReason").and_then(Value::as_str) {
        out!("degraded    {r}");
    }
    let level = v.pointer("/sync/backoff/failureLevel").and_then(Value::as_u64).unwrap_or(0);
    if level > 0 {
        out!(
            "backoff     level {level}, window {}s, next attempt in {} ms, cooldown {} ms — {}",
            s(v, "/sync/backoff/windowSeconds"),
            s(v, "/sync/backoff/nextAttemptInMs"),
            s(v, "/sync/backoff/serverCooldownMs"),
            s(v, "/sync/backoff/lastFailure")
        );
    }
    if let Some(e) = v.pointer("/sync/lastError").and_then(Value::as_str) {
        out!("last error  {e}");
    }
    if let Some(m) = v.get("maintenance").filter(|m| !m.is_null()) {
        out!("maintenance {} by {} until {} (apply paused: {})", s(m, "/reason"), s(m, "/by"), s(m, "/until"), s(m, "/applyPaused"));
    }
    out!("features:");
    for f in v.get("features").and_then(Value::as_array).into_iter().flatten() {
        let state = if !f["registered"].as_bool().unwrap_or(false) {
            "not registered"
        } else if !f["enabled"].as_bool().unwrap_or(false) {
            "installed, not enabled"
        } else if !f["available"].as_bool().unwrap_or(false) {
            "enabled, UNAVAILABLE"
        } else if f["healthy"].as_bool().unwrap_or(false) {
            "healthy"
        } else if f["running"].as_bool().unwrap_or(false) {
            "running"
        } else {
            "enabled"
        };
        let reason = f.get("reason").or_else(|| f.get("limitation")).and_then(Value::as_str).map(|r| format!(" — {r}")).unwrap_or_default();
        out!("  {:<28} {state}{reason}", s(f, "/id"));
    }
    out!("instances:");
    for i in v.get("instances").and_then(Value::as_array).into_iter().flatten() {
        let reason = i.get("reason").and_then(Value::as_str).map(|r| format!(" — {r}")).unwrap_or_default();
        out!("  {:<40} {:<10} {}{reason}", s(i, "/id"), s(i, "/state"), s(i, "/kind"));
    }
}

fn print_value(v: &Value) {
    out!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn main() {
    let args = parse_args();
    let paths = NodePaths::from_env();
    let planner = paths.planner_socket();
    let updater = paths.updater_socket();
    let rest = &args.rest;
    let cmd = rest.first().map(String::as_str).unwrap_or("help");
    if cmd == "dashboard-link" {
        return dashboard_link(&paths, flag(rest, "--url"));
    }
    let (socket, method, params): (&PathBuf, &str, Value) = match cmd {
        "status" => (&planner, "status", Value::Null),
        "features" => (&planner, "features", Value::Null),
        "packages" => (&planner, "packages", Value::Null),
        "instances" => (&planner, "instances", Value::Null),
        "config" => (&planner, "config.current", Value::Null),
        "mihomo-panel" => (&planner, "mihomo.panel", Value::Null),
        "mihomo-panel-status" => (&planner, "mihomo.panel.status", Value::Null),
        "sync" => (&planner, "sync.now", Value::Null),
        "validate" => (&planner, "validate", Value::Null),
        "plan" => (&planner, "plan.preview", Value::Null),
        "doctor" => (&planner, "doctor", Value::Null),
        "support-bundle" => (&planner, "support.bundle", Value::Null),
        "rescan" => (&planner, "registry.rescan", Value::Null),
        "logs" => (&planner, "logs", json!({"lines": flag(rest, "-n").and_then(|n| n.parse::<u64>().ok()).unwrap_or(100)})),
        "enroll" => {
            let url = flag(rest, "--url").unwrap_or_else(|| fail("--url is required"));
            let token = flag(rest, "--token").unwrap_or_else(|| fail("--token is required"));
            (
                &planner,
                "enroll",
                json!({"managerUrl": url, "token": token, "caFile": flag(rest, "--ca-file"), "allowInsecure": rest.iter().any(|a| a == "--insecure")}),
            )
        }
        "maintenance" => match rest.get(1).map(String::as_str) {
            Some("status") | None => (&planner, "maintenance.status", Value::Null),
            Some("stop") => (&planner, "maintenance.stop", Value::Null),
            Some("start") => {
                let reason = flag(rest, "--reason").unwrap_or_else(|| fail("--reason is required"));
                let minutes: u32 = flag(rest, "--minutes").and_then(|m| m.parse().ok()).unwrap_or(60);
                (
                    &planner,
                    "maintenance.start",
                    json!({"reason": reason, "durationSeconds": minutes * 60, "pauseApply": rest.iter().any(|a| a == "--pause-apply"), "stopInstances": flags(rest, "--stop")}),
                )
            }
            Some(other) => fail(&format!("unknown maintenance command {other}")),
        },
        "instance" => {
            let action = rest.get(1).map(String::as_str).unwrap_or_else(|| fail("instance start|stop|restart INSTANCE"));
            let instance = rest.get(2).cloned().unwrap_or_else(|| fail("instance id required, e.g. wireguard/uplink"));
            let method = match action {
                "start" => "instance.start",
                "stop" => "instance.stop",
                "restart" => "instance.restart",
                other => fail(&format!("unknown instance action {other}")),
            };
            (&planner, method, json!({"instance": instance}))
        }
        "update" => {
            if !updater.exists() {
                fail("automatic updates are not available: meduza-np-updater is not installed or not running");
            }
            let method = match rest.get(1).map(String::as_str).unwrap_or("status") {
                "status" => "update.status",
                "check" => "update.check",
                "plan" => "update.plan",
                "apply" => "update.apply",
                "pause" => "update.pause",
                "resume" => "update.resume",
                other => fail(&format!("unknown update command {other}")),
            };
            (&updater, method, Value::Null)
        }
        "version" | "--version" => {
            out!("meduza-np {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        _ => {
            out!("{USAGE}");
            return;
        }
    };
    match call(socket, method, params) {
        Ok(v) if args.json && cmd != "support-bundle" => print_value(&v),
        Ok(v) => match cmd {
            "status" => print_status(&v),
            "doctor" => {
                let mut bad = 0;
                for c in v.as_array().into_iter().flatten() {
                    let ok = c["ok"].as_bool().unwrap_or(false);
                    bad += usize::from(!ok);
                    out!("{} {:<36} {}", if ok { "ok  " } else { "FAIL" }, s(c, "/check"), s(c, "/detail"));
                }
                if bad > 0 {
                    std::process::exit(1);
                }
            }
            "support-bundle" => write_bundle(&v, flag(rest, "-o")),
            "logs" => {
                for l in v.as_array().into_iter().flatten() {
                    out!("{}", l.as_str().unwrap_or_default());
                }
            }
            "validate" => {
                if v["valid"].as_bool().unwrap_or(false) {
                    out!("configuration {} is valid for the installed packages", s(&v, "/revision"));
                } else {
                    out!("INVALID: {} ({})", s(&v, "/message"), s(&v, "/code"));
                    for d in v["details"].as_array().into_iter().flatten() {
                        out!("  {}: {}", s(d, "/path"), s(d, "/message"));
                    }
                    std::process::exit(1);
                }
            }
            _ => print_value(&v),
        },
        Err(e) => fail(&e),
    }
}

/// Writes the support bundle to `-o FILE` (mode 0600, never overwrites) or stdout. Credentials are already
/// redacted by the planner, but the bundle still describes the node, so it is not world-readable.
fn write_bundle(v: &Value, out: Option<String>) {
    let text = serde_json::to_string_pretty(v).unwrap_or_default() + "\n";
    let Some(path) = out.filter(|p| p != "-") else {
        emit(format_args!("{text}"));
        return;
    };
    let path = PathBuf::from(path);
    if path.exists() {
        fail(&format!("{} already exists", path.display()));
    }
    if let Err(e) = meduza_np_runtime::fsutil::write_atomic(&path, text.as_bytes(), 0o600) {
        fail(&format!("cannot write {}: {e}", path.display()));
    }
    let cut = v.get("truncated").and_then(Value::as_array).map(|c| c.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
    eprintln!("wrote {} ({} bytes{})", path.display(), text.len(), cut.map(|c| format!("; shortened: {c}")).unwrap_or_default());
}

/// Break-glass sign-in for the local dashboard (works without the manager): only someone who can write the node's
/// run directory (root / meduza-np group) can mint it; the dashboard accepts it once, within 10 minutes.
fn dashboard_link(paths: &NodePaths, url: Option<String>) {
    let dir = paths.run.join("dashboard").join("login-tokens");
    if let Err(e) = meduza_np_runtime::fsutil::ensure_dir(&dir, 0o700) {
        eprintln!("cannot create {}: {e} (run as root)", dir.display());
        std::process::exit(1);
    }
    let mut raw = [0u8; 32];
    if let Err(e) = getrandom::getrandom(&mut raw) {
        eprintln!("no OS randomness: {e}");
        std::process::exit(1);
    }
    let token = hex::encode(raw);
    let name = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(token.as_bytes()));
    // prune stale tokens (older than the 10 minute validity)
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let old =
                e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|a| a.as_secs() > 600).unwrap_or(true);
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    if let Err(e) = meduza_np_runtime::fsutil::write_atomic(&dir.join(name), b"", 0o600) {
        eprintln!("cannot write token: {e}");
        std::process::exit(1);
    }
    let base = url
        .or_else(|| std::env::var("MEDUZA_NP_DASHBOARD_PUBLIC_URL").ok())
        .unwrap_or_else(|| format!("http://{}", std::env::var("MEDUZA_NP_DASHBOARD_LISTEN").unwrap_or_else(|_| "127.0.0.1:8443".into())));
    out!("{}/auth/local?token={token}", base.trim_end_matches('/'));
    eprintln!("valid once, for 10 minutes; grants an admin session on this node's dashboard");
}
