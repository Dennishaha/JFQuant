//! Explicit original-byte migration. Filesystem journals are authoritative; SQLite is a cache.
use super::*;
use crate::signature;
use analytics_storage::{
    corporate_actions::{ActionBundle, review},
    evidence_store::{self as store, PackageLocator, PackageMap, Registry},
    filesystem::{FileStamp, available_bytes},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::atomic::Ordering,
    time::Instant,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    schema_version: u32,
    task: Uuid,
    plan: MigrationPlan,
    result: MigrationResult,
    registry: Registry,
    created: Vec<String>,
}
fn txn(w: &Workspace, id: Uuid) -> Result<PathBuf> {
    owned_path(
        w,
        &PathBuf::from(format!(".jfquant/storage/migrations/{id}")),
    )
}
fn registry_hash(w: &Workspace) -> Result<String> {
    let p = owned_path(w, Path::new(store::REGISTRY))?;
    Ok(store::digest(&match fs::read(p) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e.into()),
    }))
}
fn fingerprint(plan: &MigrationPlan) -> Result<String> {
    super::json::signature(&(&plan.packages, &plan.registry_sha256))
}
fn source_usage<'a>(members: impl Iterator<Item = &'a MigrationMember>) -> SpaceUsage {
    let mut identities = BTreeMap::<(u64, u64), (u64, &FileStamp)>::new();
    for m in members {
        let entry = identities
            .entry((m.stamp.device, m.stamp.inode))
            .or_insert((0, &m.stamp));
        entry.0 += 1;
    }
    let mut usage = SpaceUsage::default();
    for (links, stamp) in identities.into_values() {
        usage.files += links;
        usage.logical_bytes += stamp.bytes;
        if links == stamp.links {
            usage.allocated_bytes += stamp.allocated;
        }
    }
    usage
}
fn save(root: &Path, j: &Journal) -> Result<()> {
    cleanup::durable_write(&root.join("journal.json"), j)?;
    cleanup::durable_write(&root.join("result.json"), &j.result)
}
fn evidence_member(
    value: &Value,
    name: &str,
    hash: &str,
    out: &mut BTreeMap<String, String>,
) -> Result<()> {
    let name = value[name]
        .as_str()
        .ok_or("migration_evidence_filename_missing")?;
    let hash = value[hash]
        .as_str()
        .ok_or("migration_evidence_hash_missing")?;
    if out
        .insert(name.into(), hash.into())
        .is_some_and(|old| old != hash)
    {
        return Err("migration_evidence_name_conflict".into());
    }
    Ok(())
}
#[derive(Debug)]
struct SourceMember {
    sha256: String,
    /// A tree inspection already read and hashed this exact file identity.
    stamp: Option<FileStamp>,
}
#[derive(Default)]
struct Estimates {
    objects: BTreeMap<String, store::ObjectRef>,
    #[cfg(test)]
    compressions: usize,
}
impl Estimates {
    fn inspect(
        &mut self,
        path: &Path,
        source: &SourceMember,
    ) -> Result<(FileStamp, store::ObjectRef)> {
        let before = FileStamp::read(path)?;
        if source.stamp.as_ref().is_some_and(|stamp| stamp != &before) {
            return Err("migration_source_changed_after_tree_read".into());
        }
        let object = if let Some(object) = self.objects.get(&source.sha256) {
            if before.bytes != object.bytes
                || (source.stamp.is_none()
                    && analytics_storage::market_view::file_hash(path)? != source.sha256)
            {
                return Err("migration_source_hash_changed".into());
            }
            object.clone()
        } else {
            let object = store::estimate(path)?;
            if object.sha256 != source.sha256 {
                return Err("migration_source_hash_changed".into());
            }
            #[cfg(test)]
            {
                self.compressions += 1;
            }
            self.objects.insert(object.sha256.clone(), object.clone());
            object
        };
        if before != FileStamp::read(path)? {
            return Err("migration_source_changed_during_read".into());
        }
        Ok((before, object))
    }
}
fn contract(w: &Workspace, manifest: &Path) -> Result<BTreeMap<PathBuf, SourceMember>> {
    let root = manifest
        .parent()
        .ok_or("migration_manifest_parent_missing")?;
    let value: Value = serde_json::from_slice(&store::read(w.resolve(manifest))?)?;
    let mut raw = BTreeMap::<String, String>::new();
    if manifest.file_name().is_some_and(|n| n == "actions.json") {
        let book = ActionBundle::read(&w.resolve(manifest))?;
        // ActionBundle retains all semantic validation. Raw members are checked below
        // by the estimator, and composite trees are checked together with leaf hashes.
        raw.extend(book.raw_sha256);
    } else if manifest
        .file_name()
        .is_some_and(|n| n == "announcements.json")
    {
        review::verified_documents(&w.resolve(manifest))?;
        // Membership comes from the verified announcement contract, never arbitrary JSON pairs
        // such as the bundled Python archive descriptor in the request.
        for document in value["documents"]
            .as_array()
            .ok_or("migration_documents_missing")?
        {
            evidence_member(document, "pdf", "pdf_sha256", &mut raw)?;
            evidence_member(document, "text", "text_sha256", &mut raw)?;
        }
        for (id, hash) in value["receipts"]
            .as_object()
            .ok_or("migration_receipts_missing")?
        {
            let name = format!("{id}-actions-structured.json");
            if !safe_relative(Path::new(&name)) {
                return Err("migration_unsafe_receipt".into());
            }
            raw.insert(
                name.clone(),
                hash.as_str().ok_or("migration_invalid_hash")?.into(),
            );
            let shard: Value =
                serde_json::from_slice(&store::read(w.resolve(root.join("raw").join(name)))?)?;
            if let Some(receipts) = shard["evidence"]["http_receipts"].as_array() {
                for receipt in receipts {
                    evidence_member(receipt, "filename", "sha256", &mut raw)?;
                }
            }
        }
    } else {
        return Err("migration_unsupported_evidence_family: only actions and announcements".into());
    }
    if raw.is_empty() {
        return Err("migration_no_verified_members".into());
    }
    let mut members = BTreeMap::new();
    for (name, hash) in raw {
        if !safe_relative(Path::new(&name))
            || Path::new(&name).components().count() != 1
            || !hash.starts_with("sha256:")
        {
            return Err("migration_invalid_evidence_member".into());
        }
        members.insert(
            PathBuf::from("raw").join(name),
            SourceMember {
                sha256: hash,
                stamp: None,
            },
        );
    }
    if let Some(expected) = members.get(Path::new("raw/source-evidence-sha256.json")) {
        let proof = store::read(w.resolve(root.join("raw/source-evidence-sha256.json")))?;
        if store::digest(&proof) != expected.sha256 {
            return Err("migration_evidence_proof_changed".into());
        }
        let expected: String = serde_json::from_slice(&proof)?;
        let tree = w.resolve(root.join("source-evidence"));
        let actual = store::tree_hash_with_members(&tree, |p, hash, stamp| {
            if p.components().any(|c| c.as_os_str() == ".tdx-runtime") {
                return Err(io::Error::other("migration_private_runtime_protected"));
            }
            let member = PathBuf::from("source-evidence")
                .join(p.strip_prefix(&tree).map_err(io::Error::other)?);
            members.insert(
                member,
                SourceMember {
                    sha256: hash.into(),
                    stamp: Some(stamp),
                },
            );
            Ok(())
        })?;
        if actual != expected {
            return Err("migration_evidence_tree_changed".into());
        }
    }
    Ok(members)
}
fn package(
    w: &Workspace,
    resource: &StorageResource,
    context: &TaskContext,
    estimates: &mut Estimates,
) -> Result<MigrationPackage> {
    let manifest = if resource.path.extension().is_some_and(|e| e == "json") {
        resource.path.clone()
    } else {
        [
            "actions.json",
            "announcements.json",
            "reference.json",
            "inputs.json",
        ]
        .into_iter()
        .map(|n| resource.path.join(n))
        .find(|p| w.resolve(p).is_file())
        .ok_or("migration_supported_manifest_missing")?
    };
    package_manifest(w, resource, &manifest, context, estimates)
}
fn package_manifest(
    w: &Workspace,
    resource: &StorageResource,
    manifest: &Path,
    context: &TaskContext,
    estimates: &mut Estimates,
) -> Result<MigrationPackage> {
    let root = manifest
        .parent()
        .ok_or("migration_package_parent_missing")?
        .to_owned();
    if !(root.starts_with("data")
        || root.starts_with(".jfquant/runs")
        || root.starts_with(".jfquant/archive"))
    {
        return Err("migration_unowned_package".into());
    }
    let expected = contract(w, manifest)?;
    let mut members = vec![];
    let mut objects = BTreeMap::new();
    let mut last = Instant::now();
    for (relative, source) in expected {
        context.check()?;
        let path = root.join(&relative);
        let absolute = owned_path(w, &path)?;
        let (before, object) = estimates.inspect(&absolute, &source)?;
        members.push(MigrationMember {
            path,
            stamp: before,
            object: object.clone(),
        });
        objects.insert(relative, object);
        if last.elapsed().as_secs_f64() >= 1. {
            context.report(format!(
                "核验证据 · {} · {} 个文件 · {} 个唯一对象已估算",
                root.display(),
                members.len(),
                estimates.objects.len()
            ))?;
            last = Instant::now();
        }
    }
    Ok(MigrationPackage {
        map: PackageMap {
            schema_version: 1,
            id: store::package_id(&root)?,
            root: root.clone(),
            identity_root: root.clone(),
            manifest: manifest.strip_prefix(root)?.into(),
            manifest_sha256: store::digest(&fs::read(w.resolve(manifest))?),
            resources: vec![resource.id.clone()],
            members: objects,
        },
        members,
    })
}
fn assemble(
    w: &Workspace,
    scan_id: Uuid,
    packages: Vec<MigrationPackage>,
    mut blockers: Vec<String>,
) -> Result<MigrationPlan> {
    let mut unique = BTreeMap::new();
    let mut reference_bytes = 0;
    for p in &packages {
        for m in &p.members {
            reference_bytes += m.object.bytes;
            unique
                .entry(m.object.sha256.clone())
                .or_insert_with(|| m.object.clone());
        }
    }
    let original_file_usage = source_usage(packages.iter().flat_map(|p| &p.members));
    let mut additional = 0;
    let mut stored = 0;
    let mut original = 0;
    for object in unique.values() {
        original += object.bytes;
        stored += object.stored_bytes;
        if !store::object_path(w.root(), &object.sha256)?.try_exists()? {
            additional += object.stored_bytes;
        }
    }
    // Maps and journal must fit even when object encoding succeeds exactly at the limit.
    let metadata = super::json::pretty_size(&packages)? * 3 + 1024 * 1024;
    let new_object_bytes = additional;
    additional += metadata;
    if available_bytes(w.root()).is_ok_and(|v| v < additional) {
        blockers.push("磁盘可用空间不足以安全完成对象写入和事务日志".into())
    }
    blockers.sort();
    blockers.dedup();
    let mut plan = MigrationPlan {
        schema_version: 1,
        id: Uuid::new_v4(),
        workspace: w.root().into(),
        scan_id,
        created_at: crate::now(),
        packages,
        blockers,
        fingerprint: String::new(),
        registry_sha256: registry_hash(w)?,
        original_reference_bytes: reference_bytes,
        original_file_usage: original_file_usage.clone(),
        unique_original_bytes: original,
        object_storage_bytes: stored,
        additional_space_required: additional,
        estimated_logical_savings: original_file_usage
            .logical_bytes
            .saturating_sub(new_object_bytes),
    };
    plan.fingerprint = fingerprint(&plan)?;
    Ok(plan)
}
pub(super) fn plan(w: &Workspace, ids: &[String], context: &TaskContext) -> Result<MigrationPlan> {
    let reporter = progress::Reporter::new(w, context, ScanLevel::Resources)?;
    let inventory = inventory::scan_with(
        w,
        context,
        ScanLevel::Resources,
        default_threads(),
        true,
        Some(&reporter),
    )?;
    reporter.phase("评估共享证据与压缩容量");
    let requested: BTreeSet<_> = ids.iter().cloned().collect();
    let registry = store::registry(w.root())?;
    let running: BTreeSet<_> = w
        .runs()?
        .into_iter()
        .filter(|r| r.status == crate::RunStatus::Running)
        .map(|r| r.id)
        .collect();
    let mut packages = Vec::<MigrationPackage>::new();
    let mut blockers = vec![];
    let mut found = BTreeSet::new();
    let mut roots = BTreeMap::<PathBuf, usize>::new();
    let mut estimates = Estimates::default();
    if !inventory.complete {
        blockers.extend(inventory.diagnostics.clone());
    }
    for resource in &inventory.resources {
        if !requested.is_empty() && !requested.contains(&resource.id) {
            continue;
        }
        if resource.category != ResourceCategory::Evidence || resource.released {
            if requested.contains(&resource.id) {
                blockers.push(format!("{}：不是可迁移的证据资源", resource.name));
            }
            continue;
        }
        found.insert(resource.id.clone());
        let supported = resource
            .path
            .file_name()
            .is_some_and(|n| n == "actions.json" || n == "announcements.json")
            || ["actions.json", "announcements.json"]
                .iter()
                .any(|n| w.resolve(resource.path.join(n)).is_file());
        if !supported {
            if requested.contains(&resource.id) {
                blockers.push(format!("{}：首版仅支持行动与公告证据包", resource.name));
            }
            continue;
        }
        let root = if resource.path.extension().is_some_and(|e| e == "json") {
            resource.path.parent().unwrap()
        } else {
            &resource.path
        };
        if registry
            .packages
            .values()
            .any(|l| l.root == root && !l.released)
        {
            continue;
        }
        if resource.task.is_some_and(|id| running.contains(&id)) {
            blockers.push(format!("{}：运行任务正在使用", resource.name));
            continue;
        }
        match package(w, resource, context, &mut estimates) {
            Ok(p) => {
                if let Some(i) = roots.get(&p.map.root) {
                    packages[*i].map.resources.extend(p.map.resources)
                } else {
                    roots.insert(p.map.root.clone(), packages.len());
                    packages.push(p)
                }
            }
            Err(e) => blockers.push(format!("{}：{e}", resource.name)),
        }
    }
    for id in requested.difference(&found) {
        blockers.push(format!("资源不存在或不支持迁移：{id}"));
    }
    let plan = assemble(w, inventory.scan_id, packages, blockers)?;
    reporter.finish(&inventory)?;
    cleanup::durable_write(
        &owned_path(
            w,
            &PathBuf::from(format!(".jfquant/storage/migration-plans/{}.json", plan.id)),
        )?,
        &plan,
    )?;
    Ok(plan)
}
fn validate(w: &Workspace, j: &Journal) -> Result<()> {
    if j.schema_version != 1
        || j.plan.schema_version != 1
        || j.result.schema_version != 1
        || j.result.plan != j.plan.id
        || fingerprint(&j.plan)? != j.plan.fingerprint
    {
        return Err("migration_invalid_journal".into());
    }
    let mut paths = BTreeSet::new();
    for p in &j.plan.packages {
        if p.map.id != store::package_id(&p.map.root)?
            || !safe_relative(&p.map.root)
            || !(p.map.root.starts_with("data")
                || p.map.root.starts_with(".jfquant/runs")
                || p.map.root.starts_with(".jfquant/archive"))
            || p.map.members.len() != p.members.len()
        {
            return Err("migration_invalid_package".into());
        }
        for m in &p.members {
            let relative = m.path.strip_prefix(&p.map.root)?;
            if !safe_relative(&m.path)
                || !(relative.starts_with("raw") || relative.starts_with("source-evidence"))
                || p.map.members.get(relative) != Some(&m.object)
                || !paths.insert(m.path.clone())
            {
                return Err("migration_invalid_member".into());
            }
            if !j.result.completed {
                owned_path(w, &m.path)?;
            }
            store::object_path(w.root(), &m.object.sha256)?;
        }
    }
    let members: BTreeSet<_> = j
        .plan
        .packages
        .iter()
        .flat_map(|p| p.members.iter().map(|m| &m.object.sha256))
        .collect();
    if j.created.iter().any(|hash| !members.contains(hash)) {
        return Err("migration_unknown_created_object".into());
    }
    Ok(())
}
fn all_members(j: &Journal) -> impl Iterator<Item = &MigrationMember> {
    j.plan.packages.iter().flat_map(|p| &p.members)
}
fn unchanged(path: &Path, expected: &FileStamp) -> Result<()> {
    let actual = FileStamp::read(path)?;
    if actual.device != expected.device
        || actual.inode != expected.inode
        || actual.bytes != expected.bytes
        || actual.modified != expected.modified
    {
        return Err("migration_staged_source_changed".into());
    }
    Ok(())
}
fn rollback(w: &Workspace, root: &Path, j: &mut Journal) -> Result<()> {
    // Restoring originals must finish even after the user requested cancellation.
    let mut recovery = TaskContext::current();
    recovery.cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _scope = recovery.install();
    for (i, m) in all_members(j).enumerate() {
        let staged = root.join("pending").join(i.to_string());
        if staged.try_exists()? {
            unchanged(&staged, &m.stamp)?;
            let original = owned_path(w, &m.path)?;
            if original.try_exists()? {
                return Err("migration_rollback_collision".into());
            }
            fs::rename(&staged, &original)?;
            fs::File::open(original.parent().unwrap())?.sync_all()?;
        }
    }
    let retained: BTreeSet<_> = store::maps(w.root())?
        .into_iter()
        .flat_map(|p| p.members.into_values().map(|o| o.sha256))
        .collect();
    for hash in &j.created {
        let receipt = root.join("created").join(format!("{}.obj", &hash[7..]));
        let object = store::object_path(w.root(), hash)?;
        let ours = match FileStamp::read(&receipt) {
            Ok(stamp) => stamp,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let actual = match FileStamp::read(&object) {
            Ok(stamp) => Some(stamp),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if !retained.contains(hash)
            && actual
                .is_some_and(|actual| actual.device == ours.device && actual.inode == ours.inode)
        {
            fs::remove_file(&object)?;
            fs::File::open(object.parent().unwrap())?.sync_all()?;
        }
        fs::remove_file(&receipt)?;
    }
    if root.join("created").is_dir() {
        fs::File::open(root.join("created"))?.sync_all()?;
    }
    let temporary = root.join("object.tmp");
    if temporary.try_exists()? {
        fs::remove_file(temporary)?;
    }
    fs::File::open(root)?.sync_all()?;
    j.result.workspace = w.root().into();
    j.result.status = "rolled_back".into();
    j.result.completed = true;
    save(root, j)
}

#[cfg(test)]
#[path = "migration_tests.rs"]
mod tests;
fn finish(w: &Workspace, root: &Path, j: &mut Journal) -> Result<MigrationResult> {
    // A durable commit completes confirmed deletion; cancellation cannot interrupt it.
    let mut recovery = TaskContext::current();
    recovery.cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _scope = recovery.install();
    let mut verified = BTreeSet::new();
    for member in all_members(j) {
        if verified.insert(member.object.sha256.clone()) {
            io::copy(
                &mut store::open_object(w.root(), &member.object)?,
                &mut io::sink(),
            )?;
        }
    }
    for package in &j.plan.packages {
        cleanup::durable_write(&store::map_path(w.root(), &package.map.id)?, &package.map)?;
    }
    cleanup::durable_write(&owned_path(w, Path::new(store::REGISTRY))?, &j.registry)?;
    store::invalidate(w.root());
    let mut errors = BTreeMap::new();
    let mut removed = vec![];
    for (i, m) in all_members(j).enumerate() {
        let staged = root.join("pending").join(i.to_string());
        let result = (|| -> Result<()> {
            if staged.try_exists()? {
                unchanged(&staged, &m.stamp)?;
                fs::remove_file(&staged)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                removed.push(m);
            }
            Err(e) => {
                errors.insert(m.path.clone(), e.to_string());
            }
        }
    }
    if let Err(e) = fs::File::open(root.join("pending")).and_then(|f| f.sync_all()) {
        errors.insert("pending".into(), e.to_string());
    }
    for hash in &j.created {
        let receipt = root.join("created").join(format!("{}.obj", &hash[7..]));
        if let Err(e) = fs::remove_file(&receipt)
            && e.kind() != io::ErrorKind::NotFound
        {
            errors.insert(receipt.strip_prefix(root)?.into(), e.to_string());
        }
    }
    if root.join("created").is_dir()
        && let Err(e) = fs::File::open(root.join("created")).and_then(|f| f.sync_all())
    {
        errors.insert("created".into(), e.to_string());
    }
    let deleted = source_usage(removed.into_iter());
    let added = j
        .created
        .iter()
        .try_fold(0u64, |total, hash| -> Result<u64> {
            Ok(total
                .checked_add(FileStamp::read(&store::object_path(w.root(), hash)?)?.bytes)
                .ok_or("migration_size_overflow")?)
        })?;
    j.result.object_bytes_added = added;
    j.result.evidence_logical_bytes_delta =
        (i128::from(added) - i128::from(deleted.logical_bytes)).try_into()?;
    j.result.workspace = w.root().into();
    j.result.errors = errors;
    j.result.deleted = deleted;
    j.result.available_after = available_bytes(w.root()).ok();
    j.result.completed = j.result.errors.is_empty();
    j.result.status = if j.result.completed {
        "completed"
    } else {
        "pending_deletion"
    }
    .into();
    save(root, j)?;
    super::mark_stale(w)?;
    Ok(j.result.clone())
}
pub(super) fn apply(w: &Workspace, id: Uuid, context: &TaskContext) -> Result<MigrationResult> {
    let activity = progress::Activity::new(context, "重新核验证据迁移计划");
    let root = txn(w, id)?;
    if root.join("journal.json").is_file() {
        let mut j: Journal = read(&root.join("journal.json"))?;
        validate(w, &j)?;
        if j.result.completed {
            j.result.workspace = w.root().into();
            return Ok(j.result);
        }
        if j.result.committed {
            let result = finish(w, &root, &mut j)?;
            context.cancel.store(false, Ordering::Relaxed);
            return Ok(result);
        }
        rollback(w, &root, &mut j)?;
        return Ok(j.result);
    }
    let plan: MigrationPlan = read(&owned_path(
        w,
        &PathBuf::from(format!(".jfquant/storage/migration-plans/{id}.json")),
    )?)?;
    if plan.id != id
        || plan.workspace != w.root()
        || plan.schema_version != 1
        || !plan.blockers.is_empty()
        || plan.registry_sha256 != registry_hash(w)?
        || fingerprint(&plan)? != plan.fingerprint
    {
        return Err("migration_plan_stale_or_blocked: 请重新预览".into());
    }
    let mut registry = store::registry(w.root())?;
    for p in &plan.packages {
        if store::digest(&fs::read(owned_path(
            w,
            &p.map.root.join(&p.map.manifest),
        )?)?)
            != p.map.manifest_sha256
        {
            return Err("migration_plan_stale: manifest changed".into());
        }
        // Re-run the producer contract; unknown membership is never inferred from a directory.
        let contract = contract(w, &p.map.root.join(&p.map.manifest))?;
        if contract.len() != p.map.members.len()
            || contract.iter().any(|(path, member)| {
                p.map
                    .members
                    .get(path)
                    .is_none_or(|o| o.sha256 != member.sha256)
            })
        {
            return Err("migration_plan_stale: producer members changed".into());
        }
        // Tree members were hashed together above. Verify every other original again,
        // including historical action schemas whose reader does not validate raw hashes.
        for (relative, member) in &contract {
            context.check()?;
            if member.stamp.is_none()
                && analytics_storage::market_view::file_hash(&owned_path(
                    w,
                    &p.map.root.join(relative),
                )?)? != member.sha256
            {
                return Err("migration_plan_stale: source hash changed".into());
            }
        }
        for m in &p.members {
            context.check()?;
            if FileStamp::read(&owned_path(w, &m.path)?)? != m.stamp {
                return Err("migration_plan_stale: source changed".into());
            }
        }
        registry.packages.insert(
            p.map.id.clone(),
            PackageLocator {
                root: p.map.root.clone(),
                map_sha256: super::json::pretty_signature(&p.map)?,
                released: false,
            },
        );
    }
    let result = MigrationResult {
        schema_version: 1,
        plan: id,
        created_at: plan.created_at.clone(),
        workspace: w.root().into(),
        committed: false,
        completed: false,
        status: "prepared".into(),
        deleted: SpaceUsage::default(),
        object_bytes_added: 0,
        evidence_logical_bytes_delta: 0,
        available_before: available_bytes(w.root()).ok(),
        available_after: None,
        errors: BTreeMap::new(),
    };
    let mut created = BTreeSet::new();
    for member in plan.packages.iter().flat_map(|p| &p.members) {
        if !store::object_path(w.root(), &member.object.sha256)?.try_exists()? {
            created.insert(member.object.sha256.clone());
        }
    }
    let mut j = Journal {
        schema_version: 1,
        task: context.id,
        plan,
        result,
        registry,
        created: created.into_iter().collect(),
    };
    validate(w, &j)?;
    fs::create_dir_all(root.join("pending"))?;
    fs::create_dir_all(root.join("created"))?;
    save(&root, &j)?;
    activity.phase("生成、同步并核验共享证据对象");
    let prepared = (|| -> Result<()> {
        let mut seen = BTreeSet::new();
        let mut last = Instant::now();
        for m in all_members(&j) {
            context.check()?;
            if seen.insert(m.object.sha256.clone()) {
                let temp = root
                    .join("created")
                    .join(format!("{}.obj", &m.object.sha256[7..]));
                if temp.try_exists()? {
                    fs::remove_file(&temp)?;
                }
                store::prepare_object(w.root(), &owned_path(w, &m.path)?, &m.object, &temp)?;
            }
            if last.elapsed().as_secs_f64() >= 1. {
                context.report(format!("生成并核验共享证据 · {} 个对象", seen.len()))?;
                last = Instant::now();
            }
        }
        let mut object_directories = BTreeSet::new();
        for hash in &j.created {
            for dir in store::object_path(w.root(), hash)?
                .ancestors()
                .skip(1)
                .take(4)
            {
                object_directories.insert(dir.to_owned());
            }
        }
        for directory in object_directories {
            context.check()?;
            fs::File::open(directory)?.sync_all()?;
        }
        activity.phase("重新核验并暂存原文件");
        fs::File::open(root.join("created"))?.sync_all()?;
        let mut source_directories = BTreeSet::new();
        for m in all_members(&j) {
            context.check()?;
            if FileStamp::read(&owned_path(w, &m.path)?)? != m.stamp {
                return Err("migration_plan_stale: source changed before staging".into());
            }
        }
        for (i, m) in all_members(&j).enumerate() {
            context.check()?;
            unchanged(&owned_path(w, &m.path)?, &m.stamp)?;
            fs::rename(
                owned_path(w, &m.path)?,
                root.join("pending").join(i.to_string()),
            )?;
            source_directories.insert(w.resolve(&m.path).parent().unwrap().to_owned());
        }
        for directory in source_directories {
            context.check()?;
            fs::File::open(directory)?.sync_all()?;
        }
        fs::File::open(root.join("pending"))?.sync_all()?;
        context.check()?;
        Ok(())
    })();
    if let Err(e) = prepared {
        rollback(w, &root, &mut j)?;
        return Err(e);
    }
    j.result.committed = true;
    j.result.status = "committed".into();
    if let Err(error) = save(&root, &j) {
        let committed = read::<Journal>(&root.join("journal.json"))
            .is_ok_and(|journal| journal.result.committed);
        if !committed {
            j.result.committed = false;
            rollback(w, &root, &mut j)?;
        }
        return Err(error);
    }
    activity.phase("已提交，发布映射并永久释放原文件");
    let result = finish(w, &root, &mut j)?;
    context.cancel.store(false, Ordering::Relaxed);
    Ok(result)
}
pub(super) fn recover(w: &Workspace) -> Result<()> {
    let root = owned_path(w, Path::new(".jfquant/storage/migrations"))?;
    if !root.exists() {
        return Ok(());
    }
    let mut journals = fs::read_dir(root)?
        .map(|e| e.map(|e| e.path()))
        .collect::<io::Result<Vec<_>>>()?;
    journals.sort();
    for p in journals {
        if !p.join("journal.json").is_file() {
            continue;
        }
        let mut j: Journal = read(&p.join("journal.json"))?;
        validate(w, &j)?;
        if j.result.completed {
            let expected = serde_json::to_value(&j.result)?;
            if read::<Value>(&p.join("result.json")).ok().as_ref() != Some(&expected) {
                cleanup::durable_write(&p.join("result.json"), &j.result)?;
            }
            continue;
        }
        if j.result.committed {
            let result = finish(w, &p, &mut j)?;
            if !result.completed {
                return Err("migration_recovery_pending: 存储迁移尚未恢复完成".into());
            }
        } else {
            rollback(w, &p, &mut j)?
        }
    }
    Ok(())
}
pub(super) fn committed_task(w: &Workspace, id: Uuid) -> bool {
    fs::read_dir(w.resolve(".jfquant/storage/migrations"))
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|p| {
            read::<Journal>(&p.path().join("journal.json"))
                .is_ok_and(|j| j.task == id && j.result.committed)
        })
}
pub(super) fn history(w: &Workspace) -> Result<Vec<MigrationResult>> {
    let root = owned_path(w, Path::new(".jfquant/storage/migrations"))?;
    if !root.is_dir() {
        return Ok(vec![]);
    }
    let mut results = vec![];
    for entry in fs::read_dir(root)? {
        let entry = entry?.path();
        if !entry.join("journal.json").is_file() {
            continue;
        }
        let path = entry.join("result.json");
        let mut result: MigrationResult = read(&owned_path(w, path.strip_prefix(w.root())?)?)?;
        if result.schema_version != 1
            || entry.file_name().and_then(|n| n.to_str()) != Some(result.plan.to_string().as_str())
        {
            return Err("migration_invalid_result_cache".into());
        }
        result.workspace = w.root().into();
        results.push(result);
    }
    results.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(results)
}
pub(super) fn export(
    w: &Workspace,
    id: &str,
    output: &Path,
    context: &TaskContext,
) -> Result<Value> {
    let activity = progress::Activity::new(context, "识别待导出的证据包");
    let resolved = w.resolve(output);
    let output = resolved.as_path();
    if output.exists() {
        return Err("storage_export_destination_exists".into());
    }
    let inventory = inventory::scan(w, context)?;
    let resource = inventory
        .resources
        .iter()
        .find(|r| r.id == id && r.category == ResourceCategory::Evidence && !r.released)
        .ok_or("storage_export_resource_unavailable")?;
    let root = if resource.path.extension().is_some_and(|e| e == "json") {
        resource.path.parent().unwrap()
    } else {
        &resource.path
    };
    let parent = output.parent().ok_or("storage_export_parent_missing")?;
    if !parent.is_dir() {
        return Err("storage_export_parent_missing".into());
    }
    let parent = parent.canonicalize()?;
    let source = owned_path(w, root)?;
    if parent.starts_with(&source) || parent.starts_with(w.root().join(".jfquant")) {
        return Err("storage_export_destination_inside_managed_package".into());
    }
    let temporary = tempfile::Builder::new()
        .prefix(".jfquant-export-")
        .tempdir_in(&parent)?;
    let staging = temporary.path().join("package");
    activity.phase("物化原文并核验独立导出包");
    store::copy_tree(&source, &staging)?;
    context.check()?;
    if store::tree_hash(&staging)? != store::tree_hash(&source)? {
        return Err("storage_export_verification_failed".into());
    }
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        &staging,
        rustix::fs::CWD,
        output,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    fs::File::open(&parent)?.sync_all()?;
    Ok(serde_json::json!({"resource":id,"output":output,"original_bytes_verified":true}))
}
pub(super) fn release_packages(w: &Workspace, resources: &[String]) -> Result<()> {
    let mut registry = store::registry(w.root())?;
    let mut changed = false;
    for (id, locator) in &mut registry.packages {
        if locator.released {
            continue;
        }
        let map = store::load_map(w.root(), id, locator)?;
        if !map.resources.is_empty() && map.resources.iter().all(|id| resources.contains(id)) {
            locator.released = true;
            changed = true;
        }
    }
    if changed {
        cleanup::durable_write(&owned_path(w, Path::new(store::REGISTRY))?, &registry)?;
        store::invalidate(w.root());
    }
    Ok(())
}
/// Called by the successful producer with its exact published manifest, never by a workspace search.
pub(super) fn published(
    w: &Workspace,
    manifest: &Path,
    data: Option<&crate::DataVersion>,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<()> {
    let manifest = manifest.strip_prefix(w.root()).unwrap_or(manifest);
    let root = manifest
        .parent()
        .ok_or("publication_manifest_parent_missing")?;
    let registry = store::registry(w.root())?;
    if registry
        .packages
        .values()
        .any(|l| l.root == root && !l.released)
    {
        return Ok(());
    }
    // These are producer contracts, not a search through arbitrary JSON keys.
    let has_members = match manifest.file_name().and_then(|n| n.to_str()) {
        Some("actions.json") => !ActionBundle::read(&w.resolve(manifest))?
            .raw_sha256
            .is_empty(),
        Some("announcements.json") => {
            let value: Value = serde_json::from_slice(&store::read(w.resolve(manifest))?)?;
            value["documents"].as_array().is_some_and(|v| !v.is_empty())
                || value["receipts"].as_object().is_some_and(|v| !v.is_empty())
        }
        _ => return Err("publication_unsupported_manifest".into()),
    };
    if !has_members {
        return Ok(());
    }
    let (path, name, task) = if let Some(data) = data {
        if data.kind != "actions" || data.path != manifest {
            return Err("publication_manifest_identity_mismatch".into());
        }
        (data.path.clone(), data.id.clone(), None)
    } else {
        (
            root.to_owned(),
            format!("采集证据 · {}", run.id),
            Some(run.id),
        )
    };
    let resource = StorageResource {
        id: signature(&(ResourceCategory::Evidence, &path, &name))?,
        name,
        category: ResourceCategory::Evidence,
        path,
        task,
        start: None,
        end: None,
        modified_at: String::new(),
        archived: false,
        temporary: false,
        released: false,
        deletable: false,
        reasons: vec![],
        usage: SpaceUsage::default(),
    };
    let package = package_manifest(w, &resource, manifest, context, &mut Estimates::default())?;
    let plan = assemble(w, context.id, vec![package], vec![])?;
    if !plan.blockers.is_empty() {
        return Err(format!("publication_storage_blocked:{}", plan.blockers.join("; ")).into());
    }
    cleanup::durable_write(
        &owned_path(
            w,
            &PathBuf::from(format!(".jfquant/storage/migration-plans/{}.json", plan.id)),
        )?,
        &plan,
    )?;
    let result = apply(w, plan.id, context)?;
    if !result.completed {
        return Err("publication_storage_pending: restart to finish migration".into());
    }
    Ok(())
}
pub(super) fn archive_packages(root: &Path, source: &Path, destination: &Path) -> Result<()> {
    let mut registry = store::registry(root)?;
    let entries: Vec<_> = registry
        .packages
        .iter()
        .filter(|(_, l)| l.root.starts_with(source))
        .map(|(id, l)| (id.clone(), l.clone()))
        .collect();
    if entries.is_empty() {
        return Ok(());
    }
    for (id, locator) in entries {
        let mut map = store::load_map(root, &id, &locator)?;
        map.root = destination.join(map.root.strip_prefix(source)?);
        map.id = store::package_id(&map.root)?;
        cleanup::durable_write(&store::map_path(root, &map.id)?, &map)?;
        registry.packages.remove(&id);
        registry.packages.insert(
            map.id.clone(),
            PackageLocator {
                root: map.root.clone(),
                map_sha256: super::json::pretty_signature(&map)?,
                released: locator.released,
            },
        );
    }
    cleanup::durable_write(&store::owned(root, Path::new(store::REGISTRY))?, &registry)?;
    store::invalidate(root);
    Ok(())
}
