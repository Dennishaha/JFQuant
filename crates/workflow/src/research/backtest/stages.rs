//! Typed phase boundaries for research execution.
use super::*;
use analytics_core::{admission::CheckStatus, task::TaskContext};
pub(super) struct PreparedResearch {
    training_source_request: ResearchRequest,
    effective_request: ResearchRequest,
    dataset: DailyDataset,
    action_book: Option<analytics_storage::corporate_actions::ActionBundle>,
    action_hash: Option<String>,
    pool: analytics_core::pool::PoolMembership,
    folds: Vec<analytics_core::schedule::ResolvedFold>,
    history_start: Date,
    needed_end: Date,
    relevant_start: Date,
    registry: FactorRegistry,
    quality_snapshot: quality::QualitySnapshot,
    members: BTreeMap<String, Vec<analytics_storage::qlib::Membership>>,
    pool_signature: String,
    admission: analytics_core::admission::AdmissionReport,
    sample_readiness: serde_json::Value,
    market_content_sha256: String,
    source: serde_json::Value,
    last: Date,
    first_member: usize,
}
pub(super) struct SignedResearch {
    prepared: PreparedResearch,
    ids: Vec<String>,
    formula_version: String,
    source_contract: String,
    signature: String,
    historical_basis_signature: String,
    execution: serde_json::Value,
    run_signature: String,
}
pub(super) struct FeatureSamples {
    signed: SignedResearch,
    cache: PathBuf,
    training_signature: String,
}
pub(super) struct FoldOutputs {
    features: FeatureSamples,
    signals: Vec<(Date, String, f64)>,
    reports: Vec<FoldReport>,
}
pub(super) fn run(
    request: &ResearchRequest,
    mode: Option<&independent::Execution>,
    context: &TaskContext,
) -> Result<()> {
    let prepared = match prepare(request, mode, context) {
        Ok(Some(prepared)) => prepared,
        Ok(None) => {
            if !request.output.join("input-check.json").exists() {
                let reason = fs::read(request.output.join("execution_readiness.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                    .and_then(|v| v["reason"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| "输入前提不满足".into());
                input_report::unresolved(request, &reason)?;
            }
            return Ok(());
        }
        Err(error) => {
            let _ = input_report::unresolved(request, &error.to_string());
            return Err(error);
        }
    };
    let signed = sign(prepared, mode, context)?;
    let Some(signed) = replay(signed, mode, context)? else {
        return Ok(());
    };
    let Some(features) = samples(signed, mode, context)? else {
        return Ok(());
    };
    let outputs = fit_predict(features, mode, context)?;
    publish(outputs, mode, context)
}

mod prepare;
use prepare::prepare;
mod sign;
use sign::sign;
mod replay;
use replay::replay;
mod samples;
use samples::samples;
mod fit_predict;
use fit_predict::fit_predict;
mod publish;
use publish::publish;

mod input_report;
