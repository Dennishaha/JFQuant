//! Daily raw-price portfolio. Models change yearly; the cash/share ledger does not.
use super::*;
use super::{data::YearMeta, portfolio::DayResult};
use analytics_core::{
    execution::{CorporateAction, ShareLedger},
    market_rules::{Board, FeeBreakdown},
};
use analytics_storage::{corporate_actions::ActionBundle, daily::DailyDataset, qlib::DailyField};
use std::collections::{BTreeSet, HashMap};

pub(super) struct RawPortfolio {
    event_index: Option<analytics_storage::corporate_actions::execution_index::ExecutionIndex>,
    ledger: ShareLedger,
    previous_nav: f64,
    rebalance_clock: usize,
    last_day: Option<usize>,
    pending_signal: Option<Vec<(String, f64)>>,
    marks: BTreeMap<String, f64>,
    pool_exits: BTreeSet<String>,
    previous_eligible: BTreeSet<String>,
}
impl RawPortfolio {
    pub(super) fn new(request: &ResearchRequest) -> Result<Self> {
        let mut ledger =
            ShareLedger::new(request.initial_cash_cny, request.dividend_withholding_rate)?;
        ledger.action_policy = request.action_policy.clone();
        Ok(Self {
            event_index: None,
            ledger,
            previous_nav: request.initial_cash_cny,
            rebalance_clock: 0,
            last_day: None,
            pending_signal: None,
            marks: BTreeMap::new(),
            pool_exits: BTreeSet::new(),
            previous_eligible: BTreeSet::new(),
        })
    }
}
fn reject_order(
    request: &ResearchRequest,
    date: Date,
    id: &str,
    side: &str,
    reason: &str,
) -> Result<()> {
    use std::io::Write;
    writeln!(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(request.output.join("order_events.jsonl"))?,
        "{}",
        serde_json::json!({"date":date,"instrument":id,"side":side,"status":"not_filled","reason":reason})
    )?;
    Ok(())
}
struct RawFields {
    open: DailyField,
    close: DailyField,
    volume: DailyField,
    factor: DailyField,
}
impl RawFields {
    fn load(d: &DailyDataset, id: &str) -> Result<Self> {
        Self::load_with_missing(d, id, false)
    }
    fn load_with_missing(d: &DailyDataset, id: &str, allow_missing: bool) -> Result<Self> {
        Ok(Self {
            open: d.research_raw_field(id, "open", allow_missing)?,
            close: d.research_raw_field(id, "close", allow_missing)?,
            volume: d.research_raw_volume(id, allow_missing)?,
            factor: match d.adjustment_factors(id) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => DailyField {
                    start: 0,
                    values: vec![None; d.calendar.len()],
                },
                r => r?,
            },
        })
    }
    fn adjustment_inputs(d: &DailyDataset, id: &str) -> Result<Self> {
        let close = match d.raw_field(id, "close") {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DailyField {
                start: 0,
                values: vec![None; d.calendar.len()],
            },
            r => r?,
        };
        Ok(Self {
            open: DailyField {
                start: 0,
                values: vec![],
            },
            close,
            volume: DailyField {
                start: 0,
                values: vec![],
            },
            factor: d.adjustment_factors(id)?,
        })
    }
    fn open(&self, day: usize) -> Option<f64> {
        self.open.get(day)
    }
    fn close(&self, day: usize) -> Option<f64> {
        self.close.get(day)
    }
}
/// Check every adjustment-factor transition against dated distributions. Unknown
/// rights issues, missing events and unexplained source adjustments stop execution.
#[cfg(test)]
pub(super) fn validate_actions(
    dataset: &DailyDataset,
    book: &ActionBundle,
    symbols: &BTreeSet<String>,
    start: Date,
    end: Date,
) -> Result<()> {
    book.require_coverage(symbols, start, end)?;
    let issues = adjustment_issues(dataset, book, symbols, start, end)?;
    if !issues.is_empty() {
        return Err(format!("company-action adjustment issues: {issues:?}").into());
    }
    Ok(())
}
/// Independently verify every coefficient transition and every implemented event,
/// including training and warmup; return individual securities without starting training.
pub(super) fn adjustment_issues(
    dataset: &DailyDataset,
    book: &ActionBundle,
    symbols: &BTreeSet<String>,
    start: Date,
    end: Date,
) -> Result<BTreeMap<String, Vec<String>>> {
    let lo = dataset.calendar.partition_point(|d| *d < start).max(1);
    let hi = dataset.calendar.partition_point(|d| *d <= end);
    let mut out = BTreeMap::new();
    for id in symbols {
        analytics_core::task::TaskContext::current().check()?;
        let result = (|| -> Result<Vec<String>> {
            let range = dataset.security_range(id, start, end)?;
            let start = range.start;
            let end = range.end;
            let lo = dataset.calendar.partition_point(|d| *d < start).max(lo);
            let hi = dataset.calendar.partition_point(|d| *d <= end).min(hi);
            if book.failures.contains_key(id) {
                return Err("company-action source failure remains unresolved".into());
            }
            for action in book
                .actions
                .iter()
                .filter(|a| a.instrument == *id && a.ex_date >= start && a.ex_date <= end)
            {
                action.validate_adjustment_terms()?;
            }

            let fields = RawFields::adjustment_inputs(dataset, id)?;
            let explicit = dataset.has_independent_coefficients();
            let events: BTreeMap<_, _> = book
                .actions
                .iter()
                .filter(|a| a.instrument == *id)
                .map(|a| (a.ex_date, a))
                .collect();
            let mut issues = Vec::new();
            for a in events
                .values()
                .filter(|a| a.ex_date >= start && a.ex_date <= end)
            {
                match dataset.calendar.binary_search(&a.ex_date) {
                    Err(_) => issues.push((
                        a.ex_date,
                        "company action outside verified trading calendar".to_string(),
                    )),
                    Ok(0) => issues.push((
                        a.ex_date,
                        "ex-date reference price unavailable at first calendar observation"
                            .to_string(),
                    )),
                    _ => {}
                }
            }
            for date in dataset.adjustment_change_dates(id)? {
                if date >= start && date <= end {
                    match dataset.calendar.binary_search(&date) {
                        Err(_) => issues.push((
                            date,
                            "coefficient change outside verified trading calendar".to_string(),
                        )),
                        Ok(0) if !events.contains_key(&date) => issues.push((
                            date,
                            "first-day coefficient change without verifiable event/reference price"
                                .to_string(),
                        )),
                        _ => {}
                    }
                }
            }
            let mut previous_factor = (0..lo).rev().find_map(|d| fields.factor.get(d));
            for day in lo..hi {
                analytics_core::task::TaskContext::current().check()?;
                let date = dataset.calendar[day];
                let current = fields.factor.get(day);
                let transition = previous_factor.zip(current).map(|(a, b)| b / a);
                if let Some(a) = events.get(&date) {
                    let reference = fields
                        .close(day - 1)
                        .and_then(|c| a.reference_price(c).ok().map(|p| (c, p)));
                    let reference = if matches!(
                        a.terms.reference(),
                        Some(analytics_core::actions::Reference::Unchanged { .. })
                    ) {
                        Some((1., 1.))
                    } else {
                        reference
                    };
                    match (reference,transition) {
                        (Some((previous,reference)),Some(ratio)) if reference>0. && ratio.is_finite() => {
                            if (ratio-1.).abs()<=1e-12 && !matches!(a.terms.reference(),Some(analytics_core::actions::Reference::Unchanged{..})) {
                                issues.push((date, "company action without coefficient change".to_string()));
                            } else if (ratio/(previous/reference)-1.).abs()>0.002 {
                                issues.push((date, "distribution disagrees with independent coefficient (possible special distribution/rights issue/source revision)".to_string()));
                            }
                        }
                        _=>issues.push((date, "ex-date reference price or coefficient unavailable; require verified exchange reference data".to_string())),
                    }
                } else if let Some(ratio) = transition {
                    let threshold = if explicit { 1e-12 } else { 0.00002 };
                    if !ratio.is_finite() || (ratio - 1.).abs() > threshold {
                        issues.push((
                            date,
                            "unexplained coefficient change; company-action history incomplete"
                                .to_string(),
                        ));
                    }
                }
                if current.is_some() {
                    previous_factor = current;
                }
            }
            Ok(compress_adjustment_issues(&dataset.calendar, issues))
        })();
        analytics_core::task::TaskContext::current().check()?;
        match result {
            Ok(issues) if !issues.is_empty() => {
                out.insert(id.clone(), issues);
            }
            Err(error) => {
                out.insert(id.clone(), vec![error.to_string()]);
            }
            _ => {}
        }
    }
    Ok(out)
}

fn compress_adjustment_issues(calendar: &[Date], issues: Vec<(Date, String)>) -> Vec<String> {
    let mut groups = BTreeMap::<String, Vec<Date>>::new();
    for (date, message) in issues {
        groups.entry(message).or_default().push(date);
    }
    let mut output = Vec::new();
    for (message, mut dates) in groups {
        dates.sort();
        dates.dedup();
        let mut ranges = Vec::<(Date, Date)>::new();
        for date in dates {
            if let Some((_, end)) = ranges.last_mut()
                && calendar
                    .binary_search(end)
                    .ok()
                    .zip(calendar.binary_search(&date).ok())
                    .is_some_and(|(a, b)| b == a + 1)
            {
                *end = date;
            } else {
                ranges.push((date, date));
            }
        }
        output.extend(ranges.into_iter().map(|(start, end)| {
            if start == end {
                format!("{start}: {message}")
            } else {
                format!("{start}..{end}: {message}")
            }
        }));
    }
    output
}

struct AdjustmentContext<'a> {
    explicit: bool,
    previous: Date,
    date: Date,
    index: &'a analytics_storage::corporate_actions::execution_index::ExecutionIndex,
}
fn check_daily_adjustment(
    id: &str,
    f: &RawFields,
    day: usize,
    actions: &[CorporateAction],
    context: AdjustmentContext<'_>,
) -> Result<()> {
    if day == 0 {
        return Ok(());
    }
    let independent = context
        .index
        .independent_ratio(id, context.previous, context.date)?;
    let explicit = context.explicit || independent.is_some();
    let transition = independent.or_else(|| {
        f.factor
            .get(day - 1)
            .zip(f.factor.get(day))
            .map(|(p, n)| n / p)
    });
    let event = actions.iter().find(|a| a.instrument == id);
    if let Some(a) = event {
        a.validate_adjustment_terms()?;
        let unchanged = matches!(
            a.terms.reference(),
            Some(analytics_core::actions::Reference::Unchanged { .. })
        );
        if unchanged
            && transition.is_some_and(|r| (r - 1.).abs() > if explicit { 1e-12 } else { 0.00002 })
        {
            return Err("execution_data_missing: unchanged_reference_coefficient_conflict".into());
        }
        if !unchanged {
            let previous = f
                .close(day - 1)
                .ok_or("execution_data_missing: event_reference_close_missing")?;
            let reference = a.reference_price(previous)?;
            let ratio = transition.ok_or("execution_data_missing: event_coefficient_missing")?;
            if (ratio / (previous / reference) - 1.).abs() > 0.002 {
                return Err("execution_data_missing: event_coefficient_conflict".into());
            }
        }
    } else if transition.is_some_and(|r| (r - 1.).abs() > if explicit { 1e-12 } else { 0.00002 }) {
        return Err("execution_data_missing: unexplained_coefficient_transition".into());
    }
    Ok(())
}
struct TradeContext<'a> {
    rules: &'a MarketRulesPreset,
    data: Option<&'a analytics_storage::execution_data::ExecutionData>,
    date: Date,
}
fn trade_allowed(
    id: &str,
    code: &str,
    field: &RawFields,
    day: usize,
    actions: &[CorporateAction],
    execution: TradeContext<'_>,
    buy: bool,
) -> Result<bool> {
    let Some(price) = field.open(day) else {
        return Ok(false);
    };
    if !field.volume.get(day).is_some_and(|v| v > 0.0) {
        return Ok(false);
    }
    if let Some(data) = execution.data {
        if data
            .suspensions
            .get(id)
            .is_some_and(|v| v.contains_key(&execution.date))
        {
            return Ok(false);
        }
        if let Some([lower, upper]) = data.limits.get(id).and_then(|v| v.get(&execution.date)) {
            if price < *lower - 1e-7 || price > *upper + 1e-7 {
                return Err(format!(
                    "data_not_ready: execution_price_outside_limits: {id} {}",
                    execution.date
                )
                .into());
            }
            return Ok(if buy {
                price < *upper - 1e-7
            } else {
                price > *lower + 1e-7
            });
        }
    }
    let Some(mut previous) = day.checked_sub(1).and_then(|d| field.close(d)) else {
        return Ok(false);
    };
    for a in actions.iter().filter(|a| a.instrument == id) {
        previous = a.reference_price(previous)?;
    }
    if previous <= 0.0 {
        return Ok(false);
    }
    let limit = execution.rules.board_for(code)?.daily_limit - 0.005;
    let change = price / previous - 1.0;
    Ok(if buy { change < limit } else { change > -limit })
}
pub(super) fn simulate(
    state: &mut RawPortfolio,
    meta: &YearMeta,
    scores: &[f64],
    dataset: &DailyDataset,
    book: &ActionBundle,
    request: &ResearchRequest,
    range: &DateRange,
) -> Result<Vec<DayResult>> {
    if meta.rows.len() != scores.len() {
        return Err("prediction length mismatch".into());
    }
    let requested_first = dataset.calendar.partition_point(|d| *d < range.start);
    let first = state.last_day.map_or(requested_first, |d| d + 1);
    let end = dataset.calendar.partition_point(|d| *d <= range.end);
    if first >= end {
        return Err("no raw execution calendar".into());
    }
    if state.last_day.is_some_and(|day| day + 1 != first) {
        return Err("raw ledger requires consecutive model folds".into());
    }
    state.ledger.source_map = dataset.source_map().clone();
    let pool = match &request.pool {
        Some(p) => p.clone(),
        None => dataset.pool(&request.universe_id)?,
    };
    let symbols: BTreeSet<_> = pool
        .intervals
        .keys()
        .chain(state.ledger.positions.keys())
        .cloned()
        .collect();
    let symbols =
        book.required_symbols(&symbols, dataset.calendar[first], dataset.calendar[end - 1]);
    // Load one field per stock; no DuckDB/LightGBM handle crosses a worker boundary.
    let fields: HashMap<_, _> = symbols
        .iter()
        .map(|id| {
            Ok((
                id.as_str(),
                RawFields::load_with_missing(dataset, id, true)?,
            ))
        })
        .collect::<Result<_>>()?;
    if state.event_index.is_none() {
        state.event_index = Some(
            analytics_storage::corporate_actions::execution_index::ExecutionIndex::build(
                book,
                request.event_check_policy,
            )?,
        );
        let warnings = state.event_index.as_ref().unwrap().warnings();
        fs::write(
            request.output.join("execution_warnings.json"),
            serde_json::to_vec_pretty(&warnings)?,
        )?;
    }
    let benchmark = RawFields::load(dataset, &request.benchmark)?;
    let mut signals = BTreeMap::<usize, Vec<(String, f64)>>::new();
    for (r, s) in meta.rows.iter().zip(scores) {
        if s.is_finite() {
            signals
                .entry(r.day + 1)
                .or_default()
                .push((r.instrument.clone(), *s));
        }
    }
    if first == requested_first
        && let Some(previous) = state.pending_signal.take()
    {
        signals.insert(first, previous);
    } else {
        state.pending_signal = None;
    }
    state.pending_signal = signals.remove(&end);
    let mut out = Vec::new();
    for day in first..end {
        analytics_core::task::TaskContext::current().check()?;
        let date = dataset.calendar[day];
        let held = state.ledger.required_instruments();
        if !held.is_empty() {
            for id in &held {
                state
                    .event_index
                    .as_ref()
                    .unwrap()
                    .check(id, date)
                    .map_err(|e| {
                        format!("data_not_ready: held account dependencies {date}: {e}")
                    })?;
            }
        }
        let todays_events = state.event_index.as_ref().unwrap().ex_events(date);
        for id in &held {
            if let Some(f) = fields.get(id.as_str()) {
                check_daily_adjustment(
                    id,
                    f,
                    day,
                    todays_events,
                    AdjustmentContext {
                        explicit: dataset.has_independent_coefficients(),
                        previous: dataset.calendar[day.saturating_sub(1)],
                        date,
                        index: state.event_index.as_ref().unwrap(),
                    },
                )
                .map_err(|e| {
                    format!("data_not_ready: held account dependencies {id} {date}: {e}")
                })?;
            }
        }
        for id in state.ledger.positions.keys() {
            let f = fields
                .get(id.as_str())
                .ok_or("held security fields unavailable")?;
            if (f.close(day).is_none() || f.volume.get(day).is_none())
                && f.volume.get(day) != Some(0.)
                && !state
                    .event_index
                    .as_ref()
                    .unwrap()
                    .full_day_suspension(id, date)
                && !quality::evidenced_nonquote(Some(book), id, date)
            {
                return Err(format!(
                    "data_not_ready: {id} {date}: held security has no close or verified suspension"
                )
                .into());
            }
        }
        let actions = state.event_index.as_ref().unwrap().ex_events(date).to_vec();
        // Adjust stale marks on an ex date even when a stock is suspended.
        for a in &actions {
            if let Some(p) = state.marks.get_mut(&a.instrument) {
                *p = a.reference_price(*p)?;
            }
        }
        state.ledger.begin_day(date)?;
        for (id, f) in &fields {
            if let Some(p) = f.open(day) {
                state.marks.insert((*id).into(), p);
            }
        }
        for (id, p) in &state.marks {
            state.ledger.mark(id, *p)?;
        }
        let opening_nav = state.ledger.nav(&state.marks)?;
        let mut traded = 0.0;
        let mut fees = FeeBreakdown::default();
        let mut blocked = 0;
        let eligible = pool.on(date);
        state.pool_exits.retain(|id| !eligible.contains(id));
        let scheduled = state
            .rebalance_clock
            .is_multiple_of(request.config.rebalance_days);
        if (scheduled && day >= requested_first)
            || request.pool_exit_policy == analytics_core::pool::PoolExitPolicy::NextTradableDay
        {
            state.pool_exits.extend(
                state
                    .ledger
                    .positions
                    .keys()
                    .filter(|id| !eligible.contains(*id))
                    .cloned(),
            );
        }
        let exit_ids: Vec<_> = state.pool_exits.iter().cloned().collect();
        for id in exit_ids {
            let Some(p) = state.ledger.positions.get(&id) else {
                state.pool_exits.remove(&id);
                continue;
            };
            let Some(f) = fields.get(id.as_str()) else {
                blocked += 1;
                continue;
            };
            let quantity = p.shares.min(p.sellable);
            let price = f.open(day);
            let can_trade = quantity > 0
                && price.is_some()
                && state.ledger.can_trade(&id)
                && trade_allowed(
                    &id,
                    &dataset.exchange_symbol(&id, date)?,
                    f,
                    day,
                    &actions,
                    TradeContext {
                        rules: &request.market_rules,
                        data: state.event_index.as_ref().and_then(|i| i.data.as_ref()),
                        date,
                    },
                    false,
                )?;
            let entry = serde_json::json!({"date":date,"instrument":id,"reason":"pool_removed","status":if can_trade{"filled"}else{"blocked"},"shares":quantity});
            use std::io::Write;
            writeln!(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(request.output.join("pool_events.jsonl"))?,
                "{}",
                entry
            )?;
            if can_trade {
                let price = price.unwrap();
                fees.add(state.ledger.fill(
                    &id,
                    OrderSide::Sell,
                    quantity,
                    price,
                    &request.market_rules,
                )?);
                traded += quantity as f64 * price;
            } else {
                blocked += 1;
            }
        }
        if eligible != state.previous_eligible {
            use std::io::Write;
            writeln!(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(request.output.join("pool_events.jsonl"))?,
                "{}",
                serde_json::json!({"date":date,"added":eligible.difference(&state.previous_eligible).collect::<Vec<_>>(),"removed":state.previous_eligible.difference(&eligible).collect::<Vec<_>>() })
            )?;
            state.previous_eligible = eligible.clone();
        }
        if state
            .rebalance_clock
            .is_multiple_of(request.config.rebalance_days)
            && day >= requested_first
            && let Some(ranked) = signals.get_mut(&day)
        {
            ranked.retain(|(id, _)| eligible.contains(id));
            ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let desired: BTreeSet<_> = ranked
                .iter()
                .take(request.config.top_n)
                .map(|(id, _)| id.as_str())
                .collect();
            let target = opening_nav / request.config.top_n as f64;
            let current: Vec<_> = state.ledger.positions.keys().cloned().collect();
            for id in current {
                if state.pool_exits.contains(&id) {
                    continue;
                }
                let Some(f) = fields.get(id.as_str()) else {
                    blocked += 1;
                    continue;
                };
                let Some(price) = f.open(day) else {
                    blocked += 1;
                    reject_order(request, date, &id, "sell", "missing_deal_price")?;
                    continue;
                };
                if !state.ledger.can_trade(&id) {
                    blocked += 1;
                    continue;
                }
                let p = &state.ledger.positions[&id];
                let sell = if desired.contains(id.as_str()) {
                    ((p.shares as f64 - target / price).max(0.0)).floor() as u64
                } else {
                    p.shares
                };
                let mut quantity = sell.min(p.sellable);
                if matches!(
                    request
                        .market_rules
                        .board_for(&dataset.exchange_symbol(&id, date)?)?
                        .board,
                    Board::Main | Board::Chinext
                ) && quantity != p.shares
                {
                    quantity = quantity / 100 * 100;
                }
                if quantity == 0 {
                    if sell > 0 {
                        blocked += 1;
                        reject_order(request, date, &id, "sell", "t_plus_one_or_minimum_lot")?;
                    }
                    continue;
                }
                if quantity
                    < u64::from(
                        request
                            .market_rules
                            .board_for(&dataset.exchange_symbol(&id, date)?)?
                            .minimum_buy_shares,
                    )
                    && quantity != p.shares
                {
                    blocked += 1;
                    continue;
                }
                if !state.ledger.can_trade(&id)
                    || !trade_allowed(
                        &id,
                        &dataset.exchange_symbol(&id, date)?,
                        f,
                        day,
                        &actions,
                        TradeContext {
                            rules: &request.market_rules,
                            data: state.event_index.as_ref().and_then(|i| i.data.as_ref()),
                            date,
                        },
                        false,
                    )?
                {
                    blocked += 1;
                    reject_order(
                        request,
                        date,
                        &id,
                        "sell",
                        "suspended_or_price_limit_or_nontradeable",
                    )?;
                    continue;
                }
                let order = state.ledger.fill(
                    &id,
                    OrderSide::Sell,
                    quantity,
                    price,
                    &request.market_rules,
                )?;
                fees.add(order);
                traded += quantity as f64 * price;
            }
            for (id, _) in ranked.iter().take(request.config.top_n) {
                let Some(f) = fields.get(id.as_str()) else {
                    blocked += 1;
                    reject_order(request, date, id, "buy", "source_missing")?;
                    continue;
                };
                let Some(price) = f.open(day) else {
                    blocked += 1;
                    reject_order(request, date, id, "buy", "missing_deal_price")?;
                    continue;
                };
                if let Err(e) = check_daily_adjustment(
                    id,
                    f,
                    day,
                    &actions,
                    AdjustmentContext {
                        explicit: dataset.has_independent_coefficients(),
                        previous: dataset.calendar[day.saturating_sub(1)],
                        date,
                        index: state.event_index.as_ref().unwrap(),
                    },
                ) {
                    blocked += 1;
                    reject_order(request, date, id, "buy", &e.to_string())?;
                    continue;
                }
                if !state.ledger.positions.contains_key(id) {
                    if let Err(e) = state.event_index.as_ref().unwrap().check(id, date) {
                        blocked += 1;
                        reject_order(
                            request,
                            date,
                            id,
                            "buy",
                            &format!("execution_data_missing: {e}"),
                        )?;
                        continue;
                    }
                    if let Err(e) = dataset.exchange_symbol(id, date) {
                        blocked += 1;
                        reject_order(request, date, id, "buy", &e.to_string())?;
                        continue;
                    }
                }
                let existing = state
                    .ledger
                    .positions
                    .get(id)
                    .map_or(0.0, |p| p.shares as f64 * price);
                let quantity = state.ledger.buy_quantity(
                    id,
                    price,
                    (target - existing).max(0.0),
                    &request.market_rules,
                )?;
                if quantity == 0 {
                    if target > existing {
                        blocked += 1;
                        reject_order(request, date, id, "buy", "insufficient_cash_or_minimum_lot")?;
                    }
                    continue;
                }
                if !state.ledger.can_trade(id)
                    || !trade_allowed(
                        id,
                        &dataset.exchange_symbol(id, date)?,
                        f,
                        day,
                        &actions,
                        TradeContext {
                            rules: &request.market_rules,
                            data: state.event_index.as_ref().and_then(|i| i.data.as_ref()),
                            date,
                        },
                        true,
                    )?
                {
                    blocked += 1;
                    reject_order(
                        request,
                        date,
                        id,
                        "buy",
                        "suspended_or_price_limit_or_nontradeable",
                    )?;
                    continue;
                }
                fees.add(state.ledger.fill(
                    id,
                    OrderSide::Buy,
                    quantity,
                    price,
                    &request.market_rules,
                )?);
                traded += quantity as f64 * price;
            }
        }
        for (id, f) in &fields {
            if let Some(p) = f.close(day) {
                state.marks.insert((*id).into(), p);
            }
        }
        for (id, p) in &state.marks {
            state.ledger.mark(id, *p)?;
        }
        let relevant_actions: Vec<_> = state
            .event_index
            .as_ref()
            .unwrap()
            .record_events(date)
            .iter()
            .filter(|a| state.ledger.positions.contains_key(&a.instrument))
            .cloned()
            .collect();
        state.ledger.end_day(&relevant_actions)?;
        let closing_nav = state.ledger.nav(&state.marks)?;
        let benchmark_return = if day > 0 {
            benchmark
                .close(day)
                .zip(benchmark.close(day - 1))
                .filter(|(_, p)| *p > 0.0)
                .map(|(p, old)| p / old - 1.0)
                .ok_or("benchmark close return missing")?
        } else {
            0.0
        };
        out.push(DayResult {
            date: date.to_string(),
            net_return: if state.previous_nav > 0. {
                closing_nav / state.previous_nav - 1.
            } else if closing_nav == 0. {
                0.
            } else {
                return Err("cannot define returns after zero-NAV resurrection".into());
            },
            benchmark_return,
            turnover: if opening_nav > 0. {
                traded / opening_nav / 2.
            } else {
                0.
            },
            cost: if opening_nav > 0. {
                (fees.total() + state.ledger.corporate_fees_today) / opening_nav
            } else {
                0.
            },
            corporate_action_cost_cny: state.ledger.corporate_fees_today,
            fee_breakdown_cny: fees,
            holdings: state.ledger.positions.len(),
            blocked_orders: blocked,
        });
        save(request.output.join("partial_daily.json"), &out)?;
        save(
            request.output.join("account_checkpoint.json"),
            &serde_json::json!({"date":date,"ledger":state.ledger,"complete":false}),
        )?;
        state.previous_nav = closing_nav;
        if state.rebalance_clock > 0 || signals.contains_key(&day) {
            state.rebalance_clock += 1;
        }
        state.last_day = Some(day);
    }
    fs::create_dir_all(
        request
            .output
            .join(format!("account-through-{}", range.end)),
    )?;
    save(
        request
            .output
            .join(format!("account-through-{}/ledger.json", range.end)),
        &state.ledger,
    )?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_event_coefficients_ignore_qlib_restoration_factor_drift() {
        let mut book: ActionBundle = serde_json::from_value(serde_json::json!({
            "schema_version":5,"downloader":"JFQuant analytics-workflow synthetic-demo-v1","downloaded_at":"fixture",
            "source_bindings":{},"coverage":{},"queried_symbols":[],"actions":[],"failures":{},"raw_sha256":{},
            "execution_data":{"schema_version":1,"suspensions":{},"limits":{},"coefficients":{"SH600000":{"2024-01-01":1}},
            "capabilities":{"SH600000:independent_sina_factor":{"status":"available","verified_through":"2024-12-31"}}}
        })).unwrap();
        let p = analytics_storage::corporate_actions::execution_index::ExecutionIndex::build(
            &book,
            Default::default(),
        )
        .unwrap();
        let field = RawFields {
            open: DailyField {
                start: 0,
                values: vec![Some(10.); 2],
            },
            close: DailyField {
                start: 0,
                values: vec![Some(10.); 2],
            },
            volume: DailyField {
                start: 0,
                values: vec![Some(100.); 2],
            },
            factor: DailyField {
                start: 0,
                values: vec![Some(1.), Some(1.0001)],
            },
        };
        let previous = "2024-01-02".parse().unwrap();
        let date = "2024-01-03".parse().unwrap();
        check_daily_adjustment(
            "SH600000",
            &field,
            1,
            &[],
            AdjustmentContext {
                explicit: false,
                previous,
                date,
                index: &p,
            },
        )
        .unwrap();
        book.execution_data = None;
        let p = analytics_storage::corporate_actions::execution_index::ExecutionIndex::build(
            &book,
            Default::default(),
        )
        .unwrap();
        assert!(
            check_daily_adjustment(
                "SH600000",
                &field,
                1,
                &[],
                AdjustmentContext {
                    explicit: false,
                    previous,
                    date,
                    index: &p
                }
            )
            .is_err()
        );
    }
    #[test]
    fn exact_limits_and_suspensions_override_buffer_without_inventing_fills() {
        let mut field = RawFields {
            open: DailyField {
                start: 0,
                values: vec![Some(10.2)],
            },
            close: DailyField {
                start: 0,
                values: vec![Some(10.)],
            },
            volume: DailyField {
                start: 0,
                values: vec![Some(100.)],
            },
            factor: DailyField {
                start: 0,
                values: vec![Some(1.)],
            },
        };
        let date: Date = "2024-01-02".parse().unwrap();
        let mut data = analytics_storage::execution_data::ExecutionData {
            schema_version: 1,
            limits: BTreeMap::from([("SH600000".into(), BTreeMap::from([(date, [9.8, 10.2])]))]),
            ..Default::default()
        };
        let rules = MarketRulesPreset::a_share_latest();
        assert!(
            !trade_allowed(
                "SH600000",
                "SH600000",
                &field,
                0,
                &[],
                TradeContext {
                    rules: &rules,
                    data: Some(&data),
                    date
                },
                true
            )
            .unwrap()
        );
        assert!(
            trade_allowed(
                "SH600000",
                "SH600000",
                &field,
                0,
                &[],
                TradeContext {
                    rules: &rules,
                    data: Some(&data),
                    date
                },
                false
            )
            .unwrap()
        );
        field.open.values[0] = Some(10.3);
        for buy in [true, false] {
            assert!(
                trade_allowed(
                    "SH600000",
                    "SH600000",
                    &field,
                    0,
                    &[],
                    TradeContext {
                        rules: &rules,
                        data: Some(&data),
                        date
                    },
                    buy
                )
                .is_err()
            );
        }
        field.open.values[0] = Some(10.2);
        data.suspensions.insert(
            "SH600000".into(),
            BTreeMap::from([(date, "intraday".into())]),
        );
        assert!(
            !trade_allowed(
                "SH600000",
                "SH600000",
                &field,
                0,
                &[],
                TradeContext {
                    rules: &rules,
                    data: Some(&data),
                    date
                },
                false
            )
            .unwrap()
        );
    }
    #[test]
    fn raw_folds_keep_real_shares_cash_and_dated_rights_across_new_year() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("qlib_bin");
        fs::create_dir_all(root.join("calendars")).unwrap();
        fs::create_dir_all(root.join("instruments")).unwrap();
        let dates = [
            "2024-12-30",
            "2024-12-31",
            "2025-01-02",
            "2025-01-03",
            "2025-01-06",
            "2025-01-07",
        ];
        fs::write(root.join("calendars/day.txt"), dates.join("\n")).unwrap();
        fs::write(
            root.join("instruments/csi500.txt"),
            "SH600000\t2024-12-30\t2025-01-07\n",
        )
        .unwrap();
        let write = |id: &str, name: &str, values: &[f32]| {
            let path = root.join(format!("features/{id}"));
            fs::create_dir_all(&path).unwrap();
            let bytes = std::iter::once(0.0_f32)
                .chain(values.iter().copied())
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>();
            fs::write(path.join(format!("{name}.day.bin")), bytes).unwrap();
        };
        // normalized price=raw*factor, factor changes by 10/6 on the ex date.
        for name in ["open", "close"] {
            write("sh600000", name, &[1.0; 6]);
            write("sh000905", name, &[100.0; 6]);
        }
        write(
            "sh600000",
            "factor",
            &[0.1, 0.1, 1.0 / 6.0, 1.0 / 6.0, 1.0 / 6.0, 1.0 / 6.0],
        );
        write("sh000905", "factor", &[1.0; 6]);
        for id in ["sh600000", "sh000905"] {
            write(id, "volume", &[10000.0; 6]);
        }
        let dataset = DailyDataset::open(&root).unwrap();
        let output = dir.path().join("run");
        fs::create_dir(&output).unwrap();
        let request = ResearchRequest::new(root.clone(), output);
        let action = CorporateAction {
            id: "SH600000:2025-01-02".into(),
            instrument: "SH600000".into(),
            announced: "2024-12-01".parse().unwrap(),
            record_date: "2024-12-31".parse().unwrap(),
            ex_date: "2025-01-02".parse().unwrap(),
            pay_date: Some("2025-01-06".parse().unwrap()),
            listing_date: Some("2025-01-07".parse().unwrap()),
            cash_per_share: 1.0,
            shares_per_share: 0.5,
            terms: Default::default(),
            source: "synthetic hand-calculated fixture".into(),
        };
        let mut book = ActionBundle {
            structured_coverage: Default::default(),
            execution_issues: Vec::new(),
            execution_data: None,
            schema_version: 5,
            source_bindings: Default::default(),
            coverage: Default::default(),
            downloader: "JFQuant analytics-workflow synthetic-demo-v1".into(),
            downloaded_at: "fixture".into(),
            queried_symbols: BTreeSet::from(["SH600000".into()]),
            actions: vec![action],
            failures: BTreeMap::new(),
            raw_sha256: BTreeMap::new(),
        };
        book.declare_synthetic_coverage(dataset.calendar[0], dataset.calendar[5])
            .unwrap();
        validate_actions(
            &dataset,
            &book,
            &book.queried_symbols,
            dataset.calendar[0],
            dataset.calendar[5],
        )
        .unwrap();
        let meta = YearMeta {
            signature: "fixture".into(),
            factor_ids: vec![],
            rows: vec![super::super::data::Sample {
                day: 0,
                instrument: "SH600000".into(),
                label: None,
                label_end: Some(dataset.calendar[5]),
            }],
            finite_counts: BTreeMap::new(),
        };
        let mut state = RawPortfolio::new(&request).unwrap();
        simulate(
            &mut state,
            &meta,
            &[1.0],
            &dataset,
            &book,
            &request,
            &DateRange {
                start: Date::from_ymd_opt(2024, 1, 1).unwrap(),
                end: Date::from_ymd_opt(2024, 12, 31).unwrap(),
            },
        )
        .unwrap();
        assert_eq!(state.ledger.positions["SH600000"].shares, 2000);
        let before_cash = state.ledger.cash;
        let before_nav = state.previous_nav;
        let empty = YearMeta {
            rows: vec![],
            ..meta
        };
        let second = simulate(
            &mut state,
            &empty,
            &[],
            &dataset,
            &book,
            &request,
            &DateRange {
                start: Date::from_ymd_opt(2025, 1, 1).unwrap(),
                end: Date::from_ymd_opt(2025, 12, 31).unwrap(),
            },
        )
        .unwrap();
        assert_eq!(state.ledger.positions["SH600000"].shares, 3000);
        assert!((state.ledger.cash - before_cash - 2000.0).abs() < 1e-8);
        assert!((state.previous_nav - before_nav).abs() < 1e-8);
        assert!(second.iter().all(|d| d.net_return.abs() < 1e-12));
        // Pool exits continue without model signals, and unlisted rights survive liquidation.
        use analytics_core::{
            DateRange,
            pool::{PoolExitPolicy, PoolMembership},
        };
        for (policy, expected_first) in [
            (PoolExitPolicy::NextTradableDay, "2025-01-02"),
            (PoolExitPolicy::NextRebalance, "2025-01-07"),
        ] {
            let mut request = ResearchRequest::new(
                dataset.root.clone(),
                dir.path().join(format!("policy-{policy:?}")),
            );
            fs::create_dir_all(&request.output).unwrap();
            request.config.rebalance_days = 4;
            request.pool_exit_policy = policy;
            request.pool = Some(PoolMembership {
                coverage: DateRange {
                    start: dataset.calendar[0],
                    end: dataset.calendar[5],
                },
                intervals: BTreeMap::from([(
                    "SH600000".into(),
                    vec![DateRange {
                        start: dataset.calendar[0],
                        end: dataset.calendar[1],
                    }],
                )]),
            });
            let mut state = RawPortfolio::new(&request).unwrap();
            let initial = YearMeta {
                signature: "fixture".into(),
                factor_ids: vec![],
                rows: vec![super::super::data::Sample {
                    day: 0,
                    instrument: "SH600000".into(),
                    label: None,
                    label_end: Some(dataset.calendar[5]),
                }],
                finite_counts: BTreeMap::new(),
            };
            simulate(
                &mut state,
                &initial,
                &[1.0],
                &dataset,
                &book,
                &request,
                &DateRange {
                    start: Date::from_ymd_opt(2024, 1, 1).unwrap(),
                    end: Date::from_ymd_opt(2024, 12, 31).unwrap(),
                },
            )
            .unwrap();
            simulate(
                &mut state,
                &empty,
                &[],
                &dataset,
                &book,
                &request,
                &DateRange {
                    start: Date::from_ymd_opt(2025, 1, 1).unwrap(),
                    end: Date::from_ymd_opt(2025, 12, 31).unwrap(),
                },
            )
            .unwrap();
            assert!(state.ledger.positions.is_empty());
            let events: Vec<serde_json::Value> =
                fs::read_to_string(request.output.join("pool_events.jsonl"))
                    .unwrap()
                    .lines()
                    .map(|s| serde_json::from_str(s).unwrap())
                    .filter(|v: &serde_json::Value| v["status"] == "filled")
                    .collect();
            assert_eq!(events[0]["date"], expected_first);
            assert_eq!(
                events
                    .iter()
                    .map(|v| v["shares"].as_u64().unwrap())
                    .sum::<u64>(),
                3000
            );
        }
        // Suspension blocks an exit; re-entry cancels it before a later usable quote.
        write(
            "sh600000",
            "volume",
            &[10000.0, 10000.0, 0.0, 10000.0, 10000.0, 10000.0],
        );
        let dataset = DailyDataset::open(&dataset.root).unwrap();
        let mut request = ResearchRequest::new(dataset.root.clone(), dir.path().join("reentry"));
        fs::create_dir_all(&request.output).unwrap();
        request.pool = Some(PoolMembership {
            coverage: DateRange {
                start: dataset.calendar[0],
                end: dataset.calendar[5],
            },
            intervals: BTreeMap::from([(
                "SH600000".into(),
                vec![
                    DateRange {
                        start: dataset.calendar[0],
                        end: dataset.calendar[1],
                    },
                    DateRange {
                        start: dataset.calendar[3],
                        end: dataset.calendar[5],
                    },
                ],
            )]),
        });
        let mut state = RawPortfolio::new(&request).unwrap();
        let initial = YearMeta {
            signature: "fixture".into(),
            factor_ids: vec![],
            rows: vec![super::super::data::Sample {
                day: 0,
                instrument: "SH600000".into(),
                label: None,
                label_end: Some(dataset.calendar[5]),
            }],
            finite_counts: BTreeMap::new(),
        };
        simulate(
            &mut state,
            &initial,
            &[1.0],
            &dataset,
            &book,
            &request,
            &DateRange {
                start: Date::from_ymd_opt(2024, 1, 1).unwrap(),
                end: Date::from_ymd_opt(2024, 12, 31).unwrap(),
            },
        )
        .unwrap();
        simulate(
            &mut state,
            &empty,
            &[],
            &dataset,
            &book,
            &request,
            &DateRange {
                start: Date::from_ymd_opt(2025, 1, 1).unwrap(),
                end: Date::from_ymd_opt(2025, 12, 31).unwrap(),
            },
        )
        .unwrap();
        assert_eq!(state.ledger.positions["SH600000"].shares, 3000);
        assert!(state.pool_exits.is_empty());
        // A score generated on the last eligible day cannot buy on the following day.
        request.output = dir.path().join("expired-signal");
        fs::create_dir_all(&request.output).unwrap();
        request
            .pool
            .as_mut()
            .unwrap()
            .intervals
            .get_mut("SH600000")
            .unwrap()
            .truncate(1);
        request
            .pool
            .as_mut()
            .unwrap()
            .intervals
            .get_mut("SH600000")
            .unwrap()[0]
            .end = dataset.calendar[0];
        let mut state = RawPortfolio::new(&request).unwrap();
        simulate(
            &mut state,
            &initial,
            &[1.0],
            &dataset,
            &book,
            &request,
            &DateRange {
                start: Date::from_ymd_opt(2024, 1, 1).unwrap(),
                end: Date::from_ymd_opt(2024, 12, 31).unwrap(),
            },
        )
        .unwrap();
        assert!(state.ledger.positions.is_empty());
        book.actions.clear();
        assert!(
            validate_actions(
                &dataset,
                &book,
                &book.queried_symbols,
                dataset.calendar[0],
                dataset.calendar[5]
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod issue_compression_tests {
    use super::*;
    #[test]
    fn consecutive_trading_issues_are_intervals_and_different_causes_stay_visible() {
        let days: Vec<Date> = ["2020-01-02", "2020-01-03", "2020-01-06"]
            .iter()
            .map(|d| d.parse().unwrap())
            .collect();
        let output = compress_adjustment_issues(
            &days,
            vec![
                (days[0], "missing event".into()),
                (days[1], "missing event".into()),
                (days[2], "missing event".into()),
                (days[1], "bad reference".into()),
            ],
        );
        assert_eq!(output.len(), 2);
        assert!(output.contains(&"2020-01-02..2020-01-06: missing event".into()));
        assert!(output.contains(&"2020-01-03: bad reference".into()));
    }
}
