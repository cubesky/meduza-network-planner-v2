//! Updater settings: environment (base-container §14.3.1, also honoured natively) over
//! `/etc/meduza-np/updater.json`, over defaults.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// Official update source (GitHub Pages of the project repository). Deployments may point elsewhere
/// (mirror, `file:///` offline import directory) with `MEDUZA_NP_UPDATE_BASE_URL`.
pub const DEFAULT_BASE_URL: &str = "https://meduza-np.github.io/meduza-network-planner";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct FileConfig {
    base_url: Option<String>,
    channel: Option<String>,
    group_version: Option<String>,
    data_dir: Option<String>,
    check_interval_seconds: Option<u64>,
    auto_apply: Option<bool>,
    window: Option<String>,
    trust_file: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Content-addressed store + atomically switched view (base-container and generic Linux).
    Store,
    /// OpenWrt native package manager.
    Opkg,
    Apk,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Store => "store",
            Backend::Opkg => "opkg",
            Backend::Apk => "apk",
        }
    }

    /// Artifact formats this backend installs, in preference order.
    pub fn formats(self) -> &'static [&'static str] {
        match self {
            Backend::Store => &["tar.zst", "tar.gz"],
            Backend::Opkg => &["ipk"],
            Backend::Apk => &["apk"],
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub channel: String,
    pub group_version: Option<String>,
    /// Persistent data root; the store lives in `<data_dir>/store`.
    pub data_dir: PathBuf,
    pub backend: Backend,
    pub platform: String,
    /// `MEDUZA_NP_COMPONENTS` when set (explicit selection, normalized short names); `None` = keep installed set.
    pub components: Option<Vec<String>>,
    pub check_interval: Duration,
    pub auto_apply: bool,
    /// Daily UTC window `HH:MM-HH:MM` for automatic applies.
    pub window: Option<(u32, u32)>,
    pub trust_files: Vec<PathBuf>,
    /// Version of the updater built into the base image (container bootstrap baseline).
    pub image_updater_version: Option<String>,
    pub paths: meduza_np_runtime::paths::NodePaths,
}

impl Config {
    pub fn store_dir(&self) -> PathBuf {
        self.data_dir.join("store")
    }

    pub fn from_env() -> Result<Self, String> {
        let paths = meduza_np_runtime::paths::NodePaths::from_env();
        let file: FileConfig = meduza_np_runtime::fsutil::read_json(&paths.etc.join("updater.json"), 64 * 1024)
            .map_err(|e| format!("updater.json: {e}"))?
            .unwrap_or_default();
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let openwrt = Path::new("/etc/openwrt_release").exists();
        let backend = match env("MEDUZA_NP_UPDATE_BACKEND").as_deref() {
            Some("store") => Backend::Store,
            Some("opkg") => Backend::Opkg,
            Some("apk") => Backend::Apk,
            Some(other) => return Err(format!("MEDUZA_NP_UPDATE_BACKEND={other}: expected store, opkg or apk")),
            None if openwrt && has_binary("apk") => Backend::Apk,
            None if openwrt => Backend::Opkg,
            None => Backend::Store,
        };
        let platform = match env("MEDUZA_NP_PLATFORM") {
            Some(p) => p,
            None if backend != Backend::Store => crate::openwrt::platform().ok_or("cannot read DISTRIB_ARCH from /etc/openwrt_release")?,
            None => host_platform().ok_or("unsupported CPU architecture")?.to_string(),
        };
        let data_dir = env("MEDUZA_NP_DATA_DIR").or(file.data_dir).map(PathBuf::from).unwrap_or_else(|| paths.state.clone());
        let components = env("MEDUZA_NP_COMPONENTS").map(|v| parse_components(&v)).transpose()?;
        let window = env("MEDUZA_NP_UPDATE_WINDOW").or(file.window).map(|w| parse_window(&w)).transpose()?;
        let auto_apply = match env("MEDUZA_NP_UPDATE_AUTO").as_deref() {
            Some("0") | Some("false") | Some("no") => false,
            Some(_) => true,
            None => file.auto_apply.unwrap_or(true),
        };
        let mut trust_files = vec![];
        if let Some(t) = env("MEDUZA_NP_TRUST_FILE").or(file.trust_file) {
            trust_files.push(PathBuf::from(t));
        }
        trust_files.push(paths.etc.join("trust/release-keys.json"));
        trust_files.push(PathBuf::from("/usr/share/meduza-np/trust/release-keys.json"));
        let base_url = env("MEDUZA_NP_UPDATE_BASE_URL").or(file.base_url).unwrap_or_else(|| DEFAULT_BASE_URL.into());
        let channel = env("MEDUZA_NP_UPDATE_CHANNEL").or(file.channel).unwrap_or_else(|| "stable".into());
        if !channel.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') || channel.is_empty() || channel.len() > 32 {
            return Err(format!("invalid channel {channel:?}"));
        }
        let group_version = env("MEDUZA_NP_GROUP_VERSION").or(file.group_version);
        if let Some(g) = &group_version {
            if !valid_version_label(g) {
                return Err(format!("invalid MEDUZA_NP_GROUP_VERSION {g:?}"));
            }
        }
        Ok(Self {
            base_url,
            channel,
            group_version,
            data_dir,
            backend,
            platform,
            components,
            check_interval: Duration::from_secs(
                env("MEDUZA_NP_UPDATE_CHECK_SECONDS")
                    .and_then(|v| v.parse().ok())
                    .or(file.check_interval_seconds)
                    .unwrap_or(6 * 3600)
                    .clamp(300, 7 * 86_400),
            ),
            auto_apply,
            window,
            trust_files,
            image_updater_version: env("MEDUZA_NP_IMAGE_UPDATER_VERSION"),
            paths,
        })
    }

    pub fn in_window(&self, now_unix: u64) -> bool {
        let Some((from, to)) = self.window else { return true };
        let m = ((now_unix % 86_400) / 60) as u32;
        if from <= to {
            m >= from && m < to
        } else {
            m >= from || m < to
        }
    }
}

fn has_binary(name: &str) -> bool {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"].iter().any(|d| Path::new(d).join(name).exists())
}

/// `linux/amd64`, `linux/arm64`, `linux/arm/v7` for this build's CPU.
pub fn host_platform() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("linux/amd64"),
        "aarch64" => Some("linux/arm64"),
        "arm" => Some("linux/arm/v7"),
        _ => None,
    }
}

pub fn valid_version_label(v: &str) -> bool {
    !v.is_empty() && v.len() <= 64 && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
}

/// `MEDUZA_NP_COMPONENTS`: comma separated short names (`wireguard,dnsmasq`) or full package names. planner and
/// updater are fixed and dropped if listed; duplicates collapse; manager/base-container are rejected (§14.3.1).
pub fn parse_components(raw: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let full = if part.starts_with(meduza_np_protocol::manifest::PACKAGE_PREFIX) {
            part.to_string()
        } else {
            format!("{}{part}", meduza_np_protocol::manifest::PACKAGE_PREFIX)
        };
        if !meduza_np_protocol::manifest::ALL_PACKAGES.contains(&full.as_str()) {
            return Err(format!("unknown component {part:?}"));
        }
        match full.as_str() {
            "meduza-np-planner" | "meduza-np-updater" => continue,
            "meduza-np-manager" => return Err("manager is not a node component (deploy it with its own Podman files)".into()),
            "meduza-np-base-container" => return Err("base-container cannot select itself".into()),
            _ => {}
        }
        if !out.contains(&full) {
            out.push(full);
        }
    }
    out.sort();
    Ok(out)
}

fn parse_window(w: &str) -> Result<(u32, u32), String> {
    let parse = |t: &str| -> Option<u32> {
        let (h, m) = t.trim().split_once(':')?;
        let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
        (h < 24 && m < 60).then_some(h * 60 + m)
    };
    let (a, b) = w.split_once('-').ok_or_else(|| format!("window {w:?}: expected HH:MM-HH:MM"))?;
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) if a != b => Ok((a, b)),
        _ => Err(format!("window {w:?}: expected HH:MM-HH:MM (UTC), start != end")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_normalize_and_reject() {
        assert_eq!(
            parse_components(" wireguard, dnsmasq,planner,meduza-np-wireguard,,updater ").unwrap(),
            vec!["meduza-np-dnsmasq", "meduza-np-wireguard"]
        );
        assert!(parse_components("").unwrap().is_empty());
        assert!(parse_components("manager").is_err());
        assert!(parse_components("base-container").is_err());
        assert!(parse_components("wireguard,https://evil.example/x").is_err());
        assert!(parse_components("nope").is_err());
    }

    #[test]
    fn windows() {
        assert_eq!(parse_window("02:00-05:30").unwrap(), (120, 330));
        assert!(parse_window("25:00-01:00").is_err());
        assert!(parse_window("01:00-01:00").is_err());
        let mut c = Config {
            base_url: String::new(),
            channel: "stable".into(),
            group_version: None,
            data_dir: PathBuf::new(),
            backend: Backend::Store,
            platform: "linux/amd64".into(),
            components: None,
            check_interval: Duration::from_secs(3600),
            auto_apply: true,
            window: Some((22 * 60, 2 * 60)),
            trust_files: vec![],
            image_updater_version: None,
            paths: meduza_np_runtime::paths::NodePaths::under(Path::new("/x")),
        };
        assert!(c.in_window(23 * 3600));
        assert!(c.in_window(86_400 + 3600));
        assert!(!c.in_window(12 * 3600));
        c.window = None;
        assert!(c.in_window(12 * 3600));
    }
}
