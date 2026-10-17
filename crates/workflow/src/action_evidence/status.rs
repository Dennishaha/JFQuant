//! Read-only action diagnostics against a terminal task's immutable dependency context.
use super::*;
use analytics_core::{
    DateRange,
    admission::{AdmissionCheck, CheckStatus},
    execution::EventCheckPolicy,
};
use analytics_storage::corporate_actions::{EVENT_CATEGORIES, dependencies::DependencyIndex};
use std::collections::{BTreeMap, BTreeSet};

const FILES: &[&str] = &[
    "run.json",
    "input-check.json",
    "backtest_plan.json",
    "adjustment-dependencies.json",
    "execution-dependencies.json",
];
/// Freeze only the records used by this diagnostic; never hash a live task directory.
pub(crate) fn freeze(w: &Workspace, task: Uuid) -> Result<Value> {
    let root = w.resolve(format!(".jfquant/runs/{task}"));
    if fs::symlink_metadata(&root)?.file_type().is_symlink()
        || !root.canonicalize()?.starts_with(w.root.canonicalize()?)
    {
        return Err("task context must belong to this workspace".into());
    }
    let record: RunRecord = serde_json::from_slice(&fs::read(root.join("run.json"))?)?;
    if record.id != task || record.finished_at.is_none() {
        return Err("action status requires a terminal task".into());
    }
    let mut hashes = BTreeMap::new();
    for name in FILES {
        let path = root.join(name);
        if path.is_file() {
            if fs::symlink_metadata(&path)?.file_type().is_symlink() {
                return Err("task context cannot contain links".into());
            }
            hashes.insert(*name, analytics_storage::market_view::file_hash(&path)?);
        }
    }
    Ok(json!({"task":task,"files":hashes}))
}
fn load(path: &std::path::Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn task_versions(record: &Value) -> &Value {
    if record["request"]["inputs"].is_array() {
        &record["request"]["inputs"]
    } else {
        &record["request"]["independent"]["inputs"]
    }
}
fn blocking(check: &AdmissionCheck) -> bool {
    check.status == CheckStatus::Failed
        && check.disposition == analytics_core::research_quality::ProblemDisposition::Block
}
fn key(check: &AdmissionCheck) -> String {
    serde_json::to_string(&(&check.security, &check.coverage, &check.code)).unwrap()
}

pub(crate) fn evaluate(
    w: &Workspace,
    task: Uuid,
    book: &ActionBundle,
    instrument: Option<&str>,
    source_signature: &str,
) -> Result<Value> {
    let mut current = evaluate_current(w, task, book, instrument, source_signature)?;
    let record = load(&w.resolve(format!(".jfquant/runs/{task}/run.json")))?;
    let baseline = task_versions(&record)
        .as_array()
        .into_iter()
        .flatten()
        .find(|v| v["kind"] == "actions");
    let comparison = (|| -> Result<Value> {
        let baseline = baseline.ok_or("task has no frozen action version for comparison")?;
        let signature = baseline["signature"]
            .as_str()
            .ok_or("baseline action signature missing")?;
        let previous = if signature == source_signature {
            current.clone()
        } else {
            let id = baseline["id"]
                .as_str()
                .ok_or("baseline action version missing")?;
            let version = crate::usecases::version(w, id, "actions")?;
            let path = w.resolve(version.path);
            if crate::signature(&ActionBundle::version_content(&path)?)? != signature {
                return Err("baseline action version changed".into());
            }
            evaluate_current(w, task, &ActionBundle::read(&path)?, instrument, signature)?
        };
        if previous["context_hash"] != current["context_hash"] {
            return Err("comparison task context changed".into());
        }
        let keys = |report: &Value| -> Result<BTreeSet<String>> {
            let checks: Vec<AdmissionCheck> = serde_json::from_value(report["checks"].clone())?;
            Ok(checks.iter().filter(|c| blocking(c)).map(key).collect())
        };
        let old = keys(&previous)?;
        let now = keys(&current)?;
        Ok(
            json!({"available":true,"context_hash":current["context_hash"],"baseline_actions":baseline["id"],"baseline_signature":signature,"baseline":"recomputed_same_task_dependencies","resolved":old.difference(&now).collect::<Vec<_>>(),"added":now.difference(&old).collect::<Vec<_>>() }),
        )
    })();
    current["comparison"] = match comparison {
        Ok(v) => v,
        Err(e) => json!({"available":false,"reason":e.to_string()}),
    };
    Ok(current)
}
fn evaluate_current(
    w: &Workspace,
    task: Uuid,
    book: &ActionBundle,
    instrument: Option<&str>,
    source_signature: &str,
) -> Result<Value> {
    let frozen = freeze(w, task)?;
    let context_hash = signature(&frozen)?;
    let root = w.resolve(format!(".jfquant/runs/{task}"));
    let source_record = load(&root.join("run.json"))?;
    let market_versions: Vec<_> = task_versions(&source_record)
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["kind"] == "market")
        .map(|v| v["id"].clone())
        .collect();
    let index = DependencyIndex::build(book)?;
    let mut execution_scope = BTreeMap::<String, DateRange>::new();
    let mut adjustment_scope = BTreeMap::<String, Vec<DateRange>>::new();
    let mut projected = BTreeSet::new();
    let mut execution_policy = EventCheckPolicy::default();
    let mut adjustment_policy = EventCheckPolicy::default();
    let mut adjustment_blocking = true;
    if root.join("execution-dependencies.json").is_file() {
        let context = load(&root.join("execution-dependencies.json"))?;
        if context["schema_version"] != 2 {
            return Err("unsupported execution dependency context; rerun input check".into());
        }
        execution_scope = serde_json::from_value(context["scope"].clone())?;
        execution_policy = serde_json::from_value(context["policy"].clone())?;
        if !context["request"].is_null() {
            let request: crate::research::backtest::ResearchRequest =
                serde_json::from_value(context["request"].clone())?;
            let pool: analytics_core::pool::PoolMembership =
                serde_json::from_value(context["pool"].clone())?;
            let range = serde_json::from_value(context["range"].clone())?;
            let dataset = analytics_storage::daily::DailyDataset::open(&request.data_root)?;
            if context["market_content_sha256"] != crate::market_content_hash(&request.data_root)? {
                return Err("task market changed".into());
            }
            let resolved = crate::research::backtest::execution::dependencies(
                &request,
                &dataset,
                Some(book),
                &pool,
                &range,
            )?;
            execution_scope = resolved.scope;
            projected = resolved.projected;
        }
    }
    if root.join("adjustment-dependencies.json").is_file() {
        let context = load(&root.join("adjustment-dependencies.json"))?;
        if context["schema_version"] != 1 {
            return Err("unsupported adjustment dependency context".into());
        }
        adjustment_scope = serde_json::from_value(context["scope"].clone())?;
        adjustment_policy = serde_json::from_value(context["policy"].clone())?;
        adjustment_blocking = context["blocking"].as_bool().unwrap_or(true);
        if context["training_only"] == false
            && !root.join("execution-dependencies.json").is_file()
            && execution_scope.is_empty()
        {
            return Err(
                "task action dependency context is incomplete; rerun its input check".into(),
            );
        }
    }
    if execution_scope.is_empty()
        && adjustment_scope.is_empty()
        && !root.join("adjustment-dependencies.json").is_file()
    {
        return Err("task has no frozen action dependencies; rerun its input check with the current App/CLI".into());
    }
    execution_scope.retain(|id, _| instrument.is_none_or(|s| s == id));
    adjustment_scope.retain(|id, _| instrument.is_none_or(|s| s == id));
    if instrument.is_some() && execution_scope.is_empty() && adjustment_scope.is_empty() {
        return Err("security has no action dependency in this task".into());
    }
    let mut checks = crate::research::backtest::execution::event_checks(
        Some(book),
        &execution_scope,
        execution_policy,
        &projected,
        Some(&index),
    );
    let mut entries = vec![];
    for (id, dates) in &execution_scope {
        TaskContext::current().check()?;
        entries.push(json!({"purpose":"execution","ranges":[dates],"coverage":index.resolve(id,std::slice::from_ref(dates),EVENT_CATEGORIES,execution_policy)?}));
    }
    for (id, ranges) in &adjustment_scope {
        TaskContext::current().check()?;
        let current = index.resolve(id, ranges, EVENT_CATEGORIES, adjustment_policy)?;
        let issues = book.adjustment_event_issues(id, ranges);
        if !current.ready() || !issues.is_empty() {
            checks.push(AdmissionCheck {
                stage: analytics_core::admission::AdmissionStage::Training,
                kind: analytics_core::research_quality::ProblemKind::ExecutionDependency,
                disposition: if adjustment_blocking {
                    analytics_core::research_quality::ProblemDisposition::Block
                } else {
                    analytics_core::research_quality::ProblemDisposition::Warn
                },
                category: "adjustment_inputs".into(),
                security: Some(id.clone()),
                coverage: None,
                status: CheckStatus::Failed,
                code: "adjustment_dependencies_missing".into(),
                message: format!("gaps: {:?}; event issues: {:?}", current.gaps, issues),
                evidence: current.evidence.clone(),
                affected_factors: vec![],
            });
        }
        entries.push(json!({"purpose":"adjustment","ranges":ranges,"coverage":current,"event_issues":issues}));
    }
    let blockers: Vec<_> = checks.iter().filter(|c| blocking(c)).collect();
    let securities: BTreeSet<_> = blockers
        .iter()
        .filter_map(|c| c.security.as_deref())
        .collect();
    let gap_count: usize = entries
        .iter()
        .filter_map(|e| e["coverage"]["gaps"].as_array())
        .map(Vec::len)
        .sum();
    let historical: BTreeSet<_> = entries
        .iter()
        .filter(|e| !e["coverage"]["historical_failure"].is_null())
        .filter_map(|e| e["coverage"]["instrument"].as_str())
        .collect();
    Ok(
        json!({"schema_version":1,"task":task,"market_versions":market_versions,"context_hash":context_hash,"actions_signature":source_signature,"purpose":"action_dependencies_only","ready":blockers.is_empty(),"gap_count":gap_count,"blocking_check_count":blockers.len(),"affected_security_count":securities.len(),"historical_failure_security_count":historical.len(),"entries":entries,"checks":checks,"note":"仅诊断本任务公司行动依赖；完整训练/回测仍需重新准入，原任务报告不改写。"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_task_reports_current_evidence_without_rewriting_history() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (_, actions, _) = crate::demo::create(&w).unwrap();
        let mut book = ActionBundle::read(&w.resolve(&actions.path)).unwrap();
        let id = "SH600000";
        book.failures
            .insert(id.into(), "historical query failed".into());
        book.queried_symbols.remove(id);
        book.structured_coverage.remove(id);
        let mut task = w
            .begin(
                Uuid::new_v4(),
                "frozen input task",
                json!({"inputs":[actions]}),
            )
            .unwrap();
        let root = w.resolve(&task.output);
        atomic_json(&root.join("execution-dependencies.json"),&json!({"schema_version":2,"policy":"audited","scope":{id:{"start":"2009-01-02","end":"2009-01-02"}}})).unwrap();
        assert!(freeze(&w, task.id).is_err());
        w.finish(&mut task, crate::RunStatus::Succeeded, None)
            .unwrap();
        // Old reports cannot substitute for an explicit frozen dependency file.
        fs::remove_file(root.join("execution-dependencies.json")).unwrap();
        atomic_json(
            &root.join("backtest_plan.json"),
            &json!({"event_check_policy":"audited"}),
        )
        .unwrap();
        atomic_json(
            &root.join("input-check.json"),
            &json!({"admission":{"checks":[]}}),
        )
        .unwrap();
        assert!(
            evaluate(&w, task.id, &book, None, "version-a")
                .unwrap_err()
                .to_string()
                .contains("no frozen action dependencies")
        );
        atomic_json(&root.join("execution-dependencies.json"),&json!({"schema_version":2,"policy":"audited","scope":{id:{"start":"2009-01-02","end":"2009-01-02"}}})).unwrap();
        let before = freeze(&w, task.id).unwrap();
        let ready = evaluate(&w, task.id, &book, None, "version-a").unwrap();
        assert_eq!(ready["ready"], true);
        assert_eq!(ready["comparison"]["available"], true);
        assert_eq!(ready["historical_failure_security_count"], 1);
        book.coverage.remove(id);
        let blocked = evaluate(&w, task.id, &book, None, "version-b").unwrap();
        assert_eq!(blocked["ready"], false);
        assert_eq!(blocked["affected_security_count"], 1);
        assert_eq!(blocked["gap_count"], 9);
        assert_eq!(ready["context_hash"], blocked["context_hash"]);
        assert_ne!(ready["actions_signature"], blocked["actions_signature"]);
        assert_eq!(freeze(&w, task.id).unwrap(), before);
        assert!(book.failures.contains_key(id));
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let context = TaskContext::new(Uuid::new_v4(), cancel, |_| {});
        assert!(
            context
                .execute(|| evaluate(&w, task.id, &book, None, "cancelled"))
                .is_err()
        );
        atomic_json(&root.join("execution-dependencies.json"),&json!({"schema_version":2,"policy":"audited","scope":{id:{"start":"2009-01-03","end":"2009-01-03"}}})).unwrap();
        assert_ne!(freeze(&w, task.id).unwrap(), before);
    }
}
