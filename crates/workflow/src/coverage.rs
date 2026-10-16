//! Read-only acquisition coverage. Download progress never substitutes for research admission.
use crate::{Result, RunRecord, Workspace, refresh::RefreshPlan};
use analytics_core::{
    Date,
    pool::{PoolDefinition, PoolExpression, PoolMembership},
};
use analytics_storage::daily::{DailyBar, read_bars};
use chrono::Datelike;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};
use uuid::Uuid;

fn read(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

/// Summarize a refresh without modifying tasks, checkpoints, credentials or stock pools.
/// `inspect_bars` adds per-session and annual historical-membership coverage from validated
/// collector output. A suggested availability pool is returned only after all quotes finish;
/// callers must explicitly save it and still run the normal training/execution admission.
pub fn refresh_report(w: &Workspace, id: Uuid, inspect_bars: bool) -> Result<Value> {
    let root = w.resolve(format!(".jfquant/runs/{id}"));
    let run: RunRecord = serde_json::from_value(read(&root.join("run.json"))?)?;
    if run.id != id
        || run.output != Path::new(&format!(".jfquant/runs/{id}"))
        || crate::signature(&run.request)? != run.signature
    {
        return Err("coverage_request_identity_mismatch".into());
    }
    analytics_core::source_map::reject_removed_sources(&run.request)?;
    if run.request["request"]["operation"] != "refresh_data" || run.read_only {
        return Err("coverage_requires_native_refresh_task".into());
    }
    let plan_path = root.join("refresh-plan.json");
    if !plan_path.exists() {
        return Ok(
            json!({"schema_version":1,"task_id":id,"status":run.status,"stage":run.stage,"quotes_complete":false,"research_ready":null,"reason":"planning_pending"}),
        );
    }
    let plan: RefreshPlan = serde_json::from_value(read(&plan_path)?)?;
    plan.source_policy.validate()?;
    analytics_core::source_map::reject_removed_sources(&serde_json::to_value(&plan)?)?;
    let request = &run.request["request"]["request"];
    if request["market"] != plan.base_market.id
        || request["template"]["pool_version"] != plan.membership["pool_version"]
            && plan.membership["version"]["id"] != request["template"]["pool_version"]
    {
        return Err("coverage_plan_identity_mismatch".into());
    }
    let owner = request["resume"]
        .as_str()
        .map(Uuid::parse_str)
        .transpose()?
        .unwrap_or(id);
    let staging = w.resolve(format!(".jfquant/staging/refresh-{owner}"));
    let owner_path = staging.join("refresh-owner.json");
    if owner_path.exists() && read(&owner_path)?["plan_signature"] != crate::signature(&plan)? {
        return Err("coverage_plan_hash_mismatch".into());
    }
    if request["mode"] == "actions" {
        let actions = root.join("action-progress.json");
        return Ok(
            json!({"schema_version":1,"task_id":id,"status":run.status,"stage":run.stage,"mode":"actions","market":plan.base_market.id,"quotes_status":"not_requested","action_progress":if actions.exists(){read(&actions)?}else{Value::Null},"research_ready":null}),
        );
    }
    let progress_path = root.join("refresh-progress.json");
    let progress = if progress_path.exists() {
        read(&progress_path)?
    } else {
        json!({"outcomes":{}})
    };
    let outcomes = progress["outcomes"]
        .as_object()
        .ok_or("invalid_refresh_progress")?;
    let expected: BTreeSet<_> = plan
        .securities
        .iter()
        .map(|s| s.instrument.clone())
        .collect();
    if outcomes
        .keys()
        .any(|id| !expected.contains(id) && !matches!(id.as_str(), "calendar" | "members"))
    {
        return Err("unrequested_refresh_security".into());
    }
    let mut downloaded = BTreeSet::new();
    let mut missing = BTreeMap::new();
    let mut sources: BTreeMap<String, usize> = BTreeMap::new();
    let mut security_sources = BTreeMap::new();
    for security in &expected {
        if let Some(outcome) = outcomes.get(security) {
            match outcome["quotes"].as_str() {
                Some("downloaded") => {
                    let last = outcome["attempts"]
                        .as_array()
                        .and_then(|a| a.last())
                        .ok_or("refresh_source_missing")?;
                    let provider = last["provider"]
                        .as_str()
                        .ok_or("refresh_provider_missing")?;
                    if last["status"] != "passed"
                        || !matches!(provider, "hithink" | "tdxaidata")
                        || outcome["rows"].as_u64().unwrap_or(0) == 0
                    {
                        return Err("invalid_refresh_success".into());
                    }
                    downloaded.insert(security.clone());
                    security_sources
                        .insert(security.clone(), json!({"provider":provider,"task_id":id}));
                    *sources.entry(provider.into()).or_default() += 1;
                }
                Some("failed") => {
                    missing.insert(security.clone(), outcome["attempts"].clone());
                }
                _ => return Err("invalid_refresh_quote_status".into()),
            }
        }
    }
    let complete = downloaded.len() + missing.len() == expected.len();
    let pool: PoolMembership = serde_json::from_value(plan.membership["membership"].clone())?;
    pool.validate()?;
    let suggestion = if complete
        && downloaded.contains(
            request["template"]["benchmark"]
                .as_str()
                .ok_or("missing_benchmark")?,
        ) {
        let definition =
            available_definition(&plan, &pool, &missing.keys().cloned().collect(), id)?;
        Some(definition)
    } else {
        None
    };
    let mut result = json!({"schema_version":1,"task_id":id,"status":run.status,"stage":run.stage,"history":plan.history,"expected_securities":expected.len(),"checked_securities":downloaded.len()+missing.len(),"downloaded_securities":downloaded.len(),"pending_securities":expected.len()-downloaded.len()-missing.len(),"quotes_complete":complete,"provider_counts":sources,"security_sources":security_sources,"missing_securities":missing,"available_pool_definition":suggestion,"availability_selection_may_bias_results":!missing.is_empty(),"research_ready":null,"research_admission_required":true,"bar_coverage":null});
    if inspect_bars {
        let calendar_source = outcomes
            .get("calendar")
            .and_then(|o| o["attempts"].as_array())
            .and_then(|a| a.last());
        if let Some(source) = calendar_source.filter(|s| s["status"] == "passed") {
            let provider = if source["provider"] == "hithink" {
                "Hithink"
            } else if source["provider"] == "tdxaidata" {
                "Tdxaidata"
            } else {
                return Err("invalid_calendar_provider".into());
            };
            let reference =
                read(&staging.join(format!("reference-calendar/{provider}/reference.json")))?;
            let calendar: Vec<Date> =
                serde_json::from_value(reference["data"]["calendar"].clone())?;
            if calendar.is_empty() || calendar.windows(2).any(|w| w[0] >= w[1]) {
                return Err("invalid_coverage_calendar".into());
            }
            let mut bars = BTreeMap::new();
            for security in &downloaded {
                let staged = staging.join(format!("published/market/bars/{security}.parquet"));
                let path = if staged.exists() {
                    staged
                } else {
                    w.resolve(format!("data/refresh-{id}/market/bars/{security}.parquet"))
                };
                bars.insert(security.clone(), read_bars(&path)?);
            }
            result["bar_coverage"] = daily_coverage(&calendar, &pool, &downloaded, &bars)?;
        } else {
            result["bar_coverage_reason"] = json!("verified_calendar_not_available");
        }
    }
    let actions = root.join("action-progress.json");
    if actions.exists() {
        result["action_progress"] = read(&actions)?;
    }
    Ok(result)
}

fn available_definition(
    plan: &RefreshPlan,
    pool: &PoolMembership,
    excluded: &BTreeSet<String>,
    id: Uuid,
) -> Result<PoolDefinition> {
    let original = &plan.membership["version"]["definition"];
    let mut definition: PoolDefinition = if original.is_object() {
        serde_json::from_value(original.clone())?
    } else {
        PoolDefinition {
            schema_version: 3,
            name: "Availability-selected historical pool".into(),
            coverage: pool.coverage.clone(),
            expression: PoolExpression::Universe {
                version: plan.base_market.id.clone(),
                universe: plan.membership["pool_version"]
                    .as_str()
                    .and_then(|s| s.strip_prefix("builtin:"))
                    .ok_or("invalid_builtin_pool")?
                    .into(),
            },
            adjustments: vec![],
        }
    };
    definition.name = format!("{} / quotes-available / {id}", definition.name);
    if !excluded.is_empty() {
        definition.expression = PoolExpression::Difference {
            base: Box::new(definition.expression),
            exclude: Box::new(PoolExpression::Symbols {
                instruments: excluded.clone(),
            }),
        };
    }
    definition.validate()?;
    Ok(definition)
}

/// Combine compatible native acquisition partitions, preferring a successful security
/// shard over a failed attempt. No bytes are copied and no data or pool is published.
/// Additional sources must have the same immutable base, history and benchmark, and
/// may only contain securities from the primary task's plan.
pub fn combined_refresh_report(
    w: &Workspace,
    id: Uuid,
    sources: &[crate::refresh::ShardSource],
    inspect_bars: bool,
) -> Result<Value> {
    if sources.is_empty() {
        return refresh_report(w, id, inspect_bars);
    }
    if sources.len() > 8 {
        return Err("too_many_coverage_sources".into());
    }
    let mut origins = vec![(w.clone(), id)];
    for source in sources {
        origins.push(crate::refresh::shard_workspace(w, source)?);
    }
    let mut reports = vec![];
    let mut plans = vec![];
    let mut requests = vec![];
    for (workspace, task) in &origins {
        reports.push(refresh_report(workspace, *task, false)?);
        let root = workspace.resolve(format!(".jfquant/runs/{task}"));
        plans.push(serde_json::from_value::<RefreshPlan>(read(
            &root.join("refresh-plan.json"),
        )?)?);
        requests.push(read(&root.join("run.json"))?["request"]["request"]["request"].clone());
    }
    let base = &plans[0];
    let expected: BTreeSet<_> = base
        .securities
        .iter()
        .map(|s| s.instrument.clone())
        .collect();
    for (index, plan) in plans.iter().enumerate().skip(1) {
        if plan.history != base.history
            || plan.base_market.signature != base.base_market.signature
            || requests[index]["template"]["benchmark"] != requests[0]["template"]["benchmark"]
            || plan
                .securities
                .iter()
                .any(|s| !expected.contains(&s.instrument))
        {
            return Err("incompatible_coverage_partition".into());
        }
    }
    let mut selected = BTreeMap::new();
    let mut missing = BTreeMap::new();
    let mut pending = 0;
    let mut providers: BTreeMap<String, usize> = BTreeMap::new();
    for security in &expected {
        if let Some((index, value)) = reports
            .iter()
            .enumerate()
            .find_map(|(i, r)| r["security_sources"].get(security).map(|v| (i, v)))
        {
            selected.insert(security.clone(), index);
            *providers
                .entry(
                    value["provider"]
                        .as_str()
                        .ok_or("missing_partition_provider")?
                        .into(),
                )
                .or_default() += 1;
        } else {
            let attempts: Vec<_> = reports
                .iter()
                .enumerate()
                .filter_map(|(index, r)| {
                    r["missing_securities"]
                        .get(security)
                        .map(|a| json!({"task_id":origins[index].1,"attempts":a}))
                })
                .collect();
            let awaited = plans.iter().zip(&reports).any(|(plan, report)| {
                plan.securities.iter().any(|s| s.instrument == *security)
                    && report["missing_securities"].get(security).is_none()
                    && report["security_sources"].get(security).is_none()
            });
            if awaited || attempts.is_empty() {
                pending += 1
            } else {
                missing.insert(security.clone(), json!(attempts));
            }
        }
    }
    let mut report = reports[0].clone();
    let complete = pending == 0;
    let pool: PoolMembership = serde_json::from_value(base.membership["membership"].clone())?;
    let available: BTreeSet<_> = selected.keys().cloned().collect();
    report["provider_counts"] = json!(providers);
    report["downloaded_securities"] = json!(selected.len());
    report["pending_securities"] = json!(pending);
    report["checked_securities"] = json!(expected.len() - pending);
    report["quotes_complete"] = json!(complete);
    report["missing_securities"] = json!(missing);
    report["availability_selection_may_bias_results"] = json!(!missing.is_empty());
    report["security_sources"]=json!(selected.iter().map(|(security,index)|(security.clone(),json!({"workspace":origins[*index].0.root(),"task_id":origins[*index].1,"provider":reports[*index]["security_sources"][security]["provider"]}))).collect::<BTreeMap<_,_>>());
    report["available_pool_definition"] = if complete
        && available.contains(
            requests[0]["template"]["benchmark"]
                .as_str()
                .ok_or("missing_benchmark")?,
        ) {
        serde_json::to_value(available_definition(
            base,
            &pool,
            &missing.keys().cloned().collect(),
            id,
        )?)?
    } else {
        Value::Null
    };
    report["partition_count"] = json!(origins.len());
    report["bar_coverage"] = Value::Null;
    if inspect_bars {
        let owner = requests[0]["resume"]
            .as_str()
            .map(Uuid::parse_str)
            .transpose()?
            .unwrap_or(id);
        let mut calendar = None;
        for provider in ["Hithink", "Tdxaidata"] {
            let path = w.resolve(format!(
                ".jfquant/staging/refresh-{owner}/reference-calendar/{provider}/reference.json"
            ));
            if path.exists() {
                calendar = Some(serde_json::from_value::<Vec<Date>>(
                    read(&path)?["data"]["calendar"].clone(),
                )?);
                break;
            }
        }
        if let Some(calendar) = calendar {
            if calendar.is_empty() || calendar.windows(2).any(|p| p[0] >= p[1]) {
                return Err("invalid_partition_calendar".into());
            }
            let mut bars = BTreeMap::new();
            for (security, index) in &selected {
                let (workspace, task) = &origins[*index];
                let owner = requests[*index]["resume"]
                    .as_str()
                    .map(Uuid::parse_str)
                    .transpose()?
                    .unwrap_or(*task);
                let staged = workspace.resolve(format!(
                    ".jfquant/staging/refresh-{owner}/published/market/bars/{security}.parquet"
                ));
                let path = if staged.exists() {
                    staged
                } else {
                    workspace.resolve(format!(
                        "data/refresh-{task}/market/bars/{security}.parquet"
                    ))
                };
                bars.insert(security.clone(), read_bars(&path)?);
            }
            report["bar_coverage"] = daily_coverage(&calendar, &pool, &available, &bars)?;
        }
    }
    Ok(report)
}

fn daily_coverage(
    calendar: &[Date],
    pool: &PoolMembership,
    available: &BTreeSet<String>,
    bars: &BTreeMap<String, Vec<DailyBar>>,
) -> Result<Value> {
    let mut counts = vec![[0usize; 3]; calendar.len()];
    for (security, ranges) in &pool.intervals {
        let mut days = BTreeSet::new();
        for range in ranges {
            days.extend(
                calendar.partition_point(|d| *d < range.start)
                    ..calendar.partition_point(|d| *d <= range.end),
            );
        }
        let present: BTreeSet<_> = bars
            .get(security)
            .into_iter()
            .flatten()
            .filter(|bar| {
                bar.open.is_some()
                    && bar.high.is_some()
                    && bar.low.is_some()
                    && bar.close.is_some()
                    && bar.volume.is_some()
                    && bar.amount.is_some()
            })
            .map(|bar| bar.trade_date)
            .collect();
        for day in days {
            counts[day][0] += 1;
            counts[day][1] += usize::from(available.contains(security));
            counts[day][2] += usize::from(present.contains(&calendar[day]));
        }
    }
    let mut annual: BTreeMap<i32, [usize; 4]> = BTreeMap::new();
    let daily:Vec<_>=calendar.iter().zip(counts).map(|(day,count)| {
        let year=annual.entry(day.year()).or_default();year[0]+=1;year[1]+=count[0];year[2]+=count[1];year[3]+=count[2];
        json!({"date":day,"historical_members":count[0],"available_members":count[1],"members_with_complete_quote":count[2]})
    }).collect();
    let annual:Vec<_>=annual.into_iter().map(|(year,c)|json!({"year":year,"sessions":c[0],"historical_member_sessions":c[1],"available_member_sessions":c[2],"complete_quote_member_sessions":c[3],"availability_ratio":if c[1]==0 {None}else{Some(c[2]as f64/c[1]as f64)},"complete_quote_ratio":if c[1]==0 {None}else{Some(c[3]as f64/c[1]as f64)}})).collect();
    Ok(
        json!({"daily":daily,"annual":annual,"quote_absence_does_not_identify_suspension":true,"research_ready":null}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dated_membership_missing_and_duplicate_intervals_are_counted_once() {
        let days: Vec<Date> = ["2021-01-04", "2021-01-05", "2022-01-04"]
            .map(|s| s.parse().unwrap())
            .into();
        let pool = PoolMembership {
            coverage: analytics_core::DateRange {
                start: days[0],
                end: days[2],
            },
            intervals: [
                (
                    "SH600000".into(),
                    vec![
                        analytics_core::DateRange {
                            start: days[0],
                            end: days[1]
                        };
                        2
                    ],
                ),
                (
                    "SZ000001".into(),
                    vec![analytics_core::DateRange {
                        start: days[1],
                        end: days[2],
                    }],
                ),
            ]
            .into(),
        };
        let report =
            daily_coverage(&days, &pool, &["SH600000".into()].into(), &BTreeMap::new()).unwrap();
        assert_eq!(report["daily"][0]["historical_members"], 1);
        assert_eq!(report["daily"][1]["historical_members"], 2);
        assert_eq!(report["daily"][2]["available_members"], 0);
        assert_eq!(report["annual"][0]["availability_ratio"], 2. / 3.);
        assert_eq!(report["annual"][0]["complete_quote_ratio"], 0.);
        assert!(report["research_ready"].is_null());
    }
    #[test]
    fn completed_availability_proposal_keeps_history_and_does_not_publish_a_pool() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (market, _, template) = crate::demo::create_canonical(&w).unwrap();
        let benchmark = template.benchmark.clone();
        let request = crate::WorkflowRequest::RefreshData {
            request: Box::new(crate::refresh::RefreshRequest {
                template: Box::new(template),
                market: market.id,
                actions: None,
                mode: crate::refresh::RefreshMode::Full,
                provider: None,
                end: None,
                preview: true,
                resume: None,
                workers: 3,
                reuse_shards_from: vec![],
            }),
        };
        let run = w
            .begin(
                Uuid::new_v4(),
                "coverage fixture",
                crate::snapshot(&w, &request).unwrap(),
            )
            .unwrap();
        crate::execute(
            &w,
            &request,
            &run,
            &analytics_core::task::TaskContext::current(),
        )
        .unwrap();
        let root = w.resolve(&run.output);
        let plan: RefreshPlan =
            serde_json::from_value(read(&root.join("refresh-plan.json")).unwrap()).unwrap();
        assert!(
            !refresh_report(&w, run.id, false).unwrap()["quotes_complete"]
                .as_bool()
                .unwrap()
        );
        let failed = plan
            .securities
            .iter()
            .find(|s| s.instrument != benchmark)
            .unwrap()
            .instrument
            .clone();
        let outcomes:BTreeMap<_,_>=plan.securities.iter().map(|s|(s.instrument.clone(),if s.instrument==failed {json!({"quotes":"failed","attempts":[]})} else {json!({"quotes":"downloaded","rows":2,"attempts":[{"provider":"hithink","status":"passed"}]})})).collect();
        crate::atomic_json(
            &root.join("refresh-progress.json"),
            &json!({"outcomes":outcomes}),
        )
        .unwrap();
        let report = refresh_report(&w, run.id, false).unwrap();
        assert_eq!(report["quotes_complete"], true);
        assert_eq!(report["availability_selection_may_bias_results"], true);
        let definition: PoolDefinition =
            serde_json::from_value(report["available_pool_definition"].clone()).unwrap();
        if let PoolExpression::Difference { exclude, .. } = definition.expression {
            assert_eq!(
                *exclude,
                PoolExpression::Symbols {
                    instruments: [failed].into()
                }
            )
        } else {
            panic!("missing explicit exclusions")
        }
        assert!(w.pools().unwrap().is_empty());
        assert!(report["research_ready"].is_null());
        let second = w
            .begin(
                Uuid::new_v4(),
                "partition fixture",
                crate::snapshot(&w, &request).unwrap(),
            )
            .unwrap();
        crate::execute(
            &w,
            &request,
            &second,
            &analytics_core::task::TaskContext::current(),
        )
        .unwrap();
        let source = crate::refresh::ShardSource::Workspace {
            workspace: w.root().to_path_buf(),
            task_id: second.id,
        };
        let pending =
            combined_refresh_report(&w, run.id, std::slice::from_ref(&source), false).unwrap();
        assert_eq!(pending["quotes_complete"], false);
        assert!(pending["available_pool_definition"].is_null());
        let recovered:BTreeMap<_,_>=plan.securities.iter().map(|s|(s.instrument.clone(),json!({"quotes":"downloaded","rows":2,"attempts":[{"provider":"hithink","status":"passed"}]}))).collect();
        crate::atomic_json(
            &w.resolve(&second.output).join("refresh-progress.json"),
            &json!({"outcomes":recovered}),
        )
        .unwrap();
        let recovered = combined_refresh_report(&w, run.id, &[source], false).unwrap();
        assert_eq!(recovered["quotes_complete"], true);
        assert!(
            recovered["missing_securities"]
                .as_object()
                .unwrap()
                .is_empty()
        );
        assert_eq!(recovered["availability_selection_may_bias_results"], false);
        assert_eq!(recovered["downloaded_securities"], plan.securities.len());
    }
    #[test]
    fn modified_request_and_wrong_task_type_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let mut run = w
            .begin(
                Uuid::new_v4(),
                "fixture",
                json!({"request":{"operation":"other"}}),
            )
            .unwrap();
        assert!(
            refresh_report(&w, run.id, false)
                .unwrap_err()
                .to_string()
                .contains("native_refresh")
        );
        run.request = json!({"request":{"operation":"refresh_data"}});
        w.save_run(&run).unwrap();
        assert!(
            refresh_report(&w, run.id, false)
                .unwrap_err()
                .to_string()
                .contains("identity_mismatch")
        );
    }
}
