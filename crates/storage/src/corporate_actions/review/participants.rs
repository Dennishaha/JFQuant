//! Cross-issuer permissions derived solely from authenticated document quotes.
use super::*;
/// One issuer identified by its own captured implementation disclosure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    /// Canonical exchange identifier, not a substitute for evidence.
    pub instrument: String,
    /// Name as printed in the identity and relationship quotes.
    pub name: String,
    /// Quote containing this name and its six-digit code.
    pub identity: FieldEvidence,
}
/// Evidence binding two independently identified issuers to one conversion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participants {
    /// Logical event being reviewed.
    pub event_id: String,
    /// Original issuer.
    pub predecessor: Participant,
    /// Receiving issuer.
    pub successor: Participant,
    /// Original quotes naming both parties and the transaction, never another review.
    pub relationship: Vec<FieldEvidence>,
}
impl Participants {
    pub(super) fn proofs(&self) -> impl Iterator<Item = &FieldEvidence> {
        [&self.predecessor.identity, &self.successor.identity]
            .into_iter()
            .chain(&self.relationship)
    }
    pub(super) fn verify(
        &self,
        event: &CorporateAction,
        documents: &BTreeMap<String, (Value, String)>,
    ) -> io::Result<()> {
        use analytics_core::actions::{ConversionKind, EventTerms};
        let EventTerms::Conversion {
            category,
            successor: Some(next),
            ..
        } = &event.terms
        else {
            return Err(invalid("participants_require_conversion"));
        };
        if self.event_id != event.id
            || self.predecessor.instrument != event.instrument
            || self.successor.instrument != *next
            || next == &event.instrument
            || self.relationship.is_empty()
        {
            return Err(invalid("participant_event_identity_mismatch"));
        }
        let compact = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        for party in [&self.predecessor, &self.successor] {
            analytics_core::pool::validate_instrument(&party.instrument).map_err(invalid)?;
            if party.name.trim().is_empty() {
                return Err(invalid("participant_name_missing"));
            }
            let (d, _) = checked_quote(documents, &party.identity)?;
            let q = compact(&party.identity.quote);
            if d["instrument"] != party.instrument
                || !q.contains(&compact(&party.name))
                || !q.contains(&party.instrument[2..])
            {
                return Err(invalid("participant_identity_quote_mismatch"));
            }
        }
        for proof in &self.relationship {
            let (d, _) = checked_quote(documents, proof)?;
            let q = compact(&proof.quote);
            let terms: &[&str] = match category {
                ConversionKind::Merger => &["换股", "吸收合并"],
                ConversionKind::CodeChange => &["证券代码", "变更"],
                _ => return Err(invalid("participant_conversion_category_unsupported")),
            };
            if (d["instrument"] != self.predecessor.instrument && d["instrument"] != *next)
                || !q.contains(&compact(&self.predecessor.name))
                || !q.contains(&compact(&self.successor.name))
                || !terms.iter().all(|term| q.contains(term))
            {
                return Err(invalid("participant_relationship_quote_mismatch"));
            }
        }
        Ok(())
    }
}
pub(super) fn checked_quote<'a>(
    documents: &'a BTreeMap<String, (Value, String)>,
    proof: &FieldEvidence,
) -> io::Result<&'a (Value, String)> {
    let (d, text) = documents
        .get(&proof.document_id)
        .ok_or_else(|| invalid("evidence_document_missing"))?;
    if d["candidates"]["stage"] != "implementation"
        || proof.quote.trim().is_empty()
        || proof.unit.trim().is_empty()
        || !text.contains(&proof.quote)
    {
        return Err(invalid("evidence_quote_or_stage_invalid"));
    }
    Ok(documents.get(&proof.document_id).unwrap())
}

/// Extend only previously unqueried successors already established by published event evidence.
pub(super) fn attach_successors(
    source: &ActionBundle,
    bundle: &mut ActionBundle,
    evidence: &ActionBundle,
    prefix: &str,
) -> io::Result<()> {
    let allowed: BTreeSet<_> = source
        .actions
        .iter()
        .filter_map(|a| a.terms.successor().map(str::to_owned))
        .filter(|id| !source.queried_symbols.contains(id))
        .collect();
    if evidence.queried_symbols.is_empty()
        || evidence
            .queried_symbols
            .iter()
            .any(|id| !allowed.contains(id))
        || evidence
            .actions
            .iter()
            .any(|a| !allowed.contains(&a.instrument))
        || evidence
            .structured_coverage
            .keys()
            .any(|id| !allowed.contains(id))
        || !evidence.failures.is_empty()
        || evidence.execution_data.is_some()
        || !evidence.execution_issues.is_empty()
        || !evidence.coverage.is_empty()
    {
        return Err(invalid(
            "successor_attachment_requires_new_published_successors_without_conflicts",
        ));
    }
    for id in &evidence.queried_symbols {
        let coverage = evidence
            .structured_coverage
            .get(id)
            .ok_or_else(|| invalid("successor_attachment_complete_provider_coverage_missing"))?;
        if coverage.source != "hithink.events/per-share-v1" {
            return Err(invalid(
                "successor_attachment_requires_authenticated_primary_history",
            ));
        }
        if let Some(binding) = evidence.source_bindings.get(id) {
            bundle.source_bindings.insert(id.clone(), binding.clone());
        }
        let mut coverage = coverage.clone();
        for name in &mut coverage.evidence {
            if name.ends_with("-actions-structured.json") {
                *name = format!("{prefix}{name}");
            }
        }
        bundle.structured_coverage.insert(id.clone(), coverage);
        bundle.queried_symbols.insert(id.clone());
    }
    for event in &evidence.actions {
        if bundle.actions.iter().any(|a| a.id == event.id) {
            return Err(invalid("successor_attachment_event_conflict"));
        }
        bundle.actions.push(event.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn successor_attachment_cannot_overwrite_predecessors_or_add_unrelated_histories() {
        let source:ActionBundle=serde_json::from_value(serde_json::json!({"schema_version":9,"source_bindings":{},"coverage":{},"structured_coverage":{},"downloader":"JFQuant analytics-storage corporate-actions","downloaded_at":"fixture","queried_symbols":["SH600000"],"actions":[{"id":"SH600000:2024-01-05","instrument":"SH600000","announced":"2024-01-01","record_date":"2024-01-03","ex_date":"2024-01-05","pay_date":null,"listing_date":null,"cash_per_share":"0","shares_per_share":"0","source":"fixture","terms":{"kind":"conversion","category":"merger","last_trade_date":"2024-01-02","successor":"SZ000001","ratio":"0.5","ordinary_holder":true,"retain_original":false,"cash":"0","pay_date":null,"listing_date":"2024-01-08","reference":{"kind":"terminal","evidence":"fixture"},"allocation":{"kind":"exact"}}}],"failures":{"SH600000":"historical"},"raw_sha256":{}})).unwrap();
        let mut incoming = source.clone();
        incoming.actions.clear();
        incoming.failures.clear();
        incoming.queried_symbols = BTreeSet::from(["SZ000001".into()]);
        incoming.structured_coverage=serde_json::from_value(serde_json::json!({"SZ000001":{"dates":{"start":"2024-01-05","end":"2024-12-31"},"source":"hithink.events/per-share-v1","evidence":["SZ000001-actions-structured.json"]}})).unwrap();
        let mut merged = source.clone();
        attach_successors(&source, &mut merged, &incoming, "attached-").unwrap();
        assert_eq!(merged.actions.len(), 1);
        assert_eq!(merged.failures, source.failures);
        assert_eq!(
            merged.structured_coverage["SZ000001"].evidence,
            vec!["attached-SZ000001-actions-structured.json"]
        );
        let mut bad = incoming.clone();
        bad.queried_symbols.insert("SH600000".into());
        assert!(attach_successors(&source, &mut source.clone(), &bad, "x-").is_err());
        let mut bad = incoming;
        bad.queried_symbols.insert("SZ000002".into());
        assert!(attach_successors(&source, &mut source.clone(), &bad, "x-").is_err());
    }
}
