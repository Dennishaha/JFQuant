//! Native annual rolling research using independently acquired daily data.
//! All factor, LightGBM and portfolio work stays on the calling worker thread.
mod account;
mod adjustment;
mod akquant;
mod artifacts;
mod corporate;
mod data;
pub(crate) mod execution;
/// Fit-only models and independent cross-pool prediction/account tasks.
pub mod independent;
mod portfolio;
mod predictions;
/// Shared dated input qualification and audit artifacts.
pub mod quality;
mod report;
mod rights;
mod stages;
/// Read-only verification of completed native research artifacts.
pub mod verification;
use crate::research::{
    DenseMatrix, LightGbmPredictor, LightGbmTrainer, Predictor, Trainer, TrainingSplit,
};
use analytics_core::market_rules::MarketRulesPreset;
use analytics_core::research_quality::{ProblemDisposition, ProblemKind, ResearchQualityPolicy};
use analytics_core::schedule::Schedule;
use analytics_core::{Date, DateRange, Objective, TrainRequest};
use analytics_factor::FactorRegistry;
use analytics_storage::daily::DailyDataset;
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
};
pub(crate) const SUPPORTED_RESEARCH_INPUTS: [&str; 15] = [
    "open",
    "high",
    "low",
    "close",
    "volume",
    "amount",
    "vwap",
    "ret",
    "hd",
    "ld",
    "tr",
    "dtm",
    "dbm",
    "banchmarkindexclose",
    "banchmarkindexopen",
];

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Multi-strategy net account execution.
pub mod combined;

/// Current strict integer-share research result format.
pub const RESEARCH_SCHEMA_VERSION: u32 = 22;

/// A result cannot be interpreted using the current research contract.
#[derive(Debug, thiserror::Error)]
#[error("unsupported_result_format: {0}")]
pub struct UnsupportedResultFormat(pub String);

fn validate_price_source(
    id: &str,
    input: &serde_json::Value,
    normalization: &str,
    synthetic: bool,
) -> Result<()> {
    use analytics_storage::adjustment::{
        AdjustmentAnchor, AdjustmentBasis, AdjustmentPoint, AdjustmentRecord, FactorSource,
        coefficient,
    };
    let normalization = input["normalization"].as_str().unwrap_or(normalization);
    let anchor: AdjustmentAnchor = serde_json::from_value(input["anchor"].clone())?;
    coefficient(&anchor.factor)?;
    if normalization == "qlib-first-stored-factor-v2" {
        if input["basis"] != "qlib_stored_factor"
            || input["anchor_mode"] != "first_stored_factor"
            || input["precision"] != "float32"
        {
            return Err("invalid Qlib price-input metadata".into());
        }
        return Ok(());
    }
    let basis = &input["basis"];
    let adjustment = match basis["kind"].as_str() {
        Some("explicit") => {
            let source: FactorSource = serde_json::from_value(basis["source"].clone())?;
            if (source == FactorSource::SyntheticFixture) != synthetic {
                return Err("coefficient source/research provenance mismatch".into());
            }
            let start: Date = serde_json::from_value(basis["history_start"].clone())?;
            let through: Date = serde_json::from_value(basis["verified_through"].clone())?;
            if basis["point_count"].as_u64().is_none_or(|n| n == 0)
                || start > anchor.trade_date
                || anchor.trade_date > through
            {
                return Err("invalid coefficient coverage/anchor metadata".into());
            }
            AdjustmentBasis::Explicit {
                source,
                source_url: basis["source_url"]
                    .as_str()
                    .ok_or("missing coefficient URL")?
                    .into(),
                verified_through: through,
                // The report carries history metadata and the full record's hash, not all points.
                points: vec![AdjustmentPoint {
                    effective_date: start,
                    factor: anchor.factor,
                }],
            }
        }
        Some("supplier_forward") => {
            if synthetic
                || normalization != analytics_storage::adjustment::HITHINK_NORMALIZATION
                || anchor.factor != "1"
            {
                return Err("supplier price provenance mismatch".into());
            }
            let through: Date = serde_json::from_value(basis["verified_through"].clone())?;
            if through < anchor.trade_date {
                return Err("supplier coverage does not include anchor".into());
            }
            AdjustmentBasis::SupplierForward {
                verified_through: through,
            }
        }
        Some("price_index") => {
            let source = basis["source"]
                .as_str()
                .ok_or("missing price-index source")?;
            if anchor.factor != "1"
                || (source == "JFQuant analytics-workflow synthetic-demo-v1") != synthetic
            {
                return Err("invalid price-index source/anchor".into());
            }
            AdjustmentBasis::PriceIndex {
                source: source.into(),
            }
        }
        _ => return Err("missing/unsupported coefficient metadata".into()),
    };
    AdjustmentRecord {
        index_input: None,
        schema_version: 4,
        instrument_id: id.into(),
        source_symbol: input["source_symbol"].as_str().unwrap_or(id).into(),
        basis: adjustment,
    }
    .validate()?;
    Ok(())
}

/// Validate a completed research result before reporting, importing or displaying it.
/// Historical files are never migrated or interpreted using a different execution mode.
pub fn validate_result(
    value: &serde_json::Value,
) -> std::result::Result<(), UnsupportedResultFormat> {
    let validate = || -> Result<()> {
        if value["kind"] == "combined" {
            if value["schema_version"] != RESEARCH_SCHEMA_VERSION
                || value["execution"]["mode"] != "akquant_ex_date"
                || !matches!(
                    value["backtest_status"].as_str(),
                    Some("completed" | "no_valid_trades")
                )
                || crate::signature(&(
                    value["factor_signature"]
                        .as_str()
                        .ok_or("combined_signature_missing")?,
                    &value["execution"],
                ))? != value["signature"]
            {
                return Err("combined_result_invalid".into());
            }
            return Ok(());
        }

        if value["schema_version"] != RESEARCH_SCHEMA_VERSION
            || value["execution"]["mode"] != "akquant_ex_date"
            || !matches!(
                value["backtest_status"].as_str(),
                Some("completed" | "no_valid_trades" | "execution_data_not_ready")
            )
        {
            return Err("requires current research schema and akquant_ex_date execution".into());
        }
        let hash = |value: &serde_json::Value| {
            value.as_str().is_some_and(|s| {
                s.strip_prefix("sha256:")
                    .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            })
        };
        if !hash(&value["signature"])
            || !hash(&value["factor_signature"])
            || !hash(&value["execution"]["corporate_action_sha256"])
            || value["formula_version"] != {
                let family = serde_json::from_value(value["execution"]["factor_set"].clone())?;
                let refs: Vec<analytics_factor::selection::FactorRef> =
                    value["execution"]["research_config"]
                        .get("factor_refs")
                        .map(|v| serde_json::from_value(v.clone()))
                        .transpose()?
                        .unwrap_or_default();
                let ids: Vec<String> = serde_json::from_value(
                    value["execution"]["research_config"]["factor_ids"].clone(),
                )?;
                if refs.is_empty() {
                    analytics_factor::alpha158::version(family).to_string()
                } else {
                    FactorRegistry::resolve(family, &refs, &ids)?.selection_version(&ids)?
                }
            }
            || !matches!(
                value["input_kind"].as_str(),
                Some("synthetic_fixture" | "public_market")
            )
            || !value["source"].is_object()
            || !value["coverage"].is_array()
            || !value["excluded_factors"].is_object()
            || value["method"].as_str().is_none_or(str::is_empty)
        {
            return Err("missing or invalid research provenance".into());
        }
        let warnings = value["execution_warnings"]
            .as_array()
            .ok_or("missing execution warnings")?;
        let skipped = value["skipped_events"]
            .as_array()
            .ok_or("missing skipped events")?;
        if value["unsupported_event_policy"] != "block_unprojected_events"
            || value["event_simulation_complete"] != warnings.is_empty()
            || value["skipped_event_count"] != skipped.len()
            || skipped
                != &warnings
                    .iter()
                    .filter(|w| w["code"] == "execution_event_unsupported_skipped")
                    .cloned()
                    .collect::<Vec<_>>()
        {
            return Err("invalid unsupported event disclosure".into());
        }
        let execution = &value["execution"];
        if execution["unsupported_event_policy"] != "block_unprojected_events"
            || execution["engine"] != "akquant"
            || execution["engine_version"] != "0.3.66"
            || execution["corporate_action_policy"] != "ex_date"
            || !hash(&execution["runtime_signature"])
            || !hash(&execution["dependency_signature"])
        {
            return Err("missing AKQuant execution receipt".into());
        }

        let _: analytics_core::execution::EventCheckPolicy =
            serde_json::from_value(execution["event_check_policy"].clone())?;
        let policy: ResearchQualityPolicy =
            serde_json::from_value(execution["quality_policy"].clone())?;
        policy.validate()?;
        let qualification: quality::QualitySnapshot =
            serde_json::from_value(execution["quality"].clone())?;
        let admission: analytics_core::admission::AdmissionReport =
            serde_json::from_value(value["admission"].clone())?;
        let price_basis: analytics_core::research_quality::PriceBasis =
            serde_json::from_value(execution["price_basis"].clone())?;
        if admission.price_basis != price_basis {
            return Err("admission/price basis mismatch".into());
        }
        if qualification.schema_version != 1
            || qualification.policy != policy
            || admission.policy != policy
            || !admission.ready()
            || execution["quality_sha256"]
                != format!(
                    "sha256:{:x}",
                    analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&qualification)?))
                )
            || execution["sample_selection_sha256"]
                != format!(
                    "sha256:{:x}",
                    analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(
                        &execution["sample_selection"]
                    )?))
                )
            || value["sample_selection"] != execution["sample_selection"]
            || serde_json::to_value(&qualification.effective_pool)? != execution["pool"]
            || value["research_scope"]
                != if policy.strictness
                    == analytics_core::research_quality::ResearchStrictness::QualifiedOnly
                {
                    "经数据筛选的研究股票池"
                } else {
                    "原股票池"
                }
        {
            return Err("invalid research quality policy/masks/admission signature".into());
        }
        qualification.original_pool.coverage.validate()?;
        for (selector, ranges) in &qualification.original_pool.intervals {
            if selector.is_empty()
                || selector.len() > 64
                || !selector
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                || ranges.iter().any(|r| {
                    r.start > r.end
                        || r.start < qualification.original_pool.coverage.start
                        || r.end > qualification.original_pool.coverage.end
                })
            {
                return Err("invalid original source membership".into());
            }
        }
        for counts in &qualification.daily_counts {
            let date: Date = serde_json::from_value(counts["date"].clone())?;
            let original = qualification.original_pool.on(date);
            let retained = qualification.effective_pool.on(date);
            if !retained.is_subset(&original)
                || counts["original"] != original.len()
                || counts["retained"] != retained.len()
            {
                return Err("invalid daily qualification counts".into());
            }
        }
        if policy.strictness != analytics_core::research_quality::ResearchStrictness::QualifiedOnly
            && serde_json::to_value(&qualification.original_pool)?
                != serde_json::to_value(&qualification.effective_pool)?
        {
            return Err("sample/null policies must preserve the original universe".into());
        }
        let source_map: analytics_core::source_map::SourceMap =
            serde_json::from_value(execution["source_map"].clone())?;
        source_map.validate()?;
        if execution["source_map_signature"]
            != format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&source_map)?))
            )
        {
            return Err("source-map signature mismatch".into());
        }
        if !hash(&execution["price_input_sha256"])
            || execution["price_input_sha256"]
                != format!(
                    "sha256:{:x}",
                    analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(
                        &execution["price_inputs"]
                    )?))
                )
            || !matches!(
                execution["price_inputs"]["normalization"].as_str(),
                Some(
                    "qlib-first-stored-factor-v2"
                        | "explicit-hfq-first-raw-close-v1;shares;cny;nulls"
                        | "hithink-forward-ohlc-v1;shares;cny;vwap-null"
                        | "per-security-price-volume-v1"
                )
            )
            || execution["price_inputs"]["securities"]
                .as_object()
                .is_none_or(|m| m.is_empty())
        {
            return Err("missing or invalid price-input provenance".into());
        }
        for (id, input) in execution["price_inputs"]["securities"]
            .as_object()
            .ok_or("missing price inputs")?
        {
            analytics_core::pool::validate_instrument(id)?;
            if !hash(&input["sha256"]) {
                return Err("invalid coefficient source hash".into());
            }
            if input["source"].is_object() {
                let mapping: analytics_core::source_map::SourceException =
                    serde_json::from_value(input["source_mapping"].clone())?;
                let receipt = serde_json::json!({"source_mapping":input["source_mapping"],"bindings":input["bindings"],"source":input["source"]});
                if input["sha256"]
                    != format!(
                        "sha256:{:x}",
                        analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&receipt)?))
                    )
                {
                    return Err("source-map receipt mismatch".into());
                }
                let sources = input["source"]["securities"]
                    .as_object()
                    .ok_or("missing bound source inputs")?;
                let bindings: Vec<analytics_core::source_map::SourceLocator> =
                    serde_json::from_value(input["bindings"].clone())?;
                if bindings.is_empty()
                    || bindings
                        .iter()
                        .any(|l| !mapping.locators.contains(l) || !sources.contains_key(&l.symbol))
                {
                    return Err("bound source locator mismatch".into());
                }
                for (symbol, source) in sources {
                    if !hash(&source["sha256"]) {
                        return Err("invalid original coefficient hash".into());
                    }
                    validate_price_source(
                        symbol,
                        source,
                        input["source"]["normalization"]
                            .as_str()
                            .ok_or("missing source normalization")?,
                        value["input_kind"] == "synthetic_fixture",
                    )?;
                }
            } else {
                validate_price_source(
                    id,
                    input,
                    execution["price_inputs"]["normalization"]
                        .as_str()
                        .ok_or("missing normalization")?,
                    value["input_kind"] == "synthetic_fixture",
                )?;
            }
        }

        if !hash(&execution["pool_signature"]) {
            return Err("missing pool signature".into());
        }
        let pool: analytics_core::pool::PoolMembership =
            serde_json::from_value(execution["pool"].clone())?;
        pool.validate()?;
        if execution["pool_signature"]
            != format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&pool)?))
            )
        {
            return Err("pool signature mismatch".into());
        }
        analytics_core::pool::validate_instrument(
            execution["benchmark"].as_str().ok_or("missing benchmark")?,
        )?;
        let _: analytics_core::pool::PoolExitPolicy =
            serde_json::from_value(execution["pool_exit_policy"].clone())?;
        if execution.get("fractional_adjusted_shares").is_some()
            || execution.get("annual_cash_reset").is_some()
            || execution.get("annual_terminal_sale").is_some()
            || execution["fee_application"] != "latest_only_for_all_dates"
            || execution["initial_cash_cny"]
                .as_f64()
                .is_none_or(|v| !v.is_finite() || v <= 0.0 || v > 1e13)
            || execution["dividend_withholding_rate"]
                .as_f64()
                .is_none_or(|v| !v.is_finite() || !(0.0..=1.0).contains(&v))
        {
            return Err("missing or invalid strict execution settings".into());
        }
        let rules: MarketRulesPreset = serde_json::from_value(execution["market_rules"].clone())?;
        rules.validate()?;
        let config: ResearchConfig = serde_json::from_value(execution["research_config"].clone())?;
        config.validate()?;
        let _: portfolio::Performance = serde_json::from_value(value["aggregate"].clone())?;
        if !value["annual_reports"].is_array() {
            return Err("missing annual summaries".into());
        }
        let schedule: Schedule = serde_json::from_value(execution["schedule"].clone())?;
        schedule.validate()?;
        let policy: analytics_core::actions::ActionPolicy =
            serde_json::from_value(execution["action_policy"].clone())?;
        if policy.exercise_optional_cash_choice {
            return Err("unsupported active cash-choice report".into());
        }
        let frozen: Vec<analytics_core::schedule::ResolvedFold> =
            serde_json::from_value(execution["resolved_folds"].clone())?;
        for f in &frozen {
            f.requested.validate()?;
            f.actual.validate()?;
        }
        if frozen
            .windows(2)
            .any(|w| w[0].actual.test.end >= w[1].actual.test.start)
        {
            return Err("overlapping frozen folds".into());
        }
        if frozen.is_empty() {
            return Err("missing frozen date schedule".into());
        }
        let selection = &execution["sample_selection"];
        if selection["schema_version"] != 1
            || selection["policy"] != serde_json::to_value(&admission.policy)?
        {
            return Err("unsupported or inconsistent sample selection".into());
        }
        let masks = selection["selections"]
            .as_array()
            .ok_or("missing sample mask manifests")?;
        let independent = execution["independent"]["contract"] == "independent-v1";
        if independent && value["schema_version"] != RESEARCH_SCHEMA_VERSION {
            return Err("independent execution requires current result schema".into());
        }
        let expected: usize = frozen
            .iter()
            .map(|f| {
                if independent {
                    1
                } else if f.actual.validation.is_some() {
                    3
                } else {
                    2
                }
            })
            .sum();
        if masks.len() != expected {
            return Err("incomplete sample mask manifests".into());
        }
        if independent {
            independent::validate_execution(&execution["independent"], &frozen)?;
        }
        let mut seen = BTreeSet::new();
        for mask in masks {
            let index = mask["fold"].as_u64().ok_or("missing mask fold")? as usize;
            let split = mask["split"].as_str().ok_or("missing mask split")?;
            if independent && split != "prediction" {
                return Err("independent backtest must only contain prediction masks".into());
            }
            let fold = frozen.get(index).ok_or("mask fold outside schedule")?;
            let range = match split {
                "train" => Some(&fold.actual.train),
                "validation" => fold.actual.validation.as_ref(),
                "prediction" => Some(&fold.actual.test),
                _ => None,
            }
            .ok_or("unsupported sample mask split")?;
            let candidates = mask["candidate_rows"]
                .as_u64()
                .ok_or("missing mask candidate count")?;
            let retained = mask["retained_rows"]
                .as_u64()
                .ok_or("missing mask retained count")?;
            if !seen.insert((index, split))
                || mask["range"] != serde_json::to_value(range)?
                || retained > candidates
                || mask["excluded_rows"] != candidates - retained
                || (split != "prediction" && retained < admission.policy.min_labeled_rows as u64)
                || !hash(&mask["sha256"])
                || mask["path"] != format!("sample_masks/fold-{index:04}-{split}.json")
            {
                return Err("invalid sample mask manifest/count/range".into());
            }
        }
        let reports: Vec<FoldReport> = serde_json::from_value(value["fold_reports"].clone())?;
        if reports.len() != frozen.len()
            || reports.is_empty()
            || reports
                .iter()
                .any(|r| r.performance.as_ref().is_none_or(|p| p.observations == 0))
            || reports.windows(2).any(|r| r[0].test_end >= r[1].test_start)
        {
            return Err("missing or unordered annual reports".into());
        }
        Ok(())
    };
    validate().map_err(|e| UnsupportedResultFormat(e.to_string()))
}

/// Reusable annual research parameters used by the current research template.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResearchConfig {
    /// Explicit account allocation contract.
    pub allocation: crate::allocation::AllocationConfig,
    /// Ranked portfolio target count.
    pub top_n: usize,
    /// Trading days between target rebalances.
    pub rebalance_days: usize,
    /// Ordered explicit factor IDs; empty selects available research factors.
    pub factor_ids: Vec<String>,
    /// Frozen per-factor definitions; absent in legacy builtin recipes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub factor_refs: Vec<analytics_factor::selection::FactorRef>,
    /// Native training parameters and early stopping.
    pub training: crate::research::TrainingConfig,
}
impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            allocation: Default::default(),
            top_n: 50,
            rebalance_days: 5,
            factor_ids: Vec::new(),
            factor_refs: Vec::new(),
            training: crate::research::TrainingConfig {
                parameters: BTreeMap::from([
                    ("num_iterations".into(), "200".into()),
                    ("learning_rate".into(), "0.05".into()),
                    ("num_leaves".into(), "31".into()),
                    ("min_data_in_leaf".into(), "200".into()),
                    ("feature_fraction".into(), "0.8".into()),
                    ("lambda_l2".into(), "10".into()),
                    ("seed".into(), "17".into()),
                    ("num_threads".into(), "4".into()),
                    ("deterministic".into(), "true".into()),
                    ("force_col_wise".into(), "true".into()),
                ]),
                early_stopping_rounds: Some(20),
            },
        }
    }
}
impl ResearchConfig {
    /// Validate bounds before allocating matrices or starting native work.
    pub fn validate(&self) -> Result<()> {
        self.allocation.validate_count(self.top_n)?;
        if !(1..=1000).contains(&self.top_n) || !(1..=252).contains(&self.rebalance_days) {
            return Err("invalid rolling window/portfolio parameters".into());
        }
        for key in [
            "num_iterations",
            "num_leaves",
            "min_data_in_leaf",
            "num_threads",
        ] {
            let value = self
                .training
                .parameters
                .get(key)
                .ok_or("missing required training parameter")?
                .parse::<u32>()?;
            if value == 0 || (key == "num_leaves" && value < 2) {
                return Err("invalid integer training parameter".into());
            }
        }
        for key in ["learning_rate", "feature_fraction", "lambda_l2"] {
            let v = self
                .training
                .parameters
                .get(key)
                .ok_or("missing required training parameter")?
                .parse::<f64>()?;
            if !v.is_finite()
                || v < 0.0
                || (key != "lambda_l2" && v == 0.0)
                || (key == "feature_fraction" && v > 1.0)
            {
                return Err("invalid real training parameter".into());
            }
        }
        if self.training.early_stopping_rounds == Some(0) {
            return Err("early stopping must be positive or disabled".into());
        }
        Ok(())
    }
}

/// Reproducible calendar-year rolling research parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResearchRequest {
    /// Explicit published predictions; this path never loads a model or runs a trainer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reuse_predictions: Option<PathBuf>,
    /// Explicit rights experiment; None preserves the historical skip contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rights_policy: Option<analytics_core::rights::Policy>,
    /// Explicit account-only research transition assumptions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corporate_event_policy: Option<analytics_core::corporate_account::Policy>,
    /// Frozen feature family.
    pub factor_set: analytics_core::research_quality::FactorSet,
    /// Explicit training source trust policy.
    pub price_basis: analytics_core::research_quality::PriceBasis,
    /// Frozen completeness and sample policy.
    pub quality_policy: ResearchQualityPolicy,
    /// Rolling/model/portfolio parameters.
    pub config: ResearchConfig,
    /// Stop after publishing independently reusable fold models.
    pub train_only: bool,
    /// Existing completed training output; never calls the trainer when present.
    pub reuse_training: Option<PathBuf>,
    /// Explicit independently downloaded qlib_bin directory.
    pub data_root: PathBuf,
    /// Ignored local research output directory.
    pub output: PathBuf,
    /// Explicit date-level model and execution schedule.
    pub schedule: Schedule,
    /// Explicit ordinary-holder corporate action policy.
    pub action_policy: analytics_core::actions::ActionPolicy,
    /// Trust policy for actual account execution data. Training format 3 carries no required execution choice.
    #[serde(default)]
    pub event_check_policy: analytics_core::execution::EventCheckPolicy,
    /// Inspect provenance and coverage without calculating factors.
    pub audit_only: bool,
    /// Latest-only public market fees and user broker contract.
    pub market_rules: MarketRulesPreset,
    /// Cash in CNY, needed for per-order minimum commissions.
    pub initial_cash_cny: f64,
    /// Independently downloaded, announcement-completed action bundle.
    pub corporate_actions: Option<PathBuf>,
    /// Fixed research dividend withholding; default 0 (gross), not personal tax advice.
    pub dividend_withholding_rate: f64,
    /// Effective pool source for the dataset when no composed membership was supplied.
    pub universe_id: String,
    /// Frozen resolved historical membership.
    pub pool: Option<analytics_core::pool::PoolMembership>,
    /// Frozen pool signature, included in caches and results.
    pub pool_signature: String,
    /// Explicit benchmark security.
    pub benchmark: String,
    /// Removal behavior for existing positions.
    pub pool_exit_policy: analytics_core::pool::PoolExitPolicy,
}
/// Account-only overrides for replaying immutable trained models.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResearchExecution {
    /// Explicit account allocation contract.
    pub allocation: crate::allocation::AllocationConfig,
    /// Explicit corporate event assumptions, independent of fitting.
    pub corporate_event_policy: analytics_core::corporate_account::Policy,
    /// Initial account cash in CNY.
    pub initial_cash_cny: f64,
    /// Supported broker and exchange fee settings.
    pub market_rules: MarketRulesPreset,
    /// Maximum selected securities.
    pub top_n: usize,
    /// Trading sessions between selection dates.
    pub rebalance_days: usize,
    /// Treatment of securities leaving the pool.
    pub pool_exit_policy: analytics_core::pool::PoolExitPolicy,
}
impl ResearchRequest {
    /// Resolve definitions without reading the mutable workspace library.
    pub fn factor_registry(&self) -> Result<FactorRegistry> {
        Ok(FactorRegistry::resolve(
            self.factor_set,
            &self.config.factor_refs,
            &self.config.factor_ids,
        )?)
    }
    /// Required history is the maximum over all frozen factors.
    pub fn warmup_sessions(&self) -> usize {
        analytics_factor::selection::warmup_sessions(self.factor_set, &self.config.factor_refs)
    }

    /// Defaults to five training years, one validation year and tests from 2013.
    pub fn new(data_root: PathBuf, output: PathBuf) -> Self {
        Self {
            reuse_predictions: None,
            rights_policy: None,
            corporate_event_policy: None,
            quality_policy: ResearchQualityPolicy::default(),
            factor_set: Default::default(),
            price_basis: Default::default(),
            config: ResearchConfig::default(),
            train_only: false,
            reuse_training: None,
            data_root,
            output,
            schedule: Schedule::default(),
            action_policy: Default::default(),
            event_check_policy: Default::default(),
            audit_only: false,
            market_rules: MarketRulesPreset::a_share_latest(),
            initial_cash_cny: 1_000_000.0,
            corporate_actions: None,
            dividend_withholding_rate: 0.0,
            universe_id: "csi500".into(),
            pool: None,
            pool_signature: String::new(),
            benchmark: "SH000905".into(),
            pool_exit_policy: Default::default(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FoldReport {
    year: i32,
    train_start: String,
    train_end: String,
    valid_start: String,
    valid_end: String,
    test_start: String,
    test_end: String,
    train_rows: usize,
    validation_rows: usize,
    test_rows: usize,
    features: usize,
    rank_ic: Option<f64>,
    ic: Option<f64>,
    performance: Option<portfolio::Performance>,
}

fn save(path: impl AsRef<Path>, value: &impl Serialize) -> Result<()> {
    analytics_core::task::TaskContext::current().check()?;
    let path = path.as_ref();
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

/// Run fresh JFQuant factor calculation, yearly LightGBM fitting and portfolio evaluation.
/// Produces provenance, coverage, fold models, predictions and return reports.
pub fn run(request: &ResearchRequest) -> Result<()> {
    run_with_context(request, &analytics_core::task::TaskContext::current())
}
/// Run with persistent desktop progress and cooperative cancellation.
pub fn run_with_context(
    request: &ResearchRequest,
    context: &analytics_core::task::TaskContext,
) -> Result<()> {
    let result = context.execute(|| crate::research::worker::scoped(|| run_scoped(request)));
    // Persist stage truth even after cooperative cancellation; never publish summary here.
    if request.output.is_dir() {
        let readiness = fs::read(request.output.join("execution_readiness.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .unwrap_or_default();
        let account_status = if request.output.join("summary.json").is_file() {
            "completed"
        } else if request.train_only || request.audit_only {
            "not_started"
        } else if context.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            "cancelled"
        } else if readiness["stages"]["backtest"] == "blocked" {
            "blocked"
        } else if readiness["stages"]["backtest"] == "failed"
            || request.output.join("prediction_manifest.json").is_file()
        {
            "failed"
        } else {
            "not_started"
        };
        let stages = serde_json::json!({"schema_version":1,
            "training":if request.output.join("training.json").is_file() || request.reuse_training.is_some(){"completed"}else if request.output.join("fold-0000/model.txt").is_file(){"incomplete"}else{"not_started"},
            "prediction":if request.output.join("prediction_manifest.json").is_file(){"completed"}else if request.output.join("fold_reports.json").is_file(){"incomplete"}else{"not_started"},
            "backtest":account_status});
        let part = request.output.join("stages.json.partial");
        fs::write(&part, serde_json::to_vec_pretty(&stages)?)?;
        fs::rename(part, request.output.join("stages.json"))?;
    }
    result
}

fn file_hash(path: &Path) -> Result<String> {
    Ok(format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(fs::read(path)?))
    ))
}
fn adjustment_basis_signature(
    book: Option<&analytics_storage::corporate_actions::ActionBundle>,
    start: Date,
    end: Date,
) -> Result<String> {
    let events:Vec<_>=book.into_iter().flat_map(|b|&b.actions).filter(|a|a.ex_date>=start&&a.ex_date<=end).map(|a|serde_json::json!({"instrument":a.instrument,"ex_date":a.ex_date,"cash":a.cash(),"shares":a.shares(),"reference":a.terms.reference()})).collect();
    Ok(format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&events)?))
    ))
}
/// Validate current training descriptor on every read; unsupported files are never converted.
pub fn validate_training_artifact(artifact: &serde_json::Value) -> Result<()> {
    if artifact["schema_version"] != artifacts::TRAINING_SCHEMA_VERSION
        || artifact["runtime_signature"] != crate::research::worker::training_runtime_signature()
        || artifact["dependency_signature"]
            != crate::research::worker::training_dependency_signature()
        || artifact["status"] != "trained"
    {
        return Err("unsupported_training_format".into());
    }
    let _: artifacts::TrainingDescriptor = serde_json::from_value(artifact.clone())?;
    let mut unsigned = artifact.clone();
    let receipt = unsigned
        .as_object_mut()
        .ok_or("invalid training artifact")?
        .remove("artifact_signature")
        .ok_or("training_artifact_damaged")?;
    if receipt
        != format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&unsigned)?))
        )
    {
        return Err("training_artifact_damaged".into());
    }
    let frozen_request: ResearchRequest = serde_json::from_value(artifact["request"].clone())?;
    let frozen_version = frozen_request
        .factor_registry()?
        .selection_version(&frozen_request.config.factor_ids)?;
    if artifact["formula_version"] != frozen_version
        || artifact["feature_order"]
            .as_array()
            .is_none_or(|ids| ids.is_empty() || ids.iter().any(|id| id.as_str().is_none()))
        || artifact["models"].as_array().is_none_or(Vec::is_empty)
    {
        return Err("training_artifact_damaged".into());
    }
    let request: ResearchRequest = serde_json::from_value(artifact["request"].clone())?;
    let family = serde_json::from_value::<analytics_core::research_quality::FactorSet>(
        artifact["factor_set"].clone(),
    )?;
    let basis = serde_json::from_value::<analytics_core::research_quality::PriceBasis>(
        artifact["price_basis"].clone(),
    )?;
    let features: Vec<String> = serde_json::from_value(artifact["feature_order"].clone())?;
    let registry = request.factor_registry()?;
    let mut seen = BTreeSet::new();
    if family != request.factor_set
        || basis != request.price_basis
        || features != request.config.factor_ids
        || features
            .iter()
            .any(|id| !seen.insert(id) || registry.get(id).is_none())
    {
        return Err("training_artifact_family_or_order_mismatch".into());
    }
    Ok(())
}
/// Execute with immutable previously trained models. Settlement fixes do not call training again.
pub fn backtest_from_training(
    source: &Path,
    output: &Path,
    actions: &Path,
    context: &analytics_core::task::TaskContext,
) -> Result<()> {
    backtest_from_training_with_data(source, output, actions, None, context)
}
/// Resolve relocated workspaces against their verified market version, without editing artifacts.
pub fn backtest_from_training_with_data(
    source: &Path,
    output: &Path,
    actions: &Path,
    data_root: Option<&Path>,
    context: &analytics_core::task::TaskContext,
) -> Result<()> {
    backtest_from_training_with_policy(source, output, actions, data_root, None, None, context)
}
/// Apply an execution-only policy without changing the frozen training manifest or model.
pub fn backtest_from_training_with_policy(
    source: &Path,
    output: &Path,
    actions: &Path,
    data_root: Option<&Path>,
    policy: Option<analytics_core::execution::EventCheckPolicy>,
    execution: Option<&ResearchExecution>,
    context: &analytics_core::task::TaskContext,
) -> Result<()> {
    let artifact: serde_json::Value =
        serde_json::from_slice(&fs::read(source.join("training.json"))?)?;
    validate_training_artifact(&artifact)?;
    let mut request: ResearchRequest = serde_json::from_value(artifact["request"].clone())?;
    if let Some(root) = data_root {
        request.data_root = root.to_owned();
    }
    let dataset = DailyDataset::open(&request.data_root)?;
    let folds = request
        .schedule
        .resolve_with_warmup(&dataset.calendar, request.warmup_sessions())?;
    let models = artifact["models"]
        .as_array()
        .ok_or("missing trained models")?;
    if models.len() != folds.len() || artifact["resolved_folds"] != serde_json::to_value(&folds)? {
        return Err("training_artifact_damaged: incomplete fold models".into());
    }
    for (expected, m) in models.iter().enumerate() {
        let i = m["fold"].as_u64().ok_or("missing fold")?;
        if i as usize != expected {
            return Err("training_artifact_damaged: duplicate or misplaced fold".into());
        }
        let p = source.join(format!("fold-{i:04}"));
        if m["model_sha256"] != file_hash(&p.join("model.txt"))?
            || m["manifest_sha256"] != file_hash(&p.join("manifest.json"))?
        {
            return Err("training_artifact_damaged".into());
        }
    }
    let lo = folds.iter().map(|f| f.warmup_start).min().unwrap();
    let hi = folds
        .iter()
        .map(|f| f.actual.validation.as_ref().unwrap_or(&f.actual.train).end)
        .max()
        .unwrap();
    let book = analytics_storage::corporate_actions::ActionBundle::read(actions)?;
    if request.price_basis == analytics_core::research_quality::PriceBasis::Verified
        && artifact["adjustment_basis_signature"]
            != adjustment_basis_signature(Some(&book), lo, hi)?
    {
        return Err("training_price_basis_changed: retraining required".into());
    }
    if let Some(policy) = policy {
        request.event_check_policy = policy;
    }
    if let Some(execution) = execution {
        request.initial_cash_cny = execution.initial_cash_cny;
        request.market_rules = execution.market_rules.clone();
        request.config.allocation = execution.allocation.clone();
        request.config.top_n = execution.top_n;
        request.config.rebalance_days = execution.rebalance_days;
        request.pool_exit_policy = execution.pool_exit_policy;
    }
    request.output = output.to_owned();
    request.reuse_training = Some(source.to_owned());
    request.corporate_actions = Some(actions.to_owned());
    request.train_only = false;
    run_with_context(&request, context)
}
/// Execute only a completed task's published predictions; no model is loaded.
pub fn backtest_from_predictions(
    source: &Path,
    output: &Path,
    actions: &Path,
    rights_policy: analytics_core::rights::Policy,
    allocation: Option<&crate::allocation::AllocationConfig>,
    context: &analytics_core::task::TaskContext,
) -> Result<()> {
    if source.join("model_selection.json").is_file() {
        return independent::replay_predictions(
            source,
            output,
            actions,
            rights_policy,
            allocation,
            context,
        );
    }
    let (training_root, _) = predictions::training_source(source)?;
    let training: serde_json::Value =
        serde_json::from_slice(&fs::read(training_root.join("training.json"))?)?;
    let mut request: ResearchRequest = serde_json::from_value(training["request"].clone())?;
    let execution: serde_json::Value =
        serde_json::from_slice(&fs::read(source.join("execution.json"))?)?;
    if execution["market_content_sha256"] != crate::market_content_hash(&request.data_root)? {
        return Err("prediction market identity mismatch".into());
    }
    request.config = serde_json::from_value(execution["research_config"].clone())?;
    if let Some(c) = allocation {
        c.validate_count(request.config.top_n)?;
        request.config.allocation = c.clone();
    }
    request.market_rules = serde_json::from_value(execution["market_rules"].clone())?;
    request.initial_cash_cny = execution["initial_cash_cny"]
        .as_f64()
        .ok_or("prediction cash missing")?;
    request.pool_exit_policy = serde_json::from_value(execution["pool_exit_policy"].clone())?;
    request.event_check_policy = serde_json::from_value(execution["event_check_policy"].clone())?;
    request.reuse_training = None;
    request.reuse_predictions = Some(source.to_owned());
    request.rights_policy = Some(rights_policy);
    request.output = output.to_owned();
    request.corporate_actions = Some(actions.to_owned());
    request.train_only = false;
    run_with_context(&request, context)
}

fn run_scoped(request: &ResearchRequest) -> Result<()> {
    stages::run(request, None, &analytics_core::task::TaskContext::current())
}

#[allow(clippy::too_many_arguments)]
fn execute_account(
    request: &ResearchRequest,
    dataset: &DailyDataset,
    action_book: Option<&analytics_storage::corporate_actions::ActionBundle>,
    pool: &analytics_core::pool::PoolMembership,
    folds: &[analytics_core::schedule::ResolvedFold],
    signals: Vec<(Date, String, f64)>,
    mut reports: Vec<FoldReport>,
    _run_signature: &str,
    signature: &str,
) -> Result<()> {
    let context = analytics_core::task::TaskContext::current();
    context.report("AKQuant 连续账户回测")?;
    let signal_dates = dataset
        .calendar
        .iter()
        .filter(|d| {
            folds
                .iter()
                .any(|f| **d >= f.actual.test.start && **d <= f.actual.test.end)
        })
        .copied()
        .collect::<Vec<_>>();
    let all_returns = match akquant::simulate(
        request,
        dataset,
        action_book,
        &signals,
        &DateRange {
            start: request
                .config
                .allocation
                .execution_start
                .unwrap_or(folds.first().unwrap().actual.test.start),
            end: folds.last().unwrap().actual.test.end,
        },
        pool,
        &signal_dates,
    ) {
        Ok(days) => days,
        Err(error) => {
            save(
                request.output.join("execution_readiness.json"),
                &serde_json::json!({"schema_version":6,"ready":false,"training_complete":true,"prediction_complete":true,"backtest_complete":false,"stages":{"training":"completed","prediction":"completed","backtest":"failed"},"aggregate":null,"reason":error.to_string()}),
            )?;
            return Err(error);
        }
    };
    for (i, report) in reports.iter_mut().enumerate() {
        fs::create_dir_all(request.output.join(format!("fold-{i:04}")))?;
        let days = all_returns
            .iter()
            .filter(|d| d.date >= report.test_start && d.date <= report.test_end)
            .cloned()
            .collect::<Vec<_>>();
        report.performance = Some(portfolio::performance(&days));
        save(
            request.output.join(format!("fold-{i:04}/daily.json")),
            &days,
        )?;
        save(
            request.output.join(format!("fold-{i:04}/report.json")),
            &report,
        )?;
    }
    save(request.output.join("fold_reports.json"), &reports)?;
    crate::allocation::publish_artifacts(&request.output)?;
    let mut audit: serde_json::Value =
        serde_json::from_slice(&fs::read(request.output.join("data_audit.json"))?)?;
    audit["execution"]["allocation_components"] = crate::allocation::component_signatures();
    audit["execution"]["allocation_display"] = crate::allocation::display(&request.output)?;
    audit["execution"]["allocation_artifacts"] =
        serde_json::from_slice(&fs::read(request.output.join("allocation-manifest.json"))?)?;
    let run_signature = crate::signature(&(signature, &audit["execution"]))?;
    audit["research_signature"] = run_signature.clone().into();
    save(request.output.join("execution.json"), &audit["execution"])?;
    save(request.output.join("data_audit.json"), &audit)?;
    finish(
        request,
        &reports,
        &all_returns,
        &run_signature,
        signature,
        &dataset.calendar,
    )?;
    render_report(&request.output)?;
    Ok(())
}

fn save_selection(
    request: &ResearchRequest,
    admission: &mut analytics_core::admission::AdmissionReport,
    fold: usize,
    split: &str,
    range: &DateRange,
    selection: &data::SelectionMask,
) -> Result<serde_json::Value> {
    let relative = format!("sample_masks/fold-{fold:04}-{split}.json");
    let mask = serde_json::json!({"schema_version":1,"fold":fold,"split":split,"range":range,"row_order_sha256":selection.row_order_sha256,"row_mask":selection.row_mask});
    save(request.output.join(&relative), &mask)?;
    admission.decisions.extend(selection.excluded.clone());
    Ok(
        serde_json::json!({"fold":fold,"split":split,"range":range,"path":relative,"sha256":format!("sha256:{:x}",analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&mask)?))),"candidate_rows":selection.row_mask.len(),"retained_rows":selection.retained_rows,"excluded_rows":selection.row_mask.len()-selection.retained_rows}),
    )
}

fn finish(
    request: &ResearchRequest,
    reports: &[FoldReport],
    days: &[portfolio::DayResult],
    run_signature: &str,
    factor_signature: &str,
    calendar: &[Date],
) -> Result<()> {
    let audit: serde_json::Value =
        serde_json::from_slice(&fs::read(request.output.join("data_audit.json"))?)?;
    save(request.output.join("daily.json"), &days)?;
    let aggregate = portfolio::performance(days);
    let mut annual_days = BTreeMap::<String, Vec<portfolio::DayResult>>::new();
    for day in days {
        annual_days
            .entry(day.date[..4].into())
            .or_default()
            .push(day.clone());
    }
    let annual:Vec<_> = annual_days.iter().map(|(year,ds)| {
        let y=year.parse::<i32>().unwrap();
        let first=calendar.iter().find(|d|d.year()==y).unwrap();
        let last=calendar.iter().rev().find(|d|d.year()==y).unwrap();
        let partial=ds.first().unwrap().date!=first.to_string() || ds.last().unwrap().date!=last.to_string() || (calendar.last().unwrap().year()==y && *last<Date::from_ymd_opt(y,12,31).unwrap());
        serde_json::json!({"year":y,"performance":portfolio::performance(ds),"partial_year":partial})
    }).collect();
    save(request.output.join("annual_reports.json"), &annual)?;
    let mut admission: analytics_core::admission::AdmissionReport =
        serde_json::from_slice(&fs::read(request.output.join("admission.json"))?)?;
    for check in &mut admission.checks {
        if check.stage == analytics_core::admission::AdmissionStage::Account {
            check.status = analytics_core::admission::CheckStatus::Passed;
            check.code = "account_path_verified".into();
            check.message =
                "continuous account execution and held-position valuation completed".into();
        }
    }
    save(request.output.join("admission.json"), &admission)?;
    let has_trades = days.iter().any(|d| d.turnover > 0.);
    let events = fs::read_to_string(request.output.join("order_events.jsonl"))?;
    let events = events
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut diagnostics = serde_json::json!({"valid_signals":reports.iter().map(|r|r.test_rows).sum::<usize>(),"filled_events":events.iter().filter(|e|e["status"]=="filled").count(),"rejected_events":events.iter().filter(|e|e["status"]=="rejected").count(),"submitted_orders":null,"reason":if has_trades{"filled"}else if events.iter().any(|e|e["status"]=="rejected"){"orders_rejected"}else if reports.iter().all(|r|r.test_rows==0){"no_valid_signals"}else{"no_fills"}});

    if request.output.join("corporate_event_report.json").is_file() {
        diagnostics["corporate_events"] = serde_json::from_slice(&fs::read(
            request.output.join("corporate_event_report.json"),
        )?)?;
    }
    let mut warnings: Vec<serde_json::Value> =
        if request.output.join("execution_warnings.json").exists() {
            serde_json::from_slice(&fs::read(request.output.join("execution_warnings.json"))?)?
        } else {
            vec![]
        };
    let snapshots: serde_json::Value =
        serde_json::from_slice(&fs::read(request.output.join("portfolio_snapshots.json"))?)?;
    for warning in &mut warnings {
        if warning["code"] == "execution_event_unsupported_skipped" {
            let quantity = warning["date"].as_str().and_then(|date| {
                snapshots
                    .as_object()
                    .and_then(|m| m.iter().rev().find(|(d, _)| d.as_str() < date))
                    .map(|(_, s)| {
                        s["positions"][warning["instrument"].as_str().unwrap_or("")]
                            .as_f64()
                            .unwrap_or(0.)
                    })
            });
            warning["held_across_event"] = quantity
                .map(|q| q != 0.)
                .map(serde_json::Value::Bool)
                .unwrap_or(serde_json::Value::Null);
        }
    }
    let skipped: Vec<_> = warnings
        .iter()
        .filter(|w| w["code"] == "execution_event_unsupported_skipped")
        .cloned()
        .collect();
    save(request.output.join("execution_warnings.json"), &warnings)?;
    let cross_pool =
        independent::scope_report(&request.output, &audit["execution"]["independent"])?;
    let summary = serde_json::json!({
        "cross_pool":cross_pool,"unsupported_event_policy":"block_unprojected_events","event_simulation_complete":warnings.is_empty(),"skipped_event_count":skipped.len(),"skipped_events":skipped,"schema_version":RESEARCH_SCHEMA_VERSION,"stages":{"training":"completed","prediction":"completed","backtest":"completed"},"execution_diagnostics":diagnostics,"backtest_status":if has_trades{"completed"}else{"no_valid_trades"},"execution_warnings":warnings,"input_kind":if audit["source"]["synthetic_fixture"]==true{"synthetic_fixture"}else{"public_market"},"method":"JFQuant Rust factors + Python LightGBM + AKQuant",
        "fold_reports":reports,"annual_reports":annual,"aggregate":aggregate,"source":audit["source"],
        "formula_version":audit["formula_version"],"signature":run_signature,"factor_signature":factor_signature,
        "admission":serde_json::from_slice::<serde_json::Value>(&fs::read(request.output.join("admission.json"))?)?,"sample_selection":serde_json::from_slice::<serde_json::Value>(&fs::read(request.output.join("sample_selection.json"))?)?,"research_scope":if request.quality_policy.strictness==analytics_core::research_quality::ResearchStrictness::QualifiedOnly{"经数据筛选的研究股票池"}else{"原股票池"},
        "execution":audit["execution"],"excluded_factors":audit["excluded_factors_missing_inputs"],"coverage":audit["coverage"],
    });
    validate_result(&summary)?;
    let summary: artifacts::ResultSummary = serde_json::from_value(summary)?;
    save(request.output.join("summary.json"), &summary)?;
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(request.output.join("summary.json"))?)?;
    if crate::signature(&(factor_signature, &persisted["execution"]))? != run_signature {
        return Err("publication_execution_signature_roundtrip_mismatch".into());
    }

    Ok(())
}

/// Render the completed native research JSON into a readable report and NAV CSV.
/// This only formats existing results and never fits a model or selects parameters.
pub fn render_report(output: &Path) -> Result<()> {
    report::render(output)
}

/// Inspect eligible labeled rows without generating factor caches or fitting a model.
/// Counts precede factor execution; null-feature counts remain explicitly uncomputed.
pub fn preview_samples(
    dataset: &DailyDataset,
    pool: &analytics_core::pool::PoolMembership,
    folds: &[analytics_core::schedule::ResolvedFold],
) -> Result<serde_json::Value> {
    preview_samples_with_policy(dataset, pool, folds, &ResearchQualityPolicy::default())
}
/// Count labeled candidates using the same missing-input policy as the worker.
pub fn preview_samples_with_policy(
    dataset: &DailyDataset,
    pool: &analytics_core::pool::PoolMembership,
    folds: &[analytics_core::schedule::ResolvedFold],
    policy: &ResearchQualityPolicy,
) -> Result<serde_json::Value> {
    let mut counts = vec![[0usize; 3]; folds.len()];
    let mut issues = BTreeMap::new();
    for (id, windows) in &pool.intervals {
        analytics_core::task::TaskContext::current().check()?;
        if dataset
            .checked_fields(id)
            .is_some_and(|fields| fields.is_empty())
        {
            continue;
        }
        let fields = dataset
            .research_field(id, "open", policy.allows_missing())
            .and_then(|o| {
                dataset
                    .research_field(id, "close", policy.allows_missing())
                    .map(|c| (o, c))
            });
        let (open, close) = match fields {
            Ok(f) => f,
            Err(e) => {
                issues.insert(id, e.to_string());
                continue;
            }
        };
        for (i, fold) in folds.iter().enumerate() {
            for (slot, range) in [
                Some(&fold.actual.train),
                fold.actual.validation.as_ref(),
                Some(&fold.actual.test),
            ]
            .into_iter()
            .enumerate()
            {
                let Some(range) = range else {
                    continue;
                };
                let lo = dataset.calendar.partition_point(|d| *d < range.start);
                let hi = dataset.calendar.partition_point(|d| *d <= range.end);
                for day in lo..hi {
                    let member = windows.iter().any(|r| {
                        r.start <= dataset.calendar[day] && dataset.calendar[day] <= r.end
                    });
                    let labeled = slot == 2
                        || (dataset
                            .calendar
                            .get(day + 2)
                            .is_some_and(|d| *d <= range.end)
                            && open.get(day + 1).is_some_and(|p| p > 0.)
                            && open.get(day + 2).is_some_and(|p| p > 0.));
                    if member && labeled && close.get(day).is_some_and(|p| p > 0.) {
                        counts[i][slot] += 1;
                    }
                }
            }
        }
    }
    Ok(
        serde_json::json!({"folds":counts.iter().enumerate().map(|(i,c)|serde_json::json!({"fold":i,"training_label_rows":c[0],"validation_label_rows":c[1],"test_prediction_rows":c[2]})).collect::<Vec<_>>(),"issues":issues,"feature_finite_counts":"not_computed_until_factor_execution"}),
    )
}

mod allocation_admission;
mod allocation_readiness;
