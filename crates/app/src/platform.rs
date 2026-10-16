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
pub struct PlatformState {
    pub workspace: Option<Workspace>,
    pub lease: Option<WorkspaceLease>,
    pub root_input: String,
    pub require_workspace: bool,
    pub error: Option<String>,
    pub template: ResearchTemplate,
    pub schedule_editor: String,
    pub date_editor_valid: bool,
    pub action_review_editor: String,
    /// Previously completed training task ID.
    pub training_job: String,
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
    pub collection_csi300: bool,
    pub collection_csi500: bool,
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
            action_review_editor: String::new(),
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
            collection_csi300: false,
            collection_csi500: false,
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
fn preferences() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|p| PathBuf::from(p).join("Library/Application Support/JFQuant/preferences.json"))
}
fn select_workspace(state: &mut AppState, path: &Path) -> analytics_workflow::Result<()> {
    if state.busy() {
        return Err("任务执行时不能切换工作区".into());
    }
    if state
        .platform
        .workspace
        .as_ref()
        .is_some_and(|w| w.root() == path)
    {
        return Ok(());
    }
    let w = Workspace::open(path)?;
    if state
        .platform
        .workspace
        .as_ref()
        .is_some_and(|old| old.root() == w.root())
    {
        return Ok(());
    }
    #[cfg(test)]
    let lease = Some(w.lease()?);
    #[cfg(not(test))]
    let lease = None;
    #[cfg(test)]
    w.recover()?;
    let datasets = w.datasets()?;
    let runs = w.runs()?;
    state.platform.root_input = w.root().display().to_string();
    state.platform.workspace = Some(w);
    state.platform.lease = lease;
    state.platform.datasets = datasets;
    state.platform.runs = runs;
    state.platform.market = state
        .platform
        .datasets
        .iter()
        .find(|d| d.kind == "market")
        .map(|d| d.id.clone())
        .unwrap_or_default();
    state.platform.actions = state
        .platform
        .datasets
        .iter()
        .find(|d| d.kind == "actions")
        .map(|d| d.id.clone())
        .unwrap_or_default();
    state.platform.selected_run = None;
    state.platform.details = Value::Null;
    state.platform.error = None;
    Ok(())
}
pub fn load_preferences(state: &mut AppState) {
    state.platform.require_workspace = true;
    if let Some(p) = preferences()
        && let Ok(bytes) = fs::read(p)
    {
        match serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v["workspace"].as_str().map(PathBuf::from))
        {
            Some(path) => {
                if !path.is_dir() {
                    state.platform.root_input = path.display().to_string();
                    state.platform.error =
                        Some("上次工作区无法访问，请连接磁盘或重新选择。".into());
                    return;
                }
                if let Err(e) = select_workspace(state, &path) {
                    state.platform.error = Some(e.to_string());
                }
            }
            None => state.platform.error = Some("工作区设置无效，请重新选择".into()),
        }
    }
}
fn save_preferences(state: &AppState) -> analytics_workflow::Result<()> {
    if let (Some(p), Some(w)) = (preferences(), &state.platform.workspace) {
        fs::create_dir_all(p.parent().unwrap())?;
        let part = p.with_extension("tmp");
        fs::write(
            &part,
            serde_json::to_vec_pretty(
                &serde_json::json!({"schema_version":1,"workspace":w.root()}),
            )?,
        )?;
        fs::rename(part, p)?;
    }
    Ok(())
}
pub fn workspace_bar(ui: &mut egui::Ui, state: &mut AppState) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label("工作区");
        ui.add_enabled(
            !state.busy(),
            egui::TextEdit::singleline(&mut state.platform.root_input).desired_width(330.0),
        );
        if ui
            .add_enabled(!state.busy(), egui::Button::new("选择目录"))
            .clicked()
            && let Some(p) = rfd::FileDialog::new().pick_folder()
        {
            state.platform.root_input = p.display().to_string();
        }
        if ui
            .add_enabled(!state.busy(), egui::Button::new("创建/打开工作区"))
            .clicked()
        {
            let p = PathBuf::from(&state.platform.root_input);
            match select_workspace(state, &p) {
                Ok(()) => {
                    changed = true;
                    if let Err(e) = save_preferences(state) {
                        state.platform.error = Some(e.to_string());
                    }
                }
                Err(e) => state.platform.error = Some(e.to_string()),
            }
        }
    });
    if state.platform.workspace.is_none() {
        ui.label("选择可写目录保存数据、模板和本地运行记录。运行不依赖源码目录。");
    }
    if let Some(e) = &state.platform.error {
        ui.colored_label(crate::theme::semantic(ui.ctx()).error, e);
    }
    changed
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
pub fn data_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
) {
    if state.platform.workspace.is_none() {
        return;
    }
    ui.heading("数据更新与版本");
    if let Some(rx) = &state.platform.credential_save {
        match rx.try_recv() {
            Ok(result) => {
                state.platform.credential_save = None;
                if let Err(error) = result {
                    state.platform.error = Some(error);
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                state.platform.credential_save = None;
                state.platform.error = Some("凭据配置进程未完成".into());
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ui.label("正在保存本机凭据…");
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
    }
    ui.label(
        "更新保存新快照，已有研究继续使用原版本。全部数据默认通达信优先，不支持的字段按来源顺序补充。",
    );
    let enabled = !state.busy() && state.worker_error.is_none();
    let mut request = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(enabled, egui::Button::new("创建合成演示数据"))
            .clicked()
        {
            request = Some(WorkflowRequest::CreateCanonicalDemo {});
        }
        if ui
            .add_enabled(enabled, egui::Button::new("检查行情更新"))
            .clicked()
        {
            request = Some(WorkflowRequest::CheckUpdates {});
        }
        ui.label("发布日期");
        ui.text_edit_singleline(&mut state.platform.release_tag);
        if ui
            .add_enabled(
                enabled && !state.platform.release_tag.is_empty(),
                egui::Button::new("下载行情快照"),
            )
            .clicked()
        {
            request = Some(WorkflowRequest::DownloadMarket {
                tag: state.platform.release_tag.clone(),
                resume: state.platform.details["resume"]
                    .as_str()
                    .and_then(|s| s.parse().ok()),
            });
        }
    });
    ui.collapsing("HiThink / 通达信采集", |ui| {
        ui.horizontal(|ui| {
            ui.label("独立系数版本");
            select_data(
                ui,
                "coefficient_version",
                "coefficients",
                &mut state.platform.coefficients,
                &state.platform.datasets,
            );
            if ui
                .add_enabled(
                    enabled
                        && !state.platform.market.is_empty()
                        && !state.platform.coefficients.is_empty(),
                    egui::Button::new("生成研究数据视图"),
                )
                .clicked()
            {
                request = Some(WorkflowRequest::ComposeMarket {
                    market: state.platform.market.clone(),
                    coefficients: state.platform.coefficients.clone(),
                });
            }
        });
        ui.add_enabled_ui(enabled, |ui| {
            use analytics_storage::collector::{CollectionRequest, Provider};
            ui.collapsing("来源优先级与刷新", |ui| {
                ui.label("所有类别默认通达信优先，再用其他来源补缺。指定单一来源时不切换。历史成员和账户日期不能由当前快照代替。");
                if ui.button("读取已保存来源设置").clicked() { request=Some(WorkflowRequest::SourceStatus {}); }
                if let Some(policy)=state.platform.details.get("policy")
                    && ui.button("将读取结果载入编辑框").clicked() {state.platform.source_policy_editor=serde_json::to_string_pretty(policy).unwrap_or_default();}
                ui.add(egui::TextEdit::multiline(&mut state.platform.source_policy_editor).desired_rows(9));
                if ui.button("保存来源顺序").clicked() {
                    match serde_json::from_str(&state.platform.source_policy_editor) {
                        Ok(policy)=>request=Some(WorkflowRequest::SaveSourcePolicy {policy}),
                        Err(e)=>state.platform.error=Some(e.to_string()),
                    }
                }
                let mut refresh=None;
                ui.horizontal(|ui| {
                    for (label, mode, preview) in [
                        ("刷新预览",analytics_workflow::refresh::RefreshMode::Incremental,true),
                        ("更新行动",analytics_workflow::refresh::RefreshMode::Actions,false),
                        ("补新数据",analytics_workflow::refresh::RefreshMode::Incremental,false),
                        ("修复缺口",analytics_workflow::refresh::RefreshMode::Repair,false),
                        ("完整重取",analytics_workflow::refresh::RefreshMode::Full,false),
                    ] {
                        if ui.add_enabled(!state.platform.market.is_empty(),egui::Button::new(label)).clicked(){refresh=Some((mode,preview));}
                    }
                });
                if let Some((mode,preview))=refresh {
                    request=Some(WorkflowRequest::RefreshData {request:Box::new(analytics_workflow::refresh::RefreshRequest {
                        template:Box::new(state.platform.template.clone()),market:state.platform.market.clone(),actions:if mode==analytics_workflow::refresh::RefreshMode::Actions {None} else {(!state.platform.actions.is_empty()).then(||state.platform.actions.clone())},mode,provider:None,end:None,preview,resume:None,workers:3,reuse_shards_from:vec![],
                    })});
                }
            });
            ui.horizontal(|ui| {
                if ui.selectable_value(&mut state.platform.collection_provider,Provider::Tdxaidata,"通达信（备用）").clicked() {state.platform.adjustment_source=analytics_storage::adjustment::AdjustmentSource::TdxForward;}
                if ui
                    .selectable_value(
                        &mut state.platform.collection_provider,
                        Provider::Hithink,
                        "HiThink（主源）",
                    )
                    .clicked()
                {
                    state.platform.adjustment_source =
                        analytics_storage::adjustment::AdjustmentSource::HithinkForward;
                }
            });
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut state.platform.tdx_key).password(true).hint_text("通达信数据服务 Key，仅保存本机私有文件"));
                if ui.add_enabled(state.platform.credential_save.is_none(),egui::Button::new("保存通达信 Key")).clicked() {
                    let key=std::mem::take(&mut state.platform.tdx_key);
                    let (tx,rx)=std::sync::mpsc::channel();state.platform.credential_save=Some(rx);
                    std::thread::spawn(move || {let _=tx.send(analytics_workflow::service::configure_source("tdxaidata",key.trim()).map_err(|e|e.to_string()));});
                }
                if ui.button("通达信小样本验证（三轮）").clicked(){request=Some(WorkflowRequest::CheckSource {provider:Provider::Tdxaidata});}
            });
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.platform.hithink_key)
                        .password(true)
                        .hint_text("HiThink API Key，仅保存本机私有文件"),
                );
                if ui
                    .add_enabled(
                        state.platform.credential_save.is_none(),
                        egui::Button::new("保存凭据"),
                    )
                    .clicked()
                {
                    let token = std::mem::take(&mut state.platform.hithink_key);
                    let (tx, rx) = std::sync::mpsc::channel();
                    state.platform.credential_save = Some(rx);
                    std::thread::spawn(move || {
                        let _ = tx.send(
                            analytics_workflow::service::configure_source("hithink",token.trim())
                                .map_err(|e| e.to_string()),
                        );
                    });
                }
                if ui.button("检测接口权限").clicked() {
                    request = Some(WorkflowRequest::CheckSource { provider: Provider::Hithink });
                }
            });
            ui.label(match state.platform.adjustment_source {
                analytics_storage::adjustment::AdjustmentSource::TdxForward => "复权系数：通达信 ForwardFactor（方向与窗口一致性独立校验）",
                analytics_storage::adjustment::AdjustmentSource::HithinkForward => {
                    "HiThink 前复权 OHLC；原始量额；研究 VWAP 为 null"
                }
                analytics_storage::adjustment::AdjustmentSource::None => {
                    "复权系数：无（候选数据，不能用于严格研究）"
                }
            });
            ui.label("证券代码，以逗号分隔，如 SH600000,SZ000001,SH000300");
            ui.text_edit_singleline(&mut state.platform.collection_symbols);
            ui.label("其中的指数证券（不按股票接口采集）");
            ui.text_edit_singleline(&mut state.platform.collection_benchmarks);
            ui.horizontal(|ui| {
                ui.checkbox(&mut state.platform.collection_csi300, "登记当前沪深300成分");
                ui.checkbox(&mut state.platform.collection_csi500, "登记当前中证500成分");
            });
            ui.label("当前成分仅保存明确日期，不能回填历史；采集日期应包含训练、验证和因子预热。");
            ui.horizontal(|ui| {
                ui.label("开始");
                ui.text_edit_singleline(&mut state.platform.start);
                ui.label("结束");
                ui.text_edit_singleline(&mut state.platform.end);
            });
            let quote_source = true;
            let collect_quotes = ui
                .add_enabled(quote_source, egui::Button::new("仅采集行情（系数另采）"))
                .clicked();
            let collect_coefficients = ui
                .add_enabled(quote_source, egui::Button::new("仅采集独立复权系数"))
                .clicked();
            if ui
                .add_enabled(quote_source, egui::Button::new("采集日线并发布新版本"))
                .clicked()
                || collect_coefficients
                || collect_quotes
            {
                let parse = || -> Result<CollectionRequest, String> {
                    let ids = |v: &str| {
                        v.split([',', '，', ' ', '\n'])
                            .filter(|s| !s.trim().is_empty())
                            .map(|s| s.trim().to_ascii_uppercase())
                            .collect()
                    };
                    let r = CollectionRequest {
                        source_bindings: Default::default(),
                        provider: state.platform.collection_provider,
                        adjustment_source: state.platform.adjustment_source,
                        start: state.platform.start.parse().map_err(|_| "开始日期无效")?,
                        end: state.platform.end.parse().map_err(|_| "结束日期无效")?,
                        instruments: ids(&state.platform.collection_symbols),
                        benchmarks: ids(&state.platform.collection_benchmarks),
                        universes: [
                            (state.platform.collection_csi300, "csi300"),
                            (state.platform.collection_csi500, "csi500"),
                        ]
                        .into_iter()
                        .filter(|(yes, _)| *yes)
                        .map(|(_, id)| id.to_string())
                        .collect(),
                    };
                    r.validate().map_err(|e| e.to_string())?;
                    Ok(r)
                };
                match parse() {
                    Ok(r) => {
                        request = Some(if collect_coefficients {
                            WorkflowRequest::CollectCoefficients {
                                request: r,
                                resume: None,
                            }
                        } else if collect_quotes {
                            WorkflowRequest::CollectQuotes {
                                request: r,
                                resume: None,
                            }
                        } else {
                            WorkflowRequest::CollectMarket {
                                request: r,
                                resume: None,
                            }
                        })
                    }
                    Err(e) => state.platform.error = Some(e),
                }
            }
            let announcements = ui.button("获取巨潮实施公告（按以上证券与日期）").clicked();
            if ui.button("按指定来源采集分红资料").clicked() || announcements {
                let parse = || -> Result<CollectionRequest, String> {
                    let ids = |v: &str| {
                        v.split([',', '，', ' ', '\n'])
                            .filter(|s| !s.trim().is_empty())
                            .map(|s| s.trim().to_ascii_uppercase())
                            .collect::<std::collections::BTreeSet<_>>()
                    };
                    let instruments = ids(&state.platform.collection_symbols)
                        .difference(&ids(&state.platform.collection_benchmarks))
                        .cloned()
                        .collect();
                    let r = CollectionRequest {
                        source_bindings: Default::default(),
                        provider: if announcements { Provider::Cninfo } else { state.platform.collection_provider },
                        adjustment_source: if announcements { analytics_storage::adjustment::AdjustmentSource::None } else { state.platform.adjustment_source },
                        start: state.platform.start.parse().map_err(|_| "开始日期无效")?,
                        end: state.platform.end.parse().map_err(|_| "结束日期无效")?,
                        instruments,
                        benchmarks: Default::default(),
                        universes: Default::default(),
                    };
                    r.validate().map_err(|e| e.to_string())?;
                    Ok(r)
                };
                match parse() {
                    Ok(r) => {
                        request = Some(if announcements { WorkflowRequest::CollectAnnouncements {
                            request: r,
                            resume: None,
                        }} else { WorkflowRequest::CollectActions {
                            request: r,
                            resume: None,
                        }})
                    }
                    Err(e) => state.platform.error = Some(e),
                }
            }
            ui.label("失败记录可恢复已验证证券；Yahoo价格不会自动作为不复权撮合价。");
        });
    });
    ui.horizontal(|ui| {
        ui.label("登记已有数据");
        ui.text_edit_singleline(&mut state.platform.register_path);
        if ui
            .add_enabled(enabled, egui::Button::new("数据目录"))
            .clicked()
            && let Some(p) = rfd::FileDialog::new().pick_folder()
        {
            state.platform.register_path = p.display().to_string();
        }
        if ui
            .add_enabled(enabled, egui::Button::new("公司行动文件"))
            .clicked()
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
        {
            state.platform.register_path = p.display().to_string();
        }
        if ui
            .add_enabled(
                enabled && !state.platform.register_path.is_empty(),
                egui::Button::new("核验并登记"),
            )
            .clicked()
        {
            request = Some(WorkflowRequest::RegisterData {
                path: state.platform.register_path.clone().into(),
            });
        }
    });
    ui.horizontal(|ui| {
        ui.label("行情版本");
        select_data(
            ui,
            "更新行情选择",
            "market",
            &mut state.platform.market,
            &state.platform.datasets,
        );
        if ui
            .add_enabled(
                enabled && !state.platform.market.is_empty(),
                egui::Button::new("下载公司行动历史"),
            )
            .clicked()
        {
            request = Some(WorkflowRequest::DownloadActions {
                market: state.platform.market.clone(),
                pool_version: state.platform.template.pool_version.clone(),
                resume: state.platform.details["resume_actions"]
                    .as_str()
                    .and_then(|s| s.parse().ok()),
            });
        }
    });
    if let Some(r) = request {
        submit(state, worker, cancel_slot, r);
    }
    egui::CollapsingHeader::new("已登记的数据版本")
        .default_open(true)
        .show(ui, |ui| {
            egui::Grid::new("dataset_versions")
                .striped(true)
                .show(ui, |ui| {
                    for d in &state.platform.datasets {
                        ui.label(&d.id);
                        ui.label(d.end.as_deref().unwrap_or("未知日期"));
                        ui.label(d.path.display().to_string());
                        ui.collapsing("详情", |ui| {
                            ui.label(serde_json::to_string_pretty(&d.details).unwrap_or_default());
                        });
                        ui.end_row();
                    }
                });
        });
    ui.separator();
}
pub fn research_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
    _registry: &analytics_factor::FactorRegistry,
) {
    ui.heading("研究流程");
    crate::quality_ui::reasons(ui, &state.platform.details);
    if state.platform.workspace.is_none() {
        ui.label("先创建或打开工作区。");
        return;
    }
    let enabled = !state.busy() && state.worker_error.is_none();
    let mut save = false;
    let mut load = false;
    let mut export = false;
    let mut import = false;
    let (mut save_factors, mut load_factors, mut save_target, mut load_target) =
        (false, false, false, false);
    ui.horizontal(|ui| {
        ui.label("模板名称");
        ui.text_edit_singleline(&mut state.platform.template.name);
        save = ui
            .add_enabled(enabled, egui::Button::new("保存模板"))
            .clicked();
        load = ui
            .add_enabled(enabled, egui::Button::new("载入保存模板"))
            .clicked();
        export = ui
            .add_enabled(enabled, egui::Button::new("导出模板"))
            .clicked();
        import = ui
            .add_enabled(enabled, egui::Button::new("导入模板"))
            .clicked();
    });
    ui.horizontal(|ui| {
        ui.label("独立配置");
        save_factors = ui
            .add_enabled(enabled, egui::Button::new("保存因子配置"))
            .clicked();
        load_factors = ui
            .add_enabled(enabled, egui::Button::new("载入因子配置"))
            .clicked();
        save_target = ui
            .add_enabled(enabled, egui::Button::new("保存标的配置"))
            .clicked();
        load_target = ui
            .add_enabled(enabled, egui::Button::new("载入标的配置"))
            .clicked();
    });
    let w = state.platform.workspace.as_ref().unwrap().clone();
    let result = (|| -> analytics_workflow::Result<()> {
        use analytics_workflow::profiles::{FactorProfile, TargetProfile};
        if save_factors
            && let Some(p) = rfd::FileDialog::new()
                .set_file_name("factors.json")
                .save_file()
        {
            FactorProfile::from_template(&state.platform.template).write(&p)?;
        }
        if load_factors
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
        {
            FactorProfile::read(&p)?.apply(&mut state.platform.template)?;
        }
        if save_target
            && let Some(p) = rfd::FileDialog::new()
                .set_file_name("target.json")
                .save_file()
        {
            TargetProfile::from_template(&state.platform.template).write(&p)?;
        }
        if load_target
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
        {
            TargetProfile::read(&p)?.apply(&mut state.platform.template)?;
        }
        if save {
            if !state.platform.date_editor_valid {
                return Err("请修正日期输入后再保存".into());
            }
            let id = analytics_workflow::signature(&state.platform.template)?;
            state
                .platform
                .template
                .write(&w.resolve(format!(".jfquant/templates/{}.json", &id[7..23])))?;
        }
        if load
            && let Some(p) = rfd::FileDialog::new()
                .set_directory(w.resolve(".jfquant/templates"))
                .add_filter("JSON", &["json"])
                .pick_file()
        {
            state.platform.template = ResearchTemplate::read(&p)?;
            state.platform.schedule_editor.clear();
            state.platform.date_editor_valid = true;
            crate::schedule_ui::reset(ui);
        }
        if export
            && let Some(p) = rfd::FileDialog::new()
                .set_file_name("research-template.json")
                .save_file()
        {
            state.platform.template.write(&p)?;
        }
        if import
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
        {
            state.platform.template = ResearchTemplate::read(&p)?;
            state.platform.schedule_editor.clear();
            state.platform.date_editor_valid = true;
            crate::schedule_ui::reset(ui);
        }
        Ok(())
    })();
    if let Err(e) = result {
        state.platform.error = Some(e.to_string());
    }
    ui.horizontal(|ui| {
        ui.label("行情");
        select_data(
            ui,
            "研究行情选择",
            "market",
            &mut state.platform.market,
            &state.platform.datasets,
        );
        ui.label("公司行动");
        select_data(
            ui,
            "研究动作选择",
            "actions",
            &mut state.platform.actions,
            &state.platform.datasets,
        );
    });
    ui.collapsing("股票池、基准与日期调整", |ui| {
        ui.add_enabled_ui(enabled, |ui| {
            ui.horizontal(|ui| {
                if ui.button("中证500默认模板").clicked(){state.platform.template=ResearchTemplate::builtin().expect("builtin");state.platform.schedule_editor.clear();state.platform.date_editor_valid=true;crate::schedule_ui::reset(ui);}
                if ui.button("沪深300默认模板").clicked(){state.platform.template=ResearchTemplate::builtin_csi300().expect("builtin");state.platform.schedule_editor.clear();state.platform.date_editor_valid=true;crate::schedule_ui::reset(ui);}
            });
            ui.horizontal(|ui| {ui.label("股票池版本");ui.text_edit_singleline(&mut state.platform.template.pool_version);ui.label("基准证券");ui.text_edit_singleline(&mut state.platform.template.benchmark);});
            use analytics_core::pool::PoolExitPolicy;
            ui.horizontal(|ui| {ui.label("移除持仓");ui.selectable_value(&mut state.platform.template.pool_exit_policy,PoolExitPolicy::NextTradableDay,"下一可交易日退出");ui.selectable_value(&mut state.platform.template.pool_exit_policy,PoolExitPolicy::NextRebalance,"下次正常调仓退出");});
            ui.horizontal(|ui| {
                if ui.button("载入股票池JSON").clicked() && let Some(p)=rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file(){match fs::read_to_string(p){Ok(s)=>state.platform.pool_editor=s,Err(e)=>state.platform.error=Some(e.to_string())}}
                if ui.button("自选池示例").clicked(){state.platform.pool_editor=serde_json::json!({"schema_version":3,"name":"自选池","coverage":{"start":state.platform.start,"end":state.platform.end},"expression":{"kind":"symbols","instruments":["SH600000","SZ000001"]},"adjustments":[]}).to_string();}
                if ui.button("指数组合示例").clicked(){state.platform.pool_editor=serde_json::json!({"schema_version":3,"name":"沪深300与中证500并集","coverage":{"start":state.platform.start,"end":state.platform.end},"expression":{"kind":"union","inputs":[{"kind":"universe","version":state.platform.market,"universe":"csi300"},{"kind":"universe","version":state.platform.market,"universe":"csi500"}]},"adjustments":[]}).to_string();}
            });
            ui.label("股票池定义：支持 union / intersection / difference；adjustments 按 effective_date 设置 add / remove。");
            ui.add(egui::TextEdit::multiline(&mut state.platform.pool_editor).desired_rows(6).desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                if ui.button("校验并保存股票池新版本").clicked(){match serde_json::from_str::<analytics_core::pool::PoolDefinition>(&state.platform.pool_editor){Ok(d)=>{if let Err(e)=d.validate(){state.platform.error=Some(e);}else{submit(state,worker,cancel_slot,WorkflowRequest::SavePool{definition:d});}},Err(e)=>state.platform.error=Some(e.to_string())}}
                if ui.button("导出股票池JSON").clicked() && let Some(p)=rfd::FileDialog::new().set_file_name("stock-pool.json").save_file(){let result=(||->analytics_workflow::Result<()>{let d:analytics_core::pool::PoolDefinition=serde_json::from_str(&state.platform.pool_editor)?;d.validate()?;fs::write(p,serde_json::to_vec_pretty(&d)?)?;Ok(())})();if let Err(e)=result{state.platform.error=Some(e.to_string());}}
            });
            ui.horizontal(|ui| {ui.label("成员预览日期");ui.text_edit_singleline(&mut state.platform.pool_preview_date);if ui.button("预览成员").clicked(){match state.platform.pool_preview_date.parse(){Ok(day)=>submit(state,worker,cancel_slot,WorkflowRequest::PreviewPool{id:state.platform.template.pool_version.clone(),market:state.platform.market.clone(),day}),Err(_)=>state.platform.error=Some("预览日期无效".into())}}});
        });
    });
    ui.add_enabled_ui(enabled,|ui|{
        ui.horizontal(|ui|{
            for (label,announcements) in [("补采所需公司行动",false),("获取实施公告证据",true)] {
                if ui.button(label).clicked(){submit(state,worker,cancel_slot,WorkflowRequest::PrepareResearchData{actions:(!state.platform.actions.is_empty()).then(||state.platform.actions.clone()),template:Box::new(state.platform.template.clone()),market:state.platform.market.clone(),announcements,resume:None});}
            }
            if ui.button("预览研究日程").clicked(){submit(state,worker,cancel_slot,WorkflowRequest::PreviewResearch{actions:(!state.platform.actions.is_empty()).then(||state.platform.actions.clone()),template:Box::new(state.platform.template.clone()),market:state.platform.market.clone()});}
        });
        ui.collapsing("公司行动：查看证据与审计修订",|ui|{
            ui.horizontal(|ui| { ui.label("核对证据任务ID（逗号分隔，可空）"); ui.text_edit_singleline(&mut state.platform.action_evidence_tasks); });
            if ui.button("自动核对并发布新版本").clicked() {
                let ids = state.platform.action_evidence_tasks.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::parse).collect::<std::result::Result<Vec<uuid::Uuid>,_>>();
                match ids { Ok(evidence_tasks) => submit(state,worker,cancel_slot,WorkflowRequest::ReconcileActions { actions:state.platform.actions.clone(), evidence_tasks, market:(!state.platform.market.is_empty()).then(||state.platform.market.clone()) }),Err(e)=>state.platform.error=Some(e.to_string()) }
            }
            ui.label("从运行结果查看原值、候选字段及公告。修订JSON必须包含原事件哈希、逐字段原文引用及理由。");
            ui.add(egui::TextEdit::multiline(&mut state.platform.action_review_editor).desired_rows(8).desired_width(f32::INFINITY));
            if ui.button("提交证据修订为新版本").clicked(){match serde_json::from_str(&state.platform.action_review_editor){Ok(request)=>submit(state,worker,cancel_slot,WorkflowRequest::ReviseActions{actions:state.platform.actions.clone(),request}),Err(e)=>state.platform.error=Some(e.to_string())}}
        });
    });
    ui.add_enabled_ui(enabled, |ui| {
            ui.horizontal(|ui|{ui.label("证券");ui.text_edit_singleline(&mut state.platform.action_review_instrument);ui.label("公告manifest");ui.text_edit_singleline(&mut state.platform.action_evidence_manifest);});
            if ui.button("加载原事件、哈希与公告候选").clicked(){submit(state,worker,cancel_slot,WorkflowRequest::InspectActions{actions:state.platform.actions.clone(),instrument:state.platform.action_review_instrument.clone(),evidence_manifest:if state.platform.action_evidence_manifest.trim().is_empty(){None}else{Some(PathBuf::from(state.platform.action_evidence_manifest.trim()))}});}
        let t = &mut state.platform.template;
        ui.horizontal(|ui| {
            ui.label("训练价格口径");
            egui::ComboBox::from_id_salt("training_price_basis").selected_text(t.price_basis.label()).show_ui(ui,|ui| {
                for basis in [analytics_core::research_quality::PriceBasis::Verified,analytics_core::research_quality::PriceBasis::Provider] {ui.selectable_value(&mut t.price_basis,basis,basis.label());}
            });
            let old=t.factor_set;
            egui::ComboBox::from_id_salt("research_factor_set").selected_text(format!("{:?}",old)).show_ui(ui,|ui| {
                ui.selectable_value(&mut t.factor_set,analytics_core::research_quality::FactorSet::Gtja191,"GTJA191");
                ui.selectable_value(&mut t.factor_set,analytics_core::research_quality::FactorSet::Alpha158,"Alpha158");
            });
            if old!=t.factor_set {t.select_factor_set(t.factor_set);}
        });
        crate::quality_ui::controls(ui, &mut t.quality_policy);
        ui.label("AKQuant：分红与拆股按除权除息日处理；不支持的已知权益事件停止回测，保留模型。");
        ui.collapsing("日期调度：单次 / 滚动 / 手动多折", |ui| {
            let before=serde_json::to_string(&t.schedule).unwrap_or_default();
            crate::schedule_ui::draw(ui,&mut t.schedule);
            if before!=serde_json::to_string(&t.schedule).unwrap_or_default(){state.platform.schedule_editor.clear();}
            if state.platform.schedule_editor.is_empty(){state.platform.schedule_editor=serde_json::to_string_pretty(&t.schedule).unwrap_or_default();}
            ui.label("任意train / validation（可为null）/ test日期；滚动支持days/months/years/trading_days及expanding。");
            ui.add(egui::TextEdit::multiline(&mut state.platform.schedule_editor).desired_rows(14).desired_width(f32::INFINITY));
            if ui.button("应用日期设置").clicked(){match serde_json::from_str::<analytics_core::schedule::Schedule>(&state.platform.schedule_editor){Ok(s)=>match s.validate(){Ok(())=>t.schedule=s,Err(e)=>state.platform.error=Some(e)},Err(e)=>state.platform.error=Some(e.to_string())}}

        });
        ui.horizontal(|ui| {
            ui.label("持仓数量");
            ui.add(egui::DragValue::new(&mut t.config.top_n).range(1..=1000));
            ui.label("调仓间隔（交易日）");
            ui.add(egui::DragValue::new(&mut t.config.rebalance_days).range(1..=252));
            ui.label("初始资金（元）");
            ui.add(
                egui::DragValue::new(&mut t.initial_cash_cny)
                    .speed(10000.0)
                    .range(1.0..=1e13),
            );
        });
        ui.horizontal(|ui| {
            ui.label("佣金比例");
            ui.add(
                egui::DragValue::new(&mut t.market_rules.commission.rate)
                    .speed(0.00001)
                    .range(0.0..=0.1)
                    .max_decimals(6),
            );
            ui.label("最低佣金（元）");
            ui.add(
                egui::DragValue::new(&mut t.market_rules.commission.minimum_cny)
                    .range(0.0..=10000.0),
            );
            use analytics_core::market_rules::CommissionMode;
            egui::ComboBox::from_id_salt("research_commission")
                .selected_text(format!("{:?}", t.market_rules.commission.mode))
                .show_ui(ui, |ui| {
                    for (mode, label) in [
                        (CommissionMode::Gross, "全佣（含经手及证管费）"),
                        (CommissionMode::AllInclusive, "含过户费全佣"),
                    ] {
                        ui.selectable_value(&mut t.market_rules.commission.mode, mode, label);
                    }
                });
        });
        ui.label("分红使用税前金额；费用只展示上游合计，不模拟个人持股期税。");
        ui.collapsing("训练参数", |ui| {
            egui::Grid::new("research_training").show(ui, |ui| {
                for key in [
                    "num_iterations",
                    "learning_rate",
                    "num_leaves",
                    "min_data_in_leaf",
                    "feature_fraction",
                    "lambda_l2",
                    "num_threads",
                    "seed",
                ] {
                    ui.label(key);
                    ui.text_edit_singleline(
                        t.config.training.parameters.entry(key.into()).or_default(),
                    );
                    ui.end_row();
                }
            });
            let mut use_early = t.config.training.early_stopping_rounds.is_some();
            if ui.checkbox(&mut use_early, "启用早停").changed() {
                t.config.training.early_stopping_rounds = use_early.then_some(20);
            }
            if let Some(n) = &mut t.config.training.early_stopping_rounds {
                ui.add(egui::DragValue::new(n).range(1..=1000));
            }
        });
    });
    ui.collapsing(
        format!(
            "研究因子（已选{}）",
            state.platform.template.config.factor_ids.len()
        ),
        |ui| {
            ui.text_edit_singleline(&mut state.platform.filter);
            let quarantined = analytics_factor::audit::quarantined_ids();
            egui::ScrollArea::vertical()
                .id_salt("research_factors")
                .max_height(240.0)
                .show(ui, |ui| {
                    let registry = analytics_factor::FactorRegistry::for_set(
                        state.platform.template.factor_set,
                    );
                    for s in registry
                        .all()
                        .filter(|s| s.id.contains(&state.platform.filter))
                    {
                        let available = s.id != "alpha_030" && !quarantined.contains(&s.id);
                        let mut selected =
                            state.platform.template.config.factor_ids.contains(&s.id);
                        let response = ui.add_enabled(
                            enabled && available,
                            egui::Checkbox::new(&mut selected, &s.id),
                        );
                        if response.changed() {
                            if selected {
                                state.platform.template.config.factor_ids.push(s.id.clone());
                            } else {
                                state
                                    .platform
                                    .template
                                    .config
                                    .factor_ids
                                    .retain(|id| id != &s.id);
                            }
                        }
                        if !available {
                            ui.label(if s.id == "alpha_030" {
                                "缺少真实MKT/SMB/HML输入"
                            } else {
                                "公式歧义已隔离，详见因子审计"
                            });
                        }
                    }
                });
        },
    );
    ui.label("不复权成交、整数股、T+1、跨年持续账户；训练与账户执行资料分别检查。");
    let ready = enabled && !state.platform.market.is_empty();
    let execute_ready = ready && !state.platform.actions.is_empty();
    let mut run = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(ready, egui::Button::new("检查研究输入"))
            .clicked()
        {
            run = Some(true);
        }
        if ui
            .add_enabled(execute_ready, egui::Button::new("执行年度研究"))
            .clicked()
        {
            run = Some(false);
        }
    });
    if let Some(audit_only) = run {
        match state.platform.template.validate() {
            Ok(()) => submit(
                state,
                worker,
                cancel_slot,
                WorkflowRequest::Research {
                    template: Box::new(state.platform.template.clone()),
                    market: state.platform.market.clone(),
                    actions: (!state.platform.actions.is_empty())
                        .then(|| state.platform.actions.clone()),
                    audit_only,
                },
            ),
            Err(e) => state.platform.error = Some(e.to_string()),
        }
    }
    ui.horizontal(|ui| {
        if ui
            .add_enabled(ready, egui::Button::new("仅训练模型"))
            .clicked()
        {
            submit(
                state,
                worker,
                cancel_slot,
                WorkflowRequest::TrainResearch {
                    template: Box::new(state.platform.template.clone()),
                    market: state.platform.market.clone(),
                    actions: (!state.platform.actions.is_empty())
                        .then(|| state.platform.actions.clone()),
                },
            );
        }
        ui.label("训练任务ID");
        ui.text_edit_singleline(&mut state.platform.training_job);
        if ui
            .add_enabled(
                enabled && !state.platform.actions.is_empty(),
                egui::Button::new("使用已有模型回测"),
            )
            .clicked()
        {
            match state.platform.training_job.parse() {
                Ok(training) => submit(
                    state,
                    worker,
                    cancel_slot,
                    WorkflowRequest::BacktestResearch {
                        execution: Some(Box::new(state.platform.template.execution_settings())),
                        event_check_policy: state.platform.template.event_check_policy,
                        training,
                        actions: state.platform.actions.clone(),
                    },
                ),
                Err(_) => state.platform.error = Some("请填写已完成训练的任务ID".into()),
            }
        }
    });
    show_results(ui, &state.platform.details);
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
pub fn runs_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
) {
    ui.heading("本地运行记录");
    let Some(w) = state.platform.workspace.as_ref().cloned() else {
        ui.label("先选择工作区。");
        return;
    };
    if ui.button("刷新运行记录").clicked() {
        match w.runs() {
            Ok(r) => state.platform.runs = r,
            Err(e) => state.platform.error = Some(e.to_string()),
        }
    }
    ui.horizontal(|ui| {
        ui.label("登记当前格式研究目录");
        ui.text_edit_singleline(&mut state.platform.import_run_path);
        if ui
            .add_enabled(!state.busy(), egui::Button::new("导入只读记录"))
            .clicked()
        {
            match w
                .import_run(&w.resolve(&state.platform.import_run_path))
                .and_then(|_| w.runs())
            {
                Ok(r) => state.platform.runs = r,
                Err(e) => state.platform.error = Some(e.to_string()),
            }
        }
    });
    let mut selected = None;
    egui::ScrollArea::vertical()
        .id_salt("run_list")
        .max_height(220.0)
        .show(ui, |ui| {
            egui::Grid::new("run_history").striped(true).show(ui, |ui| {
                for r in &state.platform.runs {
                    if ui
                        .selectable_label(
                            state.platform.selected_run == Some(r.id),
                            format!("{} · {}", r.started_at, r.operation),
                        )
                        .clicked()
                    {
                        selected = Some(r.id);
                    }
                    ui.label(status_label(r.status));
                    ui.label(&r.stage);
                    ui.end_row();
                }
            });
        });
    if let Some(id) = selected {
        state.platform.selected_run = Some(id);
    }
    if let Some(r) = state
        .platform
        .runs
        .iter()
        .find(|r| Some(r.id) == state.platform.selected_run)
        .cloned()
    {
        ui.monospace(r.id.to_string());
        ui.label(status_label(r.status));
        if let Some(e) = &r.error {
            ui.colored_label(crate::theme::semantic(ui.ctx()).error, e);
        }
        if ui.button("打开运行文件夹").clicked() {
            let _ = std::process::Command::new("/usr/bin/open")
                .arg(w.resolve(&r.output))
                .spawn();
        }
        if !state.busy()
            && matches!(
                r.status,
                RunStatus::Interrupted | RunStatus::Cancelled | RunStatus::Failed
            )
        {
            let request = serde_json::from_value::<WorkflowRequest>(
                r.request
                    .get("request")
                    .cloned()
                    .unwrap_or(r.request.clone()),
            );
            match request {
                Ok(WorkflowRequest::DownloadMarket { tag, resume }) => {
                    if ui.button("准备恢复行情").clicked() {
                        state.platform.release_tag = tag;
                        state.platform.details =
                            serde_json::json!({"resume":resume.unwrap_or(r.id)});
                        state.panel = crate::state::Panel::Dataset;
                    }
                }
                Ok(WorkflowRequest::DownloadActions {
                    market,
                    resume,
                    pool_version,
                }) => {
                    if ui.button("准备恢复公司行动").clicked() {
                        state.platform.market = market;
                        state.platform.template.pool_version = pool_version;
                        state.platform.details =
                            serde_json::json!({"resume_actions":resume.unwrap_or(r.id)});
                        state.panel = crate::state::Panel::Dataset;
                    }
                }
                Ok(WorkflowRequest::CollectMarket { request, resume }) => {
                    if ui.button("恢复行情采集").clicked() {
                        submit(
                            state,
                            worker,
                            cancel_slot,
                            WorkflowRequest::CollectMarket {
                                request,
                                resume: Some(resume.unwrap_or(r.id)),
                            },
                        );
                    }
                }
                Ok(WorkflowRequest::CollectAnnouncements { request, resume }) => {
                    if ui.button("恢复巨潮公告采集").clicked() {
                        submit(
                            state,
                            worker,
                            cancel_slot,
                            WorkflowRequest::CollectAnnouncements {
                                request,
                                resume: Some(resume.unwrap_or(r.id)),
                            },
                        );
                    }
                }
                Ok(WorkflowRequest::CollectActions { request, resume }) => {
                    if ui.button("恢复公司行动采集").clicked() {
                        submit(
                            state,
                            worker,
                            cancel_slot,
                            WorkflowRequest::CollectActions {
                                request,
                                resume: Some(resume.unwrap_or(r.id)),
                            },
                        );
                    }
                }
                _ => {
                    ui.label("该任务不支持恢复；旧来源任务请重新采集，研究任务请重新执行。");
                }
            }
        }
        ui.collapsing("参数及来源快照", |ui| {
            ui.add(
                egui::Label::new(serde_json::to_string_pretty(&r.request).unwrap_or_default())
                    .selectable(true),
            );
        });
        let mut synthetic = false;
        let mut result_supported = false;
        match w.results(&r) {
            Ok(v) => {
                result_supported = true;
                synthetic = v["input_kind"] == "synthetic_fixture";
                crate::quality_ui::reasons(ui, &v);
                show_results(ui, &v);
            }
            Err(e) => {
                ui.colored_label(
                    crate::theme::semantic(ui.ctx()).error,
                    format!("读取结果失败：{e}"),
                );
            }
        }
        let artifact_dir = if r.read_only {
            r.request["source"].as_str().map(PathBuf::from)
        } else {
            Some(w.resolve(&r.output))
        };
        if result_supported && let Some(dir) = artifact_dir {
            show_nav(ui, &dir.join("nav.csv"), synthetic);
        }
        if let Ok(log) = fs::read_to_string(w.resolve(&r.output).join("events.jsonl")) {
            ui.collapsing("持久化日志", |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("run_log")
                    .max_height(240.0)
                    .show(ui, |ui| {
                        for line in log.lines().rev().take(200) {
                            if let Ok(v) = serde_json::from_str::<Value>(line) {
                                ui.label(format!(
                                    "{} {}",
                                    v["time"].as_str().unwrap_or(""),
                                    v["message"].as_str().unwrap_or("")
                                ));
                            }
                        }
                    });
            });
        }
    }
}
fn show_results(ui: &mut egui::Ui, value: &Value) {
    if value.is_null() {
        return;
    }
    let trained = value["status"] == "trained"
        || value["training_artifact"]["status"] == "trained"
        || value["training_complete"] == true;
    if value["status"] == "trained" {
        ui.label("训练已完成，模型已保存。尚未执行回测。");
    }
    let predicted = value["prediction_complete"] == true
        || value["prediction_artifact"]["status"] == "completed"
        || value["stages"]["prediction"] == "completed";
    if value["ready"] == false {
        ui.colored_label(
            crate::theme::semantic(ui.ctx()).warning,
            if trained && predicted {
                "回测未完成。已保存训练模型与预测；账户记录以阶段产物为准。"
            } else if trained {
                "回测未完成。已保存训练模型，预测阶段尚未完成。"
            } else {
                "研究数据未就绪，前置检查未通过；尚未完成训练与回测。"
            },
        );
    }
    if value["skipped_event_count"].as_u64().unwrap_or(0) > 0 {
        ui.colored_label(
            crate::theme::semantic(ui.ctx()).warning,
            "回测完成，含未模拟事件。收益按披露的简化口径计算。",
        );
    }
    if value["backtest_status"] == "no_valid_trades" {
        ui.colored_label(
            crate::theme::semantic(ui.ctx()).warning,
            "无有效交易。账户保留现金，本次结果不能用于判断策略效果。",
        );
    }
    if value["execution_diagnostics"].is_object() {
        let d = &value["execution_diagnostics"];
        ui.label(format!(
            "有效信号：{}；成交回报：{}；拒单回报：{}",
            d["valid_signals"], d["filled_events"], d["rejected_events"]
        ));
    }
    if value["input_kind"] == "synthetic_fixture" {
        ui.colored_label(
            crate::theme::semantic(ui.ctx()).warning,
            "合成演示数据，仅验证流程与账本，不代表真实市场收益。",
        );
    }
    if value["aggregate"].is_object() {
        if let Err(error) = analytics_workflow::research::backtest::validate_result(value) {
            ui.colored_label(crate::theme::semantic(ui.ctx()).error, error.to_string());
            return;
        }
        ui.label("不复权整数股账户结果");
        let percent = |v: &Value| {
            v.as_f64()
                .map(|n| format!("{:.2}%", n * 100.0))
                .unwrap_or_else(|| "—".into())
        };
        let a = &value["aggregate"];
        let number = |v: &Value| {
            v.as_f64()
                .map(|n| format!("{n:.4}"))
                .unwrap_or_else(|| "—".into())
        };
        ui.label(format!(
            "年化 {} · 最大回撤 {} · Sharpe {}",
            percent(&a["annualized_return"]),
            percent(&a["max_drawdown"]),
            number(&a["sharpe"])
        ));
        egui::Grid::new("research_annual")
            .striped(true)
            .show(ui, |ui| {
                for h in ["年度", "收益", "回撤", "IC", "RankIC", "费用（元）"] {
                    ui.label(h);
                }
                ui.end_row();
                if let Some(rows) = value["annual_reports"].as_array() {
                    for r in rows {
                        ui.label(r["year"].to_string());
                        ui.label(percent(&r["performance"]["total_return"]));
                        ui.label(percent(&r["performance"]["max_drawdown"]));
                        ui.label(number(&r["ic"]));
                        ui.label(number(&r["rank_ic"]));
                        let cost = r["performance"]["execution_fee_cny"]
                            .as_f64()
                            .unwrap_or(0.0);
                        ui.label(format!("{cost:.2}"));
                        ui.end_row();
                    }
                }
            });
    }
    if let Some(folds) = value["resolved_folds"]
        .as_array()
        .or_else(|| value["execution"]["resolved_folds"].as_array())
    {
        ui.collapsing("实际训练 / 验证 / 回测日期", |ui| {
            egui::Grid::new("resolved-date-folds")
                .striped(true)
                .show(ui, |ui| {
                    for label in ["折", "训练", "验证", "回测", "预热交易日"] {
                        ui.label(label);
                    }
                    ui.end_row();
                    for (i, f) in folds.iter().enumerate() {
                        ui.label((i + 1).to_string());
                        for key in ["train", "validation", "test"] {
                            let r = &f["actual"][key];
                            ui.label(if r.is_null() {
                                "无".into()
                            } else {
                                format!(
                                    "{} ~ {}",
                                    r["start"].as_str().unwrap_or("?"),
                                    r["end"].as_str().unwrap_or("?")
                                )
                            });
                        }
                        ui.label(f["warmup_sessions"].to_string());
                        ui.end_row();
                    }
                });
        });
    }
    ui.collapsing("完整结果与检查明细", |ui| {
        ui.add(
            egui::Label::new(serde_json::to_string_pretty(value).unwrap_or_default())
                .selectable(true),
        );
    });
}

fn show_nav(ui: &mut egui::Ui, path: &Path, synthetic: bool) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let rows: Vec<(String, f64, f64)> = text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let c: Vec<_> = line.split(',').collect();
            if c.len() != 3 {
                return None;
            }
            let a = c[1].parse::<f64>().ok()?;
            let b = c[2].parse::<f64>().ok()?;
            if !a.is_finite() || !b.is_finite() {
                return None;
            }
            Some((c[0].into(), a, b))
        })
        .collect();
    if rows.len() < 2 {
        return;
    }
    ui.label(format!(
        "净值 · {} — {} · 蓝色策略 / 绿色{}",
        rows[0].0,
        rows.last().unwrap().0,
        if synthetic {
            "合成基准"
        } else {
            "中证500价格指数"
        }
    ));
    let lo = rows
        .iter()
        .flat_map(|r| [r.1, r.2])
        .fold(f64::INFINITY, f64::min);
    let hi = rows
        .iter()
        .flat_map(|r| [r.1, r.2])
        .fold(f64::NEG_INFINITY, f64::max);
    // Keep constant NAV visible and leave room above and below the curves.
    let padding = (hi - lo).max(0.01) * 0.1;
    let lo = lo - padding;
    let span = hi + padding - lo;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().max(100.0), 180.0),
        egui::Sense::hover(),
    );
    let rect = rect.shrink(4.0);
    ui.painter().rect_stroke(
        rect,
        0.0,
        ui.visuals().widgets.noninteractive.bg_stroke,
        egui::StrokeKind::Inside,
    );
    let step = (rows.len() / 1000).max(1);
    for (strategy, color) in [
        (true, crate::theme::semantic(ui.ctx()).accent),
        (false, crate::theme::semantic(ui.ctx()).success),
    ] {
        let points = rows
            .iter()
            .enumerate()
            .step_by(step)
            .map(|(i, r)| {
                egui::pos2(
                    rect.left() + rect.width() * i as f32 / (rows.len() - 1) as f32,
                    rect.bottom()
                        - rect.height() * ((if strategy { r.1 } else { r.2 }) - lo) as f32
                            / span as f32,
                )
            })
            .collect();
        ui.painter()
            .add(egui::Shape::line(points, egui::Stroke::new(1.5, color)));
    }
}
