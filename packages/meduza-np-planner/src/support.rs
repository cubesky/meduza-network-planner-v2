//! Redacted support bundle (AGENT.md §6.2 "诊断": bounded logs, dependency checks, generation errors, 脱敏支持导出).
//!
//! One JSON document an operator can hand to support: status report, packages/providers, doctor checks, applied
//! instances, the last applied configuration and a dry-run validation, plus recent planner logs. Everything passes
//! through [`redact_value`] (secret-like keys, PEM private keys, credentials in text) and the result is capped at
//! [`MAX_BUNDLE_BYTES`] so it also fits a manager job result. Served by the local API method `support.bundle`
//! (CLI `meduza-np support-bundle`, dashboard download) and the manager's `collect-diagnostics` job.

use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::sync::Planner;

/// Below the manager's 256 000 character job result limit, with room for the envelope.
pub const MAX_BUNDLE_BYTES: usize = 192 * 1024;
const LOG_LINES: usize = 500;
const REDACTED: &str = "<redacted>";

/// Normalized (lowercase, no `-`/`_`) key fragments whose string values are credentials.
const SENSITIVE: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "psk",
    "presharedkey",
    "privatekey",
    "apikey",
    "authorization",
    "cookie",
    "credential",
];
/// Keys that only *name* or *describe* a secret (`secretRef`, `tokenExpiresAt`, `passwordFile`) stay readable.
const DESCRIPTIVE_SUFFIXES: &[&str] =
    &["ref", "refs", "id", "ids", "file", "path", "hash", "kind", "at", "state", "status", "source", "mode"];

fn normalize(key: &str) -> String {
    key.chars().filter(|c| *c != '-' && *c != '_').flat_map(char::to_lowercase).collect()
}

/// Whether a JSON key holds a credential.
pub fn sensitive_key(key: &str) -> bool {
    let k = normalize(key);
    SENSITIVE.iter().any(|s| k.contains(s)) && !DESCRIPTIVE_SUFFIXES.iter().any(|s| k.ends_with(s))
}

/// Recursively redacts a JSON value: string values under sensitive keys, `secrets.*.value` envelopes and anything
/// [`redact_text`] finds in the remaining strings. Numbers, booleans and structure are kept.
pub fn redact_value(v: Value) -> Value {
    match v {
        Value::Object(o) => {
            let o: Map<String, Value> = o
                .into_iter()
                .map(|(k, v)| {
                    let v = if sensitive_key(&k) && k != "secrets" { redact_leaves(v) } else { redact_value(v) };
                    (k, v)
                })
                .collect();
            redact_secret_envelopes(Value::Object(o))
        }
        Value::Array(a) => Value::Array(a.into_iter().map(redact_value).collect()),
        Value::String(s) => Value::String(redact_text(&s)),
        other => other,
    }
}

/// Every string below a sensitive key, whatever its shape (a list of tokens, a nested auth object).
fn redact_leaves(v: Value) -> Value {
    match v {
        Value::String(s) if s.is_empty() => Value::String(s),
        Value::String(_) => Value::String(REDACTED.into()),
        Value::Array(a) => Value::Array(a.into_iter().map(redact_leaves).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, redact_leaves(v))).collect()),
        other => other,
    }
}

/// `{"secrets": {"<id>": {"kind": …, "value": …}}}` of a node configuration: ids and kinds stay, values go.
fn redact_secret_envelopes(mut v: Value) -> Value {
    if let Some(secrets) = v.get_mut("secrets").and_then(Value::as_object_mut) {
        for s in secrets.values_mut() {
            match s {
                Value::Object(o) => {
                    if let Some(val) = o.get_mut("value") {
                        *val = Value::String(REDACTED.into());
                    }
                }
                Value::String(x) => *x = REDACTED.into(),
                _ => {}
            }
        }
    }
    v
}

/// Redacts credentials inside free text (log lines, error messages, URLs):
/// PEM private key blocks, `Bearer …`, `scheme://user:pass@`, and `key=value` / `key: value` / `"key":"value"`
/// pairs whose key is sensitive.
pub fn redact_text(s: &str) -> String {
    let mut out = redact_pem(s);
    out = redact_bearer(&out);
    out = redact_userinfo(&out);
    redact_pairs(&out)
}

fn redact_pem(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("-----BEGIN ") {
        let header_end = rest[i..].find("-----\n").or_else(|| rest[i + 11..].find("-----").map(|j| j + 11)).map(|j| i + j + 5);
        let header = &rest[i..header_end.unwrap_or(rest.len())];
        if !header.contains("PRIVATE KEY") && !header.contains("OpenVPN Static key") {
            out.push_str(&rest[..i + 11]);
            rest = &rest[i + 11..];
            continue;
        }
        out.push_str(&rest[..i]);
        out.push_str(REDACTED);
        let after = &rest[header_end.unwrap_or(rest.len())..];
        rest = match after.find("-----END ") {
            Some(e) => {
                let tail = &after[e + 9..];
                match tail.find("-----") {
                    Some(t) => &tail[t + 5..],
                    None => "",
                }
            }
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn redact_bearer(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    let mut from = 0;
    while let Some(i) = lower[from..].find("bearer ").map(|i| i + from) {
        let start = i + 7;
        let end = s[start..].find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',').map(|e| start + e).unwrap_or(s.len());
        if end > start {
            out.push_str(&s[last..start]);
            out.push_str(REDACTED);
            last = end;
        }
        from = start;
    }
    out.push_str(&s[last..]);
    out
}

fn redact_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    let mut from = 0;
    while let Some(i) = s[from..].find("://").map(|i| i + from) {
        let start = i + 3;
        let authority_end = s[start..]
            .find(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace() || c == '"')
            .map(|e| start + e)
            .unwrap_or(s.len());
        if let Some(at) = s[start..authority_end].rfind('@') {
            out.push_str(&s[last..start]);
            out.push_str(REDACTED);
            last = start + at;
        }
        from = start;
    }
    out.push_str(&s[last..]);
    out
}

fn redact_pairs(s: &str) -> String {
    let bytes = s.as_bytes();
    let skip_spaces = |mut j: usize| {
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        j
    };
    // value ranges to replace, in order
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !is_key_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let ks = i;
        while i < bytes.len() && is_key_byte(bytes[i]) {
            i += 1;
        }
        if !sensitive_key(&s[ks..i]) {
            continue;
        }
        let mut j = i;
        if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
            j += 1;
        }
        j = skip_spaces(j);
        if j >= bytes.len() || (bytes[j] != b'=' && bytes[j] != b':') {
            continue;
        }
        j = skip_spaces(j + 1);
        // `Authorization: Bearer xyz` / `Basic xyz`: the credential is the next word
        for scheme in ["bearer ", "basic "] {
            if s.len() >= j + scheme.len() && s.as_bytes()[j..j + scheme.len()].eq_ignore_ascii_case(scheme.as_bytes()) {
                j = skip_spaces(j + scheme.len());
            }
        }
        let quote = (j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'')).then(|| bytes[j]);
        if quote.is_some() {
            j += 1;
        }
        let mut e = j;
        while e < bytes.len() {
            let c = bytes[e];
            let stop = match quote {
                Some(q) => c == q,
                None => c.is_ascii_whitespace() || matches!(c, b',' | b'&' | b';' | b'}' | b']' | b')' | b'"' | b'\''),
            };
            if stop {
                break;
            }
            e += 1;
        }
        if e > j && !s[j..e].starts_with(REDACTED) {
            ranges.push((j, e));
        }
        i = e.max(i);
    }
    if ranges.is_empty() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (a, b) in ranges {
        out.push_str(&s[last..a]);
        out.push_str(REDACTED);
        last = b;
    }
    out.push_str(&s[last..]);
    out
}

fn is_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Shrinks a bundle until it serializes within `max` bytes: older log lines first, then generation plans, then the
/// configuration body (its digest stays). What was cut is listed in `truncated`.
pub fn fit(mut bundle: Value, max: usize) -> Value {
    let size = |v: &Value| serde_json::to_vec(v).map(|b| b.len()).unwrap_or(usize::MAX);
    let mut cut: Vec<&str> = Vec::new();
    while size(&bundle) > max {
        let what = if let Some(l) = bundle.get_mut("logs").and_then(Value::as_array_mut).filter(|l| l.len() > 20) {
            let drop = l.len() / 2;
            l.drain(..drop);
            "logs"
        } else if bundle.pointer("/validation/plans").is_some_and(|p| !p.is_null()) {
            bundle["validation"]["plans"] = Value::Null;
            "validation.plans"
        } else if bundle.pointer("/config/config").is_some_and(|c| !c.is_null()) {
            bundle["config"]["config"] = Value::Null;
            "config.config"
        } else if bundle.get("report").is_some_and(|r| !r.is_null()) {
            bundle["report"] = Value::Null;
            "report"
        } else if let Some(l) = bundle.get_mut("logs").and_then(Value::as_array_mut).filter(|l| !l.is_empty()) {
            l.clear();
            "logs"
        } else {
            break;
        };
        if !cut.contains(&what) {
            cut.push(what);
        }
        // the marker counts towards the limit too
        bundle["truncated"] = json!(cut);
    }
    bundle
}

async fn validation(p: &Arc<Planner>) -> Value {
    let lg = match crate::state::load_last_good(&p.paths) {
        Ok(Some(lg)) => lg,
        Ok(None) => return json!({"valid": null, "message": "no configuration applied yet"}),
        Err(e) => return json!({"valid": null, "message": e.to_string()}),
    };
    match p.engine.prepare(&lg.raw, &lg.digest, &p.registry(), &p.platform).await {
        Ok(prepared) => json!({
            "valid": true,
            "revision": prepared.revision,
            "warnings": prepared.warnings,
            // file lists only: generated contents may hold credentials that no key name gives away
            "plans": prepared.plans.iter().map(|(pkg, plan)| json!({
                "package": pkg,
                "instance": plan.instance,
                "files": plan.files.iter().map(|f| json!({"path": f.path, "mode": f.mode, "bytes": f.content.len(), "sensitive": f.sensitive})).collect::<Vec<_>>(),
                "processes": plan.processes.iter().map(|pr| &pr.id).collect::<Vec<_>>(),
                "dependsOn": plan.depends_on,
            })).collect::<Vec<_>>(),
        }),
        Err(r) => json!({"valid": false, "code": r.code, "message": r.message, "details": r.details}),
    }
}

/// Builds the redacted, size-bounded support bundle.
pub async fn bundle(p: &Arc<Planner>) -> Value {
    let registry = p.registry();
    let applied = crate::state::AppliedState::load(&p.paths).unwrap_or_default();
    let instances: Map<String, Value> = applied
        .instances
        .iter()
        .map(|(id, a)| (id.clone(), json!({"package": a.package, "processes": a.processes.keys().collect::<Vec<_>>()})))
        .collect();
    let config = match crate::state::load_last_good(&p.paths) {
        Ok(Some(lg)) => json!({"digest": lg.digest, "config": lg.raw}),
        Ok(None) => Value::Null,
        Err(e) => json!({"error": e.to_string()}),
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let raw = json!({
        "kind": "meduza-np-support-bundle",
        "formatVersion": 1,
        "generatedAt": meduza_np_plugin_sdk::unix_to_rfc3339(now),
        "planner": {"version": crate::VERSION, "platform": p.platform},
        "report": serde_json::to_value(crate::reporter::build(p)).unwrap_or_default(),
        "doctor": crate::local_api::doctor(p),
        "packages": {"packages": registry.package_statuses(), "providers": registry.providers, "providerErrors": registry.provider_errors},
        "instances": {"processes": p.engine.supervisor.statuses(), "applied": instances},
        "maintenance": p.status.lock().unwrap().maintenance,
        "config": config,
        "validation": validation(p).await,
        "logs": crate::log::tail(LOG_LINES),
    });
    fit(redact_value(raw), MAX_BUNDLE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        for k in ["password", "bindPassword", "private_key", "privateKey", "presharedKey", "auth-token", "apiSecret", "Authorization"] {
            assert!(sensitive_key(k), "{k}");
        }
        for k in ["publicKey", "secretRef", "secretId", "tokenExpiresAt", "passwordFile", "secrets_path", "hostname", "keyKind"] {
            assert!(!sensitive_key(k), "{k}");
        }
    }

    #[test]
    fn values_are_redacted_but_structure_kept() {
        let v = json!({
            "node": {"id": "n1"},
            "secrets": {"wg-key": {"kind": "wireguard-private-key", "value": "QUJD"}, "plain": "x"},
            "ldap": {"bindPassword": "hunter2", "bindDn": "cn=admin", "secretRef": "ldap-bind", "tokens": ["a", "b"]},
            "peers": [{"publicKey": "PUB=", "presharedKey": "PSK=", "port": 51820}],
            "tls": {"key": "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n", "cert": "-----BEGIN CERTIFICATE-----\nCCCC\n-----END CERTIFICATE-----\n"},
            "subscription": "https://user:pw@example.com/sub?token=abc123&x=1",
            "auth": {"token": ""},
            "enabled": true,
        });
        let r = redact_value(v);
        assert_eq!(r["node"]["id"], "n1");
        assert_eq!(r["secrets"]["wg-key"]["kind"], "wireguard-private-key");
        assert_eq!(r["secrets"]["wg-key"]["value"], REDACTED);
        assert_eq!(r["secrets"]["plain"], REDACTED);
        assert_eq!(r["ldap"]["bindPassword"], REDACTED);
        assert_eq!(r["ldap"]["bindDn"], "cn=admin");
        assert_eq!(r["ldap"]["secretRef"], "ldap-bind");
        assert_eq!(r["ldap"]["tokens"], json!([REDACTED, REDACTED]));
        assert_eq!(r["peers"][0]["publicKey"], "PUB=");
        assert_eq!(r["peers"][0]["presharedKey"], REDACTED);
        assert_eq!(r["peers"][0]["port"], 51820);
        assert_eq!(r["tls"]["key"], format!("{REDACTED}\n"));
        assert!(r["tls"]["cert"].as_str().unwrap().contains("CCCC"));
        assert_eq!(r["subscription"], format!("https://{REDACTED}@example.com/sub?token={REDACTED}&x=1"));
        assert_eq!(r["auth"]["token"], "");
        assert_eq!(r["enabled"], true);
    }

    #[test]
    fn text() {
        assert_eq!(redact_text("ldap bind failed password=hunter2 user=bob"), format!("ldap bind failed password={REDACTED} user=bob"));
        assert_eq!(redact_text(r#"body {"token":"abc","ok":true}"#), format!(r#"body {{"token":"{REDACTED}","ok":true}}"#));
        assert_eq!(redact_text("Authorization: Bearer eyJ.abc.def next"), format!("Authorization: Bearer {REDACTED} next"));
        assert_eq!(redact_text("api_key: 'k k'"), format!("api_key: '{REDACTED}'"));
        assert_eq!(redact_text("tokenExpiresAt=2026 passwordFile=/etc/x"), "tokenExpiresAt=2026 passwordFile=/etc/x");
        assert_eq!(redact_text("no secrets here, just a secret word"), "no secrets here, just a secret word");
        assert_eq!(
            redact_text("x -----BEGIN OpenVPN Static key V1-----\nab\n-----END OpenVPN Static key V1----- y"),
            format!("x {REDACTED} y")
        );
        assert_eq!(redact_text("-----BEGIN PRIVATE KEY----- truncated"), REDACTED);
        assert_eq!(redact_text("dial ssh://root@host:22 ok"), format!("dial ssh://{REDACTED}@host:22 ok"));
        assert_eq!(redact_text("ünïcödé password=ö ok"), format!("ünïcödé password={REDACTED} ok"));
    }

    #[test]
    fn fit_cuts_logs_then_plans_then_config() {
        let logs: Vec<String> = (0..2000).map(|i| format!("line {i} {}", "x".repeat(100))).collect();
        let b = json!({"report": {"a": 1}, "config": {"digest": "d", "config": {"big": "y".repeat(5000)}}, "validation": {"plans": [1]}, "logs": logs});
        let r = fit(b.clone(), 50_000);
        assert!(serde_json::to_vec(&r).unwrap().len() <= 50_000);
        assert_eq!(r["truncated"], json!(["logs"]));
        let last = r["logs"].as_array().unwrap().last().unwrap().as_str().unwrap().to_string();
        assert!(last.starts_with("line 1999 "), "newest lines are kept");
        let r = fit(b.clone(), 2_500);
        assert!(serde_json::to_vec(&r).unwrap().len() <= 2_500);
        assert_eq!(r["config"]["digest"], "d");
        assert_eq!(r["truncated"], json!(["logs", "validation.plans", "config.config"]));
        assert_eq!(fit(json!({"a": 1}), 1000), json!({"a": 1}));
    }
}
