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
            if ui.button("查看所选版本证据缺口").clicked(){submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::Status{actions:state.platform.actions.clone(),instrument:None}});}
            if ui.button("查看本证券缺口").clicked(){submit(state,worker,cancel_slot,WorkflowRequest::Evidence{request:EvidenceRequest::Status{actions:state.platform.actions.clone(),instrument:Some(state.platform.action_review_instrument.clone())}});}
        });
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
        if let Some(manifest)=details["manifest"].as_str() && ui.button("使用本次报告的公告manifest").clicked(){state.platform.action_evidence_manifest=manifest.into();}
        if let Some(draft)=details.get("draft_request") {
            ui.label(format!("草稿校验：{}",details["validation"]));
            if ui.button("将本次草稿载入修订编辑器").clicked(){state.platform.action_review_editor=serde_json::to_string_pretty(draft).unwrap_or_default();}
        }
        ui.label("草稿不会发布新版本。核对逐字段引用后，使用下方“提交证据修订为新版本”；CLI 可直接使用报告中的 request_path。");
    });
}
