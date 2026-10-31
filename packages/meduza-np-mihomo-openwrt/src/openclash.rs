//! OpenClash integration primitives (AGENT.md §9.8): detection of the installed OpenClash version against the
//! tested range, a small UCI access layer, and the recorded takeover (original option values saved before the
//! first change, restored on release).
//!
//! OpenClash has no stable cross-version REST API for its own settings; the adapter therefore only touches the
//! few UCI options listed in [`MANAGED_OPTIONS`] of section `openclash.config` and reads a few more for status.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// OpenClash (luci-app-openclash) versions the adapter was written and tested against: `[MIN, MAX)`.
/// The UCI option names below are those of these releases; other versions are reported as incompatible.
pub const TESTED_MIN: (u32, u32, u32) = (0, 45, 0);
pub const TESTED_MAX_EXCLUSIVE: (u32, u32, u32) = (0, 48, 0);

/// The only options of `openclash.config` this adapter ever writes.
pub const MANAGED_OPTIONS: &[&str] = &["enable", "config_path", "en_mode"];

/// Options read for status/diagnostics (never written).
pub const STATUS_OPTIONS: &[&str] =
    &["enable", "config_path", "en_mode", "cn_port", "dashboard_password", "dns_port", "core_type", "enable_redirect_dns"];

/// Running modes OpenClash offers through `en_mode` in the tested range.
pub const SUPPORTED_MODES: &[&str] = &["redir-host", "fake-ip", "redir-host-tun", "fake-ip-tun", "redir-host-mix", "fake-ip-mix"];

pub const CONFIG_DIR: &str = "/etc/openclash/config";
pub const INIT_SCRIPT: &str = "/etc/init.d/openclash";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Detection {
    pub installed: bool,
    pub version: Option<String>,
    pub compatible: bool,
    pub reason: Option<String>,
}

pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let t = text.trim().trim_start_matches('v');
    let numeric: String = t.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let mut it = numeric.split('.').filter(|s| !s.is_empty()).map(|s| s.parse::<u32>().ok());
    Some((it.next()??, it.next()??, it.next().flatten().unwrap_or(0)))
}

/// Installed luci-app-openclash version from the opkg control file or the apk database (OpenWrt ≥ 24).
pub fn installed_version(root: &Path) -> Option<String> {
    if let Ok(t) = std::fs::read_to_string(root.join("usr/lib/opkg/info/luci-app-openclash.control")) {
        if let Some(v) = t.lines().find_map(|l| l.strip_prefix("Version:")) {
            return Some(v.trim().to_string());
        }
    }
    let db = std::fs::read_to_string(root.join("lib/apk/db/installed")).ok()?;
    for block in db.split("\n\n") {
        if block.lines().any(|l| l == "P:luci-app-openclash") {
            return block.lines().find_map(|l| l.strip_prefix("V:")).map(|v| v.trim().to_string());
        }
    }
    None
}

pub fn detect(root: &Path) -> Detection {
    let init = root.join(INIT_SCRIPT.trim_start_matches('/'));
    let uci = root.join("etc/config/openclash");
    if !init.exists() || !uci.exists() {
        return Detection {
            installed: false,
            version: None,
            compatible: false,
            reason: Some("OpenClash is not installed (no /etc/init.d/openclash or /etc/config/openclash)".into()),
        };
    }
    let version = installed_version(root);
    let reason = match version.as_deref().map(|v| (v, parse_version(v))) {
        None => Some("cannot determine the installed luci-app-openclash version".to_string()),
        Some((v, None)) => Some(format!("unrecognized OpenClash version {v:?}")),
        Some((v, Some(n))) if n < TESTED_MIN || n >= TESTED_MAX_EXCLUSIVE => Some(format!(
            "OpenClash {v} is outside the tested range {}.{}.{} – <{}.{}.{}",
            TESTED_MIN.0, TESTED_MIN.1, TESTED_MIN.2, TESTED_MAX_EXCLUSIVE.0, TESTED_MAX_EXCLUSIVE.1, TESTED_MAX_EXCLUSIVE.2
        )),
        Some(_) => None,
    };
    Detection { installed: true, version, compatible: reason.is_none(), reason }
}

/// Parses `uci show openclash.config` output (`openclash.config.opt='value'`) into option → value.
pub fn parse_uci_show(text: &str, section: &str) -> BTreeMap<String, String> {
    let prefix = format!("{section}.");
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix(&prefix) else { continue };
        let Some((opt, raw)) = rest.split_once('=') else { continue };
        out.insert(opt.to_string(), unquote(raw));
    }
    out
}

fn unquote(raw: &str) -> String {
    let r = raw.trim();
    if r.len() >= 2 && r.starts_with('\'') && r.ends_with('\'') {
        r[1..r.len() - 1].replace("'\\''", "'")
    } else {
        r.to_string()
    }
}

/// Access to the `openclash.config` section.
pub trait Uci {
    fn get(&self, option: &str) -> Option<String>;
    fn set(&mut self, option: &str, value: &str) -> Result<(), String>;
    fn delete(&mut self, option: &str) -> Result<(), String>;
    fn commit(&mut self) -> Result<(), String>;
}

/// The system `uci` tool.
pub struct CliUci;

fn uci_cmd(args: &[&str]) -> Result<String, String> {
    let out = meduza_np_plugin_sdk::run_status_command("uci", args, Duration::from_secs(5), 256 * 1024).map_err(|e| format!("uci: {e}"))?;
    if out.timed_out {
        return Err("uci timed out".into());
    }
    if out.status != Some(0) {
        return Err(format!("uci {}: {}", args.first().copied().unwrap_or_default(), out.stderr.trim()));
    }
    Ok(out.stdout)
}

fn checked(option: &str) -> Result<(), String> {
    if MANAGED_OPTIONS.contains(&option) {
        Ok(())
    } else {
        Err(format!("option {option} is not managed by this adapter"))
    }
}

impl Uci for CliUci {
    fn get(&self, option: &str) -> Option<String> {
        uci_cmd(&["-q", "get", &format!("openclash.config.{option}")]).ok().map(|s| s.trim_end_matches('\n').to_string())
    }
    fn set(&mut self, option: &str, value: &str) -> Result<(), String> {
        checked(option)?;
        if value.contains(['\n', '\r', '\'']) {
            return Err("invalid characters in UCI value".into());
        }
        uci_cmd(&["set", &format!("openclash.config.{option}={value}")]).map(|_| ())
    }
    fn delete(&mut self, option: &str) -> Result<(), String> {
        checked(option)?;
        uci_cmd(&["-q", "delete", &format!("openclash.config.{option}")]).map(|_| ())
    }
    fn commit(&mut self) -> Result<(), String> {
        uci_cmd(&["commit", "openclash"]).map(|_| ())
    }
}

impl CliUci {
    pub fn show() -> BTreeMap<String, String> {
        uci_cmd(&["-q", "show", "openclash.config"]).map(|t| parse_uci_show(&t, "openclash.config")).unwrap_or_default()
    }
}

/// In-memory UCI section (tests, previews).
#[derive(Debug, Default, Clone)]
pub struct MemUci {
    pub values: BTreeMap<String, String>,
    pub commits: u32,
}

impl Uci for MemUci {
    fn get(&self, option: &str) -> Option<String> {
        self.values.get(option).cloned()
    }
    fn set(&mut self, option: &str, value: &str) -> Result<(), String> {
        checked(option)?;
        self.values.insert(option.into(), value.into());
        Ok(())
    }
    fn delete(&mut self, option: &str) -> Result<(), String> {
        checked(option)?;
        self.values.remove(option);
        Ok(())
    }
    fn commit(&mut self) -> Result<(), String> {
        self.commits += 1;
        Ok(())
    }
}

/// What the adapter changed, saved before the change so it can always be handed back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Takeover {
    pub taken_at: String,
    #[serde(default)]
    pub openclash_version: Option<String>,
    /// Original value of every option the adapter touched (`None` = the option was unset).
    pub options: BTreeMap<String, Option<String>>,
    /// Configuration files created by the adapter (the only files it may overwrite or delete).
    #[serde(default)]
    pub managed_files: Vec<String>,
}

impl Takeover {
    pub fn load(path: &Path) -> Option<Takeover> {
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok())
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        write_synced(path, &text)
    }

    /// Records the current value of each option not recorded yet. Returns true if something new was recorded
    /// (the caller must persist the takeover before changing anything).
    pub fn record(&mut self, uci: &dyn Uci, options: &[&str]) -> bool {
        let mut new = false;
        for o in options {
            if !self.options.contains_key(*o) {
                self.options.insert((*o).to_string(), uci.get(o));
                new = true;
            }
        }
        new
    }

    /// Restores every recorded option. Commits only if something changed. Returns the number of changes.
    pub fn restore(&self, uci: &mut dyn Uci) -> Result<usize, String> {
        let mut changed = 0;
        for (o, orig) in &self.options {
            let cur = uci.get(o);
            if &cur == orig {
                continue;
            }
            match orig {
                Some(v) => uci.set(o, v)?,
                None => uci.delete(o)?,
            }
            changed += 1;
        }
        if changed > 0 {
            uci.commit()?;
        }
        Ok(changed)
    }
}

/// Sets the desired managed options; every key must already be recorded in the takeover. Returns the changes.
pub fn apply_options(uci: &mut dyn Uci, takeover: &Takeover, desired: &BTreeMap<String, String>) -> Result<usize, String> {
    let mut changed = 0;
    for (o, v) in desired {
        if !takeover.options.contains_key(o) {
            return Err(format!("option {o} changed before its original value was recorded"));
        }
        if uci.get(o).as_deref() != Some(v.as_str()) {
            uci.set(o, v)?;
            changed += 1;
        }
    }
    if changed > 0 {
        uci.commit()?;
    }
    Ok(changed)
}

fn write_synced(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::OpenOptions::new().write(true).open(&tmp).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// `/etc/init.d/openclash <verb>` with a bound (OpenClash restarts take tens of seconds on small routers).
pub fn init_script(verb: &str) -> Result<(), String> {
    if !["restart", "reload", "stop", "start"].contains(&verb) {
        return Err(format!("unsupported init verb {verb}"));
    }
    let out =
        meduza_np_plugin_sdk::run_status_command(INIT_SCRIPT, &[verb], Duration::from_secs(180), 64 * 1024).map_err(|e| e.to_string())?;
    match (out.timed_out, out.status) {
        (true, _) => Err(format!("openclash {verb} timed out")),
        (false, Some(0)) => Ok(()),
        (false, code) => Err(format!("openclash {verb} exited with {code:?}: {}", out.stderr.trim().chars().take(300).collect::<String>())),
    }
}

/// Whether the OpenClash-managed core is running (pidfile-free check through /proc).
pub fn core_running() -> bool {
    let Ok(rd) = std::fs::read_dir("/proc") else { return false };
    rd.filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit())).any(|e| {
        std::fs::read(e.path().join("cmdline"))
            .map(|c| {
                c.split(|b| *b == 0)
                    .next()
                    .map(|a| a.ends_with(b"/etc/openclash/clash") || a.starts_with(b"/etc/openclash/core/"))
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    })
}
