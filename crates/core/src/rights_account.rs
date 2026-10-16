//! Deterministic rights-only state machine. Ordinary fills remain the engine's responsibility.
use crate::rights::{Attempt, InsufficientCash, Participation, Policy, Terms};
use chrono::NaiveDate as Date;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

type Result<T> = std::result::Result<T, String>;
fn d(s: &str) -> Result<Decimal> {
    let v = s.parse::<Decimal>().map_err(|_| "rights_decimal_invalid")?;
    if v < Decimal::ZERO {
        return Err("rights_decimal_negative".into());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rights::Fact;
    fn date(s: &str) -> Date {
        s.parse().unwrap()
    }
    fn fixture() -> (
        Ledger,
        Decimal,
        BTreeMap<String, Decimal>,
        BTreeMap<String, Decimal>,
    ) {
        let record = date("2022-01-18");
        let ex = date("2022-01-27");
        let terms = Terms {
            announced: Fact::Confirmed {
                value: date("2022-01-14"),
            },
            subscription_start: Fact::Confirmed {
                value: date("2022-01-19"),
            },
            subscription_end: Fact::Confirmed {
                value: date("2022-01-25"),
            },
            price: Fact::Confirmed {
                value: "14.43".into(),
            },
            ratio: Fact::Confirmed {
                value: "0.15".into(),
            },
            unit: Fact::Confirmed { value: 1 },
            fee_per_share: Fact::Confirmed { value: "0".into() },
            allotment_ratio: Fact::Confirmed { value: "1".into() },
            allotment_date: Fact::Confirmed { value: ex },
            refund_date: Fact::NotApplicable {
                reason: "fixture successful full allocation without refund".into(),
            },
            listing_date: Fact::Confirmed {
                value: date("2022-02-15"),
            },
            conditional_refund: Fact::Unknown,
        };
        let event = Event {
            id: format!("SH600030:{ex}"),
            instrument: "SH600030".into(),
            record_date: record,
            ex_date: ex,
            terms,
            sessions: vec![
                record,
                date("2022-01-19"),
                date("2022-01-25"),
                ex,
                date("2022-02-15"),
            ],
        };
        let l = Ledger {
            policy: Policy {
                participation: Participation::Participate,
                ..Default::default()
            },
            events: vec![event],
            ..Default::default()
        };
        let p = BTreeMap::from([("SH600030".into(), Decimal::from(800))]);
        (l, Decimal::from(10000), p.clone(), p)
    }
    #[test]
    fn registered_rights_survive_sale_and_list_without_market_fill() {
        let (mut l, mut cash, mut p, mut a) = fixture();
        l.validate().unwrap();
        l.close(date("2022-01-18"), &p).unwrap();
        p.clear();
        a.clear();
        l.open(date("2022-01-19"), &mut cash, Decimal::ZERO, &mut p, &mut a)
            .unwrap();
        assert_eq!(cash, d("8268.40").unwrap());
        assert!(p.is_empty());
        assert_eq!(l.value(&BTreeMap::new()).unwrap(), d("1731.60").unwrap());
        l.open(date("2022-01-27"), &mut cash, Decimal::ZERO, &mut p, &mut a)
            .unwrap();
        assert!(l.value(&BTreeMap::new()).is_err());
        let prices = BTreeMap::from([("SH600030".into(), Decimal::from(20))]);
        assert_eq!(l.value(&prices).unwrap(), Decimal::from(2400));
        assert!(a.is_empty());
        l.open(date("2022-02-15"), &mut cash, Decimal::ZERO, &mut p, &mut a)
            .unwrap();
        assert_eq!(a["SH600030"], Decimal::from(120));
        assert_eq!(p, a);
        assert_eq!(l.value(&prices).unwrap(), Decimal::ZERO);
        assert!(
            l.open(date("2022-02-15"), &mut cash, Decimal::ZERO, &mut p, &mut a)
                .is_err()
        );
    }
    #[test]
    fn subscription_date_allocation_is_valued_without_waiting_for_next_session() {
        let (mut l, mut cash, mut p, mut a) = fixture();
        l.events[0].terms.allotment_date = Fact::Confirmed {
            value: date("2022-01-19"),
        };
        l.close(date("2022-01-18"), &p).unwrap();
        l.open(date("2022-01-19"), &mut cash, Decimal::ZERO, &mut p, &mut a)
            .unwrap();
        assert_eq!(
            l.states.values().next().unwrap().allocated,
            Decimal::from(120)
        );
        assert_eq!(l.value(&BTreeMap::new()).unwrap(), d("1731.60").unwrap());
        assert!(!l.states.values().next().unwrap().listed);
    }
    #[test]
    fn registered_closing_fills_and_unknown_terms_are_not_ignored() {
        let (mut l, mut cash, mut p, mut a) = fixture();
        p.insert("SH600030".into(), Decimal::from(1000));
        l.close(date("2022-01-18"), &p).unwrap();
        assert_eq!(
            l.states.values().next().unwrap().registered,
            Decimal::from(1000)
        );
        l.events[0].terms.fee_per_share = Fact::Unknown;
        assert!(
            l.open(date("2022-01-19"), &mut cash, Decimal::ZERO, &mut p, &mut a)
                .unwrap_err()
                .contains("fee_per_share")
        );
        assert_eq!(cash, Decimal::from(10000));
    }
    #[test]
    fn abandon_needs_no_subscription_terms_and_keeps_original_shares() {
        let (mut l, mut cash, mut p, mut a) = fixture();
        l.policy.participation = Participation::Abandon;
        l.events[0].terms = Terms::default();
        l.close(date("2022-01-18"), &p).unwrap();
        l.open(date("2022-01-19"), &mut cash, Decimal::ZERO, &mut p, &mut a)
            .unwrap();
        assert_eq!(cash, Decimal::from(10000));
        assert_eq!(p["SH600030"], Decimal::from(800));
        assert_eq!(l.value(&BTreeMap::new()).unwrap(), Decimal::ZERO);
    }
    #[test]
    fn last_day_reserved_cash_and_all_cash_policies() {
        for policy in [
            InsufficientCash::Partial,
            InsufficientCash::AllOrAbandon,
            InsufficientCash::Stop,
        ] {
            let (mut l, mut cash, mut p, mut a) = fixture();
            l.policy.attempt = Attempt::LastDay;
            l.policy.insufficient_cash = policy;
            l.close(date("2022-01-18"), &p).unwrap();
            l.open(date("2022-01-19"), &mut cash, Decimal::ZERO, &mut p, &mut a)
                .unwrap();
            assert!(!l.states.values().next().unwrap().attempted);
            let result = l.open(
                date("2022-01-25"),
                &mut cash,
                Decimal::from(9900),
                &mut p,
                &mut a,
            );
            match policy {
                InsufficientCash::Partial => {
                    result.unwrap();
                    assert_eq!(
                        l.states.values().next().unwrap().subscribed,
                        Decimal::from(6)
                    );
                    assert!(cash >= Decimal::from(9900));
                }
                InsufficientCash::AllOrAbandon => {
                    result.unwrap();
                    assert_eq!(cash, Decimal::from(10000));
                }
                InsufficientCash::Stop => {
                    assert!(result.unwrap_err().contains("cash_insufficient"))
                }
            }
        }
    }
    #[test]
    fn fractional_entitlement_is_a_gap_not_rounding() {
        let (mut l, mut cash, mut p, mut a) = fixture();
        p.insert("SH600030".into(), Decimal::from(801));
        l.close(date("2022-01-18"), &p).unwrap();
        assert!(
            l.open(date("2022-01-19"), &mut cash, Decimal::ZERO, &mut p, &mut a)
                .unwrap_err()
                .contains("fractional_entitlement")
        );
    }
    #[test]
    fn concurrent_events_partial_allocation_refund_and_no_listing() {
        let (mut l, _, mut p, mut a) = fixture();
        let mut second = l.events[0].clone();
        second.instrument = "SZ002142".into();
        second.id = format!("SZ002142:{}", second.ex_date);
        l.events.push(second);
        p.insert("SZ002142".into(), Decimal::from(800));
        let mut cash = d("144.30").unwrap();
        l.close(date("2022-01-18"), &p).unwrap();
        l.open(date("2022-01-19"), &mut cash, Decimal::ZERO, &mut p, &mut a)
            .unwrap();
        assert_eq!(
            l.states.values().map(|s| s.subscribed).sum::<Decimal>(),
            Decimal::from(10)
        );
        assert!(l.states.values().all(|s| s.subscribed == Decimal::from(5)));
        let (mut failed, mut cash, mut p, mut a) = fixture();
        failed.events[0].terms.allotment_ratio = Fact::Confirmed { value: "0".into() };
        failed.events[0].terms.refund_date = Fact::Confirmed {
            value: date("2022-02-15"),
        };
        failed.events[0].terms.listing_date = Fact::NotApplicable {
            reason: "fixture failed issue, no new shares".into(),
        };
        failed.validate().unwrap();
        failed.close(date("2022-01-18"), &p).unwrap();
        failed
            .open(date("2022-01-19"), &mut cash, Decimal::ZERO, &mut p, &mut a)
            .unwrap();
        let resumed: Ledger =
            serde_json::from_slice(&serde_json::to_vec(&failed).unwrap()).unwrap();
        failed = resumed;
        failed
            .open(date("2022-01-27"), &mut cash, Decimal::ZERO, &mut p, &mut a)
            .unwrap();
        assert_eq!(
            failed.value(&BTreeMap::new()).unwrap(),
            d("1731.60").unwrap()
        );
        failed
            .open(date("2022-02-15"), &mut cash, Decimal::ZERO, &mut p, &mut a)
            .unwrap();
        assert_eq!(cash, Decimal::from(10000));
        assert_eq!(failed.value(&BTreeMap::new()).unwrap(), Decimal::ZERO);
        assert_eq!(p["SH600030"], Decimal::from(800));
    }
}
/// Frozen event projection; execution dates are selected from the account calendar.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// Canonical immutable event identity.
    pub id: String,
    /// Canonical security.
    pub instrument: String,
    /// Entitlement close date.
    pub record_date: Date,
    /// Ex-rights day.
    pub ex_date: Date,
    /// Reviewed partial facts.
    pub terms: Terms,
    /// Account calendar, covering payment and settlement dates.
    pub sessions: Vec<Date>,
}
/// One account's registered entitlement, independent of later original-share sales.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// Exact registered old shares.
    pub registered: Decimal,
    /// Subscription submitted once.
    pub attempted: bool,
    /// Shares paid for.
    pub subscribed: Decimal,
    /// Cash principal, excluding separately expensed fees.
    pub principal: Decimal,
    /// Shares actually allocated.
    pub allocated: Decimal,
    /// Principal refunded.
    pub refunded: bool,
    /// Allocated shares transferred to the tradeable account.
    pub listed: bool,
}
/// Fully replayable rights state, with no native engine handles.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ledger {
    /// Frozen research choices.
    pub policy: Policy,
    /// Frozen event terms.
    pub events: Vec<Event>,
    /// Per-event account rights.
    pub states: BTreeMap<String, State>,
    /// Last processed account open; duplicate processing fails.
    pub date: Option<Date>,
    /// Captured entitlement dates, including zero holdings.
    pub closes: BTreeSet<Date>,
    /// Dated audit entries, separate from exchange fills.
    pub audit: Vec<serde_json::Value>,
}
impl Ledger {
    /// Reject malformed projections before the engine starts.
    pub fn validate(&self) -> Result<()> {
        let mut ids = BTreeSet::new();
        for e in &self.events {
            if !ids.insert(&e.id)
                || e.id != format!("{}:{}", e.instrument, e.ex_date)
                || e.record_date >= e.ex_date
                || e.sessions.windows(2).any(|w| w[0] >= w[1])
                || !e.sessions.contains(&e.record_date)
                || !e.sessions.contains(&e.ex_date)
            {
                return Err("rights_event_identity_or_calendar_invalid".into());
            }
            e.terms.validate(Some(e.record_date), e.ex_date)?;
        }
        Ok(())
    }
    /// Called only after all closing fills, never from a lazy Python close callback.
    pub fn close(&mut self, date: Date, positions: &BTreeMap<String, Decimal>) -> Result<()> {
        if !self.closes.insert(date) {
            return Err("rights_close_repeated".into());
        }
        for e in self.events.iter().filter(|e| e.record_date == date) {
            let registered = *positions.get(&e.instrument).unwrap_or(&Decimal::ZERO);
            if registered < Decimal::ZERO || registered.fract() != Decimal::ZERO {
                return Err("rights_registration_invalid".into());
            }
            if self
                .states
                .insert(
                    e.id.clone(),
                    State {
                        registered,
                        ..Default::default()
                    },
                )
                .is_some()
            {
                return Err("rights_registration_repeated".into());
            }
            self.audit.push(serde_json::json!({"date":date,"event":e.id,"kind":"register","quantity":registered.to_string()}));
        }
        Ok(())
    }
    /// Advance before ordinary orders; existing reserved cash cannot fund subscriptions.
    pub fn open(
        &mut self,
        date: Date,
        cash: &mut Decimal,
        reserved: Decimal,
        positions: &mut BTreeMap<String, Decimal>,
        available: &mut BTreeMap<String, Decimal>,
    ) -> Result<()> {
        if self.date.is_some_and(|d| d >= date) {
            return Err("rights_open_repeated_or_out_of_order".into());
        }
        self.date = Some(date);
        // Existing settlements can fund today's subscription budget.
        self.settle(date, cash, positions, available)?;
        if self.policy.participation == Participation::Abandon {
            return Ok(());
        }
        let mut due = Vec::new();
        for e in &self.events {
            let Some(s) = self.states.get(&e.id) else {
                if date > e.record_date && !self.closes.contains(&e.record_date) {
                    return Err(format!("rights_registration_close_missing: {}", e.id));
                }
                continue;
            };
            if s.registered == Decimal::ZERO || s.attempted {
                continue;
            }
            // Missing dates cannot mean the event was never due.
            let gaps = e.terms.participation_gaps();
            if !gaps.is_empty() {
                return Err(format!("rights_terms_missing: {} {}", e.id, gaps.join(",")));
            }
            let start = *e.terms.subscription_start.confirmed().unwrap();
            let end = *e.terms.subscription_end.confirmed().unwrap();
            let eligible = e
                .sessions
                .iter()
                .filter(|d| **d >= start && **d <= end)
                .copied()
                .collect::<Vec<_>>();
            let attempt = match self.policy.attempt {
                Attempt::FirstDay => eligible.first(),
                Attempt::LastDay => eligible.last(),
            }
            .ok_or("rights_payment_calendar_missing")?;
            if date < *attempt {
                continue;
            }
            if date > *attempt {
                return Err(format!("rights_subscription_session_missed: {}", e.id));
            }
            let unit = Decimal::from(*e.terms.unit.confirmed().unwrap());
            let quota = s.registered * d(e.terms.ratio.confirmed().unwrap())?;
            if quota.fract() != Decimal::ZERO || quota % unit != Decimal::ZERO {
                return Err(format!("rights_fractional_entitlement: {}", e.id));
            }
            let price = d(e.terms.price.confirmed().unwrap())?;
            let fee = d(e.terms.fee_per_share.confirmed().unwrap())?;
            due.push((e.instrument.clone(), e.id.clone(), quota, unit, price, fee));
        }
        due.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let mut budget = (*cash - reserved).max(Decimal::ZERO);
        let total = due.iter().map(|x| x.2 * (x.4 + x.5)).sum::<Decimal>();
        if total > budget && self.policy.insufficient_cash == InsufficientCash::Stop {
            return Err("rights_cash_insufficient".into());
        }
        let mut quantities = Vec::new();
        for (_, _, quota, unit, price, fee) in &due {
            let qty = if total <= budget {
                *quota
            } else {
                match self.policy.insufficient_cash {
                    InsufficientCash::Partial => (budget / total * *quota / *unit).floor() * *unit,
                    InsufficientCash::AllOrAbandon => Decimal::ZERO,
                    InsufficientCash::Stop => unreachable!(),
                }
            };
            quantities.push(qty.min(*quota));
            let _ = (price, fee);
        }
        if self.policy.insufficient_cash == InsufficientCash::AllOrAbandon {
            for (i, x) in due.iter().enumerate() {
                let cost = x.2 * (x.4 + x.5);
                if cost <= budget {
                    quantities[i] = x.2;
                    budget -= cost;
                }
            }
        } else {
            budget -= due
                .iter()
                .zip(&quantities)
                .map(|(x, q)| *q * (x.4 + x.5))
                .sum::<Decimal>();
            // Allocate each security its remaining affordable whole units in code order.
            for (i, x) in due.iter().enumerate() {
                let extra = (budget / (x.3 * (x.4 + x.5))).floor() * x.3;
                let extra = extra.min(x.2 - quantities[i]);
                quantities[i] += extra;
                budget -= extra * (x.4 + x.5);
            }
        }
        for (x, qty) in due.iter().zip(quantities) {
            let s = self.states.get_mut(&x.1).unwrap();
            s.attempted = true;
            s.subscribed = qty;
            s.principal = qty * x.4;
            let fee = qty * x.5;
            *cash -= s.principal + fee;
            self.audit.push(serde_json::json!({"date":date,"event":x.1,"kind":"subscribe","quantity":qty.to_string(),"principal":s.principal.to_string(),"fee":fee.to_string()}));
        }
        // New subscriptions may also settle on today's verified effective date.
        self.settle(date, cash, positions, available)
    }
    fn settle(
        &mut self,
        date: Date,
        cash: &mut Decimal,
        positions: &mut BTreeMap<String, Decimal>,
        available: &mut BTreeMap<String, Decimal>,
    ) -> Result<()> {
        for e in &self.events {
            let Some(s) = self.states.get_mut(&e.id) else {
                continue;
            };
            if s.subscribed == Decimal::ZERO {
                continue;
            }
            if let Some(allot) = e.terms.allotment_date.confirmed()
                && date >= *allot
            {
                s.allocated = s.subscribed
                    * d(e
                        .terms
                        .allotment_ratio
                        .confirmed()
                        .ok_or("rights_allotment_ratio_missing")?)?;
                if s.allocated.fract() != Decimal::ZERO {
                    return Err(format!("rights_fractional_allocation: {}", e.id));
                }
            }
            if !s.refunded && e.terms.refund_date.confirmed().is_some_and(|v| date >= *v) {
                let refund = (s.subscribed - s.allocated)
                    * d(e.terms.price.confirmed().ok_or("rights_price_missing")?)?;
                *cash += refund;
                s.refunded = true;
                self.audit.push(serde_json::json!({"date":date,"event":e.id,"kind":"refund","cash":refund.to_string()}));
            }
            if !s.listed && e.terms.listing_date.confirmed().is_some_and(|v| date >= *v) {
                *positions.entry(e.instrument.clone()).or_default() += s.allocated;
                *available.entry(e.instrument.clone()).or_default() += s.allocated;
                s.listed = true;
                self.audit.push(serde_json::json!({"date":date,"event":e.id,"kind":"list","quantity":s.allocated.to_string()}));
            }
        }
        Ok(())
    }
    /// Pending assets are valued separately and never exposed as sellable positions.
    pub fn value(&self, prices: &BTreeMap<String, Decimal>) -> Result<Decimal> {
        let Some(date) = self.date else {
            return Ok(Decimal::ZERO);
        };
        let mut value = Decimal::ZERO;
        for e in &self.events {
            let Some(s) = self.states.get(&e.id) else {
                continue;
            };
            if s.subscribed == Decimal::ZERO {
                continue;
            }
            let confirmed = e
                .terms
                .allotment_date
                .confirmed()
                .is_some_and(|d| date >= *d);
            let qty = if confirmed { s.allocated } else { s.subscribed };
            if !s.listed && qty > Decimal::ZERO {
                if date < e.ex_date || !confirmed {
                    value += qty * d(e.terms.price.confirmed().ok_or("rights_price_missing")?)?;
                } else {
                    let price = *prices
                        .get(&e.instrument)
                        .filter(|p| **p > Decimal::ZERO)
                        .ok_or_else(|| {
                            format!("rights_valuation_missing: {} {date}", e.instrument)
                        })?;
                    value += qty * price;
                }
            }
            if confirmed && !s.refunded {
                value += (s.subscribed - s.allocated)
                    * d(e.terms.price.confirmed().ok_or("rights_price_missing")?)?;
            }
        }
        Ok(value)
    }
}
