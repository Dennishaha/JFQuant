//! Production-backend tests: relaxed policies never certify bad account inputs.
use super::*;
use analytics_core::research_quality::ResearchStrictness;

fn execute_quality(
    w: &Workspace,
    m: &DataVersion,
    a: &DataVersion,
    t: &ResearchTemplate,
    audit: bool,
) -> (RunRecord, Result<WorkflowOutcome>) {
    let request = WorkflowRequest::Research {
        template: Box::new(t.clone()),
        market: m.id.clone(),
        actions: Some(a.id.clone()),
        audit_only: audit,
    };
    let context = TaskContext::default();
    let run = w
        .begin(
            Uuid::new_v4(),
            "quality fixture",
            snapshot(w, &request).unwrap(),
        )
        .unwrap();
    let result = execute(w, &request, &run, &context);
    (run, result)
}
fn set_qlib_point(root: &Path, code: &str, field: &str, index: usize, value: f32) {
    let path = root.join(format!("features/{code}/{field}.day.bin"));
    let mut bytes = fs::read(&path).unwrap();
    bytes[(index + 1) * 4..(index + 2) * 4].copy_from_slice(&value.to_le_bytes());
    fs::write(path, bytes).unwrap();
}

fn require_amount(t: &mut ResearchTemplate) {
    use analytics_factor::selection::{CustomFactorDefinition, FactorRef};
    let def = CustomFactorDefinition::new(
        "custom_amount_dependency".into(),
        "amount dependency".into(),
        String::new(),
        "GTJA_IF(CLOSE > 0, CLOSE, AMOUNT)".into(),
        1,
    )
    .unwrap();
    t.config.factor_refs = analytics_factor::selection::frozen_refs(
        t.factor_set,
        &t.config.factor_refs,
        &t.config.factor_ids,
    );
    t.config.factor_ids.push(def.id.clone());
    t.config.factor_refs.push(FactorRef::Custom {
        definition: Box::new(def),
    });
    t.formula_version = t.resolved_formula_version().unwrap();
}

#[test]
fn three_policies_distinguish_missing_input_null_propagation_and_security_exclusion() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    require_amount(&mut t);
    let root = w.resolve(m.path);
    let absent = root.join("features/sh600000/amount.day.bin");
    fs::remove_file(&absent).unwrap();
    let m = w.register(&root).unwrap();
    let (strict, result) = execute_quality(&w, &m, &a, &t, false);
    assert!(result.is_err());
    assert!(!w.resolve(strict.output).join("factor-cache").exists());
    t.quality_policy.strictness = ResearchStrictness::AllowMissing;
    let (_, result) = execute_quality(&w, &m, &a, &t, false);
    let relaxed = result.unwrap().details;
    assert_eq!(relaxed["admission"]["schema_version"], 5);
    assert!(
        relaxed["admission"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["status"] == "failed"
                && c["kind"] == "missing"
                && c["disposition"] == "keep_null")
    );
    assert_eq!(
        relaxed["execution"]["quality"]["effective_pool"],
        relaxed["execution"]["quality"]["original_pool"]
    );
    t.quality_policy.strictness = ResearchStrictness::QualifiedOnly;
    let (_, result) = execute_quality(&w, &m, &a, &t, false);
    let selected = result.unwrap().details;
    let removed = demo::fixture_security_id("SH600000");
    assert!(
        selected["execution"]["quality"]["original_pool"]["intervals"]
            .get(&removed)
            .is_some()
    );
    assert!(
        selected["execution"]["pool"]["intervals"]
            .get(&removed)
            .is_none()
    );
    assert_eq!(selected["research_scope"], "经数据筛选的研究股票池");
    assert_ne!(selected["signature"], relaxed["signature"]);
    assert!(!absent.exists());
    crate::research::backtest::validate_result(&selected).unwrap();
}

#[test]
fn future_missing_observation_changes_only_future_qualification() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    require_amount(&mut t);
    t.quality_policy.strictness = ResearchStrictness::QualifiedOnly;
    let before = research_requirements_with_actions(&w, &t, &m.id, Some(&a.id)).unwrap();
    let root = w.resolve(m.path);
    set_qlib_point(&root, "sh600000", "amount", 29, f32::NAN);
    let m = w.register(&root).unwrap();
    let after = research_requirements_with_actions(&w, &t, &m.id, Some(&a.id)).unwrap();
    let before = &before["quality"]["daily_counts"];
    let after = &after["quality"]["daily_counts"];
    assert_eq!(
        &before.as_array().unwrap()[..29],
        &after.as_array().unwrap()[..29]
    );
    assert_eq!(before[29]["retained"], 2);
    assert_eq!(after[29]["retained"], 1);
    assert_eq!(after[30]["retained"], 2);
    let (run, result) = execute_quality(&w, &m, &a, &t, false);
    let result = result.unwrap().details;
    assert_eq!(result["execution"]["quality"]["daily_counts"], *after);
    assert!(
        w.resolve(run.output)
            .join("sample_masks/fold-0000-train.json")
            .exists()
    );
}

#[test]
fn damaged_fields_and_coefficient_conflicts_block_all_modes_before_fit() {
    for case in ["truncated", "coefficient", "negative_volume"] {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (m, a, mut t) = demo::create(&w).unwrap();
        require_amount(&mut t);
        let root = w.resolve(m.path);
        if case == "truncated" {
            fs::write(root.join("features/sh600000/amount.day.bin"), [1, 2, 3]).unwrap();
        } else if case == "coefficient" {
            set_qlib_point(&root, "sh600000", "factor", 5, 0.2);
        } else {
            set_qlib_point(&root, "sh600000", "volume", 25, -1.);
        }

        let m = w.register(&root).unwrap();
        for mode in [
            ResearchStrictness::Strict,
            ResearchStrictness::AllowMissing,
            ResearchStrictness::QualifiedOnly,
        ] {
            t.quality_policy.strictness = mode;
            let (run, result) = execute_quality(&w, &m, &a, &t, false);
            assert!(result.is_err(), "{mode:?}");
            assert!(!w.resolve(&run.output).join("factor-cache").exists());
            assert!(!w.resolve(run.output).join("fold-0000").exists());
        }
    }
}

#[test]
fn qualified_unresolved_candidates_do_not_change_original_membership_and_empty_pool_blocks() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    require_amount(&mut t);
    let root = w.resolve(m.path);
    let path = root.join("instruments/csi500.txt");
    let original = fs::read_to_string(&path).unwrap() + "SHT00018\t2007-01-02\t2010-01-12\n";
    fs::write(&path, &original).unwrap();
    let m = w.register(&root).unwrap();
    t.quality_policy.strictness = ResearchStrictness::QualifiedOnly;
    let (_, result) = execute_quality(&w, &m, &a, &t, false);
    let result = result.unwrap().details;
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    assert!(
        result["execution"]["quality"]["original_pool"]["intervals"]
            .get("SHT00018")
            .is_some()
    );
    assert!(
        result["execution"]["pool"]["intervals"]
            .get("SHT00018")
            .is_none()
    );
    for code in ["sh600000", "sh600001"] {
        fs::remove_file(root.join(format!("features/{code}/amount.day.bin"))).unwrap();
    }
    let m = w.register(&root).unwrap();
    let (run, result) = execute_quality(&w, &m, &a, &t, true);
    assert_eq!(result.unwrap().details["ready"], false);
    assert!(!w.resolve(run.output).join("factor-cache").exists());
}

#[test]
fn minimum_samples_and_feature_thresholds_are_frozen_and_tampering_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    t.quality_policy.strictness = ResearchStrictness::AllowMissing;
    t.config.factor_ids = vec!["alpha_002".into(), "alpha_014".into()];
    let (_, low) = execute_quality(&w, &m, &a, &t, false);
    let low = low.unwrap().details;
    t.quality_policy.min_valid_feature_ratio = 1.;
    let (_, high) = execute_quality(&w, &m, &a, &t, false);
    let high = high.unwrap().details;
    assert!(
        high["fold_reports"][0]["train_rows"].as_u64().unwrap()
            < low["fold_reports"][0]["train_rows"].as_u64().unwrap()
    );
    assert_ne!(low["factor_signature"], high["factor_signature"]);
    let mut tampered = high.clone();
    tampered["execution"]["quality_policy"]["strictness"] = json!("strict");
    assert!(crate::research::backtest::validate_result(&tampered).is_err());
    let mut incomplete = high.clone();
    incomplete["sample_selection"]["selections"] = json!([]);
    incomplete["execution"]["sample_selection"] = incomplete["sample_selection"].clone();
    incomplete["execution"]["sample_selection_sha256"] = json!(format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(
            serde_json::to_vec(&incomplete["sample_selection"]).unwrap()
        ))
    ));
    assert!(crate::research::backtest::validate_result(&incomplete).is_err());
    let mut old = high;
    old["schema_version"] = json!(8);
    assert!(crate::research::backtest::validate_result(&old).is_err());
    t.quality_policy.min_labeled_rows = 1000;
    let (run, result) = execute_quality(&w, &m, &a, &t, true);
    assert_eq!(result.unwrap().details["ready"], false);
    assert!(!w.resolve(run.output).join("factor-cache").exists());
    t.quality_policy.min_labeled_rows = 1;
    t.schema_version = 4;
    assert!(t.validate().is_err());
}

#[test]
fn all_three_policies_preserve_qlib_parquet_identity_account_and_feature_parity() {
    for mode in [
        ResearchStrictness::Strict,
        ResearchStrictness::AllowMissing,
        ResearchStrictness::QualifiedOnly,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (qm, qa, mut qt) = demo::create(&w).unwrap();
        let (pm, pa, mut pt) = demo::create_canonical(&w).unwrap();
        qt.quality_policy.strictness = mode;
        pt.quality_policy.strictness = mode;
        let (_, q) = execute_quality(&w, &qm, &qa, &qt, false);
        let (_, p) = execute_quality(&w, &pm, &pa, &pt, false);
        let q = q.unwrap().details;
        let p = p.unwrap().details;
        assert_eq!(q["execution"]["pool"], p["execution"]["pool"]);
        assert_eq!(
            q["execution"]["resolved_folds"],
            p["execution"]["resolved_folds"]
        );
        assert_eq!(q["aggregate"], p["aggregate"]);
        assert_eq!(
            q["sample_selection"]["selections"],
            p["sample_selection"]["selections"]
        );
    }
}

#[test]
fn future_event_category_gap_does_not_change_training_qualification() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    t.quality_policy.strictness = ResearchStrictness::QualifiedOnly;
    let before = research_requirements_with_actions(&w, &t, &m.id, Some(&a.id)).unwrap();
    let path = w.resolve(a.path);
    let mut book = ActionBundle::read(&path).unwrap();
    let id = demo::fixture_security_id("SH600000");
    for claim in book.coverage.get_mut(&id).unwrap() {
        if claim.category == "rights_issue" {
            claim.dates.end = "2009-01-07".parse().unwrap();
        }
    }
    atomic_json(&path, &book).unwrap();
    let a = w.register(&path).unwrap();
    let after = research_requirements_with_actions(&w, &t, &m.id, Some(&a.id)).unwrap();
    let before = before["quality"]["daily_counts"].as_array().unwrap();
    let after = after["quality"]["daily_counts"].as_array().unwrap();
    for (b, a) in before.iter().zip(after) {
        if b["date"].as_str().unwrap() < "2009-01-08" {
            assert_eq!(a, b);
        } else {
            assert_eq!(a["retained"], b["retained"]);
        }
    }
    let (_, result) = execute_quality(&w, &m, &a, &t, true);
    assert_eq!(result.unwrap().details["ready"], false);
}

#[test]
fn missing_later_source_alias_does_not_erase_earlier_qualified_history() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    require_amount(&mut t);
    t.quality_policy.strictness = ResearchStrictness::QualifiedOnly;
    let root = w.resolve(&m.path);
    let next = root.join("features/sh600002");
    fs::create_dir_all(&next).unwrap();
    for entry in fs::read_dir(root.join("features/sh600000")).unwrap() {
        let path = entry.unwrap().path();
        fs::copy(&path, next.join(path.file_name().unwrap())).unwrap();
    }
    use analytics_core::source_map::{SourceException, SourceLocator, SourceMap};
    let id = demo::fixture_security_id("SH600000");
    let range = analytics_core::DateRange {
        start: "2007-01-02".parse().unwrap(),
        end: "2010-01-12".parse().unwrap(),
    };
    let directory = SourceMap {
        schema_version: 2,
        exceptions: std::collections::BTreeMap::from([(
            id.clone(),
            SourceException {
                exchange_symbol: id.clone(),
                kind: "a_share".into(),
                coverage: range.clone(),
                locators: vec![
                    SourceLocator {
                        provider: "qlib".into(),
                        symbol: id.clone(),
                        coverage: analytics_core::DateRange {
                            start: range.start,
                            end: "2009-01-12".parse().unwrap(),
                        },
                        evidence: vec![],
                    },
                    SourceLocator {
                        provider: "qlib".into(),
                        symbol: "SH600002".into(),
                        coverage: analytics_core::DateRange {
                            start: "2010-01-02".parse().unwrap(),
                            end: range.end,
                        },
                        evidence: vec![],
                    },
                ],
            },
        )]),
    };
    atomic_json(&root.join("source_map.json"), &directory).unwrap();
    let m = w.register(&root).unwrap();
    let before = research_requirements_with_actions(&w, &t, &m.id, Some(&a.id)).unwrap();
    fs::remove_file(next.join("amount.day.bin")).unwrap();
    let m = w.register(&root).unwrap();
    let after = research_requirements_with_actions(&w, &t, &m.id, Some(&a.id)).unwrap();
    let before = before["quality"]["daily_counts"].as_array().unwrap();
    let after = after["quality"]["daily_counts"].as_array().unwrap();
    assert_eq!(&before[..33], &after[..33]);
    assert_eq!(after[33]["retained"], 1);
    let (_, result) = execute_quality(&w, &m, &a, &t, false);
    assert_eq!(
        result.unwrap().details["schema_version"],
        crate::research::backtest::RESEARCH_SCHEMA_VERSION
    );
    let mut dataset = analytics_storage::daily::DailyDataset::open(&root).unwrap();
    dataset.bind_source_map(directory).unwrap();
    dataset
        .restrict_reads(analytics_core::DateRange {
            start: "2007-01-02".parse().unwrap(),
            end: "2009-01-12".parse().unwrap(),
        })
        .unwrap();
    assert!(dataset.field(&id, "amount").is_ok());
    // A later alias must be validated under the logical security's dated reads;
    // stale observations from the former source must not become dependencies.
    dataset.restrict_reads(range).unwrap();
    dataset.restrict_input_checks(std::collections::BTreeMap::from([(
        id.clone(),
        std::collections::BTreeMap::from([("high".into(), std::collections::BTreeSet::from([34]))]),
    )]));
    set_qlib_point(&root, "sh600000", "high", 34, -1.);
    assert!(dataset.field(&id, "high").is_ok());
    set_qlib_point(&root, "sh600002", "high", 34, -1.);
    assert!(dataset.research_field(&id, "high", true).is_err());
}

#[test]
fn provider_training_without_events_keeps_warning_and_model_and_verified_input_stops() {
    use analytics_core::research_quality::PriceBasis;
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, _, mut t) = demo::create(&w).unwrap();
    let request = WorkflowRequest::TrainResearch {
        template: Box::new(t.clone()),
        market: m.id.clone(),
        actions: None,
    };
    let id = Uuid::new_v4();
    let run = w
        .begin(id, "verified", snapshot(&w, &request).unwrap())
        .unwrap();
    assert!(execute(&w, &request, &run, &TaskContext::default()).is_err());
    assert!(!w.resolve(&run.output).join("training.json").exists());
    t.price_basis = PriceBasis::Provider;
    let request = WorkflowRequest::TrainResearch {
        template: Box::new(t),
        market: m.id.clone(),
        actions: None,
    };
    let run = w
        .begin(Uuid::new_v4(), "provider", snapshot(&w, &request).unwrap())
        .unwrap();
    execute(&w, &request, &run, &TaskContext::default()).unwrap();
    let artifact: Value =
        serde_json::from_slice(&fs::read(w.resolve(&run.output).join("training.json")).unwrap())
            .unwrap();
    crate::research::backtest::validate_training_artifact(&artifact).unwrap();
    assert_eq!(artifact["schema_version"], 4);
    assert_eq!(artifact["price_basis"], "provider");
    assert!(
        artifact["source_warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["category"] == "historical_events" && c["status"] == "not_run")
    );
    assert!(
        artifact["source_warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["code"] == "account_path_not_run")
    );
    let admission: Value =
        serde_json::from_slice(&fs::read(w.resolve(&run.output).join("admission.json")).unwrap())
            .unwrap();
    assert!(
        admission["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["category"] == "historical_events"
                && c["status"] == "not_run"
                && c["disposition"] == "warn")
    );
    let report: analytics_core::admission::AdmissionReport =
        serde_json::from_value(admission).unwrap();
    assert!(report.ready());
    assert!(!w.resolve(&run.output).join("summary.json").exists());
}

#[test]
fn alpha158_template_family_order_and_native_training_are_independent_from_gtja() {
    use analytics_core::research_quality::{FactorSet, PriceBasis};
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, _, mut t) = demo::create(&w).unwrap();
    let gtja = ResearchTemplate::builtin().unwrap();
    t.select_factor_set(FactorSet::Alpha158);
    assert_eq!(t.config.factor_ids.len(), 158);
    assert_eq!(gtja.config.factor_ids.len(), 178);
    t.price_basis = PriceBasis::Provider;
    t.config.factor_ids = vec!["alpha158_001_KMID".into(), "alpha158_014_ROC5".into()];
    t.validate().unwrap();
    let request = WorkflowRequest::TrainResearch {
        template: Box::new(t.clone()),
        market: m.id,
        actions: None,
    };
    let run = w
        .begin(Uuid::new_v4(), "alpha158", snapshot(&w, &request).unwrap())
        .unwrap();
    execute(&w, &request, &run, &TaskContext::default()).unwrap();
    let artifact: Value =
        serde_json::from_slice(&fs::read(w.resolve(&run.output).join("training.json")).unwrap())
            .unwrap();
    crate::research::backtest::validate_training_artifact(&artifact).unwrap();
    assert_eq!(artifact["feature_order"], json!(t.config.factor_ids));
    assert_eq!(artifact["factor_set"], "alpha158");
    assert_eq!(
        artifact["formula_version"],
        analytics_factor::alpha158::FORMULA_VERSION
    );
    t.config.factor_ids.push("alpha_001".into());
    assert!(t.validate().is_err());
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/factors/alpha158.json");
    assert_eq!(
        profiles::FactorProfile::read(&path)
            .unwrap()
            .factor_ids
            .len(),
        158
    );
}

#[test]
fn split_profiles_are_reusable_portable_and_reject_flat_or_conflicting_templates() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("one");
    let mut t = ResearchTemplate::builtin_csi300().unwrap();
    t.write(&root.join("workflow.json")).unwrap();
    let d: Value = serde_json::from_slice(&fs::read(root.join("workflow.json")).unwrap()).unwrap();
    assert_eq!(d["schema_version"], 12);
    for k in [
        "config",
        "factor_ids",
        "factor_set",
        "pool_version",
        "benchmark",
    ] {
        assert!(d.get(k).is_none());
    }
    let fpath = root.join(d["factors"].as_str().unwrap());
    let upath = root.join(d["target"].as_str().unwrap());
    let before = fs::read(&fpath).unwrap();
    let mut target = profiles::TargetProfile::read(&upath).unwrap();
    target.pool_version = "builtin:csi500".into();
    target.benchmark = "SH000905".into();
    target.apply(&mut t).unwrap();
    t.write(&root.join("other.json")).unwrap();
    let other: Value = serde_json::from_slice(&fs::read(root.join("other.json")).unwrap()).unwrap();
    assert_eq!(other["factors"], d["factors"]);
    assert_ne!(other["target"], d["target"]);
    assert_eq!(before, fs::read(&fpath).unwrap());
    fs::rename(&root, tmp.path().join("moved")).unwrap();
    assert_eq!(
        signature(&t).unwrap(),
        signature(&ResearchTemplate::read(&tmp.path().join("moved/other.json")).unwrap()).unwrap()
    );
    fs::write(
        tmp.path().join("flat.json"),
        serde_json::to_vec(&t).unwrap(),
    )
    .unwrap();
    assert!(ResearchTemplate::read(&tmp.path().join("flat.json")).is_err());
    let mut factor = profiles::FactorProfile::from_template(&t);
    factor.formula_version = "unreviewed".into();
    let unchanged = signature(&t).unwrap();
    assert!(factor.apply(&mut t).is_err());
    assert_eq!(unchanged, signature(&t).unwrap());
}

#[test]
fn alpha158_verified_qlib_and_parquet_keep_features_fees_and_accounts_equal() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (qm, qa, mut qt) = demo::create(&w).unwrap();
    let (pm, pa, mut pt) = demo::create_canonical(&w).unwrap();
    for t in [&mut qt, &mut pt] {
        t.select_factor_set(analytics_core::research_quality::FactorSet::Alpha158);
        t.config.factor_ids = vec!["alpha158_001_KMID".into(), "alpha158_014_ROC5".into()];
    }
    let (_, q) = execute_quality(&w, &qm, &qa, &qt, false);
    let (_, p) = execute_quality(&w, &pm, &pa, &pt, false);
    let q = q.unwrap().details;
    let p = p.unwrap().details;
    assert_eq!(q["aggregate"], p["aggregate"]);
    assert_eq!(q["sample_selection"], p["sample_selection"]);
    assert_eq!(
        q["execution"]["resolved_folds"],
        p["execution"]["resolved_folds"]
    );
}

#[test]
fn native_check_ignores_unused_fields_phase_gaps_and_pre_backtest_benchmark() {
    use analytics_core::{
        DateRange,
        schedule::{Fold, Schedule},
    };
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    let root = w.resolve(m.path);
    let r = |a: &str, b: &str| DateRange {
        start: a.parse().unwrap(),
        end: b.parse().unwrap(),
    };
    t.schedule = Schedule::Single {
        fold: Fold {
            train: r("2007-01-02", "2007-01-05"),
            validation: Some(r("2008-01-09", "2008-01-12")),
            test: r("2009-01-09", "2009-01-12"),
        },
    };
    let m = w.register(&root).unwrap();
    let (before, result) = execute_quality(&w, &m, &a, &t, true);
    result.unwrap();
    let before: crate::input_check::InputCheckReport = serde_json::from_slice(
        &fs::read(w.resolve(before.output).join("input-check.json")).unwrap(),
    )
    .unwrap();
    assert!(before.ready(), "{:?}", before.admission.checks);
    fs::write(
        root.join("features/sh600000/amount.day.bin"),
        b"unused malformed field",
    )
    .unwrap();
    set_qlib_point(&root, "sh600000", "high", 9, -1.);
    for day in 0..22 {
        set_qlib_point(&root, "sh000905", "close", day, f32::NAN);
    }
    let m = w.register(&root).unwrap();
    let (after, result) = execute_quality(&w, &m, &a, &t, true);
    result.unwrap();
    let after: crate::input_check::InputCheckReport = serde_json::from_slice(
        &fs::read(w.resolve(after.output).join("input-check.json")).unwrap(),
    )
    .unwrap();
    assert!(after.ready(), "{:?}", after.admission.checks);
    assert_eq!(before.missing_counts, after.missing_counts);
    assert_eq!(
        serde_json::to_value(before.factors).unwrap(),
        serde_json::to_value(after.factors).unwrap()
    );
}

#[test]
fn complete_native_check_is_reused_by_training_without_running_a_trainer() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, t) = demo::create(&w).unwrap();
    let request = WorkflowRequest::Independent {
        request: Box::new(
            crate::research::backtest::independent::Job::TrainingInputs {
                plan: Box::new(
                    crate::research::backtest::independent::TrainingPlan::from_template(&t),
                ),
                market: m.id.clone(),
                actions: Some(a.id.clone()),
                prepare: false,
                announcements: false,
            },
        ),
    };
    let run = w
        .begin(Uuid::new_v4(), "check", snapshot(&w, &request).unwrap())
        .unwrap();
    let out = execute(&w, &request, &run, &TaskContext::default()).unwrap();
    assert_eq!(out.details["ready"], true);
    assert_eq!(out.details["stages"][3]["status"], "passed");
    assert!(!w.resolve(&run.output).join("fold-0000/model.txt").exists());
    let request = WorkflowRequest::TrainResearch {
        template: Box::new(t),
        market: m.id,
        actions: Some(a.id),
    };
    let run = w
        .begin(Uuid::new_v4(), "train", snapshot(&w, &request).unwrap())
        .unwrap();
    execute(&w, &request, &run, &TaskContext::default()).unwrap();
    let receipt: Value = serde_json::from_slice(
        &fs::read(
            w.resolve(&run.output)
                .join("factor-cache/2007.receipt.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["cache_hit"], true);
}

#[test]
fn benchmark_valid_tail_clips_prediction_only_and_internal_holes_block() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, t) = demo::create(&w).unwrap();
    let root = w.resolve(m.path);
    for day in 42..44 {
        set_qlib_point(&root, "sh000905", "close", day, f32::NAN);
    }
    let m = w.register(&root).unwrap();
    let (run, result) = execute_quality(&w, &m, &a, &t, true);
    result.unwrap();
    let report: crate::input_check::InputCheckReport =
        serde_json::from_slice(&fs::read(w.resolve(&run.output).join("input-check.json")).unwrap())
            .unwrap();
    assert!(report.ready());
    let last = report.plan.phases.last().unwrap();
    assert_eq!(last.effective.end.to_string(), "2010-01-10");
    assert!(last.requested.end > last.effective.end);
    assert_eq!(
        report.plan.phases[0].effective.start.to_string(),
        "2007-01-02"
    );
    set_qlib_point(&root, "sh000905", "close", 40, f32::NAN);
    let m = w.register(&root).unwrap();
    let (run, result) = execute_quality(&w, &m, &a, &t, true);
    result.unwrap();
    let report: crate::input_check::InputCheckReport =
        serde_json::from_slice(&fs::read(w.resolve(&run.output).join("input-check.json")).unwrap())
            .unwrap();
    assert!(!report.ready());
    assert_eq!(
        report.stages[2].status,
        analytics_core::admission::CheckStatus::NotRun
    );
}

#[test]
fn fit_check_requires_neither_an_unused_benchmark_nor_unrelated_former_members() {
    use crate::research::backtest::independent::{Job, TrainingPlan};
    use analytics_core::{
        DateRange,
        schedule::{Fold, Schedule},
    };
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    let root = w.resolve(m.path);
    let range = |a: &str, b: &str| DateRange {
        start: a.parse().unwrap(),
        end: b.parse().unwrap(),
    };
    t.schedule = Schedule::Single {
        fold: Fold {
            train: range("2008-01-03", "2008-01-12"),
            validation: Some(range("2009-01-03", "2009-01-12")),
            test: range("2010-01-02", "2010-01-12"),
        },
    };
    let pool = root.join("instruments/csi500.txt");
    fs::write(
        &pool,
        fs::read_to_string(&pool).unwrap() + "SHT00018\t2007-01-02\t2007-01-03\n",
    )
    .unwrap();
    fs::remove_dir_all(root.join("features/sh000905")).unwrap();
    let m = w.register(&root).unwrap();
    let request = WorkflowRequest::Independent {
        request: Box::new(Job::TrainingInputs {
            plan: Box::new(TrainingPlan::from_template(&t)),
            market: m.id,
            actions: Some(a.id),
            prepare: false,
            announcements: false,
        }),
    };
    let run = w
        .begin(Uuid::new_v4(), "check", snapshot(&w, &request).unwrap())
        .unwrap();
    let report = execute(&w, &request, &run, &TaskContext::default())
        .unwrap()
        .details;
    assert_eq!(report["ready"], true, "{report}");
    assert!(
        report["dependency_ranges"]
            .as_object()
            .unwrap()
            .keys()
            .all(|k| !k.starts_with("SH000905/") && !k.starts_with("SHT00018/"))
    );
}

#[test]
fn legacy_fit_entry_ignores_a_future_unused_test_window() {
    use analytics_core::{
        DateRange,
        schedule::{Fold, Schedule},
    };
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    let range = |a: &str, b: &str| DateRange {
        start: a.parse().unwrap(),
        end: b.parse().unwrap(),
    };
    t.schedule = Schedule::Single {
        fold: Fold {
            train: range("2008-01-03", "2008-01-12"),
            validation: Some(range("2009-01-03", "2009-01-12")),
            test: range("2030-01-02", "2030-01-12"),
        },
    };
    let request = WorkflowRequest::TrainResearch {
        template: Box::new(t),
        market: m.id,
        actions: Some(a.id),
    };
    let run = w
        .begin(Uuid::new_v4(), "fit", snapshot(&w, &request).unwrap())
        .unwrap();
    execute(&w, &request, &run, &TaskContext::default()).unwrap();
    let report: crate::input_check::InputCheckReport =
        serde_json::from_slice(&fs::read(w.resolve(&run.output).join("input-check.json")).unwrap())
            .unwrap();
    assert!(report.ready());
    assert!(
        report
            .plan
            .phases
            .iter()
            .all(|p| p.phase == "train" || p.phase == "validation")
    );
    assert!(w.resolve(&run.output).join("fold-0000/model.txt").is_file());
}

#[test]
fn allocation_protocol_requires_current_template_and_all_new_fields_without_runtime_repair() {
    let current = serde_json::to_value(ResearchTemplate::default()).unwrap();
    for mutation in [
        "old_version",
        "missing_version",
        "missing_candidate_policy",
        "missing_solver_tolerance",
    ] {
        let mut value = current.clone();
        match mutation {
            "old_version" => value["schema_version"] = 11.into(),
            "missing_version" => {
                value.as_object_mut().unwrap().remove("schema_version");
            }
            "missing_candidate_policy" => {
                value["config"]["allocation"]
                    .as_object_mut()
                    .unwrap()
                    .remove("candidate_policy");
            }
            _ => {
                value["config"]["allocation"]
                    .as_object_mut()
                    .unwrap()
                    .remove("solver_tolerance");
            }
        }
        let rejected = match serde_json::from_value::<ResearchTemplate>(value.clone()) {
            Ok(t) => t.validate().is_err(),
            Err(_) => true,
        };
        assert!(rejected, "{mutation}");
        assert_ne!(value, current);
    }
}
