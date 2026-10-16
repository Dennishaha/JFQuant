//! Panel rendering for the four workbenches.
//!
//! Each function is a thin egui view over [`AppState`]: it renders current
//! state, validates form input through the parsers in [`crate::state`], and
//! hands work to the background worker through [`WorkerCommand`]. No DuckDB
//! or LightGBM handle ever appears here.

use analytics_core::{ExportFormat, ExportRequest, QueryPage};
use analytics_factor::FactorRegistry;
use analytics_storage::validate_required_market_columns;
use analytics_workflow::research::pipeline::{Cancellation, cancellation};
use egui::{Color32, RichText, ScrollArea};
use std::path::PathBuf;
use uuid::Uuid;

use crate::state::{AppState, Panel, apply_event, data_type_name, normalize_sql, scalar_text};
use crate::theme;
use crate::worker::{RunSummary, WorkerCommand, WorkerEvent, WorkerHandle};

/// Renders the active central panel.
pub fn render(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
    registry: &FactorRegistry,
) {
    if let Some(message) = &state.worker_error {
        let color = theme::semantic(ui.ctx()).error;
        egui::Frame::new()
            .stroke(egui::Stroke::new(1.0, color))
            .inner_margin(10)
            .show(ui, |ui| {
                ui.colored_label(color, RichText::new("后台不可用").strong());
                ui.add(egui::Label::new(message).wrap());
                ui.label("点击顶部的“重启后台”，然后重新执行操作。");
            });
        ui.add_space(8.0);
    }
    if state.platform.require_workspace && state.platform.workspace.is_none() {
        ui.heading("欢迎使用JFQuant");
        ui.label("请在顶部选择或创建工作区，然后更新数据并运行研究。");
        return;
    }
    match state.panel {
        Panel::Dataset => {
            crate::platform::data_panel(ui, state, worker, cancel_slot);
            dataset_panel(ui, state, worker)
        }
        Panel::Query => query_panel(ui, state, worker),
        Panel::Factor => {
            ScrollArea::vertical()
                .id_salt("factor_workbench")
                .show(ui, |ui| {
                    factor_panel(ui, state, worker, cancel_slot, registry);
                });
        }
        Panel::Model => model_panel(ui, state, worker, cancel_slot),
        Panel::Research => {
            crate::platform::research_panel(ui, state, worker, cancel_slot, registry)
        }
        Panel::Runs => crate::platform::runs_panel(ui, state, worker, cancel_slot),
    }
    if state.worker_error.is_some() {
        *cancel_slot = None;
    }
}

pub(crate) fn send_command(state: &mut AppState, worker: &WorkerHandle, command: WorkerCommand) {
    let (task, kind) = command.task_info();
    let query = match &command {
        WorkerCommand::RunQuery { request, .. } => Some(request.clone()),
        _ => None,
    };
    if let Err(message) = worker.send(command) {
        apply_event(state, &WorkerEvent::Unavailable { message });
    } else {
        if let Some(request) = query {
            state.query.submitted(request);
        }
        apply_event(
            state,
            &WorkerEvent::Started {
                task,
                kind,
                label: "等待后台处理…".into(),
            },
        );
    }
}

fn primary_button(label: &str, accent: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(accent)
}

fn inline_error(ui: &mut egui::Ui, error: &Option<String>) {
    if let Some(message) = error {
        ui.add_space(4.0);
        ui.colored_label(theme::semantic(ui.ctx()).error, message);
    }
}

fn query_error(ui: &mut egui::Ui, error: &Option<String>) {
    let Some(message) = error else { return };
    let color = theme::semantic(ui.ctx()).error;
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(color.gamma_multiply(0.08))
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(6)
        .inner_margin(10)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.colored_label(color, RichText::new("查询操作失败").strong());
            if message.contains("No files found") {
                ui.label("未找到匹配的 Parquet 文件，请检查 SQL 中的路径，或先准备行情数据。");
            }
            ScrollArea::vertical()
                .id_salt("query_error_details")
                .max_height(140.0)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(message).wrap().selectable(true));
                });
        });
}

fn dataset_panel(ui: &mut egui::Ui, state: &mut AppState, worker: &WorkerHandle) {
    theme::section_heading(
        ui,
        "本地 Parquet 数据集",
        "扫描目录读取 footer 元数据；选中文件查看 schema 与行情契约校验。",
    );
    ui.add_space(8.0);
    let busy = state.busy() || state.worker_error.is_some();
    ui.horizontal(|ui| {
        ui.label("路径");
        ui.add(egui::TextEdit::singleline(&mut state.dataset.root).desired_width(420.0));
        let scan = ui
            .add_enabled(
                !busy,
                primary_button("扫描数据集", theme::semantic(ui.ctx()).accent),
            )
            .clicked();
        if scan {
            let root = state.dataset.root.trim().to_string();
            if root.is_empty() {
                state.dataset.error = Some("路径不能为空".into());
            } else {
                state.dataset.error = None;
                send_command(
                    state,
                    worker,
                    WorkerCommand::ScanDataset {
                        task: Uuid::new_v4(),
                        root: PathBuf::from(root),
                    },
                );
            }
        }
    });
    inline_error(ui, &state.dataset.error);
    ui.add_space(8.0);

    let files = &state.dataset.files;
    if files.is_empty() {
        ui.weak("尚未扫描。相对路径从所选工作区解析，默认目录为 data/market。");
        return;
    }
    let total_rows: u64 = files.iter().filter_map(|file| file.rows).sum();
    ui.label(format!("{} 个文件 · 共 {} 行", files.len(), total_rows));
    ui.add_space(4.0);
    let height = (ui.available_height() * 0.45).clamp(120.0, 320.0);
    ScrollArea::vertical().max_height(height).show(ui, |ui| {
        egui::Grid::new("dataset_files")
            .striped(true)
            .num_columns(3)
            .show(ui, |ui| {
                ui.strong("文件");
                ui.strong("行数");
                ui.strong("列数");
                ui.end_row();
                for (index, file) in state.dataset.files.iter().enumerate() {
                    let name = file.path.display().to_string();
                    let selected = state.dataset.selected == Some(index);
                    if ui
                        .selectable_label(selected, egui::RichText::new(name).monospace())
                        .clicked()
                    {
                        state.dataset.selected = Some(index);
                    }
                    ui.monospace(
                        file.rows
                            .map(|rows| rows.to_string())
                            .unwrap_or_else(|| "—".into()),
                    );
                    ui.monospace(file.schema.len().to_string());
                    ui.end_row();
                }
            });
    });

    if let Some(index) = state.dataset.selected
        && let Some(file) = state.dataset.files.get(index)
    {
        ui.add_space(8.0);
        ui.separator();
        ui.strong("Schema");
        let validation = validate_required_market_columns(&file.schema);
        let colors = theme::semantic(ui.ctx());
        match &validation {
            Ok(()) => theme::status_dot(ui, colors.success, "满足行情数据契约"),
            Err(error) => {
                theme::status_dot(ui, colors.warning, &format!("非标准行情表：{error}"));
            }
        }
        ui.add_space(4.0);
        ScrollArea::both().max_height(200.0).show(ui, |ui| {
            egui::Grid::new("dataset_schema")
                .striped(true)
                .num_columns(3)
                .show(ui, |ui| {
                    ui.strong("列名");
                    ui.strong("类型");
                    ui.strong("可空");
                    ui.end_row();
                    for column in &file.schema {
                        ui.monospace(&column.name);
                        ui.monospace(data_type_name(&column.data_type));
                        ui.monospace(if column.nullable { "yes" } else { "no" });
                        ui.end_row();
                    }
                });
        });
    }
}

fn query_panel(ui: &mut egui::Ui, state: &mut AppState, worker: &WorkerHandle) {
    theme::section_heading(
        ui,
        "SQL 查询工作台",
        "DuckDB 方言；默认按结果列排序，使用隐藏行号分页，导出支持 Parquet 与 CSV。",
    );
    query_error(ui, &state.query.error);
    ui.add_space(8.0);
    ui.add(
        egui::TextEdit::multiline(&mut state.query.sql)
            .desired_rows(6)
            .desired_width(f32::INFINITY)
            .code_editor(),
    );
    ui.add_space(6.0);
    let busy = state.busy() || state.worker_error.is_some();
    let accent = theme::semantic(ui.ctx()).accent;
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(!busy, primary_button("执行查询", accent))
            .clicked()
        {
            match state.query.first_page_request() {
                Ok(request) => {
                    state.query.error = None;
                    state.query.page = 0;
                    send_command(
                        state,
                        worker,
                        WorkerCommand::RunQuery {
                            task: Uuid::new_v4(),
                            request,
                        },
                    );
                }
                Err(error) => state.query.error = Some(error),
            }
        }
        if ui
            .add_enabled(!busy, egui::Button::new("Describe"))
            .clicked()
        {
            match normalize_sql(&state.query.sql) {
                Ok(sql) => send_command(
                    state,
                    worker,
                    WorkerCommand::DescribeQuery {
                        task: Uuid::new_v4(),
                        sql,
                    },
                ),
                Err(error) => state.query.error = Some(error),
            }
        }
        ui.separator();
        ui.label("每页");
        egui::ComboBox::from_id_salt("query_limit")
            .selected_text(state.query.limit.to_string())
            .show_ui(ui, |ui| {
                for limit in [100_u32, 500, 2000] {
                    ui.selectable_value(&mut state.query.limit, limit, limit.to_string());
                }
            });
        ui.label("排序列");
        let selected = state
            .query
            .sort_column
            .clone()
            .unwrap_or_else(|| "（未选择）".into());
        egui::ComboBox::from_id_salt("query_sort")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut state.query.sort_column, None, "（未选择）");
                for column in &state.query.columns {
                    ui.selectable_value(
                        &mut state.query.sort_column,
                        Some(column.name.clone()),
                        &column.name,
                    );
                }
            });
        ui.checkbox(&mut state.query.descending, "降序");
        let next = state.query.next_page_request();
        if ui
            .add_enabled(next.is_some() && !busy, egui::Button::new("下一页"))
            .clicked()
            && let Some(request) = next
        {
            send_command(
                state,
                worker,
                WorkerCommand::RunQuery {
                    task: Uuid::new_v4(),
                    request,
                },
            );
        }
    });
    if state.query.next_cursor.is_some() && state.query.next_page_request().is_none() {
        ui.weak("查询条件已改变，请重新执行查询后再翻页。");
    }
    ui.horizontal_wrapped(|ui| {
        ui.label("导出到");
        ui.add(egui::TextEdit::singleline(&mut state.query.export_path).desired_width(320.0));
        egui::ComboBox::from_id_salt("export_format")
            .selected_text(if state.query.export_csv {
                "CSV"
            } else {
                "Parquet"
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut state.query.export_csv, false, "Parquet");
                ui.selectable_value(&mut state.query.export_csv, true, "CSV");
            });
        if ui.add_enabled(!busy, egui::Button::new("导出")).clicked() {
            match normalize_sql(&state.query.sql) {
                Ok(sql) if !state.query.export_path.trim().is_empty() => {
                    send_command(
                        state,
                        worker,
                        WorkerCommand::ExportQuery {
                            task: Uuid::new_v4(),
                            request: ExportRequest {
                                sql,
                                destination: PathBuf::from(state.query.export_path.trim()),
                                format: if state.query.export_csv {
                                    ExportFormat::Csv
                                } else {
                                    ExportFormat::Parquet
                                },
                                compression: None,
                            },
                        },
                    );
                }
                Ok(_) => state.query.error = Some("导出路径不能为空".into()),
                Err(error) => state.query.error = Some(error),
            }
        }
    });

    if let Some(columns) = &state.query.described {
        ui.add_space(6.0);
        ui.label(RichText::new("结果结构").strong());
        ui.horizontal_wrapped(|ui| {
            for column in columns {
                ui.monospace(format!(
                    "{}: {}",
                    column.name,
                    data_type_name(&column.data_type)
                ));
            }
        });
    }

    if !state.query.rows.is_empty() {
        ui.add_space(8.0);
        ui.label(format!(
            "结果 · 第 {} 页 · 累计 {} 行{}",
            state.query.page,
            state.query.rows.len(),
            if state.query.next_cursor.is_some() {
                " · 还有下一页"
            } else {
                ""
            }
        ));
        ui.add_space(4.0);
        let height = (ui.available_height() - 24.0).max(160.0);
        ScrollArea::both().max_height(height).show(ui, |ui| {
            egui::Grid::new("query_results")
                .striped(true)
                .show(ui, |ui| {
                    for column in &state.query.columns {
                        ui.strong(&column.name);
                    }
                    ui.end_row();
                    for row in &state.query.rows {
                        for value in row {
                            ui.monospace(scalar_text(value));
                        }
                        ui.end_row();
                    }
                });
        });
    }
}

fn factor_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
    _registry: &FactorRegistry,
) {
    theme::section_heading(
        ui,
        "因子组与有效性测试",
        "管理158/191因子子集，独立检验覆盖率、IC和分层收益，也可运行建模流水线。",
    );
    crate::factor_groups::controls(ui, state);
    let readonly = state
        .factor
        .group
        .as_ref()
        .is_some_and(analytics_workflow::factor_groups::FactorGroup::is_builtin);
    ui.add_enabled_ui(!readonly, |ui| {
        let previous = state.factor.factor_set;
        egui::ComboBox::from_id_salt("pipeline_factor_set")
            .selected_text(previous.label())
            .show_ui(ui, |ui| {
                for family in [
                    analytics_core::research_quality::FactorSet::Gtja191,
                    analytics_core::research_quality::FactorSet::Alpha158,
                ] {
                    ui.selectable_value(
                        &mut state.factor.factor_set,
                        family,
                        format!("{family:?}"),
                    );
                }
            });
        // Family selects the visible builtin catalogue; mixed selections are retained.
    });
    crate::custom_factors::editor(ui, state, worker, cancel_slot);
    let registry = FactorRegistry::for_set(state.factor.factor_set);
    ui.add_space(8.0);
    egui::Grid::new("factor_form")
        .num_columns(2)
        .show(ui, |ui| {
            ui.label("行情路径");
            ui.add(egui::TextEdit::singleline(&mut state.factor.market_path).desired_width(360.0));
            ui.end_row();
            ui.label("股票池路径");
            ui.add(
                egui::TextEdit::singleline(&mut state.factor.universe_path).desired_width(360.0),
            );
            ui.end_row();
            ui.label("股票池 ID");
            ui.add(egui::TextEdit::singleline(&mut state.factor.universe_id).desired_width(160.0));
            ui.end_row();
            ui.label("日期区间");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.factor.date_start).desired_width(110.0),
                );
                ui.label("至");
                ui.add(egui::TextEdit::singleline(&mut state.factor.date_end).desired_width(110.0));
            });
            ui.end_row();
            ui.label("输出目录");
            if state.platform.workspace.is_some() {
                ui.label(".jfquant/runs/任务编号（自动创建）");
            } else {
                ui.add(
                    egui::TextEdit::singleline(&mut state.factor.output_dir).desired_width(360.0),
                );
            }
            ui.end_row();
        });
    ui.add_space(8.0);

    ui.horizontal(|ui| {
        ui.label(format!(
            "因子选择（已选 {} / {} 可用，{} 项隔离）",
            state.factor.selected.len(),
            registry.len()
                - registry.unsupported_ids().len()
                - usize::from(registry.get("alpha_030").is_some()),
            registry.unsupported_ids().len()
        ));
        ui.add(
            egui::TextEdit::singleline(&mut state.factor.filter)
                .desired_width(200.0)
                .hint_text("按 ID 或公式检索"),
        );
        if ui
            .add_enabled(!readonly, egui::Button::new("全选筛选结果"))
            .clicked()
        {
            for spec in registry.all() {
                if state.factor.matches_filter(&spec.id, &spec.formula)
                    && spec.id != "alpha_030"
                    && !matches!(
                        spec.expression,
                        analytics_factor::FactorExpr::Unsupported { .. }
                    )
                {
                    state.factor.selected.insert(spec.id.clone());
                }
            }
        }
        if ui
            .add_enabled(!readonly, egui::Button::new("清空"))
            .clicked()
        {
            state.factor.selected.clear();
        }
    });
    ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
        egui::Grid::new("factor_picker")
            .striped(true)
            .num_columns(2)
            .show(ui, |ui| {
                for spec in registry.all() {
                    if !state.factor.matches_filter(&spec.id, &spec.formula) {
                        continue;
                    }
                    let mut checked = state.factor.selected.contains(&spec.id);
                    let unsupported = spec.id == "alpha_030"
                        || matches!(
                            spec.expression,
                            analytics_factor::FactorExpr::Unsupported { .. }
                        );
                    if unsupported {
                        state.factor.selected.remove(&spec.id);
                        checked = false;
                    }
                    let response = ui.add_enabled(
                        !unsupported && !readonly,
                        egui::Checkbox::new(
                            &mut checked,
                            egui::RichText::new(&spec.id).monospace(),
                        ),
                    );
                    if response.changed() {
                        if checked {
                            state.factor.selected.insert(spec.id.clone());
                        } else {
                            state.factor.selected.remove(&spec.id);
                        }
                    }
                    if spec.id == "alpha_030" {
                        ui.label(
                            egui::RichText::new("缺少MKT/SMB/HML输入，暂不可选")
                                .small()
                                .weak(),
                        );
                    } else if let analytics_factor::FactorExpr::Unsupported { formula: reason } =
                        &spec.expression
                    {
                        ui.label(
                            egui::RichText::new(format!("已隔离：{reason}"))
                                .small()
                                .weak(),
                        );
                    } else {
                        ui.label(egui::RichText::new(&spec.formula).small().weak())
                            .on_hover_text(&spec.formula);
                    }
                    ui.end_row();
                }
            });
    });
    ui.add_space(8.0);

    let busy = state.busy() || state.worker_error.is_some();
    ui.horizontal(|ui| {
        ui.label("未来交易日");
        ui.add(egui::DragValue::new(&mut state.factor.horizon).range(1..=252));
        ui.label("分层数");
        ui.add(egui::DragValue::new(&mut state.factor.quantiles).range(2..=10));
        ui.label("每日最少样本");
        ui.add(egui::DragValue::new(&mut state.factor.min_samples).range(3..=10000));
    });
    if ui
        .add_enabled(
            !busy,
            primary_button("因子有效性测试", theme::semantic(ui.ctx()).accent),
        )
        .clicked()
    {
        match run_pipeline_request(state) {
            Ok(mut input) => {
                // Standalone runs also get a fresh destination; never overwrite previous diagnostics.
                let task = Uuid::new_v4();
                if state.platform.workspace.is_none() {
                    input.output_dir = input.output_dir.join(task.to_string());
                }
                let request = analytics_workflow::factor_evaluation::EvaluationRequest {
                    input: input.into(),
                    horizon: state.factor.horizon,
                    quantiles: state.factor.quantiles,
                    min_samples: state.factor.min_samples,
                };
                let flag = cancellation();
                *cancel_slot = Some(flag.clone());
                state.factor.error = None;
                send_command(
                    state,
                    worker,
                    WorkerCommand::EvaluateFactors {
                        task,
                        request,
                        cancel: flag,
                    },
                );
            }
            Err(e) => state.factor.error = Some(e),
        }
    }
    crate::factor_groups::report(ui, state);
    if ui
        .add_enabled(
            !busy,
            primary_button("运行因子流水线", theme::semantic(ui.ctx()).accent),
        )
        .clicked()
    {
        match run_pipeline_request(state) {
            Ok(request) => {
                state.factor.error = None;
                let flag = cancellation();
                *cancel_slot = Some(flag.clone());
                send_command(
                    state,
                    worker,
                    WorkerCommand::RunPipeline {
                        task: Uuid::new_v4(),
                        request,
                        cancel: flag,
                    },
                );
            }
            Err(error) => state.factor.error = Some(error),
        }
    }
    inline_error(ui, &state.factor.error);
    task_log(ui, state, crate::worker::TaskKind::Pipeline);
    if let Some(summary) = &state.factor.summary {
        run_summary_view(ui, summary);
    }
}

fn run_pipeline_request(
    state: &AppState,
) -> Result<analytics_workflow::research::pipeline::PipelineRequest, String> {
    let factor = &state.factor;
    let date_range = factor.date_range()?;
    if factor.market_path.trim().is_empty() || factor.universe_path.trim().is_empty() {
        return Err("行情与股票池路径不能为空".into());
    }
    if factor.output_dir.trim().is_empty() {
        return Err("输出目录不能为空".into());
    }
    let factor_ids = factor.selected_ids();
    if factor_ids.is_empty() {
        return Err("请至少选择一个因子".into());
    }
    Ok(analytics_workflow::research::pipeline::PipelineRequest {
        factor_refs: factor.selected_refs()?,
        factor_set: factor.factor_set,
        market_path: PathBuf::from(factor.market_path.trim()),
        universe_path: PathBuf::from(factor.universe_path.trim()),
        universe_id: factor.universe_id.trim().to_string(),
        date_range,
        factor_ids,
        output_dir: PathBuf::from(factor.output_dir.trim()),
        training: analytics_workflow::research::TrainingConfig::default(),
    })
}

fn model_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
) {
    theme::section_heading(
        ui,
        "LightGBM 建模",
        "从既有因子缓存以新超参重训，展示指标、特征重要性与预测预览。",
    );
    ui.add_space(8.0);
    egui::Grid::new("model_form").num_columns(2).show(ui, |ui| {
        ui.label("因子缓存");
        ui.add(egui::TextEdit::singleline(&mut state.model.cache_path).desired_width(360.0));
        ui.end_row();
        ui.label("输出目录");
        if state.platform.workspace.is_some() {
            ui.label(".jfquant/runs/任务编号（自动创建）");
        } else {
            ui.add(egui::TextEdit::singleline(&mut state.model.output_dir).desired_width(360.0));
        }
        ui.end_row();
        ui.label("股票池 ID");
        ui.add(
            egui::TextEdit::singleline(&mut state.model.universe_id)
                .desired_width(160.0)
                .hint_text("留空沿用缓存"),
        );
        ui.end_row();
        ui.label("num_iterations");
        ui.add(egui::TextEdit::singleline(&mut state.model.num_iterations).desired_width(100.0));
        ui.end_row();
        ui.label("learning_rate");
        ui.add(egui::TextEdit::singleline(&mut state.model.learning_rate).desired_width(100.0));
        ui.end_row();
        ui.label("num_leaves");
        ui.add(egui::TextEdit::singleline(&mut state.model.num_leaves).desired_width(100.0));
        ui.end_row();
        ui.label("min_data_in_leaf");
        ui.add(egui::TextEdit::singleline(&mut state.model.min_data_in_leaf).desired_width(100.0));
        ui.end_row();
        ui.label("early_stopping");
        ui.add(
            egui::TextEdit::singleline(&mut state.model.early_stopping)
                .desired_width(100.0)
                .hint_text("留空关闭"),
        );
        ui.end_row();
    });
    ui.add_space(8.0);

    let busy = state.busy() || state.worker_error.is_some();
    if ui
        .add_enabled(
            !busy,
            primary_button("开始训练", theme::semantic(ui.ctx()).accent),
        )
        .clicked()
    {
        let cache_path = state.model.cache_path.trim().to_string();
        let output_dir = state.model.output_dir.trim().to_string();
        let universe_id = state.model.universe_id.trim().to_string();
        match (
            cache_path.is_empty(),
            output_dir.is_empty(),
            state.model.training_config(),
        ) {
            (true, _, _) => state.model.error = Some("因子缓存路径不能为空".into()),
            (_, true, _) => state.model.error = Some("输出目录不能为空".into()),
            (_, _, Err(error)) => state.model.error = Some(error),
            (_, _, Ok(config)) => {
                state.model.error = None;
                let flag = cancellation();
                *cancel_slot = Some(flag.clone());
                send_command(
                    state,
                    worker,
                    WorkerCommand::TrainFromCache {
                        task: Uuid::new_v4(),
                        cache_path: PathBuf::from(cache_path),
                        universe_id: if universe_id.is_empty() {
                            None
                        } else {
                            Some(universe_id)
                        },
                        output_dir: PathBuf::from(output_dir),
                        config,
                        cancel: flag,
                    },
                );
            }
        }
    }
    inline_error(ui, &state.model.error);
    task_log(ui, state, crate::worker::TaskKind::Training);
    if let Some(summary) = &state.model.summary {
        run_summary_view(ui, summary);
    }
}

fn task_log(ui: &mut egui::Ui, state: &AppState, kind: crate::worker::TaskKind) {
    let Some(task) = &state.active_task else {
        return;
    };
    if task.kind != kind {
        return;
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.spinner();
        ui.strong(&task.label);
    });
    egui::Frame::group(ui.style())
        .fill(theme::semantic(ui.ctx()).badge_fill)
        .show(ui, |ui| {
            ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                for line in task.log.iter().rev().take(12).rev() {
                    ui.monospace(line);
                }
            });
        });
}

fn run_summary_view(ui: &mut egui::Ui, summary: &RunSummary) {
    ui.add_space(8.0);
    ui.separator();
    ui.strong("运行产物");
    ui.add_space(4.0);
    egui::Grid::new("run_artifacts")
        .num_columns(2)
        .show(ui, |ui| {
            let mut row = |label: &str, value: String| {
                ui.label(label);
                ui.monospace(value);
                ui.end_row();
            };
            if let Some(path) = &summary.artifact.factor_cache_path {
                row("因子缓存", path.display().to_string());
            }
            if let Some(path) = &summary.artifact.factor_manifest_path {
                row("因子清单", path.display().to_string());
            }
            row(
                "模型",
                summary.artifact.training.model_path.display().to_string(),
            );
            row(
                "模型清单",
                summary
                    .artifact
                    .training
                    .manifest_path
                    .display()
                    .to_string(),
            );
            row(
                "预测",
                summary.artifact.predictions_path.display().to_string(),
            );
            row(
                "拆分行数",
                format!(
                    "train {} / valid {} / test {}",
                    summary.artifact.train_rows,
                    summary.artifact.validation_rows,
                    summary.artifact.test_rows
                ),
            );
        });
    if let Some(metrics) = &summary.metrics_json {
        metric_cards(ui, metrics);
    }
    if !summary.importance.is_empty() {
        importance_bars(ui, &summary.importance);
    }
    if let Some(preview) = &summary.preview {
        preview_table(ui, preview);
    }
}

fn metric_cards(ui: &mut egui::Ui, metrics_json: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(metrics_json) else {
        return;
    };
    ui.add_space(8.0);
    let iterations = value["iterations"].as_u64().unwrap_or_default();
    let best = value["best_iteration"].as_u64().unwrap_or_default();
    let rmse = value["test_rmse"]
        .as_f64()
        .map(|rmse| format!("{rmse:.6}"))
        .unwrap_or_else(|| "—".into());
    ui.horizontal(|ui| {
        for (label, metric) in [
            ("迭代轮数", iterations.to_string()),
            ("最佳迭代", best.to_string()),
            ("测试 RMSE", rmse),
        ] {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(label).small().weak());
                    ui.label(RichText::new(metric).monospace().size(18.0));
                });
            });
        }
    });
}

fn importance_bars(ui: &mut egui::Ui, importance: &[(String, f64)]) {
    ui.add_space(8.0);
    ui.strong("特征重要性（gain）");
    let max = importance
        .iter()
        .map(|(_, gain)| *gain)
        .fold(0.0_f64, f64::max)
        .max(1e-12);
    egui::Grid::new("importance").num_columns(2).show(ui, |ui| {
        for (name, gain) in importance.iter().take(12) {
            ui.monospace(name);
            ui.add(
                egui::ProgressBar::new((*gain / max) as f32)
                    .desired_width(220.0)
                    .text(format!("{gain:.2}")),
            );
            ui.end_row();
        }
    });
}

fn preview_table(ui: &mut egui::Ui, page: &QueryPage) {
    ui.add_space(8.0);
    ui.strong(format!("预测预览（前 {} 行）", page.rows.len()));
    ScrollArea::both().max_height(220.0).show(ui, |ui| {
        egui::Grid::new("predictions_preview")
            .striped(true)
            .show(ui, |ui| {
                for column in &page.columns {
                    ui.strong(&column.name);
                }
                ui.end_row();
                for row in &page.rows {
                    for value in row {
                        ui.monospace(scalar_text(value));
                    }
                    ui.end_row();
                }
            });
    });
}
