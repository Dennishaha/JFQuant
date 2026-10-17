//! Explicit review of cash-only differential distributions; one authenticated document suffices.
use super::*;
use crate::execution_data::ExecutionIssue;
use analytics_core::actions::{Allocation, EventTerms, Reference};

pub(super) fn verify(
    issue: &ExecutionIssue,
    request: &RevisionRequest,
    documents: &BTreeMap<String, (Value, String)>,
) -> io::Result<()> {
    let resolution = request
        .resolve_issues
        .iter()
        .find(|r| r.id == issue.id)
        .ok_or_else(|| invalid("distribution_resolution_missing"))?;
    if request.schema_version < 5
        || issue.event_category != "distribution"
        || issue.dates.start != issue.dates.end
        || resolution.reason.trim().is_empty()
        || resolution.expected_sha256 != digest(issue)?
    {
        return Err(invalid("distribution_resolution_identity_or_hash_changed"));
    }
    let revision = request
        .events
        .iter()
        .find(|r| {
            r.replacement.instrument == issue.instrument
                && r.replacement.ex_date == issue.dates.start
        })
        .ok_or_else(|| invalid("distribution_resolution_event_missing"))?;
    let event = &revision.replacement;
    let EventTerms::Differential {
        reference:
            Reference::Formula {
                cash,
                share_multiplier,
                subscription_cash,
                evidence,
            },
        allocation: Allocation::Exact,
    } = &event.terms
    else {
        return Err(invalid(
            "distribution_resolution_requires_cash_only_differential",
        ));
    };
    if event.shares() != 0.
        || share_multiplier.parse::<f64>().ok() != Some(1.)
        || subscription_cash.parse::<f64>().ok() != Some(0.)
        || event.record_date.is_none()
        || event.pay_date.is_none()
    {
        return Err(invalid(
            "distribution_resolution_settlement_or_share_terms_missing",
        ));
    }
    let actual = revision
        .fields
        .get("cash_per_share")
        .ok_or_else(|| invalid("distribution_actual_cash_evidence_missing"))?;
    let reference = revision
        .fields
        .get("terms.reference.cash")
        .ok_or_else(|| invalid("distribution_reference_cash_evidence_missing"))?;
    let basis = actual.unit.strip_prefix("gross_CNY_per_");
    if !matches!(basis, Some("CDR" | "old_share"))
        || reference.unit != format!("reference_CNY_per_{}", basis.unwrap_or(""))
        || !number_in(&actual.quote, &event.cash_per_share)
        || !number_in(&reference.quote, cash)
        || !evidence.contains(&reference.quote)
    {
        return Err(invalid("distribution_cash_basis_or_quote_mismatch"));
    }
    verify_leaf_document(
        documents,
        actual,
        event,
        "cash_per_share",
        revision.participants.as_ref(),
    )?;
    verify_leaf_document(
        documents,
        reference,
        event,
        "terms.reference.cash",
        revision.participants.as_ref(),
    )?;
    Ok(())
}
fn number_in(quote: &str, value: &str) -> bool {
    quote
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .filter(|s| !s.is_empty())
        .any(|s| s == value)
}
pub(super) fn replay(
    bundle: &ActionBundle,
    request: &RevisionRequest,
    receipt: &Value,
    documents: &BTreeMap<String, (Value, String)>,
) -> io::Result<()> {
    let Some(review) = receipt.get("distribution_review") else {
        return Ok(());
    };
    if review["schema_version"] != 1 {
        return Err(invalid("unsupported_distribution_review"));
    }
    let issues: Vec<ExecutionIssue> =
        serde_json::from_value(review["issues"].clone()).map_err(invalid)?;
    if issues.is_empty() {
        return Err(invalid("distribution_review_issues_missing"));
    }
    for issue in issues {
        if issue.code != "distribution_conflict"
            || bundle.execution_issues.iter().any(|i| i.id == issue.id)
        {
            return Err(invalid("distribution_review_resolution_changed"));
        }
        verify(&issue, request, documents)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corporate_actions::evidence::{CaptureRequest, DocumentSpec, capture_with};
    #[test]
    fn single_official_document_resolves_only_reviewed_cash_conflict_and_replays() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("capture");
        let base = tmp.path().join("base");
        fs::create_dir_all(base.join("raw")).unwrap();
        let text = "600000 公司实际每股现金红利 0.2835 元。差异化除息参考现金 0.2798 元。登记日2024-05-23，除息和派息2024-05-24，不送股。";
        let url = "https://example.org/implementation.pdf";
        capture_with(
            &CaptureRequest {
                schema_version: 1,
                documents: vec![DocumentSpec {
                    id: "official".into(),
                    instrument: "SH600000".into(),
                    url: url.into(),
                    announced: "2024-05-18".parse().unwrap(),
                    title: "公司实际每股现金红利".into(),
                    implementation: true,
                }],
            },
            &capture,
            &analytics_core::task::TaskContext::default(),
            |_, p| {
                fs::write(p, b"%PDF-fixture")?;
                Ok((200, Some(0)))
            },
            |_| Ok(serde_json::json!({"text":text,"pypdf_version":"fixture"})),
        )
        .unwrap();
        let event:CorporateAction=serde_json::from_value(serde_json::json!({"id":"SH600000:2024-05-24","instrument":"SH600000","announced":"2024-05-18","record_date":null,"ex_date":"2024-05-24","pay_date":null,"listing_date":null,"cash_per_share":"0.2835","shares_per_share":"0","source":url,"terms":{"kind":"distribution","allocation":null}})).unwrap();
        let issue:ExecutionIssue=serde_json::from_value(serde_json::json!({"id":"conflict","instrument":"SH600000","announced":null,"dates":{"start":"2024-05-24","end":"2024-05-24"},"code":"distribution_conflict","event_category":"distribution","evidence":["original-provider"],"message":"actual and reference differ"})).unwrap();
        let mut unrelated = issue.clone();
        unrelated.id = "other-date".into();
        unrelated.dates.start = "2025-05-24".parse().unwrap();
        unrelated.dates.end = unrelated.dates.start;
        let source:ActionBundle=serde_json::from_value(serde_json::json!({"schema_version":10,"downloader":"JFQuant analytics-storage corporate-actions","downloaded_at":"fixture","source_bindings":{},"coverage":{},"queried_symbols":["SH600000"],"actions":[event],"failures":{"SH600000":"historical supplier failure"},"execution_issues":[issue,unrelated],"raw_sha256":{}})).unwrap();
        let mut replacement = source.actions[0].clone();
        replacement.record_date = Some("2024-05-23".parse().unwrap());
        replacement.pay_date = Some(replacement.ex_date);
        replacement.terms = EventTerms::Differential {
            reference: Reference::Formula {
                cash: "0.2798".into(),
                share_multiplier: "1".into(),
                subscription_cash: "0".into(),
                evidence: text.into(),
            },
            allocation: Allocation::Exact,
        };
        let proof = |quote: &str, unit: &str| FieldEvidence {
            document_id: "official".into(),
            quote: quote.into(),
            unit: unit.into(),
        };
        let mut request = RevisionRequest {
            schema_version: 5,
            evidence_manifest: capture.join("announcements.json"),
            structured_manifest: None,
            suspensions: vec![],
            coverage_reviews: vec![],
            resolve_failures: vec![],
            events: vec![EventRevision {
                participants: None,
                structured_fields: Default::default(),
                original_id: Some(replacement.id.clone()),
                original_sha256: Some(digest(&source.actions[0]).unwrap()),
                replacement,
                reason: "manual review of one complete formal document".into(),
                fields: BTreeMap::from([
                    ("record_date".into(), proof(text, "calendar_date")),
                    ("pay_date".into(), proof(text, "calendar_date")),
                    ("terms".into(), proof(text, "reviewed_differential_terms")),
                    (
                        "cash_per_share".into(),
                        proof(
                            "公司实际每股现金红利 0.2835 元。",
                            "gross_CNY_per_old_share",
                        ),
                    ),
                    (
                        "terms.reference.cash".into(),
                        proof(
                            "差异化除息参考现金 0.2798 元。",
                            "reference_CNY_per_old_share",
                        ),
                    ),
                ]),
            }],
            resolve_issues: vec![IssueResolution {
                id: issue.id.clone(),
                expected_sha256: digest(&issue).unwrap(),
                reason: "actual cash and reference independently recorded".into(),
            }],
        };
        assert!(validate_proposal(&source, &request).unwrap().is_empty());
        let output = tmp.path().join("published");
        let book = super::super::apply(&source, &base, &request, &output).unwrap();
        assert_eq!(book.execution_issues.len(), 1);
        assert_eq!(book.execution_issues[0].id, "other-date");
        assert_eq!(book.failures, source.failures);
        assert_eq!(book.actions[0].cash_per_share, "0.2835");
        assert_eq!(book.actions[0].reference_price(10.).unwrap(), 9.7202);
        ActionBundle::read(&output.join("actions.json")).unwrap();
        request.resolve_issues[0].expected_sha256 = "stale".into();
        assert!(!validate_proposal(&source, &request).unwrap().is_empty());
        request.resolve_issues[0].expected_sha256 = digest(&issue).unwrap();
        request.events[0].replacement.cash_per_share = "0.2798".into();
        assert!(
            super::super::apply(&source, &base, &request, &tmp.path().join("wrong-cash")).is_err()
        );
        assert!(!tmp.path().join("wrong-cash").exists());
        let mut changed = book;
        changed.execution_issues.push(issue);
        fs::write(
            output.join("actions.json"),
            serde_json::to_vec(&changed).unwrap(),
        )
        .unwrap();
        assert!(ActionBundle::read(&output.join("actions.json")).is_err());
    }
}
