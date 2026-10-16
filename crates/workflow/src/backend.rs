#![allow(missing_docs)]
// Background worker thread that owns every native handle.
//
// Per the repository engineering rules, the DuckDB connection and all
// LightGBM handles live exclusively on this single worker thread; the egui
// thread only exchanges plain data through [`WorkerCommand`] /
// [`WorkerEvent`] channels. Cancellation is cooperative: the UI keeps a
// shared `Arc<AtomicBool>` flag for the active task and the pipeline checks
// it between stages.

use crate::research::TrainEvent;
use crate::research::TrainingConfig;
use crate::research::pipeline::{
    Cancellation, PipelineArtifact, PipelineError, PipelineEvent, PipelineRequest,
    run_factor_pipeline, train_from_factor_cache,
};
use analytics_core::{
    ColumnSchema, DatasetFile, ExportRequest, PageRequest, QueryPage, ScalarValue, SortKey,
};
use analytics_engine::{DuckDbEngine, QueryEngine};
use analytics_factor::FactorEvent;
use analytics_storage::ParquetCatalog;
use serde::{Deserialize, Serialize};
/// UI-independent event notification.
#[derive(Clone, Default)]
pub struct Wake(Option<std::sync::Arc<dyn Fn() + Send + Sync>>);
impl Wake {
    /// Construct an event notification callback.
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Some(std::sync::Arc::new(f)))
    }
    /// Notify a client of a new event.
    pub fn request_repaint(&self) {
        if let Some(f) = &self.0 {
            f();
        }
    }
}
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use uuid::Uuid;

/// Subsystem a background task belongs to; used to route errors back to the
/// panel that issued the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskKind {
    /// Parquet catalog scan.
    Scan,
    /// SQL `DESCRIBE`.
    Describe,
    /// SQL page query.
    Query,
    /// SQL export.
    Export,
    /// Full factor pipeline run.
    Pipeline,
    /// Cache-only retraining run.
    Training,
    /// Data acquisition or saved annual research.
    Workflow,
}

/// Commands sent from the egui thread to the worker.
#[derive(Debug)]
pub enum WorkerCommand {
    PreviewFactor {
        task: Uuid,
        draft: crate::custom_factors::FactorDraft,
        cancel: Cancellation,
    },
    CheckFactors {
        task: Uuid,
        request: crate::input_check::CheckRequest,
        cancel: Cancellation,
    },
    EvaluateFactors {
        task: Uuid,
        request: crate::factor_evaluation::EvaluationRequest,
        cancel: Cancellation,
    },
    /// Execute a typed platform workflow.
    ExecuteWorkflow {
        task: Uuid,
        request: Box<crate::WorkflowRequest>,
        cancel: Cancellation,
    },
    /// Scan a Parquet root into file metadata.
    ScanDataset {
        /// Task identifier.
        task: Uuid,
        /// Directory or single file.
        root: PathBuf,
    },
    /// Describe the result columns of a SQL statement.
    DescribeQuery {
        /// Task identifier.
        task: Uuid,
        /// SQL without a trailing semicolon.
        sql: String,
    },
    /// Fetch one keyset page of a SQL statement.
    RunQuery {
        /// Task identifier.
        task: Uuid,
        /// Validated page request.
        request: PageRequest,
    },
    #[cfg(any(test, feature = "test-support"))]
    TestingPause {
        task: Uuid,
        milliseconds: u64,
    },
    Shutdown,
    WithContext {
        task: Uuid,
        command: Box<WorkerCommand>,
        cancel: Cancellation,
    },
    /// Request service cancellation without sharing process memory.
    CancelTask {
        task: Uuid,
    },
    /// Independent client query session, owned by the same native worker thread.
    SessionQuery {
        task: Uuid,
        session: Uuid,
        request: PageRequest,
    },
    SessionNext {
        task: Uuid,
        session: Uuid,
        after: analytics_core::KeysetCursor,
    },
    /// Export a SQL result to Parquet or CSV.
    ExportQuery {
        /// Task identifier.
        task: Uuid,
        /// Validated export request.
        request: ExportRequest,
    },
    /// Run the full market-to-model pipeline.
    RunPipeline {
        /// Task identifier.
        task: Uuid,
        /// Pipeline configuration.
        request: PipelineRequest,
        /// Cooperative cancellation flag owned by the UI.
        cancel: Cancellation,
    },
    /// Retrain from an existing factor cache with fresh hyperparameters.
    TrainFromCache {
        /// Task identifier.
        task: Uuid,
        /// Factor cache Parquet path.
        cache_path: PathBuf,
        /// Universe override; `None` keeps the cache value.
        universe_id: Option<String>,
        /// Output directory for retrained artifacts.
        output_dir: PathBuf,
        /// LightGBM hyperparameters.
        config: TrainingConfig,
        /// Cooperative cancellation flag owned by the UI.
        cancel: Cancellation,
    },
    #[cfg(any(test, feature = "test-support"))]
    /// Injects a task panic to exercise failure routing and worker retirement.
    PanicTask {
        task: Uuid,
        kind: TaskKind,
    },
    #[cfg(any(test, feature = "test-support"))]
    /// Injects a panic outside the task boundary to exercise the thread guard.
    PanicOutsideTask,
}

/// Display-ready summary of a finished pipeline or retraining run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    /// Artifact paths and split row counts.
    pub artifact: PipelineArtifact,
    /// Raw `metrics.json` content for the metrics cards.
    pub metrics_json: Option<String>,
    /// Feature importance pairs `(factor, gain)` sorted descending.
    pub importance: Vec<(String, f64)>,
    /// First 100 rows of `predictions.parquet` for preview.
    pub preview: Option<QueryPage>,
}

/// Events streamed from the worker back to the egui thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerEvent {
    FactorPreviewed {
        task: Uuid,
        preview: crate::custom_factors::FormulaPreview,
    },
    FactorsChecked {
        task: Uuid,
        report: crate::input_check::InputCheckReport,
    },
    FactorsEvaluated {
        task: Uuid,
        report: crate::factor_evaluation::EvaluationReport,
    },
    /// Durable workflow result and refreshed local catalogs.
    WorkflowFinished {
        task: Uuid,
        outcome: crate::WorkflowOutcome,
        record: crate::RunRecord,
        datasets: Vec<crate::DataVersion>,
        runs: Vec<crate::RunRecord>,
    },
    /// A command started executing.
    Started {
        /// Task identifier.
        task: Uuid,
        /// Owning subsystem.
        kind: TaskKind,
        /// Human-readable label.
        label: String,
    },
    /// Intermediate progress line.
    Progress {
        /// Task identifier.
        task: Uuid,
        /// Display-ready progress text.
        message: String,
    },
    /// Scan finished with discovered files.
    DatasetScanned {
        /// Task identifier.
        task: Uuid,
        /// Actual scanned path, independent of later form edits.
        root: PathBuf,
        /// Discovered Parquet files with metadata.
        files: Vec<DatasetFile>,
    },
    /// `DESCRIBE` finished.
    QueryDescribed {
        /// Task identifier.
        task: Uuid,
        /// Result column schemas.
        columns: Vec<ColumnSchema>,
    },
    /// A result page arrived.
    QueryPaged {
        /// Task identifier.
        task: Uuid,
        /// Fetched page.
        page: QueryPage,
    },
    /// Export finished.
    Exported {
        /// Task identifier.
        task: Uuid,
        /// Written file.
        destination: PathBuf,
    },
    /// Factor pipeline finished successfully.
    PipelineFinished {
        /// Task identifier.
        task: Uuid,
        /// Display summary.
        summary: RunSummary,
    },
    /// Cache retraining finished successfully.
    TrainingFinished {
        /// Task identifier.
        task: Uuid,
        /// Display summary.
        summary: RunSummary,
    },
    /// The command failed; `kind` routes the error to its panel.
    Failed {
        /// Task identifier.
        task: Uuid,
        /// Owning subsystem.
        kind: TaskKind,
        /// Error text.
        message: String,
    },
    /// The command finished (after any payload event).
    Finished {
        /// Task identifier.
        task: Uuid,
    },
    /// The worker must be replaced before accepting more commands.
    Unavailable {
        /// Display-ready reason for stopping the worker.
        message: String,
    },
}

/// Sending half of the worker protocol, held by the egui app.
#[derive(Clone)]
pub struct WorkerHandle {
    pub(crate) tx: Sender<WorkerCommand>,
}

impl WorkerHandle {
    #[cfg(any(test, feature = "test-support"))]
    pub fn recording() -> (Self, Receiver<WorkerCommand>) {
        let (tx, rx) = channel();
        (Self { tx }, rx)
    }
    /// Request cancellation via the client adapter.
    pub fn cancel(&self, task: Uuid) -> Result<(), String> {
        self.send(WorkerCommand::CancelTask { task })
    }
    /// Queues a command, or reports that the receiving worker has exited.
    pub fn send(&self, command: WorkerCommand) -> Result<(), String> {
        self.tx
            .send(command)
            .map_err(|_| "后台线程已退出，请重启后台后重试。".into())
    }
}

impl WorkerCommand {
    /// Identifies a submitted task before the background thread starts it.
    pub fn task_info(&self) -> (Uuid, TaskKind) {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::TestingPause { task, .. } => (*task, TaskKind::Workflow),
            Self::Shutdown => (Uuid::nil(), TaskKind::Workflow),
            Self::WithContext { command, .. } => command.task_info(),
            Self::ExecuteWorkflow { task, .. } | Self::CancelTask { task } => {
                (*task, TaskKind::Workflow)
            }
            Self::ScanDataset { task, .. } => (*task, TaskKind::Scan),
            Self::DescribeQuery { task, .. } => (*task, TaskKind::Describe),
            Self::RunQuery { task, .. }
            | Self::SessionQuery { task, .. }
            | Self::SessionNext { task, .. } => (*task, TaskKind::Query),
            Self::ExportQuery { task, .. } => (*task, TaskKind::Export),
            Self::PreviewFactor { task, .. }
            | Self::EvaluateFactors { task, .. }
            | Self::CheckFactors { task, .. }
            | Self::RunPipeline { task, .. } => (*task, TaskKind::Pipeline),
            Self::TrainFromCache { task, .. } => (*task, TaskKind::Training),
            #[cfg(any(test, feature = "test-support"))]
            Self::PanicTask { task, kind } => (*task, *kind),
            #[cfg(any(test, feature = "test-support"))]
            Self::PanicOutsideTask => unreachable!("fault injection has no task"),
        }
    }
}

/// Spawns the worker thread and returns both protocol halves.
#[cfg(any(test, feature = "test-support"))]
pub fn spawn(ctx: Wake) -> (WorkerHandle, Receiver<WorkerEvent>) {
    spawn_in_workspace(ctx, None)
}
/// Spawn a worker whose relative paths refer to an explicit workspace.
pub fn spawn_in_workspace(
    ctx: Wake,
    root: Option<PathBuf>,
) -> (WorkerHandle, Receiver<WorkerEvent>) {
    let (command_tx, command_rx) = channel();
    let (event_tx, event_rx) = channel();
    let worker_events = event_tx.clone();
    let worker_ctx = ctx.clone();
    let started = std::thread::Builder::new()
        .name("analytics-worker".into())
        .spawn(move || {
            let event_tx = worker_events;
            let ctx = worker_ctx;
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                Worker {
                    root: root.clone(),
                    record_request: None,
                    active_record: None,
                    active_cancel: None,
                    outcome: None,
                    query: QuerySession::new(root.clone()),
                    events: event_tx.clone(),
                    ctx: ctx.clone(),
                    stop_reason: None,
                    query_sessions: std::collections::HashMap::new(),
                }
                .run(command_rx)
            }));
            let message = match outcome {
                Ok(Some(message)) => message,
                Ok(None) => "后台线程已停止，请重启后台后重试。".into(),
                Err(payload) => format!(
                    "后台线程发生内部错误：{}。请重启后台后重试。",
                    panic_message(payload.as_ref())
                ),
            };
            let _ = event_tx.send(WorkerEvent::Unavailable { message });
            ctx.request_repaint();
        });
    if let Err(error) = started {
        let _ = event_tx.send(WorkerEvent::Unavailable {
            message: format!("后台线程启动失败：{error}。请重启后台后重试。"),
        });
        ctx.request_repaint();
    }
    (WorkerHandle { tx: command_tx }, event_rx)
}

#[doc(hidden)]
pub(crate) struct Worker {
    root: Option<PathBuf>,
    record_request: Option<serde_json::Value>,
    active_record: Option<crate::RunRecord>,
    active_cancel: Option<Cancellation>,
    outcome: Option<crate::WorkflowOutcome>,
    query: QuerySession,
    events: Sender<WorkerEvent>,
    ctx: Wake,
    stop_reason: Option<String>,
    query_sessions: std::collections::HashMap<Uuid, (QuerySession, std::time::Instant)>,
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("未知 panic")
}

impl Worker {
    fn expire_queries(&mut self) {
        self.query_sessions
            .retain(|_, (_, used)| used.elapsed() < std::time::Duration::from_secs(600));
    }

    fn run(&mut self, commands: Receiver<WorkerCommand>) -> Option<String> {
        loop {
            self.expire_queries();
            match commands.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(command) => self.dispatch(command),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
            if self.stop_reason.is_some() {
                break;
            }
        }
        self.stop_reason.take()
    }

    fn engine(&mut self) -> Result<&mut DuckDbEngine, String> {
        self.query.engine()
    }
    fn query_page(&mut self, request: PageRequest) -> Result<QueryPage, String> {
        self.query.query_page(request)
    }
    fn emit_result(&self, event: WorkerEvent) -> Result<(), String> {
        if let Some(root) = &self.root {
            let task = match &event {
                WorkerEvent::DatasetScanned { task, .. }
                | WorkerEvent::QueryDescribed { task, .. }
                | WorkerEvent::QueryPaged { task, .. }
                | WorkerEvent::Exported { task, .. }
                | WorkerEvent::FactorPreviewed { task, .. }
                | WorkerEvent::FactorsEvaluated { task, .. }
                | WorkerEvent::FactorsChecked { task, .. }
                | WorkerEvent::PipelineFinished { task, .. }
                | WorkerEvent::TrainingFinished { task, .. } => Some(task),
                _ => None,
            };
            if let Some(task) = task {
                let path = root.join(format!(".jfquant/runs/{task}/payload.json"));
                crate::atomic_json(&path, &event).map_err(|e| format!("结果写入失败：{e}"))?;
            }
        }
        self.emit(event);
        Ok(())
    }

    fn emit(&self, event: WorkerEvent) {
        let _ = self.events.send(event);
        self.ctx.request_repaint();
    }

    fn path(&self, path: PathBuf) -> PathBuf {
        if path.is_absolute() {
            path
        } else {
            self.root.as_ref().map(|r| r.join(&path)).unwrap_or(path)
        }
    }
    fn context(&self, task: Uuid, cancel: Cancellation) -> analytics_core::task::TaskContext {
        let events = self.events.clone();
        let ctx = self.ctx.clone();
        let root = self.root.clone();
        analytics_core::task::TaskContext::new(task, cancel, move |message| {
            persist_progress(root.as_deref(), task, message);
            let _ = events.send(WorkerEvent::Progress {
                task,
                message: message.into(),
            });
            ctx.request_repaint();
        })
    }
    fn dispatch(&mut self, command: WorkerCommand) {
        let _research_session = crate::research::worker::TaskSession;
        match command {
            #[cfg(any(test, feature = "test-support"))]
            WorkerCommand::TestingPause { task, milliseconds } => {
                self.run_task(task, TaskKind::Workflow, "testing pause".into(), |worker| {
                    let context =
                        worker.context(task, worker.active_cancel.clone().unwrap_or_default());
                    context
                        .report("testing cancellable computation")
                        .map_err(|e| e.to_string())?;
                    let start = std::time::Instant::now();
                    while start.elapsed() < std::time::Duration::from_millis(milliseconds) {
                        context.check().map_err(|e| e.to_string())?;
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Ok(())
                });
            }
            WorkerCommand::Shutdown => {
                self.stop_reason = Some("后台已停止".into());
            }
            WorkerCommand::WithContext {
                task,
                command,
                cancel,
            } => {
                let context = self.context(task, cancel.clone());
                self.active_cancel = Some(cancel);
                context.execute(|| self.dispatch(*command));
            }
            WorkerCommand::CancelTask { .. } => {
                if let Some(cancel) = &self.active_cancel {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            WorkerCommand::ExecuteWorkflow {
                task,
                request,
                cancel,
            } => {
                self.record_request = serde_json::to_value(&request).ok();
                self.active_cancel = Some(cancel.clone());
                self.run_task(task, TaskKind::Workflow, request.label(), |worker| {
                    let root = worker.root.as_ref().ok_or("请先选择工作区")?;
                    let workspace = crate::Workspace::open(root).map_err(|e| e.to_string())?;
                    let mut record = worker.active_record.clone().ok_or("任务记录未创建")?;
                    if record.request.get("inputs").is_none() {
                        record.request =
                            crate::snapshot(&workspace, &request).map_err(|e| e.to_string())?;
                        record.signature =
                            crate::signature(&record.request).map_err(|e| e.to_string())?;
                        workspace.save_run(&record).map_err(|e| e.to_string())?;
                    }
                    worker.active_record = Some(record.clone());
                    let context = worker.context(task, cancel);
                    worker.outcome = Some(
                        crate::execute(&workspace, &request, &record, &context)
                            .map_err(|e| e.to_string())?,
                    );
                    Ok(())
                });
            }
            #[cfg(any(test, feature = "test-support"))]
            WorkerCommand::PanicTask { task, kind } => {
                self.run_task(task, kind, "测试内部错误".into(), |worker| {
                    let _ = worker.engine()?;
                    panic!("模拟后台内部错误");
                })
            }
            #[cfg(any(test, feature = "test-support"))]
            WorkerCommand::PanicOutsideTask => panic!("模拟任务外内部错误"),
            WorkerCommand::ScanDataset { task, root } => {
                let root = self.path(root);
                self.run_task(
                    task,
                    TaskKind::Scan,
                    format!("扫描数据集 {}", root.display()),
                    |worker| {
                        let catalog =
                            ParquetCatalog::open(&root).map_err(|error| error.to_string())?;
                        let files = catalog.scan().map_err(|error| error.to_string())?;
                        worker.emit_result(WorkerEvent::DatasetScanned { task, root, files })?;
                        Ok(())
                    },
                )
            }
            WorkerCommand::DescribeQuery { task, sql } => {
                self.run_task(
                    task,
                    TaskKind::Describe,
                    "描述查询结构".into(),
                    |worker| {
                        let columns = worker
                            .engine()?
                            .describe(&sql)
                            .map_err(|error| error.to_string())?;
                        worker.emit_result(WorkerEvent::QueryDescribed { task, columns })?;
                        Ok(())
                    },
                );
            }
            WorkerCommand::RunQuery { task, request } => {
                self.run_task(task, TaskKind::Query, "执行查询".into(), |worker| {
                    let page = worker.query_page(request)?;
                    worker.emit_result(WorkerEvent::QueryPaged { task, page })?;
                    Ok(())
                });
            }
            WorkerCommand::SessionNext {
                task,
                session,
                after,
            } => {
                self.run_task(task, TaskKind::Query, "查询下一页".into(), |worker| {
                    let (query, used) = worker
                        .query_sessions
                        .get_mut(&session)
                        .ok_or("query_expired: 查询会话已过期，请重新执行查询。")?;
                    *used = std::time::Instant::now();
                    let page = query.next(after)?;
                    worker.emit_result(WorkerEvent::QueryPaged { task, page })?;
                    Ok(())
                });
            }
            WorkerCommand::SessionQuery {
                task,
                session,
                request,
            } => {
                self.run_task(task, TaskKind::Query, "执行查询".into(), |worker| {
                    if request.after.is_none() {
                        worker.query_sessions.insert(
                            session,
                            (
                                QuerySession::new(worker.root.clone()),
                                std::time::Instant::now(),
                            ),
                        );
                    }
                    let (query, used) = worker
                        .query_sessions
                        .get_mut(&session)
                        .ok_or("query_expired: 查询会话已过期，请重新执行查询。")?;
                    *used = std::time::Instant::now();
                    let page = query.query_page(request)?;
                    worker.emit_result(WorkerEvent::QueryPaged { task, page })?;
                    Ok(())
                });
            }
            WorkerCommand::ExportQuery { task, request } => {
                self.run_task(
                    task,
                    TaskKind::Export,
                    "导出查询结果".into(),
                    |worker| {
                        let mut request = request;
                        request.destination = worker.path(request.destination);
                        let destination = request.destination.clone();
                        worker
                            .engine()?
                            .export(request)
                            .map_err(|error| error.to_string())?;
                        worker.emit_result(WorkerEvent::Exported { task, destination })?;
                        Ok(())
                    },
                );
            }
            WorkerCommand::PreviewFactor {
                task,
                draft,
                cancel,
            } => {
                self.record_request = serde_json::to_value(&draft).ok();
                self.active_cancel = Some(cancel.clone());
                self.run_task(
                    task,
                    TaskKind::Pipeline,
                    "自定义因子合成预览".into(),
                    |worker| {
                        let preview = crate::custom_factors::preview(&draft, cancel)
                            .map_err(|e| e.to_string())?;
                        if let Some(root) = &worker.root {
                            crate::atomic_json(
                                &root.join(format!(".jfquant/runs/{task}/artifacts.json")),
                                &preview,
                            )
                            .map_err(|e| e.to_string())?;
                        }
                        worker.emit_result(WorkerEvent::FactorPreviewed { task, preview })?;
                        Ok(())
                    },
                );
            }
            WorkerCommand::CheckFactors {
                task,
                mut request,
                cancel,
            } => {
                request.input.market_path = self.path(request.input.market_path);
                request.input.universe_path = self.path(request.input.universe_path);
                request.input.output_dir = if let Some(root) = &self.root {
                    root.join(format!(".jfquant/runs/{task}"))
                } else {
                    self.path(request.input.output_dir)
                };
                self.record_request = serde_json::to_value(&request).ok();
                self.active_cancel = Some(cancel.clone());
                self.run_task(
                    task,
                    TaskKind::Pipeline,
                    "检查组合数据".into(),
                    |worker| {
                        let (progress, thread) = worker.pipeline_progress(task);
                        let result = crate::input_check::run(&request, progress, cancel)
                            .map_err(pipeline_error_message);
                        let _ = thread.join();
                        let report = result?;
                        if let Some(root) = &worker.root {
                            crate::atomic_json(
                                &root.join(format!(".jfquant/runs/{task}/artifacts.json")),
                                &report,
                            )
                            .map_err(|e| e.to_string())?;
                        }
                        worker.emit_result(WorkerEvent::FactorsChecked { task, report })?;
                        Ok(())
                    },
                );
            }
            WorkerCommand::EvaluateFactors {
                task,
                mut request,
                cancel,
            } => {
                request.input.market_path = self.path(request.input.market_path);
                request.input.universe_path = self.path(request.input.universe_path);
                request.input.output_dir = if let Some(root) = &self.root {
                    root.join(format!(".jfquant/runs/{task}"))
                } else {
                    self.path(request.input.output_dir)
                };
                self.record_request = serde_json::to_value(&request).ok();
                self.active_cancel = Some(cancel.clone());
                self.run_task(
                    task,
                    TaskKind::Pipeline,
                    "因子有效性测试".into(),
                    |worker| {
                        let (progress, thread) = worker.pipeline_progress(task);
                        let result = crate::factor_evaluation::run(&request, progress, cancel)
                            .map_err(pipeline_error_message);
                        let _ = thread.join();
                        let report = result?;
                        if let Some(root) = &worker.root {
                            crate::atomic_json(
                                &root.join(format!(".jfquant/runs/{task}/artifacts.json")),
                                &report,
                            )
                            .map_err(|e| e.to_string())?;
                        }
                        worker.emit_result(WorkerEvent::FactorsEvaluated { task, report })?;
                        Ok(())
                    },
                );
            }
            WorkerCommand::RunPipeline {
                task,
                request,
                cancel,
            } => {
                let mut request = request;
                request.market_path = self.path(request.market_path);
                request.universe_path = self.path(request.universe_path);
                request.output_dir = if let Some(root) = &self.root {
                    root.join(format!(".jfquant/runs/{task}"))
                } else {
                    self.path(request.output_dir)
                };
                self.record_request = serde_json::to_value(&request).ok();
                self.active_cancel = Some(cancel.clone());
                self.run_task(
                    task,
                    TaskKind::Pipeline,
                    "运行因子流水线".into(),
                    |worker| {
                        let (progress, progress_thread) = worker.pipeline_progress(task);
                        let artifact_result = run_factor_pipeline(&request, progress, cancel)
                            .map_err(pipeline_error_message);
                        let _ = progress_thread.join();
                        let artifact = artifact_result?;
                        let summary = worker.summarize(task, &artifact)?;
                        worker.save_summary(task, &summary)?;
                        worker.emit_result(WorkerEvent::PipelineFinished { task, summary })?;
                        Ok(())
                    },
                );
            }
            WorkerCommand::TrainFromCache {
                task,
                cache_path,
                universe_id,
                output_dir,
                config,
                cancel,
            } => {
                let cache_path = self.path(cache_path);
                let output_dir = if let Some(root) = &self.root {
                    root.join(format!(".jfquant/runs/{task}"))
                } else {
                    self.path(output_dir)
                };
                self.record_request = Some(
                    serde_json::json!({"cache_path":cache_path,"output_dir":output_dir,"universe_id":universe_id,"training":config}),
                );
                self.active_cancel = Some(cancel.clone());
                self.run_task(task, TaskKind::Training, "训练模型".into(), |worker| {
                    let (progress, progress_thread) = worker.pipeline_progress(task);
                    let artifact_result = train_from_factor_cache(
                        &cache_path,
                        universe_id.as_deref(),
                        &output_dir,
                        &config,
                        progress,
                        cancel,
                    )
                    .map_err(pipeline_error_message);
                    let _ = progress_thread.join();
                    let artifact = artifact_result?;
                    let summary = worker.summarize(task, &artifact)?;
                    worker.save_summary(task, &summary)?;
                    worker.emit_result(WorkerEvent::TrainingFinished { task, summary })?;
                    Ok(())
                });
            }
        }
    }

    fn pipeline_progress(
        &self,
        task: Uuid,
    ) -> (Sender<PipelineEvent>, std::thread::JoinHandle<()>) {
        let (tx, rx) = channel::<PipelineEvent>();
        let events = self.events.clone();
        let ctx = self.ctx.clone();
        let root = self.root.clone();
        let thread = std::thread::spawn(move || {
            while let Ok(event) = rx.recv() {
                persist_progress(root.as_deref(), task, &describe_pipeline_event(&event));
                let _ = events.send(WorkerEvent::Progress {
                    task,
                    message: describe_pipeline_event(&event),
                });
                ctx.request_repaint();
            }
        });
        (tx, thread)
    }

    fn save_summary(&self, task: Uuid, summary: &RunSummary) -> Result<(), String> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let mut artifact = serde_json::to_value(&summary.artifact).map_err(|e| e.to_string())?;
        fn relative(value: &mut serde_json::Value, root: &std::path::Path) {
            match value {
                serde_json::Value::String(s) => {
                    if let Ok(p) = std::path::Path::new(s.as_str()).strip_prefix(root) {
                        *s = p.display().to_string();
                    }
                }
                serde_json::Value::Object(map) => {
                    for v in map.values_mut() {
                        relative(v, root);
                    }
                }
                _ => {}
            }
        }
        relative(&mut artifact, root);
        let metrics = summary
            .metrics_json
            .as_ref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
        let value = serde_json::json!({"artifact":artifact,"metrics":metrics,"importance":summary.importance,"preview":summary.preview});
        std::fs::write(
            root.join(format!(".jfquant/runs/{task}/artifacts.json")),
            serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())
    }

    fn summarize(&mut self, task: Uuid, artifact: &PipelineArtifact) -> Result<RunSummary, String> {
        let metrics_json = std::fs::read_to_string(&artifact.training.metrics_path).ok();
        let mut importance = Vec::new();
        if let Ok(bytes) = std::fs::read(&artifact.training.feature_importance_path)
            && let Ok(gains) = serde_json::from_slice::<Vec<f64>>(&bytes)
        {
            let names = crate::research::load_manifest(&artifact.training.manifest_path)
                .map(|manifest| {
                    manifest
                        .features
                        .iter()
                        .map(|feature| feature.name.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for (index, gain) in gains.into_iter().enumerate() {
                let name = names
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| format!("f{index}"));
                importance.push((name, gain));
            }
            importance.sort_by(|a, b| b.1.total_cmp(&a.1));
        }
        let preview = self.predictions_preview(task, artifact);
        Ok(RunSummary {
            artifact: artifact.clone(),
            metrics_json,
            importance,
            preview,
        })
    }

    fn predictions_preview(
        &mut self,
        task: Uuid,
        artifact: &PipelineArtifact,
    ) -> Option<QueryPage> {
        let path = artifact
            .predictions_path
            .display()
            .to_string()
            .replace('\'', "''");
        let request = PageRequest {
            sql: format!("SELECT * FROM read_parquet('{path}')"),
            limit: 100,
            sort: Vec::new(),
            after: None,
        };
        match self.engine() {
            Ok(engine) => match engine.query_page(request) {
                Ok(page) => Some(page),
                Err(error) => {
                    self.emit(WorkerEvent::Progress {
                        task,
                        message: format!("预测预览不可用：{error}"),
                    });
                    None
                }
            },
            Err(_) => None,
        }
    }
}

pub fn pipeline_error_message(error: PipelineError) -> String {
    if matches!(&error, PipelineError::Cancelled)
        || matches!(&error, PipelineError::Ml(crate::research::MlError::Native(message)) if message == "training cancelled")
    {
        "任务已取消".into()
    } else {
        error.to_string()
    }
}

/// Maps a pipeline stage event to a display line.
fn describe_pipeline_event(event: &PipelineEvent) -> String {
    match event {
        PipelineEvent::CheckingInputs { stage } => format!("组合检查：{stage}"),
        PipelineEvent::LoadingMarket => "正在加载行情与股票池…".into(),
        PipelineEvent::MarketLoaded { rows } => format!("行情加载完成：{rows} 行"),
        PipelineEvent::Factors(FactorEvent::Started { total_factors }) => {
            format!("开始计算 {total_factors} 个因子")
        }
        PipelineEvent::Factors(FactorEvent::Progress {
            factor_id,
            completed,
            total,
        }) => format!("因子 {factor_id}（{completed}/{total}）"),
        PipelineEvent::Factors(FactorEvent::Finished { output_rows }) => {
            format!("因子计算完成：{output_rows} 行")
        }
        PipelineEvent::Training(TrainEvent::Started { rows, features }) => {
            format!("开始训练：{rows} 行 × {features} 特征")
        }
        PipelineEvent::Training(TrainEvent::Iteration { iteration, metrics }) => {
            let metrics = metrics
                .iter()
                .map(|value| format!("{value:.6}"))
                .collect::<Vec<_>>()
                .join(" / ");
            format!("迭代 {iteration}：{metrics}")
        }
        PipelineEvent::Training(TrainEvent::Finished { iterations }) => {
            format!("训练完成：{iterations} 轮")
        }
        PipelineEvent::Training(TrainEvent::Cancelled) => "训练已取消".into(),
        PipelineEvent::WritingArtifacts => "正在写出产物…".into(),
    }
}

fn persist_progress(root: Option<&std::path::Path>, task: Uuid, message: &str) {
    if let Some(root) = root {
        let result = (|| -> crate::Result<()> {
            let w = crate::Workspace::open(root)?;
            let run: crate::RunRecord = serde_json::from_slice(&std::fs::read(
                root.join(format!(".jfquant/runs/{task}/run.json")),
            )?)?;
            w.progress(&run, message)
        })();
        if let Err(e) = result {
            eprintln!("task log write failed: {e}");
        }
    }
}

#[cfg(test)]
mod session_lifecycle_tests {
    use super::*;
    #[test]
    fn service_accepted_snapshot_is_not_rewritten_when_execution_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let w = crate::Workspace::open(tmp.path()).unwrap();
        let task = Uuid::new_v4();
        let request = crate::WorkflowRequest::CreateDemo {};
        let mut frozen = crate::snapshot(&w, &request).unwrap();
        frozen["accepted_marker"] = serde_json::json!("keep the original snapshot");
        let record = w.begin(task, "demo", frozen.clone()).unwrap();
        let (worker, events) = spawn_in_workspace(Wake::default(), Some(tmp.path().into()));
        worker
            .send(WorkerCommand::ExecuteWorkflow {
                task,
                request: Box::new(request),
                cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            })
            .unwrap();
        loop {
            let event = events
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            assert!(!matches!(event, WorkerEvent::Failed { .. }));
            if matches!(event, WorkerEvent::Finished { .. }) {
                break;
            }
        }
        let actual = w.runs().unwrap().pop().unwrap();
        assert_eq!(actual.request, frozen);
        assert_eq!(actual.signature, record.signature);
        worker.send(WorkerCommand::Shutdown).unwrap();
    }
    #[test]
    fn failed_payload_write_marks_task_failed_instead_of_success() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = crate::Workspace::open(tmp.path()).unwrap();
        let task = Uuid::new_v4();
        let run = workspace
            .begin(task, "query", serde_json::json!({}))
            .unwrap();
        // A directory at the final result path simulates an unwritable output.
        std::fs::create_dir(workspace.resolve(&run.output).join("payload.json")).unwrap();
        let (worker, events) = spawn_in_workspace(Wake::default(), Some(tmp.path().into()));
        worker
            .send(WorkerCommand::RunQuery {
                task,
                request: PageRequest {
                    sql: "SELECT 42".into(),
                    limit: 1,
                    sort: vec![],
                    after: None,
                },
            })
            .unwrap();
        loop {
            let event = events
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            if let WorkerEvent::Failed { message, .. } = event {
                assert!(message.contains("结果写入失败"), "{message}");
                break;
            }
            assert!(!matches!(event, WorkerEvent::Finished { .. }));
        }
        assert_eq!(
            workspace.runs().unwrap()[0].status,
            crate::RunStatus::Failed
        );
        worker.send(WorkerCommand::Shutdown).unwrap();
    }
    #[test]
    fn expired_query_handles_are_released_on_their_own_thread() {
        let (events, _) = channel();
        let mut worker = Worker {
            root: None,
            record_request: None,
            active_record: None,
            active_cancel: None,
            outcome: None,
            query: QuerySession::default(),
            events,
            ctx: Wake::default(),
            stop_reason: None,
            query_sessions: std::collections::HashMap::new(),
        };
        let session = Uuid::new_v4();
        worker.dispatch(WorkerCommand::SessionQuery {
            task: Uuid::new_v4(),
            session,
            request: PageRequest {
                sql: "SELECT * FROM range(4)".into(),
                limit: 2,
                sort: vec![],
                after: None,
            },
        });
        worker.query_sessions.get_mut(&session).unwrap().1 =
            std::time::Instant::now() - std::time::Duration::from_secs(601);
        worker.expire_queries();
        assert!(worker.query_sessions.is_empty());
    }
}

mod query;
use query::QuerySession;
mod lifecycle;

/// Synchronous harness for backend acceptance tests; absent from production builds.
#[cfg(any(test, feature = "test-support"))]
pub struct TestWorker(Worker);
#[cfg(any(test, feature = "test-support"))]
impl TestWorker {
    pub fn new(events: Sender<WorkerEvent>) -> Self {
        Self(Worker {
            root: None,
            record_request: None,
            active_record: None,
            active_cancel: None,
            outcome: None,
            query: QuerySession::default(),
            events,
            ctx: Wake::default(),
            stop_reason: None,
            query_sessions: Default::default(),
        })
    }
    pub fn engine(&mut self) -> Result<&mut DuckDbEngine, String> {
        self.0.engine()
    }
    pub fn query_page(&mut self, request: PageRequest) -> Result<QueryPage, String> {
        self.0.query_page(request)
    }
    pub fn run_task(
        &mut self,
        task: Uuid,
        kind: TaskKind,
        label: String,
        body: impl FnOnce() -> Result<(), String>,
    ) {
        self.0.run_task(task, kind, label, |_| body());
    }
    pub fn stop_reason(&self) -> Option<&str> {
        self.0.stop_reason.as_deref()
    }
}
