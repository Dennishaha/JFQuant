//! Compare completed allocation methods with identical signals and execution contracts.
use crate::{Result, RunStatus, Workspace};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs};
use uuid::Uuid;
/// Freeze and validate completed runs before comparing their outcomes.
pub fn snapshot(w: &Workspace, tasks: &[Uuid]) -> Result<Value> {
    if !(2..=10).contains(&tasks.len())
        || tasks.iter().collect::<BTreeSet<_>>().len() != tasks.len()
    {
        return Err("allocation_comparison_requires_distinct_tasks".into());
    }
    let mut rows = Vec::new();
    let mut identity = None;
    let mut baseline = false;
    for id in tasks {
        let root = w.resolve(format!(".jfquant/runs/{id}"));
        let run: crate::RunRecord = serde_json::from_slice(&fs::read(root.join("run.json"))?)?;
        if run.status != RunStatus::Succeeded || run.read_only || run.id != *id {
            return Err("allocation_comparison_requires_completed_native_tasks".into());
        }
        let result: Value = serde_json::from_slice(&fs::read(root.join("summary.json"))?)?;
        crate::research::backtest::validate_result(&result)?;
        crate::allocation::verify_artifacts(&root)?;
        let input: Value = serde_json::from_slice(&fs::read(root.join("allocation-input.json"))?)?;
        let request: Value =
            serde_json::from_slice(&fs::read(root.join("research-request.json"))?)?;
        let predictions = if root.join("combined-input.json").exists() {
            let combined: Value =
                serde_json::from_slice(&fs::read(root.join("combined-input.json"))?)?;
            json!({"sources":combined["plan"]["sources"],"kind":"combined"})
        } else {
            let prediction: Value =
                serde_json::from_slice(&fs::read(root.join("prediction_manifest.json"))?)?;
            json!({"sha256":prediction["sha256"],"kind":"stock"})
        };
        let key = json!({"candidate_policy":request["config"]["allocation"]["candidate_policy"],"eligible_dates_sha256":input["admission"]["eligibility_sha256"],"risk_window":request["config"]["allocation"]["risk_window"],"max_stock_weight":request["config"]["allocation"]["max_stock_weight"],"max_strategy_weight":request["config"]["allocation"]["max_strategy_weight"],"max_turnover":request["config"]["allocation"]["max_turnover"],"predictions":predictions,"market":result["execution"]["market_content_sha256"],"events":result["execution"]["corporate_action_sha256"],"range":input["formal_range"],"rebalance_days":request["config"]["rebalance_days"],"top_n":request["config"]["top_n"],"rules":request["market_rules"],"cash":request["initial_cash_cny"],"pool":request["pool"],"exit":request["pool_exit_policy"],"rights":request["rights_policy"],"events_policy":request["corporate_event_policy"]});
        if identity.as_ref().is_some_and(|v| v != &key) {
            return Err("allocation_comparison_inputs_differ".into());
        }
        identity = Some(key);
        let method = &request["config"]["allocation"]["method"];
        baseline |= method == "equal";
        let allocation: Value =
            serde_json::from_slice(&fs::read(root.join("allocation_report.json"))?)?;
        let targets = allocation["targets"]
            .as_array()
            .ok_or("comparison_targets_missing")?;
        let concentrations = targets
            .iter()
            .filter_map(|t| {
                let w = t["weights"].as_object()?;
                Some(
                    w.values()
                        .filter_map(Value::as_f64)
                        .map(|v| v * v)
                        .sum::<f64>(),
                )
            })
            .collect::<Vec<_>>();
        let timing: Value = serde_json::from_slice(&fs::read(root.join("backtest_timing.json"))?)?;
        rows.push(json!({"task":id,"method":method,"aggregate":result["aggregate"],"mean_target_hhi":if concentrations.is_empty(){None}else{Some(concentrations.iter().sum::<f64>()/concentrations.len() as f64)},"timing":timing,"summary_sha256":crate::signature(&result)?,"allocation_manifest":result["execution"]["allocation_artifacts"]}));
    }
    if !baseline {
        return Err("allocation_comparison_requires_equal_baseline".into());
    }
    Ok(
        json!({"schema_version":1,"kind":"allocation_comparison","identity":identity,"runs":rows,"interpretation":"same frozen signals and execution assumptions; realized fees included; no guarantee of optimizer outperformance"}),
    )
}
