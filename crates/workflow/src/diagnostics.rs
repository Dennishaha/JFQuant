//! Diagnostics.
use crate::Result;
use analytics_core::task::TaskContext;
use serde_json::{Value, json};
use std::{fs, path::Path};

/// Invoke on a worker thread; generated test artifacts remain in the supplied local folder.
/// Verify native query, training and prediction in the packaged runtime.
pub fn self_check(root: &Path) -> Result<Value> {
    use crate::research::{
        DenseMatrix, LightGbmPredictor, LightGbmTrainer, Predictor, Trainer, TrainingSplit,
    };
    use analytics_engine::{DuckDbEngine, QueryEngine};
    fs::create_dir_all(root)?;
    let mut engine = DuckDbEngine::open_in_memory()?;
    let query = engine.query_page(analytics_core::PageRequest {
        sql: "SELECT 42 AS answer, version() AS duckdb_version".into(),
        limit: 100,
        sort: Vec::new(),
        after: None,
    })?;
    let matrix = || DenseMatrix {
        features: (0..24).flat_map(|i| [i as f64, (i % 3) as f64]).collect(),
        labels: (0..24).map(|i| (i as f32) * 0.1).collect(),
        rows: 24,
        columns: 2,
    };
    let split = TrainingSplit {
        train: matrix(),
        validation: matrix(),
        test: matrix(),
    };
    let request = analytics_core::TrainRequest {
        factor_ids: vec!["self_check_1".into(), "self_check_2".into()],
        universe_id: "synthetic".into(),
        date_range: analytics_core::DateRange {
            start: "2020-01-01".parse()?,
            end: "2020-12-31".parse()?,
        },
        objective: analytics_core::Objective::Regression,
        parameters: std::collections::BTreeMap::from([
            ("num_iterations".into(), "3".into()),
            ("num_leaves".into(), "3".into()),
            ("min_data_in_leaf".into(), "2".into()),
            ("num_threads".into(), "2".into()),
            ("verbosity".into(), "-1".into()),
        ]),
        early_stopping_rounds: None,
    };
    let (tx, _) = std::sync::mpsc::channel();
    let model =
        LightGbmTrainer.train_split(&request, &split, root, tx, TaskContext::current().cancel)?;
    let scores = LightGbmPredictor {
        model_path: model.model_path,
    }
    .predict_dense(&split.test.features, 24, 2, false)?
    .predictions;
    if scores.len() != 24 || scores.iter().flatten().any(|v| !v.is_finite()) {
        return Err("native prediction self-check failed".into());
    }
    Ok(
        json!({"ok":true,"query":query,"predictions":scores.len(),"formula_version":analytics_factor::FORMULA_VERSION}),
    )
}
