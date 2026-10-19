//! Original-report ambiguities and the explicit versioned interpretations.
use serde::Serialize;

/// The issue that required an explicit v5 interpretation, retained for provenance.
pub fn interpretation_note(number: usize) -> Option<&'static str> {
    match number {
        10 => Some(
            "v5: move window 5 into rolling MAX, then cross-sectional RANK; corroborated by DolphinDB",
        ),
        36 => Some(
            "v5: CORR window 6, SUM window 2, then RANK; corrected catalogue agrees with DolphinDB",
        ),
        54 => Some(
            "v5: explicitly choose STD window 10, matching CORR and DolphinDB; not specified by paper",
        ),
        127 => Some(
            "v5: choose rolling MAX 12 and MEAN 12; paper omits MEAN window; differs from DolphinDB elementwise MAX",
        ),
        143 => Some(
            "v5: per-instrument SELF seed 1 at first valid positive close; up days multiply by simple return; no +1; null gaps preserve state without bridging",
        ),
        146 => Some(
            "v5: explicitly choose missing SMA M=2; retain paper numerator product and denominator squared smoother; DolphinDB instead omits product and uses MEAN",
        ),
        157 => Some(
            "v5: TSMIN 2, SUM 1, LOG, double RANK, PROD 1, TSMIN 5; retain delayed-return TSRANK 5; rolling outer MIN is explicit",
        ),
        165 | 183 => Some(
            "v5: explicit trailing-window rescaled range of close, window 48/24; cumulative deviations use the same window mean; both extrema divided by sample STD; not DolphinDB cross-sectional extrema",
        ),
        166 => Some(
            "v5: remove stray comma inside price ratio, make denominator multiplication explicit; retain printed first-power numerator and squared gross-return denominator, not a guessed skewness formula",
        ),
        181 => Some(
            "v5: choose missing denominator SUM window 20, corroborated by DolphinDB; retain paper subtraction and benchmark price levels",
        ),
        190 => Some(
            "v5: use the same centered (simple return minus 19-lag geometric threshold) squared in both SUMIF terms; denominator uses COUNT, not current-day condition; paper ambiguities recorded",
        ),
        _ => None,
    }
}
/// Current unresolved factors. v5 supplies explicit interpretations for all IDs.
pub fn quarantine_reason(_number: usize) -> Option<&'static str> {
    None
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
    /// Versioned interpretation, distinct from the immutable original formula.
    pub interpretation: Option<&'static str>,
}
/// All 191 records, in feature order, suitable for a research audit manifest.
pub fn entries() -> Vec<AuditEntry> {
    (1..=191).map(|n| AuditEntry {
        id: format!("alpha_{n:03}"),
        original_report_page: match n {1..=35=>11,36..=58=>12,59..=91=>13,92..=124=>14,125..=151=>15,152..=180=>16,_=>17},
        interpretation: interpretation_note(n),
        status: quarantine_reason(n).map(|s| format!("quarantined: {s}")).unwrap_or_else(|| {
            if n == 30 { "executable; requires genuine MKT/SMB/HML".into() }
            else if n == 28 { "executable printed formula; second denominator differs from common stochastic variant".into() }
            else if n == 149 { "executable conditional regression; FILTER selects paired observations in last 252 trading rows".into() }
            else { "executable under documented operator conventions; not certified equivalent".into() }
        }),
        validation: if interpretation_note(n).is_some() { "independent direct NumPy formula fixture plus hand-calculated recurrence/range edges; explicit v5 interpretation, not original-formula certification" } else if n == 2 { "hand-calculated alpha002 fixture plus operator fixtures" } else { "independent NumPy operator oracle on a synthetic 320-day/6-stock panel; AST/formula interpretation is shared, not independent original-formula certification" },
    }).collect()
}
