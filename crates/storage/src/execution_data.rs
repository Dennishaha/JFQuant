//! One immutable execution-data snapshot; indices separate acquisition from account eligibility.
use analytics_core::{Date, DateRange};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
/// A complete structured-source query, not a claim about every corporate-action category.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredCoverage {
    /// Inclusive effective-date coverage.
    pub dates: DateRange,
    /// Fixed source and interpretation rule.
    pub source: String,
    /// Captured structured shard/HTTP evidence names.
    pub evidence: Vec<String>,
}
/// A discovered unresolved dependency. Absence is never silently replaced by zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionIssue {
    /// Internal data key.
    pub instrument: String,
    /// Earliest public knowledge date; missing dates do not guess an earlier eligibility change.
    pub announced: Option<Date>,
    /// Account dates affected, or source coverage if event timing itself is unknown.
    pub dates: DateRange,
    /// Stable machine-readable cause.
    pub code: String,
    /// Human-readable explanation, without credentials.
    pub message: String,
}
/// Frozen auxiliary daily data. It never replaces the selected price source or factor files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionData {
    /// Execution-data format 1.
    pub schema_version: u32,
    /// Known suspension dates; values full_day or intraday.
    pub suspensions: BTreeMap<String, BTreeMap<Date, String>>,
    /// Exact daily (lower, upper) raw-CNY limits.
    pub limits: BTreeMap<String, BTreeMap<Date, [f64; 2]>>,
    /// Source coefficients used only for transition diagnostics.
    pub coefficients: BTreeMap<String, BTreeMap<Date, f64>>,
    /// Endpoint outcomes, including unavailable optional permissions.
    pub capabilities: BTreeMap<String, serde_json::Value>,
}
impl ExecutionData {
    /// Reject invalid values before indexing or publishing a snapshot.
    pub fn validate(&self) -> std::io::Result<()> {
        if self.schema_version != 1
            || self
                .limits
                .values()
                .flat_map(|v| v.values())
                .any(|v| !v[0].is_finite() || !v[1].is_finite() || v[0] <= 0. || v[1] < v[0])
            || self
                .coefficients
                .values()
                .flat_map(|v| v.values())
                .any(|v| !v.is_finite() || *v <= 0.)
            || self
                .suspensions
                .values()
                .flat_map(|v| v.values())
                .any(|v| !matches!(v.as_str(), "full_day" | "intraday"))
        {
            return Err(std::io::Error::other("invalid_execution_data"));
        }
        Ok(())
    }
}
