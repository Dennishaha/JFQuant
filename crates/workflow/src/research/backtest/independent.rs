//! Independent fit-only plans and frozen model selection for cross-universe execution.
use super::*;
use analytics_core::schedule::{Fold, ResolvedFold, Step, StepUnit};
use serde_json::{Value, json};

/// One fitting window; no prediction dates are required.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FitWindow {
    /// Inclusive training observations and label boundary.
    pub train: DateRange,
    /// Optional early-stopping observations and label boundary.
    pub validation: Option<DateRange>,
}
impl FitWindow {
    fn end(&self) -> Date {
        self.validation.as_ref().unwrap_or(&self.train).end
    }
    fn validate(&self) -> Result<()> {
        self.train.validate()?;
        if let Some(v) = &self.validation {
            v.validate()?;
            if self.train.end >= v.start {
                return Err("require training before validation".into());
            }
        }
        Ok(())
    }
}
/// Fit-only schedule, independent of all future backtest dates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum TrainingSchedule {
    /// Fit once.
    Single {
        /// Window.
        window: FitWindow,
    },
    /// Explicit windows; publication order resolves equal cutoff ties.
    Manual {
        /// Ordered windows.
        windows: Vec<FitWindow>,
    },
    /// Shift a window through an explicit final fitting date.
    Rolling {
        /// First window.
        first: FitWindow,
        /// Positive offset.
        step: Step,
        /// Keep initial training start.
        expanding: bool,
        /// Last allowed fitting date.
        until: Date,
    },
}
impl TrainingSchedule {
    /// Resolve fitting windows against the training snapshot calendar.
    pub fn resolve(&self, calendar: &[Date]) -> Result<Vec<FitWindow>> {
        fn shift(d: Date, step: Step, n: u32, calendar: &[Date]) -> Result<Date> {
            let count = step.count.checked_mul(n).ok_or("step overflow")?;
            let day = match step.unit {
                StepUnit::Days => d.checked_add_days(chrono::Days::new(count.into())),
                StepUnit::Months | StepUnit::Years => {
                    let months = if step.unit == StepUnit::Years {
                        count.checked_mul(12).ok_or("step overflow")?
                    } else {
                        count
                    };
                    d.checked_add_months(chrono::Months::new(months))
                        .and_then(|shifted| {
                            if d.succ_opt().is_some_and(|next| next.month() != d.month()) {
                                let next_month = shifted
                                    .with_day(1)?
                                    .checked_add_months(chrono::Months::new(1))?;
                                next_month.pred_opt()
                            } else {
                                Some(shifted)
                            }
                        })
                }
                StepUnit::TradingDays => calendar
                    .get(calendar.partition_point(|x| *x < d) + count as usize)
                    .copied(),
            };
            day.ok_or_else(|| "training step outside calendar".into())
        }
        let windows = match self {
            Self::Single { window } => vec![window.clone()],
            Self::Manual { windows } => windows.clone(),
            Self::Rolling {
                first,
                step,
                expanding,
                until,
            } => {
                if step.count == 0 {
                    return Err("training step must be positive".into());
                }
                first.validate()?;
                if calendar.last().is_none_or(|d| *until > *d) {
                    return Err("training rolling end exceeds calendar".into());
                }
                if first.end() > *until {
                    return Err("rolling until precedes first fit".into());
                }
                let mut out = Vec::new();
                for n in 0..10000 {
                    let map = |r: &DateRange| -> Result<DateRange> {
                        Ok(DateRange {
                            start: shift(r.start, *step, n, calendar)?,
                            end: shift(r.end, *step, n, calendar)?,
                        })
                    };
                    let mut w = match (
                        map(&first.train),
                        first.validation.as_ref().map(map).transpose(),
                    ) {
                        (Ok(train), Ok(validation)) => FitWindow { train, validation },
                        _ => break,
                    };
                    if *expanding {
                        w.train.start = first.train.start;
                    }
                    if w.end() > *until {
                        break;
                    }
                    out.push(w);
                }
                out
            }
        };
        if windows.is_empty() || windows.len() > 1000 {
            return Err("require 1..1000 fitting windows".into());
        }
        windows
            .into_iter()
            .map(|w| {
                w.validate()?;
                let align = |r: DateRange| -> Result<DateRange> {
                    if calendar.first().is_none_or(|d| *d > r.start)
                        || calendar.last().is_none_or(|d| *d < r.end)
                    {
                        return Err("training calendar coverage incomplete".into());
                    }
                    let lo = calendar.partition_point(|d| *d < r.start);
                    let hi = calendar.partition_point(|d| *d <= r.end);
                    if lo >= hi {
                        return Err("empty fitting interval".into());
                    }
                    Ok(DateRange {
                        start: calendar[lo],
                        end: calendar[hi - 1],
                    })
                };
                Ok(FitWindow {
                    train: align(w.train)?,
                    validation: w.validation.map(align).transpose()?,
                })
            })
            .collect()
    }
}
/// Feature and trainer settings, deliberately independent of all account parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FitConfig {
    /// Ordered feature identifiers; empty selects available builtins.
    pub factor_ids: Vec<String>,
    /// Frozen mixed feature definitions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub factor_refs: Vec<analytics_factor::selection::FactorRef>,
    /// Trainer parameters and early stopping.
    pub training: crate::research::TrainingConfig,
}
impl From<&ResearchConfig> for FitConfig {
    fn from(c: &ResearchConfig) -> Self {
        Self {
            factor_ids: c.factor_ids.clone(),
            factor_refs: c.factor_refs.clone(),
            training: c.training.clone(),
        }
    }
}
impl FitConfig {
    fn research_config(&self) -> ResearchConfig {
        ResearchConfig {
            factor_ids: self.factor_ids.clone(),
            factor_refs: self.factor_refs.clone(),
            training: self.training.clone(),
            ..ResearchConfig::default()
        }
    }
    fn validate(&self) -> Result<()> {
        self.research_config().validate()
    }
}
/// Portable fit-only configuration. Data versions are selected by the enclosing task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrainingPlan {
    /// Current version, 1.
    pub schema_version: u32,
    /// Frozen A pool recipe.
    pub pool_version: String,
    /// Benchmark input.
    pub benchmark: String,
    /// Legacy family fallback for empty references.
    pub factor_set: analytics_core::research_quality::FactorSet,
    /// Model feature/source contract.
    pub price_basis: analytics_core::research_quality::PriceBasis,
    /// Fitting sample policy.
    pub quality_policy: ResearchQualityPolicy,
    /// Ordered features and trainer settings; contains no account configuration.
    pub config: FitConfig,
    /// No test dates.
    pub schedule: TrainingSchedule,
}
impl TrainingPlan {
    /// Seed a fit-only plan from a legacy recipe, dropping its test dates.
    pub fn from_template(t: &crate::ResearchTemplate) -> Self {
        let convert = |f: &Fold| FitWindow {
            train: f.train.clone(),
            validation: f.validation.clone(),
        };
        let schedule = match &t.schedule {
            Schedule::Single { fold } => TrainingSchedule::Single {
                window: convert(fold),
            },
            Schedule::Manual { folds } => TrainingSchedule::Manual {
                windows: folds.iter().map(convert).collect(),
            },
            Schedule::Rolling {
                first,
                step,
                expanding,
                until,
            } => TrainingSchedule::Rolling {
                first: convert(first),
                step: *step,
                expanding: *expanding,
                until: until
                    .unwrap_or_else(|| first.validation.as_ref().unwrap_or(&first.train).end),
            },
        };
        Self {
            schema_version: 1,
            pool_version: t.pool_version.clone(),
            benchmark: t.benchmark.clone(),
            factor_set: t.factor_set,
            price_basis: t.price_basis,
            quality_policy: t.quality_policy.clone(),
            config: FitConfig::from(&t.config),
            schedule,
        }
    }
    /// Validate input contracts without opening native handles.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 || self.pool_version.is_empty() {
            return Err("unsupported training plan/pool".into());
        }
        analytics_core::pool::validate_instrument(&self.benchmark)?;
        self.config.validate()?;
        self.quality_policy.validate()?;
        FactorRegistry::resolve(
            self.factor_set,
            &self.config.factor_refs,
            &self.config.factor_ids,
        )?;
        Ok(())
    }
}
/// Select one model, or all models from the same frozen training task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelSelection {
    /// Latest available model each day.
    #[default]
    Rolling,
    /// Use exactly this model throughout.
    Single {
        /// Stable model identifier.
        model_id: String,
    },
}
/// Independent account and prediction request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BacktestPlan {
    /// Current independent backtest plan version, 4.
    pub schema_version: u32,
    /// One completed training task.
    pub training: uuid::Uuid,
    /// Defaults to rolling.
    #[serde(default)]
    pub models: ModelSelection,
    /// B pool recipe.
    pub pool_version: String,
    /// Independent prediction/execution dates.
    pub range: DateRange,
    /// Independent market snapshot.
    pub market: String,
    /// Account event snapshot.
    pub actions: String,
    /// Benchmark for inputs; must match training's feature contract.
    pub benchmark: String,
    /// B input policy; missing-value semantics must match the model contract.
    pub quality_policy: ResearchQualityPolicy,
    /// Account parameters only.
    pub execution: ResearchExecution,
    /// Account event trust.
    #[serde(default)]
    pub event_check_policy: analytics_core::execution::EventCheckPolicy,
}
impl BacktestPlan {
    /// Structural validation.
    pub fn validate(&self) -> Result<()> {
        self.range.validate()?;
        self.quality_policy.validate()?;
        if self.schema_version != 4
            || self.pool_version.is_empty()
            || self.market.is_empty()
            || self.actions.is_empty()
            || self.execution.top_n == 0
            || self.execution.rebalance_days == 0
        {
            return Err("invalid independent backtest plan".into());
        }
        analytics_core::pool::validate_instrument(&self.benchmark)?;
        self.execution.market_rules.validate()?;
        self.execution
            .allocation
            .validate_count(self.execution.top_n)?;
        if !self.execution.initial_cash_cny.is_finite() || self.execution.initial_cash_cny <= 0. {
            return Err("invalid initial cash".into());
        }
        Ok(())
    }
}
/// Immutable model receipt; fitting securities come from retained rows, not the declared pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelReceipt {
    /// Stable content-derived ID.
    pub id: String,
    /// Original publication order.
    pub index: usize,
    /// Training bounds.
    pub window: FitWindow,
    /// Last feature/label date actually used for fit/early stopping.
    pub fit_data_end: Date,
    /// Next exchange date, if present in the training calendar.
    pub available_from: Option<Date>,
    /// Actual retained fitting identifiers.
    pub instruments: BTreeSet<String>,
    /// Rows used for training and validation.
    pub sample_counts: [usize; 2],
    /// Model file receipt.
    pub model_sha256: String,
    /// Feature manifest receipt.
    pub manifest_sha256: String,
}
/// Compressed inclusive prediction segment for one frozen model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSegment {
    /// Prediction dates.
    pub range: DateRange,
    /// Catalogue index.
    pub model_index: usize,
    /// Stable ID.
    pub model_id: String,
}
/// Verified portable training catalogue; contains no native handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogue {
    /// Schema version, 1.
    pub schema_version: u32,
    /// Frozen fit configuration.
    pub plan: TrainingPlan,
    /// Ordered model receipts.
    pub models: Vec<ModelReceipt>,
    /// Stable identity of all fitting dependencies.
    pub training_signature: String,
    /// Actual completion time; independent of simulated availability.
    pub completed_at: String,
    /// Original identity map for compatibility checks.
    pub source_map: Value,
    /// Full A pool provenance.
    pub pool: Value,
    /// Self receipt (computed without this field).
    pub signature: String,
}
impl ModelCatalogue {
    /// Deterministic, time-safe selection. Never truncate the requested interval.
    pub fn segments(
        &self,
        selection: &ModelSelection,
        range: &DateRange,
        calendar: &[Date],
    ) -> Result<Vec<ModelSegment>> {
        range.validate()?;
        if calendar.first().is_none_or(|d| *d > range.start)
            || calendar.last().is_none_or(|d| *d < range.end)
        {
            return Err("backtest calendar coverage incomplete".into());
        }
        if let ModelSelection::Single { model_id } = selection
            && !self.models.iter().any(|m| m.id == *model_id)
        {
            return Err("unknown selected model".into());
        }
        let mut segments: Vec<ModelSegment> = Vec::new();
        for &date in calendar
            .iter()
            .filter(|d| **d >= range.start && **d <= range.end)
        {
            let model=self.models.iter().filter(|m| m.fit_data_end<date && m.available_from.is_none_or(|d| date>=d) && match selection {ModelSelection::Rolling=>true,ModelSelection::Single {model_id}=>m.id==*model_id}).max_by_key(|m|(m.fit_data_end,m.index)).ok_or_else(||format!("no available model on {date}; fitting/validation must finish before prediction"))?;
            if let Some(last) = segments.last_mut()
                && last.model_id == model.id
            {
                last.range.end = date;
            } else {
                segments.push(ModelSegment {
                    range: DateRange {
                        start: date,
                        end: date,
                    },
                    model_index: model.index,
                    model_id: model.id.clone(),
                });
            }
        }
        if segments.is_empty() {
            return Err("no backtest trading dates".into());
        }
        Ok(segments)
    }
}

/// Internal execution adapter: legacy account code receives resolved windows, never A data.
pub(super) struct Execution {
    pub folds: Vec<ResolvedFold>,
    pub catalogue: Option<ModelCatalogue>,
    pub segments: Vec<ModelSegment>,
    pub training_plan: Option<TrainingPlan>,
}
impl Execution {
    pub fn predicting(&self) -> bool {
        self.catalogue.is_some()
    }
}

fn sign_catalogue(c: &ModelCatalogue) -> Result<String> {
    let mut v = serde_json::to_value(c)?;
    v.as_object_mut().unwrap().remove("signature");
    crate::signature(&v)
}

/// Load current fit-only catalogues (5) or signed pipeline descriptors (4).
/// Both are current producer formats; descriptor adaptation never upgrades old versions.
pub fn catalogue(source: &Path) -> Result<ModelCatalogue> {
    crate::storage_management::require_path_available(source, "models")?;
    let v: Value = serde_json::from_slice(&fs::read(source.join("training.json"))?)?;
    let c: ModelCatalogue = if v["schema_version"] == 5 {
        if v["status"] != "trained" {
            return Err("incomplete training artifact".into());
        }
        let c: ModelCatalogue = serde_json::from_value(v["catalogue"].clone())?;
        if c.signature != sign_catalogue(&c)? {
            return Err("training catalogue damaged".into());
        }
        c
    } else {
        validate_training_artifact(&v)?;
        if v["fit_calendar_sha256"] != file_hash(&source.join("fit_calendar.json"))? {
            return Err(
                "legacy training lacks signed fitting calendar evidence; use original replay entry"
                    .into(),
            );
        }
        let cache_receipts: BTreeMap<String, String> =
            serde_json::from_value(v["fit_cache_receipts"].clone())
                .map_err(|_| "legacy training lacks signed sample cache evidence")?;
        if cache_receipts.is_empty() {
            return Err("legacy fitting cache receipts missing".into());
        }
        for (name, hash) in cache_receipts {
            if !name.starts_with("factor-cache/")
                || Path::new(&name)
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
                || file_hash(&source.join(name))? != hash
            {
                return Err("legacy fitting cache damaged".into());
            }
        }
        for selected in v["sample_selection"]["selections"]
            .as_array()
            .ok_or("legacy sample receipts missing")?
            .iter()
            .filter(|r| r["split"] != "prediction")
        {
            let name = selected["path"]
                .as_str()
                .ok_or("legacy sample path missing")?;
            if !name.starts_with("sample_masks/")
                || Path::new(name)
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err("unsafe fitting sample path".into());
            }
            let mask: Value = serde_json::from_slice(&fs::read(source.join(name))?)?;
            if crate::signature(&mask)? != selected["sha256"] {
                return Err("legacy sample mask damaged".into());
            }
        }
        let request: ResearchRequest = serde_json::from_value(v["request"].clone())?;
        let folds: Vec<ResolvedFold> = serde_json::from_value(v["resolved_folds"].clone())?;
        let plan = TrainingPlan {
            schema_version: 1,
            pool_version: request.universe_id.clone(),
            benchmark: request.benchmark.clone(),
            factor_set: request.factor_set,
            price_basis: request.price_basis,
            quality_policy: request.quality_policy.clone(),
            config: FitConfig::from(&request.config),
            schedule: TrainingSchedule::Manual {
                windows: folds
                    .iter()
                    .map(|f| FitWindow {
                        train: f.actual.train.clone(),
                        validation: f.actual.validation.clone(),
                    })
                    .collect(),
            },
        };
        let mut models = Vec::new();
        for (i, f) in folds.iter().enumerate() {
            let mut m = receipt(
                source,
                i,
                &FitWindow {
                    train: f.actual.train.clone(),
                    validation: f.actual.validation.clone(),
                },
                &[],
            )?;
            if v["models"][i]["model_sha256"] != m.model_sha256
                || v["models"][i]["manifest_sha256"] != m.manifest_sha256
            {
                return Err("legacy training model damaged".into());
            }
            m.available_from = None;
            models.push(m);
        }
        let mut c = ModelCatalogue {
            schema_version: 1,
            plan,
            models,
            training_signature: v["training_signature"]
                .as_str()
                .ok_or("legacy training signature missing")?
                .into(),
            completed_at: "legacy_completion_not_recorded".into(),
            source_map: v["source_map"].clone(),
            pool: request
                .pool
                .as_ref()
                .map(serde_json::to_value)
                .transpose()?
                .unwrap_or(Value::Null),
            signature: String::new(),
        };
        c.signature = sign_catalogue(&c)?;
        c
    };
    c.plan.validate()?;
    if c.schema_version != 1 || c.models.is_empty() {
        return Err("invalid model catalogue".into());
    }
    for (i, m) in c.models.iter().enumerate() {
        if m.index != i
            || m.instruments.is_empty()
            || m.fit_data_end > m.window.end()
            || m.fit_data_end < m.window.train.start
            || m.available_from.is_some_and(|d| d <= m.fit_data_end)
        {
            return Err("invalid fitting provenance".into());
        }
        let dir = source.join(format!("fold-{i:04}"));
        if file_hash(&dir.join("model.txt"))? != m.model_sha256
            || file_hash(&dir.join("manifest.json"))? != m.manifest_sha256
        {
            return Err("model receipt mismatch".into());
        }
        let manifest = crate::research::load_manifest(dir.join("manifest.json"))?;
        if manifest
            .features
            .iter()
            .map(|f| f.name.clone())
            .collect::<Vec<_>>()
            != c.plan.config.factor_ids
        {
            return Err("model feature order differs from catalogue".into());
        }
        let raw: Value = serde_json::from_slice(&fs::read(dir.join("manifest.json"))?)?;
        let refs = analytics_factor::selection::frozen_refs(
            c.plan.factor_set,
            &c.plan.config.factor_refs,
            &c.plan.config.factor_ids,
        );
        if raw["factor_refs"] != serde_json::to_value(refs)?
            || raw["research_execution"]["price_basis"] != serde_json::to_value(c.plan.price_basis)?
        {
            return Err("model feature/source contract differs from catalogue".into());
        }
    }
    Ok(c)
}

fn receipt(
    source: &Path,
    index: usize,
    window: &FitWindow,
    calendar: &[Date],
) -> Result<ModelReceipt> {
    let dates: Vec<Date> = if calendar.is_empty() {
        serde_json::from_slice(&fs::read(source.join("fit_calendar.json"))?)?
    } else {
        calendar.to_vec()
    };
    let cache = source.join("factor-cache");
    let ids: Vec<String> = serde_json::from_slice::<Value>(&fs::read(
        source.join(format!("fold-{index:04}/manifest.json")),
    )?)?["features"]
        .as_array()
        .ok_or("feature manifest missing")?
        .iter()
        .map(|f| {
            f["name"]
                .as_str()
                .map(str::to_owned)
                .ok_or("feature name missing")
        })
        .collect::<std::result::Result<_, _>>()?;
    let mut instruments = BTreeSet::new();
    let mut counts = [0, 0];
    let mut fit_end = None;
    for (part, range) in std::iter::once(&window.train)
        .chain(window.validation.iter())
        .enumerate()
    {
        let mask: Value = serde_json::from_slice(&fs::read(source.join(format!(
            "sample_masks/fold-{index:04}-{}.json",
            if part == 0 { "train" } else { "validation" }
        )))?)?;
        let selection: Vec<bool> = serde_json::from_value(mask["row_mask"].clone())?;
        let mut selected = selection.iter();
        for year in range.start.year()..=range.end.year() {
            let data = data::load_year(&cache, year, &ids)?;
            for row in data.meta.rows {
                // Legacy caches store calendar indices; require the frozen calendar receipt.
                let date = *dates.get(row.day).ok_or("fitting calendar index invalid")?;
                if date < range.start || date > range.end {
                    continue;
                }
                let keep = *selected.next().ok_or("sample mask truncated")?;
                if keep {
                    let label_end = row
                        .label_end
                        .ok_or("retained fitting row lacks label boundary")?;
                    if label_end > range.end || row.label.is_none() {
                        return Err("fitting label crosses window".into());
                    }
                    instruments.insert(row.instrument);
                    counts[part] += 1;
                    fit_end = Some(
                        fit_end.map_or(date.max(label_end), |d: Date| d.max(date).max(label_end)),
                    );
                }
            }
        }
        if selected.next().is_some() {
            return Err("sample mask row count mismatch".into());
        }
    }
    let fit_data_end = fit_end.ok_or("no retained fit samples")?;
    let model_sha256 = file_hash(&source.join(format!("fold-{index:04}/model.txt")))?;
    let manifest_sha256 = file_hash(&source.join(format!("fold-{index:04}/manifest.json")))?;
    Ok(ModelReceipt {
        id: crate::signature(&(index, &model_sha256, &manifest_sha256))?,
        index,
        window: window.clone(),
        fit_data_end,
        available_from: calendar.iter().find(|d| **d > fit_data_end).copied(),
        instruments,
        sample_counts: counts,
        model_sha256,
        manifest_sha256,
    })
}

pub(super) fn publish_training(
    request: &ResearchRequest,
    plan: &TrainingPlan,
    dataset: &DailyDataset,
    folds: &[ResolvedFold],
    legacy: &Value,
) -> Result<()> {
    save(request.output.join("fit_calendar.json"), &dataset.calendar)?;
    let mut plan = plan.clone();
    plan.config.factor_ids = serde_json::from_value(legacy["feature_order"].clone())?;
    plan.config.factor_refs = analytics_factor::selection::frozen_refs(
        plan.factor_set,
        &plan.config.factor_refs,
        &plan.config.factor_ids,
    );
    let mut models = Vec::new();
    for (i, f) in folds.iter().enumerate() {
        let w = FitWindow {
            train: f.actual.train.clone(),
            validation: f.actual.validation.clone(),
        };
        let mut m = receipt(&request.output, i, &w, &dataset.calendar)?;
        let path = request.output.join(format!("fold-{i:04}/manifest.json"));
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path)?)?;
        manifest.as_object_mut().unwrap().remove("rolling_fold");
        manifest["fitting"] = json!({"window":m.window,"fit_data_end":m.fit_data_end,"available_from":m.available_from,"instruments":m.instruments,"sample_counts":m.sample_counts});
        save(&path, &manifest)?;
        m.manifest_sha256 = file_hash(&path)?;
        m.id = crate::signature(&(i, &m.model_sha256, &m.manifest_sha256))?;
        models.push(m);
    }
    let mut c = ModelCatalogue {
        schema_version: 1,
        plan,
        models,
        training_signature: legacy["training_signature"]
            .as_str()
            .ok_or("missing training signature")?
            .into(),
        completed_at: chrono::Utc::now().to_rfc3339(),
        source_map: serde_json::to_value(dataset.source_map())?,
        pool: serde_json::to_value(&request.pool)?,
        signature: String::new(),
    };
    c.signature = sign_catalogue(&c)?;
    save(
        request.output.join("training.json"),
        &json!({"schema_version":5,"status":"trained","catalogue":c,"training_signature":c.training_signature,"feature_order":c.plan.config.factor_ids,"models":c.models,"completed_at":c.completed_at}),
    )?;
    Ok(())
}
fn fit_folds(windows: &[FitWindow], calendar: &[Date], warmup: usize) -> Vec<ResolvedFold> {
    windows
        .iter()
        .map(|w| {
            // Private adapter only: fit-only public artifacts never expose a test interval.
            // Do not bind the fitting signature to whichever future sessions happen to
            // exist in this snapshot. Real availability is recorded separately.
            let next = w.end().succ_opt().expect("validated market date");
            let fold = Fold {
                train: w.train.clone(),
                validation: w.validation.clone(),
                test: DateRange {
                    start: next,
                    end: next,
                },
            };
            let start = calendar.partition_point(|d| *d < w.train.start);
            let lo = start.saturating_sub(warmup);
            ResolvedFold {
                requested: fold.clone(),
                actual: fold,
                warmup_start: calendar[lo],
                warmup_sessions: start - lo,
            }
        })
        .collect()
}
/// Fit models without test dates. All native work stays on the calling worker.
pub fn train(
    request: &ResearchRequest,
    plan: &TrainingPlan,
    context: &analytics_core::task::TaskContext,
) -> Result<()> {
    plan.validate()?;
    let dataset = DailyDataset::open(&request.data_root)?;
    let windows = plan.schedule.resolve(&dataset.calendar)?;
    let mut r = request.clone();
    r.train_only = true;
    r.reuse_training = None;
    r.reuse_predictions = None;
    r.factor_set = plan.factor_set;
    r.config = plan.config.research_config();
    r.price_basis = plan.price_basis;
    r.quality_policy = plan.quality_policy.clone();
    r.benchmark = plan.benchmark.clone();
    let mode = Execution {
        folds: fit_folds(&windows, &dataset.calendar, r.warmup_sessions()),
        catalogue: None,
        segments: vec![],
        training_plan: Some(plan.clone()),
    };
    let result = context
        .execute(|| crate::research::worker::scoped(|| stages::run(&r, Some(&mode), context)));
    if r.output.is_dir() {
        let stages = json!({"schema_version":2,"training":if r.output.join("training.json").is_file(){"completed"}else if context.cancel.load(std::sync::atomic::Ordering::Relaxed){"cancelled"}else{"incomplete"},"prediction":"not_started","backtest":"not_started"});
        crate::atomic_json(&r.output.join("stages.json"), &stages)?;
    }
    result
}
/// Build prediction segments and reject incompatible feature/source contracts before execution.
pub fn preview(source: &Path, plan: &BacktestPlan, data_root: &Path) -> Result<Value> {
    preview_for_pool(source, plan, data_root, None)
}
fn preview_for_pool(
    source: &Path,
    plan: &BacktestPlan,
    data_root: &Path,
    pool: Option<&analytics_core::pool::PoolMembership>,
) -> Result<Value> {
    plan.validate()?;
    let c = catalogue(source)?;
    let dataset = DailyDataset::open(data_root)?;
    if plan.benchmark != c.plan.benchmark {
        return Err("backtest feature/missing-value contract differs from training".into());
    }
    if c.plan.price_basis == analytics_core::research_quality::PriceBasis::Provider
        && !dataset.supports_provider_training()
    {
        return Err("model requires supplier-compatible prices".into());
    }
    let old: analytics_core::source_map::SourceMap = serde_json::from_value(c.source_map.clone())?;
    let new: analytics_core::source_map::SourceMap =
        serde_json::from_value(serde_json::to_value(dataset.source_map())?)?;
    // Compare only B identities. A-only source bindings are never new B dependencies.
    let relevant = pool
        .map(|p| p.intervals.keys().cloned().collect::<BTreeSet<_>>())
        .unwrap_or_else(|| {
            c.models
                .iter()
                .flat_map(|m| m.instruments.iter().cloned())
                .chain(new.exceptions.keys().cloned())
                .collect()
        });
    for id in relevant
        .into_iter()
        .chain(std::iter::once(plan.benchmark.clone()))
    {
        match (old.exceptions.get(&id), new.exceptions.get(&id)) {
            (Some(a), Some(b)) if a.exchange_symbol != b.exchange_symbol || a.kind != b.kind => {
                return Err(format!("security identity contract changed: {id}").into());
            }
            (Some(_), None) => {
                return Err(format!("security lost its explicit identity binding: {id}").into());
            }
            (None, Some(_)) if c.models.iter().any(|m| m.instruments.contains(&id)) => {
                return Err(
                    format!("shared security changed from direct-code to alias: {id}").into(),
                );
            }
            _ => {}
        }
    }
    let mut effective_range = plan.range.clone();
    effective_range.end = effective_range.end.min(
        dataset
            .last_valid_close_date(&plan.benchmark)?
            .ok_or("benchmark has no valid close")?,
    );
    effective_range
        .validate()
        .map_err(|_| "no backtest trading dates after tail adjustment")?;
    let segments = c.segments(&plan.models, &effective_range, &dataset.calendar)?;
    Ok(
        json!({"schema_version":1,"catalogue":c,"segments":segments,"range":plan.range,"effective_range":effective_range,"training_data_required":false}),
    )
}
/// Run a separately selected B universe using frozen models; never calls a trainer.
pub fn backtest(
    request: &ResearchRequest,
    source: &Path,
    plan: &BacktestPlan,
    frozen: &Value,
    context: &analytics_core::task::TaskContext,
) -> Result<()> {
    let current = preview_for_pool(source, plan, &request.data_root, request.pool.as_ref())?;
    if current != *frozen {
        return Err("model selection changed after submission".into());
    }
    let c: ModelCatalogue = serde_json::from_value(frozen["catalogue"].clone())?;
    let segments: Vec<ModelSegment> = serde_json::from_value(frozen["segments"].clone())?;
    let dataset = DailyDataset::open(&request.data_root)?;
    let mut r = request.clone();
    r.train_only = false;
    r.reuse_training = Some(source.to_owned());
    r.reuse_predictions = None;
    r.factor_set = c.plan.factor_set;
    r.config = c.plan.config.research_config();
    r.config.allocation = plan.execution.allocation.clone();
    r.config.top_n = plan.execution.top_n;
    r.config.rebalance_days = plan.execution.rebalance_days;
    r.price_basis = c.plan.price_basis;
    r.quality_policy = plan.quality_policy.clone();
    r.benchmark = plan.benchmark.clone();
    r.market_rules = plan.execution.market_rules.clone();
    r.initial_cash_cny = plan.execution.initial_cash_cny;
    r.pool_exit_policy = plan.execution.pool_exit_policy;
    r.corporate_event_policy = Some(plan.execution.corporate_event_policy);
    r.event_check_policy = plan.event_check_policy;
    let mut folds = segments
        .iter()
        .map(|s| {
            let m = &c.models[s.model_index];
            let f = Fold {
                train: m.window.train.clone(),
                validation: m.window.validation.clone(),
                test: s.range.clone(),
            };
            // The account's display window must respect the fit-only time cutoff, even when
            // the declared fit window ends after its last retained fitting observation.
            let mut actual = f.clone();
            actual.train.end = actual.train.end.min(m.fit_data_end);
            actual.validation = actual.validation.map(|mut v| {
                v.end = v.end.min(m.fit_data_end);
                v
            });
            let lo = dataset
                .calendar
                .partition_point(|d| *d < s.range.start)
                .saturating_sub(r.warmup_sessions());
            ResolvedFold {
                requested: actual.clone(),
                actual,
                warmup_start: dataset.calendar[lo],
                warmup_sessions: dataset.calendar.partition_point(|d| *d < s.range.start) - lo,
            }
        })
        .collect::<Vec<_>>();
    if let Some(last) = folds.last_mut() {
        last.requested.test.end = plan.range.end;
    }
    r.schedule = Schedule::Manual {
        folds: folds.iter().map(|f| f.actual.clone()).collect(),
    };
    fs::create_dir_all(&r.output)?;
    save(r.output.join("independent_request.json"), &r)?;
    let mode = Execution {
        folds,
        catalogue: Some(c),
        segments,
        training_plan: None,
    };
    fs::create_dir_all(&r.output)?;
    save(r.output.join("backtest_plan.json"), plan)?;
    save(r.output.join("model_selection.json"), frozen)?;
    let result = context
        .execute(|| crate::research::worker::scoped(|| stages::run(&r, Some(&mode), context)));
    save(
        r.output.join("stages.json"),
        &json!({"schema_version":2,"training":"reused","prediction":if r.output.join("prediction_manifest.json").exists(){"completed"}else{"incomplete"},"backtest":if result.is_ok(){"completed"}else if context.cancel.load(std::sync::atomic::Ordering::Relaxed){"cancelled"}else{"failed"}}),
    )?;
    result
}

/// Shared worker operations exposed through App and CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Job {
    /// Fit only, without test dates.
    Train {
        /// Fit plan.
        plan: Box<TrainingPlan>,
        /// A market version.
        market: String,
        /// Optional fitting action source.
        actions: Option<String>,
    },
    /// Inspect fitting coverage; optionally acquire missing company-action evidence.
    TrainingInputs {
        /// Fit plan.
        plan: Box<TrainingPlan>,
        /// A market version.
        market: String,
        /// Fitting action source.
        actions: Option<String>,
        /// Execute acquisition instead of preview.
        prepare: bool,
        /// Collect announcements.
        announcements: bool,
    },
    /// List verified saved models, without opening A data.
    Models {
        /// Completed training task.
        training: uuid::Uuid,
    },
    /// Independently predict and trade B.
    Backtest {
        /// Backtest definition.
        plan: Box<BacktestPlan>,
    },
    /// Preview B requirements/model switches, or acquire action evidence.
    BacktestInputs {
        /// Backtest definition.
        plan: Box<BacktestPlan>,
        /// Execute acquisition.
        prepare: bool,
        /// Collect announcements.
        announcements: bool,
    },
}
fn source_root(w: &crate::Workspace, id: uuid::Uuid) -> Result<PathBuf> {
    let r = w
        .runs()?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or("training task not found")?;
    Ok(w.resolve(r.output))
}
/// Freeze both pool membership and model sequence when the service accepts a task.
pub fn snapshot(w: &crate::Workspace, job: &Job) -> Result<Value> {
    let mut inputs = Vec::new();
    let (pool, selection) = match job {
        Job::Train {
            plan,
            market,
            actions,
        }
        | Job::TrainingInputs {
            plan,
            market,
            actions,
            ..
        } => {
            plan.validate()?;
            inputs.push(crate::version_metadata(w, market, "market")?);
            if let Some(a) = actions {
                inputs.push(crate::version_metadata(w, a, "actions")?);
            }
            let (_, p) = w.resolve_pool(&plan.pool_version, market)?;
            let m = crate::version(w, market, "market")?;
            let ds = DailyDataset::open(w.resolve(m.path))?;
            let ranges: Vec<_> = plan
                .schedule
                .resolve(&ds.calendar)?
                .into_iter()
                .flat_map(|f| std::iter::once(f.train).chain(f.validation))
                .collect();
            crate::membership_history::verify_task_use(w, &p, market, &ranges)?;
            (p, Value::Null)
        }
        Job::Backtest { plan } | Job::BacktestInputs { plan, .. } => {
            plan.validate()?;
            let m = crate::version_metadata(w, &plan.market, "market")?;
            let (resolved, p) = w.resolve_pool(&plan.pool_version, &plan.market)?;
            let selected = preview_for_pool(
                &source_root(w, plan.training)?,
                plan,
                &w.resolve(&m.path),
                Some(&resolved),
            )?;
            let effective: DateRange = serde_json::from_value(selected["effective_range"].clone())?;
            crate::membership_history::verify_task_use(
                w,
                &p,
                &plan.market,
                std::slice::from_ref(&effective),
            )?;
            inputs.push(m);
            inputs.push(crate::version_metadata(w, &plan.actions, "actions")?);
            (p, selected)
        }
        Job::Models { training } => (
            Value::Null,
            serde_json::to_value(catalogue(&source_root(w, *training)?)?)?,
        ),
    };
    Ok(json!({"inputs":inputs,"pool":pool,"selection":selection}))
}
fn membership(v: &Value) -> Result<analytics_core::pool::PoolMembership> {
    // Both builtin and custom frozen pool structures expose resolved membership here.
    Ok(serde_json::from_value(v["membership"].clone())?)
}
#[allow(clippy::too_many_arguments)]
fn requirements(
    w: &crate::Workspace,
    market: &str,
    pool: &Value,
    range: DateRange,
    warmup: usize,
    benchmark: &str,
    quality_policy: &ResearchQualityPolicy,
    registry: &FactorRegistry,
    ids: &[String],
) -> Result<Value> {
    let m = crate::version(w, market, "market")?;
    let mut dataset = DailyDataset::open(w.resolve(m.path))?;
    let start = dataset
        .calendar
        .partition_point(|d| *d < range.start)
        .saturating_sub(warmup);
    let lo = *dataset.calendar.get(start).ok_or("empty calendar")?;
    dataset.restrict_reads(DateRange {
        start: lo,
        end: range.end,
    })?;
    let mut p = membership(pool)?;
    p.intervals
        .retain(|_, rs| rs.iter().any(|r| r.end >= lo && r.start <= range.end));
    let mut symbols: BTreeSet<_> = p.intervals.keys().cloned().collect();
    if !dataset.is_price_index(benchmark)? {
        symbols.insert(benchmark.to_owned());
    }
    let dependencies=symbols.iter().map(|id|(id.clone(),json!({"required_history":{"start":lo,"end":range.end},"membership":p.intervals.get(id)}))).collect::<BTreeMap<_,_>>();
    let quality = quality::qualify_with_registry(
        &dataset,
        &p,
        quality_policy,
        &DateRange {
            start: lo,
            end: range.end,
        },
        ids,
        registry,
    )?;
    Ok(
        json!({"schema_version":4,"securities":symbols,"dependencies":dependencies,"history_start":lo,"execution_end":range.end,"pool":pool,"source_map":dataset.source_map(),"benchmark":benchmark,"quality":quality,"training_data_required":false}),
    )
}
/// Execute the same frozen operation in the service, App test adapter and CLI.
pub fn execute(
    w: &crate::Workspace,
    run: &crate::RunRecord,
    job: &Job,
    context: &analytics_core::task::TaskContext,
) -> Result<Value> {
    let frozen = &run.request["independent"];
    if !matches!(job, Job::Models { .. })
        && !crate::membership_history::history_inputs(&frozen["pool"])?.is_empty()
        && snapshot(w, job)? != *frozen
    {
        return Err("independent frozen inputs changed since submission".into());
    }
    let out = w.resolve(&run.output);
    fs::create_dir_all(&out)?;
    match job {
        Job::Models { training } => {
            let c = catalogue(&source_root(w, *training)?)?;
            if serde_json::to_value(&c)? != frozen["selection"] {
                return Err("models changed after submission".into());
            }
            Ok(json!({"training":training,"catalogue":c}))
        }
        Job::Train {
            plan,
            market,
            actions,
        }
        | Job::TrainingInputs {
            plan,
            market,
            actions,
            ..
        } => {
            let m = crate::version(w, market, "market")?;
            let a = actions
                .as_ref()
                .map(|id| crate::version(w, id, "actions"))
                .transpose()?;
            let dataset = DailyDataset::open(w.resolve(&m.path))?;
            let windows = plan.schedule.resolve(&dataset.calendar)?;
            if let Job::TrainingInputs {
                prepare,
                announcements,
                ..
            } = job
                && *prepare
            {
                let registry = FactorRegistry::resolve(
                    plan.factor_set,
                    &plan.config.factor_refs,
                    &plan.config.factor_ids,
                )?;
                let warmup = analytics_factor::selection::warmup_sessions(
                    plan.factor_set,
                    &plan.config.factor_refs,
                );
                let range = DateRange {
                    start: windows.iter().map(|f| f.train.start).min().unwrap(),
                    end: windows.iter().map(FitWindow::end).max().unwrap(),
                };
                let mut req = requirements(
                    w,
                    market,
                    &frozen["pool"],
                    range,
                    warmup,
                    &plan.benchmark,
                    &plan.quality_policy,
                    &registry,
                    &plan.config.factor_ids,
                )?;
                req["windows"] = serde_json::to_value(windows)?;
                req["training_data_required"] = true.into();
                if *prepare {
                    req = crate::preparation::prepare(w, run, &req, *announcements, None, context)?;
                }
                save(out.join("requirements.json"), &req)?;
                return Ok(req);
            }
            let mut r = ResearchRequest::new(w.resolve(m.path), out.clone());
            r.pool = Some(membership(&frozen["pool"])?);
            r.pool_signature = crate::signature(&frozen["pool"])?;
            r.universe_id = plan.pool_version.clone();
            r.corporate_actions = a.map(|v| w.resolve(v.path));
            r.audit_only = matches!(job, Job::TrainingInputs { .. });
            train(&r, plan, context)?;
            if r.audit_only {
                let mut report: Value =
                    serde_json::from_slice(&fs::read(out.join("input-check.json"))?)?;
                report["training_data_required"] = true.into();
                report["windows"] = serde_json::to_value(windows)?;
                return Ok(report);
            }

            Ok(serde_json::from_slice(&fs::read(
                out.join("training.json"),
            )?)?)
        }
        Job::Backtest { plan } | Job::BacktestInputs { plan, .. } => {
            let source = source_root(w, plan.training)?;
            let c: ModelCatalogue =
                serde_json::from_value(frozen["selection"]["catalogue"].clone())?;
            let m = crate::version(w, &plan.market, "market")?;
            let a = crate::version(w, &plan.actions, "actions")?;
            if let Job::BacktestInputs {
                prepare,
                announcements,
                ..
            } = job
                && *prepare
            {
                let registry = FactorRegistry::resolve(
                    c.plan.factor_set,
                    &c.plan.config.factor_refs,
                    &c.plan.config.factor_ids,
                )?;
                let mut req = requirements(
                    w,
                    &plan.market,
                    &frozen["pool"],
                    plan.range.clone(),
                    analytics_factor::selection::warmup_sessions(
                        c.plan.factor_set,
                        &c.plan.config.factor_refs,
                    ),
                    &plan.benchmark,
                    &plan.quality_policy,
                    &registry,
                    &c.plan.config.factor_ids,
                )?;
                req["model_selection"] = frozen["selection"].clone();
                if *prepare {
                    req = crate::preparation::prepare(w, run, &req, *announcements, None, context)?;
                }
                save(out.join("requirements.json"), &req)?;
                return Ok(req);
            }
            let mut r = ResearchRequest::new(w.resolve(m.path), out.clone());
            r.pool = Some(membership(&frozen["pool"])?);
            r.pool_signature = crate::signature(&frozen["pool"])?;
            r.universe_id = plan.pool_version.clone();
            r.corporate_actions = Some(w.resolve(a.path));
            r.audit_only = matches!(job, Job::BacktestInputs { .. });
            backtest(&r, &source, plan, &frozen["selection"], context)?;
            if r.audit_only {
                let mut report: Value =
                    serde_json::from_slice(&fs::read(out.join("input-check.json"))?)?;
                report["training_data_required"] = false.into();
                report["model_selection"] = frozen["selection"].clone();
                return Ok(report);
            }

            Ok(serde_json::from_slice(&fs::read(
                out.join("summary.json"),
            )?)?)
        }
    }
}

pub(super) fn record_predictions(
    request: &ResearchRequest,
    mode: &Execution,
    segment: usize,
    calendar: &[Date],
    rows: &[data::Sample],
    scores: &[f64],
) -> Result<()> {
    let path = request.output.join("independent_predictions.json");
    let mut records: Vec<Value> = if path.exists() {
        serde_json::from_slice(&fs::read(&path)?)?
    } else {
        vec![]
    };
    let c = mode.catalogue.as_ref().ok_or("missing model catalogue")?;
    let m = &c.models[mode.segments[segment].model_index];
    for (row, score) in rows.iter().zip(scores) {
        records.push(json!({"date":calendar[row.day],"instrument":row.instrument,"score":score,"label":row.label,"seen":m.instruments.contains(&row.instrument),"model_id":m.id}));
    }
    save(path, &records)
}
/// Daily model-relative seen/unseen attribution within the same continuous account.
pub(super) fn scope_report(output: &Path, execution: &Value) -> Result<Value> {
    if execution["contract"] != "independent-v1" {
        return Ok(Value::Null);
    }
    let c: ModelCatalogue = serde_json::from_value(execution["catalogue"].clone())?;
    let segments: Vec<ModelSegment> = serde_json::from_value(execution["segments"].clone())?;
    let predictions: Vec<Value> =
        serde_json::from_slice(&fs::read(output.join("independent_predictions.json"))?)?;
    let mut predictions_by_date = BTreeMap::<Date, Vec<&Value>>::new();
    for prediction in &predictions {
        let date: Date = serde_json::from_value(prediction["date"].clone())?;
        predictions_by_date
            .entry(date)
            .or_default()
            .push(prediction);
    }
    let snapshots: BTreeMap<Date, Value> =
        serde_json::from_slice(&fs::read(output.join("portfolio_snapshots.json"))?)?;
    let mut trades: BTreeMap<Date, Vec<Value>> = BTreeMap::new();
    for line in fs::read_to_string(output.join("order_events.jsonl"))?.lines() {
        let v: Value = serde_json::from_str(line)?;
        let d: Date = serde_json::from_value(v["date"].clone())?;
        trades.entry(d).or_default().push(v);
    }
    let actions: Value = serde_json::from_slice(&fs::read(output.join("execution_actions.json"))?)?;
    let account: Value = serde_json::from_slice(&fs::read(output.join("execution.json"))?)?;
    let pool: analytics_core::pool::PoolMembership =
        serde_json::from_value(account["pool"].clone())?;
    let corporate: Option<analytics_core::rights_account::Ledger> =
        if output.join("rights_state.json").is_file() {
            Some(serde_json::from_slice(&fs::read(
                output.join("rights_state.json"),
            )?)?)
        } else {
            None
        };
    let number = |v: &Value| -> Result<f64> {
        v.as_f64()
            .or_else(|| v.as_str()?.parse().ok())
            .filter(|v| v.is_finite())
            .ok_or_else(|| "corporate_attribution_number_invalid".into())
    };
    let mut previous = BTreeMap::<String, f64>::new();
    let mut previous_qty = BTreeMap::<String, f64>::new();
    let mut contributions = [0.; 2];
    let mut fees = [0.; 2];
    let mut filled = [0; 2];
    let mut daily = Vec::new();
    for (date, snapshot) in &snapshots {
        let segment = segments
            .iter()
            .find(|s| s.range.contains(*date))
            .ok_or("account date outside model selection")?;
        let m = &c.models[segment.model_index];
        let mut values = BTreeMap::new();
        for (id, qty) in snapshot["positions"]
            .as_object()
            .ok_or("positions missing")?
        {
            values.insert(
                id.clone(),
                qty.as_f64().ok_or("invalid position")?
                    * snapshot["valuation_prices"][id]
                        .as_f64()
                        .ok_or("valuation missing")?,
            );
        }
        for (id, value) in snapshot["corporate_pending_values"]
            .as_object()
            .into_iter()
            .flatten()
        {
            *values.entry(id.clone()).or_default() += number(value)?;
        }
        let mut pnl = BTreeMap::<String, f64>::new();
        if let Some(ledger) = &corporate {
            for row in
                ledger.corporate.audit.iter().filter(|r| {
                    r["date"] == date.to_string() && r["phase"] == "research_liquidation"
                })
            {
                let event = ledger
                    .corporate
                    .events
                    .iter()
                    .find(|e| row["event"] == e.id)
                    .ok_or("corporate_attribution_event_missing")?;
                *pnl.entry(event.instrument.clone()).or_default() +=
                    number(&row["notional"])? - number(&row["fee"])?;
            }
        }
        for (id, v) in &values {
            *pnl.entry(id.clone()).or_default() += v;
        }
        for (id, v) in &previous {
            *pnl.entry(id.clone()).or_default() -= v;
        }
        for order in trades.get(date).into_iter().flatten() {
            let qty = order["filled_quantity"].as_f64().unwrap_or(0.);
            if qty <= 0. {
                continue;
            }
            let id = order["instrument"]
                .as_str()
                .ok_or("fill instrument missing")?;
            let part = usize::from(!m.instruments.contains(id));
            let fee = order["fee"].as_f64().ok_or("fill fee missing")?;
            let amount = qty * order["price"].as_f64().ok_or("fill price missing")?;
            *pnl.entry(id.into()).or_default() += if order["side"] == "sell" {
                amount - fee
            } else {
                -amount - fee
            };
            fees[part] += fee;
            filled[part] += 1;
        }
        for action in actions["actions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a["date"] == date.to_string() && a["kind"] == "Dividend")
        {
            let id = action["symbol"].as_str().ok_or("dividend symbol missing")?;
            *pnl.entry(id.into()).or_default() += previous_qty.get(id).copied().unwrap_or(0.)
                * action["value"]
                    .as_f64()
                    .or_else(|| action["value"].as_str()?.parse::<f64>().ok())
                    .filter(|v| v.is_finite())
                    .ok_or("dividend amount missing")?;
        }
        let mut day = [0.; 2];
        for (id, p) in pnl {
            day[usize::from(!m.instruments.contains(&id))] += p;
        }
        for part in 0..2 {
            contributions[part] += day[part];
        }
        let predicted = predictions_by_date
            .get(date)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let seen = predicted.iter().filter(|p| p["seen"] == true).count();
        let candidates = pool.on(*date);
        let seen_candidates = candidates.intersection(&m.instruments).count();
        daily.push(json!({"date":date,"model_id":m.id,"seen_candidates":seen_candidates,"unseen_candidates":candidates.len()-seen_candidates,"seen_predictions":seen,"unseen_predictions":predicted.len()-seen,"seen_pnl_cny":day[0],"unseen_pnl_cny":day[1]}));
        previous = values;
        previous_qty = serde_json::from_value(snapshot["positions"].clone())?;
    }
    let groups=(0..2).map(|part| {
        let records=predictions.iter().filter(|p|p["seen"]==(part==0)).collect::<Vec<_>>();
        let mut rows=Vec::new();let mut scores=Vec::new();let dates=snapshots.keys().copied().collect::<Vec<_>>();
        for p in &records {if let (Some(label),Some(score))=(p["label"].as_f64(),p["score"].as_f64()) {let d:Date=serde_json::from_value(p["date"].clone()).unwrap();rows.push(data::Sample {day:dates.binary_search(&d).unwrap(),instrument:p["instrument"].as_str().unwrap().into(),label:Some(label),label_end:Some(d)});scores.push(score);}}
        let (ic,rank_ic)=portfolio::information_coefficients(&rows,&scores);
        json!({"group":if part==0{"seen"}else{"unseen"},"prediction_rows":records.len(),"labeled_rows":rows.len(),"ic":ic,"rank_ic":rank_ic,"pnl_contribution_cny":contributions[part],"execution_fees_cny":fees[part],"filled_orders":filled[part]})
    }).collect::<Vec<_>>();
    let seen = daily
        .iter()
        .any(|d| d["seen_candidates"].as_u64().unwrap_or(0) > 0);
    let equity_delta = snapshots
        .last_key_value()
        .and_then(|(_, v)| v["equity"].as_f64())
        .ok_or("final equity missing")?
        - account["initial_cash_cny"]
            .as_f64()
            .ok_or("initial cash missing")?;
    let residual = equity_delta - contributions.iter().sum::<f64>();
    let report = json!({"schema_version":1,"unattributed_pnl_cny":residual,"attribution_complete":residual.abs()<0.01,"label":if seen{"跨池回测（含曾见标的）"}else{"完全未见标的回测"},"classification":"relative to the active model's retained train/validation securities; daily PnL uses the day's model, not entry cohorts","groups":groups,"daily":daily,"model_segments":segments,"training_pool":c.pool});
    save(output.join("cross_pool_report.json"), &report)?;
    Ok(report)
}

pub(super) fn validate_execution(value: &Value, folds: &[ResolvedFold]) -> Result<()> {
    let c: ModelCatalogue = serde_json::from_value(value["catalogue"].clone())?;
    let segments: Vec<ModelSegment> = serde_json::from_value(value["segments"].clone())?;
    if c.signature != sign_catalogue(&c)? || segments.len() != folds.len() {
        return Err("invalid frozen model execution".into());
    }
    for (s, f) in segments.iter().zip(folds) {
        let m = c
            .models
            .get(s.model_index)
            .ok_or("model index outside catalogue")?;
        if m.id != s.model_id
            || s.range != f.actual.test
            || m.fit_data_end >= s.range.start
            || m.available_from.is_some_and(|d| d > s.range.start)
        {
            return Err("future/mismatched model execution".into());
        }
    }
    Ok(())
}

pub(super) fn replay_predictions(
    source: &Path,
    output: &Path,
    actions: &Path,
    rights_policy: analytics_core::rights::Policy,
    allocation: Option<&crate::allocation::AllocationConfig>,
    context: &analytics_core::task::TaskContext,
) -> Result<()> {
    let summary: Value = serde_json::from_slice(&fs::read(source.join("summary.json"))?)?;
    validate_result(&summary)?;
    let mut r: ResearchRequest =
        serde_json::from_slice(&fs::read(source.join("independent_request.json"))?)?;
    if summary["execution"]["market_content_sha256"] != crate::market_content_hash(&r.data_root)? {
        return Err("prediction market identity mismatch".into());
    }
    let selected: Value = serde_json::from_slice(&fs::read(source.join("model_selection.json"))?)?;
    let folds = serde_json::from_value(summary["execution"]["resolved_folds"].clone())?;
    let mode = Execution {
        folds,
        catalogue: Some(serde_json::from_value(selected["catalogue"].clone())?),
        segments: serde_json::from_value(selected["segments"].clone())?,
        training_plan: None,
    };
    if let Some(c) = allocation {
        c.validate_count(r.config.top_n)?;
        r.config.allocation = c.clone();
    }
    r.output = output.to_owned();
    r.reuse_training = None;
    r.reuse_predictions = Some(source.to_owned());
    r.corporate_actions = Some(actions.to_owned());
    r.rights_policy = Some(rights_policy);
    fs::create_dir_all(output)?;
    save(output.join("model_selection.json"), &selected)?;
    save(output.join("independent_request.json"), &r)?;
    fs::copy(
        source.join("backtest_plan.json"),
        output.join("backtest_plan.json"),
    )?;
    context.execute(|| crate::research::worker::scoped(|| stages::run(&r, Some(&mode), context)))
}

/// Hash the numeric/identity inputs actually used by A or B, excluding other securities.
#[allow(clippy::too_many_arguments)]
pub(super) fn input_signature(
    dataset: &DailyDataset,
    members: &BTreeMap<String, Vec<analytics_storage::qlib::Membership>>,
    benchmark: &str,
    start: Date,
    end: Date,
) -> Result<String> {
    let mut hasher = Sha256::new();
    if members.keys().any(|id| {
        dataset.checked_fields(id).is_some_and(|fields| {
            fields
                .iter()
                .any(|f| analytics_storage::three_factor::FIELDS.contains(&f.as_str()))
        })
    }) {
        let path = dataset
            .root
            .join(analytics_storage::three_factor::FILE_NAME);
        if path.exists() {
            hasher.update(fs::read(path)?);
        }
    }
    let symbols = members
        .keys()
        .cloned()
        .chain(std::iter::once(benchmark.to_owned()))
        .collect::<BTreeSet<_>>();
    let dates = dataset
        .calendar
        .iter()
        .enumerate()
        .filter(|(_, d)| **d >= start && **d <= end)
        .map(|(i, d)| (i, *d))
        .collect::<Vec<_>>();
    hasher.update(serde_json::to_vec(
        &dates.iter().map(|(_, d)| d).collect::<Vec<_>>(),
    )?);
    for id in symbols {
        analytics_core::task::TaskContext::current().check()?;
        let scope = dataset.checked_days(&id, None);
        if scope.as_ref().is_some_and(|days| days.is_empty()) {
            continue;
        }
        let identities = dates
            .iter()
            .filter(|(day, _)| scope.as_ref().is_none_or(|days| days.contains(day)))
            .map(|(_, d)| dataset.exchange_symbol(&id, *d))
            .collect::<std::io::Result<Vec<_>>>()?;
        hasher.update(serde_json::to_vec(&(
            &id,
            dataset.uses_supplier_prices(&id),
            identities,
        ))?);
        for field in dataset.checked_fields(&id).unwrap_or_else(|| {
            vec![
                "open".into(),
                "high".into(),
                "low".into(),
                "close".into(),
                "volume".into(),
                "amount".into(),
                "vwap".into(),
                "factor".into(),
            ]
        }) {
            let values = dataset.research_field(&id, &field, field != "factor")?;
            let field_scope = dataset.checked_days(&id, Some(&field));
            let selected = dates
                .iter()
                .filter(|(i, _)| field_scope.as_ref().is_none_or(|days| days.contains(i)))
                .map(|(i, _)| (*i, values.get(*i)))
                .collect::<Vec<_>>();
            hasher.update(serde_json::to_vec(&(&field, selected))?);
            if field == "factor" {
                hasher.update(serde_json::to_vec(&values.values.iter().flatten().next())?);
            }
        }
    }
    Ok(format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(hasher.finalize())
    ))
}

#[cfg(test)]
#[path = "independent_tests.rs"]
mod tests;
