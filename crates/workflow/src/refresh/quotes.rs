//! Refresh quotes stage.
use super::*;
pub(super) fn collect_shards<T: Send>(
    securities: &[SecurityPlan],
    workers: usize,
    context: &TaskContext,
    fetch: impl Fn(&SecurityPlan) -> Result<T> + Sync,
    mut accept: impl FnMut(&SecurityPlan, T) -> Result<()>,
) -> Result<()> {
    validate_workers(workers)?;
    for batch in securities.chunks(workers) {
        context.check()?;
        let results = std::thread::scope(|scope| {
            let jobs: Vec<_> = batch
                .iter()
                .map(|security| {
                    let fetch = &fetch;
                    scope.spawn(move || {
                        context.execute(|| fetch(security).map_err(|e| e.to_string()))
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|job| {
                    job.join()
                        .unwrap_or_else(|_| Err("acquisition_worker_panicked".into()))
                })
                .collect::<Vec<_>>()
        });
        for (security, result) in batch.iter().zip(results) {
            context.check()?;
            accept(security, result.map_err(invalid)?)?;
        }
    }
    Ok(())
}

type CollectedQuotes = (Vec<DailyBar>, AdjustmentRecord, PathBuf);
pub(super) struct QuoteShard {
    pub(super) attempts: Vec<Value>,
    pub(super) selected: Option<(Provider, CollectedQuotes)>,
    pub(super) calendar: Vec<Date>,
}
pub(super) fn acquire_quotes(
    security: &SecurityPlan,
    request: &RefreshRequest,
    plan: &RefreshPlan,
    base_path: &Path,
    staging: &Path,
    calendar: (&[Date], bool),
    context: &TaskContext,
) -> Result<QuoteShard> {
    let (calendar, calendar_verified) = calendar;
    let id = &security.instrument;
    let mut attempts = vec![];
    let mut selected = None;
    let mut extra_calendar = vec![];
    for provider in &security.quote_sources {
        context.check()?;
        if !matches!(provider, Provider::Tdxaidata | Provider::Hithink) {
            attempts.push(json!({"provider":provider,"status":"not_run","code":"quotation_route_unsupported"}));
            continue;
        }
        let root = staging.join(format!("quotes-{id}-{provider:?}"));
        let same = native_provider(&plan.base_market, id) == Some(*provider)
            && base_path.join("daily_manifest.json").is_file();
        let result = (|| -> Result<_> {
            if *provider == Provider::Hithink {
                let req = collection(*provider, id, &plan.history, &request.template.benchmark);
                let market = analytics_storage::hithink::collect_with_calendar(
                    &req,
                    &root,
                    context,
                    "market",
                    Some(calendar),
                )?;
                return Ok((
                    read_bars(&market.join("bars").join(format!("{id}.parquet")))?,
                    AdjustmentRecord::read(&market, id)?,
                    root.clone(),
                ));
            }
            let mut rows = BTreeMap::new();
            if same && request.mode != RefreshMode::Full {
                let base = DailyDataset::open(base_path)?;
                for bar in raw_rows(&base, id, &plan.history)? {
                    rows.insert(bar.trade_date, bar);
                }
            }
            let intervals = if same && request.mode != RefreshMode::Full {
                security.refresh_ranges.clone()
            } else {
                vec![plan.history.clone()]
            };
            for (index, range) in intervals.iter().enumerate() {
                let req = collection(*provider, id, range, &request.template.benchmark);
                let market =
                    collector::collect(&req, &root.join(format!("part-{index}")), context)?;
                let daily = DailyDataset::open(&market)?;
                if calendar_verified && daily.calendar.iter().any(|day| !calendar.contains(day)) {
                    return Err("source_calendar_conflict".into());
                }
                if !calendar_verified {
                    extra_calendar.extend(daily.calendar.iter().copied());
                }
                for bar in read_bars(&market.join("bars").join(format!("{id}.parquet")))? {
                    rows.insert(bar.trade_date, bar);
                }
            }
            let full = root.join("coefficients");
            collector::collect_coefficients(
                &collection(*provider, id, &plan.history, &request.template.benchmark),
                &full,
                context,
            )?;
            if rows.is_empty() {
                return Err("empty_refresh_history".into());
            }
            Ok((
                rows.into_values().collect(),
                AdjustmentRecord::read(&full.join("market"), id)?,
                root.clone(),
            ))
        })();
        match result {
            Ok(value) => {
                selected = Some((*provider, value));
                attempts.push(json!({"provider":provider,"status":"passed"}));
                break;
            }
            Err(error) => {
                context.check()?;
                attempts.push(
                    json!({"provider":provider,"status":"failed","reason":error.to_string()}),
                );
            }
        }
    }
    Ok(QuoteShard {
        attempts,
        selected,
        calendar: extra_calendar,
    })
}
