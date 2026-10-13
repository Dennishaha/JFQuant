//! Native annual rolling research using independently acquired daily data.
//! All factor, LightGBM and portfolio work stays on the calling worker thread.
mod data;
mod portfolio;
mod raw_portfolio;
mod report;
use crate::{DenseMatrix, LightGbmPredictor, LightGbmTrainer, Predictor, Trainer, TrainingSplit};
use analytics_core::market_rules::{MarketRulesPreset, OrderSide};
use analytics_core::{Date, DateRange, Objective, TrainRequest};
use analytics_factor::FactorRegistry;
use analytics_storage::qlib::QlibDataset;
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Current strict integer-share research result format.
pub const RESEARCH_SCHEMA_VERSION: u32 = 4;

/// A result cannot be interpreted using the current research contract.
#[derive(Debug, thiserror::Error)]
#[error("unsupported_result_format: {0}")]
pub struct UnsupportedResultFormat(pub String);

/// Validate a completed research result before reporting, importing or displaying it.
/// Historical files are never migrated or interpreted using a different execution mode.
pub fn validate_result(
    value: &serde_json::Value,
) -> std::result::Result<(), UnsupportedResultFormat> {
    let validate = || -> Result<()> {
        if value["schema_version"] != RESEARCH_SCHEMA_VERSION
            || value["execution"]["mode"] != "raw_integer_shares"
        {
            return Err("requires research schema 4 and raw_integer_shares execution".into());
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
        let reports: Vec<FoldReport> = serde_json::from_value(value["annual_reports"].clone())?;
        if reports.is_empty() || reports.windows(2).any(|r| r[0].year >= r[1].year) {
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
    /// Previous training calendar years.
    pub training_years: u32,
    /// Calendar years immediately preceding each test year.
    pub validation_years: u32,
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
            training_years: 5,
            validation_years: 1,
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
        if !(1..=30).contains(&self.training_years)
            || !(1..=10).contains(&self.validation_years)
            || !(1..=1000).contains(&self.top_n)
            || !(1..=252).contains(&self.rebalance_days)
        {
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
    /// Rolling/model/portfolio parameters.
    pub config: ResearchConfig,
    /// Explicit independently downloaded qlib_bin directory.
    pub data_root: PathBuf,
    /// Ignored local research output directory.
    pub output: PathBuf,
    /// First out-of-sample calendar year.
    pub first_test_year: i32,
    /// Last calendar year, truncated to the data's actual last trading date.
    pub last_year: i32,
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
}
impl ResearchRequest {
    /// Defaults to five training years, one validation year and tests from 2013.
    pub fn new(data_root: PathBuf, output: PathBuf) -> Self {
        Self {
            config: ResearchConfig::default(),
            data_root,
            output,
            first_test_year: 2013,
            last_year: 2026,
            audit_only: false,
            market_rules: MarketRulesPreset::a_share_latest(),
            initial_cash_cny: 1_000_000.0,
            corporate_actions: None,
            dividend_withholding_rate: 0.0,
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
    if request.output.join("summary.json").exists() {
        let existing = serde_json::from_slice(&fs::read(request.output.join("summary.json"))?)?;
        validate_result(&existing)?;
    }
    let valid_years = request.config.validation_years as i32;
    let history_years = request.config.training_years as i32 + valid_years;
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
    if !(2000..=9999).contains(&request.first_test_year)
        || !(2000..=9999).contains(&request.last_year)
        || request.first_test_year > request.last_year
    {
        return Err("invalid research year range".into());
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
            || source["archive_verified"] != true)
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
    let dataset = QlibDataset::open(&request.data_root)?;
    let members = dataset.membership("csi500")?;
    for symbol in members.keys() {
        request
            .market_rules
            .fees(symbol, OrderSide::Buy, 100_000.0)?;
    }
    let first_member = members
        .values()
        .flatten()
        .map(|w| w.start)
        .min()
        .ok_or("empty CSI500 membership")?;
    let last = dataset.calendar.last().copied().ok_or("empty calendar")?;
    let registry = FactorRegistry::default();
    let supported_inputs = [
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
    let signature = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&(
            source.clone(),
            &ids,
            &formula_version,
            "open-next-2,pit-mask,warmup756,rebase-v2"
        ))?)
    );
    let execution = serde_json::json!({"mode":"raw_integer_shares",
        "fee_application":"latest_only_for_all_dates", "market_rules":request.market_rules, "initial_cash_cny":request.initial_cash_cny,
        "price_limit_buffer":0.005,

        "corporate_action_sha256":action_hash,"dividend_withholding_rate":request.dividend_withholding_rate,
        "research_config":request.config,"dividend_tax_convention":"explicit fixed research rate; default gross; not personal FIFO holding-period tax"});
    let run_signature = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&(
            &signature,
            &execution,
            request.first_test_year,
            request.last_year,
            "a-share-raw-execution-v4"
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
    let coverage:Vec<_>=(2000..=request.last_year).map(|year|serde_json::json!({"year":year,"status":if year<dataset.calendar[first_member].year(){"no_historical_csi500_membership"}else if year<request.first_test_year{"training_or_validation_only"}else{"out_of_sample"},"partial_year":year==last.year()})).collect();
    save(
        request.output.join("data_audit.json"),
        &serde_json::json!({"source":source,"provider":dataset.root,"requested":"2000-2026","calendar_start":dataset.calendar[0],"calendar_end":last,"historical_csi500_start":dataset.calendar[first_member],"unique_historical_members":members.len(),"registered_factors":registry.len(),"usable_factor_ids":ids,"excluded_factors_missing_inputs":excluded,"formula_version":formula_version,"factor_signature":signature,"research_signature":run_signature,"execution":execution,"coverage":coverage,"uses_jfqlib_code_or_cache":false,"label":"adjusted_open[t+2]/adjusted_open[t+1]-1","rolling_config":request.config,"split":"configured preceding training/validation calendar years, next year test; purge labels crossing boundaries","portfolio":"integer raw-price fills; T+1; persistent cash/share/receivable ledger; close NAV; no terminal fake fills"}),
    )?;
    eprintln!(
        "data_end={last} csi500_start={} factor_count={} excluded={excluded:?}",
        dataset.calendar[first_member],
        ids.len()
    );
    if !request.audit_only
        && request.first_test_year < dataset.calendar[first_member].year() + history_years
    {
        return Err(
            "first test year requires 5 historical training years and 1 validation year".into(),
        );
    }
    let final_year = request.last_year.min(last.year());
    if request.first_test_year > final_year {
        return Err("no out-of-sample dates within actual data coverage".into());
    }
    {
        let validation = (|| -> Result<()> {
            let book = action_book.as_ref().ok_or("raw execution requires a registered corporate-action snapshot; download using jfquant data actions and complete missing settlement dates")?;
            let symbols = members.keys().cloned().collect();
            let start = Date::from_ymd_opt(request.first_test_year, 1, 1).ok_or("invalid start")?;
            let end = last.min(Date::from_ymd_opt(final_year, 12, 31).ok_or("invalid end")?);
            raw_portfolio::validate_actions(&dataset, book, &symbols, start, end)
        })();
        let action_coverage=action_book.as_ref().map(|book| {
            let start=Date::from_ymd_opt(request.first_test_year,1,1).expect("validated year");
            let end=last.min(Date::from_ymd_opt(final_year,12,31).expect("validated year"));
            let events:Vec<_>=book.actions.iter().filter(|a|a.ex_date>=start && a.record_date<=end).collect();
            serde_json::json!({"range_events":events.len(),"missing_cash_payment_dates":events.iter().filter(|a|a.cash_per_share>0.0 && a.pay_date.is_none()).count(),
                "missing_share_listing_dates":events.iter().filter(|a|a.shares_per_share>0.0 && a.listing_date.is_none()).count(),
                "invalid_events":events.iter().filter(|a|a.validate().is_err()).count(),"source_failures":book.failures,
                "issues":events.iter().filter_map(|a|a.validate().err().map(|e|serde_json::json!({"id":a.id,"instrument":a.instrument,"record_date":a.record_date,"ex_date":a.ex_date,"pay_date":a.pay_date,"listing_date":a.listing_date,"reason":e.to_string()}))).collect::<Vec<_>>()})
        });
        save(
            request.output.join("execution_readiness.json"),
            &serde_json::json!({"ready":validation.is_ok(),"reason":validation.as_ref().err().map(|e|e.to_string()),"corporate_action_coverage":action_coverage}),
        )?;
        if !request.audit_only {
            validation?;
        }
    }
    if request.audit_only {
        return Ok(());
    }
    let cache = request.output.join("factor-cache");
    fs::create_dir_all(&cache)?;
    for year in request.first_test_year - history_years..=final_year {
        context.report(format!("因子年度 {year}"))?;
        data::prepare_year(
            &dataset, &members, &registry, &ids, year, &cache, &signature,
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
    let mut raw_state = raw_portfolio::RawPortfolio::new(request)?;
    let mut reports = Vec::new();
    let mut all_returns = Vec::new();
    for year in request.first_test_year..=final_year {
        context.check()?;
        eprintln!("fold={year} loading five-year training matrices");
        let mut train = DenseMatrix {
            features: Vec::new(),
            labels: Vec::new(),
            rows: 0,
            columns: ids.len(),
        };
        for y in year - history_years..=year - valid_years - 1 {
            data::append_matrix(&cache, y, &ids, &mut train, year - valid_years - 1)?;
        }
        let mut valid = DenseMatrix {
            features: Vec::new(),
            labels: Vec::new(),
            rows: 0,
            columns: ids.len(),
        };
        for y in year - valid_years..year {
            data::append_matrix(&cache, y, &ids, &mut valid, year - 1)?;
        }
        let test = data::load_year(&cache, year, &ids)?;
        let mut test_matrix = DenseMatrix {
            features: Vec::new(),
            labels: Vec::new(),
            rows: 0,
            columns: ids.len(),
        };
        for (row, values) in test
            .meta
            .rows
            .iter()
            .zip(test.features.chunks_exact(ids.len()))
        {
            if let Some(label) = row.label
                && row.exit_year == year
            {
                test_matrix
                    .features
                    .extend(values.iter().map(|v| *v as f64));
                test_matrix.labels.push(label as f32);
                test_matrix.rows += 1;
            }
        }
        let split = TrainingSplit {
            train,
            validation: valid,
            test: test_matrix,
        };
        let output = request.output.join(format!("fold-{year}"));
        fs::create_dir_all(&output)?;
        context.report(format!("训练年度 {year}"))?;
        let parameters = request.config.training.parameters.clone();
        let training_request = TrainRequest {
            factor_ids: ids.clone(),
            universe_id: "csi500".into(),
            date_range: DateRange {
                start: Date::from_ymd_opt(year - history_years, 1, 1).ok_or("invalid year")?,
                end: Date::from_ymd_opt(year - valid_years - 1, 12, 31).ok_or("invalid year")?,
            },
            objective: Objective::Regression,
            parameters,
            early_stopping_rounds: request.config.training.early_stopping_rounds,
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
        manifest["rolling_fold"] = serde_json::json!({"test_year":year,"train_years":request.config.training_years,"validation_years":request.config.validation_years,"purged_label_boundary":true});
        save(&artifact.manifest_path, &manifest)?;
        let scores = LightGbmPredictor {
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
        .collect::<Vec<_>>();
        drop(split);
        let (ic_rows, ic_scores): (Vec<_>, Vec<_>) = test
            .meta
            .rows
            .iter()
            .zip(&scores)
            .filter(|(row, _)| row.exit_year == year)
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
            year,
        )?;
        let performance = portfolio::performance(&days);
        save(output.join("daily.json"), &days)?;
        all_returns.extend(days);
        let report = FoldReport {
            year,
            train_start: format!("{}-01-01", year - history_years),
            train_end: format!("{}-12-31", year - valid_years - 1),
            valid_start: format!("{}-01-01", year - valid_years),
            valid_end: format!("{}-12-31", year - 1),
            test_start: format!("{year}-01-01"),
            test_end: last
                .min(Date::from_ymd_opt(year, 12, 31).ok_or("invalid year")?)
                .to_string(),
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
        save(request.output.join("annual_reports.json"), &reports)?;
    }
    finish(request, &reports, &all_returns, &run_signature, &signature)?;
    render_report(&request.output)?;
    Ok(())
}

fn finish(
    request: &ResearchRequest,
    reports: &[FoldReport],
    days: &[portfolio::DayResult],
    run_signature: &str,
    factor_signature: &str,
) -> Result<()> {
    let audit: serde_json::Value =
        serde_json::from_slice(&fs::read(request.output.join("data_audit.json"))?)?;
    let aggregate = portfolio::performance(days);
    save(
        request.output.join("summary.json"),
        &serde_json::json!({
            "schema_version":RESEARCH_SCHEMA_VERSION,"input_kind":if audit["source"]["synthetic_fixture"]==true{"synthetic_fixture"}else{"public_market"},"method":"JFQuant native GTJA annual walk-forward",
            "annual_reports":reports,"aggregate":aggregate,"source":audit["source"],
            "formula_version":audit["formula_version"],"signature":run_signature,"factor_signature":factor_signature,
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
