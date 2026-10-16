use super::*;
use crate::{
    factor_evaluation::EvaluationInput,
    research::pipeline::{
        Cancellation, PipelineError, PipelineEvent, Result, panel_from_rows, snapshot_hash,
    },
};
use analytics_core::{
    Date, MarketRow,
    admission::{AdmissionCheck, AdmissionStage},
    research_quality::{
        PriceBasis, ProblemDisposition as Disposition, ProblemKind, ResearchStrictness,
    },
};
use analytics_engine::{DuckDbEngine, QueryEngine};
use analytics_factor::{FactorRunArtifact, FactorRunRequest, Panel};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        atomic::Ordering,
        mpsc::{Sender, channel},
    },
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRequest {
    /// Frozen by the service at acceptance; revalidated by the owning worker.
    #[serde(default)]
    pub expected_input_hash: Option<String>,
    /// Cheap identity frozen at acceptance; full byte hashing stays on the worker.
    #[serde(default)]
    pub expected_source_stamp: Option<String>,
    pub input: EvaluationInput,
    pub horizon: usize,
    #[serde(default)]
    pub policy: ResearchQualityPolicy,
}
#[derive(Clone)]
pub(crate) struct Prepared {
    pub rows: Vec<MarketRow>,
    pub eligible: Vec<bool>,
    pub panel: Panel,
    pub registry: FactorRegistry,
    pub calendar: Vec<Date>,
    pub labels: Vec<Option<f64>>,
    pub input_hash: String,
    pub absent: BTreeSet<String>,
    pub expected: BTreeSet<(String, Date)>,
    pub cache_context: serde_json::Value,
}
fn cancelled(cancel: &Cancellation) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(PipelineError::Cancelled)
    } else {
        Ok(())
    }
}
fn invalid(e: impl std::fmt::Display) -> PipelineError {
    PipelineError::InvalidInput(e.to_string())
}
pub(crate) fn market_columns(
    engine: &mut DuckDbEngine,
    path: &std::path::Path,
) -> Result<BTreeSet<String>> {
    let path = if path.is_dir() {
        path.join("**/*.parquet")
    } else {
        path.to_path_buf()
    };
    let path = path.to_string_lossy();
    let columns: BTreeSet<_> = engine
        .describe(&format!(
            "SELECT * FROM read_parquet('{}',union_by_name=true)",
            path.replace('\'', "''")
        ))?
        .into_iter()
        .map(|c| c.name)
        .collect();
    Ok(columns)
}
pub(crate) fn missing_inputs(
    registry: &FactorRegistry,
    ids: &[String],
    columns: &BTreeSet<String>,
    panel: &mut Panel,
) -> Result<BTreeSet<String>> {
    let mut absent = BTreeSet::new();
    for id in ids {
        let spec = registry
            .get(id)
            .ok_or_else(|| invalid(format!("unknown factor {id}")))?;
        for col in &spec.input_columns {
            if !panel.columns.contains_key(col) {
                absent.insert(col.clone());
            }
            for (raw, _) in parquet_raw_inputs(col) {
                if !columns.contains(raw) && !columns.contains(&format!("adj_{raw}")) {
                    absent.insert(raw.into());
                }
            }
            panel
                .columns
                .entry(col.clone())
                .or_insert_with(|| vec![None; panel.instrument_id.len()]);
        }
    }
    Ok(absent)
}
/// Factor-page adapter: retain the original whole-file seed history and label rule.
pub(crate) fn prepare(
    input: &EvaluationInput,
    horizon: usize,
    progress: &Sender<PipelineEvent>,
    cancel: &Cancellation,
) -> Result<Prepared> {
    input.date_range.validate().map_err(invalid)?;
    if input.factor_ids.is_empty() || !(1..=252).contains(&horizon) {
        return Err(invalid("请选择因子；标签周期为1—252交易日"));
    }
    cancelled(cancel)?;
    let input_hash = snapshot_hash(&input.market_path, &input.universe_path)?;
    let _ = progress.send(PipelineEvent::LoadingMarket);
    let mut engine = DuckDbEngine::open_in_memory()?;
    let columns = market_columns(&mut engine, &input.market_path)?;
    let expected = engine
        .factor_expected_members(
            &input.market_path,
            &input.universe_path,
            &input.universe_id,
            input.date_range.end,
        )?
        .into_iter()
        .collect();
    let (rows, members) = engine.load_factor_research_rows(
        &input.market_path,
        &input.universe_path,
        &input.universe_id,
        &DateRange {
            start: "1900-01-01".parse().unwrap(),
            end: input.date_range.end,
        },
    )?;
    cancelled(cancel)?;
    let _ = progress.send(PipelineEvent::MarketLoaded { rows: rows.len() });
    let eligible: Vec<_> = rows
        .iter()
        .zip(members)
        .map(|(r, m)| {
            m && r.is_trading.unwrap_or(true) && r.listing_date.is_none_or(|d| d <= r.trade_date)
        })
        .collect();
    let mut panel = panel_from_rows(&rows);
    panel.columns.insert(
        "__universe_member".into(),
        eligible.iter().map(|m| Some(f64::from(*m))).collect(),
    );
    let registry =
        FactorRegistry::resolve(input.factor_set, &input.factor_refs, &input.factor_ids)?;
    let absent = missing_inputs(&registry, &input.factor_ids, &columns, &mut panel)?;
    let calendar: Vec<_> = rows
        .iter()
        .map(|r| r.trade_date)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let dates: HashMap<_, _> = calendar.iter().enumerate().map(|(i, d)| (*d, i)).collect();
    let prices: HashMap<_, _> = rows
        .iter()
        .filter(|r| r.is_trading.unwrap_or(true))
        .map(|r| ((r.instrument_id.as_str(), r.trade_date), r.adj_close))
        .collect();
    let mut labels = vec![None; rows.len()];
    for (i, r) in rows.iter().enumerate() {
        if !input.date_range.contains(r.trade_date) || !eligible[i] {
            continue;
        }
        if let Some(end) = calendar.get(dates[&r.trade_date] + horizon) {
            labels[i] = r
                .adj_close
                .zip(
                    prices
                        .get(&(r.instrument_id.as_str(), *end))
                        .copied()
                        .flatten(),
                )
                .and_then(|(a, b)| (a > 0. && b > 0.).then_some(b / a - 1.))
                .filter(|v| v.is_finite());
        }
    }
    Ok(Prepared {
        rows,
        eligible,
        panel,
        registry,
        calendar,
        labels,
        cache_context: serde_json::json!({"adapter":"parquet-history-v1","price_basis":"adjusted_with_raw_fallback","data":input_hash,"horizon":horizon,"policy":ResearchQualityPolicy::default()}),
        input_hash,
        absent,
        expected,
    })
}
fn parquet_raw_inputs(input: &str) -> Vec<(&str, usize)> {
    // Only transformations actually implemented by panel_from_rows; GTJA-specific
    // derived series unavailable in this adapter stay explicit absent inputs.
    match input {
        "vwap" => vec![("amount", 0), ("volume", 0)],
        "ret" => vec![("close", 0), ("close", 1)],
        other => vec![(other, 0)],
    }
}
pub(crate) fn compute(
    input: &EvaluationInput,
    p: &Prepared,
    progress: Sender<PipelineEvent>,
    cancel: Cancellation,
) -> Result<(FactorRunArtifact, cache::Receipt)> {
    let (tx, rx) = channel();
    let forward = std::thread::spawn(move || {
        for event in rx {
            let _ = progress.send(PipelineEvent::Factors(event));
        }
    });
    let request = FactorRunRequest {
        date_range: input.date_range.clone(),
        universe_id: input.universe_id.clone(),
        factor_ids: input.factor_ids.clone(),
        formula_version: p.registry.selection_version(&input.factor_ids)?,
    };
    let result = cache::compute(
        p.registry.clone(),
        p.panel.clone(),
        request,
        &p.cache_context,
        &input.output_dir,
        tx,
        cancel,
    );
    let _ = forward.join();
    result
}
pub fn context(
    input: &EvaluationInput,
    horizon: usize,
    policy: &ResearchQualityPolicy,
) -> serde_json::Value {
    serde_json::json!({"factor_set":input.factor_set,"factor_ids":input.factor_ids,"factor_refs":input.factor_refs,"market_path":input.market_path,"universe_path":input.universe_path,"universe_id":input.universe_id,"date_range":input.date_range,"horizon":horizon,"policy":policy,"adapter":"parquet-history-v1"})
}
fn ranges(calendar: &[Date], dates: &BTreeSet<Date>) -> Vec<DateRange> {
    let mut result: Vec<DateRange> = Vec::new();
    for date in dates {
        if let Some(last) = result.last_mut()
            && calendar
                .binary_search(&last.end)
                .ok()
                .zip(calendar.binary_search(date).ok())
                .is_some_and(|(a, b)| a + 1 == b)
        {
            last.end = *date;
        } else {
            result.push(DateRange {
                start: *date,
                end: *date,
            });
        }
    }
    result
}
fn check_input(
    plan: &InputCheckPlan,
    p: &Prepared,
    report: &mut InputCheckReport,
    cancel: &Cancellation,
) -> Result<()> {
    let indices: HashMap<_, _> = p
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| ((r.instrument_id.as_str(), r.trade_date), i))
        .collect();
    let mut membership: BTreeMap<String, BTreeSet<Date>> = BTreeMap::new();
    for (id, date) in &p.expected {
        membership.entry(id.clone()).or_default().insert(*date);
    }
    let mut pool = analytics_core::pool::PoolMembership {
        coverage: DateRange {
            start: p
                .calendar
                .first()
                .copied()
                .unwrap_or(plan.phases[0].effective.start),
            end: p
                .calendar
                .last()
                .copied()
                .unwrap_or(plan.phases[0].effective.end),
        },
        intervals: BTreeMap::new(),
    };
    for (id, dates) in membership {
        pool.intervals.insert(id, ranges(&p.calendar, &dates));
    }
    let mut raw_plan = plan.clone();
    raw_plan.context["seed"] = "parquet-full-history".into();
    for dependency in raw_plan.dependencies.values_mut() {
        let original = std::mem::take(dependency);
        for (input, windows) in original {
            for (raw, extra) in parquet_raw_inputs(&input) {
                dependency
                    .entry(raw.into())
                    .or_default()
                    .extend(
                        windows
                            .iter()
                            .map(|w| analytics_factor::dependencies::Window {
                                first: w.first + extra,
                                last: w.last + extra,
                                ..*w
                            }),
                    );
            }
        }
    }
    let needs = crate::input_check::daily::requirements(&raw_plan, &p.calendar, &pool, "");
    let short = plan
        .dependencies
        .values()
        .flat_map(|d| d.values())
        .flatten()
        .filter(|w| {
            w.last
                > p.calendar
                    .partition_point(|d| *d < plan.phases[0].effective.start)
        })
        .count();
    let mut dependency_ranges = BTreeMap::new();
    for (id, fields) in &needs {
        for (field, days) in fields {
            cancelled(cancel)?;
            let dates = days.iter().map(|&day| p.calendar[day]).collect();
            dependency_ranges.insert(format!("{id}/{field}"), ranges(&p.calendar, &dates));
            let missing = days
                .iter()
                .filter(|&&day| {
                    indices
                        .get(&(id.as_str(), p.calendar[day]))
                        .and_then(|i| p.panel.columns.get(field).and_then(|v| v[*i]))
                        .is_none_or(|v| !v.is_finite())
                })
                .count();
            if missing > 0 {
                *report.missing_counts.entry(field.clone()).or_default() += missing;
                let history = days
                    .iter()
                    .filter(|&&day| {
                        let date = p.calendar[day];
                        let missing = indices
                            .get(&(id.as_str(), date))
                            .and_then(|i| p.panel.columns.get(field).and_then(|v| v[*i]))
                            .is_none_or(|v| !v.is_finite());
                        missing
                            && !(plan.phases.iter().any(|p| p.effective.contains(date))
                                && pool
                                    .intervals
                                    .get(id)
                                    .is_some_and(|rs| rs.iter().any(|r| r.contains(date))))
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
            let invalid_dates: BTreeSet<_> = days
                .iter()
                .filter_map(|&day| {
                    indices
                        .get(&(id.as_str(), p.calendar[day]))
                        .and_then(|i| p.panel.columns.get(field).and_then(|v| v[*i]))
                        .filter(|v| {
                            !v.is_finite()
                                || (matches!(field.as_str(), "open" | "high" | "low" | "close")
                                    && *v <= 0.)
                                || (matches!(field.as_str(), "volume" | "amount") && *v < 0.)
                        })
                        .map(|_| p.calendar[day])
                })
                .collect();
            for coverage in ranges(&p.calendar, &invalid_dates) {
                report.admission.checks.push(AdmissionCheck {
                    stage: AdmissionStage::Training,
                    kind: ProblemKind::Damaged,
                    disposition: Disposition::Block,
                    category: "market".into(),
                    security: Some(id.clone()),
                    coverage: Some(coverage),
                    status: CheckStatus::Failed,
                    code: "input_value_invalid".into(),
                    message: format!("{field} 包含无效的价格或成交数据"),
                    evidence: vec![p.input_hash.clone()],
                    affected_factors: plan.factor_ids.clone(),
                });
            }
        }
    }
    report.dependency_ranges = serde_json::to_value(dependency_ranges)?;
    if report.missing_scope_counts.contains_key("history") {
        report
            .warnings
            .push("历史依赖缺失单独记录在 missing_scope_counts.history".into());
    }
    if short > 0 {
        report
            .warnings
            .push(format!("历史回看不足：{short} 个依赖窗口，结果保留空值"));
    }
    if !report.missing_counts.is_empty() {
        report
            .warnings
            .push("任务依赖范围内存在局部空值；不会据此认定停牌或损坏".into());
    }
    for field in &p.absent {
        report.admission.checks.push(AdmissionCheck {
            stage: AdmissionStage::Training,
            kind: ProblemKind::Missing,
            disposition: policy_disposition(&report.admission.policy),
            category: "market".into(),
            security: None,
            coverage: None,
            status: CheckStatus::Failed,
            code: "input_field_absent".into(),
            message: format!("输入缺少字段 {field}"),
            evidence: vec![p.input_hash.clone()],
            affected_factors: plan.factor_ids.clone(),
        });
    }
    Ok(())
}
pub fn run(
    request: &CheckRequest,
    progress: Sender<PipelineEvent>,
    cancel: Cancellation,
) -> Result<InputCheckReport> {
    request.policy.validate().map_err(invalid)?;
    if let Some(expected) = &request.expected_source_stamp
        && source_stamp(&request.input.market_path, &request.input.universe_path)
            .map_err(invalid)?
            != *expected
    {
        return Err(invalid("输入文件在任务提交后发生变化，请重新检查"));
    }
    let _ = progress.send(PipelineEvent::CheckingInputs {
        stage: "dependencies".into(),
    });
    let mut p = prepare(&request.input, request.horizon, &progress, &cancel)?;
    if request
        .expected_input_hash
        .as_ref()
        .is_some_and(|h| h != &p.input_hash)
    {
        return Err(invalid("输入文件在任务提交后发生变化，请重新检查"));
    }
    p.cache_context["policy"] = serde_json::to_value(&request.policy)?;
    run_prepared(request, &mut p, progress, cancel).map(|(report, _)| report)
}
pub(crate) fn run_prepared(
    request: &CheckRequest,
    p: &mut Prepared,
    progress: Sender<PipelineEvent>,
    cancel: Cancellation,
) -> Result<(InputCheckReport, Option<FactorRunArtifact>)> {
    request.policy.validate().map_err(invalid)?;
    p.cache_context["policy"] = serde_json::to_value(&request.policy)?;
    let mut computed_artifact = None;
    let context = context(&request.input, request.horizon, &request.policy);
    let mut effective = request.input.date_range.clone();
    if let Some(last) = p
        .rows
        .iter()
        .filter(|r| r.adj_close.is_some_and(|v| v.is_finite() && v > 0.))
        .map(|r| r.trade_date)
        .max()
    {
        effective.end = effective.end.min(last);
    }
    let plan = InputCheckPlan::new(
        request.input.factor_set,
        &request.input.factor_refs,
        &request.input.factor_ids,
        &p.registry,
        "factor_page",
        context,
        vec![PhaseRange {
            phase: "factor_page".into(),
            requested: request.input.date_range.clone(),
            effective: effective.clone(),
        }],
    )
    .map_err(invalid)?;
    let mut report=InputCheckReport{ready:false,schema_version:1,context_hash:crate::signature(&(&plan,&p.input_hash)).map_err(invalid)?,plan:plan.clone(),stages:stages(),admission:AdmissionReport{schema_version:5,price_basis:PriceBasis::Provider,policy:request.policy.clone(),decisions:vec![],checks:vec![]},factors:vec![],samples:serde_json::json!({}),dependency_ranges:serde_json::json!({}),missing_scope_counts:BTreeMap::new(),missing_counts:BTreeMap::new(),warnings:vec!["通过仅表示因子页当前计算上下文可用；训练还需检查训练标签、样本掩码和最少样本门槛。标签使用复权收盘至未来H个市场交易日收盘；标签尾部不足不截短计算周期。递归因子沿用文件内全部前置历史。".into()],artifact:None};
    report.stages[0].status = CheckStatus::Passed;
    if effective.end < request.input.date_range.end {
        report.warnings.push(format!(
            "数据尾部不足：请求结束 {}，实际结束 {}",
            request.input.date_range.end, effective.end
        ));
    }
    let _ = progress.send(PipelineEvent::CheckingInputs {
        stage: "inputs".into(),
    });
    check_input(&plan, p, &mut report, &cancel)?;
    let eligible = p
        .expected
        .iter()
        .filter(|(_, d)| effective.contains(*d))
        .count();
    let blocked = eligible == 0
        || report
            .admission
            .checks
            .iter()
            .any(|c| c.disposition == Disposition::Block);
    report.admission.checks.push(AdmissionCheck {
        stage: AdmissionStage::Training,
        kind: if eligible == 0 {
            ProblemKind::Missing
        } else {
            ProblemKind::None
        },
        disposition: Disposition::Block,
        category: "membership".into(),
        security: None,
        coverage: Some(effective.clone()),
        status: if eligible == 0 {
            CheckStatus::Failed
        } else {
            CheckStatus::Passed
        },
        code: if eligible == 0 {
            "no_eligible_members"
        } else {
            "membership_verified"
        }
        .into(),
        message: format!("区间内有效成员行：{eligible}"),
        evidence: vec![p.input_hash.clone()],
        affected_factors: plan.factor_ids.clone(),
    });
    report.stages[1].status = if blocked {
        CheckStatus::Failed
    } else {
        CheckStatus::Passed
    };
    if !blocked {
        if request.policy.strictness == ResearchStrictness::QualifiedOnly {
            for (i, member) in p.eligible.iter_mut().enumerate() {
                if *member
                    && request
                        .input
                        .factor_ids
                        .iter()
                        .filter_map(|id| p.registry.get(id))
                        .flat_map(|s| &s.input_columns)
                        .any(|f| {
                            p.panel
                                .columns
                                .get(f)
                                .is_none_or(|v| v[i].is_none_or(|v| !v.is_finite()))
                        })
                {
                    *member = false;
                }
            }
            p.panel.columns.insert(
                "__universe_member".into(),
                p.eligible.iter().map(|m| Some(f64::from(*m))).collect(),
            );
            // Qualification changes cross sections, so the full context is recalculated.
        }
        let _ = progress.send(PipelineEvent::CheckingInputs {
            stage: "factors".into(),
        });
        let sample_progress = progress.clone();
        let (artifact, receipt) = compute(&request.input, p, progress, cancel.clone())?;
        report.artifact = Some(receipt);
        let _ = sample_progress.send(PipelineEvent::CheckingInputs {
            stage: "samples".into(),
        });
        report.stages[2].status = CheckStatus::Passed;
        let selected: Vec<_> = p
            .rows
            .iter()
            .enumerate()
            .filter(|(i, r)| p.eligible[*i] && effective.contains(r.trade_date))
            .map(|(i, _)| i)
            .collect();
        for id in &request.input.factor_ids {
            let values = &artifact.values[id];
            let finite = selected
                .iter()
                .filter(|&&i| values[i].is_some_and(f64::is_finite))
                .count();
            let paired = selected
                .iter()
                .filter(|&&i| values[i].is_some_and(f64::is_finite) && p.labels[i].is_some())
                .count();
            let missing_days = selected
                .iter()
                .filter(|&&i| values[i].is_none_or(|v| !v.is_finite()))
                .map(|&i| p.rows[i].trade_date)
                .collect();
            if finite == 0 {
                report.warnings.push(format!("{id}：计算结果为空"));
            }
            report.factors.push(FactorCoverage {
                factor_id: id.clone(),
                phase: "factor_page".into(),
                eligible,
                finite,
                paired,
                coverage: (eligible > 0).then(|| finite as f64 / eligible as f64),
                empty_intervals: ranges(&p.calendar, &missing_days),
            });
        }
        let labeled = selected.iter().filter(|&&i| p.labels[i].is_some()).count();
        report.samples = serde_json::json!({"eligible_rows":selected.len(),"labeled_rows":labeled,"unpaired_rows":selected.len()-labeled,"training_readiness_checked":false});
        if labeled < selected.len() {
            report.warnings.push(format!(
                "{} 行标签不可计算（含区间尾部），不缩短成交周期",
                selected.len() - labeled
            ));
        }
        report.stages[3].status = if selected.is_empty() {
            CheckStatus::Failed
        } else {
            CheckStatus::Passed
        };
        report.stages[3].message =
            format!("有效成员 {} 行；可配对标签 {labeled} 行", selected.len());
        computed_artifact = Some(artifact);
    }
    cancelled(&cancel)?;
    if p.input_hash != snapshot_hash(&request.input.market_path, &request.input.universe_path)? {
        return Err(invalid("检查期间输入变化，请重新检查"));
    }
    report.samples["source_stamp"] =
        source_stamp(&request.input.market_path, &request.input.universe_path)
            .map_err(invalid)?
            .into();
    report.ready = report.ready();
    std::fs::create_dir_all(&request.input.output_dir)?;
    crate::atomic_json(&request.input.output_dir.join("input-check.json"), &report)
        .map_err(invalid)?;
    crate::atomic_json(&request.input.output_dir.join("execution_readiness.json"),&serde_json::json!({"schema_version":1,"purpose":"factor_page","ready":report.ready(),"report":"input-check.json","training_readiness_checked":false})).map_err(invalid)?;
    Ok((report, computed_artifact))
}
