//! Unadjusted CNY prices, integer shares, T+1 and dated corporate-action rights.
//! Account state persists between model folds. No adjusted price enters this API.
use crate::{
    CoreError, Date,
    market_rules::{FeeBreakdown, MarketRulesPreset, OrderSide},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
type Result<T> = std::result::Result<T, CoreError>;
fn invalid(message: &str) -> CoreError {
    CoreError::InvalidRequest(message.into())
}
fn positive(p: f64) -> bool {
    p.is_finite() && p > 0.0
}

/// One implemented distribution. Missing settlement dates remain missing data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorporateAction {
    /// Stable source event key (instrument and ex date).
    pub id: String,
    /// Canonical A-share symbol.
    pub instrument: String,
    /// Public implementation announcement date.
    pub announced: Date,
    /// Holdings at this day's close establish entitlement.
    pub record_date: Date,
    /// Price loses the cash/share right before this day's opening auction.
    pub ex_date: Date,
    /// Actual cash payment date, required only when cash is positive.
    pub pay_date: Option<Date>,
    /// Actual additional-share tradability date, required for bonus/transfer shares.
    pub listing_date: Option<Date>,
    /// Gross CNY dividend per old share, not per ten shares.
    pub cash_per_share: f64,
    /// Additional shares per old share (bonus plus capital transfer).
    pub shares_per_share: f64,
    /// Evidence URL(s), never used as executable instructions.
    pub source: String,
}
impl CorporateAction {
    /// Validate dates and rates; never infer payment/listing dates from ex date.
    pub fn validate(&self) -> Result<()> {
        crate::market_rules::classify(&self.instrument)?;
        if self.id.is_empty()
            || self.source.is_empty()
            || self.announced > self.record_date
            || self.record_date >= self.ex_date
            || !self.cash_per_share.is_finite()
            || self.cash_per_share < 0.0
            || !self.shares_per_share.is_finite()
            || !(0.0..=100.0).contains(&self.shares_per_share)
            || self.cash_per_share + self.shares_per_share == 0.0
        {
            return Err(invalid(
                "invalid corporate-action identity, chronology or distribution",
            ));
        }
        if self.cash_per_share > 0.0 && !self.pay_date.is_some_and(|d| d >= self.ex_date) {
            return Err(invalid(
                "cash dividend requires verified pay date >= ex date",
            ));
        }
        if self.shares_per_share > 0.0 && !self.listing_date.is_some_and(|d| d >= self.ex_date) {
            return Err(invalid(
                "stock distribution requires verified listing date >= ex date",
            ));
        }
        Ok(())
    }
}

/// Actual integer share balance; unsettled purchases are not sellable today.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Position {
    /// Total listed shares.
    pub shares: u64,
    /// Listed shares available to sell after T+1.
    pub sellable: u64,
    /// Last observed unadjusted mark; absence of a quote never invents a fill.
    pub mark: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Right {
    action: CorporateAction,
    shares: u64,
    ex_applied: bool,
    cash_paid: bool,
    stock_listed: bool,
}

/// Persistent portfolio ledger; monetary values are in CNY.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareLedger {
    /// Spendable cash, excluding dividend receivables.
    pub cash: f64,
    /// Integer listed positions.
    pub positions: BTreeMap<String, Position>,
    /// Explicit fixed research cash-dividend withholding (0 means gross).
    /// This does not model bonus-share income tax or personal FIFO holding-period tax.
    pub dividend_withholding_rate: f64,
    rights: Vec<Right>,
    recorded: BTreeSet<String>,
    date: Option<Date>,
    closed: bool,
}
impl ShareLedger {
    /// New cash account with explicit dividend tax convention.
    pub fn new(cash: f64, dividend_withholding_rate: f64) -> Result<Self> {
        if !positive(cash)
            || cash > 1e13
            || !dividend_withholding_rate.is_finite()
            || !(0.0..=1.0).contains(&dividend_withholding_rate)
        {
            return Err(invalid("invalid initial cash or dividend withholding rate"));
        }
        Ok(Self {
            cash,
            positions: BTreeMap::new(),
            dividend_withholding_rate,
            rights: Vec::new(),
            recorded: BTreeSet::new(),
            date: None,
            closed: true,
        })
    }
    /// Advance before trading. Release T+1 shares, then apply due rights once.
    /// Days may skip weekends; every trading day must be supplied by the caller.
    pub fn begin_day(&mut self, date: Date) -> Result<()> {
        if !self.closed || self.date.is_some_and(|old| date <= old) {
            return Err(invalid("ledger dates must increase after end_day"));
        }
        // Work on a copy so invalid/fractional corporate actions are atomic.
        let mut next = self.clone();
        next.date = Some(date);
        next.closed = false;
        for position in next.positions.values_mut() {
            position.sellable = position.shares;
        }
        for right in &mut next.rights {
            if date < right.action.ex_date {
                continue;
            }
            right.ex_applied = true;
            if !right.cash_paid && right.action.pay_date.is_some_and(|d| date >= d) {
                next.cash += cash_entitlement(
                    right.shares,
                    right.action.cash_per_share,
                    next.dividend_withholding_rate,
                );
                right.cash_paid = true;
            }
            if !right.stock_listed && right.action.listing_date.is_some_and(|d| date >= d) {
                let quantity = entitlement_shares(right.shares, right.action.shares_per_share)?;
                let p = next
                    .positions
                    .entry(right.action.instrument.clone())
                    .or_default();
                p.shares = p
                    .shares
                    .checked_add(quantity)
                    .ok_or_else(|| invalid("share overflow"))?;
                p.sellable = p
                    .sellable
                    .checked_add(quantity)
                    .ok_or_else(|| invalid("share overflow"))?;
                right.stock_listed = true;
            }
        }
        *self = next;
        Ok(())
    }
    /// Capture record-date closing holdings, including shares bought that day.
    /// Call after all fills; ex-date buyers cannot acquire an old entitlement.
    pub fn end_day(&mut self, actions: &[CorporateAction]) -> Result<()> {
        let date = self.date.ok_or_else(|| invalid("begin_day required"))?;
        if self.closed {
            return Err(invalid("end_day already applied"));
        }
        let mut next = self.clone();
        for action in actions.iter().filter(|a| a.record_date == date) {
            action.validate()?;
            if !next.recorded.insert(action.id.clone()) {
                return Err(invalid("duplicate corporate action"));
            }
            let shares = next
                .positions
                .get(&action.instrument)
                .map_or(0, |p| p.shares);
            if shares > 0 {
                entitlement_shares(shares, action.shares_per_share)?;
                if next.rights.iter().any(|r| {
                    r.ex_applied
                        && !r.stock_listed
                        && r.action.shares_per_share > 0.0
                        && r.action.instrument == action.instrument
                }) {
                    return Err(invalid(
                        "overlapping unlisted distributions require registrar entitlement data",
                    ));
                }
                next.rights.push(Right {
                    action: action.clone(),
                    shares,
                    ex_applied: false,
                    cash_paid: action.cash_per_share == 0.0,
                    stock_listed: action.shares_per_share == 0.0,
                });
            }
        }
        next.closed = true;
        *self = next;
        Ok(())
    }
    /// Update a position using an observed unadjusted price.
    pub fn mark(&mut self, instrument: &str, raw_price: f64) -> Result<()> {
        if !positive(raw_price) {
            return Err(invalid("mark must be a positive raw price"));
        }
        if let Some(p) = self.positions.get_mut(instrument) {
            p.mark = raw_price;
        }
        Ok(())
    }
    /// Assets include cash receivables and unlisted bonus-share rights from ex date.
    /// Rights survive sale of the old shares before payment/listing.
    pub fn nav(&self, marks: &BTreeMap<String, f64>) -> Result<f64> {
        let price = |id: &str| -> Result<f64> {
            marks
                .get(id)
                .copied()
                .or_else(|| self.positions.get(id).map(|p| p.mark))
                .filter(|p| positive(*p))
                .ok_or_else(|| invalid("missing raw mark for an owned share right"))
        };
        let mut total = self.cash;
        for (id, p) in &self.positions {
            total += p.shares as f64 * price(id)?;
        }
        for r in &self.rights {
            if !r.ex_applied {
                continue;
            }
            if !r.cash_paid {
                total += cash_entitlement(
                    r.shares,
                    r.action.cash_per_share,
                    self.dividend_withholding_rate,
                );
            }
            if !r.stock_listed {
                total += entitlement_shares(r.shares, r.action.shares_per_share)? as f64
                    * price(&r.action.instrument)?;
            }
        }
        if !positive(total) {
            return Err(invalid("portfolio exhausted or nonfinite NAV"));
        }
        Ok(total)
    }
    /// Largest valid buy quantity within a target and cash, including minimum fees.
    pub fn buy_quantity(
        &self,
        id: &str,
        price: f64,
        target_cny: f64,
        rules: &MarketRulesPreset,
    ) -> Result<u64> {
        if !positive(price) || !target_cny.is_finite() || target_cny < 0.0 {
            return Err(invalid("invalid buy budget/price"));
        }
        let board = rules.board_for(id)?;
        let budget = rules
            .affordable_buy_notional(id, self.cash.max(0.0))?
            .min(target_cny);
        let n = (budget / price).floor() as u64;
        let minimum = u64::from(board.minimum_buy_shares);
        let step = u64::from(board.buy_increment_shares);
        Ok(if n < minimum {
            0
        } else {
            minimum + (n - minimum) / step * step
        })
    }
    /// Fill at an already checked raw price. Reject invalid lots, oversells and T+0.
    /// Exchange suspension/price-limit checks belong to the quote adapter.
    pub fn fill(
        &mut self,
        id: &str,
        side: OrderSide,
        quantity: u64,
        price: f64,
        rules: &MarketRulesPreset,
    ) -> Result<FeeBreakdown> {
        if self.closed
            || self.date.is_none()
            || quantity == 0
            || !positive(price)
            || quantity > 1_000_000_000_000
        {
            return Err(invalid("invalid fill or closed trading day"));
        }
        let board = rules.board_for(id)?;
        if board.sell_delay_trading_days != 1 {
            return Err(invalid(
                "raw ledger currently requires the A-share T+1 preset",
            ));
        }
        if (price / board.tick_cny - (price / board.tick_cny).round()).abs() > 1e-6 {
            return Err(invalid("fill price violates raw tick size"));
        }
        let value = quantity as f64 * price;
        let fees = rules.fees(id, side, value)?;
        match side {
            OrderSide::Buy => {
                let min = u64::from(board.minimum_buy_shares);
                let step = u64::from(board.buy_increment_shares);
                if quantity < min
                    || !(quantity - min).is_multiple_of(step)
                    || value + fees.total() > self.cash + 1e-8
                {
                    return Err(invalid("buy violates lot size or available cash"));
                }
                let p = self.positions.entry(id.into()).or_default();
                let total = p
                    .shares
                    .checked_add(quantity)
                    .ok_or_else(|| invalid("share overflow"))?;
                self.cash -= value + fees.total();
                p.shares = total;
                p.mark = price;
            }
            OrderSide::Sell => {
                let p = self
                    .positions
                    .get_mut(id)
                    .ok_or_else(|| invalid("no holding to sell"))?;
                // Main/ChiNext odd-lot remainder must be sold in one order.
                let main_lot = matches!(
                    board.board,
                    crate::market_rules::Board::Main | crate::market_rules::Board::Chinext
                );
                let minimum_sell = u64::from(board.minimum_buy_shares);
                if quantity > p.sellable
                    || (!main_lot && quantity < minimum_sell && quantity != p.shares)
                    || (main_lot && !quantity.is_multiple_of(100) && quantity != p.shares)
                    || self.cash + value < fees.total()
                {
                    return Err(invalid("sell violates T+1, position or odd-lot rule"));
                }
                p.shares -= quantity;
                p.sellable -= quantity;
                p.mark = price;
                self.cash += value - fees.total();
                if p.shares == 0 {
                    self.positions.remove(id);
                }
            }
        }
        Ok(fees)
    }
}
fn cash_entitlement(shares: u64, per_share: f64, withholding: f64) -> f64 {
    (shares as f64 * per_share * (1.0 - withholding) * 100.0).round() / 100.0
}

fn entitlement_shares(shares: u64, ratio: f64) -> Result<u64> {
    let value = shares as f64 * ratio;
    if !value.is_finite() || value > 1e12 || (value - value.round()).abs() > 1e-7 {
        return Err(invalid(
            "fractional bonus entitlement requires actual registrar allocation; cannot invent or discard shares",
        ));
    }
    Ok(value.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn d(n: u32) -> Date {
        Date::from_ymd_opt(2024, 6, n).unwrap()
    }
    fn action() -> CorporateAction {
        CorporateAction {
            id: "test".into(),
            instrument: "SH600000".into(),
            announced: d(1),
            record_date: d(3),
            ex_date: d(4),
            pay_date: Some(d(6)),
            listing_date: Some(d(7)),
            cash_per_share: 1.0,
            shares_per_share: 0.5,
            source: "synthetic fixture".into(),
        }
    }
    #[test]
    fn delayed_cash_and_stock_rights_survive_sale_without_becoming_spendable() {
        let rules = MarketRulesPreset::a_share_latest();
        let a = action();
        let mut l = ShareLedger::new(10000.0, 0.0).unwrap();
        l.begin_day(d(3)).unwrap();
        l.fill("SH600000", OrderSide::Buy, 100, 10.0, &rules)
            .unwrap();
        assert!(
            l.fill("SH600000", OrderSide::Sell, 100, 10.0, &rules)
                .is_err()
        );
        l.end_day(std::slice::from_ref(&a)).unwrap();
        l.begin_day(d(4)).unwrap();
        let marks = BTreeMap::from([("SH600000".into(), 6.0)]);
        let nav = l.nav(&marks).unwrap(); // (10-1)/1.5=6, entitlement preserves assets.
        assert!((nav - (10000.0 - 5.01)).abs() < 1e-8);
        l.fill("SH600000", OrderSide::Sell, 100, 6.0, &rules)
            .unwrap();
        let cash = l.cash;
        assert!((l.nav(&marks).unwrap() - cash - 400.0).abs() < 1e-8);
        l.end_day(&[]).unwrap();
        l.begin_day(d(6)).unwrap();
        assert!((l.cash - cash - 100.0).abs() < 1e-8);
        l.end_day(&[]).unwrap();
        l.begin_day(d(7)).unwrap();
        assert_eq!(l.positions["SH600000"].shares, 50);
        l.fill("SH600000", OrderSide::Sell, 50, 6.0, &rules)
            .unwrap();
        assert!(l.positions.is_empty());
    }
    #[test]
    fn ex_date_buyer_has_no_old_right_and_lot_budget_includes_minimum_fee() {
        let rules = MarketRulesPreset::a_share_latest();
        let mut l = ShareLedger::new(1004.0, 0.0).unwrap();
        l.begin_day(d(4)).unwrap();
        assert_eq!(l.buy_quantity("SH600000", 10.0, 2000.0, &rules).unwrap(), 0);
        assert!(
            l.fill("SH600000", OrderSide::Buy, 100, 10.0, &rules)
                .is_err()
        );
        l.cash = 2000.0;
        l.fill("SH600000", OrderSide::Buy, 100, 10.0, &rules)
            .unwrap();
        l.end_day(&[action()]).unwrap();
        let cash = l.cash;
        l.begin_day(d(7)).unwrap();
        assert_eq!(l.cash, cash);
        assert_eq!(l.positions["SH600000"].shares, 100);
    }
    #[test]
    fn missing_dates_fractional_allocation_and_repeated_dates_fail_atomically() {
        let mut a = action();
        a.listing_date = None;
        assert!(a.validate().is_err());
        a = action();
        a.pay_date = None;
        assert!(a.validate().is_err());
        assert!(entitlement_shares(101, 0.5).is_err());
        let mut l = ShareLedger::new(1000.0, 0.0).unwrap();
        l.begin_day(d(3)).unwrap();
        assert!(l.begin_day(d(4)).is_err());
        l.end_day(&[]).unwrap();
        assert!(l.begin_day(d(3)).is_err());
    }
}
