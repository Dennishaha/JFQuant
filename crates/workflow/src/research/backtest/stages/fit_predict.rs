//! Research fit predict phase.
use super::*;
pub(super) fn fit_predict(
    features: FeatureSamples,
    mode: Option<&independent::Execution>,
    context: &TaskContext,
) -> Result<FoldOutputs> {
    let FeatureSamples {
        signed,
        cache,
        training_signature,
    } = features;
    let SignedResearch {
        prepared,
        ids,
        formula_version,
        source_contract,
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
    let mut signals = Vec::<(Date, String, f64)>::new();
    let mut reports = Vec::new();

    for (fold_index, resolved) in folds.iter().enumerate() {
        context.check()?;
        let f = &resolved.actual;
        let year = f.test.start.year();
        let train = if predicting {
            data::empty_matrix(ids.len())
        } else {
            data::matrix_range_with_policy(
                &cache,
                &f.train,
                &ids,
                &dataset.calendar,
                &request.quality_policy,
            )?
            .matrix
        };
        let valid = match &f.validation {
            Some(v) if !predicting => {
                data::matrix_range_with_policy(
                    &cache,
                    v,
                    &ids,
                    &dataset.calendar,
                    &request.quality_policy,
                )?
                .matrix
            }
            _ => data::empty_matrix(ids.len()),
        };
        let test = if request.train_only {
            None
        } else {
            Some(data::prediction_range_with_policy(
                &cache,
                &f.test,
                &ids,
                &dataset.calendar,
                &request.quality_policy,
            )?)
        };
        let test_matrix = if request.train_only || predicting {
            data::empty_matrix(ids.len())
        } else {
            data::matrix_range_with_policy(
                &cache,
                &f.test,
                &ids,
                &dataset.calendar,
                &request.quality_policy,
            )?
            .matrix
        };
        let split = TrainingSplit {
            train,
            validation: valid,
            test: test_matrix,
        };
        let output = request.output.join(format!("fold-{fold_index:04}"));
        fs::create_dir_all(&output)?;
        context.report(format!(
            "{}年度 {year}",
            if request.reuse_training.is_some() {
                "预测/回测"
            } else {
                "训练"
            }
        ))?;
        let parameters = request.config.training.parameters.clone();
        let training_request = TrainRequest {
            factor_ids: ids.clone(),
            universe_id: request.universe_id.clone(),
            date_range: f.train.clone(),
            objective: Objective::Regression,
            parameters,
            early_stopping_rounds: if f.validation.is_some() {
                request.config.training.early_stopping_rounds
            } else {
                None
            },
        };
        let (model_path, _manifest_path) = if let Some(source) = &request.reuse_training {
            let model_index = mode
                .filter(|m| m.predicting())
                .map_or(fold_index, |m| m.segments[fold_index].model_index);
            let fold = source.join(format!("fold-{model_index:04}"));
            (fold.join("model.txt"), fold.join("manifest.json"))
        } else {
            let (tx, rx) = mpsc::channel();
            let training_context = context.clone();
            let progress_thread = std::thread::spawn(move || {
                while let Ok(event) = rx.recv() {
                    let _ = training_context.report(format!("{event:?}"));
                }
            });
            let artifact_result = LightGbmTrainer.train_split(
                &training_request,
                &split,
                &output,
                tx,
                context.cancel.clone(),
            );
            let _ = progress_thread.join();
            let artifact = artifact_result?;
            context.check()?;
            let mut manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(&artifact.manifest_path)?)?;
            let factor_refs = analytics_factor::selection::frozen_refs(
                request.factor_set,
                &request.config.factor_refs,
                &ids,
            );
            let annotations = artifacts::ModelAnnotations {
                schema_version: 3,
                factor_definitions: registry.manifest_definitions(&ids)?,
                training_schema_hash: analytics_factor::selection::hash(&(
                    &manifest["features"],
                    &serde_json::to_value(&factor_refs)?,
                ))?,
                factor_refs,
                formula_version: &formula_version,
                input_snapshot_hash: &signature,
                training_signature: &training_signature,
                research_execution: serde_json::json!({"factor_set":request.factor_set,"price_basis":request.price_basis}),
                rolling_fold: serde_json::json!({"fold_index":fold_index,"schedule":resolved,"purged_label_boundary":true}),
            };
            manifest
                .as_object_mut()
                .ok_or("invalid model manifest")?
                .extend(
                    serde_json::to_value(annotations)?
                        .as_object()
                        .ok_or("invalid annotations")?
                        .clone(),
                );
            save(&artifact.manifest_path, &manifest)?;
            (artifact.model_path, artifact.manifest_path)
        };
        if request.train_only {
            continue;
        }
        let train_rows = split.train.rows;
        let validation_rows = split.validation.rows;
        let test = test.expect("prediction phase");
        let scores = if test.meta.rows.is_empty() {
            Vec::new()
        } else {
            LightGbmPredictor { model_path }
                .predict_dense(
                    &test.features.iter().map(|v| *v as f64).collect::<Vec<_>>(),
                    test.meta.rows.len(),
                    ids.len(),
                    false,
                )?
                .predictions
                .into_iter()
                .map(|row| row[0])
                .collect::<Vec<_>>()
        };
        drop(split);
        if let Some(m) = mode
            && m.predicting()
        {
            independent::record_predictions(
                request,
                m,
                fold_index,
                &dataset.calendar,
                &test.meta.rows,
                &scores,
            )?;
        }
        let (ic_rows, ic_scores): (Vec<_>, Vec<_>) = test
            .meta
            .rows
            .iter()
            .zip(&scores)
            .filter(|(row, _)| {
                row.label.is_some() && row.label_end.is_some_and(|d| d <= f.test.end)
            })
            .map(|(row, score)| (row.clone(), *score))
            .unzip();
        let (ic, rank_ic) = portfolio::information_coefficients(&ic_rows, &ic_scores);
        signals.extend(
            test.meta
                .rows
                .iter()
                .zip(&scores)
                .map(|(r, s)| (dataset.calendar[r.day], r.instrument.clone(), *s)),
        );
        let performance = None;
        let report = FoldReport {
            year,
            train_start: f.train.start.to_string(),
            train_end: f.train.end.to_string(),
            valid_start: f
                .validation
                .as_ref()
                .map(|v| v.start.to_string())
                .unwrap_or_default(),
            valid_end: f
                .validation
                .as_ref()
                .map(|v| v.end.to_string())
                .unwrap_or_default(),
            test_start: f.test.start.to_string(),
            test_end: f.test.end.to_string(),
            train_rows,
            validation_rows,
            test_rows: scores.len(),
            features: ids.len(),
            ic,
            rank_ic,
            performance,
        };
        eprintln!(
            "fold={year} rank_ic={rank_ic:?} metrics={}",
            serde_json::to_string(&report.performance)?
        );
        save(output.join("report.json"), &report)?;
        let mut csv = String::from("signal_date,instrument,score,label_next_open_return\n");
        for (r, score) in test.meta.rows.iter().zip(&scores) {
            csv.push_str(&format!(
                "{},{},{},{}\n",
                dataset.calendar[r.day],
                r.instrument,
                score,
                r.label.map(|v| v.to_string()).unwrap_or_default()
            ));
        }
        fs::write(output.join("predictions.csv"), csv)?;
        reports.push(report);
        save(request.output.join("fold_reports.json"), &reports)?;
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
    let features = FeatureSamples {
        signed,
        cache,
        training_signature,
    };
    Ok(FoldOutputs {
        features,
        signals,
        reports,
    })
}
