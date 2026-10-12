use analytics_core::{Date, DateRange};
use arrow_array::{ArrayRef, Date32Array, Float64Array, RecordBatch, StringArray};
use arrow_schema::{DataType as ArrowDataType, Field, Schema};
use chrono::NaiveDate;
use parquet::arrow::ArrowWriter;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
};
use thiserror::Error;

mod formulas;
mod parser;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FactorExpr {
    Column(String),
    Const(f64),
    Add(Box<FactorExpr>, Box<FactorExpr>),
    Sub(Box<FactorExpr>, Box<FactorExpr>),
    Mul(Box<FactorExpr>, Box<FactorExpr>),
    Div(Box<FactorExpr>, Box<FactorExpr>),
    Neg(Box<FactorExpr>),
    Abs(Box<FactorExpr>),
    Log(Box<FactorExpr>),
    Sign(Box<FactorExpr>),
    Pow(Box<FactorExpr>, f64),
    PowExpr(Box<FactorExpr>, Box<FactorExpr>),
    Eq(Box<FactorExpr>, Box<FactorExpr>),
    Gt(Box<FactorExpr>, Box<FactorExpr>),
    Lt(Box<FactorExpr>, Box<FactorExpr>),
    If(Box<FactorExpr>, Box<FactorExpr>, Box<FactorExpr>),
    Delay(Box<FactorExpr>, usize),
    Delta(Box<FactorExpr>, usize),
    TsSum(Box<FactorExpr>, usize),
    TsMean(Box<FactorExpr>, usize),
    TsStd(Box<FactorExpr>, usize),
    TsMin(Box<FactorExpr>, usize),
    TsMax(Box<FactorExpr>, usize),
    TsRank(Box<FactorExpr>, usize),
    DecayLinear(Box<FactorExpr>, usize),
    Corr(Box<FactorExpr>, Box<FactorExpr>, usize),
    Cov(Box<FactorExpr>, Box<FactorExpr>, usize),
    RegBeta(Box<FactorExpr>, Box<FactorExpr>, usize),
    RegResi(Box<FactorExpr>, Box<FactorExpr>, usize),
    Rank(Box<FactorExpr>),
    ZScore(Box<FactorExpr>),
    Scale(Box<FactorExpr>),
    Min(Box<FactorExpr>, Box<FactorExpr>),
    Max(Box<FactorExpr>, Box<FactorExpr>),
    Count(Box<FactorExpr>, usize),
    Product(Box<FactorExpr>, usize),
    WeightedMean(Box<FactorExpr>, usize),
    Ema(Box<FactorExpr>, usize, f64),
    HighDay(Box<FactorExpr>, usize),
    LowDay(Box<FactorExpr>, usize),
    SumIf(Box<FactorExpr>, Box<FactorExpr>, usize),
    Filter(Box<FactorExpr>, Box<FactorExpr>),
    CumulativeSum(Box<FactorExpr>),
    And(Box<FactorExpr>, Box<FactorExpr>),
    Or(Box<FactorExpr>, Box<FactorExpr>),
    Sequence(usize),
    Unsupported { formula: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactorSpec {
    pub id: String,
    pub expression: FactorExpr,
    pub lookback: u32,
    pub input_columns: Vec<String>,
    pub version: String,
    pub formula: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactorRunRequest {
    pub date_range: DateRange,
    pub universe_id: String,
    pub factor_ids: Vec<String>,
    pub formula_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FactorEvent {
    Started {
        total_factors: usize,
    },
    Progress {
        factor_id: String,
        completed: usize,
        total: usize,
    },
    Finished {
        output_rows: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactorRunArtifact {
    pub factor_ids: Vec<String>,
    pub rows: usize,
    pub values: BTreeMap<String, Vec<Option<f64>>>,
    pub manifest_json: String,
}

impl FactorRunArtifact {
    pub fn write_parquet(
        &self,
        destination: impl AsRef<Path>,
        instrument_ids: &[String],
        trade_dates: &[Date],
        universe_id: &str,
    ) -> Result<()> {
        self.write_parquet_with_labels(destination, instrument_ids, trade_dates, universe_id, None)
    }

    pub fn write_parquet_with_labels(
        &self,
        destination: impl AsRef<Path>,
        instrument_ids: &[String],
        trade_dates: &[Date],
        universe_id: &str,
        label_next_return: Option<&[Option<f64>]>,
    ) -> Result<()> {
        if instrument_ids.len() != self.rows || trade_dates.len() != self.rows {
            return Err(FactorError::LengthMismatch);
        }
        if label_next_return.is_some_and(|labels| labels.len() != self.rows) {
            return Err(FactorError::LengthMismatch);
        }
        if self.values.values().any(|values| values.len() != self.rows) {
            return Err(FactorError::LengthMismatch);
        }
        let mut fields = vec![
            Field::new("instrument_id", ArrowDataType::Utf8, false),
            Field::new("trade_date", ArrowDataType::Date32, false),
            Field::new("universe_id", ArrowDataType::Utf8, false),
        ];
        fields.extend(
            self.factor_ids
                .iter()
                .map(|id| Field::new(id, ArrowDataType::Float64, true)),
        );
        if label_next_return.is_some() {
            fields.push(Field::new(
                "label_next_return",
                ArrowDataType::Float64,
                true,
            ));
        }
        let schema = std::sync::Arc::new(Schema::new(fields));
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid epoch");
        let mut columns: Vec<ArrayRef> = vec![
            std::sync::Arc::new(StringArray::from(instrument_ids.to_vec())),
            std::sync::Arc::new(Date32Array::from(
                trade_dates
                    .iter()
                    .map(|date| (*date - epoch).num_days() as i32)
                    .collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(StringArray::from(vec![universe_id.to_string(); self.rows])),
        ];
        for id in &self.factor_ids {
            let values = self
                .values
                .get(id)
                .ok_or_else(|| FactorError::InvalidRequest(format!("missing values for {id}")))?;
            columns.push(std::sync::Arc::new(Float64Array::from(values.clone())));
        }
        if let Some(labels) = label_next_return {
            columns.push(std::sync::Arc::new(Float64Array::from(labels.to_vec())));
        }
        let batch = RecordBatch::try_new(schema.clone(), columns)
            .map_err(|error| FactorError::InvalidRequest(error.to_string()))?;
        if let Some(parent) = destination.as_ref().parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| FactorError::InvalidRequest(error.to_string()))?;
        }
        let file = File::create(destination)
            .map_err(|error| FactorError::InvalidRequest(error.to_string()))?;
        let mut writer = ArrowWriter::try_new(file, schema, None)
            .map_err(|error| FactorError::InvalidRequest(error.to_string()))?;
        writer
            .write(&batch)
            .map_err(|error| FactorError::InvalidRequest(error.to_string()))?;
        writer
            .close()
            .map_err(|error| FactorError::InvalidRequest(error.to_string()))?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum FactorError {
    #[error("unknown factor: {0}")]
    UnknownFactor(String),
    #[error("factor {0} is registered but not yet executable: {1}")]
    Unsupported(String, String),
    #[error("missing input column: {0}")]
    MissingColumn(String),
    #[error("input columns have different lengths")]
    LengthMismatch,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

pub type Result<T> = std::result::Result<T, FactorError>;

pub trait FactorExecutor {
    fn compute(
        &self,
        request: FactorRunRequest,
        progress: Sender<FactorEvent>,
        cancel: Arc<AtomicBool>,
    ) -> Result<FactorRunArtifact>;
}

/// Creates the next available observation return per instrument.
/// The value at row `t` only uses a later row for the label, so it must be
/// joined after factor computation when building a training table.
pub fn next_return_labels(
    instrument_ids: &[String],
    trade_dates: &[Date],
    adjusted_close: &[Option<f64>],
) -> Result<Vec<Option<f64>>> {
    if instrument_ids.len() != trade_dates.len() || instrument_ids.len() != adjusted_close.len() {
        return Err(FactorError::LengthMismatch);
    }
    let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, instrument) in instrument_ids.iter().enumerate() {
        groups.entry(instrument.as_str()).or_default().push(index);
    }
    let mut labels = vec![None; instrument_ids.len()];
    for indices in groups.values_mut() {
        indices.sort_by_key(|index| trade_dates[*index]);
        for pair in indices.windows(2) {
            let current = adjusted_close[pair[0]];
            let next = adjusted_close[pair[1]];
            labels[pair[0]] = current.zip(next).and_then(|(current, next)| {
                (current != 0.0 && current.is_finite() && next.is_finite())
                    .then_some(next / current - 1.0)
            });
        }
    }
    Ok(labels)
}

#[derive(Debug, Clone)]
pub struct FactorRegistry {
    specs: BTreeMap<String, FactorSpec>,
}

impl Default for FactorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl FactorRegistry {
    pub fn new() -> Self {
        let mut specs = BTreeMap::new();
        for (index, formula) in formulas::ALPHA_FORMULAS.iter().enumerate() {
            let id = format!("alpha_{:03}", index + 1);
            let expression =
                parser::parse_formula(formula).unwrap_or_else(|_| FactorExpr::Unsupported {
                    formula: (*formula).to_string(),
                });
            let input_columns = parser::input_columns(formula);
            specs.insert(
                id.clone(),
                FactorSpec {
                    id,
                    expression,
                    lookback: parser::estimate_lookback(formula),
                    input_columns,
                    version: "dolphindb-gtja191-2023.01.17".into(),
                    formula: (*formula).to_string(),
                },
            );
        }
        Self { specs }
    }

    pub fn get(&self, id: &str) -> Option<&FactorSpec> {
        self.specs.get(id)
    }
    pub fn all(&self) -> impl Iterator<Item = &FactorSpec> {
        self.specs.values()
    }
    pub fn len(&self) -> usize {
        self.specs.len()
    }
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// Returns registered factors whose source formula still needs an operator
    /// implementation or source normalization before execution.
    pub fn unsupported_ids(&self) -> Vec<String> {
        self.specs
            .values()
            .filter(|spec| matches!(spec.expression, FactorExpr::Unsupported { .. }))
            .map(|spec| spec.id.clone())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct Panel {
    pub instrument_id: Vec<String>,
    pub trade_date: Vec<Date>,
    pub columns: HashMap<String, Vec<Option<f64>>>,
}

impl Panel {
    pub fn validate(&self) -> Result<()> {
        if self.instrument_id.len() != self.trade_date.len()
            || self
                .columns
                .values()
                .any(|v| v.len() != self.instrument_id.len())
        {
            return Err(FactorError::LengthMismatch);
        }
        Ok(())
    }

    fn groups_by_instrument(&self) -> BTreeMap<&str, Vec<usize>> {
        let mut groups = BTreeMap::new();
        for (index, instrument) in self.instrument_id.iter().enumerate() {
            groups
                .entry(instrument.as_str())
                .or_insert_with(Vec::new)
                .push(index);
        }
        for indices in groups.values_mut() {
            indices.sort_by_key(|index| self.trade_date[*index]);
        }
        groups
    }

    fn groups_by_date(&self) -> BTreeMap<Date, Vec<usize>> {
        let mut groups = BTreeMap::new();
        for (index, date) in self.trade_date.iter().enumerate() {
            groups.entry(*date).or_insert_with(Vec::new).push(index);
        }
        groups
    }
}

pub struct InMemoryFactorExecutor {
    pub registry: FactorRegistry,
    pub panel: Panel,
}

impl FactorExecutor for InMemoryFactorExecutor {
    fn compute(
        &self,
        request: FactorRunRequest,
        progress: Sender<FactorEvent>,
        cancel: Arc<AtomicBool>,
    ) -> Result<FactorRunArtifact> {
        self.panel.validate()?;
        if request.factor_ids.is_empty() {
            return Err(FactorError::InvalidRequest("no factors selected".into()));
        }
        let _ = progress.send(FactorEvent::Started {
            total_factors: request.factor_ids.len(),
        });
        let mut outputs = BTreeMap::new();
        let mut completed_ids = Vec::new();
        for (completed, id) in request.factor_ids.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let spec = self
                .registry
                .get(id)
                .ok_or_else(|| FactorError::UnknownFactor(id.clone()))?;
            let values = evaluate(&spec.expression, &self.panel)?;
            outputs.insert(id.clone(), values);
            completed_ids.push(id.clone());
            let _ = progress.send(FactorEvent::Progress {
                factor_id: id.clone(),
                completed: completed + 1,
                total: request.factor_ids.len(),
            });
        }
        let manifest_json = serde_json::to_string(&serde_json::json!({
            "schema_version": 1,
            "formula_version": request.formula_version,
            "universe_id": request.universe_id,
            "date_range": request.date_range,
            "factor_ids": completed_ids,
            "rows": self.panel.instrument_id.len(),
        }))
        .map_err(|e| FactorError::InvalidRequest(e.to_string()))?;
        let artifact = FactorRunArtifact {
            factor_ids: request.factor_ids.into_iter().take(outputs.len()).collect(),
            rows: self.panel.instrument_id.len(),
            values: outputs,
            manifest_json,
        };
        let _ = progress.send(FactorEvent::Finished {
            output_rows: artifact.rows,
        });
        Ok(artifact)
    }
}

fn evaluate(expr: &FactorExpr, panel: &Panel) -> Result<Vec<Option<f64>>> {
    let n = panel.instrument_id.len();
    let binary =
        |a: &[Option<f64>], b: &[Option<f64>], f: fn(f64, f64) -> f64| -> Vec<Option<f64>> {
            a.iter()
                .zip(b)
                .map(|(x, y)| match (x, y) {
                    (Some(x), Some(y)) if y.is_finite() => Some(f(*x, *y)),
                    _ => None,
                })
                .collect()
        };
    match expr {
        FactorExpr::Column(name) => panel
            .columns
            .get(name)
            .cloned()
            .ok_or_else(|| FactorError::MissingColumn(name.clone())),
        FactorExpr::Const(value) => Ok(vec![Some(*value); n]),
        FactorExpr::Add(a, b) => Ok(binary(
            &evaluate(a, panel)?,
            &evaluate(b, panel)?,
            |x, y| x + y,
        )),
        FactorExpr::Sub(a, b) => Ok(binary(
            &evaluate(a, panel)?,
            &evaluate(b, panel)?,
            |x, y| x - y,
        )),
        FactorExpr::Mul(a, b) => Ok(binary(
            &evaluate(a, panel)?,
            &evaluate(b, panel)?,
            |x, y| x * y,
        )),
        FactorExpr::Div(a, b) => Ok(binary(&evaluate(a, panel)?, &evaluate(b, panel)?, |x, y| {
            if y == 0.0 { f64::NAN } else { x / y }
        })
        .into_iter()
        .map(|x| x.filter(|v| v.is_finite()))
        .collect()),
        FactorExpr::Neg(a) => Ok(evaluate(a, panel)?
            .into_iter()
            .map(|v| v.map(|x| -x))
            .collect()),
        FactorExpr::Abs(a) => Ok(evaluate(a, panel)?
            .into_iter()
            .map(|v| v.map(f64::abs))
            .collect()),
        FactorExpr::Log(a) => Ok(evaluate(a, panel)?
            .into_iter()
            .map(|v| v.and_then(|x| (x > 0.0).then_some(x.ln())))
            .collect()),
        FactorExpr::Sign(a) => Ok(evaluate(a, panel)?
            .into_iter()
            .map(|v| v.map(|x| x.signum()))
            .collect()),
        FactorExpr::Pow(a, p) => Ok(evaluate(a, panel)?
            .into_iter()
            .map(|v| v.map(|x| x.powf(*p)))
            .collect()),
        FactorExpr::PowExpr(a, b) => Ok(binary(
            &evaluate(a, panel)?,
            &evaluate(b, panel)?,
            |x, y| x.powf(y),
        )),
        FactorExpr::Eq(a, b) => Ok(binary(
            &evaluate(a, panel)?,
            &evaluate(b, panel)?,
            |x, y| (x == y) as i32 as f64,
        )),
        FactorExpr::Gt(a, b) => Ok(binary(
            &evaluate(a, panel)?,
            &evaluate(b, panel)?,
            |x, y| (x > y) as i32 as f64,
        )),
        FactorExpr::Lt(a, b) => Ok(binary(
            &evaluate(a, panel)?,
            &evaluate(b, panel)?,
            |x, y| (x < y) as i32 as f64,
        )),
        FactorExpr::If(c, a, b) => {
            let c = evaluate(c, panel)?;
            let a = evaluate(a, panel)?;
            let b = evaluate(b, panel)?;
            Ok(c.into_iter()
                .zip(a)
                .zip(b)
                .map(|((c, a), b)| if c.unwrap_or(0.0) != 0.0 { a } else { b })
                .collect())
        }
        FactorExpr::Delay(a, window) => delay_values(&evaluate(a, panel)?, panel, *window),
        FactorExpr::Delta(a, window) => {
            let input = evaluate(a, panel)?;
            let delayed = delay_values(&input, panel, *window)?;
            Ok(input
                .into_iter()
                .zip(delayed)
                .map(|(a, b)| a.zip(b).map(|(a, b)| a - b))
                .collect())
        }
        FactorExpr::TsSum(a, w) => rolling_unary(&evaluate(a, panel)?, panel, *w, |v| {
            Some(v.iter().flatten().sum())
        }),
        FactorExpr::TsMean(a, w) => rolling_unary(&evaluate(a, panel)?, panel, *w, |v| {
            let x: Vec<_> = v.iter().flatten().copied().collect();
            (!x.is_empty()).then(|| x.iter().sum::<f64>() / x.len() as f64)
        }),
        FactorExpr::TsStd(a, w) => rolling_unary(&evaluate(a, panel)?, panel, *w, |v| {
            let x: Vec<_> = v.iter().flatten().copied().collect();
            if x.len() < 2 {
                None
            } else {
                let m = x.iter().sum::<f64>() / x.len() as f64;
                Some((x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (x.len() - 1) as f64).sqrt())
            }
        }),
        FactorExpr::TsMin(a, w) => rolling_unary(&evaluate(a, panel)?, panel, *w, |v| {
            v.iter().flatten().copied().reduce(f64::min)
        }),
        FactorExpr::TsMax(a, w) => rolling_unary(&evaluate(a, panel)?, panel, *w, |v| {
            v.iter().flatten().copied().reduce(f64::max)
        }),
        FactorExpr::TsRank(a, w) => rolling_unary(&evaluate(a, panel)?, panel, *w, |v| {
            let current = v.last().copied().flatten()?;
            let x: Vec<_> = v.iter().flatten().copied().collect();
            Some(x.iter().filter(|value| **value <= current).count() as f64 / x.len() as f64)
        }),
        FactorExpr::DecayLinear(a, w) => rolling_unary(&evaluate(a, panel)?, panel, *w, |v| {
            let x: Vec<_> = v.iter().flatten().copied().collect();
            if x.is_empty() {
                None
            } else {
                let denom = (x.len() * (x.len() + 1) / 2) as f64;
                Some(
                    x.iter()
                        .enumerate()
                        .map(|(i, v)| *v * (i + 1) as f64)
                        .sum::<f64>()
                        / denom,
                )
            }
        }),
        FactorExpr::Rank(a) => cross_section_rank(&evaluate(a, panel)?, panel),
        FactorExpr::ZScore(a) => cross_section_zscore(&evaluate(a, panel)?, panel),
        FactorExpr::Scale(a) => cross_section_scale(&evaluate(a, panel)?, panel),
        FactorExpr::Corr(a, b, w) => {
            rolling_binary(&evaluate(a, panel)?, &evaluate(b, panel)?, panel, *w, corr)
        }
        FactorExpr::Cov(a, b, w) => {
            rolling_binary(&evaluate(a, panel)?, &evaluate(b, panel)?, panel, *w, cov)
        }
        FactorExpr::RegBeta(a, b, w) => rolling_binary(
            &evaluate(a, panel)?,
            &evaluate(b, panel)?,
            panel,
            *w,
            |x, y| {
                cov(x, y)
                    .zip(var(y))
                    .and_then(|(c, v)| (v != 0.0).then_some(c / v))
            },
        ),
        FactorExpr::RegResi(a, b, w) => rolling_binary(
            &evaluate(a, panel)?,
            &evaluate(b, panel)?,
            panel,
            *w,
            |x, y| {
                if x.len() < 2 || x.len() != y.len() {
                    return None;
                }
                let mean_x = x.iter().sum::<f64>() / x.len() as f64;
                let mean_y = y.iter().sum::<f64>() / y.len() as f64;
                let beta = cov(x, y)
                    .zip(var(y))
                    .and_then(|(c, v)| (v != 0.0).then_some(c / v))?;
                Some(x.last()? - (mean_x + beta * (y.last()? - mean_y)))
            },
        ),
        FactorExpr::Min(a, b) => Ok(binary(&evaluate(a, panel)?, &evaluate(b, panel)?, f64::min)),
        FactorExpr::Max(a, b) => Ok(binary(&evaluate(a, panel)?, &evaluate(b, panel)?, f64::max)),
        FactorExpr::Count(a, w) => rolling_unary(&evaluate(a, panel)?, panel, *w, |values| {
            Some(
                values
                    .iter()
                    .flatten()
                    .filter(|value| **value != 0.0)
                    .count() as f64,
            )
        }),
        FactorExpr::Product(a, w) => rolling_unary(&evaluate(a, panel)?, panel, *w, |values| {
            let mut product = 1.0;
            let mut count = 0;
            for value in values.iter().flatten() {
                product *= *value;
                count += 1;
            }
            (count > 0).then_some(product)
        }),
        FactorExpr::WeightedMean(a, w) => {
            rolling_unary(&evaluate(a, panel)?, panel, *w, |values| {
                let values: Vec<_> = values.iter().flatten().copied().collect();
                if values.is_empty() {
                    None
                } else {
                    let denom = (values.len() * (values.len() + 1) / 2) as f64;
                    Some(
                        values
                            .iter()
                            .enumerate()
                            .map(|(i, value)| *value * (i + 1) as f64)
                            .sum::<f64>()
                            / denom,
                    )
                }
            })
        }
        FactorExpr::Ema(a, w, alpha) => rolling_unary(&evaluate(a, panel)?, panel, *w, |values| {
            let mut result = None;
            let factor = *alpha;
            for value in values.iter().flatten() {
                result = Some(match result {
                    Some(previous) => factor * *value + (1.0 - factor) * previous,
                    None => *value,
                });
            }
            result
        }),
        FactorExpr::HighDay(a, w) => rolling_unary(&evaluate(a, panel)?, panel, *w, |values| {
            let values: Vec<_> = values.iter().flatten().copied().collect();
            values.iter().enumerate().rev().find_map(|(index, value)| {
                (value == values.iter().max_by(|left, right| left.total_cmp(right))?)
                    .then_some((values.len() - index) as f64)
            })
        }),
        FactorExpr::LowDay(a, w) => rolling_unary(&evaluate(a, panel)?, panel, *w, |values| {
            let values: Vec<_> = values.iter().flatten().copied().collect();
            values.iter().enumerate().rev().find_map(|(index, value)| {
                (value == values.iter().min_by(|left, right| left.total_cmp(right))?)
                    .then_some((values.len() - index) as f64)
            })
        }),
        FactorExpr::SumIf(value, condition, w) => {
            let values = evaluate(value, panel)?;
            let conditions = evaluate(condition, panel)?;
            rolling_binary(&values, &conditions, panel, *w, |values, conditions| {
                Some(
                    values
                        .iter()
                        .zip(conditions)
                        .filter_map(|(value, condition)| (*condition != 0.0).then_some(*value))
                        .sum(),
                )
            })
        }
        FactorExpr::Filter(value, condition) => {
            let values = evaluate(value, panel)?;
            let conditions = evaluate(condition, panel)?;
            Ok(values
                .into_iter()
                .zip(conditions)
                .map(|(value, condition)| condition.filter(|v| *v != 0.0).and(value))
                .collect())
        }
        FactorExpr::CumulativeSum(value) => {
            let input = evaluate(value, panel)?;
            let mut output = vec![None; n];
            for indices in panel.groups_by_instrument().values() {
                let mut total = 0.0;
                for index in indices {
                    if let Some(value) = input[*index] {
                        total += value;
                        output[*index] = Some(total);
                    }
                }
            }
            Ok(output)
        }
        FactorExpr::And(a, b) => Ok(binary(
            &evaluate(a, panel)?,
            &evaluate(b, panel)?,
            |x, y| ((x != 0.0) && (y != 0.0)) as i32 as f64,
        )),
        FactorExpr::Or(a, b) => Ok(binary(
            &evaluate(a, panel)?,
            &evaluate(b, panel)?,
            |x, y| ((x != 0.0) || (y != 0.0)) as i32 as f64,
        )),
        FactorExpr::Sequence(w) => {
            let mut output = vec![None; n];
            for indices in panel.groups_by_instrument().values() {
                for (position, index) in indices.iter().enumerate() {
                    output[*index] = Some((position % (*w).max(1)) as f64);
                }
            }
            Ok(output)
        }
        FactorExpr::Unsupported { formula } => {
            Err(FactorError::Unsupported("alpha".into(), formula.clone()))
        }
    }
}

fn rolling_unary(
    input: &[Option<f64>],
    panel: &Panel,
    w: usize,
    f: impl Fn(&[Option<f64>]) -> Option<f64>,
) -> Result<Vec<Option<f64>>> {
    let mut out = vec![None; input.len()];
    for indices in panel.groups_by_instrument().values() {
        for (pos, idx) in indices.iter().enumerate() {
            if w == 0 || pos + 1 < w {
                continue;
            }
            let start = pos.saturating_add(1).saturating_sub(w);
            out[*idx] = f(&indices[start..=pos]
                .iter()
                .map(|i| input[*i])
                .collect::<Vec<_>>());
        }
    }
    Ok(out)
}

fn delay_values(input: &[Option<f64>], panel: &Panel, window: usize) -> Result<Vec<Option<f64>>> {
    let mut out = vec![None; input.len()];
    for indices in panel.groups_by_instrument().values() {
        for (position, index) in indices.iter().enumerate() {
            out[*index] = position
                .checked_sub(window)
                .and_then(|previous| input[indices[previous]]);
        }
    }
    Ok(out)
}

fn rolling_binary(
    a: &[Option<f64>],
    b: &[Option<f64>],
    panel: &Panel,
    w: usize,
    f: impl Fn(&[f64], &[f64]) -> Option<f64>,
) -> Result<Vec<Option<f64>>> {
    let mut out = vec![None; a.len()];
    for inds in panel.groups_by_instrument().values() {
        for (pos, idx) in inds.iter().enumerate() {
            if w == 0 || pos + 1 < w {
                continue;
            }
            let start = pos.saturating_add(1).saturating_sub(w);
            let mut x = Vec::new();
            let mut y = Vec::new();
            for i in &inds[start..=pos] {
                if let (Some(xi), Some(yi)) = (a[*i], b[*i]) {
                    x.push(xi);
                    y.push(yi)
                }
            }
            out[*idx] = f(&x, &y);
        }
    }
    Ok(out)
}
fn corr(x: &[f64], y: &[f64]) -> Option<f64> {
    if x.len() < 2 || x.len() != y.len() {
        return None;
    }
    let mx = x.iter().sum::<f64>() / x.len() as f64;
    let my = y.iter().sum::<f64>() / y.len() as f64;
    let (n, a, b) = x.iter().zip(y).fold((0.0, 0.0, 0.0), |(n, a, b), (x, y)| {
        (
            n + (x - mx) * (y - my),
            a + (x - mx).powi(2),
            b + (y - my).powi(2),
        )
    });
    (a > 0.0 && b > 0.0).then_some(n / (a * b).sqrt())
}
fn cov(x: &[f64], y: &[f64]) -> Option<f64> {
    if x.len() < 2 || x.len() != y.len() {
        return None;
    }
    let mx = x.iter().sum::<f64>() / x.len() as f64;
    let my = y.iter().sum::<f64>() / y.len() as f64;
    Some(
        x.iter()
            .zip(y)
            .map(|(x, y)| (x - mx) * (y - my))
            .sum::<f64>()
            / (x.len() - 1) as f64,
    )
}
fn var(x: &[f64]) -> Option<f64> {
    cov(x, x)
}

fn cross_section_rank(input: &[Option<f64>], panel: &Panel) -> Result<Vec<Option<f64>>> {
    let mut out = vec![None; input.len()];
    for inds in panel.groups_by_date().values() {
        let vals: Vec<_> = inds.iter().filter_map(|i| input[*i]).collect();
        for i in inds {
            if let Some(v) = input[*i] {
                out[*i] = Some(vals.iter().filter(|x| **x <= v).count() as f64 / vals.len() as f64)
            }
        }
    }
    Ok(out)
}
fn cross_section_zscore(input: &[Option<f64>], panel: &Panel) -> Result<Vec<Option<f64>>> {
    let mut out = vec![None; input.len()];
    for inds in panel.groups_by_date().values() {
        let vals: Vec<_> = inds.iter().filter_map(|i| input[*i]).collect();
        if vals.len() < 2 {
            continue;
        }
        let m = vals.iter().sum::<f64>() / vals.len() as f64;
        let s =
            (vals.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (vals.len() - 1) as f64).sqrt();
        for i in inds {
            if let Some(v) = input[*i] {
                out[*i] = (s > 0.0).then_some((v - m) / s)
            }
        }
    }
    Ok(out)
}
fn cross_section_scale(input: &[Option<f64>], panel: &Panel) -> Result<Vec<Option<f64>>> {
    let mut out = vec![None; input.len()];
    for inds in panel.groups_by_date().values() {
        let denom: f64 = inds.iter().filter_map(|i| input[*i]).map(f64::abs).sum();
        for i in inds {
            if let Some(v) = input[*i] {
                out[*i] = (denom > 0.0).then_some(v / denom)
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn panel() -> Panel {
        Panel {
            instrument_id: vec![
                "A".into(),
                "A".into(),
                "A".into(),
                "B".into(),
                "B".into(),
                "B".into(),
            ],
            trade_date: (1..=3)
                .map(|d| NaiveDate::from_ymd_opt(2024, 1, d).unwrap())
                .chain((1..=3).map(|d| NaiveDate::from_ymd_opt(2024, 1, d).unwrap()))
                .collect(),
            columns: [
                (
                    "open".into(),
                    vec![
                        Some(1.0),
                        Some(2.0),
                        Some(3.0),
                        Some(2.0),
                        Some(2.0),
                        Some(4.0),
                    ],
                ),
                (
                    "close".into(),
                    vec![
                        Some(2.0),
                        Some(3.0),
                        Some(4.0),
                        Some(1.0),
                        Some(3.0),
                        Some(5.0),
                    ],
                ),
                ("high".into(), vec![Some(2.0); 6]),
                ("low".into(), vec![Some(1.0); 6]),
                (
                    "volume".into(),
                    vec![
                        Some(10.0),
                        Some(20.0),
                        Some(30.0),
                        Some(11.0),
                        Some(21.0),
                        Some(31.0),
                    ],
                ),
            ]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn registry_contains_all_191() {
        assert_eq!(FactorRegistry::default().len(), 191);
    }

    #[test]
    fn alpha_one_executes() {
        let p = panel();
        let r = FactorRegistry::default();
        let values = evaluate(&r.get("alpha_001").unwrap().expression, &p).unwrap();
        assert_eq!(values.len(), 6);
    }

    #[test]
    fn factor_result_writes_parquet_cache() {
        let p = panel();
        let executor = InMemoryFactorExecutor {
            registry: FactorRegistry::default(),
            panel: p.clone(),
        };
        let (sender, _receiver) = std::sync::mpsc::channel();
        let request = FactorRunRequest {
            date_range: DateRange {
                start: p.trade_date[0],
                end: *p.trade_date.last().unwrap(),
            },
            universe_id: "csi500".into(),
            factor_ids: vec!["alpha_001".into()],
            formula_version: "dolphindb-gtja191-2023.01.17".into(),
        };
        let artifact = executor
            .compute(request, sender, Arc::new(AtomicBool::new(false)))
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("factors.parquet");
        artifact
            .write_parquet(&destination, &p.instrument_id, &p.trade_date, "csi500")
            .unwrap();
        assert!(destination.exists());
        assert!(std::fs::metadata(destination).unwrap().len() > 0);
    }

    #[test]
    fn factor_output_preserves_requested_feature_order() {
        let p = panel();
        let executor = InMemoryFactorExecutor {
            registry: FactorRegistry::default(),
            panel: p.clone(),
        };
        let (sender, _) = std::sync::mpsc::channel();
        let artifact = executor
            .compute(
                FactorRunRequest {
                    date_range: DateRange {
                        start: p.trade_date[0],
                        end: *p.trade_date.last().unwrap(),
                    },
                    universe_id: "csi500".into(),
                    factor_ids: vec!["alpha_014".into(), "alpha_002".into()],
                    formula_version: "test".into(),
                },
                sender,
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert_eq!(artifact.factor_ids, ["alpha_014", "alpha_002"]);
    }

    #[test]
    fn next_return_labels_are_per_instrument_and_leave_tail_null() {
        let p = panel();
        let close = p.columns.get("close").unwrap();
        let labels = next_return_labels(&p.instrument_id, &p.trade_date, close).unwrap();
        assert_eq!(labels[0], Some(0.5));
        assert_eq!(labels[2], None);
        assert_eq!(labels[3], Some(2.0));
        assert_eq!(labels[5], None);
    }

    #[test]
    fn registry_reports_source_formulas_needing_normalization() {
        let registry = FactorRegistry::default();
        assert_eq!(registry.len(), 191);
        let unsupported = registry.unsupported_ids();
        assert!(unsupported.is_empty(), "unparsed factors: {unsupported:?}");
    }

    #[test]
    fn every_registered_factor_executes_against_complete_fixture_schema() {
        let registry = FactorRegistry::default();
        let mut fixture = panel();
        let columns = registry
            .all()
            .flat_map(|spec| spec.input_columns.iter().cloned())
            .collect::<std::collections::BTreeSet<_>>();
        let fixture_len = fixture.instrument_id.len();
        for column in columns {
            fixture
                .columns
                .entry(column)
                .or_insert_with(|| vec![Some(1.0); fixture_len]);
        }
        for spec in registry.all() {
            evaluate(&spec.expression, &fixture)
                .unwrap_or_else(|error| panic!("{} failed: {error}", spec.id));
        }
    }

    #[test]
    fn dolphin_db_golden_fixture_matches_expected_values() {
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/alpha191_expected.json")).unwrap();
        let expected = golden["expected"]["alpha_002"].as_array().unwrap();
        let panel = panel();
        let actual = evaluate(
            &FactorRegistry::default()
                .get("alpha_002")
                .unwrap()
                .expression,
            &panel,
        )
        .unwrap();
        for (actual, expected) in actual.iter().zip(expected) {
            match (actual, expected) {
                (None, value) if value.is_null() => {}
                (Some(actual), value) => {
                    let expected = value.as_f64().unwrap();
                    assert!((actual - expected).abs() <= 1e-12);
                }
                _ => panic!("golden null mismatch"),
            }
        }
    }

    #[test]
    fn factor_value_does_not_depend_on_future_observation() {
        let registry = FactorRegistry::default();
        let panel = panel();
        let baseline = evaluate(&registry.get("alpha_002").unwrap().expression, &panel).unwrap();
        let mut changed = panel.clone();
        changed.columns.get_mut("close").unwrap()[2] = Some(10_000.0);
        let recomputed =
            evaluate(&registry.get("alpha_002").unwrap().expression, &changed).unwrap();
        assert_eq!(baseline[0], recomputed[0]);
        assert_eq!(baseline[1], recomputed[1]);
    }
}
