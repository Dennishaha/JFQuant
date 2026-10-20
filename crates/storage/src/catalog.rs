//! Indexed immutable data catalog and reverse evidence lookup.
#![allow(missing_docs)]
use crate::database::{err, json, path};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};
/// A verified local input version. Corporate action readiness is separate from download success.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// Stable ID.
    pub id: String,
    /// market, actions or coefficients.
    pub kind: String,
    /// Workspace-relative dataset root or action JSON path.
    pub path: PathBuf,
    /// Acquisition/provenance fingerprint.
    pub signature: String,
    /// First covered date when known.
    pub start: Option<String>,
    /// Last covered date when known.
    pub end: Option<String>,
    /// Complete source identity/provenance or coverage details.
    pub details: Value,
}

pub struct CatalogStore {
    pub(crate) conn: Connection,
}
impl CatalogStore {
    pub fn open(p: &Path) -> io::Result<Self> {
        Ok(Self {
            conn: crate::database::open(p)?,
        })
    }
    /// Catalog headers for resource discovery, without parsing nested provenance.
    pub fn headers(&self, kind: Option<&str>) -> io::Result<Vec<CatalogEntry>> {
        let mut q=self.conn.prepare("SELECT id,kind,path,signature,start_date,end_date,'null' FROM catalog_entries WHERE (?1 IS NULL OR kind=?1) ORDER BY id").map_err(err)?;
        q.query_map([kind], row)
            .map_err(err)?
            .map(|r| r.map_err(err))
            .collect()
    }
    pub fn revision(&self) -> io::Result<i64> {
        self.conn
            .query_row(
                "SELECT revision FROM metadata_revisions WHERE name='catalog'",
                [],
                |r| r.get(0),
            )
            .map_err(err)
    }
    pub fn get(&self, id: &str) -> io::Result<Option<CatalogEntry>> {
        let mut q=self.conn.prepare("SELECT id,kind,path,signature,start_date,end_date,details_json FROM catalog_entries WHERE id=?1").map_err(err)?;
        q.query_row([id], row).optional().map_err(err)
    }
    pub fn list_by_kind(&self, kind: &str) -> io::Result<Vec<CatalogEntry>> {
        self.list(Some(kind), None, None)
    }
    pub fn list(
        &self,
        kind: Option<&str>,
        start: Option<&str>,
        end: Option<&str>,
    ) -> io::Result<Vec<CatalogEntry>> {
        let mut sql = String::from(
            "SELECT id,kind,path,signature,start_date,end_date,details_json FROM catalog_entries",
        );
        let mut filters = Vec::new();
        let mut values = Vec::new();
        if let Some(kind) = kind {
            values.push(kind);
            filters.push(format!("kind=?{}", values.len()));
        }
        if let Some(start) = start {
            values.push(start);
            filters.push(format!("(end_date IS NULL OR end_date>=?{})", values.len()));
        }
        if let Some(end) = end {
            values.push(end);
            filters.push(format!(
                "(start_date IS NULL OR start_date<=?{})",
                values.len()
            ));
        }
        if !filters.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&filters.join(" AND "));
        }
        sql.push_str(" ORDER BY id");
        let mut q = self.conn.prepare(&sql).map_err(err)?;
        q.query_map(rusqlite::params_from_iter(values), row)
            .map_err(err)?
            .map(|r| r.map_err(err))
            .collect()
    }
    pub fn list_by_path(&self, p: &Path) -> io::Result<Vec<CatalogEntry>> {
        let mut q=self.conn.prepare("SELECT id,kind,path,signature,start_date,end_date,details_json FROM catalog_entries WHERE path=?1 ORDER BY id").map_err(err)?;
        q.query_map([path(p)?], row)
            .map_err(err)?
            .map(|r| r.map_err(err))
            .collect()
    }

    pub fn insert(&mut self, e: &CatalogEntry) -> io::Result<()> {
        let tx = self.conn.transaction().map_err(err)?;
        insert(&tx, e)?;
        tx.commit().map_err(err)
    }
    pub fn remove(&self, id: &str) -> io::Result<()> {
        self.conn
            .execute("DELETE FROM catalog_entries WHERE id=?1", [id])
            .map_err(err)?;
        Ok(())
    }
    pub fn evidence(&self, id: &str) -> io::Result<BTreeMap<String, String>> {
        let mut q=self.conn.prepare("SELECT filename,sha256 FROM catalog_evidence WHERE catalog_id=?1 ORDER BY filename").map_err(err)?;
        q.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(err)?
            .map(|r| r.map_err(err))
            .collect()
    }
    pub fn find_by_evidence_hash(&self, hash: &str) -> io::Result<Vec<String>> {
        let mut q=self.conn.prepare("SELECT DISTINCT catalog_id FROM catalog_evidence WHERE sha256=?1 ORDER BY catalog_id").map_err(err)?;
        q.query_map([hash], |r| r.get(0))
            .map_err(err)?
            .map(|r| r.map_err(err))
            .collect()
    }
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CatalogEntry> {
    let details: String = r.get(6)?;
    Ok(CatalogEntry {
        id: r.get(0)?,
        kind: r.get(1)?,
        path: PathBuf::from(r.get::<_, String>(2)?),
        signature: r.get(3)?,
        start: r.get(4)?,
        end: r.get(5)?,
        details: json(details).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?,
    })
}
pub fn insert(db: &Connection, e: &CatalogEntry) -> io::Result<()> {
    if e.id.is_empty() || !crate::evidence_store::safe(&e.path) {
        return Err(err("invalid catalog identity/path"));
    }
    for date in [e.start.as_deref(), e.end.as_deref()].into_iter().flatten() {
        chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").map_err(err)?;
    }
    if let (Some(start), Some(end)) = (&e.start, &e.end)
        && start > end
    {
        return Err(err("catalog date range reversed"));
    }
    let mut evidence = BTreeMap::new();
    for field in ["evidence_sha256", "raw_sha256"] {
        if let Some(entries) = e.details.get(field) {
            for (name, value) in entries
                .as_object()
                .ok_or_else(|| err("invalid catalog evidence map"))?
            {
                let hash = value.as_str().ok_or_else(|| err("invalid evidence hash"))?;
                if !crate::evidence_store::safe(Path::new(name))
                    || evidence
                        .insert(name.clone(), hash.to_owned())
                        .is_some_and(|old| old != hash)
                {
                    return Err(err("invalid/conflicting catalog evidence"));
                }
            }
        }
    }
    db.execute("INSERT INTO catalog_entries(id,kind,path,signature,start_date,end_date,details_json,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO UPDATE SET kind=excluded.kind,path=excluded.path,signature=excluded.signature,start_date=excluded.start_date,end_date=excluded.end_date,details_json=excluded.details_json",params![e.id,e.kind,path(&e.path)?,e.signature,e.start,e.end,serde_json::to_string(&e.details).map_err(err)?,chrono::Utc::now().to_rfc3339()]).map_err(err)?;
    db.execute("DELETE FROM catalog_evidence WHERE catalog_id=?1", [&e.id])
        .map_err(err)?;
    for (name, hash) in evidence {
        db.execute(
            "INSERT INTO catalog_evidence VALUES(?1,?2,?3)",
            params![e.id, name, hash],
        )
        .map_err(err)?;
    }
    Ok(())
}
