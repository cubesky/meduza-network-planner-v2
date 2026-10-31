use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use meduza_np_plugin_sdk::{run, PlanResult, Plugin, PluginError, PluginRequest, ProbeResult, ValidateResult};
use meduza_np_tinc as tinc;
use serde_json::{json, Value};

struct Tinc;

fn reach_records(run_dir: &str, cfg: &tinc::TincConfig) -> BTreeMap<String, BTreeMap<String, tinc::Reach>> {
    let mut out = BTreeMap::new();
    for n in &cfg.instances {
        let dir = Path::new(run_dir).join("tinc").join(n.id.as_str()).join("reach");
        let mut m = BTreeMap::new();
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if let Some(r) = std::fs::read_to_string(entry.path()).ok().and_then(|t| tinc::parse_reach(&t)) {
                m.insert(name, r);
            }
        }
        out.insert(n.id.to_string(), m);
    }
    out
}

/// Runs a key generation command in a scratch config dir, answering prompts with defaults, bounded in time.
fn keygen(program: &Path, args: &[&str], dir: &Path) -> Result<(), PluginError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| PluginError::new("provider-missing", format!("{}: {e}", program.display())))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"\n\n\n");
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| PluginError::new("keygen-failed", e.to_string()))? {
            if status.success() {
                return Ok(());
            }
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut e, &mut err);
            }
            let _ = std::fs::remove_dir_all(dir);
            return Err(PluginError::new("keygen-failed", err.trim().chars().take(300).collect::<String>()));
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = std::fs::remove_dir_all(dir);
            return Err(PluginError::new("keygen-failed", "key generation timed out"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn extract_block(text: &str, begin: &str, end: &str) -> Option<String> {
    let s = text.find(begin)?;
    let e = text[s..].find(end)? + s + end.len();
    Some(text[s..e].to_string())
}

impl Plugin for Tinc {
    fn validate(&self, req: &PluginRequest) -> Result<ValidateResult, PluginError> {
        let cfg = tinc::load(req)?;
        Ok(ValidateResult { errors: tinc::validate(&cfg, req), warnings: Vec::new() })
    }

    fn plan(&self, req: &PluginRequest) -> Result<PlanResult, PluginError> {
        let cfg = tinc::load(req)?;
        let errors = tinc::validate(&cfg, req);
        if !errors.is_empty() {
            return Err(PluginError::invalid(errors));
        }
        Ok(PlanResult { plans: tinc::plan(&cfg, req)?, warnings: Vec::new() })
    }

    fn probe(&self, req: &PluginRequest) -> Result<ProbeResult, PluginError> {
        let cfg = tinc::load(req)?;
        Ok(ProbeResult { instances: tinc::statuses(&cfg, &reach_records(&req.context.run_dir, &cfg)) })
    }

    /// `generate-keypair`: generates the key with the installed tinc so the format matches the provider.
    /// The private key is returned to planner (stored node-locally); only the public key reaches the manager.
    fn action(&self, req: &PluginRequest) -> Result<Value, PluginError> {
        if req.params.get("action").and_then(Value::as_str) != Some("generate-keypair") {
            return Err(PluginError::new("unsupported", "unknown action"));
        }
        let format = match req.params.get("keyKind").and_then(Value::as_str) {
            Some("tinc-ed25519-private") => tinc::KeyFormat::Ed25519,
            _ => tinc::KeyFormat::Rsa,
        };
        let tincd = PathBuf::from(&req.provider(tinc::SLOT)?.path);
        let mut rnd = [0u8; 8];
        for (i, b) in rnd.iter_mut().enumerate() {
            *b = (std::process::id() as u8).wrapping_add(i as u8).wrapping_mul(31)
                ^ (std::time::SystemTime::now().elapsed().map(|d| d.subsec_nanos() as u8).unwrap_or(7));
        }
        let dir = Path::new(&req.context.run_dir).join(format!(
            "tinc-keygen-{}-{}",
            std::process::id(),
            rnd.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ));
        std::fs::create_dir_all(dir.join("hosts")).map_err(|e| PluginError::new("internal", e.to_string()))?;
        std::fs::write(dir.join("tinc.conf"), "Name = keygen\n").map_err(|e| PluginError::new("internal", e.to_string()))?;
        let dir_s = dir.to_string_lossy().into_owned();
        let result = match format {
            tinc::KeyFormat::Rsa => {
                keygen(&tincd, &["-c", &dir_s, "-K4096"], &dir)?;
                let private =
                    std::fs::read_to_string(dir.join("rsa_key.priv")).map_err(|e| PluginError::new("keygen-failed", e.to_string()))?;
                let host =
                    std::fs::read_to_string(dir.join("hosts/keygen")).map_err(|e| PluginError::new("keygen-failed", e.to_string()))?;
                let public = extract_block(&host, "-----BEGIN RSA PUBLIC KEY-----", "-----END RSA PUBLIC KEY-----")
                    .ok_or_else(|| PluginError::new("keygen-failed", "no public key in generated host file"))?;
                json!({"privateKey": private.trim(), "publicKey": public})
            }
            tinc::KeyFormat::Ed25519 => {
                let ctl = tincd.with_file_name("tinc");
                keygen(&ctl, &["-c", &dir_s, "generate-ed25519-keys"], &dir)?;
                let private =
                    std::fs::read_to_string(dir.join("ed25519_key.priv")).map_err(|e| PluginError::new("keygen-failed", e.to_string()))?;
                let host =
                    std::fs::read_to_string(dir.join("hosts/keygen")).map_err(|e| PluginError::new("keygen-failed", e.to_string()))?;
                let public = host
                    .lines()
                    .find_map(|l| l.split_once('=').filter(|(k, _)| k.trim() == "Ed25519PublicKey").map(|(_, v)| v.trim().to_string()))
                    .ok_or_else(|| PluginError::new("keygen-failed", "no Ed25519PublicKey in generated host file"))?;
                json!({"privateKey": private.trim(), "publicKey": public})
            }
        };
        let _ = std::fs::remove_dir_all(&dir);
        Ok(result)
    }
}

fn main() {
    run(&Tinc, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}
