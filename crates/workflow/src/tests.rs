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
    assert_eq!(result.details["schema_version"], 4);
    assert!(
        result.details["aggregate"]["total_return"]
            .as_f64()
            .unwrap()
            .abs()
            < 1e-6
    );
    let ledger: Value = serde_json::from_slice(
        &fs::read(w.resolve(&run.output).join("fold-2009/ledger.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(ledger["positions"]["SH600000"]["shares"], 110);
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
