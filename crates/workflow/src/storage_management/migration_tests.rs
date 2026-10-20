use super::*;
use std::sync::{Arc, atomic::AtomicBool};
fn context() -> TaskContext {
    TaskContext::new(Uuid::new_v4(), Arc::new(AtomicBool::new(false)), |_| {})
}
fn fixture() -> (tempfile::TempDir, Workspace, Vec<StorageResource>) {
    fixture_layout(false)
}
fn fixture_layout(tree: bool) -> (tempfile::TempDir, Workspace, Vec<StorageResource>) {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let (_, actions, _) = crate::demo::create_canonical(&w).unwrap();
    let shared = serde_json::to_vec(
        &serde_json::json!({"receipt":"original bytes with whitespace ".repeat(12000)}),
    )
    .unwrap();
    let mut versions = vec![];
    for i in 0..2 {
        let root = w.resolve(format!("data/evidence-{i}"));
        store::copy_tree(&w.resolve(actions.path.parent().unwrap()), &root).unwrap();
        let manifest = root.join("actions.json");
        let mut v: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        v["downloaded_at"] = format!("fixture-{i}").into();
        v["raw_sha256"]["shared-audit.json"] = store::digest(&shared).into();
        fs::write(root.join("raw/shared-audit.json"), &shared).unwrap();
        if tree {
            let tree = root.join("source-evidence/nested");
            fs::create_dir_all(&tree).unwrap();
            fs::write(tree.join("same-original.json"), &shared).unwrap();
            fs::write(tree.join("other.txt"), b"other original bytes\n").unwrap();
            let proof =
                serde_json::to_vec(&store::tree_hash(&root.join("source-evidence")).unwrap())
                    .unwrap();
            fs::write(root.join("raw/source-evidence-sha256.json"), &proof).unwrap();
            v["raw_sha256"]["source-evidence-sha256.json"] = store::digest(&proof).into();
        }
        crate::atomic_json(&manifest, &v).unwrap();
        versions.push(w.register(&manifest).unwrap());
    }
    let inventory = inventory::scan(&w, &context()).unwrap();
    let resources = versions
        .iter()
        .map(|v| {
            inventory
                .resources
                .iter()
                .find(|r| r.name == v.id)
                .unwrap()
                .clone()
        })
        .collect();
    (temp, w, resources)
}
fn planned(w: &Workspace, resources: &[StorageResource]) -> MigrationPlan {
    let p = plan(
        w,
        &resources.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        &context(),
    )
    .unwrap();
    assert!(p.blockers.is_empty(), "{:?}", p.blockers);
    assert!(!p.packages.is_empty());
    p
}
#[test]
fn storage_migration_estimates_compress_each_original_once_and_reject_changed_copies() {
    let (_temp, w, resources) = fixture_layout(true);
    let mut estimates = Estimates::default();
    let first = package(&w, &resources[0], &context(), &mut estimates).unwrap();
    let runs = estimates.compressions;
    assert!(
        runs < first.members.len(),
        "same raw bytes also appear inside the tree"
    );
    let second = package(&w, &resources[1], &context(), &mut estimates).unwrap();
    assert_eq!(
        estimates.compressions, runs,
        "copies must reuse the compression estimate"
    );
    assert_eq!(first.map.members, second.map.members);
    let raw = w.resolve(
        resources[1]
            .path
            .parent()
            .unwrap()
            .join("raw/shared-audit.json"),
    );
    let original = fs::read(&raw).unwrap();
    let source = SourceMember {
        sha256: store::digest(&original),
        stamp: None,
    };
    fs::write(&raw, vec![b'!'; original.len()]).unwrap();
    assert!(
        estimates.inspect(&raw, &source).is_err(),
        "cache reuse still hashes every raw copy"
    );
    let tree = w.resolve(
        resources[0]
            .path
            .parent()
            .unwrap()
            .join("source-evidence/nested/same-original.json"),
    );
    let source = SourceMember {
        sha256: store::digest(&original),
        stamp: Some(FileStamp::read(&tree).unwrap()),
    };
    fs::write(&tree, vec![b'?'; original.len()]).unwrap();
    assert!(
        estimates.inspect(&tree, &source).is_err(),
        "previously inspected tree identity changed"
    );
    assert!(
        contract(&w, &resources[0].path)
            .unwrap_err()
            .to_string()
            .contains("migration_evidence_tree_changed")
    );
}
#[test]
fn publication_uses_only_the_manifest_supplied_by_the_producer() {
    let (_temp, w, resources) = fixture();
    let context = context();
    let run = w
        .begin(context.id, "publication fixture", serde_json::json!({}))
        .unwrap();
    // An unrelated package contains this task ID and a conventional output directory exists.
    // Neither may be adopted by the producer's explicit publication.
    let unrelated = w.resolve(format!("data/unrelated-{}", run.id));
    store::copy_tree(&w.resolve(resources[1].path.parent().unwrap()), &unrelated).unwrap();
    w.register(&unrelated.join("actions.json")).unwrap();
    let stray = w.resolve(&run.output).join("captured");
    store::copy_tree(&w.resolve(resources[1].path.parent().unwrap()), &stray).unwrap();
    let data = w
        .datasets()
        .unwrap()
        .into_iter()
        .find(|d| d.path == resources[0].path)
        .unwrap();
    published(&w, &w.resolve(&data.path), Some(&data), &run, &context).unwrap();
    let maps = store::maps(w.root()).unwrap();
    assert_eq!(maps.len(), 1);
    assert_eq!(maps[0].root, resources[0].path.parent().unwrap());
    assert_eq!(maps[0].resources, vec![resources[0].id.clone()]);
    assert!(
        !w.resolve(
            resources[0]
                .path
                .parent()
                .unwrap()
                .join("raw/shared-audit.json")
        )
        .exists()
    );
    assert!(unrelated.join("raw/shared-audit.json").is_file());
    assert!(stray.join("raw/shared-audit.json").is_file());
}
fn crashed(
    w: &Workspace,
    plan: MigrationPlan,
    stage: usize,
    committed: bool,
) -> (PathBuf, Journal) {
    let root = txn(w, plan.id).unwrap();
    fs::create_dir_all(root.join("pending")).unwrap();
    fs::create_dir_all(root.join("created")).unwrap();
    let mut registry = store::registry(w.root()).unwrap();
    for p in &plan.packages {
        registry.packages.insert(
            p.map.id.clone(),
            PackageLocator {
                root: p.map.root.clone(),
                map_sha256: store::digest(&serde_json::to_vec_pretty(&p.map).unwrap()),
                released: false,
            },
        );
    }
    let result = MigrationResult {
        schema_version: 1,
        plan: plan.id,
        created_at: plan.created_at.clone(),
        workspace: w.root().into(),
        committed,
        completed: false,
        status: "fixture crash".into(),
        deleted: SpaceUsage::default(),
        object_bytes_added: 0,
        evidence_logical_bytes_delta: 0,
        available_before: None,
        available_after: None,
        errors: BTreeMap::new(),
    };
    let mut journal = Journal {
        schema_version: 1,
        task: Uuid::new_v4(),
        plan,
        result,
        registry,
        created: vec![],
    };
    for member in all_members(&journal) {
        let temp = root
            .join("created")
            .join(format!("{}.obj", &member.object.sha256[7..]));
        store::prepare_object(w.root(), &w.resolve(&member.path), &member.object, &temp).unwrap();
    }
    journal.created = all_members(&journal)
        .map(|m| m.object.sha256.clone())
        .collect();
    journal.created.sort();
    journal.created.dedup();
    for (i, m) in all_members(&journal).enumerate().take(stage) {
        fs::rename(w.resolve(&m.path), root.join("pending").join(i.to_string())).unwrap();
    }
    save(&root, &journal).unwrap();
    (root, journal)
}
#[test]
fn storage_migration_deduplicates_preserves_bytes_hashes_and_exports_independent_package() {
    let (temp, w, resources) = fixture();
    let p = planned(&w, &resources);
    assert_eq!(
        p.fingerprint,
        crate::signature(&(&p.packages, &p.registry_sha256)).unwrap()
    );
    assert!(p.original_reference_bytes > p.unique_original_bytes);
    assert!(p.object_storage_bytes < p.unique_original_bytes / 2);
    let manifest_bytes: Vec<_> = resources
        .iter()
        .map(|r| fs::read(w.resolve(&r.path)).unwrap())
        .collect();
    let hashes: Vec<_> = p
        .packages
        .iter()
        .map(|p| store::tree_hash(&w.resolve(&p.map.root)).unwrap())
        .collect();
    let result = apply(&w, p.id, &context()).unwrap();
    assert!(result.completed && result.committed);
    assert!(result.object_bytes_added > 0);
    assert_eq!(
        result.evidence_logical_bytes_delta,
        result.object_bytes_added as i64 - result.deleted.logical_bytes as i64
    );
    assert!(result.evidence_logical_bytes_delta < 0);
    assert_eq!(
        apply(&w, p.id, &context()).unwrap().deleted.files,
        result.deleted.files
    );
    for (i, package) in p.packages.iter().enumerate() {
        assert_eq!(
            store::tree_hash(&w.resolve(&package.map.root)).unwrap(),
            hashes[i]
        );
        for m in &package.members {
            assert!(!w.resolve(&m.path).exists());
            assert_eq!(
                store::digest(&store::read(w.resolve(&m.path)).unwrap()),
                m.object.sha256
            );
        }
        assert_eq!(
            fs::read(w.resolve(&resources[i].path)).unwrap(),
            manifest_bytes[i]
        );
        crate::verify_action_evidence(
            &w.resolve(&resources[i].path),
            &ActionBundle::read(&w.resolve(&resources[i].path)).unwrap(),
        )
        .unwrap();
    }
    let output = temp.path().join("exported");
    export(&w, &resources[0].id, &output, &context()).unwrap();
    assert_eq!(
        store::tree_hash(&output).unwrap(),
        store::tree_hash(&w.resolve(resources[0].path.parent().unwrap())).unwrap()
    );
    assert!(ActionBundle::read(&output.join("actions.json")).is_ok());
    assert!(export(&w, &resources[0].id, &output, &context()).is_err());
    rusqlite::Connection::open(w.database_path().unwrap()).unwrap().execute_batch("DELETE FROM records; DELETE FROM owners; DELETE FROM resources; DELETE FROM generations; DELETE FROM metadata WHERE key='latest';").unwrap();
    let rebuilt = inventory::scan(&w, &context()).unwrap();
    assert!(rebuilt.complete, "{:?}", rebuilt.diagnostics);
    assert!(
        resources
            .iter()
            .all(|r| rebuilt.resources.iter().any(|v| v.id == r.id))
    );
    let preview = MigrationPreview::from(&p);
    assert_eq!(
        preview.packages.iter().map(|p| p.members).sum::<u64>(),
        p.original_file_usage.files
    );
    assert!(preview.unique_objects < p.original_file_usage.files);
    assert!(
        serde_json::to_vec(&preview).unwrap().len() < serde_json::to_vec(&p).unwrap().len() / 2
    );
    assert!(w.resolve(&preview.members_database).is_file());
    let json = serde_json::to_value(&p).unwrap();
    assert!(json["packages"][0]["members"].is_array());
}
#[test]
fn storage_migration_shared_object_cleanup_keeps_retained_owner_and_rejects_fallback() {
    let (_temp, w, resources) = fixture();
    let p = planned(&w, &resources);
    apply(&w, p.id, &context()).unwrap();
    let shared = &p.packages[0].map.members[Path::new("raw/shared-audit.json")];
    let object = store::object_path(w.root(), &shared.sha256).unwrap();
    let first = super::super::plan(&w, &[resources[0].id.clone()], &context()).unwrap();
    assert!(first.blockers.is_empty(), "{:?}", first.blockers);
    cleanup::durable_write(
        &w.resolve(format!(".jfquant/storage/plans/{}.json", first.id)),
        &first,
    )
    .unwrap();
    cleanup::apply(&w, first.id, &context()).unwrap();
    assert!(object.is_file());
    assert!(ActionBundle::read(&w.resolve(&resources[1].path)).is_ok());
    let second = super::super::plan(&w, &[resources[1].id.clone()], &context()).unwrap();
    assert!(second.blockers.is_empty(), "{:?}", second.blockers);
    cleanup::durable_write(
        &w.resolve(format!(".jfquant/storage/plans/{}.json", second.id)),
        &second,
    )
    .unwrap();
    cleanup::apply(&w, second.id, &context()).unwrap();
    assert!(!object.exists());
    let original = w.resolve(p.packages[0].map.root.join("raw/shared-audit.json"));
    fs::write(&original, b"untrusted old copy").unwrap();
    assert!(
        store::read(original)
            .unwrap_err()
            .to_string()
            .contains("已释放")
    );
}
#[test]
fn storage_migration_stale_plan_and_cancel_preserve_originals() {
    let (_temp, w, resources) = fixture();
    let p = planned(&w, &resources);
    let member = &p.packages[0].members[0];
    fs::write(w.resolve(&member.path), b"changed").unwrap();
    assert!(apply(&w, p.id, &context()).is_err());
    assert!(!w.resolve(store::REGISTRY).exists());
    let (_temp, w, resources) = fixture();
    let p = planned(&w, &resources);
    let c = context();
    c.cancel.store(true, Ordering::Relaxed);
    assert!(apply(&w, p.id, &c).is_err());
    for m in p.packages.iter().flat_map(|p| &p.members) {
        assert!(w.resolve(&m.path).is_file());
    }
}
#[test]
fn storage_migration_recovers_before_and_after_commit_and_partial_map_publication() {
    for phase in 0..5 {
        let (_temp, w, resources) = fixture();
        let plan = planned(&w, &resources);
        let count = plan.packages.iter().map(|p| p.members.len()).sum();
        let committed = phase >= 2;
        let (root, journal) = crashed(&w, plan, if committed { count } else { phase }, committed);
        if phase >= 3 {
            analytics_storage::package_store::PackageStore::open(&w.database_path().unwrap())
                .unwrap()
                .save(&journal.plan.packages[0].map, false)
                .unwrap();
        }
        if phase == 4 {
            fs::remove_file(root.join("pending/0")).unwrap();
        }
        recover(&w).unwrap();
        recover(&w).unwrap();
        let recovered: Journal = read(&root.join("journal.json")).unwrap();
        assert!(recovered.result.completed);
        for resource in &resources {
            assert!(ActionBundle::read(&w.resolve(&resource.path)).is_ok());
        }
        if !committed {
            for m in all_members(&journal) {
                assert!(w.resolve(&m.path).exists());
                assert!(
                    !store::object_path(w.root(), &m.object.sha256)
                        .unwrap()
                        .exists()
                );
            }
        }
    }
}
#[test]
fn storage_migration_failed_delete_stays_pending_then_recovers_and_bad_map_is_protected() {
    let (_temp, w, resources) = fixture();
    let p = planned(&w, &resources);
    let count = p.packages.iter().map(|p| p.members.len()).sum();
    let (root, j) = crashed(&w, p, count, true);
    fs::rename(root.join("pending/0"), root.join("held")).unwrap();
    fs::create_dir(root.join("pending/0")).unwrap();
    assert!(recover(&w).is_err());
    let pending: Journal = read(&root.join("journal.json")).unwrap();
    assert!(
        pending.result.committed && !pending.result.completed && !pending.result.errors.is_empty()
    );
    fs::remove_dir(root.join("pending/0")).unwrap();
    fs::rename(root.join("held"), root.join("pending/0")).unwrap();
    recover(&w).unwrap();
    rusqlite::Connection::open(w.database_path().unwrap())
        .unwrap()
        .execute(
            "UPDATE package_maps SET integrity_sha256='corrupt' WHERE id=?1",
            [&j.plan.packages[0].map.id],
        )
        .unwrap();
    let raw = w.resolve(j.plan.packages[0].members[0].path.clone());
    fs::write(&raw, b"old copy").unwrap();
    assert!(store::read(raw).is_err());
    assert!(inventory::scan(&w, &context()).is_err());
}
#[test]
fn storage_migration_archive_and_workspace_move_keep_original_tree_identity() {
    let (temp, w, resources) = fixture();
    let p = planned(&w, &resources);
    apply(&w, p.id, &context()).unwrap();
    let before = store::tree_hash(&w.resolve(&p.packages[0].map.root)).unwrap();
    let old = p.packages[0].map.root.clone();
    let new = PathBuf::from("data/moved-evidence");
    fs::rename(w.resolve(&old), w.resolve(&new)).unwrap();
    archive_packages(w.root(), &old, &new).unwrap();
    assert_eq!(store::tree_hash(&w.resolve(&new)).unwrap(), before);
    let destination = temp
        .path()
        .with_extension(format!("moved-{}", Uuid::new_v4()));
    let original = temp.keep();
    fs::rename(&original, &destination).unwrap();
    let moved = Workspace::open(&destination).unwrap();
    assert_eq!(store::tree_hash(&moved.resolve(&new)).unwrap(), before);
    assert_eq!(
        apply(&moved, p.id, &context()).unwrap().workspace,
        moved.root()
    );
    fs::remove_dir_all(destination).unwrap();
}
#[test]
fn storage_migration_maintenance_alias_preserves_owner_and_completed_journal_recovery() {
    let (_temp, w, resources) = fixture();
    let c = context();
    let mut run = w
        .begin(
            c.id,
            "evidence",
            serde_json::json!({"request":{"operation":"evidence"}}),
        )
        .unwrap();
    let root = run.output.join("validated-preview");
    store::copy_tree(
        &w.resolve(resources[0].path.parent().unwrap()),
        &w.resolve(&root),
    )
    .unwrap();
    w.finish(&mut run, crate::RunStatus::Succeeded, None)
        .unwrap();
    let resource = inventory::scan(&w, &context())
        .unwrap()
        .resources
        .into_iter()
        .find(|r| r.path == root)
        .unwrap();
    let p = planned(&w, std::slice::from_ref(&resource));
    apply(&w, p.id, &context()).unwrap();
    let hash = store::tree_hash(&w.resolve(&root)).unwrap();
    crate::maintenance::apply(w.root()).unwrap();
    assert_eq!(store::tree_hash(&w.resolve(&root)).unwrap(), hash);
    super::super::recover(&w).unwrap();
    let inventory = inventory::scan(&w, &context()).unwrap();
    assert!(inventory.complete, "{:?}", inventory.diagnostics);
    assert!(
        inventory
            .resources
            .iter()
            .any(|r| r.id == resource.id && r.archived)
    );
}
#[test]
fn storage_migration_preserves_external_hardlinks_and_reports_deduplicated_capacity() {
    let (_temp, w, resources) = fixture();
    let root = resources[0].path.parent().unwrap();
    let source = w.resolve(root.join("raw/shared-audit.json"));
    let alias = w.resolve(root.join("raw/alias-audit.json"));
    fs::hard_link(&source, &alias).unwrap();
    let retained = w.resolve("retained-audit.bin");
    fs::hard_link(&source, &retained).unwrap();
    let mut value: Value =
        serde_json::from_slice(&fs::read(w.resolve(&resources[0].path)).unwrap()).unwrap();
    value["raw_sha256"]["alias-audit.json"] = value["raw_sha256"]["shared-audit.json"].clone();
    crate::atomic_json(&w.resolve(&resources[0].path), &value).unwrap();
    let data = w.register(&w.resolve(&resources[0].path)).unwrap();
    let resource = inventory::scan(&w, &context())
        .unwrap()
        .resources
        .into_iter()
        .find(|r| r.name == data.id)
        .unwrap();
    let p = planned(&w, &[resource]);
    assert!(p.original_reference_bytes > p.original_file_usage.logical_bytes);
    let result = apply(&w, p.id, &context()).unwrap();
    assert!(result.completed);
    assert!(retained.is_file());
    assert_eq!(
        result.deleted.allocated_bytes,
        p.original_file_usage.allocated_bytes
    );
}

#[test]
fn storage_migration_cancellation_cannot_interrupt_rollback_or_committed_completion() {
    for committed in [false, true] {
        let (_temp, w, resources) = fixture();
        let p = planned(&w, &resources);
        let total = p.packages.iter().map(|p| p.members.len()).sum();
        let (root, mut journal) = crashed(&w, p.clone(), total, committed);
        fs::write(root.join("object.tmp"), b"interrupted encoding").unwrap();
        let cancelled = context();
        cancelled.cancel.store(true, Ordering::Relaxed);
        cancelled.execute(|| {
            if committed {
                assert!(finish(&w, &root, &mut journal).unwrap().completed);
            } else {
                rollback(&w, &root, &mut journal).unwrap();
            }
        });
        for member in p.packages.iter().flat_map(|p| &p.members) {
            assert_eq!(w.resolve(&member.path).is_file(), !committed);
            if committed {
                assert_eq!(
                    store::digest(&store::read(w.resolve(&member.path)).unwrap()),
                    member.object.sha256
                );
            }
        }
        if !committed {
            assert!(!root.join("object.tmp").exists());
        }
        recover(&w).unwrap();
    }
}

#[test]
fn storage_migration_rollback_protects_objects_without_its_inode_receipt() {
    let (_temp, w, resources) = fixture();
    let plan = planned(&w, &resources);
    let (root, mut journal) = crashed(&w, plan, 1, false);
    let hash = &journal.created[0];
    let path = store::object_path(w.root(), hash).unwrap();
    let encoded = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    fs::write(&path, &encoded).unwrap();
    rollback(&w, &root, &mut journal).unwrap();
    assert_eq!(fs::read(path).unwrap(), encoded);
    assert!(journal.result.completed);
    for resource in &resources {
        assert!(ActionBundle::read(&w.resolve(&resource.path)).is_ok());
    }
}

#[test]
fn storage_migration_commit_receipt_write_failure_recovers_before_reading_sources() {
    let (_temp, w, resources) = fixture();
    let plan = planned(&w, &resources);
    let count = plan.packages.iter().map(|p| p.members.len()).sum();
    let (root, mut journal) = crashed(&w, plan, count, true);
    fs::remove_file(root.join("result.json")).unwrap();
    fs::create_dir(root.join("result.json")).unwrap();
    assert!(finish(&w, &root, &mut journal).is_err());
    assert!(
        read::<Journal>(&root.join("journal.json"))
            .unwrap()
            .result
            .committed
    );
    fs::remove_dir(root.join("result.json")).unwrap();
    recover(&w).unwrap();
    assert!(
        read::<Journal>(&root.join("journal.json"))
            .unwrap()
            .result
            .completed
    );
    for resource in &resources {
        assert!(ActionBundle::read(&w.resolve(&resource.path)).is_ok());
    }
}

#[test]
fn storage_migration_announcement_contract_ignores_runtime_descriptors_and_preserves_http_proof() {
    let temp = tempfile::tempdir().unwrap();
    let w = Workspace::open(temp.path()).unwrap();
    let c = context();
    let mut run = w
        .begin(
            c.id,
            "evidence",
            serde_json::json!({"request":{"operation":"evidence"}}),
        )
        .unwrap();
    let root = run.output.join("announcements");
    fs::create_dir_all(w.resolve(root.join("raw"))).unwrap();
    let pdf = b"%PDF-immutable fixture";
    let text = b"original document text";
    for (name, bytes) in [
        ("notice.pdf", pdf.as_slice()),
        ("notice.txt", text.as_slice()),
        ("http-notice.body", pdf.as_slice()),
    ] {
        fs::write(w.resolve(root.join("raw").join(name)), bytes).unwrap();
    }
    let document = serde_json::json!({"id":"fixture","instrument":"SH600000","url":"https://example.org/notice.pdf","pdf":"notice.pdf","pdf_sha256":store::digest(pdf),"text":"notice.txt","text_sha256":store::digest(text)});
    let shard = serde_json::json!({"evidence":{"documents":[document],"http_receipts":[{"filename":"http-notice.body","url":"https://example.org/notice.pdf","status":200,"sha256":store::digest(pdf)}]}});
    let bytes = serde_json::to_vec_pretty(&shard).unwrap();
    fs::write(
        w.resolve(root.join("raw/SH600000-actions-structured.json")),
        &bytes,
    )
    .unwrap();
    let manifest = serde_json::json!({"schema_version":2,"documents":[document],"receipts":{"SH600000":store::digest(&bytes)},"request":{"runtime":{"filename":"python-runtime.tar.gz","sha256":"archive descriptors are not evidence members"}}});
    fs::write(
        w.resolve(root.join("announcements.json")),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    w.finish(&mut run, crate::RunStatus::Succeeded, None)
        .unwrap();
    let resource = inventory::scan(&w, &context())
        .unwrap()
        .resources
        .into_iter()
        .find(|r| r.path == root)
        .unwrap();
    let plan = planned(&w, &[resource]);
    assert_eq!(plan.packages[0].members.len(), 4);
    apply(&w, plan.id, &context()).unwrap();
    assert_eq!(
        review::verified_documents(&w.resolve(root.join("announcements.json")))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn storage_migration_history_reads_small_receipts_and_recovery_rebuilds_them() {
    let (_temp, w, resources) = fixture();
    let plan = planned(&w, &resources);
    apply(&w, plan.id, &context()).unwrap();
    let root = txn(&w, plan.id).unwrap();
    let journal = fs::read(root.join("journal.json")).unwrap();
    fs::write(root.join("journal.json"), b"not loaded by receipt query").unwrap();
    assert_eq!(history(&w).unwrap()[0].plan, plan.id);
    assert!(recover(&w).is_err()); // Authoritative recovery still validates the full journal.
    fs::write(root.join("journal.json"), journal).unwrap();
    fs::remove_file(root.join("result.json")).unwrap();
    recover(&w).unwrap();
    assert_eq!(history(&w).unwrap()[0].plan, plan.id);
}
