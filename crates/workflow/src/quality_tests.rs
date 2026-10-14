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
        actions: a.id.clone(),
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

#[test]
fn three_policies_distinguish_missing_input_null_propagation_and_security_exclusion() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
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
    assert_eq!(relaxed["admission"]["schema_version"], 2);
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
    analytics_ml::backtest::validate_result(&selected).unwrap();
}

#[test]
fn future_missing_observation_changes_only_future_qualification() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
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
        let root = w.resolve(m.path);
        if case == "truncated" {
            fs::write(root.join("features/sh600000/amount.day.bin"), [1, 2, 3]).unwrap();
        } else if case == "coefficient" {
            set_qlib_point(&root, "sh600000", "factor", 5, 0.2);
        } else {
            set_qlib_point(&root, "sh600000", "volume", 5, -1.);
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
fn possible_held_security_missing_quote_volume_or_payment_blocks_before_fit_in_every_mode() {
    for case in ["payment", "close", "volume"] {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (m, a, mut t) = demo::create(&w).unwrap();
        let (m, a) = if case == "payment" {
            let path = w.resolve(&a.path);
            let mut book = ActionBundle::read(&path).unwrap();
            book.actions[0].pay_date = None;
            atomic_json(&path, &book).unwrap();
            (m, w.register(&path).unwrap())
        } else {
            let root = w.resolve(m.path);
            set_qlib_point(&root, "sh600000", case, 27, f32::NAN);
            (w.register(&root).unwrap(), a)
        };
        for mode in [
            ResearchStrictness::Strict,
            ResearchStrictness::AllowMissing,
            ResearchStrictness::QualifiedOnly,
        ] {
            t.quality_policy.strictness = mode;
            let (run, result) = execute_quality(&w, &m, &a, &t, true);
            let result = result.unwrap().details;
            assert_eq!(result["ready"], false, "{mode:?}");
            assert!(!result["settlement_issues"].as_object().unwrap().is_empty());
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
    assert!(analytics_ml::backtest::validate_result(&tampered).is_err());
    let mut incomplete = high.clone();
    incomplete["sample_selection"]["selections"] = json!([]);
    incomplete["execution"]["sample_selection"] = incomplete["sample_selection"].clone();
    incomplete["execution"]["sample_selection_sha256"] = json!(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&incomplete["sample_selection"]).unwrap())
    ));
    assert!(analytics_ml::backtest::validate_result(&incomplete).is_err());
    let mut old = high;
    old["schema_version"] = json!(8);
    assert!(analytics_ml::backtest::validate_result(&old).is_err());
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
fn future_event_category_gap_excludes_from_the_gap_without_changing_past() {
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
            assert_eq!(a["retained"], 1);
        }
    }
    let (_, result) = execute_quality(&w, &m, &a, &t, true);
    assert_eq!(result.unwrap().details["ready"], false); // Earlier possible holdings keep later settlement requirements.
}

#[test]
fn missing_later_source_alias_does_not_erase_earlier_qualified_history() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, mut t) = demo::create(&w).unwrap();
    t.quality_policy.strictness = ResearchStrictness::QualifiedOnly;
    let root = w.resolve(&m.path);
    let next = root.join("features/sh600002");
    fs::create_dir_all(&next).unwrap();
    for entry in fs::read_dir(root.join("features/sh600000")).unwrap() {
        let path = entry.unwrap().path();
        fs::copy(&path, next.join(path.file_name().unwrap())).unwrap();
    }
    let mut directory = w.identities(&t.identity_version).unwrap();
    let id = demo::fixture_security_id("SH600000");
    let record = directory.securities.get_mut(&id).unwrap();
    let locator = record
        .locators
        .iter_mut()
        .find(|l| l.provider == "qlib")
        .unwrap();
    locator.coverage.end = "2009-01-12".parse().unwrap();
    let mut locator = locator.clone();
    locator.symbol = "SH600002".into();
    locator.coverage = analytics_core::DateRange {
        start: "2010-01-02".parse().unwrap(),
        end: "2010-01-12".parse().unwrap(),
    };
    record.locators.push(locator);
    let input = w.resolve("data/changed-alias/registry.json");
    fs::create_dir_all(input.parent().unwrap()).unwrap();
    atomic_json(&input, &directory).unwrap();
    t.identity_version = w.import_identities(&input).unwrap().id;
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
    assert_eq!(result.unwrap().details["schema_version"], 9);
    let mut dataset = analytics_storage::daily::DailyDataset::open(&root).unwrap();
    dataset.bind_registry(directory).unwrap();
    dataset
        .restrict_reads(analytics_core::DateRange {
            start: "2007-01-02".parse().unwrap(),
            end: "2009-01-12".parse().unwrap(),
        })
        .unwrap();
    assert!(dataset.field(&id, "amount").is_ok());
}

#[test]
fn failure_reasons_reference_the_verified_bundle_without_repeating_every_raw_document() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (m, a, t) = demo::create(&w).unwrap();
    let path = w.resolve(a.path);
    let mut book = ActionBundle::read(&path).unwrap();
    for n in 0..100 {
        let name = format!("unrelated-{n}.json");
        let bytes = format!("{{\"fixture\":{n}}}").into_bytes();
        fs::write(path.parent().unwrap().join("raw").join(&name), &bytes).unwrap();
        book.raw_sha256
            .insert(name, format!("sha256:{:x}", Sha256::digest(bytes)));
    }
    book.actions[0].pay_date = None;
    atomic_json(&path, &book).unwrap();
    let a = w.register(&path).unwrap();
    let (_, result) = execute_quality(&w, &m, &a, &t, true);
    let result = result.unwrap().details;
    let check = result["admission"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["category"] == "settlement" && c["status"] == "failed")
        .unwrap();
    let evidence = check["evidence"].as_array().unwrap();
    assert!(evidence.len() < 10);
    assert!(
        evidence
            .iter()
            .any(|e| e.as_str().unwrap().starts_with("action-bundle:sha256:"))
    );
}
