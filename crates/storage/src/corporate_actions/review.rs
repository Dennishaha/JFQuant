//! Immutable, evidence-backed event corrections; never delete a failure marker alone.
use super::*;
use std::path::PathBuf;
fn inherit_acquisition(source: &Path, output: &Path) -> io::Result<()> {
    fn copy(source: &Path, output: &Path) -> io::Result<()> {
        fs::create_dir(output)?;
        for entry in fs::read_dir(source)? {
            task_context_check()?;
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                copy(&entry.path(), &output.join(entry.file_name()))?;
            } else if ty.is_file() {
                fs::copy(entry.path(), output.join(entry.file_name()))?;
            } else {
                return Err(invalid("acquisition evidence cannot contain links"));
            }
        }
        Ok(())
    }
    let bytes = fs::read(source.join("raw/source-evidence-sha256.json"))?;
    let expected: String = serde_json::from_slice(&bytes).map_err(invalid)?;
    if crate::market_view::tree_hash(&source.join("source-evidence"))? != expected {
        return Err(invalid("source acquisition evidence changed"));
    }
    copy(
        &source.join("source-evidence"),
        &output.join("source-evidence"),
    )?;
    if crate::market_view::tree_hash(&output.join("source-evidence"))? != expected {
        return Err(invalid("inherited acquisition evidence changed"));
    }
    Ok(())
}
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
    /// Version-2 leaf field to an authenticated structured fact (not an announcement quote).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub structured_fields: BTreeMap<String, StructuredFieldRef>,
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
/// Select an exact field from the source bundle's replayed structured facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredFieldRef {
    /// Ex-date anchored event identity.
    pub event_id: String,
    /// Field within that event, e.g. record_date or subscription_price.
    pub field: String,
}
fn verify_structured(
    source: &ActionBundle,
    event: &CorporateAction,
    path: &str,
    value: &Value,
    reference: &StructuredFieldRef,
) -> io::Result<()> {
    let expected = match path {
        "record_date" => "record_date",
        "ex_date" => "ex_date",
        "terms.price" | "terms.facts.price.value" => "subscription_price",
        "terms.ratio" | "terms.facts.ratio.value" => "rights_ratio",
        _ => return Err(invalid("structured fact cannot prove this event field")),
    };
    if expected != reference.field || reference.event_id != event.id {
        return Err(invalid("structured field/event identity mismatch"));
    }
    let facts = source
        .field_facts
        .get(&event.instrument)
        .and_then(|r| {
            r.events
                .iter()
                .find(|e| e.event_id == reference.event_id && e.status == "verified")
        })
        .and_then(|e| e.fields.get(&reference.field))
        .ok_or_else(|| invalid("verified structured field missing"))?;
    if facts.value != *value {
        return Err(invalid("structured field value mismatch"));
    }
    Ok(())
}
/// Verify structured revision fields and immutable outputs again when loading a bundle.
pub(super) fn verify_structured_revisions(bundle: &ActionBundle, root: &Path) -> io::Result<()> {
    let mut affected = BTreeSet::new();
    let mut outputs = Vec::new();
    for name in bundle
        .raw_sha256
        .keys()
        .filter(|n| n.starts_with("review-") && n.ends_with(".json"))
    {
        let receipt: Value =
            serde_json::from_slice(&fs::read(root.join("raw").join(name))?).map_err(invalid)?;
        if let Some(events) = receipt["request"]["events"].as_array() {
            outputs.extend(events.iter().map(|r| r["replacement"].clone()));
        }
        if !matches!(receipt["request"]["schema_version"].as_u64(), Some(2 | 3)) {
            continue;
        }
        let request: RevisionRequest =
            serde_json::from_value(receipt["request"].clone()).map_err(invalid)?;
        for revision in &request.events {
            for (path, reference) in &revision.structured_fields {
                let event = serde_json::to_value(&revision.replacement).map_err(invalid)?;
                let pointer = format!("/{}", path.replace('.', "/"));
                let value = event
                    .pointer(&pointer)
                    .ok_or_else(|| invalid("structured revision path missing"))?;
                verify_structured(bundle, &revision.replacement, path, value, reference)?;
                affected.insert(revision.replacement.id.clone());
            }
        }
    }
    for id in affected {
        let current = bundle
            .actions
            .iter()
            .find(|e| e.id == id)
            .ok_or_else(|| invalid("structured revised event missing"))?;
        if !outputs.contains(&serde_json::to_value(current).map_err(invalid)?) {
            return Err(invalid(
                "structured revised event differs from captured output",
            ));
        }
    }
    Ok(())
}
fn changed_leaves(prefix: &str, old: &Value, new: &Value, out: &mut Vec<(String, Value)>) {
    if old == new {
        return;
    }
    if let Some(fields) = new.as_object() {
        for (field, value) in fields {
            changed_leaves(&format!("{prefix}.{field}"), &old[field], value, out);
        }
        // Deleting any key also needs evidence; structured facts cannot prove deletions.
        if let Some(fields) = old.as_object() {
            for field in fields.keys().filter(|f| new.get(*f).is_none()) {
                out.push((format!("{prefix}.{field}"), Value::Null));
            }
        }
    } else {
        out.push((prefix.into(), new.clone()));
    }
}
fn verify_leaf_document(
    documents: &BTreeMap<String, (Value, String)>,
    proof: &FieldEvidence,
    event: &CorporateAction,
    path: &str,
) -> io::Result<()> {
    let (document, text) = documents
        .get(&proof.document_id)
        .ok_or_else(|| invalid("unknown implementation document"))?;
    let announced: Date = document["announced"]
        .as_str()
        .ok_or_else(|| invalid("missing disclosure date"))?
        .parse()
        .map_err(invalid)?;
    let settlement = [
        "terms.facts.allotment_date",
        "terms.facts.allotment_ratio",
        "terms.facts.refund_date",
        "terms.facts.listing_date",
        "terms.facts.conditional_refund",
    ]
    .iter()
    .any(|p| path == *p || path.starts_with(&format!("{p}.")));
    if document["instrument"] != event.instrument
        || document["candidates"]["stage"] != "implementation"
        || proof.quote.trim().is_empty()
        || !text.contains(&proof.quote)
        || proof.unit.trim().is_empty()
        || document["url"]
            .as_str()
            .is_none_or(|u| !event.source.contains(u))
        || (!settlement && event.record_date.is_some_and(|d| announced > d))
    {
        return Err(invalid("leaf evidence identity/disclosure/quote mismatch"));
    }
    Ok(())
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
    /// Version-3 separately captured, replay-verified structured action facts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_manifest: Option<PathBuf>,
    /// Version 1 for announcement quotes, or 2 for structured field references.
    pub schema_version: u32,
    /// Captured announcements.json, or the current actions.json for a structured-only revision.
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
        analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(value).map_err(invalid)?))
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
    analytics_core::source_map::reject_removed_sources(value).map_err(invalid)?;
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
        if *expected
            != format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(&bytes))
            )
        {
            return Err(invalid("announcement metadata receipt mismatch"));
        }
        let shard: Value = serde_json::from_slice(&bytes).map_err(invalid)?;
        analytics_core::source_map::reject_removed_sources(&shard).map_err(invalid)?;
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
            if d[h]
                != format!(
                    "sha256:{:x}",
                    analytics_core::hash::Hex(Sha256::digest(bytes))
                )
            {
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
/// Validate the exact scope and candidate set both at publication and on replay.
pub(super) fn verify_coverage_review(review: &CoverageReview, shard: &Value) -> io::Result<()> {
    review.dates.validate().map_err(invalid)?;
    if review.reason.trim().is_empty()
        || review.categories.is_empty()
        || review
            .categories
            .iter()
            .any(|c| !EVENT_CATEGORIES.contains(&c.as_str()))
        || shard["instrument"] != review.instrument
        || shard["evidence"]["pagination_complete"] != true
        || !shard["evidence"]["all_announcements"].is_array()
    {
        return Err(invalid(
            "coverage review requires complete identity-matched disclosure metadata",
        ));
    }
    let start: Date =
        serde_json::from_value(shard["evidence"]["requested_announcement_dates"]["start"].clone())
            .map_err(invalid)?;
    let end: Date =
        serde_json::from_value(shard["evidence"]["requested_announcement_dates"]["end"].clone())
            .map_err(invalid)?;
    if start > review.dates.start || end < review.dates.end {
        return Err(invalid("coverage review exceeds captured disclosure dates"));
    }
    let docs = shard["evidence"]["documents"]
        .as_array()
        .ok_or_else(|| invalid("coverage documents missing"))?;
    let captured: BTreeSet<_> = docs.iter().filter_map(|d| d["id"].as_str()).collect();
    let requested: BTreeSet<_> = review.document_ids.iter().map(String::as_str).collect();
    if requested != captured
        || captured.len() != docs.len()
        || requested.len() != review.document_ids.len()
        || docs.iter().any(|d| d["instrument"] != review.instrument)
    {
        return Err(invalid(
            "coverage review must include every identity-matched captured action candidate",
        ));
    }
    Ok(())
}

/// Replay complete CNINFO disclosure metadata from original successful response bodies.
/// Partial/selected PDF receipts cannot establish an exhaustive search.
pub(super) fn verify_coverage_capture(
    root: &Path,
    review: &CoverageReview,
    shard: &Value,
) -> io::Result<()> {
    verify_coverage_review(review, shard)?;
    crate::collector::verify_http_receipts(root, &shard["evidence"])?;
    let metadata = shard["evidence"]["all_announcements"].as_array().unwrap();
    let code = review
        .instrument
        .get(2..)
        .ok_or_else(|| invalid("invalid coverage security"))?;
    let mut found = BTreeMap::<String, Value>::new();
    let mut empty_query = false;
    let mut bound_queries = 0;
    for receipt in shard["evidence"]["http_receipts"]
        .as_array()
        .ok_or_else(|| invalid("coverage HTTP query receipts missing"))?
    {
        if receipt["url"] != "https://www.cninfo.com.cn/new/hisAnnouncement/query"
            || receipt["status"] != 200
        {
            continue;
        }
        let binding = &receipt["query_request"];
        let expected_dates = format!(
            "{}~{}",
            shard["evidence"]["requested_announcement_dates"]["start"]
                .as_str()
                .unwrap_or(""),
            shard["evidence"]["requested_announcement_dates"]["end"]
                .as_str()
                .unwrap_or("")
        );
        let venue = match review.instrument.get(..2) {
            Some("SH") => "sse",
            Some("SZ") => "szse",
            Some("BJ") => "bse",
            _ => return Err(invalid("coverage exchange missing")),
        };
        let stock = binding["stock"].as_str().unwrap_or("");
        let bound = stock
            .split_once(',')
            .is_some_and(|(c, org)| c == code && !org.is_empty())
            && binding["column"] == venue
            && binding["searchkey"] == ""
            && binding["tabName"] == "fulltext"
            && binding["seDate"] == expected_dates;
        if !bound {
            continue;
        }
        if bound {
            bound_queries += 1;
        }
        let name = receipt["filename"]
            .as_str()
            .ok_or_else(|| invalid("query receipt filename missing"))?;
        let body: Value =
            serde_json::from_slice(&fs::read(root.join("raw").join(name))?).map_err(invalid)?;
        let total = body["totalAnnouncement"]
            .as_u64()
            .ok_or_else(|| invalid("query total missing"))?;
        let rows = body["announcements"].as_array();
        if total == 0 && (rows.is_some_and(Vec::is_empty) || body["announcements"].is_null()) {
            empty_query = true;
        }
        for row in rows.into_iter().flatten().filter(|r| r["secCode"] == code) {
            if stock
                .split_once(',')
                .is_none_or(|(_, org)| row["orgId"] != org)
            {
                return Err(invalid("coverage query institution mismatch"));
            }
            if total != metadata.len() as u64 {
                return Err(invalid("coverage query total changed or incomplete"));
            }
            let id = row["announcementId"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| row["announcementId"].to_string());
            let raw_title = row["announcementTitle"]
                .as_str()
                .ok_or_else(|| invalid("query title missing"))?;
            let mut tag = false;
            let title: String = raw_title
                .chars()
                .filter(|c| {
                    if *c == '<' {
                        tag = true;
                        false
                    } else if *c == '>' {
                        tag = false;
                        false
                    } else {
                        !tag
                    }
                })
                .collect();
            let item = serde_json::json!({"id":id,"title":title,"institution_id":row["orgId"],"announcement_time":row["announcementTime"],"attachment":row["adjunctUrl"]});
            if found.get(&id).is_some_and(|old| old != &item) {
                return Err(invalid("conflicting query metadata"));
            }
            found.insert(id, item);
        }
    }
    if bound_queries == 0 {
        return Err(invalid(
            "coverage query identity/range binding missing; recollect with current collector",
        ));
    }
    if (metadata.is_empty() && !empty_query)
        || found.len() != metadata.len()
        || metadata
            .iter()
            .any(|m| m["id"].as_str().and_then(|id| found.get(id)) != Some(m))
    {
        return Err(invalid(
            "coverage metadata does not replay from complete query responses",
        ));
    }
    let terms = [
        "权益分派",
        "利润分配",
        "转增",
        "送股",
        "派息",
        "股利",
        "分红",
        "配股",
        "缩股",
        "重整",
        "股权分置",
        "换股",
        "合并",
        "终止上市",
        "摘牌",
        "清偿",
        "清算",
        "交割",
        "现金选择权",
    ];
    let documents: BTreeSet<_> = review.document_ids.iter().map(String::as_str).collect();
    if metadata.iter().any(|m| {
        m["title"].as_str().is_some_and(|t| {
            terms.iter().any(|term| t.contains(term))
                || (t.contains("证券代码") && t.contains("变更"))
        }) && m["id"].as_str().is_none_or(|id| !documents.contains(id))
    }) {
        return Err(invalid(
            "coverage action candidate was not captured/reviewed",
        ));
    }
    Ok(())
}

/// Replay one coverage claim; callers cache expensive capture replay per receipt/range.
pub(super) fn verify_coverage_receipt(
    bundle: &ActionBundle,
    root: &Path,
    id: &str,
    c: &EventCoverage,
    name: &str,
    verify_capture: bool,
) -> io::Result<()> {
    if !name.starts_with("review-") {
        return Err(invalid(
            "event completeness needs an auditable review receipt",
        ));
    }
    let bytes = fs::read(root.join("raw").join(name))?;
    if bundle.raw_sha256.get(name)
        != Some(&format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(&bytes))
        ))
    {
        return Err(invalid("category review receipt hash mismatch"));
    }
    let review: Value = serde_json::from_slice(&bytes).map_err(invalid)?;
    if review["coverage_validation_version"] != 2 {
        return Err(invalid(
            "unsupported coverage validation version; recollect and republish review",
        ));
    }
    let consistent = review["request"]["coverage_reviews"]
        .as_array()
        .is_some_and(|rs| {
            rs.iter().any(|r| {
                r["instrument"] == id
                    && r["dates"] == serde_json::json!(c.dates)
                    && r["reason"] == c.reason
                    && r["categories"]
                        .as_array()
                        .is_some_and(|cs| cs.iter().any(|v| v == &c.category))
                    && r["expected_source_sha256"] == review["source_sha256"]
            })
        });
    if !consistent {
        return Err(invalid("category review claim/receipt mismatch"));
    }
    if !verify_capture {
        return Ok(());
    }
    let receipt = &review["collection_receipts"][id];
    if receipt["evidence"]["pagination_complete"] != true
        || !receipt["evidence"]["all_announcements"].is_array()
    {
        return Err(invalid(
            "category review lacks complete disclosure metadata",
        ));
    }
    let coverage_review: review::CoverageReview = serde_json::from_value(
        review["request"]["coverage_reviews"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| {
                r["instrument"] == id
                    && r["dates"] == serde_json::json!(c.dates)
                    && r["reason"] == c.reason
                    && r["categories"]
                        .as_array()
                        .is_some_and(|cs| cs.iter().any(|v| v == &c.category))
            })
            .unwrap()
            .clone(),
    )
    .map_err(invalid)?;
    review::verify_coverage_capture(root, &coverage_review, receipt)?;
    for doc in receipt["evidence"]["documents"].as_array().unwrap() {
        for (field, hash_field) in [("pdf", "pdf_sha256"), ("text", "text_sha256")] {
            let name = doc[field]
                .as_str()
                .ok_or_else(|| invalid("coverage document file missing"))?;
            if Path::new(name).file_name().and_then(|n| n.to_str()) != Some(name) {
                return Err(invalid("unsafe coverage document path"));
            }
            let bytes = fs::read(root.join("raw").join(name))?;
            let hash = format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(&bytes))
            );
            if doc[hash_field] != hash || bundle.raw_sha256.get(name) != Some(&hash) {
                return Err(invalid("coverage document evidence changed"));
            }
        }
    }
    crate::collector::verify_http_receipts(root, &receipt["evidence"])?;
    Ok(())
}

/// Produce a new current-format action bundle; source files are never rewritten.
pub fn apply(
    source: &ActionBundle,
    source_root: &Path,
    request: &RevisionRequest,
    output: &Path,
) -> io::Result<ActionBundle> {
    source.validate_identity()?;
    analytics_core::source_map::reject_removed_sources(
        &serde_json::to_value(request).map_err(invalid)?,
    )
    .map_err(invalid)?;
    if !matches!(request.schema_version, 1..=3)
        || (request.events.is_empty() && request.coverage_reviews.is_empty())
    {
        return Err(invalid("invalid/empty action revision"));
    }
    let structured = request
        .events
        .iter()
        .any(|r| !r.structured_fields.is_empty());
    if structured
        && (request.schema_version < 2
            || !request.resolve_failures.is_empty()
            || !request.resolve_issues.is_empty())
    {
        return Err(invalid(
            "structured revisions require version 2 and cannot clear history failures or execution issues",
        ));
    }
    if structured {
        let authenticated = ActionBundle::read(&source_root.join("actions.json"))
            .map_err(|e| invalid(format!("source snapshot: {e}")))?;
        if digest(&authenticated)? != digest(source)? {
            return Err(invalid("structured revision source snapshot mismatch"));
        }
    }
    let manifest_value: Value =
        serde_json::from_slice(&fs::read(&request.evidence_manifest)?).map_err(invalid)?;
    let documents = if request.schema_version >= 2
        && matches!(manifest_value["schema_version"].as_u64(), Some(7 | 8))
    {
        let authenticated = ActionBundle::read(&request.evidence_manifest)?;
        if digest(&authenticated)? != digest(source)?
            || !request.coverage_reviews.is_empty()
            || request.events.iter().any(|e| !e.fields.is_empty())
        {
            return Err(invalid(
                "structured manifest must match source and cannot prove announcement fields",
            ));
        }
        BTreeMap::new()
    } else {
        verified_documents(&request.evidence_manifest)
            .map_err(|e| invalid(format!("announcement evidence: {e}")))?
    };
    let mut bundle = source.clone();
    let mut attached = BTreeMap::new();
    if let Some(path) = &request.structured_manifest {
        if request.schema_version != 3 {
            return Err(invalid("external structured facts require revision 3"));
        }
        let evidence =
            ActionBundle::read(path).map_err(|e| invalid(format!("structured attachment: {e}")))?;
        let prefix = format!("rights-v3-{}-", &digest(&evidence)?[7..23]);
        let root = path
            .parent()
            .ok_or_else(|| invalid("structured evidence root missing"))?;
        for (id, report) in &evidence.field_facts {
            if !source.coverage.contains_key(id) && !source.queried_symbols.contains(id)
                || source.source_symbol(id)? != evidence.source_symbol(id)?
                || bundle.field_facts.contains_key(id)
            {
                return Err(invalid(
                    "structured attachment identity or duplicate facts conflict",
                ));
            }
            let mut report = report.clone();
            report.evidence_name = format!("{prefix}{}", report.evidence_name);
            bundle.field_facts.insert(id.clone(), report);
        }
        for (name, hash) in &evidence.raw_sha256 {
            let bytes = fs::read(root.join("raw").join(name))?;
            if format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(&bytes))
            ) != *hash
            {
                return Err(invalid("structured attachment changed"));
            }
            let name = if name.ends_with("-actions-structured.json")
                || name == "complete-sources-structured.json"
            {
                format!("{prefix}{name}")
            } else {
                name.clone()
            };
            if bundle.raw_sha256.get(&name).is_some_and(|h| h != hash) {
                return Err(invalid("structured attachment filename conflict"));
            }
            bundle.raw_sha256.insert(name.clone(), hash.clone());
            attached.insert(name, bytes);
        }
    }
    let evidence_source = bundle.clone();
    let mut changed = BTreeSet::new();
    let mut used = BTreeSet::new();
    let manifest: Value =
        serde_json::from_slice(&fs::read(&request.evidence_manifest)?).map_err(invalid)?;
    for review in &request.coverage_reviews {
        review.dates.validate().map_err(invalid)?;
        if review.reason.trim().is_empty()
            || review.categories.is_empty()
            || review.expected_source_sha256 != digest(source)?
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
        if receipt.is_null() {
            return Err(invalid("coverage review source receipt missing"));
        }
        verify_coverage_capture(root, review, &shard)?;
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
        if revision.reason.trim().is_empty()
            || (revision.fields.is_empty() && revision.structured_fields.is_empty())
        {
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
        if matches!(
            a.terms,
            analytics_core::actions::EventTerms::RightsIssueFacts { .. }
        ) && (request.schema_version != 3 || revision.fields.contains_key("terms"))
        {
            return Err(invalid(
                "partial rights require revision 3 with field-level evidence",
            ));
        }
        if original == replacement {
            return Err(invalid("no-op revision cannot resolve a source failure"));
        }
        for (field, value) in replacement.as_object().unwrap() {
            if original.get(field) == Some(value) {
                continue;
            }
            if !revision.fields.contains_key(field) {
                let mut leaves = vec![];
                changed_leaves(field, &original[field], value, &mut leaves);
                for (path, value) in leaves {
                    if request.schema_version == 3 {
                        let pointer = format!("/{}", path.replace('.', "/"));
                        let old = original.pointer(&pointer);
                        if old.is_none()
                            && (value.is_null() || (path.ends_with(".state") && value == "unknown"))
                        {
                            continue;
                        }
                        if path.ends_with(".state") && value == "confirmed" && old.is_none() {
                            let sibling = format!("{}.value", path.trim_end_matches(".state"));
                            if revision.structured_fields.contains_key(&sibling) {
                                continue;
                            }
                        }
                        if let Some((_, proof)) = revision
                            .fields
                            .iter()
                            .filter(|(prefix, _)| {
                                path == ***prefix || path.starts_with(&format!("{prefix}."))
                            })
                            .max_by_key(|(prefix, _)| prefix.len())
                        {
                            verify_leaf_document(&documents, proof, a, &path)?;
                            used.insert(proof.document_id.clone());
                            continue;
                        }
                    }
                    let reference = revision
                        .structured_fields
                        .get(&path)
                        .ok_or_else(|| invalid(format!("changed field lacks evidence: {path}")))?;
                    verify_structured(&evidence_source, a, &path, &value, reference)?;
                }
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
    fs::create_dir(output).map_err(|e| invalid(format!("revision staging directory: {e}")))?;
    fs::create_dir(output.join("raw"))?;
    if source
        .raw_sha256
        .contains_key("source-evidence-sha256.json")
    {
        inherit_acquisition(source_root, output)?;
    }
    for (name, expected) in &source.raw_sha256 {
        let bytes = fs::read(source_root.join("raw").join(name))?;
        if format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(&bytes))
        ) != *expected
        {
            return Err(invalid("source action evidence hash mismatch"));
        }
        fs::write(output.join("raw").join(name), bytes)?;
    }
    for (name, bytes) in attached {
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
    super::facts::refresh(&mut bundle, output)?;
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
            let hash = format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(&bytes))
            );
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
            let hash = format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(&bytes))
            );
            if bundle.raw_sha256.get(name).is_some_and(|h| h != &hash) {
                return Err(invalid("evidence filename collision"));
            }
            fs::write(output.join("raw").join(name), bytes)?;
            bundle.raw_sha256.insert(name.into(), hash);
        }
    }
    let receipt = serde_json::json!({"schema_version":1,"coverage_validation_version":2,"source_sha256":digest(source)?,"request":request,"source_events":source.actions,"captured_documents":selected_documents,"collection_receipts":collection_receipts,"collector_completion":manifest["complete"],"evidence_manifest_sha256":format!("sha256:{:x}",analytics_core::hash::Hex(Sha256::digest(&manifest_bytes))),"applied_at":chrono::Utc::now().to_rfc3339()});
    let name = format!("review-{}.json", &digest(&receipt)?[7..23]);
    let bytes = serde_json::to_vec_pretty(&receipt).map_err(invalid)?;
    bundle.raw_sha256.insert(
        name.clone(),
        format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(&bytes))
        ),
    );
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
        format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(bytes))
        )
    }
    #[test]
    fn acquisition_tree_is_inherited_and_tampering_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let out = tmp.path().join("out");
        fs::create_dir_all(source.join("raw")).unwrap();
        fs::create_dir_all(source.join("source-evidence/nested")).unwrap();
        fs::create_dir(&out).unwrap();
        fs::write(source.join("source-evidence/nested/body"), b"evidence").unwrap();
        let hash = crate::market_view::tree_hash(&source.join("source-evidence")).unwrap();
        fs::write(
            source.join("raw/source-evidence-sha256.json"),
            serde_json::to_vec(&hash).unwrap(),
        )
        .unwrap();
        inherit_acquisition(&source, &out).unwrap();
        assert_eq!(
            crate::market_view::tree_hash(&out.join("source-evidence")).unwrap(),
            hash
        );
        fs::write(source.join("source-evidence/nested/body"), b"changed").unwrap();
        assert!(inherit_acquisition(&source, &tmp.path().join("bad")).is_err());
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
            field_facts: Default::default(),
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
        let owner = serde_json::json!({"operation":"announcements","collector_protocol":13,"collector_signature":crate::collector::collector_signature(),"normalization":crate::adjustment::NORMALIZATION,"dependencies_sha256":format!("sha256:{:x}",analytics_core::hash::Hex(Sha256::digest(include_bytes!("../../collectors/requirements.lock")))),"python_runtime_sha256":format!("sha256:{:x}",analytics_core::hash::Hex(Sha256::digest(include_bytes!("../../collectors/python-runtime.lock.json")))),"request":{"provider":"cninfo","adjustment_source":"none","start":"2024-06-01","end":"2024-06-30","instruments":["SH600000"],"source_bindings":{},"benchmarks":[],"universes":[]}});
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
            structured_manifest: None,
            coverage_reviews: Vec::new(),
            schema_version: 1,
            evidence_manifest: root.join("announcements.json"),
            events: vec![EventRevision {
                structured_fields: Default::default(),
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
            field_facts: Default::default(),
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
        // A failed primary query must remain auditable while a complete independent audit passes.
        let mut source = source;
        source.queried_symbols.clear();
        source
            .failures
            .insert("SH600000".into(), "primary query failed".into());
        let body = b"{\"totalAnnouncement\":0,\"announcements\":[]}";
        let hash = format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(body))
        );
        fs::write(evidence.join("raw/http-query.body"), body).unwrap();
        let mut shard = serde_json::json!({"instrument":"SH600000","evidence":{"documents":[],"all_announcements":[],"pagination_complete":true,"requested_announcement_dates":{"start":"2024-01-01","end":"2024-12-31"},"http_receipts":[{"url":"https://www.cninfo.com.cn/new/hisAnnouncement/query","status":200,"filename":"http-query.body","sha256":hash,"query_request":{"stock":"600000,gssh0600000","column":"sse","searchkey":"","tabName":"fulltext","seDate":"2024-01-01~2024-12-31","pageNum":"1","pageSize":"30"}}]}});
        let write = |shard: &Value| {
            let bytes = serde_json::to_vec(shard).unwrap();
            fs::write(
                evidence.join("raw/SH600000-actions-structured.json"),
                &bytes,
            )
            .unwrap();
            fs::write(evidence.join("announcements.json"),serde_json::to_vec(&serde_json::json!({"schema_version":2,"documents":[],"receipts":{"SH600000":format!("sha256:{:x}",analytics_core::hash::Hex(Sha256::digest(bytes)))},"complete":{}})).unwrap()).unwrap();
        };
        write(&shard);
        let mut request = RevisionRequest {
            structured_manifest: None,
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
        let loaded = ActionBundle::read(&output.join("actions.json")).unwrap();
        // Obsolete reviews must fail even when expensive capture replay is cached.
        let name = loaded
            .raw_sha256
            .keys()
            .find(|n| n.starts_with("review-"))
            .unwrap();
        let mut obsolete: Value =
            serde_json::from_slice(&fs::read(output.join("raw").join(name)).unwrap()).unwrap();
        obsolete
            .as_object_mut()
            .unwrap()
            .remove("coverage_validation_version");
        let bytes = serde_json::to_vec(&obsolete).unwrap();
        let obsolete_name = "review-obsolete.json";
        fs::write(output.join("raw").join(obsolete_name), &bytes).unwrap();
        let mut obsolete_book = loaded.clone();
        obsolete_book.raw_sha256.insert(
            obsolete_name.into(),
            format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(&bytes))
            ),
        );
        for verify_capture in [false, true] {
            let error = verify_coverage_receipt(
                &obsolete_book,
                &output,
                "SH600000",
                &loaded.coverage["SH600000"][0],
                obsolete_name,
                verify_capture,
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("unsupported coverage validation version")
            );
        }
        assert_eq!(loaded.failures, source.failures);
        assert!(loaded.queried_symbols.is_empty());
        loaded
            .require_adjustment_coverage(
                &BTreeSet::from(["SH600000".into()]),
                "2024-01-01".parse().unwrap(),
                "2024-12-31".parse().unwrap(),
            )
            .unwrap();
        super::super::execution_index::ExecutionIndex::build(
            &loaded,
            analytics_core::execution::EventCheckPolicy::Audited,
        )
        .unwrap()
        .check("SH600000", "2024-06-01".parse().unwrap())
        .unwrap();
        assert!(
            loaded
                .require_adjustment_coverage(
                    &BTreeSet::from(["SH600000".into()]),
                    "2023-12-31".parse().unwrap(),
                    "2024-12-31".parse().unwrap()
                )
                .is_err()
        );
        let mut conflicted = loaded.clone();
        conflicted
            .execution_issues
            .push(crate::execution_data::ExecutionIssue {
                id: "open-conflict".into(),
                instrument: "SH600000".into(),
                announced: None,
                dates: analytics_core::DateRange {
                    start: "2024-06-01".parse().unwrap(),
                    end: "2024-06-01".parse().unwrap(),
                },
                code: "source_event_conflict".into(),
                event_category: "distribution".into(),
                evidence: vec!["fixture independent conflict".into()],
                message: "coverage does not close an event conflict".into(),
            });
        assert!(
            super::super::execution_index::ExecutionIndex::build(
                &conflicted,
                analytics_core::execution::EventCheckPolicy::Audited
            )
            .unwrap()
            .check("SH600000", "2024-06-01".parse().unwrap())
            .is_err()
        );
        request.coverage_reviews[0].expected_source_sha256 = "stale".into();
        assert!(apply(&source, &original, &request, &d.path().join("stale")).is_err());
        request.coverage_reviews[0].expected_source_sha256 = digest(&source).unwrap();
        shard["evidence"]["pagination_complete"] = serde_json::json!(false);
        write(&shard);
        assert!(apply(&source, &original, &request, &d.path().join("truncated")).is_err());
        shard["evidence"]["pagination_complete"] = serde_json::json!(true);
        let incomplete = br#"{"totalAnnouncement":1,"announcements":[]}"#;
        fs::write(evidence.join("raw/http-query.body"), incomplete).unwrap();
        shard["evidence"]["http_receipts"][0]["sha256"] = serde_json::json!(format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(incomplete))
        ));
        write(&shard);
        assert!(
            apply(
                &source,
                &original,
                &request,
                &d.path().join("false-complete")
            )
            .is_err()
        );
        fs::write(output.join("raw/http-query.body"), "altered evidence").unwrap();
        assert!(ActionBundle::read(&output.join("actions.json")).is_err());
    }
}
