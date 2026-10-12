use analytics_core::{Date, DateRange, MarketRow, Objective, TrainRequest};
use analytics_engine::DuckDbEngine;
use analytics_factor::{
    FactorExecutor, FactorRegistry, FactorRunRequest, InMemoryFactorExecutor, Panel,
    next_return_labels,
};
use analytics_ml::{
    DenseMatrix, LightGbmPredictor, LightGbmTrainer, Predictor, TrainEvent, Trainer, TrainingSplit,
};
use arrow_array::{ArrayRef, Date32Array, Float64Array, RecordBatch, StringArray};
use arrow_schema::{DataType as ArrowDataType, Field, Schema};
use chrono::NaiveDate;
use parquet::arrow::ArrowWriter;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    env,
    fs::File,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool, mpsc},
};

#[derive(Debug)]
struct Args {
    market: PathBuf,
    universe: PathBuf,
    universe_id: String,
    date_range: DateRange,
    factors: Vec<String>,
    output: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args(env::args().skip(1).collect())?;
    std::fs::create_dir_all(&args.output)?;
    let mut engine = DuckDbEngine::open_in_memory()?;
    let rows = engine.load_market_rows(
        &args.market,
        &args.universe,
        &args.universe_id,
        &args.date_range,
    )?;
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|row| row.is_trading.unwrap_or(true))
        .filter(|row| {
            row.listing_date
                .map(|listed| listed <= row.trade_date)
                .unwrap_or(true)
        })
        .collect();
    if rows.is_empty() {
        return Err("market/universe join returned no rows".into());
    }
    let mut panel = panel_from_rows(&rows);
    let registry = FactorRegistry::default();
    for id in &args.factors {
        if registry.get(id).is_none() {
            return Err(format!("unknown factor {id}").into());
        }
    }
    let panel_len = panel.instrument_id.len();
    for id in &args.factors {
        for column in &registry.get(id).unwrap().input_columns {
            panel
                .columns
                .entry(column.clone())
                .or_insert_with(|| vec![None; panel_len]);
        }
    }
    let executor = InMemoryFactorExecutor {
        registry,
        panel: panel.clone(),
    };
    let (factor_tx, factor_rx) = mpsc::channel();
    let factor_artifact = executor.compute(
        FactorRunRequest {
            date_range: args.date_range.clone(),
            universe_id: args.universe_id.clone(),
            factor_ids: args.factors.clone(),
            formula_version: "dolphindb-gtja191-2023.01.17".into(),
        },
        factor_tx,
        Arc::new(AtomicBool::new(false)),
    )?;
    drain_factor_events(factor_rx);
    let adjusted_close = panel.columns.get("adj_close").ok_or("adj_close missing")?;
    let labels = next_return_labels(&panel.instrument_id, &panel.trade_date, adjusted_close)?;
    let factor_cache = args.output.join("factors.parquet");
    factor_artifact.write_parquet_with_labels(
        &factor_cache,
        &panel.instrument_id,
        &panel.trade_date,
        &args.universe_id,
        Some(&labels),
    )?;
    write_factor_manifest(&args, &factor_artifact.manifest_json, &factor_cache)?;
    let bounds = split_dates(&panel.trade_date)?;
    let matrices = build_training_split(
        &factor_artifact,
        &args.factors,
        &labels,
        &panel.instrument_id,
        &panel.trade_date,
        bounds,
    );
    let request = TrainRequest {
        factor_ids: args.factors.clone(),
        universe_id: args.universe_id.clone(),
        date_range: args.date_range.clone(),
        objective: Objective::Regression,
        parameters: BTreeMap::from([
            ("num_iterations".into(), "100".into()),
            ("learning_rate".into(), "0.08".into()),
            ("num_leaves".into(), "15".into()),
            ("min_data_in_leaf".into(), "10".into()),
            ("seed".into(), "17".into()),
        ]),
        early_stopping_rounds: Some(8),
    };
    let (train_tx, train_rx) = mpsc::channel();
    let artifact = LightGbmTrainer.train_split(
        &request,
        &matrices.split,
        &args.output,
        train_tx,
        Arc::new(AtomicBool::new(false)),
    )?;
    drain_train_events(train_rx);
    annotate_model_manifest(
        &artifact.manifest_path,
        bounds,
        &snapshot_hash(&args.market, &args.universe)?,
    )?;
    let predictor = LightGbmPredictor {
        model_path: artifact.model_path.clone(),
    };
    let predictions = predictor.predict_dense(
        &matrices.split.test.features,
        matrices.split.test.rows,
        matrices.split.test.columns,
        false,
    )?;
    let prediction_path = args.output.join("predictions.parquet");
    write_predictions(
        &prediction_path,
        &matrices.test_rows,
        &predictions.predictions,
    )?;
    println!(
        "factor_cache={}\nmodel={}\npredictions={}\ntrain_rows={}\nvalidation_rows={}\ntest_rows={}",
        factor_cache.display(),
        artifact.model_path.display(),
        prediction_path.display(),
        matrices.split.train.rows,
        matrices.split.validation.rows,
        matrices.split.test.rows
    );
    Ok(())
}

fn parse_args(arguments: Vec<String>) -> Result<Args, String> {
    let mut market = None;
    let mut universe = None;
    let mut universe_id = "csi500".to_string();
    let mut start = None;
    let mut end = None;
    let mut factors = Vec::new();
    let mut output = None;
    let mut index = 0;
    while index < arguments.len() {
        let key = &arguments[index];
        index += 1;
        let next = |index: &mut usize| {
            let value = arguments
                .get(*index)
                .cloned()
                .ok_or_else(|| format!("missing value for {key}"))?;
            *index += 1;
            Ok::<String, String>(value)
        };
        match key.as_str() {
            "--market" => market = Some(PathBuf::from(next(&mut index)?)),
            "--universe" => universe = Some(PathBuf::from(next(&mut index)?)),
            "--universe-id" => universe_id = next(&mut index)?,
            "--start" => {
                start = Some(
                    next(&mut index)?
                        .parse::<Date>()
                        .map_err(|e| e.to_string())?,
                )
            }
            "--end" => {
                end = Some(
                    next(&mut index)?
                        .parse::<Date>()
                        .map_err(|e| e.to_string())?,
                )
            }
            "--factor" => factors.push(next(&mut index)?),
            "--output" => output = Some(PathBuf::from(next(&mut index)?)),
            "--help" => return Err(usage()),
            other => return Err(format!("unknown argument {other}\n{}", usage())),
        }
    }
    Ok(Args {
        market: market.ok_or_else(usage)?,
        universe: universe.ok_or_else(usage)?,
        universe_id,
        date_range: DateRange {
            start: start.ok_or_else(usage)?,
            end: end.ok_or_else(usage)?,
        },
        factors: if factors.is_empty() {
            vec!["alpha_001".into(), "alpha_002".into()]
        } else {
            factors
        },
        output: output.ok_or_else(usage)?,
    })
}

fn usage() -> String {
    "usage: factor_pipeline --market PATH --universe PATH --start YYYY-MM-DD --end YYYY-MM-DD --output PATH [--universe-id csi500] [--factor alpha_001]".into()
}

fn panel_from_rows(rows: &[MarketRow]) -> Panel {
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

#[allow(clippy::type_complexity)]
fn split_dates(
    dates: &[Date],
) -> Result<(Date, Date, Date, Date, Date, Date), Box<dyn std::error::Error>> {
    let mut unique = dates.to_vec();
    unique.sort();
    unique.dedup();
    if unique.len() < 3 {
        return Err("at least three trading dates are required".into());
    }
    let train_end = (unique.len() * 70 / 100).max(1).min(unique.len() - 2);
    let validation_end = (unique.len() * 85 / 100)
        .max(train_end + 1)
        .min(unique.len() - 1);
    Ok((
        unique[0],
        unique[train_end - 1],
        unique[train_end],
        unique[validation_end - 1],
        unique[validation_end],
        *unique.last().unwrap(),
    ))
}

struct SplitOutput {
    split: TrainingSplit,
    test_rows: Vec<(String, Date)>,
}

fn build_training_split(
    artifact: &analytics_factor::FactorRunArtifact,
    factor_ids: &[String],
    labels: &[Option<f64>],
    instruments: &[String],
    dates: &[Date],
    bounds: (Date, Date, Date, Date, Date, Date),
) -> SplitOutput {
    let (train_start, train_end, validation_start, validation_end, test_start, test_end) = bounds;
    let mut rows = [Vec::new(), Vec::new(), Vec::new()];
    for (index, date) in dates.iter().enumerate() {
        let Some(label) = labels[index] else { continue };
        let bucket = if *date >= train_start && *date <= train_end {
            0
        } else if *date >= validation_start && *date <= validation_end {
            1
        } else if *date >= test_start && *date <= test_end {
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

fn write_predictions(
    path: &Path,
    rows: &[(String, Date)],
    predictions: &[Vec<f64>],
) -> Result<(), Box<dyn std::error::Error>> {
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
    )?;
    let mut writer = ArrowWriter::try_new(File::create(path)?, schema, None)?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}

fn write_factor_manifest(
    args: &Args,
    base: &str,
    cache: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut value: serde_json::Value = serde_json::from_str(base)?;
    value["market_path"] = args.market.to_string_lossy().to_string().into();
    value["universe_path"] = args.universe.to_string_lossy().to_string().into();
    value["universe_id"] = args.universe_id.clone().into();
    value["date_range"] = serde_json::to_value(&args.date_range)?;
    value["factor_cache"] = cache.to_string_lossy().to_string().into();
    value["input_snapshot_hash"] = snapshot_hash(&args.market, &args.universe)?.into();
    value["price_mode"] = "adjusted_price".into();
    value["missing_value_rule"] = "preserve_null".into();
    let registry = FactorRegistry::default();
    value["max_lookback"] = args
        .factors
        .iter()
        .filter_map(|id| registry.get(id).map(|spec| spec.lookback))
        .max()
        .unwrap_or_default()
        .into();
    std::fs::write(
        cache.with_file_name("factor_run_manifest.json"),
        serde_json::to_vec_pretty(&value)?,
    )?;
    Ok(())
}

fn annotate_model_manifest(
    path: &Path,
    bounds: (Date, Date, Date, Date, Date, Date),
    input_snapshot_hash: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    value["split_boundaries"] = serde_json::json!({
        "train_start": bounds.0,
        "train_end": bounds.1,
        "validation_start": bounds.2,
        "validation_end": bounds.3,
        "test_start": bounds.4,
        "test_end": bounds.5,
    });
    value["input_snapshot_hash"] = input_snapshot_hash.into();
    std::fs::write(path, serde_json::to_vec_pretty(&value)?)?;
    Ok(())
}

fn snapshot_hash(market: &Path, universe: &Path) -> Result<String, Box<dyn std::error::Error>> {
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
    Ok(format!("sha256:{:x}", hasher.finalize()))
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

fn walkdir(root: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
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

fn drain_factor_events(receiver: mpsc::Receiver<analytics_factor::FactorEvent>) {
    while let Ok(event) = receiver.try_recv() {
        eprintln!("factor_event={event:?}");
    }
}
fn drain_train_events(receiver: mpsc::Receiver<TrainEvent>) {
    while let Ok(event) = receiver.try_recv() {
        if let TrainEvent::Iteration { iteration, metrics } = event {
            eprintln!("iteration={iteration} metrics={metrics:?}");
        }
    }
}
