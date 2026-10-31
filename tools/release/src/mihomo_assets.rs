//! `mihomo-assets`: pins a Mihomo upstream release into `packages/meduza-np-mihomo-pkg/assets.json` (§3.3, §15) during
//! release preparation. Never "latest": the tag is named explicitly, must be a published non-prerelease release, and
//! every platform asset of `platformMapping` is downloaded and checked before it is written:
//!
//! - size and SHA-256 of the compressed asset match GitHub's release metadata (`digest`, when GitHub provides one);
//! - it decompresses (bounded) to an ELF for exactly that platform (machine, width, little-endian);
//! - the binary carries the tag string (Go embeds it via ldflags), and on a matching host `mihomo -v` reports it.
//!
//! The digest recorded is the one computed here from the downloaded bytes; mihomo-pkg verifies downloads against it.
//! Existing entries for other tags are kept (nodes may pin them), the tag's entry is replaced, newest first.

use std::io::Read;
use std::path::Path;

use meduza_np_mihomo_pkg::{elf_matches, parse_elf, sha256_file, Asset, AssetManifest, VersionAssets, MAX_BINARY, MAX_DOWNLOAD};
use serde_json::Value;

/// `owner/repo` of a `https://github.com/<owner>/<repo>/releases` upstream.
pub fn github_repo(upstream: &str) -> Result<String, String> {
    let rest = upstream.strip_prefix("https://github.com/").ok_or("upstream must be a https://github.com/<owner>/<repo> URL")?;
    let mut parts = rest.trim_end_matches('/').split('/');
    let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else { return Err("upstream has no owner/repo".into()) };
    let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if !ok(owner) || !ok(repo) {
        return Err(format!("unexpected upstream {upstream}"));
    }
    Ok(format!("{owner}/{repo}"))
}

/// Tags look like `v1.19.33`; anything else (`Prerelease-Alpha`, `latest`) is refused.
pub fn parse_tag(tag: &str) -> Result<Vec<u64>, String> {
    let nums = tag.strip_prefix('v').ok_or_else(|| format!("{tag} is not a vX.Y.Z release tag"))?;
    let v: Result<Vec<u64>, _> = nums.split('.').map(str::parse).collect();
    match v {
        Ok(v) if (2..=4).contains(&v.len()) => Ok(v),
        _ => Err(format!("{tag} is not a vX.Y.Z release tag")),
    }
}

/// One asset of the release document (`GET /repos/{repo}/releases/tags/{tag}`) for a platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub platform: String,
    pub name: String,
    pub url: String,
    pub size: u64,
    /// GitHub's `sha256:` digest without the prefix, if the release has one.
    pub digest: Option<String>,
}

pub fn plan(manifest: &Value, release: &Value, repo: &str, tag: &str) -> Result<Vec<Planned>, String> {
    parse_tag(tag)?;
    if release["tag_name"].as_str() != Some(tag) {
        return Err(format!("release document is for {:?}, not {tag}", release["tag_name"].as_str()));
    }
    if release["draft"].as_bool() != Some(false) || release["prerelease"].as_bool() != Some(false) {
        return Err(format!("{tag} is a draft or prerelease; only published releases are pinned"));
    }
    let mapping = manifest["platformMapping"].as_object().ok_or("assets.json has no platformMapping")?;
    if mapping.is_empty() {
        return Err("platformMapping is empty".into());
    }
    let assets = release["assets"].as_array().ok_or("release has no assets")?;
    let prefix = format!("https://github.com/{repo}/releases/download/{tag}/");
    let mut out = Vec::new();
    for (platform, template) in mapping {
        let name = template.as_str().ok_or("platformMapping values must be strings")?.replace("{tag}", tag);
        let a = assets
            .iter()
            .find(|a| a["name"].as_str() == Some(name.as_str()))
            .ok_or_else(|| format!("{tag} has no asset {name} for {platform}"))?;
        let url = a["browser_download_url"].as_str().ok_or_else(|| format!("{name}: no download URL"))?;
        if url != format!("{prefix}{name}") {
            return Err(format!("{name}: unexpected download URL {url}"));
        }
        let size =
            a["size"].as_u64().filter(|s| *s > 0 && *s <= MAX_DOWNLOAD).ok_or_else(|| format!("{name}: size missing or too large"))?;
        let digest = match a["digest"].as_str() {
            None => None,
            Some(d) => {
                let hex = d.strip_prefix("sha256:").ok_or_else(|| format!("{name}: unsupported digest {d}"))?;
                if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!("{name}: malformed digest {d}"));
                }
                Some(hex.to_ascii_lowercase())
            }
        };
        out.push(Planned { platform: platform.clone(), name, url: url.to_string(), size, digest });
    }
    Ok(out)
}

/// Version probe of a candidate binary (`mihomo -v`), returning the reported tag.
pub type Probe<'a> = &'a dyn Fn(&Path) -> Result<String, String>;

/// Checks a downloaded asset and returns the entry to record. `probe` runs the binary (only on a matching host).
pub fn verify(p: &Planned, tag: &str, gz: &Path, probe: Option<Probe<'_>>) -> Result<Asset, String> {
    let size = std::fs::metadata(gz).map_err(|e| format!("{}: {e}", gz.display()))?.len();
    if size != p.size {
        return Err(format!("{}: downloaded {size} bytes, release says {}", p.name, p.size));
    }
    let sha = sha256_file(gz)?;
    if let Some(d) = &p.digest {
        if *d != sha {
            return Err(format!("{}: SHA-256 {sha} does not match the release digest {d}", p.name));
        }
    }
    let mut bin = Vec::new();
    let file = std::fs::File::open(gz).map_err(|e| e.to_string())?;
    flate2::read::GzDecoder::new(file).take(MAX_BINARY + 1).read_to_end(&mut bin).map_err(|e| format!("{}: decompress: {e}", p.name))?;
    if bin.len() as u64 > MAX_BINARY {
        return Err(format!("{}: decompressed binary too large", p.name));
    }
    let info = parse_elf(&bin).ok_or_else(|| format!("{}: not an ELF executable", p.name))?;
    elf_matches(info, &p.platform).map_err(|e| format!("{}: {e}", p.name))?;
    if !bin.windows(tag.len()).any(|w| w == tag.as_bytes()) {
        return Err(format!("{}: the binary does not contain its version {tag}", p.name));
    }
    if let Some(probe) = probe {
        let dir = gz.parent().unwrap_or(Path::new("."));
        let exe = dir.join(format!("mihomo-probe-{}", p.platform.replace('/', "-")));
        std::fs::write(&exe, &bin).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        }
        let got = probe(&exe);
        let _ = std::fs::remove_file(&exe);
        let got = got.map_err(|e| format!("{}: {e}", p.name))?;
        if got != tag {
            return Err(format!("{}: binary reports {got}, expected {tag}", p.name));
        }
    }
    Ok(Asset { platform: p.platform.clone(), name: p.name.clone(), url: p.url.clone(), sha256: sha, size })
}

/// Records `entry` in the manifest document (other keys such as `platformMapping` are kept), newest tag first.
/// `keep` limits the number of pinned tags; older ones are dropped.
pub fn merge(manifest: &Value, entry: VersionAssets, keep: Option<usize>) -> Result<Value, String> {
    let typed: AssetManifest = serde_json::from_value(manifest.clone()).map_err(|e| format!("assets.json: {e}"))?;
    let mut versions: Vec<VersionAssets> = typed.versions.into_iter().filter(|v| v.tag != entry.tag).collect();
    versions.push(entry);
    let mut keyed = Vec::new();
    for v in versions {
        keyed.push((parse_tag(&v.tag)?, v));
    }
    keyed.sort_by(|a, b| b.0.cmp(&a.0));
    let mut versions: Vec<VersionAssets> = keyed.into_iter().map(|(_, v)| v).collect();
    if let Some(k) = keep {
        versions.truncate(k.max(1));
    }
    let mut out = manifest.clone();
    out["versions"] = serde_json::to_value(&versions).map_err(|e| e.to_string())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn manifest() -> Value {
        json!({
            "formatVersion": 1,
            "upstream": "https://github.com/MetaCubeX/mihomo/releases",
            "platformMapping": {"linux/amd64": "mihomo-linux-amd64-compatible-{tag}.gz", "linux/arm/v7": "mihomo-linux-armv7-{tag}.gz"},
            "versions": []
        })
    }

    fn release(tag: &str, prerelease: bool) -> Value {
        let asset = |name: String, digest: Value| {
            json!({"name": name, "size": 100, "digest": digest,
                   "browser_download_url": format!("https://github.com/MetaCubeX/mihomo/releases/download/{tag}/{name}")})
        };
        json!({"tag_name": tag, "draft": false, "prerelease": prerelease, "assets": [
            asset(format!("mihomo-linux-amd64-compatible-{tag}.gz"), json!(format!("sha256:{}", "AB".repeat(32)))),
            asset(format!("mihomo-linux-amd64-{tag}.gz"), Value::Null),
            asset(format!("mihomo-linux-armv7-{tag}.gz"), Value::Null),
        ]})
    }

    /// Minimal ELF header for `machine` followed by the version string, gzip-compressed.
    fn fake_gz(dir: &Path, machine: u16, class64: bool, body: &str) -> std::path::PathBuf {
        let mut elf = vec![0x7f, b'E', b'L', b'F', if class64 { 2 } else { 1 }, 1, 1];
        elf.resize(18, 0);
        elf.extend_from_slice(&machine.to_le_bytes());
        elf.resize(64, 0);
        elf.extend_from_slice(body.as_bytes());
        let p = dir.join(format!("asset-{machine}.gz"));
        let mut enc = flate2::write::GzEncoder::new(std::fs::File::create(&p).unwrap(), flate2::Compression::default());
        enc.write_all(&elf).unwrap();
        enc.finish().unwrap();
        p
    }

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mnp-mihomo-assets-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn repo_and_tags() {
        assert_eq!(github_repo("https://github.com/MetaCubeX/mihomo/releases").unwrap(), "MetaCubeX/mihomo");
        assert!(github_repo("https://example.com/x/y").is_err());
        assert_eq!(parse_tag("v1.19.33").unwrap(), vec![1, 19, 33]);
        for bad in ["Prerelease-Alpha", "latest", "v1", "1.19.0", "v1.x.0"] {
            assert!(parse_tag(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn plan_maps_every_platform_and_refuses_prereleases() {
        let p = plan(&manifest(), &release("v1.19.33", false), "MetaCubeX/mihomo", "v1.19.33").unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].platform, "linux/amd64");
        assert_eq!(p[0].name, "mihomo-linux-amd64-compatible-v1.19.33.gz");
        assert_eq!(p[0].digest.as_deref(), Some("ab".repeat(32).as_str()));
        assert_eq!(p[1].digest, None);
        assert!(plan(&manifest(), &release("v1.19.33", true), "MetaCubeX/mihomo", "v1.19.33").unwrap_err().contains("prerelease"));
        assert!(plan(&manifest(), &release("v1.19.33", false), "MetaCubeX/mihomo", "v1.19.32").is_err());
        let mut r = release("v1.19.33", false);
        r["assets"][0]["browser_download_url"] = json!("https://evil.example/x.gz");
        assert!(plan(&manifest(), &r, "MetaCubeX/mihomo", "v1.19.33").unwrap_err().contains("unexpected download URL"));
        let mut r = release("v1.19.33", false);
        r["assets"].as_array_mut().unwrap().remove(2);
        assert!(plan(&manifest(), &r, "MetaCubeX/mihomo", "v1.19.33").unwrap_err().contains("no asset"));
    }

    #[test]
    fn verify_checks_digest_size_architecture_and_version() {
        let d = tmpdir("verify");
        let gz = fake_gz(&d, 0x3e, true, "Mihomo Meta v1.19.33 linux amd64");
        let size = std::fs::metadata(&gz).unwrap().len();
        let sha = sha256_file(&gz).unwrap();
        let p = Planned {
            platform: "linux/amd64".into(),
            name: "a.gz".into(),
            url: "https://github.com/MetaCubeX/mihomo/releases/download/v1.19.33/a.gz".into(),
            size,
            digest: Some(sha.clone()),
        };
        let a = verify(&p, "v1.19.33", &gz, None).unwrap();
        assert_eq!((a.sha256.as_str(), a.size), (sha.as_str(), size));
        let probe_ok = |_: &Path| Ok("v1.19.33".to_string());
        assert!(verify(&p, "v1.19.33", &gz, Some(&probe_ok)).is_ok());
        let probe_old = |_: &Path| Ok("v1.19.32".to_string());
        assert!(verify(&p, "v1.19.33", &gz, Some(&probe_old)).unwrap_err().contains("reports v1.19.32"));
        assert!(verify(&p, "v1.19.34", &gz, None).unwrap_err().contains("does not contain"));
        assert!(verify(&Planned { digest: Some("00".repeat(32)), ..p.clone() }, "v1.19.33", &gz, None)
            .unwrap_err()
            .contains("does not match"));
        assert!(verify(&Planned { size: size + 1, ..p.clone() }, "v1.19.33", &gz, None).unwrap_err().contains("bytes"));
        assert!(verify(&Planned { platform: "linux/arm/v7".into(), ..p }, "v1.19.33", &gz, None).unwrap_err().contains("ELF machine"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn merge_replaces_the_tag_sorts_and_keeps_other_keys() {
        let entry = |tag: &str, sha: &str| VersionAssets {
            tag: tag.into(),
            assets: vec![Asset { platform: "linux/amd64".into(), name: "n".into(), url: "u".into(), sha256: sha.into(), size: 1 }],
        };
        let m = merge(&manifest(), entry("v1.19.9", "a"), None).unwrap();
        let m = merge(&m, entry("v1.19.33", "b"), None).unwrap();
        let m = merge(&m, entry("v1.19.9", "c"), None).unwrap();
        let tags: Vec<&str> = m["versions"].as_array().unwrap().iter().map(|v| v["tag"].as_str().unwrap()).collect();
        assert_eq!(tags, ["v1.19.33", "v1.19.9"]);
        assert_eq!(m["versions"][1]["assets"][0]["sha256"], "c");
        assert_eq!(m["platformMapping"], manifest()["platformMapping"]);
        let kept = merge(&m, entry("v1.20.0", "d"), Some(2)).unwrap();
        assert_eq!(kept["versions"].as_array().unwrap().len(), 2);
        assert_eq!(kept["versions"][0]["tag"], "v1.20.0");
        let typed: AssetManifest = serde_json::from_value(kept).unwrap();
        assert_eq!(typed.versions[1].tag, "v1.19.33");
    }
}
