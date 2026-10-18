//! Research publish phase.
use super::*;
pub(super) fn publish(
    outputs: FoldOutputs,
    mode: Option<&independent::Execution>,
    _context: &TaskContext,
) -> Result<()> {
    let FoldOutputs {
        features,
        signals,
        reports,
    } = outputs;
    let FeatureSamples {
        signed,
        cache: _,
        training_signature,
    } = features;
    let SignedResearch {
        prepared,
        ids,
        formula_version,
        source_contract: _,
        signature,
        historical_basis_signature,
        execution,
        run_signature,
    } = signed;
    let PreparedResearch {
        training_source_request,
        effective_request,
        dataset,
        action_book,
        action_hash: _,
        pool,
        folds,
        history_start,
        needed_end,
        relevant_start: _,
        registry: _,
        quality_snapshot: _,
        members: _,
        pool_signature: _,
        admission,
        sample_readiness: _,
        market_content_sha256: _,
        source: _,
        last: _,
        first_member: _,
    } = prepared;
    let request = &effective_request;
    let _predicting = mode.is_some_and(|m| m.predicting());
    if request.reuse_training.is_none() {
        save(request.output.join("fit_calendar.json"), &dataset.calendar)?;
        let models:Vec<_>=(0..folds.len()).map(|i|{let folder=request.reuse_training.as_ref().unwrap_or(&request.output).join(format!("fold-{i:04}"));Ok(serde_json::json!({"fold":i,"model_sha256":file_hash(&folder.join("model.txt"))?,"manifest_sha256":file_hash(&folder.join("manifest.json"))?}))}).collect::<Result<_>>()?;
        let mut original = training_source_request;
        original.train_only = false;
        original.config.factor_ids = ids.clone();
        original.reuse_training = None;
        let mut training = serde_json::json!({"schema_version":artifacts::TRAINING_SCHEMA_VERSION,"runtime_signature":crate::research::worker::training_runtime_signature(),"dependency_signature":crate::research::worker::training_dependency_signature(),"factor_set":request.factor_set,"price_basis":request.price_basis,"status":"trained","request":original,"factor_signature":signature,"models":models,"adjustment_basis_signature":historical_basis_signature,"training_signature":training_signature,"source_map":dataset.source_map(),"resolved_folds":folds,"formula_version":formula_version,"feature_order":ids});
        let fit_cache_receipts = (history_start.year()..=needed_end.year())
            .map(|year| {
                let paths = [
                    format!("factor-cache/{year}.json"),
                    format!("factor-cache/{year}.bin"),
                ];
                paths
                    .into_iter()
                    .map(|name| Ok((name.clone(), file_hash(&request.output.join(name))?)))
                    .collect::<Result<BTreeMap<_, _>>>()
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<BTreeMap<_, _>>();
        training["fit_cache_receipts"] = serde_json::to_value(fit_cache_receipts)?;
        training["fit_calendar_sha256"] =
            file_hash(&request.output.join("fit_calendar.json"))?.into();
        training["source_warnings"] = serde_json::to_value(
            admission
                .checks
                .iter()
                .filter(|c| c.disposition == ProblemDisposition::Warn)
                .collect::<Vec<_>>(),
        )?;
        training["admission_report"] = serde_json::json!("admission.json");
        training["sample_selection"] = execution["sample_selection"].clone();
        training["artifact_signature"] = serde_json::json!(format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&training)?))
        ));
        if let Some(m) = mode
            && let Some(plan) = &m.training_plan
        {
            independent::publish_training(request, plan, &dataset, &folds, &training)?;
        } else {
            let typed: artifacts::TrainingDescriptor = serde_json::from_value(training)?;
            save(request.output.join("training.json"), &typed)?;
        }
        if request.train_only {
            return Ok(());
        }
    }
    predictions::publish(request, &signature, &folds, &signals, &reports)?;
    execute_account(
        request,
        &dataset,
        action_book.as_ref(),
        &pool,
        &folds,
        signals,
        reports,
        &run_signature,
        &signature,
    )
}
