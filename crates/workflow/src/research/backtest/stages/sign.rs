//! Research sign phase.
use super::*;
pub(super) fn sign(
    prepared: PreparedResearch,
    mode: Option<&independent::Execution>,
    _context: &TaskContext,
) -> Result<SignedResearch> {
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
        admission,
        sample_readiness,
        market_content_sha256,
        source,
        last,
        first_member,
    } = prepared;
    let request = &effective_request;
    let predicting = mode.is_some_and(|m| m.predicting());
    let price_symbols: BTreeSet<String> = members
        .keys()
        .chain(std::iter::once(&request.benchmark))
        .filter(|id| {
            dataset
                .checked_fields(id)
                .is_none_or(|fields| !fields.is_empty())
        })
        .cloned()
        .collect();
    let price_inputs = dataset.price_inputs(&price_symbols)?;
    let price_input_sha256 = format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&price_inputs)?))
    );
    let supported_inputs = SUPPORTED_RESEARCH_INPUTS;
    let mut excluded = BTreeMap::new();
    let mut ids = Vec::new();
    for spec in registry.all() {
        if matches!(
            spec.expression,
            analytics_factor::FactorExpr::Unsupported { .. }
        ) {
            excluded.insert(
                spec.id.clone(),
                vec!["quarantined_formula: see factor_audit.json".into()],
            );
            continue;
        }
        let missing: Vec<_> = spec
            .input_columns
            .iter()
            .filter(|c| !supported_inputs.contains(&c.as_str()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            excluded.insert(spec.id.clone(), missing);
        } else {
            ids.push(spec.id.clone());
        }
    }
    if !request.config.factor_ids.is_empty() {
        let mut seen = std::collections::BTreeSet::new();
        for id in &request.config.factor_ids {
            if !seen.insert(id) || !ids.contains(id) {
                return Err(format!("duplicate, quarantined or unavailable factor: {id}").into());
            }
        }
        ids = request.config.factor_ids.clone();
    }
    let formula_version = registry.selection_version(&ids)?;
    let source_contract = if mode.is_some() {
        independent::input_signature(
            &dataset,
            &members,
            &request.benchmark,
            relevant_start,
            needed_end,
        )?
    } else {
        market_content_sha256.clone()
    };
    let signature = if mode.is_some() {
        crate::signature(&(
            "independent-factor-input-v1",
            &source_contract,
            &ids,
            &formula_version,
            &pool,
            &request.benchmark,
            &request.price_basis,
            &quality_snapshot,
            request.warmup_sessions(),
        ))?
    } else {
        let legacy_signature = format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&(
                source.clone(),
                &market_content_sha256,
                &ids,
                &formula_version,
                &pool,
                &request.pool_signature,
                &request.benchmark,
                &price_inputs,
                &request.price_basis,
                &request.factor_set,
                &quality_snapshot,
                request.warmup_sessions(),
                "open-next-2,pit-mask,explicit-or-provider-price-input-v3"
            ))?))
        );
        legacy_signature
    };
    let historical_basis_signature =
        if request.price_basis == analytics_core::research_quality::PriceBasis::Verified {
            if mode.is_some() {
                let mut scoped = action_book.clone();
                if let Some(book) = scoped.as_mut() {
                    book.actions.retain(|a| {
                        members.contains_key(&a.instrument) || a.instrument == request.benchmark
                    });
                }
                adjustment_basis_signature(scoped.as_ref(), relevant_start, needed_end)?
            } else {
                adjustment_basis_signature(action_book.as_ref(), relevant_start, needed_end)?
            }
        } else {
            "provider-quotations;events-not-training-input".into()
        };
    let mut execution = serde_json::json!({"accounting_version":"rust-filled-trade-ledger-v1","event_validation_version":"actual-opening-exposure-v2+current-evidence-v1","reference_precision_policy":"two-decimal-per-ten-compatible-primary-terms-retained","suspension_valuation_policy":"gp29-status-days-v2+dated-current-episode-v1;retain-traded-bars;zero-volume-valuation-only-v2","unsupported_event_policy":"block_unprojected_events","market_content_sha256":market_content_sha256,"runtime_signature":crate::research::worker::runtime_signature(),"dependency_signature":crate::research::worker::dependency_signature(),"engine":"akquant","engine_version":"0.3.66","target_exposure":0.98,"volume_limit_pct":0.1,"trade_unit_policy":"AKQuant lot_size uses board increment; distinct minimum quantity and price-limit rules are not modeled","corporate_action_policy":"ex_date","mode":"akquant_ex_date","factor_set":request.factor_set,"price_basis":request.price_basis,
        "quality_policy":request.quality_policy,"quality":quality_snapshot,"quality_sha256":format!("sha256:{:x}",analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&quality_snapshot)?))),
        "fee_application":"latest_only_for_all_dates", "market_rules":request.market_rules, "initial_cash_cny":request.initial_cash_cny,

        "source_map":dataset.source_map(),"source_map_signature":format!("sha256:{:x}",analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(dataset.source_map())?))),"corporate_action_sha256":action_hash,"dividend_withholding_rate":request.dividend_withholding_rate,
        "price_inputs":price_inputs,"price_input_sha256":price_input_sha256,"pool":pool,"pool_signature":pool_signature,"benchmark":request.benchmark,"pool_exit_policy":request.pool_exit_policy,
        "research_config":request.config,"schedule":request.schedule,"action_policy":request.action_policy,"event_check_policy":request.event_check_policy,"resolved_folds":folds,"dividend_tax_convention":"gross dividend at ex-date; no withholding model"});
    if !request.train_only {
        execution["execution_checks_sha256"] =
            file_hash(&request.output.join("execution_checks.json"))?.into();
        execution["execution_dependencies_sha256"] =
            file_hash(&request.output.join("execution-dependencies.json"))?.into();
    }
    if let Some(source) = &request.reuse_training {
        let artifact: serde_json::Value =
            serde_json::from_slice(&fs::read(source.join("training.json"))?)?;
        if !predicting {
            validate_training_artifact(&artifact)?;
        }
        execution["training_provenance"] = serde_json::json!({"training_signature":artifact["training_signature"],"models":artifact["models"].as_array().ok_or("missing model receipts")?.iter().map(|m|serde_json::json!({"fold":m["fold"],"model_sha256":m["model_sha256"]})).collect::<Vec<_>>(),"feature_order":artifact["feature_order"]});
    }
    if let Some(source) = &request.reuse_predictions {
        let original: serde_json::Value =
            serde_json::from_slice(&fs::read(source.join("execution.json"))?)?;
        execution["prediction_provenance"] = serde_json::json!({"source":source,"manifest_sha256":file_hash(&source.join("prediction_manifest.json"))?,"producer_runtime":original["runtime_signature"],"producer_dependencies":original["dependency_signature"],"training":original["training_provenance"]});
    }
    if let Some(m) = mode
        && let Some(c) = &m.catalogue
    {
        execution["training_provenance"] = serde_json::json!({"training_signature":c.training_signature,"models":c.models,"feature_order":c.plan.config.factor_ids});
        execution["independent"] =
            serde_json::json!({"catalogue":c,"segments":m.segments,"contract":"independent-v1"});
    }
    if request.rights_policy.is_some() || corporate::enabled(request) {
        let projection: analytics_core::rights_account::Ledger =
            serde_json::from_slice(&fs::read(request.output.join("rights_input.json"))?)?;
        projection.validate()?;
        execution["rights_policy"] = serde_json::to_value(&projection.policy)?;
        execution["corporate_event_policy"] = serde_json::to_value(projection.corporate.policy)?;
        save(request.output.join("rights_input.json"), &projection)?;
        execution["rights_input_sha256"] =
            file_hash(&request.output.join("rights_input.json"))?.into();
        execution["rights_source_lock_sha256"] = format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(include_bytes!(
                "../../../../research/akquant-source.lock.json"
            )))
        )
        .into();
        execution["accounting_version"] = "rust-filled-trade-ledger-events-v3".into();
        execution["rights_valuation_policy"] =
            "cost-before-ex-or-allocation;parent-raw-after-ex-and-allocation;nontradable".into();
    }
    let run_signature = format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&(
            &signature,
            &execution,
            &request.schedule,
            &folds,
            "akquant-ex-date-v1"
        ))?))
    );
    if request.output.join("summary.json").exists() {
        let old: serde_json::Value =
            serde_json::from_slice(&fs::read(request.output.join("summary.json"))?)?;
        if old["schema_version"] != RESEARCH_SCHEMA_VERSION || old["signature"] != run_signature {
            return Err("existing output uses another execution schema/settings; choose a new output directory".into());
        }
    }
    save(
        request.output.join("factor_audit.json"),
        &if matches!(
            request.factor_set,
            analytics_core::research_quality::FactorSet::Gtja191
        ) {
            serde_json::to_value(analytics_factor::audit::entries())?
        } else {
            serde_json::to_value(registry.all().collect::<Vec<_>>())?
        },
    )?;
    save(request.output.join("execution.json"), &execution)?;
    let coverage:Vec<_>=folds.iter().map(|f|serde_json::json!({"requested":f.requested,"actual":f.actual,"warmup_start":f.warmup_start,"warmup_sessions":f.warmup_sessions,"pool_coverage_start":pool.coverage.start,"unknown_cross_section_before":pool.coverage.start})).collect();
    save(
        request.output.join("data_audit.json"),
        &serde_json::json!({"source":source,"provider":dataset.root,"requested":request.schedule,"calendar_start":dataset.calendar[0],"calendar_end":last,"historical_pool_start":dataset.calendar[first_member],"unique_historical_members":members.len(),"registered_factors":registry.len(),"usable_factor_ids":ids,"excluded_factors_missing_inputs":excluded,"formula_version":formula_version,"factor_signature":signature,"research_signature":run_signature,"execution":execution,"coverage":coverage,"uses_jfqlib_code_or_cache":false,"label":"adjusted_open[t+2]/adjusted_open[t+1]-1","rolling_config":request.config,"split":"explicit requested and resolved date folds; purge labels ending beyond split cutoff","portfolio":"AKQuant raw-price fills; T+1; continuous portfolio; ex-date dividends/splits; no terminal fake fills"}),
    )?;
    eprintln!(
        "data_end={last} pool_start={} factor_count={} excluded={excluded:?}",
        dataset.calendar[first_member],
        ids.len()
    );
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
    Ok(SignedResearch {
        prepared,
        ids,
        formula_version,
        source_contract,
        signature,
        historical_basis_signature,
        execution,
        run_signature,
    })
}
