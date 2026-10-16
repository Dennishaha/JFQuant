//! Read-only consistency audit of published research artifacts, without Python or native handles.
use super::{portfolio, *};
use crate::{RunRecord, RunStatus, Workspace};
use serde_json::{Value, json};
use uuid::Uuid;

fn read(root: &Path, name: &str) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(root.join(name))?)?)
}
fn number(value: &Value) -> Result<f64> {
    value
        .as_f64()
        .filter(|v| v.is_finite())
        .ok_or_else(|| "verification_nonfinite_or_missing_number".into())
}
fn equal(actual: f64, expected: f64) -> Result<()> {
    if !actual.is_finite()
        || !expected.is_finite()
        || (actual - expected).abs() > 1e-8_f64.max(expected.abs() * 1e-10)
    {
        return Err(
            format!("verification_numeric_mismatch: actual={actual} expected={expected}").into(),
        );
    }
    Ok(())
}
fn json_equal(actual: &Value, expected: &Value) -> Result<()> {
    match (actual, expected) {
        (Value::Number(_), Value::Number(_)) => equal(number(actual)?, number(expected)?),
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (a, b) in a.iter().zip(b) {
                json_equal(a, b)?;
            }
            Ok(())
        }
        (Value::Object(a), Value::Object(b)) if a.len() == b.len() => {
            for (k, v) in a {
                json_equal(v, b.get(k).ok_or("verification_copy_field_missing")?)?;
            }
            Ok(())
        }
        _ if actual == expected => Ok(()),
        _ => Err("verification_copy_mismatch".into()),
    }
}
fn performance(value: &Value, days: &[portfolio::DayResult]) -> Result<()> {
    let expected = serde_json::to_value(portfolio::performance(days))?;
    for (key, want) in expected.as_object().unwrap() {
        if want.is_number() {
            equal(number(&value[key])?, number(want)?)?;
        } else if value.get(key) != Some(want) {
            return Err(format!("verification_performance_mismatch:{key}").into());
        }
    }
    Ok(())
}

/// Audit a successful native task's frozen request, result signature, daily returns,
/// snapshots, fees, event exposure and the prohibition on valuation-only fills.
/// This checks consistency, not complete event coverage or absence of sample bias.
/// It never rewrites a task, changes its state, or starts a Python process.
pub fn report(workspace: &Workspace, task_id: Uuid) -> Result<Value> {
    let root = workspace.resolve(format!(".jfquant/runs/{task_id}"));
    let run: RunRecord = serde_json::from_value(read(&root, "run.json")?)?;
    if run.id != task_id
        || run.output != Path::new(&format!(".jfquant/runs/{task_id}"))
        || run.read_only
        || run.status != RunStatus::Succeeded
        || crate::signature(&run.request)? != run.signature
    {
        return Err("verification_requires_completed_native_task".into());
    }
    analytics_core::source_map::reject_removed_sources(&run.request)?;
    let summary = read(&root, "summary.json")?;
    validate_result(&summary)?;
    if !matches!(
        summary["backtest_status"].as_str(),
        Some("completed" | "no_valid_trades")
    ) || crate::signature(&(
        summary["factor_signature"]
            .as_str()
            .ok_or("verification_factor_signature_missing")?,
        &summary["execution"],
    ))? != summary["signature"]
    {
        return Err("verification_publication_mismatch".into());
    }
    json_equal(&read(&root, "outcome.json")?["details"], &summary)?;
    if summary["execution"]["accounting_version"] == "rust-filled-trade-ledger-v1"
        && root.join("rights_input.json").exists()
    {
        return Err("unexpected_rights_input_for_legacy_account".into());
    }
    if !matches!(
        summary["execution"]["accounting_version"].as_str(),
        Some("rust-filled-trade-ledger-v1" | "rust-filled-trade-ledger-rights-v2")
    ) {
        return Err("verification_requires_rust_closing_ledger: rerun backtest; historical files are unchanged".into());
    }
    if summary["execution"]["accounting_version"] == "rust-filled-trade-ledger-rights-v2" {
        if summary["execution"]["rights_input_sha256"]
            != super::file_hash(&root.join("rights_input.json"))?
        {
            return Err("rights_projection_hash_mismatch".into());
        }
        let input: analytics_core::rights_account::Ledger =
            serde_json::from_slice(&fs::read(root.join("rights_input.json"))?)?;
        input.validate()?;
        if serde_json::to_value(&input.policy)? != summary["execution"]["rights_policy"] {
            return Err("rights_policy_mismatch".into());
        }
    }
    let snapshots: BTreeMap<Date, Value> =
        serde_json::from_value(read(&root, "portfolio_snapshots.json")?)?;
    let days: Vec<portfolio::DayResult> = serde_json::from_value(read(&root, "daily.json")?)?;
    let calendar = days
        .iter()
        .map(|d| d.date.parse::<Date>())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut closes: BTreeMap<String, Vec<Option<f64>>> = BTreeMap::new();
    for (index, date) in calendar.iter().enumerate() {
        for (id, price) in
            snapshots.get(date).ok_or("verification_snapshot_missing")?["valuation_prices"]
                .as_object()
                .ok_or("verification_valuation_prices_missing")?
        {
            closes
                .entry(id.clone())
                .or_insert_with(|| vec![None; calendar.len()])[index] = Some(number(price)?);
        }
    }
    let actions = read(&root, "execution_actions.json")?;
    if actions["schema_version"] != 1 {
        return Err("verification_action_projection_format".into());
    }
    let range = DateRange {
        start: *calendar.first().ok_or("verification_empty_account")?,
        end: *calendar.last().unwrap(),
    };
    let benchmark = days
        .iter()
        .map(|d| (d.date.clone(), d.benchmark_return))
        .collect();
    let replay = super::account::reconcile(
        &root,
        &calendar,
        &range,
        number(&summary["execution"]["initial_cash_cny"])?,
        &closes,
        actions["actions"]
            .as_array()
            .ok_or("verification_actions_missing")?,
        &benchmark,
    )?;
    json_equal(&json!(replay.snapshots), &json!(snapshots))?;
    json_equal(&json!(replay.days), &json!(days))?;
    json_equal(
        &read(&root, "engine_curve_differences.json")?,
        &json!(replay.differences),
    )?;
    let mut report = artifacts(&root, &summary)?;
    report["engine_curve_difference_days"] = json!(replay.differences.len());
    let names = [
        "run.json",
        "outcome.json",
        "summary.json",
        "execution.json",
        "daily.json",
        "portfolio_snapshots.json",
        "order_events.jsonl",
        "event_exposure_checks.json",
        "suspension_valuation_marks.json",
        "fold_reports.json",
        "annual_reports.json",
        "native_account_daily.json",
        "execution_actions.json",
        "engine_curve_differences.json",
    ];
    let hashes = names
        .into_iter()
        .map(|name| {
            Ok((
                name,
                format!(
                    "sha256:{:x}",
                    analytics_core::hash::Hex(Sha256::digest(fs::read(root.join(name))?))
                ),
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    report["task_id"] = json!(task_id);
    report["artifact_sha256"] = json!(hashes);
    Ok(report)
}

fn artifacts(root: &Path, summary: &Value) -> Result<Value> {
    if read(root, "execution.json")? != summary["execution"] {
        return Err("verification_execution_mismatch".into());
    }
    let days: Vec<portfolio::DayResult> = serde_json::from_value(read(root, "daily.json")?)?;
    if days.is_empty() || days.windows(2).any(|d| d[0].date >= d[1].date) {
        return Err("verification_daily_order_or_empty".into());
    }
    let dates = days
        .iter()
        .map(|d| d.date.parse::<Date>())
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    if summary["execution"]["accounting_version"] == "rust-filled-trade-ledger-rights-v2" {
        if summary["execution"]["rights_input_sha256"]
            != super::file_hash(&root.join("rights_input.json"))?
        {
            return Err("rights_projection_hash_mismatch".into());
        }
        let input: analytics_core::rights_account::Ledger =
            serde_json::from_slice(&fs::read(root.join("rights_input.json"))?)?;
        input.validate()?;
        if serde_json::to_value(&input.policy)? != summary["execution"]["rights_policy"] {
            return Err("rights_policy_mismatch".into());
        }
    }
    let snapshots: BTreeMap<Date, Value> =
        serde_json::from_value(read(root, "portfolio_snapshots.json")?)?;
    if snapshots.keys().copied().collect::<BTreeSet<_>>() != dates {
        return Err("verification_snapshot_dates_mismatch".into());
    }
    let mut previous = number(&summary["execution"]["initial_cash_cny"])?;
    if previous <= 0. {
        return Err("verification_initial_cash_invalid".into());
    }
    for day in &days {
        for v in [
            day.net_return,
            day.benchmark_return,
            day.turnover,
            day.cost,
            day.execution_fee_cny,
        ] {
            if !v.is_finite() {
                return Err("verification_daily_nonfinite".into());
            }
        }
        let snapshot = &snapshots[&day.date.parse::<Date>()?];
        number(&snapshot["cash"])?;
        let positions = snapshot["positions"]
            .as_object()
            .ok_or("verification_positions_missing")?;
        for (id, qty) in positions {
            analytics_core::pool::validate_instrument(id)?;
            number(qty)?;
        }
        if snapshot["holdings"].as_u64() != Some(day.holdings as u64)
            || positions
                .values()
                .filter(|v| v.as_f64() != Some(0.))
                .count()
                != day.holdings
        {
            return Err("verification_holdings_mismatch".into());
        }
        if day.net_return < -1.
            || day.benchmark_return < -1.
            || day.execution_fee_cny < 0.
            || day.turnover < 0.
            || day.cost < 0.
        {
            return Err("verification_daily_invalid".into());
        }
        let equity = number(&snapshot["equity"])?;
        equal(equity, previous * (1. + day.net_return))?;
        equal(day.cost, day.execution_fee_cny / previous)?;
        previous = equity;
    }
    performance(&summary["aggregate"], &days)?;
    for (name, key) in [
        ("fold_reports.json", "fold_reports"),
        ("annual_reports.json", "annual_reports"),
    ] {
        let reports = read(root, name)?;
        json_equal(&reports, &summary[key])?;
        for report in reports.as_array().ok_or("verification_reports_missing")? {
            let p = &report["performance"];
            let start = p["start"]
                .as_str()
                .ok_or("verification_report_start_missing")?;
            let end = p["end"].as_str().ok_or("verification_report_end_missing")?;
            let subset = days
                .iter()
                .filter(|d| d.date.as_str() >= start && d.date.as_str() <= end)
                .cloned()
                .collect::<Vec<_>>();
            performance(p, &subset)?;
        }
    }
    let marks = read(root, "suspension_valuation_marks.json")?;
    let mut marked = BTreeSet::new();
    let mut held_marks = 0;
    for mark in marks.as_array().ok_or("verification_marks_missing")? {
        let date: Date = mark["date"]
            .as_str()
            .ok_or("verification_mark_date_missing")?
            .parse()?;
        let id = mark["instrument"]
            .as_str()
            .ok_or("verification_mark_instrument_missing")?;
        analytics_core::pool::validate_instrument(id)?;
        if !dates.contains(&date)
            || number(&mark["mark"])? <= 0.
            || number(&mark["volume"])? != 0.
            || !marked.insert((date, id.to_owned()))
        {
            return Err("verification_mark_invalid".into());
        }
        held_marks += usize::from(
            snapshots[&date]["positions"][id]
                .as_f64()
                .is_some_and(|q| q != 0.),
        );
    }
    let mut fees: BTreeMap<Date, f64> = BTreeMap::new();
    let mut filled = 0;
    for line in fs::read_to_string(root.join("order_events.jsonl"))?.lines() {
        let order: Value = serde_json::from_str(line)?;
        let date: Date = order["date"]
            .as_str()
            .ok_or("verification_order_date_missing")?
            .parse()?;
        let id = order["instrument"]
            .as_str()
            .ok_or("verification_order_instrument_missing")?;
        analytics_core::pool::validate_instrument(id)?;
        let qty = number(&order["filled_quantity"])?;
        let fee = number(&order["fee"])?;
        if !dates.contains(&date) || qty < 0. || fee < 0. || (qty == 0. && fee != 0.) {
            return Err("verification_order_invalid".into());
        }
        if qty > 0. {
            if number(&order["price"])? <= 0. || marked.contains(&(date, id.to_owned())) {
                return Err("verification_valuation_only_fill".into());
            }
            filled += 1;
        }
        *fees.entry(date).or_default() += fee;
    }
    for day in &days {
        equal(
            *fees.get(&day.date.parse()?).unwrap_or(&0.),
            day.execution_fee_cny,
        )?;
    }
    let exposure = read(root, "event_exposure_checks.json")?;
    let checks = exposure
        .as_array()
        .ok_or("verification_exposure_checks_missing")?;
    let mut reference_exposure = 0;
    for check in checks {
        let qty = number(&check["opening_quantity"])?;
        let reference = check["code"] == "tdx_reference_precision_difference"
            && summary["execution"]["event_check_policy"] == "provider";
        if check["status"] != "unexposed_or_reference_only" || (qty != 0. && !reference) {
            return Err("verification_unresolved_event_exposure".into());
        }
        reference_exposure += usize::from(qty != 0. && reference);
    }
    let skipped = summary["skipped_events"]
        .as_array()
        .ok_or("verification_skipped_events_missing")?;
    if summary["skipped_event_count"].as_u64() != Some(skipped.len() as u64) {
        return Err("verification_skipped_count_mismatch".into());
    }
    Ok(
        json!({"schema_version":1,"status":"consistent","aggregate":summary["aggregate"],"daily_rows":days.len(),"ending_equity_cny":previous,"filled_order_events":filled,"valuation_mark_rows":marked.len(),"held_valuation_mark_rows":held_marks,"fills_on_valuation_only_rows":0,"exposure_checks":checks.len(),"reference_precision_checks_with_exposure":reference_exposure,"skipped_events":skipped.len(),"skipped_events_with_exposure":skipped.iter().filter(|s|s["held_across_event"]==true).count(),"event_simulation_complete":summary["event_simulation_complete"],"research_ready":null,"scope":"artifact-consistency-only;not-complete-event-coverage-or-unbiased-performance"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(root: &Path, name: &str, value: &Value) {
        fs::write(root.join(name), serde_json::to_vec(value).unwrap()).unwrap();
    }
    fn fixture() -> (tempfile::TempDir, Value) {
        let root = tempfile::tempdir().unwrap();
        let days = json!([
            {"date":"2021-01-04","net_return":-0.01,"benchmark_return":0.02,"turnover":0.1,"cost":0.01,"execution_fee_cny":1.,"holdings":1,"blocked_orders":0},
            {"date":"2021-01-05","net_return":0.,"benchmark_return":-0.01,"turnover":0.,"cost":0.,"execution_fee_cny":0.,"holdings":1,"blocked_orders":0}
        ]);
        let parsed: Vec<portfolio::DayResult> = serde_json::from_value(days.clone()).unwrap();
        let aggregate = serde_json::to_value(portfolio::performance(&parsed)).unwrap();
        let reports = json!([{"performance":aggregate}]);
        let summary = json!({"execution":{"initial_cash_cny":100.,"event_check_policy":"provider"},"aggregate":aggregate,"annual_reports":reports,"fold_reports":reports,"skipped_events":[{"held_across_event":true}],"skipped_event_count":1,"event_simulation_complete":false});
        for (name, value) in [
            ("daily.json", days),
            ("execution.json", summary["execution"].clone()),
            ("annual_reports.json", reports.clone()),
            ("fold_reports.json", reports),
            (
                "portfolio_snapshots.json",
                json!({"2021-01-04":{"positions":{"SH600000":1.},"holdings":1,"cash":89.,"equity":99.},"2021-01-05":{"positions":{"SH600000":1.},"holdings":1,"cash":89.,"equity":99.}}),
            ),
            (
                "suspension_valuation_marks.json",
                json!([{"date":"2021-01-05","instrument":"SH600000","mark":10.,"volume":0}]),
            ),
            (
                "event_exposure_checks.json",
                json!([{"date":"2021-01-05","code":"tdx_reference_precision_difference","opening_quantity":1.,"status":"unexposed_or_reference_only"}]),
            ),
        ] {
            write(root.path(), name, &value);
        }
        fs::write(root.path().join("order_events.jsonl"),json!({"date":"2021-01-04","instrument":"SH600000","filled_quantity":1.,"price":10.,"fee":1.}).to_string()+"\n").unwrap();
        (
            root,
            serde_json::from_slice(&serde_json::to_vec(&summary).unwrap()).unwrap(),
        )
    }
    #[test]
    fn audit_preserves_incomplete_event_disclosure_and_rejects_corrupted_account_artifacts() {
        let (root, summary) = fixture();
        let report = artifacts(root.path(), &summary).unwrap();
        assert_eq!(report["status"], "consistent");
        assert_eq!(report["held_valuation_mark_rows"], 1);
        assert_eq!(report["event_simulation_complete"], false);
        assert_eq!(report["skipped_events_with_exposure"], 1);
        assert_eq!(report["research_ready"], Value::Null);
        for (name, field) in [
            ("daily.json", "net_return"),
            ("daily.json", "execution_fee_cny"),
            ("portfolio_snapshots.json", "equity"),
        ] {
            let original = read(root.path(), name).unwrap();
            let mut bad = original.clone();
            if name == "daily.json" {
                bad[0][field] = json!(0.5);
            } else {
                bad["2021-01-05"][field] = json!(1.);
            }
            write(root.path(), name, &bad);
            assert!(artifacts(root.path(), &summary).is_err(), "{name}:{field}");
            write(root.path(), name, &original);
        }
        let mut bad = read(root.path(), "event_exposure_checks.json").unwrap();
        bad[0]["code"] = json!("source_event_conflict");
        write(root.path(), "event_exposure_checks.json", &bad);
        assert!(artifacts(root.path(), &summary).is_err());
    }
    #[test]
    fn audit_rejects_fills_on_zero_volume_valuation_dates_and_order_fee_disagreements() {
        for (date, fee) in [("2021-01-05", 0.), ("2021-01-04", 2.)] {
            let (root, summary) = fixture();
            fs::write(root.path().join("order_events.jsonl"),json!({"date":date,"instrument":"SH600000","filled_quantity":1.,"price":10.,"fee":fee}).to_string()).unwrap();
            assert!(artifacts(root.path(), &summary).is_err());
        }
    }
}
