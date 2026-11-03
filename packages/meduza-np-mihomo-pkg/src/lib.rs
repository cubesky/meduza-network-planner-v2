//! Mihomo binary provider (AGENT.md §3.3). This package never contains Mihomo itself; it prepares and
//! registers a binary in a persistent directory:
//!
//! - `auto-download`: a pinned release asset for the detected platform from `assets.json` (shipped and
//!   versioned with the package), verified against its trusted SHA-256, staged, decompressed, ELF-checked,
//!   probed for its version, then atomically activated (one previous copy kept). Never "latest".
//! - `manual`: never touches the network; the operator copies `mihomo` into the data directory and triggers
//!   check/activate. A file without an ownership record is always treated as user-provided and never
//!   overwritten, even in auto mode.
//!
//! `provider.json` in the provider state directory records source, mode, version, platform, digest,
//! verification and the last error; planner's registry reads it (§3.3.3).

use std::io::Read;
pub mod progress;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use meduza_np_plugin_sdk::PluginRequest;
use meduza_np_protocol::{ValidationError, Validator};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_DOWNLOAD: u64 = 64 * 1024 * 1024;
pub const MAX_BINARY: u64 = 160 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(title = "Mihomo binary source")]
pub struct BinarySourceConfig {
    #[serde(default)]
    pub mode: Mode,
    /// Release tag to install in auto mode; must be listed in the package's tested asset manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_version: Option<String>,
    /// Persistent directory holding `mihomo`, `.staging/` and `previous/`. Never /run, /tmp or a container layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_proxy: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    AutoDownload,
    #[default]
    Manual,
}

/// Tested upstream assets shipped with this package version (`assets.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetManifest {
    pub format_version: u32,
    pub upstream: String,
    #[serde(default)]
    pub versions: Vec<VersionAssets>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionAssets {
    pub tag: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    /// `linux/amd64`, `linux/arm/v7`, `linux/arm64`.
    pub platform: String,
    pub name: String,
    pub url: String,
    /// SHA-256 of the downloaded (compressed) asset.
    pub sha256: String,
    pub size: u64,
}

/// Platform of this host as used in asset manifests; tested mapping, not derived from OpenWrt package labels.
pub fn host_platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux/amd64"),
        ("linux", "aarch64") => Some("linux/arm64"),
        // Rust reports "arm" for armv7 hard-float musl targets; armv6/soft-float are not supported targets.
        ("linux", "arm") => Some("linux/arm/v7"),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElfInfo {
    pub class64: bool,
    pub little_endian: bool,
    pub machine: u16,
}

pub fn parse_elf(head: &[u8]) -> Option<ElfInfo> {
    if head.len() < 20 || &head[..4] != b"\x7fELF" {
        return None;
    }
    let class64 = head[4] == 2;
    let little_endian = head[5] == 1;
    let machine = if little_endian { u16::from_le_bytes([head[18], head[19]]) } else { u16::from_be_bytes([head[18], head[19]]) };
    Some(ElfInfo { class64, little_endian, machine })
}

/// ELF machine/class expected for a platform (§3.3.1: architecture, width, endianness).
pub fn elf_matches(info: ElfInfo, platform: &str) -> Result<(), String> {
    let (machine, class64) = match platform {
        "linux/amd64" => (0x3e, true),
        "linux/arm64" => (0xb7, true),
        "linux/arm/v7" => (0x28, false),
        other => return Err(format!("unsupported platform {other}")),
    };
    if !info.little_endian {
        return Err("big-endian binary".into());
    }
    if info.machine != machine || info.class64 != class64 {
        return Err(format!(
            "binary is for ELF machine {:#x} ({}-bit), this node needs {machine:#x} ({}-bit)",
            info.machine,
            if info.class64 { 64 } else { 32 },
            if class64 { 64 } else { 32 }
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderState {
    /// `registered`, `invalid`, `waiting-binary`, `download-failed`.
    pub state: String,
    pub path: String,
    /// `github-release` (verified digest) or `user-provided`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// True only if the digest matched a trusted, pre-shipped value.
    #[serde(default)]
    pub verified: bool,
    /// Set only when this tool installed the current file (ownership for safe replacement).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub updated_at: String,
}

pub struct Dirs {
    /// Where `provider.json` lives (registry reads it): `<state>/providers/mihomo`.
    pub state_dir: PathBuf,
    /// Where the binary lives (`binarySource.dataDir`, defaults to `state_dir`).
    pub data_dir: PathBuf,
}

impl Dirs {
    pub fn new(state_root: &str, cfg: &BinarySourceConfig) -> Self {
        let state_dir = Path::new(state_root).join("providers").join("mihomo");
        let data_dir = cfg.data_dir.as_ref().map(PathBuf::from).unwrap_or_else(|| state_dir.clone());
        Self { state_dir, data_dir }
    }
    pub fn binary(&self) -> PathBuf {
        self.data_dir.join("mihomo")
    }
    pub fn staging(&self) -> PathBuf {
        self.data_dir.join(".staging")
    }
    pub fn previous(&self) -> PathBuf {
        self.data_dir.join("previous")
    }
    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("provider.json")
    }
}

pub fn load_state(d: &Dirs) -> Option<ProviderState> {
    std::fs::read_to_string(d.state_file()).ok().and_then(|t| serde_json::from_str(&t).ok())
}

pub fn save_state(d: &Dirs, s: &ProviderState) -> Result<(), String> {
    std::fs::create_dir_all(&d.state_dir).map_err(|e| e.to_string())?;
    let tmp = d.state_dir.join(".provider.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, d.state_file()).map_err(|e| e.to_string())
}

pub fn sha256_file(p: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(p).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Bounded version probe (`mihomo -v`) of a candidate binary.
pub fn probe_version(bin: &Path) -> Result<String, String> {
    let mut child = Command::new(bin)
        .arg("-v")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot execute: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            let mut out = String::new();
            if let Some(mut s) = child.stdout.take() {
                let _ = (&mut s).take(4096).read_to_string(&mut out);
            }
            if !status.success() {
                return Err(format!("version probe exited with {status}"));
            }
            // "Mihomo Meta v1.19.0 linux amd64 with go1.22 ..."
            return out
                .split_whitespace()
                .find(|w| w.starts_with('v') && w[1..].starts_with(|c: char| c.is_ascii_digit()))
                .map(str::to_string)
                .ok_or_else(|| "no version in output".into());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err("version probe timed out".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Checks a candidate binary: regular file, size, ELF for this platform, executable, version.
pub fn inspect(bin: &Path, platform: &str) -> Result<(String, String), String> {
    let meta = std::fs::symlink_metadata(bin).map_err(|_| format!("{} does not exist", bin.display()))?;
    if !meta.file_type().is_file() {
        return Err("not a regular file (symlinks are not accepted)".into());
    }
    if meta.len() > MAX_BINARY || meta.len() < 1024 {
        return Err(format!("unexpected size {} bytes", meta.len()));
    }
    let mut head = [0u8; 64];
    std::fs::File::open(bin).and_then(|mut f| f.read(&mut head)).map_err(|e| e.to_string())?;
    let info = parse_elf(&head).ok_or("not an ELF executable (copy the extracted file, not the .gz)")?;
    elf_matches(info, platform)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err("not executable (chmod 0755)".into());
        }
    }
    let version = probe_version(bin)?;
    Ok((version, sha256_file(bin)?))
}

pub fn validate(cfg: &BinarySourceConfig, assets: &AssetManifest) -> Vec<ValidationError> {
    let mut v = Validator::new();
    if let Some(d) = &cfg.data_dir {
        v.check(d.starts_with('/') && !d.contains(".."), "/dataDir", "absolute path required");
        v.check(
            !(d.starts_with("/run") || d.starts_with("/tmp") || d.starts_with("/var/run") || d.starts_with("/dev/shm")),
            "/dataDir",
            "must be persistent storage, not /run or /tmp",
        );
    }
    if cfg.mode == Mode::AutoDownload {
        match &cfg.target_version {
            None => v.error("/targetVersion", "auto-download needs a pinned target version (never latest)"),
            Some(t) => {
                let known = assets.versions.iter().find(|x| &x.tag == t);
                v.check(
                    known.is_some(),
                    "/targetVersion",
                    format!(
                        "{t} is not a tested version of this package; tested: {:?}",
                        assets.versions.iter().map(|x| &x.tag).collect::<Vec<_>>()
                    ),
                );
                if let (Some(k), Some(p)) = (known, host_platform()) {
                    v.check(k.assets.iter().any(|a| a.platform == p), "/targetVersion", format!("no tested {p} asset for {t}"));
                }
            }
        }
    }
    if let Some(p) = &cfg.download_proxy {
        v.check(p.starts_with("http://") || p.starts_with("socks5h://"), "/downloadProxy", "http:// or socks5h:// proxy");
    }
    v.finish().err().unwrap_or_default()
}

pub fn load(req: &PluginRequest) -> Result<BinarySourceConfig, meduza_np_plugin_sdk::PluginError> {
    Ok(req.service::<BinarySourceConfig>("mihomo-pkg")?.unwrap_or(BinarySourceConfig {
        mode: Mode::Manual,
        target_version: None,
        data_dir: None,
        download_proxy: None,
    }))
}

/// Registers whatever is in the data directory (manual flow and post-download verification).
pub fn check_local(d: &Dirs, cfg: &BinarySourceConfig, trusted: Option<&str>) -> ProviderState {
    let platform = host_platform().unwrap_or("unknown");
    let prev = load_state(d);
    let mut st = ProviderState {
        state: "waiting-binary".into(),
        path: d.binary().to_string_lossy().into_owned(),
        mode: match cfg.mode {
            Mode::Manual => "manual".into(),
            Mode::AutoDownload => "auto-download".into(),
        },
        platform: Some(platform.into()),
        updated_at: meduza_np_plugin_sdk::now_rfc3339(),
        installed_by: prev.as_ref().and_then(|p| p.installed_by.clone()),
        ..Default::default()
    };
    match inspect(&d.binary(), platform) {
        Ok((version, digest)) => {
            let ours = prev.as_ref().and_then(|p| p.installed_by.as_deref()) == Some("meduza-np-mihomo-pkg")
                && prev.as_ref().and_then(|p| p.sha256.as_deref()) == Some(digest.as_str());
            st.verified = trusted.map(|t| t == digest).unwrap_or(false) || (ours && prev.as_ref().map(|p| p.verified).unwrap_or(false));
            // An unknown file is user-provided: digest computed ≠ official source verified (§3.3.2).
            st.source = Some(if ours { "github-release".into() } else { "user-provided".into() });
            if !ours {
                st.installed_by = None;
            }
            st.version = Some(version);
            st.sha256 = Some(digest);
            st.state = "registered".into();
        }
        Err(e) if !d.binary().exists() => st.last_error = Some(e),
        Err(e) => {
            st.state = "invalid".into();
            st.last_error = Some(e);
        }
    }
    st
}

/// Downloads, verifies and activates the pinned asset (auto mode). Never overwrites a user-provided file.
pub fn download(d: &Dirs, cfg: &BinarySourceConfig, assets: &AssetManifest) -> Result<ProviderState, String> {
    if cfg.mode != Mode::AutoDownload {
        return Err("manual mode never downloads".into());
    }
    let platform = host_platform().ok_or("unsupported host platform")?;
    let tag = cfg.target_version.clone().ok_or("no target version")?;
    let asset = assets
        .versions
        .iter()
        .find(|v| v.tag == tag)
        .and_then(|v| v.assets.iter().find(|a| a.platform == platform))
        .ok_or("no tested asset for this platform")?;
    if d.binary().exists() {
        let prev = load_state(d);
        let ours = prev.as_ref().and_then(|p| p.installed_by.as_deref()) == Some("meduza-np-mihomo-pkg");
        if !ours {
            return Err(
                "a user-provided mihomo exists in the data directory; it is never overwritten (switch to manual mode or remove it)".into(),
            );
        }
        if prev.as_ref().and_then(|p| p.version.as_deref()) == Some(tag.as_str()) && prev.as_ref().map(|p| p.verified).unwrap_or(false) {
            return Ok(check_local(d, cfg, prev.as_ref().and_then(|p| p.sha256.as_deref())));
        }
    }
    std::fs::create_dir_all(d.staging()).map_err(|e| e.to_string())?;
    let gz = d.staging().join(&asset.name);
    let transfer = progress::Transfer::start(d, &gz, &tag, platform, asset.size)?;
    let result = download_asset(d, cfg, asset, platform, &tag, &gz, &transfer).and_then(|st| {
        save_state(d, &st)?;
        Ok(st)
    });
    if let Err(e) = transfer.finish(result.as_ref().err().map(String::as_str)) {
        eprintln!("cannot record final binary progress: {e}");
    }
    result
}

fn download_asset(
    d: &Dirs,
    cfg: &BinarySourceConfig,
    asset: &Asset,
    platform: &str,
    tag: &str,
    gz: &Path,
    transfer: &progress::Transfer,
) -> Result<ProviderState, String> {
    let opts = meduza_np_plugin_sdk::fetch::FetchOptions {
        timeout_seconds: 300,
        max_bytes: asset.size.clamp(1, MAX_DOWNLOAD),
        proxy: cfg.download_proxy.as_deref(),
        header: None,
    };
    meduza_np_plugin_sdk::fetch::fetch_to_file(&asset.url, gz, &opts)?;
    transfer.stage("verifying")?;
    let size = std::fs::metadata(gz).map_err(|e| e.to_string())?.len();
    if size != asset.size {
        return Err(format!("download size mismatch for {} (got {size}, expected {})", asset.name, asset.size));
    }
    let got = sha256_file(gz)?;
    if got != asset.sha256 {
        let _ = std::fs::remove_file(gz);
        return Err(format!("digest mismatch for {} (got {got}, expected {})", asset.name, asset.sha256));
    }
    let candidate = d.staging().join("mihomo.new");
    transfer.stage("decompressing")?;
    {
        let input = std::fs::File::open(gz).map_err(|e| e.to_string())?;
        let mut dec = flate2::read::GzDecoder::new(input).take(MAX_BINARY + 1);
        let mut out = std::fs::File::create(&candidate).map_err(|e| e.to_string())?;
        let n = std::io::copy(&mut dec, &mut out).map_err(|e| format!("decompress: {e}"))?;
        if n > MAX_BINARY {
            let _ = std::fs::remove_file(&candidate);
            return Err("decompressed binary too large".into());
        }
    }
    let _ = std::fs::remove_file(gz);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    transfer.stage("validating")?;
    let (version, digest) = inspect(&candidate, platform).inspect_err(|_| {
        let _ = std::fs::remove_file(&candidate);
    })?;
    if version != tag {
        let _ = std::fs::remove_file(&candidate);
        return Err(format!("binary reports {version}, expected {tag}"));
    }
    // Keep exactly one previous copy, then atomically replace (never write into the running binary).
    transfer.stage("activating")?;
    if d.binary().exists() {
        std::fs::create_dir_all(d.previous()).map_err(|e| e.to_string())?;
        let _ = std::fs::rename(d.binary(), d.previous().join("mihomo"));
    }
    std::fs::rename(&candidate, d.binary()).map_err(|e| e.to_string())?;
    let mut st = check_local(d, cfg, Some(&asset.sha256));
    st.installed_by = Some("meduza-np-mihomo-pkg".into());
    st.source = Some("github-release".into());
    st.verified = true;
    st.sha256 = Some(digest);
    st.download_stage = Some("activated".into());
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn real_http_transfer_reports_bytes_and_preserves_previous_binary_on_validation_failure() {
        use std::io::Write;
        use std::net::TcpListener;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&std::fs::read("/bin/true").unwrap()).unwrap();
        let payload = encoder.finish().unwrap();
        let size = payload.len() as u64;
        let sha256 = hex::encode(Sha256::digest(&payload));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/asset.gz", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    other => panic!("HTTP fixture did not receive a connection: {other:?}"),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let _ = stream.read(&mut [0u8; 4096]).unwrap();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n").unwrap();
            for part in payload.chunks(payload.len().div_ceil(8)) {
                stream.write_all(part).unwrap();
                stream.flush().unwrap();
                std::thread::sleep(Duration::from_millis(350));
            }
        });
        let root = std::env::temp_dir().join(format!("mnp-binary-http-{}", std::process::id()));
        let cfg = BinarySourceConfig {
            mode: Mode::AutoDownload,
            target_version: Some("v0.0.0-fixture".into()),
            data_dir: None,
            download_proxy: None,
        };
        let d = Dirs::new(root.to_str().unwrap(), &cfg);
        std::fs::create_dir_all(&d.data_dir).unwrap();
        std::fs::write(d.binary(), b"previous binary").unwrap();
        save_state(
            &d,
            &ProviderState {
                state: "registered".into(),
                version: Some("v0.0.0-previous".into()),
                installed_by: Some("meduza-np-mihomo-pkg".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let assets = AssetManifest {
            format_version: 1,
            upstream: url.clone(),
            versions: vec![VersionAssets {
                tag: cfg.target_version.clone().unwrap(),
                assets: vec![Asset { platform: host_platform().unwrap().into(), name: "asset.gz".into(), url, sha256, size }],
            }],
        };
        let (root2, cfg2) = (root.clone(), cfg.clone());
        let task = std::thread::spawn(move || download(&Dirs::new(root2.to_str().unwrap(), &cfg2), &cfg2, &assets));
        let mut partial = false;
        while !task.is_finished() {
            if let Some(p) = progress::load(&d) {
                partial |= p.active && p.downloaded_bytes > 0 && p.downloaded_bytes < p.bytes_total;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(task.join().unwrap().is_err(), "the fixture is a real ELF but not Mihomo; version validation must reject it");
        server.join().unwrap();
        let p = progress::load(&d).unwrap();
        assert!(partial, "the real transfer must report intermediate bytes, not only 0% and 100%");
        assert_eq!(p.phase, "failed");
        assert_eq!(p.downloaded_bytes, size);
        assert!(p.error.is_some());
        assert_eq!(std::fs::read(d.binary()).unwrap(), b"previous binary");
        assert_eq!(load_state(&d).unwrap().version.as_deref(), Some("v0.0.0-previous"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn elf_checks() {
        let mut h = [0u8; 64];
        h[..4].copy_from_slice(b"\x7fELF");
        h[4] = 2;
        h[5] = 1;
        h[18] = 0x3e;
        let info = parse_elf(&h).unwrap();
        assert!(elf_matches(info, "linux/amd64").is_ok());
        assert!(elf_matches(info, "linux/arm64").unwrap_err().contains("0x3e"));
        h[4] = 1;
        h[18] = 0x28;
        assert!(elf_matches(parse_elf(&h).unwrap(), "linux/arm/v7").is_ok());
        assert!(parse_elf(b"\x1f\x8b\x08 gzip data......").is_none(), "a .gz is not accepted as binary");
    }

    #[test]
    fn validation_requires_pinned_tested_version_and_persistent_dir() {
        let assets: AssetManifest =
            serde_json::from_str(r#"{"formatVersion": 1, "upstream": "https://github.com/MetaCubeX/mihomo/releases", "versions": []}"#)
                .unwrap();
        let cfg = BinarySourceConfig {
            mode: Mode::AutoDownload,
            target_version: None,
            data_dir: Some("/tmp/mihomo".into()),
            download_proxy: None,
        };
        let text = format!("{:?}", validate(&cfg, &assets));
        assert!(text.contains("pinned target version"));
        assert!(text.contains("persistent storage"));
        let cfg =
            BinarySourceConfig { mode: Mode::AutoDownload, target_version: Some("v9.9.9".into()), data_dir: None, download_proxy: None };
        assert!(format!("{:?}", validate(&cfg, &assets)).contains("not a tested version"));
        let manual = BinarySourceConfig { mode: Mode::Manual, target_version: None, data_dir: None, download_proxy: None };
        assert!(validate(&manual, &assets).is_empty(), "manual mode needs no network configuration");
    }

    #[test]
    fn manual_missing_file_waits_and_foreign_file_is_user_provided() {
        let root = std::env::temp_dir().join(format!("mnp-mihomo-pkg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cfg = BinarySourceConfig { mode: Mode::Manual, target_version: None, data_dir: None, download_proxy: None };
        let d = Dirs::new(&root.to_string_lossy(), &cfg);
        let manual_with_version = BinarySourceConfig { target_version: Some("v1.0.0".into()), ..cfg.clone() };
        assert!(download(&d, &manual_with_version, &AssetManifest::default()).unwrap_err().contains("manual mode never downloads"));
        assert!(!d.staging().exists());
        let st = check_local(&d, &cfg, None);
        assert_eq!(st.state, "waiting-binary");
        std::fs::create_dir_all(&d.data_dir).unwrap();
        std::fs::write(d.binary(), b"not an elf at all, just text that is long enough to pass the size check........................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................................").unwrap();
        let st = check_local(&d, &cfg, None);
        assert_eq!(st.state, "invalid");
        assert!(!st.verified);
        // Auto mode refuses to overwrite a file it does not own.
        let auto =
            BinarySourceConfig { mode: Mode::AutoDownload, target_version: Some("v1.0.0".into()), data_dir: None, download_proxy: None };
        let assets = AssetManifest {
            format_version: 1,
            upstream: String::new(),
            versions: vec![VersionAssets {
                tag: "v1.0.0".into(),
                assets: vec![Asset {
                    platform: host_platform().unwrap_or("linux/amd64").into(),
                    name: "x.gz".into(),
                    url: "https://example.invalid/x.gz".into(),
                    sha256: "00".into(),
                    size: 10,
                }],
            }],
        };
        if host_platform().is_some() {
            assert!(download(&d, &auto, &assets).unwrap_err().contains("never overwritten"));
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn config_schema_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/config.json");
        meduza_np_protocol::schema::check_file::<BinarySourceConfig>(&path).unwrap();
    }
}
