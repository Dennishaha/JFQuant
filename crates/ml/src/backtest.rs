//! Native annual rolling research using independently acquired daily data.
//! All factor, LightGBM and portfolio work stays on the calling worker thread.
mod data;
mod portfolio;
/// Shared dated input qualification and audit artifacts.
pub mod quality;
mod raw_portfolio;
mod report;
use crate::{DenseMatrix, LightGbmPredictor, LightGbmTrainer, Predictor, Trainer, TrainingSplit};
use analytics_core::market_rules::{MarketRulesPreset, OrderSide};
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
const SUPPORTED_RESEARCH_INPUTS: [&str; 15] = [
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

/// Current strict integer-share research result format.
pub const RESEARCH_SCHEMA_VERSION: u32 = 9;

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
        schema_version: 2,
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
        if value["schema_version"] != RESEARCH_SCHEMA_VERSION
            || value["execution"]["mode"] != "raw_integer_shares"
        {
            return Err("requires research schema 9 and raw_integer_shares execution".into());
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
            || value["formula_version"] != analytics_factor::FORMULA_VERSION
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
        let execution = &value["execution"];
        let policy: ResearchQualityPolicy =
            serde_json::from_value(execution["quality_policy"].clone())?;
        policy.validate()?;
        let qualification: quality::QualitySnapshot =
            serde_json::from_value(execution["quality"].clone())?;
        let admission: analytics_core::admission::AdmissionReport =
            serde_json::from_value(value["admission"].clone())?;
        if qualification.schema_version != 1
            || qualification.policy != policy
            || admission.policy != policy
            || !admission.ready()
            || execution["quality_sha256"]
                != format!(
                    "sha256:{:x}",
                    Sha256::digest(serde_json::to_vec(&qualification)?)
                )
            || execution["sample_selection_sha256"]
                != format!(
                    "sha256:{:x}",
                    Sha256::digest(serde_json::to_vec(&execution["sample_selection"])?)
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
        let directory: analytics_core::security::SecurityRegistry =
            serde_json::from_value(execution["identity_registry"].clone())?;
        directory.validate()?;
        if directory.synthetic_fixture != (value["input_kind"] == "synthetic_fixture")
            || execution["identity_signature"]
                != format!(
                    "sha256:{:x}",
                    Sha256::digest(serde_json::to_vec(&Some(&directory))?)
                )
        {
            return Err("identity directory signature/provenance mismatch".into());
        }
        if !hash(&execution["price_input_sha256"])
            || execution["price_input_sha256"]
                != format!(
                    "sha256:{:x}",
                    Sha256::digest(serde_json::to_vec(&execution["price_inputs"])?)
                )
            || !matches!(
                execution["price_inputs"]["normalization"].as_str(),
                Some(
                    "qlib-first-stored-factor-v2"
                        | "explicit-hfq-first-raw-close-v1;shares;cny;nulls"
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
            if !analytics_core::security::is_security_id(id)
                || !directory.securities.contains_key(id)
            {
                return Err("result price inputs require registered opaque identities".into());
            }
            if !hash(&input["sha256"]) {
                return Err("invalid coefficient source hash".into());
            }
            if input["source"].is_object() {
                let identity: analytics_core::security::SecurityRecord =
                    serde_json::from_value(input["identity"].clone())?;
                if identity.security_id != *id || !analytics_core::security::is_security_id(id) {
                    return Err("invalid bound security identity".into());
                }
                let receipt = serde_json::json!({"identity":input["identity"],"bindings":input["bindings"],"source":input["source"]});
                if input["sha256"]
                    != format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(&receipt)?))
                {
                    return Err("identity/source receipt mismatch".into());
                }
                let sources = input["source"]["securities"]
                    .as_object()
                    .ok_or("missing bound source inputs")?;
                let bindings: Vec<analytics_core::security::SecurityLocator> =
                    serde_json::from_value(input["bindings"].clone())?;
                if bindings.is_empty()
                    || bindings
                        .iter()
                        .any(|l| !identity.locators.contains(l) || !sources.contains_key(&l.symbol))
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
            != format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(&pool)?))
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
        let expected: usize = frozen
            .iter()
            .map(|f| if f.actual.validation.is_some() { 3 } else { 2 })
            .sum();
        if masks.len() != expected {
            return Err("incomplete sample mask manifests".into());
        }
        let mut seen = BTreeSet::new();
        for mask in masks {
            let index = mask["fold"].as_u64().ok_or("missing mask fold")? as usize;
            let split = mask["split"].as_str().ok_or("missing mask split")?;
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
    /// Ranked portfolio target count.
    pub top_n: usize,
    /// Trading days between target rebalances.
    pub rebalance_days: usize,
    /// Ordered explicit factor IDs; empty selects available research factors.
    pub factor_ids: Vec<String>,
    /// Native training parameters and early stopping.
    pub training: crate::TrainingConfig,
}
impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            top_n: 50,
            rebalance_days: 5,
            factor_ids: Vec::new(),
            training: crate::TrainingConfig {
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
#[derive(Debug, Clone)]
pub struct ResearchRequest {
    /// Frozen completeness and sample policy.
    pub quality_policy: ResearchQualityPolicy,
    /// Rolling/model/portfolio parameters.
    pub config: ResearchConfig,
    /// Reviewed security identity and dated source directory.
    pub identity_registry: Option<analytics_core::security::SecurityRegistry>,
    /// Explicit independently downloaded qlib_bin directory.
    pub data_root: PathBuf,
    /// Ignored local research output directory.
    pub output: PathBuf,
    /// Explicit date-level model and execution schedule.
    pub schedule: Schedule,
    /// Explicit ordinary-holder corporate action policy.
    pub action_policy: analytics_core::actions::ActionPolicy,
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
impl ResearchRequest {
    /// Defaults to five training years, one validation year and tests from 2013.
    pub fn new(data_root: PathBuf, output: PathBuf) -> Self {
        Self {
            quality_policy: ResearchQualityPolicy::default(),
            config: ResearchConfig::default(),
            identity_registry: None,
            data_root,
            output,
            schedule: Schedule::default(),
            action_policy: Default::default(),
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
    performance: portfolio::Performance,
}

fn save(path: impl AsRef<Path>, value: &impl Serialize) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
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
    context.execute(|| run_scoped(request))
}
fn run_scoped(request: &ResearchRequest) -> Result<()> {
    let context = analytics_core::task::TaskContext::current();
    context.check()?;
    request.config.validate()?;
    request.quality_policy.validate()?;
    if request.output.join("summary.json").exists() {
        let existing = serde_json::from_slice(&fs::read(request.output.join("summary.json"))?)?;
        validate_result(&existing)?;
    }
    request.schedule.validate()?;
    if request.action_policy.exercise_optional_cash_choice {
        return Err("unsupported active cash-choice policy".into());
    }
    request.market_rules.validate()?;
    if !request.dividend_withholding_rate.is_finite()
        || !(0.0..=1.0).contains(&request.dividend_withholding_rate)
    {
        return Err("invalid dividend withholding rate".into());
    }
    let action_book = request
        .corporate_actions
        .as_ref()
        .map(|path| analytics_storage::corporate_actions::ActionBundle::read(path))
        .transpose()?;
    let action_hash = request
        .corporate_actions
        .as_ref()
        .map(|path| fs::read(path).map(|bytes| format!("sha256:{:x}", Sha256::digest(bytes))))
        .transpose()?;
    if !request.initial_cash_cny.is_finite()
        || request.initial_cash_cny <= 0.0
        || request.initial_cash_cny > 1e13
    {
        return Err("invalid initial cash in CNY".into());
    }
    fs::create_dir_all(&request.output)?;
    let source_path = request
        .data_root
        .parent()
        .ok_or("provider needs source parent")?
        .join("jfquant_source.json");
    let source: serde_json::Value = serde_json::from_slice(&fs::read(&source_path)?)?;
    let synthetic = source["generator"] == "JFQuant analytics-workflow synthetic-demo-v1"
        && source["synthetic_fixture"] == true;
    if !synthetic
        && (source["downloader"] != "JFQuant analytics-storage"
            || (source["archive_verified"] != true && source["snapshot_verified"] != true))
    {
        return Err(
            "data must be independently acquired and verified by jfquant data download".into(),
        );
    }
    if action_book.as_ref().is_some_and(|book| {
        (book.downloader == "JFQuant analytics-workflow synthetic-demo-v1") != synthetic
    }) {
        return Err("cannot mix synthetic and public company actions/market data".into());
    }
    let mut dataset = DailyDataset::open(&request.data_root)?;
    if let Some(r) = &request.identity_registry {
        dataset.bind_registry(r.clone())?;
    }
    let mut pool = match &request.pool {
        Some(p) => p.clone(),
        None => dataset.pool(&request.universe_id)?,
    };
    let last = dataset.calendar.last().copied().ok_or("empty calendar")?;
    let folds = match request.schedule.resolve(&dataset.calendar) {
        Ok(f) => f,
        Err(e) => {
            save(
                request.output.join("execution_readiness.json"),
                &serde_json::json!({"ready":false,"reason":e,"checks":{"schedule":"failed","events":"not_run"}}),
            )?;
            return if request.audit_only {
                Ok(())
            } else {
                Err(format!("data_not_ready: {e}").into())
            };
        }
    };
    let history_start = folds.iter().map(|f| f.actual.train.start).min().unwrap();
    let first_required = history_start;
    let execution_start = folds[0].actual.test.start;
    let needed_end = folds.last().unwrap().actual.test.end;
    // Irrelevant former constituents are kept in the frozen source but cannot block later windows.
    let history_index = dataset
        .calendar
        .partition_point(|d| *d < history_start)
        .saturating_sub(756);
    let relevant_start = dataset.calendar[history_index];
    dataset.restrict_reads(DateRange {
        start: relevant_start,
        end: needed_end,
    })?;
    pool.intervals.retain(|_, ranges| {
        ranges.retain(|r| r.end >= relevant_start && r.start <= needed_end);
        !ranges.is_empty()
    });
    let quality_snapshot = quality::qualify(
        &dataset,
        &pool,
        request.identity_registry.as_ref(),
        &request.quality_policy,
        &DateRange {
            start: relevant_start,
            end: needed_end,
        },
        &request.config.factor_ids,
        action_book.as_ref(),
    )?;
    save(request.output.join("quality.json"), &quality_snapshot)?;
    pool = quality_snapshot.effective_pool.clone();
    // Both factor and execution workers consume exactly this frozen derived pool.
    let mut effective_request = request.clone();
    effective_request.pool = Some(pool.clone());
    let request = &effective_request;
    let pool_problem = pool.validate().err();
    let members = dataset.align_membership(&pool);
    let pool_signature = format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(&pool)?));
    analytics_core::pool::validate_instrument(&request.benchmark)?;
    let first_member = dataset
        .calendar
        .partition_point(|d| *d < pool.coverage.start)
        .min(dataset.calendar.len() - 1);
    let mut admission_issues = Vec::<String>::new();
    let identity_problem = request.identity_registry.as_ref().is_none_or(|r| {
        !r.securities.contains_key(&request.benchmark)
            || pool
                .intervals
                .keys()
                .any(|id| !r.securities.contains_key(id))
    });
    if quality_snapshot
        .decisions
        .iter()
        .any(|d| d.disposition == ProblemDisposition::Block)
    {
        admission_issues
            .push("input integrity/identity checks failed; see quality decisions".into());
    }
    if identity_problem {
        admission_issues.push(
            "required pool/benchmark identities are missing from the reviewed directory".into(),
        );
    }
    if pool
        .intervals
        .keys()
        .any(|id| !analytics_core::security::is_security_id(id))
    {
        admission_issues.push("unresolved source identities in required pool".into());
    }
    if let Some(problem) = &pool_problem {
        admission_issues.push(problem.clone());
    }
    if !dataset.has_raw_prices() {
        admission_issues.push("raw execution price basis is unverified".into());
    }
    if dataset.calendar[0] > history_start + chrono::Duration::days(7)
        || pool.coverage.start > first_required
        || pool.coverage.end < needed_end
    {
        admission_issues.push("historical pool/market coverage incomplete".into());
    }
    for symbol in members.keys() {
        if dataset.is_price_index(symbol).unwrap_or(false) {
            admission_issues.push(format!(
                "{symbol}: price indices cannot be traded as A-share pool constituents"
            ));
        }
        if let Err(e) = dataset
            .exchange_symbol(
                symbol,
                members[symbol]
                    .first()
                    .map_or(execution_start, |r| dataset.calendar[r.start]),
            )
            .and_then(|code| {
                request
                    .market_rules
                    .fees(&code, OrderSide::Buy, 100_000.0)
                    .map_err(std::io::Error::other)
            })
        {
            admission_issues.push(e.to_string());
        }
    }
    let mut market_issues = BTreeMap::new();
    let mut allowed_market_missing = BTreeMap::new();
    for symbol in members.keys().chain(std::iter::once(&request.benchmark)) {
        let mut problems = vec![];
        for field in [
            "open", "high", "low", "close", "volume", "amount", "vwap", "factor",
        ] {
            if let Err(e) = dataset.research_field(
                symbol,
                field,
                request.quality_policy.strictness
                    == analytics_core::research_quality::ResearchStrictness::QualifiedOnly,
            ) {
                if request.quality_policy.allows_missing()
                    && symbol != &request.benchmark
                    && e.kind() == std::io::ErrorKind::NotFound
                    && field != "factor"
                {
                    allowed_market_missing
                        .entry(symbol.clone())
                        .or_insert_with(Vec::new)
                        .push(format!("{field}: {e}"));
                } else {
                    problems.push(format!("{field}: {e}"));
                }
            }
        }
        if !problems.is_empty() {
            market_issues.insert(symbol.clone(), problems);
        }
    }
    if !market_issues.is_empty() {
        admission_issues.push("required market fields unavailable; see market_issues".into());
    }
    let mut factor_basis_issues = BTreeMap::new();
    for symbol in members.keys().chain(std::iter::once(&request.benchmark)) {
        context.check()?;
        if let Err(error) = dataset
            .validate_factor_basis(symbol)
            .and_then(|_| dataset.validate_coefficient_origin(symbol, synthetic))
        {
            factor_basis_issues.insert(symbol.clone(), error.to_string());
        }
    }
    let mut adjustment_issues = BTreeMap::new();
    {
        let mut symbols: BTreeSet<_> = members.keys().cloned().collect();
        if !dataset.is_price_index(&request.benchmark).unwrap_or(false) {
            symbols.insert(request.benchmark.clone());
        }
        match &action_book {
            Some(book) => {
                for id in &symbols {
                    let until = pool
                        .intervals
                        .get(id)
                        .and_then(|rs| rs.iter().map(|r| r.end).max())
                        .map_or(needed_end, |d| d.min(needed_end));
                    adjustment_issues.extend(raw_portfolio::adjustment_issues(
                        &dataset,
                        book,
                        &BTreeSet::from([id.clone()]),
                        relevant_start,
                        until,
                    )?);
                }
            }
            None => {
                adjustment_issues.insert(
                    "corporate_actions".into(),
                    vec![
                        "registered history required for training/warmup adjustment verification"
                            .into(),
                    ],
                );
            }
        }
    }
    let mut settlement_issues = BTreeMap::new();
    let mut successor_issues = BTreeMap::new();
    if let Some(book) = &action_book {
        let required = book.required_symbols(
            &members
                .iter()
                .filter(|(_, rs)| {
                    rs.iter().any(|r| {
                        dataset.calendar[r.end] >= execution_start
                            && dataset.calendar[r.start] <= needed_end
                    })
                })
                .map(|(id, _)| id.clone())
                .collect(),
            execution_start,
            needed_end,
        );
        for id in &required {
            if let Err(e) =
                book.require_coverage(&BTreeSet::from([id.clone()]), execution_start, needed_end)
            {
                settlement_issues.insert(id.clone(), e.to_string());
            }
        }
        for a in book.actions.iter().filter(|a| {
            a.ex_date >= execution_start
                && a.record_date <= needed_end
                && required.contains(&a.instrument)
        }) {
            if !synthetic
                && a.shares_per_share.fract() != 0.
                && matches!(
                    &a.terms,
                    analytics_core::actions::EventTerms::Distribution {
                        allocation: None | Some(analytics_core::actions::Allocation::Exact)
                    }
                )
            {
                settlement_issues.insert(a.instrument.clone(),"ordinary bonus shares require evidenced registrar fractional allocation before training".into());
            }
            if let Some(id) = a.terms.successor() {
                if let analytics_core::actions::EventTerms::Conversion {
                    category,
                    ratio,
                    allocation,
                    ordinary_holder: true,
                    ..
                } = &a.terms
                {
                    if !synthetic
                        && matches!(allocation, analytics_core::actions::Allocation::Exact)
                        && analytics_core::actions::decimal(ratio).is_ok_and(|r| r.fract() != 0.)
                    {
                        settlement_issues.insert(
                            a.instrument.clone(),
                            "conversion requires evidenced fractional allocation before training"
                                .into(),
                        );
                    }
                    if *category == analytics_core::actions::ConversionKind::CodeChange {
                        if id != a.instrument {
                            successor_issues.insert(
                                id.to_owned(),
                                "code change must retain the same security identity".into(),
                            );
                        }
                    } else {
                        let expected = request.identity_registry.as_ref().and_then(|r| {
                            r.successions
                                .iter()
                                .find(|l| l.predecessor == a.instrument && l.successor == id)
                        });
                        let kind = match category {
                            analytics_core::actions::ConversionKind::Merger => "merger",
                            analytics_core::actions::ConversionKind::Reduction => "reduction",
                            analytics_core::actions::ConversionKind::ShareReform => "reform",
                            analytics_core::actions::ConversionKind::Reorganization => {
                                "reorganization"
                            }
                            analytics_core::actions::ConversionKind::CodeChange => "code_change",
                        };
                        if expected.is_none_or(|l| {
                            l.kind != kind
                                || analytics_core::actions::decimal(&l.ratio).ok()
                                    != analytics_core::actions::decimal(ratio).ok()
                                || l.event_id.as_ref().is_some_and(|event| event != &a.id)
                        }) {
                            successor_issues.insert(
                                id.to_owned(),
                                "succession identity/ratio/event proof missing or conflicting"
                                    .into(),
                            );
                        }
                    }
                }
                match dataset
                    .raw_field(id, "close")
                    .and_then(|field| dataset.validate_factor_basis(id).map(|_| field))
                {
                    Err(e) => {
                        successor_issues.insert(id.to_string(), e.to_string());
                    }
                    Ok(field) => {
                        if let analytics_core::actions::EventTerms::Conversion {
                            listing_date: Some(listing),
                            claim_valuation,
                            ..
                        } = &a.terms
                        {
                            let missing = dataset.calendar.iter().enumerate().any(|(i, d)| {
                                *d >= a.ex_date
                                    && *d < *listing
                                    && *d <= needed_end
                                    && field.get(i).is_none()
                                    && !claim_valuation
                                        .as_ref()
                                        .is_some_and(|v| v.date <= *d && *d <= v.valid_until)
                            });
                            if missing {
                                successor_issues.insert(
                                    id.to_string(),
                                    "successor claim lacks raw quote or dated valuation".into(),
                                );
                            }
                        }
                    }
                }
            }
        }
    } else {
        settlement_issues.insert(
            "corporate_actions".into(),
            "no action snapshot selected".into(),
        );
    }
    settlement_issues.extend(quality::possible_holding_issues(
        &dataset,
        &pool,
        action_book.as_ref(),
        execution_start,
        needed_end,
    )?);
    let benchmark_check = (|| -> Result<()> {
        let b = dataset.raw_field(&request.benchmark, "close")?;
        let lo = dataset
            .calendar
            .partition_point(|d| *d < execution_start)
            .saturating_sub(1);
        let hi = dataset.calendar.partition_point(|d| *d <= needed_end);
        if (lo..hi).any(|i| b.get(i).is_none_or(|p| p <= 0.)) {
            return Err("missing benchmark execution closes".into());
        }
        Ok(())
    })();
    if !settlement_issues.is_empty() {
        admission_issues.push("incomplete account settlement events; see settlement_issues".into());
    }
    if !successor_issues.is_empty() {
        admission_issues.push("successor data missing; see successor_issues".into());
    }
    if let Err(e) = &benchmark_check {
        admission_issues.push(e.to_string());
    }
    let sample_readiness =
        preview_samples_with_policy(&dataset, &pool, &folds, &request.quality_policy)?;
    if sample_readiness["issues"]
        .as_object()
        .is_some_and(|m| !m.is_empty())
    {
        admission_issues.push("sample input reads failed; see sample_readiness.issues".into());
    }
    if sample_readiness["issues"]
        .as_object()
        .is_some_and(|m| m.is_empty())
        && sample_readiness["folds"].as_array().is_some_and(|rows| {
            rows.iter().enumerate().any(|(i, r)| {
                r["training_label_rows"] == 0
                    || (folds[i].actual.validation.is_some() && r["validation_label_rows"] == 0)
            })
        })
    {
        admission_issues.push("no eligible labeled training/validation rows".into());
    }
    if !adjustment_issues.is_empty() {
        admission_issues
            .push("historical coefficient/company-action mismatch; see adjustment_issues".into());
    }
    if !factor_basis_issues.is_empty() {
        admission_issues
            .push("factor price/volume normalization unavailable; see factor_basis_issues".into());
    }
    use analytics_core::admission::{AdmissionCheck, AdmissionReport, CheckStatus};
    let mut admission = AdmissionReport {
        schema_version: 2,
        policy: request.quality_policy.clone(),
        decisions: quality_snapshot.decisions.clone(),
        checks: Vec::new(),
    };
    let mut add_check =
        |category: &str, security: Option<String>, failed: bool, message: String| {
            let mut evidence = vec![format!("market-source:{}", request.data_root.display())];
            if let Some(registry) = &request.identity_registry {
                let records: Vec<_> = match &security {
                    Some(id) => registry.securities.get(id).into_iter().collect(),
                    None => registry.securities.values().collect(),
                };
                for record in records {
                    for key in &record.evidence {
                        if let Some(proof) = registry.evidence.get(key) {
                            evidence.push(format!(
                                "identity:{key}:{}:{}",
                                proof.original_sha256, proof.source_url
                            ));
                        }
                    }
                }
            }
            if matches!(category, "historical_events" | "settlement" | "successor")
                && let Some(book) = &action_book
            {
                if let Some(hash) = &action_hash {
                    evidence.push(format!("action-bundle:{hash}"));
                }
                // The verified bundle indexes all originals. Repeat only this
                // security's coverage receipts, rather than every document per issue.
                if let Some(id) = &security {
                    for name in book
                        .coverage
                        .get(id)
                        .into_iter()
                        .flatten()
                        .flat_map(|c| &c.evidence)
                    {
                        if let Some(hash) = book.raw_sha256.get(name) {
                            evidence.push(format!("action:{name}:{hash}"));
                        }
                    }
                }
            }
            evidence.sort();
            evidence.dedup();
            admission.checks.push(AdmissionCheck {
                kind: if failed {
                    ProblemKind::ExecutionDependency
                } else {
                    ProblemKind::None
                },
                disposition: ProblemDisposition::Block,
                category: category.into(),
                security,
                coverage: Some(DateRange {
                    start: relevant_start,
                    end: needed_end,
                }),
                status: if failed {
                    CheckStatus::Failed
                } else {
                    CheckStatus::Passed
                },
                code: if failed {
                    format!("{category}_not_ready")
                } else {
                    format!("{category}_verified")
                },
                message,
                evidence,
                affected_factors: request.config.factor_ids.clone(),
            });
        };
    add_check(
        "identity",
        None,
        identity_problem,
        "required identities resolved independently from source codes".into(),
    );
    add_check(
        "membership",
        None,
        pool_problem.is_some()
            || pool.coverage.start > first_required
            || pool.coverage.end < needed_end,
        "historical membership and unknown warmup coverage".into(),
    );
    for (category, issues) in [
        ("market", serde_json::to_value(&market_issues)?),
        ("coefficient", serde_json::to_value(&factor_basis_issues)?),
        (
            "historical_events",
            serde_json::to_value(&adjustment_issues)?,
        ),
        ("settlement", serde_json::to_value(&settlement_issues)?),
        ("successor", serde_json::to_value(&successor_issues)?),
    ] {
        if let Some(map) = issues.as_object() {
            if map.is_empty() {
                add_check(category, None, false, "required inputs checked".into());
            } else {
                for (id, issue) in map {
                    add_check(category, Some(id.clone()), true, issue.to_string());
                }
            }
        }
    }
    add_check(
        "samples",
        None,
        sample_readiness["issues"]
            .as_object()
            .is_some_and(|m| !m.is_empty())
            || sample_readiness["folds"].as_array().is_some_and(|rs| {
                rs.iter().enumerate().any(|(i, r)| {
                    r["training_label_rows"] == 0
                        || (folds[i].actual.validation.is_some() && r["validation_label_rows"] == 0)
                })
            }),
        sample_readiness.to_string(),
    );
    add_check(
        "benchmark",
        Some(request.benchmark.clone()),
        benchmark_check.is_err(),
        benchmark_check
            .as_ref()
            .err()
            .map_or_else(|| "benchmark closes checked".into(), |e| e.to_string()),
    );
    for issue in &admission_issues {
        add_check("dependency", None, true, issue.clone());
    }
    for (id, problems) in &allowed_market_missing {
        admission.checks.push(AdmissionCheck {
            kind: ProblemKind::Missing,
            disposition: ProblemDisposition::KeepNull,
            category: "market".into(),
            security: Some(id.clone()),
            coverage: Some(DateRange {
                start: relevant_start,
                end: needed_end,
            }),
            status: CheckStatus::Failed,
            code: "input_file_missing".into(),
            message: problems.join("; "),
            evidence: vec![format!("market:{}", dataset.root.display())],
            affected_factors: request.config.factor_ids.clone(),
        });
    }
    if action_book.is_none() {
        for check in &mut admission.checks {
            if matches!(
                check.category.as_str(),
                "historical_events" | "settlement" | "successor"
            ) {
                check.status = CheckStatus::NotRun;
                check.code = "action_snapshot_missing".into();
                check.message = "no action snapshot supplied; event checks not executed".into();
            }
        }
    }
    admission.checks.push(AdmissionCheck {
        kind: ProblemKind::Missing,
        disposition: ProblemDisposition::Block,
        category: "feature_eligibility".into(),
        security: None,
        coverage: Some(DateRange {
            start: history_start,
            end: needed_end,
        }),
        status: CheckStatus::NotRun,
        code: "prerequisites_pending".into(),
        message: "finite factor samples not yet computed in private staging".into(),
        evidence: vec![],
        affected_factors: request.config.factor_ids.clone(),
    });
    save(request.output.join("admission.json"), &admission)?;
    if !admission_issues.is_empty() {
        let affected_factor_ids: Vec<_> =
            if factor_basis_issues.is_empty() && adjustment_issues.is_empty() {
                Vec::new()
            } else {
                FactorRegistry::default()
                    .all()
                    .filter(|s| {
                        !matches!(
                            s.expression,
                            analytics_factor::FactorExpr::Unsupported { .. }
                        ) && (request.config.factor_ids.is_empty()
                            || request.config.factor_ids.contains(&s.id))
                    })
                    .map(|s| s.id.clone())
                    .collect()
            };
        save(
            request.output.join("execution_readiness.json"),
            &serde_json::json!({"schema_version":2,"quality":quality_snapshot,"admission":admission,"ready":false,"reason":admission_issues.join("; "),"issues":admission_issues,"required_start":first_required,"pool_coverage":pool.coverage,"verified_raw_prices":dataset.has_raw_prices(),"market_issues":market_issues,"factor_basis_issues":factor_basis_issues,"adjustment_issues":adjustment_issues,"settlement_issues":settlement_issues,"successor_issues":successor_issues,"checks":{"schedule":"passed","membership":if pool_problem.is_some(){"failed"}else{"passed"},"factor_basis":if factor_basis_issues.is_empty(){"passed"}else{"failed"},"historical_events":if action_book.is_none(){"not_run"}else if adjustment_issues.is_empty(){"passed"}else{"failed"},"account_events":if action_book.is_none(){"not_run"}else if settlement_issues.is_empty(){"passed"}else{"failed"},"benchmark":if benchmark_check.is_ok(){"passed"}else{"failed"}},"resolved_folds":folds,"sample_readiness":sample_readiness,"affected_factor_ids":affected_factor_ids}),
        )?;
        return if request.audit_only {
            Ok(())
        } else {
            Err("data_not_ready: input admission failed; see execution_readiness.json".into())
        };
    }
    let mut price_symbols: BTreeSet<String> = members
        .keys()
        .chain(std::iter::once(&request.benchmark))
        .cloned()
        .collect();
    if let Some(book) = &action_book {
        price_symbols = book.required_symbols(&price_symbols, execution_start, needed_end);
    }
    let price_inputs = dataset.price_inputs(&price_symbols)?;
    let price_input_sha256 = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&price_inputs)?)
    );
    let registry = FactorRegistry::default();
    let supported_inputs = SUPPORTED_RESEARCH_INPUTS;
    let mut excluded = BTreeMap::new();
    let mut ids = Vec::new();
    for spec in registry.all() {
        if matches!(
            spec.expression,
            analytics_factor::FactorExpr::Unsupported { .. }
        ) {
            excluded.insert(
                spec.id.clone(),
                vec!["quarantined_formula: see factor_audit.json".into()],
            );
            continue;
        }
        let missing: Vec<_> = spec
            .input_columns
            .iter()
            .filter(|c| !supported_inputs.contains(&c.as_str()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            excluded.insert(spec.id.clone(), missing);
        } else {
            ids.push(spec.id.clone());
        }
    }
    if !request.config.factor_ids.is_empty() {
        let mut seen = std::collections::BTreeSet::new();
        for id in &request.config.factor_ids {
            if !seen.insert(id) || !ids.contains(id) {
                return Err(format!("duplicate, quarantined or unavailable factor: {id}").into());
            }
        }
        ids = request.config.factor_ids.clone();
    }
    let formula_version = registry
        .all()
        .next()
        .ok_or("empty registry")?
        .version
        .clone();
    let mut signature = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&(
            source.clone(),
            &ids,
            &formula_version,
            &pool,
            &request.pool_signature,
            &request.benchmark,
            &price_inputs,
            &quality_snapshot,
            "open-next-2,pit-mask,warmup756,explicit-price-input-v2"
        ))?)
    );
    let mut execution = serde_json::json!({"mode":"raw_integer_shares",
        "quality_policy":request.quality_policy,"quality":quality_snapshot,"quality_sha256":format!("sha256:{:x}",Sha256::digest(serde_json::to_vec(&quality_snapshot)?)),
        "fee_application":"latest_only_for_all_dates", "market_rules":request.market_rules, "initial_cash_cny":request.initial_cash_cny,
        "price_limit_buffer":0.005,

        "identity_registry":request.identity_registry,"identity_signature":format!("sha256:{:x}",Sha256::digest(serde_json::to_vec(&request.identity_registry)?)),"corporate_action_sha256":action_hash,"dividend_withholding_rate":request.dividend_withholding_rate,
        "price_inputs":price_inputs,"price_input_sha256":price_input_sha256,"pool":pool,"pool_signature":pool_signature,"benchmark":request.benchmark,"pool_exit_policy":request.pool_exit_policy,
        "research_config":request.config,"schedule":request.schedule,"action_policy":request.action_policy,"resolved_folds":folds,"dividend_tax_convention":"explicit fixed research rate; default gross; not personal FIFO holding-period tax"});
    let mut run_signature = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&(
            &signature,
            &execution,
            &request.schedule,
            &folds,
            "a-share-raw-execution-explicit-input-v1"
        ))?)
    );
    if request.output.join("summary.json").exists() {
        let old: serde_json::Value =
            serde_json::from_slice(&fs::read(request.output.join("summary.json"))?)?;
        if old["schema_version"] != RESEARCH_SCHEMA_VERSION || old["signature"] != run_signature {
            return Err("existing output uses another execution schema/settings; choose a new output directory".into());
        }
    }
    save(
        request.output.join("factor_audit.json"),
        &analytics_factor::audit::entries(),
    )?;
    save(request.output.join("execution.json"), &execution)?;
    let coverage:Vec<_>=folds.iter().map(|f|serde_json::json!({"requested":f.requested,"actual":f.actual,"warmup_start":f.warmup_start,"warmup_sessions":f.warmup_sessions,"pool_coverage_start":pool.coverage.start,"unknown_cross_section_before":pool.coverage.start})).collect();
    save(
        request.output.join("data_audit.json"),
        &serde_json::json!({"source":source,"provider":dataset.root,"requested":request.schedule,"calendar_start":dataset.calendar[0],"calendar_end":last,"historical_pool_start":dataset.calendar[first_member],"unique_historical_members":members.len(),"registered_factors":registry.len(),"usable_factor_ids":ids,"excluded_factors_missing_inputs":excluded,"formula_version":formula_version,"factor_signature":signature,"research_signature":run_signature,"execution":execution,"coverage":coverage,"uses_jfqlib_code_or_cache":false,"label":"adjusted_open[t+2]/adjusted_open[t+1]-1","rolling_config":request.config,"split":"explicit requested and resolved date folds; purge labels ending beyond split cutoff","portfolio":"integer raw-price fills; T+1; persistent cash/share/receivable ledger; close NAV; no terminal fake fills"}),
    )?;
    eprintln!(
        "data_end={last} pool_start={} factor_count={} excluded={excluded:?}",
        dataset.calendar[first_member],
        ids.len()
    );
    {
        let validation = (|| -> Result<()> {
            let book = action_book.as_ref().ok_or("raw execution requires a registered corporate-action snapshot; download using jfquant data actions and complete missing settlement dates")?;
            let start = execution_start;
            let end = needed_end;
            let symbols = pool
                .intervals
                .iter()
                .filter(|(_, rs)| rs.iter().any(|r| r.end >= start && r.start <= end))
                .map(|(id, _)| id.clone())
                .collect();
            let benchmark = dataset.raw_field(&request.benchmark, "close")?;
            let lo = dataset
                .calendar
                .partition_point(|d| *d < start)
                .saturating_sub(1);
            let hi = dataset.calendar.partition_point(|d| *d <= end);
            let missing: Vec<_> = (lo..hi)
                .filter(|day| benchmark.get(*day).is_none_or(|p| p <= 0.0))
                .map(|day| dataset.calendar[day])
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "data_not_ready: benchmark {} missing execution closes {:?}",
                    request.benchmark, missing
                )
                .into());
            }
            raw_portfolio::validate_actions(&dataset, book, &symbols, start, end)
        })();
        let action_coverage=action_book.as_ref().map(|book| {
            let start=execution_start;
            let end=needed_end;
            let events:Vec<_>=book.actions.iter().filter(|a|a.ex_date>=start && a.record_date<=end).collect();
            serde_json::json!({"range_events":events.len(),"missing_cash_payment_dates":events.iter().filter(|a|a.cash_per_share>0.0 && a.pay_date.is_none()).count(),
                "missing_share_listing_dates":events.iter().filter(|a|a.shares_per_share>0.0 && a.listing_date.is_none()).count(),
                "invalid_events":events.iter().filter(|a|a.validate().is_err()).count(),"source_failures":book.failures,
                "issues":events.iter().filter_map(|a|a.validate().err().map(|e|serde_json::json!({"id":a.id,"instrument":a.instrument,"record_date":a.record_date,"ex_date":a.ex_date,"pay_date":a.pay_date,"listing_date":a.listing_date,"reason":e.to_string()}))).collect::<Vec<_>>()})
        });
        save(
            request.output.join("execution_readiness.json"),
            &serde_json::json!({"ready":validation.is_ok(),"resolved_folds":folds,"sample_readiness":sample_readiness,"reason":validation.as_ref().err().map(|e|e.to_string()),"corporate_action_coverage":action_coverage,"adjustment_issues":adjustment_issues,"factor_basis_issues":factor_basis_issues,"affected_factor_ids":if validation.is_ok(){Vec::<String>::new()}else{ids.clone()}}),
        )?;
        if !request.audit_only {
            validation?;
        }
    }
    let cache = request.output.join(".admission-factor-staging");
    fs::create_dir_all(&cache)?;
    for year in history_start.year()..=needed_end.year() {
        context.report(format!("因子年度 {year}"))?;
        data::prepare_year(
            data::YearInputs {
                dataset: &dataset,
                members: &members,
                benchmark: &request.benchmark,
                pool_start: pool.coverage.start,
                allow_missing: request.quality_policy.strictness
                    != analytics_core::research_quality::ResearchStrictness::Strict,
            },
            &registry,
            &ids,
            year,
            &cache,
            &signature,
        )?;
        let meta = data::load_meta(&cache, year)?;
        let empty: Vec<_> = ids
            .iter()
            .filter(|id| meta.finite_counts.get(*id).copied().unwrap_or(0) == 0)
            .collect();
        if !empty.is_empty() {
            // A full trailing window may legitimately be unavailable in an early
            // membership year. Preserve nulls rather than invent partial history.
            eprintln!(
                "year={year} all-null factors retained as missing (see finite_counts): {empty:?}"
            );
        }
    }
    let mut feature_issues = Vec::new();
    let mut sample_selections = Vec::new();
    fs::create_dir_all(request.output.join("sample_masks"))?;
    for (index, f) in folds.iter().enumerate() {
        let selection = data::matrix_range_with_policy(
            &cache,
            &f.actual.train,
            &ids,
            &dataset.calendar,
            &request.quality_policy,
        )?;
        sample_selections.push(save_selection(
            request,
            &mut admission,
            index,
            "train",
            &f.actual.train,
            &selection.mask,
        )?);
        let matrix = selection.matrix;
        if matrix.rows < request.quality_policy.min_labeled_rows {
            feature_issues.push(format!(
                "fold {index}: eligible training rows {} below required {}",
                matrix.rows, request.quality_policy.min_labeled_rows
            ));
        }
        for (column, id) in ids.iter().enumerate() {
            if !(0..matrix.rows).any(|row| matrix.features[row * ids.len() + column].is_finite()) {
                feature_issues.push(format!("fold {index}: {id} has no finite training samples"));
            }
        }
        if let Some(v) = &f.actual.validation {
            let selection = data::matrix_range_with_policy(
                &cache,
                v,
                &ids,
                &dataset.calendar,
                &request.quality_policy,
            )?;
            sample_selections.push(save_selection(
                request,
                &mut admission,
                index,
                "validation",
                v,
                &selection.mask,
            )?);
            if selection.matrix.rows < request.quality_policy.min_labeled_rows {
                feature_issues.push(format!(
                    "fold {index}: insufficient eligible validation labels"
                ));
            }
        }
    }
    for (index, fold) in folds.iter().enumerate() {
        let mask = data::prediction_mask(
            &cache,
            &fold.actual.test,
            &ids,
            &dataset.calendar,
            &request.quality_policy,
        )?;
        sample_selections.push(save_selection(
            request,
            &mut admission,
            index,
            "prediction",
            &fold.actual.test,
            &mask,
        )?);
    }
    let sample_selection = serde_json::json!({"schema_version":1,"policy":request.quality_policy,"selections":sample_selections});
    save(
        request.output.join("sample_selection.json"),
        &sample_selection,
    )?;
    execution["sample_selection_sha256"] = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&sample_selection)?)
    )
    .into();
    execution["sample_selection"] = sample_selection;
    signature = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&(
            &signature,
            &execution["sample_selection_sha256"]
        ))?)
    );
    run_signature = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&(&signature, &execution))?)
    );
    save(request.output.join("execution.json"), &execution)?;
    let mut audit: serde_json::Value =
        serde_json::from_slice(&fs::read(request.output.join("data_audit.json"))?)?;
    audit["execution"] = execution.clone();
    audit["factor_signature"] = signature.clone().into();
    audit["research_signature"] = run_signature.clone().into();
    save(request.output.join("data_audit.json"), &audit)?;
    for year in history_start.year()..=needed_end.year() {
        let path = cache.join(format!("{year}.json"));
        let mut meta: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        meta["signature"] = signature.clone().into();
        save(path, &meta)?;
    }
    if let Some(check) = admission.checks.last_mut() {
        check.status = if feature_issues.is_empty() {
            CheckStatus::Passed
        } else {
            CheckStatus::Failed
        };
        check.code = if feature_issues.is_empty() {
            "feature_samples_verified"
        } else {
            "no_valid_feature_samples"
        }
        .into();
        check.message = if feature_issues.is_empty() {
            "all selected features have finite eligible training observations".into()
        } else {
            feature_issues.join("; ")
        };
    }
    save(request.output.join("admission.json"), &admission)?;
    if !feature_issues.is_empty() {
        save(
            request.output.join("execution_readiness.json"),
            &serde_json::json!({"schema_version":2,"admission":admission,"quality":quality_snapshot,"sample_selection":execution["sample_selection"],"ready":false,"reason":"factor/sample eligibility failed before training","feature_issues":feature_issues,"resolved_folds":folds}),
        )?;
        return if request.audit_only {
            Ok(())
        } else {
            Err("data_not_ready: factor/sample eligibility failed before training".into())
        };
    }
    if request.audit_only {
        save(
            request.output.join("execution_readiness.json"),
            &serde_json::json!({"schema_version":2,"ready":admission.ready(),"quality":quality_snapshot,"sample_selection":execution["sample_selection"],"admission":admission,"resolved_folds":folds,"sample_readiness":sample_readiness}),
        )?;
        return Ok(());
    }
    let published = request.output.join("factor-cache");
    if published.exists() {
        return Err("existing factor cache must not be replaced".into());
    }
    fs::rename(&cache, &published)?;
    let cache = published;
    let mut raw_state = raw_portfolio::RawPortfolio::new(request)?;
    let mut reports = Vec::new();
    let mut all_returns = Vec::new();
    for (fold_index, resolved) in folds.iter().enumerate() {
        context.check()?;
        let f = &resolved.actual;
        let year = f.test.start.year();
        let train = data::matrix_range_with_policy(
            &cache,
            &f.train,
            &ids,
            &dataset.calendar,
            &request.quality_policy,
        )?
        .matrix;
        let valid = match &f.validation {
            Some(v) => {
                data::matrix_range_with_policy(
                    &cache,
                    v,
                    &ids,
                    &dataset.calendar,
                    &request.quality_policy,
                )?
                .matrix
            }
            None => data::empty_matrix(ids.len()),
        };
        let test = data::prediction_range_with_policy(
            &cache,
            &f.test,
            &ids,
            &dataset.calendar,
            &request.quality_policy,
        )?;
        let test_matrix = data::matrix_range_with_policy(
            &cache,
            &f.test,
            &ids,
            &dataset.calendar,
            &request.quality_policy,
        )?
        .matrix;
        let split = TrainingSplit {
            train,
            validation: valid,
            test: test_matrix,
        };
        let output = request.output.join(format!("fold-{fold_index:04}"));
        fs::create_dir_all(&output)?;
        context.report(format!("训练年度 {year}"))?;
        let parameters = request.config.training.parameters.clone();
        let training_request = TrainRequest {
            factor_ids: ids.clone(),
            universe_id: request.universe_id.clone(),
            date_range: f.train.clone(),
            objective: Objective::Regression,
            parameters,
            early_stopping_rounds: if f.validation.is_some() {
                request.config.training.early_stopping_rounds
            } else {
                None
            },
        };
        let (tx, rx) = mpsc::channel();
        let training_context = context.clone();
        let progress_thread = std::thread::spawn(move || {
            while let Ok(event) = rx.recv() {
                let _ = training_context.report(format!("{event:?}"));
            }
        });
        let artifact_result = LightGbmTrainer.train_split(
            &training_request,
            &split,
            &output,
            tx,
            context.cancel.clone(),
        );
        let _ = progress_thread.join();
        let artifact = artifact_result?;
        context.check()?;
        let train_rows = split.train.rows;
        let validation_rows = split.validation.rows;
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&artifact.manifest_path)?)?;
        manifest["formula_version"] = formula_version.clone().into();
        manifest["input_snapshot_hash"] = signature.clone().into();
        manifest["research_execution"] = execution.clone();
        manifest["rolling_fold"] = serde_json::json!({"fold_index":fold_index,"schedule":resolved,"purged_label_boundary":true});
        save(&artifact.manifest_path, &manifest)?;
        let scores = if test.meta.rows.is_empty() {
            Vec::new()
        } else {
            LightGbmPredictor {
                model_path: artifact.model_path,
            }
            .predict_dense(
                &test.features.iter().map(|v| *v as f64).collect::<Vec<_>>(),
                test.meta.rows.len(),
                ids.len(),
                false,
            )?
            .predictions
            .into_iter()
            .map(|row| row[0])
            .collect::<Vec<_>>()
        };
        drop(split);
        let (ic_rows, ic_scores): (Vec<_>, Vec<_>) = test
            .meta
            .rows
            .iter()
            .zip(&scores)
            .filter(|(row, _)| {
                row.label.is_some() && row.label_end.is_some_and(|d| d <= f.test.end)
            })
            .map(|(row, score)| (row.clone(), *score))
            .unzip();
        let (ic, rank_ic) = portfolio::information_coefficients(&ic_rows, &ic_scores);
        let days = raw_portfolio::simulate(
            &mut raw_state,
            &test.meta,
            &scores,
            &dataset,
            action_book.as_ref().expect("validated action book"),
            request,
            &f.test,
        )?;
        let test_days: Vec<_> = days
            .iter()
            .filter(|d| d.date >= f.test.start.to_string())
            .cloned()
            .collect();
        let performance = portfolio::performance(&test_days);
        save(output.join("daily.json"), &test_days)?;
        save(
            output.join("gap_daily.json"),
            &days
                .iter()
                .filter(|d| d.date < f.test.start.to_string())
                .collect::<Vec<_>>(),
        )?;
        all_returns.extend(days);
        let report = FoldReport {
            year,
            train_start: f.train.start.to_string(),
            train_end: f.train.end.to_string(),
            valid_start: f
                .validation
                .as_ref()
                .map(|v| v.start.to_string())
                .unwrap_or_default(),
            valid_end: f
                .validation
                .as_ref()
                .map(|v| v.end.to_string())
                .unwrap_or_default(),
            test_start: f.test.start.to_string(),
            test_end: f.test.end.to_string(),
            train_rows,
            validation_rows,
            test_rows: scores.len(),
            features: ids.len(),
            ic,
            rank_ic,
            performance,
        };
        eprintln!(
            "fold={year} rank_ic={rank_ic:?} metrics={}",
            serde_json::to_string(&report.performance)?
        );
        save(output.join("report.json"), &report)?;
        let mut csv = String::from("signal_date,instrument,score,label_next_open_return\n");
        for (r, score) in test.meta.rows.iter().zip(&scores) {
            csv.push_str(&format!(
                "{},{},{},{}\n",
                dataset.calendar[r.day],
                r.instrument,
                score,
                r.label.map(|v| v.to_string()).unwrap_or_default()
            ));
        }
        fs::write(output.join("predictions.csv"), csv)?;
        reports.push(report);
        save(request.output.join("fold_reports.json"), &reports)?;
    }
    finish(
        request,
        &reports,
        &all_returns,
        &run_signature,
        &signature,
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
        serde_json::json!({"fold":fold,"split":split,"range":range,"path":relative,"sha256":format!("sha256:{:x}",Sha256::digest(serde_json::to_vec(&mask)?)),"candidate_rows":selection.row_mask.len(),"retained_rows":selection.retained_rows,"excluded_rows":selection.row_mask.len()-selection.retained_rows}),
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
    save(
        request.output.join("summary.json"),
        &serde_json::json!({
            "schema_version":RESEARCH_SCHEMA_VERSION,"input_kind":if audit["source"]["synthetic_fixture"]==true{"synthetic_fixture"}else{"public_market"},"method":"JFQuant native GTJA date-scheduled research",
            "fold_reports":reports,"annual_reports":annual,"aggregate":aggregate,"source":audit["source"],
            "formula_version":audit["formula_version"],"signature":run_signature,"factor_signature":factor_signature,
            "admission":serde_json::from_slice::<serde_json::Value>(&fs::read(request.output.join("admission.json"))?)?,"sample_selection":serde_json::from_slice::<serde_json::Value>(&fs::read(request.output.join("sample_selection.json"))?)?,"research_scope":if request.quality_policy.strictness==analytics_core::research_quality::ResearchStrictness::QualifiedOnly{"经数据筛选的研究股票池"}else{"原股票池"},
            "execution":audit["execution"],"excluded_factors":audit["excluded_factors_missing_inputs"],"coverage":audit["coverage"],
        }),
    )?;
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
