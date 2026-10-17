//! Snapshot.
use crate::{Result, RunRecord, WorkflowRequest, Workspace, market_content_hash};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;

/// Snapshot selected input versions when creating a run, rather than resolving them later.
pub fn snapshot(workspace: &Workspace, request: &WorkflowRequest) -> Result<Value> {
    if let WorkflowRequest::Independent { request: job } = request {
        return Ok(
            json!({"request":request,"independent":research::backtest::independent::snapshot(workspace,job)?,"app_version":env!("CARGO_PKG_VERSION")}),
        );
    }
    match request {
        WorkflowRequest::SaveSourcePolicy { policy } => policy.validate()?,
        WorkflowRequest::RefreshData { request } => request.template.validate()?,
        WorkflowRequest::CollectAnnouncements { request, .. } => {
            request.validate()?;
            if request.provider != analytics_storage::collector::Provider::Cninfo {
                return Err(
                    "announcement_source_unsupported: select cninfo for CNINFO evidence".into(),
                );
            }
        }
        WorkflowRequest::CollectReference { request, .. }
        | WorkflowRequest::CollectQuotes { request, .. }
        | WorkflowRequest::CollectMarket { request, .. }
        | WorkflowRequest::CollectActions { request, .. }
        | WorkflowRequest::CollectCoefficients { request, .. } => request.validate()?,
        WorkflowRequest::TrainResearch { template, .. }
        | WorkflowRequest::Research { template, .. }
        | WorkflowRequest::PreviewResearch { template, .. }
        | WorkflowRequest::PrepareResearchData { template, .. } => template.validate()?,
        WorkflowRequest::SavePool { definition } => definition.validate()?,
        _ => {}
    };
    let versions = match request {
        WorkflowRequest::Evidence { request } => request
            .actions()
            .map(|id| version_metadata(workspace, id, "actions"))
            .transpose()?
            .into_iter()
            .collect(),
        WorkflowRequest::RefreshData { request } => {
            let mut v = vec![version_metadata(workspace, &request.market, "market")?];
            if let Some(id) = &request.actions {
                v.push(version_metadata(workspace, id, "actions")?);
            }
            v
        }
        WorkflowRequest::ReviseMarket { market, .. } => {
            vec![version_metadata(workspace, market, "market")?]
        }
        WorkflowRequest::ComposeMarket {
            market,
            coefficients,
        } => vec![
            version_metadata(workspace, market, "market")?,
            version_metadata(workspace, coefficients, "coefficients")?,
        ],
        WorkflowRequest::TrainResearch {
            market, actions, ..
        }
        | WorkflowRequest::Research {
            market, actions, ..
        } => {
            let mut inputs = vec![version_metadata(workspace, market, "market")?];
            if let Some(a) = actions {
                inputs.push(version_metadata(workspace, a, "actions")?);
            }
            inputs
        }
        WorkflowRequest::DownloadActions { market, .. }
        | WorkflowRequest::PreviewResearch { market, .. }
        | WorkflowRequest::PrepareResearchData { market, .. } => {
            vec![version_metadata(workspace, market, "market")?]
        }
        WorkflowRequest::ReconcileActions {
            actions, market, ..
        } => {
            let mut v = vec![version_metadata(workspace, actions, "actions")?];
            if let Some(market) = market {
                v.push(version_metadata(workspace, market, "market")?);
            }
            v
        }
        WorkflowRequest::BacktestPredictions { actions, .. }
        | WorkflowRequest::BacktestResearch { actions, .. }
        | WorkflowRequest::ReviseActions { actions, .. }
        | WorkflowRequest::InspectActions { actions, .. } => {
            vec![version_metadata(workspace, actions, "actions")?]
        }
        _ => Vec::new(),
    };
    let formula_version = match request {
        WorkflowRequest::Research { template, .. }
        | WorkflowRequest::TrainResearch { template, .. }
        | WorkflowRequest::PreviewResearch { template, .. }
        | WorkflowRequest::PrepareResearchData { template, .. } => {
            template.formula_version.as_str()
        }
        _ => analytics_factor::FORMULA_VERSION,
    };
    let mut frozen = json!({"request":request,"inputs":versions,"app_version":env!("CARGO_PKG_VERSION"),"formula_version":formula_version,"pool_definition": match request { WorkflowRequest::Research{template,..}|WorkflowRequest::TrainResearch{template,..} if !template.pool_version.starts_with("builtin:") => Some(workspace.pool(&template.pool_version)?), _ => None }});
    if let WorkflowRequest::BacktestPredictions { source, .. } = request {
        let root = workspace.resolve(format!(".jfquant/runs/{source}"));
        let mut hashes = serde_json::Map::new();
        for name in [
            "run.json",
            "summary.json",
            "prediction_manifest.json",
            "predictions.parquet",
        ] {
            hashes.insert(
                name.into(),
                format!(
                    "sha256:{:x}",
                    analytics_core::hash::Hex(Sha256::digest(fs::read(root.join(name))?))
                )
                .into(),
            );
        }
        frozen["prediction_source_hashes"] = hashes.into();
    }
    if let WorkflowRequest::Evidence { request } = request {
        frozen["evidence_inputs"] = crate::action_evidence::freeze(workspace, request)?;
    }
    if let WorkflowRequest::ReconcileActions { evidence_tasks, .. } = request {
        let mut evidence = serde_json::Map::new();
        for task in evidence_tasks {
            let record: RunRecord = serde_json::from_slice(&fs::read(
                workspace.resolve(format!(".jfquant/runs/{task}/run.json")),
            )?)?;
            let root = reconciliation_evidence_root(workspace, *task, &record)?;
            evidence.insert(
                task.to_string(),
                json!({"sha256":market_content_hash(&root)?,"status":record.status}),
            );
        }
        frozen["reconciliation_evidence"] = evidence.into();
    }
    if matches!(request, WorkflowRequest::RefreshData { .. }) {
        frozen["source_policy"] = serde_json::to_value(refresh::SourcePolicy::load(workspace)?)?;
    }
    if let WorkflowRequest::BacktestResearch { training, .. } = request {
        let record = workspace
            .runs()?
            .into_iter()
            .find(|r| r.id == *training)
            .ok_or("training task not found")?;
        let artifact_path = workspace.resolve(record.output).join("training.json");
        if !artifact_path.is_file() {
            return Err(
                "training_artifact_unavailable: selected task has no completed training models"
                    .into(),
            );
        }
        frozen["training_artifact_sha256"] = json!(format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(fs::read(artifact_path)?))
        ));
    }
    Ok(frozen)
}

use crate::usecases::{reconciliation_evidence_root, version_metadata};
use crate::{refresh, research};
