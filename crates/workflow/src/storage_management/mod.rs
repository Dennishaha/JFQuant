//! Workspace resource inventory, dependency-aware release and recoverable permanent cleanup.
#![allow(missing_docs)]
mod cleanup;
mod dependencies;
mod history;
mod inventory;
mod members;
mod types;

use crate::{Result, RunRecord, WorkflowOutcome, Workspace, atomic_json};
use analytics_core::task::TaskContext;
pub(crate) use cleanup::committed_task;
pub use cleanup::{apply, recover};
pub use history::{availability, historical_view, require_available};
pub(crate) use history::{dataset_released, released_dataset_ids, require_path_available};
pub use inventory::scan;
use serde::de::DeserializeOwned;
use std::{
    fs,
    path::{Component, Path, PathBuf},
};
pub use types::*;
use uuid::Uuid;

pub(crate) fn read<T: DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub(crate) fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}
pub(crate) fn owned_path(w: &Workspace, path: &Path) -> Result<PathBuf> {
    if !safe_relative(path) {
        return Err("storage_unsafe_path".into());
    }
    let mut current = w.root().to_owned();
    for part in path.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_symlink() => return Err("storage_symlink_not_owned".into()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(current)
}
pub fn latest(w: &Workspace) -> Result<StorageInventory> {
    let inventory: StorageInventory = read(&owned_path(
        w,
        Path::new(".jfquant/storage/inventory.json"),
    )?)?;
    if inventory.schema_version != 1 || inventory.workspace != w.root() {
        return Err("storage_inventory_format_or_workspace_mismatch: 请重新扫描".into());
    }
    Ok(inventory)
}
pub fn cleanup_history(w: &Workspace) -> Result<Vec<CleanupResult>> {
    let root = owned_path(w, Path::new(".jfquant/storage/transactions"))?;
    if !root.exists() {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for e in fs::read_dir(root)? {
        let path = e?.path().join("journal.json");
        let path = owned_path(w, path.strip_prefix(w.root())?)?;
        if path.is_file() {
            let journal: cleanup::Journal = read(&path)?;
            cleanup::validate(w, &journal)?;
            out.push(journal.result);
        }
    }
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Ok(out)
}
pub fn execute(
    w: &Workspace,
    request: &StorageRequest,
    _run: &RunRecord,
    context: &TaskContext,
) -> Result<WorkflowOutcome> {
    let details = match request {
        StorageRequest::Scan => {
            let mut inventory = scan(w, context)?;
            atomic_json(
                &owned_path(w, Path::new(".jfquant/storage/inventory.json"))?,
                &inventory,
            )?;
            inventory.files.clear();
            serde_json::to_value(inventory)?
        }
        StorageRequest::Plan { resources } => {
            let plan = plan(w, resources, context)?;
            atomic_json(
                &owned_path(
                    w,
                    &PathBuf::from(format!(".jfquant/storage/plans/{}.json", plan.id)),
                )?,
                &plan,
            )?;
            serde_json::to_value(plan)?
        }
        StorageRequest::Apply { plan } => {
            let result = apply(w, *plan, context)?;
            if result.committed {
                let inventory = scan(w, context)?;
                atomic_json(
                    &owned_path(w, Path::new(".jfquant/storage/inventory.json"))?,
                    &inventory,
                )?;
            }
            if !result.errors.is_empty() {
                return Err(format!("storage_cleanup_pending: {} 个文件待重试，请查看 storage history 并重试相同计划", result.errors.len()).into());
            }
            serde_json::to_value(result)?
        }
    };
    Ok(WorkflowOutcome {
        data: None,
        details,
    })
}
pub fn plan(w: &Workspace, resources: &[String], context: &TaskContext) -> Result<CleanupPlan> {
    let inventory = scan(w, context)?;
    dependencies::build_plan(w, &inventory, resources, Uuid::new_v4())
}

#[cfg(test)]
mod tests;
