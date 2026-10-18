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
        ui.label("完整优化配置（含校准、预热日期、权重及换手约束）");
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
                    schema_version: 1,
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
