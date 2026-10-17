//! Original-document verification for index member history; no model or account execution.
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
}
/// Versioned shared review request; paths never enter the core algorithm.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Format 1.
    pub schema_version: u32,
    /// Structured facts and explicit reviews.
    pub history: History,
    /// Referenced originals.
    pub documents: Vec<Document>,
}
/// Hash original bytes rather than parsed text.
pub fn bytes_hash(bytes: &[u8]) -> String {
    format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(bytes))
    )
}
fn invalid(s: impl std::fmt::Display) -> io::Error {
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
fn text(root: &Path, d: &Document) -> io::Result<String> {
    let url = url::Url::parse(&d.source_url).map_err(invalid)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid("invalid history source URL"));
    }
    let p = document_path(root, &d.path)?;
    let b = fs::read(&p)?;
    if bytes_hash(&b) != d.sha256 {
        return Err(invalid("history evidence hash mismatch"));
    }
    match d.format.as_str() {
        "text" => String::from_utf8(b).map_err(invalid),
        "pdf" => crate::corporate_actions::evidence::extract(&p)?["text"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| invalid("missing extracted PDF text")),
        _ => Err(invalid("unsupported history document format")),
    }
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
    if request.schema_version != 1 {
        return Err(invalid("unsupported membership request format"));
    }
    let mut texts = BTreeMap::new();
    let mut problems = vec![];
    let mut duplicate = BTreeSet::new();
    let mut docs = BTreeMap::new();
    for d in &request.documents {
        context.check()?;
        if docs.insert(d.id.clone(), d).is_some() {
            duplicate.insert(d.id.clone());
        }
        match text(root, d) {
            Ok(t) => {
                texts.insert(d.id.clone(), compact(&t));
            }
            Err(e) => {
                core::issue(&mut problems, &d.id, "evidence_verification_failed", None);
                problems.last_mut().unwrap().details = Some(e.to_string());
            }
        }
    }
    for id in duplicate {
        texts.remove(&id);
        core::issue(&mut problems, &id, "duplicate_document_id", None);
    }
    let check = |citations: &[Citation]| {
        !citations.is_empty()
            && citations.iter().all(|c| {
                !compact(&c.excerpt).is_empty()
                    && texts
                        .get(&c.document)
                        .is_some_and(|t| t.contains(&compact(&c.excerpt)))
            })
    };
    let mut invalid_subjects = BTreeSet::new();
    let identity = check(&request.history.identity)
        && request.history.identity.iter().any(|c| {
            compact(&c.excerpt).contains(&compact(&request.history.index_name))
                && compact(&c.excerpt).contains(&request.history.official_code)
        })
        && !request.history.index_name.trim().is_empty();
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
    for (id, at, members, citations) in request
        .history
        .anchors
        .iter()
        .map(|a| {
            (
                &a.id,
                &a.at,
                a.members.iter().collect::<Vec<_>>(),
                &a.citations,
            )
        })
        .chain(request.history.events.iter().map(|e| {
            (
                &e.id,
                &e.at,
                e.add.iter().chain(&e.remove).collect(),
                &e.citations,
            )
        }))
    {
        if !identity || !check(citations) {
            invalid_subjects.insert(id.clone());
            core::issue(&mut problems, id, "identity_or_citation_unverified", None);
            continue;
        }
        let excerpts = compact(
            &citations
                .iter()
                .map(|c| c.excerpt.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        );
        let day = at.opening(calendar).unwrap_or(at.date);
        for instrument in members {
            match source_map.exchange_symbol(instrument, day) {
                Ok(symbol)
                    if !source_map.is_price_index(instrument)
                        && excerpts.contains(&symbol[2..]) => {}
                _ => {
                    invalid_subjects.insert(id.clone());
                    core::issue(
                        &mut problems,
                        id,
                        "security_identity_or_roster_reference_unverified",
                        None,
                    );
                }
            }
        }
    }
    for r in &request.history.reviews {
        if !identity
            || !check(&r.citations)
            || !r
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
    report.issues.extend(problems);
    report.issues.sort_by(|a, b| a.id.cmp(&b.id));
    report.issues.dedup();
    Ok(report)
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
        || !matches!(format, "pdf" | "text")
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
    })
}
