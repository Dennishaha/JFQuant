//! Clap frontend only: task execution and lifecycle live in analytics-workflow.
use analytics_core::{ExportFormat, ExportRequest, PageRequest, SortKey};
use analytics_workflow::{
    ResearchTemplate, WorkflowRequest, Workspace,
    service::{Client, Rpc, ServiceError, TaskRequest},
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum Format {
    #[default]
    Json,
    Text,
}
#[derive(Debug, Parser)]
#[command(
    name = "jfquant",
    version,
    about = "JFQuant local research platform; commands share the App backend"
)]
pub struct Cli {
    #[arg(long, global = true)]
    pub workspace: Option<PathBuf>,
    #[arg(long, global = true, value_enum, default_value = "json")]
    pub format: Format,
    #[command(subcommand)]
    pub command: Command,
}
#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
    #[command(subcommand)]
    Data(DataCommand),
    #[command(subcommand)]
    Pools(PoolCommand),
    #[command(subcommand)]
    Query(QueryCommand),
    #[command(subcommand)]
    Factors(FactorCommand),
    #[command(subcommand)]
    Model(ModelCommand),
    #[command(subcommand)]
    Research(Box<ResearchCommand>),
    #[command(subcommand)]
    Templates(TemplateCommand),
    #[command(subcommand)]
    Jobs(JobCommand),
    #[command(subcommand)]
    Service(ServiceCommand),
    /// Submit serialized shared TaskRequest.
    Run {
        #[arg(long)]
        request: PathBuf,
    },
    /// Validate packaged DuckDB and LightGBM without opening the GUI.
    SelfCheck {
        #[arg(long)]
        output: PathBuf,
    },
}
#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    Create { path: PathBuf },
    Show,
}
#[derive(Debug, Subcommand)]
pub enum DataCommand {
    #[command(subcommand)]
    Sources(SourceCommand),
    Refresh(RefreshArgs),
    /// Inspect acquisition progress and optional daily historical-member quote coverage.
    Coverage {
        #[arg(long)]
        refresh: Uuid,
        #[arg(long)]
        inspect_bars: bool,
        /// Optional JSON array of additional native workspace/task shard sources.
        #[arg(long)]
        sources_manifest: Option<PathBuf>,
    },
    Compose {
        #[arg(long)]
        market: String,
        #[arg(long)]
        coefficients: String,
    },
    Scan {
        path: PathBuf,
    },
    Collect {
        #[arg(long, value_enum, default_value = "market")]
        kind: CollectionKind,
        #[arg(long, value_enum, default_value = "hithink")]
        provider: CollectionProvider,
        #[arg(long, value_enum)]
        adjustment_source: Option<CollectionAdjustment>,
        #[arg(long)]
        start: String,
        #[arg(long)]
        end: String,
        #[arg(long, value_delimiter = ',', required = true)]
        instruments: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        benchmarks: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        universes: Vec<String>,
        #[arg(long)]
        resume: Option<Uuid>,
    },
    InspectActions {
        #[arg(long)]
        actions: String,
        #[arg(long)]
        instrument: String,
        #[arg(long)]
        evidence_manifest: Option<PathBuf>,
    },
    ReconcileActions {
        #[arg(long)]
        actions: String,
        #[arg(long = "evidence-task")]
        evidence_tasks: Vec<Uuid>,
        #[arg(long)]
        market: Option<String>,
    },
    ReviseActions {
        #[arg(long)]
        actions: String,
        #[arg(long)]
        request: PathBuf,
    },
    Versions,
    CheckUpdates,
    Download {
        #[arg(long)]
        tag: String,
        #[arg(long)]
        resume: Option<Uuid>,
    },
    Register {
        path: PathBuf,
    },
    Actions {
        #[arg(long, default_value = "builtin:csi500")]
        pool: String,
        #[arg(long)]
        market: String,
        #[arg(long)]
        resume: Option<Uuid>,
    },
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CollectionKind {
    /// CNINFO implementation documents and review candidates; not account-ready events.
    Announcements,
    /// Independent exchange calendar, without daily quote downloads.
    Calendar,
    /// Current dated members, never historical backfill.
    Members,
    /// Quotations only; independent coefficient availability is checked separately.
    Quotes,
    /// Independent source factors, without quote downloads.
    Coefficients,
    Market,
    Actions,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CollectionProvider {
    Tdxaidata,
    Hithink,
    Cninfo,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CollectionAdjustment {
    TdxForward,
    HithinkForward,
    None,
}
#[derive(Debug, Subcommand)]
pub enum SourceCommand {
    List,
    Configure {
        #[arg(long, value_enum)]
        provider: CollectionProvider,
        #[arg(long, required = true)]
        key_stdin: bool,
    },
    Check {
        #[arg(long, value_enum, default_value = "hithink")]
        provider: CollectionProvider,
    },
    /// Save validated source priorities, separate from factor and universe profiles.
    Policy {
        #[arg(long)]
        file: PathBuf,
    },
}
impl From<CollectionProvider> for analytics_storage::collector::Provider {
    fn from(value: CollectionProvider) -> Self {
        match value {
            CollectionProvider::Tdxaidata => Self::Tdxaidata,
            CollectionProvider::Hithink => Self::Hithink,
            CollectionProvider::Cninfo => Self::Cninfo,
        }
    }
}
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum RefreshModeValue {
    #[default]
    Incremental,
    Repair,
    Full,
    Actions,
}
#[derive(Debug, Args)]
pub struct RefreshArgs {
    #[arg(long)]
    pub template: PathBuf,
    #[arg(long)]
    pub market: String,
    #[arg(long)]
    pub actions: Option<String>,
    #[arg(long, value_enum, default_value = "incremental")]
    pub mode: RefreshModeValue,
    #[arg(long, value_enum)]
    pub provider: Option<CollectionProvider>,
    #[arg(long)]
    pub end: Option<String>,
    #[arg(long)]
    pub preview: bool,
    #[arg(long)]
    pub resume: Option<Uuid>,
    /// Number of independently checkpointed security workers (1..8).
    #[arg(long, default_value_t = 3)]
    pub workers: usize,
    /// Import completed compatible security checkpoints; collectors revalidate all hashes.
    #[arg(long = "reuse-shards-from")]
    pub reuse_shards_from: Vec<Uuid>,
    /// JSON array of task UUIDs or {workspace,task_id} native shard sources.
    #[arg(long)]
    pub reuse_shards_manifest: Option<PathBuf>,
}
#[derive(Debug, Subcommand)]
pub enum PoolCommand {
    List,
    Create {
        #[arg(long)]
        definition: PathBuf,
    },
    Validate {
        #[arg(long)]
        definition: PathBuf,
    },
    Import {
        path: PathBuf,
    },
    Export {
        id: String,
        #[arg(long)]
        output: PathBuf,
    },
    Versions,
    Preview {
        id: String,
        #[arg(long)]
        market: String,
        #[arg(long)]
        date: String,
    },
}
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub struct Sql {
    #[arg(long)]
    pub sql: Option<String>,
    #[arg(long)]
    pub sql_file: Option<PathBuf>,
}
impl Sql {
    fn read(&self, root: &Path) -> Result<String, ServiceError> {
        match (&self.sql, &self.sql_file) {
            (Some(s), None) => Ok(s.trim().trim_end_matches(';').into()),
            (None, Some(p)) => fs::read_to_string(resolve(root, p))
                .map(|s| s.trim().trim_end_matches(';').into())
                .map_err(invalid),
            _ => Err(invalid("specify --sql or --sql-file")),
        }
    }
}
#[derive(Debug, Subcommand)]
pub enum QueryCommand {
    Describe {
        #[command(flatten)]
        source: Sql,
    },
    Run {
        #[command(flatten)]
        source: Sql,
        #[arg(long,default_value_t=500,value_parser=clap::value_parser!(u32).range(1..=5000))]
        page_size: u32,
        #[arg(long)]
        sort: Vec<String>,
        #[arg(long)]
        descending: bool,
    },
    Next {
        #[arg(long)]
        query_id: Uuid,
        #[arg(long)]
        cursor_json: String,
    },
    Export {
        #[command(flatten)]
        source: Sql,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, value_enum, default_value = "parquet")]
        file_format: FileFormat,
    },
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum FileFormat {
    Parquet,
    Csv,
}
#[derive(Debug, Subcommand)]
pub enum FactorCommand {
    List {
        #[arg(long, value_enum, default_value = "gtja191")]
        factor_set: FactorSetValue,
    },
    Run {
        #[arg(long)]
        request: PathBuf,
    },
}
#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    Train {
        #[arg(long)]
        cache: PathBuf,
        #[arg(long)]
        universe: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    Artifacts {
        id: Uuid,
    },
}
/// Date overrides are translated to the same public Schedule used by App.
#[derive(Debug, Default, Args)]
pub struct DateOptions {
    #[arg(long,requires_all=["train_end","test_start","test_end"],conflicts_with="schedule")]
    pub train_start: Option<String>,
    #[arg(long, requires = "train_start")]
    pub train_end: Option<String>,
    #[arg(long, requires = "train_start")]
    pub test_start: Option<String>,
    #[arg(long, requires = "train_start")]
    pub test_end: Option<String>,
    #[arg(long,requires_all=["train_start","validation_end"],conflicts_with="no_validation")]
    pub validation_start: Option<String>,
    #[arg(long, requires = "validation_start")]
    pub validation_end: Option<String>,
    #[arg(long)]
    pub no_validation: bool,
    #[arg(long, requires = "step_unit")]
    pub step: Option<u32>,
    #[arg(long, value_enum, requires = "step")]
    pub step_unit: Option<DateUnit>,
    #[arg(long, requires = "step")]
    pub expanding: bool,
    #[arg(long, requires = "step")]
    pub until: Option<String>,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DateUnit {
    Days,
    Months,
    Years,
    TradingDays,
}

/// CLI-only overrides; business validation belongs to the common policy.
#[derive(Debug, Default, Args)]
pub struct QualityOptions {
    /// Replace only the independently saved factor profile.
    #[arg(long)]
    pub factor_profile: Option<PathBuf>,
    /// Replace only the independently saved universe/benchmark selection.
    #[arg(long)]
    pub target_profile: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub price_basis: Option<PriceBasisValue>,
    #[arg(long, value_enum)]
    pub factor_set: Option<FactorSetValue>,
    #[arg(long, value_enum)]
    pub strictness: Option<StrictnessValue>,
    #[arg(long, value_enum)]
    pub event_check: Option<EventCheckValue>,
    #[arg(long)]
    pub min_valid_feature_ratio: Option<f64>,
    #[arg(long)]
    pub min_labeled_rows: Option<usize>,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum EventCheckValue {
    Provider,
    Audited,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PriceBasisValue {
    Verified,
    Provider,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum FactorSetValue {
    #[value(name = "gtja191")]
    Gtja191,
    #[value(name = "alpha158")]
    Alpha158,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StrictnessValue {
    #[value(name = "strict")]
    Strict,
    #[value(name = "allow_missing")]
    AllowMissing,
    #[value(name = "qualified_only")]
    QualifiedOnly,
}
impl QualityOptions {
    fn apply(&self, t: &mut ResearchTemplate) -> std::result::Result<(), ServiceError> {
        use analytics_core::research_quality::{FactorSet, PriceBasis, ResearchStrictness};
        if let Some(policy) = self.event_check {
            t.event_check_policy = match policy {
                EventCheckValue::Provider => analytics_core::execution::EventCheckPolicy::Provider,
                EventCheckValue::Audited => analytics_core::execution::EventCheckPolicy::Audited,
            };
        }
        if let Some(basis) = self.price_basis {
            t.price_basis = match basis {
                PriceBasisValue::Verified => PriceBasis::Verified,
                PriceBasisValue::Provider => PriceBasis::Provider,
            };
        }
        if let Some(set) = self.factor_set {
            t.select_factor_set(match set {
                FactorSetValue::Gtja191 => FactorSet::Gtja191,
                FactorSetValue::Alpha158 => FactorSet::Alpha158,
            });
        }
        if let Some(mode) = self.strictness {
            t.quality_policy.strictness = match mode {
                StrictnessValue::Strict => ResearchStrictness::Strict,
                StrictnessValue::AllowMissing => ResearchStrictness::AllowMissing,
                StrictnessValue::QualifiedOnly => ResearchStrictness::QualifiedOnly,
            };
        }
        if let Some(ratio) = self.min_valid_feature_ratio {
            t.quality_policy.min_valid_feature_ratio = ratio;
        }
        if let Some(rows) = self.min_labeled_rows {
            t.quality_policy.min_labeled_rows = rows;
        }
        t.quality_policy.validate().map_err(invalid)
    }
}

#[derive(Debug, Subcommand)]
pub enum ResearchCommand {
    /// Replay a completed task's published scores without loading or training models.
    ReplayPredictions {
        #[arg(long)]
        source: Uuid,
        #[arg(long)]
        actions: String,
        #[arg(long, default_value = "abandon", value_parser = ["participate", "abandon"])]
        participation: String,
        #[arg(long, default_value = "first_day", value_parser = ["first_day", "last_day"])]
        attempt: String,
        #[arg(long, default_value = "partial", value_parser = ["partial", "all_or_abandon", "stop"])]
        insufficient_cash: String,
    },
    /// Verify completed result artifacts without rerunning training or simulation.
    Verify {
        #[arg(long)]
        task: Uuid,
    },
    Demo,
    DemoParquet,
    Preview {
        #[arg(long)]
        actions: Option<String>,
        #[arg(long)]
        template: PathBuf,
        #[arg(long)]
        market: String,
        #[arg(long)]
        schedule: Option<PathBuf>,
        #[command(flatten)]
        dates: DateOptions,
        #[command(flatten)]
        quality: QualityOptions,
    },
    PrepareData {
        #[arg(long)]
        actions: Option<String>,
        #[arg(long)]
        schedule: Option<PathBuf>,
        #[command(flatten)]
        dates: DateOptions,
        #[command(flatten)]
        quality: QualityOptions,
        #[arg(long)]
        template: PathBuf,
        #[arg(long)]
        market: String,
        #[arg(long)]
        announcements: bool,
        #[arg(long)]
        resume: Option<Uuid>,
    },

    Check {
        #[arg(long)]
        template: PathBuf,
        #[arg(long)]
        schedule: Option<PathBuf>,
        #[command(flatten)]
        dates: DateOptions,
        #[command(flatten)]
        quality: QualityOptions,
        #[arg(long)]
        market: String,
        #[arg(long)]
        actions: Option<String>,
    },
    Train {
        #[arg(long)]
        template: PathBuf,
        #[arg(long)]
        schedule: Option<PathBuf>,
        #[command(flatten)]
        dates: DateOptions,
        #[command(flatten)]
        quality: QualityOptions,
        #[arg(long)]
        market: String,
        #[arg(long)]
        actions: Option<String>,
    },
    Backtest {
        /// Apply only account/portfolio settings from this template; retain trained model inputs.
        #[arg(long)]
        template: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "provider")]
        event_check: EventCheckValue,
        #[arg(long)]
        training: Uuid,
        #[arg(long)]
        actions: String,
    },
    Run {
        #[arg(long)]
        template: PathBuf,
        #[arg(long)]
        schedule: Option<PathBuf>,
        #[command(flatten)]
        dates: DateOptions,
        #[command(flatten)]
        quality: QualityOptions,
        #[arg(long)]
        market: String,
        #[arg(long)]
        actions: String,
    },
}
#[derive(Debug, Subcommand)]
pub enum TemplateCommand {
    /// Export the feature selection independently from a workflow.
    Factors {
        #[arg(long)]
        template: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Export the pool/benchmark selection independently from a workflow.
    Target {
        #[arg(long)]
        template: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Combine reusable workflow parameters with separately selected profiles.
    Compose {
        #[arg(long)]
        workflow: PathBuf,
        #[arg(long)]
        factors: PathBuf,
        #[arg(long)]
        target: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    List,
    Default {
        #[arg(long, value_enum, default_value = "gtja191")]
        factor_set: FactorSetValue,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value="csi500", value_parser=["csi300","csi500"])]
        universe: String,
    },
    Validate {
        path: PathBuf,
    },
    Save {
        path: PathBuf,
    },
    Import {
        path: PathBuf,
    },
    Export {
        path: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}
#[derive(Debug, Subcommand)]
pub enum JobCommand {
    List,
    Status {
        id: Uuid,
    },
    Wait {
        id: Uuid,
        #[arg(long)]
        timeout: Option<u64>,
    },
    Logs {
        id: Uuid,
    },
    Cancel {
        id: Uuid,
    },
    Results {
        id: Uuid,
    },
    Import {
        path: PathBuf,
    },
}
#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    Status,
    Stop,
    Restart,
    #[command(hide = true)]
    Serve,
}
fn invalid(e: impl ToString) -> ServiceError {
    ServiceError::new("invalid_request", e)
}
fn resolve(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.into()
    } else {
        root.join(path)
    }
}
fn read<T: serde::de::DeserializeOwned>(root: &Path, path: &Path) -> Result<T, ServiceError> {
    serde_json::from_slice(&fs::read(resolve(root, path)).map_err(invalid)?).map_err(invalid)
}
fn workflow(request: WorkflowRequest) -> TaskRequest {
    TaskRequest::Workflow {
        request: Box::new(request),
    }
}
pub fn exit_code(error: &ServiceError) -> i32 {
    match error.code.as_str() {
        "invalid_request" | "workspace_missing" => 2,
        "blocked" => 3,
        "busy" => 4,
        "timeout" => 124,
        "cancelled" => 130,
        _ => 1,
    }
}
pub fn wait(client: &Client, id: Uuid, timeout: Option<u64>) -> Result<Value, ServiceError> {
    client.wait(id, timeout)
}

pub fn execute(cli: Cli) -> Result<Value, ServiceError> {
    if let Command::Workspace(WorkspaceCommand::Create { path }) = &cli.command {
        let w = Workspace::open(path).map_err(invalid)?;
        return Ok(json!({"workspace":w.root()}));
    }
    if let Command::Factors(FactorCommand::List { factor_set }) = &cli.command {
        let set = match factor_set {
            FactorSetValue::Gtja191 => analytics_core::research_quality::FactorSet::Gtja191,
            FactorSetValue::Alpha158 => analytics_core::research_quality::FactorSet::Alpha158,
        };
        let registry = analytics_factor::FactorRegistry::for_set(set);
        return Ok(
            json!({"factor_set":set,"formula_version":analytics_factor::alpha158::version(set),"factors":registry.all().collect::<Vec<_>>(),"audit":if matches!(set,analytics_core::research_quality::FactorSet::Gtja191){serde_json::to_value(analytics_factor::audit::entries()).map_err(invalid)?}else{json!([])}}),
        );
    }
    if let Command::SelfCheck { output } = &cli.command {
        let output = output.clone();
        return std::thread::spawn(move || {
            analytics_workflow::self_check(&output).map_err(invalid)
        })
        .join()
        .map_err(|_| invalid("native self-check panicked"))?;
    }
    if let Command::Data(DataCommand::Sources(SourceCommand::Configure { provider, .. })) =
        &cli.command
    {
        use std::io::Read;
        let name = match provider {
            CollectionProvider::Tdxaidata => "tdxaidata",
            CollectionProvider::Hithink => "hithink",
            _ => return Err(invalid("credential_provider_unsupported")),
        };
        let mut token = String::new();
        std::io::stdin()
            .take(4096)
            .read_to_string(&mut token)
            .map_err(invalid)?;
        analytics_storage::credentials::save_source(name, token.trim()).map_err(invalid)?;
        return Ok(json!({"credential":name,"stored":true,"storage":"private_local_file"}));
    }
    let root = analytics_workflow::service::resolve_workspace(cli.workspace)?;
    if matches!(cli.command, Command::Service(ServiceCommand::Serve)) {
        analytics_workflow::service::serve(&root)?;
        return Ok(Value::Null);
    }
    let client = Client::connect(&root)?;
    client.ensure(None)?;
    let mut synchronous = false;
    let task = match cli.command {
        Command::Workspace(WorkspaceCommand::Show) | Command::Service(ServiceCommand::Status) => {
            return client.call(Rpc::Status);
        }
        Command::Service(ServiceCommand::Stop) => return client.call(Rpc::Stop),
        Command::Service(ServiceCommand::Restart) => return client.call(Rpc::Restart),
        Command::Data(command) => match command {
            DataCommand::Sources(SourceCommand::List) => {
                synchronous = true;
                workflow(WorkflowRequest::SourceStatus {})
            }
            DataCommand::Sources(SourceCommand::Policy { file }) => {
                workflow(WorkflowRequest::SaveSourcePolicy {
                    policy: read(&client.root, &file)?,
                })
            }
            DataCommand::Sources(SourceCommand::Check { provider }) => {
                workflow(WorkflowRequest::CheckSource {
                    provider: provider.into(),
                })
            }
            DataCommand::Sources(SourceCommand::Configure { .. }) => {
                unreachable!("handled before connecting")
            }
            DataCommand::Coverage {
                refresh,
                inspect_bars,
                sources_manifest,
            } => {
                let sources = sources_manifest
                    .map(|path| read(&client.root, &path))
                    .transpose()?
                    .unwrap_or_default();
                return client.call(Rpc::RefreshCoverage {
                    id: refresh,
                    inspect_bars,
                    sources,
                });
            }
            DataCommand::Refresh(args) => {
                let mut reuse_shards_from: Vec<_> = args
                    .reuse_shards_from
                    .into_iter()
                    .map(analytics_workflow::refresh::ShardSource::Task)
                    .collect();
                if let Some(path) = args.reuse_shards_manifest {
                    reuse_shards_from.extend(
                        read::<Vec<analytics_workflow::refresh::ShardSource>>(&client.root, &path)?,
                    );
                }
                let template = ResearchTemplate::read(&resolve(&client.root, &args.template))
                    .map_err(invalid)?;
                synchronous = args.preview;
                workflow(WorkflowRequest::RefreshData {
                    request: Box::new(analytics_workflow::refresh::RefreshRequest {
                        template: Box::new(template),
                        market: args.market,
                        actions: args.actions,
                        mode: match args.mode {
                            RefreshModeValue::Incremental => {
                                analytics_workflow::refresh::RefreshMode::Incremental
                            }
                            RefreshModeValue::Repair => {
                                analytics_workflow::refresh::RefreshMode::Repair
                            }
                            RefreshModeValue::Full => {
                                analytics_workflow::refresh::RefreshMode::Full
                            }
                            RefreshModeValue::Actions => {
                                analytics_workflow::refresh::RefreshMode::Actions
                            }
                        },
                        provider: args.provider.map(Into::into),
                        end: args.end.map(|s| s.parse().map_err(invalid)).transpose()?,
                        preview: args.preview,
                        resume: args.resume,
                        workers: args.workers,
                        reuse_shards_from,
                    }),
                })
            }
            DataCommand::Compose {
                market,
                coefficients,
            } => workflow(WorkflowRequest::ComposeMarket {
                market,
                coefficients,
            }),
            DataCommand::InspectActions {
                actions,
                instrument,
                evidence_manifest,
            } => workflow(WorkflowRequest::InspectActions {
                actions,
                instrument,
                evidence_manifest,
            }),
            DataCommand::ReconcileActions {
                actions,
                evidence_tasks,
                market,
            } => workflow(WorkflowRequest::ReconcileActions {
                actions,
                evidence_tasks,
                market,
            }),
            DataCommand::ReviseActions { actions, request } => {
                workflow(WorkflowRequest::ReviseActions {
                    actions,
                    request: read(&client.root, &request)?,
                })
            }
            DataCommand::Collect {
                kind,
                provider,
                adjustment_source,
                start,
                end,
                instruments,
                benchmarks,
                universes,
                resume,
            } => {
                use analytics_storage::adjustment::AdjustmentSource;
                let adjustment_source = match adjustment_source {
                    Some(CollectionAdjustment::TdxForward) => AdjustmentSource::TdxForward,
                    Some(CollectionAdjustment::HithinkForward) => AdjustmentSource::HithinkForward,
                    Some(CollectionAdjustment::None) => AdjustmentSource::None,
                    None => match provider {
                        CollectionProvider::Tdxaidata => AdjustmentSource::TdxForward,
                        CollectionProvider::Hithink => AdjustmentSource::HithinkForward,
                        CollectionProvider::Cninfo => AdjustmentSource::None,
                    },
                };
                let mut request = analytics_storage::collector::CollectionRequest {
                    source_bindings: Default::default(),
                    adjustment_source,
                    provider: match provider {
                        CollectionProvider::Tdxaidata => {
                            analytics_storage::collector::Provider::Tdxaidata
                        }
                        CollectionProvider::Hithink => {
                            analytics_storage::collector::Provider::Hithink
                        }
                        CollectionProvider::Cninfo => {
                            analytics_storage::collector::Provider::Cninfo
                        }
                    },
                    start: start.parse().map_err(invalid)?,
                    end: end.parse().map_err(invalid)?,
                    instruments: instruments.into_iter().collect(),
                    benchmarks: benchmarks.into_iter().collect(),
                    universes: universes.into_iter().collect(),
                };
                if matches!(
                    kind,
                    CollectionKind::Actions | CollectionKind::Announcements
                ) {
                    request.instruments = request
                        .instruments
                        .difference(&request.benchmarks)
                        .cloned()
                        .collect();
                    request.benchmarks.clear();
                    request.universes.clear();
                }

                request.validate().map_err(invalid)?;
                workflow(match kind {
                    CollectionKind::Announcements => {
                        WorkflowRequest::CollectAnnouncements { request, resume }
                    }
                    CollectionKind::Calendar | CollectionKind::Members => {
                        WorkflowRequest::CollectReference {
                            request,
                            kind: if matches!(kind, CollectionKind::Calendar) {
                                "calendar"
                            } else {
                                "members"
                            }
                            .into(),
                            resume,
                        }
                    }
                    CollectionKind::Coefficients => {
                        WorkflowRequest::CollectCoefficients { request, resume }
                    }
                    CollectionKind::Quotes => WorkflowRequest::CollectQuotes { request, resume },
                    CollectionKind::Market => WorkflowRequest::CollectMarket { request, resume },
                    CollectionKind::Actions => WorkflowRequest::CollectActions { request, resume },
                })
            }
            DataCommand::Versions => return client.call(Rpc::Versions),
            DataCommand::Scan { path } => {
                synchronous = true;
                TaskRequest::Scan { root: path }
            }
            DataCommand::CheckUpdates => workflow(WorkflowRequest::CheckUpdates {}),
            DataCommand::Download { tag, resume } => {
                workflow(WorkflowRequest::DownloadMarket { tag, resume })
            }
            DataCommand::Register { path } => workflow(WorkflowRequest::RegisterData { path }),
            DataCommand::Actions {
                market,
                resume,
                pool,
            } => workflow(WorkflowRequest::DownloadActions {
                market,
                resume,
                pool_version: pool,
            }),
        },
        Command::Pools(command) => {
            return match command {
                PoolCommand::List | PoolCommand::Versions => client.call(Rpc::Pools),
                PoolCommand::Create { definition } | PoolCommand::Import { path: definition } => {
                    client.call(Rpc::SavePool {
                        definition: read(&client.root, &definition)?,
                    })
                }
                PoolCommand::Validate { definition } => {
                    let p: analytics_core::pool::PoolDefinition = read(&client.root, &definition)?;
                    p.validate().map_err(invalid)?;
                    Ok(
                        json!({"valid":true,"signature":analytics_workflow::signature(&p).map_err(invalid)?}),
                    )
                }
                PoolCommand::Export { id, output } => {
                    let p = client.call(Rpc::Pool { id })?;
                    fs::write(
                        resolve(&client.root, &output),
                        serde_json::to_vec_pretty(&p["definition"]).map_err(invalid)?,
                    )
                    .map_err(invalid)?;
                    Ok(json!({"output":output}))
                }
                PoolCommand::Preview { id, market, date } => client.call(Rpc::PreviewPool {
                    id,
                    market,
                    day: date.parse().map_err(invalid)?,
                }),
            };
        }
        Command::Query(command) => {
            synchronous = true;
            match command {
                QueryCommand::Describe { source } => TaskRequest::Describe {
                    sql: source.read(&client.root)?,
                },
                QueryCommand::Run {
                    source,
                    page_size,
                    sort,
                    descending,
                } => TaskRequest::Query {
                    request: PageRequest {
                        sql: source.read(&client.root)?,
                        limit: page_size,
                        sort: sort
                            .into_iter()
                            .map(|column| SortKey { column, descending })
                            .collect(),
                        after: None,
                    },
                    query_id: None,
                },
                QueryCommand::Next {
                    query_id,
                    cursor_json,
                } => TaskRequest::QueryNext {
                    query_id,
                    after: serde_json::from_str(&cursor_json).map_err(invalid)?,
                },
                QueryCommand::Export {
                    source,
                    output,
                    file_format,
                } => TaskRequest::Export {
                    request: ExportRequest {
                        sql: source.read(&client.root)?,
                        destination: output,
                        format: match file_format {
                            FileFormat::Csv => ExportFormat::Csv,
                            FileFormat::Parquet => ExportFormat::Parquet,
                        },
                        compression: None,
                    },
                },
            }
        }
        Command::Factors(FactorCommand::Run { request }) => TaskRequest::Pipeline {
            request: Box::new(read(&client.root, &request)?),
        },
        Command::Model(ModelCommand::Train {
            cache,
            universe,
            config,
        }) => TaskRequest::Train {
            cache_path: cache,
            universe_id: universe,
            config: config
                .map(|p| read(&client.root, &p))
                .transpose()?
                .unwrap_or_default(),
        },
        Command::Model(ModelCommand::Artifacts { id }) => return client.call(Rpc::Results { id }),
        Command::Research(command) => match *command {
            ResearchCommand::Verify { task } => {
                return client.call(Rpc::VerifyResearch { id: task });
            }
            ResearchCommand::Preview {
                actions,
                template,
                market,
                schedule,
                dates,
                quality,
            } => workflow(WorkflowRequest::PreviewResearch {
                actions,
                template: Box::new(research_template(
                    &client.root,
                    &template,
                    schedule.as_deref(),
                    &dates,
                    &quality,
                )?),
                market,
            }),
            ResearchCommand::PrepareData {
                actions,
                template,
                schedule,
                dates,
                quality,
                market,
                announcements,
                resume,
            } => workflow(WorkflowRequest::PrepareResearchData {
                actions,
                template: Box::new(research_template(
                    &client.root,
                    &template,
                    schedule.as_deref(),
                    &dates,
                    &quality,
                )?),
                market,
                announcements,
                resume,
            }),
            ResearchCommand::DemoParquet => workflow(WorkflowRequest::CreateCanonicalDemo {}),
            ResearchCommand::Demo => workflow(WorkflowRequest::CreateDemo {}),
            ResearchCommand::Check {
                template,
                schedule,
                dates,
                quality,
                market,
                actions,
            } => workflow(WorkflowRequest::Research {
                template: Box::new(research_template(
                    &client.root,
                    &template,
                    schedule.as_deref(),
                    &dates,
                    &quality,
                )?),
                market,
                actions,
                audit_only: true,
            }),
            ResearchCommand::Train {
                template,
                schedule,
                dates,
                quality,
                market,
                actions,
            } => workflow(WorkflowRequest::TrainResearch {
                template: Box::new(research_template(
                    &client.root,
                    &template,
                    schedule.as_deref(),
                    &dates,
                    &quality,
                )?),
                market,
                actions,
            }),
            ResearchCommand::ReplayPredictions { source, actions, participation, attempt, insufficient_cash } => workflow(WorkflowRequest::BacktestPredictions {
                source, actions, rights_policy: serde_json::from_value(serde_json::json!({"participation":participation,"attempt":attempt,"insufficient_cash":insufficient_cash})).map_err(invalid)?,
            }),
            ResearchCommand::Backtest {
                template,
                training,
                actions,
                event_check,
            } => workflow(WorkflowRequest::BacktestResearch {
                execution: template
                    .as_ref()
                    .map(|path| {
                        ResearchTemplate::read(&resolve(&client.root, path))
                            .map(|t| Box::new(t.execution_settings()))
                            .map_err(invalid)
                    })
                    .transpose()?,
                training,
                actions,
                event_check_policy: match event_check {
                    EventCheckValue::Provider => {
                        analytics_core::execution::EventCheckPolicy::Provider
                    }
                    EventCheckValue::Audited => {
                        analytics_core::execution::EventCheckPolicy::Audited
                    }
                },
            }),
            ResearchCommand::Run {
                template,
                schedule,
                dates,
                quality,
                market,
                actions,
            } => workflow(WorkflowRequest::Research {
                template: Box::new(research_template(
                    &client.root,
                    &template,
                    schedule.as_deref(),
                    &dates,
                    &quality,
                )?),
                market,
                actions: Some(actions),
                audit_only: false,
            }),
        },
        Command::Templates(command) => {
            return match command {
                TemplateCommand::List => client.call(Rpc::Templates),
                TemplateCommand::Factors { template, output } => {
                    let t = ResearchTemplate::read(&resolve(&client.root, &template))
                        .map_err(invalid)?;
                    analytics_workflow::profiles::FactorProfile::from_template(&t)
                        .write(&resolve(&client.root, &output))
                        .map_err(invalid)?;
                    Ok(json!({"output":output,"kind":"factor_profile"}))
                }
                TemplateCommand::Target { template, output } => {
                    let t = ResearchTemplate::read(&resolve(&client.root, &template))
                        .map_err(invalid)?;
                    analytics_workflow::profiles::TargetProfile::from_template(&t)
                        .write(&resolve(&client.root, &output))
                        .map_err(invalid)?;
                    Ok(json!({"output":output,"kind":"target_profile"}))
                }
                TemplateCommand::Compose {
                    workflow,
                    factors,
                    target,
                    output,
                } => {
                    let d: analytics_workflow::profiles::WorkflowDocument =
                        read(&client.root, &workflow)?;
                    let f = analytics_workflow::profiles::FactorProfile::read(&resolve(
                        &client.root,
                        &factors,
                    ))
                    .map_err(invalid)?;
                    let u = analytics_workflow::profiles::TargetProfile::read(&resolve(
                        &client.root,
                        &target,
                    ))
                    .map_err(invalid)?;
                    let t = d.resolve(f, u).map_err(invalid)?;
                    t.write(&resolve(&client.root, &output)).map_err(invalid)?;
                    Ok(
                        json!({"output":output,"signature":analytics_workflow::signature(&t).map_err(invalid)?}),
                    )
                }
                TemplateCommand::Default {
                    output,
                    universe,
                    factor_set,
                } => {
                    let mut t = if universe == "csi300" {
                        ResearchTemplate::builtin_csi300()
                    } else {
                        ResearchTemplate::builtin()
                    }
                    .map_err(invalid)?;
                    t.select_factor_set(match factor_set {
                        FactorSetValue::Gtja191 => {
                            analytics_core::research_quality::FactorSet::Gtja191
                        }
                        FactorSetValue::Alpha158 => {
                            analytics_core::research_quality::FactorSet::Alpha158
                        }
                    });
                    if let Some(p) = output {
                        t.write(&resolve(&client.root, &p)).map_err(invalid)?;
                    }
                    serde_json::to_value(t).map_err(invalid)
                }
                TemplateCommand::Validate { path } => {
                    let t =
                        ResearchTemplate::read(&resolve(&client.root, &path)).map_err(invalid)?;
                    Ok(
                        json!({"valid":true,"signature":analytics_workflow::signature(&t).map_err(invalid)?}),
                    )
                }
                TemplateCommand::Save { path } | TemplateCommand::Import { path } => {
                    client.call(Rpc::SaveTemplate {
                        template: Box::new(
                            ResearchTemplate::read(&resolve(&client.root, &path))
                                .map_err(invalid)?,
                        ),
                    })
                }
                TemplateCommand::Export { path, output } => {
                    let t =
                        ResearchTemplate::read(&resolve(&client.root, &path)).map_err(invalid)?;
                    t.write(&resolve(&client.root, &output)).map_err(invalid)?;
                    Ok(json!({"output":output}))
                }
            };
        }
        Command::Jobs(command) => {
            return match command {
                JobCommand::List => client.call(Rpc::Jobs),
                JobCommand::Status { id } => client.call(Rpc::Job { id }),
                JobCommand::Wait { id, timeout } => wait(&client, id, timeout),
                JobCommand::Logs { id } => client.call(Rpc::Logs { id }),
                JobCommand::Cancel { id } => client.call(Rpc::Cancel { id }),
                JobCommand::Results { id } => client.call(Rpc::Results { id }),
                JobCommand::Import { path } => client.call(Rpc::ImportRun { path }),
            };
        }
        Command::Run { request } => {
            let value: Value = read(&client.root, &request)?;
            serde_json::from_value::<TaskRequest>(value).map_err(invalid)?
        }
        _ => return Err(invalid("invalid command")),
    };
    let id = Uuid::new_v4();
    let submitted = client.call(Rpc::Submit {
        id,
        request: Box::new(task),
    })?;
    if synchronous {
        let result = wait(&client, id, None)?;
        Ok(json!({"submission":submitted,"result":result}))
    } else {
        Ok(submitted)
    }
}

fn research_template(
    root: &Path,
    path: &Path,
    schedule: Option<&Path>,
    dates: &DateOptions,
    quality: &QualityOptions,
) -> std::result::Result<ResearchTemplate, ServiceError> {
    let mut t = ResearchTemplate::read(&resolve(root, path)).map_err(invalid)?;
    if let Some(p) = &quality.factor_profile {
        analytics_workflow::profiles::FactorProfile::read(&resolve(root, p))
            .map_err(invalid)?
            .apply(&mut t)
            .map_err(invalid)?;
    }
    if let Some(p) = &quality.target_profile {
        analytics_workflow::profiles::TargetProfile::read(&resolve(root, p))
            .map_err(invalid)?
            .apply(&mut t)
            .map_err(invalid)?;
    }
    quality.apply(&mut t)?;
    if let Some(path) = schedule {
        t.schedule = read(root, path)?;
    }
    use analytics_core::{
        DateRange,
        schedule::{Fold, Schedule, Step, StepUnit},
    };
    let range = |start: &str, end: &str| -> std::result::Result<DateRange, ServiceError> {
        Ok(DateRange {
            start: start.parse().map_err(invalid)?,
            end: end.parse().map_err(invalid)?,
        })
    };
    if let Some(start) = &dates.train_start {
        t.schedule = Schedule::Single {
            fold: Fold {
                train: range(
                    start,
                    dates
                        .train_end
                        .as_deref()
                        .ok_or_else(|| invalid("missing train end"))?,
                )?,
                validation: dates
                    .validation_start
                    .as_ref()
                    .map(|s| range(s, dates.validation_end.as_deref().unwrap_or("")))
                    .transpose()?,
                test: range(
                    dates.test_start.as_deref().unwrap_or(""),
                    dates.test_end.as_deref().unwrap_or(""),
                )?,
            },
        };
    }
    if dates.no_validation {
        match &mut t.schedule {
            Schedule::Single { fold } | Schedule::Rolling { first: fold, .. } => {
                fold.validation = None
            }
            Schedule::Manual { folds } => {
                for fold in folds {
                    fold.validation = None;
                }
            }
        }
    }
    if let Some(count) = dates.step {
        let first = match t.schedule {
            Schedule::Single { fold } | Schedule::Rolling { first: fold, .. } => fold,
            Schedule::Manual { .. } => {
                return Err(invalid("rolling override conflicts with manual folds"));
            }
        };
        let unit = match dates
            .step_unit
            .ok_or_else(|| invalid("missing step unit"))?
        {
            DateUnit::Days => StepUnit::Days,
            DateUnit::Months => StepUnit::Months,
            DateUnit::Years => StepUnit::Years,
            DateUnit::TradingDays => StepUnit::TradingDays,
        };
        t.schedule = Schedule::Rolling {
            first,
            step: Step { count, unit },
            expanding: dates.expanding,
            until: dates
                .until
                .as_ref()
                .map(|s| s.parse())
                .transpose()
                .map_err(invalid)?,
        };
    }
    t.validate().map_err(invalid)?;
    Ok(t)
}
