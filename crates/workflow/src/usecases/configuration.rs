//! Configuration use cases.
use super::*;
pub(super) fn execute(
    w: &Workspace,
    request: &WorkflowRequest,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<WorkflowOutcome> {
    let mut data = None;
    let mut details = Value::Null;
    match request {
        WorkflowRequest::SavePool { definition } => {
            details = serde_json::to_value(w.save_pool(definition.clone())?)?;
        }
        WorkflowRequest::PreviewPool { id, market, day } => {
            details = w.preview_pool(id, market, *day)?;
        }
        WorkflowRequest::CreateCanonicalDemo {} => {
            let (m, a, t) = demo::create_canonical(w)?;
            let template_path = run.output.join("workflow.json");
            t.write(&w.resolve(&template_path))?;
            details =
                json!({"demo_actions":a.id,"demo_template":t,"demo_template_path":template_path});
            data = Some(m);
        }
        WorkflowRequest::CreateDemo {} => {
            context.report("创建合成演示输入（不是真实行情）")?;
            let (m, a, t) = demo::create(w)?;
            let template_path = run.output.join("workflow.json");
            t.write(&w.resolve(&template_path))?;
            details =
                json!({"demo_actions":a.id,"demo_template":t,"demo_template_path":template_path});
            data = Some(m);
        }
        WorkflowRequest::RegisterData { path } => {
            context.report("核验本地数据来源")?;
            data = Some(w.register(&w.resolve(path))?);
        }
        _ => unreachable!("domain dispatcher mismatch"),
    }
    Ok(WorkflowOutcome { data, details })
}
