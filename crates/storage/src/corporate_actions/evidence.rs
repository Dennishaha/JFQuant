//! Reusable selected-document capture, partial recovery and review-request drafting.
use super::*;
use analytics_core::task::TaskContext;
use serde_json::json;
use std::process::Command;

/// One explicitly selected public implementation document, not a history search.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentSpec {
    /// Stable document identity, unique in this capture.
    pub id: String,
    /// Canonical exchange symbol.
    pub instrument: String,
    /// Public HTTPS PDF URL; redirects are not followed.
    pub url: String,
    /// Published date from the original disclosure.
    pub announced: Date,
    /// Exact title excerpt, compared after removing whitespace.
    pub title: String,
    /// Explicit reviewer classification; default captures an unverified candidate.
    #[serde(default)]
    pub implementation: bool,
}
/// Versioned batch of selected documents; never asserts complete history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    /// Current request format, 1.
    pub schema_version: u32,
    /// Explicit independent document selection.
    pub documents: Vec<DocumentSpec>,
}
fn hash(bytes: &[u8]) -> String {
    format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(bytes))
    )
}
fn write(path: &Path, value: &impl Serialize) -> io::Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value).map_err(invalid)?)
}
impl CaptureRequest {
    /// Validate before starting network requests or writing any evidence.
    pub fn validate(&self) -> io::Result<()> {
        if self.schema_version != 1 || self.documents.is_empty() || self.documents.len() > 200 {
            return Err(invalid("invalid selected-document request"));
        }
        let mut seen = BTreeSet::new();
        for d in &self.documents {
            analytics_core::pool::validate_exchange_symbol(&d.instrument).map_err(invalid)?;
            let url = url::Url::parse(&d.url).map_err(invalid)?;
            if d.id.is_empty()
                || d.id.len() > 100
                || !d
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
                || !seen.insert(&d.id)
                || d.title.trim().is_empty()
                || url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                return Err(invalid(
                    "invalid/duplicate document identity, title or public HTTPS URL",
                ));
            }
        }
        Ok(())
    }
}
const EXTRACT: &str = r#"
import json,sys
from importlib.metadata import version
from pathlib import Path
from jfquant_collectors.announcements import extract_pdf_text
t=extract_pdf_text(Path(sys.argv[1]).read_bytes())
print(json.dumps({'text':t,'pypdf_version':version('pypdf')},ensure_ascii=False))
"#;
/// Capture public PDFs with bounded transport and bundled text extraction.
/// Failed responses remain auditable; only verified PDFs enter the manifest.
pub fn capture(
    request: &CaptureRequest,
    output: &Path,
    context: &TaskContext,
) -> io::Result<Value> {
    capture_with(request, output, context, download, extract)
}
fn download(spec: &DocumentSpec, path: &Path) -> io::Result<(u16, Option<i32>)> {
    let response = crate::transport::curl(
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
            .args(["--write-out", "%{http_code}", &spec.url]),
    )?;
    Ok((
        String::from_utf8_lossy(&response.stdout)
            .parse()
            .unwrap_or(0),
        response.status.code(),
    ))
}
fn extract(path: &Path) -> io::Result<Value> {
    let parsed = crate::transport::curl(
        Command::new(crate::collector::runtime()?)
            .args(["-I", "-B", "-c", EXTRACT])
            .arg(path),
    )?;
    if !parsed.status.success() {
        return Err(invalid("document_pdf_extraction_failed"));
    }
    serde_json::from_slice(&parsed.stdout).map_err(invalid)
}
fn capture_with(
    request: &CaptureRequest,
    output: &Path,
    context: &TaskContext,
    download: impl Fn(&DocumentSpec, &Path) -> io::Result<(u16, Option<i32>)>,
    extract: impl Fn(&Path) -> io::Result<Value>,
) -> io::Result<Value> {
    request.validate()?;
    context.check()?;
    fs::create_dir(output)?;
    fs::create_dir(output.join("raw"))?;
    write(&output.join("capture-request.json"), request)?;
    let mut grouped = BTreeMap::<String, (Vec<Value>, Vec<Value>)>::new();
    let mut failures = BTreeMap::new();
    for spec in &request.documents {
        context.check()?;
        context.report(format!("捕获公告 {} / {}", spec.instrument, spec.id))?;
        let stem = format!("{}-{}", spec.instrument, spec.id);
        let body = format!("http-{stem}.body");
        let mut receipts = vec![];
        let result = (|| -> io::Result<Value> {
            let path = output.join("raw").join(&body);
            let (status, exit_code) = context.execute(|| download(spec, &path))?;
            let bytes = fs::read(output.join("raw").join(&body)).unwrap_or_default();
            if path.is_file() {
                receipts.push(json!({"url":spec.url,"status":status,"filename":body,"sha256":hash(&bytes),"kind":"http_response_body_decoded","retrieved_at":chrono::Utc::now().to_rfc3339()}));
            }
            if exit_code != Some(0)
                || status != 200
                || !bytes.starts_with(b"%PDF-")
                || bytes.len() > 64 * 1024 * 1024
            {
                return Err(invalid(format!(
                    "document_transport_or_pdf_invalid:status={status};curl={:?}",
                    exit_code
                )));
            }
            let pdf = format!("{stem}.pdf");
            fs::write(output.join("raw").join(&pdf), &bytes)?;
            let parsed = context.execute(|| extract(&output.join("raw").join(&pdf)))?;
            let text = parsed["text"]
                .as_str()
                .ok_or_else(|| invalid("document_text_missing"))?;
            let compact = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
            if !text.contains(&spec.instrument[2..])
                || !compact(text).contains(&compact(&spec.title))
            {
                return Err(invalid("document_security_or_title_mismatch"));
            }
            let txt = format!("{stem}.txt");
            fs::write(output.join("raw").join(&txt), text)?;
            Ok(
                json!({"id":spec.id,"instrument":spec.instrument,"announced":spec.announced,"title":spec.title,"url":spec.url,"pdf":pdf,"pdf_sha256":hash(&bytes),"text":txt,"text_sha256":hash(text.as_bytes()),"parser_version":"jfquant-selected-document-v1","pypdf_version":parsed["pypdf_version"],"candidates":{"stage":if spec.implementation {"implementation"} else {"unverified"},"requires_review":true}}),
            )
        })();
        context.check()?;
        let group = grouped.entry(spec.instrument.clone()).or_default();
        group.1.extend(receipts);
        match result {
            Ok(doc) => group.0.push(doc),
            Err(e) => {
                failures.insert(spec.id.clone(), e.to_string());
            }
        }
        // Each completed iteration is inspectable even if cancellation ends the task later.
        publish_manifest(output, &grouped, &failures)?;
    }
    Ok(
        json!({"manifest":output.join("announcements.json"),"captured_documents":grouped.values().map(|(docs,_)|docs.len()).sum::<usize>(),"ready_for_review":grouped.values().any(|(docs,_)|!docs.is_empty()),"failures":failures,"selected_documents_only":true,"full_history":false}),
    )
}
fn publish_manifest(
    output: &Path,
    grouped: &BTreeMap<String, (Vec<Value>, Vec<Value>)>,
    failures: &BTreeMap<String, String>,
) -> io::Result<()> {
    let mut documents = vec![];
    let mut receipts = BTreeMap::new();
    for (id, (docs, http)) in grouped {
        let shard = json!({"instrument":id,"history_verified":false,"evidence":{"kind":"selected_public_documents","documents":docs,"http_receipts":http,"pagination_complete":false,"event_category_completeness":"not_reviewed"}});
        let bytes = serde_json::to_vec_pretty(&shard).map_err(invalid)?;
        fs::write(
            output
                .join("raw")
                .join(format!("{id}-actions-structured.json")),
            &bytes,
        )?;
        receipts.insert(id, hash(&bytes));
        documents.extend(docs.clone());
    }
    let manifest = json!({"schema_version":2,"documents":documents,"receipts":receipts,"complete":{"status":"selected_documents_only","native_collector":false,"full_history":false},"failures":failures});
    write(&output.join("announcements.json"), &manifest)?;
    super::review::verified_documents(&output.join("announcements.json"))?;
    Ok(())
}
/// Copy an evidence tree without links, checking cancellation on every entry.
pub fn copy_tree(source: &Path, output: &Path) -> io::Result<()> {
    fs::create_dir(output)?;
    for entry in fs::read_dir(source)? {
        TaskContext::current().check()?;
        let e = entry?;
        // SDK credentials are neither evidence nor resumable checkpoints.
        if e.file_name() == ".tdx-runtime" {
            continue;
        }
        let ty = e.file_type()?;
        if ty.is_dir() {
            copy_tree(&e.path(), &output.join(e.file_name()))?;
        } else if ty.is_file() {
            fs::copy(e.path(), output.join(e.file_name()))?;
        } else {
            return Err(invalid("evidence tree contains a link"));
        }
    }
    Ok(())
}
/// Recover only checksum-verified completed announcement shards from a partial batch.
/// The manifest retains the incomplete batch identity and never invents coverage.
pub fn recover_announcements(source: &Path, output: &Path) -> io::Result<Value> {
    let receipts: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(source.join("receipts.json"))?).map_err(invalid)?;
    if receipts.is_empty() {
        return Err(invalid("no completed announcement shards"));
    }
    let mut documents = vec![];
    for (id, expected) in &receipts {
        analytics_core::pool::validate_instrument(id).map_err(invalid)?;
        let bytes = fs::read(
            source
                .join("raw")
                .join(format!("{id}-actions-structured.json")),
        )?;
        if hash(&bytes) != *expected {
            return Err(invalid("partial shard hash mismatch"));
        }
        let shard: Value = serde_json::from_slice(&bytes).map_err(invalid)?;
        if shard["instrument"] != *id {
            return Err(invalid("partial shard identity mismatch"));
        }
        documents.extend(
            shard["evidence"]["documents"]
                .as_array()
                .ok_or_else(|| invalid("partial documents missing"))?
                .clone(),
        );
    }
    let manifest = json!({"schema_version":2,"receipts":receipts,"documents":documents,"complete":{"status":"recovered_partial_batch","native_collector":false,"full_history":false}});
    // Validate bytes and HTTP proofs before copying anything into the recovered view.
    super::review::verified_document_records(source, &manifest)?;
    copy_tree(source, output)?;
    write(&output.join("announcements.json"), &manifest)?;
    super::review::verified_documents(&output.join("announcements.json"))?;
    Ok(manifest)
}
/// Rebind reviewed event proposals to the selected immutable version. No evidence,
/// issue resolution or failure resolution is invented; callers must validate the draft.
pub fn draft(
    book: &ActionBundle,
    request: &review::RevisionRequest,
) -> io::Result<(review::RevisionRequest, Vec<String>)> {
    let mut draft = request.clone();
    let mut unchanged = vec![];
    let mut events = vec![];
    for mut revision in std::mem::take(&mut draft.events) {
        let original = revision
            .original_id
            .as_ref()
            .and_then(|id| book.actions.iter().find(|a| a.id == *id));
        let replacement_event = book
            .actions
            .iter()
            .find(|a| a.id == revision.replacement.id);
        if original
            .zip(replacement_event)
            .is_some_and(|(a, b)| a.id != b.id)
        {
            return Err(invalid("draft event identity collision"));
        }
        let previous = original.or(replacement_event);
        let replacement = serde_json::to_vec(&revision.replacement).map_err(invalid)?;
        let original = previous
            .map(serde_json::to_vec)
            .transpose()
            .map_err(invalid)?;
        if original.as_ref() == Some(&replacement) {
            unchanged.push(revision.replacement.id.clone());
            continue;
        }
        revision.original_id = previous.map(|a| a.id.clone());
        revision.original_sha256 = original.map(|bytes| hash(&bytes));
        events.push(revision);
    }
    draft.events = events;
    Ok((draft, unchanged))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec(id: &str) -> DocumentSpec {
        DocumentSpec {
            id: id.into(),
            instrument: "SH600000".into(),
            url: format!("https://example.org/{id}.pdf"),
            announced: "2024-01-01".parse().unwrap(),
            title: "Implementation notice".into(),
            implementation: true,
        }
    }
    #[test]
    fn selected_capture_preserves_failed_http_and_cannot_assert_full_history() {
        let t = tempfile::tempdir().unwrap();
        let output = t.path().join("capture");
        let request = CaptureRequest {
            schema_version: 1,
            documents: vec![spec("good"), spec("html"), spec("foreign")],
        };
        let report=capture_with(&request,&output,&TaskContext::default(),|spec,path| {
            fs::write(path,if spec.id=="html" {b"<html>challenge</html>".as_slice()}else{b"%PDF-fixture".as_slice()})?;Ok((200,Some(0)))
        },|path|Ok(json!({"text":if path.to_string_lossy().contains("foreign") {"000001 Implementation notice"} else {"600000 Implementation\n notice"},"pypdf_version":"fixture"}))).unwrap();
        assert_eq!(report["captured_documents"], 1);
        assert_eq!(report["full_history"], false);
        assert_eq!(report["failures"].as_object().unwrap().len(), 2);
        let docs = review::verified_documents(&output.join("announcements.json")).unwrap();
        assert_eq!(docs.len(), 1);
        let shard: Value = serde_json::from_slice(
            &fs::read(output.join("raw/SH600000-actions-structured.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(shard["history_verified"], false);
        assert_eq!(shard["evidence"]["pagination_complete"], false);
        assert_eq!(
            shard["evidence"]["http_receipts"].as_array().unwrap().len(),
            3
        );
        fs::write(output.join("raw/http-SH600000-html.body"), b"changed").unwrap();
        assert!(review::verified_documents(&output.join("announcements.json")).is_err());
    }
    #[test]
    fn malformed_requests_and_cancellation_do_not_start_network() {
        let t = tempfile::tempdir().unwrap();
        let mut req = CaptureRequest {
            schema_version: 1,
            documents: vec![spec("a")],
        };
        for url in [
            "http://example.org/a.pdf",
            "https://user:secret@example.org/a.pdf",
            "file:///tmp/a",
        ] {
            req.documents[0].url = url.into();
            assert!(req.validate().is_err());
        }
        req.documents = vec![spec("a"), spec("a")];
        assert!(req.validate().is_err());
        req.documents = vec![spec("a")];
        let ctx = TaskContext::default();
        ctx.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            capture_with(
                &req,
                &t.path().join("cancelled"),
                &ctx,
                |_, _| panic!("must not fetch"),
                |_| panic!("must not extract")
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
    }
    #[test]
    fn completed_partial_shards_recover_without_claiming_an_exhaustive_search() {
        let t = tempfile::tempdir().unwrap();
        let source = t.path().join("source");
        let req = CaptureRequest {
            schema_version: 1,
            documents: vec![spec("a")],
        };
        capture_with(
            &req,
            &source,
            &TaskContext::default(),
            |_, p| {
                fs::write(p, b"%PDF-fixture")?;
                Ok((200, Some(0)))
            },
            |_| Ok(json!({"text":"600000 Implementation notice"})),
        )
        .unwrap();
        let manifest: Value =
            serde_json::from_slice(&fs::read(source.join("announcements.json")).unwrap()).unwrap();
        write(&source.join("receipts.json"), &manifest["receipts"]).unwrap();
        fs::remove_file(source.join("announcements.json")).unwrap();
        let result = recover_announcements(&source, &t.path().join("recovered")).unwrap();
        assert_eq!(result["complete"]["full_history"], false);
        assert_eq!(
            review::verified_documents(&t.path().join("recovered/announcements.json"))
                .unwrap()
                .len(),
            1
        );
        fs::write(
            source.join("raw/SH600000-actions-structured.json"),
            b"tampered",
        )
        .unwrap();
        assert!(recover_announcements(&source, &t.path().join("bad")).is_err());
        assert!(!t.path().join("bad").exists());
    }
    #[test]
    fn drafting_rebinds_exact_event_hashes_without_inventing_failure_resolutions() {
        let mut book:ActionBundle=serde_json::from_value(json!({"schema_version":8,"source_bindings":{},"structured_coverage":{},"coverage":{},"downloader":"JFQuant analytics-storage corporate-actions","downloaded_at":"fixture","queried_symbols":[],"actions":[],"failures":{"SH600000":"unknown"},"raw_sha256":{}})).unwrap();
        let mut event:CorporateAction=serde_json::from_value(json!({"id":"SH600000:2024-01-02","instrument":"SH600000","announced":null,"record_date":null,"ex_date":"2024-01-02","pay_date":null,"listing_date":null,"cash_per_share":"0.2","shares_per_share":"0","source":"fixture"})).unwrap();
        book.actions.push(event.clone());
        event.cash_per_share = "0.3".into();
        let req = review::RevisionRequest {
            schema_version: 3,
            evidence_manifest: "announcements.json".into(),
            structured_manifest: None,
            events: vec![review::EventRevision {
                original_id: None,
                original_sha256: None,
                replacement: event,
                reason: "reviewed".into(),
                fields: Default::default(),
                structured_fields: Default::default(),
            }],
            resolve_failures: vec![],
            resolve_issues: vec![],
            coverage_reviews: vec![],
        };
        let (rebased, _) = draft(&book, &req).unwrap();
        assert_eq!(
            rebased.events[0].original_sha256.as_deref(),
            Some(hash(&serde_json::to_vec(&book.actions[0]).unwrap()).as_str())
        );
        assert!(rebased.resolve_failures.is_empty());
        assert_eq!(book.failures.len(), 1);
        let mut moved = rebased;
        moved.events[0].replacement.id = "SH600000:2024-01-03".into();
        moved.events[0].replacement.ex_date = "2024-01-03".parse().unwrap();
        let (moved, _) = draft(&book, &moved).unwrap();
        assert_eq!(
            moved.events[0].original_id.as_deref(),
            Some("SH600000:2024-01-02")
        );
        let mut same = req;
        same.events[0].replacement = book.actions[0].clone();
        let (same, unchanged) = draft(&book, &same).unwrap();
        assert!(same.events.is_empty());
        assert_eq!(unchanged.len(), 1);
    }
}

#[cfg(test)]
mod copy_tests {
    use super::*;
    #[test]
    fn copying_excludes_private_sdk_state_and_rejects_evidence_links() {
        let t = tempfile::tempdir().unwrap();
        let source = t.path().join("source");
        fs::create_dir_all(source.join(".tdx-runtime")).unwrap();
        fs::write(source.join(".tdx-runtime/user.ini"), b"private fixture").unwrap();
        fs::write(source.join("receipt.json"), b"{}").unwrap();
        let output = t.path().join("copy");
        copy_tree(&source, &output).unwrap();
        assert!(output.join("receipt.json").is_file());
        assert!(!output.join(".tdx-runtime").exists());
        std::os::unix::fs::symlink(source.join("receipt.json"), source.join("linked.json"))
            .unwrap();
        assert!(copy_tree(&source, &t.path().join("linked")).is_err());
    }
}
