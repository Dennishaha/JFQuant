use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};
use thiserror::Error;
use uuid::Uuid;

pub type Date = NaiveDate;

/// One normalized daily market observation returned by the local query layer.
/// Numeric fields stay nullable so missing observations reach the factor DSL as
/// nulls instead of being silently imputed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketRow {
    pub instrument_id: String,
    pub trade_date: Date,
    pub open: Option<f64>,
    pub high: Option<f64>,
    pub low: Option<f64>,
    pub close: Option<f64>,
    pub volume: Option<f64>,
    pub amount: Option<f64>,
    pub adj_open: Option<f64>,
    pub adj_high: Option<f64>,
    pub adj_low: Option<f64>,
    pub adj_close: Option<f64>,
    pub is_trading: Option<bool>,
    pub listing_date: Option<Date>,
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid date range: {0}")]
    InvalidDateRange(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("serialization error: {0}")]
    Serialization(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DateRange {
    pub start: Date,
    pub end: Date,
}

impl DateRange {
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DateSplit {
    pub train: DateRange,
    pub validation: DateRange,
    pub test: DateRange,
}

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DataType {
    Null,
    Boolean,
    Int64,
    Float64,
    Utf8,
    Date,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ScalarValue {
    Null,
    Boolean(bool),
    Int64(i64),
    Float64(f64),
    Utf8(String),
    Date(Date),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColumnSchema {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetFile {
    pub path: PathBuf,
    pub rows: Option<u64>,
    pub schema: Vec<ColumnSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetRef {
    pub id: Uuid,
    pub root: PathBuf,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SortKey {
    pub column: String,
    pub descending: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeysetCursor(pub Vec<ScalarValue>);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageRequest {
    pub sql: String,
    pub limit: u32,
    pub sort: Vec<SortKey>,
    pub after: Option<KeysetCursor>,
}

impl PageRequest {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPage {
    pub columns: Vec<ColumnSchema>,
    pub rows: Vec<Vec<ScalarValue>>,
    pub next_cursor: Option<KeysetCursor>,
    pub total_rows: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExportFormat {
    Parquet,
    Csv,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRequest {
    pub sql: String,
    pub destination: PathBuf,
    pub format: ExportFormat,
    pub compression: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureSpec {
    pub name: String,
    pub source_column: String,
    pub data_type: DataType,
    pub categorical: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelManifest {
    pub schema_version: u32,
    pub model_id: Uuid,
    pub features: Vec<FeatureSpec>,
    pub objective: String,
    pub training_schema_hash: String,
    pub formula_version: Option<String>,
    #[serde(default)]
    pub universe_id: Option<String>,
    #[serde(default)]
    pub date_range: Option<DateRange>,
    #[serde(default)]
    pub input_snapshot_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Objective {
    Regression,
    Binary,
    Multiclass { classes: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainRequest {
    pub factor_ids: Vec<String>,
    pub universe_id: String,
    pub date_range: DateRange,
    pub objective: Objective,
    pub parameters: BTreeMap<String, String>,
    pub early_stopping_rounds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionBatch {
    pub predictions: Vec<Vec<f64>>,
    pub contributions: Option<Vec<Vec<f64>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingArtifact {
    pub model_path: PathBuf,
    pub manifest_path: PathBuf,
    pub metrics_path: PathBuf,
    pub feature_importance_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskEvent {
    Started {
        task_id: Uuid,
        label: String,
    },
    Progress {
        task_id: Uuid,
        completed: u64,
        total: Option<u64>,
        message: String,
    },
    Finished {
        task_id: Uuid,
    },
    Failed {
        task_id: Uuid,
        message: String,
    },
    Cancelled {
        task_id: Uuid,
    },
}

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
