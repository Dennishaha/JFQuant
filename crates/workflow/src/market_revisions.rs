//! Shared preview/publication of immutable missing-cell market views.
use crate::{Result, RunRecord, WorkflowOutcome, Workspace, atomic_json, usecases};
use analytics_core::task::TaskContext;
use analytics_storage::market_view::supplement::SupplementView;
use serde_json::{Value, json};
/// Read-only preparation; both clients and publication use this exact validator.
pub(crate) fn preview(w: &Workspace, market: &str, request: &SupplementView) -> Result<Value> {
    let base = usecases::version(w, market, "market")?;
    if request.base_path != base.path
        || request.base_sha256 != base.details["content_sha256"].as_str().unwrap_or("")
    {
        return Err("supplement_frozen_base_mismatch".into());
    }
    for selection in &request.selections {
        let candidate = w
            .datasets()?
            .into_iter()
            .find(|v| v.kind == "market" && v.path == selection.source_path)
            .ok_or("supplement_source_not_registered")?;
        usecases::version(w, &candidate.id, "market")?;
    }
    let (_, _, mut report) = request.validate(&w.root)?;
    if let Some(problems) = report["problems"].as_array_mut() {
        for problem in problems {
            problem["id"] = crate::signature(&json!([
                market,
                problem["instrument"],
                problem["date"],
                "verified_raw_bar_with_original_normalization"
            ]))?
            .into();
            problem["required_fact"] = "verified_raw_bar_with_original_normalization".into();
            problem["next_action"] =
                "select_verified_capture_or_resolve_source_and_normalization_conflict".into();
        }
    }
    report["market"] = market.into();
    report["source_signature"] = base.signature.into();
    report["request_sha256"] = crate::signature(request)?.into();
    report["request"] = serde_json::to_value(request)?;
    Ok(report)
}
pub(crate) fn publish(
    w: &Workspace,
    market: &str,
    request: &SupplementView,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<WorkflowOutcome> {
    let report = preview(w, market, request)?;
    if report["ready"] != true {
        return Err(format!("market_repair_not_ready:{report}").into());
    }
    let staging = w.resolve(format!(".jfquant/staging/market-revision-{}", run.id));
    let root = staging.join("market");
    std::fs::create_dir_all(&root)?;
    atomic_json(&root.join("market_supplement.json"), request)?;
    atomic_json(
        &staging.join("jfquant_source.json"),
        &json!({"downloader":"JFQuant analytics-storage","snapshot_verified":true,"independent_download":true,"repair_schema_version":1,"base_market":market,"validation":report}),
    )?;
    analytics_storage::daily::DailyDataset::open(&root)?;
    context.check()?;
    let output = w.resolve(format!("data/market-revision-{}", run.id));
    std::fs::rename(staging, &output)?;
    let data = w.register(&output.join("market"))?;
    Ok(WorkflowOutcome {
        data: Some(data),
        details: report,
    })
}
