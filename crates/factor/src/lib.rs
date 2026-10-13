//! Alpha191 registry and panel executor.
//!
//! The registry contains all 191 canonical GTJA Alpha191 formula strings
//! pinned to the DolphinDB reference version; a small recursive-descent
//! parser normalizes them into the [`FactorExpr`] DSL. The panel executor
//! evaluates expressions with time-series groups by instrument and
//! cross-sectional groups by trade date; missing history stays null rather
//! than being forward-filled, matching the "missing values are data" rule.
//!
//! ```text
//! FactorSpec registry -> Panel executor -> {time-series, cross-sectional} ops -> Parquet cache
//! ```
#![warn(missing_docs)]

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

pub mod audit;
mod formulas;
mod parser;

/// Pinned formula and operator interpretation; required by execution requests.
pub const FORMULA_VERSION: &str = "gtja191-paper-2017+jfquant-audit-v4";

/// Factor expression AST produced by the formula parser and executed by the
/// panel executor. Window lengths are in observations (per instrument).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FactorExpr {
    /// Panel input column (`close`, `volume`, ...).
    Column(String),
    /// Numeric literal.
    Const(f64),
    /// Element-wise addition.
    Add(Box<FactorExpr>, Box<FactorExpr>),
    /// Element-wise subtraction.
    Sub(Box<FactorExpr>, Box<FactorExpr>),
    /// Element-wise multiplication.
    Mul(Box<FactorExpr>, Box<FactorExpr>),
    /// Element-wise division (null on zero divisor).
    Div(Box<FactorExpr>, Box<FactorExpr>),
    /// Element-wise negation.
    Neg(Box<FactorExpr>),
    /// Element-wise absolute value.
    Abs(Box<FactorExpr>),
    /// Natural logarithm (null for non-positive input).
    Log(Box<FactorExpr>),
    /// Sign function (-1, 0, 1).
    Sign(Box<FactorExpr>),
    /// Constant exponent power.
    Pow(Box<FactorExpr>, f64),
    /// Expression exponent power.
    PowExpr(Box<FactorExpr>, Box<FactorExpr>),
    /// Element-wise equality (1.0/0.0).
    Eq(Box<FactorExpr>, Box<FactorExpr>),
    /// Element-wise greater-than (1.0/0.0).
    Gt(Box<FactorExpr>, Box<FactorExpr>),
    /// Element-wise less-than (1.0/0.0).
    Lt(Box<FactorExpr>, Box<FactorExpr>),
    /// Conditional select: `If(cond, then, else)`.
    If(Box<FactorExpr>, Box<FactorExpr>, Box<FactorExpr>),
    /// Value `n` observations ago (time series).
    Delay(Box<FactorExpr>, usize),
    /// Difference from `n` observations ago (time series).
    Delta(Box<FactorExpr>, usize),
    /// Rolling sum over `n` observations.
    TsSum(Box<FactorExpr>, usize),
    /// Rolling mean over `n` observations.
    TsMean(Box<FactorExpr>, usize),
    /// Rolling standard deviation over `n` observations.
    TsStd(Box<FactorExpr>, usize),
    /// Rolling minimum over `n` observations.
    TsMin(Box<FactorExpr>, usize),
    /// Rolling maximum over `n` observations.
    TsMax(Box<FactorExpr>, usize),
    /// Rank of the latest value within the trailing `n` observations.
    TsRank(Box<FactorExpr>, usize),
    /// Linearly decayed weighted mean over `n` observations.
    DecayLinear(Box<FactorExpr>, usize),
    /// Rolling Pearson correlation of two series over `n` observations.
    Corr(Box<FactorExpr>, Box<FactorExpr>, usize),
    /// Rolling covariance of two series over `n` observations.
    Cov(Box<FactorExpr>, Box<FactorExpr>, usize),
    /// Rolling OLS beta of the first series on the second over `n` points.
    RegBeta(Box<FactorExpr>, Box<FactorExpr>, usize),
    /// Rolling OLS residual of the first series on the second.
    RegResi(Box<FactorExpr>, Box<FactorExpr>, usize),
    /// Rolling OLS residual with an intercept and every supplied predictor.
    RegResiMulti(Box<FactorExpr>, Vec<FactorExpr>, usize),
    /// Cross-sectional percentile rank per trade date.
    Rank(Box<FactorExpr>),
    /// Cross-sectional z-score per trade date.
    ZScore(Box<FactorExpr>),
    /// Cross-sectional scaling to sum(|x|) = 1 per trade date.
    Scale(Box<FactorExpr>),
    /// Element-wise minimum of two series.
    Min(Box<FactorExpr>, Box<FactorExpr>),
    /// Element-wise maximum of two series.
    Max(Box<FactorExpr>, Box<FactorExpr>),
    /// Rolling count of non-null truthy values over `n` observations.
    Count(Box<FactorExpr>, usize),
    /// Rolling product over `n` observations.
    Product(Box<FactorExpr>, usize),
    /// Rolling weighted mean over `n` observations.
    WeightedMean(Box<FactorExpr>, usize),
    /// Chinese SMA(X, N, M), recursively smoothed with M/N.
    Ema(Box<FactorExpr>, usize, f64),
    /// Observations since the rolling `n`-window maximum occurred.
    HighDay(Box<FactorExpr>, usize),
    /// Observations since the rolling `n`-window minimum occurred.
    LowDay(Box<FactorExpr>, usize),
    /// Rolling sum of the first series where the second is truthy.
    SumIf(Box<FactorExpr>, Box<FactorExpr>, usize),
    /// Values of the first series where the second is truthy, else null.
    Filter(Box<FactorExpr>, Box<FactorExpr>),
    /// Cumulative sum per instrument.
    CumulativeSum(Box<FactorExpr>),
    /// Element-wise logical and (1.0/0.0).
    And(Box<FactorExpr>, Box<FactorExpr>),
    /// Element-wise logical or (1.0/0.0).
    Or(Box<FactorExpr>, Box<FactorExpr>),
    /// Time positions used by regression; rolling slopes are invariant to the
    /// origin, so positions continue increasing instead of wrapping at n.
    Sequence(usize),
    /// Formula parsed but needs an operator that is not yet implemented.
    /// Formula parsed but needs an operator that is not yet implemented.
    Unsupported {
        /// Original formula text.
        formula: String,
    },
}

impl FactorExpr {
    fn history_requirement(&self) -> (u32, bool) {
        use FactorExpr::*;
        let merge = |a: (u32, bool), b: (u32, bool)| (a.0.max(b.0), a.1 || b.1);
        match self {
            Column(_) | Const(_) | Sequence(_) => (1, false),
            Unsupported { .. } => (0, false),
            Neg(a) | Abs(a) | Log(a) | Sign(a) | Pow(a, _) | Rank(a) | ZScore(a) | Scale(a) => {
                a.history_requirement()
            }
            Ema(a, ..) | CumulativeSum(a) => {
                let (n, _) = a.history_requirement();
                (n, true)
            }
            Delay(a, n) | Delta(a, n) => {
                let (lookback, r) = a.history_requirement();
                (lookback.saturating_add(*n as u32), r)
            }
            TsSum(a, n)
            | TsMean(a, n)
            | TsStd(a, n)
            | TsMin(a, n)
            | TsMax(a, n)
            | TsRank(a, n)
            | DecayLinear(a, n)
            | Count(a, n)
            | Product(a, n)
            | WeightedMean(a, n)
            | HighDay(a, n)
            | LowDay(a, n) => {
                let (lookback, r) = a.history_requirement();
                (lookback.saturating_add(n.saturating_sub(1) as u32), r)
            }
            Corr(a, b, n) | Cov(a, b, n) | RegBeta(a, b, n) | RegResi(a, b, n) | SumIf(a, b, n) => {
                let (lookback, r) = merge(a.history_requirement(), b.history_requirement());
                (lookback.saturating_add(n.saturating_sub(1) as u32), r)
            }
            RegResiMulti(y, x, n) => {
                let (lookback, r) = x.iter().fold(y.history_requirement(), |acc, x| {
                    merge(acc, x.history_requirement())
                });
                (lookback.saturating_add(n.saturating_sub(1) as u32), r)
            }
            If(c, a, b) => merge(
                c.history_requirement(),
                merge(a.history_requirement(), b.history_requirement()),
            ),
            Add(a, b)
            | Sub(a, b)
            | Mul(a, b)
            | Div(a, b)
            | PowExpr(a, b)
            | Eq(a, b)
            | Gt(a, b)
            | Lt(a, b)
            | Min(a, b)
            | Max(a, b)
            | Filter(a, b)
            | And(a, b)
            | Or(a, b) => merge(a.history_requirement(), b.history_requirement()),
        }
    }
}

/// One registered Alpha191 factor: parsed expression plus catalog metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactorSpec {
    /// Canonical ID such as `alpha_001`.
    pub id: String,
    /// Parsed expression (or [`FactorExpr::Unsupported`]).
    pub expression: FactorExpr,
    /// Minimum observations for the first finite result, composed through the AST.
    /// Recursive smoothers additionally depend on all history since their seed.
    pub lookback: u32,
    /// Whether execution depends on recursive state before the finite lookback.
    #[serde(default)]
    pub requires_recursive_state: bool,
    /// Panel input columns the expression reads.
    pub input_columns: Vec<String>,
    /// Formula catalog version pin.
    pub version: String,
    /// Original published formula string.
    pub formula: String,
}

/// One factor computation run over a panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactorRunRequest {
    /// Inclusive date bounds (validated by the caller).
    pub date_range: DateRange,
    /// Universe recorded in the output manifest.
    pub universe_id: String,
    /// Factors to compute, in output column order.
    pub factor_ids: Vec<String>,
    /// Formula catalog version pin recorded in the manifest.
    pub formula_version: String,
}

/// Progress events emitted by [`FactorExecutor::compute`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FactorEvent {
    /// Computation started.
    Started {
        /// Number of factors that will be computed.
        total_factors: usize,
    },
    /// One factor finished.
    Progress {
        /// Factor that just completed.
        factor_id: String,
        /// Factors completed so far.
        completed: usize,
        /// Total factors in this run.
        total: usize,
    },
    /// All factors finished.
    Finished {
        /// Rows in the output panel.
        output_rows: usize,
    },
}

/// Computed factor values plus the serialized run manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactorRunArtifact {
    /// Factor IDs in output column order.
    pub factor_ids: Vec<String>,
    /// Panel row count.
    pub rows: usize,
    /// Nullable values keyed by factor ID, each `rows` long.
    pub values: BTreeMap<String, Vec<Option<f64>>>,
    /// Run manifest JSON (formula version, factor IDs, ...).
    pub manifest_json: String,
}

impl FactorRunArtifact {
    /// Writes the wide factor cache Parquet without a label column.
    ///
    /// # Errors
    ///
    /// Returns [`FactorError::LengthMismatch`] when key or value lengths do
    /// not match `rows`.
    pub fn write_parquet(
        &self,
        destination: impl AsRef<Path>,
        instrument_ids: &[String],
        trade_dates: &[Date],
        universe_id: &str,
    ) -> Result<()> {
        self.write_parquet_with_labels(destination, instrument_ids, trade_dates, universe_id, None)
    }

    /// Writes the wide factor cache Parquet with an optional
    /// `label_next_return` column appended.
    ///
    /// # Errors
    ///
    /// Returns [`FactorError::LengthMismatch`] when key, label, or value
    /// lengths do not match `rows`.
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

/// Errors returned by parsing, execution, and factor cache writing.
#[derive(Debug, Error)]
pub enum FactorError {
    /// Requested factor ID is absent from the registry.
    #[error("unknown factor: {0}")]
    UnknownFactor(String),
    /// Formula is registered but its expression is not executable.
    #[error("factor {0} is registered but not yet executable: {1}")]
    Unsupported(String, String),
    /// Expression references a missing panel column.
    #[error("missing input column: {0}")]
    MissingColumn(String),
    /// Parallel panel vectors have different lengths.
    #[error("input columns have different lengths")]
    LengthMismatch,
    /// Request or output contract is invalid.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

/// Convenience result alias for factor calls.
pub type Result<T> = std::result::Result<T, FactorError>;

/// Executes a factor request and emits progress while preserving null values.
pub trait FactorExecutor {
    /// Computes selected factors. Cancellation is checked between factors;
    /// when set, the implementation returns the artifact for completed work
    /// or an error according to its executor contract.
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
///
/// # Errors
///
/// Returns [`FactorError::LengthMismatch`] when the three input vectors do not
/// have equal lengths. The last observation of every instrument is `None`.
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

/// Registry of the pinned 191 Alpha191 formula specifications.
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
    /// Builds the 191-entry registry from the pinned formula catalog.
    pub fn new() -> Self {
        let mut specs = BTreeMap::new();
        for (index, formula) in formulas::ALPHA_FORMULAS.iter().enumerate() {
            let id = format!("alpha_{:03}", index + 1);
            let expression = if let Some(reason) = audit::quarantine_reason(index + 1) {
                FactorExpr::Unsupported {
                    formula: format!("{}: {reason}", formula),
                }
            } else {
                parser::parse_formula(formula).unwrap_or_else(|error| FactorExpr::Unsupported {
                    formula: format!("{formula}: {error}"),
                })
            };
            let input_columns = parser::input_columns(formula);
            let (lookback, requires_recursive_state) = expression.history_requirement();
            specs.insert(
                id.clone(),
                FactorSpec {
                    id,
                    expression,
                    lookback,
                    requires_recursive_state,
                    input_columns,
                    version: FORMULA_VERSION.into(),
                    formula: (*formula).to_string(),
                },
            );
        }
        Self { specs }
    }

    /// Looks up a factor by canonical ID.
    pub fn get(&self, id: &str) -> Option<&FactorSpec> {
        self.specs.get(id)
    }
    /// Iterates factors in canonical ID order.
    pub fn all(&self) -> impl Iterator<Item = &FactorSpec> {
        self.specs.values()
    }
    /// Number of registered factors.
    pub fn len(&self) -> usize {
        self.specs.len()
    }
    /// Whether the registry has no factors.
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// Returns registered factors whose source formula still needs an operator
    /// implementation or source normalization before execution.
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

/// Row-aligned market inputs consumed by the factor executor.
#[derive(Debug, Clone)]
pub struct Panel {
    /// Instrument ID per row.
    pub instrument_id: Vec<String>,
    /// Trade date per row.
    pub trade_date: Vec<Date>,
    /// Nullable numeric inputs keyed by column name.
    pub columns: HashMap<String, Vec<Option<f64>>>,
}

impl Panel {
    /// Checks all vectors have the same row count.
    pub fn validate(&self) -> Result<()> {
        if self.instrument_id.len() != self.trade_date.len()
            || self
                .columns
                .values()
                .any(|v| v.len() != self.instrument_id.len())
        {
            return Err(FactorError::LengthMismatch);
        }
        let mut keys = std::collections::HashSet::new();
        for (id, date) in self.instrument_id.iter().zip(&self.trade_date) {
            if !keys.insert((id, date)) {
                return Err(FactorError::InvalidRequest(
                    "duplicate instrument/date in factor panel".into(),
                ));
            }
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
            if self
                .columns
                .get("__universe_member")
                .is_some_and(|values| values[index] != Some(1.0))
            {
                continue;
            }
            groups.entry(*date).or_insert_with(Vec::new).push(index);
        }
        groups
    }
}

/// In-memory factor executor for one panel and registry.
pub struct InMemoryFactorExecutor {
    /// Factor definitions used by this executor.
    pub registry: FactorRegistry,
    /// Input panel; kept worker-local during computation.
    pub panel: Panel,
}

impl InMemoryFactorExecutor {
    fn validate_request(&self, request: &FactorRunRequest) -> Result<()> {
        self.panel.validate()?;
        if request.factor_ids.is_empty() || request.date_range.start > request.date_range.end {
            return Err(FactorError::InvalidRequest(
                "empty factor selection or reversed date range".into(),
            ));
        }
        let mut selected = std::collections::HashSet::new();
        for id in &request.factor_ids {
            if !selected.insert(id) {
                return Err(FactorError::InvalidRequest("duplicate factor IDs".into()));
            }
            let spec = self
                .registry
                .get(id)
                .ok_or_else(|| FactorError::UnknownFactor(id.clone()))?;
            if request.formula_version != spec.version {
                return Err(FactorError::InvalidRequest(format!(
                    "formula version mismatch: requested {}, executing {}",
                    request.formula_version, spec.version
                )));
            }
            if let FactorExpr::Unsupported { formula } = &spec.expression {
                return Err(FactorError::Unsupported(id.clone(), formula.clone()));
            }
        }
        Ok(())
    }
    /// Compute one factor buffer at a time, sharing panel group indices. The
    /// callback may stream selected rows to disk rather than retaining a wide
    /// `Option<f64>` panel. Callback errors stop the run; cancellation is an error.
    pub fn compute_streaming(
        &self,
        request: &FactorRunRequest,
        cancel: &AtomicBool,
        mut consume: impl FnMut(&FactorSpec, &[Option<f64>]) -> Result<()>,
    ) -> Result<()> {
        self.validate_request(request)?;
        let context = EvaluationPanel::new(&self.panel);
        for id in &request.factor_ids {
            if cancel.load(Ordering::Relaxed) {
                return Err(FactorError::InvalidRequest(
                    "factor calculation cancelled".into(),
                ));
            }
            let spec = self
                .registry
                .get(id)
                .ok_or_else(|| FactorError::UnknownFactor(id.clone()))?;
            let values = evaluate(&spec.expression, &context)?;
            consume(spec, &values)?;
        }
        Ok(())
    }
}

impl FactorExecutor for InMemoryFactorExecutor {
    fn compute(
        &self,
        request: FactorRunRequest,
        progress: Sender<FactorEvent>,
        cancel: Arc<AtomicBool>,
    ) -> Result<FactorRunArtifact> {
        self.validate_request(&request)?;
        let _ = progress.send(FactorEvent::Started {
            total_factors: request.factor_ids.len(),
        });
        let context = EvaluationPanel::new(&self.panel);
        let mut outputs = BTreeMap::new();
        let mut completed_ids = Vec::new();
        for (completed, id) in request.factor_ids.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Err(FactorError::InvalidRequest(
                    "factor calculation cancelled".into(),
                ));
            }
            let spec = self
                .registry
                .get(id)
                .ok_or_else(|| FactorError::UnknownFactor(id.clone()))?;
            let values = evaluate(&spec.expression, &context)?;
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

struct EvaluationPanel<'a> {
    panel: &'a Panel,
    instruments: BTreeMap<&'a str, Vec<usize>>,
    dates: BTreeMap<Date, Vec<usize>>,
}
impl<'a> EvaluationPanel<'a> {
    fn new(panel: &'a Panel) -> Self {
        Self {
            panel,
            instruments: panel.groups_by_instrument(),
            dates: panel.groups_by_date(),
        }
    }
    fn groups_by_instrument(&self) -> &BTreeMap<&'a str, Vec<usize>> {
        &self.instruments
    }
    fn groups_by_date(&self) -> &BTreeMap<Date, Vec<usize>> {
        &self.dates
    }
}
impl std::ops::Deref for EvaluationPanel<'_> {
    type Target = Panel;
    fn deref(&self) -> &Panel {
        self.panel
    }
}

fn evaluate(expr: &FactorExpr, panel: &EvaluationPanel<'_>) -> Result<Vec<Option<f64>>> {
    let n = panel.instrument_id.len();
    let binary =
        |a: &[Option<f64>], b: &[Option<f64>], f: fn(f64, f64) -> f64| -> Vec<Option<f64>> {
            a.iter()
                .zip(b)
                .map(|(x, y)| match (x, y) {
                    (Some(x), Some(y)) if x.is_finite() && y.is_finite() => {
                        Some(f(*x, *y)).filter(|v| v.is_finite())
                    }
                    _ => None,
                })
                .collect()
        };
    match expr {
        FactorExpr::Column(name) => panel
            .columns
            .get(name)
            .map(|values| values.iter().map(|v| v.filter(|x| x.is_finite())).collect())
            .ok_or_else(|| FactorError::MissingColumn(name.clone())),
        FactorExpr::Const(value) => Ok(vec![value.is_finite().then_some(*value); n]),
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
            .map(|v| v.map(|x| if x == 0.0 { 0.0 } else { x.signum() }))
            .collect()),
        FactorExpr::Pow(a, p) => Ok(evaluate(a, panel)?
            .into_iter()
            .map(|v| v.map(|x| x.powf(*p)).filter(|x| x.is_finite()))
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
                .map(|((c, a), b)| c.and_then(|c| if c != 0.0 { a } else { b }))
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
            Some(
                x.iter()
                    .filter(|value| **value <= current || rank_tied(**value, current))
                    .count() as f64
                    / x.len() as f64,
            )
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
        FactorExpr::RegBeta(a, b, w) => rolling_binary_with_filter(
            &evaluate(a, panel)?,
            &evaluate(b, panel)?,
            panel,
            *w,
            matches!(a.as_ref(), FactorExpr::Filter(..))
                && matches!(b.as_ref(), FactorExpr::Filter(..)),
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
        FactorExpr::RegResiMulti(response, predictors, window) => {
            let y = evaluate(response, panel)?;
            let x: Vec<_> = predictors
                .iter()
                .map(|p| evaluate(p, panel))
                .collect::<Result<_>>()?;
            let mut out = vec![None; n];
            for indices in panel.groups_by_instrument().values() {
                for position in window.saturating_sub(1)..indices.len() {
                    if *window == 0 {
                        continue;
                    }
                    let rows = &indices[position + 1 - window..=position];
                    out[indices[position]] = ols_residual(&y, &x, rows);
                }
            }
            Ok(out)
        }
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
                let (mut total, mut weights) = (0.0, 0.0);
                for (lag, value) in values.iter().rev().enumerate() {
                    let weight = 0.9_f64.powi(lag as i32);
                    total += (*value)? * weight;
                    weights += weight;
                }
                Some(total / weights)
            })
        }
        FactorExpr::Ema(a, w, m) => {
            if *w == 0 || *m <= 0.0 || *m > *w as f64 {
                return Err(FactorError::InvalidRequest(
                    "SMA requires 0 < M <= N".into(),
                ));
            }
            let input = evaluate(a, panel)?;
            let mut out = vec![None; input.len()];
            let alpha = *m / *w as f64;
            for indices in panel.groups_by_instrument().values() {
                let mut previous = None;
                for index in indices {
                    if let Some(value) = input[*index].filter(|value| value.is_finite()) {
                        let value = previous.map_or(value, |p| alpha * value + (1.0 - alpha) * p);
                        previous = Some(value);
                        out[*index] = Some(value);
                    }
                }
            }
            Ok(out)
        }
        FactorExpr::HighDay(a, w) => rolling_unary(&evaluate(a, panel)?, panel, *w, |values| {
            let values: Vec<_> = values.iter().flatten().copied().collect();
            values.iter().enumerate().rev().find_map(|(index, value)| {
                (value == values.iter().max_by(|left, right| left.total_cmp(right))?)
                    .then_some((values.len() - 1 - index) as f64)
            })
        }),
        FactorExpr::LowDay(a, w) => rolling_unary(&evaluate(a, panel)?, panel, *w, |values| {
            let values: Vec<_> = values.iter().flatten().copied().collect();
            values.iter().enumerate().rev().find_map(|(index, value)| {
                (value == values.iter().min_by(|left, right| left.total_cmp(right))?)
                    .then_some((values.len() - 1 - index) as f64)
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
                    if let Some(value) = input[*index].filter(|value| value.is_finite()) {
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
        FactorExpr::Sequence(_) => {
            // Absolute positions give every rolling regression the same 1..N
            // slope (translation does not change covariance); never wrap.
            let mut output = vec![None; n];
            for indices in panel.groups_by_instrument().values() {
                for (position, index) in indices.iter().enumerate() {
                    output[*index] = Some((position + 1) as f64);
                }
            }
            Ok(output)
        }
        FactorExpr::Unsupported { formula } => {
            Err(FactorError::Unsupported("alpha".into(), formula.clone()))
        }
    }
}

fn ols_residual(y: &[Option<f64>], x: &[Vec<Option<f64>>], rows: &[usize]) -> Option<f64> {
    let k = x.len() + 1;
    if rows.len() <= k {
        return None;
    }
    let mut a = vec![vec![0.0; k + 1]; k];
    for &row in rows {
        let y = y[row].filter(|v| v.is_finite())?;
        let mut design = vec![1.0];
        for column in x {
            design.push(column[row].filter(|v| v.is_finite())?);
        }
        for i in 0..k {
            for j in 0..k {
                a[i][j] += design[i] * design[j];
            }
            a[i][k] += design[i] * y;
        }
    }
    for i in 0..k {
        let pivot = (i..k).max_by(|&l, &r| a[l][i].abs().total_cmp(&a[r][i].abs()))?;
        a.swap(i, pivot);
        let divisor = a[i][i];
        if divisor.abs() <= 1e-12 {
            return None;
        }
        for value in &mut a[i][i..=k] {
            *value /= divisor;
        }
        let pivot_row = a[i].clone();
        for (row_index, row) in a.iter_mut().enumerate() {
            if row_index != i {
                let scale = row[i];
                for (value, pivot) in row[i..=k].iter_mut().zip(&pivot_row[i..=k]) {
                    *value -= scale * pivot;
                }
            }
        }
    }
    let last = *rows.last()?;
    let prediction = a[0][k]
        + x.iter()
            .enumerate()
            .map(|(i, col)| a[i + 1][k] * col[last].unwrap())
            .sum::<f64>();
    (y[last]? - prediction)
        .is_finite()
        .then_some(y[last]? - prediction)
}

fn rolling_unary(
    input: &[Option<f64>],
    panel: &EvaluationPanel<'_>,
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
            let window: Vec<_> = indices[start..=pos].iter().map(|i| input[*i]).collect();
            if window.iter().all(|v| v.is_some_and(f64::is_finite)) {
                out[*idx] = f(&window).filter(|v| v.is_finite());
            }
        }
    }
    Ok(out)
}

fn delay_values(
    input: &[Option<f64>],
    panel: &EvaluationPanel<'_>,
    window: usize,
) -> Result<Vec<Option<f64>>> {
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
    panel: &EvaluationPanel<'_>,
    w: usize,
    f: impl Fn(&[f64], &[f64]) -> Option<f64>,
) -> Result<Vec<Option<f64>>> {
    rolling_binary_with_filter(a, b, panel, w, false, f)
}
fn rolling_binary_with_filter(
    a: &[Option<f64>],
    b: &[Option<f64>],
    panel: &EvaluationPanel<'_>,
    w: usize,
    filtered: bool,
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
            // FILTER intentionally selects observations for conditional regressions.
            // All other rolling binary operations require the complete window.
            if x.len() == w || filtered {
                out[*idx] = f(&x, &y).filter(|v| v.is_finite());
            }
        }
    }
    Ok(out)
}
fn corr(x: &[f64], y: &[f64]) -> Option<f64> {
    if x.len() < 2 || x.len() != y.len() {
        return None;
    }
    if x.len() == 2 {
        let dx = x[1] - x[0];
        let dy = y[1] - y[0];
        return (dx != 0.0 && dy != 0.0).then_some(if dx.signum() == dy.signum() {
            1.0
        } else {
            -1.0
        });
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

fn rank_tied(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-12 * a.abs().max(b.abs()).max(1.0)
}

fn cross_section_rank(
    input: &[Option<f64>],
    panel: &EvaluationPanel<'_>,
) -> Result<Vec<Option<f64>>> {
    let mut out = vec![None; input.len()];
    for inds in panel.groups_by_date().values() {
        let vals: Vec<_> = inds.iter().filter_map(|i| input[*i]).collect();
        for i in inds {
            if let Some(v) = input[*i] {
                out[*i] = Some(
                    vals.iter()
                        .filter(|x| **x <= v || rank_tied(**x, v))
                        .count() as f64
                        / vals.len() as f64,
                )
            }
        }
    }
    Ok(out)
}
fn cross_section_zscore(
    input: &[Option<f64>],
    panel: &EvaluationPanel<'_>,
) -> Result<Vec<Option<f64>>> {
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
fn cross_section_scale(
    input: &[Option<f64>],
    panel: &EvaluationPanel<'_>,
) -> Result<Vec<Option<f64>>> {
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

    #[test]
    fn chinese_sma_uses_m_over_n_recurrence_and_keeps_missing_outputs() {
        let dates: Vec<_> = (1..=5)
            .map(|d| Date::from_ymd_opt(2024, 1, d).unwrap())
            .collect();
        let panel = Panel {
            instrument_id: vec!["A".into(); 5],
            trade_date: dates,
            columns: HashMap::from([(
                "close".into(),
                vec![Some(1.0), Some(4.0), None, Some(7.0), Some(10.0)],
            )]),
        };
        let expr = FactorExpr::Ema(Box::new(FactorExpr::Column("close".into())), 3, 1.0);
        let actual = evaluate(&expr, &EvaluationPanel::new(&panel)).unwrap();
        let expected = [
            Some(1.0),
            Some(2.0),
            None,
            Some(11.0 / 3.0),
            Some(52.0 / 9.0),
        ];
        for (a, b) in actual.into_iter().zip(expected) {
            match (a, b) {
                (Some(a), Some(b)) => assert!((a - b).abs() < 1e-12),
                (None, None) => {}
                _ => panic!("SMA golden mismatch"),
            };
        }
    }

    #[test]
    fn zero_max_is_elementwise_and_sequence_regression_does_not_wrap() {
        let mut p = panel();
        p.columns.insert(
            "close".into(),
            vec![
                Some(-2.0),
                Some(3.0),
                Some(8.0),
                Some(1.0),
                Some(6.0),
                Some(11.0),
            ],
        );
        let context = EvaluationPanel::new(&p);
        let maximum = parser::parse_formula("MAX(CLOSE,0)").unwrap();
        assert_eq!(
            evaluate(&maximum, &context).unwrap(),
            vec![
                Some(0.0),
                Some(3.0),
                Some(8.0),
                Some(1.0),
                Some(6.0),
                Some(11.0)
            ]
        );
        for formula in ["REGBETA(CLOSE,SEQUENCE,2)", "REGBETA(CLOSE,SEQUENCE(2))"] {
            let actual = evaluate(&parser::parse_formula(formula).unwrap(), &context).unwrap();
            assert_eq!(
                actual,
                vec![None, Some(5.0), Some(5.0), None, Some(5.0), Some(5.0)]
            );
        }
    }

    #[test]
    fn source_typo_aliases_match_price_volume_fixture() {
        let panel = panel();
        let expression = parser::parse_formula("VOL+HGIH+L+DELAY(C LOSE)").unwrap();
        let actual = evaluate(&expression, &EvaluationPanel::new(&panel)).unwrap();
        assert_eq!(
            actual,
            vec![None, Some(25.0), Some(36.0), None, Some(25.0), Some(37.0)]
        );
    }
    #[test]
    fn cross_sectional_rank_excludes_nonmembers_but_retains_their_price_history() {
        let panel = Panel {
            instrument_id: vec!["A".into(), "B".into(), "C".into()],
            trade_date: vec![Date::from_ymd_opt(2024, 1, 2).unwrap(); 3],
            columns: HashMap::from([
                ("close".into(), vec![Some(1.0), Some(2.0), Some(3.0)]),
                (
                    "__universe_member".into(),
                    vec![Some(1.0), Some(0.0), Some(1.0)],
                ),
            ]),
        };
        let values = evaluate(
            &FactorExpr::Rank(Box::new(FactorExpr::Column("close".into()))),
            &EvaluationPanel::new(&panel),
        )
        .unwrap();
        assert!(values[0].is_some());
        assert_eq!(values[1], None);
        assert!(values[2] > values[0]);
    }
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
    fn all_executable_factors_match_independent_numpy_operator_golden() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/operator_golden.json")).unwrap();
        assert_eq!(fixture["formula_version"], FORMULA_VERSION);
        let days = fixture["days"].as_u64().unwrap() as usize;
        let stocks = fixture["stocks"].as_u64().unwrap() as usize;
        let first = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let p = Panel {
            instrument_id: (0..days)
                .flat_map(|_| (0..stocks).map(|s| format!("S{s}")))
                .collect(),
            trade_date: (0..days)
                .flat_map(|d| std::iter::repeat_n(first + chrono::Duration::days(d as i64), stocks))
                .collect(),
            columns: fixture["columns"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, col)| {
                    (
                        name.clone(),
                        col.as_array().unwrap().iter().map(|v| v.as_f64()).collect(),
                    )
                })
                .collect(),
        };
        let registry = FactorRegistry::default();
        let panel = EvaluationPanel::new(&p);
        let expected = fixture["expected"].as_object().unwrap();
        assert_eq!(expected.len(), 191 - audit::quarantined_ids().len());
        let mut mismatches = Vec::new();
        for (id, tail) in expected {
            let actual = evaluate(&registry.get(id).unwrap().expression, &panel).unwrap();
            let tail = tail.as_array().unwrap();
            for (i, (a, b)) in actual[actual.len() - tail.len()..]
                .iter()
                .zip(tail)
                .enumerate()
            {
                let b = b.as_f64();
                if !match (a, b) {
                    (None, None) => true,
                    (Some(a), Some(b)) => (*a - b).abs() <= 1e-8 + 1e-7 * b.abs(),
                    _ => false,
                } {
                    mismatches.push(format!("{id}[{i}] rust={a:?} numpy={b:?}"));
                }
            }
        }
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    }

    #[test]
    fn original_operator_golden_values_cover_weights_zero_nulls_and_lags() {
        let mut p = panel();
        p.columns.insert(
            "x".into(),
            vec![
                Some(1.0),
                Some(2.0),
                Some(4.0),
                Some(-1.0),
                Some(0.0),
                Some(1.0),
            ],
        );
        let e = EvaluationPanel::new(&p);
        let run = |source| evaluate(&parser::parse_formula(source).unwrap(), &e).unwrap();
        assert!((run("WMA(x,3)")[2].unwrap() - (0.81 + 1.8 + 4.0) / 2.71).abs() < 1e-12);
        assert_eq!(run("SIGN(x)")[4], Some(0.0));
        assert_eq!(run("HIGHDAY(x,3)")[2], Some(0.0));
        assert_eq!(run("LOWDAY(x,3)")[2], Some(2.0));
        assert_eq!(run("x != 0")[4], Some(0.0));
        assert_eq!(run("x != 0")[5], Some(1.0));
        p.columns.get_mut("x").unwrap()[1] = None;
        let e = EvaluationPanel::new(&p);
        for f in [
            "SUM(x,3)",
            "WMA(x,3)",
            "MEAN(x,3)",
            "STD(x,3)",
            "HIGHDAY(x,3)",
        ] {
            assert_eq!(
                evaluate(&parser::parse_formula(f).unwrap(), &e).unwrap()[2],
                None,
                "{f}"
            );
        }
        assert_eq!(
            evaluate(&parser::parse_formula("x>0?1:2").unwrap(), &e).unwrap()[1],
            None
        );
        assert_eq!(
            evaluate(&parser::parse_formula("(-1)^0.5").unwrap(), &e).unwrap()[0],
            None
        );
    }
    #[test]
    fn multivariate_ols_uses_all_three_predictors_and_rejects_singular_design() {
        let mut p = Panel {
            instrument_id: vec!["A".into(); 8],
            trade_date: (1..=8)
                .map(|d| NaiveDate::from_ymd_opt(2024, 1, d).unwrap())
                .collect(),
            columns: HashMap::new(),
        };
        for (bit, name) in ["mkt", "smb", "hml"].iter().enumerate() {
            p.columns.insert(
                (*name).into(),
                (0..8)
                    .map(|i| Some(if i & (1 << bit) == 0 { -1.0 } else { 1.0 }))
                    .collect(),
            );
        }
        let y = (0..8)
            .map(|i| {
                let a = p.columns["mkt"][i].unwrap();
                let b = p.columns["smb"][i].unwrap();
                let c = p.columns["hml"][i].unwrap();
                Some(3.0 + 2.0 * a + 4.0 * b + 8.0 * c + a * b)
            })
            .collect();
        p.columns.insert("y".into(), y);
        let e = parser::parse_formula("REGRESI(y,mkt,smb,hml,8)").unwrap();
        let out = evaluate(&e, &EvaluationPanel::new(&p)).unwrap();
        assert!(out[..7].iter().all(Option::is_none));
        assert!((out[7].unwrap() - 1.0).abs() < 1e-12);
        p.columns.insert("hml".into(), p.columns["smb"].clone());
        assert_eq!(evaluate(&e, &EvaluationPanel::new(&p)).unwrap()[7], None);
    }
    #[test]
    fn version_duplicates_and_cancellation_cannot_publish_a_partial_artifact() {
        let p = panel();
        let e = InMemoryFactorExecutor {
            registry: FactorRegistry::default(),
            panel: p.clone(),
        };
        let base = FactorRunRequest {
            date_range: DateRange {
                start: p.trade_date[0],
                end: p.trade_date[2],
            },
            universe_id: "test".into(),
            factor_ids: vec!["alpha_002".into()],
            formula_version: FORMULA_VERSION.into(),
        };
        let (tx, _) = std::sync::mpsc::channel();
        let mut wrong = base.clone();
        wrong.formula_version = "old".into();
        assert!(
            e.compute(wrong, tx.clone(), Arc::new(AtomicBool::new(false)))
                .is_err()
        );
        let mut duplicate = base.clone();
        duplicate.factor_ids.push("alpha_002".into());
        assert!(
            e.compute_streaming(&duplicate, &AtomicBool::new(false), |_, _| panic!(
                "must reject before callback"
            ))
            .is_err()
        );
        assert!(
            e.compute(base, tx, Arc::new(AtomicBool::new(true)))
                .is_err()
        );
    }
    #[test]
    fn duplicate_trading_rows_are_rejected_and_nonfinite_inputs_remain_null() {
        let mut p = panel();
        p.trade_date[1] = p.trade_date[0];
        assert!(p.validate().is_err());
        let mut p = panel();
        p.columns.get_mut("close").unwrap()[0] = Some(f64::INFINITY);
        assert_eq!(
            evaluate(
                &FactorExpr::Column("close".into()),
                &EvaluationPanel::new(&p)
            )
            .unwrap()[0],
            None
        );
        assert_eq!(
            evaluate(&FactorExpr::Const(f64::INFINITY), &EvaluationPanel::new(&p)).unwrap()[0],
            None
        );
    }
    #[test]
    fn nested_lookback_is_composed_and_recursive_history_is_disclosed() {
        assert_eq!(
            parser::parse_formula("MEAN(DELAY(close,5),20)")
                .unwrap()
                .history_requirement(),
            (25, false)
        );
        assert_eq!(
            parser::parse_formula("SMA(MEAN(close,20),6,1)")
                .unwrap()
                .history_requirement(),
            (20, true)
        );
        let r = FactorRegistry::default();
        assert_eq!(r.get("alpha_026").unwrap().lookback, 235);
        assert!(r.get("alpha_009").unwrap().requires_recursive_state);
    }
    #[test]
    fn parser_never_ignores_arguments_or_invents_missing_windows() {
        for f in [
            "RANK(close,5)",
            "STD(close)",
            "MEAN(close)",
            "MAX(close)",
            "SUM(close)",
            "SMA(close,60)",
            "MEAN(close,2.5)",
            "MEAN(close,0)",
        ] {
            assert!(parser::parse_formula(f).is_err(), "{f}");
        }
    }
    #[test]
    fn filtered_beta_selects_paired_down_market_observations() {
        let mut p = panel();
        p.columns.insert(
            "x".into(),
            vec![
                Some(1.0),
                Some(50.0),
                Some(3.0),
                Some(1.0),
                Some(2.0),
                Some(3.0),
            ],
        );
        p.columns.insert(
            "y".into(),
            vec![
                Some(2.0),
                Some(-50.0),
                Some(6.0),
                Some(2.0),
                Some(4.0),
                Some(6.0),
            ],
        );
        p.columns.insert(
            "c".into(),
            vec![
                Some(1.0),
                Some(0.0),
                Some(1.0),
                Some(1.0),
                Some(1.0),
                Some(1.0),
            ],
        );
        let e = parser::parse_formula("REGBETA(FILTER(y,c),FILTER(x,c),3)").unwrap();
        assert_eq!(
            evaluate(&e, &EvaluationPanel::new(&p)).unwrap()[2],
            Some(2.0)
        );
    }

    #[test]
    fn registry_contains_all_191() {
        assert_eq!(FactorRegistry::default().len(), 191);
    }

    #[test]
    fn alpha_one_executes() {
        let p = panel();
        let r = FactorRegistry::default();
        let values = evaluate(
            &r.get("alpha_001").unwrap().expression,
            &EvaluationPanel::new(&p),
        )
        .unwrap();
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
            formula_version: FORMULA_VERSION.into(),
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
                    formula_version: FORMULA_VERSION.into(),
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
        assert_eq!(unsupported, audit::quarantined_ids());
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
        for spec in registry
            .all()
            .filter(|spec| !matches!(spec.expression, FactorExpr::Unsupported { .. }))
        {
            evaluate(&spec.expression, &EvaluationPanel::new(&fixture))
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
            &EvaluationPanel::new(&panel),
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
        let baseline = evaluate(
            &registry.get("alpha_002").unwrap().expression,
            &EvaluationPanel::new(&panel),
        )
        .unwrap();
        let mut changed = panel.clone();
        changed.columns.get_mut("close").unwrap()[2] = Some(10_000.0);
        let recomputed = evaluate(
            &registry.get("alpha_002").unwrap().expression,
            &EvaluationPanel::new(&changed),
        )
        .unwrap();
        assert_eq!(baseline[0], recomputed[0]);
        assert_eq!(baseline[1], recomputed[1]);
    }
}
