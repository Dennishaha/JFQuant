//! Versioned local workspace, built-in research recipes and persistent run history.
//! All execution is synchronous on the caller's worker; no native handle escapes.
#![warn(missing_docs)]
/// Shared computation worker and plain-data protocol.
pub mod backend;
/// Explicit synthetic demo generator.
pub mod demo;
/// Immutable date-effective stock pools.
pub mod pools;
/// Independently validated acquisition checkpoints.
pub mod preparation;
/// Independently saved workflow, factor and target configuration.
pub mod profiles;
/// Prioritized, independently resumable acquisition and refresh.
pub mod refresh;
/// Local service, task requests and transport client.
#[cfg(unix)]
pub mod service;

/// Shared research orchestration and supervised Python runtime.
pub mod research;

use crate::research::backtest::{ResearchConfig, ResearchRequest};
use analytics_core::{market_rules::MarketRulesPreset, task::TaskContext};
use analytics_storage::{corporate_actions::ActionBundle, daily::DailyDataset};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write},
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
    Ok(analytics_storage::market_view::tree_hash(root)?)
}

fn verify_action_evidence(path: &Path, book: &ActionBundle) -> Result<()> {
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
    /// Independently versioned feature family.
    pub factor_set: analytics_core::research_quality::FactorSet,
    /// Training source trust, separate from sample completeness.
    pub price_basis: analytics_core::research_quality::PriceBasis,
    /// Resolved template schema, currently 10; absence is unsupported.
    #[serde(default = "missing_template_version")]
    pub schema_version: u32,
    /// Immutable pool version, or builtin:csi300/builtin:csi500 bound to the selected market.
    pub pool_version: String,
    /// Explicit benchmark security for factor inputs and performance.
    pub benchmark: String,
    /// Position treatment when membership ends.
    pub pool_exit_policy: analytics_core::pool::PoolExitPolicy,
    /// Built-in workflow kind.
    pub id: String,
    /// User-visible name.
    pub name: String,
    /// Required implementation version.
    pub formula_version: String,
    /// Date-level training, validation and test windows.
    pub schedule: analytics_core::schedule::Schedule,
    /// Explicit ordinary-holder corporate action policy.
    pub action_policy: analytics_core::actions::ActionPolicy,
    /// Account-data trust, independent of training sample quality.
    pub event_check_policy: analytics_core::execution::EventCheckPolicy,
    /// Shared research completeness and sample policy.
    pub quality_policy: analytics_core::research_quality::ResearchQualityPolicy,
    /// Rolling and model parameters.
    pub config: ResearchConfig,
    /// User account and latest-only exchange fees.
    pub market_rules: MarketRulesPreset,
    /// Initial account CNY.
    pub initial_cash_cny: f64,
    /// Explicit fixed research dividend tax convention.
    pub dividend_withholding_rate: f64,
}
fn missing_template_version() -> u32 {
    0
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
            schema_version: 10,
            factor_set: Default::default(),
            price_basis: Default::default(),
            pool_version: "builtin:csi500".into(),
            benchmark: "SH000905".into(),
            pool_exit_policy: Default::default(),
            id: "gtja191-csi500-annual".into(),
            name: "GTJA191 / 中证500年度研究".into(),
            formula_version: analytics_factor::FORMULA_VERSION.into(),
            schedule: Default::default(),
            action_policy: Default::default(),
            event_check_policy: Default::default(),
            quality_policy: Default::default(),
            config,
            market_rules: MarketRulesPreset::a_share_latest(),
            initial_cash_cny: 1_000_000.0,
            dividend_withholding_rate: 0.0,
        }
    }
}
impl ResearchTemplate {
    /// Extract account settings without changing frozen training inputs.
    pub fn execution_settings(&self) -> research::backtest::ResearchExecution {
        research::backtest::ResearchExecution {
            initial_cash_cny: self.initial_cash_cny,
            market_rules: self.market_rules.clone(),
            top_n: self.config.top_n,
            rebalance_days: self.config.rebalance_days,
            pool_exit_policy: self.pool_exit_policy,
        }
    }

    /// Select a single family and reset its canonical feature order.
    pub fn select_factor_set(&mut self, set: analytics_core::research_quality::FactorSet) {
        self.factor_set = set;
        self.formula_version = analytics_factor::alpha158::version(set).into();
        self.config.factor_ids = analytics_factor::FactorRegistry::for_set(set)
            .all()
            .filter(|s| {
                !matches!(
                    s.expression,
                    analytics_factor::FactorExpr::Unsupported { .. }
                ) && s.id != "alpha_030"
            })
            .map(|s| s.id.clone())
            .collect();
    }
    /// Load the source-controlled built-in recipe.
    pub fn builtin() -> Result<Self> {
        Self::builtin_target(false)
    }
    /// Load the CSI300 builtin recipe.
    pub fn builtin_csi300() -> Result<Self> {
        Self::builtin_target(true)
    }
    fn builtin_target(csi300: bool) -> Result<Self> {
        let d: profiles::WorkflowDocument = serde_json::from_str(if csi300 {
            include_str!("../templates/csi300-annual.json")
        } else {
            include_str!("../templates/annual-rolling.json")
        })?;
        d.resolve(
            serde_json::from_str(include_str!("../templates/factors/gtja191.json"))?,
            serde_json::from_str(if csi300 {
                include_str!("../templates/targets/csi300.json")
            } else {
                include_str!("../templates/targets/csi500.json")
            })?,
        )
    }
    /// Validate before saving or starting a task.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 10
            || self.id.trim().is_empty()
            || self.formula_version != analytics_factor::alpha158::version(self.factor_set)
            || self.name.trim().is_empty()
        {
            return Err("unsupported recipe identity/version".into());
        }
        self.schedule.validate()?;
        if self.action_policy.exercise_optional_cash_choice {
            return Err(
                "optional cash-choice eligibility is outside passive ordinary-holder policy".into(),
            );
        }
        if !self.initial_cash_cny.is_finite()
            || self.initial_cash_cny <= 0.0
            || self.initial_cash_cny > 1e13
            || !self.dividend_withholding_rate.is_finite()
            || !(0.0..=1.0).contains(&self.dividend_withholding_rate)
        {
            return Err("invalid research years/cash/tax".into());
        }
        analytics_core::pool::validate_instrument(&self.benchmark)?;
        if self.pool_version.is_empty() {
            return Err("missing pool version".into());
        }
        self.config.validate()?;
        self.quality_policy.validate()?;
        self.market_rules.validate()?;
        let registry = analytics_factor::FactorRegistry::for_set(self.factor_set);
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
        profiles::WorkflowDocument::read(path)
    }
    /// Save a validated recipe atomically.
    pub fn write(&self, path: &Path) -> Result<()> {
        profiles::WorkflowDocument::write(self, path)
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
            let dataset = DailyDataset::open(path)?;
            let mut source: Value = serde_json::from_slice(&fs::read(
                path.parent()
                    .ok_or("market parent missing")?
                    .join("jfquant_source.json"),
            )?)?;
            if !(source["generator"] == "JFQuant analytics-workflow synthetic-demo-v1"
                && source["synthetic_fixture"] == true)
                && (source["downloader"] != "JFQuant analytics-storage"
                    || (source["archive_verified"] != true && source["snapshot_verified"] != true)
                    || source["independent_download"] != true)
            {
                return Err("unverified market provenance".into());
            }
            source["content_sha256"] = json!(market_content_hash(path)?);
            source["source_map"] = serde_json::to_value(dataset.source_map())?;
            (
                "market",
                source,
                dataset.calendar.first().map(ToString::to_string),
                dataset.calendar.last().map(ToString::to_string),
            )
        } else if path.file_name().is_some_and(|n| n == "coefficients.json") {
            let bundle = analytics_storage::collector::CoefficientBundle::read(path)?;
            (
                "coefficients",
                serde_json::to_value(&bundle)?,
                Some(bundle.request.start.to_string()),
                Some(bundle.request.end.to_string()),
            )
        } else {
            let book = ActionBundle::read(path)?;
            verify_action_evidence(path, &book)?;
            (
                "actions",
                ActionBundle::version_content(path)?,
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
            json!({"events":book.actions.len(),"queried_symbols":book.queried_symbols.len(),"source_failures":book.failures,"downloaded_at":book.downloaded_at,"downloader":book.downloader,"missing_cash_dates":book.actions.iter().filter(|a|a.cash()>0.0 && a.pay_date.is_none()).count(),"missing_listing_dates":book.actions.iter().filter(|a|a.shares()>0.0 && a.listing_date.is_none()).count()})
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
        crate::research::backtest::validate_result(&summary)?;
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
            crate::research::backtest::validate_result(&run.request["summary"])?;
            return Ok(run.request["summary"].clone());
        }
        let dir = self.resolve(&run.output);
        let workbench = dir.join("artifacts.json");
        if workbench.exists() {
            return Ok(serde_json::from_slice(&fs::read(workbench)?)?);
        }
        let training = dir.join("training.json");
        let training_value = if training.exists() {
            {
                let artifact = serde_json::from_slice::<Value>(&fs::read(&training)?)?;
                crate::research::backtest::validate_training_artifact(&artifact)?;
                Some(artifact)
            }
        } else {
            None
        };
        let p = dir.join("summary.json");
        if p.exists() {
            let summary = serde_json::from_slice(&fs::read(p)?)?;
            crate::research::backtest::validate_result(&summary)?;
            Ok(summary)
        } else {
            if matches!(
                run.request["request"]["operation"].as_str(),
                Some("train_research")
            ) && let Some(artifact) = training_value.as_ref()
            {
                return Ok(artifact.clone());
            }
            let p = dir.join("execution_readiness.json");
            if p.exists() {
                let mut readiness: Value = serde_json::from_slice(&fs::read(p)?)?;
                if let Some(t) = &training_value {
                    readiness["training_artifact"] = t.clone();
                }
                let prediction = dir.join("prediction_manifest.json");
                if prediction.is_file() {
                    readiness["prediction_artifact"] =
                        serde_json::from_slice(&fs::read(prediction)?)?;
                }
                Ok(readiness)
            } else {
                let payload = dir.join("payload.json");
                if payload.exists() {
                    return Ok(serde_json::from_slice(&fs::read(payload)?)?);
                }
                let p = dir.join("outcome.json");
                if p.exists() {
                    Ok(serde_json::from_slice(&fs::read(p)?)?)
                } else {
                    Ok(training_value.unwrap_or(Value::Null))
                }
            }
        }
    }
}
/// An operation selectable in the data or research UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowRequest {
    /// Fetch bounded implementation evidence independently of model/universe preparation.
    CollectAnnouncements {
        /// Explicit CNINFO collection scope, using the AKShare collector boundary.
        request: analytics_storage::collector::CollectionRequest,
        /// Prior checkpoint for the identical source, dates and collector code.
        resume: Option<Uuid>,
    },
    /// Independently acquire a calendar or a current-only constituent observation.
    CollectReference {
        /// Exact source and reference range.
        request: analytics_storage::collector::CollectionRequest,
        /// calendar or members; never historical members synthesized from a current list.
        kind: String,
        /// Prior owned reference checkpoint.
        resume: Option<Uuid>,
    },
    /// Inspect provider capabilities and workspace priority policy.
    SourceStatus {},
    /// Persist a validated workspace policy separately from research recipes.
    SaveSourcePolicy {
        /// Ordered providers for every data purpose.
        policy: refresh::SourcePolicy,
    },
    /// Bounded source probe; never starts a full universe collection.
    CheckSource {
        /// Explicitly selected source.
        provider: analytics_storage::collector::Provider,
    },
    /// Preview or execute independently verified incremental refresh shards.
    RefreshData {
        /// Source range, base versions and explicit overrides.
        request: Box<refresh::RefreshRequest>,
    },
    /// Collect independent coefficients without requiring quote acquisition.
    CollectCoefficients {
        /// Frozen source and securities.
        request: analytics_storage::collector::CollectionRequest,
        /// Matching resumable task.
        resume: Option<Uuid>,
    },
    /// Explicitly compose original Qlib raw recovery with a frozen independent coefficient version.
    ComposeMarket {
        /// Original registered Qlib snapshot.
        market: String,
        /// Independent registered coefficient version.
        coefficients: String,
    },

    /// Save an immutable composed pool through the shared backend.
    SavePool {
        /// Validated composition and dated overrides.
        definition: analytics_core::pool::PoolDefinition,
    },
    /// Preview effective dated membership.
    PreviewPool {
        /// Pool version or explicit builtin name.
        id: String,
        /// Selected immutable market version.
        market: String,
        /// Effective exchange date to inspect.
        day: analytics_core::Date,
    },
    /// Preview requirements without creating factor caches or native trainers.
    PreviewResearch {
        /// Optional selected action snapshot for transitive successor requirements.
        actions: Option<String>,
        /// Selected date schedule and ordinary-holder strategy.
        template: Box<ResearchTemplate>,
        /// Immutable market snapshot ID.
        market: String,
    },
    /// Independently fetch the selected research pool's action history or announcement evidence.
    PrepareResearchData {
        /// Optional reviewed action snapshot for successor requirements.
        actions: Option<String>,
        /// Selected date schedule and ordinary-holder strategy.
        template: Box<ResearchTemplate>,
        /// Immutable market snapshot ID.
        market: String,
        /// Fetch implementation documents when true; action histories otherwise.
        announcements: bool,
        /// Matching prior task checkpoint ID.
        resume: Option<Uuid>,
    },
    /// Inspect original events, stable hashes and captured candidates for a single security.
    InspectActions {
        /// Immutable original action snapshot.
        actions: String,
        /// Canonical security to inspect.
        instrument: String,
        /// Optional captured announcement manifest.
        evidence_manifest: Option<PathBuf>,
    },
    /// Replay frozen evidence through the shared deterministic action rules.
    ReconcileActions {
        /// Selected base action version; source files are never edited.
        actions: String,
        /// Terminal native evidence tasks to freeze into the new version.
        evidence_tasks: Vec<Uuid>,
        /// Optional explicit market calendar for record/ex-date joins.
        market: Option<String>,
    },
    /// Apply reviewed implementation evidence as a new immutable action version.
    ReviseActions {
        /// Original immutable action snapshot ID.
        actions: String,
        /// Evidence quotes, stale event hashes and reasons.
        request: analytics_storage::corporate_actions::review::RevisionRequest,
    },
    /// Create clearly marked synthetic inputs and a small acceptance recipe.
    CreateDemo {},
    /// Complete synthetic research in canonical Parquet format.
    CreateCanonicalDemo {},
    /// Discover a public release without changing the selected input version.
    CheckUpdates {},
    /// Download a fixed release; resume owned staging when supplied.
    DownloadMarket {
        /// Immutable release tag.
        tag: String,
        /// Previous interrupted task staging ID.
        resume: Option<Uuid>,
    },
    /// Collect daily data using the packaged Python provider.
    CollectQuotes {
        /// Frozen quote source and range; independent of coefficients.
        request: analytics_storage::collector::CollectionRequest,
        /// Resume only complete hash-verified securities.
        resume: Option<Uuid>,
    },
    /// Collect quotations together with their selected adjustment source.
    CollectMarket {
        /// Explicit provider and range.
        request: analytics_storage::collector::CollectionRequest,
        /// Existing owned task checkpoints.
        resume: Option<Uuid>,
    },
    /// Fetch company-action candidates through the Python provider.
    CollectActions {
        /// Explicit securities/provider; no date guessing.
        request: analytics_storage::collector::CollectionRequest,
        /// Previous owned collection task.
        resume: Option<Uuid>,
    },
    /// Inspect endpoint permissions without declaring source completeness.
    TusharePermissions {},
    /// Register existing JFQuant-owned data from an explicit workspace path.
    RegisterData {
        /// Relative or absolute input path.
        path: PathBuf,
    },
    /// Fetch first company action history using a registered market version.
    DownloadActions {
        /// Effective pool for action coverage.
        pool_version: String,
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
    /// Train reusable fold models without validating future account settlements.
    TrainResearch {
        /// Recipe.
        template: Box<ResearchTemplate>,
        /// Market snapshot.
        market: String,
        /// Historical adjustment/event source.
        actions: Option<String>,
    },
    /// Execute previously trained models without calling the trainer.
    BacktestResearch {
        /// Optional account-only settings; omitted values reuse the training task account.
        #[serde(default)]
        execution: Option<Box<research::backtest::ResearchExecution>>,
        /// Explicit execution trust for the new account task; it does not invalidate models.
        #[serde(default)]
        event_check_policy: analytics_core::execution::EventCheckPolicy,
        /// Completed training or combined task containing training.json.
        training: Uuid,
        /// Selected execution event version.
        actions: String,
    },
    /// Readiness-only or full annual research.
    Research {
        /// Frozen recipe.
        template: Box<ResearchTemplate>,
        /// Market version ID.
        market: String,
        /// Action version ID.
        actions: Option<String>,
        /// Only perform input audit.
        audit_only: bool,
    },
}
impl WorkflowRequest {
    /// Display operation names without exposing backend details in the user flow.
    pub fn label(&self) -> String {
        match self {
            Self::CollectAnnouncements { .. } => "采集巨潮实施公告证据".into(),
            Self::CollectReference { .. } => "采集日历或当前成分".into(),
            Self::SourceStatus {} => "查看数据来源与优先级".into(),
            Self::SaveSourcePolicy { .. } => "保存数据来源优先级".into(),
            Self::CheckSource { .. } => "小样本验证数据来源".into(),
            Self::RefreshData { request } => if request.preview {
                "预览数据刷新"
            } else {
                "刷新研究数据"
            }
            .into(),
            Self::TrainResearch { .. } => "训练研究模型".into(),
            Self::BacktestResearch { .. } => "使用已有模型回测".into(),
            Self::PreviewResearch { .. } => "预览研究日期和数据需求".into(),
            Self::PrepareResearchData { .. } => "补采研究所需公司行动/公告".into(),
            Self::InspectActions { .. } => "查看公司行动原值与实施证据".into(),
            Self::ReconcileActions { .. } => "自动核对公司行动并发布新版本".into(),
            Self::ReviseActions { .. } => "实施公告对账修订".into(),
            Self::SavePool { .. } => "保存股票池版本".into(),
            Self::PreviewPool { .. } => "预览股票池成员".into(),
            Self::CreateCanonicalDemo {} => "创建Parquet合成演示数据".into(),
            Self::CreateDemo {} => "创建合成演示数据".into(),
            Self::CheckUpdates {} => "检查行情更新".into(),
            Self::DownloadMarket { tag, .. } => format!("下载行情快照 {tag}"),
            Self::TusharePermissions {} => "Tushare接口权限检查".into(),
            Self::CollectActions { .. } => "Python公司行动采集".into(),
            Self::CollectCoefficients { .. } => "独立复权系数采集".into(),
            Self::ComposeMarket { .. } => "组合研究行情版本".into(),
            Self::CollectQuotes { .. } => "独立行情采集".into(),
            Self::CollectMarket { .. } => "Python双源行情采集".into(),
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
fn reconciliation_evidence_root(w: &Workspace, task: Uuid, record: &RunRecord) -> Result<PathBuf> {
    if record.id != task || record.finished_at.is_none() {
        return Err("evidence task must be terminal".into());
    }
    let request = &record.request["request"];
    let resume = request["resume"].as_str().unwrap_or("");
    let staging_id = if resume.is_empty() {
        task.to_string()
    } else {
        resume.to_owned()
    };
    match request["operation"].as_str() {
        Some("collect_actions") => {
            let published = w.resolve(format!("data/python-actions-{task}"));
            Ok(if published.is_dir() {
                published
            } else {
                w.resolve(format!(".jfquant/staging/python-actions-{staging_id}"))
            })
        }
        Some("collect_announcements") => {
            let published = w.resolve(&record.output).join("announcements");
            Ok(if published.is_dir() {
                published
            } else {
                w.resolve(format!(".jfquant/staging/announcements-{staging_id}"))
            })
        }
        _ => Err(
            "evidence task must be a supported company-action or announcement collection".into(),
        ),
    }
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
        source["source_map"] = data
            .details
            .get("source_map")
            .cloned()
            .ok_or("market source map missing; register it again")?;
        source
    } else if kind == "coefficients" {
        serde_json::to_value(analytics_storage::collector::CoefficientBundle::read(
            &path,
        )?)?
    } else {
        ActionBundle::version_content(&path)?
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
        if let Some(hashes) = data.details["evidence_sha256"].as_object() {
            for (name, expected) in hashes {
                if Path::new(name).file_name().and_then(|n| n.to_str()) != Some(name) {
                    return Err("unsafe market evidence name".into());
                }
                let raw = path
                    .parent()
                    .ok_or("missing parent")?
                    .join("raw")
                    .join(name);
                let actual = format!("sha256:{:x}", Sha256::digest(fs::read(raw)?));
                if expected.as_str() != Some(actual.as_str()) {
                    return Err("market evidence changed".into());
                }
            }
        }
        if let Some(expected) = data.details["evidence_tree_sha256"].as_str()
            && expected
                != market_content_hash(
                    &path
                        .parent()
                        .ok_or("market parent missing")?
                        .join("evidence"),
                )?
        {
            return Err("market acquisition evidence changed".into());
        }
        if data.details["content_sha256"] != market_content_hash(&path)? {
            return Err("registered market bytes changed; register it as a new version".into());
        }
        if path.join("market_view.json").is_file() {
            DailyDataset::open(&path)?;
        }
    } else if kind == "coefficients" {
        analytics_storage::collector::CoefficientBundle::read(&path)?;
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
        WorkflowRequest::CollectAnnouncements { request, resume } => {
            if request.provider != analytics_storage::collector::Provider::Akshare {
                return Err(
                    "announcement_source_unsupported: select akshare for CNINFO evidence".into(),
                );
            }
            let staging = w.resolve(format!(
                ".jfquant/staging/announcements-{}",
                resume.unwrap_or(run.id)
            ));
            let manifest =
                analytics_storage::collector::collect_announcements(request, &staging, context)?;
            details = serde_json::from_slice(&fs::read(&manifest)?)?;
            let output = w.resolve(&run.output).join("announcements");
            fs::rename(staging, &output)?;
            details["artifact"] = json!(output.join("announcements.json"));
            details["candidate_only"] = json!(true);
        }
        WorkflowRequest::CollectReference {
            request,
            kind,
            resume,
        } => {
            let staging = w.resolve(format!(
                ".jfquant/staging/reference-{}",
                resume.unwrap_or(run.id)
            ));
            let file =
                analytics_storage::collector::collect_reference(request, &staging, context, kind)?;
            details = serde_json::from_slice(&fs::read(&file)?)?;
            let output = w.resolve(&run.output).join("reference");
            fs::rename(&staging, &output)?;
            details["artifact"] = json!(output.join("reference.json"));
        }
        WorkflowRequest::SourceStatus {} => details = refresh::catalog(w)?,
        WorkflowRequest::SaveSourcePolicy { policy } => {
            policy.save(w)?;
            details = refresh::catalog(w)?;
        }
        WorkflowRequest::CheckSource { provider } => {
            details = refresh::check_source(w, run, *provider, context)?
        }
        WorkflowRequest::RefreshData { request } => {
            let policy = serde_json::from_value(run.request["source_policy"].clone())?;
            details = refresh::execute(w, run, request, &policy, context)?;
            data = details
                .get("market")
                .filter(|v| !v.is_null())
                .map(|v| serde_json::from_value(v.clone()))
                .transpose()?;
        }
        WorkflowRequest::PreviewResearch {
            template,
            market,
            actions,
        } => {
            details = research_requirements_with_actions(w, template, market, actions.as_deref())?;
        }
        WorkflowRequest::PrepareResearchData {
            actions,
            template,
            market,
            announcements,
            resume,
        } => {
            details = research_requirements_with_actions(w, template, market, actions.as_deref())?;
            atomic_json(&w.resolve(&run.output).join("requirements.json"), &details)?;
            details = preparation::prepare(w, run, &details, *announcements, *resume, context)?;
            atomic_json(
                &w.resolve(&run.output).join("execution_readiness.json"),
                &details,
            )?;
            atomic_json(&w.resolve(&run.output).join("outcome.json"), &details)?;
            return Err("data_not_ready: acquisition candidates require source and event-category validation; see execution_readiness.json".into());
        }

        WorkflowRequest::InspectActions {
            actions,
            instrument,
            evidence_manifest,
        } => {
            analytics_core::pool::validate_instrument(instrument)?;
            let source = version(w, actions, "actions")?;
            let book = ActionBundle::read(&w.resolve(source.path))?;
            let events: Vec<_> = book
                .actions
                .iter()
                .filter(|a| a.instrument == *instrument)
                .map(|a| Ok(json!({"original":a,"original_sha256":signature(a)?})))
                .collect::<Result<_>>()?;
            let documents = if let Some(path) = evidence_manifest {
                let path = w.resolve(path).canonicalize()?;
                if !path.starts_with(w.root.canonicalize()?) {
                    return Err("announcement evidence must belong to this workspace".into());
                }
                let docs = analytics_storage::corporate_actions::review::verified_documents(&path)?;
                docs.into_values()
                    .filter(|(d, _)| d["instrument"] == *instrument)
                    .map(|(d, text)| json!({"document":d,"extracted_text":text}))
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            details = json!({"instrument":instrument,"events":events,"source_failure":book.failures.get(instrument),"issues":book.execution_issues.iter().filter(|i| i.instrument == *instrument).map(|i| Ok(json!({"issue":i,"sha256":signature(i)?}))).collect::<Result<Vec<_>>>()?,"reconciliation_evidence":book.raw_sha256.get("reconciliation-v1.json"),"documents":documents});
        }
        WorkflowRequest::ReconcileActions {
            actions,
            evidence_tasks,
            market,
        } => {
            let source = version(w, actions, "actions")?;
            let source_path = w.resolve(&source.path);
            verify_action_evidence(&source_path, &ActionBundle::read(&source_path)?)?;
            let mut roots = Vec::new();
            for task in evidence_tasks {
                let record: RunRecord = serde_json::from_slice(&fs::read(
                    w.resolve(format!(".jfquant/runs/{task}/run.json")),
                )?)?;
                let root = reconciliation_evidence_root(w, *task, &record)?;
                if run.request["reconciliation_evidence"][task.to_string()]["sha256"]
                    != market_content_hash(&root)?
                {
                    return Err("reconciliation evidence changed after request freeze".into());
                }
                roots.push(root);
            }
            let calendar = if let Some(id) = market {
                analytics_storage::daily::DailyDataset::open(
                    w.resolve(version(w, id, "market")?.path),
                )?
                .calendar
            } else {
                Vec::new()
            };
            let staging = w.resolve(format!(".jfquant/staging/action-reconcile-{}", run.id));
            let book = analytics_storage::corporate_actions::reconcile::apply(
                &source_path,
                &roots,
                &calendar,
                &staging,
                context,
            )?;
            context.check()?;
            let output = w.resolve(format!("data/action-reconcile-{}", run.id));
            fs::rename(staging, &output)?;
            data = Some(w.register(&output.join("actions.json"))?);
            details = json!({"reconciliation":output.join("raw/reconciliation-v1.json"),"ordinary_issues":book.execution_issues.iter().filter(|i| !i.skipped_by_engine()).count(),"unsupported_event_warnings":book.execution_issues.iter().filter(|i| i.skipped_by_engine()).count()});
        }
        WorkflowRequest::ReviseActions { actions, request } => {
            let source = version(w, actions, "actions")?;
            let source_path = w.resolve(&source.path);
            let book = ActionBundle::read(&source_path)?;
            let mut request = request.clone();
            request.evidence_manifest = w.resolve(&request.evidence_manifest).canonicalize()?;
            if !request
                .evidence_manifest
                .starts_with(w.root.canonicalize()?)
            {
                return Err("announcement evidence must belong to this workspace".into());
            }
            let staging = w.resolve(format!(".jfquant/staging/action-review-{}", run.id));
            analytics_storage::corporate_actions::review::apply(
                &book,
                source_path.parent().ok_or("missing source root")?,
                &request,
                &staging,
            )?;
            let output = w.resolve(format!("data/action-review-{}", run.id));
            fs::rename(staging, &output)?;
            data = Some(w.register(&output.join("actions.json"))?);
        }
        WorkflowRequest::SavePool { definition } => {
            details = serde_json::to_value(w.save_pool(definition.clone())?)?;
        }
        WorkflowRequest::PreviewPool { id, market, day } => {
            details = w.preview_pool(id, market, *day)?;
        }
        WorkflowRequest::CreateCanonicalDemo {} => {
            let (m, a, t) = demo::create_canonical(w)?;
            let template_path = run.output.join("workflow.json");
            t.write(&w.resolve(&template_path))?;
            details =
                json!({"demo_actions":a.id,"demo_template":t,"demo_template_path":template_path});
            data = Some(m);
        }
        WorkflowRequest::CreateDemo {} => {
            context.report("创建合成演示输入（不是真实行情）")?;
            let (m, a, t) = demo::create(w)?;
            let template_path = run.output.join("workflow.json");
            t.write(&w.resolve(&template_path))?;
            details =
                json!({"demo_actions":a.id,"demo_template":t,"demo_template_path":template_path});
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
        WorkflowRequest::CollectCoefficients { request, resume } => {
            let owner = resume.unwrap_or(run.id);
            let staging = w
                .root
                .join(format!(".jfquant/staging/python-coefficients-{owner}"));
            let manifest =
                analytics_storage::collector::collect_coefficients(request, &staging, context)?;
            let relative = manifest.strip_prefix(&staging)?.to_owned();
            let output = w.root.join(format!("data/python-coefficients-{}", run.id));
            fs::rename(&staging, &output)?;
            data = Some(w.register(&output.join(relative))?);
        }
        WorkflowRequest::ComposeMarket {
            market,
            coefficients,
        } => {
            let base = version(w, market, "market")?;
            let c = version(w, coefficients, "coefficients")?;
            let d = DailyDataset::open(w.resolve(&base.path))?;
            if !d.is_original_qlib() {
                return Err("composition requires an original Qlib snapshot".into());
            }
            let bundle =
                analytics_storage::collector::CoefficientBundle::read(&w.resolve(&c.path))?;
            let staging = w
                .root
                .join(format!(".jfquant/staging/market-view-{}", run.id));
            let root = staging.join("market");
            fs::create_dir_all(root.join("adjustments"))?;
            let mut anchors = BTreeMap::new();
            for id in bundle.records.keys() {
                context.check()?;
                let close = d.raw_field(id, "close")?;
                let first = (0..d.calendar.len())
                    .find(|i| close.get(*i).is_some())
                    .ok_or("raw close anchor missing")?;
                let record = analytics_storage::adjustment::AdjustmentRecord::read(
                    &w.resolve(&c.path)
                        .parent()
                        .ok_or("coefficient parent")?
                        .join("market"),
                    id,
                )?;
                anchors.insert(id.clone(), record.anchor(d.calendar[first])?);
                fs::copy(
                    w.resolve(&c.path)
                        .parent()
                        .unwrap()
                        .join("market/adjustments")
                        .join(format!("{id}.json")),
                    root.join("adjustments").join(format!("{id}.json")),
                )?;
            }
            let view = analytics_storage::market_view::MarketView {
                schema_version: 1,
                base_path: base.path.clone(),
                base_sha256: base.details["content_sha256"]
                    .as_str()
                    .ok_or("base digest missing")?
                    .into(),
                coefficients_path: c.path.clone(),
                coefficients_sha256: analytics_storage::market_view::file_hash(
                    &w.resolve(&c.path),
                )?,
                recovery: "qlib-normalized-price-over-original-factor;cent-tick;shares-v1".into(),
                records: bundle.records,
                anchors,
            };
            atomic_json(&root.join("market_view.json"), &view)?;
            atomic_json(
                &staging.join("jfquant_source.json"),
                &json!({"downloader":"JFQuant analytics-storage","snapshot_verified":true,"independent_download":true,"derived_view":true,"raw_source":"Qlib reconstructed raw quotations","base_version":base,"coefficient_version":c,"normalization":analytics_storage::adjustment::NORMALIZATION}),
            )?;
            DailyDataset::open(&root)?;
            let output = w.root.join(format!("data/market-view-{}", run.id));
            fs::rename(&staging, &output)?;
            data = Some(w.register(&output.join("market"))?);
        }
        WorkflowRequest::TusharePermissions {} => {
            let date = chrono::Utc::now().date_naive();
            let request = analytics_storage::collector::CollectionRequest {
                source_bindings: Default::default(),
                provider: analytics_storage::collector::Provider::Tushare,
                adjustment_source: analytics_storage::adjustment::AdjustmentSource::None,
                start: date,
                end: date,
                instruments: BTreeSet::from(["SH600018".into()]),
                benchmarks: Default::default(),
                universes: Default::default(),
            };
            let staging = w.resolve(format!(".jfquant/staging/tushare-permissions-{}", run.id));
            let path = analytics_storage::collector::check_tushare_permissions(
                &request, &staging, context,
            )?;
            details = serde_json::from_slice(&fs::read(path)?)?;
        }
        WorkflowRequest::CollectActions { request, resume } => {
            let owner = resume.unwrap_or(run.id);
            let staging = w
                .root
                .join(format!(".jfquant/staging/python-actions-{owner}"));
            analytics_storage::collector::collect_actions(request, &staging, context)?;
            let output = w.root.join(format!("data/python-actions-{}", run.id));
            fs::rename(staging, &output)?;
            data = Some(w.register(&output.join("actions.json"))?);
        }
        WorkflowRequest::CollectQuotes { request, resume } => {
            let owner = resume.unwrap_or(run.id);
            let staging = w
                .root
                .join(format!(".jfquant/staging/python-quotes-{owner}"));
            analytics_storage::collector::collect_quotes(request, &staging, context)?;
            let output = w.root.join(format!("data/python-quotes-{}", run.id));
            fs::rename(staging, &output)?;
            data = Some(w.register(&output.join("market"))?);
        }
        WorkflowRequest::CollectMarket { request, resume } => {
            let owner = resume.unwrap_or(run.id);
            let staging = w.root.join(format!(".jfquant/staging/collector-{owner}"));
            analytics_storage::collector::collect(request, &staging, context)?;
            let output = w.root.join(format!("data/collector-{}", run.id));
            fs::rename(staging, &output)?;
            data = Some(w.register(&output.join("market"))?);
        }
        WorkflowRequest::RegisterData { path } => {
            context.report("核验本地数据来源")?;
            data = Some(w.register(&w.resolve(path))?);
        }
        WorkflowRequest::DownloadActions {
            market,
            resume,
            pool_version,
        } => {
            let market_version = version(w, market, "market")?;
            let (pool, _) = w.resolve_pool(pool_version, market)?;
            let symbols = pool.intervals.into_keys().collect::<Vec<_>>();
            let path = w.root.join(format!("data/actions-{}", run.id));
            let owner = resume.unwrap_or(run.id);
            let staging = w.root.join(format!(".jfquant/staging/actions-{owner}"));
            let dataset = DailyDataset::open(w.resolve(&market_version.path))?;
            let request = analytics_storage::collector::CollectionRequest {
                source_bindings: Default::default(),
                provider: analytics_storage::collector::Provider::Akshare,
                adjustment_source: analytics_storage::adjustment::AdjustmentSource::SinaHfq,
                start: *dataset.calendar.first().ok_or("empty market calendar")?,
                end: *dataset.calendar.last().ok_or("empty market calendar")?,
                instruments: symbols
                    .iter()
                    .filter(|s| analytics_core::pool::validate_exchange_symbol(s).is_ok())
                    .cloned()
                    .collect(),
                benchmarks: Default::default(),
                universes: Default::default(),
            };
            context.report("巨潮优先：分红派息，新浪补充上市日及独立系数")?;
            let file = analytics_storage::collector::collect_actions(&request, &staging, context)?;
            let mut book = ActionBundle::read(&file)?;
            for id in symbols.iter().filter(|s| !request.instruments.contains(*s)) {
                book.failures.insert(
                    id.clone(),
                    "provider_locator_missing: historical key has no formal source code".into(),
                );
            }
            fs::write(&file, serde_json::to_vec_pretty(&book)?)?;
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
            if source.schema_version == 6 {
                return Err("format_6_requires_reconciliation: use data reconcile-actions or evidence-backed revise-actions".into());
            }
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
        WorkflowRequest::TrainResearch {
            template,
            market,
            actions,
        } => {
            let m = version(w, market, "market")?;
            let a = actions
                .as_deref()
                .map(|a| version(w, a, "actions"))
                .transpose()?;
            let mut r = research_request(w, run, template, &m, a.as_ref(), false)?;
            r.train_only = true;
            context.report("批量检查训练输入")?;
            crate::research::backtest::run_with_context(&r, context)?;
            details = w.results(run)?;
        }
        WorkflowRequest::BacktestResearch {
            execution,
            training,
            actions,
            event_check_policy,
        } => {
            let saved = w
                .runs()?
                .into_iter()
                .find(|r| r.id == *training)
                .ok_or("training task not found")?;
            let frozen = saved
                .request
                .get("workflow_snapshot")
                .unwrap_or(&saved.request);
            let mut training_market = None;
            for input in frozen["inputs"].as_array().into_iter().flatten() {
                if input["kind"] == "market" {
                    training_market = Some(version(
                        w,
                        input["id"].as_str().ok_or("training market missing")?,
                        "market",
                    )?);
                }
            }
            let market = training_market.ok_or("training market snapshot missing")?;
            let source = w.resolve(&saved.output);
            if let Some(expected) = run.request.get("training_artifact_sha256") {
                let actual = format!(
                    "sha256:{:x}",
                    Sha256::digest(fs::read(source.join("training.json"))?)
                );
                if expected != &json!(actual) {
                    return Err("training_artifact_damaged".into());
                }
            }
            let a = version(w, actions, "actions")?;
            context.report("读取已有模型并回测")?;
            crate::research::backtest::backtest_from_training_with_policy(
                &source,
                &w.resolve(&run.output),
                &w.resolve(a.path),
                Some(&w.resolve(market.path)),
                Some(*event_check_policy),
                execution.as_deref(),
                context,
            )?;
            details = w.results(run)?;
        }
        WorkflowRequest::Research {
            template,
            market,
            actions,
            audit_only,
        } => {
            template.validate()?;
            let m = version(w, market, "market")?;
            let a = actions
                .as_deref()
                .map(|a| version(w, a, "actions"))
                .transpose()?;
            if !audit_only && a.is_none() {
                return Err("execution_actions_required".into());
            }
            details = research(w, run, context, template, &m, a.as_ref(), *audit_only)?;
        }
    }
    context.check()?;
    Ok(WorkflowOutcome { data, details })
}
/// Snapshot selected input versions when creating a run, rather than resolving them later.
pub fn snapshot(workspace: &Workspace, request: &WorkflowRequest) -> Result<Value> {
    match request {
        WorkflowRequest::SaveSourcePolicy { policy } => policy.validate()?,
        WorkflowRequest::RefreshData { request } => request.template.validate()?,
        WorkflowRequest::CollectAnnouncements { request, .. } => {
            request.validate()?;
            if request.provider != analytics_storage::collector::Provider::Akshare {
                return Err(
                    "announcement_source_unsupported: select akshare for CNINFO evidence".into(),
                );
            }
        }
        WorkflowRequest::CollectReference { request, .. }
        | WorkflowRequest::CollectQuotes { request, .. }
        | WorkflowRequest::CollectMarket { request, .. }
        | WorkflowRequest::CollectActions { request, .. }
        | WorkflowRequest::CollectCoefficients { request, .. } => request.validate()?,
        WorkflowRequest::TrainResearch { template, .. }
        | WorkflowRequest::Research { template, .. }
        | WorkflowRequest::PreviewResearch { template, .. }
        | WorkflowRequest::PrepareResearchData { template, .. } => template.validate()?,
        WorkflowRequest::SavePool { definition } => definition.validate()?,
        _ => {}
    };
    let versions = match request {
        WorkflowRequest::RefreshData { request } => {
            let mut v = vec![version_metadata(workspace, &request.market, "market")?];
            if let Some(id) = &request.actions {
                v.push(version_metadata(workspace, id, "actions")?);
            }
            v
        }
        WorkflowRequest::ComposeMarket {
            market,
            coefficients,
        } => vec![
            version_metadata(workspace, market, "market")?,
            version_metadata(workspace, coefficients, "coefficients")?,
        ],
        WorkflowRequest::TrainResearch {
            market, actions, ..
        }
        | WorkflowRequest::Research {
            market, actions, ..
        } => {
            let mut inputs = vec![version_metadata(workspace, market, "market")?];
            if let Some(a) = actions {
                inputs.push(version_metadata(workspace, a, "actions")?);
            }
            inputs
        }
        WorkflowRequest::DownloadActions { market, .. }
        | WorkflowRequest::PreviewResearch { market, .. }
        | WorkflowRequest::PrepareResearchData { market, .. } => {
            vec![version_metadata(workspace, market, "market")?]
        }
        WorkflowRequest::ReconcileActions {
            actions, market, ..
        } => {
            let mut v = vec![version_metadata(workspace, actions, "actions")?];
            if let Some(market) = market {
                v.push(version_metadata(workspace, market, "market")?);
            }
            v
        }
        WorkflowRequest::BacktestResearch { actions, .. }
        | WorkflowRequest::SupplementActions { actions, .. }
        | WorkflowRequest::ReviseActions { actions, .. }
        | WorkflowRequest::InspectActions { actions, .. } => {
            vec![version_metadata(workspace, actions, "actions")?]
        }
        _ => Vec::new(),
    };
    let formula_version = match request {
        WorkflowRequest::Research { template, .. }
        | WorkflowRequest::TrainResearch { template, .. }
        | WorkflowRequest::PreviewResearch { template, .. }
        | WorkflowRequest::PrepareResearchData { template, .. } => {
            template.formula_version.as_str()
        }
        _ => analytics_factor::FORMULA_VERSION,
    };
    let mut frozen = json!({"request":request,"inputs":versions,"app_version":env!("CARGO_PKG_VERSION"),"formula_version":formula_version,"pool_definition": match request { WorkflowRequest::Research{template,..}|WorkflowRequest::TrainResearch{template,..} if !template.pool_version.starts_with("builtin:") => Some(workspace.pool(&template.pool_version)?), _ => None }});
    if let WorkflowRequest::ReconcileActions { evidence_tasks, .. } = request {
        let mut evidence = serde_json::Map::new();
        for task in evidence_tasks {
            let record: RunRecord = serde_json::from_slice(&fs::read(
                workspace.resolve(format!(".jfquant/runs/{task}/run.json")),
            )?)?;
            let root = reconciliation_evidence_root(workspace, *task, &record)?;
            evidence.insert(
                task.to_string(),
                json!({"sha256":market_content_hash(&root)?,"status":record.status}),
            );
        }
        frozen["reconciliation_evidence"] = evidence.into();
    }
    if matches!(request, WorkflowRequest::RefreshData { .. }) {
        frozen["source_policy"] = serde_json::to_value(refresh::SourcePolicy::load(workspace)?)?;
    }
    if let WorkflowRequest::BacktestResearch { training, .. } = request {
        let record = workspace
            .runs()?
            .into_iter()
            .find(|r| r.id == *training)
            .ok_or("training task not found")?;
        let artifact_path = workspace.resolve(record.output).join("training.json");
        if !artifact_path.is_file() {
            return Err(
                "training_artifact_unavailable: selected task has no completed training models"
                    .into(),
            );
        }
        frozen["training_artifact_sha256"] = json!(format!(
            "sha256:{:x}",
            Sha256::digest(fs::read(artifact_path)?)
        ));
    }
    Ok(frozen)
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
    use crate::research::{
        DenseMatrix, LightGbmPredictor, LightGbmTrainer, Predictor, Trainer, TrainingSplit,
    };
    use analytics_engine::{DuckDbEngine, QueryEngine};
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
mod quality_tests;
#[cfg(test)]
mod tests;

fn research_request(
    w: &Workspace,
    run: &RunRecord,
    template: &ResearchTemplate,
    market: &DataVersion,
    actions: Option<&DataVersion>,
    audit_only: bool,
) -> Result<ResearchRequest> {
    template.validate()?;
    let mut r = ResearchRequest::new(w.resolve(&market.path), w.resolve(&run.output));
    r.config = template.config.clone();
    r.price_basis = template.price_basis;
    r.factor_set = template.factor_set;
    r.quality_policy = template.quality_policy.clone();
    r.schedule = template.schedule.clone();
    r.action_policy = template.action_policy.clone();
    r.event_check_policy = template.event_check_policy;
    r.market_rules = template.market_rules.clone();
    r.initial_cash_cny = template.initial_cash_cny;
    r.dividend_withholding_rate = template.dividend_withholding_rate;
    r.corporate_actions = actions.map(|a| w.resolve(&a.path));
    r.audit_only = audit_only;
    r.benchmark = template.benchmark.clone();
    r.pool_exit_policy = template.pool_exit_policy;
    let (pool, frozen) = w.resolve_pool(&template.pool_version, &market.id)?;
    r.pool_signature = signature(&frozen)?;
    r.pool = Some(pool);
    r.universe_id = template.pool_version.clone();
    atomic_json(&w.resolve(&run.output).join("pool.json"), &frozen)?;
    Ok(r)
}
pub(crate) fn research(
    w: &Workspace,
    run: &RunRecord,
    context: &TaskContext,
    template: &ResearchTemplate,
    market: &DataVersion,
    actions: Option<&DataVersion>,
    audit_only: bool,
) -> Result<Value> {
    let r = research_request(w, run, template, market, actions, audit_only)?;
    context.report("检查训练输入；训练完成后按需回测")?;
    crate::research::backtest::run_with_context(&r, context)?;
    w.results(run)
}

/// Shared pre-run schedule and security requirements; does not allocate native model handles.
pub fn research_requirements(w: &Workspace, t: &ResearchTemplate, market: &str) -> Result<Value> {
    research_requirements_with_actions(w, t, market, None)
}
/// Include reviewed transitive successors while retaining an explicit not-run when no book is selected.
pub fn research_requirements_with_actions(
    w: &Workspace,
    t: &ResearchTemplate,
    market: &str,
    actions: Option<&str>,
) -> Result<Value> {
    t.validate()?;
    let selected_market = version(w, market, "market")?;
    let mut dataset =
        analytics_storage::daily::DailyDataset::open(w.resolve(selected_market.path))?;
    let folds = t
        .schedule
        .resolve_with_warmup(&dataset.calendar, t.factor_set.warmup_sessions())?;
    let (pool, frozen) = w.resolve_pool(&t.pool_version, market)?;
    let lo = folds
        .iter()
        .map(|f| f.warmup_start)
        .min()
        .ok_or("empty schedule")?;
    let hi = folds.last().unwrap().actual.test.end;
    dataset.restrict_reads(analytics_core::DateRange { start: lo, end: hi })?;
    let benchmark = t.benchmark.clone();
    let mut symbols: std::collections::BTreeSet<_> = pool
        .intervals
        .iter()
        .filter(|(_, rs)| rs.iter().any(|r| r.start <= hi && r.end >= lo))
        .map(|(id, _)| id.clone())
        .collect();
    if !dataset.is_price_index(&benchmark).unwrap_or(false) {
        symbols.insert(benchmark.clone());
    }
    let successor_check = match actions {
        Some(id) => match version(w, id, "actions")
            .and_then(|v| Ok(ActionBundle::read(&w.resolve(v.path))?))
        {
            Ok(book) => {
                let seeds = pool
                    .intervals
                    .iter()
                    .filter(|(_, rs)| {
                        rs.iter()
                            .any(|r| r.end >= folds[0].actual.test.start && r.start <= hi)
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                symbols.extend(book.required_symbols(&seeds, folds[0].actual.test.start, hi));
                json!({"status":"passed","action_version":id})
            }
            Err(e) => json!({"status":"failed","action_version":id,"message":e.to_string()}),
        },
        None => {
            json!({"status":"not_run","message":"select an action version to enumerate successors"})
        }
    };
    let mut dependencies = std::collections::BTreeMap::new();
    for id in &symbols {
        let history = analytics_core::DateRange { start: lo, end: hi };
        let member = pool.intervals.get(id).cloned().unwrap_or_default();
        dependencies.insert(id,json!({"instrument":id,"required_history":history,"membership":member,"purpose":if *id==benchmark{"benchmark"}else{"features_and_account"}}));
    }
    let mut preview_pool = pool.clone();
    preview_pool.intervals.retain(|_, rs| {
        rs.retain(|r| r.end >= lo && r.start <= hi);
        !rs.is_empty()
    });
    let quality = crate::research::backtest::quality::qualify(
        &dataset,
        &preview_pool,
        &t.quality_policy,
        &analytics_core::DateRange { start: lo, end: hi },
        &t.config.factor_ids,
    )?;
    let sample_readiness = crate::research::backtest::preview_samples_with_policy(
        &dataset,
        &quality.effective_pool,
        &folds,
        &t.quality_policy,
    )?;
    Ok(
        json!({"quality":quality,"dependencies":dependencies,"successor_check":successor_check,"resolved_folds":folds,"sample_readiness":sample_readiness,"securities":symbols,"history_start":lo,"execution_end":hi,"pool":frozen,"schema_version":3,"source_map":dataset.source_map(),"benchmark":benchmark,"unknown_cross_section_before":pool.coverage.start}),
    )
}
