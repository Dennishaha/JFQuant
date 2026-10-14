//! Explicit check outcomes prevent absent or skipped checks from looking successful.
use crate::DateRange;
use crate::research_quality::{
    ProblemDisposition, ProblemKind, QualityDecision, ResearchQualityPolicy,
};
use serde::{Deserialize, Serialize};
/// A check is either proved, failed, or could not run with the supplied prerequisites.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// All required evidence was checked.
    Passed,
    /// A required dependency is missing or inconsistent.
    Failed,
    /// Prerequisites are unavailable; this is never a pass.
    NotRun,
}
/// One dependency check, shared by App, CLI and the training gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionCheck {
    /// Structured classification; missing is distinct from corrupt or conflicting data.
    pub kind: ProblemKind,
    /// Handling under the selected policy; factual status is retained.
    pub disposition: ProblemDisposition,
    /// identity, market, membership, coefficient, events, settlement, samples, or benchmark.
    pub category: String,
    /// Optional security key or unresolved source symbol.
    pub security: Option<String>,
    /// Required inclusive dates.
    pub coverage: Option<DateRange>,
    /// Explicit outcome.
    pub status: CheckStatus,
    /// Stable machine-readable reason.
    pub code: String,
    /// User-facing cause, including missing prerequisites.
    pub message: String,
    /// Evidence hashes or source references checked.
    pub evidence: Vec<String>,
    /// Requested factor IDs affected by this check.
    pub affected_factors: Vec<String>,
}
/// Complete admission decision; an empty checklist cannot permit training.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmissionReport {
    /// Current admission report format, 3.
    pub schema_version: u32,
    /// Actual frozen execution policy.
    pub policy: ResearchQualityPolicy,
    /// Dated exclusions and null propagation, separate from required checks.
    pub decisions: Vec<QualityDecision>,
    /// All required check results.
    pub checks: Vec<AdmissionCheck>,
}
impl AdmissionReport {
    /// True only when all checks ran and every failure has policy-permitted missing-data handling.
    pub fn ready(&self) -> bool {
        self.schema_version == 4
            && !self.checks.is_empty()
            && self.policy.validate().is_ok()
            && self.decisions.iter().all(|d| {
                d.kind == ProblemKind::Missing
                    && match d.disposition {
                        ProblemDisposition::Block => false,
                        ProblemDisposition::KeepNull => self.policy.allows_missing(),
                        ProblemDisposition::ExcludeSample => {
                            self.policy.strictness
                                != crate::research_quality::ResearchStrictness::Strict
                        }
                        ProblemDisposition::ExcludeSecurity => {
                            self.policy.strictness
                                == crate::research_quality::ResearchStrictness::QualifiedOnly
                        }
                    }
            })
            && self.checks.iter().all(|c| {
                c.status == CheckStatus::Passed
                    || (c.status == CheckStatus::Failed
                        && c.kind == ProblemKind::Missing
                        && c.disposition == ProblemDisposition::KeepNull
                        && self.policy.allows_missing())
            })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn skipped_failed_and_empty_checks_never_authorize_training() {
        let mut r = AdmissionReport {
            schema_version: 4,
            policy: ResearchQualityPolicy::default(),
            decisions: vec![],
            checks: vec![],
        };
        assert!(!r.ready());
        r.checks.push(AdmissionCheck {
            kind: ProblemKind::Missing,
            disposition: ProblemDisposition::Block,
            category: "events".into(),
            security: None,
            coverage: None,
            status: CheckStatus::NotRun,
            code: "missing_source".into(),
            message: "no evidence".into(),
            evidence: vec![],
            affected_factors: vec![],
        });
        assert!(!r.ready());
        r.checks[0].status = CheckStatus::Failed;
        assert!(!r.ready());
        r.checks[0].status = CheckStatus::Passed;
        assert!(r.ready());
    }
    #[test]
    fn only_genuine_missing_features_can_be_nonblocking_and_not_run_never_passes() {
        let mut r = AdmissionReport {
            schema_version: 4,
            policy: ResearchQualityPolicy {
                strictness: crate::research_quality::ResearchStrictness::AllowMissing,
                ..Default::default()
            },
            decisions: vec![],
            checks: vec![AdmissionCheck {
                kind: ProblemKind::Missing,
                disposition: ProblemDisposition::KeepNull,
                category: "market".into(),
                security: None,
                coverage: None,
                status: CheckStatus::Failed,
                code: "input_file_missing".into(),
                message: "amount missing".into(),
                evidence: vec![],
                affected_factors: vec![],
            }],
        };
        assert!(r.ready());
        r.checks[0].kind = ProblemKind::Damaged;
        assert!(!r.ready());
        r.checks[0].kind = ProblemKind::Conflict;
        assert!(!r.ready());
        r.checks[0].kind = ProblemKind::Missing;
        r.checks[0].status = CheckStatus::NotRun;
        assert!(!r.ready());
        r.checks[0].status = CheckStatus::Failed;
        r.policy.strictness = crate::research_quality::ResearchStrictness::Strict;
        assert!(!r.ready());
    }
}
