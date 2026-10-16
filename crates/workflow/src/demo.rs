//! Explicit synthetic demo inputs for full app acceptance without public market data.
use crate::*;
use analytics_storage::corporate_actions::ActionBundle;
use serde_json::json;
use std::{collections::BTreeSet, fs};
use uuid::Uuid;

/// Deterministic IDs belonging only to explicitly marked synthetic inputs.
pub fn fixture_security_id(code: &str) -> String {
    code.to_owned()
}
/// Small independently constructed input package, not a public provider cache.
pub fn create(w: &Workspace) -> Result<(DataVersion, DataVersion, ResearchTemplate)> {
    let base = format!("data/demo-{}", Uuid::new_v4());
    let root = w.resolve(format!("{base}/market/qlib_bin"));
    fs::create_dir_all(root.join("calendars"))?;
    fs::create_dir_all(root.join("instruments"))?;
    let mut dates = Vec::new();
    for year in 2007..=2010 {
        for day in 2..=12 {
            dates.push(format!("{year}-01-{day:02}"));
        }
    }
    fs::write(root.join("calendars/day.txt"), dates.join("\n"))?;
    fs::write(
        root.join("instruments/csi500.txt"),
        "SH600000\t2007-01-02\t2010-01-12\nSH600001\t2007-01-02\t2010-01-12\n",
    )?;
    fs::write(
        root.parent().unwrap().join("jfquant_source.json"),
        serde_json::to_vec(
            &json!({"generator":"JFQuant analytics-workflow synthetic-demo-v1","synthetic_fixture":true,"demo_id":base}),
        )?,
    )?;
    let write = |id: &str, name: &str, values: Vec<f32>| -> Result<()> {
        fs::create_dir_all(root.join(format!("features/{id}")))?;
        let bytes: Vec<u8> = std::iter::once(0.0f32)
            .chain(values)
            .flat_map(f32::to_le_bytes)
            .collect();
        fs::write(root.join(format!("features/{id}/{name}.day.bin")), bytes)?;
        Ok(())
    };
    for id in ["sh600000", "sh600001", "sh000905"] {
        for name in [
            "open", "close", "high", "low", "volume", "amount", "factor", "vwap",
        ] {
            let values = dates
                .iter()
                .map(|d| {
                    if id == "sh000905" {
                        return match name {
                            "factor" => 1.0,
                            "volume" => 10000.0,
                            "amount" => 1000.0,
                            _ => 100.0,
                        };
                    }
                    match name {
                        "factor" => {
                            if d.as_str() >= "2009-01-04" {
                                1.0 / 9.0
                            } else {
                                0.1
                            }
                        }
                        "open" | "close" | "vwap" => 1.0,
                        "high" => 1.2,
                        "low" => 0.8,
                        "volume" => 10000.0,
                        "amount" => 1000.0,
                        _ => unreachable!(),
                    }
                })
                .collect();
            write(id, name, values)?;
        }
    }
    let lifetime = analytics_core::DateRange {
        start: dates[0].parse()?,
        end: dates.last().unwrap().parse()?,
    };
    let dir = w.resolve(format!("{base}/actions"));
    fs::create_dir_all(dir.join("raw"))?;
    let actions = ["SH600000", "SH600001"]
        .map(|id| analytics_core::execution::CorporateAction {
            id: format!("{}:2009-01-04", fixture_security_id(id)),
            instrument: fixture_security_id(id),
            announced: Some("2009-01-02".parse().unwrap()),
            record_date: Some("2009-01-03".parse().unwrap()),
            ex_date: "2009-01-04".parse().unwrap(),
            pay_date: Some("2009-01-05".parse().unwrap()),
            listing_date: Some("2009-01-06".parse().unwrap()),
            cash_per_share: (0.1).to_string(),
            shares_per_share: (0.1).to_string(),
            terms: Default::default(),
            source: "https://example.test/synthetic-fixture".into(),
        })
        .to_vec();
    let mut book = ActionBundle {
        field_facts: Default::default(),
        structured_coverage: Default::default(),
        execution_issues: Vec::new(),
        execution_data: None,
        schema_version: 5,
        source_bindings: Default::default(),
        coverage: Default::default(),
        downloader: "JFQuant analytics-workflow synthetic-demo-v1".into(),
        downloaded_at: now(),
        queried_symbols: ["SH600000", "SH600001"].map(fixture_security_id).into(),
        actions,
        failures: Default::default(),
        raw_sha256: Default::default(),
    };
    book.declare_synthetic_coverage(lifetime.start, lifetime.end)?;
    fs::write(dir.join("actions.json"), serde_json::to_vec_pretty(&book)?)?;
    let m = w.register(&root)?;
    let a = w.register(&dir.join("actions.json"))?;
    let mut t = ResearchTemplate::builtin()?;
    t.name = "合成演示 · 流程和股数账本验证".into();
    t.schedule = analytics_core::schedule::Schedule::Rolling {
        first: analytics_core::schedule::Fold {
            train: analytics_core::DateRange {
                start: "2007-01-02".parse()?,
                end: "2007-12-31".parse()?,
            },
            validation: Some(analytics_core::DateRange {
                start: "2008-01-01".parse()?,
                end: "2008-12-31".parse()?,
            }),
            test: analytics_core::DateRange {
                start: "2009-01-01".parse()?,
                end: "2009-12-31".parse()?,
            },
        },
        step: analytics_core::schedule::Step {
            count: 1,
            unit: analytics_core::schedule::StepUnit::Years,
        },
        expanding: false,
        until: None,
    };
    t.config.top_n = 1;
    t.config.rebalance_days = 1;
    t.config.factor_ids = vec!["alpha_002".into()];
    t.config
        .training
        .parameters
        .insert("num_iterations".into(), "3".into());
    t.config
        .training
        .parameters
        .insert("min_data_in_leaf".into(), "1".into());
    t.config.training.early_stopping_rounds = None;
    t.initial_cash_cny = 1050.0;
    t.market_rules.commission.rate = 0.0;
    t.market_rules.commission.minimum_cny = 0.0;
    for f in &mut t.market_rules.exchanges {
        f.handling_rate = 0.0;
        f.regulatory_rate = 0.0;
        f.transfer_rate = 0.0;
        f.sell_stamp_rate = 0.0;
    }
    Ok((m, a, t))
}

/// Create the same complete synthetic acceptance inputs in canonical Parquet format.
/// Preserves source float32 observations so parity tests compare like-for-like inputs.
pub fn create_canonical(w: &Workspace) -> Result<(DataVersion, DataVersion, ResearchTemplate)> {
    use analytics_storage::daily::{DailyBar, DailyDataset, DailyManifest, write_bars};
    let (market, actions, template) = create(w)?;
    let source = DailyDataset::open(w.resolve(&market.path))?;
    let output = w.resolve(format!("data/demo-canonical-{}", Uuid::new_v4()));
    let root = output.join("market");
    fs::create_dir_all(&root)?;
    let ids = BTreeSet::from([
        "SH600000".to_string(),
        "SH600001".to_string(),
        "SH000905".to_string(),
    ]);
    for id in &ids {
        let raw = ["open", "high", "low", "close"]
            .map(|n| source.raw_field(id, n))
            .into_iter()
            .collect::<std::io::Result<Vec<_>>>()?;
        let adj = ["open", "high", "low", "close"]
            .map(|n| source.field(id, n))
            .into_iter()
            .collect::<std::io::Result<Vec<_>>>()?;
        let factor = source.field(id, "factor")?;
        let volume = source.field(id, "volume")?;
        let amount = source.field(id, "amount")?;
        let bars: Vec<_> = source
            .calendar
            .iter()
            .enumerate()
            .map(|(i, day)| DailyBar {
                instrument_id: fixture_security_id(id),
                trade_date: *day,
                open: raw[0].get(i),
                high: raw[1].get(i),
                low: raw[2].get(i),
                close: raw[3].get(i),
                adj_open: adj[0].get(i),
                adj_high: adj[1].get(i),
                adj_low: adj[2].get(i),
                adj_close: adj[3].get(i),
                volume: volume.get(i).zip(factor.get(i)).map(|(v, f)| v * f * 100.0),
                amount: amount.get(i).map(|v| v * 1000.0),
            })
            .collect();
        write_bars(
            &root
                .join("bars")
                .join(format!("{}.parquet", fixture_security_id(id))),
            &bars,
        )?;
        use analytics_storage::adjustment::{
            AdjustmentBasis, AdjustmentPoint, AdjustmentRecord, FactorSource,
        };
        let basis = if matches!(id.as_str(), "SH000905" | "SH000300") {
            AdjustmentBasis::PriceIndex {
                source: "JFQuant analytics-workflow synthetic-demo-v1".into(),
            }
        } else {
            let mut points = Vec::<AdjustmentPoint>::new();
            for (i, date) in source.calendar.iter().enumerate() {
                if let Some(f) = factor.get(i) {
                    let value = f.to_string();
                    if points.last().is_none_or(|p| p.factor != value) {
                        points.push(AdjustmentPoint {
                            effective_date: *date,
                            factor: value,
                        });
                    }
                }
            }
            AdjustmentBasis::Explicit {
                source: FactorSource::SyntheticFixture,
                source_url: "JFQuant analytics-workflow synthetic-demo-v1".into(),
                verified_through: *source.calendar.last().unwrap(),
                points,
            }
        };
        let record = AdjustmentRecord {
            schema_version: 3,
            instrument_id: fixture_security_id(id),
            source_symbol: id.clone(),
            basis,
        };
        record.validate()?;
        fs::create_dir_all(root.join("adjustments"))?;
        fs::write(
            root.join("adjustments")
                .join(format!("{}.json", fixture_security_id(id))),
            serde_json::to_vec_pretty(&record)?,
        )?;
    }
    let pool = source.pool("csi500")?;
    let mut adjustment_anchors = std::collections::BTreeMap::new();
    for id in &ids {
        let record =
            analytics_storage::adjustment::AdjustmentRecord::read(&root, &fixture_security_id(id))?;
        adjustment_anchors.insert(fixture_security_id(id), record.anchor(source.calendar[0])?);
    }
    let manifest = DailyManifest {
        schema_version: 5,
        calendar: source.calendar,
        universes: std::collections::BTreeMap::from([("csi500".into(), pool)]),
        instruments: ids.iter().map(|id| fixture_security_id(id)).collect(),
        price_basis: "unadjusted".into(),
        normalization: analytics_storage::adjustment::NORMALIZATION.into(),
        adjustment_anchors,
    };
    fs::write(
        root.join("daily_manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::write(
        output.join("jfquant_source.json"),
        serde_json::to_vec(
            &json!({"generator":"JFQuant analytics-workflow synthetic-demo-v1","synthetic_fixture":true,"demo_id":output.file_name().unwrap().to_string_lossy(),"input_format":"canonical_parquet"}),
        )?,
    )?;
    Ok((w.register(&root)?, actions, template))
}
