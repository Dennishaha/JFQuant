//! Daily raw-price portfolio. Models change yearly; the cash/share ledger does not.
use super::*;
use super::{data::YearMeta, portfolio::DayResult};
use analytics_core::{
    execution::{CorporateAction, ShareLedger},
    market_rules::{Board, FeeBreakdown},
};
use analytics_storage::{
    corporate_actions::ActionBundle,
    qlib::{DailyField, raw_price},
};
use std::collections::{BTreeSet, HashMap};

pub(super) struct RawPortfolio {
    ledger: ShareLedger,
    previous_nav: f64,
    rebalance_clock: usize,
    last_day: Option<usize>,
    pending_signal: Option<Vec<(String, f64)>>,
    marks: BTreeMap<String, f64>,
}
impl RawPortfolio {
    pub(super) fn new(request: &ResearchRequest) -> Result<Self> {
        Ok(Self {
            ledger: ShareLedger::new(request.initial_cash_cny, request.dividend_withholding_rate)?,
            previous_nav: request.initial_cash_cny,
            rebalance_clock: 0,
            last_day: None,
            pending_signal: None,
            marks: BTreeMap::new(),
        })
    }
}
struct RawFields {
    open: DailyField,
    close: DailyField,
    volume: DailyField,
    factor: DailyField,
}
impl RawFields {
    fn load(d: &QlibDataset, id: &str) -> Result<Self> {
        Ok(Self {
            open: d.field(id, "open")?,
            close: d.field(id, "close")?,
            volume: d.field(id, "volume")?,
            factor: d.field(id, "factor")?,
        })
    }
    fn open(&self, day: usize) -> Option<f64> {
        raw_price(self.open.get(day), self.factor.get(day))
    }
    fn close(&self, day: usize) -> Option<f64> {
        raw_price(self.close.get(day), self.factor.get(day))
    }
}
/// Check every adjustment-factor transition against dated distributions. Unknown
/// rights issues, missing events and unexplained source adjustments stop execution.
pub(super) fn validate_actions(
    dataset: &QlibDataset,
    book: &ActionBundle,
    symbols: &BTreeSet<String>,
    start: Date,
    end: Date,
) -> Result<()> {
    book.require_coverage(symbols, start, end)?;
    let lo = dataset.calendar.partition_point(|d| *d < start).max(1);
    let hi = dataset.calendar.partition_point(|d| *d <= end);
    let mut by_date = BTreeMap::new();
    for a in &book.actions {
        if by_date
            .insert((a.instrument.as_str(), a.ex_date), a)
            .is_some()
        {
            return Err("duplicate action for security/ex date".into());
        }
    }
    for id in symbols {
        analytics_core::task::TaskContext::current().check()?;
        let fields = RawFields::load(dataset, id)?;
        let mut previous_factor = (0..lo).rev().find_map(|day| fields.factor.get(day));
        for day in lo..hi {
            analytics_core::task::TaskContext::current().check()?;
            let current_factor = fields.factor.get(day);
            let action = by_date.get(&(id.as_str(), dataset.calendar[day]));
            if let Some(a) = action {
                let previous = fields.close(day - 1).ok_or(
                    "ex-date reference price unavailable; require exchange reference data",
                )?;
                let reference = (previous - a.cash_per_share) / (1.0 + a.shares_per_share);
                if reference <= 0.0 {
                    return Err("invalid ex-right reference price".into());
                }
                if let Some((old, new)) = previous_factor.zip(current_factor) {
                    let expected = previous / reference;
                    if (new / old / expected - 1.0).abs() > 0.002 {
                        return Err(format!("{id} {} distribution disagrees with adjustment factor (possible rights issue/source revision)",dataset.calendar[day]).into());
                    }
                }
            } else if let Some((old, new)) = previous_factor.zip(current_factor)
                && old > 0.0
                && (new / old - 1.0).abs() > 0.00002
            {
                return Err(format!(
                    "{id} {} unexplained factor change; company-action history incomplete",
                    dataset.calendar[day]
                )
                .into());
            }
            if current_factor.is_some() {
                previous_factor = current_factor;
            }
        }
    }
    Ok(())
}
fn trade_allowed(
    id: &str,
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
        previous = (previous - a.cash_per_share) / (1.0 + a.shares_per_share);
    }
    if previous <= 0.0 {
        return Ok(false);
    }
    let limit = rules.board_for(id)?.daily_limit - 0.005;
    let change = price / previous - 1.0;
    Ok(if buy { change < limit } else { change > -limit })
}
pub(super) fn simulate(
    state: &mut RawPortfolio,
    meta: &YearMeta,
    scores: &[f64],
    dataset: &QlibDataset,
    book: &ActionBundle,
    request: &ResearchRequest,
    year: i32,
) -> Result<Vec<DayResult>> {
    if meta.rows.len() != scores.len() {
        return Err("prediction length mismatch".into());
    }
    let first = dataset.calendar.partition_point(|d| d.year() < year);
    let end = dataset.calendar.partition_point(|d| d.year() <= year);
    if first >= end {
        return Err("no raw execution calendar".into());
    }
    if state.last_day.is_some_and(|day| day + 1 != first) {
        return Err("raw ledger requires consecutive model folds".into());
    }
    let symbols: Vec<_> = dataset.membership("csi500")?.into_keys().collect();
    // Load one field per stock; no DuckDB/LightGBM handle crosses a worker boundary.
    let fields: HashMap<_, _> = symbols
        .iter()
        .map(|id| Ok((id.as_str(), RawFields::load(dataset, id)?)))
        .collect::<Result<_>>()?;
    let benchmark = RawFields::load(dataset, "SH000905")?;
    let mut signals = BTreeMap::<usize, Vec<(String, f64)>>::new();
    for (r, s) in meta.rows.iter().zip(scores) {
        if s.is_finite() {
            signals
                .entry(r.day + 1)
                .or_default()
                .push((r.instrument.clone(), *s));
        }
    }
    if let Some(previous) = state.pending_signal.take() {
        signals.insert(first, previous);
    }
    state.pending_signal = signals.remove(&end);
    let mut out = Vec::new();
    for day in first..end {
        analytics_core::task::TaskContext::current().check()?;
        let date = dataset.calendar[day];
        let actions: Vec<_> = book
            .actions
            .iter()
            .filter(|a| a.ex_date == date)
            .cloned()
            .collect();
        // Adjust stale marks on an ex date even when a stock is suspended.
        for a in &actions {
            if let Some(p) = state.marks.get_mut(&a.instrument) {
                *p = (*p - a.cash_per_share) / (1.0 + a.shares_per_share);
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
        if state
            .rebalance_clock
            .is_multiple_of(request.config.rebalance_days)
            && let Some(ranked) = signals.get_mut(&day)
        {
            ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let desired: BTreeSet<_> = ranked
                .iter()
                .take(request.config.top_n)
                .map(|(id, _)| id.as_str())
                .collect();
            let target = opening_nav / request.config.top_n as f64;
            let current: Vec<_> = state.ledger.positions.keys().cloned().collect();
            for id in current {
                let Some(f) = fields.get(id.as_str()) else {
                    blocked += 1;
                    continue;
                };
                let Some(price) = f.open(day) else {
                    blocked += 1;
                    continue;
                };
                let p = &state.ledger.positions[&id];
                let sell = if desired.contains(id.as_str()) {
                    ((p.shares as f64 - target / price).max(0.0)).floor() as u64
                } else {
                    p.shares
                };
                let mut quantity = sell.min(p.sellable);
                if matches!(
                    request.market_rules.board_for(&id)?.board,
                    Board::Main | Board::Chinext
                ) && quantity != p.shares
                {
                    quantity = quantity / 100 * 100;
                }
                if quantity == 0 {
                    continue;
                }
                if quantity < u64::from(request.market_rules.board_for(&id)?.minimum_buy_shares)
                    && quantity != p.shares
                {
                    blocked += 1;
                    continue;
                }
                if !trade_allowed(&id, f, day, &actions, &request.market_rules, false)? {
                    blocked += 1;
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
                    continue;
                };
                let Some(price) = f.open(day) else {
                    blocked += 1;
                    continue;
                };
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
                    continue;
                }
                if !trade_allowed(id, f, day, &actions, &request.market_rules, true)? {
                    blocked += 1;
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
        state.ledger.end_day(&book.actions)?;
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
            net_return: closing_nav / state.previous_nav - 1.0,
            benchmark_return,
            turnover: traded / opening_nav / 2.0,
            cost: fees.total() / opening_nav,
            fee_breakdown_cny: fees,
            holdings: state.ledger.positions.len(),
            blocked_orders: blocked,
        });
        state.previous_nav = closing_nav;
        if state.rebalance_clock > 0 || signals.contains_key(&day) {
            state.rebalance_clock += 1;
        }
        state.last_day = Some(day);
    }
    fs::create_dir_all(request.output.join(format!("fold-{year}")))?;
    save(
        request.output.join(format!("fold-{year}/ledger.json")),
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
        let dataset = QlibDataset::open(&root).unwrap();
        let output = dir.path().join("run");
        fs::create_dir(&output).unwrap();
        let request = ResearchRequest::new(root, output);
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
            source: "synthetic hand-calculated fixture".into(),
        };
        let mut book = ActionBundle {
            schema_version: 1,
            downloader: "JFQuant analytics-storage corporate-actions".into(),
            downloaded_at: "fixture".into(),
            queried_symbols: BTreeSet::from(["SH600000".into()]),
            actions: vec![action],
            failures: BTreeMap::new(),
            raw_sha256: BTreeMap::new(),
        };
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
                exit_year: 2025,
            }],
            finite_counts: BTreeMap::new(),
        };
        let mut state = RawPortfolio::new(&request).unwrap();
        simulate(&mut state, &meta, &[1.0], &dataset, &book, &request, 2024).unwrap();
        assert_eq!(state.ledger.positions["SH600000"].shares, 2000);
        let before_cash = state.ledger.cash;
        let before_nav = state.previous_nav;
        let empty = YearMeta {
            rows: vec![],
            ..meta
        };
        let second = simulate(&mut state, &empty, &[], &dataset, &book, &request, 2025).unwrap();
        assert_eq!(state.ledger.positions["SH600000"].shares, 3000);
        assert!((state.ledger.cash - before_cash - 2000.0).abs() < 1e-8);
        assert!((state.previous_nav - before_nav).abs() < 1e-8);
        assert!(second.iter().all(|d| d.net_return.abs() < 1e-12));
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
