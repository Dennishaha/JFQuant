use analytics_core::{DateRange, Objective, TrainRequest};
use analytics_ml::{
    DenseMatrix, LightGbmPredictor, LightGbmTrainer, Predictor, TrainEvent, Trainer, TrainingSplit,
};
use chrono::NaiveDate;
use std::{
    collections::BTreeMap,
    env,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool, mpsc},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("jfquant-lightgbm-smoke"));
    std::fs::create_dir_all(&output)?;
    let features = vec![
        0.0, 1.0, 0.0, 1.0, 0.0, 2.0, 0.0, 2.0, 0.0, 1.0, 1.0, 2.0, 0.0, 2.0, 1.0, 4.0, 1.0, 1.0,
        1.0, 2.0, 1.0, 2.0, 1.0, 4.0, 1.0, 1.0, 2.0, 2.0, 1.0, 3.0, 1.0, 4.0,
    ];
    let labels: Vec<f32> = (0..8)
        .map(|row| 0.25 * features[row * 4] as f32 + 0.5 * features[row * 4 + 1] as f32)
        .collect();
    let mut parameters = BTreeMap::new();
    parameters.insert("num_iterations".into(), "12".into());
    parameters.insert("learning_rate".into(), "0.2".into());
    parameters.insert("num_leaves".into(), "7".into());
    parameters.insert("min_data_in_leaf".into(), "1".into());
    parameters.insert("seed".into(), "7".into());
    let request = TrainRequest {
        factor_ids: vec![
            "feature_000".into(),
            "feature_001".into(),
            "feature_002".into(),
            "feature_003".into(),
        ],
        universe_id: "smoke".into(),
        date_range: DateRange {
            start: NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            end: NaiveDate::from_ymd_opt(2024, 1, 8).unwrap(),
        },
        objective: Objective::Regression,
        parameters,
        early_stopping_rounds: Some(3),
    };
    let split = TrainingSplit {
        train: DenseMatrix {
            features: features[..16].to_vec(),
            labels: labels[..4].to_vec(),
            rows: 4,
            columns: 4,
        },
        validation: DenseMatrix {
            features: features[16..24].to_vec(),
            labels: labels[4..6].to_vec(),
            rows: 2,
            columns: 4,
        },
        test: DenseMatrix {
            features: features[24..].to_vec(),
            labels: labels[6..].to_vec(),
            rows: 2,
            columns: 4,
        },
    };
    let (sender, receiver) = mpsc::channel();
    let artifact = LightGbmTrainer.train_split(
        &request,
        &split,
        &output,
        sender,
        Arc::new(AtomicBool::new(false)),
    )?;
    while let Ok(event) = receiver.try_recv() {
        if let TrainEvent::Iteration { iteration, metrics } = event {
            eprintln!("iteration={iteration} metrics={metrics:?}");
        }
    }
    let predictor = LightGbmPredictor {
        model_path: artifact.model_path.clone(),
    };
    let predictions = predictor.predict_dense(&split.test.features, 2, 4, false)?;
    let contributions = predictor.predict_dense(&split.test.features, 2, 4, true)?;
    assert_eq!(predictions.predictions.len(), 2);
    assert_eq!(contributions.contributions.as_ref().unwrap().len(), 2);
    println!("model={}", artifact.model_path.display());
    println!("prediction_0={:?}", predictions.predictions[0]);
    println!(
        "contribution_0={:?}",
        contributions.contributions.as_ref().unwrap()[0]
    );
    Ok(())
}
