//! Frozen source strategies, shared allocation and a single native execution account.
use super::*;
use crate::{RunStatus, Workspace};
use analytics_core::{DateRange, pool::PoolMembership, task::TaskContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

/// Explicit combined-account recipe. Source selection itself is a research choice.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CombinedPortfolioPlan {
    /// Current plan format only.
    pub schema_version: u32,
    /// Two to twenty completed, non-combined source tasks.
    pub sources: Vec<Uuid>,
    /// Formal account interval; sources must also cover estimation history.
    pub range: DateRange,
    /// Account and capital allocation settings.
    pub execution: ResearchExecution,
}
impl CombinedPortfolioPlan {
    /// Validate structural constraints without querying mutable sources.
    pub fn validate(&self) -> Result<()> {
        self.range.validate()?;
        self.execution.allocation.validate()?;
        self.execution.market_rules.validate()?;
        if self.schema_version != 2
            || !(2..=20).contains(&self.sources.len())
            || self.sources.iter().collect::<BTreeSet<_>>().len() != self.sources.len()
            || !(1..=252).contains(&self.execution.rebalance_days)
            || self.sources.len() as f64 * self.execution.allocation.max_strategy_weight < 1. - 1e-6
            || !self.execution.initial_cash_cny.is_finite()
            || self.execution.initial_cash_cny <= 0.
        {
            return Err("combined_plan_invalid_or_infeasible".into());
        }
        Ok(())
    }
}
/// Shared asynchronous App/CLI entry points.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Job {
    /// Verify sources, temporal coverage and structural constraints without execution.
    Check {
        /// Frozen plan.
        plan: CombinedPortfolioPlan,
    },
    /// Optimize and execute one combined account.
    Run {
        /// Frozen plan.
        plan: CombinedPortfolioPlan,
    },
}
impl Job {
    /// Selected immutable recipe.
    pub fn plan(&self) -> &CombinedPortfolioPlan {
        match self {
            Self::Check { plan } | Self::Run { plan } => plan,
        }
    }
}
const SOURCE_FILES: &[&str] = &[
    "run.json",
    "summary.json",
    "research-request.json",
    "allocation_report.json",
    "reference-account.json",
    "reference-input.json",
    "predictions.parquet",
    "prediction_manifest.json",
    "allocation-manifest.json",
];
/// Freeze exact source bytes. No source is inferred from current library configuration.
pub fn snapshot(w: &Workspace, job: &Job) -> Result<Value> {
    job.plan().validate()?;
    let mut sources = Vec::new();
    for id in &job.plan().sources {
        crate::storage_management::require_available(w, *id, "predictions")?;
        let root = w.resolve(format!(".jfquant/runs/{id}"));
        let run: crate::RunRecord = serde_json::from_slice(
            &analytics_storage::evidence_store::read(root.join("run.json"))?,
        )?;
        if run.id != *id
            || run.status != RunStatus::Succeeded
            || run.read_only
            || root.join("combined-input.json").exists()
            || fs::symlink_metadata(&root)?.file_type().is_symlink()
        {
            return Err("combined_source_requires_current_native_stock_task".into());
        }
        let summary: Value = serde_json::from_slice(&analytics_storage::evidence_store::read(
            root.join("summary.json"),
        )?)?;
        validate_result(&summary)?;
        if summary["kind"] == "combined" || summary["execution"].get("combined_sources").is_some() {
            return Err("combined_source_recursion_forbidden".into());
        }
        let hashes = SOURCE_FILES
            .iter()
            .map(|n| Ok((n.to_string(), file_hash(&root.join(n))?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        sources.push(json!({"id":id,"files":hashes}));
    }
    Ok(
        json!({"schema_version":1,"plan":job.plan(),"sources":sources,"selection_warning":"source strategies selected retrospectively; no claim of selection-level out-of-sample evidence"}),
    )
}
/// Execute with revalidated frozen sources and existing raw-price/event account checks.
pub fn execute(
    w: &Workspace,
    run: &crate::RunRecord,
    job: &Job,
    context: &TaskContext,
) -> Result<Value> {
    let frozen = snapshot(w, job)?;
    if frozen != run.request["combined"] {
        return Err("combined_sources_changed_after_submission".into());
    }
    let plan = job.plan();
    let output = w.resolve(&run.output);
    fs::create_dir_all(&output)?;
    let mut sources = Vec::new();
    let mut base = None;
    let mut contract = None;
    let mut pool = PoolMembership {
        coverage: plan.range.clone(),
        intervals: BTreeMap::new(),
    };
    for id in &plan.sources {
        context.check()?;
        let root = w.resolve(format!(".jfquant/runs/{id}"));
        crate::allocation::verify_artifacts(&root)?;
        let r: ResearchRequest = serde_json::from_slice(&analytics_storage::evidence_store::read(
            root.join("research-request.json"),
        )?)?;
        let summary: Value = serde_json::from_slice(&analytics_storage::evidence_store::read(
            root.join("summary.json"),
        )?)?;
        let identity = json!({"market":summary["execution"]["market_content_sha256"],"events":summary["execution"]["corporate_action_sha256"],"policy":r.corporate_event_policy.unwrap_or_default(),"rights":r.rights_policy,"action_policy":r.action_policy,"event_check_policy":r.event_check_policy});
        if contract.as_ref().is_some_and(|c| c != &identity) {
            return Err("combined_sources_market_calendar_or_event_contract_mismatch".into());
        }
        contract = Some(identity);
        let reference: Value = serde_json::from_slice(&analytics_storage::evidence_store::read(
            root.join("reference-account.json"),
        )?)?;
        let rows = reference["rows"]
            .as_array()
            .ok_or("combined_reference_rows_missing")?;
        let first: Date = rows
            .first()
            .and_then(|r| r["date"].as_str())
            .ok_or("combined_reference_empty")?
            .parse()?;
        let last: Date = rows
            .last()
            .and_then(|r| r["date"].as_str())
            .ok_or("combined_reference_empty")?
            .parse()?;
        if first > plan.range.start || last < plan.range.end {
            return Err("combined_source_does_not_cover_formal_range".into());
        }
        let allocation: Value = serde_json::from_slice(&analytics_storage::evidence_store::read(
            root.join("allocation_report.json"),
        )?)?;
        for t in allocation["targets"]
            .as_array()
            .ok_or("combined_targets_missing")?
        {
            for s in t["weights"]
                .as_object()
                .ok_or("combined_weights_missing")?
                .keys()
            {
                pool.intervals.insert(s.clone(), vec![plan.range.clone()]);
            }
        }
        for row in rows {
            for s in row["weights"]
                .as_object()
                .ok_or("combined_reference_weights_missing")?
                .keys()
            {
                pool.intervals.insert(s.clone(), vec![plan.range.clone()]);
            }
        }
        sources.push(
            json!({"id":id.to_string(),"reference":reference,"targets":allocation["targets"]}),
        );
        if base.is_none() {
            base = Some(r);
        }
    }
    let mut request = base.ok_or("combined_sources_empty")?;
    request.output = output.clone();
    request.config.allocation = plan.execution.allocation.clone();
    request.config.rebalance_days = plan.execution.rebalance_days;
    request.market_rules = plan.execution.market_rules.clone();
    request.initial_cash_cny = plan.execution.initial_cash_cny;
    request.pool = Some(pool.clone());
    request.pool_signature = crate::signature(&pool)?;
    request.pool_exit_policy = plan.execution.pool_exit_policy;
    request.corporate_event_policy = Some(plan.execution.corporate_event_policy);
    if contract.as_ref().unwrap()["policy"]
        != serde_json::to_value(request.corporate_event_policy.unwrap_or_default())?
    {
        return Err("combined_execution_policy_differs_from_sources".into());
    }
    let dataset = DailyDataset::open(&request.data_root)?;
    let dates = dataset
        .calendar
        .iter()
        .filter(|d| plan.range.contains(**d))
        .copied()
        .collect::<Vec<_>>();
    if dates.first() != Some(&plan.range.start) || dates.last() != Some(&plan.range.end) {
        return Err("combined_range_must_use_market_trading_dates".into());
    }
    let needed = if request.config.allocation.method == crate::allocation::AllocationMethod::Equal {
        0
    } else {
        request.config.allocation.risk_window + request.config.rebalance_days
    };
    let start = dataset.calendar.partition_point(|d| *d < plan.range.start);
    if start < needed {
        return Err("combined_risk_warmup_insufficient".into());
    }
    for source in &sources {
        let rows = source["reference"]["rows"].as_array().unwrap();
        let available = rows
            .iter()
            .map(|r| r["date"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        for d in &dataset.calendar[start - needed..=start] {
            if !available.contains(d.to_string().as_str()) {
                return Err(format!("combined_reference_warmup_gap:{d}").into());
            }
        }
    }
    let book = analytics_storage::corporate_actions::ActionBundle::read(
        request
            .corporate_actions
            .as_ref()
            .ok_or("combined_events_missing")?,
    )?;
    let checks = execution::checks(&request, &dataset, Some(&book), &pool, &plan.range)?;
    save(output.join("execution_checks.json"), &checks)?;
    if checks
        .iter()
        .any(|c| c.status == analytics_core::admission::CheckStatus::Failed)
    {
        return Err("combined_execution_inputs_not_ready".into());
    }
    save(
        output.join("combined-input.json"),
        &json!({"schema_version":1,"plan":plan,"sources":sources}),
    )?;
    let check = json!({"schema_version":1,"ready":true,"range":plan.range,"source_receipts":frozen,"reference_history_observations":needed,"prediction_warmup":"frozen source tasks","selection_warning":"retrospective strategy selection"});
    save(output.join("input-check.json"), &check)?;
    if matches!(job, Job::Check { .. }) {
        return Ok(check);
    }
    let mut execution = json!({"mode":"akquant_ex_date","accounting_version":"rust-filled-trade-ledger-v1","market_content_sha256":crate::market_content_hash(&request.data_root)?,"corporate_action_sha256":file_hash(request.corporate_actions.as_ref().unwrap())?,"execution_checks_sha256":file_hash(&output.join("execution_checks.json"))?,"execution_dependencies_sha256":file_hash(&output.join("execution-dependencies.json"))?,"event_check_policy":request.event_check_policy,"initial_cash_cny":request.initial_cash_cny,"market_rules":request.market_rules,"research_config":request.config,"combined_sources":frozen,"corporate_event_policy":request.corporate_event_policy,"runtime_signature":crate::research::worker::runtime_signature(),"dependency_signature":crate::research::worker::dependency_signature()});
    if output.join("rights_input.json").exists() {
        let l: analytics_core::rights_account::Ledger = serde_json::from_slice(
            &analytics_storage::evidence_store::read(output.join("rights_input.json"))?,
        )?;
        execution["accounting_version"] = "rust-filled-trade-ledger-events-v3".into();
        execution["rights_input_sha256"] = file_hash(&output.join("rights_input.json"))?.into();
        execution["rights_policy"] = serde_json::to_value(l.policy)?;
    }
    save(output.join("execution.json"), &execution)?;
    let signals = dates
        .iter()
        .map(|d| (*d, request.benchmark.clone(), 0.))
        .collect::<Vec<_>>();
    akquant::table(
        &output.join("predictions.parquet"),
        &["date", "symbol", "score"],
        vec![
            std::sync::Arc::new(arrow_array::StringArray::from(
                signals.iter().map(|s| s.0.to_string()).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(arrow_array::StringArray::from(
                signals.iter().map(|s| s.1.clone()).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(arrow_array::Float64Array::from(
                signals.iter().map(|s| s.2).collect::<Vec<_>>(),
            )),
        ],
    )?;
    save(
        output.join("prediction_manifest.json"),
        &json!({"schema_version":1,"kind":"combined_clock_only","rows":signals.len(),"sha256":file_hash(&output.join("predictions.parquet"))?}),
    )?;
    let days = context.execute(|| {
        crate::research::worker::scoped(|| {
            akquant::simulate(
                &request,
                &dataset,
                Some(&book),
                &signals,
                &plan.range,
                &pool,
                &dates,
            )
        })
    })?;
    crate::allocation::publish_artifacts(&output)?;
    execution["allocation_components"] = crate::allocation::component_signatures();
    execution["allocation_display"] = crate::allocation::display(&output)?;
    execution["allocation_artifacts"] = serde_json::from_slice(
        &analytics_storage::evidence_store::read(output.join("allocation-manifest.json"))?,
    )?;
    save(output.join("execution.json"), &execution)?;
    let factor_signature = crate::signature(&frozen)?;
    let signature = crate::signature(&(&factor_signature, &execution))?;
    let summary = json!({"schema_version":RESEARCH_SCHEMA_VERSION,"kind":"combined","skipped_events":[],"skipped_event_count":0,"event_simulation_complete":false,"backtest_status":if days.iter().any(|d|d.turnover>0.){"completed"}else{"no_valid_trades"},"signature":signature,"factor_signature":factor_signature,"execution":execution,"aggregate":portfolio::performance(&days),"fold_reports":[],"annual_reports":[],"execution_diagnostics":{"corporate_events":if output.join("corporate_event_report.json").exists(){serde_json::from_slice::<Value>(&analytics_storage::evidence_store::read(output.join("corporate_event_report.json"))?)?}else{Value::Null}},"reference_return_basis":"fractional zero-fee targets; actual account includes execution fees","source_selection_warning":"retrospective source strategy choice"});
    save(output.join("daily.json"), &days)?;
    save(output.join("fold_reports.json"), &json!([]))?;
    save(output.join("annual_reports.json"), &json!([]))?;
    save(output.join("summary.json"), &summary)?;
    fs::write(
        output.join("report.md"),
        format!(
            "# 多策略统一账户\n\n策略来源为显式冻结的历史研究任务；参考账无费用，最终收益来自单一成交账户。来源选择不构成策略选择层面的样本外证明。\n\n```json\n{}\n```\n",
            serde_json::to_string_pretty(&summary["aggregate"])?
        ),
    )?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_sources_net_into_one_account_and_verify_without_retraining() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (market, actions, t) = crate::demo::create_canonical(&w).unwrap();
        let mut sources = Vec::new();
        for _ in 0..2 {
            let request = crate::WorkflowRequest::Research {
                template: Box::new(t.clone()),
                market: market.id.clone(),
                actions: Some(actions.id.clone()),
                audit_only: false,
            };
            let context = TaskContext::default();
            let mut run = w
                .begin(context.id, "source", crate::snapshot(&w, &request).unwrap())
                .unwrap();
            crate::execute(&w, &request, &run, &context).unwrap();
            w.finish(&mut run, RunStatus::Succeeded, None).unwrap();
            sources.push(run.id);
        }
        let plan = CombinedPortfolioPlan {
            schema_version: 2,
            sources,
            range: DateRange {
                start: "2010-01-02".parse().unwrap(),
                end: "2010-01-12".parse().unwrap(),
            },
            execution: t.execution_settings(),
        };
        let request = crate::WorkflowRequest::Combined {
            request: Box::new(Job::Run { plan }),
        };
        let context = TaskContext::default();
        let mut run = w
            .begin(
                context.id,
                "combined",
                crate::snapshot(&w, &request).unwrap(),
            )
            .unwrap();
        let attempted = crate::execute(&w, &request, &run, &context);
        if let Err(e) = &attempted {
            eprintln!(
                "combined failure {e}; files={:?}",
                fs::read_dir(w.resolve(&run.output))
                    .unwrap()
                    .map(|e| e.unwrap().file_name())
                    .collect::<Vec<_>>()
            );
        }
        let result = attempted.unwrap();
        assert_eq!(result.details["kind"], "combined");
        validate_result(&result.details).unwrap();
        w.finish(&mut run, RunStatus::Succeeded, None).unwrap();
        super::super::verification::report(&w, run.id).unwrap();
        let root = w.resolve(&run.output);
        let report: Value = serde_json::from_slice(
            &analytics_storage::evidence_store::read(root.join("allocation_report.json")).unwrap(),
        )
        .unwrap();
        assert!(report["targets"].as_array().unwrap().iter().all(|t| {
            t["weights"]
                .as_object()
                .unwrap()
                .values()
                .map(|v| v.as_f64().unwrap())
                .sum::<f64>()
                <= 0.98 + 1e-6
        }));
        assert!(!root.join("fold-0000/model.txt").exists());
        let combined_bytes =
            analytics_storage::evidence_store::read(root.join("combined-input.json")).unwrap();
        fs::write(root.join("combined-input.json"), b"{}").unwrap();
        assert!(crate::allocation::verify_artifacts(&root).is_err());
        fs::write(root.join("combined-input.json"), &combined_bytes).unwrap();
        let bytes =
            analytics_storage::evidence_store::read(root.join("allocation_report.json")).unwrap();
        let manifest =
            analytics_storage::evidence_store::read(root.join("allocation-manifest.json")).unwrap();
        let mut changed = report.clone();
        changed["decisions"][0]["mapping"][0][0] = json!(0.123);
        save(root.join("allocation_report.json"), &changed).unwrap();
        assert!(crate::allocation::publish_artifacts(&root).is_err());
        fs::write(root.join("allocation_report.json"), bytes).unwrap();
        fs::write(root.join("allocation-manifest.json"), manifest).unwrap();
        let nested = Job::Run {
            plan: CombinedPortfolioPlan {
                schema_version: 2,
                sources: vec![
                    run.id,
                    match &request {
                        crate::WorkflowRequest::Combined { request } => request.plan().sources[0],
                        _ => unreachable!(),
                    },
                ],
                range: DateRange {
                    start: "2010-01-02".parse().unwrap(),
                    end: "2010-01-12".parse().unwrap(),
                },
                execution: t.execution_settings(),
            },
        };
        fs::remove_file(root.join("combined-input.json")).unwrap();
        assert!(snapshot(&w, &nested).is_err());
    }
}
