//! Compare verified reports under the same requested task context, independently of evidence edits.
use crate::{Result, Workspace};
use analytics_core::membership_history::Report;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
/// Compare only explicit current preview envelopes; diagnostic and coverage changes stay separate.
pub fn compare(before: &Value, after: &Value) -> Result<Value> {
    let context = before["task_context_signature"]
        .as_str()
        .ok_or("before preview has no current task context")?;
    if before["schema_version"] != 2
        || after["schema_version"] != 2
        || after["task_context_signature"] != context
    {
        return Err(
            "history comparison requires identical task context and current report schema".into(),
        );
    }
    let b: Report = serde_json::from_value(before["history_report"].clone())?;
    let a: Report = serde_json::from_value(after["history_report"].clone())?;
    let issues = |r: Report| -> BTreeMap<String, analytics_core::membership_history::Issue> {
        r.issues.into_iter().map(|i| (i.id.clone(), i)).collect()
    };
    let prior = issues(b.clone());
    let current = issues(a.clone());
    let old: BTreeSet<_> = prior.keys().collect();
    let new: BTreeSet<_> = current.keys().collect();
    Ok(json!({"schema_version":2,"task_context_signature":context,
        "resolved":old.difference(&new).map(|id|&prior[*id]).collect::<Vec<_>>(),
        "remaining":old.intersection(&new).map(|id|&current[*id]).collect::<Vec<_>>(),
        "added":new.difference(&old).map(|id|&current[*id]).collect::<Vec<_>>(),
        "newly_proven_dates":b.missing_dates.difference(&a.missing_dates).collect::<Vec<_>>(),
        "newly_unproven_dates":a.missing_dates.difference(&b.missing_dates).collect::<Vec<_>>(),
        "before_unproven_days":b.missing_dates.len(),"after_unproven_days":a.missing_dates.len(),
        "before_segments":b.segments.len(),"after_segments":a.segments.len()}))
}
pub(crate) fn snapshot(w: &Workspace, before: uuid::Uuid, after: uuid::Uuid) -> Result<Value> {
    let load = |id| -> Result<Value> {
        let v = crate::views::run(&w.root, id)?
            .result
            .map_err(std::io::Error::other)?;
        let d = v.get("details").ok_or("expected history task details")?;
        if d["schema_version"] != 2 || d.get("history_report").is_none() {
            return Err("expected current history preview task".into());
        }
        Ok(d.clone())
    };
    Ok(json!({"before":load(before)?,"after":load(after)?}))
}
