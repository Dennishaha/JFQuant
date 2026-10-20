//! Explicit, offline JSON import with durable backup, atomic SQL commit and crash recovery.
#![allow(missing_docs)]
use super::*;
use analytics_storage::{database, evidence_store as store};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Write};

#[derive(Debug, Serialize, Deserialize)]
pub struct MigrationReport {
    pub id: Uuid,
    pub files: usize,
    pub catalog: usize,
    pub runs: usize,
    pub packages: usize,
    pub plans: usize,
    pub configs: usize,
    pub backup: PathBuf,
    pub journal: PathBuf,
}
#[derive(Debug, Serialize, Deserialize)]
struct Source {
    path: PathBuf,
    sha256: String,
    kind: String,
}
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    schema_version: u32,
    id: Uuid,
    sources: Vec<Source>,
    complete: bool,
    #[serde(default)]
    rollback: bool,
    #[serde(default)]
    rolled_back: bool,
}
#[cfg(test)]
thread_local! {static FAULT:std::cell::Cell<Option<&'static str>>=const {std::cell::Cell::new(None)};}
fn fault(point: &str) -> Result<()> {
    #[cfg(test)]
    if FAULT.with(|f| {
        if f.get() == Some(point) {
            f.set(None);
            true
        } else {
            false
        }
    }) {
        return Err(format!("injected_{point}").into());
    }
    let _ = point;
    Ok(())
}
fn log(path: &Path, event: Value) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{event}")?;
    f.sync_all()?;
    Ok(())
}
fn discover(w: &Workspace) -> Result<Vec<Source>> {
    fn visit(
        w: &Workspace,
        p: &Path,
        out: &mut Vec<Source>,
        kind: &str,
        recursive: bool,
    ) -> Result<()> {
        let owned = owned_path(w, p.strip_prefix(w.root())?)?;
        let p = owned.as_path();
        if !p.try_exists()? {
            return Ok(());
        }
        for e in fs::read_dir(p)? {
            let e = e?;
            let ty = e.file_type()?;
            if ty.is_symlink() {
                if kind == "run"
                    && e.path()
                        .canonicalize()?
                        .starts_with(w.resolve(".jfquant/archive"))
                {
                    continue;
                }
                return Err(format!("json_migration_symlink:{}", e.path().display()).into());
            }
            if ty.is_dir() && recursive {
                // JSON migration backups are snapshots, never new import sources.
                if !e
                    .file_name()
                    .to_string_lossy()
                    .starts_with("json-migration-")
                {
                    visit(w, &e.path(), out, kind, true)?;
                }
            } else if ty.is_file()
                && e.path().extension().is_some_and(|s| s == "json")
                && (kind != "run" || e.file_name() == "run.json")
            {
                let path = e.path().strip_prefix(w.root())?.to_owned();
                let file_kind = if kind == "package" && e.file_name() == "registry.json" {
                    "registry"
                } else {
                    kind
                };
                out.push(Source {
                    sha256: store::digest(&fs::read(e.path())?),
                    path,
                    kind: file_kind.into(),
                });
            }
        }
        Ok(())
    }
    let mut out = vec![];
    for (dir, kind, recursive) in [
        (".jfquant/catalog", "catalog", false),
        (".jfquant/runs", "run", true),
        (".jfquant/archive", "run", true),
        (".jfquant/storage/packages", "package", false),
        (".jfquant/storage/migration-plans", "plan", false),
        (".jfquant/pools", "config", false),
    ] {
        visit(w, &w.resolve(dir), &mut out, kind, recursive)?;
    }
    for name in [
        "source-policy.json",
        "tdx-first-policy.json",
        "shared-ui-jobs.json",
        "allocation-migration.json",
        "data-upgrade-20261018.json",
    ] {
        let path = PathBuf::from(".jfquant").join(name);
        let absolute = owned_path(w, &path)?;
        if absolute.try_exists()? {
            out.push(Source {
                sha256: store::digest(&fs::read(absolute)?),
                path,
                kind: "config".into(),
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}
fn archive(w: &Workspace, id: Uuid) -> Result<PathBuf> {
    owned_path(
        w,
        &PathBuf::from(format!(".jfquant/archive/json-migration-{id}")),
    )
}
fn finish(w: &Workspace, root: &Path, manifest: &mut Manifest) -> Result<()> {
    let backup = archive(w, manifest.id)?;
    let journal = root.join("journal.jsonl");
    for source in &manifest.sources {
        let original = owned_path(w, &source.path)?;
        let saved = store::owned(&backup, &source.path)?;
        if store::digest(&fs::read(&saved)?) != source.sha256 {
            return Err("json_migration_backup_changed".into());
        }
        if original.try_exists()? {
            if store::digest(&fs::read(&original)?) != source.sha256 {
                return Err(
                    format!("json_migration_source_changed:{}", source.path.display()).into(),
                );
            }
            // Durable intent precedes removal. Recovery repeats it idempotently.
            log(
                &journal,
                json!({"op":"delete","path":source.path,"ts":crate::now()}),
            )?;
            fs::remove_file(&original)?;
            fs::File::open(original.parent().unwrap())?.sync_all()?;
        }
    }
    super::mark_stale(w)?;
    manifest.complete = true;
    cleanup::durable_write(&root.join("manifest.json"), manifest)?;
    log(
        &journal,
        json!({"op":"complete","id":manifest.id,"ts":crate::now()}),
    )?;
    Ok(())
}
fn rollback_import(w: &Workspace, root: &Path, manifest: &mut Manifest) -> Result<()> {
    use rusqlite::OptionalExtension;
    let backup = archive(w, manifest.id)?;
    let journal = root.join("journal.jsonl");
    let mut db = database::open(&w.database_path()?)?;
    let revision: Option<i64> = db
        .query_row(
            "SELECT revision FROM json_imports WHERE id=?1",
            [manifest.id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(expected) = revision {
        let current: i64 = db.query_row(
            "SELECT revision FROM metadata_revisions WHERE name='workspace'",
            [],
            |r| r.get(0),
        )?;
        if current != expected {
            return Err("json_migration_rollback_blocked_by_metadata_changes: 迁移后已有新记录或更新，请保留数据库和备份".into());
        }
    }
    // Validate the entire backup and all destinations before restoring any source.
    for source in &manifest.sources {
        let bytes = fs::read(store::owned(&backup, &source.path)?)?;
        if store::digest(&bytes) != source.sha256 {
            return Err("json_migration_backup_changed".into());
        }
        let original = owned_path(w, &source.path)?;
        if original.try_exists()? && store::digest(&fs::read(original)?) != source.sha256 {
            return Err("json_migration_rollback_destination_changed".into());
        }
    }
    manifest.rollback = true;
    cleanup::durable_write(&root.join("manifest.json"), manifest)?;
    for source in &manifest.sources {
        let original = owned_path(w, &source.path)?;
        if !original.try_exists()? {
            let bytes = fs::read(store::owned(&backup, &source.path)?)?;
            fs::create_dir_all(original.parent().unwrap())?;
            let mut temporary = tempfile::NamedTempFile::new_in(original.parent().unwrap())?;
            temporary.write_all(&bytes)?;
            temporary.as_file().sync_all()?;
            temporary
                .persist_noclobber(&original)
                .map_err(|e| e.error)?;
            fs::File::open(original.parent().unwrap())?.sync_all()?;
        }
        log(
            &journal,
            json!({"op":"restore","path":source.path,"ts":crate::now()}),
        )?;
    }
    fault("rollback_after_restore")?;
    if revision.is_some() {
        let tx = db.transaction()?;
        for source in &manifest.sources {
            let value: Value =
                serde_json::from_slice(&fs::read(store::owned(&backup, &source.path)?)?)?;
            let (table, column, key) = match source.kind.as_str() {
                "catalog" => (
                    "catalog_entries",
                    "id",
                    value["id"].as_str().ok_or("catalog id missing")?,
                ),
                "run" => (
                    "run_records",
                    "id",
                    value["id"].as_str().ok_or("run id missing")?,
                ),
                "package" => (
                    "package_maps",
                    "id",
                    value["id"].as_str().ok_or("package id missing")?,
                ),
                "plan" => (
                    "migration_plans",
                    "id",
                    value["id"].as_str().ok_or("plan id missing")?,
                ),
                "config" => (
                    "workspace_config",
                    "key",
                    source.path.to_str().ok_or("non-UTF8 path")?,
                ),
                "registry" => continue,
                _ => return Err("json_migration_unknown_kind".into()),
            };
            let rows = tx.execute(&format!("DELETE FROM {table} WHERE {column}=?1"), [key])?;
            log(
                &journal,
                json!({"op":"rollback_delete","table":table,"id":key,"rows":rows}),
            )?;
        }
        tx.execute(
            "DELETE FROM json_imports WHERE id=?1",
            [manifest.id.to_string()],
        )?;
        tx.commit()?;
    }
    super::mark_stale(w)?;
    manifest.rolled_back = true;
    manifest.complete = true;
    cleanup::durable_write(&root.join("manifest.json"), manifest)?;
    log(
        &journal,
        json!({"op":"rolled_back","id":manifest.id,"ts":crate::now()}),
    )?;
    store::invalidate(w.root());
    Ok(())
}

/// Recover after a crash between SQL commit and deletion. Uncommitted imports keep originals.
pub fn recover(w: &Workspace) -> Result<()> {
    let storage = w.resolve(".jfquant/storage");
    if !storage.is_dir() {
        return Ok(());
    }
    for e in fs::read_dir(storage)? {
        let e = e?;
        if !e
            .file_name()
            .to_string_lossy()
            .starts_with("json-migration-")
        {
            continue;
        }
        let root = owned_path(w, e.path().strip_prefix(w.root())?)?;
        let path = store::owned(&root, Path::new("manifest.json"))?;
        if !path.is_file() {
            continue;
        }
        let mut manifest: Manifest = serde_json::from_slice(&fs::read(&path)?)?;
        if manifest.schema_version != 2
            || e.file_name() != format!("json-migration-{}", manifest.id).as_str()
        {
            return Err("json_migration_invalid_manifest".into());
        }
        if manifest.rollback && !manifest.rolled_back {
            rollback_import(w, &root, &mut manifest)?;
            continue;
        }
        if manifest.complete {
            continue;
        }
        let db = database::open(&w.database_path()?)?;
        let committed: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM json_imports WHERE id=?1)",
            [manifest.id.to_string()],
            |r| r.get(0),
        )?;
        if committed {
            finish(w, &root, &mut manifest)?;
        }
    }
    Ok(())
}
/// One-time migration; call while holding the exclusive workspace lease.
pub struct JsonToSqliteMigration {
    workspace: Workspace,
}
impl JsonToSqliteMigration {
    /// Open only the marker: this entry point must also accept a legacy workspace.
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.canonicalize()?;
        let marker: Value =
            serde_json::from_slice(&fs::read(root.join(".jfquant/workspace.json"))?)?;
        if marker["schema_version"] != 1 {
            return Err("unsupported workspace marker".into());
        }
        Ok(Self {
            workspace: Workspace { root },
        })
    }
    /// Restore the imported JSON and remove its SQL rows, only before any later metadata write.
    pub fn rollback(&mut self, id: Uuid) -> Result<Value> {
        let w = &self.workspace;
        let _lease = w.lease()?;
        let root = owned_path(
            w,
            &PathBuf::from(format!(".jfquant/storage/json-migration-{id}")),
        )?;
        let mut manifest: Manifest =
            serde_json::from_slice(&fs::read(store::owned(&root, Path::new("manifest.json"))?)?)?;
        if manifest.schema_version != 2 || manifest.id != id {
            return Err("json_migration_invalid_manifest".into());
        }
        if !manifest.rolled_back {
            rollback_import(w, &root, &mut manifest)?;
        }
        Ok(json!({"id":id,"rolled_back":true,"restored":manifest.sources.len()}))
    }
    pub fn execute(&mut self, context: &TaskContext) -> Result<MigrationReport> {
        let w = &self.workspace;
        let _lease = w.lease()?;
        recover(w)?;
        let sources = discover(w)?;
        context.check()?;
        let id = Uuid::new_v4();
        let root = owned_path(
            w,
            &PathBuf::from(format!(".jfquant/storage/json-migration-{id}")),
        )?;
        let backup = archive(w, id)?;
        fs::create_dir_all(&root)?;
        fs::create_dir_all(&backup)?;
        let journal = root.join("journal.jsonl");
        let mut manifest = Manifest {
            schema_version: 2,
            id,
            sources,
            complete: false,
            rollback: false,
            rolled_back: false,
        };
        cleanup::durable_write(&root.join("manifest.json"), &manifest)?;
        let mut report = MigrationReport {
            id,
            files: manifest.sources.len(),
            catalog: 0,
            runs: 0,
            packages: 0,
            plans: 0,
            configs: 0,
            backup: backup.strip_prefix(w.root())?.into(),
            journal: journal.strip_prefix(w.root())?.into(),
        };
        if manifest.sources.is_empty() {
            manifest.complete = true;
            cleanup::durable_write(&root.join("manifest.json"), &manifest)?;
            return Ok(report);
        }
        // Snapshot the existing database before any schema/data modification. The lease excludes
        // service writers; checkpoint includes all committed WAL pages in this durable copy.
        let database = w.database_path()?;
        if database.try_exists()? {
            let db = rusqlite_connection(&database)?;
            let busy: i64 = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
            if busy != 0 {
                return Err("json_migration_snapshot_checkpoint_busy".into());
            }
            drop(db);
            let saved = backup.join("index.sqlite");
            fs::copy(&database, &saved)?;
            fs::File::open(saved)?.sync_all()?;
            log(
                &journal,
                json!({"op":"snapshot","from":database,"to":backup.join("index.sqlite")}),
            )?;
        }
        for source in &manifest.sources {
            context.check()?;
            let from = owned_path(w, &source.path)?;
            let to = store::owned(&backup, &source.path)?;
            let bytes = fs::read(&from)?;
            if store::digest(&bytes) != source.sha256 {
                return Err("json_migration_source_changed_before_backup".into());
            }
            fs::create_dir_all(to.parent().unwrap())?;
            let mut f = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&to)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            fs::File::open(to.parent().unwrap())?.sync_all()?;
            log(
                &journal,
                json!({"op":"backup","from":source.path,"to":to,"sha256":source.sha256,"ts":crate::now()}),
            )?;
        }
        let mut db = database::open(&database)?;
        let tx = db.transaction()?;
        let imported = (|| -> Result<()> {
            let mut registry: Option<store::Registry> = None;
            let mut maps = BTreeMap::new();
            for source in &manifest.sources {
                context.check()?;
                let bytes = fs::read(store::owned(&backup, &source.path)?)?;
                let value: Value = serde_json::from_slice(&bytes)?;
                let (table, key) = match source.kind.as_str() {
                    "catalog" => {
                        let entry: crate::DataVersion = serde_json::from_value(value)?;
                        let exists: bool = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM catalog_entries WHERE id=?1)",
                            [&entry.id],
                            |r| r.get(0),
                        )?;
                        if exists {
                            return Err(
                                format!("json_migration_catalog_conflict:{}", entry.id).into()
                            );
                        }
                        analytics_storage::catalog::insert(&tx, &entry)?;
                        let rows: i64 = tx.query_row(
                            "SELECT count(*) FROM catalog_evidence WHERE catalog_id=?1",
                            [&entry.id],
                            |r| r.get(0),
                        )?;
                        log(
                            &journal,
                            json!({"op":"insert","table":"catalog_evidence","catalog_id":entry.id,"rows":rows}),
                        )?;
                        report.catalog += 1;
                        ("catalog_entries", entry.id)
                    }
                    "run" => {
                        let run: RunRecord = serde_json::from_value(value)?;
                        let exists: bool = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM run_records WHERE id=?1)",
                            [run.id.to_string()],
                            |r| r.get(0),
                        )?;
                        if exists {
                            return Err(format!("json_migration_run_conflict:{}", run.id).into());
                        }
                        analytics_storage::run_store::insert(
                            &tx,
                            source.path.parent().unwrap(),
                            &run,
                        )?;
                        tx.execute(
                            "UPDATE run_records SET original_json=?2 WHERE id=?1",
                            rusqlite::params![run.id.to_string(), bytes],
                        )?;
                        report.runs += 1;
                        ("run_records", run.id.to_string())
                    }
                    "package" => {
                        let map: store::PackageMap = serde_json::from_value(value)?;
                        if maps
                            .insert(map.id.clone(), (map, store::digest(&bytes)))
                            .is_some()
                        {
                            return Err("json_migration_duplicate_package".into());
                        }
                        report.packages += 1;
                        continue;
                    }
                    "registry" => {
                        let r: store::Registry = serde_json::from_value(value)?;
                        if r.schema_version != 1 {
                            return Err("json_migration_invalid_registry_version".into());
                        }
                        registry = Some(r);
                        continue;
                    }
                    "plan" => {
                        analytics_storage::migration_store::insert(&tx, &value)?;
                        report.plans += 1;
                        (
                            "migration_plans",
                            value["id"].as_str().ok_or("missing plan id")?.into(),
                        )
                    }
                    "config" => {
                        let key = source.path.to_str().ok_or("non-UTF8 config path")?;
                        let category = analytics_storage::metadata::config_category(&source.path)
                            .ok_or("unrecognized config")?;
                        let exists: bool = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM workspace_config WHERE key=?1)",
                            [key],
                            |r| r.get(0),
                        )?;
                        if exists {
                            return Err(format!("json_migration_config_conflict:{key}").into());
                        }
                        analytics_storage::metadata::insert_config(&tx, key, category, &value)?;
                        report.configs += 1;
                        ("workspace_config", key.into())
                    }
                    _ => return Err("json_migration_unknown_kind".into()),
                };
                log(
                    &journal,
                    json!({"op":"insert","table":table,"id":key,"rows":1,"ts":crate::now()}),
                )?;
            }
            let registry = registry.unwrap_or_default();
            for (id, (map, hash)) in &maps {
                let locator = registry
                    .packages
                    .get(id)
                    .ok_or("json_migration_unregistered_package")?;
                if hash != &locator.map_sha256 || map.root != locator.root {
                    return Err("json_migration_map_hash_or_root_mismatch".into());
                }
                analytics_storage::package_store::insert(&tx, map, locator.released)?;
                tx.execute(
                    "UPDATE package_maps SET map_sha256=?2 WHERE id=?1",
                    rusqlite::params![id, hash],
                )?;
                log(
                    &journal,
                    json!({"op":"insert","table":"package_maps","id":id,"members":map.members.len()}),
                )?;
            }
            if registry.packages.len() != maps.len() {
                return Err("json_migration_registry_member_missing".into());
            }
            for source in &manifest.sources {
                context.check()?;
                if store::digest(&fs::read(owned_path(w, &source.path)?)?) != source.sha256 {
                    return Err("json_migration_source_changed_before_commit".into());
                }
            }
            fault("before_commit")?;
            tx.execute(
                "INSERT INTO json_imports VALUES(?1,?2,(SELECT revision FROM metadata_revisions WHERE name='workspace'))",
                rusqlite::params![id.to_string(), crate::now()],
            )?;
            context.check()?;
            Ok(())
        })();
        if let Err(error) = imported {
            tx.rollback()?;
            log(
                &journal,
                json!({"op":"rollback","error":error.to_string(),"ts":crate::now()}),
            )?;
            return Err(error);
        }
        tx.commit()?;
        fault("after_commit")?;
        log(&journal, json!({"op":"commit","id":id,"ts":crate::now()}))?;
        finish(w, &root, &mut manifest)?;
        store::invalidate(w.root());
        Ok(report)
    }
}
// Keep the pre-upgrade snapshot opener separate from the schema installer.
fn rusqlite_connection(path: &Path) -> Result<rusqlite::Connection> {
    Ok(rusqlite::Connection::open(path)?)
}

#[cfg(test)]
mod tests;
