//! End-to-end orchestration from local Parquet inputs to trained LightGBM
//! artifacts.
//!
//! The pipeline sits at the top of the workspace data flow:
//!
//! ```text
//! market + universe Parquet
//!   -> DuckDB as-of join (analytics-engine)
//!   -> Alpha191 factor execution (analytics-factor)
//!   -> factor cache + labels
//!   -> 70/15/15 chronological split
//!   -> LightGBM training + test prediction
//! ```
//!
//! DuckDB handles stay on the Rust worker thread; LightGBM handles stay in
//! the supervised Python process. App and jfquant
//! use the same service worker for pipeline execution. Cancellation
//! is cooperative through a shared [`Cancellation`] flag checked between
//! stages and inside factor/trainer loops.

use analytics_core::{
    Date, DateRange, MarketRow, Objective, TrainRequest, TrainingArtifact, chronological_date_split,
};
use analytics_engine::DuckDbEngine;
use analytics_factor::{FactorEvent, FactorRegistry, FactorRunArtifact, Panel, next_return_labels};
use arrow_array::{Array, ArrayRef, Date32Array, Float64Array, RecordBatch, StringArray};
use arrow_schema::{DataType as ArrowDataType, Field, Schema};
use chrono::NaiveDate;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Sender, channel},
    },
};
use thiserror::Error;

use crate::research::{
    DenseMatrix, LightGbmPredictor, LightGbmTrainer, Predictor, TrainEvent, Trainer, TrainingSplit,
};

/// Shared pipeline request used by the App and standalone CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineRequest {
    /// Single independently versioned feature family.
    pub factor_set: analytics_core::research_quality::FactorSet,
    /// Directory or single file of normalized daily market Parquet.
    pub market_path: PathBuf,
    /// Directory or single file of historical-universe membership Parquet.
    pub universe_path: PathBuf,
    /// Universe identifier joined with `universe_id` in the membership data.
    pub universe_id: String,
    /// Inclusive trade-date bounds for the run.
    pub date_range: DateRange,
    /// Alpha191 factor IDs such as `alpha_001`.
    pub factor_ids: Vec<String>,
    /// Frozen definitions for mixed selections; legacy builtin requests omit these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub factor_refs: Vec<analytics_factor::selection::FactorRef>,
    /// Output directory for the factor cache, model, and predictions.
    pub output_dir: PathBuf,
    /// Model parameters and early stopping.
    pub training: TrainingConfig,
}

use crate::research::TrainingConfig;

/// Progress stream emitted by [`run_factor_pipeline`] and
/// [`train_from_factor_cache`].
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// Market and universe paths are being joined in DuckDB.
    LoadingMarket,
    /// The as-of join finished with the given number of normalized rows.
    MarketLoaded {
        /// Rows kept after `is_trading` and listing-date filtering.
        rows: usize,
    },
    /// Current shared input-check phase.
    CheckingInputs {
        /// Machine-readable phase name.
        stage: String,
    },
    /// Factor execution progress, forwarded from `analytics-factor`.
    Factors(FactorEvent),
    /// Training progress, forwarded from the LightGBM trainer.
    Training(TrainEvent),
    /// Final artifacts are being written to disk.
    WritingArtifacts,
}

/// Output paths and row counts of a finished run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineArtifact {
    /// Wide factor cache Parquet; `None` for cache-only retraining runs.
    pub factor_cache_path: Option<PathBuf>,
    /// Factor run manifest JSON; `None` for cache-only retraining runs.
    pub factor_manifest_path: Option<PathBuf>,
    /// Model, manifest, metrics, and feature-importance paths.
    pub training: TrainingArtifact,
    /// Test-split predictions Parquet.
    pub predictions_path: PathBuf,
    /// Rows in the training split.
    pub train_rows: usize,
    /// Rows in the validation split.
    pub validation_rows: usize,
    /// Rows in the test split.
    pub test_rows: usize,
}

/// Errors surfaced by pipeline orchestration.
#[derive(Debug, Error)]
pub enum PipelineError {
    /// The run was cancelled through the shared flag.
    #[error("pipeline cancelled")]
    Cancelled,
    /// Invalid request or inconsistent intermediate data.
    #[error("invalid pipeline input: {0}")]
    InvalidInput(String),
    /// DuckDB engine failure.
    #[error("engine error: {0}")]
    Engine(#[from] analytics_engine::EngineError),
    /// Factor execution failure.
    #[error("factor error: {0}")]
    Factor(#[from] analytics_factor::FactorError),
    /// LightGBM training or prediction failure.
    #[error("ml error: {0}")]
    Ml(#[from] crate::research::MlError),
    /// Filesystem failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON (de)serialization failure.
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    /// Arrow/Parquet failure.
    #[error("arrow error: {0}")]
    Arrow(String),
}

/// Convenience result alias for pipeline calls.
pub type Result<T> = std::result::Result<T, PipelineError>;

/// Shared cancellation flag type for pipeline callers.
pub type Cancellation = Arc<AtomicBool>;

/// Creates a fresh, unset cancellation flag.
pub fn cancellation() -> Cancellation {
    Arc::new(AtomicBool::new(false))
}

/// Runs the full market-to-model pipeline and writes all artifacts under
/// `request.output_dir`.
///
/// Progress is streamed through `progress`; `cancel` is honored between
/// stages and inside the factor executor and trainer loops.
pub fn run_factor_pipeline(
    request: &PipelineRequest,
    progress: Sender<PipelineEvent>,
    cancel: Cancellation,
) -> Result<PipelineArtifact> {
    request
        .date_range
        .validate()
        .map_err(|error| PipelineError::InvalidInput(error.to_string()))?;
    if request.factor_ids.is_empty() {
        return Err(PipelineError::InvalidInput("no factors selected".into()));
    }
    std::fs::create_dir_all(&request.output_dir)?;
    check_cancel(&cancel)?;

    let _ = progress.send(PipelineEvent::LoadingMarket);
    let mut engine = DuckDbEngine::open_in_memory()?;
    let (rows, members) = if request.factor_refs.is_empty() {
        let rows = engine.load_market_rows(
            &request.market_path,
            &request.universe_path,
            &request.universe_id,
            &request.date_range,
        )?;
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|r| {
                r.is_trading.unwrap_or(true) && r.listing_date.is_none_or(|d| d <= r.trade_date)
            })
            .collect();
        let members = vec![true; rows.len()];
        (rows, members)
    } else {
        engine.load_factor_research_rows(
            &request.market_path,
            &request.universe_path,
            &request.universe_id,
            &DateRange {
                start: "1900-01-01".parse().unwrap(),
                end: request.date_range.end,
            },
        )?
    };
    let eligible: Vec<_> = rows
        .iter()
        .zip(&members)
        .map(|(r, m)| {
            *m && r.is_trading.unwrap_or(true) && r.listing_date.is_none_or(|d| d <= r.trade_date)
        })
        .collect();
    if rows.is_empty() {
        return Err(PipelineError::InvalidInput(
            "market/universe join returned no rows".into(),
        ));
    }
    let _ = progress.send(PipelineEvent::MarketLoaded { rows: rows.len() });
    check_cancel(&cancel)?;

    let mut panel = panel_from_rows(&rows);
    if !request.factor_refs.is_empty() {
        panel.columns.insert(
            "__universe_member".into(),
            eligible
                .iter()
                .map(|m| Some(if *m { 1. } else { 0. }))
                .collect(),
        );
    }
    let registry = FactorRegistry::resolve(
        request.factor_set,
        &request.factor_refs,
        &request.factor_ids,
    )?;
    let input_columns =
        crate::input_check::parquet::market_columns(&mut engine, &request.market_path)?;
    let absent = crate::input_check::parquet::missing_inputs(
        &registry,
        &request.factor_ids,
        &input_columns,
        &mut panel,
    )?;
    let formula_version = registry.selection_version(&request.factor_ids)?;
    let snapshot = snapshot_hash(&request.market_path, &request.universe_path)?;
    let labels = next_return_labels(
        &panel.instrument_id,
        &panel.trade_date,
        &panel.columns["adj_close"],
    )?;
    let calendar = panel
        .trade_date
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let expected = rows
        .iter()
        .zip(&eligible)
        .filter(|(_, m)| **m)
        .map(|(r, _)| (r.instrument_id.clone(), r.trade_date))
        .collect();
    let mut prepared = crate::input_check::parquet::Prepared {
        rows: rows.clone(),
        eligible: eligible.clone(),
        panel: panel.clone(),
        registry,
        calendar,
        labels,
        input_hash: snapshot.clone(),
        absent,
        expected,
        cache_context: serde_json::json!({"adapter":if request.factor_refs.is_empty(){"pipeline-legacy-v1"}else{"parquet-history-v1"},"price_basis":"adjusted_with_raw_fallback","data":snapshot,"horizon":1,"policy":analytics_core::research_quality::ResearchQualityPolicy::default()}),
    };
    let check_request = crate::input_check::CheckRequest {
        expected_input_hash: Some(snapshot),
        expected_source_stamp: None,
        input: request.clone().into(),
        horizon: 1,
        policy: Default::default(),
    };
    let (report, artifact) = crate::input_check::parquet::run_prepared(
        &check_request,
        &mut prepared,
        progress.clone(),
        cancel.clone(),
    )?;
    if !report.ready() {
        return Err(PipelineError::InvalidInput(
            "data_not_ready: input check failed; see input-check.json".into(),
        ));
    }
    let mut factor_artifact = artifact
        .ok_or_else(|| PipelineError::InvalidInput("factor computation did not complete".into()))?;
    crate::atomic_json(
        &request.output_dir.join("factor-receipt.json"),
        &report.artifact,
    )
    .map_err(|e| PipelineError::InvalidInput(e.to_string()))?;
    check_cancel(&cancel)?;

    let _ = progress.send(PipelineEvent::WritingArtifacts);
    let adjusted_close = panel
        .columns
        .get("adj_close")
        .ok_or_else(|| PipelineError::InvalidInput("adj_close missing".into()))?;
    let mut labels = next_return_labels(&panel.instrument_id, &panel.trade_date, adjusted_close)?;
    if !request.factor_refs.is_empty() {
        let selected: Vec<_> = panel
            .trade_date
            .iter()
            .enumerate()
            .filter_map(|(i, d)| (request.date_range.contains(*d) && eligible[i]).then_some(i))
            .collect();
        if selected.is_empty() {
            return Err(PipelineError::InvalidInput(
                "requested dates contain no eligible members".into(),
            ));
        }
        labels = selected.iter().map(|&i| labels[i]).collect();
        for values in factor_artifact.values.values_mut() {
            *values = selected.iter().map(|&i| values[i]).collect();
        }
        for values in panel.columns.values_mut() {
            *values = selected.iter().map(|&i| values[i]).collect();
        }
        panel.instrument_id = selected
            .iter()
            .map(|&i| panel.instrument_id[i].clone())
            .collect();
        panel.trade_date = selected.iter().map(|&i| panel.trade_date[i]).collect();
        factor_artifact.rows = selected.len();
        let mut manifest: serde_json::Value = serde_json::from_str(&factor_artifact.manifest_json)?;
        manifest["rows"] = selected.len().into();
        factor_artifact.manifest_json = serde_json::to_string(&manifest)?;
    }
    let factor_cache = request.output_dir.join("factors.parquet");
    factor_artifact.write_parquet_with_labels(
        &factor_cache,
        &panel.instrument_id,
        &panel.trade_date,
        &request.universe_id,
        Some(&labels),
    )?;
    let snapshot = snapshot_hash(&request.market_path, &request.universe_path)?;
    let factor_manifest =
        write_factor_manifest(request, &factor_artifact, &factor_cache, &snapshot)?;
    check_cancel(&cancel)?;

    let split_bounds = chronological_date_split(&panel.trade_date)
        .map_err(|error| PipelineError::InvalidInput(error.to_string()))?;
    let prepared = build_training_split(
        &factor_artifact,
        &request.factor_ids,
        &labels,
        &panel.instrument_id,
        &panel.trade_date,
        &split_bounds,
    );
    finish_training(
        &prepared,
        &request.factor_ids,
        &request.universe_id,
        &request.date_range,
        &request.training,
        &request.output_dir,
        &split_bounds,
        Some(snapshot.as_str()),
        progress,
        cancel,
    )
    .and_then(|mut artifact| {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&artifact.training.manifest_path)?)?;
        manifest["schema_version"] = 3.into();
        manifest["factor_definitions"] = serde_json::from_str::<serde_json::Value>(
            &factor_artifact.manifest_json,
        )?["factor_definitions"]
            .clone();
        manifest["formula_version"] = formula_version.into();
        manifest["factor_set"] = serde_json::to_value(request.factor_set)?;
        manifest["factor_refs"] = serde_json::to_value(analytics_factor::selection::frozen_refs(
            request.factor_set,
            &request.factor_refs,
            &request.factor_ids,
        ))?;
        manifest["training_schema_hash"] =
            analytics_factor::selection::hash(&(&manifest["features"], &manifest["factor_refs"]))?
                .into();
        manifest["factor_definition_signature"] =
            analytics_factor::selection::hash(&request.factor_refs)?.into();
        std::fs::write(
            &artifact.training.manifest_path,
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        artifact.factor_cache_path = Some(factor_cache);
        artifact.factor_manifest_path = Some(factor_manifest);
        Ok(artifact)
    })
}

/// A parsed factor cache Parquet, as written by
/// [`FactorRunArtifact::write_parquet_with_labels`].
#[derive(Debug, Clone)]
pub struct FactorCache {
    /// Instrument per row.
    pub instrument_ids: Vec<String>,
    /// Trade date per row.
    pub trade_dates: Vec<Date>,
    /// Universe stored in the cache, when present.
    pub universe_id: Option<String>,
    /// Factor column names in file order.
    pub factor_ids: Vec<String>,
    /// Nullable factor values keyed by factor ID.
    pub values: BTreeMap<String, Vec<Option<f64>>>,
    /// `label_next_return` column when the cache carries labels.
    pub labels: Option<Vec<Option<f64>>>,
}

/// Loads a factor cache Parquet previously written by the pipeline.
///
/// Returns an error when required key columns are missing or have unexpected
/// physical types.
pub fn load_factor_cache(path: &Path) -> Result<FactorCache> {
    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|error| PipelineError::Arrow(error.to_string()))?;
    let reader = builder
        .build()
        .map_err(|error| PipelineError::Arrow(error.to_string()))?;
    let mut instrument_ids = Vec::new();
    let mut trade_dates = Vec::new();
    let mut universe_id: Option<String> = None;
    let mut factor_ids: Vec<String> = Vec::new();
    let mut values: BTreeMap<String, Vec<Option<f64>>> = BTreeMap::new();
    let mut labels: Option<Vec<Option<f64>>> = None;
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid epoch");
    for batch in reader {
        let batch = batch.map_err(|error| PipelineError::Arrow(error.to_string()))?;
        let schema = batch.schema();
        for (index, field) in schema.fields().iter().enumerate() {
            let name = field.name().as_str();
            match name {
                "instrument_id" => {
                    let column = string_column(&batch, index, name)?;
                    for row in 0..batch.num_rows() {
                        instrument_ids.push(column.value(row).to_string());
                    }
                }
                "trade_date" => {
                    let column = batch
                        .column(index)
                        .as_any()
                        .downcast_ref::<Date32Array>()
                        .ok_or_else(|| PipelineError::Arrow("trade_date must be Date32".into()))?;
                    for row in 0..batch.num_rows() {
                        trade_dates
                            .push(epoch + chrono::Duration::days(i64::from(column.value(row))));
                    }
                }
                "universe_id" => {
                    if universe_id.is_none() && batch.num_rows() > 0 {
                        let column = string_column(&batch, index, name)?;
                        universe_id = Some(column.value(0).to_string());
                    }
                }
                "label_next_return" => {
                    let column = float_column(&batch, index, name)?;
                    let entry = labels.get_or_insert_with(Vec::new);
                    for row in 0..batch.num_rows() {
                        entry.push(if column.is_null(row) {
                            None
                        } else {
                            Some(column.value(row))
                        });
                    }
                }
                factor => {
                    if !factor_ids.iter().any(|id| id == factor) {
                        factor_ids.push(factor.to_string());
                    }
                    let column = float_column(&batch, index, factor)?;
                    let entry = values.entry(factor.to_string()).or_default();
                    for row in 0..batch.num_rows() {
                        entry.push(if column.is_null(row) {
                            None
                        } else {
                            Some(column.value(row))
                        });
                    }
                }
            }
        }
    }
    if instrument_ids.is_empty() || trade_dates.len() != instrument_ids.len() {
        return Err(PipelineError::InvalidInput(format!(
            "factor cache {} has no instrument_id/trade_date rows",
            path.display()
        )));
    }
    if factor_ids.is_empty() {
        return Err(PipelineError::InvalidInput(format!(
            "factor cache {} has no factor columns",
            path.display()
        )));
    }
    Ok(FactorCache {
        instrument_ids,
        trade_dates,
        universe_id,
        factor_ids,
        values,
        labels,
    })
}

/// Retrains a model from an existing factor cache with fresh
/// hyperparameters, writing model artifacts and test predictions into
/// `output_dir`.
///
/// The cache must contain a `label_next_return` column. `universe_id`
/// overrides the value stored in the cache when provided.
pub fn train_from_factor_cache(
    cache_path: &Path,
    universe_id: Option<&str>,
    output_dir: &Path,
    config: &TrainingConfig,
    progress: Sender<PipelineEvent>,
    cancel: Cancellation,
) -> Result<PipelineArtifact> {
    let cache = load_factor_cache(cache_path)?;
    let labels = cache.labels.clone().ok_or_else(|| {
        PipelineError::InvalidInput(format!(
            "factor cache {} has no label_next_return column",
            cache_path.display()
        ))
    })?;
    let universe_id = universe_id
        .map(str::to_string)
        .or_else(|| cache.universe_id.clone())
        .unwrap_or_else(|| "unknown".into());
    let date_range = DateRange {
        start: *cache.trade_dates.iter().min().expect("non-empty cache"),
        end: *cache.trade_dates.iter().max().expect("non-empty cache"),
    };
    std::fs::create_dir_all(output_dir)?;
    check_cancel(&cancel)?;
    let artifact = FactorRunArtifact {
        factor_ids: cache.factor_ids.clone(),
        rows: cache.instrument_ids.len(),
        values: cache.values.clone(),
        manifest_json: String::new(),
    };
    let split_bounds = chronological_date_split(&cache.trade_dates)
        .map_err(|error| PipelineError::InvalidInput(error.to_string()))?;
    let prepared = build_training_split(
        &artifact,
        &cache.factor_ids,
        &labels,
        &cache.instrument_ids,
        &cache.trade_dates,
        &split_bounds,
    );
    let source_path = cache_path.with_file_name("factor_run_manifest.json");
    let source_provenance = if source_path.is_file() {
        let source: serde_json::Value = serde_json::from_slice(&std::fs::read(&source_path)?)?;
        let family = serde_json::from_value(source["factor_set"].clone())?;
        let refs: Vec<analytics_factor::selection::FactorRef> = source
            .get("factor_refs")
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()?
            .unwrap_or_default();
        let registry = FactorRegistry::resolve(family, &refs, &cache.factor_ids)?;
        if source["formula_version"] != registry.selection_version(&cache.factor_ids)?
            || source["factor_ids"] != serde_json::to_value(&cache.factor_ids)?
        {
            return Err(PipelineError::InvalidInput(
                "factor cache provenance/order mismatch".into(),
            ));
        }
        if let Some(expected) = source.get("factor_cache_sha256") {
            let actual = format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(std::fs::read(cache_path)?))
            );
            if expected != &actual {
                return Err(PipelineError::InvalidInput(
                    "factor cache content changed".into(),
                ));
            }
        }
        Some(source)
    } else {
        None
    };
    finish_training(
        &prepared,
        &cache.factor_ids,
        &universe_id,
        &date_range,
        config,
        output_dir,
        &split_bounds,
        None,
        progress,
        cancel,
    )
    .and_then(|artifact| {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&artifact.training.manifest_path)?)?;
        manifest["factor_cache_sha256"] = format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(std::fs::read(cache_path)?))
        )
        .into();
        if let Some(source) = source_provenance {
            if source.get("factor_refs").is_some() {
                manifest["schema_version"] = 3.into();
                manifest["factor_refs"] = source["factor_refs"].clone();
                let family = serde_json::from_value(source["factor_set"].clone())?;
                let refs: Vec<analytics_factor::selection::FactorRef> =
                    serde_json::from_value(source["factor_refs"].clone())?;
                manifest["factor_definitions"] =
                    FactorRegistry::resolve(family, &refs, &cache.factor_ids)?
                        .manifest_definitions(&cache.factor_ids)?;
            }
            manifest["formula_version"] = source["formula_version"].clone();
            manifest["factor_set"] = source["factor_set"].clone();
            if manifest["schema_version"] == 3 {
                manifest["training_schema_hash"] = analytics_factor::selection::hash(&(
                    &manifest["features"],
                    &manifest["factor_refs"],
                ))?
                .into();
            }
            manifest["factor_provenance"] = source;
        }
        std::fs::write(
            &artifact.training.manifest_path,
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        Ok(artifact)
    })
}

/// Trains, predicts on the test split, and writes all model artifacts.
#[allow(clippy::too_many_arguments)]
fn finish_training(
    prepared: &SplitOutput,
    factor_ids: &[String],
    universe_id: &str,
    date_range: &DateRange,
    config: &TrainingConfig,
    output_dir: &Path,
    split_bounds: &analytics_core::DateSplit,
    input_snapshot_hash: Option<&str>,
    progress: Sender<PipelineEvent>,
    cancel: Cancellation,
) -> Result<PipelineArtifact> {
    let request = TrainRequest {
        factor_ids: factor_ids.to_vec(),
        universe_id: universe_id.to_string(),
        date_range: date_range.clone(),
        objective: Objective::Regression,
        parameters: config.parameters.clone(),
        early_stopping_rounds: config.early_stopping_rounds,
    };
    let train_progress = forward_events(&progress, PipelineEvent::Training);
    let training = LightGbmTrainer.train_split(
        &request,
        &prepared.split,
        output_dir,
        train_progress,
        cancel.clone(),
    )?;
    check_cancel(&cancel)?;
    annotate_model_manifest(&training.manifest_path, split_bounds, input_snapshot_hash)?;

    let _ = progress.send(PipelineEvent::WritingArtifacts);
    let predictor = LightGbmPredictor {
        model_path: training.model_path.clone(),
    };
    let predictions = predictor.predict_dense(
        &prepared.split.test.features,
        prepared.split.test.rows,
        prepared.split.test.columns,
        false,
    )?;
    let predictions_path = output_dir.join("predictions.parquet");
    write_predictions(
        &predictions_path,
        &prepared.test_rows,
        &predictions.predictions,
    )?;
    Ok(PipelineArtifact {
        factor_cache_path: None,
        factor_manifest_path: None,
        training,
        predictions_path,
        train_rows: prepared.split.train.rows,
        validation_rows: prepared.split.validation.rows,
        test_rows: prepared.split.test.rows,
    })
}

/// Builds the factor panel layout expected by the executor from normalized
/// market rows. Adjusted prices feed both the raw and `adj_*` columns, and
/// `ret`/`vwap` are derived per instrument.
pub fn panel_from_rows(rows: &[MarketRow]) -> Panel {
    let mut columns: HashMap<String, Vec<Option<f64>>> = HashMap::new();
    let mut instrument_id = Vec::with_capacity(rows.len());
    let mut trade_date = Vec::with_capacity(rows.len());
    let mut previous_close: HashMap<String, f64> = HashMap::new();
    for row in rows {
        instrument_id.push(row.instrument_id.clone());
        trade_date.push(row.trade_date);
        for (name, value) in [
            ("open", row.adj_open),
            ("high", row.adj_high),
            ("low", row.adj_low),
            ("close", row.adj_close),
            ("adj_open", row.adj_open),
            ("adj_high", row.adj_high),
            ("adj_low", row.adj_low),
            ("adj_close", row.adj_close),
            ("volume", row.volume),
            ("amount", row.amount),
            (
                "vwap",
                row.amount
                    .zip(row.volume)
                    .and_then(|(amount, volume)| (volume != 0.0).then_some(amount / volume)),
            ),
        ] {
            columns.entry(name.into()).or_default().push(value);
        }
        let return_value = row.adj_close.and_then(|close| {
            previous_close
                .insert(row.instrument_id.clone(), close)
                .map(|previous| close / previous - 1.0)
        });
        columns.entry("ret".into()).or_default().push(return_value);
    }
    Panel {
        instrument_id,
        trade_date,
        columns,
    }
}

/// Dense train/validation/test matrices plus the test-split row keys used
/// for prediction output.
pub struct SplitOutput {
    /// Chronologically split dense matrices.
    pub split: TrainingSplit,
    /// `(instrument_id, trade_date)` per test row.
    pub test_rows: Vec<(String, Date)>,
}

/// Assembles dense matrices from computed factor values and labels. Rows
/// without a label are skipped; missing factor values become `NaN`, which the
/// LightGBM adapter maps to native missing values.
pub fn build_training_split(
    artifact: &FactorRunArtifact,
    factor_ids: &[String],
    labels: &[Option<f64>],
    instruments: &[String],
    dates: &[Date],
    bounds: &analytics_core::DateSplit,
) -> SplitOutput {
    let mut rows = [Vec::new(), Vec::new(), Vec::new()];
    for (index, date) in dates.iter().enumerate() {
        let Some(label) = labels[index] else {
            continue;
        };
        let bucket = if *date >= bounds.train.start && *date <= bounds.train.end {
            0
        } else if *date >= bounds.validation.start && *date <= bounds.validation.end {
            1
        } else if *date >= bounds.test.start && *date <= bounds.test.end {
            2
        } else {
            continue;
        };
        rows[bucket].push((index, label));
    }
    let matrix = |items: &[(usize, f64)]| DenseMatrix {
        features: items
            .iter()
            .flat_map(|(index, _)| {
                factor_ids
                    .iter()
                    .map(move |id| artifact.values[id][*index].unwrap_or(f64::NAN))
            })
            .collect(),
        labels: items.iter().map(|(_, label)| *label as f32).collect(),
        rows: items.len(),
        columns: factor_ids.len(),
    };
    let test_rows = rows[2]
        .iter()
        .map(|(index, _)| (instruments[*index].clone(), dates[*index]))
        .collect();
    SplitOutput {
        split: TrainingSplit {
            train: matrix(&rows[0]),
            validation: matrix(&rows[1]),
            test: matrix(&rows[2]),
        },
        test_rows,
    }
}

fn write_predictions(path: &Path, rows: &[(String, Date)], predictions: &[Vec<f64>]) -> Result<()> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("instrument_id", ArrowDataType::Utf8, false),
        Field::new("trade_date", ArrowDataType::Date32, false),
        Field::new("prediction", ArrowDataType::Float64, false),
    ]));
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(
                rows.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            )) as ArrayRef,
            Arc::new(Date32Array::from(
                rows.iter()
                    .map(|(_, date)| (*date - epoch).num_days() as i32)
                    .collect::<Vec<_>>(),
            )) as ArrayRef,
            Arc::new(Float64Array::from(
                predictions.iter().map(|row| row[0]).collect::<Vec<_>>(),
            )) as ArrayRef,
        ],
    )
    .map_err(|error| PipelineError::Arrow(error.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut writer = ArrowWriter::try_new(File::create(path)?, schema, None)
        .map_err(|error| PipelineError::Arrow(error.to_string()))?;
    writer
        .write(&batch)
        .map_err(|error| PipelineError::Arrow(error.to_string()))?;
    writer
        .close()
        .map_err(|error| PipelineError::Arrow(error.to_string()))?;
    Ok(())
}

fn write_factor_manifest(
    request: &PipelineRequest,
    artifact: &FactorRunArtifact,
    cache: &Path,
    input_snapshot_hash: &str,
) -> Result<PathBuf> {
    let mut value: serde_json::Value = serde_json::from_str(&artifact.manifest_json)?;
    value["schema_version"] = 2.into();
    value["factor_refs"] = serde_json::to_value(analytics_factor::selection::frozen_refs(
        request.factor_set,
        &request.factor_refs,
        &request.factor_ids,
    ))?;
    value["factor_set"] = serde_json::to_value(request.factor_set)?;
    value["market_path"] = request.market_path.to_string_lossy().to_string().into();
    value["universe_path"] = request.universe_path.to_string_lossy().to_string().into();
    value["universe_id"] = request.universe_id.clone().into();
    value["date_range"] = serde_json::to_value(&request.date_range)?;
    value["factor_cache"] = cache.to_string_lossy().to_string().into();
    value["factor_cache_sha256"] = format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(std::fs::read(cache)?))
    )
    .into();
    value["input_snapshot_hash"] = input_snapshot_hash.into();
    value["price_mode"] = "adjusted_price".into();
    value["missing_value_rule"] = "preserve_null".into();
    let registry = FactorRegistry::resolve(
        request.factor_set,
        &request.factor_refs,
        &request.factor_ids,
    )?;
    value["max_lookback"] = request
        .factor_ids
        .iter()
        .filter_map(|id| registry.get(id).map(|spec| spec.lookback))
        .max()
        .unwrap_or_default()
        .into();
    value["requires_recursive_state"] = request
        .factor_ids
        .iter()
        .filter_map(|id| registry.get(id))
        .any(|spec| spec.requires_recursive_state)
        .into();
    let path = cache.with_file_name("factor_run_manifest.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&value)?)?;
    Ok(path)
}

fn annotate_model_manifest(
    path: &Path,
    bounds: &analytics_core::DateSplit,
    input_snapshot_hash: Option<&str>,
) -> Result<()> {
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    value["split_boundaries"] = serde_json::json!({
        "train_start": bounds.train.start,
        "train_end": bounds.train.end,
        "validation_start": bounds.validation.start,
        "validation_end": bounds.validation.end,
        "test_start": bounds.test.start,
        "test_end": bounds.test.end,
    });
    if let Some(hash) = input_snapshot_hash {
        value["input_snapshot_hash"] = hash.into();
    }
    std::fs::write(path, serde_json::to_vec_pretty(&value)?)?;
    Ok(())
}

/// Hashes the sorted Parquet inputs of a run so manifests can be tied back to
/// the exact data snapshot that produced them.
pub fn snapshot_hash(market: &Path, universe: &Path) -> Result<String> {
    let mut files = Vec::new();
    for root in [market, universe] {
        if root.is_file() {
            files.push(root.to_path_buf());
        } else {
            for entry in walkdir(root)? {
                if entry
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("parquet"))
                {
                    files.push(entry);
                }
            }
        }
    }
    files.sort();
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(file.to_string_lossy().as_bytes());
        let mut input = File::open(file)?;
        std::io::copy(&mut input, &mut HashWriter(&mut hasher))?;
    }
    Ok(format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(hasher.finalize())
    ))
}

struct HashWriter<'a>(&'a mut Sha256);

impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn walkdir(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut output = Vec::new();
    if !root.is_dir() {
        return Ok(output);
    }
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            output.extend(walkdir(&path)?);
        } else {
            output.push(path);
        }
    }
    Ok(output)
}

fn string_column<'a>(batch: &'a RecordBatch, index: usize, name: &str) -> Result<&'a StringArray> {
    batch
        .column(index)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| PipelineError::Arrow(format!("{name} must be Utf8")))
}

fn float_column<'a>(batch: &'a RecordBatch, index: usize, name: &str) -> Result<&'a Float64Array> {
    batch
        .column(index)
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| PipelineError::Arrow(format!("{name} must be Float64")))
}

fn check_cancel(cancel: &Cancellation) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(PipelineError::Cancelled)
    } else {
        Ok(())
    }
}

/// Spawns a short-lived forwarder so stage-specific events reach the caller
/// as [`PipelineEvent`] while the stage runs on this thread.
fn forward_events<T, F>(progress: &Sender<PipelineEvent>, wrap: F) -> Sender<T>
where
    T: Send + 'static,
    F: Fn(T) -> PipelineEvent + Send + 'static,
{
    let (tx, rx) = channel::<T>();
    let progress = progress.clone();
    std::thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            if progress.send(wrap(event)).is_err() {
                break;
            }
        }
    });
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(instrument: &str, day: i64, close: f64) -> MarketRow {
        let date = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap() + chrono::Duration::days(day);
        MarketRow {
            instrument_id: instrument.into(),
            trade_date: date,
            open: Some(close),
            high: Some(close),
            low: Some(close),
            close: Some(close),
            volume: Some(1_000.0),
            amount: Some(1_000.0 * close),
            adj_open: Some(close),
            adj_high: Some(close),
            adj_low: Some(close),
            adj_close: Some(close),
            is_trading: Some(true),
            listing_date: Some(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap()),
        }
    }

    #[test]
    fn panel_from_rows_derives_vwap_and_returns() {
        let rows = vec![
            row("A", 0, 10.0),
            row("A", 1, 11.0),
            row("B", 0, 20.0),
            row("B", 1, 22.0),
        ];
        let panel = panel_from_rows(&rows);
        assert_eq!(panel.instrument_id.len(), 4);
        assert_eq!(panel.columns["vwap"][0], Some(10.0));
        let ret = &panel.columns["ret"];
        assert_eq!(ret[0], None);
        assert!((ret[1].unwrap() - 0.1).abs() < 1e-9);
        assert_eq!(ret[2], None);
    }

    #[test]
    fn build_training_split_skips_unlabeled_rows() {
        let dates: Vec<Date> = (0..10)
            .map(|day| NaiveDate::from_ymd_opt(2024, 1, 1).unwrap() + chrono::Duration::days(day))
            .collect();
        let instruments: Vec<String> = dates.iter().map(|_| "A".to_string()).collect();
        let labels: Vec<Option<f64>> = (0..10).map(|index| (index < 9).then_some(0.01)).collect();
        let artifact = FactorRunArtifact {
            factor_ids: vec!["alpha_001".into()],
            rows: 10,
            values: BTreeMap::from([(
                "alpha_001".to_string(),
                (0..10).map(|v| Some(v as f64)).collect(),
            )]),
            manifest_json: String::new(),
        };
        let bounds = chronological_date_split(&dates).unwrap();
        let prepared = build_training_split(
            &artifact,
            &["alpha_001".to_string()],
            &labels,
            &instruments,
            &dates,
            &bounds,
        );
        assert_eq!(prepared.split.train.columns, 1);
        assert_eq!(
            prepared.split.train.rows + prepared.split.validation.rows + prepared.split.test.rows,
            9
        );
        assert_eq!(prepared.test_rows.len(), prepared.split.test.rows);
    }

    #[test]
    fn cancellation_flag_starts_unset() {
        let flag = cancellation();
        assert!(check_cancel(&flag).is_ok());
        flag.store(true, Ordering::Relaxed);
        assert!(matches!(check_cancel(&flag), Err(PipelineError::Cancelled)));
    }

    /// Writes a synthetic market/universe pair, runs the full native pipeline,
    /// and asserts every artifact lands on disk. Requires the native DuckDB
    /// and LightGBM libraries at link time.
    #[test]
    fn end_to_end_pipeline_on_synthetic_data() {
        for family in [
            analytics_core::research_quality::FactorSet::Gtja191,
            analytics_core::research_quality::FactorSet::Alpha158,
        ] {
            use arrow_array::{ArrayRef, BooleanArray, Date32Array, Float64Array, StringArray};

            let dir = tempfile::tempdir().unwrap();
            let market = dir.path().join("market");
            let universe = dir.path().join("universe");
            std::fs::create_dir_all(&market).unwrap();
            std::fs::create_dir_all(&universe).unwrap();

            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            let start = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
            let instruments = ["AAA", "BBB", "CCC"];
            let days = 30_i64;
            let mut ids = Vec::new();
            let mut dates = Vec::new();
            let mut prices = Vec::new();
            for (offset, instrument) in instruments.iter().enumerate() {
                for day in 0..days {
                    ids.push(instrument.to_string());
                    dates.push(((start + chrono::Duration::days(day)) - epoch).num_days() as i32);
                    prices.push(10.0 + offset as f64 + (day as f64) * 0.05);
                }
            }
            let write_parquet = |path: &Path, schema: Arc<Schema>, batch: RecordBatch| {
                let file = File::create(path).unwrap();
                let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
                writer.write(&batch).unwrap();
                writer.close().unwrap();
            };
            let listing: Vec<i32> = ids
                .iter()
                .map(|_| ((start - epoch).num_days() - 10) as i32)
                .collect();
            let market_schema = Arc::new(Schema::new(vec![
                Field::new("instrument_id", ArrowDataType::Utf8, false),
                Field::new("trade_date", ArrowDataType::Date32, false),
                Field::new("open", ArrowDataType::Float64, true),
                Field::new("high", ArrowDataType::Float64, true),
                Field::new("low", ArrowDataType::Float64, true),
                Field::new("close", ArrowDataType::Float64, true),
                Field::new("volume", ArrowDataType::Float64, true),
                Field::new("amount", ArrowDataType::Float64, true),
                Field::new("adj_open", ArrowDataType::Float64, true),
                Field::new("adj_high", ArrowDataType::Float64, true),
                Field::new("adj_low", ArrowDataType::Float64, true),
                Field::new("adj_close", ArrowDataType::Float64, true),
                Field::new("is_trading", ArrowDataType::Boolean, true),
                Field::new("listing_date", ArrowDataType::Date32, true),
            ]));
            let price_column = || -> ArrayRef { Arc::new(Float64Array::from(prices.clone())) };
            let market_batch = RecordBatch::try_new(
                market_schema.clone(),
                vec![
                    Arc::new(StringArray::from(ids.clone())) as ArrayRef,
                    Arc::new(Date32Array::from(dates.clone())) as ArrayRef,
                    price_column(),
                    price_column(),
                    price_column(),
                    price_column(),
                    Arc::new(Float64Array::from(vec![Some(1_000.0); ids.len()])) as ArrayRef,
                    Arc::new(Float64Array::from(
                        prices.iter().map(|p| Some(p * 1_000.0)).collect::<Vec<_>>(),
                    )) as ArrayRef,
                    price_column(),
                    price_column(),
                    price_column(),
                    price_column(),
                    Arc::new(BooleanArray::from(vec![Some(true); ids.len()])) as ArrayRef,
                    Arc::new(Date32Array::from(listing)) as ArrayRef,
                ],
            )
            .unwrap();
            write_parquet(&market.join("part.parquet"), market_schema, market_batch);

            let universe_schema = Arc::new(Schema::new(vec![
                Field::new("instrument_id", ArrowDataType::Utf8, false),
                Field::new("universe_id", ArrowDataType::Utf8, false),
                Field::new("trade_date", ArrowDataType::Date32, false),
                Field::new("is_member", ArrowDataType::Boolean, false),
            ]));
            let first_day = ((start - epoch).num_days()) as i32;
            let universe_batch = RecordBatch::try_new(
                universe_schema.clone(),
                vec![
                    Arc::new(StringArray::from(
                        instruments
                            .iter()
                            .map(|id| id.to_string())
                            .collect::<Vec<_>>(),
                    )) as ArrayRef,
                    Arc::new(StringArray::from(vec!["test-univ"; instruments.len()])) as ArrayRef,
                    Arc::new(Date32Array::from(vec![first_day; instruments.len()])) as ArrayRef,
                    Arc::new(BooleanArray::from(vec![true; instruments.len()])) as ArrayRef,
                ],
            )
            .unwrap();
            write_parquet(
                &universe.join("part.parquet"),
                universe_schema,
                universe_batch,
            );

            let output = dir.path().join("runs");
            let (tx, rx) = channel();
            std::thread::spawn(move || while rx.recv().is_ok() {});
            let artifact = run_factor_pipeline(
                &PipelineRequest {
                    factor_refs: Vec::new(),
                    factor_set: family,
                    market_path: market,
                    universe_path: universe,
                    universe_id: "test-univ".into(),
                    date_range: DateRange {
                        start,
                        end: start + chrono::Duration::days(days - 1),
                    },
                    factor_ids: vec![if matches!(
                        family,
                        analytics_core::research_quality::FactorSet::Gtja191
                    ) {
                        "alpha_001".into()
                    } else {
                        "alpha158_014_ROC5".into()
                    }],
                    output_dir: output.clone(),
                    training: TrainingConfig {
                        parameters: BTreeMap::from([
                            ("num_iterations".into(), "5".into()),
                            ("num_leaves".into(), "7".into()),
                            ("min_data_in_leaf".into(), "2".into()),
                        ]),
                        early_stopping_rounds: None,
                    },
                },
                tx,
                cancellation(),
            )
            .expect("pipeline succeeds on synthetic data");

            assert!(output.join("factors.parquet").exists());
            assert!(output.join("factor_run_manifest.json").exists());
            assert!(artifact.training.model_path.exists());
            assert!(artifact.training.metrics_path.exists());
            assert!(artifact.predictions_path.exists());
            assert!(artifact.train_rows > 0);
            assert!(artifact.test_rows > 0);
            let metrics: serde_json::Value =
                serde_json::from_slice(&std::fs::read(artifact.training.metrics_path).unwrap())
                    .unwrap();
            assert!(metrics["iterations"].as_u64().unwrap_or_default() > 0);

            // Cache retraining reuses the same artifacts without market data.
            let (tx2, rx2) = channel();
            std::thread::spawn(move || while rx2.recv().is_ok() {});
            let retrain = train_from_factor_cache(
                &output.join("factors.parquet"),
                None,
                &dir.path().join("retrain"),
                &TrainingConfig {
                    parameters: BTreeMap::from([
                        ("num_iterations".into(), "3".into()),
                        ("min_data_in_leaf".into(), "2".into()),
                    ]),
                    early_stopping_rounds: None,
                },
                tx2,
                cancellation(),
            )
            .expect("retraining from cache succeeds");
            let manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&retrain.training.manifest_path).unwrap())
                    .unwrap();
            assert_eq!(
                manifest["formula_version"],
                analytics_factor::alpha158::version(family)
            );
            assert!(retrain.training.model_path.exists());
            assert!(retrain.predictions_path.exists());
        }
    }
}
