//! Human-readable completeness choices and the shared worker's factual reasons.
use analytics_core::research_quality::{ResearchQualityPolicy, ResearchStrictness};
use serde_json::Value;

pub(crate) fn controls(ui: &mut egui::Ui, policy: &mut ResearchQualityPolicy) {
    ui.horizontal(|ui| {
        ui.label("数据检查方式");
        egui::ComboBox::from_id_salt("research_strictness")
            .selected_text(policy.strictness.label())
            .show_ui(ui, |ui| {
                for mode in [
                    ResearchStrictness::Strict,
                    ResearchStrictness::AllowMissing,
                    ResearchStrictness::QualifiedOnly,
                ] {
                    ui.selectable_value(&mut policy.strictness, mode, mode.label());
                }
            });
    });
    ui.label(policy.strictness.description());
    if policy.strictness != ResearchStrictness::Strict {
        ui.horizontal(|ui| {
            ui.label("最低有效因子比例");
            ui.add(
                egui::DragValue::new(&mut policy.min_valid_feature_ratio)
                    .range(0.0..=1.0)
                    .speed(0.01),
            );
            ui.label("0表示至少一项有效");
            ui.label("每折最低有标签样本数");
            ui.add(egui::DragValue::new(&mut policy.min_labeled_rows).range(1..=10_000_000));
        });
        ui.label("门槛是执行下限；已有持仓的估值和权益缺项仍会停止研究。");
    }
}

pub(crate) fn reasons(ui: &mut egui::Ui, value: &Value) {
    let basis = if value["price_basis"].is_string() {
        &value["price_basis"]
    } else {
        &value["execution"]["price_basis"]
    };
    if let Ok(basis) =
        serde_json::from_value::<analytics_core::research_quality::PriceBasis>(basis.clone())
    {
        ui.label(format!("训练价格口径：{}", basis.label()));
    }

    let policy = &value["admission"]["policy"];
    let policy = if policy.is_object() {
        policy
    } else if value["execution"]["quality_policy"].is_object() {
        &value["execution"]["quality_policy"]
    } else {
        &value["request"]["quality_policy"]
    };
    if let Ok(policy) = serde_json::from_value::<ResearchQualityPolicy>(policy.clone()) {
        ui.label(format!("检查方式：{}", policy.strictness.label()));
        if policy.strictness == ResearchStrictness::QualifiedOnly {
            ui.label("经数据筛选的研究股票池；收益不代表完整原股票池。");
        }
    }
    let quality = if value["quality"].is_object() {
        &value["quality"]
    } else {
        &value["execution"]["quality"]
    };
    if let Some(counts) = quality["daily_counts"].as_array() {
        let original: usize = counts
            .iter()
            .filter_map(|c| c["original"].as_u64())
            .map(|n| n as usize)
            .sum();
        let retained: usize = counts
            .iter()
            .filter_map(|c| c["retained"].as_u64())
            .map(|n| n as usize)
            .sum();
        ui.label(format!(
            "证券日：原始 {original} · 保留 {retained} · 排除 {}",
            original.saturating_sub(retained)
        ));
    }
    let selections = if value["sample_selection"].is_object() {
        &value["sample_selection"]
    } else {
        &value["execution"]["sample_selection"]
    };
    if let Some(rows) = selections["selections"].as_array() {
        ui.collapsing("逐折样本数量", |ui| {
            for r in rows {
                ui.label(format!(
                    "折 {} · {} · 候选 {} · 保留 {} · 排除 {}",
                    r["fold"],
                    r["split"],
                    r["candidate_rows"],
                    r["retained_rows"],
                    r["excluded_rows"]
                ));
            }
        });
    }
    let admission = &value["admission"];
    let mut rows = Vec::new();
    if let Some(checks) = admission["checks"].as_array() {
        rows.extend(checks.iter());
    }
    if let Some(decisions) = admission["decisions"].as_array() {
        rows.extend(decisions.iter());
    }
    if let Some(warnings) = value["source_warnings"].as_array() {
        rows.extend(warnings.iter());
    }

    if rows.is_empty() {
        return;
    }
    ui.collapsing(
        format!("数据检查原因列表（{}项）", rows.len()),
        |ui| {
            let key = ui.id().with("admission_filters");
            let (mut filter, mut handling) =
                ui.data_mut(|d| d.get_temp::<(String, String)>(key).unwrap_or_default());
            ui.horizontal(|ui| {
                ui.label("证券 / 原因 / 类别");
                ui.text_edit_singleline(&mut filter);
                egui::ComboBox::from_id_salt("admission_handling")
                    .selected_text(handling_label(&handling))
                    .show_ui(ui, |ui| {
                        for item in [
                            "",
                            "block",
                            "keep_null",
                            "exclude_sample",
                            "exclude_security",
                            "warn",
                            "passed",
                            "not_run",
                        ] {
                            ui.selectable_value(
                                &mut handling,
                                item.to_string(),
                                handling_label(item),
                            );
                        }
                    });
                if ui.button("导出完整检查JSON").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_file_name("research-checks.json")
                        .save_file()
                {
                    let result = serde_json::to_vec_pretty(value)
                        .map_err(|e| e.to_string())
                        .and_then(|bytes| std::fs::write(path, bytes).map_err(|e| e.to_string()));
                    if let Err(error) = result {
                        ui.colored_label(crate::theme::semantic(ui.ctx()).error, error);
                    }
                }
            });
            ui.data_mut(|d| d.insert_temp(key, (filter.clone(), handling.clone())));
            egui::ScrollArea::vertical()
                .id_salt("reason_rows")
                .max_height(380.)
                .show(ui, |ui| {
                    for (i, row) in rows.into_iter().enumerate().filter(|(_, r)| {
                        (filter.is_empty()
                            || r.to_string()
                                .to_lowercase()
                                .contains(&filter.to_lowercase()))
                            && (handling.is_empty()
                                || r["status"] == handling
                                || (r["status"] != "passed" && r["disposition"] == handling))
                    }) {
                        let security = row["security"].as_str().unwrap_or("公共依赖");
                        let status = match row["status"].as_str() {
                            Some("passed") => "通过",
                            Some("not_run") => "未执行",
                            _ => "未通过",
                        };
                        ui.push_id(i, |ui| {
                            ui.label(format!(
                                "{security} · {status} · {} · {}",
                                row["category"].as_str().unwrap_or("数据筛选"),
                                if row["status"] == "passed" {
                                    "无需处理"
                                } else {
                                    handling_label(row["disposition"].as_str().unwrap_or("block"))
                                }
                            ));
                            ui.label(format!(
                                "{} — {} · {}",
                                row["coverage"]["start"].as_str().unwrap_or("—"),
                                row["coverage"]["end"].as_str().unwrap_or("—"),
                                row["code"].as_str().unwrap_or("")
                            ));
                            ui.add(
                                egui::Label::new(row["message"].as_str().unwrap_or("未提供原因"))
                                    .selectable(true),
                            );
                            ui.collapsing("证据与受影响因子", |ui| {
                                ui.add(
                                    egui::Label::new(format!(
                                        "证据：{}\n因子：{}",
                                        row["evidence"], row["affected_factors"]
                                    ))
                                    .selectable(true),
                                );
                            });
                            ui.separator();
                        });
                    }
                });
        },
    );
}
fn handling_label(value: &str) -> &str {
    match value {
        "block" => "阻断执行",
        "keep_null" => "保留为空值",
        "exclude_sample" => "排除样本",
        "exclude_security" => "排除股票",
        "warn" => "告警保留",
        "passed" => "已通过",
        "not_run" => "未执行",
        _ => "全部",
    }
}
