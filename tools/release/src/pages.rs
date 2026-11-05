//! Publish full non-OpenWrt archives and signed 500 KB parts before signing the group.
use std::io::{Read, Write};
use std::path::Path;

use meduza_np_protocol::group::{Artifact, ArtifactChunk, ARTIFACT_CHUNK_BYTES};

pub fn publish(artifact: &mut Artifact, artifacts: &Path, site: &Path, base: &str) -> Result<(), String> {
    if matches!(artifact.format.as_str(), "ipk" | "apk" | "oci") || artifact.platform.starts_with("openwrt/") {
        artifact.chunks.clear();
        return Ok(());
    }
    let filename =
        artifact.url.rsplit('/').next().filter(|v| !v.is_empty() && *v != "." && *v != "..").ok_or("missing artifact filename")?;
    if !filename.bytes().all(|v| v.is_ascii_alphanumeric() || b"._+-".contains(&v)) {
        return Err("invalid artifact filename".into());
    }
    let source = artifacts.join(filename);
    if artifact.size == 0 {
        return Err("empty archive cannot be published".into());
    }
    meduza_np_updater::store::verify_file(&source, artifact.size, &artifact.sha256)?;
    let relative = format!("packages/{}", artifact.sha256);
    let directory = site.join(&relative);
    std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let full = directory.join(filename);
    if full.exists() {
        meduza_np_updater::store::verify_file(&full, artifact.size, &artifact.sha256)?;
    } else {
        std::fs::copy(&source, &full).map_err(|e| e.to_string())?;
    }
    let mut input = std::fs::File::open(&source).map_err(|e| e.to_string())?;
    let mut chunks = Vec::new();
    let mut remaining = artifact.size;
    while remaining > 0 {
        let size = remaining.min(ARTIFACT_CHUNK_BYTES);
        let mut buffer = vec![0; size as usize];
        input.read_exact(&mut buffer).map_err(|e| e.to_string())?;
        let sha256 = meduza_np_protocol::digest::sha256_hex(&buffer);
        let part = format!("{:06}.part", chunks.len());
        let path = directory.join(&part);
        if path.exists() {
            meduza_np_updater::store::verify_file(&path, size, &sha256)?;
        } else {
            let mut file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
            file.write_all(&buffer).map_err(|e| e.to_string())?;
        }
        chunks.push(ArtifactChunk { url: format!("{relative}/{part}"), size, sha256 });
        remaining -= size;
    }
    artifact.url = format!("{}/{relative}/{filename}", base.trim_end_matches('/'));
    artifact.chunks = chunks;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_archive_and_parts_are_deterministic_reused_and_exclude_openwrt() {
        let root = std::env::temp_dir().join(format!("mnp-release-pages-{}", std::process::id()));
        let dist = root.join("dist");
        let site = root.join("site");
        std::fs::create_dir_all(&dist).unwrap();
        let body: Vec<_> = (0..1_000_017).map(|i| (i % 251) as u8).collect();
        std::fs::write(dist.join("pkg.tar.gz"), &body).unwrap();
        let mut artifact = Artifact {
            platform: "linux/amd64".into(),
            format: "tar.gz".into(),
            url: "https://github.com/o/r/releases/download/v1/pkg.tar.gz".into(),
            size: body.len() as u64,
            sha256: meduza_np_protocol::digest::sha256_hex(&body),
            oci: None,
            chunks: vec![],
        };
        publish(&mut artifact, &dist, &site, "https://pages.example/project").unwrap();
        meduza_np_updater::chunks::validate(&artifact).unwrap();
        assert_eq!(artifact.chunks.iter().map(|c| c.size).collect::<Vec<_>>(), [500_000, 500_000, 17]);
        let mut reassembled = Vec::new();
        for part in &artifact.chunks {
            reassembled.extend(std::fs::read(site.join(&part.url)).unwrap());
        }
        assert_eq!(body, reassembled);
        let full = site.join(artifact.url.strip_prefix("https://pages.example/project/").unwrap());
        assert_eq!(std::fs::read(full).unwrap(), body);
        let previous = artifact.clone();
        publish(&mut artifact, &dist, &site, "https://pages.example/project").unwrap();
        assert_eq!(artifact, previous, "unchanged packages share their content-addressed files");
        let mut native = artifact.clone();
        native.format = "ipk".into();
        native.platform = "openwrt/test".into();
        native.url = "https://github.com/o/r/releases/download/v1/pkg.ipk".into();
        publish(&mut native, &dist, &site, "https://pages.example/project").unwrap();
        assert!(native.chunks.is_empty());
        assert!(native.url.ends_with("/pkg.ipk"));
        std::fs::write(site.join(&artifact.chunks[0].url), b"corrupt").unwrap();
        assert!(publish(&mut artifact, &dist, &site, "https://pages.example/project").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
