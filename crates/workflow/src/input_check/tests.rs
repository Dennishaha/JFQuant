use super::*;
use crate::{factor_evaluation::EvaluationInput, research::pipeline::cancellation};
use analytics_core::{
    DateRange, ExportFormat, ExportRequest,
    research_quality::{FactorSet, ResearchStrictness},
};
use analytics_engine::{DuckDbEngine, QueryEngine};
use analytics_factor::selection::{CustomFactorDefinition, FactorRef};
use std::sync::mpsc::channel;
fn fixture(dir: &std::path::Path, extra: &str) -> CheckRequest {
    let mut engine = DuckDbEngine::open_in_memory().unwrap();
    let market = dir.join("market.parquet");
    let universe = dir.join("pool.parquet");
    engine.export(ExportRequest{sql:format!("SELECT 's' || s::VARCHAR AS instrument_id, DATE '2030-01-01'+d::INTEGER AS trade_date, (10+d+s)::DOUBLE AS open, (12+d+s)::DOUBLE AS high, (9+d+s)::DOUBLE AS low, (11+d+s)::DOUBLE AS close, 100.0::DOUBLE AS volume, NULL::DOUBLE AS amount, true AS is_trading, DATE '2020-01-01' AS listing_date FROM range(1,4) a(s) CROSS JOIN range(0,30) b(d) {extra}"),destination:market.clone(),format:ExportFormat::Parquet,compression:None}).unwrap();
    engine.export(ExportRequest{sql:"SELECT 's'||s::VARCHAR AS instrument_id,'test' AS universe_id,DATE '2030-01-10' AS trade_date,true AS is_member FROM range(1,4) a(s)".into(),destination:universe.clone(),format:ExportFormat::Parquet,compression:None}).unwrap();
    let custom = CustomFactorDefinition::new(
        "custom_close".into(),
        "close".into(),
        String::new(),
        "CLOSE".into(),
        1,
    )
    .unwrap();
    CheckRequest {
        expected_input_hash: None,
        expected_source_stamp: None,
        input: EvaluationInput {
            factor_set: FactorSet::Alpha158,
            market_path: market,
            universe_path: universe,
            universe_id: "test".into(),
            date_range: DateRange {
                start: "2030-01-10".parse().unwrap(),
                end: "2030-01-20".parse().unwrap(),
            },
            factor_ids: vec![custom.id.clone()],
            factor_refs: vec![FactorRef::Custom {
                definition: Box::new(custom),
            }],
            output_dir: dir.join("one"),
        },
        horizon: 2,
        policy: Default::default(),
    }
}
fn execute(r: &CheckRequest) -> InputCheckReport {
    run(r, channel().0, cancellation()).unwrap()
}
#[test]
fn unselected_fields_outside_gaps_and_membership_do_not_block_and_cache_is_verified() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = fixture(dir.path(), "");
    let first = execute(&r);
    assert!(first.ready());
    assert!(first.missing_counts.is_empty());
    assert_eq!(first.factors[0].eligible, 33);
    assert_eq!(first.factors[0].finite, 33);
    assert_eq!(first.factors[0].paired, 27);
    r.input.output_dir = dir.path().join("two");
    let second = execute(&r);
    assert!(second.artifact.as_ref().unwrap().cache_hit);
    assert_eq!(first.context_hash, second.context_hash);
    std::fs::write(&second.artifact.as_ref().unwrap().artifact_path, "corrupt").unwrap();
    let third = execute(&r);
    assert!(!third.artifact.as_ref().unwrap().cache_hit);
    assert!(third.ready());
    fixture(dir.path(), "WHERE NOT (d=0 AND s=1)");
    let gap = execute(&r);
    assert_eq!(gap.missing_counts, first.missing_counts);
    assert_eq!(gap.factors[0].finite, first.factors[0].finite);
    assert!(!gap.artifact.unwrap().cache_hit);
}
#[test]
fn mixed_subsets_custom_revisions_order_horizon_and_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = fixture(dir.path(), "");
    r.input
        .factor_refs
        .push(FactorRef::builtin(FactorSet::Alpha158, "alpha158_001_KMID"));
    r.input
        .factor_refs
        .push(FactorRef::builtin(FactorSet::Gtja191, "alpha_002"));
    r.input.factor_ids = r
        .input
        .factor_refs
        .iter()
        .map(|r| r.id().to_owned())
        .collect();
    let first = execute(&r);
    assert!(first.complete());
    assert_eq!(first.factors.len(), 3);
    let p = execute(&r);
    assert!(p.artifact.unwrap().cache_hit);
    r.input.factor_ids.swap(0, 1);
    r.input.factor_refs.swap(0, 1);
    let changed = execute(&r);
    assert!(!changed.artifact.unwrap().cache_hit);
    r.horizon = 3;
    assert!(!execute(&r).artifact.unwrap().cache_hit);
    let cancel = cancellation();
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(matches!(
        run(&r, channel().0, cancel),
        Err(crate::research::pipeline::PipelineError::Cancelled)
    ));
}
#[test]
fn unavailable_inputs_have_policy_handling_and_never_fake_a_complete_pass() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = fixture(dir.path(), "");
    r.input.factor_ids = vec!["alpha_075".into()];
    r.input.factor_refs = vec![FactorRef::builtin(FactorSet::Gtja191, "alpha_075")];
    let strict = execute(&r);
    assert!(!strict.complete());
    assert!(!strict.ready());
    assert!(strict.artifact.is_none());
    r.policy.strictness = ResearchStrictness::AllowMissing;
    let relaxed = execute(&r);
    assert!(relaxed.complete());
    assert!(relaxed.ready());
    assert_eq!(relaxed.factors[0].finite, 0);
    assert!(relaxed.warnings.iter().any(|w| w.contains("计算结果为空")));
    r.policy.strictness = ResearchStrictness::QualifiedOnly;
    let qualified = execute(&r);
    assert!(!qualified.ready());
}
#[test]
fn daily_dependencies_are_field_specific_and_preserve_historical_member_context() {
    use analytics_core::pool::PoolMembership;
    let custom = CustomFactorDefinition::new(
        "custom_window".into(),
        "window".into(),
        String::new(),
        "GTJA_MEAN(VOLUME, 5) + GTJA_DELAY(CLOSE, 2)".into(),
        1,
    )
    .unwrap();
    let refs = vec![FactorRef::Custom {
        definition: Box::new(custom),
    }];
    let ids = vec![refs[0].id().to_owned()];
    let registry =
        analytics_factor::FactorRegistry::resolve(FactorSet::Gtja191, &refs, &ids).unwrap();
    let range = DateRange {
        start: "2030-01-10".parse().unwrap(),
        end: "2030-01-10".parse().unwrap(),
    };
    let plan = InputCheckPlan::new(
        FactorSet::Gtja191,
        &refs,
        &ids,
        &registry,
        "factor_page",
        serde_json::json!({}),
        vec![PhaseRange {
            phase: "prediction".into(),
            requested: range.clone(),
            effective: range.clone(),
        }],
    )
    .unwrap();
    let calendar: Vec<_> = (1..=15)
        .map(|d| analytics_core::Date::from_ymd_opt(2030, 1, d).unwrap())
        .collect();
    let pool = PoolMembership {
        coverage: range.clone(),
        intervals: BTreeMap::from([("s".into(), vec![range])]),
    };
    let needs = daily::requirements(&plan, &calendar, &pool, "");
    assert_eq!(
        needs["s"]["volume"].iter().copied().collect::<Vec<_>>(),
        [5, 6, 7, 8, 9]
    );
    assert!(!needs["s"].contains_key("high"));
    assert!(!needs["s"].contains_key("amount"));
}

#[test]
fn cross_sectional_dependencies_include_former_members_and_recursive_seed_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = fixture(dir.path(), "");
    let definition = CustomFactorDefinition::new(
        "custom_recursive".into(),
        "recursive".into(),
        String::new(),
        "GTJA_SMA(CLOSE, 3, 1) + GTJA_DELAY(GTJA_RANK(VOLUME), 2)".into(),
        1,
    )
    .unwrap();
    r.input.factor_ids = vec![definition.id.clone()];
    r.input.factor_refs = vec![FactorRef::Custom {
        definition: Box::new(definition),
    }];
    let report = execute(&r);
    assert!(report.ready());
    assert!(
        report.dependency_ranges["s1/close"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["start"] == "2030-01-01")
    );
    let p = parquet::prepare(&r.input, r.horizon, &channel().0, &cancellation()).unwrap();
    let (expected, _) = parquet::compute(&r.input, &p, channel().0, cancellation()).unwrap();
    let reused =
        crate::research::pipeline::load_factor_cache(&report.artifact.unwrap().artifact_path)
            .unwrap();
    assert_eq!(
        serde_json::to_value(expected.values).unwrap(),
        serde_json::to_value(reused.values).unwrap()
    );
    r.expected_input_hash = Some("sha256:wrong".into());
    assert!(
        run(&r, channel().0, cancellation())
            .unwrap_err()
            .to_string()
            .contains("提交后发生变化")
    );
}

#[test]
fn nested_ranks_do_not_check_the_dates_between_their_contexts() {
    use analytics_core::pool::PoolMembership;
    let def = CustomFactorDefinition::new(
        "custom_nested_rank".into(),
        "nested".into(),
        String::new(),
        "GTJA_RANK(GTJA_DELAY(GTJA_RANK(VOLUME), 20))".into(),
        1,
    )
    .unwrap();
    let refs = vec![FactorRef::Custom {
        definition: Box::new(def),
    }];
    let ids = vec![refs[0].id().into()];
    let registry = FactorRegistry::resolve(FactorSet::Gtja191, &refs, &ids).unwrap();
    let calendar: Vec<_> = (1..=30)
        .map(|d| analytics_core::Date::from_ymd_opt(2030, 1, d).unwrap())
        .collect();
    let range = |i| DateRange {
        start: calendar[i],
        end: calendar[i],
    };
    let plan = InputCheckPlan::new(
        FactorSet::Gtja191,
        &refs,
        &ids,
        &registry,
        "factor_page",
        serde_json::json!({"seed":"parquet-full-history"}),
        vec![PhaseRange {
            phase: "factor_page".into(),
            requested: range(24),
            effective: range(24),
        }],
    )
    .unwrap();
    let pool = PoolMembership {
        coverage: DateRange {
            start: calendar[0],
            end: calendar[29],
        },
        intervals: BTreeMap::from([
            ("current".into(), vec![range(24)]),
            ("former".into(), vec![range(4)]),
            ("irrelevant".into(), vec![range(14)]),
        ]),
    };
    let needs = daily::requirements(&plan, &calendar, &pool, "");
    assert_eq!(
        needs["former"]["volume"]
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        [4]
    );
    assert!(!needs.contains_key("irrelevant"));
}
