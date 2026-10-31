//! Rule file refresh and the MosDNS wrapper (`run` mode). Downloads go to a temp file in the same directory,
//! are validated (text, size, every entry a domain rule), then atomically replace the previous version; on any
//! failure the previous file stays in use and the error is reported. MosDNS is restarted only when a rule file
//! really changed.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{rule_path, valid_domain_entry, DownloadPolicy, RuleFile, RunnerSpec};

pub const MAX_LINES: usize = 1_000_000;

/// Validates downloaded rule text. Returns the number of entries.
pub fn validate_rules(text: &str) -> Result<usize, String> {
    let mut n = 0;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !valid_domain_entry(line) {
            return Err(format!("line {}: not a domain rule", i + 1));
        }
        n += 1;
        if n > MAX_LINES {
            return Err("too many entries".into());
        }
    }
    if n == 0 {
        return Err("no entries".into());
    }
    Ok(n)
}

fn fetch_cmd(url: &str, tmp: &Path, policy: &DownloadPolicy, proxy: Option<&str>) -> Result<(), String> {
    let opts = meduza_np_plugin_sdk::fetch::FetchOptions {
        timeout_seconds: policy.timeout_seconds,
        max_bytes: policy.max_bytes,
        proxy,
        header: None,
    };
    meduza_np_plugin_sdk::fetch::fetch_to_file(url, tmp, &opts)
}

/// Downloads and installs one rule file. `Ok(true)` = content changed.
pub fn refresh(rule: &RuleFile, spec: &RunnerSpec) -> Result<bool, String> {
    let dest = rule_path(&spec.state_dir, rule.id.as_str());
    let dest = Path::new(&dest);
    let dir = dest.parent().ok_or("bad path")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".{}.download", rule.id));
    let _ = std::fs::remove_file(&tmp);
    let first = fetch_cmd(&rule.url, &tmp, &spec.download, spec.download.proxy.as_deref());
    if let Err(e) = first {
        if spec.download.proxy.is_some() && spec.download.direct_fallback {
            fetch_cmd(&rule.url, &tmp, &spec.download, None).map_err(|e2| format!("via proxy: {e}; direct: {e2}"))?;
        } else {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    let text = std::fs::read_to_string(&tmp).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("not UTF-8 text: {e}")
    })?;
    if let Err(e) = validate_rules(&text) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let new_digest = hex::encode(Sha256::digest(text.as_bytes()));
    let old_digest = std::fs::read(dest).map(|b| hex::encode(Sha256::digest(&b))).unwrap_or_default();
    if new_digest == old_digest {
        let _ = std::fs::remove_file(&tmp);
        return Ok(false);
    }
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
    Ok(true)
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleState {
    /// `ok`, `pending` (never downloaded), `stale` (refresh failed, previous rules in use), `failed` (none available).
    pub status: String,
    pub sha256: Option<String>,
    pub updated_at: Option<String>,
    pub last_error: Option<String>,
    #[serde(skip)]
    next_attempt: Option<Instant>,
    #[serde(skip)]
    failures: u32,
}

fn spawn(mosdns: &str, dir: &str) -> std::io::Result<Child> {
    Command::new(mosdns).args(["start", "-d", dir, "-c", &format!("{dir}/config.yaml")]).stdin(Stdio::null()).spawn()
}

#[cfg(unix)]
fn term_requested() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    static TERM: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_term(_: libc::c_int) {
        TERM.store(true, Ordering::SeqCst);
    }
    if !INSTALLED.swap(true, Ordering::SeqCst) {
        // SAFETY: the handler only stores to an atomic.
        unsafe {
            libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
        }
    }
    TERM.load(Ordering::SeqCst)
}

#[cfg(not(unix))]
fn term_requested() -> bool {
    false
}

pub fn run(args: &[String]) -> ! {
    let get = |k: &str| args.windows(2).find(|w| w[0] == k).map(|w| w[1].clone());
    let (Some(mosdns), Some(dir), Some(run_dir), Some(spec)) = (get("--mosdns"), get("--dir"), get("--run-dir"), get("--spec")) else {
        eprintln!("usage: run --mosdns BIN --dir GEN_DIR --run-dir DIR --spec JSON");
        std::process::exit(2)
    };
    let spec: RunnerSpec = serde_json::from_str(&spec).unwrap_or_else(|e| {
        eprintln!("invalid spec: {e}");
        std::process::exit(2)
    });
    term_requested();
    let state_file = format!("{run_dir}/mosdns-main.state");
    let mut states: BTreeMap<String, RuleState> = BTreeMap::new();
    for r in &spec.rules {
        let path = rule_path(&spec.state_dir, r.id.as_str());
        let existing = std::fs::read(&path).ok();
        if existing.is_none() {
            // MosDNS needs the file to exist; start with an empty set until the first download succeeds.
            if let Some(p) = Path::new(&path).parent() {
                let _ = std::fs::create_dir_all(p);
            }
            let _ = std::fs::write(&path, "");
        }
        states.insert(
            r.id.to_string(),
            RuleState {
                status: if existing.as_ref().map(|b| !b.is_empty()).unwrap_or(false) { "ok".into() } else { "pending".into() },
                sha256: existing.map(|b| hex::encode(Sha256::digest(&b))),
                next_attempt: Some(Instant::now()),
                ..Default::default()
            },
        );
    }
    let mut child = match spawn(&mosdns, &dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot start {mosdns}: {e}");
            std::process::exit(1)
        }
    };
    loop {
        if term_requested() {
            let _ = child.kill();
            let _ = child.wait();
            // a stopped (disabled) instance leaves nothing in the run dir; a restart writes it again
            let _ = std::fs::remove_file(&state_file);
            std::process::exit(0);
        }
        if let Ok(Some(status)) = child.try_wait() {
            eprintln!("mosdns exited: {status}");
            std::process::exit(status.code().unwrap_or(1));
        }
        let mut changed = false;
        for r in &spec.rules {
            let st = states.get_mut(r.id.as_str()).expect("state per rule");
            if st.next_attempt.map(|t| Instant::now() < t).unwrap_or(false) {
                continue;
            }
            match refresh(r, &spec) {
                Ok(c) => {
                    changed |= c;
                    st.status = "ok".into();
                    st.last_error = None;
                    st.failures = 0;
                    st.updated_at = Some(meduza_np_plugin_sdk::now_rfc3339());
                    st.sha256 = std::fs::read(rule_path(&spec.state_dir, r.id.as_str())).ok().map(|b| hex::encode(Sha256::digest(&b)));
                    st.next_attempt = Some(Instant::now() + Duration::from_secs(u64::from(r.refresh_hours) * 3600));
                }
                Err(e) => {
                    eprintln!("rule {}: {e}", r.id);
                    st.failures += 1;
                    st.last_error = Some(e);
                    st.status = if st.sha256.as_ref().map(|d| d != &hex::encode(Sha256::digest(b""))).unwrap_or(false) {
                        "stale".into()
                    } else {
                        "failed".into()
                    };
                    // Bounded exponential retry: 1, 2, 4 … 60 minutes.
                    let mins = 1u64 << st.failures.min(6);
                    st.next_attempt = Some(Instant::now() + Duration::from_secs(mins.min(60) * 60));
                }
            }
        }
        if changed {
            eprintln!("rule files changed; restarting mosdns");
            let _ = child.kill();
            let _ = child.wait();
            child = match spawn(&mosdns, &dir) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("cannot restart mosdns: {e}");
                    std::process::exit(1)
                }
            };
        }
        let tmp = format!("{state_file}.tmp");
        if std::fs::write(&tmp, serde_json::json!({"rules": states, "updatedAt": meduza_np_plugin_sdk::now_rfc3339()}).to_string()).is_ok()
        {
            let _ = std::fs::rename(&tmp, &state_file);
        }
        for _ in 0..50 {
            if term_requested() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_validation() {
        assert_eq!(validate_rules("# list\nexample.com\ndomain:cn\nfull:www.example.org\n\n").unwrap(), 3);
        assert!(validate_rules("<html>error page</html>\n").is_err(), "HTML error pages never replace rules");
        assert!(validate_rules("# only comments\n").is_err());
        assert!(validate_rules("good.example\nbad entry\n").is_err());
    }
}
