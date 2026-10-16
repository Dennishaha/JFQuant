//! Factor combination checks and report freshness; all market work stays off the UI thread.
use crate::{
    panels::{primary_button, run_pipeline_request, send_command},
    state::AppState,
    theme,
    worker::{WorkerCommand, WorkerHandle},
};
use analytics_workflow::research::pipeline::{Cancellation, cancellation};
use uuid::Uuid;
pub(crate) fn controls(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
    busy: bool,
) {
    ui.horizontal(|ui| {
        ui.label("组合数据策略");
        for choice in [
            analytics_core::research_quality::ResearchStrictness::Strict,
            analytics_core::research_quality::ResearchStrictness::AllowMissing,
            analytics_core::research_quality::ResearchStrictness::QualifiedOnly,
        ] {
            ui.selectable_value(
                &mut state.factor.check_policy.strictness,
                choice,
                choice.label(),
            );
        }
    });
    if ui
        .add_enabled(
            !busy,
            primary_button("检查组合数据", theme::semantic(ui.ctx()).accent),
        )
        .clicked()
    {
        match run_pipeline_request(state) {
            Ok(mut input) => {
                let task = Uuid::new_v4();
                if state.platform.workspace.is_none() {
                    input.output_dir = input.output_dir.join(task.to_string());
                }
                let request = analytics_workflow::input_check::CheckRequest {
                    expected_input_hash: None,
                    expected_source_stamp: None,
                    input: input.into(),
                    horizon: state.factor.horizon,
                    policy: state.factor.check_policy.clone(),
                };
                let flag = cancellation();
                *cancel_slot = Some(flag.clone());
                state.factor.error = None;
                send_command(
                    state,
                    worker,
                    WorkerCommand::CheckFactors {
                        task,
                        request,
                        cancel: flag,
                    },
                );
            }
            Err(e) => state.factor.error = Some(e),
        }
    }
    report(ui, state);
}
fn report(ui: &mut egui::Ui, state: &mut AppState) {
    refresh_source_stamp(ui, state);
    let Some(report) = &state.factor.input_check else {
        return;
    };
    let current = run_pipeline_request(state).ok().map(|p| {
        let mut input: analytics_workflow::factor_evaluation::EvaluationInput = p.into();
        if let Some(w) = &state.platform.workspace {
            if input.market_path.is_relative() {
                input.market_path = w.root().join(&input.market_path);
            }
            if input.universe_path.is_relative() {
                input.universe_path = w.root().join(&input.universe_path);
            }
        }
        analytics_workflow::input_check::factor_context(
            &input,
            state.factor.horizon,
            &state.factor.check_policy,
        )
    });
    let stale = current.as_ref() != Some(&report.plan.context)
        || state
            .factor
            .check_source_stamp
            .as_ref()
            .is_some_and(|stamp| report.samples["source_stamp"].as_str() != Some(stamp));
    ui.group(|ui| {
        ui.strong(if stale {
            "组合数据报告已过期：输入或因子选择已改变，请重新检查"
        } else if report.ready() {
            "组合数据检查通过（本页计算上下文）"
        } else {
            "组合数据检查未通过"
        });
        ui.label(format!(
            "{} 个因子 · {}",
            report.plan.factor_ids.len(),
            report.plan.purpose
        ));
        for stage in &report.stages {
            ui.label(format!(
                "{}：{:?} {}",
                stage.stage, stage.status, stage.message
            ));
        }
        egui::Grid::new("factor_input_check_coverage")
            .striped(true)
            .show(ui, |ui| {
                for title in ["因子", "有效 / 成员样本", "标签配对", "有效率"] {
                    ui.strong(title);
                }
                ui.end_row();
                for f in &report.factors {
                    ui.label(&f.factor_id);
                    ui.label(format!("{} / {}", f.finite, f.eligible));
                    ui.label(f.paired.to_string());
                    ui.label(
                        f.coverage
                            .map_or("—".into(), |v| format!("{:.2}%", v * 100.)),
                    );
                    ui.end_row();
                }
            });
        for warning in &report.warnings {
            ui.label(warning);
        }
        ui.collapsing("受影响日期与输入明细", |ui| {
            for f in &report.factors {
                for range in &f.empty_intervals {
                    ui.label(format!(
                        "{}：{} 至 {} 有空值",
                        f.factor_id, range.start, range.end
                    ));
                }
            }
            for c in &report.admission.checks {
                ui.label(&c.message);
            }
            ui.label(format!("依赖缺失计数：{:?}", report.missing_counts));
            ui.label(format!("任务与历史依赖：{:?}", report.missing_scope_counts));
        });
        if let Some(r) = &report.artifact {
            ui.label(if r.cache_hit {
                "已复用并验证完整上下文因子产物"
            } else {
                "已保存因子产物，正式任务将验证上下文后复用"
            });
        }
    });
}

fn refresh_source_stamp(ui: &egui::Ui, state: &mut AppState) {
    if state.factor.input_check.is_none() {
        return;
    }
    if let Some(rx) = &state.factor.check_probe {
        match rx.try_recv() {
            Ok(result) => {
                state.factor.check_source_stamp = Some(result.unwrap_or_else(|_| "missing".into()));
                state.factor.check_probe = None;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                state.factor.check_source_stamp = Some("unavailable".into());
                state.factor.check_probe = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }
    if state.factor.check_probe.is_none()
        && state
            .factor
            .check_probe_at
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(3))
    {
        let Some(report) = &state.factor.input_check else {
            return;
        };
        let market =
            std::path::PathBuf::from(report.plan.context["market_path"].as_str().unwrap_or(""));
        let universe =
            std::path::PathBuf::from(report.plan.context["universe_path"].as_str().unwrap_or(""));
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ui.ctx().clone();
        std::thread::spawn(move || {
            let _ = tx.send(
                analytics_workflow::input_check::source_stamp(&market, &universe)
                    .map_err(|e| e.to_string()),
            );
            ctx.request_repaint();
        });
        state.factor.check_probe = Some(rx);
        state.factor.check_probe_at = Some(std::time::Instant::now());
    }
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_secs(3));
}
