//! Native, task-scoped input and sample checks shared by App, CLI and execution.
//! A static input pass never substitutes for factor/sample verification.
#![allow(missing_docs)]
use analytics_core::{
    DateRange,
    admission::{AdmissionReport, CheckStatus},
    research_quality::{FactorSet, ResearchQualityPolicy},
};
use analytics_factor::{
    FactorRegistry,
    dependencies::{self, Dependencies},
    selection::FactorRef,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub mod cache;
pub mod daily;
mod source;
pub use source::source_stamp;
pub(crate) mod parquet;
pub use parquet::{CheckRequest, context as factor_context, run};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseRange {
    pub phase: String,
    pub requested: DateRange,
    pub effective: DateRange,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputCheckPlan {
    pub schema_version: u32,
    pub purpose: String,
    pub context: serde_json::Value,
    pub factor_refs: Vec<FactorRef>,
    pub factor_ids: Vec<String>,
    pub formula_version: String,
    pub phases: Vec<PhaseRange>,
    pub dependencies: BTreeMap<String, Dependencies>,
}
impl InputCheckPlan {
    /// Union identical field/window contexts once, before scanning any observations.
    pub fn input_dependencies(&self) -> Dependencies {
        let mut out = Dependencies::new();
        for dependencies in self.dependencies.values() {
            for (input, windows) in dependencies {
                out.entry(input.clone()).or_default().extend(windows);
            }
        }
        for windows in out.values_mut() {
            windows.sort_by_key(|w| {
                (
                    w.recursive,
                    w.cross_sectional,
                    w.context_first,
                    w.context_last,
                    w.first,
                    w.last,
                )
            });
            let mut merged: Vec<dependencies::Window> = vec![];
            for w in windows.drain(..) {
                if let Some(last) = merged.last_mut()
                    && last.recursive == w.recursive
                    && last.cross_sectional == w.cross_sectional
                    && last.context_first == w.context_first
                    && last.context_last == w.context_last
                    && w.first <= last.last.saturating_add(1)
                {
                    last.last = last.last.max(w.last);
                } else {
                    merged.push(w);
                }
            }
            *windows = merged;
        }
        out
    }
    pub fn new(
        set: FactorSet,
        refs: &[FactorRef],
        ids: &[String],
        registry: &FactorRegistry,
        purpose: &str,
        context: serde_json::Value,
        phases: Vec<PhaseRange>,
    ) -> crate::Result<Self> {
        if ids.is_empty() || phases.is_empty() {
            return Err("检查需要因子及任务区间".into());
        }
        let mut dependencies = BTreeMap::new();
        for id in ids {
            let spec = registry
                .get(id)
                .ok_or_else(|| format!("unknown factor {id}"))?;
            dependencies.insert(id.clone(), dependencies::resolve(&spec.expression)?);
        }
        Ok(Self {
            schema_version: 1,
            purpose: purpose.into(),
            context,
            factor_refs: analytics_factor::selection::frozen_refs(set, refs, ids),
            factor_ids: ids.to_vec(),
            formula_version: registry.selection_version(ids)?,
            phases,
            dependencies,
        })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactorCoverage {
    pub factor_id: String,
    pub phase: String,
    pub eligible: usize,
    pub finite: usize,
    pub paired: usize,
    pub coverage: Option<f64>,
    pub empty_intervals: Vec<DateRange>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckStage {
    pub stage: String,
    pub status: CheckStatus,
    pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputCheckReport {
    pub ready: bool,
    pub schema_version: u32,
    pub plan: InputCheckPlan,
    pub context_hash: String,
    pub stages: Vec<CheckStage>,
    pub admission: AdmissionReport,
    pub factors: Vec<FactorCoverage>,
    pub samples: serde_json::Value,
    pub dependency_ranges: serde_json::Value,
    pub missing_counts: BTreeMap<String, usize>,
    pub missing_scope_counts: BTreeMap<String, BTreeMap<String, usize>>,
    pub warnings: Vec<String>,
    pub artifact: Option<cache::Receipt>,
}
impl InputCheckReport {
    pub fn complete(&self) -> bool {
        self.stages.len() == 4 && self.stages.iter().all(|s| s.status == CheckStatus::Passed)
    }
    pub fn ready(&self) -> bool {
        self.complete() && self.admission.ready()
    }
}
pub(crate) fn stages() -> Vec<CheckStage> {
    ["dependencies", "inputs", "factors", "samples"]
        .into_iter()
        .map(|s| CheckStage {
            stage: s.into(),
            status: CheckStatus::NotRun,
            message: String::new(),
        })
        .collect()
}
pub(crate) fn policy_disposition(
    policy: &ResearchQualityPolicy,
) -> analytics_core::research_quality::ProblemDisposition {
    use analytics_core::research_quality::{ProblemDisposition as D, ResearchStrictness as S};
    match policy.strictness {
        S::Strict => D::Block,
        S::AllowMissing => D::KeepNull,
        S::QualifiedOnly => D::ExcludeSecurity,
    }
}
/// Provider transformations: raw reads and additional lag for derived inputs.
pub fn raw_inputs(input: &str) -> Vec<(&str, usize)> {
    match input {
        "ret" => vec![("close", 0), ("close", 1)],
        "hd" => vec![("high", 0), ("high", 1)],
        "ld" => vec![("low", 0), ("low", 1)],
        "tr" => vec![("high", 0), ("low", 0), ("close", 1)],
        "dtm" => vec![("open", 0), ("open", 1), ("high", 0)],
        "dbm" => vec![("open", 0), ("open", 1), ("low", 0)],
        // Dataset VWAP follows the provider field; Parquet derives amount/volume.
        _ => vec![(input, 0)],
    }
}

#[cfg(test)]
mod tests;
