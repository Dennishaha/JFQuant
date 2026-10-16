//! Research replay phase.
use super::*;
pub(super) fn replay(
    signed: SignedResearch,
    mode: Option<&independent::Execution>,
    context: &TaskContext,
) -> Result<Option<SignedResearch>> {
    let SignedResearch {
        prepared,
        ids,
        formula_version,
        source_contract,
        signature,
        historical_basis_signature,
        mut execution,
        mut run_signature,
    } = signed;
    let PreparedResearch {
        training_source_request,
        effective_request,
        dataset,
        action_book,
        action_hash,
        pool,
        folds,
        history_start,
        needed_end,
        relevant_start,
        registry,
        quality_snapshot,
        members,
        pool_signature,
        mut admission,
        sample_readiness,
        market_content_sha256,
        source,
        last,
        first_member,
    } = prepared;
    let request = &effective_request;
    let _predicting = mode.is_some_and(|m| m.predicting());
    if !request.audit_only
        && (request.reuse_training.is_some() || request.reuse_predictions.is_some())
        && let Some((signals, reports, selection)) =
            predictions::restore(request, &signature, &folds)?
    {
        if let Some(check) = admission.checks.last_mut() {
            check.status = CheckStatus::Passed;
            check.code = "feature_samples_verified".into();
            check.message = "verified immutable prediction cache and sample receipts".into();
        }
        let sample_decisions: Vec<analytics_core::research_quality::QualityDecision> =
            serde_json::from_slice(&fs::read(request.output.join("sample_decisions.json"))?)?;
        admission.decisions.extend(sample_decisions);
        save(request.output.join("admission.json"), &admission)?;
        execution["unsupported_event_policy"] = "warn_and_skip".into();
        execution["sample_selection_sha256"] = format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&selection)?))
        )
        .into();
        execution["sample_selection"] = selection;
        run_signature = format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&(
                &signature, &execution
            ))?))
        );
        save(request.output.join("execution.json"), &execution)?;
        let mut audit: serde_json::Value =
            serde_json::from_slice(&fs::read(request.output.join("data_audit.json"))?)?;
        audit["execution"] = execution;
        audit["research_signature"] = run_signature.clone().into();
        save(request.output.join("data_audit.json"), &audit)?;
        context.report("预测缓存命中，直接执行账户回测")?;
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
        )?;
        return Ok(None);
    }
    if request.reuse_predictions.is_some() {
        return Err(
            "published_predictions_missing: prediction-only execution cannot train or predict"
                .into(),
        );
    }
    let prepared = PreparedResearch {
        training_source_request,
        effective_request,
        dataset,
        action_book,
        action_hash,
        pool,
        folds,
        history_start,
        needed_end,
        relevant_start,
        registry,
        quality_snapshot,
        members,
        pool_signature,
        admission,
        sample_readiness,
        market_content_sha256,
        source,
        last,
        first_member,
    };
    Ok(Some(SignedResearch {
        prepared,
        ids,
        formula_version,
        source_contract,
        signature,
        historical_basis_signature,
        execution,
        run_signature,
    }))
}
