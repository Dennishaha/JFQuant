//! Reconstruct actual closing positions from dated fills and the exact supported
//! action projection. Callback-time portfolios are not closing account evidence.
use super::*;
use serde_json::{Value, json};
fn number(v: &Value) -> Result<f64> {
    v.as_f64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|v| v.is_finite())
        .ok_or_else(|| "execution_account_number_invalid".into())
}
fn same(a: f64, b: f64) -> Result<()> {
    if !a.is_finite() || !b.is_finite() || (a - b).abs() > 1e-8_f64.max(b.abs() * 1e-10) {
        return Err(format!("execution_account_inconsistent: replay={a} native={b}").into());
    }
    Ok(())
}
pub(super) struct Reconciled {
    pub snapshots: BTreeMap<Date, Value>,
    pub days: Vec<portfolio::DayResult>,
    pub differences: Vec<Value>,
}
pub(super) fn reconcile(
    root: &Path,
    calendar: &[Date],
    range: &DateRange,
    initial: f64,
    closes: &BTreeMap<String, Vec<Option<f64>>>,
    actions: &[Value],
    benchmark: &BTreeMap<String, f64>,
) -> Result<Reconciled> {
    let native: Value = serde_json::from_slice(&fs::read(root.join("native_account_daily.json"))?)?;
    if native["schema_version"] != 1 {
        return Err("execution_account_format_unsupported".into());
    }
    let days: BTreeMap<Date, Value> = serde_json::from_value(native["days"].clone())?;
    let wanted = calendar
        .iter()
        .copied()
        .filter(|d| range.contains(*d))
        .collect::<BTreeSet<_>>();
    if days.keys().copied().collect::<BTreeSet<_>>() != wanted {
        return Err("execution_account_dates_missing".into());
    }
    let mut fills: BTreeMap<Date, Vec<Value>> = BTreeMap::new();
    let mut rejected: BTreeMap<Date, usize> = BTreeMap::new();
    for line in fs::read_to_string(root.join("order_events.jsonl"))?.lines() {
        let order: Value = serde_json::from_str(line)?;
        let date: Date = order["date"]
            .as_str()
            .ok_or("execution_order_date_missing")?
            .parse()?;
        let qty = number(&order["filled_quantity"])?;
        if !wanted.contains(&date) || qty < 0. {
            return Err("execution_order_invalid".into());
        }
        if qty > 0. {
            fills.entry(date).or_default().push(order);
        } else if number(&order["fee"])? != 0. {
            return Err("execution_unfilled_fee_invalid".into());
        } else if order["status"] == "rejected" {
            *rejected.entry(date).or_default() += 1;
        }
    }
    let mut cash = initial;
    let mut positions = BTreeMap::<String, f64>::new();
    let mut snapshots = BTreeMap::new();
    let mut results = Vec::new();
    let mut differences = Vec::new();
    let mut previous = initial;
    let mut rights: Option<analytics_core::rights_account::Ledger> =
        if root.join("rights_input.json").is_file() {
            Some(serde_json::from_slice(&fs::read(
                root.join("rights_input.json"),
            )?)?)
        } else {
            None
        };
    let native_rights: Option<analytics_core::rights_account::Ledger> = rights
        .as_ref()
        .map(|_| fs::read(root.join("rights_state.json")))
        .transpose()?
        .map(|v| serde_json::from_slice(&v))
        .transpose()?;
    for (index, date) in calendar
        .iter()
        .enumerate()
        .filter(|(_, d)| range.contains(**d))
    {
        for action in actions.iter().filter(|a| a["date"] == date.to_string()) {
            let id = action["symbol"]
                .as_str()
                .ok_or("execution_action_symbol_missing")?;
            let qty = positions.entry(id.to_owned()).or_default();
            let value = number(&action["value"])?;
            match action["kind"].as_str() {
                Some("Dividend") if value >= 0. => cash += *qty * value,
                Some("Split") if value > 0. => *qty *= value,
                _ => return Err("execution_action_projection_unsupported".into()),
            }
        }
        if let Some(ledger) = rights.as_mut().filter(|l| l.active()) {
            super::rights::advance(
                ledger,
                native_rights.as_ref().unwrap(),
                *date,
                index.checked_sub(1),
                &mut cash,
                &mut positions,
                closes,
            )?;
        }
        let mut fees = 0.;
        let mut notional = 0.;
        for order in fills.get(date).into_iter().flatten() {
            let id = order["instrument"]
                .as_str()
                .ok_or("execution_order_symbol_missing")?;
            analytics_core::pool::validate_instrument(id)?;
            let qty = number(&order["filled_quantity"])?;
            let price = number(&order["price"])?;
            let fee = number(&order["fee"])?;
            if price <= 0. || fee < 0. {
                return Err("execution_fill_terms_invalid".into());
            }
            fees += fee;
            notional += qty * price;
            let position = positions.entry(id.to_owned()).or_default();
            match order["side"].as_str() {
                Some("buy") => {
                    *position += qty;
                    cash -= qty * price + fee;
                }
                Some("sell") => {
                    *position -= qty;
                    cash += qty * price - fee;
                }
                _ => return Err("execution_fill_side_invalid".into()),
            }
            if *position < -1e-8 {
                return Err("execution_long_account_negative_position".into());
            }
        }
        if let Some(ledger) = rights.as_mut().filter(|l| l.active()) {
            use rust_decimal::{Decimal, prelude::ToPrimitive};
            let mut balance: Decimal = cash.to_string().parse()?;
            let mut p = positions
                .iter()
                .map(|(k, v)| Ok((k.clone(), v.to_string().parse::<Decimal>()?)))
                .collect::<Result<BTreeMap<_, _>>>()?;
            let mut available = p.clone();
            ledger.close_account(*date, &mut balance, &mut p, &mut available)?;
            cash = balance.to_f64().ok_or("corporate_cash_invalid")?;
            positions = p
                .into_iter()
                .map(|(k, v)| Ok((k, v.to_f64().ok_or("corporate_quantity_invalid")?)))
                .collect::<Result<_>>()?;
        }
        positions.retain(|_, q| q.abs() > 1e-8);
        let mut equity = cash;
        let mut prices = BTreeMap::new();
        for (id, qty) in &positions {
            let price = closes
                .get(id)
                .and_then(|p| p.get(index))
                .copied()
                .flatten()
                .ok_or_else(|| {
                    format!("execution_valuation_missing: {id} {date}; no verified execution mark")
                })?;
            if !price.is_finite() || price <= 0. {
                return Err("execution_valuation_price_invalid".into());
            }
            equity += qty * price;
            prices.insert(id.clone(), price);
        }
        if !cash.is_finite() || !equity.is_finite() || previous <= 0. {
            return Err("execution_account_equity_invalid".into());
        }
        let pending = if let Some(ledger) = &rights {
            use rust_decimal::prelude::ToPrimitive;
            let marks = closes
                .iter()
                .filter_map(|(id, p)| p.get(index).copied().flatten().map(|v| (id, v)))
                .map(|(id, p)| Ok((id.clone(), p.to_string().parse()?)))
                .collect::<Result<_>>()?;
            ledger
                .value(&marks)?
                .to_f64()
                .ok_or("rights_valuation_invalid")?
        } else {
            0.
        };
        equity += pending;
        if let Some(ledger) = &rights {
            for event in &ledger.events {
                if ledger
                    .states
                    .get(&event.id)
                    .is_some_and(|s| s.subscribed > rust_decimal::Decimal::ZERO && !s.listed)
                    && let Some(mark) = closes
                        .get(&event.instrument)
                        .and_then(|v| v.get(index))
                        .copied()
                        .flatten()
                {
                    prices.insert(event.instrument.clone(), mark);
                }
            }
        }
        let native_cash = number(&days[date]["cash"])?;
        let native_equity = number(&days[date]["equity"])?;
        if same(cash, native_cash).is_err() || same(equity, native_equity).is_err() {
            differences.push(json!({"date":date,"native_curve_cash":native_cash,"native_curve_equity":native_equity,"ledger_cash":cash,"ledger_equity":equity,"reason":"native daily curve may precede final deferred fills; closing ledger uses all dated fills"}));
        }
        snapshots.insert(*date,json!({"positions":positions,"holdings":positions.len(),"cash":cash,"equity":equity,"valuation_prices":prices}));
        if let Some(ledger) = &rights {
            let snapshot = snapshots.get_mut(date).unwrap();
            snapshot["rights"] = serde_json::to_value(&ledger.states)?;
            snapshot["corporate_events"] = serde_json::to_value(&ledger.corporate.states)?;
            snapshot["corporate_pending_values"] =
                serde_json::to_value(ledger.corporate.pending_values()?)?;
            snapshot["pending_rights_value"] = pending.into();
        }
        results.push(portfolio::DayResult {
            date: date.to_string(),
            net_return: equity / previous - 1.,
            benchmark_return: *benchmark.get(&date.to_string()).unwrap_or(&0.),
            turnover: notional / previous,
            cost: fees / previous,
            execution_fee_cny: fees,
            holdings: positions.len(),
            blocked_orders: *rejected.get(date).unwrap_or(&0),
        });
        previous = equity;
    }
    let terminal: BTreeMap<String, f64> =
        serde_json::from_value(native["terminal"]["positions"].clone())?;
    for id in positions
        .keys()
        .chain(terminal.keys())
        .collect::<BTreeSet<_>>()
    {
        same(
            *positions.get(id).unwrap_or(&0.),
            *terminal.get(id).unwrap_or(&0.),
        )?;
    }
    same(cash, number(&native["terminal"]["cash"])?)?;
    same(previous, number(&native["terminal"]["equity"])?)?;
    if let Some(ledger) = &rights {
        let native = native_rights.as_ref().unwrap();
        if serde_json::to_value(ledger)? != serde_json::to_value(native)? {
            return Err("rights_native_replay_state_mismatch".into());
        }
    }
    Ok(Reconciled {
        snapshots,
        days: results,
        differences,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires native synthetic rights fixture; see research README"]
    fn native_rights_fixture_replays_exact_account() {
        let root = PathBuf::from(
            std::env::var_os("JFQUANT_RIGHTS_FIXTURE_DIR")
                .expect("synthetic fixture directory required"),
        );
        let input: analytics_core::rights_account::Ledger =
            serde_json::from_slice(&fs::read(root.join("rights_input.json")).unwrap()).unwrap();
        let calendar = input.events[0].sessions.clone();
        let range = DateRange {
            start: calendar[0],
            end: *calendar.last().unwrap(),
        };
        let closes = BTreeMap::from([
            ("SH600000".into(), vec![Some(10.); calendar.len()]),
            ("SZ000001".into(), vec![Some(10.); calendar.len()]),
        ]);
        let r = reconcile(
            &root,
            &calendar,
            &range,
            100000.,
            &closes,
            &[],
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(r.snapshots[&range.end]["positions"]["SH600000"], 10780.);
        assert_eq!(r.snapshots[&range.end]["pending_rights_value"], 0.);
    }
    #[test]
    fn replay_uses_all_dated_fills_and_ex_date_old_share_base_and_checks_terminal_account() {
        let root = tempfile::tempdir().unwrap();
        let calendar: Vec<Date> = ["2021-01-04", "2021-01-05", "2021-01-06"]
            .map(|d| d.parse().unwrap())
            .into();
        let range = DateRange {
            start: calendar[0],
            end: calendar[2],
        };
        let native = json!({"schema_version":1,"days":{"2021-01-04":{"cash":1000.,"equity":1000.},"2021-01-05":{"cash":909.,"equity":1009.},"2021-01-06":{"cash":943.,"equity":1013.}},"terminal":{"positions":{"SH600000":10.},"cash":943.,"equity":1013.}});
        save(root.path().join("native_account_daily.json"), &native).unwrap();
        let orders = [
            json!({"date":"2021-01-04","instrument":"SH600000","side":"buy","filled_quantity":10.,"price":10.,"fee":1.}),
            json!({"date":"2021-01-06","instrument":"SH600000","side":"sell","filled_quantity":5.,"price":7.,"fee":1.}),
        ];
        fs::write(
            root.path().join("order_events.jsonl"),
            orders
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let mut closes = BTreeMap::from([(
            "SH600000".into(),
            vec![Some(10.), Some(100. / 15.), Some(7.)],
        )]);
        let actions = [
            json!({"symbol":"SH600000","date":"2021-01-05","kind":"Dividend","value":"1"}),
            json!({"symbol":"SH600000","date":"2021-01-05","kind":"Split","value":1.5}),
        ];
        let result = reconcile(
            root.path(),
            &calendar,
            &range,
            1000.,
            &closes,
            &actions,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(result.snapshots[&calendar[0]]["cash"], 899.);
        assert_eq!(result.snapshots[&calendar[1]]["positions"]["SH600000"], 15.);
        assert_eq!(result.snapshots[&calendar[1]]["cash"], 909.);
        same(result.days[0].net_return, -0.001).unwrap();
        assert_eq!(result.days[0].execution_fee_cny, 1.);
        assert_eq!(result.differences.len(), 1);
        closes.get_mut("SH600000").unwrap()[1] = None;
        assert!(
            reconcile(
                root.path(),
                &calendar,
                &range,
                1000.,
                &closes,
                &actions,
                &BTreeMap::new()
            )
            .is_err()
        );
        closes.get_mut("SH600000").unwrap()[1] = Some(100. / 15.);
        let mut wrong = native.clone();
        wrong["terminal"]["positions"]["SH600000"] = json!(11.);
        save(root.path().join("native_account_daily.json"), &wrong).unwrap();
        assert!(
            reconcile(
                root.path(),
                &calendar,
                &range,
                1000.,
                &closes,
                &actions,
                &BTreeMap::new()
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod corporate_tests {
    use super::*;
    #[test]
    #[ignore = "export native fixtures with JFQUANT_CORPORATE_FIXTURE_DIR; see research README"]
    fn native_corporate_fixtures_replay_exact_account() {
        let base = PathBuf::from(
            std::env::var_os("JFQUANT_CORPORATE_FIXTURE_DIR").expect("fixture directory required"),
        );
        for kind in ["liquidation", "migration", "merger"] {
            let root = base.join(kind);
            let fixture: Value =
                serde_json::from_slice(&fs::read(root.join("fixture.json")).unwrap()).unwrap();
            let calendar: Vec<Date> = serde_json::from_value(fixture["calendar"].clone()).unwrap();
            let closes = serde_json::from_value(fixture["closes"].clone()).unwrap();
            let range = DateRange {
                start: calendar[0],
                end: *calendar.last().unwrap(),
            };
            let replay = reconcile(
                &root,
                &calendar,
                &range,
                fixture["initial"].as_f64().unwrap(),
                &closes,
                &[],
                &BTreeMap::new(),
            )
            .unwrap();
            assert_eq!(replay.days.len(), calendar.len());
            let terminal = &replay.snapshots[&range.end];
            assert!(terminal["positions"].get("SH600000").is_none());
            if kind != "liquidation" {
                assert!(terminal["positions"]["SZ000001"].as_f64().unwrap() > 0.);
            }
        }
    }
}
