//! Bounded announcement-backed full-day suspension facts, never trading quotations.
use super::*;
/// One interval beginning at the open and ending before the resumption session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuspensionRevision {
    /// Canonical security in both authenticated disclosures.
    pub instrument: String,
    /// First suspended session, inclusive.
    pub start: Date,
    /// First resumed session, exclusive; never inferred from publication time.
    pub resume: Date,
    /// Exact announcement excerpt proving the start boundary.
    pub start_evidence: FieldEvidence,
    /// Exact announcement excerpt proving the resumption boundary.
    pub resume_evidence: FieldEvidence,
    /// Human review explanation.
    pub reason: String,
}
fn compact(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}
fn has_date(text: &str, date: Date) -> bool {
    use chrono::Datelike;
    text.contains(&date.to_string())
        || text.contains(&format!(
            "{}年{}月{}日",
            date.year(),
            date.month(),
            date.day()
        ))
}
fn boundary_clause(text: &str, date: Date, starting: bool) -> bool {
    use chrono::Datelike;
    let dates = [
        date.to_string(),
        format!("{}年{}月{}日", date.year(), date.month(), date.day()),
    ];
    let markers: &[&str] = if starting {
        &["开市起停牌", "停牌一天"]
    } else {
        &["开市起复牌", "复牌之日起", "复牌后"]
    };
    text.split(['。', '；', ';']).any(|clause| {
        dates.iter().any(|date| {
            clause.match_indices(date).any(|(at, _)| {
                let prefix = &clause[..at];
                let tail = &clause[at + date.len()..];
                !["若", "预计", "最晚", "拟于", "计划于"]
                    .iter()
                    .any(|word| prefix.contains(word))
                    && markers.iter().any(|marker| {
                        tail.find(marker).is_some_and(|end| {
                            tail[..end].chars().count() <= 24
                                && !["预计", "最晚", "拟", "计划"]
                                    .iter()
                                    .any(|word| tail[..end].contains(word))
                        })
                    })
            })
        })
    })
}
pub(super) fn verify(
    s: &SuspensionRevision,
    documents: &BTreeMap<String, (Value, String)>,
) -> io::Result<()> {
    analytics_core::pool::validate_instrument(&s.instrument).map_err(invalid)?;
    if s.start >= s.resume || s.reason.trim().is_empty() {
        return Err(invalid("invalid_suspension_interval_or_reason"));
    }
    for (proof, date, starting) in [
        (&s.start_evidence, s.start, true),
        (&s.resume_evidence, s.resume, false),
    ] {
        let (d, text) = documents
            .get(&proof.document_id)
            .ok_or_else(|| invalid("suspension_document_missing"))?;
        let quote = compact(&proof.quote);
        let announced: Date = serde_json::from_value(d["announced"].clone()).map_err(invalid)?;
        let symbol = d["instrument"]
            .as_str()
            .ok_or_else(|| invalid("suspension_issuer_missing"))?;
        analytics_core::pool::validate_exchange_symbol(symbol).map_err(invalid)?;
        if d["instrument"] != s.instrument
            || !text.contains(&proof.quote)
            || proof.unit.trim().is_empty()
            || !compact(text).contains(&symbol[2..])
            || !has_date(&quote, date)
            || announced > date
            || quote.contains("盘中")
            || quote.contains("临时停牌")
            || !boundary_clause(&quote, date, starting)
        {
            return Err(invalid(
                "suspension_boundary_identity_quote_or_timing_invalid",
            ));
        }
    }
    Ok(())
}
pub(super) fn apply(
    bundle: &mut ActionBundle,
    request: &RevisionRequest,
    documents: &BTreeMap<String, (Value, String)>,
) -> io::Result<()> {
    for s in &request.suspensions {
        verify(s, documents)?;
        if !bundle.queried_symbols.contains(&s.instrument)
            && !bundle.failures.contains_key(&s.instrument)
        {
            return Err(invalid("suspension_security_not_in_source"));
        }
        let data = bundle.execution_data.get_or_insert_with(Default::default);
        data.schema_version = 2;
        data.capabilities.insert(
            format!("announcement-suspension-v1:{}", digest(s)?),
            serde_json::to_value(s).map_err(invalid)?,
        );
    }
    Ok(())
}
pub(super) fn replay(
    bundle: &ActionBundle,
    request: &RevisionRequest,
    documents: &BTreeMap<String, (Value, String)>,
) -> io::Result<()> {
    for s in &request.suspensions {
        verify(s, documents)?;
        if request.schema_version < 5
            || bundle.schema_version < 10
            || bundle.execution_data.as_ref().and_then(|d| {
                d.capabilities
                    .get(&format!("announcement-suspension-v1:{}", digest(s).ok()?))
            }) != Some(&serde_json::to_value(s).map_err(invalid)?)
        {
            return Err(invalid("suspension_revision_changed"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corporate_actions::evidence::{CaptureRequest, DocumentSpec, capture_with};
    #[test]
    fn tentative_resumption_is_not_a_verified_boundary() {
        let day = "2025-05-09".parse().unwrap();
        assert!(!boundary_clause(
            "若公司未能披露预案，公司股票最晚将于2025年5月9日开市起复牌",
            day,
            false
        ));
        assert!(!boundary_clause(
            "预计公司股票于2025年5月9日开市起复牌",
            day,
            false
        ));
        assert!(boundary_clause(
            "预计停牌不超过十天。经公司申请，公司股票将于2025年5月9日（星期五）开市起复牌",
            day,
            false
        ));
        assert!(boundary_clause(
            "公司股票于2025年5月9日停牌一天，并于2025年5月12日开市起复牌",
            day,
            true
        ));
    }
    #[test]
    fn paired_disclosures_publish_replay_and_cannot_clear_history_or_invent_boundaries() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("capture");
        let base = tmp.path().join("base");
        fs::create_dir_all(base.join("raw")).unwrap();
        let start = "600000 测试公司股票自2021年8月11日开市起停牌";
        let resume = "600000 测试公司股票自2021年8月12日复牌之日起恢复交易";
        let specs = [("stop", "2021-08-11"), ("resume", "2021-08-12")]
            .into_iter()
            .map(|(id, date)| DocumentSpec {
                id: id.into(),
                instrument: "SH600000".into(),
                url: format!("https://example.org/{id}.pdf"),
                announced: date.parse().unwrap(),
                title: "测试公司股票".into(),
                implementation: true,
            })
            .collect();
        capture_with(&CaptureRequest{schema_version:1,documents:specs},&capture,&analytics_core::task::TaskContext::default(),|_,p|{fs::write(p,b"%PDF-fixture")?;Ok((200,Some(0)))},|p|Ok(serde_json::json!({"text":if p.to_string_lossy().contains("stop"){start}else{resume},"pypdf_version":"fixture"}))).unwrap();
        let source:ActionBundle=serde_json::from_value(serde_json::json!({"schema_version":9,"downloader":"JFQuant analytics-workflow synthetic-demo-v1","downloaded_at":"fixture","source_bindings":{},"coverage":{},"queried_symbols":[],"actions":[],"failures":{"SH600000":"historical"},"raw_sha256":{}})).unwrap();
        let suspension = SuspensionRevision {
            instrument: "SH600000".into(),
            start: "2021-08-11".parse().unwrap(),
            resume: "2021-08-12".parse().unwrap(),
            reason: "reviewed bounded interval".into(),
            start_evidence: FieldEvidence {
                document_id: "stop".into(),
                quote: start.into(),
                unit: "session_open".into(),
            },
            resume_evidence: FieldEvidence {
                document_id: "resume".into(),
                quote: resume.into(),
                unit: "session_open".into(),
            },
        };
        let mut request = RevisionRequest {
            suspensions: vec![suspension],
            schema_version: 5,
            evidence_manifest: capture.join("announcements.json"),
            structured_manifest: None,
            events: vec![],
            resolve_failures: vec![],
            resolve_issues: vec![],
            coverage_reviews: vec![],
        };
        let output = tmp.path().join("published");
        let book = super::super::apply(&source, &base, &request, &output).unwrap();
        assert_eq!(book.failures, source.failures);
        assert!(book.actions.is_empty());
        let book = ActionBundle::read(&output.join("actions.json")).unwrap();
        let index = crate::trading_status::StatusIndex::build(&book).unwrap();
        assert!(index.full_day("SH600000", "2021-08-11".parse().unwrap()));
        assert!(!index.suspended("SH600000", "2021-08-12".parse().unwrap()));
        let mut tampered = book;
        tampered
            .execution_data
            .as_mut()
            .unwrap()
            .capabilities
            .clear();
        fs::write(
            output.join("actions.json"),
            serde_json::to_vec(&tampered).unwrap(),
        )
        .unwrap();
        assert!(ActionBundle::read(&output.join("actions.json")).is_err());
        request.suspensions[0].resume = "2021-08-13".parse().unwrap();
        assert!(
            super::super::apply(&source, &base, &request, &tmp.path().join("wrong_date")).is_err()
        );
        request.suspensions[0].resume = "2021-08-12".parse().unwrap();
        request.suspensions[0].start_evidence.quote = "2021年8月11日盘中临时停牌".into();
        assert!(
            super::super::apply(&source, &base, &request, &tmp.path().join("intraday")).is_err()
        );
    }
}
