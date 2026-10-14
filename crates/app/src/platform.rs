//! Workspace onboarding, immutable data versions, saved recipes and local results.
use crate::{
    state::AppState,
    worker::{WorkerCommand, WorkerHandle},
};
use analytics_ml::pipeline::{Cancellation, cancellation};
use analytics_workflow::{
    DataVersion, ResearchTemplate, RunRecord, RunStatus, WorkflowRequest, Workspace, WorkspaceLease,
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;
#[derive(Debug)]
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
    pub identity_path: String,
    pub identity_symbol: String,
    pub identity_date: String,
    pub action_review_instrument: String,
    pub action_evidence_manifest: String,
    pub datasets: Vec<DataVersion>,
    pub runs: Vec<RunRecord>,
    pub selected_run: Option<Uuid>,
    pub market: String,
    pub actions: String,
    pub release_tag: String,
    pub register_path: String,
    pub collection_provider: analytics_storage::collector::Provider,
    pub adjustment_source: analytics_storage::adjustment::AdjustmentSource,
    pub collection_symbols: String,
    pub collection_benchmarks: String,
    pub collection_csi300: bool,
    pub collection_csi500: bool,
    pub pool_editor: String,
    pub pool_preview_date: String,
    pub method: String,
    pub start: String,
    pub end: String,
    pub details: Value,
    pub filter: String,
    pub import_run_path: String,
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
            action_review_editor: String::new(),
            identity_path: String::new(),
            identity_symbol: "SHT00018".into(),
            identity_date: "2005-06-01".into(),
            action_review_instrument: "SH600000".into(),
            action_evidence_manifest: String::new(),
            datasets: vec![],
            runs: vec![],
            selected_run: None,
            market: String::new(),
            actions: String::new(),
            release_tag: String::new(),
            register_path: String::new(),
            collection_provider: Default::default(),
            adjustment_source: analytics_storage::adjustment::AdjustmentSource::SinaHfq,
            collection_symbols: "SH600000,SZ000001,SH000300".into(),
            collection_benchmarks: "SH000300".into(),
            collection_csi300: false,
            collection_csi500: false,
            pool_editor: String::new(),
            pool_preview_date: "2026-10-12".into(),
            method: "enrich".into(),
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
    ui.label("更新保存新快照，已有研究继续使用原版本。公司行动缺项在研究前检查。");
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
    ui.collapsing("AKShare / yfinance 日线采集", |ui| {
        ui.add_enabled_ui(enabled, |ui| {
            use analytics_storage::collector::{CollectionRequest, Provider};
            ui.horizontal(|ui| {
                if ui
                    .selectable_value(
                        &mut state.platform.collection_provider,
                        Provider::Akshare,
                        "AKShare（A股默认）",
                    )
                    .clicked()
                {
                    state.platform.adjustment_source =
                        analytics_storage::adjustment::AdjustmentSource::SinaHfq;
                }
                if ui
                    .selectable_value(
                        &mut state.platform.collection_provider,
                        Provider::Yfinance,
                        "yfinance（候选与核验）",
                    )
                    .clicked()
                {
                    state.platform.adjustment_source =
                        analytics_storage::adjustment::AdjustmentSource::None;
                }
            });
            ui.label(match state.platform.adjustment_source {
                analytics_storage::adjustment::AdjustmentSource::SinaHfq => {
                    "复权系数：新浪独立 hfq-factor；原价行情仍来自 AKShare，历史事件须对账"
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
            if ui.button("采集日线并发布新版本").clicked() {
                let parse = || -> Result<CollectionRequest, String> {
                    let ids = |v: &str| {
                        v.split([',', '，', ' ', '\n'])
                            .filter(|s| !s.trim().is_empty())
                            .map(|s| {
                                if analytics_core::security::is_security_id(s.trim()) {
                                    s.trim().to_owned()
                                } else {
                                    s.trim().to_ascii_uppercase()
                                }
                            })
                            .collect()
                    };
                    let mut r = CollectionRequest {
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
                    if !state.platform.template.identity_version.is_empty() {
                        let w = state.platform.workspace.as_ref().ok_or("工作区尚未选择")?;
                        r.bind_identities(
                            &w.identities(&state.platform.template.identity_version)
                                .map_err(|e| e.to_string())?,
                        )
                        .map_err(|e| e.to_string())?;
                    }
                    r.validate().map_err(|e| e.to_string())?;
                    Ok(r)
                };
                match parse() {
                    Ok(r) => {
                        request = Some(WorkflowRequest::CollectMarket {
                            request: r,
                            resume: None,
                        })
                    }
                    Err(e) => state.platform.error = Some(e),
                }
            }
            if ui.button("采集公司行动候选").clicked() {
                let parse = || -> Result<CollectionRequest, String> {
                    let ids = |v: &str| {
                        v.split([',', '，', ' ', '\n'])
                            .filter(|s| !s.trim().is_empty())
                            .map(|s| {
                                if analytics_core::security::is_security_id(s.trim()) {
                                    s.trim().to_owned()
                                } else {
                                    s.trim().to_ascii_uppercase()
                                }
                            })
                            .collect::<std::collections::BTreeSet<_>>()
                    };
                    let instruments = ids(&state.platform.collection_symbols)
                        .difference(&ids(&state.platform.collection_benchmarks))
                        .cloned()
                        .collect();
                    let mut r = CollectionRequest {
                        source_bindings: Default::default(),
                        provider: state.platform.collection_provider,
                        adjustment_source: state.platform.adjustment_source,
                        start: state.platform.start.parse().map_err(|_| "开始日期无效")?,
                        end: state.platform.end.parse().map_err(|_| "结束日期无效")?,
                        instruments,
                        benchmarks: Default::default(),
                        universes: Default::default(),
                    };
                    if !state.platform.template.identity_version.is_empty() {
                        let w = state.platform.workspace.as_ref().ok_or("工作区尚未选择")?;
                        r.bind_identities(
                            &w.identities(&state.platform.template.identity_version)
                                .map_err(|e| e.to_string())?,
                        )
                        .map_err(|e| e.to_string())?;
                    }
                    r.validate().map_err(|e| e.to_string())?;
                    Ok(r)
                };
                match parse() {
                    Ok(r) => {
                        request = Some(WorkflowRequest::CollectActions {
                            request: r,
                            resume: None,
                        })
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
    ui.horizontal(|ui| {
        ui.label("公司行动版本");
        select_data(
            ui,
            "补充动作选择",
            "actions",
            &mut state.platform.actions,
            &state.platform.datasets,
        );
        egui::ComboBox::from_id_salt("supplement_method")
            .selected_text(method_label(&state.platform.method))
            .show_ui(ui, |ui| {
                for m in [
                    "enrich",
                    "details",
                    "payments",
                    "retry",
                    "repair",
                    "repair-payments",
                ] {
                    ui.selectable_value(&mut state.platform.method, m.into(), method_label(m));
                }
            });
    });
    ui.horizontal(|ui| {
        ui.label("补充区间");
        ui.add(egui::TextEdit::singleline(&mut state.platform.start).desired_width(120.0));
        ui.label("至");
        ui.add(egui::TextEdit::singleline(&mut state.platform.end).desired_width(120.0));
        if ui
            .add_enabled(
                enabled && !state.platform.actions.is_empty(),
                egui::Button::new("执行公司行动补充"),
            )
            .clicked()
        {
            match state
                .platform
                .start
                .parse()
                .and_then(|start| state.platform.end.parse().map(|end| (start, end)))
            {
                Ok((start, end)) if start <= end => {
                    request = Some(WorkflowRequest::SupplementActions {
                        actions: state.platform.actions.clone(),
                        method: state.platform.method.clone(),
                        start,
                        end,
                        resume: state.platform.details["resume_supplement"]
                            .as_str()
                            .and_then(|s| s.parse().ok()),
                    })
                }
                _ => state.platform.error = Some("请填写有效的起止日期 YYYY-MM-DD".into()),
            }
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
fn method_label(method: &str) -> &str {
    match method {
        "enrich" => "新浪历史与上市日",
        "details" => "实施详情补充",
        "payments" => "现金派送日补充",
        "retry" => "重试失败证券",
        "repair" => "重解析本项目历史证据",
        "repair-payments" => "重解析本项目现金证据",
        _ => method,
    }
}
pub fn research_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
    registry: &analytics_factor::FactorRegistry,
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
    let w = state.platform.workspace.as_ref().unwrap().clone();
    let result = (|| -> analytics_workflow::Result<()> {
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
    ui.collapsing("证券身份、别名与承继证据",|ui|{
        ui.label("同一交易代码可能先后属于不同证券。身份目录保存主体、有效日期和证据；历史行情缺失仍会阻止研究。");
        ui.add_enabled_ui(enabled,|ui|{
            ui.horizontal(|ui|{ui.label("身份目录版本");ui.text_edit_singleline(&mut state.platform.template.identity_version);});
            ui.horizontal(|ui|{ui.label("输入JSON（采集地址／身份目录）或导出目录");ui.text_edit_singleline(&mut state.platform.identity_path);});
            ui.horizontal(|ui|{ui.label("来源别名");ui.text_edit_singleline(&mut state.platform.identity_symbol);ui.label("有效日期");ui.text_edit_singleline(&mut state.platform.identity_date);});
            let mut operation=None;
            ui.horizontal(|ui|{
                for (label,method) in [("列出身份版本","list"),("查看身份与证据","show"),("核验并登记新版本","review"),("导出完整证据包","export"),("按日期查别名","check")] {if ui.button(label).clicked(){operation=Some(method);}}
            });
            if let Some(method)=operation {let date=if method=="check" {match state.platform.identity_date.parse(){Ok(d)=>Some(d),Err(_)=>{state.platform.error=Some("身份查询日期无效".into());return;}}}else{None};
                submit(state,worker,cancel_slot,WorkflowRequest::IdentityOperation{method:method.into(),id:state.platform.template.identity_version.clone(),path:PathBuf::from(state.platform.identity_path.trim()),provider:"qlib".into(),symbol:state.platform.identity_symbol.clone(),date});
            }
        });
        if !state.platform.details.is_null(){ui.label("核验结果与证据明细见下方任务结果；登记后的版本ID填入研究模板。");}
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
                if ui.button("自选池示例").clicked(){state.platform.pool_editor=serde_json::json!({"schema_version":2,"name":"自选池","coverage":{"start":state.platform.start,"end":state.platform.end},"expression":{"kind":"symbols","instruments":["SH600000","SZ000001"]},"adjustments":[]}).to_string();}
                if ui.button("指数组合示例").clicked(){state.platform.pool_editor=serde_json::json!({"schema_version":2,"name":"沪深300与中证500并集","coverage":{"start":state.platform.start,"end":state.platform.end},"expression":{"kind":"union","inputs":[{"kind":"universe","version":state.platform.market,"universe":"csi300"},{"kind":"universe","version":state.platform.market,"universe":"csi500"}]},"adjustments":[]}).to_string();}
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
            ui.label("从运行结果查看原值、候选字段及公告。修订JSON必须包含原事件哈希、逐字段原文引用及理由。");
            ui.add(egui::TextEdit::multiline(&mut state.platform.action_review_editor).desired_rows(8).desired_width(f32::INFINITY));
            if ui.button("提交证据修订为新版本").clicked(){match serde_json::from_str(&state.platform.action_review_editor){Ok(request)=>submit(state,worker,cancel_slot,WorkflowRequest::ReviseActions{actions:state.platform.actions.clone(),request}),Err(e)=>state.platform.error=Some(e.to_string())}}
        });
    });
    ui.add_enabled_ui(enabled, |ui| {
            ui.horizontal(|ui|{ui.label("证券");ui.text_edit_singleline(&mut state.platform.action_review_instrument);ui.label("公告manifest");ui.text_edit_singleline(&mut state.platform.action_evidence_manifest);});
            if ui.button("加载原事件、哈希与公告候选").clicked(){submit(state,worker,cancel_slot,WorkflowRequest::InspectActions{actions:state.platform.actions.clone(),instrument:state.platform.action_review_instrument.clone(),evidence_manifest:if state.platform.action_evidence_manifest.trim().is_empty(){None}else{Some(PathBuf::from(state.platform.action_evidence_manifest.trim()))}});}
        let t = &mut state.platform.template;
        crate::quality_ui::controls(ui, &mut t.quality_policy);
        ui.collapsing("日期调度：单次 / 滚动 / 手动多折", |ui| {
            let before=serde_json::to_string(&t.schedule).unwrap_or_default();
            crate::schedule_ui::draw(ui,&mut t.schedule);
            if before!=serde_json::to_string(&t.schedule).unwrap_or_default(){state.platform.schedule_editor.clear();}
            if state.platform.schedule_editor.is_empty(){state.platform.schedule_editor=serde_json::to_string_pretty(&t.schedule).unwrap_or_default();}
            ui.label("任意train / validation（可为null）/ test日期；滚动支持days/months/years/trading_days及expanding。");
            ui.add(egui::TextEdit::multiline(&mut state.platform.schedule_editor).desired_rows(14).desired_width(f32::INFINITY));
            if ui.button("应用日期设置").clicked(){match serde_json::from_str::<analytics_core::schedule::Schedule>(&state.platform.schedule_editor){Ok(s)=>match s.validate(){Ok(())=>t.schedule=s,Err(e)=>state.platform.error=Some(e)},Err(e)=>state.platform.error=Some(e.to_string())}}
            ui.checkbox(&mut t.action_policy.subscribe_with_available_cash,"配股：使用可用现金认购（不勾选则明确放弃）");
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
                        (CommissionMode::Net, "净佣"),
                        (CommissionMode::AllInclusive, "含过户费全佣"),
                    ] {
                        ui.selectable_value(&mut t.market_rules.commission.mode, mode, label);
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.label("固定研究股息扣税比例");
            ui.add(
                egui::DragValue::new(&mut t.dividend_withholding_rate)
                    .speed(0.01)
                    .range(0.0..=1.0),
            );
            ui.label("默认0为税前；不模拟个人持股期税制");
        });
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
    ui.label("不复权成交、整数股、T+1、跨年持续账户；公司行动不完整时停止训练。");
    let ready = enabled && !state.platform.market.is_empty() && !state.platform.actions.is_empty();
    let mut run = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(ready, egui::Button::new("检查研究输入"))
            .clicked()
        {
            run = Some(true);
        }
        if ui
            .add_enabled(ready, egui::Button::new("执行年度研究"))
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
                    actions: state.platform.actions.clone(),
                    audit_only,
                },
            ),
            Err(e) => state.platform.error = Some(e.to_string()),
        }
    }
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
                    if ui.button("恢复Python行情采集").clicked() {
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
                Ok(WorkflowRequest::CollectActions { request, resume }) => {
                    if ui.button("恢复Python公司行动采集").clicked() {
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
                Ok(WorkflowRequest::SupplementActions {
                    actions,
                    method,
                    start,
                    end,
                    resume,
                }) if method != "retry" => {
                    if ui.button("准备恢复日期补充").clicked() {
                        state.platform.actions = actions;
                        state.platform.method = method;
                        state.platform.start = start.to_string();
                        state.platform.end = end.to_string();
                        state.platform.details =
                            serde_json::json!({"resume_supplement":resume.unwrap_or(r.id)});
                        state.panel = crate::state::Panel::Dataset;
                    }
                }
                _ => {
                    ui.label("研究需重新执行，首版不恢复训练中途状态。");
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
    if value["ready"] == false {
        ui.colored_label(
            crate::theme::semantic(ui.ctx()).warning,
            "数据未就绪，未进行训练与回测。",
        );
    }
    if value["input_kind"] == "synthetic_fixture" {
        ui.colored_label(
            crate::theme::semantic(ui.ctx()).warning,
            "合成演示数据，仅验证流程与账本，不代表真实市场收益。",
        );
    }
    if value.get("aggregate").is_some() {
        if let Err(error) = analytics_ml::backtest::validate_result(value) {
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
                        let cost = r["performance"]["fee_totals_cny"]
                            .as_object()
                            .map(|o| o.values().filter_map(Value::as_f64).sum::<f64>())
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
