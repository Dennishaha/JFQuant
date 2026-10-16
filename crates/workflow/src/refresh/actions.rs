//! Refresh actions stage.
use super::*;
pub(super) fn reference_route(
    p: &RefreshPlan,
    request: &RefreshRequest,
    staging: &Path,
    context: &TaskContext,
    role: DataRole,
) -> Result<(Option<Value>, Vec<Value>)> {
    let kind = if role == DataRole::Calendar {
        "calendar"
    } else {
        "members"
    };
    let mut attempts = vec![];
    for provider in p.source_policy.routes(role, request.provider) {
        if !matches!(provider, Provider::Tdxaidata | Provider::Hithink) {
            attempts.push(json!({"provider":provider,"status":"not_run","code":"reference_capability_unavailable"}));
            continue;
        }
        let mut r = collection(
            provider,
            &request.template.benchmark,
            &p.history,
            &request.template.benchmark,
        );
        if role == DataRole::Members {
            r.universes.insert(
                if request.template.benchmark == "SH000300" {
                    "csi300"
                } else {
                    "csi500"
                }
                .into(),
            );
        }
        let root = staging
            .join(format!("reference-{kind}"))
            .join(format!("{provider:?}"));
        match collector::collect_reference(&r, &root, context, kind) {
            Ok(path) => {
                let value = serde_json::from_slice(&fs::read(path)?)?;
                attempts.push(json!({"provider":provider,"status":"passed"}));
                return Ok((Some(value), attempts));
            }
            Err(e) => {
                context.check()?;
                attempts.push(json!({"provider":provider,"status":"failed","code":"reference_collection_failed","reason":e.to_string()}));
            }
        }
    }
    Ok((None, attempts))
}

pub(super) fn action_histories_match(a: &ActionBundle, b: &ActionBundle) -> bool {
    a.actions.len() == b.actions.len()
        && a.actions.iter().all(|x| {
            b.actions.iter().any(|y| {
                x.instrument == y.instrument
                    && x.ex_date == y.ex_date
                    && x.record_date == y.record_date
                    && (x.cash() - y.cash()).abs() <= 1e-6
                    && (x.shares() - y.shares()).abs() <= 1e-6
                    && x.terms == y.terms
                    && (x.pay_date.is_none() || y.pay_date.is_none() || x.pay_date == y.pay_date)
                    && (x.listing_date.is_none()
                        || y.listing_date.is_none()
                        || x.listing_date == y.listing_date)
            })
        })
}

pub(super) fn classify_zero_rights(
    issue: &mut analytics_storage::execution_data::ExecutionIssue,
    candidates: &[Value],
    evidence: &str,
) -> bool {
    if issue.code != "hithink_event_type_unknown" || issue.dates.start != issue.dates.end {
        return false;
    }
    let number = |row: &Value, key: &str| {
        row[key]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|n| n.is_finite())
    };
    let matches: Vec<_> = candidates
        .iter()
        .filter(|row| row["date"] == issue.dates.start.to_string())
        .collect();
    if matches.len() != 1 {
        return false;
    }
    let row = matches[0];
    if row["type"] != "1"
        || !number(row, "allotment_native").is_some_and(|n| n > 0.)
        || !number(row, "allot_price").is_some_and(|n| n > 0.)
        || number(row, "bonus_per_ten_shares") != Some(0.)
        || number(row, "share_bonus_native") != Some(0.)
    {
        return false;
    }
    issue.code = "execution_event_unsupported_skipped".into();
    issue.event_category = "rights_issue".into();
    issue.evidence.push(evidence.into());
    issue.message="HiThink零现金零送股事件与同日通达信配股参考匹配；无认购及交付条款，AKQuant跳过，不补造现金或股份".into();
    true
}
// Compatibility with the observed two-decimal per-ten reference is not proof
// of rounding or an independent verification. Never modify primary source terms.
pub(super) fn reference_precision_compatible(
    action: &analytics_core::execution::CorporateAction,
    candidate: &Value,
) -> bool {
    let compatible = |field: &str, per_share: f64| {
        let Some(text) = candidate[field].as_str() else {
            return false;
        };
        let Some((_, fraction)) = text.split_once('.') else {
            return false;
        };
        fraction.len() == 2
            && fraction.bytes().all(|b| b.is_ascii_digit())
            && text.parse::<f64>().ok().is_some_and(|value| {
                value.is_finite()
                    && per_share.is_finite()
                    && (per_share * 10. - value).abs() <= 0.005 + 1e-10
            })
    };
    compatible("bonus_per_ten_shares", action.cash())
        && compatible("share_bonus_native", action.shares())
}

pub(super) fn attach_reference_evidence(
    chosen: &mut (ActionBundle, PathBuf),
    reference: &ActionBundle,
    root: &Path,
) -> Result<String> {
    let mut structured = None;
    for (name, hash) in &reference.raw_sha256 {
        let target = if name.ends_with("-actions-structured.json")
            || name == "complete-sources-structured.json"
        {
            format!("tdx-reference-{name}")
        } else {
            name.clone()
        };
        if name.ends_with("-actions-structured.json") {
            structured = Some(target.clone());
        }
        if let Some(previous) = chosen.0.raw_sha256.get(&target) {
            if previous != hash {
                return Err("reference_evidence_collision".into());
            }
            continue;
        }
        fs::copy(
            root.join("raw").join(name),
            chosen.1.join("raw").join(&target),
        )?;
        chosen.0.raw_sha256.insert(target, hash.clone());
    }
    structured.ok_or_else(|| "reference_structured_evidence_missing".into())
}

pub(super) struct ActionShard {
    pub(super) selected: Option<(ActionBundle, PathBuf)>,
    pub(super) diagnostics: Vec<Value>,
    pub(super) action_attempts: Vec<Value>,
    pub(super) execution_attempts: Vec<Value>,
}
pub(super) fn acquire_actions(
    s: &SecurityPlan,
    request: &RefreshRequest,
    p: &RefreshPlan,
    staging: &Path,
    context: &TaskContext,
) -> Result<ActionShard> {
    let mut selected = vec![];
    let mut diagnostics = vec![];
    let mut action_attempts = vec![];
    let mut execution_attempts = vec![];
    let mut choices: Vec<(ActionBundle, PathBuf)> = vec![];
    let mut tdx_candidates = vec![];
    let mut tdx_reference = None;
    let action_routes = p.source_policy.routes(DataRole::Actions, request.provider);
    let execution_routes = p
        .source_policy
        .routes(DataRole::Execution, request.provider);
    let mut routes = action_routes.clone();
    for provider in &execution_routes {
        if !routes.contains(provider) {
            routes.push(*provider);
        }
    }
    for provider in routes {
        context.check()?;
        if !matches!(provider, Provider::Tdxaidata | Provider::Hithink) {
            action_attempts.push(
                json!({"provider":provider,"status":"not_run","code":"action_route_unsupported"}),
            );
            continue;
        }
        let path = staging.join(format!("actions-{}-{provider:?}", s.instrument));
        let r = collection(provider, &s.instrument, &p.history, "");
        match collector::collect_actions(&r, &path, context) {
            Ok(file) => {
                let b = ActionBundle::read(&file)?;
                if provider == Provider::Tdxaidata {
                    tdx_reference = Some((b.clone(), path.clone()));
                    let shard: Value = serde_json::from_slice(&fs::read(
                        path.join("raw")
                            .join(format!("{}-actions-structured.json", s.instrument)),
                    )?)?;
                    tdx_candidates = shard["evidence"]["candidates"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    diagnostics.push(json!({"instrument":s.instrument,"provider":provider,"candidates":tdx_candidates,"native_reference_only":true,"path":path}));
                }
                if b.failures.is_empty() && b.structured_coverage.contains_key(&s.instrument) {
                    action_attempts.push(json!({"provider":provider,"status":"passed"}));
                    choices.push((b, path));
                } else {
                    action_attempts.push(json!({"provider":provider,"status":"not_run","code":"account_fields_require_supplement","failures":b.failures}));
                }
            }
            Err(e) => {
                context.check()?;
                action_attempts
                    .push(json!({"provider":provider,"status":"failed","reason":e.to_string()}));
            }
        }
    }
    for provider in execution_routes {
        let status = "not_run";
        execution_attempts.push(json!({"provider":provider,"status":status,"code":if provider==Provider::Tdxaidata {"historical_execution_not_verified"}else if provider==Provider::Hithink {"historical_execution_route_unavailable"}else{"see_source_capabilities"}}));
    }
    if !choices.is_empty() {
        let mut chosen = choices.remove(0);
        for other in choices {
            let missing = |book: &ActionBundle| {
                book.actions
                    .iter()
                    .filter(|a| {
                        (a.cash() > 0. && a.pay_date.is_none())
                            || (a.shares() > 0. && a.listing_date.is_none())
                    })
                    .count()
            };
            // A complete preferred history does not need a second provider
            // to approve it. Match events only when joining missing dates;
            // other providers can still supply independent execution fields.
            if missing(&chosen.0) == 0 || action_histories_match(&chosen.0, &other.0) {
                if missing(&chosen.0) > 0 && missing(&other.0) < missing(&chosen.0) {
                    diagnostics.push(json!({"instrument":s.instrument,"code":"matched_event_history_source_supplement","selected":other.1}));
                    chosen = other;
                } else if let Some(aux) = other.0.execution_data {
                    let target = chosen.0.execution_data.get_or_insert_with(|| {
                        analytics_storage::execution_data::ExecutionData {
                            schema_version: 1,
                            ..Default::default()
                        }
                    });
                    target.suspensions.extend(aux.suspensions);
                    target.limits.extend(aux.limits);
                    target.capabilities.extend(aux.capabilities);
                    // Every auxiliary source byte remains authenticated, including its structured reply.
                    for (name, hash) in other.0.raw_sha256 {
                        let new = if name.ends_with("-actions-structured.json")
                            || name == "complete-sources-structured.json"
                        {
                            format!("supplement-{name}")
                        } else {
                            name.clone()
                        };
                        if !chosen.0.raw_sha256.contains_key(&new) {
                            fs::copy(
                                other.1.join("raw").join(name),
                                chosen.1.join("raw").join(&new),
                            )?;
                            chosen.0.raw_sha256.insert(new, hash);
                        }
                    }
                }
            } else {
                chosen
                    .0
                    .execution_issues
                    .push(analytics_storage::execution_data::ExecutionIssue {
                        id: String::new(),
                        event_category: String::new(),
                        evidence: Vec::new(),
                        instrument: s.instrument.clone(),
                        announced: None,
                        dates: p.history.clone(),
                        code: "source_event_conflict".into(),
                        message: "完整事件来源在金额、登记日或事件集合上不一致，未按优先级覆盖"
                            .into(),
                    });
                diagnostics.push(json!({"instrument":s.instrument,"code":"source_event_conflict","other":other.1}));
            }
        }
        let mut reference_evidence = Vec::new();
        if let Some((reference, root)) = &tdx_reference {
            let evidence = attach_reference_evidence(&mut chosen, reference, root)?;
            reference_evidence.push(evidence.clone());
            let shard: Value = serde_json::from_slice(&fs::read(
                root.join("raw")
                    .join(format!("{}-actions-structured.json", s.instrument)),
            )?)?;
            if shard["evidence"]["stock_status"]["status"] == "captured" {
                let source = reference.source_symbol(&s.instrument)?;
                let symbol = format!("{}.{}", &source[2..], &source[..2]);
                match analytics_storage::tdx_status::from_evidence(
                    root,
                    &shard["evidence"],
                    &symbol,
                    &p.history,
                ) {
                    Ok(suspended) => {
                let data = chosen.0.execution_data.get_or_insert_with(|| {
                    analytics_storage::execution_data::ExecutionData {
                        schema_version: 1,
                        ..Default::default()
                    }
                });
                data.schema_version = 2;
                data.suspensions.insert(s.instrument.clone(), suspended);
                data.capabilities.insert(format!("{}:{}", analytics_storage::tdx_status::RULE, s.instrument), json!({"status":"available","source_symbol":symbol,"range":p.history,"structured":evidence,"use":"retrospective-account-validation-only;not-a-signal","right_censored":true}));
                    }
                    Err(error) => diagnostics.push(json!({"instrument":s.instrument,"code":"historical_status_unavailable","reason":error.to_string(),"evidence":evidence})),
                }
            }
            for issue in &mut chosen.0.execution_issues {
                classify_zero_rights(issue, &tdx_candidates, &evidence);
            }
        }
        for candidate in &tdx_candidates {
            let date: Date = serde_json::from_value(candidate["date"].clone())?;
            let allot = candidate["allotment_native"]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.);
            let ordinary = candidate["type"] == "1" && allot == 0.;
            if candidate["type"] == "1"
                && allot > 0.
                && chosen.0.execution_issues.iter().any(|issue| {
                    issue.dates.start == date
                        && issue.event_category == "rights_issue"
                        && issue.skipped_by_engine()
                })
            {
                continue;
            }
            let cash = candidate["bonus_per_ten_shares"]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .map(|v| v / 10.);
            let matched = chosen.0.actions.iter().any(|a| {
                a.ex_date == date
                    && cash.is_some_and(|v| (v - a.cash()).abs() <= 1e-6)
                    && candidate["share_bonus_native"]
                        .as_str()
                        .and_then(|s| s.parse::<f64>().ok())
                        .is_some_and(|shares| (shares / 10. - a.shares()).abs() <= 1e-6)
            });
            let precision_compatible = ordinary
                && !matched
                && chosen
                    .0
                    .actions
                    .iter()
                    .any(|a| a.ex_date == date && reference_precision_compatible(a, candidate));
            if !ordinary || !matched {
                chosen
                    .0
                    .execution_issues
                    .push(analytics_storage::execution_data::ExecutionIssue {
                        id: String::new(),
                        event_category: String::new(),
                        evidence: reference_evidence.clone(),
                        instrument: s.instrument.clone(),
                        announced: None,
                        dates: DateRange {
                            start: date,
                            end: date,
                        },
                        code: if precision_compatible {
                            "tdx_reference_precision_difference"
                        } else if ordinary {
                            "source_event_conflict"
                        } else {
                            "tdx_special_action_requires_review"
                        }
                        .into(),
                        message: if precision_compatible {
                            "通达信两位小数每十股参考与供应商事件在半个显示单位内相容；保留差异，采用主源条款，未宣称独立核验".into()
                        } else {
                            "通达信事件参考与账户事件未完成匹配，保留原始证据".into()
                        },
                    });
            }
        }
        selected.push(chosen);
    }
    Ok(ActionShard {
        selected: selected.pop(),
        diagnostics,
        action_attempts,
        execution_attempts,
    })
}

pub(super) fn refresh_actions(
    w: &Workspace,
    run: &RunRecord,
    request: &RefreshRequest,
    p: &RefreshPlan,
    staging: &Path,
    result: &mut RefreshResult,
    context: &TaskContext,
) -> Result<()> {
    let mut selected = vec![];
    let mut diagnostics = vec![];
    let securities: Vec<_> = p
        .securities
        .iter()
        .filter(|s| s.instrument != request.template.benchmark)
        .cloned()
        .collect();
    let mut completed = BTreeSet::new();
    collect_shards(
        &securities,
        request.workers,
        context,
        |security| acquire_actions(security, request, p, staging, context),
        |security, shard| {
            if let Some(chosen) = shard.selected {
                selected.push(chosen);
            }
            diagnostics.extend(shard.diagnostics);
            let outcome = result
                .outcomes
                .entry(security.instrument.clone())
                .or_insert_with(|| json!({}));
            outcome["action_attempts"] = json!(shard.action_attempts);
            outcome["execution_attempts"] = json!(shard.execution_attempts);
            completed.insert(security.instrument.clone());
            atomic_json(
                &w.resolve(&run.output).join("action-progress.json"),
                &json!({"completed":completed,"complete_securities":selected.len(),"requested":securities.len()}),
            )?;
            Ok(())
        },
    )?;
    atomic_json(
        &w.resolve(&run.output).join("source-diagnostics.json"),
        &diagnostics,
    )?;
    let expected = p
        .securities
        .iter()
        .filter(|s| s.instrument != request.template.benchmark)
        .count();
    if selected.len() != expected {
        result.account = json!({"status":"failed","code":"action_history_incomplete","complete_securities":selected.len(),"requested":expected,"account_path":"not_run"});
        return Ok(());
    }
    let dest = staging.join("published-actions");
    fs::create_dir_all(dest.join("raw"))?;
    let (mut book, _) = selected.first().cloned().ok_or("empty_action_scope")?;
    book.actions.clear();
    book.queried_symbols.clear();
    book.raw_sha256.clear();
    book.structured_coverage.clear();
    book.execution_issues.clear();
    book.execution_data = None;
    for (b, root) in selected {
        for (name, hash) in &b.raw_sha256 {
            let path = dest.join("raw").join(name);
            if path.exists()
                && format!(
                    "sha256:{:x}",
                    analytics_core::hash::Hex(Sha256::digest(fs::read(&path)?))
                ) != *hash
            {
                // Per-collector completion receipts share a name; the action bundle's
                // structured shards already reference and authenticate native/HTTP bodies.
                if name == "complete-sources-structured.json" {
                    continue;
                }
                return Err("action_evidence_name_conflict".into());
            }
            fs::copy(root.join("raw").join(name), path)?;
            book.raw_sha256.insert(name.clone(), hash.clone());
        }
        book.actions.extend(b.actions);
        book.queried_symbols.extend(b.queried_symbols);
        book.structured_coverage.extend(b.structured_coverage);
        book.execution_issues.extend(b.execution_issues);
        book.source_bindings.extend(b.source_bindings);
        if let Some(aux) = b.execution_data {
            let all = book.execution_data.get_or_insert_with(|| {
                analytics_storage::execution_data::ExecutionData {
                    schema_version: 1,
                    ..Default::default()
                }
            });
            all.schema_version = all.schema_version.max(aux.schema_version);
            all.suspensions.extend(aux.suspensions);
            all.limits.extend(aux.limits);
            all.coefficients.extend(aux.coefficients);
            all.capabilities.extend(aux.capabilities);
        }
    }
    let all_evidence = dest.join("source-evidence");
    fs::create_dir_all(&all_evidence)?;
    for s in &p.securities {
        for provider in [Provider::Hithink, Provider::Tdxaidata] {
            let root = staging.join(format!("actions-{}-{provider:?}", s.instrument));
            if root.is_dir() {
                copy_tree(
                    &root,
                    &all_evidence.join(format!("{}-{provider:?}", s.instrument)),
                )?;
            }
        }
    }
    atomic_json(&dest.join("source-diagnostics.json"), &diagnostics)?;
    let proof = dest.join("raw/source-evidence-sha256.json");
    atomic_json(&proof, &market_content_hash(&all_evidence)?)?;
    book.raw_sha256.insert(
        "source-evidence-sha256.json".into(),
        format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(fs::read(&proof)?))
        ),
    );
    analytics_storage::corporate_actions::facts::refresh(&mut book, &dest)?;
    atomic_json(&dest.join("actions.json"), &book)?;
    ActionBundle::read(&dest.join("actions.json"))?;
    let published = w.resolve(format!("data/refresh-actions-{}", run.id));
    fs::rename(&dest, &published)?;
    result.actions = Some(w.register(&published.join("actions.json"))?);
    result.account = json!({"status":"not_run","code":"account_path_not_run","action_securities":book.queried_symbols.len(),"missing_cash_dates":book.actions.iter().filter(|a|a.cash()>0.&&a.pay_date.is_none()).count(),"missing_listing_dates":book.actions.iter().filter(|a|a.shares()>0.&&a.listing_date.is_none()).count(),"execution_issues":book.execution_issues});
    Ok(())
}

// Compare independent windows at a shared anchor. Absolute source coefficients
// are intentionally not compared: each valid request may use a different anchor.
