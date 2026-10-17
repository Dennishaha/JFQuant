//! Worker-owned material registration, extraction and explicit human review.
use crate::{Result, RunRecord, Workspace};
use analytics_core::task::TaskContext;
use analytics_storage::{
    membership_history::{self as storage, Request},
    membership_history_material as material,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, path::PathBuf};
/// Local material operations; none performs implicit networking or publishes history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum MaterialAction {
    /// Verify all material states, optionally returning a selected image preview.
    Inspect {
        /// Raster document ID selected for display.
        document: Option<String>,
    },
    /// Register one existing file, computing its hash rather than trusting pasted bytes.
    Register {
        /// New unique ID.
        id: String,
        /// Workspace-relative original.
        path: PathBuf,
        /// Original HTTPS source.
        url: String,
        /// Explicit pdf/text/png/jpeg.
        format: String,
    },
    /// Extract one exact embedded PDF raster, preserving parent/page/image identity.
    Extract {
        /// Existing PDF document ID.
        document: String,
        /// One-based page.
        page: u32,
        /// Zero-based image index.
        image: u32,
        /// png/jpeg matching the embedded bytes.
        format: String,
    },
    /// Confirm a transcription explicitly; changed content/region invalidates its receipt.
    Review {
        /// Existing transcription draft ID.
        transcription: String,
        /// Human reviewer identity.
        reviewer: String,
    },
}
/// Freeze original files and the exact draft/action; material work stays on its worker.
pub(crate) fn freeze(w: &Workspace, request: &Request, action: &MaterialAction) -> Result<Value> {
    if request.schema_version != 2 || request.history.schema_version != 2 {
        return Err("unsupported history draft; use explicit migration".into());
    }
    let mut hashes = vec![];
    for d in &request.documents {
        hashes.push(json!([
            d.path,
            storage::bytes_hash(&fs::read(storage::document_path(&w.root, &d.path)?)?)
        ]));
    }
    if let MaterialAction::Register { path, .. } = action {
        hashes.push(json!([
            path,
            storage::bytes_hash(&fs::read(storage::document_path(&w.root, path)?)?)
        ]));
    }
    Ok(json!({"request":request,"action":action,"hashes":hashes}))
}
/// Execute an explicit local action. Returns an updated draft, never mutates the source draft.
pub(crate) fn execute(
    w: &Workspace,
    request: &Request,
    action: &MaterialAction,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<Value> {
    let mut request = request.clone();
    let mut preview = Value::Null;
    match action {
        MaterialAction::Inspect { document } => {
            if let Some(id) = document {
                let d = request
                    .documents
                    .iter()
                    .find(|d| &d.id == id)
                    .ok_or("image document missing")?;
                let bytes = fs::read(storage::document_path(&w.root, &d.path)?)?;
                if storage::bytes_hash(&bytes) != d.sha256 {
                    return Err("image original changed".into());
                }
                let image = material::raster(&bytes, &d.format)?;
                let small = image.thumbnail(900, 900).to_rgba8();
                preview = json!({"document":id,"sha256":d.sha256,"width":image.width(),"height":image.height(),"preview_width":small.width(),"preview_height":small.height(),"rgba":small.into_raw()});
            }
        }
        MaterialAction::Register {
            id,
            path,
            url,
            format,
        } => {
            storage::validate_source(url)?;
            if id.trim().is_empty()
                || request.documents.iter().any(|d| &d.id == id)
                || !matches!(format.as_str(), "pdf" | "text" | "png" | "jpeg")
            {
                return Err("invalid material registration".into());
            }
            let bytes = fs::read(storage::document_path(&w.root, path)?)?;
            request.documents.push(storage::Document {
                id: id.clone(),
                path: path.clone(),
                sha256: storage::bytes_hash(&bytes),
                source_url: url.clone(),
                format: format.clone(),
                coverage_source: false,
                derived_from: None,
            });
        }
        MaterialAction::Extract {
            document,
            page,
            image,
            format,
        } => {
            if !matches!(format.as_str(), "png" | "jpeg") {
                return Err("embedded format must be png/jpeg".into());
            }
            let d = request
                .documents
                .iter()
                .find(|d| &d.id == document)
                .ok_or("PDF document missing")?;
            if d.format != "pdf" || d.derived_from.is_some() {
                return Err("expected original PDF".into());
            }
            let p = storage::document_path(&w.root, &d.path)?;
            if storage::bytes_hash(&fs::read(&p)?) != d.sha256 {
                return Err("PDF original changed".into());
            }
            let bytes = material::embedded_bytes(&p, *page, *image, context)?;
            material::raster(&bytes, format)?;
            let id = format!("{document}-page-{page}-image-{image}");
            if request.documents.iter().any(|d| d.id == id) {
                return Err("derived image already registered".into());
            }
            let path = run.output.join(format!("embedded.{format}"));
            fs::write(w.resolve(&path), &bytes)?;
            request.documents.push(storage::Document {
                id,
                path,
                sha256: storage::bytes_hash(&bytes),
                source_url: d.source_url.clone(),
                format: format.clone(),
                coverage_source: false,
                derived_from: Some(storage::DerivedFrom {
                    document: document.clone(),
                    page: *page,
                    image: *image,
                }),
            });
        }
        MaterialAction::Review {
            transcription,
            reviewer,
        } => {
            if request
                .transcriptions
                .iter()
                .filter(|t| &t.id == transcription)
                .count()
                != 1
            {
                return Err("review requires one unique transcription".into());
            }
            let t = request
                .transcriptions
                .iter_mut()
                .find(|t| &t.id == transcription)
                .unwrap();
            t.reviewer = reviewer.clone();
            t.reviewed_at = Some(chrono::Utc::now());
            t.confirmed = true;
            let d = request
                .documents
                .iter()
                .find(|d| d.id == t.document)
                .ok_or("image document missing")?;
            material::verify_origin(&w.root, d, &request.documents, context)?;
            t.receipt = Some(t.binding(&request.documents)?);
            material::verify_transcription(&w.root, t, &request.documents)?;
        }
    }
    context.check()?;
    let states: Vec<_> = request
        .documents
        .iter()
        .map(|d| material::inspect(&w.root, d))
        .collect();
    let extracted:Vec<_>=request.documents.iter().filter(|d|matches!(d.format.as_str(),"pdf"|"text")).filter_map(|d|storage::extracted_text(&w.root,d).ok().map(|text|json!({"document":d.id,"original_sha256":d.sha256,"text_sha256":storage::bytes_hash(text.as_bytes()),"text":text,"extractor":"packaged-pypdf-or-original-text-v2"}))).collect();
    let output = run.output.join("history-request-v2.json");
    crate::atomic_json(&w.resolve(&output), &request)?;
    Ok(
        json!({"schema_version":2,"output":output,"history_request":request,"material_status":states,"image_preview":preview,"extracted_texts":extracted,"membership_verified":false,"complete_coverage_verified":false}),
    )
}
