use super::*;
use std::{fs, path::Path};

fn date(s: &str) -> Date {
    s.parse().unwrap()
}
fn request() -> BuildRequest {
    let coverage = DateRange {
        start: date("2022-12-30"),
        end: date("2024-07-01"),
    };
    let mut request = BuildRequest {
        schema_version: SCHEMA_VERSION,
        construction_version: VERSION.into(),
        sources: BTreeMap::from([
            ("universe".into(), "synthetic dated universe".into()),
            ("shares".into(), "synthetic share history".into()),
            (
                "book_equity".into(),
                "synthetic annual equity versions".into(),
            ),
        ]),
        market: "synthetic twelve-stock hand-calculated fixture".into(),
        range: DateRange {
            start: date("2023-07-03"),
            end: date("2023-07-03"),
        },
        price_basis: PriceBasis::Provider,
        market_basis: MarketBasis::MarketReturn,
        universe: PoolMembership {
            coverage: coverage.clone(),
            intervals: BTreeMap::new(),
        },
        shares: vec![],
        book_equity: vec![],
        risk_free: vec![],
        provider_history: None,
    };
    for i in 1..=12 {
        let id = format!("SH{:06}", 600000 + i);
        request
            .universe
            .intervals
            .insert(id.clone(), vec![coverage.clone()]);
        request.shares.push(Shares {
            instrument: id.clone(),
            effective_date: date("2022-01-01"),
            published_on: date("2022-01-01"),
            total_shares: Some(i as f64),
        });
        request.book_equity.push(BookEquity {
            instrument: id,
            period_end: date("2022-12-31"),
            published_on: date("2023-04-01"),
            book_equity_cny: Some((i as f64 * 10.) * (((i - 1) % 6) / 2 + 1) as f64),
        });
    }
    request
}
fn calendar() -> Vec<Date> {
    [
        "2022-12-30",
        "2023-06-30",
        "2023-07-03",
        "2023-07-04",
        "2023-12-29",
        "2024-06-28",
        "2024-07-01",
    ]
    .map(date)
    .into()
}
fn inputs<'a>(request: &'a BuildRequest, calendar: &'a [Date]) -> Inputs<'a> {
    let mut result = Inputs {
        request,
        calendar,
        prices: BTreeMap::new(),
        shares: BTreeMap::new(),
        books: BTreeMap::new(),
    };
    for (i, id) in request.universe.intervals.keys().enumerate() {
        result.prices.insert(
            id.clone(),
            Prices {
                raw: DailyField {
                    start: 0,
                    values: vec![Some(10.); calendar.len()],
                },
                adjusted: DailyField {
                    start: 0,
                    values: vec![
                        Some(100.),
                        Some(100.),
                        Some(101. + i as f64),
                        Some(101. + i as f64),
                        Some(100.),
                        Some(100.),
                        Some(100.),
                    ],
                },
            },
        );
    }
    for row in &request.shares {
        result
            .shares
            .entry(row.instrument.clone())
            .or_default()
            .push(row);
    }
    for row in &request.book_equity {
        result
            .books
            .entry(row.instrument.clone())
            .or_default()
            .push(row);
    }
    result
}
fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-13, "{a} != {b}");
}
#[test]
fn input_protocol_requires_current_schema_and_explicit_nullable_history() {
    let current = serde_json::to_value(request()).unwrap();
    assert!(
        current
            .as_object()
            .unwrap()
            .contains_key("provider_history")
    );
    assert!(current["provider_history"].is_null());
    serde_json::from_value::<BuildRequest>(current.clone())
        .unwrap()
        .validate()
        .unwrap();
    let mut missing = current.clone();
    missing.as_object_mut().unwrap().remove("provider_history");
    assert!(serde_json::from_value::<BuildRequest>(missing).is_err());
    let mut old = current;
    old["schema_version"] = serde_json::json!(1);
    assert!(
        serde_json::from_value::<BuildRequest>(old)
            .unwrap()
            .validate()
            .is_err()
    );
}
#[test]
fn provider_snapshot_branch_matches_hand_golden_and_never_carries_missing_dates() {
    use crate::three_factor::acquire::{Annual, Availability, Captured, History, ShareAtClose};
    let mut request = request();
    let calendar = calendar();
    let captures = request
        .universe
        .intervals
        .keys()
        .enumerate()
        .map(|(i, id)| {
            (
                id.clone(),
                Captured {
                    observed_on: date("2024-07-02"),
                    shares: calendar
                        .iter()
                        .map(|d| ShareAtClose {
                            trade_date: *d,
                            total_shares: Some((i + 1) as f64),
                        })
                        .collect(),
                    annual: vec![Annual {
                        period_end: date("2022-12-31"),
                        reported_on: date("2023-04-01"),
                        book_equity_cny: request.book_equity[i].book_equity_cny,
                    }],
                    issues: vec![],
                },
            )
        })
        .collect();
    request.shares.clear();
    request.book_equity.clear();
    request.provider_history = Some(History {
        normalization: crate::three_factor::acquire::VERSION.into(),
        bundle_sha256: format!("sha256:{}", "a".repeat(64)),
        availability: Availability::ProviderDates,
        instruments: captures,
    });
    request.validate().unwrap();
    let input = inputs(&request, &calendar);
    let ids: Vec<_> = request.universe.intervals.keys().cloned().collect();
    close(input.weighted(&ids, 2).0.unwrap(), 1. / 12.);
    assert!(input.formation(2023).problems.is_empty());
    let history = request.provider_history.as_mut().unwrap();
    history
        .instruments
        .get_mut(&ids[0])
        .unwrap()
        .shares
        .remove(1);
    assert!(inputs(&request, &calendar).weighted(&ids, 2).0.is_none());
    assert!(
        !inputs(&request, &calendar)
            .formation(2023)
            .problems
            .is_empty()
    );
    request.provider_history.as_mut().unwrap().availability = Availability::ObservedOn;
    assert!(inputs(&request, &calendar).weighted(&ids, 2).0.is_none());
}

#[test]
fn hand_calculated_six_portfolios_and_lagged_weight_golden() {
    // Sizes 10..120, B/M 1,1,2,2,3,3 repeated, day returns 1%..12%.
    // Each portfolio has two members: its return is a hand-calculated fraction.
    let request = request();
    let calendar = calendar();
    let input = inputs(&request, &calendar);
    let formation = input.formation(2023);
    assert!(formation.problems.is_empty());
    assert_eq!(
        formation.groups.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![2; 6]
    );
    for (actual, expected) in formation
        .breakpoints
        .unwrap()
        .into_iter()
        .zip([65., 1.3, 2.7])
    {
        close(actual, expected);
    }
    let expected = [
        1. / 60.,
        1. / 28.,
        61. / 1100.,
        113. / 1500.,
        181. / 1900.,
        53. / 460.,
    ];
    for (members, golden) in formation.groups.iter().zip(expected) {
        close(input.weighted(members, 2).0.unwrap(), golden);
    }
    let all = request
        .universe
        .on(calendar[2])
        .into_iter()
        .collect::<Vec<_>>();
    close(input.weighted(&all, 2).0.unwrap(), 1. / 12.);
}

#[test]
fn future_revisions_and_same_day_share_changes_cannot_change_past_weights() {
    let original = request();
    let calendar = calendar();
    let before = inputs(&original, &calendar).formation(2023);
    let mut changed = original.clone();
    changed.book_equity.push(BookEquity {
        instrument: "SH600001".into(),
        period_end: date("2022-12-31"),
        published_on: date("2023-07-01"),
        book_equity_cny: Some(1e12),
    });
    changed.shares.push(Shares {
        instrument: "SH600001".into(),
        effective_date: date("2023-07-03"),
        published_on: date("2023-06-01"),
        total_shares: Some(1e9),
    });
    let after = inputs(&changed, &calendar);
    assert_eq!(before.groups, after.formation(2023).groups);
    let all = changed
        .universe
        .on(calendar[2])
        .into_iter()
        .collect::<Vec<_>>();
    close(after.weighted(&all, 2).0.unwrap(), 1. / 12.);
    assert!(after.cap("SH600001", 2).unwrap() > 1e9);
    // A correction disclosed before formation must change the grouping.
    changed.book_equity.last_mut().unwrap().published_on = date("2023-06-29");
    assert_ne!(
        before.groups,
        inputs(&changed, &calendar).formation(2023).groups
    );
}

#[test]
fn missing_constituent_is_not_dropped_and_unknown_formation_is_reported() {
    let mut request = request();
    let calendar = calendar();
    let mut input = inputs(&request, &calendar);
    let formation = input.formation(2023);
    input.prices.get_mut("SH600001").unwrap().adjusted.values[2] = None;
    let (value, missing) = input.weighted(&formation.groups[0], 2);
    assert!(value.is_none());
    assert_eq!(missing, vec!["SH600001"]);
    assert!(input.weighted(&formation.groups[1], 2).0.is_some());
    request.book_equity[0].published_on = date("2023-06-30");
    let result = inputs(&request, &calendar).formation(2023);
    assert_eq!(result.problems, vec!["formation_facts_missing"]);
    assert!(result.groups.iter().all(Vec::is_empty));
    assert!(result.exclusions["SH600001"].contains("missing"));
}

#[test]
fn nonpositive_book_equity_and_new_listing_are_explicit_exclusions() {
    let mut request = request();
    let calendar = calendar();
    request.book_equity[0].book_equity_cny = Some(-10.);
    request.universe.intervals.get_mut("SH600002").unwrap()[0].start = date("2023-01-01");
    let formation = inputs(&request, &calendar).formation(2023);
    assert!(formation.problems.is_empty());
    assert_eq!(formation.exclusions["SH600001"], "nonpositive_book_equity");
    assert_eq!(
        formation.exclusions["SH600002"],
        "not_listed_at_previous_december"
    );
    request.book_equity[1].book_equity_cny = None;
    assert!(
        inputs(&request, &calendar)
            .formation(2023)
            .problems
            .is_empty()
    );
}

#[test]
fn validation_rejects_ambiguous_units_dates_versions_and_duplicate_revisions() {
    let original = request();
    original.validate().unwrap();
    let mut r = original.clone();
    r.shares.push(r.shares[0].clone());
    assert!(r.validate().is_err());
    let mut r = original.clone();
    r.book_equity.push(r.book_equity[0].clone());
    assert!(r.validate().is_err());
    let mut r = original.clone();
    r.shares[0].total_shares = Some(0.);
    assert!(r.validate().is_err());
    let mut r = original.clone();
    r.book_equity[0].period_end = date("2022-09-30");
    assert!(r.validate().is_err());
    let mut r = original.clone();
    r.book_equity[0].published_on = date("2022-12-31");
    assert!(r.validate().is_err());
    let mut r = original.clone();
    r.construction_version = "other".into();
    assert!(r.validate().is_err());
    let mut r = original.clone();
    r.sources.remove("shares");
    assert!(r.validate().is_err());
    let mut r = original;
    r.market_basis = MarketBasis::ExcessReturn;
    assert!(r.validate().is_err());
}

fn write_qlib(root: &Path, request: &BuildRequest, calendar: &[Date]) {
    fs::create_dir_all(root.join("calendars")).unwrap();
    fs::write(
        root.join("calendars/day.txt"),
        calendar
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    for (i, id) in request.universe.intervals.keys().enumerate() {
        let dir = root.join("features").join(id.to_lowercase());
        fs::create_dir_all(&dir).unwrap();
        for (name, values) in [
            (
                "close",
                vec![
                    100.,
                    100.,
                    101. + i as f32,
                    101. + i as f32,
                    100.,
                    100.,
                    100.,
                ],
            ),
            ("factor", vec![10.; calendar.len()]),
        ] {
            fs::write(
                dir.join(format!("{name}.day.bin")),
                std::iter::once(0_f32)
                    .chain(values)
                    .flat_map(f32::to_le_bytes)
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        }
    }
}

#[test]
fn daily_reader_end_to_end_golden_rf_nulls_and_june_rollover() {
    let dir = tempfile::tempdir().unwrap();
    let mut request = request();
    let calendar = calendar();
    write_qlib(dir.path(), &request, &calendar);
    let dataset = DailyDataset::open(dir.path()).unwrap();
    let (factors, report) = calculate(&dataset, &request).unwrap();
    let value = &factors.observations[0];
    close(value.mkt.unwrap(), 1. / 12.);
    close(value.smb.unwrap(), -2245798. / 37855125.);
    close(value.hml.unwrap(), 1244. / 31625.);
    assert_eq!(report.days[0].portfolio_members, [2; 6]);
    // Its native reader sees the exact same unscaled calculated daily factors.
    fs::write(
        dir.path().join(super::super::FILE_NAME),
        serde_json::to_vec(&factors).unwrap(),
    )
    .unwrap();
    close(
        DailyDataset::open(dir.path())
            .unwrap()
            .field("SH600001", "smb")
            .unwrap()
            .get(2)
            .unwrap(),
        value.smb.unwrap(),
    );
    request.market_basis = MarketBasis::ExcessReturn;
    request
        .sources
        .insert("rf".into(), "synthetic daily RF".into());
    let (missing, _) = calculate(&dataset, &request).unwrap();
    assert!(missing.observations[0].mkt.is_none());
    request.risk_free.push(RiskFree {
        trade_date: date("2023-07-03"),
        value: Some(0.001),
    });
    close(
        calculate(&dataset, &request).unwrap().0.observations[0]
            .mkt
            .unwrap(),
        1. / 12. - 0.001,
    );
    request.range.end = date("2024-07-01");
    let (factors, report) = calculate(&dataset, &request).unwrap();
    assert_eq!(report.formations.len(), 2);
    assert!(
        report.formations[1]
            .problems
            .contains(&"formation_facts_missing".into())
    );
    assert!(factors.observations.last().unwrap().smb.is_none());
    // A neutral-portfolio missing return invalidates SMB while HML stays available.
    let file = dir.path().join("features/sh600003/close.day.bin");
    let mut bytes = fs::read(&file).unwrap();
    bytes[12..16].copy_from_slice(&f32::NAN.to_le_bytes());
    fs::write(file, bytes).unwrap();
    let dataset = DailyDataset::open(dir.path()).unwrap();
    let (factors, _) = calculate(&dataset, &request).unwrap();
    assert!(factors.observations[0].smb.is_none());
    assert!(factors.observations[0].hml.is_some());
}
