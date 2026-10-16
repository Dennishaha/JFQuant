//! Independent cross-sectional diagnostics; no model fit or trading simulation.
#![allow(missing_docs)]
use crate::research::pipeline::{
    Cancellation, PipelineError, PipelineEvent, PipelineRequest, Result, snapshot_hash,
};
use analytics_core::{Date, DateRange};
#[cfg(test)]
use analytics_engine::DuckDbEngine;

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{atomic::Ordering, mpsc::Sender},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationRequest {
    #[serde(default)]
    pub policy: analytics_core::research_quality::ResearchQualityPolicy,
    pub input: EvaluationInput,
    pub horizon: usize,
    pub quantiles: usize,
    pub min_samples: usize,
}
/// Data-only input, independent of training parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationInput {
    pub factor_set: analytics_core::research_quality::FactorSet,
    pub market_path: std::path::PathBuf,
    pub universe_path: std::path::PathBuf,
    pub universe_id: String,
    pub date_range: DateRange,
    pub factor_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub factor_refs: Vec<analytics_factor::selection::FactorRef>,
    pub output_dir: std::path::PathBuf,
}
impl From<PipelineRequest> for EvaluationInput {
    fn from(p: PipelineRequest) -> Self {
        Self {
            factor_set: p.factor_set,
            market_path: p.market_path,
            universe_path: p.universe_path,
            universe_id: p.universe_id,
            date_range: p.date_range,
            factor_ids: p.factor_ids,
            factor_refs: p.factor_refs,
            output_dir: p.output_dir,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyDiagnostic {
    pub date: Date,
    pub eligible: usize,
    pub finite: usize,
    pub paired: usize,
    pub ic: Option<f64>,
    pub rank_ic: Option<f64>,
    pub quantile_returns: Vec<Option<f64>>,
    pub spread: Option<f64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactorDiagnostic {
    pub factor_id: String,
    pub coverage: Option<f64>,
    pub labeled_coverage: Option<f64>,
    pub valid_days: usize,
    pub mean_ic: Option<f64>,
    pub mean_rank_ic: Option<f64>,
    pub rank_ic_ir: Option<f64>,
    pub positive_rank_ic_ratio: Option<f64>,
    pub quantile_returns: Vec<Option<f64>>,
    pub mean_spread: Option<f64>,
    pub daily: Vec<DailyDiagnostic>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationReport {
    #[serde(default)]
    pub computation_receipt: Option<crate::input_check::cache::Receipt>,
    #[serde(default)]
    pub execution_range: Option<DateRange>,
    pub schema_version: u32,
    pub request: EvaluationRequest,
    pub formula_version: String,
    pub input_hash: String,
    pub feature_schema_hash: String,
    pub factor_manifest: serde_json::Value,
    pub warnings: Vec<String>,
    pub factors: Vec<FactorDiagnostic>,
}
fn check(cancel: &Cancellation) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(PipelineError::Cancelled)
    } else {
        Ok(())
    }
}
fn mean(v: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = v.fold((0., 0usize), |(s, n), x| (s + x, n + 1));
    (n > 0).then(|| sum / n as f64).filter(|x| x.is_finite())
}
fn correlation(a: &[f64], b: &[f64]) -> Option<f64> {
    if a.len() < 2 || a.len() != b.len() {
        return None;
    }
    let ma = mean(a.iter().copied())?;
    let mb = mean(b.iter().copied())?;
    let (mut cov, mut va, mut vb) = (0., 0., 0.);
    for (a, b) in a.iter().zip(b) {
        cov += (a - ma) * (b - mb);
        va += (a - ma).powi(2);
        vb += (b - mb).powi(2);
    }
    (va > 0. && vb > 0.)
        .then(|| (cov / va.sqrt() / vb.sqrt()).clamp(-1., 1.))
        .filter(|x| x.is_finite())
}
fn ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<_> = (0..values.len()).collect();
    order.sort_by(|&a, &b| values[a].total_cmp(&values[b]));
    let mut result = vec![0.; values.len()];
    let mut start = 0;
    while start < order.len() {
        let mut end = start + 1;
        while end < order.len() && values[order[end]] == values[order[start]] {
            end += 1;
        }
        for i in start..end {
            result[order[i]] = (start + end - 1) as f64 / 2.;
        }
        start = end;
    }
    result
}
fn diagnose(
    id: &str,
    values: &[Option<f64>],
    labels: &[Option<f64>],
    days: &BTreeMap<Date, Vec<usize>>,
    request: &EvaluationRequest,
    cancel: &Cancellation,
) -> Result<FactorDiagnostic> {
    let mut daily = Vec::new();
    for (&date, indices) in days {
        check(cancel)?;
        let finite = indices
            .iter()
            .filter(|&&i| values[i].is_some_and(f64::is_finite))
            .count();
        let pairs: Vec<_> = indices
            .iter()
            .filter_map(|&i| {
                values[i]
                    .zip(labels[i])
                    .filter(|(a, b)| a.is_finite() && b.is_finite())
            })
            .collect();
        let (x, y): (Vec<_>, Vec<_>) = pairs.iter().copied().unzip();
        let mut d = DailyDiagnostic {
            date,
            eligible: indices.len(),
            finite,
            paired: pairs.len(),
            ic: None,
            rank_ic: None,
            quantile_returns: vec![None; request.quantiles],
            spread: None,
        };
        if pairs.len() >= request.min_samples {
            d.ic = correlation(&x, &y);
            let ranked = ranks(&x);
            d.rank_ic = correlation(&ranked, &ranks(&y));
            // All ties remain in the same bucket; never split a constant signal by ticker.
            if d.ic.is_some() {
                let mut buckets = vec![Vec::new(); request.quantiles];
                for (r, y) in ranked.iter().zip(&y) {
                    let q = ((*r * request.quantiles as f64 / x.len() as f64).floor() as usize)
                        .min(request.quantiles - 1);
                    buckets[q].push(*y);
                }
                d.quantile_returns = buckets.iter().map(|v| mean(v.iter().copied())).collect();
                d.spread = d
                    .quantile_returns
                    .last()
                    .copied()
                    .flatten()
                    .zip(d.quantile_returns[0])
                    .map(|(hi, lo)| hi - lo);
            }
        }
        daily.push(d);
    }
    let eligible: usize = daily.iter().map(|d| d.eligible).sum();
    let rank_ics: Vec<_> = daily.iter().filter_map(|d| d.rank_ic).collect();
    let rank_mean = mean(rank_ics.iter().copied());
    let ir = rank_mean.and_then(|m| {
        if rank_ics.len() < 2 {
            return None;
        }
        let sd = (rank_ics.iter().map(|x| (x - m).powi(2)).sum::<f64>()
            / (rank_ics.len() - 1) as f64)
            .sqrt();
        (sd > 1e-12).then(|| m / sd).filter(|x| x.is_finite())
    });
    Ok(FactorDiagnostic {
        factor_id: id.into(),
        coverage: (eligible > 0)
            .then(|| daily.iter().map(|d| d.finite).sum::<usize>() as f64 / eligible as f64),
        labeled_coverage: (eligible > 0)
            .then(|| daily.iter().map(|d| d.paired).sum::<usize>() as f64 / eligible as f64),
        valid_days: rank_ics.len(),
        mean_ic: mean(daily.iter().filter_map(|d| d.ic)),
        mean_rank_ic: rank_mean,
        rank_ic_ir: ir,
        positive_rank_ic_ratio: mean(rank_ics.iter().map(|x| if *x > 0. { 1. } else { 0. })),
        quantile_returns: (0..request.quantiles)
            .map(|q| mean(daily.iter().filter_map(|d| d.quantile_returns[q])))
            .collect(),
        mean_spread: mean(daily.iter().filter_map(|d| d.spread)),
        daily,
    })
}
/// Runs on the owning worker, preserving all available pre-period history.
pub fn run(
    request: &EvaluationRequest,
    progress: Sender<PipelineEvent>,
    cancel: Cancellation,
) -> Result<EvaluationReport> {
    let input = &request.input;
    if !(2..=10).contains(&request.quantiles) || request.min_samples < request.quantiles.max(3) {
        return Err(PipelineError::InvalidInput(
            "分层2—10组、最少样本不低于分层数及3".into(),
        ));
    }
    let mut prepared =
        crate::input_check::parquet::prepare(input, request.horizon, &progress, &cancel)?;
    let check_request = crate::input_check::CheckRequest {
        expected_input_hash: None,
        expected_source_stamp: None,
        input: input.clone(),
        horizon: request.horizon,
        policy: request.policy.clone(),
    };
    let (availability, artifact) = crate::input_check::parquet::run_prepared(
        &check_request,
        &mut prepared,
        progress,
        cancel.clone(),
    )?;
    if !availability.ready() {
        return Err(PipelineError::InvalidInput(
            "data_not_ready: input check failed; see input-check.json".into(),
        ));
    }
    let artifact = artifact
        .ok_or_else(|| PipelineError::InvalidInput("factor computation did not complete".into()))?;
    let execution_range = availability.plan.phases[0].effective.clone();
    let rows = &prepared.rows;
    let eligible = &prepared.eligible;
    let labels = &prepared.labels;
    let hash = prepared.input_hash.clone();
    let missing_inputs = &prepared.absent;
    let formula_version = prepared.registry.selection_version(&input.factor_ids)?;
    let mut days: BTreeMap<Date, Vec<usize>> = BTreeMap::new();
    for (i, r) in rows.iter().enumerate() {
        if execution_range.contains(r.trade_date) && eligible[i] {
            days.entry(r.trade_date).or_default().push(i);
        }
    }
    if days.is_empty() {
        return Err(PipelineError::InvalidInput(
            "测试区间没有有效股票池成员".into(),
        ));
    }
    let mut factors = Vec::new();
    for id in &input.factor_ids {
        factors.push(diagnose(
            id,
            &artifact.values[id],
            labels,
            &days,
            request,
            &cancel,
        )?);
    }
    check(&cancel)?;
    if hash != snapshot_hash(&input.market_path, &input.universe_path)? {
        return Err(PipelineError::InvalidInput(
            "测试期间输入文件发生变化，请重试".into(),
        ));
    }
    let feature_schema_hash = crate::signature(&(formula_version.as_str(), &input.factor_ids))
        .map_err(|e| PipelineError::InvalidInput(e.to_string()))?;
    let mut report = EvaluationReport { computation_receipt:availability.artifact,execution_range:Some(execution_range),feature_schema_hash, schema_version: 3, request: request.clone(), formula_version, input_hash: hash, factor_manifest: serde_json::from_str(&artifact.manifest_json)?, warnings: vec![
        "收益口径：当日复权收盘至未来H个市场交易日复权收盘；复权价缺失时沿用加载器的原价回退。仅统计诊断，不含费用、成交限制或行业/市值中性化。".into(),
        "日期历取行情中所有日期；缺失终点、停牌终点和区间尾部保留空值，不顺延。日均分层收益可重叠，不作为可交易净值。".into(),
        "使用文件内全部前置历史预热；短历史正常输出空值，完全缺失的行情行不会补齐。RankIC IR为均值/样本标准差，不年化，不判定显著性。".into(),
    ], factors };
    if !missing_inputs.is_empty() {
        report.warnings.push(format!(
            "输入行情不含这些因子依赖，相关结果保留空值：{}",
            missing_inputs
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    std::fs::create_dir_all(&input.output_dir)?;
    crate::atomic_json(&input.output_dir.join("factor-evaluation.json"), &report)
        .map_err(|e| PipelineError::InvalidInput(e.to_string()))?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::pipeline::cancellation;
    use std::sync::mpsc::channel;
    fn request() -> EvaluationRequest {
        EvaluationRequest {
            policy: Default::default(),
            input: PipelineRequest {
                factor_refs: Vec::new(),
                factor_set: Default::default(),
                market_path: "market".into(),
                universe_path: "pool".into(),
                universe_id: "test".into(),
                date_range: DateRange {
                    start: "2024-01-01".parse().unwrap(),
                    end: "2024-01-02".parse().unwrap(),
                },
                factor_ids: vec!["alpha_001".into()],
                output_dir: "output".into(),
                training: Default::default(),
            }
            .into(),
            horizon: 1,
            quantiles: 2,
            min_samples: 4,
        }
    }
    #[test]
    fn parquet_evaluation_preserves_membership_history_and_tail_nulls() {
        use analytics_core::{ExportFormat, ExportRequest, research_quality::FactorSet};
        use analytics_engine::QueryEngine;
        let dir = tempfile::tempdir().unwrap();
        let mut engine = DuckDbEngine::open_in_memory().unwrap();
        let market = dir.path().join("market.parquet");
        let pool = dir.path().join("pool.parquet");
        let market_sql = "SELECT 's' || s::VARCHAR AS instrument_id, DATE '2024-01-01' + d::INTEGER AS trade_date,
            100.0::DOUBLE AS open, (101.0+s+d)::DOUBLE AS high, 99.0::DOUBLE AS low,
            (100.0+s*d)::DOUBLE AS close, 1000.0::DOUBLE AS volume, 100000.0::DOUBLE AS amount,
            100.0::DOUBLE AS adj_open, (101.0+s+d)::DOUBLE AS adj_high, 99.0::DOUBLE AS adj_low,
            (100.0+s*d)::DOUBLE AS adj_close, true AS is_trading, DATE '2020-01-01' AS listing_date
            FROM range(1,7) a(s) CROSS JOIN range(0,7) b(d)";
        for (sql, path) in [
            (market_sql, &market),
            (
                "SELECT 's' || s::VARCHAR AS instrument_id, 'test' AS universe_id, DATE '2024-01-03' AS trade_date, true AS is_member FROM range(1,7) a(s)",
                &pool,
            ),
        ] {
            engine
                .export(ExportRequest {
                    sql: sql.into(),
                    destination: path.clone(),
                    format: ExportFormat::Parquet,
                    compression: None,
                })
                .unwrap();
        }
        let mut request = request();
        request.input.factor_set = FactorSet::Alpha158;
        request.input.factor_ids = vec!["alpha158_001_KMID".into()];
        request.input.market_path = market;
        request.input.universe_path = pool;
        request.input.output_dir = dir.path().join("result");
        request.input.date_range.end = "2024-01-07".parse().unwrap();
        let (tx, _) = channel();
        let report = run(&request, tx, cancellation()).unwrap();
        let factor = &report.factors[0];
        assert_eq!(factor.daily.len(), 5); // membership starts Jan 3; earlier rows only warm up
        assert_eq!(factor.daily[0].date.to_string(), "2024-01-03");
        assert_eq!(factor.daily.last().unwrap().paired, 0); // never reach beyond evaluation end
        assert_eq!(factor.valid_days, 4);
        assert!(factor.mean_rank_ic.unwrap() > 0.999);
        assert_eq!(report.factor_manifest["rows"], 42); // retains all six instruments' pre-entry history
        assert!(
            request
                .input
                .output_dir
                .join("factor-evaluation.json")
                .is_file()
        );
        let original_rows = engine
            .load_market_rows(
                &request.input.market_path,
                &request.input.universe_path,
                "test",
                &request.input.date_range,
            )
            .unwrap();
        assert_eq!(original_rows.len(), 30); // compatibility: old loader still returns only members
        // Shared worker publication persists a complete, independently reloadable task.
        let workspace = crate::Workspace::open(dir.path()).unwrap();
        let (worker, events) = crate::backend::spawn_in_workspace(
            crate::backend::Wake::default(),
            Some(dir.path().to_owned()),
        );
        let task = uuid::Uuid::new_v4();
        let serialized = serde_json::to_value(crate::service::TaskRequest::EvaluateFactors {
            request: Box::new(request.clone()),
        })
        .unwrap();
        assert_eq!(serialized["operation"], "evaluate_factors");
        let _: crate::service::TaskRequest = serde_json::from_value(serialized).unwrap();
        worker
            .send(crate::backend::WorkerCommand::EvaluateFactors {
                task,
                request: request.clone(),
                cancel: cancellation(),
            })
            .unwrap();
        let mut received = false;
        loop {
            match events
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap()
            {
                crate::backend::WorkerEvent::FactorsEvaluated { report, .. } => {
                    received = true;
                    assert_eq!(report.factors[0].valid_days, 4);
                }
                crate::backend::WorkerEvent::Failed { message, .. } => panic!("{message}"),
                crate::backend::WorkerEvent::Finished { .. } => break,
                _ => {}
            }
        }
        assert!(received);
        let records = workspace.runs().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, crate::RunStatus::Succeeded);
        assert_eq!(workspace.results(&records[0]).unwrap()["schema_version"], 3);
        let (tx, _) = channel();
        let flag = cancellation();
        flag.store(true, Ordering::Relaxed);
        assert!(matches!(
            run(&request, tx, flag),
            Err(PipelineError::Cancelled)
        ));
    }

    #[test]
    fn hand_calculated_positive_negative_null_and_constant_signals() {
        let r = request();
        let c = cancellation();
        let days = BTreeMap::from([(r.input.date_range.start, vec![0, 1, 2, 3, 4])]);
        let y = vec![Some(0.01), Some(0.02), Some(0.03), Some(0.04), None];
        let x = vec![Some(1.), Some(2.), Some(3.), Some(4.), None];
        let d = diagnose("x", &x, &y, &days, &r, &c).unwrap();
        assert!((d.mean_rank_ic.unwrap() - 1.).abs() < 1e-12);
        assert!((d.mean_spread.unwrap() - 0.02).abs() < 1e-12);
        assert_eq!(d.coverage, Some(0.8));
        assert_eq!(d.rank_ic_ir, None);
        let neg: Vec<_> = x.iter().map(|v| v.map(|v| -v)).collect();
        assert!(
            (diagnose("x", &neg, &y, &days, &r, &c)
                .unwrap()
                .mean_rank_ic
                .unwrap()
                + 1.)
                .abs()
                < 1e-12
        );
        let constant = diagnose("c", &[Some(1.); 5], &y, &days, &r, &c).unwrap();
        assert_eq!(constant.mean_ic, None);
        assert_eq!(constant.mean_spread, None);
        c.store(true, Ordering::Relaxed);
        assert!(matches!(
            diagnose("x", &x, &y, &days, &r, &c),
            Err(PipelineError::Cancelled)
        ));
    }
    #[test]
    fn ties_are_averaged_and_small_samples_not_reported_as_zero() {
        assert_eq!(ranks(&[2., 1., 2., 4.]), vec![1.5, 0., 1.5, 3.]);
        let r = request();
        let days = BTreeMap::from([(r.input.date_range.start, vec![0, 1])]);
        let d = diagnose(
            "x",
            &[Some(1.), Some(2.)],
            &[Some(0.1), Some(0.2)],
            &days,
            &r,
            &cancellation(),
        )
        .unwrap();
        assert_eq!(d.valid_days, 0);
        assert_eq!(d.mean_ic, None);
    }
}
