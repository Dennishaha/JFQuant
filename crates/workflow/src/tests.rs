use super::*;
fn context() -> TaskContext {
    TaskContext::new(
        Uuid::new_v4(),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        |_| {},
    )
}
#[test]
fn recipes_validate_order_version_and_parameter_signatures() {
    let mut recipe = ResearchTemplate::builtin().unwrap();
    assert_eq!(recipe.config.factor_ids.len(), 178);
    let initial = signature(&recipe).unwrap();
    recipe.config.top_n = 30;
    assert_ne!(signature(&recipe).unwrap(), initial);
    recipe.config.factor_ids.swap(0, 1);
    let reordered = signature(&recipe).unwrap();
    recipe.config.factor_ids.swap(0, 1);
    assert_ne!(signature(&recipe).unwrap(), reordered);
    let tmp = tempfile::tempdir().unwrap();
    recipe.write(&tmp.path().join("recipe.json")).unwrap();
    assert_eq!(
        signature(&ResearchTemplate::read(&tmp.path().join("recipe.json")).unwrap()).unwrap(),
        signature(&recipe).unwrap()
    );
    recipe.config.factor_ids.push("alpha_054".into());
    assert!(recipe.validate().is_err());
    recipe.config.factor_ids.pop();
    recipe.formula_version = "old".into();
    assert!(recipe.validate().is_err());
}
#[test]
fn workspace_move_history_recovery_and_lease_keep_records_local() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    let w = Workspace::open(&root).unwrap();
    let lease = w.lease().unwrap();
    assert!(w.lease().is_err());
    let run = w
        .begin(Uuid::new_v4(), "test", json!({"frozen":17}))
        .unwrap();
    w.progress(&run, "completed stage").unwrap();
    drop(lease);
    w.recover().unwrap();
    let r = w.runs().unwrap().pop().unwrap();
    assert_eq!(r.status, RunStatus::Interrupted);
    assert_eq!(r.request["frozen"], 17);
    let moved = tmp.path().join("moved");
    fs::rename(root, &moved).unwrap();
    let w = Workspace::open(moved).unwrap();
    assert!(w.resolve(&r.output).join("events.jsonl").exists());
    assert_eq!(w.runs().unwrap()[0].id, r.id);
    w.progress(&run, "late progress").unwrap();
    assert_eq!(w.runs().unwrap()[0].status, RunStatus::Interrupted);
}
use crate::testing::synthetic;
#[test]
fn synthetic_annual_recipe_runs_native_model_and_dated_real_share_account() {
    let tmp = tempfile::tempdir().unwrap();
    let w = Workspace::open(tmp.path()).unwrap();
    let (m, a, t) = synthetic(&w);
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: m.id,
        actions: a.id,
        audit_only: false,
    };
    let c = context();
    let mut run = w
        .begin(c.id, "synthetic research", snapshot(&w, &request).unwrap())
        .unwrap();
    let result = execute(&w, &request, &run, &c).unwrap();
    assert_eq!(result.details["schema_version"], 9);
    assert!(
        result.details["aggregate"]["total_return"]
            .as_f64()
            .unwrap()
            .abs()
            < 1e-6
    );
    let ledger: Value = serde_json::from_slice(
        &fs::read(
            w.resolve(&run.output)
                .join("account-through-2009-01-12/ledger.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        ledger["positions"]
            .as_object()
            .unwrap()
            .values()
            .map(|p| p["shares"].as_u64().unwrap())
            .sum::<u64>(),
        110
    );
    assert_eq!(ledger["cash"], 10.0);
    let report = fs::read_to_string(w.resolve(&run.output).join("report.md")).unwrap();
    assert!(report.contains("合成的恒定指数，仅用于验收"));
    assert!(!report.contains("本项目独立下载行情"));
    w.finish(&mut run, RunStatus::Succeeded, None).unwrap();
    assert_eq!(w.runs().unwrap()[0].status, RunStatus::Succeeded);
}
#[test]
fn incomplete_action_input_blocks_before_factor_cache_or_model_creation() {
    let tmp = tempfile::tempdir().unwrap();
    let w = Workspace::open(tmp.path()).unwrap();
    let (m, a, t) = synthetic(&w);
    let path = w.resolve(&a.path);
    let mut book = ActionBundle::read(&path).unwrap();
    book.actions[0].pay_date = None;
    fs::write(&path, serde_json::to_vec(&book).unwrap()).unwrap();
    let a = w.register(&path).unwrap();
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: m.id,
        actions: a.id,
        audit_only: false,
    };
    let c = context();
    let run = w
        .begin(c.id, "blocked research", snapshot(&w, &request).unwrap())
        .unwrap();
    assert!(execute(&w, &request, &run, &c).is_err());
    assert!(!w.resolve(&run.output).join("factor-cache").exists());
    assert!(!w.resolve(&run.output).join("summary.json").exists());
    assert_eq!(w.results(&run).unwrap()["ready"], false);
}

#[test]
fn changed_registered_actions_are_rejected_and_new_registration_keeps_old_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let w = Workspace::open(tmp.path()).unwrap();
    let (m, a, t) = synthetic(&w);
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: m.id,
        actions: a.id.clone(),
        audit_only: true,
    };
    let original = snapshot(&w, &request).unwrap();
    let p = w.resolve(&a.path);
    let mut book = ActionBundle::read(&p).unwrap();
    book.actions[0].pay_date = Some("2009-01-07".parse().unwrap());
    fs::write(p, serde_json::to_vec(&book).unwrap()).unwrap();
    assert!(snapshot(&w, &request).is_err());
    let new = w.register(&w.resolve(&a.path)).unwrap();
    assert_ne!(new.id, a.id);
    assert_eq!(original["inputs"][1]["signature"], a.signature);
}

#[test]
fn market_bytes_and_action_evidence_are_verified_before_execution() {
    let tmp = tempfile::tempdir().unwrap();
    let w = Workspace::open(tmp.path()).unwrap();
    let (m, a, t) = synthetic(&w);
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: m.id.clone(),
        actions: a.id.clone(),
        audit_only: false,
    };
    let frozen = snapshot(&w, &request).unwrap();
    let root = w.resolve(&m.path);
    // Provenance is unchanged, but the data itself is edited after acceptance.
    fs::write(root.join("unexpected.bin"), b"changed input").unwrap();
    let c = context();
    let run = w.begin(c.id, "changed input", frozen).unwrap();
    let error = execute(&w, &request, &run, &c).unwrap_err().to_string();
    assert!(error.contains("market bytes changed"), "{error}");
    assert!(!w.resolve(&run.output).join("summary.json").exists());
    let new = w.register(&root).unwrap();
    assert_ne!(new.id, m.id);
    fs::remove_file(root.join("unexpected.bin")).unwrap();
    assert!(version(&w, &m.id, "market").is_ok());
    let path = w.resolve(&a.path);
    let raw = path.parent().unwrap().join("raw");
    fs::create_dir_all(&raw).unwrap();
    fs::write(raw.join("proof.txt"), b"original").unwrap();
    let mut book = ActionBundle::read(&path).unwrap();
    book.raw_sha256.insert(
        "proof.txt".into(),
        format!("sha256:{:x}", Sha256::digest(b"original")),
    );
    fs::write(&path, serde_json::to_vec(&book).unwrap()).unwrap();
    let a = w.register(&path).unwrap();
    fs::write(raw.join("proof.txt"), b"changed").unwrap();
    assert!(
        version(&w, &a.id, "actions")
            .unwrap_err()
            .to_string()
            .contains("evidence hash mismatch")
    );
}

#[test]
fn canonical_parquet_and_qlib_preserve_account_fees_and_feature_order() {
    let dir = tempfile::tempdir().unwrap();
    let w = Workspace::open(dir.path()).unwrap();
    let (mq, aq, tq) = demo::create(&w).unwrap();
    let (mp, ap, tp) = demo::create_canonical(&w).unwrap();
    let run = |m: DataVersion, a: DataVersion, t: ResearchTemplate| {
        let c = context();
        let request = WorkflowRequest::Research {
            template: Box::new(t),
            market: m.id,
            actions: a.id,
            audit_only: false,
        };
        let r = w
            .begin(c.id, "parity", snapshot(&w, &request).unwrap())
            .unwrap();
        let out = execute(&w, &request, &r, &c).unwrap();
        (out.details, r)
    };
    let (q, rq) = run(mq, aq, tq);
    let (p, rp) = run(mp, ap, tp);
    assert_eq!(
        q["execution"]["research_config"]["factor_ids"],
        p["execution"]["research_config"]["factor_ids"]
    );
    assert!(
        q["execution"]["research_config"]["factor_ids"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    );
    assert_eq!(q["aggregate"], p["aggregate"]);
    for year in [2009, 2010] {
        let ledger = |r: &RunRecord| {
            serde_json::from_slice::<Value>(
                &fs::read(
                    w.resolve(&r.output)
                        .join(format!("account-through-{year}-01-12/ledger.json")),
                )
                .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(ledger(&rq), ledger(&rp));
    }
}
#[test]
fn pools_are_immutable_portable_and_reject_past_coverage_gaps() {
    use analytics_core::{
        DateRange,
        pool::{PoolAdjustment, PoolDefinition, PoolExpression},
    };
    let dir = tempfile::tempdir().unwrap();
    let w = Workspace::open(dir.path()).unwrap();
    let (market, _, _) = demo::create(&w).unwrap();
    let mut definition = PoolDefinition {
        schema_version: 2,
        name: "CSI500 fixture".into(),
        coverage: DateRange {
            start: "2007-01-02".parse().unwrap(),
            end: "2010-01-12".parse().unwrap(),
        },
        expression: PoolExpression::Universe {
            version: market.id.clone(),
            universe: "csi500".into(),
        },
        adjustments: vec![],
    };
    let first = w.save_pool(definition.clone()).unwrap();
    let past = w
        .preview_pool(&first.id, &market.id, "2008-01-03".parse().unwrap())
        .unwrap();
    definition.adjustments.push(PoolAdjustment {
        effective_date: "2009-01-04".parse().unwrap(),
        add: BTreeSet::new(),
        remove: BTreeSet::from(["SH600000".into()]),
    });
    let second = w.save_pool(definition).unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(
        past["instruments"],
        w.preview_pool(&second.id, &market.id, "2008-01-03".parse().unwrap())
            .unwrap()["instruments"]
    );
    assert_eq!(w.pools().unwrap().len(), 2);
    let after = w
        .preview_pool(&second.id, &market.id, "2009-01-04".parse().unwrap())
        .unwrap();
    assert_eq!(after["instruments"].as_array().unwrap().len(), 1);
    let mut future = first.definition.clone();
    future.expression = PoolExpression::Symbols {
        instruments: BTreeSet::from(["SH600000".into()]),
    };
    future.coverage.start = "2006-01-01".parse().unwrap();
    future.coverage.end = "2012-12-31".parse().unwrap();
    let future = w.save_pool(future).unwrap();
    let (resolved, _) = w.resolve_pool(&future.id, &market.id).unwrap();
    let dataset = DailyDataset::open(w.resolve(&market.path)).unwrap();
    assert_eq!(resolved.coverage.start, dataset.calendar[0]);
    assert_eq!(resolved.coverage.end, *dataset.calendar.last().unwrap());
    assert!(
        w.preview_pool(&future.id, &market.id, "2011-01-03".parse().unwrap())
            .unwrap_err()
            .to_string()
            .contains("outside verified coverage")
    );
    let mut t = ResearchTemplate::builtin_csi300().unwrap();
    assert_eq!(t.benchmark, "SH000300");
    t.schema_version = 1;
    assert!(t.validate().is_err());
}

#[test]
fn unexplained_historical_security_codes_are_readiness_issues_not_training_failures() {
    let dir = tempfile::tempdir().unwrap();
    let w = Workspace::open(dir.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    let root = w.resolve(&m.path);
    fs::write(
        root.join("instruments/csi300.txt"),
        "SHT00018\t2007-01-02\t2010-01-12\n",
    )
    .unwrap();
    let market = w.register(&root).unwrap();
    t.pool_version = "builtin:csi300".into();
    let c = context();
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: market.id,
        actions: a.id,
        audit_only: true,
    };
    let r = w
        .begin(
            c.id,
            "unsupported-security audit",
            snapshot(&w, &request).unwrap(),
        )
        .unwrap();
    let out = execute(&w, &request, &r, &c).unwrap();
    assert_eq!(out.details["ready"], false);
    assert!(out.details["reason"].as_str().unwrap().contains("SHT00018"));
    assert!(!w.resolve(&r.output).join("factor-cache").exists());
}

#[test]
fn independent_coefficient_conflicts_in_training_history_stop_before_cache_and_training() {
    let dir = tempfile::tempdir().unwrap();
    let w = Workspace::open(dir.path()).unwrap();
    let (m, a, t) = demo::create_canonical(&w).unwrap();
    let root = w.resolve(&m.path);
    let path = root.join(format!(
        "adjustments/{}.json",
        demo::fixture_security_id("SH600000")
    ));
    let mut record = analytics_storage::adjustment::AdjustmentRecord::read(
        &root,
        &demo::fixture_security_id("SH600000"),
    )
    .unwrap();
    if let analytics_storage::adjustment::AdjustmentBasis::Explicit { points, .. } =
        &mut record.basis
    {
        points.insert(
            1,
            analytics_storage::adjustment::AdjustmentPoint {
                effective_date: "2008-01-02".parse().unwrap(),
                factor: "0.101".into(),
            },
        );
    }
    fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
    let m = w.register(&root).unwrap();
    let c = context();
    let request = WorkflowRequest::Research {
        template: Box::new(t.clone()),
        market: m.id.clone(),
        actions: a.id.clone(),
        audit_only: true,
    };
    let r = w
        .begin(c.id, "price-basis audit", snapshot(&w, &request).unwrap())
        .unwrap();
    let out = execute(&w, &request, &r, &c).unwrap();
    assert_eq!(out.details["ready"], false);
    assert!(
        out.details["adjustment_issues"][demo::fixture_security_id("SH600000")]
            .to_string()
            .contains("unexplained coefficient change")
    );
    assert_eq!(
        serde_json::from_value::<Vec<String>>(out.details["affected_factor_ids"].clone()).unwrap(),
        t.config.factor_ids
    );
    assert!(!w.resolve(&r.output).join("factor-cache").exists());
    let c = context();
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: m.id,
        actions: a.id,
        audit_only: false,
    };
    let r = w
        .begin(c.id, "invalid price basis", snapshot(&w, &request).unwrap())
        .unwrap();
    assert!(
        execute(&w, &request, &r, &c)
            .unwrap_err()
            .to_string()
            .contains("data_not_ready")
    );
    assert!(!w.resolve(&r.output).join("factor-cache").exists());
}

#[test]
fn historical_adjustment_terms_do_not_require_old_settlement_dates_but_execution_stays_strict() {
    let dir = tempfile::tempdir().unwrap();
    let w = Workspace::open(dir.path()).unwrap();
    let (m, a, mut t) = demo::create_canonical(&w).unwrap();
    let path = w.resolve(&a.path);
    let mut book = ActionBundle::read(&path).unwrap();
    for event in &mut book.actions {
        event.pay_date = None;
        event.listing_date = None;
    }
    fs::write(&path, serde_json::to_vec(&book).unwrap()).unwrap();
    let a = w.register(&path).unwrap();
    // The 2009 distribution affects 2010 training inputs but never the fresh 2010 account.
    if let analytics_core::schedule::Schedule::Rolling { first, .. } = &mut t.schedule {
        first.train = analytics_core::DateRange {
            start: "2008-01-01".parse().unwrap(),
            end: "2008-12-31".parse().unwrap(),
        };
        first.validation = Some(analytics_core::DateRange {
            start: "2009-01-01".parse().unwrap(),
            end: "2009-12-31".parse().unwrap(),
        });
        first.test = analytics_core::DateRange {
            start: "2010-01-01".parse().unwrap(),
            end: "2010-12-31".parse().unwrap(),
        };
    }
    let c = context();
    let request = WorkflowRequest::Research {
        template: Box::new(t.clone()),
        market: m.id.clone(),
        actions: a.id.clone(),
        audit_only: false,
    };
    let r = w
        .begin(c.id, "historical terms", snapshot(&w, &request).unwrap())
        .unwrap();
    let out = execute(&w, &request, &r, &c).unwrap();
    assert_eq!(out.details["schema_version"], 9);
    assert_eq!(out.details["annual_reports"][0]["year"], 2010);
    t = demo::create_canonical(&w).unwrap().2;
    let c = context();
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: m.id,
        actions: a.id,
        audit_only: true,
    };
    let r = w
        .begin(
            c.id,
            "current rights missing settlement",
            snapshot(&w, &request).unwrap(),
        )
        .unwrap();
    let out = execute(&w, &request, &r, &c).unwrap();
    assert_eq!(out.details["ready"], false);
    assert!(out.details.to_string().contains("pay date"));
    assert!(!w.resolve(&r.output).join("factor-cache").exists());
}

#[test]
fn source_adjusted_quotes_are_audit_only_and_report_provenance_is_required() {
    let dir = tempfile::tempdir().unwrap();
    let w = Workspace::open(dir.path()).unwrap();
    let (m, a, t) = demo::create_canonical(&w).unwrap();
    let root = w.resolve(&m.path);
    let path = root.join(format!(
        "bars/{}.parquet",
        demo::fixture_security_id("SH600000")
    ));
    let mut bars = analytics_storage::daily::read_bars(&path).unwrap();
    for row in &mut bars {
        row.adj_open = Some(-20.);
        row.adj_close = Some(-30.);
        row.adj_high = Some(-10.);
        row.adj_low = Some(-40.);
    }
    analytics_storage::daily::write_bars(&path, &bars).unwrap();
    let m = w.register(&root).unwrap();
    let c = context();
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: m.id,
        actions: a.id,
        audit_only: false,
    };
    let r = w
        .begin(
            c.id,
            "archive quote separation",
            snapshot(&w, &request).unwrap(),
        )
        .unwrap();
    let out = execute(&w, &request, &r, &c).unwrap();
    assert_eq!(out.details["schema_version"], 9);
    let input = &out.details["execution"]["price_inputs"]["securities"]
        [demo::fixture_security_id("SH600000")];
    let frozen = if input["source"].is_object() {
        &input["source"]["securities"]["SH600000"]
    } else {
        input
    };
    assert_eq!(frozen["basis"]["source"], "synthetic_fixture");
    assert!(frozen["anchor"]["factor"].is_string());
    let mut bad = out.details.clone();
    bad["schema_version"] = json!(5);
    assert!(
        analytics_ml::backtest::validate_result(&bad)
            .unwrap_err()
            .to_string()
            .contains("unsupported_result_format")
    );
    let mut bad = out.details.clone();
    bad["execution"]["price_inputs"]["securities"][demo::fixture_security_id("SH600000")]["anchor"]
        ["factor"] = json!("999");
    assert!(analytics_ml::backtest::validate_result(&bad).is_err());
    for field in ["anchor", "basis", "sha256"] {
        let mut bad = out.details.clone();
        bad["execution"]["price_inputs"]["securities"][demo::fixture_security_id("SH600000")]
            .as_object_mut()
            .unwrap()
            .remove(field);
        bad["execution"]["price_input_sha256"] = json!(format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(&bad["execution"]["price_inputs"]).unwrap())
        ));
        assert!(
            analytics_ml::backtest::validate_result(&bad).is_err(),
            "{field}"
        );
    }
}

#[test]
fn missing_reverse_special_and_unpriced_distribution_terms_block_every_entry_before_training() {
    use analytics_storage::adjustment::{AdjustmentBasis, AdjustmentRecord};
    for (mode, expected) in [
        ("missing_jump", "without coefficient change"),
        ("reverse", "disagrees"),
        ("special", "disagrees"),
        ("missing_reference", "reference price"),
        ("first_reference", "reference price"),
        ("mixed_origin", "provenance mismatch"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let w = Workspace::open(dir.path()).unwrap();
        let (m, mut a, t) = demo::create_canonical(&w).unwrap();
        let root = w.resolve(&m.path);
        let mut record =
            AdjustmentRecord::read(&root, &demo::fixture_security_id("SH600000")).unwrap();
        if let AdjustmentBasis::Explicit {
            points,
            source,
            source_url,
            ..
        } = &mut record.basis
        {
            match mode {
                "missing_jump" => points[1].factor = points[0].factor.clone(),
                "reverse" => points[1].factor = "0.09".into(),
                "special" => points[1].factor = "0.2".into(),
                "mixed_origin" => {
                    *source = analytics_storage::adjustment::FactorSource::SinaHfq;
                    *source_url =
                        "https://finance.sina.com.cn/realstock/company/sh600000/hfq.js".into();
                }
                _ => {}
            }
        }
        fs::write(
            root.join(format!(
                "adjustments/{}.json",
                demo::fixture_security_id("SH600000")
            )),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        if mode == "missing_reference" {
            let path = root.join(format!(
                "bars/{}.parquet",
                demo::fixture_security_id("SH600000")
            ));
            let mut rows = analytics_storage::daily::read_bars(&path).unwrap();
            rows.iter_mut()
                .find(|b| b.trade_date == "2009-01-03".parse::<analytics_core::Date>().unwrap())
                .unwrap()
                .close = None;
            analytics_storage::daily::write_bars(&path, &rows).unwrap();
        }
        if mode == "first_reference" {
            let path = w.resolve(&a.path);
            let mut book: ActionBundle = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let mut event = book.actions[0].clone();
            event.id = format!("{}:2007-01-02", event.instrument);
            event.announced = "2007-01-01".parse().unwrap();
            event.record_date = event.announced;
            event.ex_date = "2007-01-02".parse().unwrap();
            event.pay_date = Some("2007-01-03".parse().unwrap());
            event.listing_date = Some("2007-01-04".parse().unwrap());
            book.actions.push(event);
            fs::write(&path, serde_json::to_vec(&book).unwrap()).unwrap();
            a = w.register(&path).unwrap();
        }
        let m = w.register(&root).unwrap();
        for audit_only in [true, false] {
            let c = context();
            let request = WorkflowRequest::Research {
                template: Box::new(t.clone()),
                market: m.id.clone(),
                actions: a.id.clone(),
                audit_only,
            };
            let r = w
                .begin(c.id, mode, snapshot(&w, &request).unwrap())
                .unwrap();
            let result = execute(&w, &request, &r, &c);
            if audit_only {
                let out = result.unwrap();
                assert_eq!(out.details["ready"], false);
                assert!(
                    out.details.to_string().contains(expected),
                    "{mode}: {}",
                    out.details
                );
                assert_eq!(
                    out.details["affected_factor_ids"],
                    json!(t.config.factor_ids)
                );
            } else {
                assert!(result.unwrap_err().to_string().contains("data_not_ready"));
            }
            assert!(!w.resolve(&r.output).join("factor-cache").exists());
            assert!(!w.resolve(&r.output).join("fold-0000").exists());
        }
    }
}

#[test]
fn arbitrary_date_folds_without_validation_keep_gaps_and_tail_predictions() {
    use analytics_core::{
        DateRange,
        schedule::{Fold, Schedule},
    };
    let dir = tempfile::tempdir().unwrap();
    let w = Workspace::open(dir.path()).unwrap();
    let (m, a, mut t) = demo::create_canonical(&w).unwrap();
    let r = |start: &str, end: &str| DateRange {
        start: start.parse().unwrap(),
        end: end.parse().unwrap(),
    };
    let first = Fold {
        train: r("2007-01-02", "2008-01-12"),
        validation: None,
        test: r("2009-01-02", "2009-01-05"),
    };
    let second = Fold {
        train: first.train.clone(),
        validation: None,
        test: r("2009-01-09", "2009-01-12"),
    };
    t.schedule = Schedule::Manual {
        folds: vec![second, first],
    };
    t.config.training.early_stopping_rounds = Some(10);
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: m.id,
        actions: a.id,
        audit_only: false,
    };
    let c = context();
    let run = w
        .begin(c.id, "date folds", snapshot(&w, &request).unwrap())
        .unwrap();
    let out = execute(&w, &request, &run, &c).unwrap();
    assert_eq!(out.details["fold_reports"].as_array().unwrap().len(), 2);
    assert_eq!(out.details["annual_reports"].as_array().unwrap().len(), 1);
    let path = w.resolve(&run.output);
    let days: Vec<Value> =
        serde_json::from_slice(&fs::read(path.join("daily.json")).unwrap()).unwrap();
    assert_eq!(days.len(), 11);
    let gaps: Vec<Value> =
        serde_json::from_slice(&fs::read(path.join("fold-0001/gap_daily.json")).unwrap()).unwrap();
    assert_eq!(gaps.len(), 3);
    let predictions = fs::read_to_string(path.join("fold-0001/predictions.csv")).unwrap();
    assert!(
        predictions
            .lines()
            .any(|l| l.starts_with("2009-01-12,") && l.ends_with(','))
    );
    assert_eq!(out.details["fold_reports"][0]["validation_rows"], 0);
}

#[test]
fn current_date_schedule_rejects_old_year_parameters_and_previews_actual_windows() {
    let mut value = serde_json::to_value(ResearchTemplate::builtin_csi300().unwrap()).unwrap();
    assert_eq!(value["schedule"]["first"]["test"]["start"], "2012-01-01");
    value["first_test_year"] = json!(2012);
    assert!(serde_json::from_value::<ResearchTemplate>(value).is_err());
    let dir = tempfile::tempdir().unwrap();
    let w = Workspace::open(dir.path()).unwrap();
    let (m, _, t) = demo::create_canonical(&w).unwrap();
    let out = research_requirements(&w, &t, &m.id).unwrap();
    assert_eq!(out["resolved_folds"].as_array().unwrap().len(), 2);
    assert_eq!(
        out["resolved_folds"][1]["actual"]["test"]["end"],
        "2010-01-12"
    );
    assert_eq!(out["securities"].as_array().unwrap().len(), 2);
}

#[test]
fn identity_packages_are_immutable_portable_and_bound_to_research_outputs() {
    let root = tempfile::tempdir().unwrap();
    let w = Workspace::open(root.path()).unwrap();
    let (_, _, t) = demo::create(&w).unwrap();
    let v = w.identity_version(&t.identity_version).unwrap();
    let export = root.path().join("export");
    w.export_identities(&v.id, &export).unwrap();
    let other = tempfile::tempdir().unwrap();
    let moved = Workspace::open(other.path()).unwrap();
    let imported = moved
        .import_identities(&export.join("registry.json"))
        .unwrap();
    assert_eq!(v.signature, imported.signature);
    assert_eq!(v.id, imported.id);
    let mut record = moved.identities(&v.id).unwrap();
    record.reason = "new audited revision".into();
    atomic_json(&export.join("registry.json"), &record).unwrap();
    let revision = moved
        .import_identities(&export.join("registry.json"))
        .unwrap();
    assert_ne!(revision.id, v.id);
    assert_eq!(
        moved.identities(&v.id).unwrap().reason,
        "complete independently generated acceptance fixture"
    );
}

#[test]
fn missing_event_categories_have_explicit_checks_and_never_create_factor_cache() {
    let dir = tempfile::tempdir().unwrap();
    let w = Workspace::open(dir.path()).unwrap();
    let (m, a, t) = demo::create(&w).unwrap();
    let path = w.resolve(a.path);
    let mut book = ActionBundle::read(&path).unwrap();
    for claims in book.coverage.values_mut() {
        claims.retain(|c| c.category == "distribution");
    }
    atomic_json(&path, &book).unwrap();
    let a = w.register(&path).unwrap();
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: m.id,
        actions: a.id,
        audit_only: true,
    };
    let c = context();
    let run = w
        .begin(
            c.id,
            "incomplete category audit",
            snapshot(&w, &request).unwrap(),
        )
        .unwrap();
    let result = execute(&w, &request, &run, &c).unwrap();
    assert_eq!(result.details["ready"], false);
    assert!(
        result.details["adjustment_issues"]
            .to_string()
            .contains("merger")
    );
    let admission: analytics_core::admission::AdmissionReport =
        serde_json::from_slice(&fs::read(w.resolve(&run.output).join("admission.json")).unwrap())
            .unwrap();
    assert!(!admission.ready());
    assert!(
        admission
            .checks
            .iter()
            .any(|c| c.status == analytics_core::admission::CheckStatus::NotRun)
    );
    assert!(!w.resolve(run.output).join("factor-cache").exists());
}

#[test]
fn a_selected_all_null_feature_blocks_before_publishing_cache_or_training() {
    let dir = tempfile::tempdir().unwrap();
    let w = Workspace::open(dir.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    t.config.factor_ids = vec!["alpha_025".into()];
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: m.id,
        actions: a.id,
        audit_only: true,
    };
    let c = context();
    let run = w
        .begin(
            c.id,
            "all-null eligibility",
            snapshot(&w, &request).unwrap(),
        )
        .unwrap();
    let out = execute(&w, &request, &run, &c).unwrap();
    assert_eq!(out.details["ready"], false);
    assert!(
        out.details["feature_issues"]
            .to_string()
            .contains("alpha_025")
    );
    assert!(!w.resolve(&run.output).join("factor-cache").exists());
    assert!(!w.resolve(run.output).join("fold-0000").exists());
}

#[test]
fn unknown_benchmark_does_not_hide_independent_settlement_failures() {
    let d = tempfile::tempdir().unwrap();
    let w = Workspace::open(d.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    t.benchmark = "SEC-ffffffffffffffffffffffffffffffff".into();
    let path = w.resolve(a.path);
    let mut book = ActionBundle::read(&path).unwrap();
    book.actions[0].pay_date = None;
    atomic_json(&path, &book).unwrap();
    let a = w.register(&path).unwrap();
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: m.id,
        actions: a.id,
        audit_only: true,
    };
    let c = context();
    let run = w
        .begin(
            c.id,
            "independent benchmark audit",
            snapshot(&w, &request).unwrap(),
        )
        .unwrap();
    let out = execute(&w, &request, &run, &c).unwrap();
    assert_eq!(out.details["ready"], false);
    assert!(
        !out.details["settlement_issues"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    assert_eq!(out.details["checks"]["benchmark"], "failed");
    assert!(!w.resolve(run.output).join("factor-cache").exists());
}
