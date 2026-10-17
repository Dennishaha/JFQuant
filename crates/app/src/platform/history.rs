//! Index history maintenance UI; filesystem, evidence and business checks belong to the worker.
use super::*;
use analytics_core::membership_history::{
    Anchor, Boundary, Category, Citation, Event, Moment, Review,
};
use analytics_storage::membership_history::Request;
use analytics_workflow::membership_history::Operation;

#[derive(Default)]
pub(crate) struct Tools {
    editor: String,
    version: String,
    path: String,
    url: String,
    capture_format: String,
    publish_start: String,
    publish_end: String,
    /// Input snapshot used by the last preview, not a ready flag.
    preview_context: String,
}
fn set_editor(ui: &mut egui::Ui, label: &str, set: &mut std::collections::BTreeSet<String>) {
    let mut value = set.iter().cloned().collect::<Vec<_>>().join(",");
    ui.label(label);
    if ui.text_edit_singleline(&mut value).changed() {
        *set = value
            .split([',', '，', ' ', '\n'])
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
    }
}
fn moment(ui: &mut egui::Ui, at: &mut Moment) {
    let mut date = at.date.to_string();
    if ui.text_edit_singleline(&mut date).changed()
        && let Ok(day) = date.parse()
    {
        at.date = day;
    }
    egui::ComboBox::from_id_salt("boundary")
        .selected_text(format!("{:?}", at.boundary))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut at.boundary, Boundary::Unspecified, "边界待核验");
            ui.selectable_value(&mut at.boundary, Boundary::BeforeOpen, "开盘前");
            ui.selectable_value(&mut at.boundary, Boundary::AfterClose, "收盘后");
        });
}
fn references(ui: &mut egui::Ui, citations: &mut Vec<Citation>) {
    for (i, c) in citations.iter_mut().enumerate() {
        ui.push_id(i, |ui| {
            ui.horizontal(|ui| {
                ui.label("文件ID");
                ui.text_edit_singleline(&mut c.document);
            });
            ui.label("原文引用");
            ui.text_edit_multiline(&mut c.excerpt);
        });
    }
    if ui.button("增加引用").clicked() {
        citations.push(Citation {
            document: String::new(),
            excerpt: String::new(),
        });
    }
}

fn edit_anchors(ui: &mut egui::Ui, r: &mut Request, start: &str) {
    ui.collapsing("完整名单", |ui| {
        for (i, a) in r.history.anchors.iter_mut().enumerate() {
            ui.push_id(("anchor", i), |ui| {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label("ID");
                        ui.text_edit_singleline(&mut a.id);
                        moment(ui, &mut a.at);
                    });
                    set_editor(ui, "完整成员（内部证券ID）", &mut a.members);
                    references(ui, &mut a.citations);
                });
            });
        }
        if ui.button("增加完整名单").clicked()
            && let Ok(date) = start.parse()
        {
            r.history.anchors.push(Anchor {
                id: format!("anchor-{}", r.history.anchors.len()),
                at: Moment {
                    date,
                    boundary: Boundary::Unspecified,
                },
                known_at: None,
                members: Default::default(),
                citations: vec![],
            });
        }
    });
}

fn edit_events(ui: &mut egui::Ui, r: &mut Request, start: &str) {
    ui.collapsing("生效变更表", |ui| {
        for (i, e) in r.history.events.iter_mut().enumerate() {
            ui.push_id(("event", i), |ui| {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut e.id);
                        moment(ui, &mut e.at);
                        ui.label("修订");
                        ui.add(egui::DragValue::new(&mut e.revision).range(1..=u32::MAX));
                    });
                    egui::ComboBox::from_id_salt("category")
                        .selected_text(format!("{:?}", e.category))
                        .show_ui(ui, |ui| {
                            for c in Category::all() {
                                ui.selectable_value(&mut e.category, c, format!("{c:?}"));
                            }
                        });
                    set_editor(ui, "调入", &mut e.add);
                    set_editor(ui, "调出", &mut e.remove);
                    references(ui, &mut e.citations);
                });
            });
        }
        if ui.button("增加变更事件").clicked()
            && let Ok(date) = start.parse()
        {
            r.history.events.push(Event {
                id: format!("event-{}", r.history.events.len()),
                revision: 1,
                supersedes: None,
                at: Moment {
                    date,
                    boundary: Boundary::Unspecified,
                },
                known_at: None,
                category: Category::Review,
                add: Default::default(),
                remove: Default::default(),
                citations: vec![],
            });
        }
    });
}

fn edit_evidence(ui: &mut egui::Ui, r: &mut Request) {
    ui.collapsing("原始文件与完整性审核", |ui| {
        for (i, d) in r.documents.iter_mut().enumerate() {
            ui.push_id(("document", i), |ui| {
                ui.label("文件ID / 工作区相对路径 / SHA256 / 原始URL");
                ui.text_edit_singleline(&mut d.id);
                let mut p = d.path.to_string_lossy().to_string();
                if ui.text_edit_singleline(&mut p).changed() {
                    d.path = p.into();
                }
                ui.text_edit_singleline(&mut d.sha256);
                ui.text_edit_singleline(&mut d.source_url);
                ui.checkbox(
                    &mut d.coverage_source,
                    "来源确实证明事件覆盖范围（单份名单或单次事件不能勾选）",
                );
            });
        }
        if ui.button("增加证据文件").clicked() {
            r.documents
                .push(analytics_storage::membership_history::Document {
                    id: format!("document-{}", r.documents.len()),
                    path: PathBuf::new(),
                    sha256: String::new(),
                    source_url: String::new(),
                    format: "pdf".into(),
                    coverage_source: false,
                });
        }
        for (i, review) in r.history.reviews.iter_mut().enumerate() {
            ui.push_id(("review", i), |ui| {
                ui.text_edit_singleline(&mut review.id);
                ui.label("审核人");
                ui.text_edit_singleline(&mut review.reviewer);
                ui.label("来源覆盖及核对过程");
                ui.text_edit_multiline(&mut review.basis);
                for (label, date) in [
                    ("开始", &mut review.range.start),
                    ("结束", &mut review.range.end),
                ] {
                    ui.horizontal(|ui| {
                        ui.label(label);
                        let mut value = date.to_string();
                        if ui.text_edit_singleline(&mut value).changed()
                            && let Ok(d) = value.parse()
                        {
                            *date = d;
                        }
                    });
                }
                for c in Category::all() {
                    let mut yes = review.categories.contains(&c);
                    if ui.checkbox(&mut yes, format!("{c:?}")).changed() {
                        if yes {
                            review.categories.insert(c);
                        } else {
                            review.categories.remove(&c);
                        }
                    }
                }
                references(ui, &mut review.citations);
            });
        }
        if ui.button("增加完整性审核").clicked() {
            r.history.reviews.push(Review {
                id: format!("review-{}", r.history.reviews.len()),
                range: r.history.requested.clone(),
                categories: Default::default(),
                reviewer: String::new(),
                basis: String::new(),
                citations: vec![],
            });
        }
    });
}
pub(super) fn maintenance(
    ui: &mut egui::Ui,
    state: &mut PlatformState,
    enabled: bool,
) -> Option<WorkflowRequest> {
    let mut op = None;
    ui.collapsing("历史成分重建", |ui| {
        ui.label("完整名单＋生效变更＋区间审核；资料不足保留缺口。查询和预览不联网。");
        ui.add_enabled_ui(enabled, |ui| {
            let t = &mut state.history_tools;
            let context = format!("{}|{}|{}", state.index_version, state.market, t.editor);
            if !t.preview_context.is_empty() && t.preview_context != context {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "输入已变化，原预览已过期；发布会重新核验。",
                );
            }
            if ui.button("生成历史成员草稿").clicked() {
                match (state.start.parse(), state.end.parse()) {
                    (Ok(start), Ok(end)) => {
                        op = Some(Operation::Draft {
                            index: state.index_version.clone(),
                            market: state.market.clone(),
                            range: analytics_core::DateRange { start, end },
                        })
                    }
                    _ => state.error = Some("请输入有效的研究日期".into()),
                }
            }
            if state.details.get("history_request").is_some()
                && ui.button("载入任务结果为草稿").clicked()
            {
                t.editor = serde_json::to_string_pretty(&state.details["history_request"])
                    .unwrap_or_default();
            }
            ui.label("高级草稿 JSON（包含原文可知时间、引用及修订链；下方提供表格编辑）");
            ui.add(
                egui::TextEdit::multiline(&mut t.editor)
                    .code_editor()
                    .desired_rows(5),
            );
            if let Ok(mut r) = serde_json::from_str::<Request>(&t.editor) {
                let before = r.clone();
                edit_anchors(ui, &mut r, &state.start);
                edit_events(ui, &mut r, &state.start);
                edit_evidence(ui, &mut r);
                if before != r {
                    t.editor = serde_json::to_string_pretty(&r).unwrap_or_default();
                }
                if ui.button("校验并预览成员区间").clicked() {
                    t.preview_context =
                        format!("{}|{}|{}", state.index_version, state.market, t.editor);
                    op = Some(Operation::Preview {
                        index: state.index_version.clone(),
                        market: state.market.clone(),
                        request: Box::new(r.clone()),
                    });
                }
                ui.horizontal(|ui| {
                    ui.label("发布连续区间");
                    ui.text_edit_singleline(&mut t.publish_start);
                    ui.text_edit_singleline(&mut t.publish_end);
                });
                if ui.button("重新核验并发布所选区间").clicked() {
                    match (t.publish_start.parse(), t.publish_end.parse()) {
                        (Ok(start), Ok(end)) => {
                            op = Some(Operation::Publish {
                                index: state.index_version.clone(),
                                market: state.market.clone(),
                                request: Box::new(r),
                                range: analytics_core::DateRange { start, end },
                            })
                        }
                        _ => state.error = Some("请选择预览中已证明的连续区间".into()),
                    }
                }
            }
            ui.horizontal(|ui| {
                ui.label("历史版本");
                ui.text_edit_singleline(&mut t.version);
            });
            if ui.button("只读检查历史版本").clicked() {
                op = Some(Operation::Status {
                    version: t.version.clone(),
                });
            }
            ui.horizontal(|ui| {
                ui.label("导入 history.json / 导出新目录");
                ui.text_edit_singleline(&mut t.path);
            });
            if ui.button("导入并重新校验").clicked() {
                op = Some(Operation::Import {
                    index: state.index_version.clone(),
                    market: state.market.clone(),
                    path: t.path.clone().into(),
                });
            }
            if ui.button("导出历史及原文").clicked() {
                op = Some(Operation::Export {
                    version: t.version.clone(),
                    path: t.path.clone().into(),
                });
            }
            ui.horizontal(|ui| {
                ui.label("显式补采原文URL");
                ui.text_edit_singleline(&mut t.url);
            });
            egui::ComboBox::from_id_salt("history_capture_format")
                .selected_text(&t.capture_format)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut t.capture_format, "pdf".into(), "PDF");
                    ui.selectable_value(&mut t.capture_format, "text".into(), "文本");
                });
            if ui.button("联网捕获指定原文").clicked() {
                op = Some(Operation::Capture {
                    url: t.url.clone(),
                    format: t.capture_format.clone(),
                });
            }
            if let Some(report) = state.details.get("history_report") {
                ui.label(format!(
                    "事实连续区间：{}；未证明日期：{}",
                    report["segments"].as_array().map_or(0, Vec::len),
                    report["missing_dates"].as_array().map_or(0, Vec::len)
                ));
                if let Some(segments) = report["segments"].as_array() {
                    for s in segments {
                        ui.label(format!(
                            "{} — {}；可知时间已确认 {} 天",
                            s["membership"]["coverage"]["start"],
                            s["membership"]["coverage"]["end"],
                            s["point_in_time_dates"].as_array().map_or(0, Vec::len)
                        ));
                    }
                }
                if let Some(issues) = report["issues"].as_array() {
                    for issue in issues {
                        ui.label(format!("{} · {}", issue["id"], issue["next_action"]));
                    }
                }
            }
            if state.details.get("pool_definition").is_some()
                && ui.button("载入重建股票池定义").clicked()
            {
                state.pool_editor = serde_json::to_string_pretty(&state.details["pool_definition"])
                    .unwrap_or_default();
            }
        });
    });
    op.map(|request| WorkflowRequest::Index {
        request: analytics_workflow::indices::IndexOperation::History {
            request: Box::new(request),
        },
    })
}
