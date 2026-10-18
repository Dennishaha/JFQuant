//! Fractional, zero-fee reference account. Owns its event state, never native orders.
use crate::{Result, atomic_json};
use analytics_core::Date;
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path};

fn dec(v: &Value) -> Result<Decimal> {
    Ok(v.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| v.to_string())
        .parse()?)
}
fn num(v: Decimal) -> Result<f64> {
    v.to_f64()
        .filter(|x| x.is_finite())
        .ok_or_else(|| "reference_number_overflow".into())
}
fn funded_quantity(wanted: Decimal, cash: Decimal, price: Decimal) -> Result<Decimal> {
    let mut quantity = wanted.max(Decimal::ZERO).min(cash / price);
    // Decimal division rounds to a representable fraction. When it rounds up,
    // use the preceding representable share quantity rather than overdrawing
    // cash or silently erasing a negative balance after the trade.
    if quantity * price > cash {
        quantity = (quantity - Decimal::new(1, quantity.scale())).max(Decimal::ZERO);
    }
    if quantity * price > cash {
        return Err("reference_funding_precision".into());
    }
    Ok(quantity)
}
fn mark(
    positions: &BTreeMap<String, Decimal>,
    prices: &BTreeMap<String, Decimal>,
) -> Result<Decimal> {
    positions
        .iter()
        .filter(|(_, q)| **q != Decimal::ZERO)
        .map(|(s, q)| {
            Ok(*q
                * prices
                    .get(s)
                    .ok_or_else(|| format!("reference_valuation_unknown:{s}"))?)
        })
        .sum()
}
/// Rebuild a reference account entirely from frozen targets, quotes and event facts.
pub fn build(root: &Path) -> Result<Value> {
    let input: Value = serde_json::from_slice(&fs::read(root.join("reference-input.json"))?)?;
    let report: Value = serde_json::from_slice(&fs::read(root.join("allocation_report.json"))?)?;
    if input["schema_version"] != 1
        || report["schema_version"] != 2
        || report["status"] != "completed"
    {
        return Err("reference_input_incomplete".into());
    }
    let mut bars = BTreeMap::<Date, BTreeMap<String, Value>>::new();
    for row in input["bars"].as_array().ok_or("reference_bars_missing")? {
        let d: Date = row["date"]
            .as_str()
            .ok_or("reference_date_missing")?
            .parse()?;
        let s = row["symbol"].as_str().ok_or("reference_symbol_missing")?;
        if bars
            .entry(d)
            .or_default()
            .insert(s.into(), row.clone())
            .is_some()
        {
            return Err("reference_duplicate_bar".into());
        }
    }
    let warmup: Value = serde_json::from_slice(&fs::read(root.join("reference-targets.json"))?)?;
    let targets = warmup["targets"]
        .as_array()
        .ok_or("reference_warmup_targets_missing")?
        .iter()
        .chain(
            report["targets"]
                .as_array()
                .ok_or("reference_targets_missing")?,
        )
        .cloned()
        .collect::<Vec<_>>();
    let mut ledger: analytics_core::rights_account::Ledger = if input["rights"].is_null() {
        Default::default()
    } else {
        serde_json::from_value(input["rights"].clone())?
    };
    if !ledger.states.is_empty() || !ledger.audit.is_empty() || !ledger.corporate.states.is_empty()
    {
        return Err("reference_requires_initial_event_state".into());
    }
    // Initial one CNY, with fractional ordinary shares. Discrete event rounding is retained.
    let mut cash = Decimal::ONE;
    let mut positions = BTreeMap::<String, Decimal>::new();
    let mut wanted = BTreeMap::<String, Decimal>::new();
    let mut pending = false;
    let mut rows = Vec::new();
    for (date, day) in &bars {
        analytics_core::task::TaskContext::current().check()?;
        let opens = day
            .iter()
            .filter_map(|(s, r)| {
                dec(&r["open"])
                    .ok()
                    .filter(|p| *p > Decimal::ZERO)
                    .map(|p| (s.clone(), p))
            })
            .collect::<BTreeMap<_, _>>();
        let closes = day
            .iter()
            .filter_map(|(s, r)| {
                dec(&r["close"])
                    .ok()
                    .filter(|p| *p > Decimal::ZERO)
                    .map(|p| (s.clone(), p))
            })
            .collect::<BTreeMap<_, _>>();
        for action in input["actions"]
            .as_array()
            .ok_or("reference_actions_missing")?
            .iter()
            .filter(|a| a["date"] == date.to_string())
        {
            let s = action["symbol"].as_str().ok_or("reference_event_symbol")?;
            let q = positions.entry(s.into()).or_default();
            match action["kind"].as_str() {
                Some("Dividend") => cash += *q * dec(&action["value"])?,
                Some("Split") => *q *= dec(&action["value"])?,
                _ => return Err("reference_event_unsupported".into()),
            }
        }
        if ledger.active() {
            let mut available = positions.clone();
            ledger.open(
                *date,
                &mut cash,
                Decimal::ZERO,
                &mut positions,
                &mut available,
            )?;
        }
        let open_nav = cash + mark(&positions, &opens)? + ledger.value(&opens)?;
        for t in targets
            .iter()
            .filter(|t| t["effective_date"] == date.to_string())
        {
            wanted = t["weights"]
                .as_object()
                .ok_or("reference_target_weights")?
                .iter()
                .map(|(s, w)| Ok((s.clone(), dec(w)?)))
                .collect::<Result<_>>()?;
            pending = true;
        }
        let mut changed = false;
        if pending {
            let names = positions
                .keys()
                .chain(wanted.keys())
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            let tradable = |s: &String| {
                day.get(s)
                    .and_then(|r| r["volume"].as_f64())
                    .is_some_and(|v| v > 0.)
            };
            let desired = names
                .iter()
                .filter(|s| tradable(s))
                .map(|s| {
                    Ok((
                        s.clone(),
                        wanted.get(s).copied().unwrap_or_default() * open_nav
                            / opens
                                .get(s)
                                .ok_or_else(|| format!("reference_open_unknown:{s}"))?,
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            for s in names.iter().filter(|s| tradable(s)) {
                let q = positions.entry(s.clone()).or_default();
                let sell = (*q - desired[s]).max(Decimal::ZERO);
                *q -= sell;
                cash += sell * opens[s];
                changed |= sell > Decimal::ZERO;
            }
            for s in names.iter().filter(|s| tradable(s)) {
                let q = positions.entry(s.clone()).or_default();
                let buy = funded_quantity(desired[s] - *q, cash, opens[s])?;
                *q += buy;
                cash -= buy * opens[s];
                changed |= buy > Decimal::ZERO;
            }
            pending = names.iter().any(|s| {
                let held = positions.get(s).copied().unwrap_or_default();
                match desired.get(s) {
                    Some(target) => (held - target).abs() > Decimal::new(1, 12),
                    None => held != Decimal::ZERO || wanted.get(s).is_some_and(|w| !w.is_zero()),
                }
            });
        }
        positions.retain(|_, q| *q != Decimal::ZERO);
        if ledger.active() {
            let mut available = positions.clone();
            ledger.close_account(*date, &mut cash, &mut positions, &mut available)?;
        }
        if cash < Decimal::ZERO || positions.values().any(|q| *q < Decimal::ZERO) {
            return Err(format!("reference_negative_cash_or_shares:{date}:cash={cash}").into());
        }
        let locked = ledger.value(&closes)?;
        let nav = cash + mark(&positions, &closes)? + locked;
        if nav <= Decimal::ZERO {
            return Err("reference_nonpositive_nav".into());
        }
        let weights = positions
            .iter()
            .map(|(s, q)| Ok((s.clone(), num(*q * closes[s] / nav)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        rows.push(json!({"date":date,"open_nav":num(open_nav)?,"nav":num(nav)?,"cash":num(cash)?,"locked_value":num(locked)?,"positions":positions,"weights":weights,"pending":pending,"changed":changed}));
    }
    let result = json!({"schema_version":1,"input_sha256":crate::signature(&input)?,"targets_sha256":crate::signature(&report["targets"] )?,"initial_cny":1,"fees":0,"fractional_shares":true,"rows":rows,"events":ledger});
    atomic_json(&root.join("reference-account.json"), &result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fractional_funding_never_rounds_quantity_above_available_cash() {
        for cash in ["1", "0.02", "0.00000000000000000000000001"] {
            for price in ["6", "91.19", "0.001", "123.456789"] {
                let cash: Decimal = cash.parse().unwrap();
                let price: Decimal = price.parse().unwrap();
                let quantity = funded_quantity(Decimal::from(1_000_000), cash, price).unwrap();
                assert!(quantity >= Decimal::ZERO);
                assert!(quantity * price <= cash);
                let remaining = cash - quantity * price;
                let next_unit = Decimal::new(1, quantity.scale()) * price;
                assert!(remaining <= next_unit);
            }
        }
        // An affordable desired quantity is preserved, without normalization.
        assert_eq!(
            funded_quantity(Decimal::new(1, 2), Decimal::ONE, Decimal::from(6)).unwrap(),
            Decimal::new(1, 2)
        );
    }
    #[test]
    fn unheld_missing_open_stays_cash_until_tradable_or_replaced() {
        for replace in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            atomic_json(&root.join("reference-input.json"), &json!({
                "schema_version":1,"rights":null,"actions":[],"bars":[
                    {"date":"2024-01-01","symbol":"SH600000","open":10.,"close":10.,"volume":100.},
                    {"date":"2024-01-02","symbol":"SZ000001","open":20.,"close":20.,"volume":100.},
                    {"date":"2024-01-03","symbol":"SH600000","open":10.,"close":10.,"volume":100.}
                ]
            })).unwrap();
            atomic_json(
                &root.join("reference-targets.json"),
                &json!({"schema_version":1,"targets":[]}),
            )
            .unwrap();
            let mut targets = vec![
                json!({"date":"2024-01-01","effective_date":"2024-01-02","weights":{"SH600000":0.98}}),
            ];
            if replace {
                targets
                    .push(json!({"date":"2024-01-02","effective_date":"2024-01-03","weights":{}}));
            }
            atomic_json(
                &root.join("allocation_report.json"),
                &json!({"schema_version":2,"status":"completed","targets":targets}),
            )
            .unwrap();
            let result = build(root).unwrap();
            let rows = result["rows"].as_array().unwrap();
            assert_eq!(rows[1]["cash"], 1.);
            assert_eq!(rows[1]["positions"], json!({}));
            assert_eq!(rows[1]["pending"], true);
            assert_eq!(rows[2]["pending"], false);
            if replace {
                assert_eq!(rows[2]["cash"], 1.);
                assert_eq!(rows[2]["positions"], json!({}));
            } else {
                assert_eq!(rows[2]["positions"]["SH600000"], "0.098");
            }
            // The same missing quote remains fatal when shares are actually held.
            targets[0]["effective_date"] = json!("2024-01-01");
            atomic_json(
                &root.join("allocation_report.json"),
                &json!({"schema_version":2,"status":"completed","targets":targets}),
            )
            .unwrap();
            assert!(
                build(root)
                    .unwrap_err()
                    .to_string()
                    .contains("reference_valuation_unknown:SH600000")
            );
        }
    }
    #[test]
    fn halted_targets_remain_pending_and_dividends_stay_cash() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        atomic_json(&root.join("reference-input.json"),&json!({"schema_version":1,"rights":null,"actions":[{"date":"2024-01-04","symbol":"SH600000","kind":"Dividend","value":1.}],"bars":[
            {"date":"2024-01-01","symbol":"SH600000","open":10.,"close":10.,"volume":10.},
            {"date":"2024-01-02","symbol":"SH600000","open":10.,"close":10.,"volume":0.},
            {"date":"2024-01-03","symbol":"SH600000","open":10.,"close":12.,"volume":10.},
            {"date":"2024-01-04","symbol":"SH600000","open":11.,"close":11.,"volume":10.}]})).unwrap();
        atomic_json(
            &root.join("reference-targets.json"),
            &json!({"schema_version":1,"targets":[]}),
        )
        .unwrap();
        atomic_json(&root.join("allocation_report.json"),&json!({"schema_version":2,"status":"completed","targets":[{"date":"2024-01-01","effective_date":"2024-01-02","weights":{"SH600000":0.98}}]})).unwrap();
        let r = build(root).unwrap();
        let rows = r["rows"].as_array().unwrap();
        assert_eq!(rows[1]["pending"], true);
        assert_eq!(rows[1]["positions"], json!({}));
        assert_eq!(rows[2]["pending"], false);
        assert_eq!(rows[2]["positions"]["SH600000"], "0.098");
        assert_eq!(rows[3]["positions"], rows[2]["positions"]);
        assert!((rows[3]["cash"].as_f64().unwrap() - 0.118).abs() < 1e-12);
        assert!(
            (rows[3]["nav"].as_f64().unwrap() - rows[2]["nav"].as_f64().unwrap()).abs() < 1e-12
        );
    }
    #[test]
    fn a_new_target_replaces_halted_orders_and_no_signal_preserves_shares() {
        let root = tempfile::tempdir().unwrap();
        let p = root.path();
        let bars = (1..=4)
            .flat_map(|d| {
                ["SH600000", "SZ000001"].map(move |s| {
                    json!({
                        "date":format!("2024-01-0{d}"), "symbol":s,
                        "open":if d==4 {20.} else {10.}, "close":if d==4 {20.} else {10.},
                        "volume":if d==2 && s=="SH600000" {0.} else {1000.}
                    })
                })
            })
            .collect::<Vec<_>>();
        atomic_json(
            &p.join("reference-input.json"),
            &json!({"schema_version":1,"rights":null,"actions":[],"bars":bars}),
        )
        .unwrap();
        atomic_json(
            &p.join("reference-targets.json"),
            &json!({"schema_version":1,"targets":[]}),
        )
        .unwrap();
        atomic_json(
            &p.join("allocation_report.json"),
            &json!({"schema_version":2,"status":"completed","targets":[
                {"date":"2024-01-01","effective_date":"2024-01-02","weights":{"SH600000":0.98}},
                {"date":"2024-01-02","effective_date":"2024-01-03","weights":{"SZ000001":0.98}}
            ]}),
        )
        .unwrap();
        let result = build(p).unwrap();
        let rows = result["rows"].as_array().unwrap();
        assert_eq!(rows[1]["pending"], true);
        assert!(rows[2]["positions"].get("SH600000").is_none());
        assert_eq!(rows[2]["positions"]["SZ000001"], "0.098");
        assert_eq!(rows[3]["positions"], rows[2]["positions"]);
        assert_eq!(rows[3]["changed"], false);
        assert!(rows[3]["nav"].as_f64().unwrap() > rows[2]["nav"].as_f64().unwrap());
    }
}

#[cfg(test)]
mod rights_tests {
    use super::*;
    use analytics_core::rights::{Fact, InsufficientCash, Policy, Terms};
    #[test]
    fn independent_reference_subscription_obeys_funding_and_locked_delivery() {
        let date = |s: &str| s.parse::<Date>().unwrap();
        let terms = Terms {
            announced: Fact::Confirmed {
                value: date("2024-01-01"),
            },
            subscription_start: Fact::Confirmed {
                value: date("2024-01-03"),
            },
            subscription_end: Fact::Confirmed {
                value: date("2024-01-03"),
            },
            price: Fact::Confirmed {
                value: "0.001".into(),
            },
            ratio: Fact::Confirmed { value: "1".into() },
            unit: Fact::Confirmed { value: 1 },
            fee_per_share: Fact::Confirmed { value: "0".into() },
            allotment_ratio: Fact::Confirmed { value: "1".into() },
            allotment_date: Fact::Confirmed {
                value: date("2024-01-03"),
            },
            refund_date: Fact::NotApplicable {
                reason: "fully allocated reference fixture".into(),
            },
            listing_date: Fact::Confirmed {
                value: date("2024-01-04"),
            },
            ..Default::default()
        };
        let event = analytics_core::rights_account::Event {
            id: "rights".into(),
            instrument: "SH600000".into(),
            record_date: date("2024-01-02"),
            ex_date: date("2024-01-03"),
            terms,
            sessions: vec![date("2024-01-02"), date("2024-01-03"), date("2024-01-04")],
        };
        for policy in [InsufficientCash::Partial, InsufficientCash::Stop] {
            let root = tempfile::tempdir().unwrap();
            let p = root.path();
            let ledger = analytics_core::rights_account::Ledger {
                policy: Policy {
                    participation: analytics_core::rights::Participation::Participate,
                    insufficient_cash: policy,
                    ..Default::default()
                },
                events: vec![event.clone()],
                ..Default::default()
            };
            atomic_json(&p.join("reference-input.json"),&json!({"schema_version":1,"rights":ledger,"actions":[],"bars":(1..=4).map(|d|json!({"date":format!("2024-01-0{d}"),"symbol":"SH600000","open":0.001,"close":0.001,"volume":100000.})).collect::<Vec<_>>()})).unwrap();
            atomic_json(
                &p.join("reference-targets.json"),
                &json!({"schema_version":1,"targets":[]}),
            )
            .unwrap();
            atomic_json(&p.join("allocation_report.json"),&json!({"schema_version":2,"status":"completed","targets":[{"date":"2024-01-01","effective_date":"2024-01-02","weights":{"SH600000":0.98}}]})).unwrap();
            let result = build(p);
            if policy == InsufficientCash::Stop {
                assert!(result.is_err());
                continue;
            }
            let result = result.unwrap();
            let rows = result["rows"].as_array().unwrap();
            assert!(rows[2]["locked_value"].as_f64().unwrap() > 0.);
            assert_eq!(rows[3]["locked_value"], 0.);
            assert_eq!(rows[3]["positions"]["SH600000"], "1000");
            assert!(rows.iter().all(|r| r["cash"].as_f64().unwrap() >= 0.));
        }
    }
}
