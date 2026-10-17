//! Read-only proposal checks shared by previews, publication and receipt replay.
use super::*;
/// One actionable problem. IDs identify the requirement, not the current error text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Stable requirement identifier within the task context.
    pub id: String,
    /// Canonical predecessor.
    pub instrument: String,
    /// Event identity, absent for coverage-only requirements.
    pub event_id: Option<String>,
    /// Required serialized fact or review scope.
    pub field: String,
    /// missing_fact, unlinked_evidence, invalid_evidence, unsupported, or waiting_identity.
    pub kind: String,
    /// Exact failing validation rule.
    pub reason: String,
    /// Existing evidence IDs, never inferred values.
    pub evidence: Vec<String>,
    /// Recommended next operation in the existing workflow.
    pub next_action: String,
}
impl Diagnostic {
    pub(crate) fn new(
        event: &CorporateAction,
        field: &str,
        kind: &str,
        reason: impl ToString,
    ) -> Self {
        let key = serde_json::json!([event.instrument, event.id, field]);
        Self {
            id: digest(&key).unwrap(),
            instrument: event.instrument.clone(),
            event_id: Some(event.id.clone()),
            field: field.into(),
            kind: kind.into(),
            reason: reason.to_string(),
            evidence: vec![],
            next_action: if kind == "waiting_identity" {
                "verify_participants"
            } else {
                "capture_or_link_evidence_then_draft"
            }
            .into(),
        }
    }
}
/// Authenticate documents once and report independent proposal errors without writing files.
pub fn validate_proposal(
    source: &ActionBundle,
    request: &RevisionRequest,
) -> io::Result<Vec<Diagnostic>> {
    if request.schema_version < 4 {
        if request.events.iter().any(|e| e.participants.is_some()) {
            return Err(invalid("participant evidence requires revision 4"));
        }
        return Ok(vec![]);
    }
    let documents = verified_documents(&request.evidence_manifest)?;
    against_documents(source, request, &documents)
}
pub(super) fn against_documents(
    source: &ActionBundle,
    request: &RevisionRequest,
    documents: &BTreeMap<String, (Value, String)>,
) -> io::Result<Vec<Diagnostic>> {
    if request.schema_version < 4 {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for suspension in &request.suspensions {
        if let Err(e) = super::suspensions::verify(suspension, documents) {
            out.push(Diagnostic {
                id: digest(suspension)?,
                instrument: suspension.instrument.clone(),
                event_id: None,
                field: "suspensions".into(),
                kind: "invalid_evidence".into(),
                reason: e.to_string(),
                evidence: vec![
                    suspension.start_evidence.document_id.clone(),
                    suspension.resume_evidence.document_id.clone(),
                ],
                next_action: "link_verified_suspension_boundaries".into(),
            });
        }
    }
    for revision in &request.events {
        task_context_check()?;
        out.extend(event_problems(source, revision, documents)?);
    }
    for resolution in &request.resolve_issues {
        if let Some(issue) = source
            .execution_issues
            .iter()
            .find(|i| i.id == resolution.id && i.code == "distribution_conflict")
            && let Err(error) = super::distribution::verify(issue, request, documents)
        {
            out.push(Diagnostic {
                id: digest(&(issue.id.clone(), "distribution_resolution"))?,
                instrument: issue.instrument.clone(),
                event_id: None,
                field: "resolve_issues".into(),
                kind: "invalid_evidence".into(),
                reason: error.to_string(),
                evidence: issue.evidence.clone(),
                next_action: "review_actual_and_reference_cash_separately".into(),
            });
        }
    }
    let root = request.evidence_manifest.parent().unwrap();
    for review in &request.coverage_reviews {
        task_context_check()?;
        let check = (|| -> io::Result<()> {
            if review.expected_source_sha256 != digest(source)? {
                return Err(invalid("coverage_source_changed"));
            }
            let shard: Value = serde_json::from_slice(&fs::read(
                root.join("raw")
                    .join(format!("{}-actions-structured.json", review.instrument)),
            )?)
            .map_err(invalid)?;
            verify_coverage_capture(root, review, &shard)
        })();
        if let Err(e) = check {
            out.push(Diagnostic {
                id: digest(&serde_json::json!([
                    review.instrument,
                    review.dates,
                    review.categories
                ]))?,
                instrument: review.instrument.clone(),
                event_id: None,
                field: "coverage_reviews".into(),
                kind: "invalid_evidence".into(),
                reason: e.to_string(),
                evidence: review.document_ids.clone(),
                next_action: "review_complete_query_receipts".into(),
            });
        }
    }
    Ok(out)
}
fn event_problems(
    source: &ActionBundle,
    revision: &EventRevision,
    documents: &BTreeMap<String, (Value, String)>,
) -> io::Result<Vec<Diagnostic>> {
    let event = &revision.replacement;
    let mut out = vec![];
    let mut push = |field: &str, kind: &str, reason: String| {
        out.push(Diagnostic::new(event, field, kind, reason))
    };
    if let Err(e) = event.validate_adjustment_terms() {
        let reason = e.to_string();
        push(
            "event",
            if reason.contains("missing") {
                "missing_fact"
            } else {
                "invalid_evidence"
            },
            reason,
        );
    }
    if revision.reason.trim().is_empty() {
        push("reason", "missing_fact", "review_reason_required".into());
    }
    let old = revision
        .original_id
        .as_ref()
        .and_then(|id| source.actions.iter().find(|e| &e.id == id));
    if revision.original_id.is_some() && old.is_none()
        || old.is_some_and(|o| {
            o.instrument != event.instrument
                || revision.original_sha256.as_ref() != digest(o).ok().as_ref()
        })
        || (old.is_none() && revision.original_sha256.is_some())
    {
        push(
            "original_sha256",
            "invalid_evidence",
            "stale_event_hash_or_identity".into(),
        );
    }
    let identity_ready = if let Some(parties) = &revision.participants {
        match parties.verify(event, documents) {
            Ok(()) => true,
            Err(e) => {
                push("participants", "unlinked_evidence", e.to_string());
                false
            }
        }
    } else {
        true
    };
    let original = old
        .map(serde_json::to_value)
        .transpose()
        .map_err(invalid)?
        .unwrap_or(serde_json::json!({}));
    let replacement = serde_json::to_value(event).map_err(invalid)?;
    let mut leaves = vec![];
    changed_leaves("", &original, &replacement, &mut leaves);
    for (path, value) in leaves {
        let path = path.trim_start_matches('.');
        let pointer = format!("/{}", path.replace('.', "/"));
        if original.pointer(&pointer).is_none() && value.is_null() {
            continue;
        }
        let proof = revision
            .fields
            .iter()
            .filter(|(p, _)| path == p.as_str() || path.starts_with(&format!("{p}.")))
            .max_by_key(|(p, _)| p.len())
            .map(|(_, p)| p);
        if let Some(proof) = proof {
            let foreign = documents
                .get(&proof.document_id)
                .is_some_and(|(d, _)| d["instrument"] != event.instrument);
            if foreign && (!identity_ready || revision.participants.is_none()) {
                push(
                    path,
                    "waiting_identity",
                    "successor_document_requires_verified_participants".into(),
                );
                continue;
            }
            if let Err(e) = verify_leaf_document(
                documents,
                proof,
                event,
                path,
                revision.participants.as_ref(),
            ) {
                push(path, "invalid_evidence", e.to_string());
            }
        } else if let Some(reference) = revision.structured_fields.get(path) {
            if let Err(e) = verify_structured(source, event, path, &value, reference) {
                push(path, "invalid_evidence", e.to_string());
            }
        } else {
            push(
                path,
                "unlinked_evidence",
                "changed_field_lacks_evidence; inspect captured documents before collecting again"
                    .into(),
            );
        }
    }
    for p in &mut out {
        p.evidence = revision
            .fields
            .get(&p.field)
            .map(|p| vec![p.document_id.clone()])
            .unwrap_or_default();
    }
    Ok(out)
}
/// Validate new-format evidence after re-reading and hashing the immutable bundle.
pub(super) fn replay(bundle: &ActionBundle, root: &Path, receipt: &Value) -> io::Result<()> {
    let request: RevisionRequest =
        serde_json::from_value(receipt["request"].clone()).map_err(invalid)?;
    let mut documents = BTreeMap::new();
    let mut verified_issuers = BTreeSet::new();
    for d in receipt["captured_documents"]
        .as_array()
        .ok_or_else(|| invalid("review_documents_missing"))?
    {
        for (field, hash_field) in [("pdf", "pdf_sha256"), ("text", "text_sha256")] {
            let name = d[field]
                .as_str()
                .ok_or_else(|| invalid("review_file_missing"))?;
            if Path::new(name).file_name().and_then(|s| s.to_str()) != Some(name) {
                return Err(invalid("unsafe_review_file"));
            }
            let bytes = fs::read(root.join("raw").join(name))?;
            let hash = format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(bytes))
            );
            if d[hash_field] != hash || bundle.raw_sha256.get(name) != Some(&hash) {
                return Err(invalid("review_document_changed"));
            }
        }
        let issuer = d["instrument"]
            .as_str()
            .ok_or_else(|| invalid("review_issuer_missing"))?;
        let shard = &receipt["collection_receipts"][issuer];
        if verified_issuers.insert(issuer) {
            crate::collector::verify_http_receipts(root, &shard["evidence"])?;
        }
        if !shard["evidence"]["documents"]
            .as_array()
            .is_some_and(|ds| ds.contains(d))
            || !shard["evidence"]["http_receipts"]
                .as_array()
                .is_some_and(|rs| {
                    rs.iter().any(|r| {
                        r["url"] == d["url"] && r["sha256"] == d["pdf_sha256"] && r["status"] == 200
                    })
                })
        {
            return Err(invalid("review_http_identity_mismatch"));
        }
        let id = d["id"]
            .as_str()
            .ok_or_else(|| invalid("review_document_id_missing"))?;
        let text = fs::read_to_string(root.join("raw").join(d["text"].as_str().unwrap()))?;
        if documents.insert(id.to_owned(), (d.clone(), text)).is_some() {
            return Err(invalid("review_duplicate_document"));
        }
    }
    super::suspensions::replay(bundle, &request, &documents)?;
    super::distribution::replay(bundle, &request, receipt, &documents)?;
    let mut source = bundle.clone();
    source.actions = serde_json::from_value(receipt["source_events"].clone()).map_err(invalid)?;
    for revision in &request.events {
        task_context_check()?;
        let problems = event_problems(&source, revision, &documents)?;
        if !problems.is_empty() {
            return Err(invalid(serde_json::to_string(&problems).map_err(invalid)?));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corporate_actions::evidence::{CaptureRequest, DocumentSpec, capture_with};
    use analytics_core::task::TaskContext;
    fn fixture(root: &Path) -> (ActionBundle, RevisionRequest) {
        let old =
            "600000 OldCo NewCo 换股吸收合并; ratio 0.5, record 2024-01-03; final trade 2024-01-02";
        let new = "000001 NewCo OldCo 换股吸收合并; registered 2024-01-05, listed 2024-01-08";
        let spec = |id: &str, instrument: &str, announced: &str| DocumentSpec {
            id: id.into(),
            instrument: instrument.into(),
            url: format!("https://example.org/{id}.pdf"),
            announced: announced.parse().unwrap(),
            title: "Co".into(),
            implementation: true,
        };
        capture_with(&CaptureRequest {schema_version:1,documents:vec![spec("old","SH600000","2024-01-01"),spec("new","SZ000001","2024-01-09")]},&root.join("capture"),&TaskContext::default(),|_,p|{fs::write(p,b"%PDF-fixture")?;Ok((200,Some(0)))},|p|Ok(serde_json::json!({"text":if p.to_string_lossy().contains("SH600000"){old}else{new},"pypdf_version":"fixture"}))).unwrap();
        let book:ActionBundle=serde_json::from_value(serde_json::json!({"schema_version":8,"source_bindings":{},"coverage":{},"structured_coverage":{},"downloader":"JFQuant analytics-storage corporate-actions","downloaded_at":"fixture","queried_symbols":["SH600000"],"actions":[],"failures":{"SH600000":"historical"},"raw_sha256":{}})).unwrap();
        let event:CorporateAction=serde_json::from_value(serde_json::json!({"id":"SH600000:2024-01-05","instrument":"SH600000","announced":"2024-01-01","record_date":"2024-01-03","ex_date":"2024-01-05","pay_date":null,"listing_date":null,"cash_per_share":"0","shares_per_share":"0","source":"https://example.org/old.pdf https://example.org/new.pdf","terms":{"kind":"conversion","category":"merger","last_trade_date":"2024-01-02","successor":"SZ000001","ratio":"0.5","ordinary_holder":true,"retain_original":false,"cash":"0","pay_date":null,"listing_date":"2024-01-08","reference":{"kind":"unchanged","evidence":"original raw reference"},"allocation":{"kind":"ranked_remainder","evidence":"verified allocation rule"}}})).unwrap();
        let proof = |id: &str, quote: &str| FieldEvidence {
            document_id: id.into(),
            quote: quote.into(),
            unit: "fixture_fact".into(),
        };
        let participants = Participants {
            event_id: event.id.clone(),
            predecessor: Participant {
                instrument: event.instrument.clone(),
                name: "OldCo".into(),
                identity: proof("old", old),
            },
            successor: Participant {
                instrument: "SZ000001".into(),
                name: "NewCo".into(),
                identity: proof("new", new),
            },
            relationship: vec![proof("old", old), proof("new", new)],
        };
        let mut leaves = vec![];
        changed_leaves(
            "",
            &serde_json::json!({}),
            &serde_json::to_value(&event).unwrap(),
            &mut leaves,
        );
        let fields = leaves
            .into_iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(p, _)| {
                let p = p.trim_start_matches('.').to_owned();
                let q = if matches!(p.as_str(), "ex_date" | "terms.listing_date") {
                    proof("new", new)
                } else {
                    proof("old", old)
                };
                (p, q)
            })
            .collect();
        let request = RevisionRequest {
            suspensions: vec![],
            schema_version: 4,
            evidence_manifest: root.join("capture/announcements.json"),
            structured_manifest: None,
            events: vec![EventRevision {
                participants: Some(participants),
                structured_fields: Default::default(),
                original_id: None,
                original_sha256: None,
                replacement: event,
                reason: "reviewed merger".into(),
                fields,
            }],
            resolve_failures: vec![],
            resolve_issues: vec![],
            coverage_reviews: vec![],
        };
        (book, request)
    }
    #[test]
    fn successor_settlement_replays_without_claiming_history_or_inventing_fractional_awards() {
        let tmp = tempfile::tempdir().unwrap();
        let (book, request) = fixture(tmp.path());
        assert!(validate_proposal(&book, &request).unwrap().is_empty());
        let output = tmp.path().join("published");
        let next = apply(&book, tmp.path(), &request, &output).unwrap();
        let read = ActionBundle::read(&output.join("actions.json")).unwrap();
        assert_eq!(read.schema_version, 9);
        assert!(read.coverage.is_empty());
        assert_eq!(next.failures, book.failures);
        let mut tampered = read.clone();
        if let analytics_core::actions::EventTerms::Conversion { ratio, .. } =
            &mut tampered.actions[0].terms
        {
            *ratio = "0.6".into();
        }
        fs::write(
            output.join("actions.json"),
            serde_json::to_vec_pretty(&tampered).unwrap(),
        )
        .unwrap();
        assert!(ActionBundle::read(&output.join("actions.json")).is_err());
        fs::write(
            output.join("actions.json"),
            serde_json::to_vec_pretty(&read).unwrap(),
        )
        .unwrap();
        let allocation = read.actions[0].terms.allocation().unwrap();
        assert!(allocation.allocate(3, 0.5).is_err());
        assert_eq!(allocation.allocate(4, 0.5).unwrap().0, 2);
        let file = output.join("raw/SZ000001-new.txt");
        fs::write(file, b"tampered").unwrap();
        assert!(ActionBundle::snapshot_content(&output.join("actions.json")).is_ok());
        assert!(ActionBundle::read(&output.join("actions.json")).is_err());
    }
    #[test]
    fn independent_errors_and_waiting_identity_do_not_become_missing_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let (book, mut request) = fixture(tmp.path());
        request.events[0]
            .participants
            .as_mut()
            .unwrap()
            .successor
            .identity
            .quote = "unverified".into();
        request.events[0]
            .fields
            .get_mut("terms.ratio")
            .unwrap()
            .quote = "invented ratio".into();
        let problems = validate_proposal(&book, &request).unwrap();
        assert!(problems.iter().any(|p| p.field == "participants"));
        assert!(
            problems
                .iter()
                .any(|p| p.field == "terms.ratio" && p.kind == "invalid_evidence")
        );
        assert!(
            problems
                .iter()
                .any(|p| p.field == "terms.listing_date" && p.kind == "waiting_identity")
        );
        assert!(problems.iter().all(|p| p.kind != "missing_fact"));
        assert!(!tmp.path().join("published").exists());
    }
    #[test]
    fn unrelated_issuer_circular_reference_and_late_decision_proof_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let (book, request) = fixture(tmp.path());
        let mut changed = request.clone();
        changed.events[0]
            .participants
            .as_mut()
            .unwrap()
            .successor
            .instrument = "SZ000002".into();
        assert!(!validate_proposal(&book, &changed).unwrap().is_empty());
        let mut changed = request.clone();
        changed.events[0]
            .participants
            .as_mut()
            .unwrap()
            .relationship[0]
            .document_id = "review-proposal".into();
        assert!(!validate_proposal(&book, &changed).unwrap().is_empty());
        let mut changed = request.clone();
        changed.events[0].fields.insert(
            "terms.ratio".into(),
            request.events[0].fields["ex_date"].clone(),
        );
        assert!(!validate_proposal(&book, &changed).unwrap().is_empty());
        let mut changed = request;
        changed.schema_version = 3;
        assert!(validate_proposal(&book, &changed).is_err());
    }
}
