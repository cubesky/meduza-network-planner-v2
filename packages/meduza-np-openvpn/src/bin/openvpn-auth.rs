//! `openvpn-auth --instance <id> <credentials-file>` — OpenVPN `auth-user-pass-verify ... via-file` hook
//! (AGENT.md §9.3). Reads username/password from the temporary file OpenVPN passes as the last argument,
//! asks planner over the restricted Access socket, and exits 0 (allow) or 1 (deny). Errors and timeouts deny.
//! The password is never logged, written to disk or put in argv/env.
//!
//! With `auth-gen-token ... external-auth` OpenVPN also calls it on renegotiation, after verifying the client's token
//! itself (`session_state=Authenticated`): then only the username is sent, as a password-less re-check (D38).
//!
//! `openvpn-auth --instance <id> --connect <config-file>` — the `client-connect` hook: writes the session lifetime the
//! manager granted to this connection as `session-timeout` into OpenVPN's dynamic config file; no grant → disconnect.

#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
const TOTAL_TIMEOUT: Duration = Duration::from_secs(9);

fn deny(reason: &str) -> ! {
    eprintln!("openvpn-auth: deny ({reason})");
    std::process::exit(1)
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

fn socket() -> String {
    env("MEDUZA_NP_ACCESS_SOCKET").unwrap_or_else(|| "/run/meduza-np/access.sock".into())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let instance = args.windows(2).find(|w| w[0] == "--instance").map(|w| w[1].clone()).unwrap_or_else(|| deny("missing --instance"));
    let file = args.last().filter(|a| !a.starts_with("--") && **a != instance).cloned().unwrap_or_else(|| deny("missing file argument"));
    if args.iter().any(|a| a == "--connect") {
        connect(&instance, &file)
    }
    let content = std::fs::read_to_string(&file).unwrap_or_else(|_| deny("cannot read credentials file"));
    let mut lines = content.lines();
    let username = lines.next().unwrap_or_default().to_string();
    // Without `external-auth` (or with an older OpenVPN) there is no session_state: always a password login.
    let reauth = match env("session_state").as_deref() {
        None | Some("Initial") => false,
        Some("Authenticated") => true,
        Some("Expired") | Some("ExpiredEmptyUser") => deny("auth token expired"),
        Some(_) => deny("invalid auth token"),
    };
    // On a renegotiation the second line is the token: it is neither read further nor sent anywhere.
    let password = if reauth { String::new() } else { lines.next().unwrap_or_default().to_string() };
    if username.is_empty() || username.len() > 256 || (!reauth && (password.is_empty() || password.len() > 1024)) {
        deny("empty or oversized credentials");
    }
    let request_id = format!(
        "ovpn-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0)
    );
    let params = serde_json::json!({
        "requestId": request_id,
        "instanceId": instance,
        "username": username,
        "password": password,
        "reauth": reauth,
        "session": {
            "untrustedIp": env("untrusted_ip").or_else(|| env("untrusted_ip6")),
            "untrustedPort": env("untrusted_port").and_then(|p| p.parse::<u16>().ok()),
            "commonName": env("common_name"),
            "clientVersion": env("IV_VER"),
        }
    });
    let v = ask(&socket(), "access.auth", params).unwrap_or_else(|e| deny(&e));
    let decision = v.pointer("/result/decision").and_then(|d| d.as_str()).unwrap_or("deny");
    let reason = v.pointer("/result/reasonCode").and_then(|d| d.as_str()).unwrap_or("unknown");
    if decision != "allow" {
        deny(reason)
    }
    std::process::exit(0)
}

/// client-connect: `username-as-common-name` makes `common_name` the authenticated username.
fn connect(instance: &str, config_file: &str) -> ! {
    let username = env("common_name").unwrap_or_else(|| deny("no common_name"));
    let params = serde_json::json!({
        "instanceId": instance,
        "username": username,
        "ip": env("trusted_ip").or_else(|| env("trusted_ip6")),
        "port": env("trusted_port").and_then(|p| p.parse::<u16>().ok()),
    });
    let v = ask(&socket(), "access.session", params).unwrap_or_else(|e| deny(&e));
    if v.pointer("/result/allow").and_then(|a| a.as_bool()) != Some(true) {
        deny(v.pointer("/result/reasonCode").and_then(|d| d.as_str()).unwrap_or("unknown"))
    }
    let secs = v.pointer("/result/sessionSeconds").and_then(|s| s.as_u64()).unwrap_or(0);
    if secs > 0 {
        std::fs::write(config_file, format!("session-timeout {secs}\n")).unwrap_or_else(|_| deny("cannot write the client config"));
    }
    std::process::exit(0)
}

#[cfg(unix)]
fn ask(socket: &str, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    let stream = UnixStream::connect(socket).map_err(|e| format!("planner unreachable: {e}"))?;
    stream.set_read_timeout(Some(TOTAL_TIMEOUT)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
    let mut w = stream.try_clone().map_err(|e| e.to_string())?;
    let mut line =
        serde_json::to_vec(&serde_json::json!({"protocol": 1, "id": 1, "method": method, "params": params})).map_err(|e| e.to_string())?;
    line.push(b'\n');
    w.write_all(&line).map_err(|e| e.to_string())?;
    let mut resp = String::new();
    BufReader::new(stream).read_line(&mut resp).map_err(|_| "no answer in time".to_string())?;
    serde_json::from_str(&resp).map_err(|_| "invalid answer".to_string())
}

#[cfg(not(unix))]
fn ask(_socket: &str, _method: &str, _params: serde_json::Value) -> Result<serde_json::Value, String> {
    Err("only supported on Linux".into())
}
