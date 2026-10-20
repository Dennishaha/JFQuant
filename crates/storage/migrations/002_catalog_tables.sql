-- Authoritative workspace records. Cache generations remain independently rebuildable.
CREATE TABLE catalog_entries(
 id TEXT PRIMARY KEY NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('market','actions','coefficients')),
 path TEXT NOT NULL, signature TEXT NOT NULL, start_date TEXT, end_date TEXT,
 details_json TEXT NOT NULL CHECK(json_valid(details_json)), created_at TEXT NOT NULL,
 schema_version INTEGER NOT NULL DEFAULT 2 CHECK(schema_version=2)
) WITHOUT ROWID;
CREATE INDEX catalog_by_kind ON catalog_entries(kind,start_date,end_date);
CREATE INDEX catalog_by_path ON catalog_entries(path);
CREATE TABLE catalog_evidence(
 catalog_id TEXT NOT NULL REFERENCES catalog_entries(id) ON DELETE CASCADE,
 filename TEXT NOT NULL, sha256 TEXT NOT NULL, PRIMARY KEY(catalog_id,filename)
) WITHOUT ROWID;
CREATE INDEX evidence_by_hash ON catalog_evidence(sha256);
CREATE TABLE run_records(
 id TEXT PRIMARY KEY NOT NULL, path TEXT NOT NULL UNIQUE, schema_version INTEGER NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('running','succeeded','failed','blocked','cancelled','interrupted','imported')),
 read_only INTEGER NOT NULL CHECK(read_only IN (0,1)), request_json TEXT NOT NULL CHECK(json_valid(request_json)),
 created_at TEXT NOT NULL, completed_at TEXT, operation TEXT NOT NULL, signature TEXT NOT NULL,
 output TEXT NOT NULL, stage TEXT NOT NULL, error TEXT, original_json BLOB
) WITHOUT ROWID;
CREATE INDEX run_by_status ON run_records(status,created_at);
CREATE INDEX run_by_path ON run_records(path);
CREATE TABLE package_maps(
 id TEXT PRIMARY KEY NOT NULL, root TEXT NOT NULL UNIQUE, identity_root TEXT NOT NULL,
 schema_version INTEGER NOT NULL, created_at TEXT NOT NULL,
 manifest TEXT NOT NULL, manifest_sha256 TEXT NOT NULL, map_sha256 TEXT NOT NULL, integrity_sha256 TEXT NOT NULL,
 released INTEGER NOT NULL DEFAULT 0 CHECK(released IN (0,1))
) WITHOUT ROWID;
CREATE INDEX package_by_root ON package_maps(root);
CREATE TABLE package_resources(
 package_id TEXT NOT NULL REFERENCES package_maps(id) ON DELETE CASCADE,
 resource_id TEXT NOT NULL, position INTEGER NOT NULL,
 PRIMARY KEY(package_id,resource_id), UNIQUE(package_id,position)
) WITHOUT ROWID;
CREATE TABLE package_members(
 package_id TEXT NOT NULL REFERENCES package_maps(id) ON DELETE CASCADE,
 relative_path TEXT NOT NULL, sha256 TEXT NOT NULL,
 original_bytes INTEGER NOT NULL CHECK(original_bytes>=0), stored_bytes INTEGER NOT NULL CHECK(stored_bytes>=0),
 PRIMARY KEY(package_id,relative_path)
) WITHOUT ROWID;
CREATE INDEX member_by_hash ON package_members(sha256);
CREATE TABLE migration_plans(
 id TEXT PRIMARY KEY NOT NULL, schema_version INTEGER NOT NULL, created_at TEXT NOT NULL,
 completed_at TEXT, status TEXT NOT NULL CHECK(status IN ('Planned','InProgress','Completed','Failed')),
 estimated_bytes INTEGER NOT NULL CHECK(estimated_bytes>=0), details_json TEXT NOT NULL CHECK(json_valid(details_json))
) WITHOUT ROWID;
CREATE TABLE migration_packages(
 plan_id TEXT NOT NULL REFERENCES migration_plans(id) ON DELETE CASCADE,
 position INTEGER NOT NULL, details_json TEXT NOT NULL CHECK(json_valid(details_json)),
 PRIMARY KEY(plan_id,position)
) WITHOUT ROWID;
CREATE TABLE migration_candidates(
 plan_id TEXT NOT NULL, package_position INTEGER NOT NULL, position INTEGER NOT NULL,
 path TEXT NOT NULL, sha256 TEXT NOT NULL, original_bytes INTEGER NOT NULL CHECK(original_bytes>=0),
 estimated_stored_bytes INTEGER NOT NULL CHECK(estimated_stored_bytes>=0), stamp_json TEXT NOT NULL CHECK(json_valid(stamp_json)),
 relative_path TEXT NOT NULL,
 PRIMARY KEY(plan_id,path), UNIQUE(plan_id,package_position,position),
 FOREIGN KEY(plan_id,package_position) REFERENCES migration_packages(plan_id,position) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE INDEX candidate_by_hash ON migration_candidates(sha256);
CREATE TABLE workspace_config(
 key TEXT PRIMARY KEY NOT NULL, value_json TEXT NOT NULL CHECK(json_valid(value_json)),
 category TEXT NOT NULL CHECK(category IN ('policy','migration','ui','pool','other')), updated_at TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX config_by_category ON workspace_config(category,updated_at);
CREATE TABLE json_imports(id TEXT PRIMARY KEY NOT NULL, committed_at TEXT NOT NULL, revision INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE metadata_revisions(name TEXT PRIMARY KEY NOT NULL,revision INTEGER NOT NULL) WITHOUT ROWID;
INSERT INTO metadata_revisions VALUES('catalog',0);
CREATE TRIGGER catalog_insert AFTER INSERT ON catalog_entries BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='catalog'; END;
CREATE TRIGGER catalog_update AFTER UPDATE ON catalog_entries BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='catalog'; END;
CREATE TRIGGER catalog_delete AFTER DELETE ON catalog_entries BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='catalog'; END;
CREATE TRIGGER evidence_insert AFTER INSERT ON catalog_evidence BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='catalog'; END;
CREATE TRIGGER evidence_update AFTER UPDATE ON catalog_evidence BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='catalog'; END;
CREATE TRIGGER evidence_delete AFTER DELETE ON catalog_evidence BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='catalog'; END;
INSERT INTO metadata_revisions VALUES('workspace',0);
CREATE TRIGGER revision_catalog_entries_insert AFTER INSERT ON catalog_entries BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_catalog_entries_update AFTER UPDATE ON catalog_entries BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_catalog_entries_delete AFTER DELETE ON catalog_entries BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_catalog_evidence_insert AFTER INSERT ON catalog_evidence BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_catalog_evidence_update AFTER UPDATE ON catalog_evidence BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_catalog_evidence_delete AFTER DELETE ON catalog_evidence BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_run_records_insert AFTER INSERT ON run_records BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_run_records_update AFTER UPDATE ON run_records BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_run_records_delete AFTER DELETE ON run_records BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_package_maps_insert AFTER INSERT ON package_maps BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_package_maps_update AFTER UPDATE ON package_maps BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_package_maps_delete AFTER DELETE ON package_maps BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_package_resources_insert AFTER INSERT ON package_resources BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_package_resources_update AFTER UPDATE ON package_resources BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_package_resources_delete AFTER DELETE ON package_resources BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_package_members_insert AFTER INSERT ON package_members BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_package_members_update AFTER UPDATE ON package_members BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_package_members_delete AFTER DELETE ON package_members BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_migration_plans_insert AFTER INSERT ON migration_plans BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_migration_plans_update AFTER UPDATE ON migration_plans BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_migration_plans_delete AFTER DELETE ON migration_plans BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_migration_packages_insert AFTER INSERT ON migration_packages BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_migration_packages_update AFTER UPDATE ON migration_packages BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_migration_packages_delete AFTER DELETE ON migration_packages BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_migration_candidates_insert AFTER INSERT ON migration_candidates BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_migration_candidates_update AFTER UPDATE ON migration_candidates BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_migration_candidates_delete AFTER DELETE ON migration_candidates BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_workspace_config_insert AFTER INSERT ON workspace_config BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_workspace_config_update AFTER UPDATE ON workspace_config BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
CREATE TRIGGER revision_workspace_config_delete AFTER DELETE ON workspace_config BEGIN UPDATE metadata_revisions SET revision=revision+1 WHERE name='workspace'; END;
PRAGMA user_version=2;
