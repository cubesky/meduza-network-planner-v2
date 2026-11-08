//! Rule file refresh and the MosDNS wrapper (`run` mode). Downloads go to a temp file in the same directory,
//! are validated (text, size, domain/regexp or IP/CIDR entries), then atomically replace the previous version; on any
//! failure the previous file stays in use and the error is reported. MosDNS is restarted only when a rule file
//! really changed.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{rule_path, valid_domain_entry, DownloadPolicy, RuleFile, RunnerSpec};

pub const MAX_LINES: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleKind {
    Domain,
    Ip,
}

fn valid_ip_entry(line: &str) -> bool {
    match line.split_once('/') {
        Some((ip, prefix)) => {
            !prefix.is_empty()
                && prefix.bytes().all(|b| b.is_ascii_digit())
                && ip
                    .parse::<IpAddr>()
                    .ok()
                    .zip(prefix.parse::<u8>().ok())
                    .is_some_and(|(ip, prefix)| prefix <= if ip.is_ipv4() { 32 } else { 128 })
        }
        None => line.parse::<IpAddr>().is_ok(),
    }
}

/// Validates a homogeneous domain/regexp or IP/CIDR file. Custom pipelines choose the appropriate MosDNS
/// consumer; never mix IP entries into domain lists. The structured pipeline always requires domain rules.
pub fn validate_rules(text: &str) -> Result<usize, String> {
    validate_rule_text(text, true)
}

fn validate_rule_text(text: &str, allow_ip: bool) -> Result<usize, String> {
    let mut n = 0;
    let mut kind = None;
    for (i, raw) in text.lines().enumerate() {
        // MosDNS's text readers strip inline comments as well as whole comment lines.
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let expected = *kind.get_or_insert_with(|| if allow_ip && valid_ip_entry(line) { RuleKind::Ip } else { RuleKind::Domain });
        let valid = match expected {
            RuleKind::Domain => valid_domain_entry(line),
            RuleKind::Ip => valid_ip_entry(line),
        };
        if !valid {
            let description = if expected == RuleKind::Ip { "an IP/CIDR" } else { "a domain/regexp" };
            return Err(format!("line {}: not {description} rule", i + 1));
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
    if let Err(e) = validate_rule_text(&text, spec.custom_pipeline) {
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

    #[test]
    fn exported_geosite_sources_accept_their_regexp_entries() {
        // Public rule entries rejected by v2's previous domain-only character filter (2026-11-08).
        for (name, text, count) in [
            ("geosite-category-ads", "domain:ads.example\nregexp:^speed\\.(coe|open)\\.ad\\.[a-z]{2,6}\\.prod\\.hosts\\.ooklaserver\\.net$\n", 2),
            ("geosite-geolocation-cn", "regexp:.+\\.awsdns-cn-[0-9][0-9]\\.(biz|com|net|top)$\nregexp:.+\\.awsdns-cn-[0-9][a-e0-9]\\.cn$\nregexp:^.+-mihayo\\.akamaized\\.net$\n", 3),
            ("geosite-private", "domain:local\nregexp:^[a-z]([a-z0-9-]{0,61}[a-z0-9])?$\n", 2),
        ] {
            assert_eq!(validate_rule_text(text, false).unwrap_or_else(|e| panic!("{name}: {e}")), count);
        }
    }

    #[test]
    fn ip_rules_are_homogeneous_and_only_allowed_for_custom_pipelines() {
        let ips = "# geoip-cn\n1.0.1.0/24\n1.0.2.0/23 # CN\n2001:db8::/32\n192.0.2.1\n::1\n";
        assert_eq!(validate_rules(ips).unwrap(), 5);
        assert!(validate_rule_text(ips, false).is_err(), "do not feed CIDRs to generated domain sets");
        for text in
            ["1.2.3.0/33", "2001:db8::/129", "1.2.3.0/-1", "1.2.3.0/+1", "999.0.0.0/8", "1.2.3.0/24\nexample.com", "domain:cn\n1.2.3.0/24"]
        {
            assert!(validate_rules(text).is_err(), "{text}");
        }
    }

    #[test]
    fn malformed_or_unbounded_regex_never_replaces_rules() {
        for entry in ["regexp:[", "regexp:(unclosed", "regexp:*", "regexp:(?=lookahead)", "regexp:", "regexp:bad\nentry"] {
            assert!(!valid_domain_entry(entry), "{entry}");
        }
        assert!(!valid_domain_entry(&format!("regexp:{}", "x".repeat(4097))));
        assert!(!valid_domain_entry(&format!("regexp:{}x{}", "(".repeat(100), ")".repeat(100))));
        assert!(valid_domain_entry(&format!("regexp:^{}$", "x".repeat(254))), "regex is not limited to 253 domain characters");
    }

    #[test]
    fn comments_match_mosdns_text_readers() {
        assert_eq!(validate_rules("# header\ndomain:example.com # note\nregexp:^example\\.org$ # note\n").unwrap(), 2);
        assert!(validate_rules("domain:good\nregexp:[ # bad\n").unwrap_err().starts_with("line 2:"));
    }

    #[cfg(unix)]
    #[test]
    fn downloads_install_valid_rules_and_preserve_previous_bytes_on_errors() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        struct Temp(std::path::PathBuf);
        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let tmp = Temp(std::env::temp_dir().join(format!(
            "meduza-mosdns-rules-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        )));
        std::fs::create_dir_all(&tmp.0).unwrap();
        let good_ip = "1.0.1.0/24\n1.0.2.0/23\n";
        let good_domain = "domain:local\nregexp:^[a-z]([a-z0-9-]{0,61}[a-z0-9])?$\n";
        let bodies = [
            good_ip,
            "1.0.1.0/24\n1.0.2.0/33\n",
            good_domain,
            "domain:local\nregexp:[\n",
            "<html>Bad gateway</html>\n",
            good_domain,
            good_ip,
        ];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/rules", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            for body in bodies {
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => panic!("test download not received: {e}"),
                    }
                };
                stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let mut spec = RunnerSpec {
            rules: vec![],
            download: DownloadPolicy::default(),
            state_dir: tmp.0.to_str().unwrap().into(),
            custom_pipeline: true,
        };
        let mut rule: RuleFile = serde_json::from_value(serde_json::json!({"id":"geoip-cn", "url":url})).unwrap();
        let ip_dest = rule_path(&spec.state_dir, rule.id.as_str());
        assert!(refresh(&rule, &spec).unwrap());
        assert!(refresh(&rule, &spec).is_err());
        assert_eq!(std::fs::read_to_string(&ip_dest).unwrap(), good_ip);
        rule.id = meduza_np_protocol::ids::StableId::new("geosite-private").unwrap();
        let domain_dest = rule_path(&spec.state_dir, rule.id.as_str());
        assert!(refresh(&rule, &spec).unwrap());
        for _ in 0..2 {
            assert!(refresh(&rule, &spec).is_err());
            assert_eq!(std::fs::read_to_string(&domain_dest).unwrap(), good_domain);
            assert!(!tmp.0.join("mosdns/rules/.geosite-private.download").exists());
        }
        assert!(!refresh(&rule, &spec).unwrap(), "unchanged bytes do not restart MosDNS");
        spec.custom_pipeline = false;
        rule.id = meduza_np_protocol::ids::StableId::new("geoip-cn").unwrap();
        assert!(refresh(&rule, &spec).is_err(), "structured domain sets cannot consume IP lists");
        assert_eq!(std::fs::read_to_string(&ip_dest).unwrap(), good_ip);
        server.join().unwrap();
    }
}
