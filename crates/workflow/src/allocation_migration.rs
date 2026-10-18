//! Explicit offline allocation migration. Legacy decoding never enters normal readers.
use crate::{Result, Workspace, atomic_json};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
struct Change {
    path: PathBuf,
    old_sha256: String,
    old: Value,
    new: Value,
}
#[derive(Clone, Serialize, Deserialize)]
struct Move {
    source: PathBuf,
    destination: PathBuf,
    files: BTreeMap<PathBuf, String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Journal {
    schema_version: u32,
    id: String,
    complete: bool,
    rolled_back: bool,
    rolling_back: bool,
    retained_runs: Vec<PathBuf>,
    owned_configs: BTreeMap<PathBuf, String>,
    changes: Vec<Change>,
    moves: Vec<Move>,
    blockers: Vec<String>,
}
fn hash(p: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(p)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(digest.finalize())
    ))
}

fn files(p: &Path, out: &mut BTreeMap<PathBuf, String>) -> Result<()> {
    if fs::symlink_metadata(p)?.file_type().is_symlink() {
        return Err(format!("migration_symlink_requires_manual_review:{}", p.display()).into());
    }
    if p.is_dir() {
        for e in fs::read_dir(p)? {
            files(&e?.path(), out)?;
        }
    } else {
        out.insert(p.to_owned(), hash(p)?);
    }
    Ok(())
}
fn allocation() -> Result<Value> {
    let c = crate::allocation::AllocationConfig {
        max_stock_weight: 1.,
        ..Default::default()
    };
    Ok(serde_json::to_value(c)?)
}
fn convert(v: &Value) -> Result<Option<Value>> {
    let mut n = v.clone();
    if v["schema_version"] == 10 && v.get("factors").is_some() && v.get("schedule").is_some() {
        n["schema_version"] = 11.into();
        n["allocation"] = allocation()?;
    } else if v["schema_version"] == 10
        && v.get("config").is_some()
        && v.get("formula_version").is_some()
    {
        n["schema_version"] = 11.into();
        n["config"]["allocation"] = allocation()?;
    } else if v["schema_version"] == 2
        && v.get("execution").is_some()
        && v.get("training").is_some()
    {
        n["schema_version"] = 3.into();
        n["execution"]["allocation"] = allocation()?;
    } else if v["schema_version"] == 11 && v.get("factors").is_some() && v.get("schedule").is_some()
    {
        let d: crate::profiles::WorkflowDocument = serde_json::from_value(v.clone())?;
        d.allocation.validate_count(d.top_n)?;
        return Ok(None);
    } else if v["schema_version"] == 11
        && v.get("config").is_some()
        && v.get("formula_version").is_some()
    {
        let t: crate::ResearchTemplate = serde_json::from_value(v.clone())?;
        t.validate()?;
        return Ok(None);
    } else if v["schema_version"] == 3
        && v.get("execution").is_some()
        && v.get("training").is_some()
    {
        let p: crate::research::backtest::independent::BacktestPlan =
            serde_json::from_value(v.clone())?;
        p.validate()?;
        return Ok(None);
    } else if v.get("factor_ids").is_some() && v.get("factor_set").is_some() {
        let p: crate::profiles::FactorProfile = serde_json::from_value(v.clone())?;
        p.apply(&mut crate::ResearchTemplate::default())?;
        return Ok(None);
    } else if v.get("pool_version").is_some() && v.get("benchmark").is_some() {
        let p: crate::profiles::TargetProfile = serde_json::from_value(v.clone())?;
        p.apply(&mut crate::ResearchTemplate::default())?;
        return Ok(None);
    } else {
        return Err("migration_unknown_configuration_format".into());
    }
    if n.get("factors").is_some() {
        let _: crate::profiles::WorkflowDocument = serde_json::from_value(n.clone())?;
    } else if n.get("formula_version").is_some() {
        let t: crate::ResearchTemplate = serde_json::from_value(n.clone())?;
        t.validate()?;
    } else {
        let p: crate::research::backtest::independent::BacktestPlan =
            serde_json::from_value(n.clone())?;
        p.validate()?;
    }
    Ok(Some(n))
}
fn validate_profile_references(
    root: &Path,
    path: &Path,
    value: &Value,
    explicit: &BTreeMap<PathBuf, String>,
) -> Result<()> {
    if value.get("schedule").is_none() || !value["factors"].is_string() {
        return Ok(());
    }
    for field in ["factors", "target"] {
        let relative = PathBuf::from(
            value[field]
                .as_str()
                .ok_or("migration_profile_reference_missing")?,
        );
        if relative.is_absolute() {
            return Err("migration_profile_reference_must_be_relative".into());
        }
        let referenced = fs::canonicalize(
            path.parent()
                .ok_or("migration_config_parent_missing")?
                .join(relative),
        )?;
        if !referenced.starts_with(root) && !explicit.contains_key(&referenced) {
            return Err(format!(
                "migration_external_reference_requires_explicit_config:{}",
                referenced.display()
            )
            .into());
        }
        let bytes = fs::read(&referenced)?;
        let mut template = crate::ResearchTemplate::default();
        if field == "factors" {
            serde_json::from_slice::<crate::profiles::FactorProfile>(&bytes)?
                .apply(&mut template)?;
        } else {
            serde_json::from_slice::<crate::profiles::TargetProfile>(&bytes)?
                .apply(&mut template)?;
        }
    }
    Ok(())
}

fn plan(root: &Path, external: &[PathBuf]) -> Result<Journal> {
    let mut j = Journal {
        schema_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        complete: false,
        rolled_back: false,
        rolling_back: false,
        retained_runs: vec![],
        owned_configs: BTreeMap::new(),
        changes: vec![],
        moves: vec![],
        blockers: vec![],
    };
    let mut configs = BTreeMap::new();
    let owned = root.join(".jfquant/templates");
    if owned.exists() {
        files(&owned, &mut configs)?;
    }
    j.owned_configs = configs.clone();
    for p in external {
        let p = fs::canonicalize(p)?;
        if p.extension().is_none_or(|s| s != "json") {
            return Err("migration_external_config_must_be_json".into());
        }
        configs.insert(p.clone(), hash(&p)?);
    }
    j.owned_configs = configs.clone();
    for (p, h) in configs {
        if p.extension().is_none_or(|s| s != "json") {
            continue;
        }
        let result = (|| -> Result<()> {
            let old: Value = serde_json::from_slice(&fs::read(&p)?)?;
            validate_profile_references(root, &p, &old, &j.owned_configs)?;
            if let Some(new) = convert(&old)? {
                j.changes.push(Change {
                    path: p.clone(),
                    old_sha256: h,
                    old,
                    new,
                });
            }
            Ok(())
        })();
        if let Err(e) = result {
            j.blockers.push(format!("{}:{e}", p.display()));
        }
    }
    let runs = root.join(".jfquant/runs");
    if runs.exists() {
        for e in fs::read_dir(&runs)? {
            let p = e?.path();
            if fs::symlink_metadata(&p)?.file_type().is_symlink() {
                continue;
            }
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let cache = matches!(
                name,
                ".factor-cache" | ".factor-shards-v1" | ".predictions-v1" | ".factor-artifacts-v2"
            );
            let research = if p.join("run.json").is_file() {
                let v: Value = serde_json::from_slice(&fs::read(p.join("run.json"))?)?;
                let op = v["request"]["request"]["operation"].as_str().unwrap_or("");
                matches!(
                    op,
                    "research"
                        | "train_research"
                        | "backtest_research"
                        | "backtest_predictions"
                        | "independent"
                        | "preview_research"
                        | "evaluate_factors"
                )
            } else if p.join("data_audit.json").is_file() {
                let audit: Value = serde_json::from_slice(&fs::read(p.join("data_audit.json"))?)?;
                audit.get("research_signature").is_some() && audit.get("formula_version").is_some()
            } else {
                false
            };
            if !cache && !research {
                j.retained_runs.push(p.clone());
            }
            if cache || research {
                let mut f = BTreeMap::new();
                files(&p, &mut f)?;
                j.moves.push(Move {
                    source: p.clone(),
                    destination: root
                        .join(format!(".jfquant/archive/allocation-{}/runs/{name}", j.id)),
                    files: f,
                });
            }
        }
    }
    Ok(j)
}
fn journal_path(root: &Path) -> PathBuf {
    root.join(".jfquant/allocation-migration.json")
}
/// Block service startup on an interrupted offline conversion.
pub(crate) fn require_complete(root: &Path) -> Result<()> {
    let p = journal_path(root);
    let mut explicit = BTreeMap::new();
    if p.exists() {
        let j: Journal = serde_json::from_slice(&fs::read(p)?)?;
        explicit = j.owned_configs.clone();
        if (!j.complete || j.rolling_back) && !j.rolled_back {
            return Err(
                "allocation_migration_incomplete: resume workspace migrate-allocation --apply"
                    .into(),
            );
        }
    }
    // Normal startup never upgrades registered documents. Legacy decoding stays here.
    let owned = root.join(".jfquant/templates");
    if owned.exists() {
        let mut configs = BTreeMap::new();
        files(&owned, &mut configs)?;
        for path in configs
            .keys()
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
        {
            let value: Value = serde_json::from_slice(&fs::read(path)?)?;
            validate_profile_references(root, path, &value, &explicit)?;
            if convert(&value)?.is_some() {
                return Err(format!("allocation_migration_required:{}", path.display()).into());
            }
        }
    }
    Ok(())
}
/// Preview, apply/resume, or roll back the explicitly selected local workspace.
pub fn run(root: &Path, external: &[PathBuf], apply: bool, rollback: bool) -> Result<Value> {
    let root = fs::canonicalize(root)?;
    let marker: Value = serde_json::from_slice(&fs::read(root.join(".jfquant/workspace.json"))?)?;
    if marker["schema_version"] != 1 {
        return Err("migration_workspace_version_unsupported".into());
    }
    if !apply && !rollback {
        return Ok(serde_json::to_value(plan(&root, external)?)?);
    }
    let w = Workspace { root: root.clone() };
    let _lease = w.lease()?;
    let p = journal_path(&root);
    let existing = if p.exists() {
        Some(serde_json::from_slice::<Journal>(&fs::read(&p)?)?)
    } else {
        None
    };
    if rollback {
        let mut j = existing.ok_or("migration_not_found")?;
        if j.rolled_back {
            return Ok(json!({"changed":false,"rolled_back":true}));
        }
        if !j.complete {
            return Err("finish_migration_before_rollback".into());
        }
        // No new task directory or config may be overwritten by rollback.
        for e in fs::read_dir(root.join(".jfquant/runs"))? {
            let e = e?;
            if uuid::Uuid::parse_str(e.file_name().to_str().unwrap_or("")).is_ok()
                && !fs::symlink_metadata(e.path())?.file_type().is_symlink()
                && !j.retained_runs.contains(&e.path())
                && !(j.rolling_back && j.moves.iter().any(|m| m.source == e.path()))
            {
                return Err("rollback_blocked_by_new_task".into());
            }
        }
        for c in &j.changes {
            let v: Value = serde_json::from_slice(&fs::read(&c.path)?)?;
            if v != c.new && !(j.rolling_back && v == c.old) {
                return Err("rollback_blocked_by_changed_configuration".into());
            }
        }
        let mut owned_now = BTreeMap::new();
        let owned = root.join(".jfquant/templates");
        if owned.exists() {
            files(&owned, &mut owned_now)?;
        }
        if owned_now.keys().any(|p| !j.owned_configs.contains_key(p))
            || j.owned_configs
                .keys()
                .filter(|p| p.starts_with(&owned))
                .any(|p| !owned_now.contains_key(p))
        {
            return Err("rollback_blocked_by_new_or_removed_configuration".into());
        }
        for (path, expected) in &j.owned_configs {
            if !j.changes.iter().any(|c| c.path == *path) && hash(path)? != *expected {
                return Err("rollback_blocked_by_changed_configuration".into());
            }
        }
        j.rolling_back = true;
        atomic_json(&p, &j)?;
        for c in &j.changes {
            atomic_json(&c.path, &c.old)?;
        }
        for m in &j.moves {
            if m.source.exists() {
                if m.destination.exists() {
                    return Err("rollback_destination_exists".into());
                }
                for (path, want) in &m.files {
                    if hash(path)? != *want {
                        return Err("rollback_restored_artifact_changed".into());
                    }
                }
            } else {
                for (path, want) in &m.files {
                    if hash(&m.destination.join(path.strip_prefix(&m.source)?))? != *want {
                        return Err("rollback_archive_changed".into());
                    }
                }
                fs::rename(&m.destination, &m.source)?;
            }
        }
        j.rolling_back = false;
        j.rolled_back = true;
        atomic_json(&p, &j)?;
        return Ok(json!({"rolled_back":true}));
    }
    let mut j = match existing {
        Some(j) if !j.rolled_back => j,
        _ => plan(&root, external)?,
    };
    if j.rolling_back {
        return Err("migration_rollback_incomplete: resume --rollback".into());
    }
    if j.complete {
        return Ok(json!({"changed":false,"complete":true,"id":j.id}));
    }
    if !j.blockers.is_empty() {
        return Ok(json!({"applied":false,"blockers":j.blockers}));
    }
    atomic_json(&p, &j)?;
    let archive = root.join(format!(".jfquant/archive/allocation-{}", j.id));
    fs::create_dir_all(&archive)?;
    atomic_json(&archive.join("manifest.json"), &j)?;
    for c in &j.changes {
        let v: Value = serde_json::from_slice(&fs::read(&c.path)?)?;
        if v != c.old && v != c.new {
            return Err("migration_configuration_changed".into());
        }
    }
    for m in &j.moves {
        let location = if m.destination.exists() {
            &m.destination
        } else {
            &m.source
        };
        for (old, want) in &m.files {
            let actual = location.join(old.strip_prefix(&m.source)?);
            if hash(&actual)? != *want {
                return Err("migration_derived_input_changed".into());
            }
        }
        if !m.destination.exists() {
            fs::create_dir_all(m.destination.parent().ok_or("migration_archive_parent")?)?;
            fs::rename(&m.source, &m.destination)?;
        }
    }
    for c in &j.changes {
        atomic_json(&c.path, &c.new)?;
    }
    j.complete = true;
    atomic_json(&p, &j)?;
    Ok(
        json!({"complete":true,"id":j.id,"archive":archive,"migrated_configs":j.changes.len(),"archived_derivatives":j.moves.len()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_conversion_is_recoverable_idempotent_and_does_not_touch_market() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let root = w.root();
        let mut old = serde_json::to_value(crate::ResearchTemplate::default()).unwrap();
        old["schema_version"] = 10.into();
        old["config"].as_object_mut().unwrap().remove("allocation");
        old["config"]["top_n"] = 1.into();
        let config = root.join(".jfquant/templates/old.json");
        atomic_json(&config, &old).unwrap();
        fs::write(root.join("data/raw.bin"), b"unchanged evidence").unwrap();
        assert!(
            run(root, &[], false, false).unwrap()["changes"]
                .as_array()
                .unwrap()
                .len()
                == 1
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&config).unwrap()).unwrap(),
            old
        );
        assert_eq!(run(root, &[], true, false).unwrap()["complete"], true);
        let t: crate::ResearchTemplate =
            serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        t.validate().unwrap();
        assert_eq!(t.config.allocation.max_stock_weight, 1.);
        assert_eq!(
            fs::read(root.join("data/raw.bin")).unwrap(),
            b"unchanged evidence"
        );
        assert_eq!(run(root, &[], true, false).unwrap()["changed"], false);
        assert_eq!(run(root, &[], false, true).unwrap()["rolled_back"], true);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(config).unwrap()).unwrap(),
            old
        );
    }
    #[test]
    fn unknown_config_is_blocked_and_new_tasks_block_rollback() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        atomic_json(
            &w.root().join(".jfquant/templates/unknown.json"),
            &json!({"schema_version":9,"schedule":{}}),
        )
        .unwrap();
        let report = run(w.root(), &[], true, false).unwrap();
        assert!(!report["blockers"].as_array().unwrap().is_empty());
        assert!(require_complete(w.root()).is_err());
    }
    #[test]
    fn interrupted_archive_and_rollback_resume_without_losing_new_work() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let root = w.root();
        let id = uuid::Uuid::new_v4();
        let artifact = root.join(format!(".jfquant/runs/{id}"));
        fs::create_dir_all(&artifact).unwrap();
        atomic_json(
            &artifact.join("run.json"),
            &json!({"request":{"request":{"operation":"research"}}}),
        )
        .unwrap();
        fs::write(artifact.join("model.txt"), b"model bytes").unwrap();
        let mut j = plan(root, &[]).unwrap();
        assert_eq!(j.moves.len(), 1);
        let moved = &j.moves[0];
        fs::create_dir_all(moved.destination.parent().unwrap()).unwrap();
        atomic_json(&journal_path(root), &j).unwrap();
        fs::rename(&moved.source, &moved.destination).unwrap();
        assert!(require_complete(root).is_err());
        assert_eq!(run(root, &[], true, false).unwrap()["complete"], true);
        let new_task = root.join(format!(".jfquant/runs/{}", uuid::Uuid::new_v4()));
        fs::create_dir(&new_task).unwrap();
        assert!(
            run(root, &[], false, true)
                .unwrap_err()
                .to_string()
                .contains("new_task")
        );
        fs::remove_dir(&new_task).unwrap();
        let new_config = root.join(".jfquant/templates/new.json");
        atomic_json(&new_config, &json!({"schema_version":1})).unwrap();
        assert!(
            run(root, &[], false, true)
                .unwrap_err()
                .to_string()
                .contains("configuration")
        );
        fs::remove_file(&new_config).unwrap();
        j.complete = true;
        j.rolling_back = true;
        atomic_json(&journal_path(root), &j).unwrap();
        fs::rename(&j.moves[0].destination, &artifact).unwrap();
        assert_eq!(run(root, &[], false, true).unwrap()["rolled_back"], true);
        assert_eq!(
            fs::read(artifact.join("model.txt")).unwrap(),
            b"model bytes"
        );
        assert_eq!(run(root, &[], false, true).unwrap()["changed"], false);
    }
    #[test]
    fn legacy_cache_and_named_research_outputs_are_archived_but_evidence_is_retained() {
        let root = tempfile::tempdir().unwrap();
        let w = Workspace::open(root.path()).unwrap();
        let cache = w.resolve(".jfquant/runs/.factor-cache");
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("old.bin"), b"factor cache").unwrap();
        let named = w.resolve(".jfquant/runs/old-research");
        fs::create_dir(&named).unwrap();
        atomic_json(
            &named.join("data_audit.json"),
            &json!({"research_signature":"old", "formula_version":"old"}),
        )
        .unwrap();
        let evidence = w.resolve(".jfquant/runs/evidence");
        fs::create_dir(&evidence).unwrap();
        fs::write(evidence.join("source.html"), b"primary event evidence").unwrap();
        let preview = run(w.root(), &[], false, false).unwrap();
        assert_eq!(preview["moves"].as_array().unwrap().len(), 2);
        assert_eq!(
            run(w.root(), &[], true, false).unwrap()["archived_derivatives"],
            2
        );
        assert!(!cache.exists() && !named.exists());
        assert_eq!(
            fs::read(evidence.join("source.html")).unwrap(),
            b"primary event evidence"
        );
        run(w.root(), &[], false, true).unwrap();
        assert_eq!(fs::read(cache.join("old.bin")).unwrap(), b"factor cache");
    }
    #[test]
    fn external_workflow_profiles_require_an_explicit_file_list() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let w = Workspace::open(root.path()).unwrap();
        let path = external.path().join("workflow.json");
        crate::ResearchTemplate::default().write(&path).unwrap();
        let mut old: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let factors = external.path().join(old["factors"].as_str().unwrap());
        let target = external.path().join(old["target"].as_str().unwrap());
        old["schema_version"] = 10.into();
        old.as_object_mut().unwrap().remove("allocation");
        atomic_json(&path, &old).unwrap();
        let preview = run(w.root(), std::slice::from_ref(&path), false, false).unwrap();
        assert!(
            preview["blockers"][0]
                .as_str()
                .unwrap()
                .contains("explicit_config")
        );
        let paths = vec![path.clone(), factors, target];
        let result = run(w.root(), &paths, true, false).unwrap();
        assert_eq!(result["complete"], true);
        crate::ResearchTemplate::read(&path).unwrap();
        assert_eq!(
            run(w.root(), &[], false, true).unwrap()["rolled_back"],
            true
        );
    }
}
