//! Refresh planning stage.
use super::*;
/// Coalesce requested trading dates without hiding internal gaps.
pub fn ranges(calendar: &[Date], needed: &BTreeSet<Date>) -> Vec<DateRange> {
    let mut out: Vec<DateRange> = vec![];
    let mut adjacent = false;
    for d in calendar {
        if needed.contains(d) {
            if adjacent {
                out.last_mut().unwrap().end = *d;
            } else {
                out.push(DateRange { start: *d, end: *d });
            }
            adjacent = true;
        } else {
            adjacent = false;
        }
    }
    out
}
pub(super) fn earliest_train(t: &ResearchTemplate) -> Result<Date> {
    use analytics_core::schedule::Schedule;
    Ok(match &t.schedule {
        Schedule::Single { fold } => fold.train.start,
        Schedule::Rolling { first, .. } => first.train.start,
        Schedule::Manual { folds } => folds
            .iter()
            .map(|f| f.train.start)
            .min()
            .ok_or("empty schedule")?,
    })
}
pub(super) fn last_requested(t: &ResearchTemplate, calendar: &[Date]) -> Result<Date> {
    use analytics_core::schedule::Schedule;
    Ok(match &t.schedule {
        Schedule::Single { fold } => fold.test.end,
        Schedule::Manual { folds } => folds
            .iter()
            .map(|f| f.test.end)
            .max()
            .ok_or("empty schedule")?,
        Schedule::Rolling { until: Some(d), .. } => *d,
        Schedule::Rolling { until: None, .. } => {
            let now = chrono::Local::now();
            let complete = if now.hour() < 16 {
                now.date_naive().pred_opt().ok_or("date_overflow")?
            } else {
                now.date_naive()
            };
            complete.max(*calendar.last().ok_or("empty calendar")?)
        }
    })
}
pub(super) fn native_provider(v: &DataVersion, id: &str) -> Option<Provider> {
    let value = v
        .details
        .get("provider")
        .cloned()
        .or_else(|| v.details["security_sources"][id].get("provider").cloned())?;
    serde_json::from_value(value).ok()
}
pub(super) fn raw_rows(d: &DailyDataset, id: &str, requested: &DateRange) -> Result<Vec<DailyBar>> {
    let fields = ["open", "high", "low", "close"]
        .into_iter()
        .map(|name| d.research_raw_field(id, name, true))
        .collect::<std::io::Result<Vec<_>>>()?;
    let volume = d.research_raw_volume(id, true)?;
    let amount = d.research_field(id, "amount", true)?;
    Ok(d.calendar
        .iter()
        .enumerate()
        .filter(|(_, day)| requested.contains(**day))
        .map(|(i, day)| DailyBar {
            instrument_id: id.into(),
            trade_date: *day,
            open: fields[0].get(i),
            high: fields[1].get(i),
            low: fields[2].get(i),
            close: fields[3].get(i),
            volume: volume.get(i),
            amount: amount.get(i),
            adj_open: None,
            adj_high: None,
            adj_low: None,
            adj_close: None,
        })
        .collect())
}
pub fn plan(w: &Workspace, request: &RefreshRequest, policy: &SourcePolicy) -> Result<RefreshPlan> {
    policy.validate()?;
    validate_workers(request.workers)?;
    if request.reuse_shards_from.len() > 8
        || request
            .reuse_shards_from
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != request.reuse_shards_from.len()
    {
        return Err("invalid_shard_reuse_tasks".into());
    }
    request.template.validate()?;
    let base = version(w, &request.market, "market")?;
    let d = DailyDataset::open(w.resolve(&base.path))?;
    let (pool, membership) = w.resolve_pool(&request.template.pool_version, &base.id)?;
    let first = earliest_train(&request.template)?;
    let pos = d.calendar.partition_point(|d| *d < first);
    let lo = *d
        .calendar
        .get(pos.saturating_sub(request.template.warmup_sessions()))
        .ok_or("warmup_calendar_missing")?;
    let end = request
        .end
        .unwrap_or(last_requested(&request.template, &d.calendar)?);
    let history = DateRange { start: lo, end };
    history.validate()?;
    let mut ids: BTreeSet<_> = pool
        .intervals
        .iter()
        .filter(|(_, rs)| rs.iter().any(|r| r.start <= end && r.end >= lo))
        .map(|(id, _)| id.clone())
        .collect();
    ids.insert(request.template.benchmark.clone());
    let actions = request
        .actions
        .as_deref()
        .map(|id| version(w, id, "actions"))
        .transpose()?;
    if let Some(a) = &actions {
        let b = ActionBundle::read(&w.resolve(&a.path))?;
        ids.extend(b.required_symbols(&ids, first, end));
    }
    let calendar: Vec<_> = d
        .calendar
        .iter()
        .copied()
        .filter(|d| history.contains(*d))
        .collect();
    let priorities = policy.routes(DataRole::Quotes, request.provider);
    let mut securities = vec![];
    for id in ids {
        TaskContext::current().check()?;
        if request.mode == RefreshMode::Actions {
            securities.push(SecurityPlan {
                instrument: id,
                requested: history.clone(),
                refresh_ranges: vec![],
                existing_rows: 0,
                quote_sources: priorities.clone(),
            });
            continue;
        }
        let rows = raw_rows(&d, &id, &history)?;
        let present: BTreeSet<_> = rows
            .iter()
            .filter(|r| {
                r.open.is_some()
                    && r.high.is_some()
                    && r.low.is_some()
                    && r.close.is_some()
                    && r.volume.is_some()
                    && r.amount.is_some()
            })
            .map(|r| r.trade_date)
            .collect();
        let mut needed: BTreeSet<_> = calendar
            .iter()
            .filter(|d| !present.contains(d))
            .copied()
            .collect();
        if request.mode == RefreshMode::Full
            || native_provider(&base, &id) != priorities.first().copied()
        {
            needed.extend(&calendar);
        }
        if request.mode == RefreshMode::Incremental {
            needed.extend(calendar.iter().rev().take(policy.overlap_sessions));
        }
        let mut rs = ranges(&calendar, &needed);
        if end > *d.calendar.last().ok_or("empty_calendar")? {
            rs.push(DateRange {
                start: d
                    .calendar
                    .last()
                    .unwrap()
                    .succ_opt()
                    .ok_or("date_overflow")?,
                end,
            });
        }
        securities.push(SecurityPlan {
            instrument: id,
            requested: history.clone(),
            refresh_ranges: rs,
            existing_rows: present.len(),
            quote_sources: priorities.clone(),
        });
    }
    Ok(RefreshPlan {
        schema_version: 1,
        source_policy: policy.clone(),
        base_market: base,
        base_actions: actions,
        history,
        securities,
        membership,
        notes: vec![
            "训练价量、成员覆盖与实际账户依赖分别核验；下载成功不等于研究准入".into(),
            "按用途优先HiThink，通达信备用；不支持或覆盖不足记录后尝试下一来源".into(),
            "历史成员保持所选版本覆盖，当前成分及当前停牌/涨跌停不回填历史".into(),
            "切换行情来源重取该证券所需历史，禁止逐字段拼接".into(),
        ],
    })
}
