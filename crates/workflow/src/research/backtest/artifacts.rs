//! Typed publication contracts. Source-specific evidence remains JSON at this boundary.
use super::*;
use analytics_core::{
    admission::{AdmissionCheck, AdmissionReport},
    research_quality::{FactorSet, PriceBasis},
    schedule::ResolvedFold,
    source_map::SourceMap,
};
use serde_json::Value;
pub(super) const TRAINING_SCHEMA_VERSION: u32 = 4;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelReceipt {
    pub fold: usize,
    pub model_sha256: String,
    pub manifest_sha256: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TrainingDescriptor {
    pub schema_version: u32,
    pub runtime_signature: String,
    pub dependency_signature: String,
    pub factor_set: FactorSet,
    pub price_basis: PriceBasis,
    pub status: String,
    pub request: ResearchRequest,
    pub factor_signature: String,
    pub models: Vec<ModelReceipt>,
    pub adjustment_basis_signature: String,
    pub training_signature: String,
    pub source_map: SourceMap,
    pub resolved_folds: Vec<ResolvedFold>,
    pub formula_version: String,
    pub feature_order: Vec<String>,
    pub fit_cache_receipts: BTreeMap<String, String>,
    pub fit_calendar_sha256: String,
    pub source_warnings: Vec<AdmissionCheck>,
    pub admission_report: String,
    pub sample_selection: SampleSelection,
    pub artifact_signature: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SampleSelection {
    pub schema_version: u32,
    pub policy: ResearchQualityPolicy,
    pub selections: Vec<SampleReceipt>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SampleReceipt {
    pub fold: usize,
    pub split: String,
    pub range: DateRange,
    pub path: PathBuf,
    pub sha256: String,
    pub candidate_rows: usize,
    pub retained_rows: usize,
    pub excluded_rows: usize,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PredictionCacheManifest {
    pub schema_version: u32,
    pub signature: String,
    pub rows: usize,
    pub files: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PredictionReceipt {
    pub schema_version: u32,
    pub signature: String,
    pub sha256: String,
    pub rows: usize,
    pub status: String,
    pub cache_hit: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResultSummary {
    pub schema_version: u32,
    pub cross_pool: Value,
    pub unsupported_event_policy: String,
    pub event_simulation_complete: bool,
    pub skipped_event_count: usize,
    pub skipped_events: Vec<Value>,
    pub stages: BTreeMap<String, String>,
    pub execution_diagnostics: Value,
    pub backtest_status: String,
    pub execution_warnings: Vec<Value>,
    pub input_kind: String,
    pub method: String,
    pub fold_reports: Vec<FoldReport>,
    pub annual_reports: Vec<Value>,
    pub aggregate: portfolio::Performance,
    pub source: Value,
    pub formula_version: String,
    pub signature: String,
    pub factor_signature: String,
    pub admission: AdmissionReport,
    pub sample_selection: SampleSelection,
    pub research_scope: String,
    pub execution: Value,
    pub excluded_factors: BTreeMap<String, Vec<String>>,
    pub coverage: Vec<Value>,
}
/// Model annotations are serialized once into the trainer's base manifest.
#[derive(Serialize)]
pub(super) struct ModelAnnotations<'a> {
    pub schema_version: u32,
    pub factor_definitions: Value,
    pub factor_refs: Vec<analytics_factor::selection::FactorRef>,
    pub training_schema_hash: String,
    pub formula_version: &'a str,
    pub input_snapshot_hash: &'a str,
    pub training_signature: &'a str,
    pub research_execution: Value,
    pub rolling_fold: Value,
}
