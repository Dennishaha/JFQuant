//! Private local provider files shared by the App/CLI, outside workspaces and research artifacts.
//! Why: internal signed/unsigned binaries must not need repeated OS-keychain authorization.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    schema_version: u32,
    provider: String,
    key: String,
}
fn provider_name(provider: &str) -> io::Result<&str> {
    match provider {
        "hithink" | "tdxaidata" => Ok(provider),
        _ => Err(io::Error::other("credential_provider_unsupported")),
    }
}
fn valid_key(key: &str) -> bool {
    (16..=2048).contains(&key.len())
        && key
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}
/// Machine-local storage, deliberately separate from workspaces, exports, models and Git.
pub fn directory() -> io::Result<PathBuf> {
    let home =
        std::env::var_os("HOME").ok_or_else(|| io::Error::other("credential_home_missing"))?;
    #[cfg(target_os = "macos")]
    let path = PathBuf::from(home).join("Library/Application Support/JFQuant/credentials");
    #[cfg(not(target_os = "macos"))]
    let path = PathBuf::from(home).join(".jfquant/credentials");
    Ok(path)
}
fn private_directory(root: &Path, create: bool) -> io::Result<()> {
    if create {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
    }
    let meta = fs::symlink_metadata(root).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            io::Error::other("credential_missing: configure the source locally")
        } else {
            io::Error::other("credential_store_unavailable")
        }
    })?;
    if !meta.is_dir()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "credential_directory_permissions: require an owned directory with mode 0700",
        ));
    }
    Ok(())
}
fn read_at(root: &Path, provider: &str) -> io::Result<String> {
    provider_name(provider)?;
    private_directory(root, false)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(root.join(format!("{provider}.json")))
        .map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                io::Error::other("credential_missing: configure the source locally")
            } else {
                io::Error::other("credential_read_failed")
            }
        })?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.nlink() != 1
        || meta.permissions().mode() & 0o077 != 0
        || meta.len() > 4096
    {
        return Err(io::Error::other(
            "credential_file_permissions: require an owned regular file with mode 0600",
        ));
    }
    let mut bytes = vec![];
    file.take(4097).read_to_end(&mut bytes)?;
    let value: Credential =
        serde_json::from_slice(&bytes).map_err(|_| io::Error::other("credential_file_invalid"))?;
    if value.schema_version != 1 || value.provider != provider || !valid_key(&value.key) {
        return Err(io::Error::other("credential_file_invalid"));
    }
    Ok(value.key)
}
fn save_at(root: &Path, provider: &str, key: &str) -> io::Result<()> {
    provider_name(provider)?;
    if !valid_key(key) {
        return Err(io::Error::other("invalid source credential"));
    }
    private_directory(root, true)?;
    let target = root.join(format!("{provider}.json"));
    if let Ok(meta) = fs::symlink_metadata(&target)
        && (!meta.is_file()
            || meta.uid() != rustix::process::geteuid().as_raw()
            || meta.nlink() != 1)
    {
        return Err(io::Error::other("unsafe_credential_file"));
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| io::Error::other("credential_clock_failed"))?
        .as_nanos();
    let temporary = root.join(format!(".{provider}-{}-{nonce}.tmp", std::process::id()));
    let result = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        let document = Credential {
            schema_version: 1,
            provider: provider.into(),
            key: key.into(),
        };
        file.write_all(
            &serde_json::to_vec(&document)
                .map_err(|_| io::Error::other("credential_write_failed"))?,
        )?;
        file.sync_all()?;
        fs::rename(&temporary, &target)?;
        fs::File::open(root)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
/// Atomically save an explicitly entered source Key in a 0600 file; never accepts arbitrary paths.
pub fn save_source(provider: &str, key: &str) -> io::Result<()> {
    save_at(&directory()?, provider, key)
}
/// Read the latest Key per task without an OS prompt. No keychain or old-format fallback.
pub fn load_source(provider: &str) -> io::Result<String> {
    read_at(&directory()?, provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn atomic_private_files_round_trip_without_losing_other_sources() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("credentials");
        save_at(&root, "tdxaidata", "fixture-private-tdx-key").unwrap();
        save_at(&root, "hithink", "fixture-private-hithink-key").unwrap();
        assert_eq!(
            read_at(&root, "tdxaidata").unwrap(),
            "fixture-private-tdx-key"
        );
        save_at(&root, "tdxaidata", "fixture-replacement-key").unwrap();
        assert_eq!(
            read_at(&root, "hithink").unwrap(),
            "fixture-private-hithink-key"
        );
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(root.join("tdxaidata.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
    }
    #[test]
    fn malformed_permissive_linked_and_unknown_files_are_rejected_without_exposing_keys() {
        let temp = tempfile::tempdir().unwrap();
        let root = &temp.path().join("credentials");
        assert!(save_at(root, "../escape", "fixture-private-key").is_err());
        assert!(save_at(root, "tdxaidata", "invalid\nkey").is_err());
        save_at(root, "tdxaidata", "fixture-private-tdx-key").unwrap();
        let file = root.join("tdxaidata.json");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_at(root, "tdxaidata").is_err());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&file, b"invalid: fixture-private-tdx-key").unwrap();
        assert!(
            !read_at(root, "tdxaidata")
                .unwrap_err()
                .to_string()
                .contains("fixture-private")
        );
        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(root.join("hithink.json"), &file).unwrap();
        assert!(save_at(root, "tdxaidata", "fixture-private-tdx-key").is_err());
        assert!(read_at(root, "tdxaidata").is_err());
    }
}
