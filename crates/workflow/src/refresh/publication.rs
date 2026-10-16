//! Refresh publication stage.
use super::*;
pub(super) fn copy_tree(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for e in fs::read_dir(source)? {
        TaskContext::current().check()?;
        let e = e?;
        let to = dest.join(e.file_name());
        if e.file_type()?.is_symlink() {
            return Err("unsafe_acquisition_symlink".into());
        }
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else {
            fs::copy(e.path(), to)?;
        }
    }
    Ok(())
}
pub(super) fn reuse_shards(
    w: &Workspace,
    run: &RunRecord,
    request: &RefreshRequest,
    plan: &RefreshPlan,
    staging: &Path,
) -> Result<Vec<Value>> {
    let mut reused = vec![];
    for source_ref in &request.reuse_shards_from {
        let (source_workspace, source_id) = shard_workspace(w, source_ref)?;
        if source_id == run.id && source_workspace.root() == w.root() {
            return Err("shard_reuse_self_reference".into());
        }
        let report = crate::coverage::refresh_report(&source_workspace, source_id, false)?;
        if report["reason"] == "planning_pending" {
            return Err("shard_reuse_plan_pending".into());
        }
        let source_run = source_workspace.resolve(format!(".jfquant/runs/{source_id}"));
        let original: RunRecord = serde_json::from_slice(&fs::read(source_run.join("run.json"))?)?;
        let source: RefreshPlan =
            serde_json::from_slice(&fs::read(source_run.join("refresh-plan.json"))?)?;
        let original_request = &original.request["request"]["request"];
        if source.history != plan.history
            || (request.mode != RefreshMode::Actions
                && (source.base_market.id != plan.base_market.id
                    || original_request["mode"] != serde_json::to_value(request.mode)?))
            || original_request["template"]["benchmark"] != request.template.benchmark
        {
            return Err("shard_reuse_window_mismatch".into());
        }
        let owner = original_request["resume"]
            .as_str()
            .map(Uuid::parse_str)
            .transpose()?
            .unwrap_or(source_id);
        let source_root = source_workspace.resolve(format!(".jfquant/staging/refresh-{owner}"));
        let progress: Value = if source_run.join("refresh-progress.json").exists() {
            serde_json::from_slice(&fs::read(source_run.join("refresh-progress.json"))?)?
        } else {
            json!({"outcomes":{}})
        };
        for security in &plan.securities {
            analytics_core::pool::validate_instrument(&security.instrument)?;
            for (kind, role) in [("quotes", DataRole::Quotes), ("actions", DataRole::Actions)] {
                if request.mode == RefreshMode::Actions && kind == "quotes" {
                    continue;
                }
                for provider in plan.source_policy.routes(role, request.provider) {
                    let name = format!("{kind}-{}-{provider:?}", security.instrument);
                    let from = source_root.join(&name);
                    let to = staging.join(&name);
                    if to.exists() || !from.is_dir() {
                        continue;
                    }
                    if provider == Provider::Tdxaidata {
                        let owner: Value =
                            serde_json::from_slice(&fs::read(from.join("owner.json"))?)?;
                        if owner["collector_signature"] != collector::collector_signature() {
                            continue;
                        }
                    }
                    if provider == Provider::Hithink {
                        let owner: Value =
                            serde_json::from_slice(&fs::read(from.join("owner.json"))?)?;
                        if owner["collector_signature"]
                            != analytics_storage::hithink::producer_signature()
                        {
                            continue;
                        }
                    }
                    let completed = if kind == "quotes" {
                        progress["outcomes"][&security.instrument]["quotes"] == "downloaded"
                            && progress["outcomes"][&security.instrument]["attempts"]
                                .as_array()
                                .and_then(|a| a.last())
                                .is_some_and(|a| {
                                    a["provider"] == serde_json::to_value(provider).unwrap()
                                })
                    } else {
                        from.join("actions.json").is_file()
                            && (ActionBundle::read(&from.join("actions.json"))?
                                .failures
                                .is_empty()
                                || provider == Provider::Tdxaidata)
                    };
                    if !completed {
                        continue;
                    }
                    // Copy atomically. Acquisition below still verifies exact request,
                    // producer identity and every checkpoint/evidence hash before use.
                    let temporary = staging.join(format!(".reuse-{name}"));
                    if temporary.exists() {
                        fs::remove_dir_all(&temporary)?
                    }
                    copy_tree(&from, &temporary)?;
                    fs::rename(&temporary, &to)?;
                    reused.push(json!({"source_task":source_id,"instrument":security.instrument,"kind":kind,"provider":provider,"checkpoint_validation":"required_by_collector_before_use"}));
                }
            }
        }
    }
    Ok(reused)
}

/// Publish typed candidates only after every requested shard has a complete source reply.
pub fn execute(
    w: &Workspace,
    run: &RunRecord,
    request: &RefreshRequest,
    policy: &SourcePolicy,
    context: &TaskContext,
) -> Result<Value> {
    let p = plan(w, request, policy)?;
    atomic_json(&w.resolve(&run.output).join("refresh-plan.json"), &p)?;
    if request.preview {
        return Ok(
            json!({"plan":p,"training":{"status":"not_run","reason":"preview_only"},"account":{"status":"not_run","reason":"account_path_not_run"}}),
        );
    }
    let owner = request.resume.unwrap_or(run.id);
    let staging = w.resolve(format!(".jfquant/staging/refresh-{owner}"));
    fs::create_dir_all(&staging)?;
    let identity = json!({"schema_version":1,"plan_signature":signature(&p)?,"mode":request.mode,"collector_signature":collector::collector_signature()});
    let owner_path = staging.join("refresh-owner.json");
    if owner_path.exists() && serde_json::from_slice::<Value>(&fs::read(&owner_path)?)? != identity
    {
        return Err("refresh_checkpoint_owner_mismatch".into());
    }
    atomic_json(&owner_path, &identity)?;
    if request.mode == RefreshMode::Actions {
        let reused = reuse_shards(w, run, request, &p, &staging)?;
        atomic_json(&w.resolve(&run.output).join("reused-shards.json"), &reused)?;
        let mut result = RefreshResult {
            schema_version: 1,
            market: Some(p.base_market.clone()),
            actions: None,
            training: json!({"status":"not_run","reason":"market_unchanged"}),
            account: json!({"status":"not_run","reason":"account_path_not_run"}),
            outcomes: BTreeMap::new(),
            failures: BTreeMap::new(),
        };
        refresh_actions(w, run, request, &p, &staging, &mut result, context)?;
        atomic_json(&w.resolve(&run.output).join("refresh-result.json"), &result)?;
        return Ok(serde_json::to_value(result)?);
    }
    let base = DailyDataset::open(w.resolve(&p.base_market.path))?;
    let output = staging.join("published");
    fs::create_dir_all(output.join("market/bars"))?;
    fs::create_dir_all(output.join("market/adjustments"))?;
    let mut outcomes = BTreeMap::new();
    let mut failures = BTreeMap::new();
    let (calendar_reference, calendar_attempts) =
        reference_route(&p, request, &staging, context, DataRole::Calendar)?;
    let (member_reference, member_attempts) =
        reference_route(&p, request, &staging, context, DataRole::Members)?;
    let mut calendar: Vec<Date> = if let Some(value) = &calendar_reference {
        serde_json::from_value(value["data"]["calendar"].clone())?
    } else {
        base.calendar
            .iter()
            .copied()
            .filter(|d| p.history.contains(*d))
            .collect()
    };
    outcomes.insert("calendar".into(),json!({"attempts":calendar_attempts,"status":if calendar_reference.is_some(){"passed"}else{"failed"}}));
    outcomes.insert("members".into(),json!({"attempts":member_attempts,"historical_source":"selected_base_version","new_members":"current_observation_only"}));
    if calendar_reference.is_none() {
        failures.insert("calendar".into(), "all_calendar_sources_failed".into());
    }
    let mut anchors = BTreeMap::new();
    let mut sources = BTreeMap::new();
    let reused = reuse_shards(w, run, request, &p, &staging)?;
    atomic_json(&w.resolve(&run.output).join("reused-shards.json"), &reused)?;
    let source_calendar = calendar.clone();
    let base_path = w.resolve(&p.base_market.path);
    collect_shards(
        &p.securities,
        request.workers,
        context,
        |security| {
            acquire_quotes(
                security,
                request,
                &p,
                &base_path,
                &staging,
                (&source_calendar, calendar_reference.is_some()),
                context,
            )
        },
        |security, shard| {
            let id = &security.instrument;
            let QuoteShard {
                attempts,
                selected,
                calendar: extra_calendar,
            } = shard;
            calendar.extend(extra_calendar);
            if let Some((provider, (rows, adjustment, root))) = selected {
                let first = rows
                    .iter()
                    .find(|r| r.close.is_some())
                    .ok_or("refresh_anchor_missing")?;
                let baseline = raw_rows(&base, id, &p.history)?
                    .into_iter()
                    .map(|r| (r.trade_date, r))
                    .collect::<BTreeMap<_, _>>();
                let revisions: Vec<Value> = rows
                    .iter()
                    .filter_map(|row| {
                        baseline
                            .get(&row.trade_date)
                            .filter(|old| {
                                serde_json::to_value(old).ok() != serde_json::to_value(row).ok()
                            })
                            .map(|old| json!({"date":row.trade_date,"before":old,"after":row}))
                    })
                    .collect();
                let previous_adjustment =
                    AdjustmentRecord::read(&w.resolve(&p.base_market.path), id).ok();
                let revision_report = json!({"instrument":id,"base_market":p.base_market.id,"source_changed":native_provider(&p.base_market,id)!=Some(provider),"quote_revisions":revisions,"adjustment_before":previous_adjustment,"adjustment_after":adjustment});
                atomic_json(
                    &w.resolve(&run.output).join(format!("revisions-{id}.json")),
                    &revision_report,
                )?;
                anchors.insert(id.clone(), adjustment.anchor(first.trade_date)?);
                write_bars(
                    &output.join("market/bars").join(format!("{id}.parquet")),
                    &rows,
                )?;
                atomic_json(
                    &output.join("market/adjustments").join(format!("{id}.json")),
                    &adjustment,
                )?;
                copy_tree(&root, &output.join("evidence").join(id))?;
                sources.insert(id.clone(), json!({"provider":provider,"attempts":attempts}));
                outcomes.insert(
                    id.clone(),
                    json!({"quotes":"downloaded","rows":rows.len(),"attempts":attempts}),
                );
            } else {
                failures.insert(
                    id.clone(),
                    "all_quotation_sources_failed; see attempts".into(),
                );
                outcomes.insert(id.clone(), json!({"quotes":"failed","attempts":attempts}));
            }
            atomic_json(
                &w.resolve(&run.output).join("refresh-progress.json"),
                &json!({"outcomes":outcomes,"failures":failures}),
            )?;
            Ok(())
        },
    )?;
    calendar.sort();
    calendar.dedup();
    calendar.retain(|d| p.history.contains(*d));
    let mut result = RefreshResult {
        schema_version: 1,
        market: None,
        actions: p.base_actions.clone(),
        training: json!({"status":"not_run","reason":"research_check_required"}),
        account: json!({"status":"not_run","reason":"account_path_not_run"}),
        outcomes,
        failures,
    };
    if result.failures.is_empty() {
        let mut universes = BTreeMap::new();
        for name in ["csi300", "csi500"] {
            if let Ok(pool) = base.pool(name) {
                universes.insert(name.to_owned(), pool);
            }
        }
        if let Some(value) = &member_reference {
            let current: BTreeMap<String, analytics_core::pool::PoolMembership> =
                serde_json::from_value(value["data"]["universes"].clone())?;
            for (name, pool) in current {
                universes.insert(format!("{name}-current"), pool);
            }
        }
        let manifest = DailyManifest {
            schema_version: 6,
            calendar,
            universes,
            instruments: p.securities.iter().map(|s| s.instrument.clone()).collect(),
            price_basis: "unadjusted".into(),
            normalization: if sources.values().any(|s| s["provider"] == "hithink")
                && sources.values().any(|s| s["provider"] == "tdxaidata")
            {
                analytics_storage::adjustment::MIXED_NORMALIZATION
            } else if sources.values().any(|s| s["provider"] == "hithink") {
                analytics_storage::adjustment::HITHINK_NORMALIZATION
            } else {
                analytics_storage::adjustment::NORMALIZATION
            }
            .into(),
            adjustment_anchors: anchors,
        };
        atomic_json(&output.join("market/daily_manifest.json"), &manifest)?;
        atomic_json(&output.join("market/source_map.json"), base.source_map())?;
        for name in ["calendar", "members"] {
            let root = staging.join(format!("reference-{name}"));
            if root.exists() {
                copy_tree(&root, &output.join("evidence").join(name))?;
            }
        }
        atomic_json(
            &output.join("market/acquisition.json"),
            &json!({"schema_version":1,"plan":p,"security_sources":sources,"provider_training_verified":sources.values().all(|s|matches!(s["provider"].as_str(),Some("tdxaidata"|"hithink")))}),
        )?;
        atomic_json(
            &output.join("jfquant_source.json"),
            &json!({"downloader":"JFQuant analytics-storage","independent_download":true,"snapshot_verified":true,"security_sources":sources,"plan":p,"evidence_tree_sha256":market_content_hash(&output.join("evidence"))?}),
        )?;
        DailyDataset::open(output.join("market"))?;
        let dest = w.resolve(format!("data/refresh-{}", run.id));
        fs::rename(&output, &dest)?;
        result.market = Some(w.register(&dest.join("market"))?);
    }
    // Action/execution routes are independent: quote failures never discard valid events.
    refresh_actions(w, run, request, &p, &staging, &mut result, context)?;
    atomic_json(&w.resolve(&run.output).join("refresh-result.json"), &result)?;
    Ok(serde_json::to_value(result)?)
}
