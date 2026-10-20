//! Workspace.
use crate::{DataVersion, Result, RunRecord, RunStatus, atomic_json, invalid, now, signature};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

static RECORD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Explicit local workspace; never uses process current directory after opening.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub(crate) root: PathBuf,
}
impl Workspace {
    /// Create/open workspace metadata. Opening does not recover running tasks automatically.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(root.as_ref())?;
        let root = fs::canonicalize(root)?;
        for p in [
            "data",
            ".jfquant/runs",
            ".jfquant/templates",
            ".jfquant/pools",
            ".jfquant/catalog",
            ".jfquant/staging",
        ] {
            fs::create_dir_all(root.join(p))?;
        }
        let marker = root.join(".jfquant/workspace.json");
        if marker.exists() {
            let v: Value = serde_json::from_slice(&fs::read(&marker)?)?;
            if v["schema_version"] != 1 {
                return Err("unsupported workspace version".into());
            }
        } else {
            atomic_json(&marker, &json!({"schema_version":1,"created_at":now()}))?;
        }
        let workspace = Self { root };
        workspace.require_metadata_migration()?;
        Ok(workspace)
    }
    fn require_metadata_migration(&self) -> Result<()> {
        let database = self.database_path()?;
        if database.is_file() {
            let db = rusqlite::Connection::open_with_flags(
                &database,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let installed: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='json_imports')",
                [],
                |r| r.get(0),
            )?;
            if installed
                && db.query_row("SELECT EXISTS(SELECT 1 FROM json_imports)", [], |r| {
                    r.get::<_, bool>(0)
                })?
            {
                return Ok(());
            }
        }
        let has_json = |directory: PathBuf| -> Result<bool> {
            if !directory.is_dir() {
                return Ok(false);
            }
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                if entry.file_type()?.is_file()
                    && entry.path().extension().is_some_and(|s| s == "json")
                {
                    return Ok(true);
                }
            }
            Ok(false)
        };
        let mut legacy = has_json(self.resolve(".jfquant/catalog"))?
            || has_json(self.resolve(".jfquant/pools"))?
            || has_json(self.resolve(".jfquant/storage/packages"))?
            || has_json(self.resolve(".jfquant/storage/migration-plans"))?;
        for name in [
            "source-policy.json",
            "tdx-first-policy.json",
            "shared-ui-jobs.json",
            "allocation-migration.json",
            "data-upgrade-20261018.json",
        ] {
            legacy |= self.resolve(Path::new(".jfquant").join(name)).is_file();
        }
        for entry in fs::read_dir(self.resolve(".jfquant/runs"))? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && entry.path().join("run.json").is_file() {
                legacy = true;
                break;
            }
        }
        fn archived_runs(directory: &Path) -> Result<bool> {
            if !directory.is_dir() {
                return Ok(false);
            }
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let kind = entry.file_type()?;
                if kind.is_symlink()
                    || entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("json-migration-")
                {
                    continue;
                }
                if (kind.is_file() && entry.file_name() == "run.json")
                    || (kind.is_dir() && archived_runs(&entry.path())?)
                {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        if legacy || archived_runs(&self.resolve(".jfquant/archive"))? {
            return Err("json_metadata_migration_required: 停止工作区服务后执行 jfquant storage migrate-json".into());
        }
        Ok(())
    }
    /// Absolute workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Resolve user input relative to the workspace. Explicit absolute inputs are supported.
    pub fn resolve(&self, p: impl AsRef<Path>) -> PathBuf {
        if p.as_ref().is_absolute() {
            p.as_ref().to_owned()
        } else {
            self.root.join(p)
        }
    }
    pub(crate) fn relative(&self, path: &Path) -> Result<PathBuf> {
        let absolute = fs::canonicalize(path)?;
        Ok(absolute
            .strip_prefix(&self.root)
            .map_err(|_| {
                invalid("数据不在当前工作区内，请选择其所在工作区，或将数据放入当前data目录。")
            })?
            .to_owned())
    }
    /// Authoritative metadata database; validates owned path components.
    pub fn database_path(&self) -> Result<PathBuf> {
        Ok(analytics_storage::evidence_store::owned(
            &self.root,
            Path::new(analytics_storage::database::INDEX),
        )?)
    }
    /// One durable record by UUID, including archived runs.
    pub fn run(&self, id: Uuid) -> Result<Option<RunRecord>> {
        Ok(analytics_storage::run_store::RunStore::open(&self.database_path()?)?.get(&id)?)
    }
    /// Scan registered immutable versions.
    pub fn datasets(&self) -> Result<Vec<DataVersion>> {
        let mut out = analytics_storage::catalog::CatalogStore::open(&self.database_path()?)?
            .list(None, None, None)?;
        let released = crate::storage_management::released_dataset_ids(self)?;
        out.retain(|item| !released.contains(&item.id));
        out.sort_by(|a: &DataVersion, b| a.id.cmp(&b.id));
        Ok(out)
    }
    /// List only durable run records, ignoring generated models or partial output.
    pub fn runs(&self) -> Result<Vec<RunRecord>> {
        let mut out: Vec<RunRecord> =
            analytics_storage::run_store::RunStore::open(&self.database_path()?)?
                .load_all()?
                .into_iter()
                .filter(|(p, _)| p.starts_with(".jfquant/runs"))
                .map(|(_, r)| (*r).clone())
                .collect();
        out.sort_by(|a: &RunRecord, b| b.started_at.cmp(&a.started_at));
        Ok(out)
    }
    /// Mark abandoned runs interrupted after acquiring the app's workspace lock.
    pub fn recover(&self) -> Result<()> {
        for mut run in self.runs()? {
            if run.status == RunStatus::Running {
                self.finish(
                    &mut run,
                    RunStatus::Interrupted,
                    Some("上次程序退出，任务未完成；研究请重新运行。".into()),
                )?;
            }
        }
        Ok(())
    }
    /// Atomically persist status.
    pub fn save_run(&self, run: &RunRecord) -> Result<()> {
        if run.schema_version != 1 || run.output != Path::new(&format!(".jfquant/runs/{}", run.id))
        {
            return Err("invalid run identity/output".into());
        }
        analytics_storage::run_store::RunStore::open(&self.database_path()?)?
            .save(&run.output, run)?;
        Ok(())
    }
    /// New independent output and frozen request.
    pub fn begin(&self, id: Uuid, operation: &str, request: Value) -> Result<RunRecord> {
        let output = PathBuf::from(format!(".jfquant/runs/{id}"));
        fs::create_dir(self.resolve(&output))?;
        let run = RunRecord {
            schema_version: 1,
            id,
            operation: operation.into(),
            started_at: now(),
            finished_at: None,
            status: RunStatus::Running,
            signature: signature(&request)?,
            request,
            output,
            stage: "开始".into(),
            error: None,
            read_only: false,
        };
        self.save_run(&run)?;
        Ok(run)
    }
    /// Update log and stage. Safe for serial callbacks from bounded acquisition workers.
    pub fn progress(&self, run: &RunRecord, message: &str) -> Result<()> {
        self.append_progress(run.id, &run.output, message)
    }
    pub(crate) fn task_progress(&self, id: Uuid, message: &str) -> Result<()> {
        self.append_progress(id, &PathBuf::from(format!(".jfquant/runs/{id}")), message)
    }
    fn append_progress(&self, id: Uuid, output: &Path, message: &str) -> Result<()> {
        let _guard = RECORD_LOCK
            .lock()
            .map_err(|_| invalid("record lock poisoned"))?;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.resolve(output).join("events.jsonl"))?;
        writeln!(f, "{}", json!({"time":now(),"message":message}))?;
        analytics_storage::run_store::RunStore::open(&self.database_path()?)?
            .progress(&id, message)?;
        Ok(())
    }
    /// Set a terminal task result without removing diagnostic artifacts.
    pub fn finish(
        &self,
        run: &mut RunRecord,
        status: RunStatus,
        error: Option<String>,
    ) -> Result<()> {
        let _guard = RECORD_LOCK
            .lock()
            .map_err(|_| invalid("record lock poisoned"))?;
        if status == RunStatus::Running {
            return Err("finish requires a terminal status".into());
        }
        let current: RunRecord = self.run(run.id)?.ok_or("run not found")?;
        if current.status != RunStatus::Running {
            if current.status == status && current.error == error {
                *run = current;
                return Ok(());
            }
            return Err("terminal run cannot transition again".into());
        }
        run.status = status;
        run.stage = match status {
            RunStatus::Succeeded => "完成",
            RunStatus::Blocked => "数据未就绪",
            RunStatus::Cancelled => "已取消",
            RunStatus::Interrupted => "已中断",
            RunStatus::Failed => "失败",
            RunStatus::Imported => "历史只读",
            RunStatus::Running => "运行中",
        }
        .into();
        run.finished_at = Some(now());
        run.error = error;
        self.save_run(run)
    }
}
/// Lifetime lock prevents a second app from recovering or writing active runs.
#[derive(Debug)]
pub struct WorkspaceLease(fs::File);
impl Workspace {
    /// Acquire exclusive app ownership before recovering interrupted runs.
    pub fn lease(&self) -> Result<WorkspaceLease> {
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(".jfquant/app.lock"))?;
        f.try_lock()
            .map_err(|e| invalid(format!("工作区正在被其他程序使用：{e}")))?;
        Ok(WorkspaceLease(f))
    }
}
impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
