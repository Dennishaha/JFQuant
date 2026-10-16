//! Actions use cases.
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
        WorkflowRequest::InspectActions {
            actions,
            instrument,
            evidence_manifest,
        } => {
            analytics_core::pool::validate_instrument(instrument)?;
            let source = version(w, actions, "actions")?;
            let book = ActionBundle::read(&w.resolve(source.path))?;
            let events: Vec<_> = book
                .actions
                .iter()
                .filter(|a| a.instrument == *instrument)
                .map(|a| Ok(json!({"original":a,"original_sha256":signature(a)?})))
                .collect::<Result<_>>()?;
            let documents = if let Some(path) = evidence_manifest {
                let path = w.resolve(path).canonicalize()?;
                if !path.starts_with(w.root.canonicalize()?) {
                    return Err("announcement evidence must belong to this workspace".into());
                }
                let docs = analytics_storage::corporate_actions::review::verified_documents(&path)?;
                docs.into_values()
                    .filter(|(d, _)| d["instrument"] == *instrument)
                    .map(|(d, text)| json!({"document":d,"extracted_text":text}))
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let stock_status=book.execution_data.as_ref().map(|data|json!({"schema_version":data.schema_version,"days":data.suspensions.get(instrument),"capability":data.capabilities.get(&format!("{}:{instrument}",analytics_storage::tdx_status::RULE)),"not_a_complete_status_history":true}));
            details = json!({"instrument":instrument,"events":events,"source_failure":book.failures.get(instrument),"issues":book.execution_issues.iter().filter(|i| i.instrument == *instrument).map(|i| Ok(json!({"issue":i,"sha256":signature(i)?}))).collect::<Result<Vec<_>>>()?,"reconciliation_evidence":book.raw_sha256.get("reconciliation-v1.json"),"documents":documents,"stock_status":stock_status,"structured_fields":book.field_facts.get(instrument)});
        }
        WorkflowRequest::ReconcileActions {
            actions,
            evidence_tasks,
            market,
        } => {
            let source = version(w, actions, "actions")?;
            let source_path = w.resolve(&source.path);
            verify_action_evidence(&source_path, &ActionBundle::read(&source_path)?)?;
            let mut roots = Vec::new();
            for task in evidence_tasks {
                let record: RunRecord = serde_json::from_slice(&fs::read(
                    w.resolve(format!(".jfquant/runs/{task}/run.json")),
                )?)?;
                let root = reconciliation_evidence_root(w, *task, &record)?;
                if run.request["reconciliation_evidence"][task.to_string()]["sha256"]
                    != market_content_hash(&root)?
                {
                    return Err("reconciliation evidence changed after request freeze".into());
                }
                roots.push(root);
            }
            let calendar = if let Some(id) = market {
                analytics_storage::daily::DailyDataset::open(
                    w.resolve(version(w, id, "market")?.path),
                )?
                .calendar
            } else {
                Vec::new()
            };
            let staging = w.resolve(format!(".jfquant/staging/action-reconcile-{}", run.id));
            let book = analytics_storage::corporate_actions::reconcile::apply(
                &source_path,
                &roots,
                &calendar,
                &staging,
                context,
            )?;
            context.check()?;
            let output = w.resolve(format!("data/action-reconcile-{}", run.id));
            fs::rename(staging, &output)?;
            data = Some(w.register(&output.join("actions.json"))?);
            details = json!({"reconciliation":output.join("raw/reconciliation-v1.json"),"ordinary_issues":book.execution_issues.iter().filter(|i| !i.skipped_by_engine()).count(),"unsupported_event_warnings":book.execution_issues.iter().filter(|i| i.skipped_by_engine()).count()});
        }
        WorkflowRequest::ReviseActions { actions, request } => {
            let source = version(w, actions, "actions")?;
            let source_path = w.resolve(&source.path);
            let book = ActionBundle::read(&source_path)?;
            let mut request = request.clone();
            request.evidence_manifest = w.resolve(&request.evidence_manifest).canonicalize()?;
            if !request
                .evidence_manifest
                .starts_with(w.root.canonicalize()?)
            {
                return Err("announcement evidence must belong to this workspace".into());
            }
            if let Some(path) = &request.structured_manifest {
                let path = w.resolve(path).canonicalize()?;
                if !path.starts_with(w.root.canonicalize()?) {
                    return Err("structured evidence must belong to this workspace".into());
                }
                request.structured_manifest = Some(path);
            }
            let staging = w.resolve(format!(".jfquant/staging/action-review-{}", run.id));
            analytics_storage::corporate_actions::review::apply(
                &book,
                source_path.parent().ok_or("missing source root")?,
                &request,
                &staging,
            )?;
            let output = w.resolve(format!("data/action-review-{}", run.id));
            fs::rename(staging, &output)?;
            data = Some(w.register(&output.join("actions.json"))?);
        }
        _ => unreachable!("domain dispatcher mismatch"),
    }
    Ok(WorkflowOutcome { data, details })
}
