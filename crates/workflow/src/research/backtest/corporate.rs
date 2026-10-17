//! Evidence-to-account projection; model features and histories are never spliced.
use super::*;
use analytics_core::{
    actions::{ConversionKind, EventTerms},
    corporate_account::{Event, Ledger, Policy, Terms},
};
use analytics_storage::corporate_actions::ActionBundle;
use rust_decimal::Decimal;

pub(super) fn enabled(request: &ResearchRequest) -> bool {
    request.corporate_event_policy == Some(Policy::ResearchTransitions)
}
/// Scope old holdings through their terminal boundary, then follow successor exposure.
pub(crate) fn scope(
    book: &ActionBundle,
    mut scope: BTreeMap<String, DateRange>,
    range: &DateRange,
) -> BTreeMap<String, DateRange> {
    // Process in time order, so a successor that later migrates gets its own boundary.
    let mut events = book.actions.iter().collect::<Vec<_>>();
    events.sort_by_key(|a| a.ex_date);
    for a in events {
        let Some(old) = scope.get(&a.instrument).cloned() else {
            continue;
        };
        let (end, successor) = match &a.terms {
            EventTerms::Delisting {
                last_trade_date: Some(last),
                ..
            } => (*last, None),
            EventTerms::Conversion {
                category: ConversionKind::Merger | ConversionKind::CodeChange,
                ordinary_holder: true,
                retain_original: false,
                successor: Some(next),
                ..
            } => (a.ex_date, Some(next)),
            _ => continue,
        };
        if end < old.start || end > old.end {
            continue;
        }
        scope.get_mut(&a.instrument).unwrap().end = end;
        if let Some(next) = successor {
            scope
                .entry(next.clone())
                .and_modify(|r| {
                    r.start = r.start.min(a.ex_date);
                    r.end = r.end.max(range.end);
                })
                .or_insert(DateRange {
                    start: a.ex_date,
                    end: range.end,
                });
        }
    }
    scope
}
pub(super) fn input(
    request: &ResearchRequest,
    dataset: &DailyDataset,
    book: &ActionBundle,
    pool: &analytics_core::pool::PoolMembership,
    range: &DateRange,
) -> Result<Ledger> {
    let policy = request.corporate_event_policy.unwrap_or_default();
    let mut ledger = Ledger {
        policy,
        ..Default::default()
    };
    if !enabled(request) {
        return Ok(ledger);
    }
    let original = super::execution::scope(pool, range);
    let scope = scope(book, original, range);
    let rates = &request.market_rules;
    let exchange = rates.exchanges.first().ok_or("missing sell fees")?;
    let number = |v: f64| -> Result<Decimal> { Ok(v.to_string().parse()?) };
    for a in &book.actions {
        if !scope.contains_key(&a.instrument) {
            continue;
        }
        let (date, last, terms) = match &a.terms {
            EventTerms::Delisting {
                last_trade_date: Some(last),
                ..
            } => (
                *last,
                *last,
                Terms::Liquidation {
                    commission_rate: number(rates.commission.rate)?,
                    minimum_commission: number(rates.commission.minimum_cny)?,
                    other_sell_rate: number(
                        exchange.sell_stamp_rate
                            + if matches!(
                                rates.commission.mode,
                                analytics_core::market_rules::CommissionMode::AllInclusive
                            ) {
                                0.
                            } else {
                                exchange.transfer_rate
                            },
                    )?,
                },
            ),
            EventTerms::Conversion {
                last_trade_date: Some(last),
                category,
                successor: Some(next),
                ratio,
                ordinary_holder: true,
                retain_original: false,
                cash,
                listing_date,
                ..
            } if matches!(
                category,
                ConversionKind::Merger | ConversionKind::CodeChange
            ) =>
            {
                if cash.parse::<Decimal>()? != Decimal::ZERO {
                    return Err(
                        format!("corporate_cash_consideration_unsupported: {}", a.id).into(),
                    );
                }
                let terms = if matches!(category, ConversionKind::CodeChange) {
                    if ratio.parse::<Decimal>()? != Decimal::ONE || *listing_date != Some(a.ex_date)
                    {
                        return Err("migration_must_preserve_quantity_and_tradability".into());
                    }
                    Terms::Migration {
                        successor: next.clone(),
                    }
                } else {
                    Terms::Merger {
                        successor: next.clone(),
                        ratio: ratio.parse()?,
                        listing_date: listing_date.ok_or("merger_listing_date_missing")?,
                    }
                };
                (a.ex_date, *last, terms)
            }
            _ => continue,
        };
        if !range.contains(date) || !scope[&a.instrument].contains(date) {
            continue;
        }
        analytics_core::pool::validate_instrument(&a.instrument)?;
        let closes = dataset.research_raw_field(&a.instrument, "close", false)?;
        let volume = dataset.research_raw_volume(&a.instrument, false)?;
        let i = dataset
            .calendar
            .binary_search(&last)
            .map_err(|_| "last_trade_not_in_calendar")?;
        let raw = closes
            .get(i)
            .filter(|v| v.is_finite() && *v > 0.)
            .ok_or("last_trade_raw_close_missing")?;
        if !volume.get(i).is_some_and(|v| v.is_finite() && v > 0.) {
            return Err("last_trade_volume_missing".into());
        }
        if dataset
            .calendar
            .iter()
            .enumerate()
            .any(|(j, d)| *d > last && *d < a.ex_date && volume.get(j).is_some_and(|v| v > 0.))
        {
            return Err(format!("last_trade_conflicts_with_observed_trading: {}", a.id).into());
        }
        let mut successor_closes = BTreeMap::new();
        if let Terms::Migration { successor } | Terms::Merger { successor, .. } = &terms {
            analytics_core::pool::validate_instrument(successor)?;
            let quotes = dataset.research_raw_field(successor, "close", false)?;
            for (j, d) in dataset
                .calendar
                .iter()
                .enumerate()
                .filter(|(_, d)| **d >= date && **d <= range.end)
            {
                if let Some(p) = quotes.get(j).filter(|p| p.is_finite() && *p > 0.) {
                    successor_closes.insert(*d, number(p)?);
                }
            }
        }
        if let Terms::Merger {
            successor,
            listing_date,
            ..
        } = &terms
            && book.actions.iter().any(|other| {
                other.instrument == *successor
                    && other.ex_date > date
                    && other.ex_date < *listing_date
            })
        {
            return Err(format!("locked_successor_event_requires_terms: {}", a.id).into());
        }
        ledger.events.push(Event {
            id: a.id.clone(),
            instrument: a.instrument.clone(),
            date,
            last_trade_date: last,
            last_close: number(raw)?,
            announced: a.announced.ok_or("corporate_announcement_date_missing")?,
            evidence: vec![a.source.clone()],
            successor_closes,
            terms,
        });
    }
    ledger
        .events
        .sort_by(|a, b| (a.date, &a.id).cmp(&(b.date, &b.id)));
    ledger.validate()?;
    Ok(ledger)
}

pub(super) fn report(input: &Ledger, result: &Ledger) -> Result<serde_json::Value> {
    if input.events != result.events || input.policy != result.policy {
        return Err("corporate_projection_changed".into());
    }
    let exposed = result
        .states
        .iter()
        .filter(|(_, s)| s.old_quantity > Decimal::ZERO)
        .collect::<BTreeMap<_, _>>();
    let mut loss_value = Decimal::ZERO;
    let mut fees = Decimal::ZERO;
    let mut notional = Decimal::ZERO;
    for event in &input.events {
        if let Some(s) = result.states.get(&event.id)
            && let Terms::Merger { ratio, .. } = &event.terms
        {
            loss_value += s.fractional_loss
                * event
                    .successor_closes
                    .get(&event.date)
                    .copied()
                    .unwrap_or(event.last_close / ratio);
        }
    }
    let mut liquidations = 0;
    for row in &result.audit {
        if row["phase"] == "research_liquidation"
            && row["quantity"].as_str().unwrap_or("0").parse::<Decimal>()? > Decimal::ZERO
        {
            liquidations += 1;
            fees += row["fee"]
                .as_str()
                .ok_or("corporate_fee_missing")?
                .parse::<Decimal>()?;
            notional += row["notional"]
                .as_str()
                .ok_or("corporate_notional_missing")?
                .parse::<Decimal>()?;
        }
    }
    Ok(
        serde_json::json!({"schema_version":1,"policy":input.policy,"candidate_event_count":input.events.len(),"affected_security_count":exposed.len(),"exposed_events":exposed,"fractional_loss_value_at_conversion_cny":loss_value,"synthetic_liquidation_count":liquidations,"synthetic_liquidation_notional_cny":notional,"synthetic_liquidation_fee_cny":fees,"audit":result.audit,"assumptions":["retrospective last-trade raw-close liquidation; proceeds reusable next session","floor fractional merger shares without compensation","pending successor quotes only on current date; otherwise carry predecessor value less fractional loss","locked claims contribute equity, not buying power"]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn date(s: &str) -> Date {
        s.parse().unwrap()
    }
    fn range(a: &str, b: &str) -> DateRange {
        DateRange {
            start: date(a),
            end: date(b),
        }
    }
    #[test]
    fn terminal_boundary_does_not_erase_prior_history_or_other_securities() {
        let mut book: ActionBundle = serde_json::from_value(serde_json::json!({"schema_version":5,"downloader":"JFQuant analytics-storage corporate-actions","downloaded_at":"2026-01-01T00:00:00Z","source_bindings":{},"coverage":{},"queried_symbols":[],"actions":[],"failures":{},"raw_sha256":{}})).unwrap();
        let mut event: analytics_core::execution::CorporateAction=serde_json::from_value(serde_json::json!({"id":"SZ300114:2025-02-17","instrument":"SZ300114","announced":"2025-02-15","record_date":null,"ex_date":"2025-02-17","pay_date":null,"listing_date":null,"cash_per_share":"0","shares_per_share":"0","source":"verified","terms":{"kind":"conversion","category":"code_change","last_trade_date":"2025-02-14","claim_valuation":null,"successor":"SZ302132","ratio":"1","ordinary_holder":true,"retain_original":false,"cash":"0","pay_date":null,"listing_date":"2025-02-17","reference":{"kind":"unchanged","evidence":"verified"},"allocation":{"kind":"exact"}}})).unwrap();
        book.actions.push(event.clone());
        let full = range("2021-01-01", "2026-10-12");
        let original = BTreeMap::from([
            ("SZ300114".into(), full.clone()),
            ("SH600000".into(), full.clone()),
        ]);
        let resolved = scope(&book, original.clone(), &full);
        assert_eq!(resolved["SZ300114"], range("2021-01-01", "2025-02-17"));
        assert_eq!(resolved["SZ302132"], range("2025-02-17", "2026-10-12"));
        assert_eq!(resolved["SH600000"], full);
        event.terms = EventTerms::Delisting {
            last_trade_date: Some(date("2025-02-14")),
            valuations: vec![],
            settlement_cash: None,
            settlement_date: None,
            reference: analytics_core::actions::Reference::Unchanged {
                evidence: "verified terminal event".into(),
            },
        };
        book.actions = vec![event];
        let resolved = scope(&book, original, &full);
        assert_eq!(resolved["SZ300114"], range("2021-01-01", "2025-02-14"));
        assert!(!resolved.contains_key("SZ302132"));
    }
}
