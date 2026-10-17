//! Deterministic research transitions, separate from observed exchange fills.
use chrono::NaiveDate as Date;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

type Result<T> = std::result::Result<T, String>;
/// Explicit account assumptions; never written into registrar evidence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Require supported factual settlement; no research liquidation assumption.
    #[default]
    EvidenceOnly,
    /// Retrospective last-trade liquidation, floor conversion and locked claims.
    ResearchTransitions,
}
/// Reviewed terminal or successor instruction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Terms {
    /// Same security interest under a different trading identifier.
    Migration {
        /// New canonical trading identifier.
        successor: String,
    },
    /// Mandatory share exchange, without electing an optional cash choice.
    Merger {
        /// Successor identifier; its own factor history remains independent.
        successor: String,
        /// Exact decimal successor shares per predecessor share.
        ratio: Decimal,
        /// First day these shares can be sold.
        listing_date: Date,
    },
    /// Research-only liquidation; date and price are independently verified.
    Liquidation {
        /// Configured broker commission rate.
        commission_rate: Decimal,
        /// Configured minimum commission for a nonzero sale.
        minimum_commission: Decimal,
        /// Configured stamp and transfer rates combined.
        other_sell_rate: Decimal,
    },
}
/// Immutable event input, containing facts rather than mutable account state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// Stable identity from the frozen evidence version.
    pub id: String,
    /// Original canonical security.
    pub instrument: String,
    /// Migration/entitlement effective date, or liquidation closing date.
    pub date: Date,
    /// Last actual predecessor trading date.
    pub last_trade_date: Date,
    /// Unadjusted predecessor close on that date.
    pub last_close: Decimal,
    /// Public announcement date, retained to expose retrospective assumptions.
    pub announced: Date,
    /// Evidence references retained in the ledger.
    pub evidence: Vec<String>,
    /// Valid observed successor closes by date; no stale or future quote backfill.
    pub successor_closes: BTreeMap<Date, Decimal>,
    /// Executable instruction.
    pub terms: Terms,
}
/// One event's actual holding exposure and conversion loss.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// Original shares exposed when the event executes.
    pub old_quantity: Decimal,
    /// Integer successor shares to receive.
    pub quantity: Decimal,
    /// Fractional successor shares discarded by the simulation policy.
    pub fractional_loss: Decimal,
    /// Shares have entered the ordinary account.
    pub delivered: bool,
}
/// Shared account transition journal; no engine or database handles.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ledger {
    /// Frozen research choices.
    pub policy: Policy,
    /// Ordered frozen facts.
    pub events: Vec<Event>,
    /// Actual exposures, including zero holdings.
    pub states: BTreeMap<String, State>,
    /// Already completed opening phases.
    pub opens: BTreeSet<Date>,
    /// Already completed closing phases.
    pub closes: BTreeSet<Date>,
    /// Separately auditable, synthetic movements; never exchange fills.
    pub audit: Vec<serde_json::Value>,
}
impl Ledger {
    /// Validate projections and disallow conflicting transitions from the same code.
    pub fn validate(&self) -> Result<()> {
        let mut ids = BTreeSet::new();
        let mut instruments = BTreeSet::new();
        for e in &self.events {
            if self.policy != Policy::ResearchTransitions
                || e.id.is_empty()
                || !ids.insert(&e.id)
                || !instruments.insert(&e.instrument)
                || e.evidence.is_empty()
                || e.evidence.iter().any(|s| s.trim().is_empty())
                || e.last_close <= Decimal::ZERO
                || e.last_trade_date > e.date
                || (!matches!(e.terms, Terms::Liquidation { .. }) && e.last_trade_date == e.date)
                || e.successor_closes
                    .iter()
                    .any(|(d, p)| *d < e.date || *p <= Decimal::ZERO)
            {
                return Err("corporate_event_identity_or_facts_invalid".into());
            }
            match &e.terms {
                Terms::Migration { successor } | Terms::Merger { successor, .. }
                    if successor == &e.instrument || successor.is_empty() =>
                {
                    return Err("corporate_successor_invalid".into());
                }
                Terms::Merger {
                    ratio,
                    listing_date,
                    ..
                } if *ratio <= Decimal::ZERO || *listing_date < e.date => {
                    return Err("corporate_conversion_terms_invalid".into());
                }
                Terms::Liquidation {
                    commission_rate,
                    minimum_commission,
                    other_sell_rate,
                } if e.date != e.last_trade_date
                    || *commission_rate < Decimal::ZERO
                    || *minimum_commission < Decimal::ZERO
                    || *other_sell_rate < Decimal::ZERO
                    || *commission_rate + *other_sell_rate >= Decimal::ONE =>
                {
                    return Err("corporate_liquidation_terms_invalid".into());
                }
                _ => {}
            }
        }
        Ok(())
    }
    /// Codes whose pending orders must be canceled before a transition can execute.
    pub fn retiring(&self, date: Date, closing: bool) -> Vec<String> {
        self.events
            .iter()
            .filter(|e| e.date == date && matches!(e.terms, Terms::Liquidation { .. }) == closing)
            .map(|e| e.instrument.clone())
            .collect()
    }
    /// Atomic opening transition. A duplicate phase is rejected, including after restore.
    pub fn open(
        &mut self,
        date: Date,
        positions: &mut BTreeMap<String, Decimal>,
        available: &mut BTreeMap<String, Decimal>,
    ) -> Result<()> {
        let (mut next, mut p, mut a) = (self.clone(), positions.clone(), available.clone());
        next.apply_open(date, &mut p, &mut a)?;
        *self = next;
        *positions = p;
        *available = a;
        Ok(())
    }
    fn apply_open(
        &mut self,
        date: Date,
        positions: &mut BTreeMap<String, Decimal>,
        available: &mut BTreeMap<String, Decimal>,
    ) -> Result<()> {
        if self.opens.last().is_some_and(|d| *d >= date) {
            return Err("corporate_open_repeated_or_out_of_order".into());
        }
        self.opens.insert(date);
        for e in &self.events {
            if e.date == date && !matches!(e.terms, Terms::Liquidation { .. }) {
                let old = positions.remove(&e.instrument).unwrap_or_default();
                available.remove(&e.instrument);
                if old < Decimal::ZERO || old.fract() != Decimal::ZERO {
                    return Err("corporate_position_invalid".into());
                }
                let exact = match &e.terms {
                    Terms::Merger { ratio, .. } => old * ratio,
                    _ => old,
                };
                let state = State {
                    old_quantity: old,
                    quantity: exact.floor(),
                    fractional_loss: exact.fract(),
                    delivered: false,
                };
                if self.states.insert(e.id.clone(), state.clone()).is_some() {
                    return Err("corporate_event_repeated".into());
                }
                self.audit.push(serde_json::json!({"date":date,"event":e.id,"phase":"convert","state":state,"simulation_fractional_rule":"floor_without_cash"}));
            }
            let Some(s) = self.states.get_mut(&e.id) else {
                continue;
            };
            let successor = match &e.terms {
                Terms::Migration { successor } if date >= e.date => Some(successor),
                Terms::Merger {
                    successor,
                    listing_date,
                    ..
                } if date >= *listing_date => Some(successor),
                _ => None,
            };
            if let Some(id) = successor.filter(|_| !s.delivered) {
                *positions.entry(id.clone()).or_default() += s.quantity;
                *available.entry(id.clone()).or_default() += s.quantity;
                s.delivered = true;
                self.audit.push(serde_json::json!({"date":date,"event":e.id,"phase":"deliver","successor":id,"quantity":s.quantity}));
            }
        }
        positions.retain(|_, q| *q != Decimal::ZERO);
        available.retain(|_, q| *q != Decimal::ZERO);
        Ok(())
    }
    /// Apply liquidation only after every dated exchange fill, including on the final day.
    pub fn close(
        &mut self,
        date: Date,
        cash: &mut Decimal,
        positions: &mut BTreeMap<String, Decimal>,
        available: &mut BTreeMap<String, Decimal>,
    ) -> Result<()> {
        let (mut next, mut c, mut p, mut a) =
            (self.clone(), *cash, positions.clone(), available.clone());
        next.apply_close(date, &mut c, &mut p, &mut a)?;
        *self = next;
        *cash = c;
        *positions = p;
        *available = a;
        Ok(())
    }
    fn apply_close(
        &mut self,
        date: Date,
        cash: &mut Decimal,
        positions: &mut BTreeMap<String, Decimal>,
        available: &mut BTreeMap<String, Decimal>,
    ) -> Result<()> {
        if self.closes.last().is_some_and(|d| *d >= date) {
            return Err("corporate_close_repeated_or_out_of_order".into());
        }
        self.closes.insert(date);
        for e in self.events.iter().filter(|e| e.date == date) {
            let Terms::Liquidation {
                commission_rate,
                minimum_commission,
                other_sell_rate,
            } = e.terms
            else {
                continue;
            };
            let quantity = positions.remove(&e.instrument).unwrap_or_default();
            available.remove(&e.instrument);
            if quantity < Decimal::ZERO || quantity.fract() != Decimal::ZERO {
                return Err("corporate_position_invalid".into());
            }
            let notional = quantity * e.last_close;
            let fee = if quantity > Decimal::ZERO {
                (notional * commission_rate).max(minimum_commission) + notional * other_sell_rate
            } else {
                Decimal::ZERO
            };
            *cash += notional - fee;
            if self
                .states
                .insert(
                    e.id.clone(),
                    State {
                        old_quantity: quantity,
                        delivered: true,
                        ..Default::default()
                    },
                )
                .is_some()
            {
                return Err("corporate_event_repeated".into());
            }
            self.audit.push(serde_json::json!({"date":date,"event":e.id,"phase":"research_liquidation","quantity":quantity,"raw_close":e.last_close,"notional":notional,"fee":fee,"announced":e.announced,"retrospective":true,"cash_reusable":"next_session"}));
        }
        Ok(())
    }
    /// Locked claims contribute equity but never become tradeable positions or cash.
    pub fn value(&self, _prices: &BTreeMap<String, Decimal>) -> Result<Decimal> {
        Ok(self.pending_values()?.values().copied().sum())
    }
    /// Locked value by predecessor, for auditable model-relative attribution.
    pub fn pending_values(&self) -> Result<BTreeMap<String, Decimal>> {
        let mut values = BTreeMap::new();
        for e in &self.events {
            let Some(s) = self
                .states
                .get(&e.id)
                .filter(|s| !s.delivered && s.old_quantity > Decimal::ZERO)
            else {
                continue;
            };
            if !matches!(e.terms, Terms::Merger { .. }) {
                continue;
            }
            let value = match self
                .opens
                .last()
                .and_then(|date| e.successor_closes.get(date))
                .filter(|p| **p > Decimal::ZERO)
            {
                Some(p) => s.quantity * p,
                None => {
                    let Terms::Merger { ratio, .. } = &e.terms else {
                        unreachable!()
                    };
                    s.quantity * e.last_close / ratio
                }
            };
            *values.entry(e.instrument.clone()).or_default() += value;
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn day(s: &str) -> Date {
        s.parse().unwrap()
    }
    fn fixture(terms: Terms) -> Ledger {
        Ledger {
            policy: Policy::ResearchTransitions,
            events: vec![Event {
                id: "reviewed:1".into(),
                instrument: "SZ002013".into(),
                date: day("2023-04-13"),
                last_trade_date: day("2023-02-07"),
                last_close: Decimal::from(10),
                announced: day("2023-04-14"),
                evidence: vec!["sha256:reviewed".into()],
                successor_closes: BTreeMap::from([(day("2023-04-13"), Decimal::from(20))]),
                terms,
            }],
            ..Default::default()
        }
    }
    #[test]
    fn merger_locked_value_floor_delivery_and_restore() {
        let mut l = fixture(Terms::Merger {
            successor: "SH600372".into(),
            ratio: "0.6647".parse().unwrap(),
            listing_date: day("2023-04-19"),
        });
        l.validate().unwrap();
        let mut p = BTreeMap::from([("SZ002013".into(), Decimal::from(101))]);
        let mut a = p.clone();
        l.open(day("2023-04-13"), &mut p, &mut a).unwrap();
        assert!(p.is_empty() && a.is_empty());
        assert_eq!(l.value(&BTreeMap::new()).unwrap(), Decimal::from(1340));
        assert_eq!(
            l.states["reviewed:1"].fractional_loss,
            "0.1347".parse().unwrap()
        );
        l = serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
        assert!(l.open(day("2023-04-13"), &mut p, &mut a).is_err());
        l.open(day("2023-04-14"), &mut p, &mut a).unwrap();
        // Missing today's quote must not reuse yesterday's successor quote.
        assert_eq!(
            l.value(&BTreeMap::new()).unwrap(),
            Decimal::from(670) / "0.6647".parse::<Decimal>().unwrap()
        );
        l.open(day("2023-04-19"), &mut p, &mut a).unwrap();
        assert_eq!(p["SH600372"], Decimal::from(67));
        assert_eq!(p, a);
        assert_eq!(l.value(&BTreeMap::new()).unwrap(), Decimal::ZERO);
    }
    #[test]
    fn final_close_liquidates_actual_quantity_once_with_separate_fees() {
        let mut l = fixture(Terms::Liquidation {
            commission_rate: "0.0003".parse().unwrap(),
            minimum_commission: Decimal::from(5),
            other_sell_rate: "0.00101".parse().unwrap(),
        });
        l.events[0].date = l.events[0].last_trade_date;
        l.validate().unwrap();
        let d = l.events[0].date;
        let mut cash = Decimal::from(50);
        // Includes same-day late fills, rather than a callback-time position.
        let mut p = BTreeMap::from([("SZ002013".into(), Decimal::from(101))]);
        let mut a = BTreeMap::new();
        l.close(d, &mut cash, &mut p, &mut a).unwrap();
        assert!(p.is_empty());
        assert_eq!(cash, "1053.9799".parse().unwrap());
        assert_eq!(l.audit[0]["retrospective"], true);
        let before = serde_json::to_value(&l).unwrap();
        let balance = cash;
        assert!(l.close(d, &mut cash, &mut p, &mut a).is_err());
        assert_eq!(balance, cash);
        assert_eq!(before, serde_json::to_value(&l).unwrap());
    }
    #[test]
    fn migration_adds_to_existing_successor_and_rejects_invalid_atomically() {
        let mut l = fixture(Terms::Migration {
            successor: "SZ302132".into(),
        });
        let mut p = BTreeMap::from([
            ("SZ002013".into(), Decimal::from(101)),
            ("SZ302132".into(), Decimal::from(20)),
        ]);
        let mut a = p.clone();
        l.open(day("2023-04-13"), &mut p, &mut a).unwrap();
        assert_eq!(p["SZ302132"], Decimal::from(121));
        assert!(!p.contains_key("SZ002013"));
        let mut invalid = fixture(Terms::Migration {
            successor: "SZ302132".into(),
        });
        p.insert("SZ002013".into(), "1.5".parse().unwrap());
        let before = p.clone();
        assert!(invalid.open(day("2023-04-13"), &mut p, &mut a).is_err());
        assert_eq!(p, before);
        assert!(invalid.opens.is_empty());
    }
}
