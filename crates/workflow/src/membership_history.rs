//! Shared index-history review, reconstruction, immutable publication and pool resolution.
use crate::{Result, RunRecord, Workspace, atomic_json, signature};
use analytics_core::{
    Date, DateRange,
    membership_history::{History, RULES_VERSION, Report, Segment},
    pool::{PoolDefinition, PoolExpression},
    source_map::SourceMap,
    task::TaskContext,
};
use analytics_storage::{
    daily::DailyDataset,
    membership_history::{self as storage, Request},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Versioned operations shared by the CLI and App, executed in the owning worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    /// Generate an editable empty scaffold, never a completeness approval.
    Draft {
        /// Selected immutable index configuration.
        index: String,
        /// Frozen exchange calendar and identity mapping.
        market: String,
        /// Requested effective interval.
        range: DateRange,
    },
    /// Verify original bytes, references and the complete reconstruction without publishing.
    Preview {
        /// Selected index version.
        index: String,
        /// Calendar/identity market.
        market: String,
        /// Portable facts with workspace-relative originals.
        request: Box<Request>,
    },
    /// Publish one independently proven continuous interval after fresh validation.
    Publish {
        /// Selected index version.
        index: String,
        /// Calendar/identity market.
        market: String,
        /// Original review facts; no trusted ready flag.
        request: Box<Request>,
        /// Exact selected proven range, not an unstable report array offset.
        range: DateRange,
    },
    /// Import an exported history.json and revalidate all files; does not publish.
    Import {
        /// Selected index version.
        index: String,
        /// Calendar/identity market.
        market: String,
        /// Workspace-relative exported history.json.
        path: PathBuf,
    },
    /// Revalidate a published artifact and its frozen evidence, offline.
    Status {
        /// Immutable history ID.
        version: String,
    },
    /// Export portable history.json and original files into a new workspace-relative directory.
    Export {
        /// Immutable history ID.
        version: String,
        /// New output directory.
        path: PathBuf,
    },
    /// Compare two current history previews under exactly the same requested task context.
    Compare {
        /// Earlier completed preview task.
        before: uuid::Uuid,
        /// Later completed preview task.
        after: uuid::Uuid,
    },
    /// Local material registration, extraction or explicit human reading.
    Material {
        /// Current draft.
        request: Box<Request>,
        /// Selected operation.
        action: crate::history_material::MaterialAction,
    },
    /// Capture explicitly chosen source bytes; this is the only network operation.
    Capture {
        /// Original HTTPS URL.
        url: String,
        /// pdf or text, explicitly selected.
        format: String,
    },
}
/// Frozen source and interpretation contract, independent of mutable download paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// Current artifact format.
    pub schema_version: u32,
    /// Content-derived identifier.
    pub id: String,
    /// Full payload hash.
    pub signature: String,
    /// Selected index configuration with publisher identity.
    pub index: crate::indices::IndexVersion,
    /// Original market/version metadata.
    pub market: Value,
    /// Complete frozen exchange calendar.
    pub calendar: Vec<Date>,
    /// Date-effective security identity rules.
    pub source_map: SourceMap,
    /// Reviewed facts with paths relative to the artifact directory.
    pub request: Request,
    /// Freshly computed report.
    pub report: Report,
    /// Selected continuous interval.
    pub segment: Segment,
}
fn artifact_signature(a: &Artifact) -> Result<String> {
    signature(&json!([
        a.schema_version,
        a.index,
        a.market,
        a.calendar,
        a.source_map,
        a.request,
        a.report,
        a.segment
    ]))
}
fn safe_output(w: &Workspace, p: &Path) -> Result<PathBuf> {
    if p.is_absolute()
        || p.components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        || p.components()
            .next()
            .is_none_or(|c| c.as_os_str() == ".jfquant")
    {
        return Err("history export requires a non-managed workspace-relative directory".into());
    }
    let root = w.root.canonicalize()?;
    let parent = w
        .resolve(p)
        .parent()
        .ok_or("export parent missing")?
        .canonicalize()?;
    if !parent.starts_with(root) || w.resolve(p).exists() {
        return Err("history export exists or escapes workspace".into());
    }
    Ok(w.resolve(p))
}
fn imported(w: &Workspace, path: &Path) -> Result<Request> {
    let path = storage::document_path(&w.root, path)?;
    let mut r: Request = serde_json::from_slice(&fs::read(&path)?)?;
    let parent = path.parent().unwrap().strip_prefix(&w.root)?;
    for d in &mut r.documents {
        // Resolve relative to export directory, never to arbitrary imported absolute paths.
        if d.path.is_absolute()
            || d.path
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("invalid imported evidence path".into());
        }
        d.path = parent.join(&d.path);
    }
    Ok(r)
}
/// Freeze task-specific resources and evidence bytes at submission; status never captures data.
pub(crate) fn freeze(w: &Workspace, op: &Operation) -> Result<Value> {
    match op {
        Operation::Draft { index, market, .. }
        | Operation::Preview { index, market, .. }
        | Operation::Publish { index, market, .. }
        | Operation::Import { index, market, .. } => {
            let index = w.index(index)?;
            let market = crate::version_metadata(w, market, "market")?;
            let ds = DailyDataset::open(w.resolve(&market.path))?;
            let request = match op {
                Operation::Preview { request, .. } | Operation::Publish { request, .. } => {
                    Some(*request.clone())
                }
                Operation::Import { path, .. } => Some(imported(w, path)?),
                _ => None,
            };
            if let Some(r) = &request
                && r.history.official_code != index.config.definition.official_code
            {
                return Err("history index identity differs from selected configuration".into());
            }
            let hashes = request
                .as_ref()
                .map(|r| {
                    r.documents
                        .iter()
                        .map(|d| {
                            Ok((
                                d.path.clone(),
                                storage::bytes_hash(&fs::read(storage::document_path(
                                    &w.root, &d.path,
                                )?)?),
                            ))
                        })
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?;
            Ok(
                json!({"index":index,"market":market,"calendar":ds.calendar,"source_map":ds.source_map(),"review_request":request,"original_hashes":hashes,"rules_version":RULES_VERSION}),
            )
        }
        Operation::Status { version } | Operation::Export { version, .. } => {
            Ok(json!({"artifact":w.history(version)?.signature}))
        }
        Operation::Compare { before, after } => {
            crate::history_diagnostics::snapshot(w, *before, *after)
        }
        Operation::Material { request, action } => {
            crate::history_material::freeze(w, request, action)
        }
        Operation::Capture { .. } => Ok(json!({"rules_version":RULES_VERSION})),
    }
}
/// Worker entrypoint; all clients receive the same structured report.
pub(crate) fn execute(
    w: &Workspace,
    op: &Operation,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<Value> {
    context.check()?;
    if freeze(w, op)? != run.request["membership_history"] {
        return Err("history task inputs changed since submission".into());
    }
    let frozen = &run.request["membership_history"];
    match op {
        Operation::Draft { range, .. } => {
            let index: crate::indices::IndexVersion =
                serde_json::from_value(frozen["index"].clone())?;
            Ok(
                json!({"history_request":Request{schema_version:2,history:History{schema_version:2,official_code:index.config.definition.official_code,index_name:index.config.definition.name,identity:vec![],requested:range.clone(),anchors:vec![],events:vec![],reviews:vec![]},documents:vec![],transcriptions:vec![]},"ready":false,"purpose":"editable scaffold, no completeness inferred"}),
            )
        }
        Operation::Preview { .. } | Operation::Import { .. } | Operation::Publish { .. } => {
            context.report("核验指数身份、原文引用与事件链并重建历史成员")?;
            let request: Request = serde_json::from_value(frozen["review_request"].clone())?;
            let calendar: Vec<Date> = serde_json::from_value(frozen["calendar"].clone())?;
            let source_map: SourceMap = serde_json::from_value(frozen["source_map"].clone())?;
            let report = storage::verify(&w.root, &request, &calendar, &source_map, context)?;
            let task_context = signature(&json!([
                frozen["index"]["calculation_signature"],
                frozen["market"],
                frozen["calendar"],
                frozen["source_map"],
                request.history.requested,
                RULES_VERSION
            ]))?;
            let integrity_failed = report.issues.iter().any(|i| {
                matches!(
                    i.reason.as_str(),
                    "evidence_verification_failed"
                        | "duplicate_document_id"
                        | "image_review_binding_unverified"
                )
            });
            let mut result = json!({"schema_version":2,"task_context_signature":task_context,"history_request":request,"history_report":report,"context_signature":signature(frozen)?,"ready":!integrity_failed && !report.segments.is_empty(),"facts_complete":report.missing_dates.is_empty(),"task_ready":!integrity_failed && report.missing_dates.is_empty() && calendar.iter().filter(|d|request.history.requested.contains(**d)).all(|d|report.segments.iter().any(|s|s.point_in_time_dates.contains(d))),"review_semantics":"evidence-backed attributed human completeness review; no automated semantic proof"});
            if let Operation::Publish { range, .. } = op {
                if report.issues.iter().any(|i| {
                    matches!(
                        i.reason.as_str(),
                        "evidence_verification_failed"
                            | "duplicate_document_id"
                            | "image_review_binding_unverified"
                    )
                }) {
                    return Err(
                        "history integrity/revision binding failed; inspect current diagnostics"
                            .into(),
                    );
                }
                let segment = report
                    .segments
                    .iter()
                    .find(|s| s.membership.coverage == *range)
                    .ok_or("selected continuous range is not proven; inspect preview")?
                    .clone();
                context.check()?;
                let staging = w.resolve(format!(".jfquant/staging/history-{}", run.id));
                if staging.exists() {
                    fs::remove_dir_all(&staging)?;
                }
                fs::create_dir_all(staging.join("evidence"))?;
                let mut portable = request.clone();
                for (i, d) in portable.documents.iter_mut().enumerate() {
                    context.check()?;
                    let b = fs::read(storage::document_path(&w.root, &d.path)?)?;
                    if storage::bytes_hash(&b) != d.sha256 {
                        return Err("history original changed during publication".into());
                    }
                    d.path = PathBuf::from(format!(
                        "evidence/{i}.{}",
                        if d.format == "text" { "txt" } else { &d.format }
                    ));
                    fs::write(staging.join(&d.path), b)?;
                }
                let mut a = Artifact {
                    schema_version: 2,
                    id: String::new(),
                    signature: String::new(),
                    index: serde_json::from_value(frozen["index"].clone())?,
                    market: frozen["market"].clone(),
                    calendar,
                    source_map,
                    request: portable,
                    report,
                    segment,
                };
                a.signature = artifact_signature(&a)?;
                a.id = format!("history-{}", &a.signature[7..]);
                atomic_json(&staging.join("artifact.json"), &a)?;
                // Verify the exact copied bytes and recompute before publication.
                let copied =
                    storage::verify(&staging, &a.request, &a.calendar, &a.source_map, context)?;
                if copied != a.report {
                    return Err("copied history evidence failed verification".into());
                }
                context.check()?;
                let destination = w.resolve(format!(".jfquant/histories/{}", a.id));
                fs::create_dir_all(destination.parent().unwrap())?;
                if destination.exists() {
                    w.history(&a.id)?;
                    fs::remove_dir_all(&staging)?;
                } else {
                    fs::rename(&staging, &destination)?;
                }
                result["history_version"] = json!(a.id);
                result["pool_definition"] = json!(PoolDefinition {
                    schema_version: 3,
                    name: a.index.config.definition.name,
                    coverage: range.clone(),
                    expression: PoolExpression::History { version: a.id },
                    adjustments: vec![]
                });
            }
            Ok(result)
        }
        Operation::Status { version } => Ok(json!({"history_artifact":w.history(version)?})),
        Operation::Export { version, path } => {
            let a = w.history(version)?;
            let dest = safe_output(w, path)?;
            let temp = w.resolve(format!(".jfquant/staging/history-export-{}", run.id));
            fs::create_dir_all(temp.join("evidence"))?;
            for d in &a.request.documents {
                context.check()?;
                fs::copy(
                    w.resolve(format!(".jfquant/histories/{version}"))
                        .join(&d.path),
                    temp.join(&d.path),
                )?;
            }
            atomic_json(&temp.join("history.json"), &a.request)?;
            context.check()?;
            fs::rename(temp, dest)?;
            Ok(json!({"output":path,"history_version":version}))
        }
        Operation::Compare { .. } => {
            crate::history_diagnostics::compare(&frozen["before"], &frozen["after"])
        }
        Operation::Material { request, action } => {
            crate::history_material::execute(w, request, action, run, context)
        }
        Operation::Capture { url, format } => {
            let path = run.output.join(format!(
                "history-original.{}",
                if format == "text" { "txt" } else { format }
            ));
            let mut d = storage::capture(url, format, &w.resolve(&path), context)?;
            d.path = path;
            Ok(
                json!({"schema_version":2,"download":"http_200_original_retained","material_status":analytics_storage::membership_history_material::inspect(&w.root,&d),"history_document":d,"retrieved_at":chrono::Utc::now(),"membership_verified":false,"complete_coverage_verified":false}),
            )
        }
    }
}
impl Workspace {
    /// Read and revalidate a frozen history against its own copied originals and rules.
    pub fn history(&self, id: &str) -> Result<Artifact> {
        if !id.starts_with("history-")
            || id.len() != 72
            || !id[8..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("invalid history ID".into());
        }
        let root = self.resolve(format!(".jfquant/histories/{id}"));
        let a: Artifact = serde_json::from_slice(&fs::read(root.join("artifact.json"))?)?;
        if a.schema_version != 2
            || a.id != id
            || a.signature != artifact_signature(&a)?
            || format!("history-{}", &a.signature[7..]) != id
            || a.report.rules_version != RULES_VERSION
        {
            return Err("history artifact or rules changed".into());
        }
        let report = storage::verify(
            &root,
            &a.request,
            &a.calendar,
            &a.source_map,
            &TaskContext::current(),
        )?;
        if report != a.report || !report.segments.contains(&a.segment) {
            return Err("frozen history evidence or reconstruction changed".into());
        }
        Ok(a)
    }
}

/// Read the explicit history dependencies of a current pool resolution.
/// Builtin/index pools use their distinct market-backed contract, without history artifacts.
pub(crate) fn history_inputs(frozen: &Value) -> Result<&[Value]> {
    if let Some(id) = frozen.get("pool_version").and_then(Value::as_str)
        && (id.starts_with("builtin:") || id.starts_with("index:"))
    {
        return Ok(&[]);
    }
    if frozen["resolution_schema_version"] != 1 {
        return Err("unsupported composed pool resolution; resolve the pool again".into());
    }
    frozen
        .get("histories")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| {
            "composed pool resolution requires a histories array; resolve the pool again".into()
        })
}

/// Verify only the dates used by this task; inventory and factual previews remain usable.
pub fn verify_task_use(
    w: &Workspace,
    frozen: &Value,
    market: &str,
    ranges: &[DateRange],
) -> Result<()> {
    let histories = history_inputs(frozen)?;
    if histories.is_empty() {
        return Ok(());
    }
    let m = crate::version_metadata(w, market, "market")?;
    let ds = DailyDataset::open(w.resolve(m.path))?;
    for value in histories {
        let a: Artifact = serde_json::from_value(value.clone())?;
        let live = w.history(&a.id)?;
        if live.signature != a.signature {
            return Err("frozen history signature changed".into());
        }
        for day in ds
            .calendar
            .iter()
            .copied()
            .filter(|d| ranges.iter().any(|r| r.contains(*d)))
        {
            if !a.segment.point_in_time_dates.contains(&day) {
                return Err(format!("history point-in-time availability unverified:{day}").into());
            }
            if !a.calendar.contains(&day) {
                return Err("history calendar contract changed".into());
            }
            for id in a.segment.membership.on(day) {
                if ds.source_map().exchange_symbol(&id, day)?
                    != a.source_map.exchange_symbol(&id, day)?
                {
                    return Err("history security identity changed".into());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use analytics_core::membership_history::{
        Anchor, Boundary, Category, Citation, Moment, Review,
    };
    use std::collections::BTreeSet;
    #[test]
    fn composed_resolution_requires_current_format_and_explicit_dependencies() {
        assert!(history_inputs(&json!({"version":{},"membership":{}})).is_err());
        for bad in [Value::Null, json!({}), json!("none")] {
            assert!(
                history_inputs(&json!({"resolution_schema_version":1,"histories":bad})).is_err()
            );
        }
        assert!(history_inputs(&json!({"resolution_schema_version":1})).is_err());
        assert!(history_inputs(&json!({"resolution_schema_version":2,"histories":[]})).is_err());
        assert!(
            history_inputs(&json!({"resolution_schema_version":1,"histories":[]}))
                .unwrap()
                .is_empty()
        );
        assert!(
            history_inputs(&json!({"pool_version":"builtin:csi500"}))
                .unwrap()
                .is_empty()
        );
        assert!(
            history_inputs(&json!({"pool_version":"index:msci_a50"}))
                .unwrap()
                .is_empty()
        );
        assert!(history_inputs(&json!({"pool_version":"pool-deadbeef"})).is_err());
    }
    fn fixture(w: &Workspace) -> (String, Request) {
        let (market, _, _) = crate::demo::create(w).unwrap();
        let ds = DailyDataset::open(w.resolve(&market.path)).unwrap();
        let days = &ds.calendar[..3];
        let range = DateRange {
            start: days[0],
            end: days[2],
        };
        let text = "Fixture index 000300 complete roster SH600000 600000. Complete event delivery: review interim merger spinoff code_change; independently reconciled; no changes.";
        fs::write(w.resolve("original.txt"), text).unwrap();
        let cite = || {
            vec![Citation {
                transcription: None,
                document: "source".into(),
                excerpt: text.into(),
            }]
        };
        let r = Request {
            schema_version: 2,
            history: History {
                schema_version: 2,
                official_code: "000300".into(),
                index_name: "Fixture index".into(),
                identity: cite(),
                requested: range.clone(),
                anchors: vec![Anchor {
                    revision: 1,
                    supersedes: None,
                    correction: vec![],
                    securities: vec![analytics_core::membership_history::SecurityEvidence {
                        instrument: "SH600000".into(),
                        name: "SH600000".into(),
                        membership: cite(),
                        identity: cite(),
                    }],
                    id: "roster".into(),
                    at: Moment {
                        date: days[0],
                        boundary: Boundary::BeforeOpen,
                    },
                    known_at: Some("1990-01-01T00:00:00Z".parse().unwrap()),
                    members: BTreeSet::from(["SH600000".into()]),
                    citations: cite(),
                }],
                events: vec![],
                reviews: vec![Review {
                    outcome: analytics_core::membership_history::ReviewOutcome::Coverage,
                    id: "review".into(),
                    range,
                    categories: Category::all(),
                    reviewer: "fixture reviewer".into(),
                    basis: "complete source independently reconciled for every category".into(),
                    citations: cite(),
                }],
            },
            transcriptions: vec![],
            documents: vec![storage::Document {
                id: "source".into(),
                path: "original.txt".into(),
                sha256: storage::bytes_hash(text.as_bytes()),
                source_url: "https://example.com/synthetic-history-fixture".into(),
                format: "text".into(),
                coverage_source: true,
                derived_from: None,
            }],
        };
        (market.id, r)
    }
    fn task(w: &Workspace, op: Operation) -> Result<Value> {
        let ctx = TaskContext::default();
        let request = crate::WorkflowRequest::Index {
            request: crate::indices::IndexOperation::History {
                request: Box::new(op),
            },
        };
        let run = w.begin(ctx.id, "history test", crate::snapshot(w, &request)?)?;
        Ok(crate::execute(w, &request, &run, &ctx)?.details)
    }
    #[test]
    fn history_retired_operation_and_versions_are_rejected() {
        assert!(
            serde_json::from_value::<Operation>(json!({
                "operation":"migrate", "path":"history.json", "artifact":false
            }))
            .is_err()
        );
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (market, request) = fixture(&w);
        for outer in [false, true] {
            let mut old = request.clone();
            if outer {
                old.schema_version = 1;
            } else {
                old.history.schema_version = 1;
            }
            assert!(
                task(
                    &w,
                    Operation::Preview {
                        index: "builtin:csi300".into(),
                        market: market.clone(),
                        request: Box::new(old)
                    }
                )
                .is_err()
            );
        }
        assert!(
            task(
                &w,
                Operation::Preview {
                    index: "builtin:csi300".into(),
                    market,
                    request: Box::new(request)
                }
            )
            .unwrap()["task_ready"]
                .as_bool()
                .unwrap()
        );
    }
    #[test]
    fn history_publish_copy_export_import_pool_and_tamper() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (market, r) = fixture(&w);
        let v = task(
            &w,
            Operation::Publish {
                index: "builtin:csi300".into(),
                market: market.clone(),
                range: r.history.requested.clone(),
                request: Box::new(r.clone()),
            },
        )
        .unwrap();
        let id = v["history_version"].as_str().unwrap();
        let pool = w
            .save_pool(serde_json::from_value(v["pool_definition"].clone()).unwrap())
            .unwrap();
        let (members, frozen) = w.resolve_pool(&pool.id, &market).unwrap();
        assert_eq!(members.on(r.history.requested.start).len(), 1);
        verify_task_use(
            &w,
            &frozen,
            &market,
            std::slice::from_ref(&r.history.requested),
        )
        .unwrap();
        // Original mutable import path is no longer required.
        fs::remove_file(w.resolve("original.txt")).unwrap();
        assert!(w.history(id).is_ok());
        task(
            &w,
            Operation::Export {
                version: id.into(),
                path: "exported".into(),
            },
        )
        .unwrap();
        let imported = task(
            &w,
            Operation::Import {
                index: "builtin:csi300".into(),
                market: market.clone(),
                path: "exported/history.json".into(),
            },
        )
        .unwrap();
        assert_eq!(imported["history_report"], v["history_report"]);
        let again: Request = serde_json::from_value(imported["history_request"].clone()).unwrap();
        let repeat = task(
            &w,
            Operation::Publish {
                index: "builtin:csi300".into(),
                market: market.clone(),
                range: r.history.requested.clone(),
                request: Box::new(again),
            },
        )
        .unwrap();
        assert_eq!(v["history_version"], repeat["history_version"]);
        let a = w.history(id).unwrap();
        fs::write(
            w.resolve(format!(".jfquant/histories/{id}"))
                .join(&a.request.documents[0].path),
            "tampered",
        )
        .unwrap();
        assert!(w.history(id).is_err());
        assert!(w.resolve_pool(&pool.id, &market).is_err());
    }
    #[test]
    fn history_missing_identity_coverage_evidence_and_known_time() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (market, mut r) = fixture(&w);
        r.documents[0].coverage_source = false;
        r.history.anchors[0].known_at = None;
        let v = task(
            &w,
            Operation::Preview {
                index: "builtin:csi300".into(),
                market: market.clone(),
                request: Box::new(r.clone()),
            },
        )
        .unwrap();
        assert_eq!(
            v["history_report"]["missing_dates"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        r.documents[0].coverage_source = true;
        let v = task(
            &w,
            Operation::Publish {
                index: "builtin:csi300".into(),
                market: market.clone(),
                range: r.history.requested.clone(),
                request: Box::new(r.clone()),
            },
        )
        .unwrap();
        let pool = w
            .save_pool(serde_json::from_value(v["pool_definition"].clone()).unwrap())
            .unwrap();
        let (_, frozen) = w.resolve_pool(&pool.id, &market).unwrap();
        assert!(
            verify_task_use(
                &w,
                &frozen,
                &market,
                std::slice::from_ref(&r.history.requested)
            )
            .is_err()
        );
        r.history.identity[0].excerpt = "another index".into();
        let v = task(
            &w,
            Operation::Preview {
                index: "builtin:csi300".into(),
                market,
                request: Box::new(r),
            },
        )
        .unwrap();
        assert_eq!(v["ready"], false);
    }
    #[test]
    fn history_task_freezes_originals_and_cancel_never_publishes() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (market, r) = fixture(&w);
        let op = Operation::Publish {
            index: "builtin:csi300".into(),
            market,
            range: r.history.requested.clone(),
            request: Box::new(r),
        };
        let request = crate::WorkflowRequest::Index {
            request: crate::indices::IndexOperation::History {
                request: Box::new(op),
            },
        };
        let ctx = TaskContext::default();
        let run = w
            .begin(
                ctx.id,
                "frozen history",
                crate::snapshot(&w, &request).unwrap(),
            )
            .unwrap();
        fs::write(w.resolve("original.txt"), "changed").unwrap();
        assert!(crate::execute(&w, &request, &run, &ctx).is_err());
        ctx.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(crate::execute(&w, &request, &run, &ctx).is_err());
        assert!(!w.resolve(".jfquant/histories").exists());
    }
    #[test]
    fn history_image_review_and_multi_document_security_identity() {
        use crate::history_material::MaterialAction;
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (market, mut r) = fixture(&w);
        let bytes: &[u8] = &[
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 8, 0, 0, 0, 1,
            8, 2, 0, 0, 0, 108, 98, 120, 20, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 143,
            3, 0, 0, 42, 252, 23, 233, 184, 254, 58, 40, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
            130,
        ];
        fs::write(w.resolve("roster.png"), bytes).unwrap();
        r.documents.push(storage::Document {
            id: "raster".into(),
            path: "roster.png".into(),
            sha256: storage::bytes_hash(bytes),
            source_url: "https://example.com/synthetic-roster.png".into(),
            format: "png".into(),
            coverage_source: false,
            derived_from: None,
        });
        let image_cite = vec![Citation {
            document: "raster".into(),
            excerpt: "Member SH600000".into(),
            transcription: Some("reading".into()),
        }];
        r.history.anchors[0].citations = image_cite.clone();
        r.history.anchors[0].securities[0].membership = image_cite;
        r.transcriptions.push(storage::Transcription {
            id: "reading".into(),
            document: "raster".into(),
            sha256: storage::bytes_hash(bytes),
            region: [0, 0, 8, 1],
            text: "Member SH600000".into(),
            reviewer: String::new(),
            reviewed_at: None,
            confirmed: false,
            receipt: None,
        });
        let review = task(
            &w,
            Operation::Material {
                request: Box::new(r),
                action: MaterialAction::Review {
                    transcription: "reading".into(),
                    reviewer: "fixture reviewer".into(),
                },
            },
        )
        .unwrap();
        let mut r: Request = serde_json::from_value(review["history_request"].clone()).unwrap();
        let before = task(
            &w,
            Operation::Preview {
                index: "builtin:csi300".into(),
                market: market.clone(),
                request: Box::new(r.clone()),
            },
        )
        .unwrap();
        assert_eq!(before["task_ready"], true);
        let published = task(
            &w,
            Operation::Publish {
                index: "builtin:csi300".into(),
                market: market.clone(),
                request: Box::new(r.clone()),
                range: r.history.requested.clone(),
            },
        )
        .unwrap();
        assert!(
            w.history(published["history_version"].as_str().unwrap())
                .is_ok()
        );
        r.transcriptions[0].text.push('!');
        let after = task(
            &w,
            Operation::Preview {
                index: "builtin:csi300".into(),
                market,
                request: Box::new(r),
            },
        )
        .unwrap();
        assert_eq!(after["ready"], false);
        let comparison = crate::history_diagnostics::compare(&before, &after).unwrap();
        assert!(
            !comparison["newly_unproven_dates"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let mut other = after.clone();
        other["task_context_signature"] = json!("another task");
        assert!(crate::history_diagnostics::compare(&before, &other).is_err());
    }
    #[test]
    fn history_retired_content_failure_is_trace_but_tampering_blocks_publication() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (market, mut r) = fixture(&w);
        fs::write(w.resolve("old.txt"), "<meta name=\"aliyun_waf_aa\">").unwrap();
        r.documents.push(storage::Document {
            id: "old".into(),
            path: "old.txt".into(),
            sha256: storage::bytes_hash(&fs::read(w.resolve("old.txt")).unwrap()),
            source_url: "https://example.com/retired-source".into(),
            format: "text".into(),
            coverage_source: false,
            derived_from: None,
        });
        let v = task(
            &w,
            Operation::Preview {
                index: "builtin:csi300".into(),
                market: market.clone(),
                request: Box::new(r.clone()),
            },
        )
        .unwrap();
        assert_eq!(v["task_ready"], true);
        assert!(!v["history_report"]["trace"].as_array().unwrap().is_empty());
        fs::write(w.resolve("old.txt"), "changed original").unwrap();
        assert!(
            task(
                &w,
                Operation::Publish {
                    index: "builtin:csi300".into(),
                    market,
                    range: r.history.requested.clone(),
                    request: Box::new(r)
                }
            )
            .is_err()
        );
    }
}
