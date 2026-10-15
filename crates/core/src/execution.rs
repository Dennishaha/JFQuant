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

/// Account-data trust is independent of factor missing-value and training-price policies.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventCheckPolicy {
    /// Accept complete structured histories, retaining unknown special-category coverage warnings.
    #[default]
    Provider,
    /// Require independently audited coverage for every event category.
    Audited,
}
impl EventCheckPolicy {
    /// User-facing execution policy, shared by App and CLI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Provider => "采用供应商资料，异常停止",
            Self::Audited => "要求全类别覆盖核验",
        }
    }
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
    /// Typed event terms for ordinary A-share holders.
    #[serde(default)]
    pub terms: crate::actions::EventTerms,
    /// Evidence URL(s), never used as executable instructions.
    pub source: String,
}
impl CorporateAction {
    /// Integer shareholder allocation plus explicit compensation.
    pub fn share_entitlement(&self, shares: u64) -> Result<(u64, f64)> {
        match &self.terms {
            crate::actions::EventTerms::Distribution {
                allocation: Some(allocation),
            }
            | crate::actions::EventTerms::Differential { allocation, .. } => allocation
                .allocate(shares, self.shares_per_share)
                .map_err(|e| invalid(&e)),
            _ => Ok((entitlement_shares(shares, self.shares_per_share)?, 0.)),
        }
    }

    /// Exchange reference is separate from actual holder rights.
    pub fn reference_price(&self, previous: f64) -> Result<f64> {
        match self.terms.reference() {
            Some(r) => r.price(previous).map_err(|e| invalid(&e)),
            None => {
                let p = (previous - self.cash_per_share) / (1. + self.shares_per_share);
                if positive(p) {
                    Ok(p)
                } else {
                    Err(invalid("invalid distribution reference"))
                }
            }
        }
    }
    /// Validate implemented adjustment terms and chronology, without requiring settlement
    /// dates for historical factor inputs that never enter the execution account.
    pub fn validate_adjustment_terms(&self) -> Result<()> {
        crate::pool::validate_instrument(&self.instrument).map_err(|e| invalid(&e))?;
        self.terms.validate(self.ex_date).map_err(|e| invalid(&e))?;
        if !matches!(
            self.terms,
            crate::actions::EventTerms::Distribution { .. }
                | crate::actions::EventTerms::Differential { .. }
        ) && (self.cash_per_share != 0. || self.shares_per_share != 0.)
        {
            return Err(invalid(
                "special event rights must use typed terms, not duplicate common distribution fields",
            ));
        }
        if let crate::actions::EventTerms::RightsIssue {
            subscription_start, ..
        } = &self.terms
            && *subscription_start <= self.record_date
        {
            return Err(invalid("subscription must follow entitlement registration"));
        }
        if self.id.is_empty()
            || self.source.is_empty()
            || self.announced > self.record_date
            || self.record_date >= self.ex_date
            || !self.cash_per_share.is_finite()
            || self.cash_per_share < 0.0
            || !self.shares_per_share.is_finite()
            || !(0.0..=100.0).contains(&self.shares_per_share)
            || (self.cash_per_share + self.shares_per_share == 0.0
                && matches!(self.terms, crate::actions::EventTerms::Distribution { .. }))
        {
            return Err(invalid(
                "invalid corporate-action identity, chronology or distribution",
            ));
        }
        Ok(())
    }
    /// Validate actual account rights, including verified payment/listing dates.
    pub fn validate(&self) -> Result<()> {
        self.validate_adjustment_terms()?;
        if matches!(self.terms, crate::actions::EventTerms::Historical { .. }) {
            return Err(invalid(
                "historical reference lacks actual account settlement terms",
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
    fractional_paid: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpecialRight {
    action: CorporateAction,
    shares: u64,
    applied: bool,
    submitted: bool,
    allocated: bool,
    subscribed: u64,
    new_shares: u64,
    cash_due: f64,
    refund_cash: f64,
    fractional_cash: f64,
    paid: bool,
    listed: bool,
    refunded: bool,
    fraction_paid: bool,
}
/// Persistent portfolio ledger; monetary values are in CNY.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareLedger {
    /// Worker-local identity directory for dated fees; serialized artifacts freeze it separately.
    #[serde(skip)]
    pub source_map: crate::source_map::SourceMap,
    /// Spendable cash, excluding dividend receivables.
    pub cash: f64,
    /// Integer listed positions.
    pub positions: BTreeMap<String, Position>,
    /// Explicit fixed research cash-dividend withholding (0 means gross).
    /// This does not model bonus-share income tax or personal FIFO holding-period tax.
    pub dividend_withholding_rate: f64,
    rights: Vec<Right>,
    special: Vec<SpecialRight>,
    nontradeable: BTreeSet<String>,
    /// Frozen cash-limited subscription and passive cash-option policy.
    pub action_policy: crate::actions::ActionPolicy,
    /// Actual fees incurred by corporate subscriptions today.
    pub corporate_fees_today: f64,
    recorded: BTreeSet<String>,
    date: Option<Date>,
    closed: bool,
}
impl ShareLedger {
    /// Securities with actual positions or pending entitlements; used by execution-only checks.
    pub fn required_instruments(&self) -> BTreeSet<String> {
        self.positions
            .keys()
            .cloned()
            .chain(
                self.rights
                    .iter()
                    .filter(|r| {
                        !r.cash_paid
                            || !r.stock_listed
                            || (r.action.terms.allocation().is_some_and(|a| {
                                matches!(a, crate::actions::Allocation::CashInLieu { .. })
                            }) && !r.fractional_paid)
                    })
                    .map(|r| r.action.instrument.clone()),
            )
            .chain(
                self.special
                    .iter()
                    .filter(|r| match &r.action.terms {
                        crate::actions::EventTerms::RightsIssue {
                            subscription_end, ..
                        } => {
                            if r.submitted {
                                !r.listed
                                    || !r.refunded
                                    || (r.fractional_cash > 0. && !r.fraction_paid)
                            } else {
                                self.date.is_none_or(|d| d <= *subscription_end)
                            }
                        }
                        crate::actions::EventTerms::Conversion { .. } => {
                            !r.applied
                                || !r.paid
                                || !r.listed
                                || (r.fractional_cash > 0. && !r.fraction_paid)
                        }
                        crate::actions::EventTerms::Delisting { .. } => !r.paid,
                        _ => false,
                    })
                    .map(|r| r.action.instrument.clone()),
            )
            .collect()
    }
    fn exchange_symbol(&self, id: &str) -> Result<String> {
        Ok(self
            .source_map
            .exchange_symbol(
                id,
                self.date.ok_or_else(|| invalid("trading day not opened"))?,
            )
            .map_err(|e| invalid(&e))?
            .into())
    }

    /// Process dated mandatory events and first-day cash-limited subscriptions atomically.
    fn special_day(&mut self, date: Date) -> Result<()> {
        use crate::actions::{EventTerms, decimal};
        self.corporate_fees_today = 0.;
        self.settle_special(date)?;
        let mut demands = Vec::new();
        for (i, r) in self.special.iter().enumerate() {
            if let EventTerms::RightsIssue {
                subscription_start,
                subscription_end,
                price,
                ratio,
                unit,
                fee_per_share,
                ..
            } = &r.action.terms
                && !r.submitted
                && date >= *subscription_start
            {
                let max = (r.shares as f64 * decimal(ratio).map_err(|e| invalid(&e))?).floor()
                    as u64
                    / unit
                    * unit;
                let cost = decimal(price).map_err(|e| invalid(&e))?
                    + decimal(fee_per_share).map_err(|e| invalid(&e))?;
                demands.push((
                    i,
                    if self.action_policy.subscribe_with_available_cash && date <= *subscription_end
                    {
                        max
                    } else {
                        0
                    },
                    *unit,
                    cost,
                ));
            }
        }
        demands.sort_by(|a, b| {
            self.special[a.0]
                .action
                .id
                .cmp(&self.special[b.0].action.id)
        });
        let needed: f64 = demands.iter().map(|(_, n, _, c)| *n as f64 * c).sum();
        let available = self.cash.max(0.);
        let fraction = if needed > 0. {
            (available / needed).min(1.)
        } else {
            0.
        };
        let mut quantities: Vec<u64> = demands
            .iter()
            .map(|(_, n, unit, _)| ((*n as f64 * fraction).floor() as u64 / unit) * unit)
            .collect();
        let mut remaining = available
            - demands
                .iter()
                .zip(&quantities)
                .map(|((_, _, _, c), n)| *n as f64 * c)
                .sum::<f64>();
        // At most one residual unit per demand after proportional floors.
        for ((_, max, unit, cost), n) in demands.iter().zip(&mut quantities) {
            if *n + unit <= *max && *unit as f64 * cost <= remaining + 1e-8 {
                *n += unit;
                remaining -= *unit as f64 * cost;
            }
        }
        for ((i, _, _, _), n) in demands.into_iter().zip(quantities) {
            let r = &mut self.special[i];
            r.submitted = true;
            r.subscribed = n;
            if let EventTerms::RightsIssue {
                price,
                fee_per_share,
                allotment_ratio,
                allocation,
                ..
            } = &r.action.terms
            {
                let price = decimal(price).map_err(|e| invalid(&e))?;
                let fee = n as f64 * decimal(fee_per_share).map_err(|e| invalid(&e))?;
                let (allocated, compensation) = allocation
                    .allocate(n, decimal(allotment_ratio).map_err(|e| invalid(&e))?)
                    .map_err(|e| invalid(&e))?;
                r.new_shares = allocated;
                r.fractional_cash = compensation;
                r.refund_cash =
                    if matches!(allocation, crate::actions::Allocation::CashInLieu { .. }) {
                        (n as f64 - n as f64 * decimal(allotment_ratio).map_err(|e| invalid(&e))?)
                            * price
                    } else {
                        (n - allocated) as f64 * price
                    };
                self.cash -= n as f64 * price + fee;
                self.corporate_fees_today += fee;
            }
        }
        self.settle_special(date)?;
        if !self.cash.is_finite() || self.cash < -1e-7 {
            return Err(invalid("invalid corporate cash balance"));
        }
        Ok(())
    }
    fn settle_special(&mut self, date: Date) -> Result<()> {
        use crate::actions::{EventTerms, decimal};
        for r in &mut self.special {
            match &r.action.terms {
                EventTerms::RightsIssue {
                    price,
                    refund_date,
                    listing_date,
                    allotment_date,
                    allocation,
                    ..
                } => {
                    if r.submitted && date >= *allotment_date {
                        r.allocated = true;
                    }
                    if r.allocated && !r.refunded && date >= *refund_date {
                        self.cash += r.refund_cash;
                        r.refunded = true;
                    }
                    if r.allocated && !r.listed && date >= *listing_date {
                        let p = self
                            .positions
                            .entry(r.action.instrument.clone())
                            .or_default();
                        p.shares = p
                            .shares
                            .checked_add(r.new_shares)
                            .ok_or_else(|| invalid("share overflow"))?;
                        p.sellable = p
                            .sellable
                            .checked_add(r.new_shares)
                            .ok_or_else(|| invalid("share overflow"))?;
                        r.listed = true;
                    }
                    if let crate::actions::Allocation::CashInLieu { pay_date, .. } = allocation
                        && r.submitted
                        && !r.fraction_paid
                        && date >= *pay_date
                    {
                        self.cash += r.fractional_cash;
                        r.fraction_paid = true;
                    }
                    let _ = price;
                }
                EventTerms::Conversion {
                    successor,
                    ratio,
                    cash,
                    pay_date,
                    listing_date,
                    ordinary_holder,
                    retain_original,
                    allocation,
                    ..
                } => {
                    if !ordinary_holder {
                        r.applied = true;
                        r.listed = true;
                        r.paid = true;
                        continue;
                    }
                    if !r.applied && date >= r.action.ex_date {
                        if !retain_original {
                            if let Some(p) = self.positions.get_mut(&r.action.instrument) {
                                if p.shares < r.shares {
                                    return Err(invalid(
                                        "conversion ownership changed before effective day; require verified entitlement/suspension",
                                    ));
                                }
                                p.shares -= r.shares;
                                p.sellable = p.sellable.min(p.shares);
                                if p.shares == 0 {
                                    self.positions.remove(&r.action.instrument);
                                }
                            } else if r.shares > 0 {
                                return Err(invalid("missing conversion holding"));
                            }
                        }
                        let (n, compensation) = allocation
                            .allocate(r.shares, decimal(ratio).map_err(|e| invalid(&e))?)
                            .map_err(|e| invalid(&e))?;
                        r.new_shares = n;
                        r.fractional_cash = compensation;
                        r.cash_due = r.shares as f64 * decimal(cash).map_err(|e| invalid(&e))?;
                        r.applied = true;
                        if !retain_original {
                            self.nontradeable.insert(r.action.instrument.clone());
                        }
                    }
                    if r.applied && !r.paid && pay_date.is_some_and(|d| date >= d) {
                        self.cash += r.cash_due;
                        r.paid = true;
                    }
                    if r.applied && !r.listed && listing_date.is_some_and(|d| date >= d) {
                        if let Some(id) = successor {
                            let p = self.positions.entry(id.clone()).or_default();
                            p.shares = p
                                .shares
                                .checked_add(r.new_shares)
                                .ok_or_else(|| invalid("share overflow"))?;
                            p.sellable = p
                                .sellable
                                .checked_add(r.new_shares)
                                .ok_or_else(|| invalid("share overflow"))?;
                            self.nontradeable.remove(id);
                        }
                        r.listed = true;
                    }
                    if let crate::actions::Allocation::CashInLieu { pay_date, .. } = allocation
                        && r.applied
                        && !r.fraction_paid
                        && date >= *pay_date
                    {
                        self.cash += r.fractional_cash;
                        r.fraction_paid = true;
                    }
                }
                EventTerms::Delisting {
                    settlement_cash,
                    settlement_date,
                    ..
                } => {
                    if !r.applied && date >= r.action.ex_date {
                        let removed = self
                            .positions
                            .remove(&r.action.instrument)
                            .map_or(0, |p| p.shares);
                        if removed != r.shares {
                            return Err(invalid(
                                "delisting entitlement changed; require verified record/effective dates",
                            ));
                        }
                        self.nontradeable.insert(r.action.instrument.clone());
                        r.applied = true;
                    }
                    if r.applied && !r.paid && settlement_date.is_some_and(|d| date >= d) {
                        self.cash += r.shares as f64
                            * decimal(
                                settlement_cash
                                    .as_ref()
                                    .ok_or_else(|| invalid("missing settlement amount"))?,
                            )
                            .map_err(|e| invalid(&e))?;
                        r.paid = true;
                    }
                }
                _ => return Err(invalid("ordinary distribution entered special ledger")),
            }
        }
        Ok(())
    }
    /// Nontradeable predecessor shares cannot be bought/sold using stale quotes.
    pub fn can_trade(&self, id: &str) -> bool {
        !self.nontradeable.contains(id)
    }
    fn special_nav(&self, marks: &BTreeMap<String, f64>) -> Result<f64> {
        use crate::actions::{EventTerms, decimal};
        let date = self.date.ok_or_else(|| invalid("missing ledger date"))?;
        let price = |id: &str| {
            marks
                .get(id)
                .copied()
                .filter(|p| positive(*p))
                .ok_or_else(|| invalid("missing successor/claim price"))
        };
        let mut total = 0.;
        for r in &self.special {
            match &r.action.terms {
                EventTerms::RightsIssue { price: p, .. } if r.submitted => {
                    if !r.allocated {
                        total += r.subscribed as f64 * decimal(p).map_err(|e| invalid(&e))?;
                        continue;
                    }

                    if !r.refunded {
                        total += r.refund_cash;
                    }
                    if !r.listed {
                        total += r.new_shares as f64
                            * if date < r.action.ex_date {
                                decimal(p).map_err(|e| invalid(&e))?
                            } else {
                                price(&r.action.instrument)?
                            };
                    }
                }
                EventTerms::Conversion {
                    successor,
                    claim_valuation,
                    ..
                } if r.applied => {
                    if !r.paid {
                        total += r.cash_due;
                    }
                    if !r.listed && r.new_shares > 0 {
                        let id = successor
                            .as_ref()
                            .ok_or_else(|| invalid("missing successor"))?;
                        if let Ok(mark) = price(id) {
                            total += r.new_shares as f64 * mark;
                        } else if let Some(v) = claim_valuation
                            && v.date <= date
                            && date <= v.valid_until
                        {
                            total += r.shares as f64
                                * decimal(&v.per_old_share).map_err(|e| invalid(&e))?;
                        } else {
                            return Err(invalid("missing successor/claim valuation"));
                        }
                    }
                }
                EventTerms::Delisting { valuations, .. } if r.applied && !r.paid => {
                    let v = valuations
                        .iter()
                        .rev()
                        .find(|v| v.date <= date && date <= v.valid_until)
                        .ok_or_else(|| invalid("unvalued delisted holding"))?;
                    total +=
                        r.shares as f64 * decimal(&v.per_old_share).map_err(|e| invalid(&e))?;
                }
                _ => {}
            }
            if !r.fraction_paid {
                total += r.fractional_cash;
            }
        }
        Ok(total)
    }

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
            source_map: Default::default(),
            cash,
            positions: BTreeMap::new(),
            dividend_withholding_rate,
            rights: Vec::new(),
            special: Vec::new(),
            nontradeable: BTreeSet::new(),
            action_policy: Default::default(),
            corporate_fees_today: 0.,
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
            if !right.fractional_paid
                && let Some(crate::actions::Allocation::CashInLieu { pay_date, .. }) =
                    right.action.terms.allocation()
                && date >= *pay_date
            {
                next.cash += right.action.share_entitlement(right.shares)?.1;
                right.fractional_paid = true;
            }
            if !right.cash_paid && right.action.pay_date.is_some_and(|d| date >= d) {
                next.cash += cash_entitlement(
                    right.shares,
                    right.action.cash_per_share,
                    next.dividend_withholding_rate,
                );
                right.cash_paid = true;
            }
            if !right.stock_listed && right.action.listing_date.is_some_and(|d| date >= d) {
                let quantity = right.action.share_entitlement(right.shares)?.0;
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
        next.special_day(date)?;
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
                if !matches!(
                    action.terms,
                    crate::actions::EventTerms::Distribution { .. }
                        | crate::actions::EventTerms::Differential { .. }
                ) {
                    next.special.push(SpecialRight {
                        action: action.clone(),
                        shares,
                        applied: false,
                        submitted: false,
                        allocated: false,
                        subscribed: 0,
                        new_shares: 0,
                        cash_due: 0.,
                        refund_cash: 0.,
                        fractional_cash: 0.,
                        paid: false,
                        listed: false,
                        refunded: false,
                        fraction_paid: false,
                    });
                    continue;
                }
                action.share_entitlement(shares)?;
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
                    fractional_paid: false,
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
            if !r.fractional_paid {
                total += r.action.share_entitlement(r.shares)?.1;
            }
            if !r.stock_listed {
                total +=
                    r.action.share_entitlement(r.shares)?.0 as f64 * price(&r.action.instrument)?;
            }
        }
        total += self.special_nav(marks)?;
        if !total.is_finite() || total < 0. {
            return Err(invalid("negative or nonfinite NAV"));
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
        let code = self.exchange_symbol(id)?;
        let board = rules.board_for(&code)?;
        let budget = rules
            .affordable_buy_notional(&code, self.cash.max(0.0))?
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
            || !self.can_trade(id)
            || quantity > 1_000_000_000_000
        {
            return Err(invalid("invalid fill or closed trading day"));
        }
        let code = self.exchange_symbol(id)?;
        let board = rules.board_for(&code)?;
        if board.sell_delay_trading_days != 1 {
            return Err(invalid(
                "raw ledger currently requires the A-share T+1 preset",
            ));
        }
        if (price / board.tick_cny - (price / board.tick_cny).round()).abs() > 1e-6 {
            return Err(invalid("fill price violates raw tick size"));
        }
        let value = quantity as f64 * price;
        let fees = rules.fees(&code, side, value)?;
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
            terms: Default::default(),
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

#[cfg(test)]
mod special_tests {
    use super::*;
    use crate::actions::*;
    fn d(n: u32) -> Date {
        Date::from_ymd_opt(2024, 6, n).unwrap()
    }
    fn reference() -> Reference {
        Reference::Unchanged {
            evidence: "implementation fixture".into(),
        }
    }
    fn a(id: &str, terms: EventTerms) -> CorporateAction {
        CorporateAction {
            id: format!("{id}:2024-06-04"),
            instrument: id.into(),
            announced: d(1),
            record_date: d(3),
            ex_date: d(4),
            pay_date: None,
            listing_date: None,
            cash_per_share: 0.,
            shares_per_share: 0.,
            terms,
            source: "fixture implementation".into(),
        }
    }
    fn held(cash: f64) -> ShareLedger {
        let mut l = ShareLedger::new(cash, 0.).unwrap();
        l.positions.insert(
            "SH600000".into(),
            Position {
                shares: 100,
                sellable: 100,
                mark: 10.,
            },
        );
        l
    }
    fn record(l: &mut ShareLedger, events: &[CorporateAction]) {
        l.begin_day(d(3)).unwrap();
        l.end_day(events).unwrap();
    }
    fn rights(id: &str) -> CorporateAction {
        a(
            id,
            EventTerms::RightsIssue {
                subscription_start: d(4),
                subscription_end: d(5),
                price: "5".into(),
                ratio: "0.5".into(),
                unit: 1,
                fee_per_share: "0.5".into(),
                allotment_ratio: "0.5".into(),
                allotment_date: d(4),
                refund_date: d(6),
                listing_date: d(7),
                reference: Reference::Formula {
                    cash: "0".into(),
                    share_multiplier: "1.5".into(),
                    subscription_cash: "2.5".into(),
                    evidence: "fixture".into(),
                },
                allocation: Allocation::Exact,
            },
        )
    }
    #[test]
    fn rights_use_only_cash_keep_refund_receivable_and_never_charge_trade_fees() {
        let mut l = held(125.);
        record(&mut l, &[rights("SH600000")]);
        l.begin_day(d(4)).unwrap();
        assert_eq!(l.special[0].subscribed, 22);
        assert_eq!(l.special[0].new_shares, 11);
        assert_eq!(l.cash, 4.);
        assert_eq!(l.corporate_fees_today, 11.);
        let marks = BTreeMap::from([("SH600000".into(), 8.)]);
        assert_eq!(l.nav(&marks).unwrap(), 947.);
        l.end_day(&[]).unwrap();
        l.begin_day(d(6)).unwrap();
        assert_eq!(l.cash, 59.);
        l.end_day(&[]).unwrap();
        l.begin_day(d(7)).unwrap();
        assert_eq!(l.positions["SH600000"].shares, 111);
        assert_eq!(l.cash, 59.);
    }
    #[test]
    fn concurrent_rights_allocate_proportionally_and_waiver_is_explicit() {
        let mut l = held(110.);
        l.positions.insert(
            "SH600001".into(),
            Position {
                shares: 100,
                sellable: 100,
                mark: 10.,
            },
        );
        record(&mut l, &[rights("SH600001"), rights("SH600000")]);
        l.begin_day(d(4)).unwrap();
        assert_eq!(
            l.special.iter().map(|r| r.subscribed).collect::<Vec<_>>(),
            [10, 10]
        );
        assert_eq!(l.cash, 0.);
        let mut l = held(110.);
        l.action_policy.subscribe_with_available_cash = false;
        record(&mut l, &[rights("SH600000")]);
        l.begin_day(d(4)).unwrap();
        assert_eq!(l.cash, 110.);
        assert_eq!(l.special[0].subscribed, 0);
    }
    fn conversion(
        category: ConversionKind,
        successor: &str,
        ratio: &str,
        ordinary: bool,
    ) -> CorporateAction {
        a(
            "SH600000",
            EventTerms::Conversion {
                category,
                claim_valuation: None,
                successor: Some(successor.into()),
                ratio: ratio.into(),
                ordinary_holder: ordinary,
                retain_original: false,
                cash: "1".into(),
                pay_date: Some(d(6)),
                listing_date: Some(d(7)),
                reference: reference(),
                allocation: Allocation::Exact,
            },
        )
    }
    #[test]
    fn merger_delayed_successor_and_cash_claim_survive_between_dates() {
        let mut l = held(100.);
        record(
            &mut l,
            &[conversion(ConversionKind::Merger, "SH600001", "0.5", true)],
        );
        l.begin_day(d(4)).unwrap();
        assert!(l.positions.is_empty());
        assert!(!l.can_trade("SH600000"));
        let marks = BTreeMap::from([("SH600001".into(), 18.)]);
        assert_eq!(l.nav(&marks).unwrap(), 1100.);
        l.end_day(&[]).unwrap();
        l.begin_day(d(6)).unwrap();
        assert_eq!(l.cash, 200.);
        l.end_day(&[]).unwrap();
        l.begin_day(d(7)).unwrap();
        assert_eq!(l.positions["SH600001"].sellable, 50);
        assert_eq!(l.nav(&marks).unwrap(), 1100.);
    }
    #[test]
    fn reorganization_excludes_nonholder_shares_and_reduction_is_real_integer() {
        let mut l = held(100.);
        record(
            &mut l,
            &[conversion(
                ConversionKind::Reorganization,
                "SH600000",
                "1.361",
                false,
            )],
        );
        l.begin_day(d(4)).unwrap();
        assert_eq!(l.positions["SH600000"].shares, 100);
        assert_eq!(l.cash, 100.);
        let mut l = held(100.);
        record(
            &mut l,
            &[conversion(
                ConversionKind::Reduction,
                "SH600000",
                "0.5",
                true,
            )],
        );
        l.begin_day(d(4)).unwrap();
        assert!(l.positions.is_empty());
        l.end_day(&[]).unwrap();
        l.begin_day(d(7)).unwrap();
        assert_eq!(l.positions["SH600000"].shares, 50);
        assert!(l.can_trade("SH600000"));
    }
    #[test]
    fn delisting_keeps_claim_until_actual_settlement_and_never_sells_at_last_mark() {
        let a = a(
            "SH600000",
            EventTerms::Delisting {
                valuations: vec![Valuation {
                    date: d(4),
                    valid_until: d(7),
                    per_old_share: "3".into(),
                    evidence: "verified valuation".into(),
                }],
                settlement_cash: Some("2".into()),
                settlement_date: Some(d(7)),
                reference: reference(),
            },
        );
        let mut l = held(100.);
        record(&mut l, &[a]);
        l.begin_day(d(4)).unwrap();
        assert!(l.positions.is_empty());
        assert_eq!(l.nav(&BTreeMap::new()).unwrap(), 400.);
        l.end_day(&[]).unwrap();
        l.begin_day(d(7)).unwrap();
        assert_eq!(l.cash, 300.);
        assert_eq!(l.nav(&BTreeMap::new()).unwrap(), 300.);
    }
    #[test]
    fn fractional_rule_requires_evidence_and_bad_conversion_does_not_mutate_account() {
        assert!(Allocation::Exact.allocate(101, 0.5).is_err());
        assert_eq!(
            Allocation::CashInLieu {
                price: "20".into(),
                pay_date: d(7),
                evidence: "registrar".into()
            }
            .allocate(101, 0.5)
            .unwrap(),
            (50, 10.)
        );
        let mut l = held(100.);
        l.positions.get_mut("SH600000").unwrap().shares = 101;
        record(
            &mut l,
            &[conversion(ConversionKind::Merger, "SH600001", "0.5", true)],
        );
        let before = serde_json::to_value(&l).unwrap();
        assert!(l.begin_day(d(4)).is_err());
        assert_eq!(serde_json::to_value(&l).unwrap(), before);
    }
    #[test]
    fn share_reform_additional_consideration_does_not_lock_existing_listed_shares() {
        let mut event = conversion(ConversionKind::ShareReform, "SH600000", "0.5", true);
        if let EventTerms::Conversion {
            retain_original,
            cash,
            ..
        } = &mut event.terms
        {
            *retain_original = true;
            *cash = "0".into();
        }
        let mut l = held(100.);
        record(&mut l, &[event]);
        l.begin_day(d(4)).unwrap();
        assert_eq!(l.positions["SH600000"].shares, 100);
        assert_eq!(l.positions["SH600000"].sellable, 100);
        assert!(l.can_trade("SH600000"));
        l.end_day(&[]).unwrap();
        l.begin_day(d(7)).unwrap();
        assert_eq!(l.positions["SH600000"].shares, 150);
    }
    #[test]
    fn historical_reference_and_expired_valuation_cannot_enter_current_account() {
        let event = a(
            "SH600000",
            EventTerms::Historical {
                category: HistoricalKind::RightsIssue,
                reference: reference(),
            },
        );
        assert!(event.validate_adjustment_terms().is_ok());
        assert!(event.validate().is_err());
        let event = a(
            "SH600000",
            EventTerms::Delisting {
                valuations: vec![Valuation {
                    date: d(4),
                    valid_until: d(5),
                    per_old_share: "3".into(),
                    evidence: "source".into(),
                }],
                settlement_cash: None,
                settlement_date: None,
                reference: reference(),
            },
        );
        let mut l = held(100.);
        record(&mut l, &[event]);
        l.begin_day(d(4)).unwrap();
        l.end_day(&[]).unwrap();
        l.begin_day(d(6)).unwrap();
        assert!(l.nav(&BTreeMap::new()).is_err());
    }
    #[test]
    fn allotment_is_effective_only_on_its_actual_date_and_explicit_zero_is_supported() {
        let mut event = rights("SH600000");
        if let EventTerms::RightsIssue { allotment_date, .. } = &mut event.terms {
            *allotment_date = d(6);
        }
        let mut l = held(125.);
        record(&mut l, &[event]);
        l.begin_day(d(4)).unwrap();
        assert!(!l.special[0].allocated);
        assert_eq!(
            l.nav(&BTreeMap::from([("SH600000".into(), 8.)])).unwrap(),
            914.
        );
        l.end_day(&[]).unwrap();
        l.begin_day(d(6)).unwrap();
        assert!(l.special[0].allocated);
        let mut l = held(100.);
        l.positions.clear();
        l.cash = 0.;
        l.begin_day(d(4)).unwrap();
        assert_eq!(l.nav(&BTreeMap::new()).unwrap(), 0.);
    }
    #[test]
    fn differential_holder_cash_and_reference_cash_are_separate() {
        let mut event = a(
            "SH600000",
            EventTerms::Differential {
                reference: Reference::Formula {
                    cash: "0.8".into(),
                    share_multiplier: "1".into(),
                    subscription_cash: "0".into(),
                    evidence: "exchange".into(),
                },
                allocation: Allocation::Exact,
            },
        );
        event.cash_per_share = 1.;
        event.pay_date = Some(d(6));
        assert!((event.reference_price(10.).unwrap() - 9.2).abs() < 1e-10);
        let mut l = held(100.);
        record(&mut l, &[event]);
        l.begin_day(d(4)).unwrap();
        assert_eq!(l.cash, 100.);
        assert!((l.nav(&BTreeMap::from([("SH600000".into(), 9.2)])).unwrap() - 1120.).abs() < 1e-8);
        l.end_day(&[]).unwrap();
        l.begin_day(d(6)).unwrap();
        assert_eq!(l.cash, 200.);
    }
}

#[cfg(test)]
mod ordinary_fraction_tests {
    use super::*;
    #[test]
    fn ordinary_bonus_fraction_uses_evidenced_cash_and_actual_payment_date() {
        let ex = "2024-06-04".parse().unwrap();
        let action = CorporateAction {
            id: "SH600000:2024-06-04".into(),
            instrument: "SH600000".into(),
            announced: "2024-06-01".parse().unwrap(),
            record_date: "2024-06-03".parse().unwrap(),
            ex_date: ex,
            pay_date: None,
            listing_date: Some(ex),
            cash_per_share: 0.,
            shares_per_share: 0.1,
            terms: crate::actions::EventTerms::Distribution {
                allocation: Some(crate::actions::Allocation::CashInLieu {
                    price: "10".into(),
                    pay_date: "2024-06-06".parse().unwrap(),
                    evidence: "registrar fixture ordinary shares".into(),
                }),
            },
            source: "fixture".into(),
        };
        let mut ledger = ShareLedger::new(1000., 0.).unwrap();
        ledger.positions.insert(
            "SH600000".into(),
            Position {
                shares: 15,
                sellable: 15,
                mark: 10.,
            },
        );
        ledger.begin_day(action.record_date).unwrap();
        ledger.end_day(&[action]).unwrap();
        ledger.begin_day(ex).unwrap();
        assert_eq!(ledger.positions["SH600000"].shares, 16);
        assert_eq!(ledger.cash, 1000.);
        ledger.end_day(&[]).unwrap();
        ledger.begin_day("2024-06-06".parse().unwrap()).unwrap();
        assert_eq!(ledger.cash, 1005.);
    }
}
