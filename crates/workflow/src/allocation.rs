//! Shared, strict portfolio allocation contracts. Native resources stay in the worker.
use crate::Result;
use serde::{Deserialize, Serialize};

/// Single authority for stock target exposure, also sent to the Python worker.
pub const STOCK_EXPOSURE: f64 = 0.98;

/// Auditable candidate admission, evaluated before ranking using realized prices only.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidatePolicy {
    /// Preserve the candidate universe; missing optimizer history blocks execution.
    #[default]
    Unfiltered,
    /// Admit only candidates with the complete configured risk window.
    CompleteRiskWindow,
}

/// Allocation objective; all methods use the same independent constraint checks.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AllocationMethod {
    /// Preserve the selected securities with equal weights.
    #[default]
    Equal,
    /// Minimize the holding-period covariance quadratic form.
    MinimumVariance,
    /// Equal risk budgets, subject to explicit constraints.
    RiskBudgeting,
    /// Expected return minus an explicitly configured variance penalty.
    MeanVariance,
}
/// Account allocation parameters. Missing wire fields are rejected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AllocationConfig {
    /// Explicit candidate admission; use the same policy for method comparisons.
    pub candidate_policy: CandidatePolicy,
    /// CLARABEL feasibility and absolute/relative gap tolerances.
    pub solver_tolerance: f64,
    /// First historical sample prediction date; null uses the schedule start.
    #[serde(deserialize_with = "explicit_option")]
    pub prediction_start: Option<analytics_core::Date>,
    /// First reference-account date; null uses the formal account start.
    #[serde(deserialize_with = "explicit_option")]
    pub reference_start: Option<analytics_core::Date>,
    /// Formal account start; earlier predictions are warmup only.
    #[serde(deserialize_with = "explicit_option")]
    pub execution_start: Option<analytics_core::Date>,
    /// Objective.
    pub method: AllocationMethod,
    /// Complete realized holding-period observations.
    pub risk_window: usize,
    /// Per-security target cap as a fraction of account NAV.
    pub max_stock_weight: f64,
    /// Per-strategy capital cap.
    pub max_strategy_weight: f64,
    /// L1 security turnover, including exiting securities.
    #[serde(deserialize_with = "explicit_option")]
    pub max_turnover: Option<f64>,
    /// Required for mean/variance; never inferred from model scores.
    #[serde(deserialize_with = "explicit_option")]
    pub risk_aversion: Option<f64>,
    /// Rolling realized calibration dates.
    pub calibration_window: usize,
    /// Required complete cross sections.
    pub calibration_min_days: usize,
    /// Required paired securities per calibration date.
    pub calibration_min_pairs: usize,
}
fn explicit_option<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl Default for AllocationConfig {
    fn default() -> Self {
        Self {
            candidate_policy: CandidatePolicy::Unfiltered,
            solver_tolerance: 1e-9,
            prediction_start: None,
            reference_start: None,
            execution_start: None,
            method: AllocationMethod::Equal,
            risk_window: 252,
            max_stock_weight: 0.1,
            max_strategy_weight: 0.6,
            max_turnover: None,
            risk_aversion: None,
            calibration_window: 252,
            calibration_min_days: 60,
            calibration_min_pairs: 30,
        }
    }
}
impl AllocationConfig {
    /// Validate without loading data. Actual candidate feasibility is checked separately.
    pub fn validate(&self) -> Result<()> {
        if self
            .prediction_start
            .zip(self.reference_start)
            .is_some_and(|(a, b)| a > b)
            || self
                .reference_start
                .zip(self.execution_start)
                .is_some_and(|(a, b)| a > b)
        {
            return Err("allocation_warmup_order_invalid".into());
        }
        if !(2..=10000).contains(&self.risk_window)
            || !self.solver_tolerance.is_finite()
            || !(1e-12..=1e-6).contains(&self.solver_tolerance)
            || !(2..=10000).contains(&self.calibration_window)
            || self.calibration_min_days < 2
            || self.calibration_min_days > self.calibration_window
            || self.calibration_min_pairs < 3
            || [self.max_stock_weight, self.max_strategy_weight]
                .iter()
                .any(|v| !v.is_finite() || *v <= 0. || *v > 1.)
            || self
                .max_turnover
                .is_some_and(|v| !v.is_finite() || !(0.0..=2.0).contains(&v))
            || self
                .risk_aversion
                .is_some_and(|v| !v.is_finite() || v <= 0.)
            || (self.method == AllocationMethod::MeanVariance && self.risk_aversion.is_none())
        {
            return Err("invalid_allocation_config".into());
        }
        Ok(())
    }
    /// Reject structurally impossible stock targets before expensive work.
    pub fn validate_count(&self, count: usize) -> Result<()> {
        self.validate()?;
        if count > 0 && count as f64 * self.max_stock_weight < STOCK_EXPOSURE - 1e-6 {
            return Err(
                format!("allocation_infeasible: candidate count * cap < {STOCK_EXPOSURE}").into(),
            );
        }
        Ok(())
    }
}

/// Separate code and dependency identities for calibration, optimization and execution.
pub fn component_signatures() -> serde_json::Value {
    use sha2::{Digest, Sha256};
    let digest = |parts: &[&[u8]]| {
        let mut h = Sha256::new();
        for part in parts {
            h.update(part);
            h.update([0]);
        }
        format!("sha256:{:x}", analytics_core::hash::Hex(h.finalize()))
    };
    let numerical = include_bytes!("../research/jfquant_research/allocation.py");
    let dependencies = include_bytes!("../research/requirements.lock");
    serde_json::json!({
        "calibration":digest(&[b"percentile-equal-date-intercept-ols-v1", numerical, include_bytes!("research/backtest/allocation_readiness.rs"), include_bytes!("research/backtest/allocation_admission.rs"), dependencies]),
        "optimization":digest(&[b"allocation-constrained-original-output-v1", numerical, include_bytes!("../research/jfquant_research/combined.py"), include_bytes!("allocation.rs"), include_bytes!("research/backtest/allocation_admission.rs"), include_bytes!("research/backtest/combined.rs"), dependencies]),
        "execution":digest(&[include_bytes!("allocation_reference.rs"), include_bytes!("persistence.rs"), include_bytes!("../research/rust-execution-dependencies.json"), include_bytes!("research/backtest/akquant.rs"), include_bytes!("../research/jfquant_research/backtest.py"), include_bytes!("../research/akquant-source.lock.json"), include_bytes!("research/worker.rs"), include_bytes!("research/backtest/account.rs"), dependencies])
    })
}

const ARTIFACT_FILES: &[&str] = &[
    "allocation-input.json",
    "allocation_report.json",
    "reference-input.json",
    "reference-account.json",
    "research-request.json",
    "reference-targets.json",
    "reference-event-checks.json",
    "allocation-admission.json",
    "allocation-decisions.jsonl",
];
fn file_hash(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(std::fs::read(path)?))
    ))
}
/// Bind all allocation and reference files to the final execution signature.
pub fn publish_artifacts(root: &std::path::Path) -> Result<()> {
    let mut hashes = ARTIFACT_FILES
        .iter()
        .map(|name| Ok((name.to_string(), file_hash(&root.join(name))?)))
        .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
    if root.join("combined-input.json").is_file() {
        hashes.insert(
            "combined-input.json".into(),
            file_hash(&root.join("combined-input.json"))?,
        );
    }
    crate::atomic_json(
        &root.join("allocation-manifest.json"),
        &serde_json::json!({"schema_version":2,"files":hashes}),
    )?;
    verify_artifacts(root)
}
/// Independent Rust verification of original solver output and temporal boundaries.
pub fn verify_artifacts(root: &std::path::Path) -> Result<()> {
    use serde_json::Value;
    let read = |name: &str| -> Result<Value> {
        Ok(serde_json::from_slice(&std::fs::read(root.join(name))?)?)
    };
    let manifest = read("allocation-manifest.json")?;
    if manifest["schema_version"] != 2 {
        return Err("allocation_manifest_format".into());
    }
    for name in ARTIFACT_FILES {
        if manifest["files"][*name] != file_hash(&root.join(name))? {
            return Err(format!("allocation_artifact_changed:{name}").into());
        }
    }
    let combined_input = if root.join("combined-input.json").exists()
        || manifest["files"].get("combined-input.json").is_some()
    {
        if manifest["files"]["combined-input.json"] != file_hash(&root.join("combined-input.json"))?
        {
            return Err("allocation_combined_input_changed".into());
        }
        Some(read("combined-input.json")?)
    } else {
        None
    };
    let input = read("allocation-input.json")?;
    if input["schema_version"] != 2 || input["stock_exposure"].as_f64() != Some(STOCK_EXPOSURE) {
        return Err("allocation_input_contract_invalid".into());
    }
    let journal = std::fs::read_to_string(root.join("allocation-decisions.jsonl"))?;
    let calendar: Vec<analytics_core::Date> = serde_json::from_value(input["calendar"].clone())?;
    if calendar.is_empty() || calendar.windows(2).any(|w| w[0] >= w[1]) {
        return Err("allocation_calendar_invalid".into());
    }
    let report = read("allocation_report.json")?;
    let config: AllocationConfig = serde_json::from_value(report["config"].clone())?;
    config.validate()?;
    if input["config"] != report["config"] {
        return Err("allocation_config_changed".into());
    }
    if report["schema_version"] != 2 || report["status"] != "completed" {
        return Err("allocation_report_incomplete".into());
    }
    let targets = report["targets"]
        .as_array()
        .ok_or("allocation_targets_missing")?;
    let decisions = report["decisions"]
        .as_array()
        .ok_or("allocation_decisions_missing")?;
    let journal: Vec<Value> = journal
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    if journal.len() != decisions.len()
        || journal
            .iter()
            .zip(decisions.iter().zip(targets))
            .any(|(r, (d, t))| r["decision"] != *d || r["target"] != *t)
    {
        return Err("allocation_decision_journal_mismatch".into());
    }
    if targets.len() != decisions.len() {
        return Err("allocation_decision_target_count".into());
    }
    let mut previous = None;
    for (target, decision) in targets.iter().zip(decisions) {
        let date: analytics_core::Date = serde_json::from_value(target["date"].clone())?;
        if previous.is_some_and(|d| d >= date)
            || decision["date"] != target["date"]
            || decision["effective_date"] != target["effective_date"]
        {
            return Err("allocation_dates_invalid".into());
        }
        previous = Some(date);
        let day = calendar
            .binary_search(&date)
            .map_err(|_| "allocation_decision_off_calendar")?;
        let expected_date = calendar.get(day + 1).map(ToString::to_string);
        if target["effective_date"].as_str() != expected_date.as_deref() {
            return Err("allocation_execution_not_next_market_open".into());
        }
        if decision["method"] != serde_json::to_value(config.method)?
            || !matches!(
                decision["status"].as_str(),
                Some("analytic" | "optimal" | "optimal_inaccurate" | "capital_drift")
            )
        {
            return Err("allocation_solver_status_or_method_invalid".into());
        }
        if !target["effective_date"].is_null() {
            let next: analytics_core::Date =
                serde_json::from_value(target["effective_date"].clone())?;
            if next <= date {
                return Err("allocation_future_execution_invalid".into());
            }
        }
        for bound in [&decision["risk_history"], &decision["calibration"]] {
            if !bound.is_null() {
                let end: analytics_core::Date =
                    serde_json::from_value(bound["last_return_date"].clone())?;
                if end > date {
                    return Err("allocation_future_input".into());
                }
            }
        }
        for record in decision["calibration"]["date_records"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let score: analytics_core::Date = serde_json::from_value(record["score_date"].clone())?;
            let realized: analytics_core::Date =
                serde_json::from_value(record["return_end"].clone())?;
            if record["used"] == false {
                if realized > date {
                    return Err("allocation_calibration_time_contract_invalid".into());
                }
                continue;
            }
            let cutoff: analytics_core::Date =
                serde_json::from_value(record["model"]["fit_cutoff"].clone())?;
            if cutoff >= score || realized > date {
                return Err("allocation_calibration_time_contract_invalid".into());
            }
        }
        let weights = target["weights"]
            .as_object()
            .ok_or("allocation_weights_missing")?;
        if combined_input.is_none() && decision["instruction_kind"] != "pool_exit" {
            verify_candidate_receipt(&input, decision, &config, date)?;
        }
        let mut sum = 0.;
        for (s, v) in weights {
            analytics_core::pool::validate_instrument(s)?;
            let w = v
                .as_f64()
                .filter(|w| w.is_finite() && *w >= -1e-6 && *w <= config.max_stock_weight + 1e-6)
                .ok_or("allocation_weight_or_cap_invalid")?;
            sum += w;
        }
        let combined = decision.get("capital_kind").is_some();
        if sum > crate::allocation::STOCK_EXPOSURE + 1e-6
            || (!combined
                && decision["instruction_kind"] != "pool_exit"
                && !weights.is_empty()
                && (sum - crate::allocation::STOCK_EXPOSURE).abs() > 1e-6)
        {
            return Err("allocation_budget_invalid".into());
        }
        let snapshot = &decision["actual_snapshot"];
        let equity = snapshot["equity"]
            .as_f64()
            .filter(|v| v.is_finite() && *v > 0.)
            .ok_or("allocation_snapshot_equity_invalid")?;
        let positions = snapshot["positions"]
            .as_object()
            .ok_or("allocation_snapshot_positions_missing")?;
        let names = weights
            .keys()
            .chain(positions.keys())
            .collect::<std::collections::BTreeSet<_>>();
        let mut turnover = 0.;
        for s in names {
            let q = positions.get(s).and_then(Value::as_f64).unwrap_or(0.);
            let held = if q == 0. {
                0.
            } else {
                q * snapshot["marks"][s]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v > 0.)
                    .ok_or("allocation_snapshot_mark_invalid")?
                    / equity
            };
            turnover += (weights.get(s).and_then(Value::as_f64).unwrap_or(0.) - held).abs();
        }
        let reported_turnover = decision["constraints"]["turnover"]
            .as_f64()
            .filter(|v| v.is_finite())
            .ok_or("allocation_turnover_receipt_missing")?;
        if (reported_turnover - turnover).abs() > 1e-6 {
            return Err("allocation_turnover_receipt_mismatch".into());
        }
        if !combined {
            let names = decision["names"]
                .as_array()
                .ok_or("allocation_identities_missing")?;
            let values = decision["weights"]
                .as_array()
                .ok_or("allocation_solver_weights_missing")?;
            if names.len() != weights.len() || values.len() != names.len() {
                return Err("allocation_solver_target_shape_mismatch".into());
            }
            let mut seen = std::collections::BTreeSet::new();
            for (name, value) in names.iter().zip(values) {
                let name = name.as_str().ok_or("allocation_identity_invalid")?;
                let value = value
                    .as_f64()
                    .filter(|v| v.is_finite())
                    .ok_or("allocation_solver_weight_invalid")?;
                if !seen.insert(name)
                    || weights
                        .get(name)
                        .and_then(Value::as_f64)
                        .is_none_or(|w| (w - value).abs() > 1e-6)
                {
                    return Err("allocation_solver_target_mismatch".into());
                }
            }
        }
        if config.max_turnover.is_some_and(|cap| turnover > cap + 1e-6) {
            return Err("allocation_turnover_invalid".into());
        }
        if combined_input.is_some() != combined {
            return Err("allocation_account_kind_mismatch".into());
        }
        if let Some(payload) = &combined_input {
            verify_combined_net(payload, decision, target)?;
        }
        if combined {
            let a = decision["weights"]
                .as_array()
                .ok_or("allocation_capital_missing")?;
            let mut total = 0.;
            for v in a {
                let w = v
                    .as_f64()
                    .filter(|w| w.is_finite() && *w >= -1e-6)
                    .ok_or("allocation_capital_invalid")?;
                if decision["capital_kind"] == "optimized" && w > config.max_strategy_weight + 1e-6
                {
                    return Err("allocation_capital_cap_invalid".into());
                }
                total += w;
            }
            if (total - 1.).abs() > 1e-6 {
                return Err("allocation_capital_budget_invalid".into());
            }
        }
    }
    Ok(())
}

fn verify_combined_net(
    payload: &serde_json::Value,
    decision: &serde_json::Value,
    target: &serde_json::Value,
) -> Result<()> {
    use serde_json::Value;
    let sources = payload["sources"]
        .as_array()
        .ok_or("allocation_sources_missing")?;
    let names = decision["names"]
        .as_array()
        .ok_or("allocation_strategy_names_missing")?;
    let capital = decision["weights"]
        .as_array()
        .ok_or("allocation_capital_missing")?;
    let symbols = decision["underlying_symbols"]
        .as_array()
        .ok_or("allocation_stock_names_missing")?;
    let mapping = decision["mapping"]
        .as_array()
        .ok_or("allocation_mapping_missing")?;
    if sources.len() != names.len()
        || capital.len() != names.len()
        || symbols.len() != mapping.len()
        || target["weights"]
            .as_object()
            .ok_or("allocation_weights_missing")?
            .len()
            != symbols.len()
    {
        return Err("allocation_combined_shape_invalid".into());
    }
    let finite = |v: &Value| {
        v.as_f64()
            .filter(|v| v.is_finite())
            .ok_or("allocation_mapping_nonfinite")
    };
    let mut recipes = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        if source["id"] != names[index] {
            return Err("allocation_strategy_order_changed".into());
        }
        let row = source["reference"]["rows"]
            .as_array()
            .ok_or("allocation_reference_rows_missing")?
            .iter()
            .find(|r| r["date"] == decision["date"])
            .ok_or("allocation_reference_date_missing")?;
        if (finite(&row["nav"])? - finite(&decision["reference_nav"][index])?).abs() > 1e-6 {
            return Err("allocation_reference_nav_changed".into());
        }
        let new_target = source["targets"]
            .as_array()
            .ok_or("allocation_source_targets_missing")?
            .iter()
            .find(|t| t["date"] == decision["date"]);
        recipes.push(new_target.map(|t| &t["weights"]).unwrap_or(&row["weights"]));
    }
    let mut seen = std::collections::BTreeSet::new();
    for (row_index, (symbol, row)) in symbols.iter().zip(mapping).enumerate() {
        let symbol = symbol.as_str().ok_or("allocation_stock_identity_invalid")?;
        if !seen.insert(symbol) {
            return Err("allocation_stock_identity_duplicate".into());
        }
        let row = row
            .as_array()
            .filter(|r| r.len() == capital.len())
            .ok_or("allocation_mapping_shape_invalid")?;
        let mut net = 0.;
        for (index, value) in row.iter().enumerate() {
            let value = finite(value)?;
            let expected = recipes[index]
                .get(symbol)
                .map(finite)
                .transpose()?
                .unwrap_or(0.);
            if value < -1e-6 || (value - expected).abs() > 1e-6 {
                return Err("allocation_source_contribution_changed".into());
            }
            net += value * finite(&capital[index])?;
        }
        if (finite(&target["weights"][symbol])? - net).abs() > 1e-6
            || (finite(&decision["underlying_weights"][row_index])? - net).abs() > 1e-6
        {
            return Err("allocation_net_target_mismatch".into());
        }
    }
    Ok(())
}

fn verify_candidate_receipt(
    input: &serde_json::Value,
    decision: &serde_json::Value,
    config: &AllocationConfig,
    date: analytics_core::Date,
) -> Result<()> {
    if decision["eligibility"]["policy"] != serde_json::to_value(config.candidate_policy)?
        || decision["eligibility"]["as_of"] != date.to_string()
    {
        return Err("allocation_candidate_receipt_invalid".into());
    }
    if config.candidate_policy != CandidatePolicy::CompleteRiskWindow {
        return Ok(());
    }
    let allowed = input["admission"]["eligibility"][date.to_string()]["eligible"]
        .as_array()
        .ok_or("allocation_eligible_dates_missing")?;
    let calendar: Vec<analytics_core::Date> = serde_json::from_value(input["calendar"].clone())?;
    let day = calendar
        .binary_search(&date)
        .map_err(|_| "allocation_candidate_date_invalid")?;
    let h = input["horizon"]
        .as_u64()
        .ok_or("allocation_horizon_missing")? as usize;
    let start = day
        .checked_sub(config.risk_window + h)
        .ok_or("allocation_candidate_warmup_missing")?;
    for symbol in decision["names"]
        .as_array()
        .ok_or("allocation_candidate_names_missing")?
    {
        if !allowed.contains(symbol) {
            return Err("allocation_candidate_not_eligible".into());
        }
        let symbol = symbol
            .as_str()
            .ok_or("allocation_candidate_identity_invalid")?;
        for i in start..start + config.risk_window {
            for endpoint in [i + 1, i + 1 + h] {
                if input["prices"][symbol][calendar[endpoint].to_string()]
                    .as_f64()
                    .is_none_or(|v| !v.is_finite() || v <= 0.)
                {
                    return Err("allocation_candidate_history_incomplete".into());
                }
            }
        }
    }
    Ok(())
}

/// Bounded presentation data; complete evidence remains in the named task artifacts.
pub fn display(root: &std::path::Path) -> Result<serde_json::Value> {
    use serde_json::{Value, json};
    let report: Value =
        serde_json::from_slice(&std::fs::read(root.join("allocation_report.json"))?)?;
    let reference: Value =
        serde_json::from_slice(&std::fs::read(root.join("reference-account.json"))?)?;
    let decisions = report["decisions"]
        .as_array()
        .ok_or("allocation_decisions_missing")?;
    let rows = reference["rows"]
        .as_array()
        .ok_or("reference_rows_missing")?;
    Ok(
        json!({"config":report["config"],"decision_count":decisions.len(),"display_limit":100,"decisions":decisions.iter().rev().take(100).map(|d|json!({"date":d["date"],"status":d["status"],"capital_kind":d["capital_kind"],"names":d["names"],"weights":d["weights"],"actual_equity_cny":d["actual_snapshot"]["equity"],"actual_positions":d["actual_snapshot"]["positions"],"actual_marks":d["actual_snapshot"]["marks"],"mapping":d["mapping"],"reference_nav":d["reference_nav"],"eligibility":d["eligibility"],"risk_contributions":d["risk_contributions"],"risk_budget_deviation":d["risk_budget_deviation"],"warnings":d["warnings"],"constraints":d["constraints"],"underlying_symbols":d["underlying_symbols"],"underlying_weights":d["underlying_weights"]})).collect::<Vec<_>>(),"reference_rows":rows.iter().rev().take(100).map(|r|json!({"date":r["date"],"nav":r["nav"],"cash":r["cash"],"locked_value":r["locked_value"],"pending":r["pending"]})).collect::<Vec<_>>(),"reference_basis":"fractional zero-fee reference, not realized account profit","files":["allocation_report.json","reference-account.json","allocation-manifest.json"]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn impossible_caps_and_missing_risk_preference_are_rejected() {
        let mut c = AllocationConfig::default();
        assert!(c.validate_count(5).is_err());
        assert!(c.validate_count(10).is_ok());
        c.method = AllocationMethod::MeanVariance;
        assert!(c.validate().is_err());
        c.risk_aversion = Some(10.);
        assert!(c.validate().is_ok());
        assert!(serde_json::from_str::<AllocationConfig>("{}").is_err());
        for field in [
            "prediction_start",
            "reference_start",
            "execution_start",
            "max_turnover",
            "risk_aversion",
        ] {
            let mut value = serde_json::to_value(&c).unwrap();
            value.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<AllocationConfig>(value).is_err(),
                "{field}"
            );
        }
    }
}
