//! Bounded downloads for plugins (rule files, subscriptions, provider binaries) using the system `curl`
//! (preferred) or `wget`: timeout, size cap, http(s) only, optional proxy, streaming straight to a temp file.
//! Callers validate the temp file and atomically rename it; nothing is kept in memory.

use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct FetchOptions<'a> {
    pub timeout_seconds: u32,
    pub max_bytes: u64,
    pub proxy: Option<&'a str>,
    /// Extra request header (e.g. `Accept: application/octet-stream`).
    pub header: Option<&'a str>,
}

pub fn fetch_to_file(url: &str, dest: &Path, opts: &FetchOptions<'_>) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) || url.contains([' ', '\n', '"', '\'']) {
        return Err("only plain http(s) URLs are fetched".into());
    }
    let t = opts.timeout_seconds.to_string();
    let max = opts.max_bytes.to_string();
    let dest_s = dest.to_string_lossy().into_owned();
    let curl = ["/usr/bin/curl", "/bin/curl", "/usr/local/bin/curl"].into_iter().find(|p| Path::new(p).exists());
    let out = if let Some(curl) = curl {
        let mut args: Vec<&str> =
            vec!["-fsSL", "--max-time", &t, "--max-filesize", &max, "--proto", "=http,https", "--proto-redir", "=https", "-o", &dest_s];
        if let Some(p) = opts.proxy {
            args.extend(["-x", p]);
        }
        if let Some(h) = opts.header {
            args.extend(["-H", h]);
        }
        args.push(url);
        Command::new(curl).args(&args).stdin(Stdio::null()).output()
    } else {
        let mut cmd = Command::new("wget");
        cmd.args(["-q", "-T", &t, "-O", &dest_s]);
        if let Some(h) = opts.header {
            cmd.args(["--header", h]);
        }
        cmd.arg(url).stdin(Stdio::null());
        if let Some(p) = opts.proxy {
            cmd.env("http_proxy", p).env("https_proxy", p);
        }
        cmd.output()
    }
    .map_err(|e| format!("no downloader available (curl/wget): {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(dest);
        return Err(String::from_utf8_lossy(&out.stderr).trim().chars().take(300).collect::<String>().replace('\n', " "));
    }
    let size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    if size > opts.max_bytes {
        let _ = std::fs::remove_file(dest);
        return Err(format!("download exceeds {} bytes", opts.max_bytes));
    }
    Ok(())
}
