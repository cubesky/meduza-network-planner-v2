//! Persistent, verified parts survive failed transactions; staging remains disposable.
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use meduza_np_protocol::group::{Artifact, ARTIFACT_CHUNK_BYTES};

use crate::meta::{MetaError, Source};

fn hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|v| v.is_ascii_hexdigit())
}

pub fn validate(artifact: &Artifact) -> Result<(), String> {
    if artifact.chunks.is_empty() {
        return Ok(());
    }
    if !matches!(artifact.format.as_str(), "tar.gz" | "tar.zst") || artifact.platform.starts_with("openwrt/") {
        return Err("only non-OpenWrt archive artifacts can contain chunks".into());
    }
    if artifact.size == 0 || !hash(&artifact.sha256) || artifact.chunks.len() as u64 != artifact.size.div_ceil(ARTIFACT_CHUNK_BYTES) {
        return Err("invalid chunk count or artifact digest".into());
    }
    let mut remaining = artifact.size;
    let mut urls = BTreeSet::new();
    for part in &artifact.chunks {
        if part.size != remaining.min(ARTIFACT_CHUNK_BYTES) || !hash(&part.sha256) || !urls.insert(&part.url) {
            return Err("invalid chunk size, digest or repeated URL".into());
        }
        remaining -= part.size;
    }
    Ok(())
}

fn directory(path: &Path) -> Result<(), String> {
    match std::fs::create_dir(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!("{}: redirected download cache directory", path.display()));
    }
    Ok(())
}

fn regular(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && !m.file_type().is_symlink())
}

pub fn download(
    source: &Source,
    artifact: &Artifact,
    dest: &Path,
    cache: &Path,
    mut notify: impl FnMut(crate::retry::Attempt),
) -> Result<(), String> {
    validate(artifact)?;
    if artifact.chunks.is_empty() {
        return Err("missing artifact chunks".into());
    }
    // Validate every URL before downloading any part, including when the matching cache already exists.
    let urls = artifact.chunks.iter().map(|part| source.resolve(&part.url).map_err(|e| e.to_string())).collect::<Result<Vec<_>, _>>()?;
    directory(cache)?;
    let root = cache.join(artifact.sha256.to_ascii_lowercase());
    directory(&root)?;
    let mut output = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    for (index, (part, url)) in artifact.chunks.iter().zip(urls).enumerate() {
        let path = root.join(format!("{index:06}.part"));
        if !regular(&path) || crate::store::verify_file(&path, part.size, &part.sha256).is_err() {
            let _ = std::fs::remove_file(&path);
            let tmp = root.join(format!("{index:06}.tmp"));
            let _ = std::fs::remove_file(&tmp);
            let opts = meduza_np_plugin_sdk::fetch::FetchOptions { timeout_seconds: 900, max_bytes: part.size, proxy: None, header: None };
            let result = crate::retry::download(
                || {
                    // The transport truncates a failed temporary file; only verified .part files are reusable.
                    source.fetch_once(&url, &tmp, &opts)?;
                    crate::store::verify_file(&tmp, part.size, &part.sha256).map_err(|e| MetaError { code: "digest-mismatch", message: e })
                },
                &mut notify,
            );
            if let Err(e) = result {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.to_string());
            }
            std::fs::File::open(&tmp).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
            meduza_np_runtime::fsutil::sync_dir(&root).map_err(|e| e.to_string())?;
        }
        let mut input = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
        output.flush().map_err(|e| e.to_string())?;
        notify(crate::retry::Attempt { number: 1, maximum: crate::retry::MAX_ATTEMPTS, retry_after: None, error: None });
    }
    output.sync_all().map_err(|e| e.to_string())?;
    crate::store::verify_file(dest, artifact.size, &artifact.sha256)
}

/// Only this updater's known part names are deleted. Unknown files and redirected directories are preserved.
pub fn retain(cache: &Path, keep: &BTreeSet<String>) -> Result<(), String> {
    if !cache.exists() {
        return Ok(());
    }
    directory(cache)?;
    for item in std::fs::read_dir(cache).map_err(|e| e.to_string())? {
        let item = item.map_err(|e| e.to_string())?;
        let digest = item.file_name().to_string_lossy().into_owned();
        if !hash(&digest) || keep.contains(&digest) || !item.file_type().map_err(|e| e.to_string())?.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(item.path()).map_err(|e| e.to_string())? {
            let file = file.map_err(|e| e.to_string())?;
            let name = file.file_name().to_string_lossy().into_owned();
            if name
                .split_once('.')
                .is_some_and(|(index, ext)| index.len() == 6 && index.bytes().all(|c| c.is_ascii_digit()) && matches!(ext, "part" | "tmp"))
            {
                let _ = std::fs::remove_file(file.path());
            }
        }
        let _ = std::fs::remove_dir(item.path());
    }
    Ok(())
}

pub fn discard_installed(cache: &Path, installed: &BTreeSet<String>) -> Result<(), String> {
    if !cache.exists() {
        return Ok(());
    }
    directory(cache)?;
    let keep = std::fs::read_dir(cache)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| !installed.contains(name))
        .collect();
    retain(cache, &keep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_protocol::group::ArtifactChunk;

    #[test]
    fn retries_reuse_verified_parts_repair_corruption_and_check_the_final_archive() {
        let root = std::env::temp_dir().join(format!("mnp-parts-{}", std::process::id()));
        let site = root.join("site");
        std::fs::create_dir_all(&site).unwrap();
        let source = Source::new(&format!("file:///{}", site.to_string_lossy().replace('\\', "/").trim_start_matches('/'))).unwrap();
        let body: Vec<_> = (0..1_000_017).map(|i| (i % 251) as u8).collect();
        let mut artifact = Artifact {
            platform: "linux/amd64".into(),
            format: "tar.gz".into(),
            url: "full.tar.gz".into(),
            size: body.len() as u64,
            sha256: meduza_np_protocol::digest::sha256_hex(&body),
            oci: None,
            chunks: Vec::new(),
        };
        for (i, part) in body.chunks(ARTIFACT_CHUNK_BYTES as usize).enumerate() {
            let name = format!("{i}.part");
            std::fs::write(site.join(&name), part).unwrap();
            artifact.chunks.push(ArtifactChunk {
                url: name,
                size: part.len() as u64,
                sha256: meduza_np_protocol::digest::sha256_hex(part),
            });
        }
        let cache = root.join("downloads");
        let dest = root.join("staging.tar.gz");
        std::fs::write(site.join("1.part"), vec![0; 500_000]).unwrap();
        assert!(download(&source, &artifact, &dest, &cache, |_| {}).unwrap_err().contains("digest-mismatch"));
        let saved = cache.join(&artifact.sha256);
        assert!(saved.join("000000.part").is_file());
        assert!(!saved.join("000001.tmp").exists());
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), 500_000);
        // A new process/pass can continue even if a previously fetched part is no longer reachable.
        std::fs::remove_file(site.join("0.part")).unwrap();
        std::fs::write(site.join("1.part"), &body[500_000..1_000_000]).unwrap();
        download(&source, &artifact, &dest, &cache, |_| {}).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        std::fs::write(saved.join("000001.part"), vec![0; 500_000]).unwrap();
        download(&source, &artifact, &dest, &cache, |_| {}).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let mut invalid = artifact.clone();
        invalid.chunks[0].size -= 1;
        assert!(validate(&invalid).is_err());
        invalid = artifact.clone();
        invalid.format = "ipk".into();
        assert!(validate(&invalid).is_err());
        std::fs::write(site.join("0.part"), &body[..500_000]).unwrap();
        invalid = artifact.clone();
        invalid.sha256 = "f".repeat(64);
        assert!(
            download(&source, &invalid, &dest, &cache, |_| {}).is_err(),
            "a correct part list cannot override the signed whole-file digest"
        );
        // New target manifests collect obsolete owned chunks while keeping unknown user files.
        std::fs::write(saved.join("unknown"), b"user file").unwrap();
        retain(&cache, &BTreeSet::new()).unwrap();
        assert!(!saved.join("000000.part").exists());
        assert!(saved.join("unknown").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn redirected_cache_directories_are_not_followed() {
        let root = std::env::temp_dir().join(format!("mnp-parts-links-{}", std::process::id()));
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("downloads")).unwrap();
        assert!(retain(&root.join("downloads"), &BTreeSet::new()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
