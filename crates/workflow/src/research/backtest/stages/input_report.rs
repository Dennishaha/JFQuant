//! Assemble the shared report from the actual research panel/sample path.
use super::*;
use crate::input_check::{FactorCoverage, InputCheckPlan, InputCheckReport};
pub(super) fn plan(
    request: &ResearchRequest,
    dataset: &mut DailyDataset,
    registry: &FactorRegistry,
    pool: &analytics_core::pool::PoolMembership,
    folds: &[analytics_core::schedule::ResolvedFold],
    predicting: bool,
    market_hash: &str,
) -> Result<(InputCheckPlan, crate::input_check::daily::RequiredInputs)> {
    let check_plan = crate::input_check::InputCheckPlan::new(
        request.factor_set,
        &request.config.factor_refs,
        &request.config.factor_ids,
        registry,
        if predicting {
            "backtest"
        } else if request.train_only {
            "training"
        } else {
            "research"
        },
        serde_json::json!({"market":market_hash,"price_basis":request.price_basis,"policy":request.quality_policy,"benchmark":request.benchmark,"seed":"research-annual-756-v1","pool":pool}),
        crate::input_check::daily::phases(folds, predicting, request.train_only),
    )?;
    save(request.output.join("input-check-plan.json"), &check_plan)?;
    let mut required_inputs = crate::input_check::daily::requirements(
        &check_plan,
        &dataset.calendar,
        pool,
        &request.benchmark,
    );
    if !request.train_only {
        for f in folds {
            let lo = dataset
                .calendar
                .partition_point(|d| *d < f.actual.test.start);
            let hi = dataset
                .calendar
                .partition_point(|d| *d <= f.actual.test.end);
            for field in ["close", "factor"] {
                required_inputs
                    .entry(request.benchmark.clone())
                    .or_default()
                    .entry(field.into())
                    .or_default()
                    .extend(lo.saturating_sub(1)..hi);
            }
            for (id, rs) in &pool.intervals {
                if rs
                    .iter()
                    .any(|r| r.start <= f.actual.test.end && r.end >= f.actual.test.start)
                {
                    for field in ["open", "high", "low", "close", "volume", "factor"] {
                        required_inputs
                            .entry(id.clone())
                            .or_default()
                            .entry(field.into())
                            .or_default()
                            .extend(lo..hi);
                    }
                }
            }
        }
    }
    dataset.restrict_input_checks(required_inputs.clone());
    Ok((check_plan, required_inputs))
}
pub(super) fn inputs(
    request: &ResearchRequest,
    plan: InputCheckPlan,
    dataset: &DailyDataset,
    required: &crate::input_check::daily::RequiredInputs,
    admission: &analytics_core::admission::AdmissionReport,
    ready: bool,
) -> Result<()> {
    let mut report=InputCheckReport{ready:false,schema_version:1,context_hash:crate::signature(&plan)?,plan,stages:crate::input_check::stages(),admission:admission.clone(),factors:vec![],samples:serde_json::json!({}),dependency_ranges:serde_json::json!({}),missing_scope_counts:BTreeMap::new(),missing_counts:BTreeMap::new(),warnings:vec!["完整检查包含实际因子与样本计算；公司行动及账户依赖沿用现有准入规则。标签使用下一交易日开盘至下下交易日开盘，标签不跨样本区间。".into()],artifact:None};
    let mut ranges = BTreeMap::new();
    for (id, fields) in required {
        for (field, days) in fields {
            TaskContext::current().check()?;
            if dataset.uses_supplier_prices(id) && matches!(field.as_str(), "vwap" | "factor") {
                continue;
            }
            let values = dataset.research_field(id, field, true);
            let missing = days
                .iter()
                .filter(|&&d| values.as_ref().ok().and_then(|f| f.get(d)).is_none())
                .count();
            if missing > 0 {
                *report.missing_counts.entry(field.clone()).or_default() += missing;
                let history = days
                    .iter()
                    .filter(|&&day| {
                        values.as_ref().ok().and_then(|f| f.get(day)).is_none()
                            && !report
                                .plan
                                .phases
                                .iter()
                                .any(|p| p.effective.contains(dataset.calendar[day]))
                    })
                    .count();
                for (scope, count) in [("history", history), ("task", missing - history)] {
                    if count > 0 {
                        *report
                            .missing_scope_counts
                            .entry(scope.into())
                            .or_default()
                            .entry(field.clone())
                            .or_default() += count;
                    }
                }
            }
            let mut intervals: Vec<DateRange> = vec![];
            for &day in days {
                let date = dataset.calendar[day];
                if let Some(last) = intervals.last_mut()
                    && dataset
                        .calendar
                        .binary_search(&last.end)
                        .ok()
                        .is_some_and(|n| n + 1 == day)
                {
                    last.end = date;
                } else {
                    intervals.push(DateRange {
                        start: date,
                        end: date,
                    });
                }
            }
            ranges.insert(format!("{id}/{field}"), intervals);
        }
    }
    report.dependency_ranges = serde_json::to_value(ranges)?;
    if request.output.join("allocation-readiness.json").is_file() {
        report.samples["allocation"] =
            serde_json::from_slice(&fs::read(request.output.join("allocation-readiness.json"))?)?;
    }
    if !report.missing_counts.is_empty() {
        report
            .warnings
            .push("依赖区间存在缺失，按已选择的三档策略处置；价格空值不代表停牌或损坏".into());
    }
    for phase in &report.plan.phases {
        if phase.requested.end > phase.effective.end {
            report.warnings.push(format!(
                "{} 请求结束 {}，实际结束 {}",
                phase.phase, phase.requested.end, phase.effective.end
            ));
        }
    }
    report.stages[0].status = CheckStatus::Passed;
    report.stages[1].status = if ready {
        CheckStatus::Passed
    } else {
        CheckStatus::Failed
    };
    report.ready = report.ready();
    save(request.output.join("input-check.json"), &report)
}
pub(super) fn samples(
    request: &ResearchRequest,
    cache: &Path,
    calendar: &[Date],
    admission: &analytics_core::admission::AdmissionReport,
    issues: &[String],
    selections: &serde_json::Value,
) -> Result<()> {
    let mut report: InputCheckReport =
        serde_json::from_slice(&fs::read(request.output.join("input-check.json"))?)?;
    let mut receipts = BTreeMap::new();
    for phase in &report.plan.phases {
        let values =
            data::prediction_range(cache, &phase.effective, &report.plan.factor_ids, calendar)?;
        let mut empty = BTreeMap::<String, Vec<DateRange>>::new();
        for (column, id) in report.plan.factor_ids.iter().enumerate() {
            let mut finite = 0;
            let mut paired = 0;
            let mut dates = BTreeSet::new();
            for (row, features) in values
                .meta
                .rows
                .iter()
                .zip(values.features.chunks_exact(report.plan.factor_ids.len()))
            {
                if features[column].is_finite() {
                    finite += 1;
                    if row.label.is_some() {
                        paired += 1;
                    }
                } else {
                    dates.insert(calendar[row.day]);
                }
            }
            let mut ranges: Vec<DateRange> = vec![];
            for date in dates {
                if let Some(last) = ranges.last_mut()
                    && calendar
                        .binary_search(&last.end)
                        .ok()
                        .zip(calendar.binary_search(&date).ok())
                        .is_some_and(|(a, b)| a + 1 == b)
                {
                    last.end = date;
                } else {
                    ranges.push(DateRange {
                        start: date,
                        end: date,
                    });
                }
            }
            empty.insert(id.clone(), ranges);
            if finite == 0 {
                report
                    .warnings
                    .push(format!("{id} {}：计算结果为空", phase.phase));
            }
            report.factors.push(FactorCoverage {
                factor_id: id.clone(),
                phase: phase.phase.clone(),
                eligible: values.meta.rows.len(),
                finite,
                paired,
                coverage: (!values.meta.rows.is_empty())
                    .then(|| finite as f64 / values.meta.rows.len() as f64),
                empty_intervals: empty.remove(id).unwrap_or_default(),
            });
        }
        for year in phase.effective.start.year()..=phase.effective.end.year() {
            let receipt: serde_json::Value =
                serde_json::from_slice(&fs::read(cache.join(format!("{year}.receipt.json")))?)?;
            receipts.insert(year, receipt);
        }
    }
    report.admission = admission.clone();
    report.samples = serde_json::json!({"selection":selections,"factor_receipts":receipts,"training_readiness_checked":report.plan.purpose!="backtest"});
    if request.output.join("allocation-readiness.json").is_file() {
        report.samples["allocation"] =
            serde_json::from_slice(&fs::read(request.output.join("allocation-readiness.json"))?)?;
    }
    report.stages[2].status = CheckStatus::Passed;
    report.stages[3].status = if issues.is_empty() && admission.ready() {
        CheckStatus::Passed
    } else {
        CheckStatus::Failed
    };
    report.stages[3].message = issues.join("; ");
    report.ready = report.ready();
    save(request.output.join("input-check.json"), &report)
}

pub(super) fn unresolved(request: &ResearchRequest, reason: &str) -> Result<()> {
    let registry = request.factor_registry()?;
    let ids = if request.config.factor_ids.is_empty() {
        registry
            .all()
            .filter(|s| {
                !matches!(
                    s.expression,
                    analytics_factor::FactorExpr::Unsupported { .. }
                ) && s
                    .input_columns
                    .iter()
                    .all(|c| SUPPORTED_RESEARCH_INPUTS.contains(&c.as_str()))
            })
            .map(|s| s.id.clone())
            .collect()
    } else {
        request.config.factor_ids.clone()
    };
    let mut dependencies = BTreeMap::new();
    for id in &ids {
        if let Some(spec) = registry.get(id)
            && let Ok(d) = analytics_factor::dependencies::resolve(&spec.expression)
        {
            dependencies.insert(id.clone(), d);
        }
    }
    let plan = InputCheckPlan {
        schema_version: 1,
        purpose: "unresolved".into(),
        context: serde_json::json!({"request":request,"execution_ranges_resolved":false}),
        factor_refs: analytics_factor::selection::frozen_refs(
            request.factor_set,
            &request.config.factor_refs,
            &ids,
        ),
        formula_version: registry.selection_version(&ids)?,
        factor_ids: ids.clone(),
        phases: vec![],
        dependencies,
    };
    let mut phases = crate::input_check::stages();
    phases[1].status = CheckStatus::Failed;
    phases[1].message = reason.into();
    let check = analytics_core::admission::AdmissionCheck {
        stage: Default::default(),
        kind: ProblemKind::Missing,
        disposition: ProblemDisposition::Block,
        category: "task_inputs".into(),
        security: None,
        coverage: None,
        status: CheckStatus::Failed,
        code: "input_ranges_unresolved".into(),
        message: reason.into(),
        evidence: vec![],
        affected_factors: ids,
    };
    let report = InputCheckReport {
        ready: false,
        schema_version: 1,
        context_hash: crate::signature(&plan)?,
        plan,
        stages: phases,
        admission: analytics_core::admission::AdmissionReport {
            schema_version: 5,
            price_basis: request.price_basis,
            policy: request.quality_policy.clone(),
            decisions: vec![],
            checks: vec![check],
        },
        factors: vec![],
        samples: serde_json::json!({}),
        dependency_ranges: serde_json::json!({}),
        missing_scope_counts: BTreeMap::new(),
        missing_counts: BTreeMap::new(),
        warnings: vec!["未能解析可执行输入范围；未计算因子或样本".into()],
        artifact: None,
    };
    save(request.output.join("input-check.json"), &report)
}
