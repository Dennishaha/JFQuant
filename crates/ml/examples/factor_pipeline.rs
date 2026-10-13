//! Thin CLI shell over `analytics_ml::pipeline::run_factor_pipeline`.
//!
//! Runs the native factor/training/prediction path against local market and
//! historical-universe Parquet files and writes the factor cache, model,
//! metrics, and predictions into the requested output directory.

use analytics_core::{Date, DateRange};
use analytics_ml::TrainEvent;
use analytics_ml::pipeline::{
    PipelineEvent, PipelineRequest, TrainingConfig, cancellation, run_factor_pipeline,
};
use std::{env, path::PathBuf, sync::mpsc};

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
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            match event {
                PipelineEvent::LoadingMarket => eprintln!("stage=load_market"),
                PipelineEvent::MarketLoaded { rows } => eprintln!("market_rows={rows}"),
                PipelineEvent::Factors(event) => eprintln!("factor_event={event:?}"),
                PipelineEvent::Training(TrainEvent::Iteration { iteration, metrics }) => {
                    eprintln!("iteration={iteration} metrics={metrics:?}");
                }
                PipelineEvent::Training(event) => eprintln!("train_event={event:?}"),
                PipelineEvent::WritingArtifacts => eprintln!("stage=write_artifacts"),
            }
        }
    });
    let artifact = run_factor_pipeline(
        &PipelineRequest {
            market_path: args.market,
            universe_path: args.universe,
            universe_id: args.universe_id,
            date_range: args.date_range,
            factor_ids: args.factors,
            output_dir: args.output,
            training: TrainingConfig::default(),
        },
        tx,
        cancellation(),
    )?;
    println!(
        "factor_cache={}\nmodel={}\npredictions={}\ntrain_rows={}\nvalidation_rows={}\ntest_rows={}",
        artifact
            .factor_cache_path
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        artifact.training.model_path.display(),
        artifact.predictions_path.display(),
        artifact.train_rows,
        artifact.validation_rows,
        artifact.test_rows
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
