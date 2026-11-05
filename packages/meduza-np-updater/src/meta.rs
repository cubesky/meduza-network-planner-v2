//! Update source access and signed metadata checks (§13.4): channel pointer → group manifest, both signed over
//! their raw bytes, pointer digest binds the manifest, sequence never goes backwards (rollback protection) and
//! expired metadata is refused (freeze protection). Sources: `https://` (http only when explicitly allowed for
//! tests), `file:///…` or an absolute directory (offline import).

use std::path::{Path, PathBuf};

use meduza_np_protocol::group::{ChannelPointer, GroupManifest, TrustStore, GROUP_FORMAT_VERSION};

use crate::sig;

pub const MAX_POINTER_BYTES: u64 = 64 * 1024;
pub const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaError {
    /// `fetch-failed`, `signature-invalid`, `digest-mismatch`, `metadata-expired`, `rollback-detected`,
    /// `invalid-metadata`, `untrusted-source`.
    pub code: &'static str,
    pub message: String,
}

impl MetaError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl std::fmt::Display for MetaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

#[derive(Debug, Clone)]
pub struct Source {
    base: String,
}

impl Source {
    pub(crate) fn fetch_once(&self, url: &str, dest: &Path, opts: &meduza_np_plugin_sdk::fetch::FetchOptions<'_>) -> Result<(), MetaError> {
        if let Some(path) = Self::local_path(url) {
            if std::fs::metadata(&path).map_err(|e| MetaError::new("fetch-failed", e.to_string()))?.len() > opts.max_bytes {
                return Err(MetaError::new("fetch-failed", "chunk exceeds signed size"));
            }
            std::fs::copy(path, dest).map_err(|e| MetaError::new("fetch-failed", e.to_string()))?;
            return Ok(());
        }
        meduza_np_plugin_sdk::fetch::fetch_to_file(url, dest, opts).map_err(|e| MetaError::new("fetch-failed", format!("{url}: {e}")))
    }
    pub fn new(base: &str) -> Result<Self, MetaError> {
        let base = base.trim().trim_end_matches('/').to_string();
        let allow_http = std::env::var("MEDUZA_NP_UPDATE_ALLOW_HTTP").map(|v| v == "1").unwrap_or(false);
        let ok = base.starts_with("https://")
            || base.starts_with("file://")
            || Path::new(&base).is_absolute()
            || (allow_http && base.starts_with("http://"));
        if !ok || base.contains(['"', '\'', ' ', '\n']) {
            return Err(MetaError::new("untrusted-source", format!("update source must be https://, file:// or an absolute path: {base}")));
        }
        Ok(Self { base })
    }

    /// Resolves a path relative to the source root, or passes through an absolute https URL from signed metadata.
    pub fn resolve(&self, rel_or_url: &str) -> Result<String, MetaError> {
        if rel_or_url.starts_with("https://") {
            return Ok(rel_or_url.to_string());
        }
        let ok = !rel_or_url.is_empty()
            && !rel_or_url.starts_with('/')
            && rel_or_url.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
            && rel_or_url.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_' | '+'));
        if !ok {
            return Err(MetaError::new("invalid-metadata", format!("bad relative path {rel_or_url:?}")));
        }
        Ok(format!("{}/{rel_or_url}", self.base))
    }

    fn local_path(url: &str) -> Option<PathBuf> {
        if let Some(p) = url.strip_prefix("file://") {
            // file:///C:/x on Windows test runs, file:///srv/x on Linux.
            let p = if cfg!(windows) { p.trim_start_matches('/') } else { p };
            return Some(PathBuf::from(p));
        }
        Path::new(url).is_absolute().then(|| PathBuf::from(url))
    }

    /// Downloads (or copies) `url` to `dest`, refusing more than `max_bytes`.
    pub fn fetch_to(&self, url: &str, dest: &Path, max_bytes: u64, timeout_seconds: u32) -> Result<(), MetaError> {
        self.fetch_to_with_progress(url, dest, max_bytes, timeout_seconds, |_| {})
    }

    pub fn fetch_to_with_progress(
        &self,
        url: &str,
        dest: &Path,
        max_bytes: u64,
        timeout_seconds: u32,
        notify: impl FnMut(crate::retry::Attempt),
    ) -> Result<(), MetaError> {
        if let Some(src) = Self::local_path(url) {
            let len = std::fs::metadata(&src).map_err(|e| MetaError::new("fetch-failed", format!("{}: {e}", src.display())))?.len();
            if len > max_bytes {
                return Err(MetaError::new("fetch-failed", format!("{url} exceeds {max_bytes} bytes")));
            }
            std::fs::copy(&src, dest).map_err(|e| MetaError::new("fetch-failed", format!("{}: {e}", src.display())))?;
            return Ok(());
        }
        let opts = meduza_np_plugin_sdk::fetch::FetchOptions { timeout_seconds, max_bytes, proxy: None, header: None };
        crate::retry::download(
            || {
                meduza_np_plugin_sdk::fetch::fetch_to_file(url, dest, &opts)
                    .map_err(|e| MetaError::new("fetch-failed", format!("{url}: {e}")))
            },
            notify,
        )
    }

    pub fn fetch_bytes(&self, rel: &str, tmp_dir: &Path, max_bytes: u64) -> Result<Vec<u8>, MetaError> {
        let url = self.resolve(rel)?;
        meduza_np_runtime::fsutil::ensure_dir(tmp_dir, 0o700).map_err(|e| MetaError::new("fetch-failed", e.to_string()))?;
        let tmp = tmp_dir.join(format!("meta-{}.tmp", rel.replace('/', "_")));
        let r = self
            .fetch_to(&url, &tmp, max_bytes, 60)
            .and_then(|_| std::fs::read(&tmp).map_err(|e| MetaError::new("fetch-failed", e.to_string())));
        let _ = std::fs::remove_file(&tmp);
        r
    }
}

#[derive(Debug, Clone)]
pub struct VerifiedGroup {
    pub pointer: Option<ChannelPointer>,
    pub manifest: GroupManifest,
    pub signed_by: Vec<String>,
}

pub struct CheckInput<'a> {
    pub source: &'a Source,
    pub trust: &'a TrustStore,
    pub channel: &'a str,
    /// `MEDUZA_NP_GROUP_VERSION`: fetch exactly this group (no channel pointer, explicit operator choice).
    pub pinned: Option<&'a str>,
    /// Highest channel sequence accepted before on this node.
    pub last_sequence: Option<u64>,
    pub now_unix: u64,
    pub tmp_dir: &'a Path,
}

fn expired(at: &str, now: u64) -> bool {
    meduza_np_plugin_sdk::rfc3339_to_unix(at).map(|t| now >= t).unwrap_or(true)
}

pub fn check(i: &CheckInput<'_>) -> Result<VerifiedGroup, MetaError> {
    let (pointer, manifest_rel) = match i.pinned {
        Some(v) => (None, format!("releases/{v}/manifest.json")),
        None => {
            let rel = format!("channels/{}.json", i.channel);
            let raw = i.source.fetch_bytes(&rel, i.tmp_dir, MAX_POINTER_BYTES)?;
            let sig = i.source.fetch_bytes(&format!("{rel}.sig"), i.tmp_dir, sig::MAX_SIG_BYTES)?;
            sig::verify(&raw, &sig, i.trust, i.now_unix).map_err(|e| MetaError::new("signature-invalid", format!("{rel}: {e}")))?;
            let p: ChannelPointer = serde_json::from_slice(&raw).map_err(|e| MetaError::new("invalid-metadata", format!("{rel}: {e}")))?;
            if p.format_version != GROUP_FORMAT_VERSION || p.channel != i.channel {
                return Err(MetaError::new("invalid-metadata", format!("{rel}: format {} / channel {:?}", p.format_version, p.channel)));
            }
            if expired(&p.expires_at, i.now_unix) {
                return Err(MetaError::new("metadata-expired", format!("{rel} expired at {}", p.expires_at)));
            }
            if let Some(last) = i.last_sequence {
                if p.sequence < last {
                    return Err(MetaError::new(
                        "rollback-detected",
                        format!("{rel} sequence {} is older than the accepted {last}", p.sequence),
                    ));
                }
            }
            let expected = format!("releases/{}/manifest.json", p.group_version);
            if p.manifest != expected {
                return Err(MetaError::new("invalid-metadata", format!("pointer names {:?}, expected {expected}", p.manifest)));
            }
            (Some(p), expected)
        }
    };
    let raw = i.source.fetch_bytes(&manifest_rel, i.tmp_dir, MAX_MANIFEST_BYTES)?;
    if let Some(p) = &pointer {
        let got = meduza_np_protocol::digest::sha256_hex(&raw);
        if !got.eq_ignore_ascii_case(&p.manifest_sha256) {
            return Err(MetaError::new("digest-mismatch", format!("{manifest_rel}: sha256 {got} != pointer {}", p.manifest_sha256)));
        }
    }
    let sig = i.source.fetch_bytes(&format!("{manifest_rel}.sig"), i.tmp_dir, sig::MAX_SIG_BYTES)?;
    let signed_by =
        sig::verify(&raw, &sig, i.trust, i.now_unix).map_err(|e| MetaError::new("signature-invalid", format!("{manifest_rel}: {e}")))?;
    let m: GroupManifest = serde_json::from_slice(&raw).map_err(|e| MetaError::new("invalid-metadata", format!("{manifest_rel}: {e}")))?;
    if m.format_version != GROUP_FORMAT_VERSION {
        return Err(MetaError::new("invalid-metadata", format!("unsupported group format {}", m.format_version)));
    }
    match (&pointer, i.pinned) {
        (Some(p), _) => {
            if m.group_version != p.group_version || m.sequence != p.sequence {
                return Err(MetaError::new("invalid-metadata", "manifest version/sequence differ from the channel pointer"));
            }
            if expired(&m.expires_at, i.now_unix) {
                return Err(MetaError::new("metadata-expired", format!("{manifest_rel} expired at {}", m.expires_at)));
            }
        }
        // A pinned group is an explicit operator choice: expiry is reported by `status` but does not block it.
        (None, Some(v)) if m.group_version != v => {
            return Err(MetaError::new("invalid-metadata", format!("{manifest_rel} holds {}", m.group_version)));
        }
        _ => {}
    }
    validate_manifest(&m)?;
    Ok(VerifiedGroup { pointer, manifest: m, signed_by })
}

fn validate_manifest(m: &GroupManifest) -> Result<(), MetaError> {
    let mut names = std::collections::BTreeSet::new();
    for p in &m.packages {
        if !names.insert(p.name.as_str()) {
            return Err(MetaError::new("invalid-metadata", format!("package {} listed twice", p.name)));
        }
        if !crate::config::valid_version_label(&p.version) {
            return Err(MetaError::new("invalid-metadata", format!("{}: bad version {:?}", p.name, p.version)));
        }
        for a in &p.artifacts {
            crate::chunks::validate(a).map_err(|e| MetaError::new("invalid-metadata", format!("{}: {e}", p.name)))?;
            let source = Source::new("https://validation.invalid")?;
            for part in &a.chunks {
                source.resolve(&part.url)?;
            }
            let hex_ok = a.sha256.len() == 64 && a.sha256.bytes().all(|b| b.is_ascii_hexdigit());
            if !hex_ok || a.size == 0 {
                return Err(MetaError::new("invalid-metadata", format!("{}: artifact {} lacks size/sha256", p.name, a.platform)));
            }
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod retry_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    #[test]
    fn real_pages_requests_resume_at_the_missing_part() {
        use meduza_np_protocol::group::{Artifact, ArtifactChunk};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let source = Source { base: format!("http://{}", listener.local_addr().unwrap()) };
        let mut body = vec![b'a'; 500_000];
        body.push(b'z');
        let parts = vec![
            ArtifactChunk { url: "parts/0".into(), size: 500_000, sha256: meduza_np_protocol::digest::sha256_hex(&body[..500_000]) },
            ArtifactChunk { url: "parts/1".into(), size: 1, sha256: meduza_np_protocol::digest::sha256_hex(b"z") },
        ];
        let artifact = Artifact {
            platform: "linux/amd64".into(),
            format: "tar.gz".into(),
            url: "full.tar.gz".into(),
            size: body.len() as u64,
            sha256: meduza_np_protocol::digest::sha256_hex(&body),
            oci: None,
            chunks: parts,
        };
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for n in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                requests.push(request);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                match n {
                    0 => {
                        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 500000\r\nConnection: close\r\n\r\n").unwrap();
                        stream.write_all(&vec![b'a'; 500_000]).unwrap();
                    }
                    1 => stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap(),
                    _ => stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nz").unwrap(),
                }
            }
            requests
        });
        let root = std::env::temp_dir().join(format!("mnp-real-pages-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let dest = root.join("archive");
        let cache = root.join("downloads");
        assert!(crate::chunks::download(&source, &artifact, &dest, &cache, |_| {}).unwrap_err().contains("404"));
        crate::chunks::download(&source, &artifact, &dest, &cache, |_| {}).unwrap();
        assert_eq!(std::fs::read(dest).unwrap(), body);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /parts/0 "));
        assert!(requests[1..].iter().all(|r| r.starts_with("GET /parts/1 ")), "the verified first part is never fetched twice");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn real_download_recovers_from_a_partial_body_and_http_503() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/package", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for number in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                let response = match number {
                    0 => "HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\nx",
                    1 => "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    _ => "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\ncomplete",
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        let dest = std::env::temp_dir().join(format!("mnp-real-retry-{}.tmp", std::process::id()));
        let mut events = vec![];
        Source::new("https://release.example").unwrap().fetch_to_with_progress(&url, &dest, 64, 10, |e| events.push(e)).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"complete");
        assert_eq!(events.iter().filter(|e| e.retry_after.is_some()).count(), 2);
        assert_eq!(events.last().unwrap().number, 3);
        server.join().unwrap();
        std::fs::remove_file(dest).unwrap();
    }
}
