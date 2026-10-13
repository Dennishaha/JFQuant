//! Versioned local workspace, built-in research recipes and persistent run history.
//! All execution is synchronous on the caller's worker; no native handle escapes.
#![warn(missing_docs)]
/// Shared computation worker and plain-data protocol.
pub mod backend;
/// Explicit synthetic demo generator.
pub mod demo;
/// Local service, task requests and transport client.
#[cfg(unix)]
pub mod service;
use analytics_core::{market_rules::MarketRulesPreset, task::TaskContext};
use analytics_ml::backtest::{ResearchConfig, ResearchRequest};
use analytics_storage::{corporate_actions::ActionBundle, qlib::QlibDataset};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;
/// Result for local workflow operations.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}
static RECORD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
/// Hash canonical serialized parameters. Field order and factor order are preserved.
pub fn signature(value: &impl Serialize) -> Result<String> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(value)?)
    ))
}
// Hash actual extracted bytes as well as provenance. Paths and lengths frame
// each file so renaming, adding, truncating or editing a feature changes identity.
fn market_content_hash(root: &Path) -> Result<String> {
    fn files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        TaskContext::current().check()?;
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                files(&entry.path(), out)?;
            } else if kind.is_file() {
                out.push(entry.path());
            } else {
                return Err("market versions cannot contain links or special files".into());
            }
        }
        Ok(())
    }
    let mut paths = vec![];
    files(root, &mut paths)?;
    paths.sort();
    let mut hash = Sha256::new();
    let mut block = vec![0u8; 1024 * 1024];
    for path in paths {
        let relative = path
            .strip_prefix(root)?
            .to_str()
            .ok_or("non-UTF8 market path")?;
        hash.update((relative.len() as u64).to_le_bytes());
        hash.update(relative.as_bytes());
        let mut file = fs::File::open(path)?;
        hash.update(file.metadata()?.len().to_le_bytes());
        loop {
            TaskContext::current().check()?;
            let n = file.read(&mut block)?;
            if n == 0 {
                break;
            }
            hash.update(&block[..n]);
        }
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

fn verify_action_evidence(path: &Path, book: &ActionBundle) -> Result<()> {
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
        if format!("sha256:{:x}", Sha256::digest(bytes)) != *expected {
            return Err("action evidence hash mismatch".into());
        }
    }
    Ok(())
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
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
/// Editable, portable research recipe. Generated data and paths are separate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResearchTemplate {
    /// Template schema, currently 1.
    pub schema_version: u32,
    /// Built-in workflow kind.
    pub id: String,
    /// User-visible name.
    pub name: String,
    /// Required implementation version.
    pub formula_version: String,
    /// First out-of-sample year.
    pub first_test_year: i32,
    /// Last requested year; clipped to actual coverage.
    pub last_year: i32,
    /// Rolling and model parameters.
    pub config: ResearchConfig,
    /// User account and latest-only exchange fees.
    pub market_rules: MarketRulesPreset,
    /// Initial account CNY.
    pub initial_cash_cny: f64,
    /// Explicit fixed research dividend tax convention.
    pub dividend_withholding_rate: f64,
}
impl Default for ResearchTemplate {
    fn default() -> Self {
        let config = ResearchConfig {
            factor_ids: analytics_factor::FactorRegistry::default()
                .all()
                .filter(|s| {
                    s.id != "alpha_030"
                        && !analytics_factor::audit::quarantined_ids().contains(&s.id)
                })
                .map(|s| s.id.clone())
                .collect(),
            ..ResearchConfig::default()
        };
        Self {
            schema_version: 1,
            id: "gtja191-csi500-annual".into(),
            name: "GTJA191 / 中证500年度研究".into(),
            formula_version: analytics_factor::FORMULA_VERSION.into(),
            first_test_year: 2013,
            last_year: 2026,
            config,
            market_rules: MarketRulesPreset::a_share_latest(),
            initial_cash_cny: 1_000_000.0,
            dividend_withholding_rate: 0.0,
        }
    }
}
impl ResearchTemplate {
    /// Load the source-controlled built-in recipe.
    pub fn builtin() -> Result<Self> {
        let t: Self = serde_json::from_str(include_str!("../templates/gtja191-csi500.json"))?;
        t.validate()?;
        Ok(t)
    }
    /// Validate before saving or starting a task.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1
            || self.id != "gtja191-csi500-annual"
            || self.formula_version != analytics_factor::FORMULA_VERSION
            || self.name.trim().is_empty()
        {
            return Err("unsupported recipe identity/version".into());
        }
        if !(2000..=9999).contains(&self.first_test_year)
            || self.first_test_year > self.last_year
            || self.last_year > 9999
            || !self.initial_cash_cny.is_finite()
            || self.initial_cash_cny <= 0.0
            || self.initial_cash_cny > 1e13
            || !self.dividend_withholding_rate.is_finite()
            || !(0.0..=1.0).contains(&self.dividend_withholding_rate)
        {
            return Err("invalid research years/cash/tax".into());
        }
        self.config.validate()?;
        self.market_rules.validate()?;
        let registry = analytics_factor::FactorRegistry::default();
        let mut seen = BTreeSet::new();
        if self.config.factor_ids.is_empty() {
            return Err("select at least one factor".into());
        }
        for id in &self.config.factor_ids {
            if !seen.insert(id)
                || id == "alpha_030"
                || analytics_factor::audit::quarantined_ids().contains(id)
                || registry.get(id).is_none()
            {
                return Err(format!("unavailable or duplicate factor {id}").into());
            }
        }
        Ok(())
    }
    /// Read an explicit user-selected JSON file; never executes code.
    pub fn read(path: &Path) -> Result<Self> {
        let t: Self = serde_json::from_slice(&fs::read(path)?)?;
        t.validate()?;
        Ok(t)
    }
    /// Save a validated recipe atomically.
    pub fn write(&self, path: &Path) -> Result<()> {
        self.validate()?;
        atomic_json(path, self)
    }
}
/// A verified local input version. Corporate action readiness is separate from download success.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataVersion {
    /// Stable ID.
    pub id: String,
    /// market or actions.
    pub kind: String,
    /// Workspace-relative dataset root or action JSON path.
    pub path: PathBuf,
    /// Acquisition/provenance fingerprint.
    pub signature: String,
    /// First covered date when known.
    pub start: Option<String>,
    /// Last covered date when known.
    pub end: Option<String>,
    /// Complete source identity/provenance or coverage details.
    pub details: Value,
}
/// Durable terminal and transient run states.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Work is executing.
    Running,
    /// Finished normally.
    Succeeded,
    /// Failed without corrupting other runs.
    Failed,
    /// Input coverage blocks research.
    Blocked,
    /// User cancelled.
    Cancelled,
    /// Process stopped before recording completion.
    Interrupted,
    /// Historical artifacts registered read-only.
    Imported,
}
/// One local task record; input snapshots are never overwritten by data updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    /// Record schema, currently 1.
    pub schema_version: u32,
    /// Task/run UUID.
    pub id: Uuid,
    /// Display operation.
    pub operation: String,
    /// UTC creation time.
    pub started_at: String,
    /// UTC terminal time.
    pub finished_at: Option<String>,
    /// Durable task state.
    pub status: RunStatus,
    /// Frozen request and data versions.
    pub request: Value,
    /// Request signature.
    pub signature: String,
    /// Workspace-relative output location.
    pub output: PathBuf,
    /// Last task stage.
    pub stage: String,
    /// Failure or incomplete-data explanation.
    pub error: Option<String>,
    /// Read-only imported run marker.
    pub read_only: bool,
}
/// Explicit local workspace; never uses process current directory after opening.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
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
        Ok(Self { root })
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
    fn relative(&self, path: &Path) -> Result<PathBuf> {
        let absolute = fs::canonicalize(path)?;
        Ok(absolute
            .strip_prefix(&self.root)
            .map_err(|_| {
                invalid("数据不在当前工作区内，请选择其所在工作区，或将数据放入当前data目录。")
            })?
            .to_owned())
    }
    /// Scan registered immutable versions.
    pub fn datasets(&self) -> Result<Vec<DataVersion>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join(".jfquant/catalog"))? {
            let p = entry?.path();
            if p.extension().is_some_and(|e| e == "json") {
                let item: DataVersion = serde_json::from_slice(&fs::read(p)?)?;
                if item.path.is_absolute()
                    || item
                        .path
                        .components()
                        .any(|c| !matches!(c, std::path::Component::Normal(_)))
                {
                    return Err("invalid data version path".into());
                }
                out.push(item);
            }
        }
        out.sort_by(|a: &DataVersion, b| a.id.cmp(&b.id));
        Ok(out)
    }
    /// Register independently acquired market data or a saved action bundle within this workspace.
    pub fn register(&self, path: &Path) -> Result<DataVersion> {
        let relative = self.relative(path)?;
        let (kind, details, start, end) = if path.is_dir() {
            let dataset = QlibDataset::open(path)?;
            let mut source: Value = serde_json::from_slice(&fs::read(
                path.parent()
                    .ok_or("market parent missing")?
                    .join("jfquant_source.json"),
            )?)?;
            if !(source["generator"] == "JFQuant analytics-workflow synthetic-demo-v1"
                && source["synthetic_fixture"] == true)
                && (source["downloader"] != "JFQuant analytics-storage"
                    || source["archive_verified"] != true
                    || source["independent_download"] != true)
            {
                return Err("unverified market provenance".into());
            }
            source["content_sha256"] = json!(market_content_hash(path)?);
            (
                "market",
                source,
                dataset.calendar.first().map(ToString::to_string),
                dataset.calendar.last().map(ToString::to_string),
            )
        } else {
            let book = ActionBundle::read(path)?;
            verify_action_evidence(path, &book)?;
            (
                "actions",
                serde_json::to_value(&book)?,
                book.actions
                    .iter()
                    .map(|a| a.ex_date)
                    .min()
                    .map(|d| d.to_string()),
                book.actions
                    .iter()
                    .map(|a| a.ex_date)
                    .max()
                    .map(|d| d.to_string()),
            )
        };
        let hash = signature(&details)?;
        let details = if kind == "actions" {
            let book: ActionBundle = serde_json::from_value(details)?;
            json!({"events":book.actions.len(),"queried_symbols":book.queried_symbols.len(),"source_failures":book.failures,"downloaded_at":book.downloaded_at,"downloader":book.downloader,"missing_cash_dates":book.actions.iter().filter(|a|a.cash_per_share>0.0 && a.pay_date.is_none()).count(),"missing_listing_dates":book.actions.iter().filter(|a|a.shares_per_share>0.0 && a.listing_date.is_none()).count()})
        } else {
            details
        };
        let id = format!("{kind}-{}", &hash[7..23]);
        let data = DataVersion {
            id: id.clone(),
            kind: kind.into(),
            path: relative,
            signature: hash,
            start,
            end,
            details,
        };
        let dest = self
            .root
            .join(".jfquant/catalog")
            .join(format!("{id}.json"));
        if dest.exists() {
            let old: DataVersion = serde_json::from_slice(&fs::read(&dest)?)?;
            return Ok(old);
        }
        atomic_json(&dest, &data)?;
        Ok(data)
    }
    /// List only durable run records, ignoring generated models or partial output.
    pub fn runs(&self) -> Result<Vec<RunRecord>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join(".jfquant/runs"))? {
            let entry = entry?;
            // Legacy output names may link to a UUID record, never duplicate it.
            if entry.file_type()?.is_symlink() {
                continue;
            }
            let p = entry.path().join("run.json");
            if p.exists() {
                let item: RunRecord = serde_json::from_slice(&fs::read(&p)?)?;
                if item.schema_version != 1
                    || item.output != Path::new(&format!(".jfquant/runs/{}", item.id))
                    || p.parent()
                        .and_then(|p| p.file_name())
                        .and_then(|s| s.to_str())
                        != Some(item.id.to_string().as_str())
                {
                    return Err("invalid run identity/path".into());
                }
                out.push(item);
            }
        }
        out.sort_by(|a: &RunRecord, b| b.started_at.cmp(&a.started_at));
        Ok(out)
    }
    /// Mark abandoned runs interrupted after acquiring the app's workspace lock.
    pub fn recover(&self) -> Result<()> {
        for mut run in self.runs()? {
            if run.status == RunStatus::Running {
                run.status = RunStatus::Interrupted;
                run.finished_at = Some(now());
                run.error = Some("上次程序退出，任务未完成；研究请重新运行。".into());
                self.save_run(&run)?;
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
        atomic_json(&self.resolve(&run.output).join("run.json"), run)
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
        let _guard = RECORD_LOCK
            .lock()
            .map_err(|_| invalid("record lock poisoned"))?;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.resolve(&run.output).join("events.jsonl"))?;
        writeln!(f, "{}", json!({"time":now(),"message":message}))?;
        let mut current: RunRecord =
            serde_json::from_slice(&fs::read(self.resolve(&run.output).join("run.json"))?)?;
        if current.status != RunStatus::Running {
            return Ok(());
        }
        current.stage = message.into();
        self.save_run(&current)
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
    /// Register a current-format strict research result as read-only.
    pub fn import_run(&self, path: &Path) -> Result<RunRecord> {
        let summary: Value = serde_json::from_slice(&fs::read(path.join("summary.json"))?)?;
        analytics_ml::backtest::validate_result(&summary)?;
        let mut run = self.begin(
            Uuid::new_v4(),
            "历史研究（只读）",
            json!({"source":path,"summary":summary}),
        )?;
        run.read_only = true;
        self.finish(&mut run, RunStatus::Imported, None)?;
        Ok(run)
    }
    /// Load saved artifacts, rejecting unsupported research result formats.
    pub fn results(&self, run: &RunRecord) -> Result<Value> {
        if run.read_only {
            analytics_ml::backtest::validate_result(&run.request["summary"])?;
            return Ok(run.request["summary"].clone());
        }
        let dir = self.resolve(&run.output);
        let workbench = dir.join("artifacts.json");
        if workbench.exists() {
            return Ok(serde_json::from_slice(&fs::read(workbench)?)?);
        }
        let p = dir.join("summary.json");
        if p.exists() {
            let summary = serde_json::from_slice(&fs::read(p)?)?;
            analytics_ml::backtest::validate_result(&summary)?;
            Ok(summary)
        } else {
            let p = dir.join("execution_readiness.json");
            if p.exists() {
                Ok(serde_json::from_slice(&fs::read(p)?)?)
            } else {
                let payload = dir.join("payload.json");
                if payload.exists() {
                    return Ok(serde_json::from_slice(&fs::read(payload)?)?);
                }
                let p = dir.join("outcome.json");
                if p.exists() {
                    Ok(serde_json::from_slice(&fs::read(p)?)?)
                } else {
                    Ok(Value::Null)
                }
            }
        }
    }
}
/// An operation selectable in the data or research UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowRequest {
    /// Create clearly marked synthetic inputs and a small acceptance recipe.
    CreateDemo {},
    /// Discover a public release without changing the selected input version.
    CheckUpdates {},
    /// Download a fixed release; resume owned staging when supplied.
    DownloadMarket {
        /// Immutable release tag.
        tag: String,
        /// Previous interrupted task staging ID.
        resume: Option<Uuid>,
    },
    /// Register existing JFQuant-owned data from an explicit workspace path.
    RegisterData {
        /// Relative or absolute input path.
        path: PathBuf,
    },
    /// Fetch first company action history using a registered market version.
    DownloadActions {
        /// Market version ID.
        market: String,
        /// Previous interrupted acquisition staging task.
        resume: Option<Uuid>,
    },
    /// Enrich, detail, cash payment, or evidence repair in a new version.
    SupplementActions {
        /// Source action version ID.
        actions: String,
        /// enrich, details, payments, repair, repair-payments, retry.
        method: String,
        /// First requested ex date.
        start: analytics_core::Date,
        /// Last requested date.
        end: analytics_core::Date,
        /// Previous interrupted derivation task.
        resume: Option<Uuid>,
    },
    /// Readiness-only or full annual research.
    Research {
        /// Frozen recipe.
        template: Box<ResearchTemplate>,
        /// Market version ID.
        market: String,
        /// Action version ID.
        actions: String,
        /// Only perform input audit.
        audit_only: bool,
    },
}
impl WorkflowRequest {
    /// Display operation names without exposing backend details in the user flow.
    pub fn label(&self) -> String {
        match self {
            Self::CreateDemo {} => "创建合成演示数据".into(),
            Self::CheckUpdates {} => "检查行情更新".into(),
            Self::DownloadMarket { tag, .. } => format!("下载行情快照 {tag}"),
            Self::RegisterData { .. } => "核验登记数据版本".into(),
            Self::DownloadActions { .. } => "下载公司行动历史".into(),
            Self::SupplementActions { method, .. } => format!("补充公司行动 ({method})"),
            Self::Research {
                template,
                audit_only,
                ..
            } => format!(
                "{} · {}",
                if *audit_only {
                    "检查研究输入"
                } else {
                    "年度滚动研究"
                },
                template.name
            ),
        }
    }
}

/// Plain-data results sent back to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowOutcome {
    /// Newly registered data version, if any.
    pub data: Option<DataVersion>,
    /// Public release candidate or report/readiness content.
    pub details: Value,
}
fn version_metadata(workspace: &Workspace, id: &str, kind: &str) -> Result<DataVersion> {
    let data = workspace
        .datasets()?
        .into_iter()
        .find(|d| d.id == id && d.kind == kind)
        .ok_or("unknown input version")?;
    let path = workspace.resolve(&data.path);
    let content = if kind == "market" {
        let mut source = serde_json::from_slice::<Value>(&fs::read(
            path.parent()
                .ok_or("market parent missing")?
                .join("jfquant_source.json"),
        )?)?;
        let expected = data.details["content_sha256"]
            .as_str()
            .ok_or("market version has no content hash; register it again before research")?;
        source["content_sha256"] = json!(expected);
        source
    } else {
        serde_json::to_value(ActionBundle::read(&path)?)?
    };
    if signature(&content)? != data.signature {
        return Err("registered data version changed; register it as a new version".into());
    }
    Ok(data)
}
// Large byte verification is worker-side, keeping submit/control RPCs responsive.
fn version(workspace: &Workspace, id: &str, kind: &str) -> Result<DataVersion> {
    let data = version_metadata(workspace, id, kind)?;
    let path = workspace.resolve(&data.path);
    if kind == "market" {
        if data.details["content_sha256"] != market_content_hash(&path)? {
            return Err("registered market bytes changed; register it as a new version".into());
        }
    } else {
        verify_action_evidence(&path, &ActionBundle::read(&path)?)?;
    }
    Ok(data)
}
/// Execute an operation in the caller's scoped task context; never starts a native UI thread.
pub fn execute(
    workspace: &Workspace,
    request: &WorkflowRequest,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<WorkflowOutcome> {
    context.execute(|| {
        let outcome = execute_scoped(workspace, request, run, context)?;
        atomic_json(
            &workspace.resolve(&run.output).join("outcome.json"),
            &outcome,
        )?;
        Ok(outcome)
    })
}
fn execute_scoped(
    w: &Workspace,
    request: &WorkflowRequest,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<WorkflowOutcome> {
    context.check()?;
    let mut data = None;
    let mut details = Value::Null;
    match request {
        WorkflowRequest::CreateDemo {} => {
            context.report("创建合成演示输入（不是真实行情）")?;
            let (m, a, t) = demo::create(w)?;
            details = json!({"demo_actions":a.id,"demo_template":t});
            data = Some(m);
        }
        WorkflowRequest::CheckUpdates {} => {
            context.report("检查公开行情版本")?;
            let value = analytics_storage::transport::json(
                "https://api.github.com/repos/chenditc/investment_data/releases/latest",
            )?;
            let tag = value["tag_name"].as_str().ok_or("release tag missing")?;
            tag.parse::<analytics_core::Date>()?;
            let assets = value["assets"].as_array().ok_or("release assets missing")?;
            for name in ["qlib_bin.tar.gz", "qlib_bin.manifest.json"] {
                if !assets.iter().any(|a| a["name"] == name) {
                    return Err("release lacks paired immutable assets".into());
                }
            }
            details =
                json!({"tag":tag,"published_at":value["published_at"],"url":value["html_url"]});
        }
        WorkflowRequest::DownloadMarket { tag, resume } => {
            tag.parse::<analytics_core::Date>()?;
            context.report("下载并验证行情")?;
            let owner = resume.unwrap_or(run.id);
            let staging = w.root.join(format!(".jfquant/staging/{owner}"));
            let output = w.root.join(format!("data/market-{tag}-{}", run.id));
            analytics_storage::acquisition::fetch_release_resumable(tag, &staging, &output)?;
            data = Some(w.register(&output.join("qlib_bin"))?);
        }
        WorkflowRequest::RegisterData { path } => {
            context.report("核验本地数据来源")?;
            data = Some(w.register(&w.resolve(path))?);
        }
        WorkflowRequest::DownloadActions { market, resume } => {
            let m = version(w, market, "market")?;
            let dataset = QlibDataset::open(w.resolve(&m.path))?;
            let symbols = dataset
                .membership("csi500")?
                .into_keys()
                .collect::<Vec<_>>();
            let path = w.root.join(format!("data/actions-{}", run.id));
            let owner = resume.unwrap_or(run.id);
            let staging = w.root.join(format!(".jfquant/staging/actions-{owner}"));
            analytics_storage::corporate_actions::fetch_public_resumable(&symbols, &staging)?;
            fs::rename(staging, &path)?;
            data = Some(w.register(&path.join("actions.json"))?);
        }
        WorkflowRequest::SupplementActions {
            actions,
            method,
            start,
            end,
            resume,
        } => {
            let a = version(w, actions, "actions")?;
            let path = w.resolve(&a.path);
            let source = ActionBundle::read(&path)?;
            let root = path.parent().ok_or("action parent missing")?;
            let final_output = w.root.join(format!("data/actions-{}", run.id));
            let owner = resume.unwrap_or(run.id);
            let output = if method == "retry" {
                final_output.clone()
            } else {
                w.root.join(format!(".jfquant/staging/supplement-{owner}"))
            };
            context.report(format!("公司行动补充：{method}"))?;
            use analytics_storage::corporate_actions as ca;
            match method.as_str() {
                "enrich" => {
                    ca::enrich_public(&source, root, &output)?;
                }
                "details" => {
                    ca::fetch_details(&source, root, &output, *start, *end)?;
                }
                "payments" => {
                    ca::fetch_pay_dates(&source, root, &output, *start, *end)?;
                }
                "repair" => {
                    ca::repair_from_evidence(&source, root, &output)?;
                }
                "repair-payments" => {
                    ca::repair_pay_dates(&source, root, &output, *start, *end)?;
                }
                "retry" => {
                    ca::retry_public(&source, root, &output)?;
                }
                _ => return Err("unknown action supplement".into()),
            };
            if output != final_output {
                fs::rename(&output, &final_output)?;
            }
            data = Some(w.register(&final_output.join("actions.json"))?);
        }
        WorkflowRequest::Research {
            template,
            market,
            actions,
            audit_only,
        } => {
            template.validate()?;
            let m = version(w, market, "market")?;
            let a = version(w, actions, "actions")?;
            details = research(w, run, context, template, &m, &a, *audit_only)?;
        }
    }
    context.check()?;
    Ok(WorkflowOutcome { data, details })
}
/// Snapshot selected input versions when creating a run, rather than resolving them later.
pub fn snapshot(workspace: &Workspace, request: &WorkflowRequest) -> Result<Value> {
    let versions = match request {
        WorkflowRequest::Research {
            market, actions, ..
        } => vec![
            version_metadata(workspace, market, "market")?,
            version_metadata(workspace, actions, "actions")?,
        ],
        WorkflowRequest::DownloadActions { market, .. } => {
            vec![version_metadata(workspace, market, "market")?]
        }
        WorkflowRequest::SupplementActions { actions, .. } => {
            vec![version_metadata(workspace, actions, "actions")?]
        }
        _ => Vec::new(),
    };
    Ok(
        json!({"request":request,"inputs":versions,"app_version":env!("CARGO_PKG_VERSION"),"formula_version":analytics_factor::FORMULA_VERSION}),
    )
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

/// Verify packaged native loading with a real query, small training and prediction.
/// Invoke on a worker thread; generated test artifacts remain in the supplied local folder.
pub fn self_check(root: &Path) -> Result<Value> {
    use analytics_engine::{DuckDbEngine, QueryEngine};
    use analytics_ml::{
        DenseMatrix, LightGbmPredictor, LightGbmTrainer, Predictor, Trainer, TrainingSplit,
    };
    fs::create_dir_all(root)?;
    let mut engine = DuckDbEngine::open_in_memory()?;
    let query = engine.query_page(analytics_core::PageRequest {
        sql: "SELECT 42 AS answer, version() AS duckdb_version".into(),
        limit: 100,
        sort: Vec::new(),
        after: None,
    })?;
    let matrix = || DenseMatrix {
        features: (0..24).flat_map(|i| [i as f64, (i % 3) as f64]).collect(),
        labels: (0..24).map(|i| (i as f32) * 0.1).collect(),
        rows: 24,
        columns: 2,
    };
    let split = TrainingSplit {
        train: matrix(),
        validation: matrix(),
        test: matrix(),
    };
    let request = analytics_core::TrainRequest {
        factor_ids: vec!["self_check_1".into(), "self_check_2".into()],
        universe_id: "synthetic".into(),
        date_range: analytics_core::DateRange {
            start: "2020-01-01".parse()?,
            end: "2020-12-31".parse()?,
        },
        objective: analytics_core::Objective::Regression,
        parameters: std::collections::BTreeMap::from([
            ("num_iterations".into(), "3".into()),
            ("num_leaves".into(), "3".into()),
            ("min_data_in_leaf".into(), "2".into()),
            ("num_threads".into(), "2".into()),
            ("verbosity".into(), "-1".into()),
        ]),
        early_stopping_rounds: None,
    };
    let (tx, _) = std::sync::mpsc::channel();
    let model =
        LightGbmTrainer.train_split(&request, &split, root, tx, TaskContext::current().cancel)?;
    let scores = LightGbmPredictor {
        model_path: model.model_path,
    }
    .predict_dense(&split.test.features, 24, 2, false)?
    .predictions;
    if scores.len() != 24 || scores.iter().flatten().any(|v| !v.is_finite()) {
        return Err("native prediction self-check failed".into());
    }
    Ok(
        json!({"ok":true,"query":query,"predictions":scores.len(),"formula_version":analytics_factor::FORMULA_VERSION}),
    )
}

/// Synthetic integration fixture, only compiled for acceptance tests.
#[cfg(any(test, feature = "test-support"))]
pub mod testing;

#[cfg(test)]
mod tests;

pub(crate) fn research(
    w: &Workspace,
    run: &RunRecord,
    context: &TaskContext,
    template: &ResearchTemplate,
    market: &DataVersion,
    actions: &DataVersion,
    audit_only: bool,
) -> Result<Value> {
    template.validate()?;
    let mut request = ResearchRequest::new(w.resolve(&market.path), w.resolve(&run.output));
    request.config = template.config.clone();
    request.first_test_year = template.first_test_year;
    request.last_year = template.last_year;
    request.market_rules = template.market_rules.clone();
    request.initial_cash_cny = template.initial_cash_cny;
    request.dividend_withholding_rate = template.dividend_withholding_rate;
    request.corporate_actions = Some(w.resolve(&actions.path));
    request.audit_only = audit_only;
    context.report("检查研究输入与公司行动")?;
    analytics_ml::backtest::run_with_context(&request, context)?;
    w.results(run)
}
