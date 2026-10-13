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
    Query(QueryCommand),
    #[command(subcommand)]
    Factors(FactorCommand),
    #[command(subcommand)]
    Model(ModelCommand),
    #[command(subcommand)]
    Research(ResearchCommand),
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
    Scan {
        path: PathBuf,
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
        #[arg(long)]
        market: String,
        #[arg(long)]
        resume: Option<Uuid>,
    },
    Supplement {
        #[arg(long)]
        actions: String,
        #[arg(long, value_enum)]
        method: ActionMethod,
        #[arg(long)]
        start: String,
        #[arg(long)]
        end: String,
        #[arg(long)]
        resume: Option<Uuid>,
    },
    Retry {
        #[arg(long)]
        actions: String,
        #[arg(long)]
        start: String,
        #[arg(long)]
        end: String,
    },
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ActionMethod {
    Enrich,
    Details,
    Payments,
    Repair,
    RepairPayments,
    Retry,
}
impl ActionMethod {
    fn name(self) -> &'static str {
        match self {
            Self::Enrich => "enrich",
            Self::Details => "details",
            Self::Payments => "payments",
            Self::Repair => "repair",
            Self::RepairPayments => "repair-payments",
            Self::Retry => "retry",
        }
    }
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
    List,
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
#[derive(Debug, Subcommand)]
pub enum ResearchCommand {
    Demo,
    Check {
        #[arg(long)]
        template: PathBuf,
        #[arg(long)]
        market: String,
        #[arg(long)]
        actions: String,
    },
    Run {
        #[arg(long)]
        template: PathBuf,
        #[arg(long)]
        market: String,
        #[arg(long)]
        actions: String,
    },
}
#[derive(Debug, Subcommand)]
pub enum TemplateCommand {
    List,
    Default {
        #[arg(long)]
        output: Option<PathBuf>,
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
    if let Command::Factors(FactorCommand::List) = &cli.command {
        return Ok(
            json!({"formula_version":analytics_factor::FORMULA_VERSION,"factors":analytics_factor::audit::entries()}),
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
            DataCommand::Actions { market, resume } => {
                workflow(WorkflowRequest::DownloadActions { market, resume })
            }
            DataCommand::Supplement {
                actions,
                method,
                start,
                end,
                resume,
            } => workflow(WorkflowRequest::SupplementActions {
                actions,
                method: method.name().into(),
                start: start.parse().map_err(invalid)?,
                end: end.parse().map_err(invalid)?,
                resume,
            }),
            DataCommand::Retry {
                actions,
                start,
                end,
            } => workflow(WorkflowRequest::SupplementActions {
                actions,
                method: "retry".into(),
                start: start.parse().map_err(invalid)?,
                end: end.parse().map_err(invalid)?,
                resume: None,
            }),
        },
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
        Command::Research(command) => match command {
            ResearchCommand::Demo => workflow(WorkflowRequest::CreateDemo {}),
            ResearchCommand::Check {
                template,
                market,
                actions,
            } => workflow(WorkflowRequest::Research {
                template: Box::new(
                    ResearchTemplate::read(&resolve(&client.root, &template)).map_err(invalid)?,
                ),
                market,
                actions,
                audit_only: true,
            }),
            ResearchCommand::Run {
                template,
                market,
                actions,
            } => workflow(WorkflowRequest::Research {
                template: Box::new(
                    ResearchTemplate::read(&resolve(&client.root, &template)).map_err(invalid)?,
                ),
                market,
                actions,
                audit_only: false,
            }),
        },
        Command::Templates(command) => {
            return match command {
                TemplateCommand::List => client.call(Rpc::Templates),
                TemplateCommand::Default { output } => {
                    let t = ResearchTemplate::builtin().map_err(invalid)?;
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
