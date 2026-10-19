use super::*;
use analytics_storage::filesystem::{self, FileStamp};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::Ordering,
};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Journal {
    pub schema_version: u32,
    pub task: Uuid,
    pub plan: CleanupPlan,
    pub result: CleanupResult,
}
pub(crate) fn durable_write(path: &Path, value: &impl Serialize) -> Result<()> {
    atomic_json(path, value)?;
    // Persist newly created storage/transaction directories as well as the final rename.
    for directory in path.ancestors().skip(1) {
        fs::File::open(directory)?.sync_all()?;
        if directory.file_name().is_some_and(|name| name == ".jfquant") {
            fs::File::open(directory.parent().ok_or("missing workspace")?)?.sync_all()?;
            break;
        }
    }
    Ok(())
}
fn transaction(w: &Workspace, id: Uuid) -> Result<PathBuf> {
    owned_path(
        w,
        &PathBuf::from(format!(".jfquant/storage/transactions/{id}")),
    )
}
fn save(root: &Path, journal: &Journal) -> Result<()> {
    durable_write(&root.join("journal.json"), journal)?;
    durable_write(&root.join("result.json"), &journal.result)
}
pub(crate) fn validate(w: &Workspace, journal: &Journal) -> Result<()> {
    if journal.schema_version != 1
        || journal.plan.schema_version != 1
        || journal.result.schema_version != 1
        || journal.result.plan != journal.plan.id
        || crate::signature(&(
            &journal.plan.resources,
            &journal.plan.files,
            &journal.plan.metadata_fingerprint,
        ))? != journal.plan.fingerprint
    {
        return Err("storage_invalid_journal".into());
    }
    let ids: BTreeSet<_> = journal
        .plan
        .resources
        .iter()
        .map(|r| r.id.clone())
        .collect();
    let mut paths = BTreeSet::new();
    for file in &journal.plan.files {
        if !safe_relative(&file.path)
            || file.path.starts_with(".jfquant/storage")
            || file.path.file_name().is_some_and(|n| n == "run.json")
            || !(file.path.starts_with("data")
                || file.path.starts_with(".jfquant/runs")
                || file.path.starts_with(".jfquant/archive")
                || file.path.starts_with(".jfquant/staging"))
            || file.owners.is_empty()
            || !file.owners.iter().all(|id| ids.contains(id))
            || !paths.insert(&file.path)
        {
            return Err("storage_invalid_journal_path".into());
        }
        owned_path(w, &file.path)?;
    }
    Ok(())
}
fn same_file(path: &Path, stamp: &FileStamp) -> Result<()> {
    let actual = FileStamp::read(path)?;
    // rename changes ctime; identity, length and mtime must still match the staged file.
    if actual.device != stamp.device
        || actual.inode != stamp.inode
        || actual.bytes != stamp.bytes
        || actual.modified != stamp.modified
    {
        return Err("storage_staged_file_changed".into());
    }
    Ok(())
}
fn rollback(w: &Workspace, root: &Path, journal: &mut Journal) -> Result<()> {
    for (i, file) in journal.plan.files.iter().enumerate().rev() {
        let staged = owned_path(
            w,
            root.join("pending")
                .join(i.to_string())
                .strip_prefix(w.root())?,
        )?;
        if !staged.try_exists()? {
            continue;
        }
        same_file(&staged, &file.stamp)?;
        let original = owned_path(w, &file.path)?;
        if original.try_exists()? {
            return Err(format!("storage_rollback_collision:{}", file.path.display()).into());
        }
        fs::rename(&staged, &original)?;
        fs::File::open(original.parent().unwrap())?.sync_all()?;
        fs::File::open(staged.parent().unwrap())?.sync_all()?;
    }
    journal.result.status = "rolled_back".into();
    journal.result.completed_at = Some(crate::now());
    save(root, journal)
}
fn purge(w: &Workspace, root: &Path, journal: &mut Journal) -> Result<CleanupResult> {
    journal.result.errors.clear();
    let mut removed = vec![];
    for (i, file) in journal.plan.files.iter().enumerate() {
        let staged = owned_path(
            w,
            root.join("pending")
                .join(i.to_string())
                .strip_prefix(w.root())?,
        )?;
        let result = (|| -> Result<()> {
            if staged.try_exists()? {
                same_file(&staged, &file.stamp)?;
                fs::remove_file(&staged)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => removed.push(file.clone()),
            Err(e) => {
                journal
                    .result
                    .errors
                    .insert(file.path.clone(), e.to_string());
            }
        }
    }
    // Persist unlinks before the completion receipt: recovery skips completed journals.
    if let Err(error) = fs::File::open(root.join("pending")).and_then(|dir| dir.sync_all()) {
        journal.result.errors.insert(
            PathBuf::from("pending"),
            format!("storage_deletion_sync_failed: {error}"),
        );
    }
    let selected = journal
        .plan
        .resources
        .iter()
        .map(|r| r.id.clone())
        .collect();
    journal.result.deleted = super::dependencies::release_usage(&removed, &selected);
    journal.result.available_after = filesystem::available_bytes(w.root()).ok();
    journal.result.status = if journal.result.errors.is_empty() {
        "completed"
    } else {
        "pending_deletion"
    }
    .into();
    journal.result.completed_at = if journal.result.errors.is_empty() {
        Some(crate::now())
    } else {
        None
    };
    save(root, journal)?;
    Ok(journal.result.clone())
}
/// Called only under service ownership, with task admission and configuration writes excluded.
pub fn apply(w: &Workspace, id: Uuid, context: &TaskContext) -> Result<CleanupResult> {
    let root = transaction(w, id)?;
    if root.join("journal.json").is_file() {
        let mut journal: Journal = read(&root.join("journal.json"))?;
        validate(w, &journal)?;
        if journal.result.status == "completed" || journal.result.status == "rolled_back" {
            return Ok(journal.result);
        }
        if journal.result.committed {
            let result = purge(w, &root, &mut journal)?;
            context.cancel.store(false, Ordering::Relaxed);
            return Ok(result);
        }
        rollback(w, &root, &mut journal)?;
        return Ok(journal.result);
    }
    let plan: CleanupPlan = read(&owned_path(
        w,
        &PathBuf::from(format!(".jfquant/storage/plans/{id}.json")),
    )?)?;
    if !plan.metadata_fingerprint.starts_with("storage-v2:") {
        return Err("storage_plan_stale: 存储扫描规则已更新，请重新预览".into());
    }
    if plan.schema_version != 1
        || plan.id != id
        || plan.workspace != w.root()
        || !plan.blockers.is_empty()
    {
        return Err("storage_plan_blocked_or_invalid".into());
    }
    context.report("重新检查清理计划与资源引用")?;
    let fresh = super::plan(
        w,
        &plan
            .resources
            .iter()
            .map(|r| r.id.clone())
            .collect::<Vec<_>>(),
        context,
    )?;
    if !fresh.blockers.is_empty() || fresh.fingerprint != plan.fingerprint {
        return Err("storage_plan_stale: 请重新扫描并预览".into());
    }
    for task in plan
        .resources
        .iter()
        .filter_map(|r| r.task)
        .collect::<BTreeSet<_>>()
    {
        super::history::capture(w, task)?;
    }
    let mut journal = Journal {
        schema_version: 1,
        task: context.id,
        result: CleanupResult {
            schema_version: 1,
            plan: id,
            started_at: crate::now(),
            completed_at: None,
            committed: false,
            status: "prepared".into(),
            resources: plan.resources.iter().map(|r| r.id.clone()).collect(),
            deleted: SpaceUsage::default(),
            available_before: filesystem::available_bytes(w.root()).ok(),
            available_after: None,
            errors: BTreeMap::new(),
        },
        plan,
    };
    validate(w, &journal)?;
    let pending = owned_path(w, root.join("pending").strip_prefix(w.root())?)?;
    fs::create_dir_all(&pending)?;
    save(&root, &journal)?;
    let staged = (|| -> Result<()> {
        for file in &journal.plan.files {
            context.check()?;
            if FileStamp::read(&owned_path(w, &file.path)?)? != file.stamp {
                return Err("storage_file_changed_before_delete".into());
            }
        }
        for (i, file) in journal.plan.files.iter().enumerate() {
            context.check()?;
            let original = owned_path(w, &file.path)?;
            same_file(&original, &file.stamp)?;
            fs::rename(&original, pending.join(i.to_string()))?;
            fs::File::open(&pending)?.sync_all()?;
            fs::File::open(original.parent().unwrap())?.sync_all()?;
        }
        fs::File::open(&pending)?.sync_all()?;
        context.check()?;
        Ok(())
    })();
    if let Err(error) = staged {
        journal
            .result
            .errors
            .insert(PathBuf::from("staging"), error.to_string());
        rollback(w, &root, &mut journal)?;
        return Err(error);
    }
    // This one durable journal write is the availability commit for every selected resource.
    journal.result.committed = true;
    journal.result.status = "purging".into();
    if let Err(error) = save(&root, &journal) {
        // If the commit journal reached disk, recovery must finish deletion, never resurrect inputs.
        let committed =
            read::<Journal>(&root.join("journal.json")).is_ok_and(|j| j.result.committed);
        if !committed {
            journal.result.committed = false;
            rollback(w, &root, &mut journal)?;
        }
        return Err(error);
    }
    let result = purge(w, &root, &mut journal);
    context.cancel.store(false, Ordering::Relaxed);
    result
}
/// Recover under the workspace lease before accepting clients.
pub fn recover(w: &Workspace) -> Result<()> {
    let root = owned_path(w, Path::new(".jfquant/storage/transactions"))?;
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let journal_path = owned_path(w, path.join("journal.json").strip_prefix(w.root())?)?;
        if !journal_path.is_file() {
            continue;
        }
        let mut journal: Journal = read(&journal_path)?;
        validate(w, &journal)?;
        if journal.result.status == "completed" || journal.result.status == "rolled_back" {
            continue;
        }
        if journal.result.committed {
            purge(w, &path, &mut journal)?;
        } else {
            rollback(w, &path, &mut journal)?;
        }
    }
    Ok(())
}
pub(crate) fn committed_task(w: &Workspace, task: Uuid) -> bool {
    let record = read::<RunRecord>(&w.resolve(format!(".jfquant/runs/{task}/run.json")));
    let Ok(record) = record else { return false };
    let Some(id) = record.request["request"]["request"]["plan"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok())
    else {
        return false;
    };
    transaction(w, id)
        .ok()
        .and_then(|p| read::<Journal>(&p.join("journal.json")).ok())
        .is_some_and(|j| j.result.committed)
}
