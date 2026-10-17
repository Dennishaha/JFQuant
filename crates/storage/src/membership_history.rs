//! Original-document verification for index member history; no model or account execution.
use crate::membership_history_material as material;
pub use crate::membership_history_material::{DerivedFrom, MaterialStatus, Transcription};
use analytics_core::{
    membership_history::{self as core, Citation, History, Report},
    source_map::SourceMap,
    task::TaskContext,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

/// Original document, either explicitly captured or supplied for human review.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Document {
    /// Unique local identity referenced by citations.
    pub id: String,
    /// Workspace-relative original file; frozen copies replace this on publication.
    pub path: PathBuf,
    /// SHA256 of original bytes.
    pub sha256: String,
    /// Original public HTTPS source, retained even for archived or manually supplied files.
    pub source_url: String,
    /// Explicit extraction format: pdf or text. No format guessing.
    pub format: String,
    /// True only for a source describing complete event coverage, not a single roster/event.
    pub coverage_source: bool,
    /// Optional embedded PDF raster origin, never a replacement original.
    pub derived_from: Option<DerivedFrom>,
}
/// Versioned shared review request; paths never enter the core algorithm.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Format 2.
    pub schema_version: u32,
    /// Structured facts and explicit reviews.
    pub history: History,
    /// Referenced originals.
    pub documents: Vec<Document>,
    /// Explicit attributed image readings.
    pub transcriptions: Vec<Transcription>,
}
/// Hash original bytes rather than parsed text.
pub fn bytes_hash(bytes: &[u8]) -> String {
    format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(bytes))
    )
}
pub(crate) fn invalid(s: impl std::fmt::Display) -> io::Error {
    io::Error::other(s.to_string())
}
/// Resolve a regular original file within this workspace; absolute paths and links are rejected.
pub fn document_path(root: &Path, path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute()
        || path
            .components()
            .any(|p| !matches!(p, std::path::Component::Normal(_)))
    {
        return Err(invalid(
            "history evidence requires a workspace-relative regular file",
        ));
    }
    let mut resolved = root.to_path_buf();
    for p in path.components() {
        resolved.push(p);
        if fs::symlink_metadata(&resolved)?.file_type().is_symlink() {
            return Err(invalid("history evidence cannot be a symlink"));
        }
    }
    if !resolved.is_file() {
        return Err(invalid("history evidence is not a regular file"));
    }
    Ok(resolved)
}
fn compact(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}
/// Validate a public source identity without contacting it.
pub fn validate_source(source: &str) -> io::Result<()> {
    let url = url::Url::parse(source).map_err(invalid)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid("invalid history source URL"));
    }
    Ok(())
}
pub(crate) fn text(root: &Path, d: &Document) -> io::Result<String> {
    validate_source(&d.source_url)?;
    let p = document_path(root, &d.path)?;
    let b = fs::read(&p)?;
    if bytes_hash(&b) != d.sha256 {
        return Err(invalid("history evidence hash mismatch"));
    }
    match d.format.as_str() {
        "text" => {
            let t = String::from_utf8(b).map_err(invalid)?;
            material::body(&t)?;
            Ok(t)
        }
        "pdf" => crate::corporate_actions::evidence::extract(&p)?["text"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| invalid("missing extracted PDF text")),
        _ => Err(invalid("unsupported history document format")),
    }
}
/// Produce an explicitly hashed derivative without changing original bytes.
pub fn extracted_text(root: &Path, d: &Document) -> io::Result<String> {
    let t = text(root, d)?;
    material::body(&t)?;
    Ok(t)
}
struct VerifiedMaterials<'a> {
    texts: BTreeMap<String, String>,
    docs: BTreeMap<String, &'a Document>,
    duplicate: BTreeSet<String>,
    readings: BTreeMap<String, &'a Transcription>,
    duplicate_readings: BTreeSet<String>,
}
impl VerifiedMaterials<'_> {
    fn check(&self, citations: &[Citation]) -> bool {
        !citations.is_empty()
            && citations.iter().all(|c| {
                let excerpt = compact(&c.excerpt);
                !excerpt.is_empty()
                    && match &c.transcription {
                        None => self
                            .texts
                            .get(&c.document)
                            .is_some_and(|t| t.contains(&excerpt)),
                        Some(id) => {
                            !self.duplicate_readings.contains(id)
                                && !self.duplicate.contains(&c.document)
                                && self.readings.get(id).is_some_and(|t| {
                                    t.document == c.document && compact(&t.text).contains(&excerpt)
                                })
                        }
                    }
            })
    }
}
fn verify_materials<'a>(
    root: &Path,
    request: &'a Request,
    context: &TaskContext,
) -> io::Result<(VerifiedMaterials<'a>, Vec<core::Issue>)> {
    let mut texts = BTreeMap::new();
    let mut problems = vec![];
    let mut duplicate = BTreeSet::new();
    let mut docs = BTreeMap::new();
    for d in &request.documents {
        context.check()?;
        if docs.insert(d.id.clone(), d).is_some() {
            duplicate.insert(d.id.clone());
        }
        if matches!(d.format.as_str(), "png" | "jpeg") {
            let status = material::inspect(root, d);
            if let Some(reason) = status.reason {
                core::issue(&mut problems, &d.id, "evidence_verification_failed", None);
                problems.last_mut().unwrap().details = Some(reason);
            }
            continue;
        }
        match text(root, d) {
            Ok(t) if material::body(&t).is_ok() => {
                texts.insert(d.id.clone(), compact(&t));
            }
            Ok(_) => {
                core::issue(&mut problems, &d.id, "capture_content_unusable", None);
            }
            Err(e) => {
                let state = material::inspect(root, d);
                core::issue(
                    &mut problems,
                    &d.id,
                    if state.integrity == "verified" {
                        "capture_content_unusable"
                    } else {
                        "evidence_verification_failed"
                    },
                    None,
                );
                problems.last_mut().unwrap().details = Some(e.to_string());
            }
        }
    }
    for id in &duplicate {
        texts.remove(id);
        core::issue(&mut problems, id, "duplicate_document_id", None);
    }
    // Derived images must reproduce exactly from the registered immutable PDF.
    for d in &request.documents {
        if let Err(error) = material::verify_origin(root, d, &request.documents, context) {
            core::issue(&mut problems, &d.id, "evidence_verification_failed", None);
            problems.last_mut().unwrap().details = Some(error.to_string());
        }
    }
    let mut readings = BTreeMap::new();
    let mut duplicate_readings = BTreeSet::new();
    for t in &request.transcriptions {
        if request
            .transcriptions
            .iter()
            .filter(|x| x.id == t.id)
            .count()
            > 1
        {
            duplicate_readings.insert(t.id.clone());
            core::issue(&mut problems, &t.id, "duplicate_transcription_id", None);
        }
        if let Err(error) = material::verify_transcription(root, t, &request.documents) {
            core::issue(
                &mut problems,
                &t.id,
                "image_review_binding_unverified",
                None,
            );
            problems.last_mut().unwrap().details = Some(error.to_string());
        } else {
            readings.insert(t.id.clone(), t);
        }
    }
    Ok((
        VerifiedMaterials {
            texts,
            docs,
            duplicate,
            readings,
            duplicate_readings,
        },
        problems,
    ))
}
/// Validate references, exact excerpts and date-effective exchange identities, then reconstruct.
/// Reviews remain explicitly attributed human judgments, never inferred from search success.
pub fn verify(
    root: &Path,
    request: &Request,
    calendar: &[analytics_core::Date],
    source_map: &SourceMap,
    context: &TaskContext,
) -> io::Result<Report> {
    if request.schema_version != 2 {
        return Err(invalid("unsupported membership request format"));
    }
    let (materials, mut problems) = verify_materials(root, request, context)?;
    let docs = &materials.docs;
    let check = |refs: &[Citation]| materials.check(refs);
    let mut invalid_subjects = BTreeSet::new();
    let identity = check(&request.history.identity)
        && request.history.identity.iter().any(|c| {
            compact(&c.excerpt).contains(&compact(&request.history.index_name))
                && compact(&c.excerpt).contains(&request.history.official_code)
        })
        && !request.history.index_name.trim().is_empty();
    if !identity {
        core::issue(
            &mut problems,
            "index",
            "index_identity_unverified",
            Some(request.history.requested.clone()),
        );
    }
    // Same event revisions/repeats are allowed; IDs cannot cross subject kinds.
    let anchor_ids: BTreeSet<_> = request.history.anchors.iter().map(|a| &a.id).collect();
    let event_ids: BTreeSet<_> = request.history.events.iter().map(|a| &a.id).collect();
    let review_ids: BTreeSet<_> = request.history.reviews.iter().map(|a| &a.id).collect();
    for id in anchor_ids
        .intersection(&event_ids)
        .chain(anchor_ids.intersection(&review_ids))
        .chain(event_ids.intersection(&review_ids))
    {
        invalid_subjects.insert((*id).clone());
    }
    let resolved = core::resolve_revisions(&request.history);
    for (id, at, members, citations, securities, correction, revision) in resolved
        .history
        .anchors
        .iter()
        .map(|a| {
            (
                &a.id,
                &a.at,
                a.members.iter().collect::<Vec<_>>(),
                &a.citations,
                &a.securities,
                &a.correction,
                a.revision,
            )
        })
        .chain(resolved.history.events.iter().map(|e| {
            (
                &e.id,
                &e.at,
                e.add.iter().chain(&e.remove).collect(),
                &e.citations,
                &e.securities,
                &e.correction,
                e.revision,
            )
        }))
    {
        if !identity || !check(citations) || revision > 1 && !check(correction) {
            invalid_subjects.insert(id.clone());
            core::issue(&mut problems, id, "identity_or_citation_unverified", None);
            continue;
        }
        let day = at.opening(calendar).unwrap_or(at.date);
        for instrument in members {
            let proof: Vec<_> = securities
                .iter()
                .filter(|p| &p.instrument == instrument)
                .collect();
            let validation =
                security_association(instrument, day, &proof, citations, source_map, &check);
            if let Err(reason) = validation {
                invalid_subjects.insert(id.clone());
                core::issue(
                    &mut problems,
                    id,
                    "security_identity_or_roster_reference_unverified",
                    Some(analytics_core::DateRange {
                        start: day,
                        end: day,
                    }),
                );
                let issue = problems.last_mut().unwrap();
                issue.details = Some(format!("{instrument}: {reason}"));
                issue.id.push_str(&format!(":{instrument}"));
            }
        }
    }
    for r in &request.history.reviews {
        if !identity
            || !check(&r.citations)
            || matches!(r.outcome, core::ReviewOutcome::Coverage)
                && !r
                    .citations
                    .iter()
                    .any(|c| docs.get(&c.document).is_some_and(|d| d.coverage_source))
        {
            invalid_subjects.insert(r.id.clone());
            core::issue(
                &mut problems,
                &r.id,
                "coverage_source_or_review_reference_unverified",
                Some(r.range.clone()),
            );
        }
    }
    let mut report =
        core::reconstruct(&request.history, calendar, &invalid_subjects).map_err(invalid)?;
    finish_report(
        &mut report,
        request,
        &resolved.history,
        &materials,
        problems,
    );
    Ok(report)
}
fn security_association(
    instrument: &str,
    day: analytics_core::Date,
    proof: &[&core::SecurityEvidence],
    subject: &[Citation],
    source_map: &SourceMap,
    check: &impl Fn(&[Citation]) -> bool,
) -> Result<(), String> {
    if proof.len() != 1 {
        return Err(if proof.is_empty() {
            "missing_per_security_association"
        } else {
            "duplicate_per_security_association"
        }
        .into());
    }
    let symbol = source_map
        .exchange_symbol(instrument, day)
        .map_err(|e| format!("date_effective_identity_failed: {e}"))?;
    if source_map.is_price_index(instrument) {
        return Err("index_is_not_a_member_security".into());
    }
    let p = proof[0];
    let name = compact(&p.name);
    if name.is_empty() {
        return Err("member_name_missing".into());
    }
    if !check(&p.membership) {
        return Err("membership_reference_unverified".into());
    }
    if !p.membership.iter().all(|r| {
        subject
            .iter()
            .any(|s| s.document == r.document && s.transcription == r.transcription)
    }) {
        return Err("membership_reference_not_linked_to_subject".into());
    }
    if !p
        .membership
        .iter()
        .any(|c| compact(&c.excerpt).contains(&name))
    {
        return Err("name_absent_from_membership_reference".into());
    }
    if !check(&p.identity) {
        return Err("identity_reference_unverified".into());
    }
    if !p.identity.iter().any(|c| {
        let t = compact(&c.excerpt);
        t.contains(&name) && t.contains(&symbol[2..])
    }) {
        return Err("name_code_relationship_unverified".into());
    }
    Ok(())
}
fn finish_report(
    report: &mut Report,
    request: &Request,
    resolved: &History,
    materials: &VerifiedMaterials<'_>,
    problems: Vec<core::Issue>,
) {
    let check = |refs: &[Citation]| materials.check(refs);
    for (id, revision, citations) in request
        .history
        .anchors
        .iter()
        .filter(|a| {
            resolved
                .anchors
                .iter()
                .any(|r| r.id == a.id && r.revision > a.revision)
        })
        .map(|a| (&a.id, a.revision, &a.citations))
        .chain(
            request
                .history
                .events
                .iter()
                .filter(|e| {
                    resolved
                        .events
                        .iter()
                        .any(|r| r.id == e.id && r.revision > e.revision)
                })
                .map(|e| (&e.id, e.revision, &e.citations)),
        )
    {
        if !check(citations) {
            core::issue(
                &mut report.trace,
                &format!("{id}@{revision}"),
                "superseded_reference_unverified",
                None,
            );
        }
    }
    let mut used = BTreeSet::new();
    let mut add_refs = |refs: &[Citation]| {
        for c in refs {
            used.insert(c.document.clone());
            if let Some(t) = &c.transcription {
                used.insert(t.clone());
            }
        }
    };
    add_refs(&resolved.identity);
    for a in &resolved.anchors {
        add_refs(&a.citations);
        add_refs(&a.correction);
        for p in &a.securities {
            add_refs(&p.membership);
            add_refs(&p.identity);
        }
    }
    for e in &resolved.events {
        add_refs(&e.citations);
        add_refs(&e.correction);
        for p in &e.securities {
            add_refs(&p.membership);
            add_refs(&p.identity);
        }
    }
    for r in &resolved.reviews {
        add_refs(&r.citations);
    }
    for problem in problems {
        if matches!(
            problem.reason.as_str(),
            "capture_content_unusable" | "image_review_binding_unverified"
        ) && !used.contains(&problem.subject)
        {
            report.trace.push(problem);
        } else {
            report.issues.push(problem);
        }
    }
    for issue in &mut report.issues {
        let id = &issue.subject;
        let refs = resolved
            .anchors
            .iter()
            .find(|a| &a.id == id)
            .map(|a| &a.citations)
            .or_else(|| {
                resolved
                    .events
                    .iter()
                    .find(|e| &e.id == id)
                    .map(|e| &e.citations)
            })
            .or_else(|| {
                resolved
                    .reviews
                    .iter()
                    .find(|r| &r.id == id)
                    .map(|r| &r.citations)
            });
        issue.evidence = refs
            .map(|r| r.iter().map(|c| c.document.clone()).collect())
            .unwrap_or_else(|| {
                if materials.docs.contains_key(id) {
                    vec![id.clone()]
                } else {
                    vec![]
                }
            });
    }
    report.issues.sort_by(|a, b| a.id.cmp(&b.id));
    report.issues.dedup();
    report.trace.sort_by(|a, b| a.id.cmp(&b.id));
    report.trace.dedup();
}
/// Capture one explicitly selected original using existing bounded HTTPS transport.
/// This does not infer identity, membership or coverage from download success.
pub fn capture(
    url: &str,
    format: &str,
    path: &Path,
    context: &TaskContext,
) -> io::Result<Document> {
    let parsed = url::Url::parse(url).map_err(invalid)?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || !matches!(format, "pdf" | "text" | "png" | "jpeg")
    {
        return Err(invalid("invalid history capture URL/format"));
    }
    if path.exists() {
        return Err(invalid("capture destination already exists"));
    }
    context.check()?;
    let output = context.execute(|| {
        crate::transport::curl(
            Command::new("/usr/bin/curl")
                .args([
                    "--disable",
                    "--silent",
                    "--show-error",
                    "--proto",
                    "=https",
                    "--connect-timeout",
                    "10",
                    "--max-time",
                    "40",
                    "--max-filesize",
                    "67108864",
                    "--output",
                ])
                .arg(path)
                .args(["--write-out", "%{http_code}", url]),
        )
    })?;
    let status = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || status != "200" {
        return Err(invalid(format!("history capture failed HTTP {status}")));
    }
    context.check()?;
    let b = fs::read(path)?;
    Ok(Document {
        id: "captured".into(),
        path: path.to_path_buf(),
        sha256: bytes_hash(&b),
        source_url: url.into(),
        format: format.into(),
        coverage_source: false,
        derived_from: None,
    })
}
