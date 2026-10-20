//! Workspace resource inventory, dependency-aware release and recoverable permanent cleanup.
#![allow(missing_docs)]
mod cleanup;
mod dependencies;
mod history;
mod inventory;
mod json;
mod members;
mod migration;
mod progress;
mod types;

use crate::{Result, RunRecord, WorkflowOutcome, Workspace, atomic_json};
use analytics_core::task::TaskContext;

pub use cleanup::apply;
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
    Ok(serde_json::from_reader(std::io::BufReader::with_capacity(
        64 * 1024,
        fs::File::open(path)?,
    ))?)
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
    let value: serde_json::Value = read(&owned_path(
        w,
        Path::new(".jfquant/storage/inventory.json"),
    )?)?;
    if value["schema_version"] != 3 {
        return Err("storage_inventory_format_or_workspace_mismatch: 请重新扫描".into());
    }
    let inventory: StorageInventory = serde_json::from_value(value)?;
    if inventory.workspace != w.root() {
        return Err("storage_inventory_format_or_workspace_mismatch: 请重新扫描".into());
    }
    Ok(inventory)
}
/// Lightweight in-progress or last scan snapshot; absence is a normal empty state.
pub fn snapshot(w: &Workspace) -> Result<Option<ScanSnapshot>> {
    let path = owned_path(w, Path::new(".jfquant/storage/snapshot.json"))?;
    if !path.exists() {
        return Ok(None);
    }
    let snapshot: ScanSnapshot = read(&path)?;
    if snapshot.schema_version != 1 || snapshot.workspace != w.root() {
        return Err("storage_snapshot_format_or_workspace_mismatch: 请重新扫描".into());
    }
    Ok(Some(snapshot))
}
/// Called under the startup lease after interrupted task records have been recovered.
/// Progress is a rebuildable cache; an invalid cache remains an explicit read error,
/// and must not prevent recovery of authoritative cleanup transactions.
pub(crate) fn recover_snapshot(w: &Workspace) -> Result<()> {
    let Ok(Some(mut snapshot)) = snapshot(w) else {
        return Ok(());
    };
    if !snapshot.finished {
        snapshot.finished = true;
        snapshot.complete = false;
        snapshot.phase = "扫描中断，请重新扫描".into();
        snapshot.updated_at = crate::now();
        snapshot
            .diagnostics
            .push("服务在扫描完成前退出，进度仅为部分统计".into());
        atomic_json(
            &owned_path(w, Path::new(".jfquant/storage/snapshot.json"))?,
            &snapshot,
        )?;
    }
    Ok(())
}
fn save_inventory(w: &Workspace, mut inventory: StorageInventory) -> Result<StorageInventory> {
    let index = analytics_storage::index::Index::open(&owned_path(
        w,
        Path::new(".jfquant/storage/index.sqlite"),
    )?)?;
    index.complete(&inventory.scan_id.to_string())?;
    let obsolete = owned_path(w, Path::new(".jfquant/storage/files.json"))?;
    if obsolete.try_exists()? {
        fs::remove_file(obsolete)?;
    }
    inventory.files.clear();
    atomic_json(
        &owned_path(w, Path::new(".jfquant/storage/inventory.json"))?,
        &inventory,
    )?;
    index.prune(&inventory.scan_id.to_string())?;
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
/// Durable migration results, including pending deletion and recovery failures.
pub fn migration_history(w: &Workspace) -> Result<Vec<MigrationResult>> {
    migration::history(w)
}
pub fn execute(
    w: &Workspace,
    request: &StorageRequest,
    _run: &RunRecord,
    context: &TaskContext,
) -> Result<WorkflowOutcome> {
    let details = match request {
        StorageRequest::MigrationPlan { resources } => {
            let plan = migration::plan(w, resources, context)?;
            serde_json::to_value(MigrationPreview::from(&plan))?
        }
        StorageRequest::MigrationApply { plan } => {
            let result = migration::apply(w, *plan, context)?;
            if !result.errors.is_empty() {
                return Err("storage_migration_pending: 请重试同一迁移计划".into());
            }
            serde_json::to_value(result)?
        }
        StorageRequest::Export { resource, output } => {
            migration::export(w, resource, output, context)?
        }
        StorageRequest::Scan { level, threads } => {
            if !(1..=8).contains(threads) {
                return Err("storage_invalid_threads: expected 1..8".into());
            }
            let reporter = progress::Reporter::new(w, context, *level)?;
            let inventory =
                inventory::scan_with(w, context, *level, *threads, false, Some(&reporter))?;
            context.check()?;
            reporter.phase("保存扫描摘要");
            let inventory = save_inventory(w, inventory)?;
            reporter.finish(&inventory)?;
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
            if result.committed
                && let Ok(mut inventory) = latest(w)
            {
                inventory.stale = true;
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
    let reporter = progress::Reporter::new(w, context, ScanLevel::Resources)?;
    let inventory = inventory::scan_with(
        w,
        context,
        ScanLevel::Resources,
        default_threads(),
        true,
        Some(&reporter),
    )?;
    reporter.finish(&inventory)?;
    dependencies::build_plan(w, &inventory, resources, Uuid::new_v4())
}

#[cfg(test)]
mod tests;

/// Recover authoritative cleanup and migration journals before accepting work.
pub fn recover(w: &Workspace) -> Result<()> {
    migration::recover(w)?;
    cleanup::recover(w)
}
pub(crate) fn publish_evidence(
    w: &Workspace,
    manifest: &Path,
    data: Option<&crate::DataVersion>,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<()> {
    migration::published(w, manifest, data, run, context)
}
pub(crate) fn archive_packages(root: &Path, source: &Path, destination: &Path) -> Result<()> {
    migration::archive_packages(root, source, destination)
}
pub(crate) fn committed_task(w: &Workspace, id: Uuid) -> bool {
    cleanup::committed_task(w, id) || migration::committed_task(w, id)
}
pub(super) fn mark_stale(w: &Workspace) -> Result<()> {
    if let Ok(mut inventory) = latest(w) {
        inventory.stale = true;
        atomic_json(
            &owned_path(w, Path::new(".jfquant/storage/inventory.json"))?,
            &inventory,
        )?;
    }
    Ok(())
}
/// SQL filtering of the last completed resource generation; never scans filesystem contents.
pub fn query_resources(
    w: &Workspace,
    scan: Uuid,
    query: &ResourceQuery,
) -> Result<Vec<StorageResource>> {
    let inventory = latest(w)?;
    if inventory.scan_id != scan {
        return Err("storage_scan_stale".into());
    }
    let index = analytics_storage::index::Index::open_read(&owned_path(
        w,
        Path::new(".jfquant/storage/index.sqlite"),
    )?)?;
    if !index.generation_exists(&scan.to_string())? {
        return Err("storage_index_rebuild_required: 请重新识别资源".into());
    }
    let category = query.category.map(|c| {
        serde_json::to_value(c)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    });
    let mut resources: Vec<StorageResource> = index.resources(
        &scan.to_string(),
        category.as_deref(),
        query.task.map(|t| t.to_string()).as_deref(),
        query.modified_before.as_deref(),
        query.newest,
    )?;
    resources.retain(|r| {
        query
            .search
            .as_ref()
            .is_none_or(|s| format!("{} {}", r.name, r.path.display()).contains(s))
            && match query.status.as_deref() {
                Some("archived") => r.archived,
                Some("temporary") => r.temporary,
                Some("released") => r.released,
                Some("deletable") => r.deletable,
                Some("protected") => !r.deletable && !r.released,
                _ => true,
            }
    });
    Ok(resources)
}
