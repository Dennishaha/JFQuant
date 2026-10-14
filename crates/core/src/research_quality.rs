//! Research completeness policy. Missing observations never certify corrupt inputs.
use crate::DateRange;
use serde::{Deserialize, Serialize};

/// The same three choices are used by GUI, CLI and the worker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResearchStrictness {
    /// Require all current dependencies; ordinary formula nulls remain legitimate.
    #[default]
    Strict,
    /// Keep the original universe and propagate genuinely absent inputs as null.
    AllowMissing,
    /// Derive a date-effective universe from verified available inputs.
    QualifiedOnly,
}
impl ResearchStrictness {
    /// Plain-language choice shown in the application.
    pub fn label(self) -> &'static str {
        match self {
            Self::Strict => "1. 严格检查",
            Self::AllowMissing => "2. 允许部分数据缺失",
            Self::QualifiedOnly => "3. 只用数据合格的股票",
        }
    }
    /// Explain what is relaxed without changing the account's evidence requirements.
    pub fn description(self) -> &'static str {
        match self {
            Self::Strict => "必要检查不通过就停止；正常因子空值仍允许。",
            Self::AllowMissing => "保留原股票池，缺失输入保留为空值，用有效样本训练。",
            Self::QualifiedOnly => "按日期排除不合格候选，使用筛选后的研究股票池。",
        }
    }
}

/// Frozen data and sample policy; thresholds are execution floors, not statistical guarantees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResearchQualityPolicy {
    /// Completeness choice, default strict.
    pub strictness: ResearchStrictness,
    /// Required fraction of finite selected features; zero still requires one.
    pub min_valid_feature_ratio: f64,
    /// Minimum eligible labeled rows in each training and required validation split.
    pub min_labeled_rows: usize,
}
impl Default for ResearchQualityPolicy {
    fn default() -> Self {
        Self {
            strictness: ResearchStrictness::Strict,
            min_valid_feature_ratio: 0.,
            min_labeled_rows: 1,
        }
    }
}
impl ResearchQualityPolicy {
    /// Reject invalid thresholds before any task is submitted.
    pub fn validate(&self) -> Result<(), String> {
        if !self.min_valid_feature_ratio.is_finite()
            || !(0.0..=1.0).contains(&self.min_valid_feature_ratio)
            || self.min_labeled_rows == 0
        {
            return Err("invalid research quality thresholds".into());
        }
        Ok(())
    }
    /// Missing input vectors are allowed only in the explicitly relaxed mode.
    pub fn allows_missing(&self) -> bool {
        self.strictness == ResearchStrictness::AllowMissing
    }
    /// Preserve existing strict-mode handling; relaxed modes drop completely empty rows.
    pub fn accepts_features(&self, finite: usize, columns: usize) -> bool {
        self.strictness == ResearchStrictness::Strict
            || (columns > 0
                && finite > 0
                && finite as f64 / columns as f64 >= self.min_valid_feature_ratio)
    }
}

/// Machine-readable problem class; policies never classify by message substrings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// A required observation or source file is absent.
    Missing,
    /// Sources or identities disagree.
    Conflict,
    /// Content cannot be parsed or its evidence hash is invalid.
    Damaged,
    /// Shares, prices or settlement cannot support an account operation.
    ExecutionDependency,
    /// A normal check without a problem classification.
    #[default]
    None,
}
/// Handling is separate from the factual check result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemDisposition {
    /// Required dependency remains blocking.
    #[default]
    Block,
    /// An absent feature input was retained as null.
    KeepNull,
    /// A training/validation row was omitted.
    ExcludeSample,
    /// A dated candidate was omitted from the derived universe.
    ExcludeSecurity,
}
impl ProblemDisposition {
    /// Plain-language handling shown beside each reason.
    pub fn label(self) -> &'static str {
        match self {
            Self::Block => "阻断执行",
            Self::KeepNull => "保留为空值",
            Self::ExcludeSample => "排除样本",
            Self::ExcludeSecurity => "排除股票",
        }
    }
}
/// Auditable handling of a source or sample problem over an inclusive date range.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityDecision {
    /// Security identity, or unresolved source selector.
    pub security: String,
    /// Dates affected; never inferred from future returns.
    pub coverage: DateRange,
    /// Stable reason code.
    pub code: String,
    /// Factual problem class.
    pub kind: ProblemKind,
    /// Policy action; this does not turn a failed check into a pass.
    pub disposition: ProblemDisposition,
    /// Human-readable cause.
    pub message: String,
    /// Source evidence or immutable version references.
    pub evidence: Vec<String>,
    /// Selected features affected.
    pub affected_factors: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normal_nulls_and_relaxed_feature_thresholds_have_distinct_semantics() {
        let mut p = ResearchQualityPolicy::default();
        assert!(p.accepts_features(0, 178));
        p.strictness = ResearchStrictness::AllowMissing;
        assert!(!p.accepts_features(0, 178));
        assert!(p.accepts_features(1, 178));
        p.min_valid_feature_ratio = 0.8;
        assert!(p.accepts_features(8, 10));
        assert!(!p.accepts_features(7, 10));
        p.min_labeled_rows = 0;
        assert!(p.validate().is_err());
        p.min_labeled_rows = 1;
        p.min_valid_feature_ratio = f64::NAN;
        assert!(p.validate().is_err());
    }
}
