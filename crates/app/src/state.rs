//! UI state and the pure event-reduction logic of the desktop shell.
//!
//! This module is deliberately free of egui and worker-thread concerns so the
//! reducer and the form parsers can be unit-tested without a windowing
//! environment. [`apply_event`] is the single place where [`WorkerEvent`]
//! payloads are folded into panel state.

use analytics_core::{
    ColumnSchema, DataType, DatasetFile, DateRange, KeysetCursor, PageRequest, QueryPage,
    ScalarValue, SortKey,
};
use analytics_ml::TrainingConfig;
use chrono::NaiveDate;
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::worker::{RunSummary, TaskKind, WorkerEvent};

/// Navigation targets of the left sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    /// Local Parquet catalog scanning.
    Dataset,
    /// Ad-hoc DuckDB SQL workbench.
    Query,
    /// Alpha191 factor pipeline runner.
    Factor,
    /// LightGBM retraining and result inspection.
    Model,
    /// Saved annual research recipes.
    Research,
    /// Persistent local task history.
    Runs,
}

/// State of the dataset management panel.
#[derive(Debug, Default)]
pub struct DatasetPanel {
    /// Directory or file path to scan.
    pub root: String,
    /// Files discovered by the last scan.
    pub files: Vec<DatasetFile>,
    /// Row index selected for schema inspection.
    pub selected: Option<usize>,
    /// Root of the last successful scan, used for display.
    pub scanned_root: Option<String>,
    /// Last scan error, if any.
    pub error: Option<String>,
}

/// State of the SQL workbench panel.
#[derive(Debug)]
pub struct QueryPanel {
    /// SQL editor content.
    pub sql: String,
    /// Page size selector value.
    pub limit: u32,
    /// Keyset sort column; required before paging beyond page one.
    pub sort_column: Option<String>,
    /// Sort direction for the keyset column.
    pub descending: bool,
    /// Column schemas of the current page.
    pub columns: Vec<ColumnSchema>,
    /// Stringified cell values of the current page.
    pub rows: Vec<Vec<ScalarValue>>,
    /// Cursor for the next page, when more rows exist.
    pub next_cursor: Option<KeysetCursor>,
    /// Output of the last `DESCRIBE` run.
    pub described: Option<Vec<ColumnSchema>>,
    /// Inline error of the last execution.
    pub error: Option<String>,
    /// Export destination path.
    pub export_path: String,
    /// Export in CSV instead of Parquet when true.
    pub export_csv: bool,
    /// One-based page counter of the current result set.
    pub page: u32,
    /// Last submitted SQL/options, used to reject cursors after editor changes.
    pub submitted: Option<PageRequest>,
}

impl Default for QueryPanel {
    fn default() -> Self {
        Self {
            sql: "SELECT * FROM read_parquet('data/market/**/*.parquet') LIMIT 100".into(),
            limit: 500,
            sort_column: None,
            descending: false,
            columns: Vec::new(),
            rows: Vec::new(),
            next_cursor: None,
            described: None,
            error: None,
            export_path: ".jfquant/exports/query.parquet".into(),
            export_csv: false,
            page: 0,
            submitted: None,
        }
    }
}

impl QueryPanel {
    /// Builds the request for the first page of the current editor SQL.
    pub fn first_page_request(&self) -> Result<PageRequest, String> {
        let sql = normalize_sql(&self.sql)?;
        Ok(PageRequest {
            sql,
            limit: self.limit,
            sort: self.sort_keys(),
            after: None,
        })
    }

    /// Builds the request for the page after the current cursor, or `None`
    /// when no further page can be fetched.
    pub fn next_page_request(&self) -> Option<PageRequest> {
        let after = self.next_cursor.clone()?;
        let submitted = self.submitted.as_ref()?;
        let current = self.first_page_request().ok()?;
        if current.sql != submitted.sql
            || current.limit != submitted.limit
            || current.sort.len() != submitted.sort.len()
            || !current
                .sort
                .iter()
                .zip(&submitted.sort)
                .all(|(a, b)| a.column == b.column && a.descending == b.descending)
        {
            return None;
        }
        Some(PageRequest {
            sql: submitted.sql.clone(),
            limit: submitted.limit,
            sort: submitted.sort.clone(),
            after: Some(after),
        })
    }

    /// Records a successfully queued query and resets stale first-page results.
    pub fn submitted(&mut self, request: PageRequest) {
        if request.after.is_none() {
            self.page = 0;
            self.rows.clear();
            self.next_cursor = None;
            self.described = None;
        }
        self.submitted = Some(request);
    }

    fn sort_keys(&self) -> Vec<SortKey> {
        self.sort_column
            .as_ref()
            .map(|column| SortKey {
                column: column.clone(),
                descending: self.descending,
            })
            .into_iter()
            .collect()
    }
}

/// State of the Alpha191 factor panel.
#[derive(Debug)]
pub struct FactorPanel {
    /// Market Parquet root.
    pub market_path: String,
    /// Historical-universe Parquet root.
    pub universe_path: String,
    /// Universe identifier joined during loading.
    pub universe_id: String,
    /// Inclusive start date (YYYY-MM-DD).
    pub date_start: String,
    /// Inclusive end date (YYYY-MM-DD).
    pub date_end: String,
    /// Output directory for pipeline artifacts.
    pub output_dir: String,
    /// Case-insensitive filter over factor IDs and formulas.
    pub filter: String,
    /// Selected factor IDs.
    pub selected: std::collections::BTreeSet<String>,
    /// Inline error of the last run attempt.
    pub error: Option<String>,
    /// Summary of the last finished pipeline run.
    pub summary: Option<RunSummary>,
}

impl Default for FactorPanel {
    fn default() -> Self {
        Self {
            market_path: "data/market".into(),
            universe_path: "data/universe".into(),
            universe_id: "csi500".into(),
            date_start: "2020-01-01".into(),
            date_end: "2025-12-31".into(),
            output_dir: ".jfquant/runs/latest".into(),
            filter: String::new(),
            selected: ["alpha_001".to_string(), "alpha_002".to_string()]
                .into_iter()
                .collect(),
            error: None,
            summary: None,
        }
    }
}

impl FactorPanel {
    /// Validates the date inputs into a [`DateRange`].
    pub fn date_range(&self) -> Result<DateRange, String> {
        parse_date_range(&self.date_start, &self.date_end)
    }

    /// Selected factor IDs in stable ascending order.
    pub fn selected_ids(&self) -> Vec<String> {
        self.selected.iter().cloned().collect()
    }

    /// Whether a registry entry matches the current filter text.
    pub fn matches_filter(&self, id: &str, formula: &str) -> bool {
        let needle = self.filter.trim().to_lowercase();
        needle.is_empty()
            || id.to_lowercase().contains(&needle)
            || formula.to_lowercase().contains(&needle)
    }
}

/// State of the LightGBM modeling panel.
#[derive(Debug)]
pub struct ModelPanel {
    /// Factor cache Parquet used as training input.
    pub cache_path: String,
    /// Output directory for retrained artifacts.
    pub output_dir: String,
    /// Universe identifier override (blank keeps the cache value).
    pub universe_id: String,
    /// `num_iterations` field.
    pub num_iterations: String,
    /// `learning_rate` field.
    pub learning_rate: String,
    /// `num_leaves` field.
    pub num_leaves: String,
    /// `min_data_in_leaf` field.
    pub min_data_in_leaf: String,
    /// Early-stopping patience field; blank disables early stopping.
    pub early_stopping: String,
    /// Inline error of the last training attempt.
    pub error: Option<String>,
    /// Summary of the last finished training run.
    pub summary: Option<RunSummary>,
}

impl Default for ModelPanel {
    fn default() -> Self {
        Self {
            cache_path: ".jfquant/runs/latest/factors.parquet".into(),
            output_dir: ".jfquant/runs/retrain".into(),
            universe_id: String::new(),
            num_iterations: "100".into(),
            learning_rate: "0.08".into(),
            num_leaves: "15".into(),
            min_data_in_leaf: "10".into(),
            early_stopping: "8".into(),
            error: None,
            summary: None,
        }
    }
}

impl ModelPanel {
    /// Validates the hyperparameter fields into a [`TrainingConfig`].
    pub fn training_config(&self) -> Result<TrainingConfig, String> {
        let mut parameters = BTreeMap::new();
        for (key, raw) in [
            ("num_iterations", &self.num_iterations),
            ("learning_rate", &self.learning_rate),
            ("num_leaves", &self.num_leaves),
            ("min_data_in_leaf", &self.min_data_in_leaf),
        ] {
            let value = raw.trim();
            if value.is_empty() {
                return Err(format!("参数 {key} 不能为空"));
            }
            let number = value
                .parse::<f64>()
                .map_err(|_| format!("参数 {key}={value} 不是数值"))?;
            if !number.is_finite() || number <= 0.0 {
                return Err(format!("参数 {key} 必须是有限正数"));
            }
            if key != "learning_rate" && value.parse::<u32>().is_err() {
                return Err(format!("参数 {key} 必须是正整数"));
            }
            if key == "num_leaves" && number < 2.0 {
                return Err("参数 num_leaves 必须至少为 2".into());
            }
            parameters.insert(key.to_string(), value.to_string());
        }
        parameters.insert("seed".into(), "17".into());
        let early_stopping_rounds = match self.early_stopping.trim() {
            "" => None,
            value => Some(
                value
                    .parse::<u32>()
                    .map_err(|_| format!("早停轮数 {value} 不是正整数"))?,
            ),
        };
        Ok(TrainingConfig {
            parameters,
            early_stopping_rounds,
        })
    }
}

/// One active background task shown in the top bar and log.
#[derive(Debug)]
pub struct TaskInfo {
    /// Task identifier shared with the worker protocol.
    pub id: Uuid,
    /// Which subsystem the task belongs to.
    pub kind: TaskKind,
    /// Human-readable label.
    pub label: String,
    /// Rolling progress log (capped).
    pub log: Vec<String>,
}

/// Root UI state reduced from worker events.
#[derive(Debug)]
pub struct AppState {
    /// Local platform state and workspace ownership.
    pub platform: crate::platform::PlatformState,
    /// Active sidebar panel.
    pub panel: Panel,
    /// Dataset panel state.
    pub dataset: DatasetPanel,
    /// Query panel state.
    pub query: QueryPanel,
    /// Factor panel state.
    pub factor: FactorPanel,
    /// Model panel state.
    pub model: ModelPanel,
    /// Currently running task, if any.
    pub active_task: Option<TaskInfo>,
    /// Global message log shown in the bottom drawer (capped).
    pub messages: Vec<String>,
    /// Whether the log drawer is open.
    pub log_open: bool,
    /// Why the background worker is unavailable; disables task submission.
    pub worker_error: Option<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            platform: crate::platform::PlatformState::default(),
            panel: Panel::Dataset,
            dataset: DatasetPanel {
                root: "data/market".into(),
                ..DatasetPanel::default()
            },
            query: QueryPanel::default(),
            factor: FactorPanel::default(),
            model: ModelPanel::default(),
            active_task: None,
            messages: Vec::new(),
            log_open: false,
            worker_error: None,
        }
    }
}

impl AppState {
    /// Whether a background task is currently running.
    pub fn busy(&self) -> bool {
        self.active_task.is_some()
    }

    /// Clears worker failure state after the UI creates a replacement worker.
    pub fn worker_restarted(&mut self) {
        self.worker_error = None;
        self.active_task = None;
        self.query.next_cursor = None;
        self.query.submitted = None;
        self.push_message("后台已重启，可重新执行操作。".into());
    }

    fn push_message(&mut self, message: String) {
        const CAP: usize = 200;
        self.messages.push(message);
        if self.messages.len() > CAP {
            let overflow = self.messages.len() - CAP;
            self.messages.drain(..overflow);
        }
    }
}

/// Folds one worker event into the UI state. Pure and synchronous so it can
/// be driven directly from tests.
pub fn apply_event(state: &mut AppState, event: &WorkerEvent) {
    // A failed worker cannot revive itself through late queued events.
    // Restart replaces the receiver and explicitly clears this state.
    if state.worker_error.is_some() {
        return;
    }
    match event {
        WorkerEvent::Unavailable { message } => {
            if let Some(task) = state.active_task.as_ref() {
                let failure = WorkerEvent::Failed {
                    task: task.id,
                    kind: task.kind,
                    message: message.clone(),
                };
                apply_event(state, &failure);
            }
            state.worker_error = Some(message.clone());
            state.push_message(message.clone());
        }
        WorkerEvent::Started { task, kind, label } => {
            if let Some(active) = &mut state.active_task
                && active.id == *task
            {
                active.label = label.clone();
                return;
            }
            if matches!(
                kind,
                TaskKind::Query | TaskKind::Describe | TaskKind::Export
            ) {
                state.query.error = None;
            }
            state.active_task = Some(TaskInfo {
                id: *task,
                kind: *kind,
                label: label.clone(),
                log: Vec::new(),
            });
            state.push_message(format!("任务开始：{label}"));
        }
        WorkerEvent::Progress { task, message } => {
            if let Some(active) = &mut state.active_task
                && active.id == *task
            {
                const LOG_CAP: usize = 64;
                active.log.push(message.clone());
                if active.log.len() > LOG_CAP {
                    let overflow = active.log.len() - LOG_CAP;
                    active.log.drain(..overflow);
                }
            }
        }
        WorkerEvent::DatasetScanned { task, root, files } => {
            if !is_active(state, task) {
                return;
            }
            state.dataset.files = files.clone();
            state.dataset.selected = None;
            state.dataset.scanned_root = Some(root.display().to_string());
            state.dataset.error = None;
            state.push_message(format!("数据集扫描完成：{} 个文件", files.len()));
        }
        WorkerEvent::QueryDescribed { task, columns } => {
            if !is_active(state, task) {
                return;
            }
            state.query.described = Some(columns.clone());
            state.query.error = None;
        }
        WorkerEvent::QueryPaged { task, page } => {
            if !is_active(state, task) {
                return;
            }
            reduce_query_page(state, page);
        }
        WorkerEvent::Exported { task, destination } => {
            if !is_active(state, task) {
                return;
            }
            state.push_message(format!("导出完成：{}", destination.display()));
        }
        WorkerEvent::PipelineFinished { task, summary } => {
            if !is_active(state, task) {
                return;
            }
            state.factor.summary = Some(summary.clone());
            state.factor.error = None;
            state.push_message("因子流水线完成".into());
        }
        WorkerEvent::TrainingFinished { task, summary } => {
            if !is_active(state, task) {
                return;
            }
            state.model.summary = Some(summary.clone());
            state.model.error = None;
            state.push_message("模型训练完成".into());
        }
        WorkerEvent::WorkflowFinished {
            task,
            outcome,
            record,
            datasets,
            runs,
        } => {
            if !is_active(state, task) {
                return;
            }
            state.platform.datasets = datasets.clone();
            state.platform.runs = runs.clone();
            state.platform.details = outcome.details.clone();
            state.platform.selected_run = Some(record.id);
            if let Some(data) = &outcome.data {
                if data.kind == "market" {
                    state.platform.market = data.id.clone();
                } else {
                    state.platform.actions = data.id.clone();
                }
            }
            if let Some(id) = outcome.details["id"]
                .as_str()
                .filter(|s| s.starts_with("pool-"))
            {
                state.platform.template.pool_version = id.into();
                state.platform.pool_editor =
                    serde_json::to_string_pretty(&outcome.details["definition"])
                        .unwrap_or_default();
            }
            if let Some(id) = outcome.details["id"]
                .as_str()
                .filter(|id| id.starts_with("identity-"))
            {
                state.platform.template.identity_version = id.into();
            }
            if let Some(id) = outcome.details["demo_actions"].as_str() {
                state.platform.actions = id.into();
            }
            if let Ok(template) = serde_json::from_value(outcome.details["demo_template"].clone()) {
                state.platform.template = template;
            }
            if let Some(tag) = outcome.details["tag"].as_str() {
                state.platform.release_tag = tag.into();
            }
            state.platform.error = record.error.clone();
            state.push_message(format!(
                "{}：{}",
                record.operation,
                crate::platform::status_label(record.status)
            ));
        }
        WorkerEvent::Failed {
            task,
            kind,
            message,
        } => {
            if !is_active(state, task) {
                return;
            }
            match kind {
                TaskKind::Scan => state.dataset.error = Some(message.clone()),
                TaskKind::Describe | TaskKind::Query | TaskKind::Export => {
                    state.query.error = Some(message.clone());
                }
                TaskKind::Pipeline => state.factor.error = Some(message.clone()),
                TaskKind::Training => state.model.error = Some(message.clone()),
                TaskKind::Workflow => state.platform.error = Some(message.clone()),
            }
            state.push_message(format!("任务失败：{message}"));
            clear_task(state, task);
        }
        WorkerEvent::Finished { task } => {
            clear_task(state, task);
        }
    }
}

fn is_active(state: &AppState, task: &Uuid) -> bool {
    state
        .active_task
        .as_ref()
        .is_some_and(|active| active.id == *task)
}

fn clear_task(state: &mut AppState, task: &Uuid) {
    if is_active(state, task) {
        state.active_task = None;
    }
}

fn reduce_query_page(state: &mut AppState, page: &QueryPage) {
    let first_page = page.columns != state.query.columns || state.query.page == 0;
    state.query.columns = page.columns.clone();
    if first_page {
        state.query.rows = page.rows.clone();
        state.query.page = 1;
    } else {
        state.query.rows.extend(page.rows.iter().cloned());
        state.query.page += 1;
    }
    state.query.next_cursor = page.next_cursor.clone();
    state.query.error = None;
}

/// Trims SQL and removes trailing semicolons so it can be embedded in the
/// engine's `SELECT * FROM (<sql>)` wrapper.
pub fn normalize_sql(sql: &str) -> Result<String, String> {
    let trimmed = sql
        .trim()
        .trim_end_matches(|ch: char| ch == ';' || ch.is_whitespace());
    if trimmed.is_empty() {
        return Err("SQL 不能为空".into());
    }
    Ok(trimmed.to_string())
}

/// Parses two YYYY-MM-DD inputs into a validated [`DateRange`].
pub fn parse_date_range(start: &str, end: &str) -> Result<DateRange, String> {
    let start = NaiveDate::parse_from_str(start.trim(), "%Y-%m-%d")
        .map_err(|_| format!("起始日期 {:?} 不是 YYYY-MM-DD", start.trim()))?;
    let end = NaiveDate::parse_from_str(end.trim(), "%Y-%m-%d")
        .map_err(|_| format!("结束日期 {:?} 不是 YYYY-MM-DD", end.trim()))?;
    let range = DateRange { start, end };
    range
        .validate()
        .map_err(|error| format!("日期区间无效：{error}"))?;
    Ok(range)
}

/// Formats a cell for the results grid.
pub fn scalar_text(value: &ScalarValue) -> String {
    match value {
        ScalarValue::Null => "NULL".into(),
        ScalarValue::Boolean(value) => value.to_string(),
        ScalarValue::Int64(value) => value.to_string(),
        ScalarValue::Float64(value) => format!("{value:.6}"),
        ScalarValue::Utf8(value) => value.clone(),
        ScalarValue::Date(value) => value.to_string(),
    }
}

/// Short display name for a [`DataType`].
pub fn data_type_name(data_type: &DataType) -> &'static str {
    match data_type {
        DataType::Null => "null",
        DataType::Boolean => "bool",
        DataType::Int64 => "int64",
        DataType::Float64 => "float64",
        DataType::Utf8 => "utf8",
        DataType::Date => "date",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(columns: &[&str], rows: usize, cursor: bool) -> QueryPage {
        QueryPage {
            columns: columns
                .iter()
                .map(|name| ColumnSchema {
                    name: name.to_string(),
                    data_type: DataType::Utf8,
                    nullable: true,
                })
                .collect(),
            rows: (0..rows)
                .map(|index| {
                    columns
                        .iter()
                        .map(|_| ScalarValue::Utf8(format!("r{index}")))
                        .collect()
                })
                .collect(),
            next_cursor: cursor.then(|| KeysetCursor(vec![ScalarValue::Utf8("r".into())])),
            total_rows: None,
        }
    }

    #[test]
    fn normalize_sql_strips_semicolons_and_whitespace() {
        assert_eq!(normalize_sql("  select 1;;  ").unwrap(), "select 1");
        assert_eq!(normalize_sql("select ';';  ; \n ;").unwrap(), "select ';'");
        assert!(normalize_sql("   ").is_err());
        assert!(normalize_sql(";;").is_err());
    }

    #[test]
    fn parse_date_range_validates_order() {
        assert!(parse_date_range("2024-01-01", "2024-12-31").is_ok());
        assert!(parse_date_range("2024-12-31", "2024-01-01").is_err());
        assert!(parse_date_range("2024/01/01", "2024-12-31").is_err());
    }

    #[test]
    fn next_page_request_requires_cursor_and_unchanged_submitted_query() {
        let mut panel = QueryPanel::default();
        assert!(panel.next_page_request().is_none());
        panel.next_cursor = Some(KeysetCursor(vec![ScalarValue::Utf8("a".into())]));
        assert!(panel.next_page_request().is_none());
        panel.sort_column = Some("trade_date".into());
        panel.submitted = Some(panel.first_page_request().unwrap());
        let request = panel.next_page_request().expect("pageable");
        assert_eq!(request.sort.len(), 1);
        assert!(request.after.is_some());
        panel.sql = "SELECT 2".into();
        assert!(panel.next_page_request().is_none());
    }

    #[test]
    fn reducer_accumulates_pages_without_changing_requested_sort() {
        let mut state = AppState::default();
        let task = Uuid::new_v4();
        apply_event(
            &mut state,
            &WorkerEvent::Started {
                task,
                kind: TaskKind::Query,
                label: "查询".into(),
            },
        );
        apply_event(
            &mut state,
            &WorkerEvent::QueryPaged {
                task,
                page: page(&["trade_date", "close"], 3, true),
            },
        );
        assert_eq!(state.query.rows.len(), 3);
        assert_eq!(state.query.page, 1);
        assert!(state.query.sort_column.is_none());

        apply_event(
            &mut state,
            &WorkerEvent::QueryPaged {
                task,
                page: page(&["trade_date", "close"], 2, false),
            },
        );
        assert_eq!(state.query.rows.len(), 5);
        assert_eq!(state.query.page, 2);
        assert!(state.query.next_cursor.is_none());
    }

    #[test]
    fn reducer_replaces_rows_when_schema_changes() {
        let mut state = AppState::default();
        let task = Uuid::new_v4();
        apply_event(
            &mut state,
            &WorkerEvent::Started {
                task,
                kind: TaskKind::Query,
                label: "查询".into(),
            },
        );
        apply_event(
            &mut state,
            &WorkerEvent::QueryPaged {
                task,
                page: page(&["a"], 2, true),
            },
        );
        apply_event(
            &mut state,
            &WorkerEvent::QueryPaged {
                task,
                page: page(&["b"], 4, false),
            },
        );
        assert_eq!(state.query.rows.len(), 4);
        assert_eq!(state.query.page, 1);
    }

    #[test]
    fn failed_events_route_errors_and_clear_task() {
        let mut state = AppState::default();
        let task = Uuid::new_v4();
        apply_event(
            &mut state,
            &WorkerEvent::Started {
                task,
                kind: TaskKind::Query,
                label: "查询".into(),
            },
        );
        assert!(state.busy());
        apply_event(
            &mut state,
            &WorkerEvent::Failed {
                task,
                kind: TaskKind::Query,
                message: "boom".into(),
            },
        );
        assert!(!state.busy());
        assert_eq!(state.query.error.as_deref(), Some("boom"));
    }

    #[test]
    fn stale_payloads_are_ignored() {
        let mut state = AppState::default();
        apply_event(
            &mut state,
            &WorkerEvent::QueryPaged {
                task: Uuid::new_v4(),
                page: page(&["a"], 2, true),
            },
        );
        assert!(state.query.rows.is_empty());
    }

    #[test]
    fn factor_filter_matches_id_and_formula() {
        let panel = FactorPanel {
            filter: "001".into(),
            ..FactorPanel::default()
        };
        assert!(panel.matches_filter("alpha_001", "rank(close)"));
        assert!(!panel.matches_filter("alpha_002", "rank(open)"));
        let panel = FactorPanel {
            filter: "rank".into(),
            ..FactorPanel::default()
        };
        assert!(panel.matches_filter("alpha_002", "rank(open)"));
    }

    #[test]
    fn training_config_parses_and_validates() {
        let panel = ModelPanel::default();
        let config = panel.training_config().unwrap();
        assert_eq!(config.parameters["num_iterations"], "100");
        assert_eq!(config.early_stopping_rounds, Some(8));
        let panel = ModelPanel {
            num_iterations: "abc".into(),
            ..ModelPanel::default()
        };
        assert!(panel.training_config().is_err());
        let panel = ModelPanel {
            early_stopping: String::new(),
            ..ModelPanel::default()
        };
        assert_eq!(panel.training_config().unwrap().early_stopping_rounds, None);
    }

    #[test]
    fn default_state_exposes_all_workbenches_and_safe_defaults() {
        let state = AppState::default();
        assert_eq!(state.panel, Panel::Dataset);
        assert_eq!(state.dataset.root, "data/market");
        assert_eq!(state.query.limit, 500);
        assert_eq!(state.factor.selected_ids(), vec!["alpha_001", "alpha_002"]);
        assert!(!state.busy());
        assert!(!state.log_open);
    }

    #[test]
    fn first_page_request_normalizes_sql_and_carries_sort_direction() {
        let panel = QueryPanel {
            sql: "  SELECT 1;  ".into(),
            limit: 100,
            sort_column: Some("trade_date".into()),
            descending: true,
            ..QueryPanel::default()
        };
        let request = panel.first_page_request().unwrap();
        assert_eq!(request.sql, "SELECT 1");
        assert_eq!(request.limit, 100);
        assert_eq!(request.sort[0].column, "trade_date");
        assert!(request.sort[0].descending);
        assert!(request.after.is_none());
    }

    #[test]
    fn query_request_rejects_blank_sql() {
        let panel = QueryPanel {
            sql: " ; \n".into(),
            ..QueryPanel::default()
        };
        assert_eq!(panel.first_page_request().unwrap_err(), "SQL 不能为空");
        assert!(panel.next_page_request().is_none());
    }

    #[test]
    fn scalar_and_data_type_display_values_are_stable() {
        assert_eq!(scalar_text(&ScalarValue::Null), "NULL");
        assert_eq!(scalar_text(&ScalarValue::Boolean(true)), "true");
        assert_eq!(scalar_text(&ScalarValue::Int64(-3)), "-3");
        assert_eq!(scalar_text(&ScalarValue::Float64(1.25)), "1.250000");
        assert_eq!(scalar_text(&ScalarValue::Utf8("abc".into())), "abc");
        assert_eq!(data_type_name(&DataType::Date), "date");
        assert_eq!(data_type_name(&DataType::Float64), "float64");
    }

    #[test]
    fn dataset_event_updates_files_selection_and_scan_root() {
        let mut state = AppState::default();
        state.dataset.root = "fixtures/market".into();
        state.dataset.selected = Some(3);
        let task = Uuid::new_v4();
        apply_event(
            &mut state,
            &WorkerEvent::Started {
                task,
                kind: TaskKind::Scan,
                label: "扫描".into(),
            },
        );
        state.dataset.root = "edited/while-scanning".into();
        apply_event(
            &mut state,
            &WorkerEvent::DatasetScanned {
                task,
                root: "fixtures/market".into(),
                files: Vec::new(),
            },
        );
        assert!(state.dataset.files.is_empty());
        assert_eq!(state.dataset.selected, None);
        assert_eq!(
            state.dataset.scanned_root.as_deref(),
            Some("fixtures/market")
        );
        assert_eq!(state.dataset.root, "edited/while-scanning");
        assert!(
            state
                .messages
                .iter()
                .any(|message| message.contains("0 个文件"))
        );
    }

    #[test]
    fn progress_log_is_capped_and_stale_progress_is_ignored() {
        let mut state = AppState::default();
        let task = Uuid::new_v4();
        apply_event(
            &mut state,
            &WorkerEvent::Started {
                task,
                kind: TaskKind::Query,
                label: "查询".into(),
            },
        );
        for index in 0..70 {
            apply_event(
                &mut state,
                &WorkerEvent::Progress {
                    task,
                    message: index.to_string(),
                },
            );
        }
        assert_eq!(state.active_task.as_ref().unwrap().log.len(), 64);
        assert_eq!(
            state.active_task.as_ref().unwrap().log.first().unwrap(),
            "6"
        );
        apply_event(
            &mut state,
            &WorkerEvent::Progress {
                task: Uuid::new_v4(),
                message: "stale".into(),
            },
        );
        assert!(
            !state
                .active_task
                .as_ref()
                .unwrap()
                .log
                .iter()
                .any(|line| line == "stale")
        );
    }

    #[test]
    fn every_failure_kind_routes_to_its_panel_and_clears_task() {
        let cases = [
            (TaskKind::Scan, 0),
            (TaskKind::Describe, 1),
            (TaskKind::Query, 1),
            (TaskKind::Export, 1),
            (TaskKind::Pipeline, 2),
            (TaskKind::Training, 3),
        ];
        for (kind, panel) in cases {
            let mut state = AppState::default();
            let task = Uuid::new_v4();
            apply_event(
                &mut state,
                &WorkerEvent::Started {
                    task,
                    kind,
                    label: "任务".into(),
                },
            );
            apply_event(
                &mut state,
                &WorkerEvent::Failed {
                    task,
                    kind,
                    message: "boom".into(),
                },
            );
            assert!(!state.busy());
            let error = match panel {
                0 => state.dataset.error.as_deref(),
                1 => state.query.error.as_deref(),
                2 => state.factor.error.as_deref(),
                _ => state.model.error.as_deref(),
            };
            assert_eq!(error, Some("boom"));
        }
    }

    #[test]
    fn finished_event_only_clears_matching_task() {
        let mut state = AppState::default();
        let task = Uuid::new_v4();
        apply_event(
            &mut state,
            &WorkerEvent::Started {
                task,
                kind: TaskKind::Query,
                label: "查询".into(),
            },
        );
        apply_event(
            &mut state,
            &WorkerEvent::Finished {
                task: Uuid::new_v4(),
            },
        );
        assert!(state.busy());
        apply_event(&mut state, &WorkerEvent::Finished { task });
        assert!(!state.busy());
    }

    #[test]
    fn message_log_keeps_only_the_latest_two_hundred_entries() {
        let mut state = AppState::default();
        for index in 0..205 {
            state.push_message(index.to_string());
        }
        assert_eq!(state.messages.len(), 200);
        assert_eq!(state.messages.first().unwrap(), "5");
        assert_eq!(state.messages.last().unwrap(), "204");
    }
}
