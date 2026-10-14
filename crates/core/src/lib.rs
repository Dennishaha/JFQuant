//! Shared domain types and contracts for the analytics workspace.
//!
//! `analytics-core` sits at the bottom of the dependency graph: storage,
//! engine, factor, ml, and the desktop app all speak these types, and this
//! crate intentionally has no DuckDB, egui, or LightGBM dependency.
//!
//! ```text
//! storage -> core :: DatasetFile / ColumnSchema
//! engine  -> core :: PageRequest / QueryPage / ExportRequest
//! factor  -> core :: MarketRow / DateRange
//! ml      -> core :: TrainRequest / ModelManifest / TaskEvent
//! ```
#![warn(missing_docs)]

pub mod actions;
pub mod admission;
pub mod execution;
pub mod market_rules;
pub mod pool;
pub mod research_quality;
mod scalar_float;
pub mod schedule;
pub mod source_map;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};
use thiserror::Error;
use uuid::Uuid;

/// Trading date used across the workspace (calendar date, no timezone).
pub type Date = NaiveDate;

/// One normalized daily market observation returned by the local query layer.
/// Numeric fields stay nullable so missing observations reach the factor DSL as
/// nulls instead of being silently imputed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketRow {
    /// Instrument identifier (e.g. `SH600000`).
    pub instrument_id: String,
    /// Observation date.
    pub trade_date: Date,
    /// Raw open price.
    pub open: Option<f64>,
    /// Raw high price.
    pub high: Option<f64>,
    /// Raw low price.
    pub low: Option<f64>,
    /// Raw close price.
    pub close: Option<f64>,
    /// Traded volume.
    pub volume: Option<f64>,
    /// Traded amount (currency).
    pub amount: Option<f64>,
    /// Adjusted open price.
    pub adj_open: Option<f64>,
    /// Adjusted high price.
    pub adj_high: Option<f64>,
    /// Adjusted low price.
    pub adj_low: Option<f64>,
    /// Adjusted close price.
    pub adj_close: Option<f64>,
    /// Whether the instrument traded that day.
    pub is_trading: Option<bool>,
    /// First listing date, used to exclude pre-listing rows.
    pub listing_date: Option<Date>,
}

/// Errors produced by request validation in this crate.
#[derive(Debug, Error)]
pub enum CoreError {
    /// A date range with `start > end`.
    #[error("invalid date range: {0}")]
    InvalidDateRange(String),
    /// A structurally invalid request (empty SQL, bad page size, ...).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Serialization failure at a crate boundary.
    #[error("serialization error: {0}")]
    Serialization(String),
}

/// Inclusive date bounds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DateRange {
    /// First included date.
    pub start: Date,
    /// Last included date.
    pub end: Date,
}

impl DateRange {
    /// Whether a date lies in this inclusive interval.
    pub fn contains(&self, date: Date) -> bool {
        self.start <= date && date <= self.end
    }

    /// Returns an error when `start` is after `end`.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.start > self.end {
            return Err(CoreError::InvalidDateRange(format!(
                "{} > {}",
                self.start, self.end
            )));
        }
        Ok(())
    }
}

/// Disjoint chronological train/validation/test ranges.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DateSplit {
    /// Oldest 70% of trading dates.
    pub train: DateRange,
    /// Next 15% of trading dates.
    pub validation: DateRange,
    /// Newest 15% of trading dates.
    pub test: DateRange,
}

/// Splits observed trading dates into disjoint 70/15/15
/// train/validation/test ranges, preserving chronological order.
///
/// # Errors
///
/// Returns [`CoreError::InvalidRequest`] when fewer than three distinct dates
/// are provided.
///
/// # Examples
///
/// ```
/// use analytics_core::{chronological_date_split, Date};
///
/// let dates: Vec<Date> = (0..20)
///     .map(|d| Date::from_ymd_opt(2024, 1, 1).unwrap() + chrono::Duration::days(d))
///     .collect();
/// let split = chronological_date_split(&dates).unwrap();
/// assert!(split.train.end < split.validation.start);
/// assert!(split.validation.end < split.test.start);
/// ```
pub fn chronological_date_split(dates: &[Date]) -> Result<DateSplit, CoreError> {
    let mut unique = dates.to_vec();
    unique.sort();
    unique.dedup();
    if unique.len() < 3 {
        return Err(CoreError::InvalidRequest(
            "at least three dates are required".into(),
        ));
    }
    let train_end = (unique.len() * 70 / 100).max(1).min(unique.len() - 2);
    let validation_end = (unique.len() * 85 / 100)
        .max(train_end + 1)
        .min(unique.len() - 1);
    Ok(DateSplit {
        train: DateRange {
            start: unique[0],
            end: unique[train_end - 1],
        },
        validation: DateRange {
            start: unique[train_end],
            end: unique[validation_end - 1],
        },
        test: DateRange {
            start: unique[validation_end],
            end: *unique.last().unwrap(),
        },
    })
}

/// Logical column type used at crate boundaries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DataType {
    /// Unknown or untyped value.
    Null,
    /// Boolean.
    Boolean,
    /// 64-bit signed integer (also covers narrower ints).
    Int64,
    /// 64-bit float.
    Float64,
    /// UTF-8 string.
    Utf8,
    /// Calendar date.
    Date,
}

/// A single typed cell value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ScalarValue {
    /// SQL NULL.
    Null,
    /// Boolean value.
    Boolean(bool),
    /// Integer value.
    Int64(i64),
    /// Float value.
    Float64(#[serde(with = "scalar_float")] f64),
    /// String value.
    Utf8(String),
    /// Date value.
    Date(Date),
}

/// Name, type, and nullability of one column.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColumnSchema {
    /// Column name.
    pub name: String,
    /// Logical data type.
    pub data_type: DataType,
    /// Whether the column may contain nulls.
    pub nullable: bool,
}

/// One Parquet file discovered by the catalog, with footer metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetFile {
    /// Absolute or working-directory-relative file path.
    pub path: PathBuf,
    /// Row count from the Parquet footer, when read.
    pub rows: Option<u64>,
    /// Leaf column schemas.
    pub schema: Vec<ColumnSchema>,
}

/// A registered dataset root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetRef {
    /// Stable identifier.
    pub id: Uuid,
    /// Root directory or file.
    pub root: PathBuf,
    /// Display name.
    pub name: String,
}

/// One sort key of a stable result ordering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SortKey {
    /// Column to sort by (identifier-validated by the engine).
    pub column: String,
    /// Descending when true.
    pub descending: bool,
}

/// Keyset cursor: values of the sort keys on the last row of a page.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeysetCursor(pub Vec<ScalarValue>);

/// One page of a paginated SQL result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageRequest {
    /// SQL statement without a trailing semicolon.
    pub sql: String,
    /// Maximum rows per page (1..=5000).
    pub limit: u32,
    /// Stable sort keys; required when `after` is set.
    pub sort: Vec<SortKey>,
    /// Keyset cursor of the previous page.
    pub after: Option<KeysetCursor>,
}

impl PageRequest {
    /// Validates SQL presence, page size bounds, and cursor/sort consistency.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.sql.trim().is_empty() {
            return Err(CoreError::InvalidRequest("SQL is empty".into()));
        }
        if self.limit == 0 || self.limit > 5_000 {
            return Err(CoreError::InvalidRequest(
                "page size must be between 1 and 5000".into(),
            ));
        }
        if self.after.is_some() && self.sort.is_empty() {
            return Err(CoreError::InvalidRequest(
                "a cursor requires a stable sort".into(),
            ));
        }
        Ok(())
    }
}

/// A fetched result page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPage {
    /// Column schemas of the result.
    pub columns: Vec<ColumnSchema>,
    /// Row-major cell values.
    pub rows: Vec<Vec<ScalarValue>>,
    /// Cursor for the next page; `None` when the result is exhausted.
    pub next_cursor: Option<KeysetCursor>,
    /// Optional total row count (usually unknown for keyset pagination).
    pub total_rows: Option<u64>,
}

/// Export file format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExportFormat {
    /// Parquet with ZSTD compression.
    Parquet,
    /// CSV with header row.
    Csv,
}

/// A SQL export job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRequest {
    /// SQL statement to export.
    pub sql: String,
    /// Destination file path.
    pub destination: PathBuf,
    /// Output format.
    pub format: ExportFormat,
    /// Optional codec hint (reserved; Parquet always uses ZSTD today).
    pub compression: Option<String>,
}

/// One model feature entry recorded in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureSpec {
    /// Feature name (factor ID).
    pub name: String,
    /// Source column in the training table.
    pub source_column: String,
    /// Logical data type.
    pub data_type: DataType,
    /// Whether the feature is categorical.
    pub categorical: bool,
}

/// Manifest written next to every trained model.
///
/// Preserves feature order, schema hash, and formula version so predictions
/// can be tied back to the exact training configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelManifest {
    /// Manifest schema version (currently 1).
    pub schema_version: u32,
    /// Unique model identifier.
    pub model_id: Uuid,
    /// Features in training column order.
    pub features: Vec<FeatureSpec>,
    /// Objective used for training.
    pub objective: String,
    /// Hash over feature names and count.
    pub training_schema_hash: String,
    /// Alpha191 formula catalog version.
    pub formula_version: Option<String>,
    /// Universe used for training.
    pub universe_id: Option<String>,
    /// Date range of the training run.
    pub date_range: Option<DateRange>,
    /// Hash of the input Parquet snapshot.
    pub input_snapshot_hash: Option<String>,
}

/// LightGBM objective family.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Objective {
    /// Regression (L2).
    Regression,
    /// Binary classification.
    Binary,
    /// Multiclass classification with the given class count.
    Multiclass {
        /// Number of classes.
        classes: u32,
    },
}

/// Training job description shared by trainer implementations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainRequest {
    /// Feature columns in matrix order.
    pub factor_ids: Vec<String>,
    /// Universe identifier recorded in the manifest.
    pub universe_id: String,
    /// Training date range recorded in the manifest.
    pub date_range: DateRange,
    /// Objective family.
    pub objective: Objective,
    /// Native LightGBM `key=value` parameters.
    pub parameters: BTreeMap<String, String>,
    /// Early-stopping patience on the validation split.
    pub early_stopping_rounds: Option<u32>,
}

/// Output of a prediction call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionBatch {
    /// Row-major predictions (one inner vec per row).
    pub predictions: Vec<Vec<f64>>,
    /// Optional SHAP-style feature contributions.
    pub contributions: Option<Vec<Vec<f64>>>,
}

/// Files written by a training run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingArtifact {
    /// LightGBM text model.
    pub model_path: PathBuf,
    /// [`ModelManifest`] JSON.
    pub manifest_path: PathBuf,
    /// Metrics JSON (iterations, history, test RMSE).
    pub metrics_path: PathBuf,
    /// Feature importance JSON (gain per column).
    pub feature_importance_path: PathBuf,
}

/// Lifecycle events emitted by long-running tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskEvent {
    /// Task started.
    Started {
        /// Task identifier.
        task_id: Uuid,
        /// Human-readable label.
        label: String,
    },
    /// Progress update.
    Progress {
        /// Task identifier.
        task_id: Uuid,
        /// Units completed so far.
        completed: u64,
        /// Total units when known.
        total: Option<u64>,
        /// Display message.
        message: String,
    },
    /// Finished successfully.
    Finished {
        /// Task identifier.
        task_id: Uuid,
    },
    /// Failed with an error message.
    Failed {
        /// Task identifier.
        task_id: Uuid,
        /// Error text.
        message: String,
    },
    /// Cancelled through the shared flag.
    Cancelled {
        /// Task identifier.
        task_id: Uuid,
    },
}

/// Cooperative cancellation flag shared between UI and worker threads.
pub type Cancellation = Arc<AtomicBool>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chronological_split_is_disjoint_and_ordered() {
        let dates = (0..20)
            .map(|offset| Date::from_ymd_opt(2024, 1, 1).unwrap() + chrono::Duration::days(offset))
            .collect::<Vec<_>>();
        let split = chronological_date_split(&dates).unwrap();
        assert!(split.train.end < split.validation.start);
        assert!(split.validation.end < split.test.start);
        assert_eq!(split.train.start, dates[0]);
        assert_eq!(split.test.end, *dates.last().unwrap());
    }
}

/// Shared progress and cancellation for local tasks.
pub mod task;
