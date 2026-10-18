//! Runs panel and interaction.
use super::*;
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
        state.platform.reads.current = None;
        state.platform.reads.overview(w.root().to_owned());
    }
    ui.horizontal(|ui| {
        ui.label("登记当前格式研究目录");
        ui.text_edit_singleline(&mut state.platform.import_run_path);
        if ui
            .add_enabled(!state.busy(), egui::Button::new("导入只读记录"))
            .clicked()
        {
            let root = w.root().to_owned();
            let path = w.resolve(&state.platform.import_run_path);
            state.platform.reads.import(root, path);
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
        state.platform.reads.select(w.root().to_owned(), &r);
        reads::poll(&mut state.platform, ui.ctx());
        match &state.platform.reads.current {
            Some((_, Ok(view))) => {
                match &view.result {
                    Ok(v) => {
                        crate::quality_ui::reasons(ui, v);
                        show_results(ui, v);
                        show_nav(
                            ui,
                            &view.nav,
                            v["input_kind"] == "synthetic_fixture",
                            v["execution"]["benchmark"].as_str().unwrap_or("所选基准"),
                        );
                    }
                    Err(e) => {
                        ui.colored_label(
                            crate::theme::semantic(ui.ctx()).error,
                            format!("读取结果失败：{e}"),
                        );
                    }
                }
                ui.collapsing("持久化日志", |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("run_log")
                        .max_height(240.0)
                        .show(ui, |ui| {
                            for v in view.logs.iter().rev() {
                                ui.label(format!(
                                    "{} {}",
                                    v["time"].as_str().unwrap_or(""),
                                    v["message"].as_str().unwrap_or("")
                                ));
                            }
                        });
                });
            }
            Some((_, Err(e))) => {
                ui.colored_label(crate::theme::semantic(ui.ctx()).error, e);
            }
            None => {
                ui.label("正在读取结果…");
            }
        }
    }
}
pub(super) fn show_results(ui: &mut egui::Ui, value: &Value) {
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
    if value["samples"]["allocation"].is_object() {
        let a = &value["samples"]["allocation"];
        ui.collapsing("优化预热与就绪边界", |ui| {
            ui.label(format!(
                "预测预热 {} → {}；参考账预热 {} → {}；正式账户 {} → {}",
                a["prediction_warmup"]["start"],
                a["prediction_warmup"]["end"],
                a["reference_warmup"]["start"],
                a["reference_warmup"]["end"],
                a["execution"]["start"],
                a["execution"]["end"]
            ));
            ui.label(format!(
                "最早可能参考账就绪日期（数据完整时）：{}",
                a["earliest_possible_reference_date_lower_bound"]
            ));
            for issue in a["issues"].as_array().into_iter().flatten() {
                ui.label(issue.as_str().unwrap_or(""));
            }
            ui.small("候选数量、配对样本和收益端点还需按已生成预测检查；不会自动推迟正式起点。");
        });
    }
    if value["execution"]["allocation_display"].is_object() {
        let a = &value["execution"]["allocation_display"];
        ui.collapsing("组合优化与资金漂移", |ui| {
            ui.label(format!(
                "方法 {}；共{}次决策，最多展示最近100次。",
                a["config"]["method"], a["decision_count"]
            ));
            ui.small("参考账无费用且允许分数股；实际费用和收益以成交账户为准。");
            for d in a["decisions"].as_array().into_iter().flatten() {
                ui.collapsing(format!("{} · {}", d["date"], d["status"]), |ui| {
                    ui.label(serde_json::to_string_pretty(d).unwrap_or_default());
                });
            }
            ui.collapsing("参考净值、现金与锁定资产", |ui| {
                for r in a["reference_rows"].as_array().into_iter().flatten() {
                    ui.label(format!(
                        "{} 净值{} 现金{} 锁定{} 待执行{}",
                        r["date"], r["nav"], r["cash"], r["locked_value"], r["pending"]
                    ));
                }
            });
        });
    }
    if value["kind"] == "allocation_comparison" {
        ui.collapsing(
            "优化方法对照（相同输入、含实际费用）",
            |ui| {
                for r in value["runs"].as_array().into_iter().flatten() {
                    ui.label(format!(
                        "{} 收益{} 回撤{} 夏普{} 换手{} 拒单{} 集中度{} 引擎秒数{}",
                        r["method"],
                        r["aggregate"]["total_return"],
                        r["aggregate"]["max_drawdown"],
                        r["aggregate"]["sharpe"],
                        r["aggregate"]["mean_daily_turnover"],
                        r["aggregate"]["blocked_orders"],
                        r["mean_target_hhi"],
                        r["timing"]["engine_seconds"]
                    ));
                }
            },
        );
    }
    if value["execution_diagnostics"].is_object() {
        let d = &value["execution_diagnostics"];
        ui.label(format!(
            "有效信号：{}；成交回报：{}；拒单回报：{}",
            d["valid_signals"], d["filled_events"], d["rejected_events"]
        ));
    }
    if let Some(label) = value["cross_pool"]["label"].as_str() {
        ui.strong(label);
        ui.small("曾见/未见按当日所用模型的实际训练及验证样本判断；贡献来自同一个连续账户。");
        for group in value["cross_pool"]["groups"]
            .as_array()
            .into_iter()
            .flatten()
        {
            ui.label(format!(
                "{}：预测 {} 行，有标签 {} 行，Rank IC {}，损益贡献 {} 元，费用 {} 元",
                if group["group"] == "seen" {
                    "曾见标的"
                } else {
                    "未见标的"
                },
                group["prediction_rows"],
                group["labeled_rows"],
                group["rank_ic"],
                group["pnl_contribution_cny"],
                group["execution_fees_cny"]
            ));
        }
        ui.collapsing("冻结模型切换计划", |ui| {
            for segment in value["cross_pool"]["model_segments"]
                .as_array()
                .into_iter()
                .flatten()
            {
                ui.label(format!(
                    "{} → {} · {}",
                    segment["range"]["start"], segment["range"]["end"], segment["model_id"]
                ));
            }
        });
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

fn show_nav(ui: &mut egui::Ui, rows: &[(String, f64, f64)], synthetic: bool, benchmark: &str) {
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
            analytics_core::indices::by_symbol(benchmark)
                .map(|i| i.name.as_str())
                .unwrap_or(benchmark)
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
