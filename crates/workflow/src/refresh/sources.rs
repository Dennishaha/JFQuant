//! Refresh sources stage.
use super::*;
pub(super) fn compare_probe_windows(
    previous: &[BTreeMap<Date, Value>],
    current: &BTreeMap<Date, Value>,
) -> Vec<Date> {
    let mut conflicts = BTreeSet::new();
    for old in previous {
        let scale = current
            .iter()
            .find_map(|(day, value)| {
                old.get(day)?
                    .get("factor")?
                    .as_f64()
                    .zip(value["factor"].as_f64())
                    .filter(|(a, b)| *a > 0. && *b > 0.)
                    .map(|(a, b)| a / b)
            })
            .unwrap_or(1.);
        for (day, value) in current {
            let Some(prior) = old.get(day) else {
                continue;
            };
            let factor_matches = match (prior["factor"].as_f64(), value["factor"].as_f64()) {
                (Some(a), Some(b)) if a > 0. && b > 0. => (b * scale / a - 1.).abs() <= 1e-5,
                (None, None) => true,
                _ => false,
            };
            let mut a = prior.clone();
            let mut b = value.clone();
            a.as_object_mut().unwrap().remove("factor");
            b.as_object_mut().unwrap().remove("factor");
            if a != b || !factor_matches {
                conflicts.insert(*day);
            }
        }
    }
    conflicts.into_iter().collect()
}

/// Bounded three-round account probe. Never schedules the full research universe.
pub fn check_source(
    w: &Workspace,
    run: &RunRecord,
    provider: Provider,
    context: &TaskContext,
) -> Result<Value> {
    if matches!(provider, Provider::Tdxaidata | Provider::Hithink) {
        analytics_storage::credentials::load_source(if provider == Provider::Tdxaidata {
            "tdxaidata"
        } else {
            "hithink"
        })?;
    }
    let today = chrono::Local::now();
    let end = if today.hour() < 16 {
        today.date_naive().pred_opt().ok_or("date_overflow")?
    } else {
        today.date_naive()
    };
    let mut checks = vec![];
    let mut common: BTreeMap<String, Vec<BTreeMap<Date, Value>>> = BTreeMap::new();
    let mut native_requests = 0;
    let windows = if provider == Provider::Hithink {
        [
            DateRange {
                start: end - chrono::Duration::days(30),
                end,
            },
            DateRange {
                start: end - chrono::Duration::days(60),
                end,
            },
            DateRange {
                start: end - chrono::Duration::days(90),
                end,
            },
        ]
    } else {
        [
            DateRange {
                start: "2006-12-04".parse()?,
                end: "2006-12-08".parse()?,
            },
            DateRange {
                start: "2016-06-13".parse()?,
                end: "2016-06-24".parse()?,
            },
            DateRange {
                start: end - chrono::Duration::days(7),
                end,
            },
        ]
    };
    for round in 0..3 {
        for id in [
            "SH600000", "SH600018", "SZ000001", "SH000300", "SH600030", "SH600068",
        ] {
            for (i, range) in windows.iter().enumerate() {
                context.check()?;
                let r = DateRange {
                    start: range.start - chrono::Duration::days(round * 3),
                    end: range.end,
                };
                let r = if provider == Provider::Tdxaidata
                    && round == 0
                    && i == 1
                    && id == "SH600000"
                {
                    DateRange {
                        start: "2006-12-04".parse()?,
                        end,
                    }
                } else {
                    r
                };
                context.report(format!(
                    "来源小样本 第{}轮 · {} · {}—{}",
                    round + 1,
                    id,
                    r.start,
                    r.end
                ))?;
                let start = std::time::Instant::now();
                let root = w
                    .resolve(&run.output)
                    .join(format!("probe-{round}-{id}-{i}"));
                let req = collection(provider, id, &r, if id == "SH000300" { id } else { "" });
                let mut outcome = match collector::collect(&req, &root, context) {
                    Ok(market) => {
                        let data = DailyDataset::open(&market)?;
                        let rows = read_bars(&market.join("bars").join(format!("{id}.parquet")))?;
                        let record = AdjustmentRecord::read(&market, id)?;
                        let observed = common.entry(id.into()).or_default();
                        let mut current = BTreeMap::new();
                        for row in &rows {
                            let value = json!({"open":row.open,"high":row.high,"low":row.low,"close":row.close,"volume":row.volume,"amount":row.amount,"factor":record.on(row.trade_date)?});
                            current.insert(row.trade_date, value);
                        }
                        let mismatches = compare_probe_windows(observed, &current);
                        observed.push(current);
                        json!({"status":if mismatches.is_empty(){"passed"}else{"failed"},"code":if mismatches.is_empty(){"quotation_validated"}else{"cross_window_conflict"},"rows":rows.len(),"calendar_sessions":data.calendar.len(),"first":rows.first().map(|r|r.trade_date),"last":rows.last().map(|r|r.trade_date),"window_conflicts":mismatches})
                    }
                    Err(error) => {
                        context.check()?;
                        let failures = fs::read(root.join("acquisition_readiness.json"))
                            .ok()
                            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
                        json!({"status":"failed","reason":error.to_string(),"source_failures":failures})
                    }
                };
                // Count native data calls even when a shard failed validation;
                // successful-only counts would hide the actual probe usage.
                native_requests += fs::read_to_string(root.join("raw/sdk-requests.jsonl"))
                    .map(|journal| journal.lines().count())
                    .unwrap_or(0);
                outcome["round"] = json!(round + 1);
                outcome["instrument"] = json!(id);
                outcome["range"] = json!(r);
                outcome["elapsed_ms"] = json!(start.elapsed().as_millis());
                checks.push(outcome);
                atomic_json(
                    &w.resolve(&run.output).join("source-check.json"),
                    &json!({"checks":checks,"complete":false,"native_requests":native_requests,"points_cost":"unknown","full_download_authorized":false}),
                )?;
            }
        }
    }
    let report = json!({"schema_version":1,"provider":provider,"checks":checks,"native_requests":native_requests,"complete":true,"points_cost":"unknown","full_download_authorized":false,"account_dates":"requires_other_sources","historical_members":"not_supported","historical_execution":"not_verified","sdk_version":"1.2.2"});
    atomic_json(&w.resolve(&run.output).join("source-check.json"), &report)?;
    Ok(report)
}
