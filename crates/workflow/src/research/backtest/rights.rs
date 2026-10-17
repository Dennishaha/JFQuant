//! Reviewed rights projection; native registration determines actual account exposure.
use super::*;
use analytics_core::{
    actions::EventTerms,
    rights_account::{Event, Ledger},
};
use rust_decimal::{Decimal, prelude::ToPrimitive};

fn decimal(v: &serde_json::Value) -> Result<Decimal> {
    Ok(v.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| v.to_string())
        .parse()?)
}
pub(super) fn advance(
    ledger: &mut Ledger,
    native: &Ledger,
    date: Date,
    previous_index: Option<usize>,
    cash: &mut f64,
    positions: &mut BTreeMap<String, f64>,
    closes: &BTreeMap<String, Vec<Option<f64>>>,
) -> Result<()> {
    let records = native
        .audit
        .iter()
        .filter(|r| r["kind"] == "open_orders" && r["date"] == date.to_string())
        .collect::<Vec<_>>();
    if records.len() != 1 {
        return Err("rights_open_order_evidence_missing_or_duplicate".into());
    }
    let row = records[0];
    let mut original = positions
        .iter()
        .map(|(k, v)| Ok((k.clone(), v.to_string().parse::<Decimal>()?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut projected = original.clone();
    let prices: BTreeMap<String, Decimal> = serde_json::from_value(row["prices"].clone())?;
    for (id, price) in &prices {
        if let Some(expected) =
            previous_index.and_then(|i| closes.get(id)?.get(i).copied().flatten())
            && (price.to_f64().ok_or("rights_mark_invalid")? - expected).abs()
                > 1e-8_f64.max(expected.abs() * 1e-10)
        {
            return Err("rights_reserved_mark_mismatch".into());
        }
    }
    for order in row["orders"].as_array().ok_or("rights_orders_invalid")? {
        if !matches!(
            order["status"].as_str(),
            Some("New" | "Submitted" | "PartiallyFilled")
        ) {
            continue;
        }
        let id = order["symbol"]
            .as_str()
            .ok_or("rights_order_symbol_invalid")?;
        let remaining = decimal(&order["quantity"])? - decimal(&order["filled_quantity"])?;
        if remaining <= Decimal::ZERO {
            continue;
        }
        let qty = projected.entry(id.into()).or_default();
        match order["side"].as_str() {
            Some("Buy") => *qty += remaining,
            Some("Sell") => *qty -= remaining,
            _ => return Err("rights_order_side_invalid".into()),
        }
    }
    let market_value = |positions: &BTreeMap<String, Decimal>| -> Result<Decimal> {
        positions
            .iter()
            .map(|(id, q)| Ok(q.abs() * prices.get(id).copied().unwrap_or_default()))
            .sum()
    };
    let reserved = (market_value(&projected)? - market_value(&original)?).max(Decimal::ZERO);
    if reserved != decimal(&row["reserved"])? {
        return Err("rights_reserved_cash_mismatch".into());
    }
    let mut available = original.clone();
    let mut balance: Decimal = cash.to_string().parse()?;
    ledger.audit.push(row.clone());
    ledger.open(date, &mut balance, reserved, &mut original, &mut available)?;
    *cash = balance.to_f64().ok_or("rights_cash_invalid")?;
    *positions = original
        .into_iter()
        .map(|(k, v)| Ok((k, v.to_f64().ok_or("rights_quantity_invalid")?)))
        .collect::<Result<_>>()?;
    Ok(())
}
pub(super) fn input(
    request: &ResearchRequest,
    book: &analytics_storage::corporate_actions::ActionBundle,
    dataset: &DailyDataset,
    pool: &analytics_core::pool::PoolMembership,
    range: &DateRange,
) -> Result<Option<Ledger>> {
    let corporate = super::corporate::input(request, dataset, book, pool, range)?;
    if request.rights_policy.is_none() && !super::corporate::enabled(request) {
        return Ok(None);
    }
    let policy = request
        .rights_policy
        .clone()
        .unwrap_or(analytics_core::rights::Policy {
            participation: analytics_core::rights::Participation::Abandon,
            ..Default::default()
        });
    let mut events = Vec::new();
    for a in &book.actions {
        let EventTerms::RightsIssueFacts { facts, .. } = &a.terms else {
            continue;
        };
        if request.rights_policy.is_none() || !range.contains(a.ex_date) {
            continue;
        }
        let record = a
            .record_date
            .ok_or_else(|| format!("rights_record_date_missing: {}", a.id))?;
        events.push(Event {
            id: a.id.clone(),
            instrument: a.instrument.clone(),
            record_date: record,
            ex_date: a.ex_date,
            terms: *facts.clone(),
            sessions: dataset
                .calendar
                .iter()
                .copied()
                .filter(|d| range.contains(*d))
                .collect(),
        });
    }
    events.sort_by(|a, b| a.id.cmp(&b.id));
    let ledger = Ledger {
        policy,
        corporate,
        events,
        ..Default::default()
    };
    ledger.validate()?;
    Ok(Some(ledger))
}
