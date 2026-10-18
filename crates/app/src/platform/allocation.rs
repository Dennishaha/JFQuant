//! Shared allocation and multi-strategy plan editor; computation stays in the service.
use super::*;
use analytics_workflow::research::backtest::combined::{CombinedPortfolioPlan, Job};
pub(super) fn panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
    enabled: bool,
) {
    ui.collapsing("优化配置与多策略统一账户", |ui| {
        use analytics_workflow::allocation::{AllocationMethod, CandidatePolicy, STOCK_EXPOSURE};
        let c = &mut state.platform.template.config.allocation;
        let before = c.clone();
        egui::ComboBox::from_id_salt("allocation-method").selected_text(match c.method {
            AllocationMethod::Equal => "等权", AllocationMethod::MinimumVariance => "最小方差",
            AllocationMethod::RiskBudgeting => "风险预算", AllocationMethod::MeanVariance => "均值方差",
        }).show_ui(ui, |ui| {
            for (method,label) in [(AllocationMethod::Equal,"等权"),(AllocationMethod::MinimumVariance,"最小方差"),(AllocationMethod::RiskBudgeting,"风险预算"),(AllocationMethod::MeanVariance,"均值方差")] { ui.selectable_value(&mut c.method,method,label); }
        });
        egui::ComboBox::from_id_salt("allocation-candidates").selected_text(match c.candidate_policy { CandidatePolicy::Unfiltered => "保留全部候选（历史不足则阻断）", CandidatePolicy::CompleteRiskWindow => "排序前检查完整风险窗口资格" }).show_ui(ui, |ui| {
            ui.selectable_value(&mut c.candidate_policy,CandidatePolicy::Unfiltered,"保留全部候选（历史不足则阻断）");
            ui.selectable_value(&mut c.candidate_policy,CandidatePolicy::CompleteRiskWindow,"排序前检查完整风险窗口资格");
        });
        ui.small("资格逐日计算并记录排除原因；方法对照须选择相同规则。等权也可使用完整窗口资格作为基线。");
        ui.add(egui::Slider::new(&mut c.max_stock_weight,0.01..=1.).clamping(egui::SliderClamping::Edits).text("单股目标上限"));
        ui.add(egui::Slider::new(&mut c.max_strategy_weight,0.05..=1.).clamping(egui::SliderClamping::Edits).text("单策略资金上限"));
        let mut turnover_enabled = c.max_turnover.is_some();
        if ui.checkbox(&mut turnover_enabled,"限制全账户目标换手").changed() { c.max_turnover = turnover_enabled.then_some(1.); }
        if let Some(value) = &mut c.max_turnover { ui.add(egui::Slider::new(value,0.0..=2.).text("目标权重绝对变化之和")); }
        ui.horizontal(|ui| { ui.label("风险窗口（收益起始交易日数）"); ui.add(egui::DragValue::new(&mut c.risk_window).range(2..=10000)); });
        if c.method == AllocationMethod::MeanVariance {
            let mut explicit = c.risk_aversion.is_some();
            if ui.checkbox(&mut explicit,"显式配置风险厌恶 λ").changed() { c.risk_aversion=explicit.then_some(1.); }
            if let Some(value) = &mut c.risk_aversion { ui.add(egui::DragValue::new(value).clamp_existing_to_range(false).range(1e-6..=1e6).prefix("λ = ")); }
            ui.horizontal(|ui| { ui.label("校准窗口 / 最少日期 / 每日最少配对"); ui.add(egui::DragValue::new(&mut c.calibration_window).range(2..=10000)); ui.add(egui::DragValue::new(&mut c.calibration_min_days).range(2..=10000)); ui.add(egui::DragValue::new(&mut c.calibration_min_pairs).clamp_existing_to_range(false).range(3..=10000)); });
        }
        let count = state.platform.template.config.top_n;
        if count > 0 && (count as f64)*c.max_stock_weight < STOCK_EXPOSURE-1e-6 {
            let required = (STOCK_EXPOSURE/c.max_stock_weight).ceil() as usize;
            ui.colored_label(egui::Color32::YELLOW,format!("98%目标仓位与当前上限至少需要{required}只股票；当前Top N为{count}。"));
            if ui.button(format!("显式将单股上限设为98%/{count}")).clicked() { c.max_stock_weight=STOCK_EXPOSURE/count as f64; }
        }
        if let Err(e) = c.validate() { ui.colored_label(egui::Color32::YELLOW,e.to_string()); }
        if *c != before { state.platform.allocation_editor=serde_json::to_string_pretty(c).unwrap(); }
        ui.label("高级JSON（预热日期、求解容差及完整配置）");
        if ui.button("读取当前优化配置").clicked() {
            state.platform.allocation_editor =
                serde_json::to_string_pretty(&state.platform.template.config.allocation).unwrap();
        }
        ui.add(
            egui::TextEdit::multiline(&mut state.platform.allocation_editor)
                .desired_rows(8)
                .desired_width(f32::INFINITY),
        );
        if ui.button("应用优化配置").clicked() {
            match serde_json::from_str::<analytics_workflow::allocation::AllocationConfig>(
                &state.platform.allocation_editor,
            ) {
                Ok(c) => match c.validate_count(state.platform.template.config.top_n) {
                    Ok(()) => state.platform.template.config.allocation = c,
                    Err(e) => state.platform.error = Some(e.to_string()),
                },
                Err(e) => state.platform.error = Some(e.to_string()),
            }
        }
        ui.separator();
        ui.label("来源任务ID（逗号分隔，2—20个成功股票策略任务）");
        ui.text_edit_singleline(&mut state.platform.combined_sources);
        if ui.button("生成组合计划").clicked() {
            let result = (|| -> Result<CombinedPortfolioPlan, String> {
                let p = CombinedPortfolioPlan {
                    schema_version: 2,
                    sources: state
                        .platform
                        .combined_sources
                        .split(',')
                        .map(|s| s.trim().parse().map_err(|_| "任务ID无效".to_owned()))
                        .collect::<Result<_, _>>()?,
                    range: analytics_core::DateRange {
                        start: state.platform.start.parse().map_err(|_| "开始日期无效")?,
                        end: state.platform.end.parse().map_err(|_| "结束日期无效")?,
                    },
                    execution: state.platform.template.execution_settings(),
                };
                p.validate().map_err(|e| e.to_string())?;
                Ok(p)
            })();
            match result {
                Ok(p) => state.platform.combined_editor = serde_json::to_string_pretty(&p).unwrap(),
                Err(e) => state.platform.error = Some(e),
            }
        }
        ui.add(
            egui::TextEdit::multiline(&mut state.platform.combined_editor)
                .desired_rows(12)
                .desired_width(f32::INFINITY),
        );
        ui.small(
            "仅使用冻结目标与参考账；统一账户重新计算现金、费用和收益，来源选择属于研究选择。",
        );
        if ui
            .add_enabled(
                enabled,
                egui::Button::new("比较以上任务的优化方法（需等权基线）"),
            )
            .clicked()
        {
            match state
                .platform
                .combined_sources
                .split(',')
                .map(|s| s.trim().parse::<Uuid>())
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(tasks) => submit(
                    state,
                    worker,
                    cancel_slot,
                    WorkflowRequest::CompareAllocations { tasks },
                ),
                Err(e) => state.platform.error = Some(e.to_string()),
            }
        }

        ui.horizontal(|ui| {
            for (label, check) in [("检查组合输入", true), ("运行统一账户", false)] {
                if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                    match serde_json::from_str(&state.platform.combined_editor) {
                        Ok(plan) => submit(
                            state,
                            worker,
                            cancel_slot,
                            WorkflowRequest::Combined {
                                request: Box::new(if check {
                                    Job::Check { plan }
                                } else {
                                    Job::Run { plan }
                                }),
                            },
                        ),
                        Err(e) => state.platform.error = Some(e.to_string()),
                    }
                }
            }
        });
    });
}
