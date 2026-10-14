//! LightGBM training and prediction boundary.
//!
//! The default build has no native dependency. With `lightgbm-native`, the
//! C API wrapper owns dataset and booster handles on the calling worker
//! thread. The optional [`pipeline`] module composes DuckDB, Alpha191, and
//! this trainer into the full local pipeline.
#![warn(missing_docs)]

use analytics_core::{ModelManifest, PredictionBatch, TrainRequest, TrainingArtifact};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::Path,
    sync::{Arc, atomic::AtomicBool, mpsc::Sender},
};
use thiserror::Error;

#[cfg(feature = "lightgbm-native")]
pub mod backtest;
#[cfg(all(feature = "lightgbm-native", feature = "duckdb-native"))]
pub mod pipeline;

/// Errors returned by matrix validation, native LightGBM calls, and artifact
/// serialization.
#[derive(Debug, Error)]
pub enum MlError {
    /// The native LightGBM feature was not compiled.
    #[error("native LightGBM backend is not enabled")]
    BackendUnavailable,
    /// Matrix shape, labels, or request metadata are invalid.
    #[error("invalid training data: {0}")]
    InvalidData(String),
    /// The native C API returned an error.
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

#[cfg(feature = "lightgbm-native")]
#[allow(dead_code)]
mod native {
    use super::*;
    use analytics_core::Objective;
    use sha2::{Digest, Sha256};
    use std::{
        ffi::{CStr, CString},
        os::raw::{c_char, c_double, c_int, c_void},
        path::PathBuf,
        sync::atomic::Ordering,
    };

    type DatasetHandle = *mut c_void;
    type BoosterHandle = *mut c_void;

    const DTYPE_FLOAT32: c_int = 0;
    const DTYPE_FLOAT64: c_int = 1;
    const PREDICT_NORMAL: c_int = 0;
    const PREDICT_CONTRIB: c_int = 3;
    const IMPORTANCE_GAIN: c_int = 1;

    unsafe extern "C" {
        fn LGBM_GetLastError() -> *const c_char;
        fn LGBM_DatasetCreateFromMat(
            data: *const c_void,
            data_type: c_int,
            nrow: c_int,
            ncol: c_int,
            is_row_major: c_int,
            parameters: *const c_char,
            reference: DatasetHandle,
            out: *mut DatasetHandle,
        ) -> c_int;
        fn LGBM_DatasetSetField(
            handle: DatasetHandle,
            field_name: *const c_char,
            field_data: *const c_void,
            num_element: c_int,
            data_type: c_int,
        ) -> c_int;
        fn LGBM_DatasetFree(handle: DatasetHandle) -> c_int;
        fn LGBM_BoosterCreate(
            train_data: DatasetHandle,
            parameters: *const c_char,
            out: *mut BoosterHandle,
        ) -> c_int;
        fn LGBM_BoosterCreateFromModelfile(
            filename: *const c_char,
            out_num_iterations: *mut c_int,
            out: *mut BoosterHandle,
        ) -> c_int;
        fn LGBM_BoosterFree(handle: BoosterHandle) -> c_int;
        fn LGBM_BoosterUpdateOneIter(
            handle: BoosterHandle,
            produced_empty_tree: *mut c_int,
        ) -> c_int;
        fn LGBM_BoosterAddValidData(handle: BoosterHandle, valid_data: DatasetHandle) -> c_int;
        fn LGBM_BoosterGetEvalCounts(handle: BoosterHandle, out_len: *mut c_int) -> c_int;
        fn LGBM_BoosterGetEval(
            handle: BoosterHandle,
            data_idx: c_int,
            out_len: *mut c_int,
            out_results: *mut c_double,
        ) -> c_int;
        fn LGBM_BoosterCalcNumPredict(
            handle: BoosterHandle,
            num_row: c_int,
            predict_type: c_int,
            start_iteration: c_int,
            num_iteration: c_int,
            out_len: *mut i64,
        ) -> c_int;
        fn LGBM_BoosterSaveModel(
            handle: BoosterHandle,
            start_iteration: c_int,
            num_iteration: c_int,
            feature_importance_type: c_int,
            filename: *const c_char,
        ) -> c_int;
        fn LGBM_BoosterFeatureImportance(
            handle: BoosterHandle,
            num_iteration: c_int,
            importance_type: c_int,
            out_results: *mut c_double,
        ) -> c_int;
        fn LGBM_BoosterPredictForMat(
            handle: BoosterHandle,
            data: *const c_void,
            data_type: c_int,
            nrow: c_int,
            ncol: c_int,
            is_row_major: c_int,
            predict_type: c_int,
            start_iteration: c_int,
            num_iteration: c_int,
            parameter: *const c_char,
            out_len: *mut i64,
            out_result: *mut c_double,
        ) -> c_int;
    }

    fn error() -> MlError {
        unsafe {
            let ptr = LGBM_GetLastError();
            if ptr.is_null() {
                MlError::Native("unknown LightGBM error".into())
            } else {
                MlError::Native(CStr::from_ptr(ptr).to_string_lossy().into_owned())
            }
        }
    }

    struct Dataset(DatasetHandle);
    impl Drop for Dataset {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    LGBM_DatasetFree(self.0);
                }
            }
        }
    }
    struct Booster(BoosterHandle);
    impl Drop for Booster {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    LGBM_BoosterFree(self.0);
                }
            }
        }
    }

    /// Native predictor backed by a model text file.
    pub struct LightGbmPredictor {
        /// Path to the saved LightGBM model.
        pub model_path: PathBuf,
    }

    impl Predictor for LightGbmPredictor {
        fn predict_dense(
            &self,
            features: &[f64],
            rows: usize,
            columns: usize,
            contributions: bool,
        ) -> Result<PredictionBatch> {
            if rows == 0
                || columns == 0
                || features.len()
                    != rows
                        .checked_mul(columns)
                        .ok_or_else(|| MlError::InvalidData("matrix size overflow".into()))?
            {
                return Err(MlError::InvalidData(
                    "feature matrix length does not match shape".into(),
                ));
            }
            let model = CString::new(self.model_path.to_string_lossy().as_bytes())
                .map_err(|e| MlError::InvalidData(e.to_string()))?;
            let mut iterations = 0;
            let mut booster_raw = std::ptr::null_mut();
            if unsafe {
                LGBM_BoosterCreateFromModelfile(model.as_ptr(), &mut iterations, &mut booster_raw)
            } != 0
            {
                return Err(error());
            }
            let booster = Booster(booster_raw);
            let prediction_type = if contributions {
                PREDICT_CONTRIB
            } else {
                PREDICT_NORMAL
            };
            let mut output_len = 0_i64;
            if unsafe {
                LGBM_BoosterCalcNumPredict(
                    booster.0,
                    rows as c_int,
                    prediction_type,
                    0,
                    -1,
                    &mut output_len,
                )
            } != 0
            {
                return Err(error());
            }
            let mut output = vec![0.0; output_len as usize];
            let prediction_parameters = CString::new("").expect("empty prediction parameters");
            if unsafe {
                LGBM_BoosterPredictForMat(
                    booster.0,
                    features.as_ptr().cast(),
                    DTYPE_FLOAT64,
                    rows as c_int,
                    columns as c_int,
                    1,
                    prediction_type,
                    0,
                    -1,
                    prediction_parameters.as_ptr(),
                    &mut output_len,
                    output.as_mut_ptr(),
                )
            } != 0
            {
                return Err(error());
            }
            output.truncate(output_len as usize);
            if contributions {
                let width = columns + 1;
                let contribution_rows = output.chunks(width).map(|row| row.to_vec()).collect();
                Ok(PredictionBatch {
                    predictions: Vec::new(),
                    contributions: Some(contribution_rows),
                })
            } else {
                Ok(PredictionBatch {
                    predictions: output.into_iter().map(|value| vec![value]).collect(),
                    contributions: None,
                })
            }
        }
    }

    /// Native LightGBM trainer. Dataset and booster handles stay in the
    /// calling worker thread.
    pub struct LightGbmTrainer;

    impl LightGbmTrainer {
        fn create_dataset(
            features: &[f64],
            labels: &[f32],
            rows: usize,
            columns: usize,
            params: &CString,
            reference: DatasetHandle,
        ) -> Result<Dataset> {
            if features.len()
                != rows
                    .checked_mul(columns)
                    .ok_or_else(|| MlError::InvalidData("matrix size overflow".into()))?
            {
                return Err(MlError::InvalidData(
                    "feature matrix length does not match shape".into(),
                ));
            }
            if labels.len() != rows {
                return Err(MlError::InvalidData(
                    "label length does not match rows".into(),
                ));
            }
            let mut raw = std::ptr::null_mut();
            let rc = unsafe {
                LGBM_DatasetCreateFromMat(
                    features.as_ptr().cast(),
                    DTYPE_FLOAT64,
                    rows as c_int,
                    columns as c_int,
                    1,
                    params.as_ptr(),
                    reference,
                    &mut raw,
                )
            };
            if rc != 0 {
                return Err(error());
            }
            let dataset = Dataset(raw);
            let name = CString::new("label").unwrap();
            let rc = unsafe {
                LGBM_DatasetSetField(
                    dataset.0,
                    name.as_ptr(),
                    labels.as_ptr().cast(),
                    labels.len() as c_int,
                    DTYPE_FLOAT32,
                )
            };
            if rc != 0 {
                return Err(error());
            }
            Ok(dataset)
        }
    }

    impl LightGbmTrainer {
        fn read_eval(handle: BoosterHandle, data_index: c_int) -> Result<Vec<f64>> {
            let mut count = 0;
            if unsafe { LGBM_BoosterGetEvalCounts(handle, &mut count) } != 0 || count == 0 {
                return Ok(Vec::new());
            }
            let mut values = vec![0.0; count as usize];
            let mut length = 0;
            if unsafe { LGBM_BoosterGetEval(handle, data_index, &mut length, values.as_mut_ptr()) }
                != 0
            {
                return Err(error());
            }
            values.truncate(length as usize);
            Ok(values)
        }

        fn predict_regression(
            handle: BoosterHandle,
            matrix: &DenseMatrix,
            iterations: c_int,
        ) -> Result<Vec<f64>> {
            let mut output_len = 0_i64;
            if unsafe {
                LGBM_BoosterCalcNumPredict(
                    handle,
                    matrix.rows as c_int,
                    PREDICT_NORMAL,
                    0,
                    iterations,
                    &mut output_len,
                )
            } != 0
            {
                return Err(error());
            }
            let mut output = vec![0.0; output_len as usize];
            let params = CString::new("").expect("empty prediction parameters");
            if unsafe {
                LGBM_BoosterPredictForMat(
                    handle,
                    matrix.features.as_ptr().cast(),
                    DTYPE_FLOAT64,
                    matrix.rows as c_int,
                    matrix.columns as c_int,
                    1,
                    PREDICT_NORMAL,
                    0,
                    iterations,
                    params.as_ptr(),
                    &mut output_len,
                    output.as_mut_ptr(),
                )
            } != 0
            {
                return Err(error());
            }
            output.truncate(output_len as usize);
            Ok(output)
        }

        #[allow(clippy::too_many_arguments)]
        fn train_impl(
            &self,
            request: &TrainRequest,
            train: &DenseMatrix,
            validation: Option<&DenseMatrix>,
            test: Option<&DenseMatrix>,
            output_dir: &Path,
            progress: Sender<TrainEvent>,
            cancel: Arc<AtomicBool>,
        ) -> Result<TrainingArtifact> {
            train.validate()?;
            if let Some(matrix) = validation {
                matrix.validate()?;
                if matrix.columns != train.columns {
                    return Err(MlError::InvalidData(
                        "validation columns do not match train".into(),
                    ));
                }
            }
            if let Some(matrix) = test {
                matrix.validate()?;
                if matrix.columns != train.columns {
                    return Err(MlError::InvalidData(
                        "test columns do not match train".into(),
                    ));
                }
            }
            if request.factor_ids.len() != train.columns {
                return Err(MlError::InvalidData(
                    "factor_ids length does not match feature columns".into(),
                ));
            }
            fs::create_dir_all(output_dir)?;
            let mut params = request.parameters.clone();
            params
                .entry("objective".into())
                .or_insert_with(|| match request.objective {
                    Objective::Regression => "regression".into(),
                    Objective::Binary => "binary".into(),
                    Objective::Multiclass { classes } => format!("multiclass num_class={classes}"),
                });
            params.entry("metric".into()).or_insert_with(|| "l2".into());
            params
                .entry("num_iterations".into())
                .or_insert_with(|| "100".into());
            params
                .entry("verbosity".into())
                .or_insert_with(|| "-1".into());
            let parameter_string = params
                .into_iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(" ");
            let parameter_string = CString::new(parameter_string)
                .map_err(|error| MlError::InvalidData(error.to_string()))?;
            let train_dataset = Self::create_dataset(
                &train.features,
                &train.labels,
                train.rows,
                train.columns,
                &parameter_string,
                std::ptr::null_mut(),
            )?;
            let validation_dataset = validation
                .map(|matrix| {
                    Self::create_dataset(
                        &matrix.features,
                        &matrix.labels,
                        matrix.rows,
                        matrix.columns,
                        &parameter_string,
                        train_dataset.0,
                    )
                })
                .transpose()?;
            let mut booster_raw = std::ptr::null_mut();
            if unsafe {
                LGBM_BoosterCreate(train_dataset.0, parameter_string.as_ptr(), &mut booster_raw)
            } != 0
            {
                return Err(error());
            }
            let booster = Booster(booster_raw);
            if let Some(dataset) = &validation_dataset
                && unsafe { LGBM_BoosterAddValidData(booster.0, dataset.0) } != 0
            {
                return Err(error());
            }
            let _ = progress.send(TrainEvent::Started {
                rows: train.rows,
                features: train.columns,
            });
            let rounds = request
                .parameters
                .get("num_iterations")
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(100);
            let patience = request.early_stopping_rounds.unwrap_or(0);
            let mut completed = 0;
            let mut best_iteration = 0;
            let mut best_validation = f64::INFINITY;
            let mut stale_rounds = 0;
            let mut history = Vec::new();
            for iteration in 0..rounds {
                if cancel.load(Ordering::Relaxed) {
                    let _ = progress.send(TrainEvent::Cancelled);
                    return Err(MlError::Native("training cancelled".into()));
                }
                let mut empty = 0;
                if unsafe { LGBM_BoosterUpdateOneIter(booster.0, &mut empty) } != 0 {
                    return Err(error());
                }
                let train_metrics = Self::read_eval(booster.0, 0)?;
                let validation_metrics = if validation.is_some() {
                    Self::read_eval(booster.0, 1)?
                } else {
                    Vec::new()
                };
                if let Some(value) = validation_metrics.first().copied() {
                    if value < best_validation {
                        best_validation = value;
                        best_iteration = iteration + 1;
                        stale_rounds = 0;
                    } else {
                        stale_rounds += 1;
                    }
                } else {
                    best_iteration = iteration + 1;
                }
                completed = iteration + 1;
                history.push(serde_json::json!({"iteration": completed, "train": train_metrics, "validation": validation_metrics}));
                let mut metrics = train_metrics;
                metrics.extend(validation_metrics);
                let _ = progress.send(TrainEvent::Iteration {
                    iteration: completed,
                    metrics,
                });
                if empty != 0 || (patience > 0 && validation.is_some() && stale_rounds >= patience)
                {
                    break;
                }
            }
            if best_iteration == 0 {
                best_iteration = completed;
            }
            let model_path = output_dir.join("model.txt");
            let model_c = CString::new(model_path.to_string_lossy().as_bytes())
                .map_err(|error| MlError::InvalidData(error.to_string()))?;
            if unsafe {
                LGBM_BoosterSaveModel(
                    booster.0,
                    0,
                    best_iteration as c_int,
                    IMPORTANCE_GAIN,
                    model_c.as_ptr(),
                )
            } != 0
            {
                return Err(error());
            }
            let mut importance = vec![0.0; train.columns];
            if unsafe {
                LGBM_BoosterFeatureImportance(
                    booster.0,
                    best_iteration as c_int,
                    IMPORTANCE_GAIN,
                    importance.as_mut_ptr(),
                )
            } != 0
            {
                return Err(error());
            }
            let importance_path = output_dir.join("feature_importance.json");
            fs::write(&importance_path, serde_json::to_vec_pretty(&importance)?)?;
            let test_rmse = if let Some(matrix) = test {
                if matches!(request.objective, Objective::Regression) {
                    let predictions =
                        Self::predict_regression(booster.0, matrix, best_iteration as c_int)?;
                    let mse = predictions
                        .iter()
                        .zip(&matrix.labels)
                        .map(|(prediction, label)| (prediction - *label as f64).powi(2))
                        .sum::<f64>()
                        / predictions.len().max(1) as f64;
                    Some(mse.sqrt())
                } else {
                    None
                }
            } else {
                None
            };
            let features = request
                .factor_ids
                .iter()
                .map(|factor_id| analytics_core::FeatureSpec {
                    name: factor_id.clone(),
                    source_column: factor_id.clone(),
                    data_type: analytics_core::DataType::Float64,
                    categorical: false,
                })
                .collect::<Vec<_>>();
            let mut schema_hasher = Sha256::new();
            for feature in &features {
                schema_hasher.update(feature.name.as_bytes());
                schema_hasher.update([0]);
            }
            schema_hasher.update((train.columns as u64).to_le_bytes());
            let schema_hash = format!("sha256:{:x}", schema_hasher.finalize());
            let manifest = ModelManifest {
                schema_version: 1,
                model_id: uuid::Uuid::new_v4(),
                features,
                objective: format!("{:?}", request.objective),
                training_schema_hash: schema_hash,
                // Low-level matrices do not prove a formula family; orchestration supplies provenance.
                formula_version: None,
                universe_id: Some(request.universe_id.clone()),
                date_range: Some(request.date_range.clone()),
                input_snapshot_hash: None,
            };
            let manifest_path = output_dir.join("manifest.json");
            fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
            let metrics_path = output_dir.join("metrics.json");
            fs::write(
                &metrics_path,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "iterations": completed, "best_iteration": best_iteration,
                    "early_stopping_rounds": request.early_stopping_rounds,
                    "history": history, "test_rmse": test_rmse,
                }))?,
            )?;
            let _ = progress.send(TrainEvent::Finished {
                iterations: completed,
            });
            Ok(TrainingArtifact {
                model_path,
                manifest_path,
                metrics_path,
                feature_importance_path: importance_path,
            })
        }
    }

    impl Trainer for LightGbmTrainer {
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
        ) -> Result<TrainingArtifact> {
            self.train_impl(
                request,
                &DenseMatrix {
                    features: features.to_vec(),
                    labels: labels.to_vec(),
                    rows,
                    columns,
                },
                None,
                None,
                output_dir,
                progress,
                cancel,
            )
        }

        fn train_split(
            &self,
            request: &TrainRequest,
            split: &TrainingSplit,
            output_dir: &Path,
            progress: Sender<TrainEvent>,
            cancel: Arc<AtomicBool>,
        ) -> Result<TrainingArtifact> {
            split.validate()?;
            self.train_impl(
                request,
                &split.train,
                (split.validation.rows > 0).then_some(&split.validation),
                (split.test.rows > 0).then_some(&split.test),
                output_dir,
                progress,
                cancel,
            )
        }
    }

    pub use LightGbmPredictor as PublicLightGbmPredictor;
    pub use LightGbmTrainer as PublicLightGbmTrainer;
}

#[cfg(feature = "lightgbm-native")]
pub use native::PublicLightGbmPredictor as LightGbmPredictor;
#[cfg(feature = "lightgbm-native")]
pub use native::PublicLightGbmTrainer as LightGbmTrainer;

#[cfg(not(feature = "lightgbm-native"))]
/// Placeholder trainer that reports [`MlError::BackendUnavailable`].
pub struct LightGbmTrainer;

#[cfg(not(feature = "lightgbm-native"))]
impl Trainer for LightGbmTrainer {
    fn train_dense(
        &self,
        _: &TrainRequest,
        _: &[f64],
        _: &[f32],
        _: usize,
        _: usize,
        _: &Path,
        _: Sender<TrainEvent>,
        _: Arc<AtomicBool>,
    ) -> Result<TrainingArtifact> {
        Err(MlError::BackendUnavailable)
    }
}

#[cfg(not(feature = "lightgbm-native"))]
/// Placeholder predictor that reports [`MlError::BackendUnavailable`].
pub struct LightGbmPredictor;

#[cfg(not(feature = "lightgbm-native"))]
impl Predictor for LightGbmPredictor {
    fn predict_dense(&self, _: &[f64], _: usize, _: usize, _: bool) -> Result<PredictionBatch> {
        Err(MlError::BackendUnavailable)
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

    #[cfg(feature = "lightgbm-native")]
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
