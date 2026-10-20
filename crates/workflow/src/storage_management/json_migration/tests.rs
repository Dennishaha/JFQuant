use super::*;
use crate::{DataVersion, RunStatus};
use analytics_storage::{catalog::CatalogStore, metadata::ConfigStore, run_store::RunStore};
fn fixture() -> (tempfile::TempDir, Workspace, RunRecord) {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let run = RunRecord {
        schema_version: 1,
        id: Uuid::new_v4(),
        operation: "fixture".into(),
        started_at: crate::now(),
        finished_at: Some(crate::now()),
        status: RunStatus::Succeeded,
        request: json!({"features":["b","a"],"formula_version":"v5","missing":null}),
        signature: "sha256:frozen".into(),
        output: PathBuf::new(),
        stage: "finished".into(),
        error: None,
        read_only: false,
    };
    let mut run = run;
    run.output = format!(".jfquant/runs/{}", run.id).into();
    let entry = DataVersion {
        id: "coefficients-test".into(),
        kind: "coefficients".into(),
        path: "data/coefficients/manifest.json".into(),
        signature: "sha256:data".into(),
        start: None,
        end: None,
        details: json!({"adjustment_anchors":{"SH000300":{"value":null}},"raw_sha256":{"response.json":"sha256:evidence"}}),
    };
    let p = w.resolve(&run.output);
    fs::create_dir_all(&p).unwrap();
    fs::write(p.join("run.json"), serde_json::to_vec_pretty(&run).unwrap()).unwrap();
    fs::write(
        w.resolve(".jfquant/catalog/coefficients-test.json"),
        serde_json::to_vec_pretty(&entry).unwrap(),
    )
    .unwrap();
    fs::write(
        w.resolve(".jfquant/shared-ui-jobs.json"),
        br#"{"task":"retained","features":["b","a"]}"#,
    )
    .unwrap();
    (temp, w, run)
}
#[test]
fn json_import_preserves_fields_bytes_backups_and_journal_and_is_repeatable() {
    let (_temp, w, run) = fixture();
    let original = fs::read(w.resolve(&run.output).join("run.json")).unwrap();
    let mut migration = JsonToSqliteMigration::open(w.root()).unwrap();
    let r = migration.execute(&TaskContext::default()).unwrap();
    assert_eq!(r.files, 3);
    assert_eq!(r.runs, 1);
    assert_eq!(r.catalog, 1);
    assert_eq!(r.configs, 1);
    assert!(!w.resolve(&run.output).join("run.json").exists());
    assert_eq!(
        fs::read(w.resolve(&r.backup).join(&run.output).join("run.json")).unwrap(),
        original
    );
    assert_eq!(
        store::read(w.resolve(&run.output).join("run.json")).unwrap(),
        original
    );
    let loaded = RunStore::open(&w.database_path().unwrap())
        .unwrap()
        .get(&run.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(loaded).unwrap(),
        serde_json::to_value(run).unwrap()
    );
    assert_eq!(
        CatalogStore::open(&w.database_path().unwrap())
            .unwrap()
            .evidence("coefficients-test")
            .unwrap()["response.json"],
        "sha256:evidence"
    );
    assert_eq!(
        ConfigStore::open(&w.database_path().unwrap())
            .unwrap()
            .get::<Value>(".jfquant/shared-ui-jobs.json")
            .unwrap()
            .unwrap()["features"],
        json!(["b", "a"])
    );
    let journal = fs::read_to_string(w.resolve(r.journal)).unwrap();
    let ops: Vec<Value> = journal
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    for op in ["backup", "insert", "commit", "delete", "complete"] {
        assert!(ops.iter().any(|v| v["op"] == op));
    }
    assert_eq!(migration.execute(&TaskContext::default()).unwrap().files, 0);
}
#[test]
fn malformed_json_rolls_back_all_tables_and_keeps_originals() {
    let (_temp, w, run) = fixture();
    fs::write(w.resolve(".jfquant/shared-ui-jobs.json"), b"invalid").unwrap();
    let mut migration = JsonToSqliteMigration::open(w.root()).unwrap();
    assert!(migration.execute(&TaskContext::default()).is_err());
    assert!(w.resolve(&run.output).join("run.json").exists());
    assert!(
        CatalogStore::open(&w.database_path().unwrap())
            .unwrap()
            .list(None, None, None)
            .unwrap()
            .is_empty()
    );
    assert!(
        RunStore::open(&w.database_path().unwrap())
            .unwrap()
            .get(&run.id)
            .unwrap()
            .is_none()
    );
}
#[test]
fn committed_crash_recovers_deletions_without_reimport_or_lost_records() {
    let (_temp, w, run) = fixture();
    FAULT.with(|f| f.set(Some("after_commit")));
    let mut migration = JsonToSqliteMigration::open(w.root()).unwrap();
    assert!(
        migration
            .execute(&TaskContext::default())
            .unwrap_err()
            .to_string()
            .contains("after_commit")
    );
    assert!(w.resolve(&run.output).join("run.json").exists());
    assert!(w.run(run.id).unwrap().is_some());
    recover(&w).unwrap();
    assert!(!w.resolve(&run.output).join("run.json").exists());
    assert!(w.run(run.id).unwrap().is_some());
    assert_eq!(migration.execute(&TaskContext::default()).unwrap().files, 0);
}
#[test]
fn foreign_workspace_lease_prevents_migration_and_cancellation_keeps_sources() {
    let (_temp, w, run) = fixture();
    let lease = w.lease().unwrap();
    let mut migration = JsonToSqliteMigration::open(w.root()).unwrap();
    assert!(migration.execute(&TaskContext::default()).is_err());
    drop(lease);
    let context = TaskContext::default();
    context
        .cancel
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(migration.execute(&context).is_err());
    assert!(w.resolve(&run.output).join("run.json").exists());
}

#[test]
fn packages_and_nested_migration_candidates_preserve_every_field_and_hash() {
    let (_temp, w, _run) = fixture();
    let root = PathBuf::from("data/evidence");
    let member = PathBuf::from("raw/body.json");
    fs::create_dir_all(w.resolve(root.join("raw"))).unwrap();
    fs::write(w.resolve(root.join(&member)), b"original evidence bytes\n").unwrap();
    let object = store::estimate(&w.resolve(root.join(&member))).unwrap();
    store::create_object(
        w.root(),
        &w.resolve(root.join(&member)),
        &object,
        &w.resolve("object.tmp"),
    )
    .unwrap();
    let map = store::PackageMap {
        schema_version: 1,
        id: store::package_id(&root).unwrap(),
        root: root.clone(),
        identity_root: root.clone(),
        manifest: "actions.json".into(),
        manifest_sha256: store::digest(b"{}"),
        resources: vec!["resource-b".into(), "resource-a".into()],
        members: BTreeMap::from([(member.clone(), object.clone())]),
    };
    // Compact legacy bytes deliberately differ from the canonical new representation.
    let original = serde_json::to_vec(&map).unwrap();
    let mut registry = store::Registry::default();
    registry.packages.insert(
        map.id.clone(),
        store::PackageLocator {
            root: root.clone(),
            map_sha256: store::digest(&original),
            released: false,
        },
    );
    fs::create_dir_all(w.resolve(".jfquant/storage/packages")).unwrap();
    fs::write(store::map_path(w.root(), &map.id).unwrap(), &original).unwrap();
    fs::write(
        w.resolve(store::REGISTRY),
        serde_json::to_vec(&registry).unwrap(),
    )
    .unwrap();
    let id = Uuid::new_v4();
    let stamp =
        analytics_storage::filesystem::FileStamp::read(&w.resolve(root.join(&member))).unwrap();
    let plan = json!({"schema_version":1,"id":id,"created_at":crate::now(),"object_storage_bytes":object.stored_bytes,"packages":[{"map":map,"members":[{"path":root.join(&member),"object":object,"stamp":stamp}]}],"frozen_extensions":{"schema_hash":"abc","formula_version":"v5","features":["z","a"],"missing":null}});
    fs::create_dir_all(w.resolve(".jfquant/storage/migration-plans")).unwrap();
    fs::write(
        w.resolve(format!(".jfquant/storage/migration-plans/{id}.json")),
        serde_json::to_vec_pretty(&plan).unwrap(),
    )
    .unwrap();
    let r = JsonToSqliteMigration::open(w.root())
        .unwrap()
        .execute(&TaskContext::default())
        .unwrap();
    assert_eq!(r.packages, 1);
    assert_eq!(r.plans, 1);
    assert_eq!(
        serde_json::to_value(store::registry(w.root()).unwrap()).unwrap(),
        serde_json::to_value(registry.clone()).unwrap()
    );
    assert_eq!(
        serde_json::to_value(
            store::load_map(w.root(), &map.id, &registry.packages[&map.id]).unwrap()
        )
        .unwrap(),
        serde_json::to_value(&map).unwrap()
    );
    assert_eq!(
        analytics_storage::migration_store::MigrationStore::open(&w.database_path().unwrap())
            .unwrap()
            .get(&id.to_string())
            .unwrap()
            .unwrap(),
        plan
    );
    fs::remove_file(w.resolve(root.join(&member))).unwrap();
    assert_eq!(
        store::read(w.resolve(root.join(member))).unwrap(),
        b"original evidence bytes\n"
    );
    let conn = database::open(&w.database_path().unwrap()).unwrap();
    let foreign_keys: i64 = conn
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(foreign_keys, 0);
}
#[test]
fn sql_failure_before_commit_rolls_back_and_modified_original_blocks_recovery() {
    let (_temp, w, run) = fixture();
    FAULT.with(|f| f.set(Some("before_commit")));
    assert!(
        JsonToSqliteMigration::open(w.root())
            .unwrap()
            .execute(&TaskContext::default())
            .unwrap_err()
            .to_string()
            .contains("before_commit")
    );
    assert!(w.run(run.id).unwrap().is_none());
    assert!(w.resolve(&run.output).join("run.json").exists());
    FAULT.with(|f| f.set(Some("after_commit")));
    assert!(
        JsonToSqliteMigration::open(w.root())
            .unwrap()
            .execute(&TaskContext::default())
            .is_err()
    );
    let p = w.resolve(&run.output).join("run.json");
    let original = fs::read(&p).unwrap();
    fs::write(&p, b"changed").unwrap();
    assert!(
        recover(&w)
            .unwrap_err()
            .to_string()
            .contains("source_changed")
    );
    assert_eq!(fs::read(&p).unwrap(), b"changed");
    fs::write(&p, &original).unwrap();
    recover(&w).unwrap();
    assert!(!p.exists());
    assert!(w.run(run.id).unwrap().is_some());
}

#[test]
fn explicit_rollback_restores_originals_and_recovers_an_interrupted_restore() {
    let (_temp, w, run) = fixture();
    let original = fs::read(w.resolve(&run.output).join("run.json")).unwrap();
    let mut migration = JsonToSqliteMigration::open(w.root()).unwrap();
    let report = migration.execute(&TaskContext::default()).unwrap();
    FAULT.with(|f| f.set(Some("rollback_after_restore")));
    assert!(
        migration
            .rollback(report.id)
            .unwrap_err()
            .to_string()
            .contains("rollback_after_restore")
    );
    assert!(w.run(run.id).unwrap().is_some());
    recover(&w).unwrap();
    assert!(w.run(run.id).unwrap().is_none());
    assert_eq!(
        fs::read(w.resolve(&run.output).join("run.json")).unwrap(),
        original
    );
    assert_eq!(migration.rollback(report.id).unwrap()["rolled_back"], true);
    assert_eq!(migration.execute(&TaskContext::default()).unwrap().files, 3);
}
#[test]
fn rollback_refuses_to_delete_records_after_new_metadata_writes() {
    let (_temp, w, run) = fixture();
    let mut migration = JsonToSqliteMigration::open(w.root()).unwrap();
    let report = migration.execute(&TaskContext::default()).unwrap();
    ConfigStore::open(&w.database_path().unwrap())
        .unwrap()
        .set("new", "ui", &json!({"keep":true}))
        .unwrap();
    assert!(
        migration
            .rollback(report.id)
            .unwrap_err()
            .to_string()
            .contains("metadata_changes")
    );
    assert!(w.run(run.id).unwrap().is_some());
    assert!(!w.resolve(&run.output).join("run.json").exists());
}

#[test]
fn normal_open_rejects_an_archive_only_legacy_workspace_before_schema_changes() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let id = Uuid::new_v4();
    let archive = w.resolve(format!(".jfquant/archive/fixture/runs/{id}"));
    fs::create_dir_all(&archive).unwrap();
    fs::write(archive.join("run.json"), b"{}").unwrap();
    assert!(
        Workspace::open(w.root())
            .unwrap_err()
            .to_string()
            .contains("json_metadata_migration_required")
    );
    assert!(!w.database_path().unwrap().exists());
}
