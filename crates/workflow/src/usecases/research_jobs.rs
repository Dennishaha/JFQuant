//! Research jobs use cases.
use super::*;
pub(super) fn execute(
    w: &Workspace,
    request: &WorkflowRequest,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<WorkflowOutcome> {
    let data = None;
    let mut details;
    match request {
        WorkflowRequest::CompareAllocations { tasks } => {
            details = crate::allocation_comparison::snapshot(w, tasks)?;
            if details != run.request["comparison"] {
                return Err("comparison_sources_changed".into());
            }
            atomic_json(
                &w.resolve(&run.output).join("allocation-comparison.json"),
                &details,
            )?;
        }
        WorkflowRequest::Combined { request } => {
            details = crate::research::backtest::combined::execute(w, run, request, context)?;
        }
        WorkflowRequest::Independent { request } => {
            details = research_api::backtest::independent::execute(w, run, request, context)?;
        }
        WorkflowRequest::PreviewResearch {
            template,
            market,
            actions,
        } => {
            let m = version(w, market, "market")?;
            let a = actions
                .as_deref()
                .map(|id| version(w, id, "actions"))
                .transpose()?;
            let r = research_request(w, run, template, &m, a.as_ref(), true)?;
            crate::research::backtest::run_with_context(&r, context)?;
            details = serde_json::from_slice(&analytics_storage::evidence_store::read(
                w.resolve(&run.output).join("input-check.json"),
            )?)?;
            let readiness: serde_json::Value =
                serde_json::from_slice(&analytics_storage::evidence_store::read(
                    w.resolve(&run.output).join("execution_readiness.json"),
                )?)?;
            details["resolved_folds"] = readiness["resolved_folds"].clone();
        }
        WorkflowRequest::PrepareResearchData {
            actions,
            template,
            market,
            announcements,
            resume,
        } => {
            details = research_requirements_with_actions(w, template, market, actions.as_deref())?;
            atomic_json(&w.resolve(&run.output).join("requirements.json"), &details)?;
            details = preparation::prepare(w, run, &details, *announcements, *resume, context)?;
            atomic_json(
                &w.resolve(&run.output).join("execution_readiness.json"),
                &details,
            )?;
            atomic_json(&w.resolve(&run.output).join("outcome.json"), &details)?;
            return Err("data_not_ready: acquisition candidates require source and event-category validation; see execution_readiness.json".into());
        }

        WorkflowRequest::TrainResearch {
            template,
            market,
            actions,
        } => {
            let m = version(w, market, "market")?;
            let a = actions
                .as_deref()
                .map(|a| version(w, a, "actions"))
                .transpose()?;
            let mut r = research_request(w, run, template, &m, a.as_ref(), false)?;
            r.train_only = true;
            context.report("批量检查训练输入")?;
            crate::research::backtest::run_with_context(&r, context)?;
            details = w.results(run)?;
        }
        WorkflowRequest::BacktestPredictions {
            allocation,
            source,
            actions,
            rights_policy,
        } => {
            let source_root = w.resolve(format!(".jfquant/runs/{source}"));
            for name in [
                "run.json",
                "summary.json",
                "prediction_manifest.json",
                "predictions.parquet",
            ] {
                if run.request["prediction_source_hashes"][name]
                    != format!(
                        "sha256:{:x}",
                        analytics_core::hash::Hex(Sha256::digest(
                            analytics_storage::evidence_store::read(source_root.join(name))?
                        ))
                    )
                {
                    return Err("prediction source changed after task submission".into());
                }
            }
            let verified = research_api::backtest::verification::report(w, *source)?;
            if verified["status"] != "consistent" {
                return Err("prediction source failed native verification".into());
            }
            let a = version(w, actions, "actions")?;
            research_api::backtest::backtest_from_predictions(
                &w.resolve(format!(".jfquant/runs/{source}")),
                &w.resolve(&run.output),
                &w.resolve(a.path),
                rights_policy.clone(),
                allocation.as_deref(),
                context,
            )?;
            details = w.results(run)?;
        }
        WorkflowRequest::BacktestResearch {
            execution,
            training,
            actions,
            event_check_policy,
        } => {
            let saved = w
                .runs()?
                .into_iter()
                .find(|r| r.id == *training)
                .ok_or("training task not found")?;
            let frozen = saved
                .request
                .get("workflow_snapshot")
                .unwrap_or(&saved.request);
            let mut training_market = None;
            for input in frozen["inputs"].as_array().into_iter().flatten() {
                if input["kind"] == "market" {
                    training_market = Some(version(
                        w,
                        input["id"].as_str().ok_or("training market missing")?,
                        "market",
                    )?);
                }
            }
            let market = training_market.ok_or("training market snapshot missing")?;
            let source = w.resolve(&saved.output);
            if let Some(expected) = run.request.get("training_artifact_sha256") {
                let actual = format!(
                    "sha256:{:x}",
                    analytics_core::hash::Hex(Sha256::digest(
                        analytics_storage::evidence_store::read(source.join("training.json"))?
                    ))
                );
                if expected != &json!(actual) {
                    return Err("training_artifact_damaged".into());
                }
            }
            let a = version(w, actions, "actions")?;
            context.report("读取已有模型并回测")?;
            crate::research::backtest::backtest_from_training_with_policy(
                &source,
                &w.resolve(&run.output),
                &w.resolve(a.path),
                Some(&w.resolve(market.path)),
                Some(*event_check_policy),
                execution.as_deref(),
                context,
            )?;
            details = w.results(run)?;
        }
        WorkflowRequest::Research {
            template,
            market,
            actions,
            audit_only,
        } => {
            template.validate()?;
            let m = version(w, market, "market")?;
            let a = actions
                .as_deref()
                .map(|a| version(w, a, "actions"))
                .transpose()?;
            if !audit_only && a.is_none() {
                return Err("execution_actions_required".into());
            }
            details = research(w, run, context, template, &m, a.as_ref(), *audit_only)?;
        }
        _ => unreachable!("domain dispatcher mismatch"),
    }
    Ok(WorkflowOutcome { data, details })
}
