//! Research contracts, Rust factor preparation and supervised Python execution.
//!
//! Rust owns contracts and factor preparation; LightGBM runs in a task-scoped
//! bundled Python process shared with AKQuant backtesting.
#![warn(missing_docs)]

use analytics_core::{ModelManifest, PredictionBatch, TrainRequest, TrainingArtifact};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::Path,
    sync::{Arc, atomic::AtomicBool, mpsc::Sender},
};
use thiserror::Error;

pub mod backtest;
pub mod pipeline;

/// Errors returned by matrix validation, Python research calls, and artifact
/// serialization.
#[derive(Debug, Error)]
pub enum MlError {
    /// Matrix shape, labels, or request metadata are invalid.
    #[error("invalid training data: {0}")]
    InvalidData(String),
    /// The research worker returned an error.
    #[error("LightGBM error: {0}")]
    Native(String),
    /// Filesystem failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization or parsing failure.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Convenience result alias for ML calls.
pub type Result<T> = std::result::Result<T, MlError>;

/// Progress events emitted by a trainer.
#[derive(Debug, Clone)]
pub enum TrainEvent {
    /// Dataset creation has started.
    Started {
        /// Number of training rows.
        rows: usize,
        /// Number of feature columns.
        features: usize,
    },
    /// One boosting iteration completed, with train then validation metrics.
    Iteration {
        /// One-based boosting iteration.
        iteration: u32,
        /// Metrics in backend order.
        metrics: Vec<f64>,
    },
    /// Training completed successfully.
    Finished {
        /// Number of completed boosting iterations.
        iterations: u32,
    },
    /// Training noticed the cancellation flag.
    Cancelled,
}

/// Row-major dense feature matrix and one label per row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DenseMatrix {
    /// Feature values in row-major order; `NaN` represents missing data.
    pub features: Vec<f64>,
    /// Regression/classification labels.
    pub labels: Vec<f32>,
    /// Number of rows.
    pub rows: usize,
    /// Number of feature columns.
    pub columns: usize,
}

impl DenseMatrix {
    /// Checks non-empty shape and feature/label lengths.
    pub fn validate(&self) -> Result<()> {
        if self.rows == 0 || self.columns == 0 {
            return Err(MlError::InvalidData("empty matrix".into()));
        }
        if self.features.len() != self.rows.saturating_mul(self.columns) {
            return Err(MlError::InvalidData(
                "feature matrix length does not match shape".into(),
            ));
        }
        if self.labels.len() != self.rows {
            return Err(MlError::InvalidData(
                "label length does not match rows".into(),
            ));
        }
        if self.labels.iter().any(|v| !v.is_finite())
            || self.features.iter().any(|v| v.is_infinite())
        {
            return Err(MlError::InvalidData(
                "non-finite labels or infinite features".into(),
            ));
        }
        Ok(())
    }
}

/// Chronological train/validation/test matrices with matching columns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingSplit {
    /// Training matrix.
    pub train: DenseMatrix,
    /// Validation matrix used by early stopping.
    pub validation: DenseMatrix,
    /// Test matrix used only for final metrics and predictions.
    pub test: DenseMatrix,
}

impl TrainingSplit {
    /// Validates every matrix and requires equal feature counts.
    pub fn validate(&self) -> Result<()> {
        self.train.validate()?;
        for matrix in [&self.validation, &self.test] {
            if matrix.rows > 0 {
                matrix.validate()?;
            } else if matrix.columns == 0
                || !matrix.features.is_empty()
                || !matrix.labels.is_empty()
            {
                return Err(MlError::InvalidData("invalid empty optional matrix".into()));
            }
        }
        if self.train.columns != self.validation.columns || self.train.columns != self.test.columns
        {
            return Err(MlError::InvalidData(
                "split column counts do not match".into(),
            ));
        }
        Ok(())
    }
}

/// Training abstraction implemented by the native LightGBM adapter and test
/// doubles.
pub trait Trainer {
    /// Trains a dense feature matrix and writes model artifacts to
    /// `output_dir`. Implementations must validate shape and honor
    /// `cancel` between boosting iterations.
    #[allow(clippy::too_many_arguments)]
    fn train_dense(
        &self,
        request: &TrainRequest,
        features: &[f64],
        labels: &[f32],
        rows: usize,
        columns: usize,
        output_dir: &Path,
        progress: Sender<TrainEvent>,
        cancel: Arc<AtomicBool>,
    ) -> Result<TrainingArtifact>;

    /// Trains on the train split and registers validation/test matrices when
    /// the backend supports them.
    fn train_split(
        &self,
        request: &TrainRequest,
        split: &TrainingSplit,
        output_dir: &Path,
        progress: Sender<TrainEvent>,
        cancel: Arc<AtomicBool>,
    ) -> Result<TrainingArtifact> {
        split.validate()?;
        self.train_dense(
            request,
            &split.train.features,
            &split.train.labels,
            split.train.rows,
            split.train.columns,
            output_dir,
            progress,
            cancel,
        )
    }
}

/// Prediction abstraction for dense feature matrices.
pub trait Predictor {
    /// Predicts `rows` rows of `columns` features; optionally returns feature
    /// contributions alongside normal predictions.
    fn predict_dense(
        &self,
        features: &[f64],
        rows: usize,
        columns: usize,
        contributions: bool,
    ) -> Result<PredictionBatch>;
}

/// Supervised research runtime and cache receipts.
pub mod worker;
/// Python LightGBM trainer; owns no native handles in Rust.
pub struct LightGbmTrainer;
/// Model text path for predictions in the task's Python process.
pub struct LightGbmPredictor {
    /// Saved LightGBM model.
    pub model_path: std::path::PathBuf,
}
impl Trainer for LightGbmTrainer {
    fn train_dense(
        &self,
        request: &TrainRequest,
        features: &[f64],
        labels: &[f32],
        rows: usize,
        columns: usize,
        output: &Path,
        progress: Sender<TrainEvent>,
        cancel: Arc<AtomicBool>,
    ) -> Result<TrainingArtifact> {
        self.train_split(
            request,
            &TrainingSplit {
                train: DenseMatrix {
                    features: features.to_vec(),
                    labels: labels.to_vec(),
                    rows,
                    columns,
                },
                validation: DenseMatrix {
                    features: vec![],
                    labels: vec![],
                    rows: 0,
                    columns,
                },
                test: DenseMatrix {
                    features: vec![],
                    labels: vec![],
                    rows: 0,
                    columns,
                },
            },
            output,
            progress,
            cancel,
        )
    }
    fn train_split(
        &self,
        request: &TrainRequest,
        split: &TrainingSplit,
        output: &Path,
        progress: Sender<TrainEvent>,
        cancel: Arc<AtomicBool>,
    ) -> Result<TrainingArtifact> {
        use sha2::{Digest, Sha256};
        split.train.validate()?;
        if request.factor_ids.len() != split.train.columns {
            return Err(MlError::InvalidData("factor order/columns mismatch".into()));
        }
        fs::create_dir_all(output)?;
        let output = fs::canonicalize(output)?;
        let names = &request.factor_ids;
        let mut paths = Vec::new();
        for (name, m) in [
            ("train", &split.train),
            ("validation", &split.validation),
            ("test", &split.test),
        ] {
            if m.columns != names.len() {
                return Err(MlError::InvalidData("split columns mismatch".into()));
            }
            if m.rows > 0 {
                m.validate()?;
                let p = output.join(format!(".worker-{name}.parquet"));
                worker::matrix(&p, &m.features, &m.labels, m.rows, names)?;
                paths.push((name, p));
            }
        }
        let mut command =
            serde_json::json!({"operation":"train","request":request,"output":output});
        for (name, path) in &paths {
            command[*name] = serde_json::json!(path);
        }
        let _ = progress.send(TrainEvent::Started {
            rows: split.train.rows,
            features: names.len(),
        });
        let result = worker::call(command, cancel);
        for (_, path) in paths {
            let _ = fs::remove_file(path);
        }
        let metrics = result?;
        let features = names
            .iter()
            .map(|name| analytics_core::FeatureSpec {
                name: name.clone(),
                source_column: name.clone(),
                data_type: analytics_core::DataType::Float64,
                categorical: false,
            })
            .collect();
        let mut h = Sha256::new();
        for name in names {
            h.update(name.as_bytes());
            h.update([0]);
        }
        h.update((names.len() as u64).to_le_bytes());
        let manifest = ModelManifest {
            schema_version: 2,
            model_id: uuid::Uuid::new_v4(),
            features,
            objective: format!("{:?}", request.objective),
            training_schema_hash: format!("sha256:{:x}", h.finalize()),
            formula_version: None,
            universe_id: Some(request.universe_id.clone()),
            date_range: Some(request.date_range.clone()),
            input_snapshot_hash: None,
        };
        let mut value = serde_json::to_value(manifest)?;
        value["runtime"] = serde_json::json!({"worker":worker::runtime_signature(),"dependencies":worker::dependency_signature(),"lightgbm":"4.7.0"});
        fs::write(
            output.join("manifest.json"),
            serde_json::to_vec_pretty(&value)?,
        )?;
        let _ = progress.send(TrainEvent::Finished {
            iterations: metrics["iterations"].as_u64().unwrap_or(0) as u32,
        });
        Ok(TrainingArtifact {
            model_path: output.join("model.txt"),
            manifest_path: output.join("manifest.json"),
            metrics_path: output.join("metrics.json"),
            feature_importance_path: output.join("feature_importance.json"),
        })
    }
}
impl Predictor for LightGbmPredictor {
    fn predict_dense(
        &self,
        features: &[f64],
        rows: usize,
        columns: usize,
        contributions: bool,
    ) -> Result<PredictionBatch> {
        if rows == 0 {
            return Ok(PredictionBatch {
                predictions: vec![],
                contributions: None,
            });
        }
        let model = fs::canonicalize(&self.model_path)?;
        let folder = model.parent().unwrap();
        let manifest = load_manifest(folder.join("manifest.json"))?;
        if manifest.schema_version != 2 || manifest.features.len() != columns {
            return Err(MlError::InvalidData(
                "unsupported model format/feature count; retrain using Python worker".into(),
            ));
        }
        let names = manifest
            .features
            .iter()
            .map(|f| f.name.clone())
            .collect::<Vec<_>>();
        let scratch = tempfile::tempdir()?;
        let path = scratch.path().join("input.parquet");
        worker::matrix(&path, features, &vec![f32::NAN; rows], rows, &names)?;
        let output = scratch.path().join("output.parquet");
        let result = worker::call(
            serde_json::json!({"operation":"predict","model":model,"matrix":path,"output":output,"contributions":contributions}),
            analytics_core::task::TaskContext::current().cancel,
        );
        let _ = fs::remove_file(path);
        let read = (|| -> Result<Vec<Vec<f64>>> {
            use arrow_array::Float64Array;
            let result = result?;
            if result["path"] != serde_json::json!(output) || result["rows"] != rows {
                return Err(MlError::InvalidData(
                    "prediction response shape/path mismatch".into(),
                ));
            }
            let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
                fs::File::open(&output)?,
            )
            .map_err(|e| MlError::Native(e.to_string()))?
            .build()
            .map_err(|e| MlError::Native(e.to_string()))?;
            let mut values = Vec::new();
            for batch in reader {
                let batch = batch.map_err(|e| MlError::Native(e.to_string()))?;
                let columns = batch
                    .columns()
                    .iter()
                    .map(|c| {
                        c.as_any()
                            .downcast_ref::<Float64Array>()
                            .ok_or_else(|| MlError::InvalidData("invalid prediction column".into()))
                    })
                    .collect::<Result<Vec<_>>>()?;
                for row in 0..batch.num_rows() {
                    values.push(columns.iter().map(|c| c.value(row)).collect());
                }
            }
            if values.len() != rows {
                return Err(MlError::InvalidData("truncated prediction output".into()));
            }
            Ok(values)
        })();
        let _ = fs::remove_file(output);
        let values = read?;
        Ok(if contributions {
            PredictionBatch {
                predictions: vec![],
                contributions: Some(values),
            }
        } else {
            PredictionBatch {
                predictions: values,
                contributions: None,
            }
        })
    }
}

/// Loads and deserializes a model manifest from JSON.
pub fn load_manifest(path: impl AsRef<Path>) -> Result<ModelManifest> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matrix(rows: usize, columns: usize) -> DenseMatrix {
        DenseMatrix {
            features: vec![0.0; rows * columns],
            labels: vec![0.0; rows],
            rows,
            columns,
        }
    }

    #[test]
    fn early_stopped_test_rmse_matches_the_saved_best_model() {
        use analytics_core::{Date, DateRange, Objective};
        use std::collections::BTreeMap;
        let dataset = |sign: f32| DenseMatrix {
            features: (-50..=50).map(|x| x as f64 / 50.0).collect(),
            labels: (-50..=50).map(|x| sign * x as f32 / 50.0).collect(),
            rows: 101,
            columns: 1,
        };
        let split = TrainingSplit {
            train: dataset(1.0),
            validation: dataset(-1.0),
            test: dataset(1.0),
        };
        let request = TrainRequest {
            factor_ids: vec!["alpha_001".into()],
            universe_id: "fixture".into(),
            date_range: DateRange {
                start: Date::from_ymd_opt(2020, 1, 1).unwrap(),
                end: Date::from_ymd_opt(2020, 12, 31).unwrap(),
            },
            objective: Objective::Regression,
            parameters: BTreeMap::from([
                ("num_iterations".into(), "20".into()),
                ("min_data_in_leaf".into(), "1".into()),
                ("num_leaves".into(), "4".into()),
                ("learning_rate".into(), "0.3".into()),
                ("num_threads".into(), "1".into()),
            ]),
            early_stopping_rounds: Some(3),
        };
        let dir = tempfile::tempdir().unwrap();
        let (tx, _) = std::sync::mpsc::channel();
        let artifact = LightGbmTrainer
            .train_split(
                &request,
                &split,
                dir.path(),
                tx,
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        let metrics: serde_json::Value =
            serde_json::from_slice(&std::fs::read(artifact.metrics_path).unwrap()).unwrap();
        assert!(
            metrics["best_iteration"].as_u64().unwrap() < metrics["iterations"].as_u64().unwrap()
        );
        let predictions = LightGbmPredictor {
            model_path: artifact.model_path,
        }
        .predict_dense(&split.test.features, 101, 1, false)
        .unwrap();
        let rmse = (predictions
            .predictions
            .iter()
            .zip(&split.test.labels)
            .map(|(p, y)| (p[0] - *y as f64).powi(2))
            .sum::<f64>()
            / 101.0)
            .sqrt();
        assert!((rmse - metrics["test_rmse"].as_f64().unwrap()).abs() < 1e-12);
    }

    #[test]
    fn dense_matrix_rejects_shape_mismatch() {
        let mut value = matrix(2, 2);
        value.features.pop();
        assert!(value.validate().is_err());
    }

    #[test]
    fn training_split_requires_matching_columns_and_nonempty_sets() {
        let split = TrainingSplit {
            train: matrix(2, 2),
            validation: matrix(2, 3),
            test: matrix(2, 2),
        };
        assert!(split.validate().is_err());
    }
}

mod training_config;
/// Training configuration shared by rolling research and pipeline.
pub use training_config::TrainingConfig;
