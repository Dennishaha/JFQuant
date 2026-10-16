//! Formula editing and shared library RPC adapter; preview computation stays in the worker.
use crate::{
    state::AppState,
    worker::{WorkerCommand, WorkerHandle},
};
use analytics_factor::operators::OperatorRegistry;
use analytics_workflow::research::pipeline::{Cancellation, cancellation};
use analytics_workflow::service::Rpc;

pub(crate) fn request(state: &mut AppState, rpc: Rpc) {
    if state.factor.library_reply.is_some() {
        return;
    }
    let Some(workspace) = state.platform.workspace.clone() else {
        state.factor.error = Some("请先打开工作区".into());
        return;
    };
    state.factor.library_root = Some(workspace.root().to_owned());
    let (tx, rx) = std::sync::mpsc::channel();
    #[cfg(test)]
    {
        let _ = tx.send(
            analytics_workflow::service::factor_library_call(&workspace, rpc)
                .map_err(|e| e.to_string()),
        );
    }
    #[cfg(not(test))]
    std::thread::spawn(move || {
        let result = analytics_workflow::service::Client::connect(workspace.root())
            .and_then(|c| {
                c.ensure(None)?;
                c.call(rpc)
            })
            .map_err(|e| e.to_string());
        let _ = tx.send(result);
    });
    state.factor.library_reply = Some(rx);
}
pub(crate) fn poll(ui: &mut egui::Ui, state: &mut AppState) {
    let received = state
        .factor
        .library_reply
        .as_ref()
        .and_then(|rx| match rx.try_recv() {
            Ok(v) => Some(v),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err("因子库连接中断".into())),
            Err(_) => None,
        });
    if let Some(result) = received {
        state.factor.library_reply = None;
        if state.factor.library_root.as_deref()
            != state.platform.workspace.as_ref().map(|w| w.root())
        {
            state.factor.library_root = None;
            state.factor.group_root = None;
            return;
        }
        match result {
            Err(e) => state.factor.error = Some(e),
            Ok(v) => {
                state.factor.error = None;
                let update = || -> Result<_, String> {
                    let groups = v
                        .get("groups")
                        .map(|g| serde_json::from_value(g.clone()).map_err(|e| e.to_string()))
                        .transpose()?;
                    let factors = v
                        .get("factors")
                        .map(|g| serde_json::from_value(g.clone()).map_err(|e| e.to_string()))
                        .transpose()?;
                    Ok((groups, factors))
                };
                match update() {
                    Ok((groups, factors)) => {
                        if let Some(g) = groups {
                            state.factor.groups = g;
                        }
                        if let Some(f) = factors {
                            state.factor.custom_factors = f;
                        }
                    }
                    Err(e) => state.factor.error = Some(e),
                }
                if let Some(saved) = v.get("saved_factor") {
                    match serde_json::from_value::<
                        analytics_factor::selection::CustomFactorDefinition,
                    >(saved.clone())
                    {
                        Ok(d) => {
                            state.factor.draft = analytics_workflow::custom_factors::FactorDraft {
                                id: Some(d.id),
                                expected_revision: Some(d.revision),
                                name: d.name,
                                description: d.description,
                                formula: d.formula,
                            };
                            state.factor.error = None;
                        }
                        Err(e) => state.factor.error = Some(e.to_string()),
                    }
                }
                if let Some(compiled) = v.get("compiled") {
                    match serde_json::from_value(compiled.clone()) {
                        Ok(c) => {
                            state.factor.compiled = Some(c);
                            state.factor.compiled_source = state.factor.draft.formula.clone();
                            state.factor.error = None;
                        }
                        Err(e) => state.factor.error = Some(e.to_string()),
                    }
                }
                if let Some(id) = v.get("archived_group").and_then(|v| v.as_str())
                    && state.factor.group.as_ref().is_some_and(|g| g.id == id)
                {
                    state.factor.group = None;
                }
            }
        }
    }
    if state.factor.library_reply.is_some() {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
        ui.small("正在同步因子库…");
    }
}
pub(crate) fn editor(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
) {
    ui.collapsing("新建 / 编辑自定义因子",|ui| {
        let busy=state.factor.library_reply.is_some();
        let mut formula_editor=None;
        ui.add_enabled_ui(!busy,|ui| {
            if ui.button("新建因子").clicked(){state.factor.draft=Default::default();state.factor.compiled=None;state.factor.preview=None;}
            if let Some(id)=&state.factor.draft.id {ui.small(format!("{} · 当前修订{}；保存将创建新修订",id,state.factor.draft.expected_revision.unwrap_or(0)));}
            ui.horizontal(|ui|{ui.label("因子名称");ui.text_edit_singleline(&mut state.factor.draft.name);});
            ui.horizontal(|ui|{ui.label("因子说明");ui.text_edit_singleline(&mut state.factor.draft.description);});
            let output=egui::TextEdit::multiline(&mut state.factor.draft.formula).id_salt("custom_factor_formula").code_editor().desired_rows(3).desired_width(f32::INFINITY).hint_text("CLOSE / GTJA_DELAY(CLOSE, 20) - 1").show(ui);
            formula_editor=Some(output.response.id);
            ui.small("输入：OPEN/HIGH/LOW/CLOSE/VOLUME/AMOUNT/VWAP/RET。算子使用GTJA_或QLIB_前缀；不引用其他因子。四则运算使用 + - * /，采用GTJA空值传播规则。");
            ui.horizontal(|ui| {
                if ui.button("校验公式").clicked(){request(state,Rpc::ValidateFactor{draft:state.factor.draft.clone()});}
                if ui.button("保存新修订").clicked(){request(state,Rpc::SaveFactor{draft:state.factor.draft.clone()});}
                if ui.add_enabled(!state.busy(),egui::Button::new("合成样本预览")).clicked(){
                    let flag=cancellation();*cancel_slot=Some(flag.clone());
                    crate::panels::send_command(state,worker,WorkerCommand::PreviewFactor{task:uuid::Uuid::new_v4(),draft:state.factor.draft.clone(),cancel:flag});
                }
            });
        });
        if state.factor.compiled_source==state.factor.draft.formula && let Some(c)=&state.factor.compiled {
            ui.label(format!("校验通过 · 输入：{} · 至少{}条历史 · 递归状态：{}",c.inputs.join(", "),c.lookback,c.requires_recursive_state));
            ui.small(format!("算子版本：{:?}",c.operator_versions));
        }
        if let Some(p)=&state.factor.preview {
            ui.label(&p.label);ui.monospace(&p.formula);
            for (date,id,value) in &p.rows {ui.monospace(format!("{date} {id} {}",value.map(|v|format!("{v:.6}")).unwrap_or_else(||"null".into())));}
        }
        ui.collapsing("算子浏览与插入",|ui| {
            ui.text_edit_singleline(&mut state.factor.operator_filter);
            egui::ScrollArea::vertical().id_salt("operator_browser").max_height(220.).show(ui,|ui| {
                for op in OperatorRegistry::search(&state.factor.operator_filter) {
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!busy,egui::Button::new(&op.id)).clicked() && let Some(id)=formula_editor {insert_operator(ui,id,&mut state.factor.draft.formula,&op.example);}
                        ui.label(&op.category);ui.small(&op.description);
                    });
                    ui.small(format!("参数 {:?} → {} · {} · {} · {}",op.parameter_names.iter().zip(&op.parameters).map(|(n,k)|format!("{n}:{k:?}")).collect::<Vec<_>>(),op.output_type,op.window_rule,op.missing_rule,op.history_rule));
                    ui.small(format!("{} · {}",op.example,op.version));
                }
            });
        });
    });
}

/// Insert at the previous editor selection rather than blindly appending a function call.
fn insert_operator(ui: &egui::Ui, id: egui::Id, formula: &mut String, example: &str) {
    use egui::text::{CCursor, CCursorRange};
    let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
    let end = formula.chars().count();
    let [start, stop] = state
        .cursor
        .char_range()
        .map(|r| r.sorted_cursors().map(|c| usize::from(c.index).min(end)))
        .unwrap_or([end, end]);
    let bytes = |at| {
        formula
            .char_indices()
            .nth(at)
            .map(|(i, _)| i)
            .unwrap_or(formula.len())
    };
    let range = bytes(start)..bytes(stop);
    formula.replace_range(range, example);
    state
        .cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(
            start + example.chars().count(),
        ))));
    egui::TextEdit::store_state(ui.ctx(), id, state);
    ui.ctx().memory_mut(|memory| memory.request_focus(id));
}
