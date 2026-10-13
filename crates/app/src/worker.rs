//! Background worker thread that owns every native handle.
//!
//! Per the repository engineering rules, the DuckDB connection and all
//! LightGBM handles live exclusively on this single worker thread; the egui
//! thread only exchanges plain data through [`WorkerCommand`] /
//! [`WorkerEvent`] channels. Cancellation is cooperative: the UI keeps a
//! shared `Arc<AtomicBool>` flag for the active task and the pipeline checks
//! it between stages.

use analytics_core::{
    ColumnSchema, DatasetFile, ExportRequest, PageRequest, QueryPage, ScalarValue, SortKey,
};
use analytics_engine::{DuckDbEngine, QueryEngine};
use analytics_factor::FactorEvent;
use analytics_ml::TrainEvent;
use analytics_ml::pipeline::{
    Cancellation, PipelineArtifact, PipelineError, PipelineEvent, PipelineRequest, TrainingConfig,
    run_factor_pipeline, train_from_factor_cache,
};
use analytics_storage::ParquetCatalog;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use uuid::Uuid;

/// Subsystem a background task belongs to; used to route errors back to the
/// panel that issued the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

/// Commands sent from the egui thread to the worker.
#[derive(Debug)]
pub enum WorkerCommand {
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
    #[cfg(test)]
    /// Injects a task panic to exercise failure routing and worker retirement.
    PanicTask { task: Uuid, kind: TaskKind },
    #[cfg(test)]
    /// Injects a panic outside the task boundary to exercise the thread guard.
    PanicOutsideTask,
}

/// Display-ready summary of a finished pipeline or retraining run.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
pub enum WorkerEvent {
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
    tx: Sender<WorkerCommand>,
}

impl WorkerHandle {
    #[cfg(test)]
    pub(crate) fn recording() -> (Self, Receiver<WorkerCommand>) {
        let (tx, rx) = channel();
        (Self { tx }, rx)
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
            Self::ScanDataset { task, .. } => (*task, TaskKind::Scan),
            Self::DescribeQuery { task, .. } => (*task, TaskKind::Describe),
            Self::RunQuery { task, .. } => (*task, TaskKind::Query),
            Self::ExportQuery { task, .. } => (*task, TaskKind::Export),
            Self::RunPipeline { task, .. } => (*task, TaskKind::Pipeline),
            Self::TrainFromCache { task, .. } => (*task, TaskKind::Training),
            #[cfg(test)]
            Self::PanicTask { task, kind } => (*task, *kind),
            #[cfg(test)]
            Self::PanicOutsideTask => unreachable!("fault injection has no task"),
        }
    }
}

/// Spawns the worker thread and returns both protocol halves.
pub fn spawn(ctx: egui::Context) -> (WorkerHandle, Receiver<WorkerEvent>) {
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
                    engine: None,
                    events: event_tx.clone(),
                    ctx: ctx.clone(),
                    stop_reason: None,
                    query_snapshot: None,
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

struct Worker {
    engine: Option<DuckDbEngine>,
    events: Sender<WorkerEvent>,
    ctx: egui::Context,
    stop_reason: Option<String>,
    query_snapshot: Option<QuerySnapshot>,
}

struct QuerySnapshot {
    request: PageRequest,
    sql: String,
    ordinal: String,
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("未知 panic")
}

impl Worker {
    fn run(&mut self, commands: Receiver<WorkerCommand>) -> Option<String> {
        while let Ok(command) = commands.recv() {
            self.dispatch(command);
            if self.stop_reason.is_some() {
                break;
            }
        }
        self.stop_reason.take()
    }

    fn engine(&mut self) -> Result<&mut DuckDbEngine, String> {
        if self.engine.is_none() {
            self.engine = Some(
                DuckDbEngine::open_in_memory()
                    .map_err(|error| format!("DuckDB 初始化失败：{error}"))?,
            );
        }
        Ok(self.engine.as_mut().expect("engine initialized"))
    }

    fn emit(&self, event: WorkerEvent) {
        let _ = self.events.send(event);
        self.ctx.request_repaint();
    }

    fn query_page(&mut self, request: PageRequest) -> Result<QueryPage, String> {
        // Desktop cursors contain a hidden ordinal, not the selected column's
        // value. Sort all remaining columns to make ties deterministic; identical
        // rows remain distinct and NULLs are placed last in either direction.
        if request.after.is_none() {
            self.query_snapshot = None;
            if let Some(engine) = &mut self.engine {
                engine
                    .clear_query_snapshot()
                    .map_err(|error| error.to_string())?;
            }
        }
        let mut validation = request.clone();
        validation.after = None;
        validation.validate().map_err(|error| error.to_string())?;
        if let Some(cursor) = &request.after
            && !matches!(cursor.0.as_slice(), [ScalarValue::Int64(value)] if *value > 0)
        {
            return Err("分页游标无效，请重新执行查询。".into());
        }
        if request.after.is_some() {
            let snapshot = self
                .query_snapshot
                .as_ref()
                .ok_or("分页结果已失效，请重新执行查询。")?;
            if !same_query(&request, &snapshot.request) {
                return Err("查询条件已改变，请重新执行查询。".into());
            }
            let sql = snapshot.sql.clone();
            let ordinal = snapshot.ordinal.clone();
            return self.snapshot_page(sql, ordinal, request.limit, request.after);
        }
        let engine = self.engine()?;
        let columns = engine
            .describe(&request.sql)
            .map_err(|error| error.to_string())?;
        let mut sort = request.sort.clone();
        for key in &mut sort {
            key.column = columns
                .iter()
                .find(|column| column.name.eq_ignore_ascii_case(&key.column))
                .ok_or_else(|| format!("排序列不存在：{}。请选择默认排序后重新执行。", key.column))?
                .name
                .clone();
        }
        for column in &columns {
            if !sort.iter().any(|key| key.column == column.name) {
                sort.push(SortKey {
                    column: column.name.clone(),
                    descending: false,
                });
            }
        }
        let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
        let order = sort
            .iter()
            .map(|key| {
                format!(
                    "q.{} {} NULLS LAST",
                    quote(&key.column),
                    if key.descending { "DESC" } else { "ASC" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut ordinal = "__jfquant_page_row".to_string();
        while columns
            .iter()
            .any(|column| column.name.eq_ignore_ascii_case(&ordinal))
        {
            ordinal.push('_');
        }
        let sql = format!(
            "SELECT row_number() OVER (ORDER BY {order}) AS {}, q.* FROM ({}) AS q",
            quote(&ordinal),
            request.sql
        );
        let sql = engine
            .replace_query_snapshot(&sql)
            .map_err(|error| error.to_string())?;
        self.query_snapshot = Some(QuerySnapshot {
            request: request.clone(),
            sql: sql.clone(),
            ordinal: ordinal.clone(),
        });
        self.snapshot_page(sql, ordinal, request.limit, request.after)
    }

    fn snapshot_page(
        &mut self,
        sql: String,
        ordinal: String,
        limit: u32,
        after: Option<analytics_core::KeysetCursor>,
    ) -> Result<QueryPage, String> {
        let mut page = self
            .engine()?
            .query_page(PageRequest {
                sql,
                limit,
                sort: vec![SortKey {
                    column: ordinal,
                    descending: false,
                }],
                after,
            })
            .map_err(|error| error.to_string())?;
        page.columns.remove(0);
        for row in &mut page.rows {
            row.remove(0);
        }
        Ok(page)
    }

    fn dispatch(&mut self, command: WorkerCommand) {
        match command {
            #[cfg(test)]
            WorkerCommand::PanicTask { task, kind } => {
                self.run_task(task, kind, "测试内部错误".into(), |worker| {
                    let _ = worker.engine()?;
                    panic!("模拟后台内部错误");
                })
            }
            #[cfg(test)]
            WorkerCommand::PanicOutsideTask => panic!("模拟任务外内部错误"),
            WorkerCommand::ScanDataset { task, root } => self.run_task(
                task,
                TaskKind::Scan,
                format!("扫描数据集 {}", root.display()),
                |worker| {
                    let catalog = ParquetCatalog::open(&root).map_err(|error| error.to_string())?;
                    let files = catalog.scan().map_err(|error| error.to_string())?;
                    worker.emit(WorkerEvent::DatasetScanned { task, root, files });
                    Ok(())
                },
            ),
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
                        worker.emit(WorkerEvent::QueryDescribed { task, columns });
                        Ok(())
                    },
                );
            }
            WorkerCommand::RunQuery { task, request } => {
                self.run_task(task, TaskKind::Query, "执行查询".into(), |worker| {
                    let page = worker.query_page(request)?;
                    worker.emit(WorkerEvent::QueryPaged { task, page });
                    Ok(())
                });
            }
            WorkerCommand::ExportQuery { task, request } => {
                self.run_task(
                    task,
                    TaskKind::Export,
                    "导出查询结果".into(),
                    |worker| {
                        let destination = request.destination.clone();
                        worker
                            .engine()?
                            .export(request)
                            .map_err(|error| error.to_string())?;
                        worker.emit(WorkerEvent::Exported { task, destination });
                        Ok(())
                    },
                );
            }
            WorkerCommand::RunPipeline {
                task,
                request,
                cancel,
            } => {
                self.run_task(
                    task,
                    TaskKind::Pipeline,
                    "运行因子流水线".into(),
                    |worker| {
                        let progress = worker.pipeline_progress(task);
                        let artifact = run_factor_pipeline(&request, progress, cancel)
                            .map_err(pipeline_error_message)?;
                        let summary = worker.summarize(task, &artifact)?;
                        worker.emit(WorkerEvent::PipelineFinished { task, summary });
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
                self.run_task(task, TaskKind::Training, "训练模型".into(), |worker| {
                    let progress = worker.pipeline_progress(task);
                    let artifact = train_from_factor_cache(
                        &cache_path,
                        universe_id.as_deref(),
                        &output_dir,
                        &config,
                        progress,
                        cancel,
                    )
                    .map_err(pipeline_error_message)?;
                    let summary = worker.summarize(task, &artifact)?;
                    worker.emit(WorkerEvent::TrainingFinished { task, summary });
                    Ok(())
                });
            }
        }
    }

    fn run_task(
        &mut self,
        task: Uuid,
        kind: TaskKind,
        label: String,
        body: impl FnOnce(&mut Self) -> Result<(), String>,
    ) {
        self.emit(WorkerEvent::Started { task, kind, label });
        let result = match catch_unwind(AssertUnwindSafe(|| body(self))) {
            Ok(result) => result,
            Err(payload) => {
                // Stop after unwinding: mutable/native state may be inconsistent.
                // Its handles are dropped here on their owning worker thread.
                let message = format!(
                    "后台任务发生内部错误：{}。请重启后台后重试。",
                    panic_message(payload.as_ref())
                );
                self.stop_reason = Some(message.clone());
                Err(message)
            }
        };
        match result {
            Ok(()) => self.emit(WorkerEvent::Finished { task }),
            Err(message) => {
                self.emit(WorkerEvent::Failed {
                    task,
                    kind,
                    message,
                });
            }
        }
    }

    fn pipeline_progress(&self, task: Uuid) -> Sender<PipelineEvent> {
        let (tx, rx) = channel::<PipelineEvent>();
        let events = self.events.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            while let Ok(event) = rx.recv() {
                let _ = events.send(WorkerEvent::Progress {
                    task,
                    message: describe_pipeline_event(&event),
                });
                ctx.request_repaint();
            }
        });
        tx
    }

    fn summarize(&mut self, task: Uuid, artifact: &PipelineArtifact) -> Result<RunSummary, String> {
        let metrics_json = std::fs::read_to_string(&artifact.training.metrics_path).ok();
        let mut importance = Vec::new();
        if let Ok(bytes) = std::fs::read(&artifact.training.feature_importance_path)
            && let Ok(gains) = serde_json::from_slice::<Vec<f64>>(&bytes)
        {
            let names = analytics_ml::load_manifest(&artifact.training.manifest_path)
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

fn same_query(a: &PageRequest, b: &PageRequest) -> bool {
    a.sql == b.sql
        && a.limit == b.limit
        && a.sort.len() == b.sort.len()
        && a.sort
            .iter()
            .zip(&b.sort)
            .all(|(a, b)| a.column == b.column && a.descending == b.descending)
}

fn pipeline_error_message(error: PipelineError) -> String {
    if matches!(&error, PipelineError::Cancelled)
        || matches!(&error, PipelineError::Ml(analytics_ml::MlError::Native(message)) if message == "training cancelled")
    {
        "任务已取消".into()
    } else {
        error.to_string()
    }
}

/// Maps a pipeline stage event to a display line.
fn describe_pipeline_event(event: &PipelineEvent) -> String {
    match event {
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

#[cfg(test)]
mod tests {
    use super::*;
    use analytics_core::{ExportFormat, ScalarValue, SortKey};
    use analytics_ml::pipeline::cancellation;
    use std::time::{Duration, Instant};

    fn collect(rx: &Receiver<WorkerEvent>, task: Uuid) -> Vec<WorkerEvent> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut events = Vec::new();
        loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("worker must terminate task within 30 seconds");
            let terminal = matches!(&event, WorkerEvent::Finished { task: id }
                | WorkerEvent::Failed { task: id, .. } if *id == task);
            events.push(event);
            if terminal {
                return events;
            }
        }
    }

    fn successful(events: &[WorkerEvent], task: Uuid) {
        assert!(
            matches!(events.first(), Some(WorkerEvent::Started { task: id, .. }) if *id == task)
        );
        assert!(
            matches!(events.last(), Some(WorkerEvent::Finished { task: id }) if *id == task),
            "{events:?}"
        );
    }

    #[test]
    fn desktop_pages_preserve_duplicate_and_null_keys_and_quote_column_names() {
        let (tx, _) = channel();
        let mut worker = Worker {
            engine: None,
            events: tx,
            ctx: egui::Context::default(),
            stop_reason: None,
            query_snapshot: None,
        };
        for descending in [false, true] {
            let sql = "SELECT * FROM (VALUES (NULL, 5), (1, 2), (1, 2), (1, 3), (2, 4), (NULL, 6)) AS v(\"排序 列\", \"__JFQUANT_PAGE_ROW\")";
            let mut after = None;
            let mut rows = Vec::new();
            let mut pages = 0;
            loop {
                let page = worker
                    .query_page(PageRequest {
                        sql: sql.into(),
                        limit: 2,
                        sort: vec![SortKey {
                            column: "排序 列".into(),
                            descending,
                        }],
                        after,
                    })
                    .unwrap();
                assert_eq!(page.columns.len(), 2);
                assert_eq!(page.columns[1].name, "__JFQUANT_PAGE_ROW");
                rows.extend(page.rows);
                after = page.next_cursor;
                pages += 1;
                assert!(pages <= 3, "pagination did not terminate");
                if after.is_none() {
                    break;
                }
            }
            let pairs = if descending {
                vec![
                    (Some(2), 4),
                    (Some(1), 2),
                    (Some(1), 2),
                    (Some(1), 3),
                    (None, 5),
                    (None, 6),
                ]
            } else {
                vec![
                    (Some(1), 2),
                    (Some(1), 2),
                    (Some(1), 3),
                    (Some(2), 4),
                    (None, 5),
                    (None, 6),
                ]
            };
            let expected: Vec<_> = pairs
                .into_iter()
                .map(|(key, id)| {
                    vec![
                        key.map(ScalarValue::Int64).unwrap_or(ScalarValue::Null),
                        ScalarValue::Int64(id),
                    ]
                })
                .collect();
            assert_eq!(rows, expected);
        }
    }

    #[test]
    fn desktop_snapshot_survives_source_changes_and_rejects_mismatched_requests() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.parquet");
        let (tx, _) = channel();
        let mut worker = Worker {
            engine: None,
            events: tx,
            ctx: egui::Context::default(),
            stop_reason: None,
            query_snapshot: None,
        };
        worker
            .engine()
            .unwrap()
            .export(ExportRequest {
                sql: "SELECT range AS id FROM range(6)".into(),
                destination: path.clone(),
                format: ExportFormat::Parquet,
                compression: None,
            })
            .unwrap();
        let mut request = PageRequest {
            sql: format!(
                "SELECT * FROM read_parquet('{}')",
                path.display().to_string().replace('\'', "''")
            ),
            limit: 2,
            sort: vec![],
            after: None,
        };
        let first = worker.query_page(request.clone()).unwrap();
        request.after = first.next_cursor;
        std::fs::remove_file(&path).unwrap();
        let second = worker.query_page(request.clone()).unwrap();
        assert_eq!(
            second.rows,
            vec![vec![ScalarValue::Int64(2)], vec![ScalarValue::Int64(3)]]
        );
        let mut mismatch = request.clone();
        mismatch.limit = 3;
        assert!(
            worker
                .query_page(mismatch)
                .unwrap_err()
                .contains("查询条件已改变")
        );
        request.after = second.next_cursor;
        assert_eq!(
            worker.query_page(request.clone()).unwrap().rows,
            vec![vec![ScalarValue::Int64(4)], vec![ScalarValue::Int64(5)]]
        );
        let engine = worker.engine().unwrap();
        let snapshot = engine
            .replace_query_snapshot("SELECT random() AS x")
            .unwrap();
        let read = PageRequest {
            sql: snapshot,
            limit: 1,
            sort: vec![],
            after: None,
        };
        assert_eq!(
            engine.query_page(read.clone()).unwrap().rows,
            engine.query_page(read).unwrap().rows
        );
        assert!(engine.replace_query_snapshot("SELECT FROM").is_err());
        assert!(
            worker
                .query_page(PageRequest {
                    after: None,
                    ..request.clone()
                })
                .is_err()
        );
        assert!(worker.query_page(request).unwrap_err().contains("失效"));
    }

    #[test]
    fn engine_keyset_handles_null_composite_keys_and_nonfinite_numbers() {
        let mut engine = DuckDbEngine::open_in_memory().unwrap();
        for descending in [false, true] {
            let mut after = None;
            let mut ids = Vec::new();
            for _ in 0..6 {
                let page = engine.query_page(PageRequest {
                    sql: "SELECT * FROM (VALUES (1, 1), (2, 2), (NULL, 3), (NULL, 4)) AS v(k, id)".into(), limit: 1,
                    sort: vec![SortKey { column: "K".into(), descending }, SortKey { column: "id".into(), descending: false }], after,
                }).unwrap();
                ids.extend(page.rows.iter().map(|row| row[1].clone()));
                after = page.next_cursor;
                if after.is_none() {
                    break;
                }
            }
            assert_eq!(
                ids,
                if descending {
                    vec![2, 1, 3, 4]
                } else {
                    vec![1, 2, 3, 4]
                }
                .into_iter()
                .map(ScalarValue::Int64)
                .collect::<Vec<_>>()
            );
            let mut after = None;
            let mut values = Vec::new();
            for _ in 0..6 {
                let page = engine.query_page(PageRequest {
                    sql: "SELECT * FROM (VALUES (CAST('-Infinity' AS DOUBLE)), (0.0), (CAST('Infinity' AS DOUBLE)), (CAST('NaN' AS DOUBLE)), (NULL)) AS v(k)".into(), limit: 1,
                    sort: vec![SortKey { column: "k".into(), descending }], after,
                }).unwrap();
                values.extend(page.rows.into_iter().flatten());
                after = page.next_cursor;
                if after.is_none() {
                    break;
                }
            }
            assert_eq!(values.len(), 5, "{values:?}");
            assert!(matches!(values.last(), Some(ScalarValue::Null)));
            let floats: Vec<_> = values[..4]
                .iter()
                .map(|value| match value {
                    ScalarValue::Float64(value) => *value,
                    _ => panic!("{value:?}"),
                })
                .collect();
            if descending {
                assert!(floats[0].is_nan());
                assert_eq!(&floats[1..], &[f64::INFINITY, 0.0, f64::NEG_INFINITY]);
            } else {
                assert_eq!(&floats[..3], &[f64::NEG_INFINITY, 0.0, f64::INFINITY]);
                assert!(floats[3].is_nan());
            }
        }
    }

    #[test]
    fn engine_rejects_ambiguous_page_boundary_and_does_not_emit_unsorted_empty_cursor() {
        let mut engine = DuckDbEngine::open_in_memory().unwrap();
        let request = PageRequest {
            sql: "SELECT 1 AS id FROM range(5)".into(),
            limit: 2,
            sort: vec![],
            after: None,
        };
        assert!(
            engine
                .query_page(request.clone())
                .unwrap()
                .next_cursor
                .is_none()
        );
        let mut request = request;
        request.sort = vec![SortKey {
            column: "id".into(),
            descending: false,
        }];
        assert!(
            engine
                .query_page(request)
                .unwrap_err()
                .to_string()
                .contains("not unique")
        );
    }

    #[test]
    fn export_filename_in_current_directory_succeeds() {
        let mut engine = DuckDbEngine::open_in_memory().unwrap();
        let path = PathBuf::from(format!("jfquant-test-{}.csv", Uuid::new_v4()));
        // A basename has an empty parent; it must not be passed to create_dir_all.
        let outcome = engine.export(ExportRequest {
            sql: "SELECT 1 AS id".into(),
            destination: path.clone(),
            format: ExportFormat::Csv,
            compression: None,
        });
        let content = std::fs::read_to_string(&path);
        let _ = std::fs::remove_file(&path);
        outcome.unwrap();
        assert_eq!(content.unwrap(), "id\n1\n");
    }

    #[test]
    fn cancellation_words_in_sql_errors_are_not_misclassified_as_user_cancellation() {
        let (worker, rx) = spawn(egui::Context::default());
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::DescribeQuery {
                task,
                sql: "SELECT * FROM cancelled_table".into(),
            })
            .unwrap();
        let events = collect(&rx, task);
        assert!(
            matches!(events.last(), Some(WorkerEvent::Failed { message, .. }) if message.contains("cancelled_table") && message != "任务已取消")
        );
        assert_eq!(
            pipeline_error_message(PipelineError::Cancelled),
            "任务已取消"
        );
        assert_eq!(
            pipeline_error_message(PipelineError::Ml(analytics_ml::MlError::Native(
                "training cancelled".into()
            ))),
            "任务已取消"
        );
        assert!(
            pipeline_error_message(PipelineError::InvalidInput("cancelled_table".into()))
                .contains("cancelled_table")
        );
    }

    #[test]
    fn task_panics_report_failure_for_every_kind_and_retire_worker() {
        for kind in [
            TaskKind::Scan,
            TaskKind::Describe,
            TaskKind::Query,
            TaskKind::Export,
            TaskKind::Pipeline,
            TaskKind::Training,
        ] {
            let (worker, rx) = spawn(egui::Context::default());
            let task = Uuid::new_v4();
            worker
                .send(WorkerCommand::PanicTask { task, kind })
                .unwrap();
            let events = collect(&rx, task);
            assert!(
                matches!(events.last(), Some(WorkerEvent::Failed { task: id, kind: failed_kind, message })
                if *id == task && *failed_kind == kind && message.contains("模拟后台内部错误")),
                "{events:?}"
            );
            assert!(
                matches!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), WorkerEvent::Unavailable { message }
                if message.contains("模拟后台内部错误"))
            );
            assert!(matches!(
                rx.recv_timeout(Duration::from_secs(10)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            ));
            assert!(
                worker
                    .send(WorkerCommand::DescribeQuery {
                        task: Uuid::new_v4(),
                        sql: "SELECT 1".into()
                    })
                    .is_err()
            );
        }
    }

    #[test]
    fn panic_outside_task_reports_worker_unavailable() {
        let (worker, rx) = spawn(egui::Context::default());
        worker.send(WorkerCommand::PanicOutsideTask).unwrap();
        assert!(
            matches!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), WorkerEvent::Unavailable { message }
            if message.contains("模拟任务外内部错误"))
        );
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(10)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn owned_and_non_string_panic_payloads_become_errors() {
        for owned_string in [true, false] {
            let (tx, rx) = channel();
            let mut worker = Worker {
                engine: None,
                events: tx,
                ctx: egui::Context::default(),
                stop_reason: None,
                query_snapshot: None,
            };
            let task = Uuid::new_v4();
            worker.run_task(task, TaskKind::Query, "测试 panic 内容".into(), |_| {
                if owned_string {
                    std::panic::panic_any(String::from("动态错误内容"));
                }
                std::panic::panic_any(42_u32);
            });
            let expected = if owned_string {
                "动态错误内容"
            } else {
                "未知 panic"
            };
            let events = collect(&rx, task);
            assert!(
                matches!(events.last(), Some(WorkerEvent::Failed { message, .. }) if message.contains(expected))
            );
            assert!(worker.stop_reason.as_ref().unwrap().contains(expected));
        }
    }

    #[test]
    fn file_scan_export_pipeline_and_training_errors_keep_worker_usable() {
        let dir = tempfile::tempdir().unwrap();
        let invalid = dir.path().join("invalid.parquet");
        std::fs::write(&invalid, b"not a parquet file").unwrap();
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, b"a file cannot be an output directory").unwrap();
        let (worker, rx) = spawn(egui::Context::default());
        let scan = Uuid::new_v4();
        let export = Uuid::new_v4();
        let pipeline = Uuid::new_v4();
        let training = Uuid::new_v4();
        let cases = [
            (
                scan,
                TaskKind::Scan,
                WorkerCommand::ScanDataset {
                    task: scan,
                    root: invalid,
                },
            ),
            (
                export,
                TaskKind::Export,
                WorkerCommand::ExportQuery {
                    task: export,
                    request: ExportRequest {
                        sql: "SELECT 1 AS id".into(),
                        destination: blocked.join("query.csv"),
                        format: ExportFormat::Csv,
                        compression: None,
                    },
                },
            ),
            (
                pipeline,
                TaskKind::Pipeline,
                WorkerCommand::RunPipeline {
                    task: pipeline,
                    request: PipelineRequest {
                        market_path: dir.path().join("missing-market.parquet"),
                        universe_path: dir.path().join("missing-universe.parquet"),
                        universe_id: "test".into(),
                        date_range: crate::state::parse_date_range("2024-01-01", "2024-01-30")
                            .unwrap(),
                        factor_ids: vec!["alpha_001".into()],
                        output_dir: dir.path().join("pipeline"),
                        training: TrainingConfig::default(),
                    },
                    cancel: cancellation(),
                },
            ),
            (
                training,
                TaskKind::Training,
                WorkerCommand::TrainFromCache {
                    task: training,
                    cache_path: dir.path().join("missing-cache.parquet"),
                    universe_id: None,
                    output_dir: dir.path().join("training"),
                    config: TrainingConfig::default(),
                    cancel: cancellation(),
                },
            ),
        ];
        for (task, kind, command) in cases {
            worker.send(command).unwrap();
            let events = collect(&rx, task);
            assert!(
                matches!(events.last(), Some(WorkerEvent::Failed { task: id, kind: failed_kind, message })
                if *id == task && *failed_kind == kind && !message.is_empty()),
                "{events:?}"
            );
            let mut state = crate::state::AppState::default();
            for event in &events {
                crate::state::apply_event(&mut state, event);
            }
            assert!(!state.busy());
            assert!(state.worker_error.is_none());
        }
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::DescribeQuery {
                task,
                sql: "SELECT 1 AS recovered".into(),
            })
            .unwrap();
        successful(&collect(&rx, task), task);
    }

    fn export(
        worker: &WorkerHandle,
        rx: &Receiver<WorkerEvent>,
        sql: &str,
        path: PathBuf,
        format: ExportFormat,
    ) {
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::ExportQuery {
                task,
                request: ExportRequest {
                    sql: sql.into(),
                    destination: path.clone(),
                    format,
                    compression: None,
                },
            })
            .unwrap();
        let events = collect(rx, task);
        successful(&events, task);
        assert!(events.iter().any(|event| matches!(event, WorkerEvent::Exported { destination, .. } if *destination == path)));
        assert!(path.is_file());
    }

    #[test]
    fn worker_describe_query_and_keyset_pages_use_real_duckdb() {
        let (worker, rx) = spawn(egui::Context::default());
        let sql = "SELECT range AS id FROM range(5)";
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::DescribeQuery {
                task,
                sql: sql.into(),
            })
            .unwrap();
        let events = collect(&rx, task);
        successful(&events, task);
        assert!(events.iter().any(
            |event| matches!(event, WorkerEvent::QueryDescribed { columns, .. }
            if columns.len() == 1 && columns[0].name == "id")
        ));
        for descending in [false, true] {
            let mut after = None;
            let mut values = Vec::new();
            loop {
                let task = Uuid::new_v4();
                worker
                    .send(WorkerCommand::RunQuery {
                        task,
                        request: PageRequest {
                            sql: sql.into(),
                            limit: 2,
                            sort: vec![SortKey {
                                column: "id".into(),
                                descending,
                            }],
                            after,
                        },
                    })
                    .unwrap();
                let events = collect(&rx, task);
                successful(&events, task);
                let page = events
                    .iter()
                    .find_map(|event| match event {
                        WorkerEvent::QueryPaged { page, .. } => Some(page),
                        _ => None,
                    })
                    .unwrap();
                values.extend(page.rows.iter().map(|row| match row[0] {
                    ScalarValue::Int64(id) => id,
                    ref value => panic!("unexpected {value:?}"),
                }));
                after = page.next_cursor.clone();
                if after.is_none() {
                    break;
                }
            }
            assert_eq!(
                values,
                if descending {
                    vec![4, 3, 2, 1, 0]
                } else {
                    vec![0, 1, 2, 3, 4]
                }
            );
        }
    }

    #[test]
    fn worker_exports_csv_and_parquet_then_scans_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let (worker, rx) = spawn(egui::Context::default());
        let csv = dir.path().join("result.csv");
        let parquet = dir.path().join("result.parquet");
        let sql = "SELECT range AS id FROM range(3)";
        export(&worker, &rx, sql, csv.clone(), ExportFormat::Csv);
        let text = std::fs::read_to_string(csv).unwrap();
        assert_eq!(text.lines().collect::<Vec<_>>(), vec!["id", "0", "1", "2"]);
        export(&worker, &rx, sql, parquet.clone(), ExportFormat::Parquet);
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::ScanDataset {
                task,
                root: dir.path().into(),
            })
            .unwrap();
        let events = collect(&rx, task);
        successful(&events, task);
        let files = events
            .iter()
            .find_map(|event| match event {
                WorkerEvent::DatasetScanned { files, .. } => Some(files),
                _ => None,
            })
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].rows, Some(3));
        assert_eq!(files[0].schema[0].name, "id");
    }

    #[test]
    fn query_results_preserve_null_numeric_boolean_date_and_text_values() {
        let (worker, rx) = spawn(egui::Context::default());
        let task = Uuid::new_v4();
        worker.send(WorkerCommand::RunQuery { task, request: PageRequest {
            sql: "SELECT NULL AS missing, true AS flag, 7::BIGINT AS count, 1.25::DOUBLE AS price, DATE '2024-01-02' AS day, 'hello' AS name".into(),
            limit: 100, sort: vec![], after: None,
        }}).unwrap();
        let events = collect(&rx, task);
        successful(&events, task);
        let page = events
            .iter()
            .find_map(|event| match event {
                WorkerEvent::QueryPaged { page, .. } => Some(page),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            page.rows[0],
            vec![
                ScalarValue::Null,
                ScalarValue::Boolean(true),
                ScalarValue::Int64(7),
                ScalarValue::Float64(1.25),
                ScalarValue::Date(chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap()),
                ScalarValue::Utf8("hello".into())
            ]
        );
    }

    #[test]
    fn export_preserves_sql_literals_and_quotes_destination_paths() {
        let dir = tempfile::tempdir().unwrap();
        let (worker, rx) = spawn(egui::Context::default());
        let path = dir.path().join("it's-a-query.csv");
        export(
            &worker,
            &rx,
            "SELECT 'it''s valid' AS text",
            path.clone(),
            ExportFormat::Csv,
        );
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("it's valid")
        );
    }

    #[test]
    fn worker_reports_sql_failure_and_recovers_for_next_command() {
        let (worker, rx) = spawn(egui::Context::default());
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::DescribeQuery {
                task,
                sql: "SELECT * FROM missing_table".into(),
            })
            .unwrap();
        let events = collect(&rx, task);
        assert!(
            matches!(events.last(), Some(WorkerEvent::Failed { kind: TaskKind::Describe, message, .. }) if !message.is_empty())
        );
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::DescribeQuery {
                task,
                sql: "SELECT 1 AS recovered".into(),
            })
            .unwrap();
        successful(&collect(&rx, task), task);
    }

    #[test]
    fn missing_parquet_glob_returns_query_error_and_worker_remains_usable() {
        let dir = tempfile::tempdir().unwrap();
        let (worker, rx) = spawn(egui::Context::default());
        let task = Uuid::new_v4();
        let glob = dir
            .path()
            .join("missing/**/*.parquet")
            .display()
            .to_string()
            .replace('\'', "''");
        worker
            .send(WorkerCommand::RunQuery {
                task,
                request: PageRequest {
                    sql: format!("SELECT * FROM read_parquet('{glob}')"),
                    limit: 100,
                    sort: vec![],
                    after: None,
                },
            })
            .unwrap();
        let events = collect(&rx, task);
        assert!(
            matches!(events.last(), Some(WorkerEvent::Failed { kind: TaskKind::Query, message, .. })
            if message.contains("No files found")),
            "{events:?}"
        );
        let mut state = crate::state::AppState::default();
        for event in &events {
            crate::state::apply_event(&mut state, event);
        }
        assert!(!state.busy());
        assert!(
            state
                .query
                .error
                .as_ref()
                .unwrap()
                .contains("No files found")
        );
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::RunQuery {
                task,
                request: PageRequest {
                    sql: "SELECT 1 AS recovered".into(),
                    limit: 100,
                    sort: vec![],
                    after: None,
                },
            })
            .unwrap();
        successful(&collect(&rx, task), task);
    }

    #[test]
    fn cancelled_pipeline_reports_cancellation_and_never_emits_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let (worker, rx) = spawn(egui::Context::default());
        let cancel = analytics_ml::pipeline::cancellation();
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::RunPipeline {
                task,
                cancel,
                request: PipelineRequest {
                    market_path: dir.path().join("missing"),
                    universe_path: dir.path().join("missing"),
                    universe_id: "test".into(),
                    date_range: crate::state::parse_date_range("2024-01-01", "2024-01-30").unwrap(),
                    factor_ids: vec!["alpha_001".into()],
                    output_dir: dir.path().join("out"),
                    training: TrainingConfig::default(),
                },
            })
            .unwrap();
        let events = collect(&rx, task);
        assert!(
            matches!(events.last(), Some(WorkerEvent::Failed { kind: TaskKind::Pipeline, message, .. }) if message == "任务已取消")
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, WorkerEvent::PipelineFinished { .. }))
        );
    }

    #[test]
    fn worker_pipeline_and_cache_retraining_publish_real_artifacts_and_previews() {
        let dir = tempfile::tempdir().unwrap();
        let (worker, rx) = spawn(egui::Context::default());
        let market = dir.path().join("market.parquet");
        let universe = dir.path().join("universe.parquet");
        export(
            &worker,
            &rx,
            "SELECT 'stock' || i::VARCHAR AS instrument_id, DATE '2024-01-01' + d::INTEGER AS trade_date, \
            p AS open, p+1 AS high, p-1 AS low, p+0.2 AS close, \
            100.0::DOUBLE AS volume, 1000.0::DOUBLE AS amount, p AS adj_open, p+1 AS adj_high, \
            p-1 AS adj_low, p+0.2 AS adj_close, true AS is_trading, DATE '2023-01-01' AS listing_date \
            FROM (SELECT i, d, (10+i+d*0.05)::DOUBLE AS p FROM range(3) a(i), range(30) b(d))",
            market.clone(),
            ExportFormat::Parquet,
        );
        export(
            &worker,
            &rx,
            "SELECT 'stock' || i::VARCHAR AS instrument_id, 'test' AS universe_id, \
            DATE '2024-01-01' AS trade_date, true AS is_member FROM range(3) a(i)",
            universe.clone(),
            ExportFormat::Parquet,
        );
        let config = TrainingConfig {
            parameters: std::collections::BTreeMap::from([
                ("num_iterations".into(), "3".into()),
                ("num_leaves".into(), "7".into()),
                ("min_data_in_leaf".into(), "2".into()),
                ("num_threads".into(), "1".into()),
            ]),
            early_stopping_rounds: None,
        };
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::RunPipeline {
                task,
                cancel: analytics_ml::pipeline::cancellation(),
                request: PipelineRequest {
                    market_path: market,
                    universe_path: universe,
                    universe_id: "test".into(),
                    date_range: crate::state::parse_date_range("2024-01-01", "2024-01-30").unwrap(),
                    factor_ids: vec!["alpha_001".into()],
                    output_dir: dir.path().join("pipeline"),
                    training: config.clone(),
                },
            })
            .unwrap();
        let events = collect(&rx, task);
        successful(&events, task);
        let summary = events
            .iter()
            .find_map(|event| match event {
                WorkerEvent::PipelineFinished { summary, .. } => Some(summary),
                _ => None,
            })
            .unwrap();
        assert!(summary.artifact.training.model_path.is_file());
        assert!(
            summary
                .metrics_json
                .as_ref()
                .unwrap()
                .contains("iterations")
        );
        assert_eq!(summary.importance.len(), 1);
        assert!(!summary.preview.as_ref().unwrap().rows.is_empty());
        let mut state = crate::state::AppState::default();
        for event in &events {
            crate::state::apply_event(&mut state, event);
        }
        assert!(!state.busy());
        assert!(state.factor.summary.is_some());
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::TrainFromCache {
                task,
                cache_path: summary.artifact.factor_cache_path.clone().unwrap(),
                universe_id: None,
                output_dir: dir.path().join("retrain"),
                config,
                cancel: analytics_ml::pipeline::cancellation(),
            })
            .unwrap();
        let events = collect(&rx, task);
        successful(&events, task);
        for event in &events {
            crate::state::apply_event(&mut state, event);
        }
        assert!(!state.busy());
        let summary = state.model.summary.unwrap();
        assert!(summary.artifact.predictions_path.is_file());
        assert!(!summary.preview.as_ref().unwrap().rows.is_empty());
    }
}
