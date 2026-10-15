//! Versioned local task service. Native handles stay in backend's worker thread.
#![allow(missing_docs)]
use crate::research::TrainingConfig;
use crate::research::pipeline::PipelineRequest;
use crate::{
    RunRecord, WorkflowRequest, Workspace,
    backend::{self, TaskKind, Wake, WorkerCommand, WorkerEvent},
};
use analytics_core::{ExportRequest, PageRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 18;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskRequest {
    #[cfg(any(test, feature = "test-support"))]
    TestingPause {
        milliseconds: u64,
    },
    Workflow {
        request: Box<WorkflowRequest>,
    },
    Scan {
        root: PathBuf,
    },
    Describe {
        sql: String,
    },
    Query {
        request: PageRequest,
        query_id: Option<Uuid>,
    },
    QueryNext {
        query_id: Uuid,
        after: analytics_core::KeysetCursor,
    },
    Export {
        request: ExportRequest,
    },
    Pipeline {
        request: Box<PipelineRequest>,
    },
    Train {
        cache_path: PathBuf,
        universe_id: Option<String>,
        config: TrainingConfig,
    },
}
impl TaskRequest {
    pub fn kind(&self) -> TaskKind {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::TestingPause { .. } => TaskKind::Workflow,
            Self::Workflow { .. } => TaskKind::Workflow,
            Self::Scan { .. } => TaskKind::Scan,
            Self::Describe { .. } => TaskKind::Describe,
            Self::Query { .. } | Self::QueryNext { .. } => TaskKind::Query,
            Self::Export { .. } => TaskKind::Export,
            Self::Pipeline { .. } => TaskKind::Pipeline,
            Self::Train { .. } => TaskKind::Training,
        }
    }
    pub fn label(&self) -> String {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::TestingPause { .. } => "testing pause".into(),
            Self::Workflow { request } => request.label(),
            Self::Scan { .. } => "扫描数据集".into(),
            Self::Describe { .. } => "描述查询结构".into(),
            Self::Query { .. } | Self::QueryNext { .. } => "执行查询".into(),
            Self::Export { .. } => "导出查询结果".into(),
            Self::Pipeline { .. } => "运行因子流水线".into(),
            Self::Train { .. } => "训练模型".into(),
        }
    }
    fn command(self, task: Uuid, cancel: Arc<AtomicBool>) -> WorkerCommand {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::TestingPause { milliseconds } => {
                WorkerCommand::TestingPause { task, milliseconds }
            }
            Self::Workflow { request } => WorkerCommand::ExecuteWorkflow {
                task,
                request,
                cancel,
            },
            Self::Scan { root } => WorkerCommand::ScanDataset { task, root },
            Self::Describe { sql } => WorkerCommand::DescribeQuery { task, sql },
            Self::Query { request, query_id } => WorkerCommand::SessionQuery {
                task,
                request,
                session: query_id.expect("assigned query session"),
            },
            Self::QueryNext { query_id, after } => WorkerCommand::SessionNext {
                task,
                session: query_id,
                after,
            },
            Self::Export { request } => WorkerCommand::ExportQuery { task, request },
            Self::Pipeline { request } => WorkerCommand::RunPipeline {
                task,
                request: *request,
                cancel,
            },
            Self::Train {
                cache_path,
                universe_id,
                config,
            } => WorkerCommand::TrainFromCache {
                task,
                cache_path,
                universe_id,
                output_dir: PathBuf::new(),
                config,
                cancel,
            },
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceError {
    pub code: String,
    pub message: String,
    pub task_id: Option<Uuid>,
}
impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for ServiceError {}
impl ServiceError {
    pub fn new(code: &str, message: impl ToString) -> Self {
        Self {
            code: code.into(),
            message: message.to_string(),
            task_id: None,
        }
    }
}
pub type Result<T> = std::result::Result<T, ServiceError>;
fn failure(e: impl ToString) -> ServiceError {
    ServiceError::new("service_error", e)
}
fn result_error(e: Box<dyn std::error::Error>, task_id: Option<Uuid>) -> ServiceError {
    let code = if e.is::<crate::research::backtest::UnsupportedResultFormat>() {
        "unsupported_result_format"
    } else {
        "service_error"
    };
    let mut error = ServiceError::new(code, e);
    error.task_id = task_id;
    error
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Rpc {
    Status,
    Submit {
        id: Uuid,
        request: Box<TaskRequest>,
    },
    Events {
        after: u64,
    },
    Jobs,
    Job {
        id: Uuid,
    },
    Results {
        id: Uuid,
    },
    Logs {
        id: Uuid,
    },
    Cancel {
        id: Uuid,
    },
    Versions,
    ImportRun {
        path: PathBuf,
    },
    Pools,
    SavePool {
        definition: analytics_core::pool::PoolDefinition,
    },
    Pool {
        id: String,
    },
    PreviewPool {
        id: String,
        market: String,
        day: analytics_core::Date,
    },
    Templates,
    SaveTemplate {
        template: Box<crate::ResearchTemplate>,
    },
    Stop,
    Restart,
}
fn validate_submission(raw: &Value) -> Result<()> {
    if raw["request"]["method"] != "submit" {
        return Ok(());
    }
    let task = &raw["request"]["params"]["request"];
    if task["operation"] != "workflow" {
        return Ok(());
    }
    analytics_core::source_map::reject_removed_sources(&task["request"])
        .map_err(|e| ServiceError::new("removed_provider", e))?;
    if task["request"]["operation"] == "supplement_actions" {
        return Err(ServiceError::new(
            "removed_operation",
            "旧补采任务不能恢复；请重新采集公司行动或使用巨潮证据修订",
        ));
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
struct Message {
    version: u32,
    request: Rpc,
}
#[derive(Serialize, Deserialize)]
struct Response {
    version: u32,
    data: Option<Value>,
    error: Option<ServiceError>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct ActiveTask {
    pub id: Uuid,
    pub kind: TaskKind,
    pub label: String,
}
struct Active {
    info: ActiveTask,
    cancel: Arc<AtomicBool>,
}
struct State {
    active: Option<Active>,
    events: Vec<(u64, WorkerEvent, usize)>,
    sequence: u64,
    unavailable: Option<String>,
    last_client: Instant,
    stopped: bool,
}
struct Server {
    workspace: Workspace,
    state: Mutex<State>,
    worker: Mutex<backend::WorkerHandle>,
}
impl Server {
    fn record(&self, id: Uuid) -> Result<RunRecord> {
        self.workspace
            .runs()
            .map_err(failure)?
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| ServiceError::new("not_found", "unknown task"))
    }
    fn handle(&self, rpc: Rpc) -> Result<Value> {
        {
            let mut state = self.state.lock().unwrap();
            if state.stopped {
                return Err(ServiceError::new(
                    "service_stopping",
                    "服务正在停止，请稍后重新连接",
                ));
            }
            state.last_client = Instant::now();
        }
        match rpc {
            Rpc::Status => {
                let s = self.state.lock().unwrap();
                Ok(
                    json!({"protocol_version":PROTOCOL_VERSION,"pid":std::process::id(),"workspace":self.workspace.root(),"active":s.active.as_ref().map(|a|&a.info),"unavailable":s.unavailable}),
                )
            }
            Rpc::Submit { id, request } => {
                let mut s = self.state.lock().unwrap();
                // Stop and submit share the lock: an accepted task must never
                // outlive the service's ownership of the workspace.
                if s.stopped {
                    return Err(ServiceError::new(
                        "service_stopping",
                        "服务正在停止，请稍后重新连接",
                    ));
                }
                if let Some(active) = &s.active {
                    return Err(ServiceError {
                        code: "busy".into(),
                        message: "工作区正在执行任务".into(),
                        task_id: Some(active.info.id),
                    });
                }
                if let Some(error) = &s.unavailable {
                    return Err(ServiceError::new("unavailable", error));
                }
                let mut request = *request;
                if let TaskRequest::Query {
                    query_id,
                    request: q,
                } = &mut request
                {
                    if q.after.is_some() && query_id.is_none() {
                        return Err(ServiceError::new("invalid_request", "分页需要query_id"));
                    }
                    if query_id.is_none() {
                        *query_id = Some(Uuid::new_v4())
                    }
                }
                let frozen = match &request {
                    TaskRequest::Workflow { request } => crate::snapshot(&self.workspace, request)
                        .map_err(|e| ServiceError::new("invalid_request", e))?,
                    _ => {
                        json!({"request":request,"app_version":env!("CARGO_PKG_VERSION"),"formula_version":match &request { TaskRequest::Pipeline {request} => analytics_factor::alpha158::version(request.factor_set), _=>analytics_factor::FORMULA_VERSION }})
                    }
                };
                let record = self
                    .workspace
                    .begin(id, &request.label(), frozen)
                    .map_err(failure)?;
                let query_id = match &request {
                    TaskRequest::Query { query_id, .. } => *query_id,
                    TaskRequest::QueryNext { query_id, .. } => Some(*query_id),
                    _ => None,
                };
                let cancel = Arc::new(AtomicBool::new(false));
                s.active = Some(Active {
                    info: ActiveTask {
                        id,
                        kind: request.kind(),
                        label: request.label(),
                    },
                    cancel: cancel.clone(),
                });
                if let Err(error) = self
                    .worker
                    .lock()
                    .unwrap()
                    .send(WorkerCommand::WithContext {
                        task: id,
                        command: Box::new(request.command(id, cancel.clone())),
                        cancel,
                    })
                {
                    let mut record = record;
                    self.workspace
                        .finish(&mut record, crate::RunStatus::Failed, Some(error.clone()))
                        .map_err(failure)?;
                    s.active = None;
                    return Err(ServiceError::new("unavailable", error));
                }
                Ok(
                    json!({"task_id":id,"status":"running","query_id":query_id,"wait":format!("jfquant jobs wait {id}")}),
                )
            }
            Rpc::Events { after } => {
                let s = self.state.lock().unwrap();
                Ok(
                    json!({"sequence":s.sequence,"events":s.events.iter().filter(|(n,_,_)|*n>after).map(|(n,event,_)| (n,event)).collect::<Vec<_>>()}),
                )
            }
            Rpc::Jobs => {
                serde_json::to_value(self.workspace.runs().map_err(failure)?).map_err(failure)
            }
            Rpc::Job { id } => serde_json::to_value(self.record(id)?).map_err(failure),
            Rpc::Results { id } => self
                .workspace
                .results(&self.record(id)?)
                .map_err(|e| result_error(e, Some(id))),
            Rpc::Logs { id } => {
                let r = self.record(id)?;
                let text =
                    fs::read_to_string(self.workspace.resolve(&r.output).join("events.jsonl"))
                        .unwrap_or_default();
                Ok(
                    json!({"task_id":id,"lines":text.lines().filter_map(|s|serde_json::from_str::<Value>(s).ok()).collect::<Vec<_>>()}),
                )
            }
            Rpc::Cancel { id } => {
                let s = self.state.lock().unwrap();
                let active = s
                    .active
                    .as_ref()
                    .filter(|a| a.info.id == id)
                    .ok_or_else(|| ServiceError::new("not_running", "task is not running"))?;
                active.cancel.store(true, Ordering::Relaxed);
                Ok(json!({"task_id":id,"cancel_requested":true}))
            }
            Rpc::Versions => {
                serde_json::to_value(self.workspace.datasets().map_err(failure)?).map_err(failure)
            }
            Rpc::ImportRun { path } => serde_json::to_value(
                self.workspace
                    .import_run(&self.workspace.resolve(path))
                    .map_err(|e| result_error(e, None))?,
            )
            .map_err(failure),
            Rpc::Pools => {
                serde_json::to_value(self.workspace.pools().map_err(failure)?).map_err(failure)
            }
            Rpc::Pool { id } => {
                serde_json::to_value(self.workspace.pool(&id).map_err(failure)?).map_err(failure)
            }
            Rpc::SavePool { definition } => {
                serde_json::to_value(self.workspace.save_pool(definition).map_err(failure)?)
                    .map_err(failure)
            }
            Rpc::PreviewPool { id, market, day } => self
                .workspace
                .preview_pool(&id, &market, day)
                .map_err(failure),
            Rpc::Templates => {
                let mut out = vec![];
                for e in
                    fs::read_dir(self.workspace.resolve(".jfquant/templates")).map_err(failure)?
                {
                    let p = e.map_err(failure)?.path();
                    if p.extension().is_some_and(|s| s == "json")
                        && let Ok(t) = crate::ResearchTemplate::read(&p)
                    {
                        out.push(t);
                    }
                }
                serde_json::to_value(out).map_err(failure)
            }
            Rpc::SaveTemplate { template } => {
                template
                    .validate()
                    .map_err(|e| ServiceError::new("invalid_request", e))?;
                let name = crate::signature(&template).map_err(failure)?;
                let path = PathBuf::from(format!(".jfquant/templates/{}.json", &name[7..23]));
                template
                    .write(&self.workspace.resolve(&path))
                    .map_err(failure)?;
                Ok(json!({"path":path,"template":template}))
            }
            Rpc::Stop => {
                let mut s = self.state.lock().unwrap();
                if let Some(a) = &s.active {
                    return Err(ServiceError {
                        code: "busy".into(),
                        message: "任务执行时不能停止服务".into(),
                        task_id: Some(a.info.id),
                    });
                }
                s.stopped = true;
                Ok(json!({"stopping":true}))
            }
            Rpc::Restart => Ok(json!({"restarted":false})),
        }
    }
}
// Collector threads share the service state, never native handles.
fn attach_events(events: std::sync::mpsc::Receiver<WorkerEvent>, server: Arc<Server>) {
    std::thread::spawn(move || {
        for event in events {
            // Bound retained payload bytes, not just event count: repeated SQL
            // pages otherwise exceed the client frame limit on later attaches.
            let bytes = serde_json::to_vec(&event).map_or(0, |v| v.len() + 32);
            let mut s = server.state.lock().unwrap();
            match &event {
                WorkerEvent::Finished { task } | WorkerEvent::Failed { task, .. } => {
                    if s.active.as_ref().is_some_and(|a| a.info.id == *task) {
                        s.active = None
                    }
                }
                WorkerEvent::Unavailable { message } => {
                    s.unavailable = Some(message.clone());
                    s.active = None
                }
                _ => {}
            }
            s.sequence += 1;
            let n = s.sequence;
            s.events.push((n, event, bytes));
            let mut retained = s.events.iter().map(|(_, _, bytes)| bytes).sum::<usize>();
            while s.events.len() > 1 && (s.events.len() > 2048 || retained > 32 * 1024 * 1024) {
                retained -= s.events.remove(0).2;
            }
            if s.unavailable.is_some() {
                break;
            }
        }
    });
}

pub fn socket_path(workspace: &Path) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let root = fs::canonicalize(workspace).map_err(failure)?;
    let uid = rustix::process::geteuid().as_raw();
    let dir = PathBuf::from(format!("/tmp/jfquant-{uid}"));
    if !dir.exists() {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(e)
                }
            })
            .map_err(failure)?;
    }
    let metadata = fs::symlink_metadata(&dir).map_err(failure)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.permissions().mode() & 0o077 != 0 {
        return Err(ServiceError::new(
            "unsafe_socket_directory",
            "runtime directory must be owned by current user with mode 0700",
        ));
    }
    let hash = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
    Ok(dir.join(format!("{}.sock", &hash[..24])))
}

pub fn serve(root: &Path) -> Result<()> {
    let workspace = Workspace::open(root).map_err(failure)?;
    let _lease = workspace
        .lease()
        .map_err(|e| ServiceError::new("workspace_locked", e))?;
    workspace.recover().map_err(failure)?;
    for directory in [".jfquant/staging", ".jfquant/runs"] {
        analytics_storage::collector::cleanup_private_runtimes(&workspace.resolve(directory))
            .map_err(failure)?;
    }
    let socket = socket_path(workspace.root())?;
    if socket.exists() {
        fs::remove_file(&socket).map_err(failure)?;
    }
    let listener = UnixListener::bind(&socket).map_err(failure)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).map_err(failure)?;
    listener.set_nonblocking(true).map_err(failure)?;
    let (worker, events) =
        backend::spawn_in_workspace(Wake::default(), Some(workspace.root().into()));
    let server = Arc::new(Server {
        workspace,
        state: Mutex::new(State {
            active: None,
            events: vec![],
            sequence: 0,
            unavailable: None,
            last_client: Instant::now(),
            stopped: false,
        }),
        worker: Mutex::new(worker),
    });
    attach_events(events, server.clone());
    loop {
        {
            let mut s = server.state.lock().unwrap();
            if s.stopped
                || (s.active.is_none() && s.last_client.elapsed() > Duration::from_secs(600))
            {
                s.stopped = true;
                break;
            }
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let server = server.clone();
                std::thread::spawn(move || {
                    // macOS accept() can inherit O_NONBLOCK from the listener.
                    // Full JSON frames must use blocking I/O with deadlines.
                    if stream.set_nonblocking(false).is_err() {
                        return;
                    }
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
                    let result = (|| -> Result<Value> {
                        let mut line = String::new();
                        BufReader::new(&stream)
                            .take(16 * 1024 * 1024)
                            .read_line(&mut line)
                            .map_err(failure)?;
                        let raw: Value = serde_json::from_str(&line)
                            .map_err(|e| ServiceError::new("invalid_request", e))?;
                        validate_submission(&raw)?;
                        let message: Message = serde_json::from_str(&line)
                            .map_err(|e| ServiceError::new("invalid_request", e))?;
                        if message.version != PROTOCOL_VERSION {
                            return Err(ServiceError::new(
                                "protocol_mismatch",
                                "客户端和服务协议版本不一致",
                            ));
                        }
                        if matches!(message.request, Rpc::Restart) {
                            let mut s = server.state.lock().unwrap();
                            s.last_client = Instant::now();
                            if s.stopped {
                                return Err(ServiceError::new("service_stopping", "服务正在停止"));
                            }
                            if let Some(active) = &s.active {
                                return Err(ServiceError {
                                    code: "busy".into(),
                                    message: "任务执行时不能重启".into(),
                                    task_id: Some(active.info.id),
                                });
                            }
                            if s.unavailable.is_some() {
                                let (worker, events) = backend::spawn_in_workspace(
                                    Wake::default(),
                                    Some(server.workspace.root().into()),
                                );
                                *server.worker.lock().unwrap() = worker;
                                s.unavailable = None;
                                s.events.clear();
                                drop(s);
                                attach_events(events, server.clone());
                                return Ok(json!({"restarted":true}));
                            }
                        }
                        server.handle(message.request)
                    })();
                    let response = match result {
                        Ok(data) => Response {
                            version: PROTOCOL_VERSION,
                            data: Some(data),
                            error: None,
                        },
                        Err(error) => Response {
                            version: PROTOCOL_VERSION,
                            data: None,
                            error: Some(error),
                        },
                    };
                    if let Ok(bytes) = serde_json::to_vec(&response) {
                        let _ = stream.write_all(&bytes);
                        let _ = stream.write_all(b"\n");
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => return Err(failure(e)),
        }
    }
    let _ = server.worker.lock().unwrap().send(WorkerCommand::Shutdown);
    drop(listener);
    let _ = fs::remove_file(socket);
    Ok(())
}
#[derive(Clone)]
pub struct Client {
    pub root: PathBuf,
}
impl Client {
    pub fn connect(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root).map_err(|e| ServiceError::new("workspace_missing", e))?;
        if !root.join(".jfquant/workspace.json").is_file() {
            return Err(ServiceError::new(
                "workspace_missing",
                "请先使用workspace create创建工作区",
            ));
        }
        Ok(Self { root })
    }
    pub fn call(&self, request: Rpc) -> Result<Value> {
        let mut stream = UnixStream::connect(socket_path(&self.root)?)
            .map_err(|e| ServiceError::new("service_unavailable", e))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .map_err(failure)?;
        stream
            .set_write_timeout(Some(Duration::from_secs(20)))
            .map_err(failure)?;
        serde_json::to_writer(
            &mut stream,
            &Message {
                version: PROTOCOL_VERSION,
                request,
            },
        )
        .map_err(failure)?;
        stream.write_all(b"\n").map_err(failure)?;
        let mut line = String::new();
        let bytes_read = BufReader::new(stream)
            .take(64 * 1024 * 1024)
            .read_line(&mut line)
            .map_err(failure)?;
        if bytes_read == 0 {
            return Err(ServiceError::new(
                "service_unavailable",
                "服务socket已关闭，可重新连接",
            ));
        }
        let response: Response = serde_json::from_str(&line).map_err(failure)?;
        if response.version != PROTOCOL_VERSION {
            return Err(ServiceError::new("protocol_mismatch", "服务协议不匹配"));
        }
        if let Some(error) = response.error {
            return Err(error);
        }
        // A successful JSON null is valid (e.g. a job with no result payload).
        Ok(response.data.unwrap_or(Value::Null))
    }
    pub fn ensure(&self, binary: Option<&Path>) -> Result<()> {
        let stopping_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.call(Rpc::Status) {
                Ok(_) => return Ok(()),
                Err(e) if e.code == "service_stopping" && Instant::now() < stopping_deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(e) if e.code == "service_unavailable" => break,
                Err(e) => return Err(e),
            }
        }
        let binary = match binary {
            Some(p) => p.to_owned(),
            None => find_binary()?,
        };
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join(".jfquant/service.log"))
            .map_err(failure)?;
        let stdout = log.try_clone().map_err(failure)?;
        let mut child = std::process::Command::new(binary)
            // A foreground client's Ctrl-C must not reach accepted background work.
            .process_group(0)
            .arg("--workspace")
            .arg(&self.root)
            .args(["service", "serve"])
            .stdin(std::process::Stdio::null())
            .stdout(stdout)
            .stderr(log)
            .spawn()
            .map_err(failure)?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.call(Rpc::Status).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(ServiceError::new(
            "service_unavailable",
            "服务未启动，请检查.jfquant/service.log和工作区锁",
        ))
    }
}
pub fn find_binary() -> Result<PathBuf> {
    locate_binary().map_err(|e| ServiceError::new("cli_missing", e))
}
/// Configure through the same executable so App and CLI share private local credential files.
/// The credential travels over a private stdin pipe; no task, argv or log contains it.
/// Save a known provider key through the same private file store as the service.
pub fn configure_source(provider: &str, token: &str) -> Result<()> {
    if !matches!(provider, "tdxaidata" | "hithink") {
        return Err(failure("credential_provider_unsupported"));
    }
    use std::process::Stdio;
    let mut child = std::process::Command::new(find_binary()?)
        .args([
            "data",
            "sources",
            "configure",
            "--provider",
            provider,
            "--key-stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(failure)?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| failure("credential_input_unavailable"))?;
    input.write_all(token.as_bytes()).map_err(failure)?;
    drop(input);
    if !child.wait().map_err(failure)?.success() {
        return Err(failure(
            "credential_store_failed: check private credential-directory permissions",
        ));
    }
    Ok(())
}
pub fn resolve_workspace(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit.or_else(|| std::env::var_os("JFQUANT_WORKSPACE").map(PathBuf::from)) {
        return Ok(p);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join("Library/Application Support/JFQuant/preferences.json");
        if let Ok(bytes) = fs::read(p)
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(s) = value["workspace"].as_str()
        {
            return Ok(PathBuf::from(s));
        }
    }
    Err(ServiceError::new(
        "workspace_missing",
        "请指定--workspace、JFQUANT_WORKSPACE或在App中选择工作区",
    ))
}

/// Egui-independent adapter; dropping the client does not cancel service jobs.
pub fn bridge(
    wake: Wake,
    root: PathBuf,
) -> (
    backend::WorkerHandle,
    std::sync::mpsc::Receiver<WorkerEvent>,
) {
    let (tx, commands) = std::sync::mpsc::channel();
    let (events, event_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let notify = |event| {
            let _ = events.send(event);
            wake.request_repaint();
        };
        let client = match Client::connect(&root).and_then(|c| {
            c.ensure(None)?;
            if c.call(Rpc::Status)?["unavailable"].is_string() {
                c.call(Rpc::Restart)?;
            }
            Ok(c)
        }) {
            Ok(c) => c,
            Err(e) => {
                notify(WorkerEvent::Unavailable {
                    message: e.to_string(),
                });
                return;
            }
        };
        let mut sequence = 0u64;
        let mut observed_active = None;
        let mut query_id = None;
        let mut local_queries = std::collections::HashSet::new();
        let mut cancellations = std::collections::HashMap::<Uuid, Arc<AtomicBool>>::new();
        loop {
            match commands.recv_timeout(Duration::from_millis(150)) {
                Ok(WorkerCommand::CancelTask { task }) => {
                    if let Err(e) = client.call(Rpc::Cancel { id: task }) {
                        notify(WorkerEvent::Progress {
                            task,
                            message: e.to_string(),
                        });
                    }
                }
                Ok(command) => {
                    let (task, kind) = command.task_info();
                    let (request, cancel) = match command {
                        WorkerCommand::ExecuteWorkflow {
                            request, cancel, ..
                        } => (TaskRequest::Workflow { request }, Some(cancel)),
                        WorkerCommand::ScanDataset { root, .. } => {
                            (TaskRequest::Scan { root }, None)
                        }
                        WorkerCommand::DescribeQuery { sql, .. } => {
                            (TaskRequest::Describe { sql }, None)
                        }
                        WorkerCommand::RunQuery { request, .. } => {
                            if request.after.is_none() {
                                query_id = Some(Uuid::new_v4());
                            }
                            (TaskRequest::Query { request, query_id }, None)
                        }
                        WorkerCommand::ExportQuery { request, .. } => {
                            (TaskRequest::Export { request }, None)
                        }
                        WorkerCommand::RunPipeline {
                            request, cancel, ..
                        } => (
                            TaskRequest::Pipeline {
                                request: Box::new(request),
                            },
                            Some(cancel),
                        ),
                        WorkerCommand::TrainFromCache {
                            cache_path,
                            universe_id,
                            config,
                            cancel,
                            ..
                        } => (
                            TaskRequest::Train {
                                cache_path,
                                universe_id,
                                config,
                            },
                            Some(cancel),
                        ),
                        WorkerCommand::SessionQuery {
                            session, request, ..
                        } => (
                            TaskRequest::Query {
                                request,
                                query_id: Some(session),
                            },
                            None,
                        ),
                        WorkerCommand::SessionNext { session, after, .. } => (
                            TaskRequest::QueryNext {
                                query_id: session,
                                after,
                            },
                            None,
                        ),
                        _ => {
                            notify(WorkerEvent::Failed {
                                task,
                                kind,
                                message: "unsupported client command".into(),
                            });
                            continue;
                        }
                    };
                    match client.call(Rpc::Submit {
                        id: task,
                        request: Box::new(request),
                    }) {
                        Ok(_) => {
                            if matches!(
                                kind,
                                TaskKind::Query | TaskKind::Describe | TaskKind::Export
                            ) {
                                local_queries.insert(task);
                            }
                            if let Some(cancel) = cancel {
                                cancellations.insert(task, cancel);
                            }
                        }
                        Err(e) => notify(WorkerEvent::Failed {
                            task,
                            kind,
                            message: e.to_string(),
                        }),
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            for (id, flag) in &cancellations {
                if flag.swap(false, Ordering::Relaxed) {
                    let _ = client.call(Rpc::Cancel { id: *id });
                }
            }
            match client.call(Rpc::Status) {
                Ok(status) => {
                    if let Ok(active) =
                        serde_json::from_value::<ActiveTask>(status["active"].clone())
                        && observed_active != Some(active.id)
                    {
                        notify(WorkerEvent::Started {
                            task: active.id,
                            kind: active.kind,
                            label: active.label,
                        });
                        observed_active = Some(active.id);
                    }
                }
                Err(e) => {
                    notify(WorkerEvent::Unavailable {
                        message: e.to_string(),
                    });
                    break;
                }
            }
            match client.call(Rpc::Events { after: sequence }) {
                Ok(value) => {
                    sequence = value["sequence"].as_u64().unwrap_or(sequence);
                    if let Some(items) = value["events"].as_array() {
                        for item in items {
                            let event = match serde_json::from_value::<WorkerEvent>(item[1].clone())
                            {
                                Ok(event) => event,
                                Err(e) => {
                                    notify(WorkerEvent::Unavailable {
                                        message: format!("服务事件解析失败：{e}"),
                                    });
                                    return;
                                }
                            };
                            {
                                // Task lifecycle remains workspace-wide, but SQL
                                // payloads must not replace another client's pager.
                                if let WorkerEvent::QueryPaged { task, .. }
                                | WorkerEvent::QueryDescribed { task, .. }
                                | WorkerEvent::Exported { task, .. } = &event
                                    && !local_queries.contains(task)
                                {
                                    continue;
                                }
                                match &event {
                                    WorkerEvent::Started { task, .. } => {
                                        observed_active = Some(*task)
                                    }
                                    WorkerEvent::Finished { task }
                                    | WorkerEvent::Failed { task, .. } => {
                                        cancellations.remove(task);
                                        local_queries.remove(task);
                                        if observed_active == Some(*task) {
                                            observed_active = None;
                                        }
                                    }
                                    _ => {}
                                }
                                notify(event);
                            }
                        }
                    }
                }
                Err(e) => {
                    notify(WorkerEvent::Unavailable {
                        message: e.to_string(),
                    });
                    break;
                }
            }
        }
    });
    (backend::WorkerHandle { tx }, event_rx)
}

impl Client {
    /// Wait for terminal status; a timeout never cancels server-side work.
    pub fn wait(&self, id: Uuid, timeout: Option<u64>) -> Result<Value> {
        let started = Instant::now();
        loop {
            let value = self.call(Rpc::Job { id })?;
            let record: RunRecord = serde_json::from_value(value.clone()).map_err(failure)?;
            if record.status != crate::RunStatus::Running {
                if self.call(Rpc::Status)?["active"]["id"] == id.to_string() {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                let code = match record.status {
                    crate::RunStatus::Blocked => Some("blocked"),
                    crate::RunStatus::Cancelled => Some("cancelled"),
                    crate::RunStatus::Failed | crate::RunStatus::Interrupted => Some(
                        if record
                            .error
                            .as_ref()
                            .is_some_and(|s| s.starts_with("query_expired"))
                        {
                            "query_expired"
                        } else {
                            "task_failed"
                        },
                    ),
                    _ => None,
                };
                if let Some(code) = code {
                    return Err(ServiceError {
                        code: code.into(),
                        message: record.error.unwrap_or(record.stage),
                        task_id: Some(id),
                    });
                }
                let details = self.call(Rpc::Results { id })?;
                return Ok(json!({"record":value,"results":details}));
            }
            if timeout.is_some_and(|s| started.elapsed() >= Duration::from_secs(s)) {
                return Err(ServiceError {
                    code: "timeout".into(),
                    message: "等待超时；后台任务继续执行".into(),
                    task_id: Some(id),
                });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn stopped_service_rejects_accepted_connections_without_creating_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(tmp.path()).unwrap();
        let (worker, _events) =
            backend::spawn_in_workspace(Wake::default(), Some(tmp.path().into()));
        let server = Server {
            workspace,
            worker: Mutex::new(worker),
            state: Mutex::new(State {
                active: None,
                events: vec![],
                sequence: 0,
                unavailable: None,
                last_client: Instant::now(),
                stopped: false,
            }),
        };
        server.handle(Rpc::Stop).unwrap();
        let error = server
            .handle(Rpc::Submit {
                id: Uuid::new_v4(),
                request: Box::new(TaskRequest::Workflow {
                    request: Box::new(WorkflowRequest::CreateDemo {}),
                }),
            })
            .unwrap_err();
        assert_eq!(error.code, "service_stopping");
        assert!(server.workspace.runs().unwrap().is_empty());
        server
            .worker
            .lock()
            .unwrap()
            .send(WorkerCommand::Shutdown)
            .unwrap();
    }
}

fn locate_binary() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("JFQUANT_CLI_BIN") {
        let path = PathBuf::from(path);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "JFQUANT_CLI_BIN没有指向jfquant文件",
            ))
        };
    }
    let exe = std::env::current_exe()?;
    if let Some(parent) = exe.parent() {
        for path in [parent.join("jfquant"), parent.join("../jfquant")] {
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&paths).map(|p| p.join("jfquant")) {
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "找不到jfquant，请使用完整安装包或配置JFQUANT_CLI_BIN；开发时先构建analytics-cli",
    ))
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    #[test]
    fn old_protocol_is_rejected_without_starting_a_replacement_service() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(tmp.path()).unwrap();
        let client = Client::connect(workspace.root()).unwrap();
        let socket = socket_path(&client.root).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["version"], PROTOCOL_VERSION);
            stream
                .write_all(b"{\"version\":1,\"data\":{\"active\":null},\"error\":null}\n")
                .unwrap();
        });
        assert_eq!(
            client
                .ensure(Some(Path::new("/missing/replacement/jfquant")))
                .unwrap_err()
                .code,
            "protocol_mismatch"
        );
        server.join().unwrap();
        fs::remove_file(socket).unwrap();
    }
}

#[cfg(test)]
mod retired_submission_tests {
    use super::*;
    #[test]
    fn old_workflow_is_rejected_before_deserialization_but_history_is_readable() {
        let mut raw = json!({"version":18,"request":{"method":"submit","params":{"id":Uuid::new_v4(),"request":{"operation":"workflow","request":{"operation":"supplement_actions","actions":"old-version"}}}}});
        assert_eq!(
            validate_submission(&raw).unwrap_err().code,
            "removed_operation"
        );
        raw["request"]["params"]["request"]["request"] =
            json!({"operation":"collect_quotes","request":{"provider":"akshare"}});
        assert_eq!(
            validate_submission(&raw).unwrap_err().code,
            "removed_provider"
        );
        raw["request"]["method"] = json!("jobs");
        assert!(validate_submission(&raw).is_ok());
    }
}
