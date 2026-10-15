//! Independent acquisition: incomplete inputs remain candidates, never research snapshots.
use crate::*;
use std::collections::BTreeMap;
/// Run each dependency independently and persist failures without discarding validated shards.
pub fn collect_independent<F>(
    root: &Path,
    requirements: &Value,
    mut collect: F,
    context: &TaskContext,
) -> Result<Value>
where
    F: FnMut(&str, &Path) -> Result<Value>,
{
    fs::create_dir_all(root)?;
    let owner = json!({"schema_version":1,"requirements_signature":signature(requirements)?,"requirements":requirements});
    let owner_path = root.join("owner.json");
    if owner_path.exists() {
        let saved: Value = serde_json::from_slice(&fs::read(&owner_path)?)?;
        if saved != owner {
            return Err("preparation checkpoint request/source mismatch".into());
        }
    } else {
        atomic_json(&owner_path, &owner)?;
    }
    let mut outcomes = BTreeMap::new();
    let symbols: Vec<String> = serde_json::from_value(requirements["securities"].clone())?;
    for (index, id) in symbols.iter().enumerate() {
        context.check()?;
        context.report(format!("补采 {}/{} · {}", index + 1, symbols.len(), id))?;
        let shard = root.join(id);
        let receipt = shard.join("verified.json");
        if receipt.exists() {
            let old: Value = serde_json::from_slice(&fs::read(&receipt)?)?;
            if old["owner"] == owner["requirements_signature"]
                && old["sha256"] == market_content_hash(&shard.join("captured"))?
            {
                outcomes.insert(id.clone(), old["result"].clone());
                continue;
            }
            return Err("preparation shard/evidence changed".into());
        }
        fs::create_dir_all(&shard)?;
        match collect(id, &shard.join("captured")) {
            Ok(result) => {
                context.check()?;
                let sha = market_content_hash(&shard.join("captured"))?;
                atomic_json(
                    &receipt,
                    &json!({"owner":owner["requirements_signature"],"sha256":sha,"result":result}),
                )?;
                outcomes.insert(id.clone(), result);
            }
            Err(e) => {
                context.check()?;
                outcomes.insert(
                    id.clone(),
                    json!({"status":"failed","code":"collection_failed","message":e.to_string()}),
                );
            }
        }
        atomic_json(
            &root.join("progress.json"),
            &json!({"schema_version":1,"outcomes":outcomes,"remaining":symbols.len()-index-1,"published":false}),
        )?;
    }
    let failures: Vec<_> = outcomes
        .iter()
        .filter(|(_, o)| o["status"] != "passed")
        .map(|(id, o)| json!({"security":id,"result":o}))
        .collect();
    let result = json!({"schema_version":1,"ready":false,"published":false,"candidate_only":true,"outcomes":outcomes,"failures":failures,"reason":"acquisition success does not prove complete execution event-category coverage"});
    atomic_json(&root.join("candidates.json"), &result)?;
    Ok(result)
}
/// Acquire source candidates using snapshot-local dated source exceptions where needed.
pub(crate) fn prepare(
    w: &Workspace,
    run: &RunRecord,
    requirements: &Value,
    announcements: bool,
    resume: Option<Uuid>,
    context: &TaskContext,
) -> Result<Value> {
    let owner = resume.unwrap_or(run.id);
    let root = w.resolve(format!(".jfquant/staging/research-actions-{owner}"));
    let mapping: analytics_core::source_map::SourceMap =
        serde_json::from_value(requirements["source_map"].clone())?;
    let mut acquisition = requirements.clone();
    acquisition["collector_signature"] = json!(analytics_storage::collector::collector_signature());
    acquisition["company_action_source"] = json!(if announcements {
        "cninfo-announcements"
    } else {
        "akshare.cninfo/implemented/per-ten-shares-v1"
    });
    collect_independent(
        &root,
        &acquisition,
        |id, path| {
            let start: analytics_core::Date = serde_json::from_value(
                requirements["dependencies"][id]["required_history"]["start"]
                    .as_str()
                    .map_or_else(|| requirements["history_start"].clone(), |s| json!(s)),
            )?;
            let end: analytics_core::Date = serde_json::from_value(
                requirements["dependencies"][id]["required_history"]["end"]
                    .as_str()
                    .map_or_else(|| requirements["execution_end"].clone(), |s| json!(s)),
            )?;
            let provider = if announcements { "cninfo" } else { "akshare" };
            let (code, start, end) = if let Some(e) = mapping.exceptions.get(id) {
                let l = e
                    .locators
                    .iter()
                    .find(|l| {
                        l.provider == provider && l.coverage.start <= end && l.coverage.end >= start
                    })
                    .ok_or("source_binding_missing")?;
                (
                    l.symbol.clone(),
                    start.max(l.coverage.start),
                    end.min(l.coverage.end),
                )
            } else {
                (id.to_owned(), start, end)
            };
            analytics_core::pool::validate_exchange_symbol(&code)?;
            if announcements {
                let request = analytics_storage::collector::CollectionRequest {
                    source_bindings: Default::default(),
                    provider: analytics_storage::collector::Provider::Akshare,
                    adjustment_source: analytics_storage::adjustment::AdjustmentSource::SinaHfq,
                    start,
                    end,
                    instruments: [code].into(),
                    benchmarks: Default::default(),
                    universes: Default::default(),
                };
                analytics_storage::collector::collect_announcements(&request, path, context)?;
                Ok(
                    json!({"status":"passed","kind":"announcement_candidates","path":path,"instrument":id}),
                )
            } else {
                let request = analytics_storage::collector::CollectionRequest {
                    source_bindings: Default::default(),
                    provider: analytics_storage::collector::Provider::Akshare,
                    adjustment_source: analytics_storage::adjustment::AdjustmentSource::SinaHfq,
                    start,
                    end,
                    instruments: [code.clone()].into(),
                    benchmarks: Default::default(),
                    universes: Default::default(),
                };
                let file = analytics_storage::collector::collect_actions(&request, path, context)?;
                let b = ActionBundle::read(&file)?;
                if !b.failures.is_empty() {
                    return Err(format!("source failures: {:?}", b.failures).into());
                }
                Ok(
                    json!({"status":"passed","kind":"distribution_candidates","path":path,"instrument":id,"provider_symbol":code,"required_dates":{"start":start,"end":end},"all_event_categories_verified":false}),
                )
            }
        },
        context,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_unknown_symbol_does_not_stop_other_shards_and_resume_rehashes() {
        let dir = tempfile::tempdir().unwrap();
        let request = json!({"securities":["SHT00018","SH600000"],"schema_version":3});
        let context = TaskContext::current();
        let mut calls = vec![];
        let first = collect_independent(
            dir.path(),
            &request,
            |id, path| {
                calls.push(id.to_owned());
                if id == "SHT00018" {
                    return Err("source_code_unavailable".into());
                }
                fs::create_dir_all(path)?;
                fs::write(path.join("body.json"), "verified source")?;
                Ok(json!({"status":"passed"}))
            },
            &context,
        )
        .unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(first["failures"].as_array().unwrap().len(), 1);
        assert_eq!(first["published"], false);
        let mut retries = vec![];
        collect_independent(
            dir.path(),
            &request,
            |id, _| {
                retries.push(id.to_owned());
                Err("still unknown".into())
            },
            &context,
        )
        .unwrap();
        assert_eq!(retries, vec!["SHT00018"]);
        fs::write(dir.path().join("SH600000/captured/body.json"), "tampered").unwrap();
        assert!(
            collect_independent(
                dir.path(),
                &request,
                |_, _| Err("still unknown".into()),
                &context
            )
            .is_err()
        );
    }
}
