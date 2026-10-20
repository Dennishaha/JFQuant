//! Shared schema and connection settings for authoritative workspace metadata.
#![allow(missing_docs)]
use rusqlite::Connection;
use std::{io, path::Path, time::Duration};
pub const INDEX: &str = ".jfquant/storage/index.sqlite";
pub(crate) fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("workspace_database: {e}"))
}
pub(crate) fn path(p: &Path) -> io::Result<&str> {
    p.to_str().ok_or_else(|| err("non-UTF8 metadata path"))
}
pub(crate) fn json<T: serde::de::DeserializeOwned>(v: String) -> io::Result<T> {
    serde_json::from_str(&v).map_err(err)
}
pub fn open(p: &Path) -> io::Result<Connection> {
    if let Some(parent) = p.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let db = Connection::open(p).map_err(err)?;
    db.busy_timeout(Duration::from_secs(10)).map_err(err)?;
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA cache_size=-32768; PRAGMA synchronous=FULL;")
        .map_err(err)?;
    let version: i64 = db
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(err)?;
    if !(0..=2).contains(&version) {
        return Err(err("unsupported schema version"));
    }
    if version < 2 {
        db.execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE;")
            .map_err(err)?;
        // Recheck after taking the write lock: another opener may have installed the schema.
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(err)?;
        if version < 2 {
            db.execute_batch(include_str!("../migrations/002_catalog_tables.sql"))
                .map_err(err)?;
        }
        db.execute_batch("COMMIT").map_err(err)?;
    }
    Ok(db)
}
