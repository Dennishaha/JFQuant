//! Research prepare phase.
use super::*;
pub(super) fn prepare(
    request: &ResearchRequest,
    mode: Option<&independent::Execution>,
    context: &TaskContext,
) -> Result<Option<PreparedResearch>> {
    let predicting = mode.is_some_and(|m| m.predicting());
    let training_source_request = request.clone();
    context.check()?;
    request.config.validate()?;
    request.quality_policy.validate()?;
    if request.output.join("summary.json").exists() {
        let existing = serde_json::from_slice(&fs::read(request.output.join("summary.json"))?)?;
        validate_result(&existing)?;
    }
    if mode.is_none() {
        request.schedule.validate()?;
    }
    if request.action_policy.exercise_optional_cash_choice {
        return Err("unsupported active cash-choice policy".into());
    }
    request.market_rules.validate()?;
    if !request.dividend_withholding_rate.is_finite()
        || !(0.0..=1.0).contains(&request.dividend_withholding_rate)
    {
        return Err("invalid dividend withholding rate".into());
    }
    let action_book = request
        .corporate_actions
        .as_ref()
        .map(|path| analytics_storage::corporate_actions::ActionBundle::read(path))
        .transpose()?;
    let action_hash = request
        .corporate_actions
        .as_ref()
        .map(|path| {
            fs::read(path).map(|bytes| {
                format!(
                    "sha256:{:x}",
                    analytics_core::hash::Hex(Sha256::digest(bytes))
                )
            })
        })
        .transpose()?;
    if !request.initial_cash_cny.is_finite()
        || request.initial_cash_cny <= 0.0
        || request.initial_cash_cny > 1e13
    {
        return Err("invalid initial cash in CNY".into());
    }
    fs::create_dir_all(&request.output)?;
    let source_path = request
        .data_root
        .parent()
        .ok_or("provider needs source parent")?
        .join("jfquant_source.json");
    let source: serde_json::Value = serde_json::from_slice(&fs::read(&source_path)?)?;
    let market_content_sha256 = crate::market_content_hash(&request.data_root)?;
    let synthetic = source["generator"] == "JFQuant analytics-workflow synthetic-demo-v1"
        && source["synthetic_fixture"] == true;
    if !synthetic
        && (source["downloader"] != "JFQuant analytics-storage"
            || (source["archive_verified"] != true && source["snapshot_verified"] != true))
    {
        return Err(
            "data must be independently acquired and verified by jfquant data download".into(),
        );
    }
    if action_book.as_ref().is_some_and(|book| {
        (book.downloader == "JFQuant analytics-workflow synthetic-demo-v1") != synthetic
    }) {
        return Err("cannot mix synthetic and public company actions/market data".into());
    }
    let mut dataset = DailyDataset::open(&request.data_root)?;
    if request.price_basis == analytics_core::research_quality::PriceBasis::Provider
        && !dataset.supports_provider_training()
    {
        return Err(
            "unsupported_provider_price_basis: registered Qlib, verified TDX or HiThink supplier quotations required".into(),
        );
    }
    if request.price_basis == analytics_core::research_quality::PriceBasis::Verified
        && dataset.requires_supplier_training()
    {
        return Err(
            "supplier_price_basis_requires_provider: select provider price basis for HiThink"
                .into(),
        );
    }
    let mut pool = match &request.pool {
        Some(p) => p.clone(),
        None => dataset.pool(&request.universe_id)?,
    };
    let last = dataset.calendar.last().copied().ok_or("empty calendar")?;
    let folds = if let Some(m) = mode {
        m.folds.clone()
    } else {
        match request
            .schedule
            .resolve_with_warmup(&dataset.calendar, request.warmup_sessions())
        {
            Ok(f) => f,
            Err(e) => {
                save(
                    request.output.join("execution_readiness.json"),
                    &serde_json::json!({"ready":false,"reason":e,"checks":{"schedule":"failed","events":"not_run"}}),
                )?;
                return if request.audit_only {
                    Ok(None)
                } else {
                    Err(format!("data_not_ready: {e}").into())
                };
            }
        }
    };
    let history_start = if predicting {
        folds[0].actual.test.start
    } else {
        folds.iter().map(|f| f.actual.train.start).min().unwrap()
    };
    let first_required = history_start;
    let execution_start = folds[0].actual.test.start;
    let needed_end = if request.train_only {
        folds
            .iter()
            .map(|f| f.actual.validation.as_ref().unwrap_or(&f.actual.train).end)
            .max()
            .unwrap()
    } else {
        folds.last().unwrap().actual.test.end
    };
    // Irrelevant former constituents are kept in the frozen source but cannot block later windows.
    let history_index = dataset
        .calendar
        .partition_point(|d| *d < history_start)
        .saturating_sub(request.warmup_sessions());
    let relevant_start = dataset.calendar[history_index];
    dataset.restrict_reads(DateRange {
        start: relevant_start,
        end: needed_end,
    })?;
    pool.intervals.retain(|_, ranges| {
        ranges.retain(|r| r.end >= relevant_start && r.start <= needed_end);
        !ranges.is_empty()
    });
    let registry = request.factor_registry()?;
    let quality_snapshot = quality::qualify_with_registry(
        &dataset,
        &pool,
        &request.quality_policy,
        &DateRange {
            start: relevant_start,
            end: needed_end,
        },
        &request.config.factor_ids,
        &registry,
    )?;
    save(request.output.join("quality.json"), &quality_snapshot)?;
    pool = quality_snapshot.effective_pool.clone();
    // Both factor and execution workers consume exactly this frozen derived pool.
    let mut effective_request = request.clone();
    effective_request.pool = Some(pool.clone());
    let request = &effective_request;
    let pool_problem = pool.validate().err();
    let members = dataset.align_membership(&pool);
    let pool_signature = format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&pool)?))
    );
    analytics_core::pool::validate_instrument(&request.benchmark)?;
    let first_member = dataset
        .calendar
        .partition_point(|d| *d < pool.coverage.start)
        .min(dataset.calendar.len() - 1);
    let mut admission_issues = Vec::<String>::new();
    if quality_snapshot
        .decisions
        .iter()
        .any(|d| d.disposition == ProblemDisposition::Block)
    {
        admission_issues.push("input/source checks failed; see quality decisions".into());
    }
    if let Some(problem) = &pool_problem {
        admission_issues.push(problem.clone());
    }
    if !dataset.has_raw_prices() {
        admission_issues.push("raw execution price basis is unverified".into());
    }
    if dataset.calendar[0] > history_start + chrono::Duration::days(7)
        || pool.coverage.start > first_required
        || pool.coverage.end < needed_end
    {
        admission_issues.push("historical pool/market coverage incomplete".into());
    }
    for symbol in members.keys() {
        if dataset.is_price_index(symbol).unwrap_or(false) {
            admission_issues.push(format!(
                "{symbol}: price indices cannot be traded as A-share pool constituents"
            ));
        }
    }
    let mut market_issues = BTreeMap::new();
    let mut allowed_market_missing = BTreeMap::new();
    for symbol in members.keys().chain(std::iter::once(&request.benchmark)) {
        let mut problems = vec![];
        for field in [
            "open", "high", "low", "close", "volume", "amount", "vwap", "factor",
        ] {
            if let Err(e) = dataset.research_field(
                symbol,
                field,
                request.quality_policy.strictness
                    == analytics_core::research_quality::ResearchStrictness::QualifiedOnly,
            ) {
                if request.quality_policy.allows_missing()
                    && symbol != &request.benchmark
                    && e.kind() == std::io::ErrorKind::NotFound
                    && field != "factor"
                {
                    allowed_market_missing
                        .entry(symbol.clone())
                        .or_insert_with(Vec::new)
                        .push(format!("{field}: {e}"));
                } else {
                    problems.push(format!("{field}: {e}"));
                }
            }
        }
        if !problems.is_empty() {
            market_issues.insert(symbol.clone(), problems);
        }
    }
    if !market_issues.is_empty() {
        admission_issues.push("required market fields unavailable; see market_issues".into());
    }
    let mut factor_basis_issues = BTreeMap::new();
    for symbol in members.keys().chain(std::iter::once(&request.benchmark)) {
        context.check()?;
        if let Err(error) = dataset
            .validate_factor_basis(symbol)
            .and_then(|_| dataset.validate_coefficient_origin(symbol, synthetic))
        {
            factor_basis_issues.insert(symbol.clone(), error.to_string());
        }
    }
    let mut adjustment_issues = BTreeMap::new();
    {
        let mut symbols: BTreeSet<_> = members.keys().cloned().collect();
        if !dataset.is_price_index(&request.benchmark).unwrap_or(false) {
            symbols.insert(request.benchmark.clone());
        }
        match &action_book {
            Some(book) => {
                for id in &symbols {
                    let until = pool
                        .intervals
                        .get(id)
                        .and_then(|rs| rs.iter().map(|r| r.end).max())
                        .map_or(needed_end, |d| d.min(needed_end));
                    adjustment_issues.extend(adjustment::adjustment_issues(
                        &dataset,
                        book,
                        &BTreeSet::from([id.clone()]),
                        relevant_start,
                        until,
                    )?);
                }
            }
            None => {
                adjustment_issues.insert(
                    "corporate_actions".into(),
                    vec![
                        "registered history required for training/warmup adjustment verification"
                            .into(),
                    ],
                );
            }
        }
    }
    // Account dependencies are checked only on actual orders, holdings and entitlements.
    let settlement_issues = BTreeMap::<String, String>::new();
    let successor_issues = BTreeMap::<String, String>::new();
    let benchmark_check = (|| -> Result<()> {
        dataset.validate_factor_basis(&request.benchmark)?;
        Ok(())
    })();
    let sample_readiness =
        preview_samples_with_policy(&dataset, &pool, &folds, &request.quality_policy)?;
    if sample_readiness["issues"]
        .as_object()
        .is_some_and(|m| !m.is_empty())
    {
        admission_issues.push("sample input reads failed; see sample_readiness.issues".into());
    }
    if !predicting
        && sample_readiness["issues"]
            .as_object()
            .is_some_and(|m| m.is_empty())
        && sample_readiness["folds"].as_array().is_some_and(|rows| {
            rows.iter().enumerate().any(|(i, r)| {
                r["training_label_rows"] == 0
                    || (folds[i].actual.validation.is_some() && r["validation_label_rows"] == 0)
            })
        })
    {
        admission_issues.push("no eligible labeled training/validation rows".into());
    }
    if !adjustment_issues.is_empty()
        && request.price_basis == analytics_core::research_quality::PriceBasis::Verified
    {
        admission_issues
            .push("historical coefficient/company-action mismatch; see adjustment_issues".into());
    }
    if !factor_basis_issues.is_empty() {
        admission_issues
            .push("factor price/volume normalization unavailable; see factor_basis_issues".into());
    }
    use analytics_core::admission::{AdmissionCheck, AdmissionReport, CheckStatus};
    let mut admission = AdmissionReport {
        price_basis: request.price_basis,
        schema_version: 5,
        policy: request.quality_policy.clone(),
        decisions: quality_snapshot.decisions.clone(),
        checks: Vec::new(),
    };
    let mut add_check =
        |category: &str, security: Option<String>, failed: bool, message: String| {
            let mut evidence = vec![format!("market-source:{}", request.data_root.display())];
            if matches!(category, "historical_events" | "settlement" | "successor")
                && let Some(book) = &action_book
            {
                if let Some(hash) = &action_hash {
                    evidence.push(format!("action-bundle:{hash}"));
                }
                // The verified bundle indexes all originals. Repeat only this
                // security's coverage receipts, rather than every document per issue.
                if let Some(id) = &security {
                    for name in book
                        .coverage
                        .get(id)
                        .into_iter()
                        .flatten()
                        .flat_map(|c| &c.evidence)
                    {
                        if let Some(hash) = book.raw_sha256.get(name) {
                            evidence.push(format!("action:{name}:{hash}"));
                        }
                    }
                }
            }
            evidence.sort();
            evidence.dedup();
            admission.checks.push(AdmissionCheck {
                stage: Default::default(),
                kind: if failed {
                    ProblemKind::ExecutionDependency
                } else {
                    ProblemKind::None
                },
                disposition: ProblemDisposition::Block,
                category: category.into(),
                security,
                coverage: Some(DateRange {
                    start: relevant_start,
                    end: needed_end,
                }),
                status: if failed {
                    CheckStatus::Failed
                } else {
                    CheckStatus::Passed
                },
                code: if failed {
                    format!("{category}_not_ready")
                } else {
                    format!("{category}_verified")
                },
                message,
                evidence,
                affected_factors: request.config.factor_ids.clone(),
            });
        };
    add_check(
        "membership",
        None,
        pool_problem.is_some()
            || pool.coverage.start > first_required
            || pool.coverage.end < needed_end,
        "historical membership and unknown warmup coverage".into(),
    );
    for (category, issues) in [
        ("market", serde_json::to_value(&market_issues)?),
        ("coefficient", serde_json::to_value(&factor_basis_issues)?),
        (
            "historical_events",
            serde_json::to_value(&adjustment_issues)?,
        ),
    ] {
        if let Some(map) = issues.as_object() {
            if map.is_empty() {
                add_check(category, None, false, "required inputs checked".into());
            } else {
                for (id, issue) in map {
                    let message = if let Some(items) = issue.as_array() {
                        format!(
                            "{} input issues; first: {}",
                            items.len(),
                            serde_json::to_string(&items.iter().take(3).collect::<Vec<_>>())
                                .unwrap_or_default()
                        )
                    } else {
                        issue.to_string()
                    };
                    add_check(category, Some(id.clone()), true, message);
                }
            }
        }
    }
    add_check(
        "samples",
        None,
        sample_readiness["issues"]
            .as_object()
            .is_some_and(|m| !m.is_empty())
            || (!predicting
                && sample_readiness["folds"].as_array().is_some_and(|rs| {
                    rs.iter().enumerate().any(|(i, r)| {
                        r["training_label_rows"] == 0
                            || (folds[i].actual.validation.is_some()
                                && r["validation_label_rows"] == 0)
                    })
                })),
        sample_readiness.to_string(),
    );
    add_check(
        "benchmark",
        Some(request.benchmark.clone()),
        benchmark_check.is_err(),
        benchmark_check
            .as_ref()
            .err()
            .map_or_else(|| "benchmark closes checked".into(), |e| e.to_string()),
    );
    for issue in &admission_issues {
        add_check("dependency", None, true, issue.clone());
    }
    for (id, problems) in &allowed_market_missing {
        admission.checks.push(AdmissionCheck {
            stage: Default::default(),
            kind: ProblemKind::Missing,
            disposition: ProblemDisposition::KeepNull,
            category: "market".into(),
            security: Some(id.clone()),
            coverage: Some(DateRange {
                start: relevant_start,
                end: needed_end,
            }),
            status: CheckStatus::Failed,
            code: "input_file_missing".into(),
            message: problems.join("; "),
            evidence: vec![format!("market:{}", dataset.root.display())],
            affected_factors: request.config.factor_ids.clone(),
        });
    }
    if action_book.is_none() {
        for check in &mut admission.checks {
            if matches!(
                check.category.as_str(),
                "historical_events" | "settlement" | "successor"
            ) {
                check.status = CheckStatus::NotRun;
                check.code = "action_snapshot_missing".into();
                check.message = "no action snapshot supplied; event checks not executed".into();
            }
        }
    }
    for check in &mut admission.checks {
        if matches!(
            check.category.as_str(),
            "settlement" | "successor" | "account_events"
        ) {
            check.status = CheckStatus::NotRun;
            check.kind = analytics_core::research_quality::ProblemKind::ExecutionDependency;
            check.disposition = analytics_core::research_quality::ProblemDisposition::Warn;
            check.code = "account_path_not_run".into();
            check.message =
                "checked on actual orders, holdings and entitlements; not a training dependency"
                    .into();
        }
    }
    if request.price_basis == analytics_core::research_quality::PriceBasis::Provider {
        for check in &mut admission.checks {
            if check.category == "historical_events" && check.status != CheckStatus::Passed {
                check.disposition = analytics_core::research_quality::ProblemDisposition::Warn;
                check.code = "provider_adjustment_unreconciled".into();
            }
        }
    }
    admission.checks.push(AdmissionCheck {
        stage: analytics_core::admission::AdmissionStage::Account,
        kind: ProblemKind::ExecutionDependency,
        disposition: ProblemDisposition::Warn,
        category: "account_events".into(),
        security: None,
        coverage: Some(DateRange {
            start: folds.first().unwrap().actual.test.start,
            end: folds.last().unwrap().actual.test.end,
        }),
        status: CheckStatus::NotRun,
        code: "account_path_not_run".into(),
        message: "静态执行前提单独核验；持仓估值与订单结果在账户运行时检查".into(),
        evidence: vec![],
        affected_factors: vec![],
    });
    if !request.train_only {
        let execution_checks = execution::checks(
            request,
            &dataset,
            action_book.as_ref(),
            &pool,
            &DateRange {
                start: execution_start,
                end: folds.last().unwrap().actual.test.end,
            },
        )?;
        if execution_checks
            .iter()
            .any(|c| c.status == CheckStatus::Failed)
        {
            admission_issues
                .push("execution dependencies unavailable; see admission execution checks".into());
        }
        admission.checks.extend(execution_checks);
    }
    admission.checks.push(AdmissionCheck {
        stage: Default::default(),
        kind: ProblemKind::Missing,
        disposition: ProblemDisposition::Block,
        category: "feature_eligibility".into(),
        security: None,
        coverage: Some(DateRange {
            start: history_start,
            end: needed_end,
        }),
        status: CheckStatus::NotRun,
        code: "prerequisites_pending".into(),
        message: "finite factor samples not yet computed in private staging".into(),
        evidence: vec![],
        affected_factors: request.config.factor_ids.clone(),
    });
    save(request.output.join("admission.json"), &admission)?;
    if !admission_issues.is_empty() {
        let affected_factor_ids: Vec<_> =
            if factor_basis_issues.is_empty() && adjustment_issues.is_empty() {
                Vec::new()
            } else {
                registry
                    .all()
                    .filter(|s| {
                        !matches!(
                            s.expression,
                            analytics_factor::FactorExpr::Unsupported { .. }
                        ) && (request.config.factor_ids.is_empty()
                            || request.config.factor_ids.contains(&s.id))
                    })
                    .map(|s| s.id.clone())
                    .collect()
            };
        save(
            request.output.join("input_issues.json"),
            &serde_json::json!({"market_issues":market_issues,"factor_basis_issues":factor_basis_issues,"adjustment_issues":adjustment_issues,"settlement_issues":settlement_issues,"successor_issues":successor_issues}),
        )?;
        save(
            request.output.join("execution_readiness.json"),
            &serde_json::json!({"schema_version":8,"ready":false,
            "stages":{"training":if request.reuse_training.is_some(){"completed"}else{"not_started"},"prediction":"not_started","backtest":"blocked"},
            "training_complete":request.reuse_training.is_some(),"backtest_complete":false,"reason":admission_issues.join("; "),"issues":admission_issues,"affected_factor_ids":affected_factor_ids,
            "references":{"admission":{"path":"admission.json","sha256":file_hash(&request.output.join("admission.json"))?},"quality":{"path":"quality.json","sha256":file_hash(&request.output.join("quality.json"))?},"input_issues":{"path":"input_issues.json","sha256":file_hash(&request.output.join("input_issues.json"))?}},
            "blocking_checks":admission.checks.iter().filter(|c|c.status==CheckStatus::Failed && c.disposition==ProblemDisposition::Block).collect::<Vec<_>>(),
            "warnings":admission.checks.iter().filter(|c|c.disposition==ProblemDisposition::Warn && c.stage==analytics_core::admission::AdmissionStage::Execution).collect::<Vec<_>>(),"resolved_folds":folds}),
        )?;
        return if request.audit_only {
            Ok(None)
        } else {
            Err("data_not_ready: input admission failed; see execution_readiness.json".into())
        };
    }
    Ok(Some(PreparedResearch {
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
    }))
}
