//! Workspace configuration and stable JSON projections of database records.
//!
//! Logical `run.json` names remain usable for frozen provenance hashes and exports;
//! these functions never fall back to legacy files for database-owned metadata.
#![allow(missing_docs)]
use crate::database::{err, json};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    io,
    path::{Path, PathBuf},
};
pub struct ConfigStore {
    conn: Connection,
}
impl ConfigStore {
    pub fn open(p: &Path) -> io::Result<Self> {
        Ok(Self {
            conn: crate::database::open(p)?,
        })
    }
    pub fn entries(&self) -> io::Result<Vec<(String, Value)>> {
        let mut q = self
            .conn
            .prepare("SELECT key,value_json FROM workspace_config ORDER BY key")
            .map_err(err)?;
        q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(err)?
            .map(|r| {
                let (k, v) = r.map_err(err)?;
                Ok((k, json(v)?))
            })
            .collect()
    }
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> io::Result<Option<T>> {
        self.conn
            .query_row(
                "SELECT value_json FROM workspace_config WHERE key=?1",
                [key],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(err)?
            .map(json)
            .transpose()
    }
    pub fn set(&self, key: &str, category: &str, value: &impl Serialize) -> io::Result<()> {
        insert_config(
            &self.conn,
            key,
            category,
            &serde_json::to_value(value).map_err(err)?,
        )
    }
    pub fn list<T: DeserializeOwned>(&self, category: &str) -> io::Result<Vec<T>> {
        let mut q = self
            .conn
            .prepare("SELECT value_json FROM workspace_config WHERE category=?1 ORDER BY key")
            .map_err(err)?;
        q.query_map([category], |r| r.get::<_, String>(0))
            .map_err(err)?
            .map(|r| json(r.map_err(err)?))
            .collect()
    }
}
pub fn insert_config(db: &Connection, key: &str, category: &str, value: &Value) -> io::Result<()> {
    db.execute("INSERT INTO workspace_config VALUES(?1,?2,?3,?4) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json,category=excluded.category,updated_at=excluded.updated_at",params![key,value.to_string(),category,chrono::Utc::now().to_rfc3339()]).map_err(err)?;
    Ok(())
}
pub fn config_category(relative: &Path) -> Option<&'static str> {
    let s = relative.to_str()?;
    if s.starts_with(".jfquant/pools/") && relative.extension()?.to_str()? == "json" {
        return Some("pool");
    }
    match s {
        ".jfquant/source-policy.json" | ".jfquant/tdx-first-policy.json" => Some("policy"),
        ".jfquant/shared-ui-jobs.json" => Some("ui"),
        ".jfquant/allocation-migration.json" | ".jfquant/data-upgrade-20261018.json" => {
            Some("migration")
        }
        _ => None,
    }
}
pub fn location(path: &Path) -> io::Result<Option<(PathBuf, PathBuf)>> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let Some(root) = absolute
        .ancestors()
        .skip(1)
        .find(|p| p.join(".jfquant/workspace.json").is_file())
    else {
        return Ok(None);
    };
    let relative = absolute.strip_prefix(root).map_err(err)?.to_owned();
    if !crate::evidence_store::safe(&relative) {
        return Err(err("unsafe metadata path"));
    }
    // Resolve only the explicit maintenance run aliases, before checking owned parents.
    let mut relative = relative;
    if relative.starts_with(".jfquant/runs") {
        let alias_relative = relative.clone();
        let parts: Vec<_> = alias_relative.components().collect();
        if parts.len() > 2 {
            let run = root.join(".jfquant/runs").join(parts[2]);
            if std::fs::symlink_metadata(&run).is_ok_and(|m| m.file_type().is_symlink()) {
                let real = run.canonicalize()?;
                if !real.starts_with(root.join(".jfquant/archive")) {
                    return Err(err("unknown run alias"));
                }
                relative = real.strip_prefix(root).map_err(err)?.to_owned();
                for part in parts.iter().skip(3) {
                    relative.push(part);
                }
            }
        }
    }
    crate::evidence_store::owned(root, &relative)?;
    Ok(Some((root.to_owned(), relative)))
}
fn is_run(relative: &Path) -> bool {
    !relative
        .to_string_lossy()
        .starts_with(".jfquant/archive/json-migration-")
        && relative.file_name().is_some_and(|s| s == "run.json")
        && (relative.starts_with(".jfquant/runs") || relative.starts_with(".jfquant/archive"))
}
pub fn projection(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let Some((root, relative)) = location(path)? else {
        return Ok(None);
    };
    let db = root.join(crate::database::INDEX);
    if is_run(&relative) {
        return crate::run_store::RunStore::open(&db)?
            .projection(relative.parent().unwrap())?
            .map(Some)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "database run missing"));
    }
    let value = if config_category(&relative).is_some() {
        ConfigStore::open(&db)?.get::<Value>(crate::database::path(&relative)?)?
    } else {
        return Ok(None);
    };
    let value = value.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("database record missing: {}", relative.display()),
        )
    })?;
    Ok(Some(serde_json::to_vec_pretty(&value).map_err(err)?))
}
pub fn write_projection(path: &Path, value: &impl Serialize) -> io::Result<bool> {
    let Some((root, relative)) = location(path)? else {
        return Ok(false);
    };
    let db = root.join(crate::database::INDEX);
    if is_run(&relative) {
        let run = serde_json::from_value(serde_json::to_value(value).map_err(err)?).map_err(err)?;
        crate::run_store::RunStore::open(&db)?.save(relative.parent().unwrap(), &run)?;
        return Ok(true);
    }
    if let Some(category) = config_category(&relative) {
        ConfigStore::open(&db)?.set(crate::database::path(&relative)?, category, value)?;
        return Ok(true);
    }
    Ok(false)
}
