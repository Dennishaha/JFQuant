//! Index history maintenance UI; filesystem, evidence and business checks belong to the worker.
use super::*;
use analytics_core::membership_history::{
    Anchor, Boundary, Category, Citation, Event, Moment, Review, ReviewOutcome, SecurityEvidence,
};
use analytics_storage::membership_history::Request;
use analytics_workflow::history_material::MaterialAction;
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
    material_action: String,
    material_error: String,
    selected_document: String,
    transcription_id: String,
    reviewer: String,
    compare_before: String,
    compare_after: String,
    texture: Option<egui::TextureHandle>,
    image_info: Value,
    drag_start: Option<egui::Pos2>,
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
            ui.horizontal(|ui| {
                ui.label("图片核对ID（文本留空）");
                let mut id = c.transcription.clone().unwrap_or_default();
                if ui.text_edit_singleline(&mut id).changed() {
                    c.transcription = if id.is_empty() { None } else { Some(id) };
                }
            });
            ui.label("原文引用");
            ui.text_edit_multiline(&mut c.excerpt);
        });
    }
    if ui.button("增加引用").clicked() {
        citations.push(Citation {
            transcription: None,
            document: String::new(),
            excerpt: String::new(),
        });
    }
}

fn publication(
    ui: &mut egui::Ui,
    known: &mut Option<chrono::DateTime<chrono::Utc>>,
    subject: &str,
) {
    let key = ui.make_persistent_id(("known_at", subject, known.map(|d| d.to_rfc3339())));
    let mut text = ui
        .data_mut(|d| d.get_temp::<String>(key))
        .unwrap_or_else(|| known.map(|d| d.to_rfc3339()).unwrap_or_default());
    ui.label("已核验发布时间 RFC3339（未知留空）");
    let response = ui.text_edit_singleline(&mut text);
    if response.lost_focus() {
        if text.trim().is_empty() {
            *known = None;
        } else if let Ok(d) = text.parse() {
            *known = Some(d);
        } else {
            *known = None;
            ui.colored_label(egui::Color32::YELLOW, "无效时间，保持待核验");
        }
    }
    ui.data_mut(|d| d.insert_temp(key, text));
}
fn revision(
    ui: &mut egui::Ui,
    value: &mut u32,
    supersedes: &mut Option<u32>,
    correction: &mut Vec<Citation>,
) {
    ui.horizontal(|ui| {
        ui.label("修订号");
        ui.add(egui::DragValue::new(value).range(1..=u32::MAX));
        let mut replaces = supersedes.unwrap_or(0);
        ui.label("替代修订（0为无）");
        if ui.add(egui::DragValue::new(&mut replaces)).changed() {
            *supersedes = if replaces == 0 { None } else { Some(replaces) };
        }
    });
    ui.collapsing("更正依据", |ui| references(ui, correction));
}
fn securities(ui: &mut egui::Ui, values: &mut Vec<SecurityEvidence>) {
    ui.collapsing("逐证券名称与代码证明", |ui| {
        for (i, p) in values.iter_mut().enumerate() {
            ui.push_id(i, |ui| {
                ui.text_edit_singleline(&mut p.instrument);
                ui.text_edit_singleline(&mut p.name);
                ui.collapsing("名单名称引用", |ui| references(ui, &mut p.membership));
                ui.collapsing("名称与代码身份引用", |ui| {
                    references(ui, &mut p.identity)
                });
            });
        }
        if ui.button("增加证券关联").clicked() {
            values.push(SecurityEvidence {
                instrument: String::new(),
                name: String::new(),
                membership: vec![],
                identity: vec![],
            });
        }
    });
}
fn edit_anchors(ui: &mut egui::Ui, r: &mut Request, start: &str) {
    ui.collapsing("完整名单", |ui| {
        let namespace = r.history.official_code.clone();
        for (i, a) in r.history.anchors.iter_mut().enumerate() {
            ui.push_id(("anchor", i), |ui| {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label("ID");
                        ui.text_edit_singleline(&mut a.id);
                        moment(ui, &mut a.at);
                    });
                    let subject = serde_json::to_string(&(
                        &namespace,
                        &a.id,
                        a.revision,
                        &a.at,
                        &a.citations,
                        &a.correction,
                    ))
                    .unwrap_or_default();
                    publication(ui, &mut a.known_at, &subject);
                    revision(ui, &mut a.revision, &mut a.supersedes, &mut a.correction);
                    securities(ui, &mut a.securities);
                    set_editor(ui, "完整成员（内部证券ID）", &mut a.members);
                    references(ui, &mut a.citations);
                });
            });
        }
        if ui.button("增加完整名单").clicked()
            && let Ok(date) = start.parse()
        {
            r.history.anchors.push(Anchor {
                revision: 1,
                supersedes: None,
                correction: vec![],
                securities: vec![],
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
        let namespace = r.history.official_code.clone();
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
                    let subject = serde_json::to_string(&(
                        &namespace,
                        &e.id,
                        e.revision,
                        &e.at,
                        &e.add,
                        &e.remove,
                        &e.citations,
                        &e.correction,
                    ))
                    .unwrap_or_default();
                    publication(ui, &mut e.known_at, &subject);
                    revision(ui, &mut e.revision, &mut e.supersedes, &mut e.correction);
                    securities(ui, &mut e.securities);
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
                correction: vec![],
                securities: vec![],
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
                egui::ComboBox::from_id_salt("format")
                    .selected_text(&d.format)
                    .show_ui(ui, |ui| {
                        for f in ["pdf", "text", "png", "jpeg"] {
                            ui.selectable_value(&mut d.format, f.into(), f);
                        }
                    });
                if let Some(origin) = &d.derived_from {
                    ui.label(format!(
                        "PDF {} 第{}页 图片{}",
                        origin.document, origin.page, origin.image
                    ));
                }
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
                    derived_from: None,
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
                let mut kind = match &review.outcome {
                    ReviewOutcome::Coverage => 0,
                    ReviewOutcome::Changes { .. } => 1,
                    ReviewOutcome::NoChange { .. } => 2,
                    ReviewOutcome::Unverified { .. } => 3,
                };
                let old = kind;
                egui::ComboBox::from_id_salt("review_outcome")
                    .selected_text(["区间完整性", "该次有变化", "该次明确无变化", "尚未核实"][kind])
                    .show_ui(ui, |ui| {
                        for (i, label) in ["区间完整性", "该次有变化", "该次明确无变化", "尚未核实"]
                            .iter()
                            .enumerate()
                        {
                            ui.selectable_value(&mut kind, i, *label);
                        }
                    });
                if old != kind {
                    let at = Moment {
                        date: review.range.start,
                        boundary: Boundary::Unspecified,
                    };
                    review.outcome = match kind {
                        1 => ReviewOutcome::Changes {
                            review_id: String::new(),
                            at,
                            events: Default::default(),
                        },
                        2 => ReviewOutcome::NoChange {
                            review_id: String::new(),
                            at,
                        },
                        3 => ReviewOutcome::Unverified {
                            review_id: String::new(),
                        },
                        _ => ReviewOutcome::Coverage,
                    };
                }
                match &mut review.outcome {
                    ReviewOutcome::Changes {
                        review_id,
                        at,
                        events,
                    } => {
                        ui.text_edit_singleline(review_id);
                        moment(ui, at);
                        set_editor(ui, "该次事件ID", events);
                    }
                    ReviewOutcome::NoChange { review_id, at } => {
                        ui.text_edit_singleline(review_id);
                        moment(ui, at);
                    }
                    ReviewOutcome::Unverified { review_id } => {
                        ui.text_edit_singleline(review_id);
                    }
                    _ => {}
                }
                references(ui, &mut review.citations);
            });
        }
        if ui.button("增加完整性审核").clicked() {
            r.history.reviews.push(Review {
                outcome: analytics_core::membership_history::ReviewOutcome::Coverage,
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
fn materials(
    ui: &mut egui::Ui,
    t: &mut Tools,
    r: &mut Request,
    details: &Value,
) -> Option<MaterialAction> {
    let mut action = None;
    ui.collapsing("资料登记、提取与图片人工核对", |ui| {
        ui.label("结构化操作：inspect / register / extract / review；与CLI共用后台服务");
        ui.text_edit_multiline(&mut t.material_action);
        if ui.button("执行本地资料操作").clicked() {
            match serde_json::from_str(&t.material_action) {
                Ok(value) => {
                    action = Some(value);
                    t.material_error.clear();
                }
                Err(e) => t.material_error = e.to_string(),
            };
        }
        if !t.material_error.is_empty() {
            ui.colored_label(egui::Color32::YELLOW, &t.material_error);
        }
        ui.horizontal(|ui| {
            ui.label("图片文件ID");
            ui.text_edit_singleline(&mut t.selected_document);
        });
        if ui.button("通过worker读取原图").clicked() {
            action = Some(MaterialAction::Inspect {
                document: Some(t.selected_document.clone()),
            });
        }
        if details["image_preview"].is_object() && ui.button("载入已核验图片预览").clicked()
        {
            let info = &details["image_preview"];
            if let (Some(w), Some(h), Ok(bytes)) = (
                info["preview_width"].as_u64(),
                info["preview_height"].as_u64(),
                serde_json::from_value::<Vec<u8>>(info["rgba"].clone()),
            ) && bytes.len() == w as usize * h as usize * 4
            {
                t.texture = Some(ui.ctx().load_texture(
                    "history-original",
                    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &bytes),
                    Default::default(),
                ));
                t.image_info = info.clone();
            }
        }
        if let Some(texture) = &t.texture {
            let response = ui.add(
                egui::Image::new(texture)
                    .max_width(ui.available_width())
                    .sense(egui::Sense::drag()),
            );
            let original_width = t.image_info["width"].as_u64().unwrap_or(1) as f32;
            let original_height = t.image_info["height"].as_u64().unwrap_or(1) as f32;
            for reading in &r.transcriptions {
                if Some(reading.document.as_str()) == t.image_info["document"].as_str() {
                    let [x, y, w, h] = reading.region;
                    let start = response.rect.left_top()
                        + egui::vec2(
                            x as f32 / original_width * response.rect.width(),
                            y as f32 / original_height * response.rect.height(),
                        );
                    let size = egui::vec2(
                        w as f32 / original_width * response.rect.width(),
                        h as f32 / original_height * response.rect.height(),
                    );
                    ui.painter().rect_stroke(
                        egui::Rect::from_min_size(start, size),
                        0.0,
                        egui::Stroke::new(1.0, egui::Color32::YELLOW),
                        egui::StrokeKind::Inside,
                    );
                }
            }
            if let (Some(start), Some(end)) = (t.drag_start, response.interact_pointer_pos()) {
                ui.painter().rect_stroke(
                    egui::Rect::from_two_pos(start, end).intersect(response.rect),
                    0.0,
                    egui::Stroke::new(2.0, egui::Color32::GREEN),
                    egui::StrokeKind::Inside,
                );
            }
            if response.drag_started() {
                t.drag_start = response.interact_pointer_pos();
            }
            if response.drag_stopped()
                && let (Some(start), Some(end)) =
                    (t.drag_start.take(), response.interact_pointer_pos())
            {
                let rect = response.rect;
                let w = t.image_info["width"].as_u64().unwrap_or(0) as f32;
                let h = t.image_info["height"].as_u64().unwrap_or(0) as f32;
                let pos = |p: egui::Pos2| {
                    [
                        ((p.x - rect.left()) / rect.width() * w).clamp(0.0, w) as u32,
                        ((p.y - rect.top()) / rect.height() * h).clamp(0.0, h) as u32,
                    ]
                };
                let a = pos(start);
                let b = pos(end);
                let region = [
                    a[0].min(b[0]),
                    a[1].min(b[1]),
                    a[0].abs_diff(b[0]),
                    a[1].abs_diff(b[1]),
                ];
                r.transcriptions
                    .push(analytics_storage::membership_history::Transcription {
                        id: format!("reading-{}", r.transcriptions.len()),
                        document: t.image_info["document"].as_str().unwrap_or_default().into(),
                        sha256: t.image_info["sha256"].as_str().unwrap_or_default().into(),
                        region,
                        text: String::new(),
                        reviewer: String::new(),
                        reviewed_at: None,
                        confirmed: false,
                        receipt: None,
                    });
            }
            ui.label("在图上拖选原图区域；坐标按原始像素保存。调整区域或文字后必须重新核对。");
        }
        for (i, reading) in r.transcriptions.iter_mut().enumerate() {
            ui.push_id(("reading", i), |ui| {
                ui.text_edit_singleline(&mut reading.id);
                ui.text_edit_singleline(&mut reading.document);
                ui.horizontal(|ui| {
                    ui.label("x y 宽 高");
                    for n in &mut reading.region {
                        ui.add(egui::DragValue::new(n));
                    }
                });
                ui.text_edit_multiline(&mut reading.text);
                ui.label(format!(
                    "审核：{} {:?}；引用绑定：{}",
                    reading.reviewer,
                    reading.reviewed_at,
                    reading.receipt.is_some()
                ));
            });
        }
        ui.horizontal(|ui| {
            ui.label("核对ID / 审核人");
            ui.text_edit_singleline(&mut t.transcription_id);
            ui.text_edit_singleline(&mut t.reviewer);
        });
        if ui.button("确认所选区域含义并生成绑定收据").clicked() {
            action = Some(MaterialAction::Review {
                transcription: t.transcription_id.clone(),
                reviewer: t.reviewer.clone(),
            });
        }
        if ui.button("只读资料状态").clicked() {
            action = Some(MaterialAction::Inspect { document: None });
        }
        if let Some(states) = details["material_status"].as_array() {
            for status in states {
                ui.label(status.to_string());
            }
        }
    });
    action
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
            ui.horizontal(|ui| {
                ui.label("同上下文预览任务对比：之前 / 之后");
                ui.text_edit_singleline(&mut t.compare_before);
                ui.text_edit_singleline(&mut t.compare_after);
            });
            if ui.button("比较解除、剩余、新增与覆盖变化").clicked() {
                match (t.compare_before.parse(), t.compare_after.parse()) {
                    (Ok(before), Ok(after)) => op = Some(Operation::Compare { before, after }),
                    _ => state.error = Some("请输入两个预览任务UUID".into()),
                }
            }
            if state.details.get("newly_proven_dates").is_some() {
                ui.label(state.details.to_string());
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
                if let Some(action) = materials(ui, t, &mut r, &state.details) {
                    op = Some(Operation::Material {
                        request: Box::new(r.clone()),
                        action,
                    });
                }
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
                    ui.selectable_value(&mut t.capture_format, "png".into(), "PNG");
                    ui.selectable_value(&mut t.capture_format, "jpeg".into(), "JPEG");
                });
            if ui.button("联网捕获指定原文").clicked() {
                op = Some(Operation::Capture {
                    url: t.url.clone(),
                    format: t.capture_format.clone(),
                });
            }
            if let Some(report) = state.details.get("history_report") {
                ui.label(format!(
                    "事实连续区间：{}；未证明日期：{}；当前诊断：{}",
                    report["segments"].as_array().map_or(0, Vec::len),
                    report["missing_dates"].as_array().map_or(0, Vec::len),
                    report["issues"].as_array().map_or(0, Vec::len)
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
                ui.collapsing("历史追溯（不作为当前事实缺项）", |ui| {
                    if let Some(trace) = report["trace"].as_array() {
                        for issue in trace {
                            ui.label(issue.to_string());
                        }
                    }
                });
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
