//! Task requirements, without guessing which event fields are absent from unreviewed history.
use super::*;
use analytics_core::admission::{AdmissionCheck, CheckStatus};
use std::collections::{BTreeMap, BTreeSet};
pub(super) fn build(
    book: &ActionBundle,
    entries: &[Value],
    checks: &[AdmissionCheck],
) -> Result<Vec<Value>> {
    let wanted: BTreeSet<_> = entries
        .iter()
        .filter_map(|e| e["coverage"]["instrument"].as_str())
        .collect();
    let mut documents = BTreeMap::<String, Value>::new();
    for entry in entries {
        for gap in entry["coverage"]["gaps"].as_array().into_iter().flatten() {
            let id = entry["coverage"]["instrument"].as_str().unwrap();
            let category = gap["category"].as_str().unwrap_or("");
            let events = book
                .actions
                .iter()
                .filter(|a| a.instrument == id)
                .map(|a| json!({"id":a.id,"effective_date":a.ex_date,"kind":a.terms}))
                .collect::<Vec<_>>();
            let key = json!([id, entry["purpose"], gap["dates"], category]);
            documents.insert(signature(&key)?,json!({"id":signature(&key)?,"instrument":id,"purpose":entry["purpose"],"dates":gap["dates"],"category":category,"kind":"missing_coverage","required_fact":"complete_category_history","field_assessment":"not_assessed_by_coverage_gap","existing_evidence":entry["coverage"]["evidence"],"known_events":events,"next_action":{"operation":"draft","reason":"review complete query and candidates; add verified event revisions separately","required_event_facts":if category=="merger" {json!(["participants","ordinary_holder_consideration","ratio","record_date","last_trade_date","effective_date","successor_tradability","registrar_allocation"])}else{json!([])}}}));
        }
    }
    for check in checks.iter().filter(|c| c.status == CheckStatus::Failed) {
        if check
            .security
            .as_deref()
            .is_some_and(|id| !wanted.contains(id))
        {
            continue;
        }
        // Coverage requirements already contain exact dates/categories; do not duplicate their legacy error code.
        if matches!(
            check.code.as_str(),
            "execution_source_failed"
                | "execution_event_coverage_missing"
                | "execution_structured_coverage_missing"
                | "execution_history_not_queried"
        ) {
            continue;
        }
        let kind =
            if check.code.contains("unsupported") || check.code == "execution_unprojected_issue" {
                "unsupported"
            } else if check.code == "execution_projection_invalid" {
                "invalid_execution_inputs"
            } else {
                "invalid_evidence"
            };
        let key = json!([check.security, check.coverage, check.code]);
        documents.insert(signature(&key)?,json!({"id":signature(&key)?,"instrument":check.security,"dates":check.coverage,"kind":kind,"required_fact":check.code,"reason":check.message,"existing_evidence":check.evidence,"disposition":check.disposition,"next_action":{"operation":"draft","reason":"inspect verified event facts and selected execution policy"}}));
    }
    Ok(documents.into_values().collect())
}
