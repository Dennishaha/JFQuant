use super::*;
use crate::{RunStatus, signature};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

fn context() -> TaskContext {
    TaskContext::new(Uuid::new_v4(), Arc::new(AtomicBool::new(false)), |_| {})
}
fn file(w: &Workspace, path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) {
    let p = w.resolve(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}
fn json_file(w: &Workspace, path: impl AsRef<Path>, value: Value) {
    file(w, path, serde_json::to_vec(&value).unwrap());
}
fn cache(w: &Workspace, key: &str) -> PathBuf {
    let root = PathBuf::from(format!(".jfquant/runs/.factor-shards-v1/{key}/2020"));
    file(w, root.join("features.parquet"), vec![42u8; 16384]);
    json_file(
        w,
        root.join("manifest.json"),
        json!({"schema_version":1,"signature":key,"features":["fixture"],"sha256":"fixture-hash"}),
    );
    root
}
fn resource(w: &Workspace, path: &Path) -> StorageResource {
    scan(w, &context())
        .unwrap()
        .resources
        .into_iter()
        .find(|r| r.path == path)
        .unwrap()
}
fn saved_plan(w: &Workspace, ids: &[String]) -> CleanupPlan {
    let p = plan(w, ids, &context()).unwrap();
    atomic_json(
        &w.resolve(format!(".jfquant/storage/plans/{}.json", p.id)),
        &p,
    )
    .unwrap();
    p
}
fn completed_run(w: &Workspace) -> RunRecord {
    let mut run = w
        .begin(
            Uuid::new_v4(),
            "fixture",
            json!({"request":{"operation":"research"},"inputs":[]}),
        )
        .unwrap();
    json_file(
        w,
        run.output.join("artifacts.json"),
        json!({"aggregate":{"annual_return":0.12},"fixture":true}),
    );
    file(
        w,
        run.output.join("nav.csv"),
        "date,nav,benchmark\n2020-01-02,1,1\n2020-01-03,1.1,1.02\n",
    );
    w.finish(&mut run, RunStatus::Succeeded, None).unwrap();
    run
}
#[test]
fn storage_inventory_counts_categories_and_hardlinks_once_without_reading_data() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let root = cache(&w, "one");
    fs::hard_link(
        w.resolve(root.join("features.parquet")),
        w.resolve("data/external-link"),
    )
    .unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::hard_link(
        w.resolve(root.join("features.parquet")),
        outside.path().join("retained"),
    )
    .unwrap();
    let s = scan(&w, &context()).unwrap();
    assert!(s.complete, "{:?}", s.diagnostics);
    assert_eq!(
        s.total.logical_bytes,
        s.categories.values().map(|u| u.logical_bytes).sum::<u64>()
    );
    assert_eq!(
        s.total.allocated_bytes,
        s.categories
            .values()
            .map(|u| u.allocated_bytes)
            .sum::<u64>()
    );
    let original =
        analytics_storage::index::Index::open_read(&w.resolve(".jfquant/storage/index.sqlite"))
            .unwrap()
            .table::<StorageFile>(&s.scan_id.to_string(), "files")
            .get(&root.join("features.parquet"))
            .unwrap()
            .stamp;
    let id = resource(&w, &root).id;
    let p = saved_plan(&w, &[id]);
    assert!(p.blockers.is_empty(), "{:?}", p.blockers);
    assert!(p.estimated_release.allocated_bytes < original.allocated);
    apply(&w, p.id, &context()).unwrap();
    assert!(w.resolve("data/external-link").is_file());
    assert!(outside.path().join("retained").is_file());
}
#[test]
fn storage_cache_release_is_selective_idempotent_and_preserves_unknown_files() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let root = cache(&w, "one");
    let other = cache(&w, "two");
    file(
        &w,
        root.join("user-note.txt"),
        b"never delete an unlisted file",
    );
    let p = saved_plan(&w, &[resource(&w, &root).id]);
    assert!(p.blockers.is_empty(), "{:?}", p.blockers);
    let first = apply(&w, p.id, &context()).unwrap();
    assert_eq!(first.status, "completed");
    assert!(!w.resolve(root.join("features.parquet")).exists());
    assert!(w.resolve(root.join("user-note.txt")).exists());
    assert!(w.resolve(other.join("features.parquet")).exists());
    assert_eq!(
        apply(&w, p.id, &context()).unwrap().deleted.logical_bytes,
        first.deleted.logical_bytes
    );
}
#[test]
fn storage_plans_reject_changed_bytes_new_dependencies_and_protected_config() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let root = cache(&w, "one");
    let id = resource(&w, &root).id;
    let p = saved_plan(&w, std::slice::from_ref(&id));
    file(&w, root.join("features.parquet"), b"changed");
    assert!(
        apply(&w, p.id, &context())
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
    let p = saved_plan(&w, std::slice::from_ref(&id));
    json_file(&w, "my-plan.json", json!({"unparsed_reference":root}));
    assert!(
        apply(&w, p.id, &context())
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
    assert!(!saved_plan(&w, &[id]).blockers.is_empty());
    let config = resource(&w, Path::new(".jfquant/workspace.json"));
    assert!(!config.deletable);
}
#[test]
fn storage_unknown_formats_are_rejected_before_deletion_or_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let root = cache(&w, "strict-format");
    let mut inventory = scan(&w, &context()).unwrap();
    for version in [0, 1, 2, 4] {
        inventory.schema_version = version;
        atomic_json(&w.resolve(".jfquant/storage/inventory.json"), &inventory).unwrap();
        assert!(
            latest(&w)
                .unwrap_err()
                .to_string()
                .contains("format_or_workspace_mismatch")
        );
    }
    inventory.schema_version = 3;
    inventory.workspace = "/stale-workspace".into();
    atomic_json(&w.resolve(".jfquant/storage/inventory.json"), &inventory).unwrap();
    assert!(latest(&w).is_err());
    let id = resource(&w, &root).id;
    let mut p = saved_plan(&w, &[id]);
    p.schema_version = 2;
    atomic_json(
        &w.resolve(format!(".jfquant/storage/plans/{}.json", p.id)),
        &p,
    )
    .unwrap();
    assert!(apply(&w, p.id, &context()).is_err());
    assert!(w.resolve(root.join("features.parquet")).is_file());
    assert!(
        !w.resolve(format!(".jfquant/storage/transactions/{}", p.id))
            .exists()
    );
    let (mut journal, txn) = crash_journal(&w, true);
    journal.result.schema_version = 2;
    cleanup::durable_write(&txn.join("journal.json"), &journal).unwrap();
    assert!(
        recover(&w)
            .unwrap_err()
            .to_string()
            .contains("storage_invalid_journal")
    );
    assert!(txn.join("pending/0").is_file());
}
#[test]
fn storage_preserves_task_summary_curve_and_signed_record_after_release() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let run = completed_run(&w);
    let original =
        analytics_storage::evidence_store::read(w.resolve(run.output.join("run.json"))).unwrap();
    let expected = w.results(&run).unwrap();
    let id = resource(&w, &run.output).id;
    let p = saved_plan(&w, &[id]);
    assert!(p.blockers.is_empty(), "{:?}", p.blockers);
    apply(&w, p.id, &context()).unwrap();
    assert_eq!(w.results(&run).unwrap(), expected);
    assert_eq!(
        analytics_storage::evidence_store::read(w.resolve(run.output.join("run.json"))).unwrap(),
        original
    );
    let view = crate::views::run(w.root(), run.id).unwrap();
    assert_eq!(view.nav.len(), 2);
    assert!(view.availability.historical_view);
    assert!(!view.availability.full_verification_available);
    assert!(
        require_available(&w, run.id, "verification")
            .unwrap_err()
            .to_string()
            .contains("released")
    );
}
#[test]
fn storage_model_release_keeps_shared_prediction_masks_and_blocks_model_reuse() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let run = completed_run(&w);
    json_file(
        &w,
        run.output.join("training.json"),
        json!({"schema_version":5,"status":"trained"}),
    );
    file(&w, run.output.join("fold-0000/model.txt"), b"model");
    file(&w, run.output.join("factor-cache/2020.bin"), b"samples");
    let notes = [
        run.output.join("fold-0000/user-note.txt"),
        run.output.join("factor-cache/user-note.txt"),
        run.output.join("sample_masks/user-note.json"),
    ];
    for note in &notes {
        file(&w, note, b"unlisted user file");
    }
    json_file(
        &w,
        run.output.join("prediction_manifest.json"),
        json!({"schema_version":1,"signature":"sha256:fixture"}),
    );
    file(&w, run.output.join("predictions.parquet"), b"scores");
    json_file(
        &w,
        run.output.join("sample_masks/fold-0000-train.json"),
        json!({"row_mask":[true]}),
    );
    let model = resource(&w, &run.output.join("training.json"));
    let p = saved_plan(&w, &[model.id]);
    assert!(p.blockers.is_empty(), "{:?}", p.blockers);
    assert!(
        !p.files
            .iter()
            .any(|f| f.path.starts_with(run.output.join("sample_masks")))
    );
    apply(&w, p.id, &context()).unwrap();
    for note in &notes {
        assert!(w.resolve(note).is_file());
    }
    assert!(
        w.resolve(run.output.join("sample_masks/fold-0000-train.json"))
            .exists()
    );
    assert!(
        require_available(&w, run.id, "models")
            .unwrap_err()
            .to_string()
            .contains("released")
    );
}
#[test]
fn storage_shared_prediction_cache_requires_explicit_dependent_selection() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let run = completed_run(&w);
    let root = PathBuf::from(".jfquant/runs/.predictions-v1/fixture");
    file(&w, root.join("predictions.parquet"), b"scores");
    json_file(
        &w,
        root.join("manifest.json"),
        json!({"schema_version":1,"signature":"sha256:fixture","files":{"predictions.parquet":"hash"}}),
    );
    json_file(
        &w,
        run.output.join("prediction_manifest.json"),
        json!({"schema_version":1,"signature":"sha256:fixture"}),
    );
    file(&w, run.output.join("predictions.parquet"), b"scores");
    let shared = resource(&w, &root);
    let task = resource(&w, &run.output.join("prediction_manifest.json"));
    assert!(
        !saved_plan(&w, std::slice::from_ref(&shared.id))
            .blockers
            .is_empty()
    );
    assert!(saved_plan(&w, &[shared.id, task.id]).blockers.is_empty());
}
#[test]
fn storage_original_data_can_be_released_with_only_historical_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (market, _, _) = crate::demo::create(&w).unwrap();
    let note = market.path.join("user-note.txt");
    file(&w, &note, b"unlisted user file inside market root");
    let mut run = completed_run(&w);
    run.request["inputs"] = json!([market]);
    run.signature = signature(&run.request).unwrap();
    w.save_run(&run).unwrap();
    let data = resource(&w, &market.path);
    let p = saved_plan(&w, &[data.id]);
    assert!(p.blockers.is_empty(), "{:?}", p.blockers);
    assert!(!p.impacts.is_empty());
    apply(&w, p.id, &context()).unwrap();
    assert!(w.resolve(note).is_file());
    assert!(!w.datasets().unwrap().iter().any(|d| d.id == market.id));
    assert_eq!(w.results(&run).unwrap()["fixture"], true);
    assert!(!availability(&w, run.id).unwrap().original_inputs_available);
    assert!(
        crate::usecases::version_metadata(&w, &market.id, "market")
            .unwrap_err()
            .to_string()
            .contains("released")
    );
    assert!(
        analytics_storage::catalog::CatalogStore::open(&w.database_path().unwrap())
            .unwrap()
            .get(&market.id)
            .unwrap()
            .is_some()
    );
}
#[test]
fn storage_tree_hashes_and_checkpoint_markers_do_not_authorize_unknown_members() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (market, _, _) = crate::demo::create(&w).unwrap();
    json_file(
        &w,
        market.path.parent().unwrap().join("jfquant_source.json"),
        json!({"synthetic_fixture":true,"evidence_tree_sha256":"opaque-tree-hash"}),
    );
    let run = completed_run(&w);
    let checkpoint = PathBuf::from(format!(".jfquant/staging/refresh-{}", run.id));
    json_file(&w, checkpoint.join("owner.json"), json!({"task":run.id}));
    file(&w, checkpoint.join("user-note.txt"), b"unknown member");
    for path in [market.path, checkpoint] {
        let r = resource(&w, &path);
        assert!(!r.deletable);
        assert!(!saved_plan(&w, &[r.id]).blockers.is_empty());
    }
}
#[test]
fn storage_redownload_registers_new_identity_without_resurrecting_old_inputs() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (market, _, _) = crate::demo::create_canonical(&w).unwrap();
    file(
        &w,
        market.path.parent().unwrap().join("raw/user-note.txt"),
        b"unlisted sibling",
    );
    let resource = resource(&w, &market.path);
    let p = saved_plan(&w, &[resource.id]);
    assert!(p.blockers.is_empty(), "{:?}", p.blockers);
    let saved: Vec<_> = p
        .files
        .iter()
        .map(|f| {
            (
                f.path.clone(),
                analytics_storage::evidence_store::read(w.resolve(&f.path)).unwrap(),
            )
        })
        .collect();
    apply(&w, p.id, &context()).unwrap();
    assert!(
        w.resolve(market.path.parent().unwrap().join("raw/user-note.txt"))
            .is_file()
    );
    for (path, bytes) in saved {
        file(&w, path, bytes);
    }
    let new = w.register(&w.resolve(&market.path)).unwrap();
    assert_ne!(new.id, market.id);
    assert_eq!(new.signature, market.signature);
    assert_eq!(w.register(&w.resolve(&market.path)).unwrap().id, new.id);
    let mut run = completed_run(&w);
    run.request["inputs"] = json!([new]);
    run.signature = signature(&run.request).unwrap();
    w.save_run(&run).unwrap();
    assert!(availability(&w, run.id).unwrap().original_inputs_available);
}
#[test]
fn storage_market_view_integrity_dependency_blocks_partial_deletion() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (base, _, _) = crate::demo::create(&w).unwrap();
    let root = PathBuf::from("data/fixture-view/market");
    json_file(
        &w,
        root.join("market_view.json"),
        json!({"schema_version":1,"base_path":base.path,"base_sha256":"hash","coefficients_path":base.path,"coefficients_sha256":"hash","recovery":"fixture","records":{},"anchors":{}}),
    );
    json_file(
        &w,
        "data/fixture-view/jfquant_source.json",
        json!({"downloader":"JFQuant analytics-storage"}),
    );
    let view = crate::DataVersion {
        id: "market-view-fixture".into(),
        kind: "market".into(),
        path: root.clone(),
        signature: "fixture".into(),
        start: None,
        end: None,
        details: json!({}),
    };
    analytics_storage::catalog::CatalogStore::open(&w.database_path().unwrap())
        .unwrap()
        .insert(&view)
        .unwrap();
    let b = resource(&w, &base.path);
    let v = resource(&w, &root);
    assert!(
        !saved_plan(&w, std::slice::from_ref(&b.id))
            .blockers
            .is_empty()
    );
    assert!(saved_plan(&w, &[b.id, v.id]).blockers.is_empty());
}
#[test]
fn storage_symlinks_are_not_followed_and_unknown_targets_stay_protected() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let root = cache(&w, "one");
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), b"external").unwrap();
    std::os::unix::fs::symlink(outside.path(), w.resolve(root.join("external"))).unwrap();
    let s = scan(&w, &context()).unwrap();
    assert!(!resource(&w, &root).deletable);
    assert!(!s.files.iter().any(|f| f.path.ends_with("secret")));
    assert!(
        s.external_references
            .contains(&outside.path().canonicalize().unwrap())
    );
}
fn crash_journal(w: &Workspace, committed: bool) -> (cleanup::Journal, PathBuf) {
    let root = cache(w, "crash");
    let p = saved_plan(w, &[resource(w, &root).id]);
    let txn = w.resolve(format!(".jfquant/storage/transactions/{}", p.id));
    fs::create_dir_all(txn.join("pending")).unwrap();
    let journal = cleanup::Journal {
        schema_version: 1,
        task: Uuid::new_v4(),
        result: CleanupResult {
            schema_version: 1,
            plan: p.id,
            started_at: crate::now(),
            completed_at: None,
            committed,
            status: if committed { "purging" } else { "prepared" }.into(),
            resources: p.resources.iter().map(|r| r.id.clone()).collect(),
            deleted: SpaceUsage::default(),
            available_before: None,
            available_after: None,
            errors: Default::default(),
        },
        plan: p,
    };
    for (i, f) in journal.plan.files.iter().enumerate() {
        fs::rename(w.resolve(&f.path), txn.join("pending").join(i.to_string())).unwrap();
    }
    cleanup::durable_write(&txn.join("journal.json"), &journal).unwrap();
    (journal, txn)
}
#[test]
fn storage_recovery_rolls_back_precommit_and_completes_committed_deletion() {
    for committed in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (j, txn) = crash_journal(&w, committed);
        recover(&w).unwrap();
        for f in &j.plan.files {
            assert_eq!(w.resolve(&f.path).exists(), !committed);
        }
        let result: CleanupResult = read(&txn.join("result.json")).unwrap();
        assert_eq!(
            result.status,
            if committed {
                "completed"
            } else {
                "rolled_back"
            }
        );
        recover(&w).unwrap();
    }
}
#[test]
fn storage_recovery_preserves_changed_pending_files_and_exposes_retry() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (j, txn) = crash_journal(&w, true);
    fs::write(txn.join("pending/0"), b"unexpected replacement").unwrap();
    recover(&w).unwrap();
    let result: CleanupResult = read(&txn.join("result.json")).unwrap();
    assert_eq!(result.status, "pending_deletion");
    assert!(!result.errors.is_empty());
    assert!(txn.join("pending/0").exists());
    assert_eq!(
        apply(&w, j.plan.id, &context()).unwrap().status,
        "pending_deletion"
    );
}
#[test]
fn storage_recovery_keeps_directory_sync_failures_pending_until_retry() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (j, txn) = crash_journal(&w, true);
    // Simulate unlinks followed by an unavailable staging directory before its sync.
    fs::remove_dir_all(txn.join("pending")).unwrap();
    recover(&w).unwrap();
    let result: CleanupResult = read(&txn.join("result.json")).unwrap();
    assert_eq!(result.status, "pending_deletion");
    assert!(result.completed_at.is_none());
    assert!(result.errors[Path::new("pending")].contains("storage_deletion_sync_failed"));
    let journal: cleanup::Journal = read(&txn.join("journal.json")).unwrap();
    assert_eq!(journal.result.status, "pending_deletion");
    fs::create_dir(txn.join("pending")).unwrap();
    let retried = apply(&w, j.plan.id, &context()).unwrap();
    assert_eq!(retried.status, "completed");
    assert!(retried.completed_at.is_some());
    assert!(retried.errors.is_empty());
}
#[test]
fn storage_cancellation_before_commit_preserves_inputs() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let root = cache(&w, "one");
    let p = saved_plan(&w, &[resource(&w, &root).id]);
    let c = context();
    c.cancel.store(true, Ordering::Relaxed);
    assert!(apply(&w, p.id, &c).is_err());
    assert!(w.resolve(root.join("features.parquet")).exists());
}
#[test]
fn storage_archive_aliases_count_once_and_release_preserves_alias_record() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let run = completed_run(&w);
    cache(&w, "archive");
    crate::maintenance::apply(w.root()).unwrap();
    let inventory = scan(&w, &context()).unwrap();
    assert!(inventory.complete, "{:?}", inventory.diagnostics);
    let resource = inventory
        .resources
        .iter()
        .find(|r| r.task == Some(run.id) && r.category == ResourceCategory::Backtest)
        .unwrap();
    assert!(resource.archived);
    let p = saved_plan(&w, std::slice::from_ref(&resource.id));
    assert!(p.blockers.is_empty(), "{:?}", p.blockers);
    apply(&w, p.id, &context()).unwrap();
    assert!(w.run(run.id).unwrap().is_some());
    assert!(!w.resolve(run.output.join("run.json")).is_file());
    assert!(historical_view(&w, run.id).unwrap().is_some());
}
#[test]
fn storage_unreadable_directories_make_scan_incomplete_and_block_cleanup() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let root = cache(&w, "one");
    file(&w, "unreadable/file", b"not zero");
    let protected = w.resolve("unreadable");
    fs::set_permissions(&protected, fs::Permissions::from_mode(0o0)).unwrap();
    let s = scan(&w, &context()).unwrap();
    fs::set_permissions(&protected, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!s.complete);
    assert!(!s.diagnostics.is_empty());
    assert_eq!(s.reclaimable_allocated_bytes_estimate, Some(0));
    let id = resource(&w, &root).id;
    assert!(
        !dependencies::build_plan(&w, &s, &[id], Uuid::new_v4())
            .unwrap()
            .blockers
            .is_empty()
    );
}
#[test]
fn storage_accounting_excludes_retained_shared_owners_from_release_estimate() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let root = cache(&w, "one");
    let inventory = scan(&w, &context()).unwrap();
    let id = inventory
        .resources
        .iter()
        .find(|r| r.path == root)
        .unwrap()
        .id
        .clone();
    let index =
        analytics_storage::index::Index::open_read(&w.resolve(".jfquant/storage/index.sqlite"))
            .unwrap();
    let mut files: Vec<StorageFile> = index
        .selected(&inventory.scan_id.to_string(), std::slice::from_ref(&id))
        .unwrap();
    let f = files
        .iter_mut()
        .find(|f| f.path == root.join("features.parquet"))
        .unwrap();
    f.owners.push("retained".into());
    let usage = dependencies::release_usage(&files, &BTreeSet::from([id]));
    assert!(usage.logical_bytes < 16384);
}
#[test]
fn storage_releasing_real_models_preserves_verified_prediction_replay_and_history() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (market, actions, template) = crate::demo::create(&w).unwrap();
    let request = crate::WorkflowRequest::Research {
        template: Box::new(template),
        market: market.id,
        actions: Some(actions.id.clone()),
        audit_only: false,
    };
    let c = context();
    let mut run = w
        .begin(
            c.id,
            "native storage acceptance",
            crate::snapshot(&w, &request).unwrap(),
        )
        .unwrap();
    crate::execute(&w, &request, &run, &c).unwrap();
    w.finish(&mut run, RunStatus::Succeeded, None).unwrap();
    let original = w.results(&run).unwrap();
    let descriptor =
        analytics_storage::evidence_store::read(w.resolve(run.output.join("training.json")))
            .unwrap();
    let model = resource(&w, &run.output.join("training.json"));
    let p = saved_plan(&w, &[model.id]);
    assert!(p.blockers.is_empty(), "{:?}", p.blockers);
    apply(&w, p.id, &context()).unwrap();
    assert_eq!(w.results(&run).unwrap(), original);
    let rescanned = scan(&w, &context()).unwrap();
    assert!(
        rescanned.resources.iter().any(|r| r.task == Some(run.id)
            && r.category == ResourceCategory::Models
            && r.released)
    );
    assert!(!w.resolve(run.output.join("fold-0000/model.txt")).exists());
    assert_eq!(
        analytics_storage::evidence_store::read(w.resolve(run.output.join("training.json")))
            .unwrap(),
        descriptor
    );
    assert_eq!(
        crate::research::backtest::verification::report(&w, run.id).unwrap()["status"],
        "consistent"
    );
    let replay = crate::WorkflowRequest::BacktestPredictions {
        allocation: None,
        source: run.id,
        actions: actions.id,
        rights_policy: Default::default(),
    };
    let c = context();
    let mut next = w
        .begin(
            c.id,
            "replay after model release",
            crate::snapshot(&w, &replay).unwrap(),
        )
        .unwrap();
    crate::execute(&w, &replay, &next, &c).unwrap();
    w.finish(&mut next, RunStatus::Succeeded, None).unwrap();
    assert_eq!(
        crate::research::backtest::verification::report(&w, next.id).unwrap()["status"],
        "consistent"
    );
}

#[test]
fn storage_tiers_keep_overview_content_free_and_require_preview_for_dependencies() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let root = cache(&w, "tiers");
    json_file(&w, "user-reference.json", json!({"unknown_reference":root}));
    let ctx = context();
    let overview = inventory::scan_with(&w, &ctx, ScanLevel::Overview, 4, false, None).unwrap();
    assert!(overview.files.is_empty());
    assert!(overview.resources.is_empty());
    assert!(!overview.dependencies_checked);
    assert!(!overview.directories.is_empty());
    let identified = inventory::scan_with(&w, &ctx, ScanLevel::Resources, 4, false, None).unwrap();
    assert!(!identified.dependencies_checked);
    assert!(identified.references.is_empty());
    let id = identified
        .resources
        .iter()
        .find(|r| r.path == root)
        .unwrap()
        .id
        .clone();
    assert!(!plan(&w, &[id], &ctx).unwrap().blockers.is_empty());
}
#[test]
fn storage_cached_summary_never_reads_full_file_index_and_old_plans_expire() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let root = cache(&w, "summary-only");
    let ctx = context();
    let inventory = scan(&w, &ctx).unwrap();
    save_inventory(&w, inventory).unwrap();
    file(&w, ".jfquant/storage/files.json", "invalid full index");
    assert!(latest(&w).unwrap().files.is_empty());
    let id = resource(&w, &root).id;
    let mut plan = saved_plan(&w, &[id]);
    plan.metadata_fingerprint = "old-fingerprint".into();
    plan.fingerprint =
        signature(&(&plan.resources, &plan.files, &plan.metadata_fingerprint)).unwrap();
    atomic_json(
        &w.resolve(format!(".jfquant/storage/plans/{}.json", plan.id)),
        &plan,
    )
    .unwrap();
    assert!(apply(&w, plan.id, &ctx).is_err());
    assert!(w.resolve(root.join("features.parquet")).exists());
}
#[test]
fn storage_live_snapshot_finishes_and_cancellation_preserves_previous_summary() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    cache(&w, "snapshot");
    let run = completed_run(&w);
    let ctx = context();
    let request = StorageRequest::Scan {
        level: ScanLevel::Overview,
        threads: 4,
    };
    execute(&w, &request, &run, &ctx).unwrap();
    let snapshot = snapshot(&w).unwrap().unwrap();
    assert!(snapshot.finished && snapshot.complete);
    assert_eq!(snapshot.scan_id, ctx.id);
    let previous =
        analytics_storage::evidence_store::read(w.resolve(".jfquant/storage/inventory.json"))
            .unwrap();
    let cancelled = context();
    cancelled.cancel.store(true, Ordering::Relaxed);
    assert!(execute(&w, &request, &run, &cancelled).is_err());
    assert_eq!(
        analytics_storage::evidence_store::read(w.resolve(".jfquant/storage/inventory.json"))
            .unwrap(),
        previous
    );
    let stopped = super::snapshot(&w).unwrap().unwrap();
    assert!(stopped.finished && !stopped.complete);
}

#[test]
fn storage_own_progress_writes_do_not_invalidate_dependency_verification() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let root = cache(&w, "own-progress");
    let id = Uuid::new_v4();
    let record = w
        .begin(id, "扫描", json!({"request":{"operation":"storage"}}))
        .unwrap();
    let progress_workspace = w.clone();
    let ctx = TaskContext::new(id, Arc::new(AtomicBool::new(false)), move |stage| {
        progress_workspace.progress(&record, stage).unwrap();
    });
    let inventory = inventory::scan_with(&w, &ctx, ScanLevel::Resources, 4, true, None).unwrap();
    assert!(inventory.complete, "{:?}", inventory.diagnostics);
    let id = inventory
        .resources
        .iter()
        .find(|r| r.path == root)
        .unwrap()
        .id
        .clone();
    assert!(
        dependencies::build_plan(&w, &inventory, &[id], Uuid::new_v4())
            .unwrap()
            .blockers
            .is_empty()
    );
}

#[test]
fn storage_resources_detect_files_changed_between_traversal_and_revalidation() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let root = cache(&w, "changed-during-scan");
    let path = w.resolve(root.join("features.parquet"));
    let ctx = TaskContext::new(
        Uuid::new_v4(),
        Arc::new(AtomicBool::new(false)),
        move |stage| {
            if stage == "重新校验文件元数据" {
                fs::write(&path, b"changed").unwrap();
            }
        },
    );
    let inventory = inventory::scan_with(&w, &ctx, ScanLevel::Resources, 4, false, None).unwrap();
    assert!(!inventory.complete);
    assert!(
        inventory
            .diagnostics
            .iter()
            .any(|d| d.contains("changed during scan"))
    );
}

#[test]
fn storage_startup_finishes_interrupted_progress_without_replacing_completed_inventory() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let run = completed_run(&w);
    execute(
        &w,
        &StorageRequest::Scan {
            level: ScanLevel::Overview,
            threads: 1,
        },
        &run,
        &context(),
    )
    .unwrap();
    let previous =
        analytics_storage::evidence_store::read(w.resolve(".jfquant/storage/inventory.json"))
            .unwrap();
    let mut progress = snapshot(&w).unwrap().unwrap();
    progress.finished = false;
    progress.complete = false;
    progress.phase = "遍历文件元数据".into();
    let path = w.resolve(".jfquant/storage/snapshot.json");
    atomic_json(&path, &progress).unwrap();
    recover_snapshot(&w).unwrap();
    let recovered = snapshot(&w).unwrap().unwrap();
    assert!(recovered.finished && !recovered.complete);
    assert_eq!(recovered.scan_id, progress.scan_id);
    assert_eq!(recovered.progress.total, progress.progress.total);
    assert!(!recovered.diagnostics.is_empty());
    assert_eq!(
        analytics_storage::evidence_store::read(w.resolve(".jfquant/storage/inventory.json"))
            .unwrap(),
        previous
    );
    let once = analytics_storage::evidence_store::read(&path).unwrap();
    recover_snapshot(&w).unwrap();
    assert_eq!(
        analytics_storage::evidence_store::read(&path).unwrap(),
        once
    );
    file(
        &w,
        ".jfquant/storage/snapshot.json",
        "invalid progress cache",
    );
    assert!(recover_snapshot(&w).is_ok());
    assert!(snapshot(&w).is_err());
}
