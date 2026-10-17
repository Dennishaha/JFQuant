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
}
pub(super) fn controls(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
) {
    ui.collapsing("证据维护：公告留存、批次恢复与修订草稿",|ui| {
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
        if ui.button("按原请求重试采集并复用检查点").clicked(){match state.platform.evidence_tools.recover_task.trim().parse(){Ok(task)=>submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::Retry{task}}),Err(_)=>state.platform.error=Some("原采集任务ID无效".into())}}
        ui.horizontal(|ui| {
            if ui.button("生成本证券修订草稿").clicked(){submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::Draft{actions:state.platform.actions.clone(),request:None,instrument:Some(state.platform.action_review_instrument.clone()),evidence_manifest:Some(PathBuf::from(&state.platform.action_evidence_manifest))}});}
            if ui.button("核验并迁移编辑中的修订").clicked(){match serde_json::from_str(&state.platform.action_review_editor){Ok(request)=>submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::Draft{actions:state.platform.actions.clone(),request:Some(Box::new(request)),instrument:None,evidence_manifest:None}}),Err(e)=>state.platform.error=Some(e.to_string())}}
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
            ui.label(format!("草稿校验：{}",details["validation"]));
            if ui.button("将本次草稿载入修订编辑器").clicked(){state.platform.action_review_editor=serde_json::to_string_pretty(draft).unwrap_or_default();}
        }
        ui.label("草稿不会发布新版本。核对逐字段引用后，使用下方“提交证据修订为新版本”；CLI 可直接使用报告中的 request_path。");
    });
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
pub(super) fn event_terms_editor(ui: &mut egui::Ui, editor: &mut String) {
    let Ok(mut request) = serde_json::from_str::<serde_json::Value>(editor) else {
        return;
    };
    let Some(events) = request["events"].as_array_mut() else {
        return;
    };
    let mut changed = false;
    ui.collapsing("迁码、换股与退市条款维护", |ui| {
        ui.small("这里维护事实，不写入模拟舍零或清算假设。修改后仍须补齐 fields 中的公告原文、文档ID和单位；发布会重新核验。");
        for (i, revision) in events.iter_mut().enumerate() {
            let event = &mut revision["replacement"];
            let kind = event["terms"]["kind"].as_str().unwrap_or("").to_owned();
            if !matches!(kind.as_str(), "conversion" | "delisting") { continue; }
            ui.push_id(i, |ui| {
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
            });
        }
    });
    if changed {
        *editor = serde_json::to_string_pretty(&request).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changing_selected_context_marks_report_stale() {
        let report = serde_json::json!({"actions":"v1","current":{"task":"task1","market_versions":["m1"],"ready":true}});
        assert!(!report_stale(&report, "v1", "task1", "m1"));
        assert!(report_stale(&report, "v2", "task1", "m1"));
        assert!(report_stale(&report, "v1", "task2", "m1"));
        assert!(report_stale(&report, "v1", "task1", "m2"));
    }
}
