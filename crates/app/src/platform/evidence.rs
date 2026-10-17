//! Evidence maintenance controls; business logic stays in the shared worker.
use super::*;
use analytics_storage::corporate_actions::evidence::{CaptureRequest, DocumentSpec};
use analytics_workflow::action_evidence::EvidenceRequest;
#[derive(Default)]
pub struct EvidenceTools {
    id: String,
    url: String,
    title: String,
    announced: String,
    implementation: bool,
    recover_task: String,
    status_task: String,
    manifests: String,
    documents: Vec<serde_json::Value>,
    field_name: String,
    market_editor: String,
}
pub(super) fn controls(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
) {
    ui.collapsing("证据维护：公告留存、批次恢复与修订草稿",|ui| {
        ui.label("合并已核验公告manifest，每行一个；同一证券可追加选定文档，多个不同完整查询须分别审核。");
        ui.text_edit_multiline(&mut state.platform.evidence_tools.manifests);
        if ui.button("合并已核验证据").clicked() {
            let manifests=state.platform.evidence_tools.manifests.lines().map(str::trim).filter(|s|!s.is_empty()).map(PathBuf::from).collect();
            submit(state,worker,cancel_slot,WorkflowRequest::Evidence {request:EvidenceRequest::Combine {manifests}});
        }
        ui.collapsing("行情缺口补录（原快照保持不变）",|ui| {
            ui.label("选择已采集的行情、日期和字段；研究口径验证失败时不能发布。行情变化后重算研究结果。");
            if ui.button("载入行情补录请求JSON").clicked() && let Some(path)=rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file() {match fs::read_to_string(path){Ok(text)=>state.platform.evidence_tools.market_editor=text,Err(e)=>state.platform.error=Some(e.to_string())}}
            ui.text_edit_multiline(&mut state.platform.evidence_tools.market_editor);
            if ui.button("预览研究与执行行情补录").clicked() && valid_task(state) {match serde_json::from_str(&state.platform.evidence_tools.market_editor){Ok(request)=>submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::MarketDraft{market:state.platform.market.clone(),request:Box::new(request),task:state.platform.evidence_tools.status_task.trim().parse().ok()}}),Err(e)=>state.platform.error=Some(e.to_string())}}
            if ui.button("重新核验并发布行情新版本").clicked(){match serde_json::from_str(&state.platform.evidence_tools.market_editor){Ok(request)=>submit(state,worker,cancel_slot,WorkflowRequest::ReviseMarket{market:state.platform.market.clone(),request}),Err(e)=>state.platform.error=Some(e.to_string())}}
            let report=state.platform.details.get("details").unwrap_or(&state.platform.details);
            if report.get("request_sha256").is_some(){
                let stale=market_preview_stale(report,&state.platform.market,&state.platform.evidence_tools.market_editor,&state.platform.evidence_tools.status_task);
                if stale {ui.colored_label(egui::Color32::YELLOW,"行情预览已过期：请重新核验");}
                ui.label(format!("研究可用：{}；执行可用：{}",report["research_ready"],report["execution_ready"]));
                ui.collapsing("候选单元格与原价／研究值",|ui|{ui.label(report["cells"].to_string());});
                ui.collapsing("未解决问题",|ui|{ui.label(report["problems"].to_string());});
            }
        });
        ui.label("选定公告只补充该文档，不代表整段历史完整。原始版本与失败任务记录保留。");
        ui.horizontal(|ui| {
            if ui.button("查看所选版本证据缺口").clicked(){submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::Status{actions:state.platform.actions.clone(),instrument:None,task:None}});}
            if ui.button("查看本证券缺口").clicked(){submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::Status{actions:state.platform.actions.clone(),instrument:Some(state.platform.action_review_instrument.clone()),task:None}});}
        });
        ui.horizontal(|ui| {
            ui.label("当前依赖检查的已结束任务ID");
            ui.text_edit_singleline(&mut state.platform.evidence_tools.status_task);
            if ui.button("检查本任务当前阻断").clicked() {
                match state.platform.evidence_tools.status_task.trim().parse() {
                    Ok(task) => submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::Status{actions:state.platform.actions.clone(),instrument:None,task:Some(task)}}),
                    Err(_) => state.platform.error=Some("任务ID无效；请选择已结束的训练或回测检查任务".into()),
                }
            }
        });
        ui.label("不指定任务的结果是证据清单；当前依赖通过不代表完整训练或回测通过。");
        ui.label("证券使用下方证券输入；URL 必须为原始 PDF。标题摘录与证券代码须出现在正文中。");
        let tools=&mut state.platform.evidence_tools;
        egui::Grid::new("evidence_document_form").show(ui,|ui| {
            for (label,value) in [("文档ID",&mut tools.id),("公开HTTPS URL",&mut tools.url),("原文标题摘录",&mut tools.title),("披露日期 YYYY-MM-DD",&mut tools.announced)] {ui.label(label);ui.text_edit_singleline(value);ui.end_row();}
        });
        ui.checkbox(&mut tools.implementation,"已人工确认属于实施类公告（否则保留为未核实候选）");
        if ui.button("留存选定公告与提取文本").clicked() {
            let t=&state.platform.evidence_tools;
            let parsed=t.announced.parse().map_err(|_|"披露日期无效".to_string()).and_then(|announced| {
                let request=CaptureRequest{schema_version:1,documents:vec![DocumentSpec{id:t.id.clone(),instrument:state.platform.action_review_instrument.clone(),url:t.url.clone(),announced,title:t.title.clone(),implementation:t.implementation}]};
                request.validate().map_err(|e|e.to_string())?;Ok(request)
            });
            match parsed {Ok(request)=>submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::Capture{request}}),Err(e)=>state.platform.error=Some(e)}
        }
        ui.horizontal(|ui| {
            ui.label("已结束的采集任务ID");ui.text_edit_singleline(&mut state.platform.evidence_tools.recover_task);
            if ui.button("核验并恢复已完成证据").clicked(){match state.platform.evidence_tools.recover_task.trim().parse(){Ok(task)=>submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::Recover{task}}),Err(_)=>state.platform.error=Some("采集任务ID无效".into())}}
        });
        if ui.button("仅导入停复牌证据，不重新协调行动条款").clicked(){match state.platform.evidence_tools.recover_task.trim().parse(){Ok(task)=>submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::ImportStatus{actions:state.platform.actions.clone(),task}}),Err(_)=>state.platform.error=Some("原采集任务ID无效".into())}}
        if ui.button("按原请求重试采集并复用检查点").clicked(){match state.platform.evidence_tools.recover_task.trim().parse(){Ok(task)=>submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::Retry{task}}),Err(_)=>state.platform.error=Some("原采集任务ID无效".into())}}
        ui.horizontal(|ui| {
            if ui.button("生成本证券修订草稿").clicked(){if !valid_task(state){return;}submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::Draft{task:state.platform.evidence_tools.status_task.trim().parse().ok(),actions:state.platform.actions.clone(),request:None,instrument:Some(state.platform.action_review_instrument.clone()),evidence_manifest:Some(PathBuf::from(&state.platform.action_evidence_manifest))}});}
            if ui.button("核验并迁移编辑中的修订").clicked(){if !valid_task(state){return;}match serde_json::from_str(&state.platform.action_review_editor){Ok(request)=>submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::Draft{task:state.platform.evidence_tools.status_task.trim().parse().ok(),actions:state.platform.actions.clone(),request:Some(Box::new(request)),instrument:None,evidence_manifest:None}}),Err(e)=>state.platform.error=Some(e.to_string())}}
            if ui.button("载入已保存修订JSON").clicked() && let Some(path)=rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file(){match fs::read_to_string(path){Ok(text)=>state.platform.action_review_editor=text,Err(e)=>state.platform.error=Some(e.to_string())}}
        });
        let report=&state.platform.details;
        // Running a different task replaces details; never auto-select an old report or publish it.
        let details=report.get("details").unwrap_or(report);
        if let Some(current)=details.get("current").filter(|v|v.is_object()) {
            let stale=report_stale(details,&state.platform.actions,&state.platform.evidence_tools.status_task,&state.platform.market);
            if stale { ui.colored_label(egui::Color32::YELLOW,"报告已过期：行动版本或任务选择已变化，请重新检查。"); }
            else { ui.label(format!("当前公司行动依赖：{}；缺口 {} 条；阻断检查 {} 项；影响证券 {} 只",if current["ready"]==true {"可用"} else {"未解决"},current["gap_count"],current["blocking_check_count"],current["affected_security_count"])); }
            if let Some(entries)=current["entries"].as_array() {
                ui.collapsing("当前覆盖与事件问题",|ui| {
                    for entry in entries {
                        let c=&entry["coverage"];
                        let label=match c["status"].as_str(){Some("available")=>"覆盖可用",Some("partial")=>"部分覆盖",_=>"覆盖阻断"};
                        ui.collapsing(format!("{} · {} · {}",c["instrument"].as_str().unwrap_or(""),entry["purpose"].as_str().unwrap_or(""),label),|ui| {
                            ui.label(format!("任务区间：{}",entry["ranges"]));
                            ui.label(format!("剩余日期与类别：{}",c["gaps"]));
                            ui.label(format!("解决证据：{}",c["evidence"]));
                            if let Some(issues)=entry.get("event_issues") {ui.label(format!("事件问题：{issues}"));}
                        });
                    }
                });
                ui.collapsing("历史采集失败（不直接计入当前阻断）",|ui| {
                    for entry in entries.iter().filter(|e|!e["coverage"]["historical_failure"].is_null()) {ui.label(format!("{}：{}",entry["coverage"]["instrument"],entry["coverage"]["historical_failure"]));}
                });
            }
            ui.collapsing("逐项补证清单",|ui| {
                for item in current["checklist"].as_array().into_iter().flatten() {
                    ui.collapsing(format!("{} · {} · {}",item["instrument"],item["kind"],item["required_fact"]),|ui| {
                        ui.label(format!("区间：{}；已有证据：{}",item["dates"],item["existing_evidence"]));
                        ui.label(format!("原因：{}；下一步：{}",item["reason"],item["next_action"]));
                        ui.small("历史覆盖缺口不代表清单中的事件字段均缺失；须核验后分别判断。");
                    });
                }
            });
            ui.collapsing("当前事件检查与补齐变化",|ui| { ui.label(format!("{}",current["checks"]));ui.label(format!("{}",current["comparison"])); });
        }
        ui.collapsing("完整历史覆盖审核",|ui| {
            ui.label("使用下方修订JSON中的 coverage_reviews 编辑证券、日期、类别、全部候选文档ID与审核理由。完整独立证据可解除当前覆盖缺口，原供应商失败保留。单份选定公告不支持完整历史审核。");
            if let Ok(request)=serde_json::from_str::<analytics_storage::corporate_actions::review::RevisionRequest>(&state.platform.action_review_editor) {
                for c in &request.coverage_reviews {ui.label(format!("{}：{}～{}，{} 个类别，{} 份候选；{}",c.instrument,c.dates.start,c.dates.end,c.categories.len(),c.document_ids.len(),c.reason));}
            }
            ui.label("编辑后使用“核验并迁移编辑中的修订”；校验通过后再提交新版本。覆盖补齐不会自动解决已知事件问题。");
        });
        if let Some(manifest)=details["manifest"].as_str() && ui.button("使用本次报告的公告manifest").clicked(){state.platform.action_evidence_manifest=manifest.into();}
        if let Some(draft)=details.get("draft_request") {
            if draft_stale(details,&state.platform.actions,&state.platform.action_review_editor,&state.platform.evidence_tools.status_task) {ui.colored_label(egui::Color32::YELLOW,"草稿校验已过期或尚未载入：请载入后重新核验；发布始终再次验证。");} else {ui.label(format!("草稿校验：{}",details["validation"]));}
            ui.collapsing("逐项校验结果",|ui|{for problem in details["validation"]["problems"].as_array().into_iter().flatten(){ui.label(format!("{} · {} · {}：{}",problem["instrument"],problem["field"],problem["kind"],problem["reason"]));}});
            if ui.button("将本次草稿载入修订编辑器").clicked(){state.platform.action_review_editor=serde_json::to_string_pretty(draft).unwrap_or_default();state.platform.evidence_tools.documents=details["documents"].as_array().cloned().unwrap_or_default();}
        }
        ui.label("草稿不会发布新版本。核对逐字段引用后，使用下方“提交证据修订为新版本”；CLI 可直接使用报告中的 request_path。");
    });
}

fn market_preview_stale(
    report: &serde_json::Value,
    market: &str,
    editor: &str,
    task: &str,
) -> bool {
    report["market"].as_str() != Some(market)
        || serde_json::from_str::<serde_json::Value>(editor)
            .ok()
            .as_ref()
            != Some(&report["request"])
        || report["task"].as_str().unwrap_or("") != task.trim()
}

fn report_stale(report: &serde_json::Value, actions: &str, task: &str, market: &str) -> bool {
    report["actions"].as_str() != Some(actions)
        || report["current"]["task"].as_str() != Some(task.trim())
        || (!market.is_empty()
            && report["current"]["market_versions"]
                .as_array()
                .is_some_and(|vs| !vs.is_empty() && !vs.iter().any(|v| v.as_str() == Some(market))))
}
/// Edit facts in the existing revision draft; publication still requires field evidence.
pub(super) fn event_terms_editor(
    ui: &mut egui::Ui,
    editor: &mut String,
    tools: &mut EvidenceTools,
) {
    let Ok(mut request) = serde_json::from_str::<serde_json::Value>(editor) else {
        return;
    };
    if ui.button("新增换股事件草稿（事实待填写）").clicked() {
        if let Some(events) = request["events"].as_array_mut() {
            events.push(analytics_storage::corporate_actions::evidence::conversion_template(""));
        }
        request["schema_version"] = 5.into();
        *editor = serde_json::to_string_pretty(&request).unwrap();
    }
    let mut status_changed = false;
    ui.collapsing("公告停复牌区间审核",|ui| {
        ui.small("区间从停牌日开市开始，到复牌日开市前结束；必须分别关联原始边界引用，不修改行情。");
        if ui.button("新增有边界证据的停牌区间").clicked(){if !request["suspensions"].is_array(){request["suspensions"]=serde_json::json!([]);}request["suspensions"].as_array_mut().unwrap().push(serde_json::json!({"instrument":"","start":"","resume":"","reason":"","start_evidence":{"document_id":"","quote":"","unit":"session_open"},"resume_evidence":{"document_id":"","quote":"","unit":"session_open"}}));status_changed=true;}
        for (i,item) in request["suspensions"].as_array_mut().into_iter().flatten().enumerate(){ui.push_id(("suspension",i),|ui|{
            for (key,label) in [("instrument","证券"),("start","停牌首日"),("resume","复牌首日"),("reason","审核理由")] {let mut value=item[key].as_str().unwrap_or("").to_owned();ui.label(label);if ui.text_edit_singleline(&mut value).changed(){item[key]=value.into();status_changed=true;}}
            ui.push_id("start",|ui|{ui.label("停牌原文");status_changed|=proof_editor(ui,&mut item["start_evidence"],&tools.documents);});
            ui.push_id("resume",|ui|{ui.label("复牌原文");status_changed|=proof_editor(ui,&mut item["resume_evidence"],&tools.documents);});
        });}
    });
    if status_changed {
        request["schema_version"] = 5.into();
        *editor = serde_json::to_string_pretty(&request).unwrap();
    }
    let Some(events) = request["events"].as_array_mut() else {
        return;
    };
    let mut changed = false;
    ui.collapsing("迁码、换股与退市条款维护", |ui| {
        ui.small("这里维护事实，不写入模拟舍零或清算假设。修改后仍须补齐 fields 中的公告原文、文档ID和单位；发布会重新核验。");
        for (i, revision) in events.iter_mut().enumerate() {
            let kind = revision["replacement"]["terms"]["kind"].as_str().unwrap_or("").to_owned();
            if !matches!(kind.as_str(), "conversion" | "delisting") { continue; }
            ui.push_id(i, |ui| {
                let mut reason=revision["reason"].as_str().unwrap_or("").to_owned();
                ui.label("审核理由");if ui.text_edit_singleline(&mut reason).changed(){revision["reason"]=reason.into();changed=true;}
                let event = &mut revision["replacement"];
                for (field,label) in [("instrument","原证券"),("id","事件ID"),("announced","方案披露日"),("record_date","权益登记日"),("ex_date","权益转换生效日"),("source","来源URL清单")] {
                    let mut value=event[field].as_str().unwrap_or("").to_owned();ui.horizontal(|ui|{ui.label(label);if ui.text_edit_singleline(&mut value).changed(){event[field]=if value.trim().is_empty(){serde_json::Value::Null}else{value.into()};changed=true;}});
                }
                ui.label(format!("{} · {}", event["instrument"], kind));
                for (field, label) in [("last_trade_date", "最后实际交易日"), ("successor", "后继证券"), ("ratio", "换股比例"), ("listing_date", "后继股份可交易日")] {
                    if kind == "delisting" && field != "last_trade_date" { continue; }
                    let mut value = event["terms"][field].as_str().unwrap_or("").to_owned();
                    ui.horizontal(|ui| {
                        ui.label(label);
                        if ui.text_edit_singleline(&mut value).changed() {
                            event["terms"][field] = if value.trim().is_empty() { serde_json::Value::Null } else { serde_json::Value::String(value) };
                            changed = true;
                        }
                    });
                }
                ui.small(format!("权益生效／终止日：{}；登记日：{}；公告：{}", event["ex_date"], event["record_date"], event["source"]));
                // End the event borrow before editing sibling evidence fields.
                let event_id=event["id"].clone();
                let predecessor=event["instrument"].clone();
                let successor=event["terms"]["successor"].clone();
                if kind=="conversion" && revision.get("participants").is_none() && ui.button("添加有证据的参与方关联").clicked() {
                    revision["participants"]=serde_json::json!({"event_id":event_id,"predecessor":{"instrument":predecessor,"name":"","identity":{"document_id":"","quote":"","unit":"issuer_identity"}},"successor":{"instrument":successor,"name":"","identity":{"document_id":"","quote":"","unit":"issuer_identity"}},"relationship":[]});
                    changed=true;
                }
                if let Some(parties)=revision.get_mut("participants") {
                    ui.collapsing("跨证券关联原文",|ui| {
                        for key in ["predecessor","successor"] {
                            ui.push_id(key,|ui| {
                                ui.label(format!("{}：{}",key,parties[key]["instrument"]));
                                let mut name=parties[key]["name"].as_str().unwrap_or("").to_owned();
                                if ui.text_edit_singleline(&mut name).changed(){parties[key]["name"]=name.into();changed=true;}
                                changed|=proof_editor(ui,&mut parties[key]["identity"],&tools.documents);
                            });
                        }
                        if ui.button("添加双方交易关系引用").clicked(){parties["relationship"].as_array_mut().unwrap().push(serde_json::json!({"document_id":"","quote":"","unit":"conversion_relationship"}));changed=true;}
                        for (j,proof) in parties["relationship"].as_array_mut().into_iter().flatten().enumerate(){ui.push_id(j,|ui|changed|=proof_editor(ui,proof,&tools.documents));}
                    });
                }
                ui.collapsing("逐字段证据维护",|ui| {
                    ui.horizontal(|ui|{ui.label("字段路径");ui.text_edit_singleline(&mut tools.field_name);
                        if !tools.field_name.trim().is_empty() && ui.button("添加字段引用").clicked(){revision["fields"][tools.field_name.trim()]=serde_json::json!({"document_id":"","quote":"","unit":""});changed=true;}
                    });
                    for (path,proof) in revision["fields"].as_object_mut().into_iter().flatten(){ui.push_id(path,|ui|{ui.label(path);changed|=proof_editor(ui,proof,&tools.documents);});}
                });

            });
        }
    });
    if changed {
        request["schema_version"] = 5.into();
        *editor = serde_json::to_string_pretty(&request).unwrap();
    }
}

fn proof_editor(
    ui: &mut egui::Ui,
    proof: &mut serde_json::Value,
    documents: &[serde_json::Value],
) -> bool {
    let mut changed = false;
    let mut id = proof["document_id"].as_str().unwrap_or("").to_owned();
    egui::ComboBox::from_id_salt("proof_document")
        .selected_text(if id.is_empty() {
            "选择已采集文档"
        } else {
            &id
        })
        .show_ui(ui, |ui| {
            for item in documents {
                let d = item.get("document").unwrap_or(item);
                if let Some(value) = d["id"].as_str() {
                    changed |= ui
                        .selectable_value(
                            &mut id,
                            value.to_owned(),
                            format!("{} · {} · {}", d["instrument"], value, d["title"]),
                        )
                        .changed();
                }
            }
        });
    ui.horizontal(|ui| {
        ui.label("文档ID");
        changed |= ui.text_edit_singleline(&mut id).changed();
    });
    proof["document_id"] = id.clone().into();
    for (key, label) in [("quote", "精确原文引用"), ("unit", "单位／含义")] {
        let mut text = proof[key].as_str().unwrap_or("").to_owned();
        ui.label(label);
        let response = if key == "quote" {
            ui.text_edit_multiline(&mut text)
        } else {
            ui.text_edit_singleline(&mut text)
        };
        if response.changed() {
            proof[key] = text.into();
            changed = true;
        }
    }
    if let Some(item) = documents.iter().find(|v| v["document"]["id"] == id) {
        ui.collapsing("查看原始提取文本", |ui| {
            ui.label(item["extracted_text"].as_str().unwrap_or(""));
        });
    }
    changed
}

fn draft_stale(report: &serde_json::Value, actions: &str, editor: &str, task: &str) -> bool {
    report["actions"].as_str() != Some(actions)
        || serde_json::from_str::<serde_json::Value>(editor)
            .ok()
            .as_ref()
            != Some(&report["draft_request"])
        || report["validation_receipt"]["task"].as_str().unwrap_or("") != task.trim()
}

fn valid_task(state: &mut AppState) -> bool {
    let task = state.platform.evidence_tools.status_task.trim();
    if !task.is_empty() && task.parse::<uuid::Uuid>().is_err() {
        state.platform.error = Some("任务ID无效；请修正或清空后再核验草稿".into());
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn market_preview_tracks_request_market_and_task() {
        let report = serde_json::json!({"market":"m1","request":{"schema_version":1},"task":"t1"});
        let editor = report["request"].to_string();
        assert!(!market_preview_stale(&report, "m1", &editor, "t1"));
        assert!(market_preview_stale(&report, "m2", &editor, "t1"));
        assert!(market_preview_stale(&report, "m1", "{}", "t1"));
        assert!(market_preview_stale(&report, "m1", &editor, "t2"));
    }
    #[test]
    fn draft_receipt_expires_when_editor_actions_or_task_changes() {
        let draft = serde_json::json!({"schema_version":4,"events":[]});
        let report = serde_json::json!({"actions":"a1","draft_request":draft,"validation_receipt":{"task":"t1"}});
        assert!(!draft_stale(&report, "a1", &draft.to_string(), "t1"));
        assert!(draft_stale(&report, "a2", &draft.to_string(), "t1"));
        assert!(draft_stale(&report, "a1", "{}", "t1"));
        assert!(draft_stale(&report, "a1", &draft.to_string(), "t2"));
    }
    #[test]
    fn changing_selected_context_marks_report_stale() {
        let report = serde_json::json!({"actions":"v1","current":{"task":"task1","market_versions":["m1"],"ready":true}});
        assert!(!report_stale(&report, "v1", "task1", "m1"));
        assert!(report_stale(&report, "v2", "task1", "m1"));
        assert!(report_stale(&report, "v1", "task2", "m1"));
        assert!(report_stale(&report, "v1", "task1", "m2"));
    }
}
