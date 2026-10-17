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
        actions: Some(a.id),
        audit_only: false,
    };
    let c = context();
    let mut run = w
        .begin(c.id, "synthetic research", snapshot(&w, &request).unwrap())
        .unwrap();
    let result = execute(&w, &request, &run, &c).unwrap();
    assert_eq!(
        result.details["schema_version"],
        crate::research::backtest::RESEARCH_SCHEMA_VERSION
    );
    assert!(
        result.details["aggregate"]["total_return"]
            .as_f64()
            .unwrap()
            .abs()
            < 1e-6
    );
    assert_eq!(result.details["backtest_status"], "completed");
    let snapshots: Value = serde_json::from_slice(
        &fs::read(w.resolve(&run.output).join("portfolio_snapshots.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        snapshots["2009-01-12"]["positions"],
        snapshots["2010-01-12"]["positions"]
    );
    assert_eq!(
        snapshots["2009-01-12"]["cash"],
        snapshots["2010-01-12"]["cash"]
    );
    let report = fs::read_to_string(w.resolve(&run.output).join("report.md")).unwrap();
    assert!(report.contains("合成演示"));
    assert!(report.contains("除权除息日"));
    w.finish(&mut run, RunStatus::Succeeded, None).unwrap();
    assert_eq!(w.runs().unwrap()[0].status, RunStatus::Succeeded);
}

#[test]
fn changed_registered_actions_are_rejected_and_new_registration_keeps_old_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let w = Workspace::open(tmp.path()).unwrap();
    let (m, a, t) = synthetic(&w);
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: m.id,
        actions: Some(a.id.clone()),
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
        actions: Some(a.id.clone()),
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
        format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(b"original"))
        ),
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
            actions: Some(a.id),
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
    let snapshots = |r: &RunRecord| {
        serde_json::from_slice::<Value>(
            &fs::read(w.resolve(&r.output).join("portfolio_snapshots.json")).unwrap(),
        )
        .unwrap()
    };
    assert_eq!(snapshots(&rq), snapshots(&rp));
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
        schema_version: 3,
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
        actions: Some(a.id),
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
    assert_eq!(
        out.details["references"]["input_issues"]["path"],
        "input_issues.json"
    );
    let issues: Value =
        serde_json::from_slice(&fs::read(w.resolve(&r.output).join("input_issues.json")).unwrap())
            .unwrap();
    assert!(issues["market_issues"].get("SHT00018").is_some());
    assert!(issues["factor_basis_issues"].get("SHT00018").is_some());
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
        actions: Some(a.id.clone()),
        audit_only: true,
    };
    let r = w
        .begin(c.id, "price-basis audit", snapshot(&w, &request).unwrap())
        .unwrap();
    let out = execute(&w, &request, &r, &c).unwrap();
    assert_eq!(out.details["ready"], false);
    assert!(
        serde_json::from_slice::<Value>(
            &fs::read(w.resolve(&r.output).join("input_issues.json")).unwrap()
        )
        .unwrap()["adjustment_issues"][demo::fixture_security_id("SH600000")]
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
        actions: Some(a.id),
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
        actions: Some(a.id.clone()),
        audit_only: false,
    };
    let r = w
        .begin(c.id, "historical terms", snapshot(&w, &request).unwrap())
        .unwrap();
    let out = execute(&w, &request, &r, &c).unwrap();
    assert_eq!(
        out.details["schema_version"],
        crate::research::backtest::RESEARCH_SCHEMA_VERSION
    );
    assert_eq!(out.details["annual_reports"][0]["year"], 2010);
    t = demo::create_canonical(&w).unwrap().2;
    let c = context();
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: m.id,
        actions: Some(a.id),
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
    assert_eq!(out.details["ready"], true);
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
        actions: Some(a.id),
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
    assert_eq!(
        out.details["schema_version"],
        crate::research::backtest::RESEARCH_SCHEMA_VERSION
    );
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
        crate::research::backtest::validate_result(&bad)
            .unwrap_err()
            .to_string()
            .contains("unsupported_result_format")
    );
    let mut bad = out.details.clone();
    bad["execution"]["price_inputs"]["securities"][demo::fixture_security_id("SH600000")]["anchor"]
        ["factor"] = json!("999");
    assert!(crate::research::backtest::validate_result(&bad).is_err());
    for field in ["anchor", "basis", "sha256"] {
        let mut bad = out.details.clone();
        bad["execution"]["price_inputs"]["securities"][demo::fixture_security_id("SH600000")]
            .as_object_mut()
            .unwrap()
            .remove(field);
        bad["execution"]["price_input_sha256"] = json!(format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(
                serde_json::to_vec(&bad["execution"]["price_inputs"]).unwrap()
            ))
        ));
        assert!(
            crate::research::backtest::validate_result(&bad).is_err(),
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
                    *source = analytics_storage::adjustment::FactorSource::TdxForward;
                    *source_url = "tdxaidata:get_market_data:none:ForwardFactor".into();
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
            event.announced = Some("2007-01-01".parse().unwrap());
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
                actions: Some(a.id.clone()),
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
        actions: Some(a.id),
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
    assert_eq!(
        days.iter()
            .filter(|d| d["date"]
                .as_str()
                .is_some_and(|d| d > "2009-01-05" && d < "2009-01-09"))
            .count(),
        3
    );
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
fn a_selected_all_null_feature_blocks_before_publishing_cache_or_training() {
    let dir = tempfile::tempdir().unwrap();
    let w = Workspace::open(dir.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    t.config.factor_ids = vec!["alpha_025".into()];
    let request = WorkflowRequest::Research {
        template: Box::new(t),
        market: m.id,
        actions: Some(a.id),
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

fn execute_stage(w: &Workspace, request: WorkflowRequest) -> (RunRecord, Result<WorkflowOutcome>) {
    let c = context();
    let run = w
        .begin(c.id, &request.label(), snapshot(w, &request).unwrap())
        .unwrap();
    let out = execute(w, &request, &run, &c);
    (run, out)
}
#[test]
fn independent_training_survives_settlement_gap_and_repaired_backtest_does_not_refit() {
    let tmp = tempfile::tempdir().unwrap();
    let w = Workspace::open(tmp.path()).unwrap();
    let (m, a, t) = demo::create(&w).unwrap();
    let mut execution = t.execution_settings();
    execution.initial_cash_cny = 500_000.0;
    execution.top_n = 1;
    execution.rebalance_days = 2;
    execution.market_rules.commission.minimum_cny = 9.0;
    let path = w.resolve(&a.path);
    let mut book = ActionBundle::read(&path).unwrap();
    book.actions.iter_mut().for_each(|a| a.pay_date = None);
    atomic_json(&path, &book).unwrap();
    let bad = w.register(&path).unwrap();
    let (run, out) = execute_stage(
        &w,
        WorkflowRequest::TrainResearch {
            template: Box::new(t),
            market: m.id,
            actions: Some(bad.id.clone()),
        },
    );
    out.unwrap();
    let root = w.resolve(&run.output);
    let original = fs::read(root.join("fold-0000/model.txt")).unwrap();
    assert!(root.join("training.json").exists());
    assert!(!root.join("summary.json").exists());
    assert_eq!(w.results(&run).unwrap()["status"], "trained");
    let (bt, result) = execute_stage(
        &w,
        WorkflowRequest::BacktestResearch {
            execution: None,
            event_check_policy: Default::default(),
            training: run.id,
            actions: bad.id,
        },
    );
    let result = result.unwrap();
    let baseline_factor_signature = result.details["factor_signature"].clone();
    assert_eq!(result.details["backtest_status"], "completed");
    assert!(w.resolve(bt.output).join("order_events.jsonl").exists());
    assert_eq!(
        fs::read(root.join("fold-0000/model.txt")).unwrap(),
        original
    );
    book.actions
        .iter_mut()
        .for_each(|a| a.pay_date = Some("2009-01-05".parse().unwrap()));
    atomic_json(&path, &book).unwrap();
    let fixed = w.register(&path).unwrap();
    let (cached_run, result) = execute_stage(
        &w,
        WorkflowRequest::BacktestResearch {
            execution: Some(Box::new(execution)),
            event_check_policy: Default::default(),
            training: run.id,
            actions: fixed.id.clone(),
        },
    );
    let details = result.unwrap().details;
    let cached = w.resolve(cached_run.output);
    let receipt: Value =
        serde_json::from_slice(&fs::read(cached.join("prediction_manifest.json")).unwrap())
            .unwrap();
    assert_eq!(receipt["cache_hit"], true);
    assert!(!cached.join("prediction-factor-cache").exists());

    assert_eq!(details["backtest_status"], "completed");
    assert_eq!(details["execution"]["initial_cash_cny"], 500_000.0);
    assert_eq!(details["execution"]["research_config"]["top_n"], 1);
    assert_eq!(details["execution"]["research_config"]["rebalance_days"], 2);
    let descriptor: Value =
        serde_json::from_slice(&fs::read(root.join("training.json")).unwrap()).unwrap();
    assert_eq!(
        details["execution"]["training_provenance"]["training_signature"],
        descriptor["training_signature"]
    );
    assert_eq!(details["factor_signature"], baseline_factor_signature);
    assert_eq!(
        fs::read(root.join("fold-0000/model.txt")).unwrap(),
        original
    );
    book.actions[0].cash_per_share = (0.2).to_string();
    atomic_json(&path, &book).unwrap();
    let changed = w.register(&path).unwrap();
    let (_, out) = execute_stage(
        &w,
        WorkflowRequest::BacktestResearch {
            execution: None,
            event_check_policy: Default::default(),
            training: run.id,
            actions: changed.id,
        },
    );
    assert!(
        out.unwrap_err()
            .to_string()
            .contains("training_price_basis_changed")
    );
}
#[test]
fn known_execution_conflict_blocks_on_actual_exposure_before_return_publication() {
    let tmp = tempfile::tempdir().unwrap();
    let w = Workspace::open(tmp.path()).unwrap();
    let (m, a, t) = demo::create(&w).unwrap();
    let path = w.resolve(&a.path);
    let mut book = ActionBundle::read(&path).unwrap();
    book.execution_issues
        .push(analytics_storage::execution_data::ExecutionIssue {
            id: String::new(),
            event_category: String::new(),
            evidence: Vec::new(),
            instrument: book.actions[0].instrument.clone(),
            announced: None,
            dates: analytics_core::DateRange {
                start: "2009-01-02".parse().unwrap(),
                end: "2010-01-12".parse().unwrap(),
            },
            code: "source_conflict".into(),
            message: "fixture conflict".into(),
        });
    atomic_json(&path, &book).unwrap();
    let a = w.register(&path).unwrap();
    let (run, out) = execute_stage(
        &w,
        WorkflowRequest::Research {
            template: Box::new(t),
            market: m.id,
            actions: Some(a.id),
            audit_only: false,
        },
    );
    assert!(
        out.unwrap_err()
            .to_string()
            .contains("execution_event_exposure_unresolved")
    );
    let p = w.resolve(&run.output);
    assert!(p.join("training.json").exists());
    assert!(p.join("event_exposure_checks.json").exists());
    assert!(!p.join("summary.json").exists());
    assert!(p.join("execution_readiness.json").exists());
}

#[test]
fn removed_identity_requests_and_templates_are_rejected() {
    assert!(serde_json::from_value::<WorkflowRequest>(json!({"operation":"prepare_identities","template":{},"market":"x","actions":null,"update_sources":false})).is_err());
    let t = ResearchTemplate::default();
    let mut value = serde_json::to_value(t).unwrap();
    value["identity_version"] = json!("old");
    assert!(serde_json::from_value::<ResearchTemplate>(value).is_err());
}

#[test]
fn unknown_special_coverage_is_explicit_warning_in_upstream_event_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let w = Workspace::open(tmp.path()).unwrap();
    let (m, a, t) = demo::create(&w).unwrap();
    let p = w.resolve(a.path);
    let mut book = ActionBundle::read(&p).unwrap();
    book.coverage.clear();
    for id in &book.queried_symbols {
        book.structured_coverage.insert(
            id.clone(),
            analytics_storage::execution_data::StructuredCoverage {
                dates: analytics_core::DateRange {
                    start: "2007-01-02".parse().unwrap(),
                    end: "2010-01-12".parse().unwrap(),
                },
                source: "hithink.events/per-share-v1".into(),
                evidence: vec!["fixture".into()],
            },
        );
    }

    atomic_json(&p, &book).unwrap();
    let a = w.register(&p).unwrap();
    let (run, out) = execute_stage(
        &w,
        WorkflowRequest::Research {
            template: Box::new(t),
            market: m.id,
            actions: Some(a.id),
            audit_only: false,
        },
    );
    let result = out.unwrap().details;
    assert_eq!(result["backtest_status"], "completed");
    assert!(
        result["execution_warnings"]
            .as_array()
            .is_some_and(|w| !w.is_empty())
    );
    assert!(
        w.resolve(run.output)
            .join("portfolio_snapshots.json")
            .exists()
    );
}

#[test]
fn supplier_account_policy_can_trade_without_manual_categories_and_audited_stays_blocked() {
    for source in ["hithink.events/per-share-v1"] {
        let tmp = tempfile::tempdir().unwrap();
        let w = Workspace::open(tmp.path()).unwrap();
        let (m, a, mut t) = demo::create(&w).unwrap();
        t.price_basis = analytics_core::research_quality::PriceBasis::Provider;
        let path = w.resolve(a.path);
        let mut book = ActionBundle::read(&path).unwrap();
        book.coverage.clear();
        for id in &book.queried_symbols {
            book.structured_coverage.insert(
                id.clone(),
                analytics_storage::execution_data::StructuredCoverage {
                    dates: analytics_core::DateRange {
                        start: "2007-01-02".parse().unwrap(),
                        end: "2010-01-12".parse().unwrap(),
                    },
                    source: source.into(),
                    evidence: vec!["explicit synthetic supplier fixture".into()],
                },
            );
        }
        // The engine starts in cash. An unresolved event at the first test
        // session is irrelevant to entitlement even if the stock is later bought.
        book.execution_issues.push(
            serde_json::from_value(json!({
                "id":"opening-cash-event","instrument":book.actions[0].instrument,
                "announced":null,"dates":{"start":"2009-01-01","end":"2009-01-01"},
                "code":"source_event_conflict","message":"fixture","evidence":["fixture"]
            }))
            .unwrap(),
        );
        atomic_json(&path, &book).unwrap();
        let a = w.register(&path).unwrap();
        let (_, out) = execute_stage(
            &w,
            WorkflowRequest::Research {
                template: Box::new(t.clone()),
                market: m.id.clone(),
                actions: Some(a.id.clone()),
                audit_only: false,
            },
        );
        let outcome = out.unwrap();
        assert_eq!(outcome.details["backtest_status"], "completed");
        assert!(
            outcome.details["aggregate"]["mean_daily_turnover"]
                .as_f64()
                .unwrap()
                > 0.
        );
        t.event_check_policy = analytics_core::execution::EventCheckPolicy::Audited;
        let (run, out) = execute_stage(
            &w,
            WorkflowRequest::Research {
                template: Box::new(t),
                market: m.id,
                actions: Some(a.id),
                audit_only: false,
            },
        );
        assert!(out.unwrap_err().to_string().contains("data_not_ready"));
        assert!(!w.resolve(run.output).join("training.json").exists());
    }
}
#[test]
fn damaged_training_descriptor_or_model_is_rejected_without_prediction_or_refitting() {
    let tmp = tempfile::tempdir().unwrap();
    let w = Workspace::open(tmp.path()).unwrap();
    let (m, a, t) = demo::create(&w).unwrap();
    let (run, out) = execute_stage(
        &w,
        WorkflowRequest::TrainResearch {
            template: Box::new(t),
            market: m.id,
            actions: Some(a.id.clone()),
        },
    );
    out.unwrap();
    let folder = w.resolve(&run.output);
    let descriptor = folder.join("training.json");
    let original = fs::read(&descriptor).unwrap();
    let mut artifact: Value = serde_json::from_slice(&original).unwrap();
    artifact["models"] = json!([]);
    atomic_json(&descriptor, &artifact).unwrap();
    let (bt, out) = execute_stage(
        &w,
        WorkflowRequest::BacktestResearch {
            execution: None,
            event_check_policy: Default::default(),
            training: run.id,
            actions: a.id.clone(),
        },
    );
    assert!(
        out.unwrap_err()
            .to_string()
            .contains("training_artifact_damaged")
    );
    assert!(!w.resolve(bt.output).join("summary.json").exists());
    fs::write(descriptor, original).unwrap();
    fs::write(folder.join("fold-0000/model.txt"), "bad model bytes").unwrap();
    let (_, out) = execute_stage(
        &w,
        WorkflowRequest::BacktestResearch {
            execution: None,
            event_check_policy: Default::default(),
            training: run.id,
            actions: a.id,
        },
    );
    assert!(
        out.unwrap_err()
            .to_string()
            .contains("training_artifact_damaged")
    );
}

#[test]
fn relocated_workspace_reuses_models_from_verified_relative_market_version() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("before");
    let w = Workspace::open(&root).unwrap();
    let (m, a, t) = demo::create(&w).unwrap();
    let (run, out) = execute_stage(
        &w,
        WorkflowRequest::TrainResearch {
            template: Box::new(t),
            market: m.id,
            actions: Some(a.id.clone()),
        },
    );
    out.unwrap();
    let original = fs::read(w.resolve(&run.output).join("fold-0000/model.txt")).unwrap();
    let moved = tmp.path().join("after");
    fs::rename(root, &moved).unwrap();
    let w = Workspace::open(&moved).unwrap();
    let (_, out) = execute_stage(
        &w,
        WorkflowRequest::BacktestResearch {
            execution: None,
            event_check_policy: Default::default(),
            training: run.id,
            actions: a.id,
        },
    );
    assert_eq!(out.unwrap().details["backtest_status"], "completed");
    assert_eq!(
        original,
        fs::read(w.resolve(run.output).join("fold-0000/model.txt")).unwrap()
    );
}

#[test]
fn changed_model_parameters_reuse_verified_factor_shards_and_corruption_recomputes() {
    let tmp = tempfile::tempdir().unwrap();
    let w = Workspace::open(tmp.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    let (first, out) = execute_stage(
        &w,
        WorkflowRequest::TrainResearch {
            template: Box::new(t.clone()),
            market: m.id.clone(),
            actions: Some(a.id.clone()),
        },
    );
    out.unwrap();
    t.config
        .training
        .parameters
        .insert("learning_rate".into(), "0.123".into());
    let (second, out) = execute_stage(
        &w,
        WorkflowRequest::TrainResearch {
            template: Box::new(t.clone()),
            market: m.id.clone(),
            actions: Some(a.id.clone()),
        },
    );
    out.unwrap();
    let cache = w.resolve(second.output).join("factor-cache");
    let receipt: Value =
        serde_json::from_slice(&fs::read(cache.join("2007.receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt["cache_hit"], true);
    assert_eq!(
        fs::read(cache.join("2007.bin")).unwrap(),
        fs::read(w.resolve(first.output).join("factor-cache/2007.bin")).unwrap()
    );
    let shared = w.root().join(".jfquant/runs/.factor-shards-v1").join(
        receipt["signature"]
            .as_str()
            .unwrap()
            .trim_start_matches("sha256:"),
    );
    fs::write(shared.join("features.parquet"), b"damaged").unwrap();
    let (_, out) = execute_stage(
        &w,
        WorkflowRequest::TrainResearch {
            template: Box::new(t),
            market: m.id,
            actions: Some(a.id),
        },
    );
    out.unwrap();
    assert!(shared.join("features.parquet").metadata().unwrap().len() > 7);
}

#[test]
fn cancellation_after_prediction_publication_preserves_models_and_retry_reuses_predictions() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let tmp = tempfile::tempdir().unwrap();
    let w = Workspace::open(tmp.path()).unwrap();
    let (m, a, t) = demo::create(&w).unwrap();
    let (trained, out) = execute_stage(
        &w,
        WorkflowRequest::TrainResearch {
            template: Box::new(t),
            market: m.id,
            actions: Some(a.id.clone()),
        },
    );
    out.unwrap();
    let source = w.resolve(trained.output);
    let before = fs::read(source.join("fold-0000/model.txt")).unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let callback = flag.clone();
    let ctx = TaskContext::new(Uuid::new_v4(), flag, move |message| {
        if message == "AKQuant 连续账户回测" {
            callback.store(true, Ordering::Relaxed);
        }
    });
    let cancelled = w.resolve(".jfquant/runs/cancelled-account");
    assert!(
        crate::research::backtest::backtest_from_training(
            &source,
            &cancelled,
            &w.resolve(&a.path),
            &ctx
        )
        .is_err()
    );
    let stages: Value =
        serde_json::from_slice(&fs::read(cancelled.join("stages.json")).unwrap()).unwrap();
    assert_eq!(stages["training"], "completed");
    assert_eq!(stages["prediction"], "completed");
    assert_eq!(stages["backtest"], "cancelled");
    assert!(!cancelled.join("summary.json").exists());
    let reports: Value =
        serde_json::from_slice(&fs::read(cancelled.join("fold_reports.json")).unwrap()).unwrap();
    assert!(
        reports
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["performance"].is_null())
    );
    let retry = w.resolve(".jfquant/runs/retried-account");
    crate::research::backtest::backtest_from_training(
        &source,
        &retry,
        &w.resolve(&a.path),
        &context(),
    )
    .unwrap();
    let receipt: Value =
        serde_json::from_slice(&fs::read(retry.join("prediction_manifest.json")).unwrap()).unwrap();
    assert_eq!(receipt["cache_hit"], true);
    assert!(!retry.join("prediction-factor-cache").exists());
    assert_eq!(
        fs::read(source.join("fold-0000/model.txt")).unwrap(),
        before
    );
    let shared = w.root().join(".jfquant/runs/.predictions-v1").join(
        receipt["signature"]
            .as_str()
            .unwrap()
            .trim_start_matches("sha256:"),
    );
    fs::write(shared.join("predictions.parquet"), b"truncated").unwrap();
    let damaged = w.resolve(".jfquant/runs/damaged-cache");
    assert!(
        crate::research::backtest::backtest_from_training(
            &source,
            &damaged,
            &w.resolve(&a.path),
            &context()
        )
        .unwrap_err()
        .to_string()
        .contains("prediction cache damaged")
    );
    assert!(!damaged.join("summary.json").exists());
}

#[test]
fn identified_unprojected_event_and_exposed_ordinary_conflict_stay_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let w = Workspace::open(tmp.path()).unwrap();
    let (m, a, t) = demo::create(&w).unwrap();
    let path = w.resolve(a.path);
    let mut book = ActionBundle::read(&path).unwrap();
    book.execution_issues
        .push(analytics_storage::execution_data::ExecutionIssue {
            id: "fixture-rights".into(),
            instrument: "SH600000".into(),
            announced: Some("2009-01-05".parse().unwrap()),
            dates: analytics_core::DateRange {
                start: "2009-01-08".parse().unwrap(),
                end: "2009-01-08".parse().unwrap(),
            },
            code: "execution_event_unsupported_skipped".into(),
            event_category: "rights_issue".into(),
            evidence: vec!["fixture-native-terms".into()],
            message: "identified rights issue".into(),
        });
    atomic_json(&path, &book).unwrap();
    let a = w.register(&path).unwrap();
    let (run, result) = execute_stage(
        &w,
        WorkflowRequest::Research {
            template: Box::new(t.clone()),
            market: m.id.clone(),
            actions: Some(a.id),
            audit_only: false,
        },
    );
    assert!(result.unwrap_err().to_string().contains("data_not_ready"));
    assert!(!w.resolve(run.output).join("summary.json").exists());
    book.execution_issues[0].code = "source_event_conflict".into();
    book.execution_issues[0].event_category = "distribution".into();
    atomic_json(&path, &book).unwrap();
    let a = w.register(&path).unwrap();
    let (run, result) = execute_stage(
        &w,
        WorkflowRequest::Research {
            template: Box::new(t),
            market: m.id,
            actions: Some(a.id),
            audit_only: false,
        },
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("execution_event_exposure_unresolved")
    );
    assert!(!w.resolve(run.output).join("summary.json").exists());
}

#[test]
fn legacy_action_version_hashes_original_numeric_format_without_mutating_files() {
    let tmp = tempfile::tempdir().unwrap();
    let w = Workspace::open(tmp.path()).unwrap();
    let (m, a, t) = demo::create(&w).unwrap();
    let path = w.resolve(a.path);
    let mut raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for a in raw["actions"].as_array_mut().unwrap() {
        for field in ["cash_per_share", "shares_per_share"] {
            a[field] = json!(a[field].as_str().unwrap().parse::<f64>().unwrap());
        }
    }
    let bytes = serde_json::to_vec(&raw).unwrap();
    fs::write(&path, &bytes).unwrap();
    let version = w.register(&path).unwrap();
    assert_eq!(version.signature, signature(&raw).unwrap());
    snapshot(
        &w,
        &WorkflowRequest::Research {
            template: Box::new(t),
            market: m.id,
            actions: Some(version.id),
            audit_only: true,
        },
    )
    .unwrap();
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn index_recipes_share_factor_order_and_require_actual_historical_membership() {
    let baseline = ResearchTemplate::builtin().unwrap();
    for index in analytics_core::indices::all()
        .iter()
        .filter(|i| i.symbol.is_some())
    {
        let t = ResearchTemplate::builtin_index(&index.id).unwrap();
        t.validate().unwrap();
        assert_eq!(t.benchmark, *index.symbol.as_ref().unwrap());
        assert_eq!(t.pool_version, format!("builtin:{}", index.id));
        assert_eq!(t.config.factor_ids, baseline.config.factor_ids);
        assert_eq!(t.formula_version, baseline.formula_version);
    }
    assert_eq!(
        signature(&ResearchTemplate::builtin_index("csi500").unwrap()).unwrap(),
        signature(&baseline).unwrap()
    );
    assert!(
        ResearchTemplate::builtin_index("msci_a50")
            .unwrap_err()
            .to_string()
            .contains("index_route_unavailable")
    );
    assert!(ResearchTemplate::builtin_index("typo").is_err());
    let tmp = tempfile::tempdir().unwrap();
    let w = Workspace::open(tmp.path()).unwrap();
    let (market, _, _) = demo::create(&w).unwrap();
    let root = w.resolve(&market.path);
    fs::copy(
        root.join("instruments/csi500.txt"),
        root.join("instruments/csi1000.txt"),
    )
    .unwrap();
    let market = w.register(&root).unwrap();
    let (pool, frozen) = w.resolve_pool("builtin:csi1000", &market.id).unwrap();
    assert_eq!(frozen["pool_version"], "builtin:csi1000");
    assert!(!pool.intervals.is_empty());
    assert!(w.resolve_pool("builtin:star50", &market.id).is_err());
    assert!(w.resolve_pool("builtin:msci_a50", &market.id).is_err());
}
