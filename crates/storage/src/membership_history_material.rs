//! Original and derived history materials. Human transcriptions never replace source bytes.
use crate::membership_history::{Document, bytes_hash, document_path, invalid};
use analytics_core::task::TaskContext;
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path, process::Command};

/// Location of an embedded raster, bound to the original PDF document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DerivedFrom {
    /// Original PDF document ID.
    pub document: String,
    /// One-based PDF page.
    pub page: u32,
    /// Zero-based pypdf page.images position.
    pub image: u32,
}
/// Attributed human reading of an exact raster region, never automatic factual approval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Transcription {
    /// Stable reference ID.
    pub id: String,
    /// Raster document ID.
    pub document: String,
    /// Bound original raster bytes.
    pub sha256: String,
    /// Pixel rectangle x, y, width, height, in original dimensions.
    pub region: [u32; 4],
    /// Human transcription or interpretation.
    pub text: String,
    /// Named human reviewer.
    pub reviewer: String,
    /// Review time; never used as an announcement time.
    pub reviewed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Explicit confirmation of the region's meaning.
    pub confirmed: bool,
    /// Binding receipt; absent until the explicit review operation.
    pub receipt: Option<String>,
}
impl Transcription {
    /// Bind the reading and exact source location; portable paths are deliberately excluded.
    pub fn binding(&self, documents: &[Document]) -> io::Result<String> {
        let mut copy = self.clone();
        copy.receipt = None;
        let d = unique_document(documents, &self.document)?;
        let parent = d
            .derived_from
            .as_ref()
            .map(|o| unique_document(documents, &o.document))
            .transpose()?;
        Ok(bytes_hash(&serde_json::to_vec(&serde_json::json!([
            copy,
            d.source_url,
            d.format,
            d.derived_from,
            parent.map(|p| (&p.id, &p.sha256, &p.source_url))
        ]))?))
    }
}
fn unique_document<'a>(documents: &'a [Document], id: &str) -> io::Result<&'a Document> {
    let mut matches = documents.iter().filter(|d| d.id == id);
    let d = matches
        .next()
        .ok_or_else(|| invalid("image_document_missing"))?;
    if matches.next().is_some() {
        return Err(invalid("image_document_ambiguous"));
    }
    Ok(d)
}
/// Reproduce an embedded raster from its original PDF using the same extraction chain.
pub fn verify_origin(
    root: &Path,
    d: &Document,
    documents: &[Document],
    context: &TaskContext,
) -> io::Result<()> {
    if let Some(origin) = &d.derived_from {
        let parent = unique_document(documents, &origin.document)?;
        if parent.format != "pdf" || parent.derived_from.is_some() {
            return Err(invalid("invalid image derivation chain"));
        }
        let path = document_path(root, &parent.path)?;
        if bytes_hash(&fs::read(&path)?) != parent.sha256 {
            return Err(invalid("parent PDF hash mismatch"));
        }
        let image = embedded_bytes(&path, origin.page, origin.image, context)?;
        if bytes_hash(&image) != d.sha256 {
            return Err(invalid("embedded image bytes changed"));
        }
    }
    Ok(())
}
/// Independent transport, extraction and semantic states.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaterialStatus {
    /// Selected document.
    pub document: String,
    /// Original byte integrity, not HTTP alone.
    pub integrity: String,
    /// Body/text/image extraction state.
    pub extraction: String,
    /// Meaning requires a cited original or attributed review.
    pub facts: String,
    /// Specific failure, when present.
    pub reason: Option<String>,
    /// Text hash; extracted text is a derivative.
    pub text_sha256: Option<String>,
}
/// Decode only supported raster formats with bounded allocation.
pub fn raster(bytes: &[u8], format: &str) -> io::Result<image::DynamicImage> {
    let format = match format {
        "png" => image::ImageFormat::Png,
        "jpeg" => image::ImageFormat::Jpeg,
        _ => return Err(invalid("unsupported raster format")),
    };
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    reader.limits(limits);
    reader.decode().map_err(invalid)
}
/// Reject successful HTTP protection/login documents as original body evidence.
pub fn body(text: &str) -> io::Result<()> {
    let lower = text.to_lowercase();
    if text.trim().is_empty() {
        return Err(invalid("empty_body"));
    }
    if [
        "captcha",
        "access denied",
        "checking your browser",
        "aliyun_waf",
        "aliyunwaf",
        "waf_captcha",
        "安全验证",
        "人机验证",
        "__jsl_clearance",
        "challenge-platform",
        "访问过于频繁",
    ]
    .iter()
    .any(|s| lower.contains(s))
    {
        return Err(invalid("protection_page"));
    }
    if lower.contains("type=\"password\"")
        || lower.contains("type='password'")
        || lower.contains("login required")
    {
        return Err(invalid("login_page"));
    }
    // An image-only page is not a textual fact; register the actual raster separately.
    let visible = strip_tags(text);
    if visible.chars().filter(|c| !c.is_whitespace()).count() < 8 {
        return Err(invalid("empty_extracted_body"));
    }
    Ok(())
}
fn strip_tags(text: &str) -> String {
    let mut body = text.to_owned();
    // Scripts, styles and metadata are not visible body evidence. Do not execute them.
    for tag in ["head", "script", "style", "template"] {
        let opening = format!("<{tag}");
        let closing = format!("</{tag}>");
        loop {
            let lower = body.to_ascii_lowercase();
            let Some(start) = lower.match_indices(&opening).find_map(|(i, _)| {
                lower
                    .as_bytes()
                    .get(i + opening.len())
                    .filter(|c| **c == b'>' || c.is_ascii_whitespace())
                    .map(|_| i)
            }) else {
                break;
            };
            let end = lower[start..]
                .find(&closing)
                .map_or(body.len(), |offset| start + offset + closing.len());
            body.replace_range(start..end, "");
        }
    }
    let mut inside = false;
    body.chars()
        .filter(|c| {
            if *c == '<' {
                inside = true;
                false
            } else if *c == '>' {
                inside = false;
                false
            } else {
                !inside
            }
        })
        .collect()
}

/// Inspect without promoting transport success into factual verification.
pub fn inspect(root: &Path, d: &Document) -> MaterialStatus {
    let mut s = MaterialStatus {
        document: d.id.clone(),
        integrity: "unverified".into(),
        extraction: "not_attempted".into(),
        facts: "unverified".into(),
        reason: None,
        text_sha256: None,
    };
    let result = (|| -> io::Result<()> {
        crate::membership_history::validate_source(&d.source_url)?;
        let bytes = fs::read(document_path(root, &d.path)?)?;
        if bytes_hash(&bytes) != d.sha256 {
            return Err(invalid("original_hash_mismatch"));
        }
        s.integrity = "verified".into();
        if matches!(d.format.as_str(), "png" | "jpeg") {
            raster(&bytes, &d.format)?;
            s.extraction = "image_requires_human_review".into();
            return Ok(());
        }
        let t = super::membership_history::text(root, d)?;
        body(&t)?;
        s.extraction = "text_extracted".into();
        s.text_sha256 = Some(bytes_hash(t.as_bytes()));
        Ok(())
    })();
    if let Err(e) = result {
        s.reason = Some(e.to_string());
        if s.integrity == "verified" {
            s.extraction = "failed".into();
        }
    }
    s
}
/// Verify an explicit reading against current source bytes and pixel coordinates.
pub fn verify_transcription(
    root: &Path,
    t: &Transcription,
    documents: &[Document],
) -> io::Result<()> {
    if t.id.trim().is_empty()
        || t.reviewer.trim().is_empty()
        || t.text.trim().is_empty()
        || t.reviewed_at.is_none()
        || !t.confirmed
        || t.receipt.as_ref() != Some(&t.binding(documents)?)
    {
        return Err(invalid("image_review_binding_unverified"));
    }
    let d = documents
        .iter()
        .find(|d| d.id == t.document)
        .ok_or_else(|| invalid("image_document_missing"))?;
    if d.sha256 != t.sha256 {
        return Err(invalid("image_review_original_changed"));
    }
    let b = fs::read(document_path(root, &d.path)?)?;
    if bytes_hash(&b) != t.sha256 {
        return Err(invalid("image_review_original_changed"));
    }
    let image = raster(&b, &d.format)?;
    let [x, y, w, h] = t.region;
    if w == 0
        || h == 0
        || x.checked_add(w).is_none_or(|v| v > image.width())
        || y.checked_add(h).is_none_or(|v| v > image.height())
    {
        return Err(invalid("image_review_region_invalid"));
    }
    Ok(())
}
/// Existing packaged pypdf path; no full-page renderer or inferred OCR is used.
pub fn embedded_bytes(
    path: &Path,
    page: u32,
    index: u32,
    context: &TaskContext,
) -> io::Result<Vec<u8>> {
    if page == 0 {
        return Err(invalid("PDF pages are one based"));
    }
    context.check()?;
    let script = r#"import sys
from pypdf import PdfReader
p=PdfReader(sys.argv[1],strict=True).pages[int(sys.argv[2])-1]
sys.stdout.buffer.write(p.images[int(sys.argv[3])].data)
"#;
    let out = context.execute(|| {
        crate::transport::curl(
            Command::new(crate::collector::runtime()?)
                .args(["-I", "-B", "-c", script])
                .arg(path)
                .arg(page.to_string())
                .arg(index.to_string()),
        )
    })?;
    if !out.status.success() {
        return Err(invalid(format!(
            "embedded_image_extraction_failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    context.check()?;
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image_fixture(root: &Path) -> Document {
        let image = image::RgbaImage::from_pixel(8, 6, image::Rgba([255, 255, 255, 255]));
        image.save(root.join("original.png")).unwrap();
        Document {
            id: "image".into(),
            path: "original.png".into(),
            sha256: bytes_hash(&fs::read(root.join("original.png")).unwrap()),
            source_url: "https://example.com/original.png".into(),
            format: "png".into(),
            coverage_source: false,
            derived_from: None,
        }
    }
    #[test]
    fn image_binding_rejects_changed_region_text_reviewer_hash_and_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let d = image_fixture(dir.path());
        let mut t = Transcription {
            id: "reading".into(),
            document: d.id.clone(),
            sha256: d.sha256.clone(),
            region: [0, 0, 8, 6],
            text: "Names in original".into(),
            reviewer: "reviewer".into(),
            reviewed_at: Some(chrono::Utc::now()),
            confirmed: true,
            receipt: None,
        };
        t.receipt = Some(t.binding(std::slice::from_ref(&d)).unwrap());
        assert!(verify_transcription(dir.path(), &t, std::slice::from_ref(&d)).is_ok());
        for field in 0..6 {
            let mut changed = t.clone();
            match field {
                0 => changed.region[0] = 1,
                1 => changed.text.push('!'),
                2 => changed.reviewer = "different".into(),
                3 => changed.sha256 = "wrong".into(),
                4 => changed.confirmed = false,
                _ => changed.reviewed_at = None,
            };
            assert!(verify_transcription(dir.path(), &changed, std::slice::from_ref(&d)).is_err());
        }
        t.region = [7, 0, 8, 6];
        t.receipt = Some(t.binding(std::slice::from_ref(&d)).unwrap());
        assert!(verify_transcription(dir.path(), &t, std::slice::from_ref(&d)).is_err());
        fs::write(dir.path().join("original.png"), "tampered").unwrap();
        assert_eq!(inspect(dir.path(), &d).integrity, "unverified");
    }
    #[test]
    fn http_success_is_not_body_or_fact_success() {
        for s in [
            "",
            "<html><body></body></html>",
            "<meta name=\"aliyun_waf_aa\">",
            "<input type=\"password\">",
            "checking your browser",
        ] {
            assert!(body(s).is_err(), "{s}");
        }
        assert!(body("MSCI index review effective at the close on 2025-08-26").is_ok());
        assert!(body("<html><head><title>long metadata title without original body</title></head><body><script>const long_value='not article content';</script><style>body {font-size: 12px}</style></body></html>").is_err());
        assert!(body("<html><head><title>Title</title></head><body><header>Actual readable article body</header></body></html>").is_ok());
        assert!(raster(b"not an image", "png").is_err());
    }
    #[test]
    fn image_receipt_binds_parent_hash_page_image_and_source_even_when_raster_is_identical() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = image_fixture(dir.path());
        let parent = Document {
            id: "pdf".into(),
            path: "source.pdf".into(),
            sha256: "sha256:parent".into(),
            source_url: "https://example.com/source.pdf".into(),
            format: "pdf".into(),
            coverage_source: false,
            derived_from: None,
        };
        d.derived_from = Some(DerivedFrom {
            document: "pdf".into(),
            page: 1,
            image: 0,
        });
        let t = Transcription {
            id: "reading".into(),
            document: d.id.clone(),
            sha256: d.sha256.clone(),
            region: [0, 0, 8, 6],
            text: "Original region".into(),
            reviewer: "reviewer".into(),
            reviewed_at: Some(chrono::Utc::now()),
            confirmed: true,
            receipt: None,
        };
        let documents = vec![d, parent];
        let receipt = t.binding(&documents).unwrap();
        for field in 0..5 {
            let mut changed = documents.clone();
            match field {
                0 => changed[0].derived_from.as_mut().unwrap().page = 2,
                1 => changed[0].derived_from.as_mut().unwrap().image = 1,
                2 => changed[1].sha256 = "sha256:other-parent".into(),
                3 => changed[0].source_url = "https://example.com/another-source.png".into(),
                _ => changed[1].source_url = "https://example.com/another-parent.pdf".into(),
            };
            assert_ne!(receipt, t.binding(&changed).unwrap());
        }
        let mut relocated = documents.clone();
        relocated[0].path = "frozen/image.png".into();
        relocated[1].path = "frozen/source.pdf".into();
        assert_eq!(receipt, t.binding(&relocated).unwrap());
    }
}
