//! Independently download and verify the public investment_data daily archive.
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read},
    path::{Component, Path},
    process::Command,
};

/// Source provenance published beside the immutable release asset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseManifest {
    /// GitHub release date/tag.
    pub release_tag: String,
    /// Last actual trading date in this release.
    pub target_trade_date: String,
    /// Archive byte count.
    pub archive_size_bytes: u64,
    /// Expected SHA-256, including sha256: prefix.
    pub archive_sha256: String,
    /// Additional upstream provenance retained without modification.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn failure(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn download(url: &str, path: &Path) -> io::Result<()> {
    analytics_core::task::TaskContext::current()
        .report(format!("下载 {url} → {}", path.display()))?;
    let output = crate::transport::curl(
        Command::new("/usr/bin/curl")
            .args([
                "--fail",
                "--http1.1",
                "--location",
                "--retry",
                "3",
                "--connect-timeout",
                "30",
                "--speed-limit",
                "1024",
                "--speed-time",
                "30",
                "--continue-at",
                "-",
                "--max-time",
                "3600",
                "--output",
            ])
            .arg(path)
            .arg(url),
    )?;
    let status = output.status;
    if !status.success() {
        return Err(failure(format!("download failed: {status}")));
    }
    Ok(())
}

/// Verify complete bytes before any archive content is used.
pub fn verify_archive(path: &Path, manifest: &ReleaseManifest) -> io::Result<()> {
    if fs::metadata(path)?.len() != manifest.archive_size_bytes {
        return Err(failure("archive size mismatch"));
    }
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut block = vec![0_u8; 1024 * 1024];
    loop {
        analytics_core::task::TaskContext::current().check()?;
        let n = file.read(&mut block)?;
        if n == 0 {
            break;
        }
        hasher.update(&block[..n]);
    }
    if format!("sha256:{:x}", hasher.finalize()) != manifest.archive_sha256 {
        return Err(failure("archive SHA-256 mismatch"));
    }
    Ok(())
}

/// Extract only regular files/directories underneath qlib_bin; refuse links and traversal.
pub fn extract_archive(path: &Path, output: &Path) -> io::Result<()> {
    let mut archive = tar::Archive::new(GzDecoder::new(fs::File::open(path)?));
    let mut seen = BTreeSet::new();
    let mut total = 0_u64;
    for entry in archive.entries()? {
        analytics_core::task::TaskContext::current().check()?;
        let mut entry = entry?;
        let name = entry.path()?.into_owned();
        if !name.starts_with("qlib_bin")
            || name
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            || !seen.insert(name.clone())
            || !(entry.header().entry_type().is_file() || entry.header().entry_type().is_dir())
        {
            return Err(failure(format!(
                "unsafe archive member: {}",
                name.display()
            )));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| failure("archive overflow"))?;
        if total > 8 * 1024 * 1024 * 1024 {
            return Err(failure("archive expansion exceeds 8 GiB"));
        }
        if !entry.unpack_in(output)? {
            return Err(failure("archive member outside output"));
        }
    }
    for required in [
        "calendars/day.txt",
        "instruments/csi500.txt",
        "instruments/all.txt",
    ] {
        if !output.join("qlib_bin").join(required).is_file() {
            return Err(failure(format!("missing archive member: {required}")));
        }
    }
    Ok(())
}

/// Download fresh bytes from a pinned GitHub release into a new JFQuant directory.
/// Refuses an existing destination. Interrupted downloads remain in a staging
/// directory for diagnosis, and are never presented as a validated dataset.
pub fn fetch_release(tag: &str, output: &Path) -> io::Result<ReleaseManifest> {
    chrono::NaiveDate::parse_from_str(tag, "%Y-%m-%d")
        .map_err(|error| failure(error.to_string()))?;
    if output.exists() {
        return Err(failure(
            "destination already exists; choose a new dataset directory",
        ));
    }
    let staging = output.with_extension(format!("downloading-{}", std::process::id()));
    fs::create_dir_all(staging.parent().unwrap_or(Path::new(".")))?;
    fs::create_dir(&staging)?;
    let base = format!("https://github.com/chenditc/investment_data/releases/download/{tag}");
    let manifest_path = staging.join("qlib_bin.manifest.json");
    download(&format!("{base}/qlib_bin.manifest.json"), &manifest_path)?;
    let manifest: ReleaseManifest = serde_json::from_slice(&fs::read(&manifest_path)?)
        .map_err(|error| failure(error.to_string()))?;
    if manifest.release_tag != tag || !manifest.archive_sha256.starts_with("sha256:") {
        return Err(failure("release manifest mismatch"));
    }
    let archive_path = staging.join("qlib_bin.tar.gz");
    download(&format!("{base}/qlib_bin.tar.gz"), &archive_path)?;
    complete_release(&staging, output)?;
    Ok(manifest)
}

/// Finish a JFQuant-owned staging directory after resuming an interrupted download.
pub fn complete_release(staging: &Path, output: &Path) -> io::Result<ReleaseManifest> {
    if output.exists() {
        return Err(failure("destination already exists"));
    }
    let manifest: ReleaseManifest =
        serde_json::from_slice(&fs::read(staging.join("qlib_bin.manifest.json"))?)
            .map_err(|error| failure(error.to_string()))?;
    let base = format!(
        "https://github.com/chenditc/investment_data/releases/download/{}",
        manifest.release_tag
    );
    let archive_path = staging.join("qlib_bin.tar.gz");
    verify_archive(&archive_path, &manifest)?;
    eprintln!("Verified SHA-256; extracting archive");
    extract_archive(&archive_path, staging)?;
    let calendar = fs::read_to_string(staging.join("qlib_bin/calendars/day.txt"))?;
    if calendar.lines().last() != Some(manifest.target_trade_date.as_str()) {
        return Err(failure(
            "calendar disagrees with manifest target trade date",
        ));
    }
    fs::write(
        staging.join("jfquant_source.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "downloader":"JFQuant analytics-storage", "release_url":base,
            "archive_verified":true, "independent_download":true,
            "manifest": manifest,
        }))
        .map_err(|error| failure(error.to_string()))?,
    )?;
    fs::rename(staging, output)?;
    Ok(manifest)
}

/// Resume only a JFQuant-owned fixed-release staging directory; never publish partial data.
pub fn fetch_release_resumable(
    tag: &str,
    staging: &Path,
    output: &Path,
) -> io::Result<ReleaseManifest> {
    tag.parse::<chrono::NaiveDate>().map_err(io::Error::other)?;
    if output.exists() {
        return Err(failure("destination already exists"));
    }
    let owner =
        serde_json::json!({"schema_version":1,"downloader":"JFQuant analytics-storage","tag":tag});
    if staging.exists() {
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(staging.join("owner.json"))?)
                .map_err(io::Error::other)?;
        if saved != owner {
            return Err(failure("staging owner/tag mismatch"));
        }
    } else {
        fs::create_dir_all(staging)?;
        fs::write(
            staging.join("owner.json"),
            serde_json::to_vec(&owner).map_err(io::Error::other)?,
        )?;
    }
    let base = format!("https://github.com/chenditc/investment_data/releases/download/{tag}");
    let manifest_path = staging.join("qlib_bin.manifest.json");
    let saved = fs::read(&manifest_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<ReleaseManifest>(&b).ok());
    let manifest = if let Some(m) = saved {
        m
    } else {
        if manifest_path.exists() {
            fs::remove_file(&manifest_path)?;
        }
        download(&format!("{base}/qlib_bin.manifest.json"), &manifest_path)?;
        serde_json::from_slice(&fs::read(&manifest_path)?).map_err(io::Error::other)?
    };
    if manifest.release_tag != tag || manifest.target_trade_date != tag {
        return Err(failure("manifest/tag/date mismatch"));
    }
    let archive = staging.join("qlib_bin.tar.gz");
    if verify_archive(&archive, &manifest).is_err() {
        download(&format!("{base}/qlib_bin.tar.gz"), &archive)?;
    }
    complete_release(staging, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn downloaded_archive_identity_is_checked_before_use() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asset");
        fs::write(&path, b"complete").unwrap();
        let mut manifest = ReleaseManifest {
            release_tag: "2026-10-12".into(),
            target_trade_date: "2026-10-12".into(),
            archive_size_bytes: 8,
            archive_sha256: format!("sha256:{:x}", Sha256::digest(b"complete")),
            extra: Default::default(),
        };
        verify_archive(&path, &manifest).unwrap();
        manifest.archive_size_bytes = 7;
        assert!(verify_archive(&path, &manifest).is_err());
        manifest.archive_size_bytes = 8;
        manifest.archive_sha256 = "sha256:wrong".into();
        assert!(verify_archive(&path, &manifest).is_err());
    }
    #[test]
    fn archive_links_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            fs::File::create(&path).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_link_name("/tmp/outside").unwrap();
        header.set_cksum();
        archive
            .append_data(&mut header, "qlib_bin/evil", io::empty())
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();
        assert!(extract_archive(&path, &dir.path().join("extract")).is_err());
    }
}
