//! Explicit synthetic demo inputs for full app acceptance without public market data.
use crate::*;
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
                            "amount" => 100000.0,
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
                        "amount" => 100000.0,
                        _ => unreachable!(),
                    }
                })
                .collect();
            write(id, name, values)?;
        }
    }
    let dir = w.resolve(format!("{base}/actions"));
    fs::create_dir_all(dir.join("raw"))?;
    let actions = ["SH600000", "SH600001"]
        .map(|id| analytics_core::execution::CorporateAction {
            id: format!("{id}:2009-01-04"),
            instrument: id.into(),
            announced: "2009-01-02".parse().unwrap(),
            record_date: "2009-01-03".parse().unwrap(),
            ex_date: "2009-01-04".parse().unwrap(),
            pay_date: Some("2009-01-05".parse().unwrap()),
            listing_date: Some("2009-01-06".parse().unwrap()),
            cash_per_share: 0.1,
            shares_per_share: 0.1,
            source: "https://example.test/synthetic-fixture".into(),
        })
        .to_vec();
    let book = ActionBundle {
        schema_version: 1,
        downloader: "JFQuant analytics-workflow synthetic-demo-v1".into(),
        downloaded_at: now(),
        queried_symbols: BTreeSet::from(["SH600000".into(), "SH600001".into()]),
        actions,
        failures: Default::default(),
        raw_sha256: Default::default(),
    };
    fs::write(dir.join("actions.json"), serde_json::to_vec_pretty(&book)?)?;
    let m = w.register(&root)?;
    let a = w.register(&dir.join("actions.json"))?;
    let mut t = ResearchTemplate::builtin()?;
    t.name = "合成演示 · 流程和股数账本验证".into();
    t.first_test_year = 2009;
    t.last_year = 2010;
    t.config.training_years = 1;
    t.config.validation_years = 1;
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
    t.initial_cash_cny = 1000.0;
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
