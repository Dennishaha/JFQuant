//! Durable run records, including archived locations and all workflow states.
#![allow(missing_docs)]
use crate::database::{err, json, path};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    rc::Rc,
};
use uuid::Uuid;
/// Durable terminal and transient run states.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Work is executing.
    Running,
    /// Finished normally.
    Succeeded,
    /// Failed without corrupting other runs.
    Failed,
    /// Input coverage blocks research.
    Blocked,
    /// User cancelled.
    Cancelled,
    /// Process stopped before recording completion.
    Interrupted,
    /// Historical artifacts registered read-only.
    Imported,
}
/// One local task record; input snapshots are never overwritten by data updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    /// Record schema, currently 1.
    pub schema_version: u32,
    /// Task/run UUID.
    pub id: Uuid,
    /// Display operation.
    pub operation: String,
    /// UTC creation time.
    pub started_at: String,
    /// UTC terminal time.
    pub finished_at: Option<String>,
    /// Durable task state.
    pub status: RunStatus,
    /// Frozen request and data versions.
    pub request: Value,
    /// Request signature.
    pub signature: String,
    /// Workspace-relative output location.
    pub output: PathBuf,
    /// Last task stage.
    pub stage: String,
    /// Failure or incomplete-data explanation.
    pub error: Option<String>,
    /// Read-only imported run marker.
    pub read_only: bool,
}

pub struct RunStore {
    pub(crate) conn: Connection,
}
impl RunStore {
    pub fn open(p: &Path) -> io::Result<Self> {
        Ok(Self {
            conn: crate::database::open(p)?,
        })
    }
    pub fn get(&self, id: &Uuid) -> io::Result<Option<RunRecord>> {
        self.one("id", &id.to_string())
    }
    pub fn at_path(&self, p: &Path) -> io::Result<Option<RunRecord>> {
        self.one("path", path(p)?)
    }
    fn one(&self, column: &str, key: &str) -> io::Result<Option<RunRecord>> {
        let sql = format!("SELECT {} FROM run_records WHERE {column}=?1", COLUMNS);
        self.conn
            .query_row(&sql, [key], row)
            .optional()
            .map_err(err)
    }
    pub fn load_all(&self) -> io::Result<BTreeMap<PathBuf, Rc<RunRecord>>> {
        let mut q = self
            .conn
            .prepare(&format!(
                "SELECT path,{} FROM run_records ORDER BY path",
                COLUMNS
            ))
            .map_err(err)?;
        q.query_map([], |r| {
            Ok((
                PathBuf::from(r.get::<_, String>(0)?),
                Rc::new(row_offset(r, 1)?),
            ))
        })
        .map_err(err)?
        .map(|r| r.map_err(err))
        .collect()
    }
    pub fn list_active(&self) -> io::Result<Vec<RunRecord>> {
        let mut q = self
            .conn
            .prepare(&format!(
                "SELECT {} FROM run_records WHERE status='running' ORDER BY created_at,id",
                COLUMNS
            ))
            .map_err(err)?;
        q.query_map([], row)
            .map_err(err)?
            .map(|r| r.map_err(err))
            .collect()
    }
    pub fn save(&self, location: &Path, run: &RunRecord) -> io::Result<()> {
        insert(&self.conn, location, run)
    }
    pub fn projection(&self, location: &Path) -> io::Result<Option<Vec<u8>>> {
        let original: Option<Option<Vec<u8>>> = self
            .conn
            .query_row(
                "SELECT original_json FROM run_records WHERE path=?1",
                [path(location)?],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        match original {
            Some(Some(bytes)) => Ok(Some(bytes)),
            Some(None) => self
                .at_path(location)?
                .map(|r| serde_json::to_vec_pretty(&r).map_err(err))
                .transpose(),
            None => Ok(None),
        }
    }
    /// Stages are advisory; a busy scan writer must not block its own heartbeat.
    pub fn progress(&self, id: &Uuid, stage: &str) -> io::Result<()> {
        self.conn
            .busy_timeout(std::time::Duration::ZERO)
            .map_err(err)?;
        match self.conn.execute(
            "UPDATE run_records SET stage=?2,original_json=NULL WHERE id=?1 AND status='running'",
            params![id.to_string(), stage],
        ) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                Ok(())
            }
            Err(e) => Err(err(e)),
        }
    }
    pub fn update_status(&self, id: &Uuid, status: RunStatus) -> io::Result<()> {
        if status == RunStatus::Running {
            return Err(err("terminal status required"));
        }
        let changed=self.conn.execute("UPDATE run_records SET status=?2,completed_at=?3,original_json=NULL WHERE id=?1 AND status='running'",params![id.to_string(),serde_json::to_value(status).map_err(err)?.as_str(),chrono::Utc::now().to_rfc3339()]).map_err(err)?;
        if changed != 1 {
            return Err(err("run missing or already terminal"));
        }
        Ok(())
    }
    pub fn archive(&self, source: &Path, destination: &Path) -> io::Result<()> {
        self.conn
            .execute(
                "UPDATE run_records SET path=?2 WHERE path=?1",
                params![path(source)?, path(destination)?],
            )
            .map_err(err)?;
        Ok(())
    }
}
const COLUMNS: &str = "id,schema_version,operation,created_at,completed_at,status,request_json,signature,output,stage,error,read_only";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RunRecord> {
    row_offset(r, 0)
}
fn row_offset(r: &rusqlite::Row<'_>, o: usize) -> rusqlite::Result<RunRecord> {
    let parse = |i: usize| -> rusqlite::Result<Value> {
        let s: String = r.get(o + i)?;
        json(s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                o + i,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })
    };
    let id: String = r.get(o)?;
    let status: String = r.get(o + 5)?;
    Ok(RunRecord {
        id: Uuid::parse_str(&id).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(o, rusqlite::types::Type::Text, Box::new(e))
        })?,
        schema_version: r.get(o + 1)?,
        operation: r.get(o + 2)?,
        started_at: r.get(o + 3)?,
        finished_at: r.get(o + 4)?,
        status: serde_json::from_value(Value::String(status)).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                o + 5,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?,
        request: parse(6)?,
        signature: r.get(o + 7)?,
        output: PathBuf::from(r.get::<_, String>(o + 8)?),
        stage: r.get(o + 9)?,
        error: r.get(o + 10)?,
        read_only: r.get(o + 11)?,
    })
}
pub fn insert(db: &Connection, location: &Path, r: &RunRecord) -> io::Result<()> {
    if r.schema_version != 1
        || !crate::evidence_store::safe(location)
        || !crate::evidence_store::safe(&r.output)
        || location.file_name().and_then(|s| s.to_str()) != Some(r.id.to_string().as_str())
    {
        return Err(err("invalid run identity/path/schema"));
    }
    db.execute("INSERT INTO run_records(id,path,schema_version,status,read_only,request_json,created_at,completed_at,operation,signature,output,stage,error) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13) ON CONFLICT(id) DO UPDATE SET path=excluded.path,schema_version=excluded.schema_version,status=excluded.status,read_only=excluded.read_only,request_json=excluded.request_json,created_at=excluded.created_at,completed_at=excluded.completed_at,operation=excluded.operation,signature=excluded.signature,output=excluded.output,stage=excluded.stage,error=excluded.error,original_json=NULL",params![r.id.to_string(),path(location)?,r.schema_version,serde_json::to_value(r.status).map_err(err)?.as_str(),r.read_only,serde_json::to_string(&r.request).map_err(err)?,r.started_at,r.finished_at,r.operation,r.signature,path(&r.output)?,r.stage,r.error]).map_err(err)?;
    Ok(())
}
