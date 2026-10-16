use super::*;
use analytics_core::task::TaskContext;
fn range(a: &str, b: &str) -> DateRange {
    DateRange {
        start: a.parse().unwrap(),
        end: b.parse().unwrap(),
    }
}
fn invoke(w: &crate::Workspace, job: Job) -> (crate::RunRecord, Value) {
    let c = TaskContext::default();
    let request = crate::WorkflowRequest::Independent {
        request: Box::new(job),
    };
    let run = w
        .begin(
            c.id,
            "independent acceptance",
            crate::snapshot(w, &request).unwrap(),
        )
        .unwrap();
    let out = crate::execute(w, &request, &run, &c).unwrap();
    (run, out.details)
}
#[test]
fn separate_fit_and_cross_pool_execution_without_training_market() {
    let tmp = tempfile::tempdir().unwrap();
    let w = crate::Workspace::open(tmp.path()).unwrap();
    let (market, actions, mut t) = crate::demo::create_canonical(&w).unwrap();
    t.config.factor_ids = vec!["alpha_002".into()];
    t.config.factor_refs = vec![
        analytics_factor::selection::FactorRef::Custom {
            definition: Box::new(
                analytics_factor::selection::CustomFactorDefinition::new(
                    "custom_recursive".into(),
                    "recursive fixture".into(),
                    String::new(),
                    "GTJA_SMA(CLOSE,3,1)".into(),
                    1,
                )
                .unwrap(),
            ),
        },
        analytics_factor::selection::FactorRef::builtin(Default::default(), "alpha_002"),
        analytics_factor::selection::FactorRef::builtin(
            analytics_core::research_quality::FactorSet::Alpha158,
            "alpha158_001_KMID",
        ),
    ];
    t.config.factor_ids = t.config.factor_refs.iter().map(|r| r.id().into()).collect();
    let save_pool = |name: &str, symbols: Vec<&str>| {
        w.save_pool(analytics_core::pool::PoolDefinition {
            schema_version: 3,
            name: name.into(),
            coverage: range("2008-01-01", "2010-12-31"),
            expression: analytics_core::pool::PoolExpression::Symbols {
                instruments: symbols.into_iter().map(str::to_owned).collect(),
            },
            adjustments: vec![],
        })
        .unwrap()
        .id
    };
    // Actual fixture identities are read from the registered universe.
    let dataset = DailyDataset::open(w.resolve(&market.path)).unwrap();
    let members = dataset.pool("csi500").unwrap();
    let symbols = members.intervals.keys().cloned().collect::<Vec<_>>();
    assert!(symbols.len() >= 2);
    t.pool_version = save_pool("A", vec![&symbols[0]]);
    let mut plan = TrainingPlan::from_template(&t);
    plan.schedule = TrainingSchedule::Manual {
        windows: vec![
            FitWindow {
                train: range("2009-01-02", "2009-01-12"),
                validation: None,
            },
            FitWindow {
                train: range("2009-01-02", "2009-01-20"),
                validation: None,
            },
        ],
    };
    let (train_run, artifact) = invoke(
        &w,
        Job::Train {
            plan: Box::new(plan),
            market: market.id.clone(),
            actions: Some(actions.id.clone()),
        },
    );
    assert_eq!(artifact["schema_version"], 5);
    let source = w.resolve(train_run.output);
    let c = catalogue(&source).unwrap();
    assert_eq!(c.models.len(), 2);
    assert!(c.models[0].instruments.contains(&symbols[0]));
    assert!(!c.models[0].instruments.contains(&symbols[1]));
    let model_hash = file_hash(&source.join("fold-0000/model.txt")).unwrap();
    // Use a new B snapshot and make the original A snapshot physically unavailable.
    fn copy_tree(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let b_parent = w.resolve("data/independent-B");
    let b_root = b_parent.join("market");
    copy_tree(&w.resolve(&market.path), &b_root);
    let mut b_source: Value = serde_json::from_slice(
        &fs::read(
            w.resolve(&market.path)
                .parent()
                .unwrap()
                .join("jfquant_source.json"),
        )
        .unwrap(),
    )
    .unwrap();
    b_source["demo_id"] = "independent-B".into();
    fs::write(
        b_parent.join("jfquant_source.json"),
        serde_json::to_vec(&b_source).unwrap(),
    )
    .unwrap();
    let training_root = w.resolve(&market.path);
    let market = w.register(&b_root).unwrap();
    fs::rename(&training_root, training_root.with_extension("unavailable")).unwrap();
    assert!(!training_root.exists());
    let b = BacktestPlan {
        schema_version: 1,
        training: train_run.id,
        models: Default::default(),
        pool_version: save_pool("B", vec![&symbols[1]]),
        range: range("2010-01-02", "2010-01-12"),
        market: market.id.clone(),
        actions: actions.id.clone(),
        benchmark: t.benchmark.clone(),
        quality_policy: t.quality_policy.clone(),
        execution: t.execution_settings(),
        event_check_policy: Default::default(),
    };
    let (backtest, result) = invoke(
        &w,
        Job::Backtest {
            plan: Box::new(b.clone()),
        },
    );
    assert_eq!(result["stages"]["backtest"], "completed");
    assert!(
        !w.resolve(&backtest.output)
            .join("fold-0000/model.txt")
            .exists()
    );
    assert_eq!(
        file_hash(&source.join("fold-0000/model.txt")).unwrap(),
        model_hash
    );
    let manifest: Value = serde_json::from_slice(
        &fs::read(w.resolve(&backtest.output).join("prediction_manifest.json")).unwrap(),
    )
    .unwrap();
    let mut account = b.clone();
    account.execution.initial_cash_cny *= 2.;
    let (replay, _) = invoke(
        &w,
        Job::Backtest {
            plan: Box::new(account),
        },
    );
    let reused: Value = serde_json::from_slice(
        &fs::read(w.resolve(replay.output).join("prediction_manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["signature"], reused["signature"]);
    assert_eq!(reused["cache_hit"], true);
    assert_eq!(result["cross_pool"]["label"], "完全未见标的回测");
    assert_eq!(result["cross_pool"]["groups"][0]["prediction_rows"], 0);
    assert_eq!(result["cross_pool"]["attribution_complete"], true);
    let mut shorter = b.clone();
    shorter.range.end = "2010-01-08".parse().unwrap();
    let (short, _) = invoke(
        &w,
        Job::Backtest {
            plan: Box::new(shorter),
        },
    );
    let short_rows: Vec<Value> = serde_json::from_slice(
        &fs::read(
            w.resolve(&short.output)
                .join("independent_predictions.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let full_rows: Vec<Value> = serde_json::from_slice(
        &fs::read(
            w.resolve(&backtest.output)
                .join("independent_predictions.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let short_features = data::load_year(
        &w.resolve(&short.output).join("prediction-factor-cache"),
        2010,
        &c.plan.config.factor_ids,
    )
    .unwrap();
    let full_features = data::load_year(
        &w.resolve(&backtest.output).join("prediction-factor-cache"),
        2010,
        &c.plan.config.factor_ids,
    )
    .unwrap();
    let width = c.plan.config.factor_ids.len();
    for (row, values) in short_features
        .meta
        .rows
        .iter()
        .zip(short_features.features.chunks_exact(width))
    {
        let index = full_features
            .meta
            .rows
            .iter()
            .position(|r| r.day == row.day && r.instrument == row.instrument)
            .unwrap();
        for (short, full) in values
            .iter()
            .zip(&full_features.features[index * width..(index + 1) * width])
        {
            assert!(short == full || (short.is_nan() && full.is_nan()));
        }
    }
    for row in &short_rows {
        let full = full_rows
            .iter()
            .find(|v| v["date"] == row["date"] && v["instrument"] == row["instrument"])
            .unwrap();
        assert_eq!(row["score"], full["score"]);
    }
    assert_eq!(full_rows.last().unwrap()["label"], Value::Null);
    assert!(full_rows.last().unwrap()["score"].is_number());
    let mut overlap = b.clone();
    overlap.pool_version = save_pool("A union B", vec![&symbols[0], &symbols[1]]);
    let (overlap_run, overlap_report) = invoke(
        &w,
        Job::Backtest {
            plan: Box::new(overlap),
        },
    );
    assert_eq!(
        overlap_report["cross_pool"]["label"],
        "跨池回测（含曾见标的）"
    );
    let changed: Value = serde_json::from_slice(
        &fs::read(
            w.resolve(overlap_run.output)
                .join("prediction_manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_ne!(manifest["signature"], changed["signature"]);
    let mut invalid = b;
    invalid.range = range("2009-01-02", "2009-01-12");
    assert!(preview(&source, &invalid, &w.resolve(&market.path)).is_err());
    // Published prediction replay must still work after deleting all trained native model files.
    fs::remove_dir_all(source.join("fold-0000")).unwrap();
    fs::remove_dir_all(source.join("fold-0001")).unwrap();
    let replay_output = w.resolve(format!(".jfquant/runs/{}", uuid::Uuid::new_v4()));
    replay_predictions(
        &w.resolve(backtest.output),
        &replay_output,
        &w.resolve(actions.path),
        Default::default(),
        &TaskContext::default(),
    )
    .unwrap();
    assert!(replay_output.join("summary.json").is_file());
}
#[test]
fn fit_schedule_requires_no_test_dates_and_purges_future_windows() {
    let calendar = (1..=31)
        .map(|d| format!("2020-01-{d:02}").parse().unwrap())
        .collect::<Vec<Date>>();
    let s:TrainingSchedule=serde_json::from_value(json!({"mode":"rolling","first":{"train":{"start":"2020-01-01","end":"2020-01-05"},"validation":null},"step":{"count":5,"unit":"days"},"expanding":false,"until":"2020-01-20"})).unwrap();
    let windows = s.resolve(&calendar).unwrap();
    assert_eq!(windows.len(), 4);
    assert_eq!(windows[3].train.end, "2020-01-20".parse::<Date>().unwrap());
    assert!(
        TrainingSchedule::Single {
            window: FitWindow {
                train: range("2020-01-01", "2020-01-05"),
                validation: Some(range("2020-01-05", "2020-01-08"))
            }
        }
        .resolve(&calendar)
        .is_err()
    );
}

#[test]
fn model_availability_switches_ties_single_and_submission_freeze() {
    let t = crate::ResearchTemplate::builtin().unwrap();
    let model = |index, end: &str| ModelReceipt {
        id: format!("m{index}"),
        index,
        window: FitWindow {
            train: range("2020-01-01", end),
            validation: None,
        },
        fit_data_end: end.parse().unwrap(),
        available_from: None,
        instruments: ["SH600000".into()].into(),
        sample_counts: [10, 0],
        model_sha256: "a".into(),
        manifest_sha256: "b".into(),
    };
    let c = ModelCatalogue {
        schema_version: 1,
        plan: TrainingPlan::from_template(&t),
        models: vec![
            model(0, "2020-01-03"),
            model(1, "2020-01-06"),
            model(2, "2020-01-06"),
        ],
        training_signature: String::new(),
        completed_at: String::new(),
        source_map: Value::Null,
        pool: Value::Null,
        signature: String::new(),
    };
    let dates = (1..=10)
        .map(|d| format!("2020-01-{d:02}").parse().unwrap())
        .collect::<Vec<Date>>();
    let segments = c
        .segments(
            &ModelSelection::Rolling,
            &range("2020-01-04", "2020-01-10"),
            &dates,
        )
        .unwrap();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].model_id, "m0");
    assert_eq!(segments[0].range.end, "2020-01-06".parse::<Date>().unwrap());
    assert_eq!(segments[1].model_id, "m2");
    assert!(
        c.segments(
            &ModelSelection::Rolling,
            &range("2020-01-03", "2020-01-10"),
            &dates
        )
        .is_err()
    );
    assert!(
        c.segments(
            &ModelSelection::Single {
                model_id: "m1".into()
            },
            &range("2020-01-04", "2020-01-10"),
            &dates
        )
        .is_err()
    );
    let single = c
        .segments(
            &ModelSelection::Single {
                model_id: "m0".into(),
            },
            &range("2020-01-04", "2020-01-10"),
            &dates,
        )
        .unwrap();
    assert_eq!(single.len(), 1);
    assert!(
        c.segments(
            &ModelSelection::Single {
                model_id: "missing".into()
            },
            &range("2020-01-04", "2020-01-10"),
            &dates
        )
        .is_err()
    );
    let mut future = c.clone();
    future.models[0].available_from = Some("2020-01-05".parse().unwrap());
    assert!(
        future
            .segments(
                &ModelSelection::Rolling,
                &range("2020-01-04", "2020-01-10"),
                &dates
            )
            .is_err()
    );
}

#[test]
fn cross_pool_rank_fixture_uses_each_membership_mask() {
    use analytics_factor::{
        FactorExecutor, FactorRunRequest, InMemoryFactorExecutor, Panel,
        selection::{CustomFactorDefinition, FactorRef},
    };
    let definition = CustomFactorDefinition::new(
        "custom_rank".into(),
        "rank".into(),
        String::new(),
        "GTJA_RANK(CLOSE)".into(),
        1,
    )
    .unwrap();
    let refs = vec![FactorRef::Custom {
        definition: Box::new(definition),
    }];
    let ids = vec!["custom_rank".into()];
    let registry = FactorRegistry::resolve(Default::default(), &refs, &ids).unwrap();
    let compute = |mask: Vec<Option<f64>>| {
        let date = "2020-01-02".parse().unwrap();
        let panel = Panel {
            instrument_id: vec!["SH600000".into(), "SH600001".into(), "SH600002".into()],
            trade_date: vec![date; 3],
            columns: std::collections::HashMap::from([
                ("close".into(), vec![Some(1.), Some(2.), Some(3.)]),
                ("__universe_member".into(), mask),
            ]),
        };
        let (tx, _) = std::sync::mpsc::channel();
        InMemoryFactorExecutor {
            registry: registry.clone(),
            panel,
        }
        .compute(
            FactorRunRequest {
                date_range: DateRange {
                    start: date,
                    end: date,
                },
                universe_id: "fixture".into(),
                factor_ids: ids.clone(),
                formula_version: registry.selection_version(&ids).unwrap(),
            },
            tx,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap()
    };
    let a = compute(vec![Some(1.), Some(1.), Some(0.)]);
    let b = compute(vec![Some(0.), Some(1.), Some(1.)]);
    assert_eq!(a.values["custom_rank"], vec![Some(0.5), Some(1.), None]);
    assert_eq!(b.values["custom_rank"], vec![None, Some(0.5), Some(1.)]);
}

#[test]
fn rolling_models_switch_inside_one_account_and_dynamic_b_pool() {
    let tmp = tempfile::tempdir().unwrap();
    let w = crate::Workspace::open(tmp.path()).unwrap();
    let (market, actions, t) = crate::demo::create_canonical(&w).unwrap();
    let coverage = range("2007-01-02", "2010-01-12");
    let a = w
        .save_pool(analytics_core::pool::PoolDefinition {
            schema_version: 3,
            name: "training A".into(),
            coverage: coverage.clone(),
            expression: analytics_core::pool::PoolExpression::Symbols {
                instruments: ["SH600000".into()].into(),
            },
            adjustments: vec![],
        })
        .unwrap();
    let b = w
        .save_pool(analytics_core::pool::PoolDefinition {
            schema_version: 3,
            name: "dynamic B".into(),
            coverage,
            expression: analytics_core::pool::PoolExpression::Symbols {
                instruments: ["SH600001".into()].into(),
            },
            adjustments: vec![analytics_core::pool::PoolAdjustment {
                effective_date: "2009-01-07".parse().unwrap(),
                add: ["SH600000".into()].into(),
                remove: Default::default(),
            }],
        })
        .unwrap();
    let mut plan = TrainingPlan::from_template(&t);
    plan.pool_version = a.id;
    plan.schedule = TrainingSchedule::Manual {
        windows: vec![
            FitWindow {
                train: range("2008-01-02", "2008-01-12"),
                validation: None,
            },
            FitWindow {
                train: range("2009-01-02", "2009-01-06"),
                validation: None,
            },
        ],
    };
    let (training, _) = invoke(
        &w,
        Job::Train {
            plan: Box::new(plan),
            market: market.id.clone(),
            actions: Some(actions.id.clone()),
        },
    );
    let plan = BacktestPlan {
        schema_version: 1,
        training: training.id,
        models: Default::default(),
        pool_version: b.id,
        range: range("2009-01-02", "2009-01-12"),
        market: market.id,
        actions: actions.id,
        benchmark: t.benchmark.clone(),
        quality_policy: t.quality_policy.clone(),
        execution: t.execution_settings(),
        event_check_policy: Default::default(),
    };
    let request = crate::WorkflowRequest::Independent {
        request: Box::new(Job::Backtest {
            plan: Box::new(plan.clone()),
        }),
    };
    let context = TaskContext::default();
    let snapshot = crate::snapshot(&w, &request).unwrap();
    let run = w.begin(context.id, "frozen selection", snapshot).unwrap();
    let source = w.resolve(&training.output);
    let model = source.join("fold-0000/model.txt");
    let bytes = fs::read(&model).unwrap();
    fs::write(&model, b"damaged after submission").unwrap();
    assert!(crate::execute(&w, &request, &run, &context).is_err());
    assert!(
        !w.resolve(&run.output)
            .join("prediction_manifest.json")
            .exists()
    );
    fs::write(&model, bytes).unwrap();
    let (backtest, result) = invoke(
        &w,
        Job::Backtest {
            plan: Box::new(plan),
        },
    );
    let segments = result["cross_pool"]["model_segments"].as_array().unwrap();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[1]["range"]["start"], "2009-01-07");
    assert_ne!(segments[0]["model_id"], segments[1]["model_id"]);
    let snapshots: BTreeMap<Date, Value> = serde_json::from_slice(
        &fs::read(w.resolve(backtest.output).join("portfolio_snapshots.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(snapshots.len(), 11);
    assert!(
        result["cross_pool"]["daily"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["seen_candidates"] == 1)
    );
    assert_eq!(result["cross_pool"]["attribution_complete"], true);
}
