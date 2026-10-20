//! Offline, recoverable archival of derived runs. Original inputs are never deleted.
use crate::{Result, Workspace, atomic_json};
use analytics_storage::evidence_store::io_fs as fs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// Immutable receipt for one file in the maintenance inventory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileReceipt {
    /// Length in bytes.
    pub bytes: u64,
    /// SHA256 of the complete file contents.
    pub sha256: String,
}
/// Reviewable inventory. Applying maintenance rescans while holding the workspace lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenancePlan {
    /// Inventory format.
    pub schema_version: u32,
    /// Task directories to archive; original paths become relative evidence aliases.
    pub runs: Vec<String>,
    /// Workspace-relative file paths and integrity receipts, including retained inputs.
    pub files: BTreeMap<PathBuf, FileReceipt>,
    /// JSON string references to files in old runs, grouped by referring file.
    pub references: BTreeMap<PathBuf, Vec<String>>,
    /// Configuration or input problems that must be fixed before applying.
    pub blockers: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize)]
struct Journal {
    id: Uuid,
    complete: bool,
    plan: MaintenancePlan,
}
fn archive_entry(name: &str) -> bool {
    Uuid::parse_str(name).is_ok()
        || matches!(
            name,
            ".factor-shards-v1" | ".predictions-v1" | ".factor-artifacts-v2"
        )
}
fn root(path: &Path) -> Result<PathBuf> {
    let root = path.canonicalize()?;
    let marker: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".jfquant/workspace.json"))?)?;
    if marker["schema_version"] != 1 {
        return Err("unsupported workspace format".into());
    }
    Ok(root)
}
fn receipt(path: &Path) -> Result<FileReceipt> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0_u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        digest.update(&buffer[..count]);
    }
    Ok(FileReceipt {
        bytes,
        sha256: format!("sha256:{:x}", analytics_core::hash::Hex(digest.finalize())),
    })
}
fn references(value: &serde_json::Value, output: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) if s.contains(".jfquant/runs/") => output.push(s.clone()),
        serde_json::Value::Array(a) => a.iter().for_each(|v| references(v, output)),
        serde_json::Value::Object(o) => o.values().for_each(|v| references(v, output)),
        _ => {}
    }
}
fn inventory(root: &Path, path: &Path, plan: &mut MaintenancePlan) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let relative = path.strip_prefix(root)?.to_owned();
    if meta.file_type().is_symlink() {
        // Prior archive aliases are not active runs and must never be archived twice.
        if relative.parent() == Some(Path::new(".jfquant/runs")) {
            let target = path.canonicalize()?;
            if target.starts_with(root.join(".jfquant/archive")) {
                return Ok(());
            }
        }
        plan.blockers.push(format!(
            "symlink requires explicit input maintenance: {}",
            relative.display()
        ));
    } else if meta.is_dir() {
        let mut entries = fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        for entry in entries {
            inventory(root, &entry, plan)?;
        }
    } else if meta.is_file() {
        plan.files.insert(relative.clone(), receipt(path)?);
        if path.extension().is_some_and(|e| e == "json")
            && meta.len() <= 16 * 1024 * 1024
            && let Ok(value) = serde_json::from_slice(&fs::read(path)?)
        {
            let mut refs = Vec::new();
            references(&value, &mut refs);
            if !refs.is_empty() {
                plan.references.insert(relative, refs);
            }
        }
    } else {
        plan.blockers.push(format!(
            "unsupported filesystem entry: {}",
            relative.display()
        ));
    }
    Ok(())
}
/// Read-only preview; does not open/create a workspace or start the service.
pub fn preview(path: &Path) -> Result<MaintenancePlan> {
    let root = root(path)?;
    let mut plan = MaintenancePlan {
        schema_version: 1,
        runs: vec![],
        files: BTreeMap::new(),
        references: BTreeMap::new(),
        blockers: vec![],
    };
    for name in [
        "data",
        ".jfquant/runs",
        ".jfquant/staging",
        ".jfquant/templates",
        ".jfquant/pools",
        ".jfquant/catalog",
        ".jfquant/factors",
        ".jfquant/factor-groups",
    ] {
        let path = root.join(name);
        if path.exists() {
            inventory(&root, &path, &mut plan)?;
        }
    }
    for entry in fs::read_dir(root.join(".jfquant/runs"))? {
        let entry = entry?;
        if !entry.file_type()?.is_symlink() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() && archive_entry(&name) {
                plan.runs.push(name);
            } else {
                plan.blockers
                    .push(format!("unrecognized run entry: {name}"));
            }
        }
    }
    let templates = root.join(".jfquant/templates");
    for entry in fs::read_dir(templates)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Err(error) = crate::profiles::WorkflowDocument::read(&path)
        {
            plan.blockers.push(format!(
                "configuration {}: {error}; export a current configuration before maintenance",
                path.display()
            ));
        }
    }
    plan.runs.sort();
    Ok(plan)
}
/// Prevent normal service startup while a maintenance transaction is incomplete.
pub(crate) fn require_complete(root: &Path) -> Result<()> {
    crate::allocation_migration::require_complete(root)?;
    let path = root.join(".jfquant/maintenance.json");
    if path.exists() {
        let journal: Journal = serde_json::from_slice(&fs::read(path)?)?;
        if !journal.complete {
            return Err(
                "maintenance_incomplete: run workspace maintain --apply offline to resume".into(),
            );
        }
    }
    Ok(())
}
/// Archive derived runs offline, or resume the interrupted transaction. Never deletes inputs.
#[cfg(unix)]
pub fn apply(path: &Path) -> Result<serde_json::Value> {
    let root = root(path)?;
    let workspace = Workspace { root: root.clone() };
    let _lease = workspace.lease()?;
    crate::storage_management::recover(&workspace)?;
    if crate::storage_management::cleanup_history(&workspace)?
        .iter()
        .any(|r| r.committed && r.completed_at.is_none())
    {
        return Err("storage_cleanup_pending: 完成已有空间释放后再归档".into());
    }
    let journal_path = root.join(".jfquant/maintenance.json");
    let previous = if journal_path.exists() {
        Some(serde_json::from_slice::<Journal>(&fs::read(
            &journal_path,
        )?)?)
    } else {
        None
    };
    let journal = match previous {
        Some(journal) if !journal.complete => journal,
        _ => {
            let plan = preview(&root)?;
            if !plan.blockers.is_empty() {
                return Err(format!("maintenance blocked: {}", plan.blockers.join("; ")).into());
            }
            if plan.runs.is_empty() {
                return Ok(serde_json::json!({"changed":false,"archived_runs":0}));
            }
            let journal = Journal {
                id: Uuid::new_v4(),
                complete: false,
                plan,
            };
            atomic_json(&journal_path, &journal)?;
            journal
        }
    };
    finish(&root, &journal_path, journal)
}
#[cfg(unix)]
fn finish(root: &Path, journal_path: &Path, mut journal: Journal) -> Result<serde_json::Value> {
    if journal.plan.schema_version != 1
        || journal.plan.runs.iter().any(|id| !archive_entry(id))
        || journal.plan.files.keys().any(|p| {
            p.components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        })
    {
        return Err("invalid maintenance journal paths/version".into());
    }
    let archive = root.join(format!(".jfquant/archive/{}", journal.id));
    fs::create_dir_all(archive.join("runs"))?;
    // Verify all files before continuing, accounting for a crash between rename and alias creation.
    for (relative, expected) in &journal.plan.files {
        let archived = relative
            .strip_prefix(".jfquant/runs")
            .ok()
            .map(|p| archive.join("runs").join(p));
        let actual = archived
            .filter(|p| p.exists())
            .unwrap_or_else(|| root.join(relative));
        if receipt(&actual)? != *expected {
            return Err(format!("maintenance input changed: {}", relative.display()).into());
        }
    }
    for id in &journal.plan.runs {
        let source = root.join(".jfquant/runs").join(id);
        let destination = archive.join("runs").join(id);
        if !destination.exists() {
            fs::rename(&source, &destination)?;
        }
        crate::storage_management::archive_packages(
            root,
            source.strip_prefix(root)?,
            destination.strip_prefix(root)?,
        )?;
        if id.starts_with('.') {
            continue;
        }
        if !source.try_exists()? {
            std::os::unix::fs::symlink(format!("../archive/{}/runs/{id}", journal.id), &source)?;
        } else if !fs::symlink_metadata(&source)?.file_type().is_symlink()
            || source.canonicalize()? != destination.canonicalize()?
        {
            return Err("maintenance archive alias collision".into());
        }
    }
    // File aliases preserve evidence and source links; active run enumeration ignores aliases.
    atomic_json(&archive.join("manifest.json"), &journal.plan)?;
    journal.complete = true;
    atomic_json(journal_path, &journal)?;
    Ok(
        serde_json::json!({"changed":true,"archive":archive,"archived_runs":journal.plan.runs.iter().filter(|id| !id.starts_with('.')).count(),"rebuild":"factor caches, models, predictions and results"}),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Workspace, Uuid) {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let id = Uuid::new_v4();
        let run = w.begin(id, "evidence", serde_json::json!({})).unwrap();
        fs::write(
            w.resolve(run.output).join("evidence.json"),
            b"original evidence",
        )
        .unwrap();
        fs::write(w.root().join("data/raw.bin"), b"original market").unwrap();
        (temp, w, id)
    }
    #[test]
    fn archives_without_losing_evidence_and_is_repeatable() {
        let (_temp, w, id) = fixture();
        let plan = preview(w.root()).unwrap();
        assert_eq!(plan.runs, vec![id.to_string()]);
        assert!(!w.root().join(".jfquant/maintenance.json").exists());
        let lease = w.lease().unwrap();
        assert!(apply(w.root()).is_err());
        drop(lease);
        apply(w.root()).unwrap();
        assert!(w.runs().unwrap().is_empty());
        assert_eq!(
            fs::read(w.root().join(format!(".jfquant/runs/{id}/evidence.json"))).unwrap(),
            b"original evidence"
        );
        assert_eq!(
            fs::read(w.root().join("data/raw.bin")).unwrap(),
            b"original market"
        );
        assert_eq!(apply(w.root()).unwrap()["changed"], false);
    }
    #[test]
    fn shared_caches_are_archived_without_active_aliases() {
        let (_temp, w, _) = fixture();
        for name in [
            ".factor-shards-v1",
            ".predictions-v1",
            ".factor-artifacts-v2",
        ] {
            let path = w.root().join(".jfquant/runs").join(name);
            fs::create_dir(&path).unwrap();
            fs::write(path.join("cache.bin"), b"derived").unwrap();
        }
        let result = apply(w.root()).unwrap();
        let archive = Path::new(result["archive"].as_str().unwrap());
        for name in [
            ".factor-shards-v1",
            ".predictions-v1",
            ".factor-artifacts-v2",
        ] {
            assert!(!w.root().join(".jfquant/runs").join(name).exists());
            assert_eq!(
                fs::read(archive.join("runs").join(name).join("cache.bin")).unwrap(),
                b"derived"
            );
        }
        assert_eq!(apply(w.root()).unwrap()["changed"], false);
    }
    #[test]
    fn corrupted_journal_cannot_escape_the_workspace() {
        let (_temp, w, _) = fixture();
        let mut plan = preview(w.root()).unwrap();
        plan.runs.push("../../elsewhere".into());
        let journal = Journal {
            id: Uuid::new_v4(),
            complete: false,
            plan,
        };
        atomic_json(&w.root().join(".jfquant/maintenance.json"), &journal).unwrap();
        assert!(
            apply(w.root())
                .unwrap_err()
                .to_string()
                .contains("invalid maintenance journal")
        );
    }
    #[test]
    fn resumes_after_rename_and_rejects_modified_originals() {
        let (_temp, w, id) = fixture();
        let journal = Journal {
            id: Uuid::new_v4(),
            complete: false,
            plan: preview(w.root()).unwrap(),
        };
        let path = w.root().join(".jfquant/maintenance.json");
        atomic_json(&path, &journal).unwrap();
        assert!(require_complete(w.root()).is_err());
        let archive = w
            .root()
            .join(format!(".jfquant/archive/{}/runs", journal.id));
        fs::create_dir_all(&archive).unwrap();
        fs::rename(
            w.root().join(format!(".jfquant/runs/{id}")),
            archive.join(id.to_string()),
        )
        .unwrap();
        fs::write(w.root().join("data/raw.bin"), b"changed").unwrap();
        assert!(apply(w.root()).is_err());
        fs::write(w.root().join("data/raw.bin"), b"original market").unwrap();
        apply(w.root()).unwrap();
        require_complete(w.root()).unwrap();
        assert!(w.runs().unwrap().is_empty());
    }
}
