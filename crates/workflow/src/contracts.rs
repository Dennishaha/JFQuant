//! Contracts.
use crate::Result;
use crate::research::backtest::ResearchConfig;
use analytics_core::market_rules::MarketRulesPreset;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};
use uuid::Uuid;

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
        self.config.factor_refs.clear();
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
    /// Resolve a frozen mixed selection, retaining the legacy builtin path.
    pub fn factor_registry(&self) -> Result<analytics_factor::FactorRegistry> {
        Ok(analytics_factor::FactorRegistry::resolve(
            self.factor_set,
            &self.config.factor_refs,
            &self.config.factor_ids,
        )?)
    }
    /// Exact selected definition version used in task and model signatures.
    pub fn resolved_formula_version(&self) -> Result<String> {
        if self.config.factor_refs.is_empty() {
            return Ok(analytics_factor::alpha158::version(self.factor_set).into());
        }
        Ok(self
            .factor_registry()?
            .selection_version(&self.config.factor_ids)?)
    }
    /// Maximum per-factor history, including recursive seed history.
    pub fn warmup_sessions(&self) -> usize {
        analytics_factor::selection::warmup_sessions(self.factor_set, &self.config.factor_refs)
    }
    /// Validate before saving or starting a task.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 10
            || self.id.trim().is_empty()
            || self.formula_version != self.resolved_formula_version()?
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
        let registry = self.factor_registry()?;
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
/// An operation selectable in the data or research UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowRequest {
    /// Independent fit-only models and cross-pool prediction/account execution.
    Independent {
        /// Shared App/CLI worker operation.
        request: Box<research::backtest::independent::Job>,
    },
    /// Fetch bounded implementation evidence independently of model/universe preparation.
    CollectAnnouncements {
        /// Explicit CNINFO collection scope, using the independent announcement collector.
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
    /// Shared evidence maintenance for agents and desktop.
    Evidence {
        /// Versioned operation and plain inputs.
        request: crate::action_evidence::EvidenceRequest,
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
    /// Replay hash-verified published predictions without loading trained models.
    BacktestPredictions {
        /// Successful native backtest containing published predictions.
        source: Uuid,
        /// Immutable event snapshot for the new experiment.
        actions: String,
        /// Explicit rights participation, timing and funding policy.
        rights_policy: analytics_core::rights::Policy,
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
            Self::Independent { .. } => "独立模型训练与回测".into(),
            Self::TrainResearch { .. } => "训练研究模型".into(),
            Self::BacktestResearch { .. } => "使用已有模型回测".into(),
            Self::BacktestPredictions { .. } => "使用已发布预测进行配股对照回测".into(),
            Self::PreviewResearch { .. } => "预览研究日期和数据需求".into(),
            Self::PrepareResearchData { .. } => "补采研究所需公司行动/公告".into(),
            Self::Evidence { .. } => "公司行动证据维护".into(),
            Self::InspectActions { .. } => "查看公司行动原值与实施证据".into(),
            Self::ReconcileActions { .. } => "自动核对公司行动并发布新版本".into(),
            Self::ReviseActions { .. } => "实施公告对账修订".into(),
            Self::SavePool { .. } => "保存股票池版本".into(),
            Self::PreviewPool { .. } => "预览股票池成员".into(),
            Self::CreateCanonicalDemo {} => "创建Parquet合成演示数据".into(),
            Self::CreateDemo {} => "创建合成演示数据".into(),
            Self::CheckUpdates {} => "检查行情更新".into(),
            Self::DownloadMarket { tag, .. } => format!("下载行情快照 {tag}"),
            Self::CollectActions { .. } => "公司行动采集".into(),
            Self::CollectCoefficients { .. } => "独立复权系数采集".into(),
            Self::ComposeMarket { .. } => "组合研究行情版本".into(),
            Self::CollectQuotes { .. } => "独立行情采集".into(),
            Self::CollectMarket { .. } => "行情采集".into(),
            Self::RegisterData { .. } => "核验登记数据版本".into(),
            Self::DownloadActions { .. } => "下载公司行动历史".into(),
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
use crate::{profiles, refresh, research};
