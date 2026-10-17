//! Workspace onboarding, immutable data versions, saved recipes and local results.
use crate::{
    state::AppState,
    worker::{WorkerCommand, WorkerHandle},
};
use analytics_workflow::research::pipeline::{Cancellation, cancellation};
use analytics_workflow::{
    DataVersion, ResearchTemplate, RunRecord, RunStatus, WorkflowRequest, Workspace, WorkspaceLease,
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;
mod evidence;
mod history;
mod indices;
mod reads;
pub struct PlatformState {
    pub(crate) reads: reads::ReadState,
    pub workspace: Option<Workspace>,
    pub lease: Option<WorkspaceLease>,
    pub root_input: String,
    pub require_workspace: bool,
    pub error: Option<String>,
    pub template: ResearchTemplate,
    pub schedule_editor: String,
    pub date_editor_valid: bool,
    pub action_review_editor: String,
    pub evidence_tools: evidence::EvidenceTools,
    /// Previously completed training task ID.
    pub training_job: String,
    pub independent_training_editor: String,
    pub independent_backtest_editor: String,
    pub independent_models: Value,
    pub prediction_job: String,
    pub rights_policy: analytics_core::rights::Policy,
    pub action_review_instrument: String,
    pub action_evidence_manifest: String,
    pub action_evidence_tasks: String,
    pub datasets: Vec<DataVersion>,
    pub runs: Vec<RunRecord>,
    pub selected_run: Option<Uuid>,
    pub market: String,
    pub actions: String,
    pub coefficients: String,
    pub release_tag: String,
    pub register_path: String,
    pub hithink_key: String,
    pub tdx_key: String,
    pub source_policy_editor: String,
    pub credential_save: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    pub collection_provider: analytics_storage::collector::Provider,
    pub adjustment_source: analytics_storage::adjustment::AdjustmentSource,
    pub collection_symbols: String,
    pub collection_benchmarks: String,
    pub collection_universes: std::collections::BTreeSet<String>,
    pub collection_index_inputs:
        std::collections::BTreeMap<String, analytics_core::indices::IndexConfig>,
    pub index_versions: Vec<analytics_workflow::indices::IndexVersion>,
    pub index_editor: String,
    pub history_tools: history::Tools,
    pub index_path: String,
    pub index_version: String,
    pub index_current_members: bool,
    pub index_query: String,
    pub index_selection: String,
    pub pool_editor: String,
    pub pool_preview_date: String,
    pub start: String,
    pub end: String,
    pub details: Value,
    pub filter: String,
    pub import_run_path: String,
}
impl std::fmt::Debug for PlatformState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformState")
            .field("workspace", &self.workspace)
            .field("template", &self.template)
            .field("error", &self.error)
            .field("hithink_key", &"[redacted]")
            .field("tdx_key", &"[redacted]")
            .finish_non_exhaustive()
    }
}
impl Default for PlatformState {
    fn default() -> Self {
        Self {
            reads: Default::default(),
            workspace: None,
            lease: None,
            root_input: std::env::var_os("HOME")
                .map(|p| {
                    PathBuf::from(p)
                        .join("JFQuantWorkspace")
                        .display()
                        .to_string()
                })
                .unwrap_or_default(),
            require_workspace: false,
            error: None,
            template: ResearchTemplate::builtin().expect("valid built-in recipe"),
            schedule_editor: String::new(),
            date_editor_valid: true,
            training_job: String::new(),
            independent_training_editor: String::new(),
            independent_backtest_editor: String::new(),
            independent_models: Value::Null,
            prediction_job: String::new(),
            rights_policy: Default::default(),
            action_review_editor: String::new(),
            evidence_tools: Default::default(),
            action_review_instrument: "SH600000".into(),
            action_evidence_manifest: String::new(),
            action_evidence_tasks: String::new(),
            datasets: vec![],
            runs: vec![],
            selected_run: None,
            market: String::new(),
            actions: String::new(),
            coefficients: String::new(),
            release_tag: String::new(),
            register_path: String::new(),
            hithink_key: String::new(),
            tdx_key: String::new(),
            source_policy_editor: serde_json::to_string_pretty(
                &analytics_workflow::refresh::SourcePolicy::default(),
            )
            .expect("serializable policy"),
            credential_save: None,
            collection_provider: analytics_storage::collector::Provider::Hithink,
            adjustment_source: analytics_storage::adjustment::AdjustmentSource::HithinkForward,
            collection_symbols: "SH600000,SZ000001,SH000300".into(),
            collection_benchmarks: "SH000300".into(),
            collection_universes: Default::default(),
            collection_index_inputs: Default::default(),
            index_versions: vec![],
            index_editor: String::new(),
            history_tools: Default::default(),
            index_path: "index.json".into(),
            index_version: String::new(),
            index_current_members: false,
            index_query: String::new(),
            index_selection: "msci_a50".into(),
            pool_editor: String::new(),
            pool_preview_date: "2026-10-12".into(),
            start: "2013-01-01".into(),
            end: chrono::Local::now().date_naive().to_string(),
            details: Value::Null,
            filter: String::new(),
            import_run_path: String::new(),
        }
    }
}
fn submit(
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
    request: WorkflowRequest,
) {
    state.platform.error = None;
    if !state.platform.date_editor_valid
        && matches!(
            request,
            WorkflowRequest::Research { .. }
                | WorkflowRequest::PreviewResearch { .. }
                | WorkflowRequest::PrepareResearchData { .. }
        )
    {
        state.platform.error = Some("请修正日期输入后再运行".into());
        return;
    }
    let cancel = cancellation();
    *cancel_slot = Some(cancel.clone());
    crate::panels::send_command(
        state,
        worker,
        WorkerCommand::ExecuteWorkflow {
            task: Uuid::new_v4(),
            request: Box::new(request),
            cancel,
        },
    );
    if state.worker_error.is_some() {
        *cancel_slot = None;
    }
}
fn select_data(
    ui: &mut egui::Ui,
    label: &str,
    kind: &str,
    selected: &mut String,
    datasets: &[DataVersion],
) {
    egui::ComboBox::from_id_salt(label)
        .selected_text(if selected.is_empty() {
            "（未选择）"
        } else {
            selected.as_str()
        })
        .show_ui(ui, |ui| {
            for d in datasets.iter().filter(|d| d.kind == kind) {
                ui.selectable_value(
                    selected,
                    d.id.clone(),
                    format!("{} · {}", d.id, d.end.as_deref().unwrap_or("未知日期")),
                );
            }
        });
}
pub(crate) fn status_label(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Running => "运行中",
        RunStatus::Succeeded => "成功",
        RunStatus::Failed => "失败",
        RunStatus::Blocked => "数据未就绪",
        RunStatus::Cancelled => "已取消",
        RunStatus::Interrupted => "已中断",
        RunStatus::Imported => "历史只读",
    }
}

mod workspace;
pub use workspace::{load_preferences, workspace_bar};
mod data;
pub use data::data_panel;
mod research;
pub use research::research_panel;
mod runs;
pub use runs::runs_panel;

#[cfg(test)]
impl PlatformState {
    pub(crate) fn result_loaded(&self) -> bool {
        self.reads.current.is_some()
    }
}
