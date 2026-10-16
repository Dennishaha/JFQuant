//! Usecases.
use crate::research::backtest::ResearchRequest;
use crate::{
    DataVersion, ResearchTemplate, Result, RunRecord, WorkflowOutcome, WorkflowRequest, Workspace,
    atomic_json, market_content_hash, signature, verify_action_evidence,
};
use analytics_core::task::TaskContext;
use analytics_storage::{corporate_actions::ActionBundle, daily::DailyDataset};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub(crate) fn reconciliation_evidence_root(
    w: &Workspace,
    task: Uuid,
    record: &RunRecord,
) -> Result<PathBuf> {
    if record.id != task || record.finished_at.is_none() {
        return Err("evidence task must be terminal".into());
    }
    let request = &record.request["request"];
    let resume = request["resume"].as_str().unwrap_or("");
    let staging_id = if resume.is_empty() {
        task.to_string()
    } else {
        resume.to_owned()
    };
    match request["operation"].as_str() {
        Some("collect_actions") => {
            let published = w.resolve(format!("data/python-actions-{task}"));
            Ok(if published.is_dir() {
                published
            } else {
                w.resolve(format!(".jfquant/staging/python-actions-{staging_id}"))
            })
        }
        Some("collect_announcements") => {
            let published = w.resolve(&record.output).join("announcements");
            Ok(if published.is_dir() {
                published
            } else {
                w.resolve(format!(".jfquant/staging/announcements-{staging_id}"))
            })
        }
        _ => Err(
            "evidence task must be a supported company-action or announcement collection".into(),
        ),
    }
}
pub(crate) fn version_metadata(workspace: &Workspace, id: &str, kind: &str) -> Result<DataVersion> {
    let data = workspace
        .datasets()?
        .into_iter()
        .find(|d| d.id == id && d.kind == kind)
        .ok_or("unknown input version")?;
    let path = workspace.resolve(&data.path);
    let content = if kind == "market" {
        let mut source = serde_json::from_slice::<Value>(&fs::read(
            path.parent()
                .ok_or("market parent missing")?
                .join("jfquant_source.json"),
        )?)?;
        let expected = data.details["content_sha256"]
            .as_str()
            .ok_or("market version has no content hash; register it again before research")?;
        source["content_sha256"] = json!(expected);
        source["source_map"] = data
            .details
            .get("source_map")
            .cloned()
            .ok_or("market source map missing; register it again")?;
        source
    } else if kind == "coefficients" {
        serde_json::to_value(analytics_storage::collector::CoefficientBundle::read(
            &path,
        )?)?
    } else {
        ActionBundle::version_content(&path)?
    };
    if signature(&content)? != data.signature {
        return Err("registered data version changed; register it as a new version".into());
    }
    Ok(data)
}
// Large byte verification is worker-side, keeping submit/control RPCs responsive.
pub(crate) fn version(workspace: &Workspace, id: &str, kind: &str) -> Result<DataVersion> {
    let data = version_metadata(workspace, id, kind)?;
    let path = workspace.resolve(&data.path);
    if kind == "market" {
        if let Some(hashes) = data.details["evidence_sha256"].as_object() {
            for (name, expected) in hashes {
                if Path::new(name).file_name().and_then(|n| n.to_str()) != Some(name) {
                    return Err("unsafe market evidence name".into());
                }
                let raw = path
                    .parent()
                    .ok_or("missing parent")?
                    .join("raw")
                    .join(name);
                let actual = format!(
                    "sha256:{:x}",
                    analytics_core::hash::Hex(Sha256::digest(fs::read(raw)?))
                );
                if expected.as_str() != Some(actual.as_str()) {
                    return Err("market evidence changed".into());
                }
            }
        }
        if let Some(expected) = data.details["evidence_tree_sha256"].as_str()
            && expected
                != market_content_hash(
                    &path
                        .parent()
                        .ok_or("market parent missing")?
                        .join("evidence"),
                )?
        {
            return Err("market acquisition evidence changed".into());
        }
        if data.details["content_sha256"] != market_content_hash(&path)? {
            return Err("registered market bytes changed; register it as a new version".into());
        }
        if path.join("market_view.json").is_file() {
            DailyDataset::open(&path)?;
        }
    } else if kind == "coefficients" {
        analytics_storage::collector::CoefficientBundle::read(&path)?;
    } else {
        verify_action_evidence(&path, &ActionBundle::read(&path)?)?;
    }
    Ok(data)
}
/// Execute an operation in the caller's scoped task context; never starts a native UI thread.
pub fn execute(
    workspace: &Workspace,
    request: &WorkflowRequest,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<WorkflowOutcome> {
    context.execute(|| {
        let outcome = execute_scoped(workspace, request, run, context)?;
        atomic_json(
            &workspace.resolve(&run.output).join("outcome.json"),
            &outcome,
        )?;
        Ok(outcome)
    })
}
fn execute_scoped(
    w: &Workspace,
    request: &WorkflowRequest,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<WorkflowOutcome> {
    context.check()?;
    match request {
        WorkflowRequest::CollectAnnouncements { .. }
        | WorkflowRequest::CollectReference { .. }
        | WorkflowRequest::SourceStatus { .. }
        | WorkflowRequest::SaveSourcePolicy { .. }
        | WorkflowRequest::CheckSource { .. }
        | WorkflowRequest::RefreshData { .. }
        | WorkflowRequest::CheckUpdates { .. }
        | WorkflowRequest::DownloadMarket { .. }
        | WorkflowRequest::CollectCoefficients { .. }
        | WorkflowRequest::ComposeMarket { .. }
        | WorkflowRequest::CollectActions { .. }
        | WorkflowRequest::CollectQuotes { .. }
        | WorkflowRequest::CollectMarket { .. }
        | WorkflowRequest::DownloadActions { .. } => acquisition::execute(w, request, run, context),
        WorkflowRequest::Evidence { request } => {
            crate::action_evidence::execute(w, request, run, context)
        }
        WorkflowRequest::InspectActions { .. }
        | WorkflowRequest::ReconcileActions { .. }
        | WorkflowRequest::ReviseActions { .. } => actions::execute(w, request, run, context),
        WorkflowRequest::Independent { .. }
        | WorkflowRequest::PreviewResearch { .. }
        | WorkflowRequest::PrepareResearchData { .. }
        | WorkflowRequest::TrainResearch { .. }
        | WorkflowRequest::BacktestPredictions { .. }
        | WorkflowRequest::BacktestResearch { .. }
        | WorkflowRequest::Research { .. } => research_jobs::execute(w, request, run, context),
        WorkflowRequest::SavePool { .. }
        | WorkflowRequest::PreviewPool { .. }
        | WorkflowRequest::CreateCanonicalDemo { .. }
        | WorkflowRequest::CreateDemo { .. }
        | WorkflowRequest::RegisterData { .. } => configuration::execute(w, request, run, context),
    }
}
fn research_request(
    w: &Workspace,
    run: &RunRecord,
    template: &ResearchTemplate,
    market: &DataVersion,
    actions: Option<&DataVersion>,
    audit_only: bool,
) -> Result<ResearchRequest> {
    template.validate()?;
    let mut r = ResearchRequest::new(w.resolve(&market.path), w.resolve(&run.output));
    r.config = template.config.clone();
    r.price_basis = template.price_basis;
    r.factor_set = template.factor_set;
    r.quality_policy = template.quality_policy.clone();
    r.schedule = template.schedule.clone();
    r.action_policy = template.action_policy.clone();
    r.event_check_policy = template.event_check_policy;
    r.market_rules = template.market_rules.clone();
    r.initial_cash_cny = template.initial_cash_cny;
    r.dividend_withholding_rate = template.dividend_withholding_rate;
    r.corporate_actions = actions.map(|a| w.resolve(&a.path));
    r.audit_only = audit_only;
    r.benchmark = template.benchmark.clone();
    r.pool_exit_policy = template.pool_exit_policy;
    let (pool, frozen) = w.resolve_pool(&template.pool_version, &market.id)?;
    r.pool_signature = signature(&frozen)?;
    r.pool = Some(pool);
    r.universe_id = template.pool_version.clone();
    atomic_json(&w.resolve(&run.output).join("pool.json"), &frozen)?;
    Ok(r)
}
pub(crate) fn research(
    w: &Workspace,
    run: &RunRecord,
    context: &TaskContext,
    template: &ResearchTemplate,
    market: &DataVersion,
    actions: Option<&DataVersion>,
    audit_only: bool,
) -> Result<Value> {
    let r = research_request(w, run, template, market, actions, audit_only)?;
    context.report("检查训练输入；训练完成后按需回测")?;
    crate::research::backtest::run_with_context(&r, context)?;
    w.results(run)
}

/// Shared pre-run schedule and security requirements; does not allocate native model handles.
pub fn research_requirements(w: &Workspace, t: &ResearchTemplate, market: &str) -> Result<Value> {
    research_requirements_with_actions(w, t, market, None)
}
/// Include reviewed transitive successors while retaining an explicit not-run when no book is selected.
pub fn research_requirements_with_actions(
    w: &Workspace,
    t: &ResearchTemplate,
    market: &str,
    actions: Option<&str>,
) -> Result<Value> {
    t.validate()?;
    let selected_market = version(w, market, "market")?;
    let mut dataset =
        analytics_storage::daily::DailyDataset::open(w.resolve(selected_market.path))?;
    let folds = t
        .schedule
        .resolve_with_warmup(&dataset.calendar, t.warmup_sessions())?;
    let (pool, frozen) = w.resolve_pool(&t.pool_version, market)?;
    let lo = folds
        .iter()
        .map(|f| f.warmup_start)
        .min()
        .ok_or("empty schedule")?;
    let hi = folds.last().unwrap().actual.test.end;
    dataset.restrict_reads(analytics_core::DateRange { start: lo, end: hi })?;
    let benchmark = t.benchmark.clone();
    let mut symbols: std::collections::BTreeSet<_> = pool
        .intervals
        .iter()
        .filter(|(_, rs)| rs.iter().any(|r| r.start <= hi && r.end >= lo))
        .map(|(id, _)| id.clone())
        .collect();
    if !dataset.is_price_index(&benchmark).unwrap_or(false) {
        symbols.insert(benchmark.clone());
    }
    let successor_check = match actions {
        Some(id) => match version(w, id, "actions")
            .and_then(|v| Ok(ActionBundle::read(&w.resolve(v.path))?))
        {
            Ok(book) => {
                let seeds = pool
                    .intervals
                    .iter()
                    .filter(|(_, rs)| {
                        rs.iter()
                            .any(|r| r.end >= folds[0].actual.test.start && r.start <= hi)
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                symbols.extend(book.required_symbols(&seeds, folds[0].actual.test.start, hi));
                json!({"status":"passed","action_version":id})
            }
            Err(e) => json!({"status":"failed","action_version":id,"message":e.to_string()}),
        },
        None => {
            json!({"status":"not_run","message":"select an action version to enumerate successors"})
        }
    };
    let mut dependencies = std::collections::BTreeMap::new();
    for id in &symbols {
        let history = analytics_core::DateRange { start: lo, end: hi };
        let member = pool.intervals.get(id).cloned().unwrap_or_default();
        dependencies.insert(id,json!({"instrument":id,"required_history":history,"membership":member,"purpose":if *id==benchmark{"benchmark"}else{"features_and_account"}}));
    }
    let mut preview_pool = pool.clone();
    preview_pool.intervals.retain(|_, rs| {
        rs.retain(|r| r.end >= lo && r.start <= hi);
        !rs.is_empty()
    });
    let registry = t.factor_registry()?;
    let ids = if t.config.factor_ids.is_empty() {
        registry
            .all()
            .filter(|s| {
                !matches!(
                    s.expression,
                    analytics_factor::FactorExpr::Unsupported { .. }
                ) && s.input_columns.iter().all(|c| {
                    crate::research::backtest::SUPPORTED_RESEARCH_INPUTS.contains(&c.as_str())
                })
            })
            .map(|s| s.id.clone())
            .collect()
    } else {
        t.config.factor_ids.clone()
    };
    let check_plan = crate::input_check::InputCheckPlan::new(
        t.factor_set,
        &t.config.factor_refs,
        &ids,
        &registry,
        "preparation",
        json!({"benchmark":benchmark,"seed":"research-annual-756-v1"}),
        crate::input_check::daily::phases(&folds, false, false),
    )?;
    dataset.restrict_input_checks(crate::input_check::daily::requirements(
        &check_plan,
        &dataset.calendar,
        &preview_pool,
        &benchmark,
    ));
    let quality = crate::research::backtest::quality::qualify_scoped(
        &dataset,
        &preview_pool,
        &t.quality_policy,
        &analytics_core::DateRange { start: lo, end: hi },
        &ids,
        &registry,
        Some(&check_plan),
    )?;
    let sample_readiness = crate::research::backtest::preview_samples_with_policy(
        &dataset,
        &quality.effective_pool,
        &folds,
        &t.quality_policy,
    )?;
    Ok(
        json!({"check_plan":check_plan,"complete_check":false,"quality":quality,"dependencies":dependencies,"successor_check":successor_check,"resolved_folds":folds,"sample_readiness":sample_readiness,"securities":symbols,"history_start":lo,"execution_end":hi,"pool":frozen,"schema_version":3,"source_map":dataset.source_map(),"benchmark":benchmark,"unknown_cross_section_before":pool.coverage.start}),
    )
}
use crate::{demo, preparation, refresh};

use crate::research as research_api;

mod catalog;
mod results;

mod acquisition;

mod actions;

mod research_jobs;

mod configuration;
