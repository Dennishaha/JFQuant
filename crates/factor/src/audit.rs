//! Original-report ambiguities are explicit and cannot silently enter a model.
use serde::Serialize;

/// An unresolved published formula or initialization requirement.
pub fn quarantine_reason(number: usize) -> Option<&'static str> {
    match number {
        10 => Some("original MAX/RANK parentheses place window outside MAX"),
        36 => Some("original CORR/SUM/RANK windows are misplaced; catalog silently reassigns them"),
        54 => Some("STD window missing in original; do not invent 10"),
        127 => Some("MEAN window missing in original; MAX(CLOSE,12) also ambiguous"),
        143 => Some("SELF means previous Alpha143, but initial value is unspecified"),
        146 => Some("denominator SMA lacks M; two arguments must not silently become MEAN"),
        157 => {
            Some("nested MIN/PROD/SUM windows and parentheses require a verified interpretation")
        }
        165 | 183 => {
            Some("SUMAC window and MAX/MIN dimensions missing; numerator parentheses ambiguous")
        }
        166 => Some("return moment expression has a misplaced comma and unclear powers"),
        181 => Some("denominator SUM window missing in original"),
        190 => Some(
            "moment/count parentheses ambiguous; catalog and executable DolphinDB source differ",
        ),
        _ => None,
    }
}
/// Canonical IDs excluded due to unresolved formula semantics.
pub fn quarantined_ids() -> Vec<String> {
    (1..=191)
        .filter(|n| quarantine_reason(*n).is_some())
        .map(|n| format!("alpha_{n:03}"))
        .collect()
}
/// Per-factor audit record. Executable does not mean independently certified.
#[derive(Serialize)]
pub struct AuditEntry {
    /// Canonical factor ID.
    pub id: String,
    /// Printed original report page (one based).
    pub original_report_page: usize,
    /// Execution eligibility and unresolved issues.
    pub status: String,
    /// Independent numerical comparison scope.
    pub validation: &'static str,
}
/// All 191 records, in feature order, suitable for a research audit manifest.
pub fn entries() -> Vec<AuditEntry> {
    (1..=191).map(|n| AuditEntry {
        id: format!("alpha_{n:03}"),
        original_report_page: match n {1..=35=>11,36..=58=>12,59..=91=>13,92..=124=>14,125..=151=>15,152..=180=>16,_=>17},
        status: quarantine_reason(n).map(|s| format!("quarantined: {s}")).unwrap_or_else(|| {
            if n == 30 { "executable; requires genuine MKT/SMB/HML".into() }
            else if n == 28 { "executable printed formula; second denominator differs from common stochastic variant".into() }
            else if n == 149 { "executable conditional regression; FILTER selects paired observations in last 252 trading rows".into() }
            else { "executable under documented operator conventions; not certified equivalent".into() }
        }),
        validation: if n == 2 { "hand-calculated alpha002 fixture plus operator fixtures" } else { if quarantine_reason(n).is_some() { "quarantined; no numerical equivalence claim" } else { "independent NumPy operator oracle on a synthetic 320-day/6-stock panel; AST/formula interpretation is shared, not independent original-formula certification" } },
    }).collect()
}
