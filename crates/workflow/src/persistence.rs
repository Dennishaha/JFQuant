//! Persistence.
use crate::Result;
use analytics_core::task::TaskContext;
use analytics_storage::corporate_actions::ActionBundle;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Write},
    path::Path,
};
use uuid::Uuid;

pub(crate) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}
pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
/// Hash canonical serialized parameters. Field order and factor order are preserved.
pub fn signature(value: &impl Serialize) -> Result<String> {
    Ok(format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(value)?))
    ))
}
// Hash actual extracted bytes as well as provenance. Paths and lengths frame
// each file so renaming, adding, truncating or editing a feature changes identity.
pub(crate) fn market_content_hash(root: &Path) -> Result<String> {
    Ok(analytics_storage::market_view::tree_hash(root)?)
}

pub(crate) fn verify_action_evidence(path: &Path, book: &ActionBundle) -> Result<()> {
    if book.raw_sha256.contains_key("source-evidence-sha256.json") {
        let root = path.parent().ok_or("actions parent missing")?;
        let expected: String =
            serde_json::from_slice(&fs::read(root.join("raw/source-evidence-sha256.json"))?)?;
        if expected != market_content_hash(&root.join("source-evidence"))? {
            return Err("action acquisition evidence changed".into());
        }
    }
    for (name, expected) in &book.raw_sha256 {
        TaskContext::current().check()?;
        if Path::new(name)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("invalid action evidence path".into());
        }
        let bytes = fs::read(
            path.parent()
                .ok_or("actions parent missing")?
                .join("raw")
                .join(name),
        )?;
        if format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(bytes))
        ) != *expected
        {
            return Err("action evidence hash mismatch".into());
        }
    }
    Ok(())
}
pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::create_dir_all(path.parent().ok_or("missing parent")?)?;
    let part = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)?;
    f.write_all(&serde_json::to_vec_pretty(value)?)?;
    f.sync_all()?;
    fs::rename(part, path)?;
    Ok(())
}
