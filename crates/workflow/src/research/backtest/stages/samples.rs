//! Research samples phase.
use super::*;
pub(super) fn samples(
    signed: SignedResearch,
    mode: Option<&independent::Execution>,
    context: &TaskContext,
) -> Result<Option<FeatureSamples>> {
    let SignedResearch {
        prepared,
        ids,
        formula_version,
        source_contract,
        signature,
        historical_basis_signature,
        mut execution,
        run_signature: _,
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
    let predicting = mode.is_some_and(|m| m.predicting());
    let cache = request.output.join(".factor-staging");
    fs::create_dir_all(&cache)?;
    for year in history_start.year()..=needed_end.year() {
        context.report(format!("因子年度 {year}"))?;
        data::prepare_year(
            data::YearInputs {
                dataset: &dataset,
                members: &members,
                benchmark: &request.benchmark,
                universe_id: &request.universe_id,
                pool_start: pool.coverage.start,
                price_basis: request.price_basis,
                allow_missing: request.quality_policy.strictness
                    != analytics_core::research_quality::ResearchStrictness::Strict,
            },
            &registry,
            &ids,
            year,
            &cache,
            &signature,
        )?;
        let meta = data::load_meta(&cache, year)?;
        let empty: Vec<_> = ids
            .iter()
            .filter(|id| meta.finite_counts.get(*id).copied().unwrap_or(0) == 0)
            .collect();
        if !empty.is_empty() {
            // A full trailing window may legitimately be unavailable in an early
            // membership year. Preserve nulls rather than invent partial history.
            eprintln!(
                "year={year} all-null factors retained as missing (see finite_counts): {empty:?}"
            );
        }
    }
    let mut feature_issues = Vec::new();
    let mut sample_selections = Vec::new();
    fs::create_dir_all(request.output.join("sample_masks"))?;
    for (index, f) in folds.iter().enumerate().filter(|_| !predicting) {
        let selection = data::matrix_range_with_policy(
            &cache,
            &f.actual.train,
            &ids,
            &dataset.calendar,
            &request.quality_policy,
        )?;
        sample_selections.push(save_selection(
            request,
            &mut admission,
            index,
            "train",
            &f.actual.train,
            &selection.mask,
        )?);
        let matrix = selection.matrix;
        if matrix.rows < request.quality_policy.min_labeled_rows {
            feature_issues.push(format!(
                "fold {index}: eligible training rows {} below required {}",
                matrix.rows, request.quality_policy.min_labeled_rows
            ));
        }
        for (column, id) in ids.iter().enumerate() {
            if !(0..matrix.rows).any(|row| matrix.features[row * ids.len() + column].is_finite()) {
                feature_issues.push(format!("fold {index}: {id} has no finite training samples"));
            }
        }
        if let Some(v) = &f.actual.validation {
            let selection = data::matrix_range_with_policy(
                &cache,
                v,
                &ids,
                &dataset.calendar,
                &request.quality_policy,
            )?;
            sample_selections.push(save_selection(
                request,
                &mut admission,
                index,
                "validation",
                v,
                &selection.mask,
            )?);
            if selection.matrix.rows < request.quality_policy.min_labeled_rows {
                feature_issues.push(format!(
                    "fold {index}: insufficient eligible validation labels"
                ));
            }
        }
    }
    if !request.train_only && !request.audit_only {
        for (index, fold) in folds.iter().enumerate() {
            let mask = data::prediction_mask(
                &cache,
                &fold.actual.test,
                &ids,
                &dataset.calendar,
                &request.quality_policy,
            )?;
            sample_selections.push(save_selection(
                request,
                &mut admission,
                index,
                "prediction",
                &fold.actual.test,
                &mask,
            )?);
        }
    }
    let sample_selection = serde_json::json!({"schema_version":1,"policy":request.quality_policy,"selections":sample_selections});
    save(
        request.output.join("sample_selection.json"),
        &sample_selection,
    )?;
    execution["sample_selection_sha256"] = format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&sample_selection)?))
    )
    .into();
    execution["sample_selection"] = sample_selection;
    let training_signature = format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&(
            &signature,
            &request.config.training,
            &request.quality_policy,
            &execution["sample_selection"]["selections"]
                .as_array()
                .map(|m| m
                    .iter()
                    .filter(|r| r["split"] != "prediction")
                    .collect::<Vec<_>>()),
            &request.schedule,
            &folds,
            &historical_basis_signature,
            if mode.is_some() {
                serde_json::json!({"input_signature":source_contract})
            } else {
                serde_json::to_value(dataset.source_map())?
            },
            &ids,
            crate::research::worker::runtime_signature(),
            crate::research::worker::dependency_signature()
        ))?))
    );
    let run_signature = format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&(
            &signature, &execution
        ))?))
    );
    save(request.output.join("execution.json"), &execution)?;
    let mut audit: serde_json::Value =
        serde_json::from_slice(&fs::read(request.output.join("data_audit.json"))?)?;
    audit["execution"] = execution.clone();
    audit["factor_signature"] = signature.clone().into();
    audit["research_signature"] = run_signature.clone().into();
    save(request.output.join("data_audit.json"), &audit)?;
    if let Some(check) = admission.checks.last_mut() {
        check.status = if feature_issues.is_empty() {
            CheckStatus::Passed
        } else {
            CheckStatus::Failed
        };
        check.code = if feature_issues.is_empty() {
            "feature_samples_verified"
        } else {
            "no_valid_feature_samples"
        }
        .into();
        check.message = if feature_issues.is_empty() {
            "all selected features have finite eligible training observations".into()
        } else {
            feature_issues.join("; ")
        };
    }
    save(request.output.join("admission.json"), &admission)?;
    if !feature_issues.is_empty() || !admission.ready() {
        save(
            request.output.join("execution_readiness.json"),
            &serde_json::json!({"schema_version":8,"references":{"admission":{"path":"admission.json","sha256":file_hash(&request.output.join("admission.json"))?},"quality":{"path":"quality.json","sha256":file_hash(&request.output.join("quality.json"))?},"samples":{"path":"sample_selection.json"}},"ready":false,"reason":"factor/sample eligibility failed before training","feature_issues":feature_issues,"resolved_folds":folds}),
        )?;
        return if request.audit_only {
            Ok(None)
        } else {
            Err("data_not_ready: factor/sample eligibility failed before training".into())
        };
    }
    if request.audit_only {
        save(
            request.output.join("execution_readiness.json"),
            &serde_json::json!({"schema_version":8,"ready":admission.ready(),"references":{"admission":{"path":"admission.json","sha256":file_hash(&request.output.join("admission.json"))?},"quality":{"path":"quality.json","sha256":file_hash(&request.output.join("quality.json"))?},"samples":{"path":"sample_selection.json"}},"resolved_folds":folds,"sample_readiness":sample_readiness,"backtest_checks":{"status":"not_run","reason":"账户检查在实际订单、持仓和权益执行时进行"}}),
        )?;
        return Ok(None);
    }
    let published = request.output.join(if request.reuse_training.is_some() {
        "prediction-factor-cache"
    } else {
        "factor-cache"
    });
    if published.exists() {
        return Err("existing factor cache must not be replaced".into());
    }
    fs::rename(&cache, &published)?;
    let cache = published;
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
    let signed = SignedResearch {
        prepared,
        ids,
        formula_version,
        source_contract,
        signature,
        historical_basis_signature,
        execution,
        run_signature,
    };
    Ok(Some(FeatureSamples {
        signed,
        cache,
        training_signature,
    }))
}
