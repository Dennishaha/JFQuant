//! Shared agent and desktop evidence maintenance tasks; no client-side repair scripts.
use crate::{
    Result, RunRecord, WorkflowOutcome, Workspace, atomic_json, market_content_hash, signature,
};
use analytics_core::task::TaskContext;
use analytics_storage::corporate_actions::{ActionBundle, evidence, review};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, path::PathBuf};
use uuid::Uuid;

/// Versioned workflow operations for selected evidence and immutable review preparation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvidenceRequest {
    /// Capture explicitly selected PDFs without claiming a complete announcement search.
    Capture {
        /// Public selected-document specification.
        request: evidence::CaptureRequest,
    },
    /// Recover independently verifiable completed output from a terminal collection task.
    Recover {
        /// Original task, including failed/cancelled batches.
        task: Uuid,
    },
    /// Retry the exact original native request, reusing its verified checkpoints.
    Retry {
        /// Terminal original collection task, not a draft or publication task.
        task: Uuid,
    },
    /// List unresolved dependencies and original evidence references.
    Status {
        /// Selected immutable action version.
        actions: String,
        /// Optional security filter.
        instrument: Option<String>,
    },
    /// Generate a review scaffold, or validate and rebase an existing reviewed proposal.
    Draft {
        /// Target immutable action version.
        actions: String,
        /// Previously reviewed proposals; None generates an editable scaffold.
        request: Option<Box<review::RevisionRequest>>,
        /// Security for a new scaffold.
        instrument: Option<String>,
        /// Captured manifest for a new scaffold.
        evidence_manifest: Option<PathBuf>,
    },
}
impl EvidenceRequest {
    pub(crate) fn actions(&self) -> Option<&str> {
        match self {
            Self::Status { actions, .. } | Self::Draft { actions, .. } => Some(actions),
            _ => None,
        }
    }
    pub(crate) fn input_roots(&self, w: &Workspace) -> Result<Vec<PathBuf>> {
        match self {
            Self::Recover { task } | Self::Retry { task } => {
                let root = recovery_root(w, *task)?;
                if fs::symlink_metadata(&root)?.file_type().is_symlink() {
                    return Err("evidence root cannot be a link".into());
                }
                Ok(vec![evidence_path(w, root)?])
            }
            Self::Draft {
                request: Some(request),
                evidence_manifest,
                ..
            } => {
                let mut paths = vec![
                    evidence_manifest
                        .clone()
                        .unwrap_or_else(|| request.evidence_manifest.clone()),
                ];
                paths.extend(request.structured_manifest.clone());
                paths
                    .into_iter()
                    .map(|p| evidence_path(w, p).map(|p| p.parent().unwrap().to_path_buf()))
                    .collect()
            }
            Self::Draft {
                evidence_manifest: Some(p),
                ..
            } => Ok(vec![
                evidence_path(w, p.clone())?.parent().unwrap().to_path_buf(),
            ]),
            _ => Ok(vec![]),
        }
    }
}
fn evidence_path(w: &Workspace, path: PathBuf) -> Result<PathBuf> {
    let path = w.resolve(path).canonicalize()?;
    if !path.starts_with(w.root.canonicalize()?) {
        return Err("evidence must belong to this workspace".into());
    }
    Ok(path)
}
fn recovery_root(w: &Workspace, task: Uuid) -> Result<PathBuf> {
    let record: RunRecord = serde_json::from_slice(&fs::read(
        w.resolve(format!(".jfquant/runs/{task}/run.json")),
    )?)?;
    if record.id != task || record.finished_at.is_none() {
        return Err("recovery requires a terminal task".into());
    }
    if record.request["request"]["operation"] == "evidence"
        && record.request["request"]["request"]["operation"] == "capture"
    {
        return Ok(w.resolve(&record.output).join("captured"));
    }
    if record.request["request"]["operation"] == "download_actions" {
        let owner = record.request["request"]["resume"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| task.to_string());
        let published = w.resolve(format!("data/actions-{task}"));
        return Ok(if published.is_dir() {
            published
        } else {
            w.resolve(format!(".jfquant/staging/actions-{owner}"))
        });
    }
    crate::usecases::reconciliation_evidence_root(w, task, &record)
}
/// Resolve and freeze evidence trees with the original request at submission.
pub(crate) fn freeze(w: &Workspace, request: &EvidenceRequest) -> Result<Value> {
    if let EvidenceRequest::Capture { request } = request {
        request.validate()?;
    }
    let mut inputs = request
        .input_roots(w)?
        .into_iter()
        .map(|p| Ok(json!({"path":p,"sha256":market_content_hash(&p)?})))
        .collect::<Result<Vec<_>>>()?;
    if let EvidenceRequest::Recover { task } | EvidenceRequest::Retry { task } = request {
        let path = w.resolve(format!(".jfquant/runs/{task}/run.json"));
        inputs.push(
            json!({"source_record_sha256":analytics_storage::market_view::file_hash(&path)?}),
        );
    }
    Ok(Value::from(inputs))
}
fn retry_request(w: &Workspace, task: Uuid) -> Result<crate::WorkflowRequest> {
    let record: RunRecord = serde_json::from_slice(&fs::read(
        w.resolve(format!(".jfquant/runs/{task}/run.json")),
    )?)?;
    let mut original: crate::WorkflowRequest =
        serde_json::from_value(record.request["request"].clone())?;
    match &mut original {
        crate::WorkflowRequest::CollectActions { resume, .. }
        | crate::WorkflowRequest::CollectAnnouncements { resume, .. }
        | crate::WorkflowRequest::DownloadActions { resume, .. } => {
            *resume = Some(resume.unwrap_or(task));
        }
        _ => {
            return Err(
                "retry requires an original native action or announcement collection task".into(),
            );
        }
    }
    Ok(original)
}
/// Execute on the task worker; old versions and source task statuses stay unchanged.
pub(crate) fn execute(
    w: &Workspace,
    request: &EvidenceRequest,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<WorkflowOutcome> {
    if freeze(w, request)? != run.request["evidence_inputs"] {
        return Err("evidence changed after request freeze".into());
    }
    let root = w.resolve(&run.output);
    let mut data = None;
    let details = match request {
        EvidenceRequest::Capture { request } => {
            evidence::capture(request, &root.join("captured"), context)?
        }
        EvidenceRequest::Retry { task } => {
            let original = retry_request(w, *task)?;
            let outcome = crate::usecases::execute(w, &original, run, context)?;
            let details = json!({"source_task":task,"result":outcome.details,"data":outcome.data});
            atomic_json(&root.join("evidence-report.json"), &details)?;
            return Ok(WorkflowOutcome {
                data: outcome.data,
                details,
            });
        }
        EvidenceRequest::Recover { task } => {
            let source = recovery_root(w, *task)?;
            let output = w.resolve(format!("data/recovered-evidence-{}", run.id));
            let actions = source.join("actions.json");
            let kind = if actions.is_file() {
                let book = ActionBundle::read(&actions)?;
                crate::verify_action_evidence(&actions, &book)?;
                evidence::copy_tree(&source, &output)?;
                let path = output.join("actions.json");
                data = Some(w.register(&path)?);
                "actions"
            } else {
                if source.join("announcements.json").is_file() {
                    review::verified_documents(&source.join("announcements.json"))?;
                    evidence::copy_tree(&source, &output)?;
                    review::verified_documents(&output.join("announcements.json"))?;
                } else {
                    evidence::recover_announcements(&source, &output)?;
                }
                "announcements"
            };
            let record: Value = serde_json::from_slice(&fs::read(
                w.resolve(format!(".jfquant/runs/{task}/run.json")),
            )?)?;
            let provenance = json!({"schema_version":1,"source_task":task,"source_status":record["status"],"source_signature":record["signature"],"source_tree_sha256":market_content_hash(&source)?,"kind":kind,"full_history_asserted":false});
            atomic_json(&output.join("recovery.json"), &provenance)?;
            json!({"root":output,"manifest":output.join(if kind=="actions" {"actions.json"} else {"announcements.json"}),"recovery":provenance})
        }
        EvidenceRequest::Status {
            actions,
            instrument,
        } => {
            if let Some(id) = instrument {
                analytics_core::pool::validate_instrument(id)?;
            }
            let source = crate::usecases::version(w, actions, "actions")?;
            let book = ActionBundle::read(&w.resolve(&source.path))?;
            let ids = book
                .queried_symbols
                .iter()
                .chain(book.failures.keys())
                .chain(book.execution_issues.iter().map(|i| &i.instrument))
                .chain(book.actions.iter().map(|a| &a.instrument))
                .collect::<std::collections::BTreeSet<_>>();
            let entries=ids.into_iter().filter(|id|instrument.as_ref().is_none_or(|s|s==*id)).map(|id|Ok(json!({"instrument":id,"queried":book.queried_symbols.contains(id),"failure":book.failures.get(id),"coverage":book.structured_coverage.get(id),"event_count":book.actions.iter().filter(|a|a.instrument==*id).count(),"issues":book.execution_issues.iter().filter(|i|i.instrument==*id).map(|i|Ok(json!({"issue":i,"sha256":signature(i)?}))).collect::<Result<Vec<_>>>()?,"failure_receipt":book.raw_sha256.get(&format!("{id}-actions-failure.json")).map(|h|json!({"filename":format!("{id}-actions-failure.json"),"sha256":h}))}))).collect::<Result<Vec<_>>>()?;
            json!({"actions":actions,"source_signature":source.signature,"evidence_root":w.resolve(&source.path).parent().unwrap().join("raw"),"entries":entries,"source_failures":book.failures.len(),"queried":book.queried_symbols.len(),"next_steps":["capture selected PDFs","recover terminal batches","draft and validate revisions","revise-actions with the saved request","rerun task admission"]})
        }
        EvidenceRequest::Draft {
            actions,
            request,
            instrument,
            evidence_manifest,
        } => {
            let source = crate::usecases::version(w, actions, "actions")?;
            let source_path = w.resolve(&source.path);
            let book = ActionBundle::read(&source_path)?;
            let (mut draft, unchanged) = if let Some(request) = request {
                evidence::draft(&book, request)?
            } else {
                let id = instrument
                    .as_ref()
                    .ok_or("draft scaffold requires instrument")?;
                analytics_core::pool::validate_instrument(id)?;
                (
                    review::RevisionRequest {
                        schema_version: 3,
                        evidence_manifest: evidence_manifest
                            .clone()
                            .ok_or("draft scaffold requires evidence manifest")?,
                        structured_manifest: None,
                        events: book
                            .actions
                            .iter()
                            .filter(|a| a.instrument == *id)
                            .map(|a| {
                                Ok(review::EventRevision {
                                    original_id: Some(a.id.clone()),
                                    original_sha256: Some(signature(a)?),
                                    replacement: a.clone(),
                                    reason: String::new(),
                                    fields: Default::default(),
                                    structured_fields: Default::default(),
                                })
                            })
                            .collect::<Result<_>>()?,
                        resolve_failures: vec![],
                        resolve_issues: vec![],
                        coverage_reviews: vec![],
                    },
                    vec![],
                )
            };
            if let Some(path) = evidence_manifest {
                draft.evidence_manifest = path.clone();
            }
            draft.evidence_manifest = evidence_path(w, draft.evidence_manifest)?;
            if let Some(path) = draft.structured_manifest {
                draft.structured_manifest = Some(evidence_path(w, path)?);
            }
            let manifest: Value = serde_json::from_slice(&fs::read(&draft.evidence_manifest)?)?;
            let documents = if manifest["schema_version"] == 2 {
                review::verified_documents(&draft.evidence_manifest)?
            } else {
                Default::default()
            };
            let validation = if request.is_some()
                && (!draft.events.is_empty() || !draft.coverage_reviews.is_empty())
            {
                match review::apply(
                    &book,
                    source_path.parent().unwrap(),
                    &draft,
                    &root.join("validated-preview"),
                ) {
                    Ok(_) => json!({"ready":true}),
                    Err(e) => json!({"ready":false,"error":e.to_string()}),
                }
            } else {
                json!({"ready":false,"reason":if request.is_some(){"all proposed events already match; no publication needed"}else{"editable scaffold; add reviewed changes, exact quotes and units before validation"}})
            };
            let path = root.join("review-request.json");
            atomic_json(&path, &draft)?;
            json!({"request_path":path,"apply_argv":["jfquant","--workspace",w.root.to_string_lossy(),"data","revise-actions","--actions",actions,"--request",path],"draft_request":draft,"validation":validation,"unchanged_events":unchanged,"source_signature":source.signature,"issues":book.execution_issues.iter().filter(|i|instrument.as_ref().is_none_or(|id|i.instrument==*id)).map(|i|Ok(json!({"issue":i,"sha256":signature(i)?}))).collect::<Result<Vec<_>>>()?,"documents":documents.into_values().filter(|(d,_)|instrument.as_ref().is_none_or(|id|d["instrument"]==*id)).map(|(document,text)|json!({"document":document,"extracted_text":text})).collect::<Vec<_>>()})
        }
    };
    atomic_json(&root.join("evidence-report.json"), &details)?;
    Ok(WorkflowOutcome { data, details })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RunStatus, WorkflowRequest};
    fn run(w: &Workspace, request: EvidenceRequest) -> Result<WorkflowOutcome> {
        let request = WorkflowRequest::Evidence { request };
        let record = w.begin(
            Uuid::new_v4(),
            "evidence fixture",
            crate::snapshot(w, &request)?,
        )?;
        crate::execute(w, &request, &record, &TaskContext::default())
    }
    #[test]
    fn status_recovery_and_drafts_share_the_frozen_workflow() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (_, version, _) = crate::demo::create_canonical(&w).unwrap();
        let original = fs::read(w.resolve(&version.path)).unwrap();
        let report = run(
            &w,
            EvidenceRequest::Status {
                actions: version.id.clone(),
                instrument: None,
            },
        )
        .unwrap();
        assert!(report.details["entries"].is_array());
        let collection = WorkflowRequest::CollectActions {
            request: analytics_storage::collector::CollectionRequest {
                provider: analytics_storage::collector::Provider::Hithink,
                adjustment_source: analytics_storage::adjustment::AdjustmentSource::HithinkForward,
                start: "2020-01-01".parse().unwrap(),
                end: "2024-01-01".parse().unwrap(),
                instruments: std::collections::BTreeSet::from(["SH600000".into()]),
                benchmarks: Default::default(),
                universes: Default::default(),
                source_bindings: Default::default(),
            },
            resume: None,
        };
        let mut record = w
            .begin(
                Uuid::new_v4(),
                "failed batch",
                crate::snapshot(&w, &collection).unwrap(),
            )
            .unwrap();
        let source = w.resolve(format!(".jfquant/staging/python-actions-{}", record.id));
        fs::create_dir_all(source.join("raw")).unwrap();
        fs::write(source.join("actions.json"), &original).unwrap();
        assert!(run(&w, EvidenceRequest::Recover { task: record.id }).is_err());
        w.finish(
            &mut record,
            RunStatus::Failed,
            Some("network failure".into()),
        )
        .unwrap();
        let retry = retry_request(&w, record.id).unwrap();
        assert!(
            matches!(retry,WorkflowRequest::CollectActions{resume:Some(owner),request} if owner==record.id && request.instruments.contains("SH600000") && request.start.to_string()=="2020-01-01")
        );
        let recovered = run(&w, EvidenceRequest::Recover { task: record.id }).unwrap();
        assert_eq!(recovered.details["recovery"]["source_status"], "failed");
        assert!(recovered.data.is_some());
        assert_eq!(fs::read(w.resolve(&version.path)).unwrap(), original);
        let owner: RunRecord =
            serde_json::from_slice(&fs::read(w.resolve(&record.output).join("run.json")).unwrap())
                .unwrap();
        assert_eq!(owner.status, RunStatus::Failed);
        let request = WorkflowRequest::Evidence {
            request: EvidenceRequest::Recover { task: record.id },
        };
        let frozen = w
            .begin(
                Uuid::new_v4(),
                "freeze",
                crate::snapshot(&w, &request).unwrap(),
            )
            .unwrap();
        fs::write(source.join("changed"), b"after submit").unwrap();
        assert!(
            crate::execute(&w, &request, &frozen, &TaskContext::default())
                .unwrap_err()
                .to_string()
                .contains("changed after request freeze")
        );
    }
    #[test]
    fn draft_is_a_preview_and_never_promotes_an_empty_review() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (_, version, _) = crate::demo::create_canonical(&w).unwrap();
        let root = w.resolve("evidence");
        fs::create_dir_all(root.join("raw")).unwrap();
        atomic_json(
            &root.join("announcements.json"),
            &json!({"schema_version":2,"documents":[],"receipts":{}}),
        )
        .unwrap();
        let before = w.datasets().unwrap().len();
        let report = run(
            &w,
            EvidenceRequest::Draft {
                actions: version.id,
                request: None,
                instrument: Some("SH600000".into()),
                evidence_manifest: Some(root.join("announcements.json")),
            },
        )
        .unwrap();
        assert_eq!(report.details["validation"]["ready"], false);
        assert!(report.data.is_none());
        assert_eq!(w.datasets().unwrap().len(), before);
        assert!(PathBuf::from(report.details["request_path"].as_str().unwrap()).is_file());
    }
    #[test]
    fn evidence_paths_outside_workspace_and_invalid_capture_are_rejected_before_work() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        fs::write(outside.path().join("announcements.json"), b"{}").unwrap();
        let request = EvidenceRequest::Draft {
            actions: "unused".into(),
            request: None,
            instrument: Some("SH600000".into()),
            evidence_manifest: Some(outside.path().join("announcements.json")),
        };
        assert!(freeze(&w, &request).is_err());
        assert!(
            freeze(
                &w,
                &EvidenceRequest::Capture {
                    request: evidence::CaptureRequest {
                        schema_version: 1,
                        documents: vec![]
                    }
                }
            )
            .is_err()
        );
    }
}
