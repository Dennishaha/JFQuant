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

fn trade_allowed(
    id: &str,
    code: &str,
    field: &RawFields,
    day: usize,
    actions: &[CorporateAction],
    rules: &MarketRulesPreset,
    buy: bool,
) -> Result<bool> {
    let Some(price) = field.open(day) else {
        return Ok(false);
    };
    if !field.volume.get(day).is_some_and(|v| v > 0.0) {
        return Ok(false);
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
    let limit = rules.board_for(code)?.daily_limit - 0.005;
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
            book.require_coverage(&held, date, date)
                .map_err(|e| format!("data_not_ready: held account dependencies {date}: {e}"))?;
        }
        for id in state.ledger.positions.keys() {
            let f = fields
                .get(id.as_str())
                .ok_or("held security fields unavailable")?;
            if (f.close(day).is_none() || f.volume.get(day).is_none())
                && f.volume.get(day) != Some(0.)
                && !quality::evidenced_nonquote(Some(book), id, date)
            {
                return Err(format!(
                    "data_not_ready: {id} {date}: held security has no close or verified suspension"
                )
                .into());
            }
        }
        let actions: Vec<_> = book
            .actions
            .iter()
            .filter(|a| a.ex_date == date)
            .cloned()
            .collect();
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
                    &request.market_rules,
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
                        &request.market_rules,
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
                if !state.ledger.positions.contains_key(id) {
                    let scope = std::iter::once(id.clone()).collect();
                    if let Err(e) = book.require_coverage(&scope, date, date) {
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
                        &request.market_rules,
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
        let relevant_actions: Vec<_> = book
            .actions
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
            schema_version: 4,
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
