use analytics_storage::{
    catalog::{CatalogEntry, CatalogStore},
    index::Index,
    metadata::ConfigStore,
    run_store::{RunRecord, RunStatus, RunStore},
};
use serde_json::json;
use std::path::PathBuf;
use uuid::Uuid;

#[test]
fn catalog_indexed_roundtrip_range_evidence_and_atomic_replacement() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("index.sqlite");
    let mut store = CatalogStore::open(&db).unwrap();
    let mut entry = CatalogEntry {
        id: "test-market".into(),
        kind: "market".into(),
        path: PathBuf::from("data/market"),
        signature: "sha256:test".into(),
        start: Some("2020-01-01".into()),
        end: Some("2020-12-31".into()),
        details: json!({"anchors":{"SH000300":{"x":null}},"evidence_sha256":{"response.json":"sha256:one"},"collector":{"calendar":["2020-01-01"],"formula_version":"v5","features":["b","a"]}}),
    };
    store.insert(&entry).unwrap();
    assert_eq!(
        serde_json::to_value(store.get(&entry.id).unwrap().unwrap()).unwrap(),
        serde_json::to_value(&entry).unwrap()
    );
    assert_eq!(store.list_by_kind("market").unwrap().len(), 1);
    assert!(
        store
            .list(Some("market"), Some("2021-01-01"), None)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.evidence(&entry.id).unwrap()["response.json"],
        "sha256:one"
    );
    assert_eq!(
        store.find_by_evidence_hash("sha256:one").unwrap(),
        vec![entry.id.clone()]
    );
    entry.details = json!({"raw_sha256":{"next.json":"sha256:two"}});
    store.insert(&entry).unwrap();
    assert!(
        store
            .find_by_evidence_hash("sha256:one")
            .unwrap()
            .is_empty()
    );
    let valid = serde_json::to_value(&entry).unwrap();
    entry.kind = "invalid".into();
    assert!(store.insert(&entry).is_err());
    assert_eq!(
        serde_json::to_value(store.get(&entry.id).unwrap().unwrap()).unwrap(),
        valid
    );
    store.remove(&entry.id).unwrap();
    assert!(store.evidence(&entry.id).unwrap().is_empty());
}
fn run(status: RunStatus) -> RunRecord {
    let id = Uuid::new_v4();
    RunRecord {
        schema_version: 1,
        id,
        operation: "fixture".into(),
        started_at: "2026-10-20T00:00:00Z".into(),
        finished_at: None,
        status,
        request: json!({"nullable":null,"features":["z","a"],"schema_hash":"unchanged"}),
        signature: "sha256:request".into(),
        output: format!(".jfquant/runs/{id}").into(),
        stage: "fixture".into(),
        error: None,
        read_only: false,
    }
}
#[test]
fn every_run_state_roundtrips_and_archived_records_remain_queryable() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("index.sqlite");
    let store = RunStore::open(&db).unwrap();
    for status in [
        RunStatus::Running,
        RunStatus::Succeeded,
        RunStatus::Failed,
        RunStatus::Blocked,
        RunStatus::Cancelled,
        RunStatus::Interrupted,
        RunStatus::Imported,
    ] {
        let r = run(status);
        store.save(&r.output, &r).unwrap();
        assert_eq!(
            serde_json::to_value(store.get(&r.id).unwrap().unwrap()).unwrap(),
            serde_json::to_value(&r).unwrap()
        );
    }
    assert_eq!(store.load_all().unwrap().len(), 7);
    assert_eq!(store.list_active().unwrap().len(), 1);
    let r = store.list_active().unwrap().pop().unwrap();
    store.update_status(&r.id, RunStatus::Cancelled).unwrap();
    assert!(store.update_status(&r.id, RunStatus::Succeeded).is_err());
    let archived = PathBuf::from(format!(".jfquant/archive/fixture/runs/{}", r.id));
    store.archive(&r.output, &archived).unwrap();
    assert!(store.at_path(&r.output).unwrap().is_none());
    assert_eq!(store.at_path(&archived).unwrap().unwrap().id, r.id);
}
#[test]
fn v1_upgrade_and_cache_pruning_preserve_authoritative_metadata() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("index.sqlite");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO metadata VALUES('fixture','kept'); PRAGMA user_version=1;").unwrap();
    drop(conn);
    let store = ConfigStore::open(&db).unwrap();
    store
        .set(
            "ui",
            "ui",
            &json!({"features":["second","first"],"missing":null}),
        )
        .unwrap();
    drop(store);
    let index = Index::open(&db).unwrap();
    index.begin("one").unwrap();
    index.complete("one").unwrap();
    index.prune("one").unwrap();
    drop(index);
    assert_eq!(
        ConfigStore::open(&db)
            .unwrap()
            .get::<serde_json::Value>("ui")
            .unwrap()
            .unwrap()["features"],
        json!(["second", "first"])
    );
    let conn = rusqlite::Connection::open(&db).unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        conn.query_row("SELECT value FROM metadata WHERE key='fixture'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "kept"
    );
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    assert!(
        conn.execute(
            "INSERT INTO catalog_evidence VALUES('orphan','x','hash')",
            []
        )
        .is_err()
    );
}
#[test]
fn progress_during_scan_does_not_deadlock_or_lose_terminal_status() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("index.sqlite");
    let r = run(RunStatus::Running);
    let store = RunStore::open(&db).unwrap();
    store.save(&r.output, &r).unwrap();
    let index = Index::open(&db).unwrap();
    index.begin("busy").unwrap();
    store.progress(&r.id, "scan").unwrap();
    drop(index);
    store.update_status(&r.id, RunStatus::Succeeded).unwrap();
    assert_eq!(
        store.get(&r.id).unwrap().unwrap().status,
        RunStatus::Succeeded
    );
}
