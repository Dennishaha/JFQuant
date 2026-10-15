//! Immutable, evidence-backed event corrections; never delete a failure marker alone.
use super::*;
use std::path::PathBuf;
/// A concrete field quote from a captured implementation document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldEvidence {
    /// Document identity within the selected manifest.
    pub document_id: String,
    /// Exact source excerpt, checked against the hashed extracted text.
    pub quote: String,
    /// Original source unit/meaning (e.g. per_ten_shares, CNY, calendar_date).
    pub unit: String,
}
/// One reviewed event replacement or new event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventRevision {
    /// Existing logical event ID, None for a verified missing event.
    pub original_id: Option<String>,
    /// Exact original serialized event hash; required for replacements.
    pub original_sha256: Option<String>,
    /// Reviewed ordinary-holder event.
    pub replacement: CorporateAction,
    /// Why the evidence resolves missing data or a source disagreement.
    pub reason: String,
    /// Evidence for all changed fields, named by serialized field name.
    pub fields: BTreeMap<String, FieldEvidence>,
}
/// Explicit resolution of a recorded source failure, requiring an actual event correction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureResolution {
    /// Canonical affected security.
    pub instrument: String,
    /// Exact stale failure prevents racing another source version.
    pub expected_failure: String,
    /// Reviewed explanation.
    pub reason: String,
}
/// Resolve one ordinary issue using an identity/date-matched implementation revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueResolution {
    /// Stable issue identity from format 6 reconciliation.
    pub id: String,
    /// Exact serialized issue hash, preventing stale or overbroad closure.
    pub expected_sha256: String,
    /// Evidence-based reason for closing this particular issue.
    pub reason: String,
}
/// Shared UI/CLI correction request. Paths resolve within the workspace in workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionRequest {
    /// Version of this review request, 1.
    pub schema_version: u32,
    /// Captured announcements.json from this project's collector.
    pub evidence_manifest: PathBuf,
    /// Event corrections, at least one.
    pub events: Vec<EventRevision>,
    /// Explicit source-failure resolutions.
    pub resolve_failures: Vec<FailureResolution>,
    /// Exact ordinary issues resolved by the provided event revisions.
    #[serde(default)]
    pub resolve_issues: Vec<IssueResolution>,
    /// Audited category coverage. Empty does not claim completeness.
    #[serde(default)]
    pub coverage_reviews: Vec<CoverageReview>,
}
/// Human-reviewed exhaustive disclosure coverage, kept separately from individual event edits.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageReview {
    /// Security whose complete captured disclosure metadata was reviewed.
    pub instrument: String,
    /// Event dates supported by that review, not merely query announcement dates.
    pub dates: analytics_core::DateRange,
    /// Categories independently reviewed.
    pub categories: Vec<String>,
    /// Matching captured implementation documents; no-events reviews may be empty.
    pub document_ids: Vec<String>,
    /// Exact source bundle hash prevents applying a review to different candidates.
    pub expected_source_sha256: String,
    /// Why the captured pages establish completeness for the event dates/categories.
    pub reason: String,
}
fn digest(value: &impl Serialize) -> io::Result<String> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(invalid)?)
    ))
}
/// Read a captured document, rehash its bytes and text and reject non-implementation candidates.
pub fn verified_documents(manifest: &Path) -> io::Result<BTreeMap<String, (Value, String)>> {
    let value: Value = serde_json::from_slice(&fs::read(manifest)?).map_err(invalid)?;
    verified_document_records(
        manifest
            .parent()
            .ok_or_else(|| invalid("missing announcement root"))?,
        &value,
    )
}
pub(super) fn verified_document_records(
    base: &Path,
    value: &Value,
) -> io::Result<BTreeMap<String, (Value, String)>> {
    if value["schema_version"] != 2 {
        return Err(invalid("unsupported announcement evidence format"));
    }
    let root = base.join("raw");
    let mut captured = Vec::new();
    for (id, expected) in value["receipts"]
        .as_object()
        .ok_or_else(|| invalid("missing announcement checkpoint receipts"))?
    {
        analytics_core::pool::validate_instrument(id).map_err(invalid)?;
        let bytes = fs::read(root.join(format!("{id}-actions-structured.json")))?;
        if *expected != format!("sha256:{:x}", Sha256::digest(&bytes)) {
            return Err(invalid("announcement metadata receipt mismatch"));
        }
        let shard: Value = serde_json::from_slice(&bytes).map_err(invalid)?;
        crate::collector::verify_http_receipts(base, &shard["evidence"])?;
        let documents = shard["evidence"]["documents"]
            .as_array()
            .ok_or_else(|| invalid("missing captured document metadata"))?;
        for d in documents {
            if d["instrument"] != *id {
                return Err(invalid("announcement identity conflict"));
            }
            if !shard["evidence"]["http_receipts"]
                .as_array()
                .is_some_and(|rs| {
                    rs.iter().any(|r| {
                        r["url"] == d["url"] && r["sha256"] == d["pdf_sha256"] && r["status"] == 200
                    })
                })
            {
                return Err(invalid("implementation PDF lacks matching HTTP proof"));
            }
            captured.push(d.clone());
        }
    }
    if value["documents"] != serde_json::json!(captured) {
        return Err(invalid(
            "announcement manifest differs from captured metadata",
        ));
    }
    let mut out = BTreeMap::new();
    for d in value["documents"]
        .as_array()
        .ok_or_else(|| invalid("missing document list"))?
    {
        let id = d["id"]
            .as_str()
            .ok_or_else(|| invalid("missing document ID"))?;
        for (f, h) in [("pdf", "pdf_sha256"), ("text", "text_sha256")] {
            let name = d[f]
                .as_str()
                .ok_or_else(|| invalid("missing evidence file"))?;
            if Path::new(name)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
                || Path::new(name).components().count() != 1
            {
                return Err(invalid("invalid evidence path"));
            }
            let bytes = fs::read(root.join(name))?;
            if d[h] != format!("sha256:{:x}", Sha256::digest(bytes)) {
                return Err(invalid("announcement evidence hash mismatch"));
            }
        }
        let text = fs::read_to_string(root.join(d["text"].as_str().unwrap()))?;
        if out.insert(id.to_string(), (d.clone(), text)).is_some() {
            return Err(invalid("duplicate announcement ID"));
        }
    }
    Ok(out)
}
/// Produce a new current-format action bundle; source files are never rewritten.
pub fn apply(
    source: &ActionBundle,
    source_root: &Path,
    request: &RevisionRequest,
    output: &Path,
) -> io::Result<ActionBundle> {
    source.validate_identity()?;
    if request.schema_version != 1
        || (request.events.is_empty() && request.coverage_reviews.is_empty())
    {
        return Err(invalid("invalid/empty action revision"));
    }
    let documents = verified_documents(&request.evidence_manifest)?;
    let mut bundle = source.clone();
    let mut changed = BTreeSet::new();
    let mut used = BTreeSet::new();
    let manifest: Value =
        serde_json::from_slice(&fs::read(&request.evidence_manifest)?).map_err(invalid)?;
    for review in &request.coverage_reviews {
        review.dates.validate().map_err(invalid)?;
        if review.reason.trim().is_empty()
            || review.categories.is_empty()
            || review.expected_source_sha256 != digest(source)?
            || !source.queried_symbols.contains(&review.instrument)
            || source.failures.contains_key(&review.instrument)
        {
            return Err(invalid(
                "coverage review requires verified source identity and stale hash",
            ));
        }
        let receipt = manifest["receipts"]
            .get(&review.instrument)
            .ok_or_else(|| invalid("coverage review source receipt missing"))?;
        let root = request.evidence_manifest.parent().unwrap();
        let shard: Value = serde_json::from_slice(&fs::read(
            root.join("raw")
                .join(format!("{}-actions-structured.json", review.instrument)),
        )?)
        .map_err(invalid)?;
        if receipt.is_null()
            || shard["evidence"]["pagination_complete"] != true
            || !shard["evidence"]["all_announcements"].is_array()
        {
            return Err(invalid(
                "coverage review requires complete unfiltered announcement metadata",
            ));
        }
        let queried_start: Date = serde_json::from_value(
            shard["evidence"]["requested_announcement_dates"]["start"].clone(),
        )
        .map_err(invalid)?;
        let queried_end: Date = serde_json::from_value(
            shard["evidence"]["requested_announcement_dates"]["end"].clone(),
        )
        .map_err(invalid)?;
        if queried_start > review.dates.start || queried_end < review.dates.end {
            return Err(invalid("coverage review exceeds captured disclosure dates"));
        }
        let captured: BTreeSet<_> = shard["evidence"]["documents"]
            .as_array()
            .ok_or_else(|| invalid("coverage documents missing"))?
            .iter()
            .filter_map(|d| d["id"].as_str())
            .collect();
        let requested: BTreeSet<_> = review.document_ids.iter().map(String::as_str).collect();
        if requested != captured {
            return Err(invalid(
                "coverage review must include every captured action candidate",
            ));
        }
        for id in &review.document_ids {
            if !documents.contains_key(id) {
                return Err(invalid("coverage document missing or altered"));
            }
            used.insert(id.clone());
        }
        for category in &review.categories {
            if !EVENT_CATEGORIES.contains(&category.as_str()) {
                return Err(invalid("unsupported event category"));
            }
            bundle
                .coverage
                .entry(review.instrument.clone())
                .or_default()
                .push(EventCoverage {
                    category: category.clone(),
                    dates: review.dates.clone(),
                    status: analytics_core::admission::CheckStatus::Passed,
                    evidence: vec!["pending-review-receipt".into()],
                    reason: review.reason.clone(),
                });
        }
    }
    for revision in &request.events {
        let a = &revision.replacement;
        a.validate_adjustment_terms().map_err(invalid)?;
        if revision.reason.trim().is_empty() || revision.fields.is_empty() {
            return Err(invalid("review reason and field evidence required"));
        }
        let old = revision
            .original_id
            .as_ref()
            .map(|id| {
                source
                    .actions
                    .iter()
                    .find(|a| a.id == *id)
                    .ok_or_else(|| invalid("original event not found"))
            })
            .transpose()?;
        if let Some(old) = old {
            if revision.original_sha256.as_ref() != Some(&digest(old)?)
                || old.instrument != a.instrument
            {
                return Err(invalid("stale event hash/security conflict"));
            }
            bundle.actions.retain(|v| v.id != old.id);
        } else if revision.original_sha256.is_some() {
            return Err(invalid("unexpected original event hash"));
        }
        let original = old
            .map(serde_json::to_value)
            .transpose()
            .map_err(invalid)?
            .unwrap_or(serde_json::json!({}));
        let replacement = serde_json::to_value(a).map_err(invalid)?;
        if original == replacement {
            return Err(invalid("no-op revision cannot resolve a source failure"));
        }
        for (field, value) in replacement.as_object().unwrap() {
            if original.get(field) == Some(value) {
                continue;
            }
            let proof = revision
                .fields
                .get(field)
                .ok_or_else(|| invalid(format!("changed field lacks evidence: {field}")))?;
            let (document, text) = documents
                .get(&proof.document_id)
                .ok_or_else(|| invalid("unknown implementation document"))?;
            if document["instrument"] != a.instrument
                || document["candidates"]["stage"] != "implementation"
                || proof.quote.trim().is_empty()
                || !text.contains(&proof.quote)
                || proof.unit.is_empty()
                || document["url"]
                    .as_str()
                    .is_none_or(|u| !a.source.contains(u))
            {
                return Err(invalid("field evidence identity/stage/quote mismatch"));
            }
            let announced: Date = document["announced"]
                .as_str()
                .ok_or_else(|| invalid("missing announcement date"))?
                .parse()
                .map_err(invalid)?;
            if a.record_date.is_some_and(|d| announced > d) {
                return Err(invalid(
                    "implementation announcement after entitlement date",
                ));
            }
            used.insert(proof.document_id.clone());
        }
        if bundle.actions.iter().any(|v| v.id == a.id) {
            return Err(invalid("duplicate corrected event"));
        }
        bundle.actions.push(a.clone());
        changed.insert(a.instrument.clone());
    }
    for resolution in &request.resolve_failures {
        if resolution.reason.trim().is_empty()
            || !changed.contains(&resolution.instrument)
            || source.failures.get(&resolution.instrument) != Some(&resolution.expected_failure)
        {
            return Err(invalid(
                "failure resolution requires matching event correction and stale failure",
            ));
        }
        bundle.failures.remove(&resolution.instrument);
    }
    for resolution in &request.resolve_issues {
        let issue = source
            .execution_issues
            .iter()
            .find(|i| !i.id.is_empty() && i.id == resolution.id)
            .ok_or_else(|| invalid("issue resolution requires a stable existing issue"))?;
        if resolution.reason.trim().is_empty()
            || resolution.expected_sha256 != digest(issue)?
            || !matches!(
                issue.code.as_str(),
                "source_event_conflict"
                    | "cninfo_history_gap"
                    | "cninfo_event_date_missing"
                    | "cninfo_event_chronology_conflict"
                    | "event_amount_missing"
                    | "event_amount_invalid"
                    | "distribution_basis_unresolved"
            )
            || !request.events.iter().any(|revision| {
                let a = &revision.replacement;
                a.instrument == issue.instrument
                    && (if issue.dates.start == issue.dates.end {
                        a.ex_date == issue.dates.start
                    } else {
                        issue.announced.is_some() && a.announced == issue.announced
                    })
                    && (revision.fields.contains_key("ex_date")
                        || revision.fields.contains_key("cash_per_share")
                        || revision.fields.contains_key("shares_per_share"))
            })
        {
            return Err(invalid(
                "issue resolution requires matching field/date evidence and stale issue hash",
            ));
        }
        bundle.execution_issues.retain(|i| i.id != resolution.id);
    }
    // Copy and hash all source evidence before publishing the corrected bundle.
    fs::create_dir(output)?;
    fs::create_dir(output.join("raw"))?;
    for (name, expected) in &source.raw_sha256 {
        let bytes = fs::read(source_root.join("raw").join(name))?;
        if format!("sha256:{:x}", Sha256::digest(&bytes)) != *expected {
            return Err(invalid("source action evidence hash mismatch"));
        }
        fs::write(output.join("raw").join(name), bytes)?;
    }
    // Archive the preceding normalization receipt: this publication is superseded
    // by explicit, hashed announcement revisions, rather than provider replay.
    if let Some(hash) = bundle.raw_sha256.remove("reconciliation-v1.json") {
        let archive = format!("previous-reconciliation-{}.json", &hash[7..]);
        fs::rename(
            output.join("raw/reconciliation-v1.json"),
            output.join("raw").join(&archive),
        )?;
        bundle.raw_sha256.insert(archive, hash);
    }
    bundle.schema_version = 6;
    let selected_documents: Vec<_> = used.iter().map(|id| documents[id].0.clone()).collect();
    let evidence_root = request.evidence_manifest.parent().unwrap();
    let mut collection_receipts = BTreeMap::new();
    let covered_instruments: BTreeSet<String> = selected_documents
        .iter()
        .filter_map(|d| d["instrument"].as_str().map(str::to_owned))
        .chain(
            request
                .coverage_reviews
                .iter()
                .map(|r| r.instrument.clone()),
        )
        .collect();
    for instrument in &covered_instruments {
        let instrument = instrument.as_str();
        if collection_receipts.contains_key(instrument) {
            continue;
        }
        let shard: Value = serde_json::from_slice(&fs::read(
            evidence_root
                .join("raw")
                .join(format!("{instrument}-actions-structured.json")),
        )?)
        .map_err(invalid)?;
        for r in shard["evidence"]["http_receipts"]
            .as_array()
            .ok_or_else(|| invalid("missing captured HTTP receipts"))?
        {
            let name = r["filename"]
                .as_str()
                .ok_or_else(|| invalid("missing receipt filename"))?;
            let bytes = fs::read(evidence_root.join("raw").join(name))?;
            let hash = format!("sha256:{:x}", Sha256::digest(&bytes));
            if bundle.raw_sha256.get(name).is_some_and(|h| h != &hash) {
                return Err(invalid("HTTP evidence name collision"));
            }
            fs::write(output.join("raw").join(name), bytes)?;
            bundle.raw_sha256.insert(name.into(), hash);
        }
        collection_receipts.insert(instrument.to_string(), shard);
    }
    let manifest_bytes = fs::read(&request.evidence_manifest)?;
    let manifest: Value = serde_json::from_slice(&manifest_bytes).map_err(invalid)?;
    for id in used {
        let d = &documents[&id].0;
        for field in ["pdf", "text"] {
            let name = d[field].as_str().unwrap();
            let bytes = fs::read(
                request
                    .evidence_manifest
                    .parent()
                    .unwrap()
                    .join("raw")
                    .join(name),
            )?;
            let hash = format!("sha256:{:x}", Sha256::digest(&bytes));
            if bundle.raw_sha256.get(name).is_some_and(|h| h != &hash) {
                return Err(invalid("evidence filename collision"));
            }
            fs::write(output.join("raw").join(name), bytes)?;
            bundle.raw_sha256.insert(name.into(), hash);
        }
    }
    let receipt = serde_json::json!({"schema_version":1,"source_sha256":digest(source)?,"request":request,"source_events":source.actions,"captured_documents":selected_documents,"collection_receipts":collection_receipts,"collector_completion":manifest["complete"],"evidence_manifest_sha256":format!("sha256:{:x}",Sha256::digest(&manifest_bytes)),"applied_at":chrono::Utc::now().to_rfc3339()});
    let name = format!("review-{}.json", &digest(&receipt)?[7..23]);
    let bytes = serde_json::to_vec_pretty(&receipt).map_err(invalid)?;
    bundle
        .raw_sha256
        .insert(name.clone(), format!("sha256:{:x}", Sha256::digest(&bytes)));
    fs::write(output.join("raw").join(&name), bytes)?;
    let review_name = name;
    for claims in bundle.coverage.values_mut() {
        for claim in claims {
            for proof in &mut claim.evidence {
                if proof == "pending-review-receipt" {
                    *proof = review_name.clone();
                }
            }
        }
    }
    bundle
        .actions
        .sort_by_key(|a| (a.instrument.clone(), a.ex_date, a.id.clone()));
    bundle.downloaded_at = chrono::Utc::now().to_rfc3339();
    fs::write(
        output.join("actions.json"),
        serde_json::to_vec_pretty(&bundle).map_err(invalid)?,
    )?;
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sha(bytes: &[u8]) -> String {
        format!("sha256:{:x}", Sha256::digest(bytes))
    }
    #[test]
    fn revision_requires_stale_hash_matching_implementation_quotes_and_new_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("raw")).unwrap();
        let mut a = CorporateAction {
            id: "SH600000:2024-06-04".into(),
            instrument: "SH600000".into(),
            announced: Some("2024-06-01".parse().unwrap()),
            record_date: Some("2024-06-03".parse().unwrap()),
            ex_date: "2024-06-04".parse().unwrap(),
            pay_date: None,
            listing_date: None,
            cash_per_share: (1.).to_string(),
            shares_per_share: (0.).to_string(),
            terms: Default::default(),
            source: "https://static.cninfo.com.cn/finalpage/2024-06-01/a.PDF".into(),
        };
        let book = ActionBundle {
            structured_coverage: Default::default(),
            execution_issues: Vec::new(),
            execution_data: None,
            schema_version: 5,
            source_bindings: Default::default(),
            coverage: Default::default(),
            downloader: "JFQuant analytics-storage corporate-actions".into(),
            downloaded_at: "original".into(),
            queried_symbols: BTreeSet::from([a.instrument.clone()]),
            actions: vec![a.clone()],
            failures: BTreeMap::from([(a.instrument.clone(), "sources disagree".into())]),
            raw_sha256: BTreeMap::new(),
        };
        let text = "verified actual cash payment 2024-06-06; gross dividend per share 0.8 CNY";
        fs::write(root.join("raw/a.txt"), text).unwrap();
        fs::write(root.join("raw/a.pdf"), b"%PDF-evidence").unwrap();
        let document = serde_json::json!({"id":"a","instrument":a.instrument,"announced":a.announced,"url":a.source,"pdf":"a.pdf","pdf_sha256":sha(b"%PDF-evidence"),"text":"a.txt","text_sha256":sha(text.as_bytes()),"candidates":{"stage":"implementation"}});
        fs::write(root.join("raw/http-pdf.body"), b"%PDF-evidence").unwrap();
        let shard = serde_json::json!({"instrument":"SH600000","evidence":{"pagination_complete":true,"documents":[document.clone()],"http_receipts":[{"url":a.source,"status":200,"filename":"http-pdf.body","sha256":sha(b"%PDF-evidence")}]}});
        let shard_bytes = serde_json::to_vec(&shard).unwrap();
        fs::write(
            root.join("raw/SH600000-actions-structured.json"),
            &shard_bytes,
        )
        .unwrap();
        fs::write(
            root.join("announcements.json"),
            serde_json::to_vec(&serde_json::json!({"schema_version":2,"documents":[document],"receipts":{"SH600000":sha(&shard_bytes)}}))
                .unwrap(),
        )
        .unwrap();
        // A failed batch can still contain a complete, immutable security receipt.
        let owner = serde_json::json!({"operation":"announcements","collector_protocol":9,"collector_signature":crate::collector::REVIEWED_COLLECTOR_V9,"normalization":crate::adjustment::NORMALIZATION,"dependencies_sha256":format!("sha256:{:x}",Sha256::digest(include_bytes!("../../collectors/requirements.lock"))),"python_runtime_sha256":format!("sha256:{:x}",Sha256::digest(include_bytes!("../../collectors/python-runtime.lock.json"))),"request":{"provider":"akshare","adjustment_source":"sina_hfq","start":"2024-06-01","end":"2024-06-30","instruments":["SH600000"],"source_bindings":{},"benchmarks":[],"universes":[]}});
        fs::write(root.join("owner.json"), serde_json::to_vec(&owner).unwrap()).unwrap();
        fs::write(
            root.join("receipts.json"),
            serde_json::to_vec(&serde_json::json!({"SH600000":sha(&shard_bytes)})).unwrap(),
        )
        .unwrap();
        let manifest = fs::read(root.join("announcements.json")).unwrap();
        fs::remove_file(root.join("announcements.json")).unwrap();
        let frozen = root.join("frozen-evidence");
        fs::create_dir_all(frozen.join("raw")).unwrap();
        let name =
            super::super::reconcile::announcement_evidence(root, &frozen, &mut BTreeMap::new())
                .unwrap();
        assert!(frozen.join("raw").join(name).exists());
        assert!(!root.join("announcements.json").exists());
        fs::write(root.join("announcements.json"), manifest).unwrap();
        let original = digest(&a).unwrap();
        a.pay_date = Some("2024-06-06".parse().unwrap());
        let mut request = RevisionRequest {
            coverage_reviews: Vec::new(),
            schema_version: 1,
            evidence_manifest: root.join("announcements.json"),
            events: vec![EventRevision {
                original_id: Some(a.id.clone()),
                original_sha256: Some(original),
                replacement: a.clone(),
                reason: "implementation fixes missing payment".into(),
                fields: BTreeMap::from([(
                    "pay_date".into(),
                    FieldEvidence {
                        document_id: "a".into(),
                        quote: text.into(),
                        unit: "calendar_date".into(),
                    },
                )]),
            }],
            resolve_issues: vec![],
            resolve_failures: vec![FailureResolution {
                instrument: a.instrument.clone(),
                expected_failure: "sources disagree".into(),
                reason: "reviewed implementation".into(),
            }],
        };
        let repaired = apply(&book, root, &request, &root.join("fixed")).unwrap();
        assert!(repaired.failures.is_empty());
        assert!(book.actions[0].pay_date.is_none());
        assert_eq!(repaired.actions[0].pay_date, a.pay_date);
        assert!(repaired.raw_sha256.contains_key("http-pdf.body"));
        let mut conflicted = book.clone();
        let issue = crate::execution_data::ExecutionIssue {
            id: "ordinary-conflict".into(),
            instrument: a.instrument.clone(),
            announced: a.announced,
            dates: analytics_core::DateRange {
                start: a.ex_date,
                end: a.ex_date,
            },
            code: "source_event_conflict".into(),
            event_category: "distribution".into(),
            evidence: vec!["original-terms".into()],
            message: "cash conflict".into(),
        };
        conflicted.execution_issues.push(issue.clone());
        let mut correction = request.clone();
        correction.events[0].replacement.cash_per_share = "0.8".into();
        correction.events[0].fields.insert(
            "cash_per_share".into(),
            FieldEvidence {
                document_id: "a".into(),
                quote: text.into(),
                unit: "gross_CNY_per_old_share".into(),
            },
        );
        correction.resolve_issues.push(IssueResolution {
            id: issue.id.clone(),
            expected_sha256: digest(&issue).unwrap(),
            reason: "implementation resolves this cash amount".into(),
        });
        let fixed = apply(&conflicted, root, &correction, &root.join("cash-fixed")).unwrap();
        assert!(fixed.execution_issues.is_empty());
        correction.resolve_issues[0].expected_sha256 = "stale".into();
        assert!(apply(&conflicted, root, &correction, &root.join("stale-issue")).is_err());
        assert!(!root.join("stale-issue").exists());
        let mut noop = request.clone();
        noop.events[0].replacement = book.actions[0].clone();
        assert!(apply(&book, root, &noop, &root.join("noop")).is_err());
        assert!(!root.join("noop").exists());
        request.events[0].original_sha256 = Some("wrong".into());
        assert!(apply(&book, root, &request, &root.join("bad")).is_err());
        assert!(!root.join("bad").exists());
        fs::write(root.join("raw/a.txt"), "tampered").unwrap();
        assert!(verified_documents(&root.join("announcements.json")).is_err());
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    #[test]
    fn category_review_requires_full_metadata_and_survives_portable_receipt_verification() {
        let d = tempfile::tempdir().unwrap();
        let original = d.path().join("source");
        let evidence = d.path().join("evidence");
        fs::create_dir_all(original.join("raw")).unwrap();
        fs::create_dir_all(evidence.join("raw")).unwrap();
        let source = ActionBundle {
            structured_coverage: Default::default(),
            execution_issues: Vec::new(),
            execution_data: None,
            schema_version: 5,
            source_bindings: Default::default(),
            coverage: Default::default(),
            downloader: "JFQuant analytics-storage corporate-actions".into(),
            downloaded_at: "fixture".into(),
            queried_symbols: ["SH600000".into()].into(),
            actions: vec![],
            failures: Default::default(),
            raw_sha256: Default::default(),
        };
        let body = b"{\"totalAnnouncement\":0,\"announcements\":[]}";
        let hash = format!("sha256:{:x}", Sha256::digest(body));
        fs::write(evidence.join("raw/http-query.body"), body).unwrap();
        let mut shard = serde_json::json!({"instrument":"SH600000","evidence":{"documents":[],"all_announcements":[],"pagination_complete":true,"requested_announcement_dates":{"start":"2024-01-01","end":"2024-12-31"},"http_receipts":[{"url":"https://www.cninfo.com.cn/new/hisAnnouncement/query","status":200,"filename":"http-query.body","sha256":hash}]}});
        let write = |shard: &Value| {
            let bytes = serde_json::to_vec(shard).unwrap();
            fs::write(
                evidence.join("raw/SH600000-actions-structured.json"),
                &bytes,
            )
            .unwrap();
            fs::write(evidence.join("announcements.json"),serde_json::to_vec(&serde_json::json!({"schema_version":2,"documents":[],"receipts":{"SH600000":format!("sha256:{:x}",Sha256::digest(bytes))},"complete":{}})).unwrap()).unwrap();
        };
        write(&shard);
        let mut request = RevisionRequest {
            schema_version: 1,
            evidence_manifest: evidence.join("announcements.json"),
            events: vec![],
            resolve_issues: vec![],
            resolve_failures: vec![],
            coverage_reviews: vec![CoverageReview {
                instrument: "SH600000".into(),
                dates: analytics_core::DateRange {
                    start: "2024-01-01".parse().unwrap(),
                    end: "2024-12-31".parse().unwrap(),
                },
                categories: EVENT_CATEGORIES.iter().map(|s| (*s).into()).collect(),
                document_ids: vec![],
                expected_source_sha256: digest(&source).unwrap(),
                reason: "fixture exhaustive disclosure search and independent category review"
                    .into(),
            }],
        };
        let output = d.path().join("reviewed");
        let reviewed = apply(&source, &original, &request, &output).unwrap();
        assert!(
            reviewed
                .coverage_issues(
                    "SH600000",
                    "2024-01-01".parse().unwrap(),
                    "2024-12-31".parse().unwrap()
                )
                .is_empty()
        );
        ActionBundle::read(&output.join("actions.json")).unwrap();
        request.coverage_reviews[0].expected_source_sha256 = "stale".into();
        assert!(apply(&source, &original, &request, &d.path().join("stale")).is_err());
        request.coverage_reviews[0].expected_source_sha256 = digest(&source).unwrap();
        shard["evidence"]["pagination_complete"] = serde_json::json!(false);
        write(&shard);
        assert!(apply(&source, &original, &request, &d.path().join("truncated")).is_err());
        fs::write(output.join("raw/http-query.body"), "altered evidence").unwrap();
        assert!(ActionBundle::read(&output.join("actions.json")).is_err());
    }
}
