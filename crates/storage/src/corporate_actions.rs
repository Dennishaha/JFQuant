//! Immutable company-action bundles and evidence validation. Incomplete dates stay null.
//! Provider success is recorded separately from execution eligibility.
pub mod execution_index;
pub mod facts;
pub mod reconcile;
pub mod review;

use analytics_core::{Date, execution::CorporateAction};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::Path,
};
fn task_context_check() -> io::Result<()> {
    analytics_core::task::TaskContext::current().check()
}
fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::other(message.to_string())
}
/// Required ordinary-holder event categories; a dividend endpoint cannot prove the others.
pub const EVENT_CATEGORIES: &[&str] = &[
    "distribution",
    "differential",
    "rights_issue",
    "reduction",
    "reform",
    "reorganization",
    "merger",
    "code_change",
    "delisting",
];
/// Explicit source coverage claim for one security, category and date range.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventCoverage {
    /// Event category from EVENT_CATEGORIES.
    pub category: String,
    /// Inclusive dates actually verified.
    pub dates: analytics_core::DateRange,
    /// passed, failed, or not_run.
    pub status: analytics_core::admission::CheckStatus,
    /// Original source or exhaustive query receipt.
    pub evidence: Vec<String>,
    /// Why the source proves or cannot prove completeness.
    pub reason: String,
}

/// Versioned public-download bundle. Missing dates never count as complete.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionBundle {
    /// Replayed structured field facts, distinct from executable event terms.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub field_facts: BTreeMap<String, facts::FactReport>,
    /// Complete structured queries accepted by the supplier execution policy.
    #[serde(default)]
    pub structured_coverage: BTreeMap<String, crate::execution_data::StructuredCoverage>,
    /// Discovered incomplete events and source conflicts, independently dated.
    #[serde(default)]
    pub execution_issues: Vec<crate::execution_data::ExecutionIssue>,
    /// Optional immutable format-1 suspension, limit and coefficient diagnostics.
    #[serde(default)]
    pub execution_data: Option<crate::execution_data::ExecutionData>,
    /// Explicit acquisition locators and their proved/requested date scope.
    pub source_bindings: BTreeMap<String, analytics_core::source_map::SourceLocator>,
    /// Per-security/category historical completeness, separate from download success.
    pub coverage: BTreeMap<String, Vec<EventCoverage>>,
    /// JSON schema version.
    pub schema_version: u32,
    /// JFQuant downloader identity.
    pub downloader: String,
    /// Acquisition time in UTC.
    pub downloaded_at: String,
    /// Successfully queried complete history pages, including genuine empty histories.
    pub queried_symbols: BTreeSet<String>,
    /// Implemented distributions, potentially missing pay/listing dates.
    pub actions: Vec<CorporateAction>,
    /// Failed symbols and reasons; must be retried before using their history.
    pub failures: BTreeMap<String, String>,
    /// Saved raw JSON filename to SHA256, for independent provenance review.
    pub raw_sha256: BTreeMap<String, String>,
}
impl ActionBundle {
    /// Resolve a frozen provider identity; opaque IDs are never sliced into stock codes.
    pub fn source_symbol<'a>(&'a self, id: &'a str) -> io::Result<&'a str> {
        if self.source_bindings.contains_key(id) {
            let l = self
                .source_bindings
                .get(id)
                .ok_or_else(|| invalid("company-action provider locator missing"))?;
            analytics_core::pool::validate_exchange_symbol(&l.symbol).map_err(invalid)?;
            Ok(&l.symbol)
        } else {
            analytics_core::pool::validate_exchange_symbol(id).map_err(invalid)?;
            Ok(id)
        }
    }
    /// Mark every event category only for an explicitly generated complete synthetic fixture.
    pub fn declare_synthetic_coverage(&mut self, start: Date, end: Date) -> io::Result<()> {
        if self.downloader != "JFQuant analytics-workflow synthetic-demo-v1" {
            return Err(invalid("cannot assert public coverage as synthetic"));
        }
        for id in &self.queried_symbols {
            self.coverage.insert(
                id.clone(),
                EVENT_CATEGORIES
                    .iter()
                    .map(|category| EventCoverage {
                        category: (*category).into(),
                        dates: analytics_core::DateRange { start, end },
                        status: analytics_core::admission::CheckStatus::Passed,
                        evidence: vec!["JFQuant complete synthetic fixture".into()],
                        reason: "all fixture events explicitly constructed".into(),
                    })
                    .collect(),
            );
        }
        Ok(())
    }
    /// Aggregate missing categories; queried_symbols alone never grants all-event completeness.
    pub fn coverage_issues(&self, id: &str, start: Date, end: Date) -> Vec<String> {
        EVENT_CATEGORIES
            .iter()
            .filter_map(|category| {
                let mut ranges: Vec<_> = self
                    .coverage
                    .get(id)
                    .into_iter()
                    .flatten()
                    .filter(|c| {
                        c.category == *category
                            && c.status == analytics_core::admission::CheckStatus::Passed
                            && !c.evidence.is_empty()
                            && !c.reason.is_empty()
                    })
                    .map(|c| &c.dates)
                    .collect();
                ranges.sort_by_key(|r| r.start);
                let mut next = start;
                let mut covered = false;
                for r in ranges {
                    if r.end < next {
                        continue;
                    }
                    if r.start > next {
                        break;
                    }
                    if r.end >= end {
                        covered = true;
                        break;
                    }
                    if let Some(d) = r.end.succ_opt() {
                        next = d;
                    }
                }
                (!covered)
                    .then(|| format!("{id} {category}: completeness unverified for {start}..{end}"))
            })
            .collect()
    }
    /// Transitive ordinary-holder successors needed during an actual account interval.
    pub fn required_symbols(
        &self,
        seeds: &BTreeSet<String>,
        start: Date,
        end: Date,
    ) -> BTreeSet<String> {
        let mut out = seeds.clone();
        loop {
            let before = out.len();
            for a in &self.actions {
                if out.contains(&a.instrument)
                    && a.ex_date >= start
                    && a.record_date.is_none_or(|d| d <= end)
                    && let analytics_core::actions::EventTerms::Conversion {
                        successor: Some(id),
                        ordinary_holder: true,
                        ..
                    } = &a.terms
                {
                    out.insert(id.clone());
                }
            }
            if out.len() == before {
                break;
            }
        }
        out
    }

    fn validate_identity(&self) -> io::Result<()> {
        if !matches!(self.schema_version, 5..=8)
            || (self.downloader != "JFQuant analytics-storage corporate-actions"
                && self.downloader != "JFQuant analytics-workflow synthetic-demo-v1")
        {
            return Err(invalid(
                "unsupported_data_format: action bundle requires supported format 5/6/7/8 and known source",
            ));
        }
        analytics_core::source_map::reject_removed_sources(
            &serde_json::to_value(self).map_err(invalid)?,
        )
        .map_err(invalid)?;
        if let Some(data) = &self.execution_data {
            data.validate()?;
        }
        for (id, c) in &self.structured_coverage {
            analytics_core::pool::validate_instrument(id).map_err(invalid)?;
            c.dates.validate().map_err(invalid)?;
            if !matches!(c.source.as_str(), "hithink.events/per-share-v1")
                || c.evidence.is_empty()
                || !self.queried_symbols.contains(id)
            {
                return Err(invalid("invalid_structured_coverage"));
            }
        }
        for issue in &self.execution_issues {
            analytics_core::pool::validate_instrument(&issue.instrument).map_err(invalid)?;
            issue.dates.validate().map_err(invalid)?;
            if issue.code.is_empty() || issue.message.is_empty() {
                return Err(invalid("invalid_execution_issue"));
            }
        }
        for claims in self.coverage.values() {
            for c in claims {
                c.dates.validate().map_err(invalid)?;
                if !EVENT_CATEGORIES.contains(&c.category.as_str()) || c.reason.trim().is_empty() {
                    return Err(invalid("invalid event category coverage"));
                }
                if c.status == analytics_core::admission::CheckStatus::Passed
                    && self.downloader != "JFQuant analytics-workflow synthetic-demo-v1"
                    && (c.evidence.is_empty()
                        || c.evidence.iter().any(|e| !self.raw_sha256.contains_key(e)))
                {
                    return Err(invalid(
                        "public coverage claim lacks captured review evidence",
                    ));
                }
            }
        }
        for id in self.queried_symbols.iter().chain(self.failures.keys()) {
            analytics_core::pool::validate_instrument(id).map_err(invalid)?;
        }
        for a in &self.actions {
            analytics_core::pool::validate_instrument(&a.instrument).map_err(invalid)?;
            if a.id != format!("{}:{}", a.instrument, a.ex_date) {
                return Err(invalid("corporate action ID/security/date mismatch"));
            }
        }
        for name in self.raw_sha256.keys() {
            if Path::new(name).file_name().and_then(|s| s.to_str()) != Some(name.as_str()) {
                return Err(invalid("unsafe evidence filename"));
            }
        }
        Ok(())
    }
    /// Require verified histories and adjustment terms over training/warmup dates.
    /// Payment/listing dates remain mandatory separately for actual execution rights.
    pub fn require_adjustment_coverage(
        &self,
        symbols: &BTreeSet<String>,
        start: Date,
        end: Date,
    ) -> io::Result<()> {
        self.validate_identity()?;
        if start > end {
            return Err(invalid("invalid action bundle identity/range"));
        }
        for id in symbols {
            if !self.queried_symbols.contains(id) || self.failures.contains_key(id) {
                return Err(invalid(format!("company-action history unavailable: {id}")));
            }
            let issues = self.coverage_issues(id, start, end);
            if !issues.is_empty() {
                return Err(invalid(issues.join("; ")));
            }
        }
        let mut seen = BTreeSet::new();
        for a in &self.actions {
            if !seen.insert(a.id.clone()) {
                return Err(invalid("duplicate corporate-action event"));
            }
            if symbols.contains(&a.instrument) && a.ex_date >= start && a.ex_date <= end {
                a.validate_adjustment_terms()
                    .map_err(|e| invalid(format!("{} {}: {e}", a.instrument, a.ex_date)))?;
            }
        }
        Ok(())
    }
    /// Require all symbols and complete settlement dates within the execution range.
    pub fn require_coverage(
        &self,
        symbols: &BTreeSet<String>,
        start: Date,
        end: Date,
    ) -> io::Result<()> {
        self.require_adjustment_coverage(symbols, start, end)?;
        for a in &self.actions {
            if symbols.contains(&a.instrument)
                && a.ex_date >= start
                && a.record_date.is_none_or(|d| d <= end)
            {
                if let analytics_core::actions::EventTerms::Delisting {
                    valuations,
                    settlement_date,
                    ..
                } = &a.terms
                {
                    let until = settlement_date.map_or(end, |d| d.min(end));
                    if valuations.last().is_none_or(|v| v.valid_until < until) {
                        return Err(invalid(format!(
                            "{}: delisting valuation coverage missing",
                            a.id
                        )));
                    }
                }
                a.validate()
                    .map_err(|e| invalid(format!("{} {}: {e}", a.instrument, a.ex_date)))?;
            }
        }
        Ok(())
    }
    /// Canonical version identity keeps legacy numeric fields and absent metadata intact.
    /// Validation uses the supported typed reader; hashing does not rewrite format 5.
    pub fn version_content(path: &Path) -> io::Result<Value> {
        let book = Self::read(path)?;
        if book.schema_version == 5 {
            serde_json::from_slice(&fs::read(path)?).map_err(invalid)
        } else {
            serde_json::to_value(book).map_err(invalid)
        }
    }
    /// Read without silently upgrading a historical schema.
    pub fn read(path: &Path) -> io::Result<Self> {
        let raw: Value = serde_json::from_slice(&fs::read(path)?).map_err(invalid)?;
        if matches!(raw["schema_version"].as_u64(), Some(6..=8))
            && raw["actions"].as_array().is_none_or(|rows| {
                rows.iter()
                    .any(|a| !a["cash_per_share"].is_string() || !a["shares_per_share"].is_string())
            })
        {
            return Err(invalid("format 6 action amounts must be decimal text"));
        }
        let bundle: Self = serde_json::from_value(raw).map_err(invalid)?;
        let mut ids = BTreeSet::new();
        if bundle.actions.iter().any(|a| !ids.insert(a.id.clone())) {
            return Err(invalid("duplicate corporate-action event"));
        }

        let root = path
            .parent()
            .ok_or_else(|| invalid("missing action root"))?;
        if root.join("owner.json").is_file() {
            let owner: Value =
                serde_json::from_slice(&fs::read(root.join("owner.json"))?).map_err(invalid)?;
            analytics_core::source_map::reject_removed_sources(&owner).map_err(invalid)?;
        }
        for name in bundle
            .raw_sha256
            .keys()
            .filter(|n| n.ends_with("-actions-structured.json"))
        {
            if Path::new(name).components().count() != 1 {
                return Err(invalid("unsafe action evidence"));
            }
            let shard: Value =
                serde_json::from_slice(&fs::read(root.join("raw").join(name))?).map_err(invalid)?;
            analytics_core::source_map::reject_removed_sources(&shard).map_err(invalid)?;
        }
        for name in bundle.raw_sha256.keys() {
            if Path::new(name).components().count() != 1 {
                return Err(invalid("unsafe action evidence"));
            }
            let bytes = fs::read(root.join("raw").join(name))?;
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                analytics_core::source_map::reject_removed_sources(&value).map_err(invalid)?;
            }
        }
        bundle.validate_identity()?;
        if bundle.schema_version < 8
            && bundle.actions.iter().any(|a| {
                matches!(
                    a.terms,
                    analytics_core::actions::EventTerms::RightsIssueFacts { .. }
                )
            })
        {
            return Err(invalid("partial rights facts require action format 8"));
        }
        if bundle.schema_version >= 6 {
            let root = path
                .parent()
                .ok_or_else(|| invalid("missing action root"))?;
            for (name, expected) in &bundle.raw_sha256 {
                if Path::new(name).components().count() != 1
                    || format!(
                        "sha256:{:x}",
                        Sha256::digest(fs::read(root.join("raw").join(name))?)
                    ) != *expected
                {
                    return Err(invalid("action evidence bytes changed"));
                }
            }
        }
        if let Some(expected) = bundle.raw_sha256.get("reconciliation-v1.json") {
            let root = path
                .parent()
                .ok_or_else(|| invalid("missing action root"))?;
            let bytes = fs::read(root.join("raw/reconciliation-v1.json"))?;
            if format!("sha256:{:x}", Sha256::digest(&bytes)) != *expected {
                return Err(invalid("reconciliation receipt hash mismatch"));
            }
            let receipt: Value = serde_json::from_slice(&bytes).map_err(invalid)?;
            if receipt["schema_version"] != 1 || receipt["rule_version"] != "jfquant-actions-v2" {
                return Err(invalid("unsupported reconciliation rules"));
            }
            for (name, expected) in receipt["evidence_sha256"]
                .as_object()
                .ok_or_else(|| invalid("missing replay evidence hashes"))?
            {
                if bundle.raw_sha256.get(name).map(String::as_str) != expected.as_str() {
                    return Err(invalid("replay evidence binding changed"));
                }
            }
            let events: BTreeMap<_, _> = bundle
                .actions
                .iter()
                .map(|a| Ok((a.id.clone(), serde_json::to_value(a).map_err(invalid)?)))
                .collect::<io::Result<_>>()?;
            let issues: BTreeMap<_, _> = bundle
                .execution_issues
                .iter()
                .filter(|i| !i.id.is_empty())
                .map(|i| Ok((i.id.clone(), serde_json::to_value(i).map_err(invalid)?)))
                .collect::<io::Result<_>>()?;
            for (id, result) in receipt["results"]
                .as_object()
                .ok_or_else(|| invalid("missing replay results"))?
            {
                task_context_check()?;
                for expected in result["records"]
                    .as_array()
                    .ok_or_else(|| invalid("missing replay events"))?
                {
                    let event: CorporateAction =
                        serde_json::from_value(expected.clone()).map_err(invalid)?;
                    if event.instrument != *id
                        || events.get(&event.id)
                            != Some(&serde_json::to_value(&event).map_err(invalid)?)
                    {
                        return Err(invalid(
                            "normalized event differs from reconciliation receipt",
                        ));
                    }
                }
                for expected in result["issues"]
                    .as_array()
                    .ok_or_else(|| invalid("missing replay issues"))?
                {
                    let issue: crate::execution_data::ExecutionIssue =
                        serde_json::from_value(expected.clone()).map_err(invalid)?;
                    if issue.instrument != *id || issues.get(&issue.id) != Some(expected) {
                        return Err(invalid("issue differs from reconciliation receipt"));
                    }
                }
            }
        }
        if bundle.downloader != "JFQuant analytics-workflow synthetic-demo-v1" {
            let root = path
                .parent()
                .ok_or_else(|| invalid("action evidence root missing"))?;
            for (id, c) in &bundle.structured_coverage {
                for name in &c.evidence {
                    if Path::new(name).components().count() != 1 {
                        return Err(invalid("unsafe structured evidence"));
                    }
                    let bytes = fs::read(root.join("raw").join(name))?;
                    if bundle.raw_sha256.get(name)
                        != Some(&format!("sha256:{:x}", Sha256::digest(&bytes)))
                    {
                        return Err(invalid("structured_evidence_hash_mismatch"));
                    }
                    let shard: Value = serde_json::from_slice(&bytes).map_err(invalid)?;
                    if shard["instrument"] != *id
                        || shard["history_verified"] != true
                        || shard["evidence"]["history_complete"] != true
                        || shard["evidence"]["provider"] != "hithink"
                        || shard["evidence"]["source_rule"] != c.source
                        || shard["evidence"]["request_range"]
                            != serde_json::to_value(&c.dates).map_err(invalid)?
                    {
                        return Err(invalid("structured_source_claim_mismatch"));
                    }
                    crate::collector::verify_http_receipts(root, &shard["evidence"])?;
                    let code = crate::hithink::thscode(bundle.source_symbol(id)?)?;
                    let events = &shard["evidence"]["events"];
                    if events["thscode"] != code || !events["item"].is_array() {
                        return Err(invalid("hithink_event_identity_missing"));
                    }
                    let captured =
                        shard["evidence"]["http_receipts"]
                            .as_array()
                            .is_some_and(|rs| {
                                rs.iter().any(|r| {
                                    r["status"] == 200
                                        && r["url"].as_str().is_some_and(|u| {
                                            crate::hithink::action_receipt_url(u, &code, &c.dates)
                                        })
                                        && r["filename"]
                                            .as_str()
                                            .and_then(|n| fs::read(root.join("raw").join(n)).ok())
                                            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                                            .is_some_and(|v| {
                                                crate::hithink::action_payload(&v, &code)
                                                    .is_ok_and(|data| data == *events)
                                            })
                                })
                            });
                    if !captured {
                        return Err(invalid("hithink_event_receipt_mismatch"));
                    }
                }
            }
            for (id, claims) in &bundle.coverage {
                for c in claims
                    .iter()
                    .filter(|c| c.status == analytics_core::admission::CheckStatus::Passed)
                {
                    for name in &c.evidence {
                        if !name.starts_with("review-") {
                            return Err(invalid(
                                "event completeness needs an auditable review receipt",
                            ));
                        }
                        let bytes = fs::read(root.join("raw").join(name))?;
                        if bundle.raw_sha256.get(name)
                            != Some(&format!("sha256:{:x}", Sha256::digest(&bytes)))
                        {
                            return Err(invalid("category review receipt hash mismatch"));
                        }
                        let review: Value = serde_json::from_slice(&bytes).map_err(invalid)?;
                        let consistent = review["request"]["coverage_reviews"]
                            .as_array()
                            .is_some_and(|rs| {
                                rs.iter().any(|r| {
                                    r["instrument"] == *id
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
                        let receipt = &review["collection_receipts"][id];
                        if receipt["evidence"]["pagination_complete"] != true
                            || !receipt["evidence"]["all_announcements"].is_array()
                        {
                            return Err(invalid(
                                "category review lacks complete disclosure metadata",
                            ));
                        }
                        crate::collector::verify_http_receipts(root, &receipt["evidence"])?;
                    }
                }
            }
        }
        if let Some(data) = &bundle.execution_data {
            for (key, capability) in &data.capabilities {
                let Some(id) = key.strip_prefix(&format!("{}:", crate::tdx_status::RULE)) else {
                    continue;
                };
                analytics_core::pool::validate_instrument(id).map_err(invalid)?;
                let name = capability["structured"]
                    .as_str()
                    .ok_or_else(|| invalid("status structured receipt missing"))?;
                if Path::new(name).file_name().and_then(|s| s.to_str()) != Some(name)
                    || !bundle.raw_sha256.contains_key(name)
                {
                    return Err(invalid("status evidence binding missing"));
                }
                let range: analytics_core::DateRange =
                    serde_json::from_value(capability["range"].clone()).map_err(invalid)?;
                range.validate().map_err(invalid)?;
                let symbol = capability["source_symbol"]
                    .as_str()
                    .ok_or_else(|| invalid("status symbol missing"))?;
                let shard: Value = serde_json::from_slice(&fs::read(root.join("raw").join(name))?)
                    .map_err(invalid)?;
                if shard["instrument"] != id || capability["status"] != "available" {
                    return Err(invalid("status identity mismatch"));
                }
                let expected =
                    crate::tdx_status::from_evidence(root, &shard["evidence"], symbol, &range)?;
                if data.suspensions.get(id) != Some(&expected) {
                    return Err(invalid("status normalization/evidence mismatch"));
                }
            }
        }
        if bundle.schema_version < 7 && !bundle.field_facts.is_empty() {
            return Err(invalid("field facts require format 7"));
        }
        if bundle.schema_version >= 7 && facts::derive(&bundle, root)? != bundle.field_facts {
            return Err(invalid(
                "structured field facts differ from original evidence",
            ));
        }
        if bundle.schema_version >= 7 {
            review::verify_structured_revisions(&bundle, root)?;
        }
        Ok(bundle)
    }
}

#[cfg(test)]
mod removed_source_tests {
    use super::*;
    #[test]
    fn retired_raw_evidence_is_rejected_even_in_supported_bundle_without_deleting_it() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("raw")).unwrap();
        let raw = br#"{"provider":"hithink","receipts":[{"url":"https://money.finance.sina.com.cn/history"}]}"#;
        fs::write(root.path().join("raw/receipt.json"), raw).unwrap();
        let book = ActionBundle {
            field_facts: Default::default(),
            schema_version: 5,
            downloader: "JFQuant analytics-storage corporate-actions".into(),
            downloaded_at: "2026-10-15T00:00:00Z".into(),
            queried_symbols: Default::default(),
            actions: vec![],
            failures: Default::default(),
            source_bindings: Default::default(),
            coverage: Default::default(),
            structured_coverage: Default::default(),
            execution_issues: vec![],
            execution_data: None,
            raw_sha256: BTreeMap::from([(
                "receipt.json".into(),
                format!("sha256:{:x}", Sha256::digest(raw)),
            )]),
        };
        let path = root.path().join("actions.json");
        fs::write(&path, serde_json::to_vec(&book).unwrap()).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(
            ActionBundle::read(&path)
                .unwrap_err()
                .to_string()
                .contains("removed_provider")
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read(root.path().join("raw/receipt.json")).unwrap(), raw);
    }
}
