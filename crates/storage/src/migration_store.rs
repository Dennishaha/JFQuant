//! Normalized migration plans. Nested contract fields retain their exact JSON values.
#![allow(missing_docs)]
use crate::database::{err, json};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json as value};
use std::{io, path::Path};
pub struct MigrationStore {
    conn: Connection,
}
impl MigrationStore {
    pub fn open(p: &Path) -> io::Result<Self> {
        Ok(Self {
            conn: crate::database::open(p)?,
        })
    }
    pub fn save(&mut self, v: &Value) -> io::Result<()> {
        let tx = self.conn.transaction().map_err(err)?;
        insert(&tx, v)?;
        tx.commit().map_err(err)
    }
    pub fn get(&self, id: &str) -> io::Result<Option<Value>> {
        let tx = self.conn.unchecked_transaction().map_err(err)?;
        let result = self.get_inner(id)?;
        tx.commit().map_err(err)?;
        Ok(result)
    }
    fn get_inner(&self, id: &str) -> io::Result<Option<Value>> {
        let header: Option<String> = self
            .conn
            .query_row(
                "SELECT details_json FROM migration_plans WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        let Some(header) = header else {
            return Ok(None);
        };
        let mut out: Value = json(header)?;
        let mut q=self.conn.prepare("SELECT position,details_json FROM migration_packages WHERE plan_id=?1 ORDER BY position").map_err(err)?;
        let heads = q
            .query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .map_err(err)?
            .map(|r| r.map_err(err))
            .collect::<io::Result<Vec<_>>>()?;
        let mut packages = vec![];
        for (i, head) in heads {
            let mut p: Value = json(head)?;
            let mut members = vec![];
            let mut objects = serde_json::Map::new();
            let mut q=self.conn.prepare("SELECT path,sha256,original_bytes,estimated_stored_bytes,stamp_json,relative_path FROM migration_candidates WHERE plan_id=?1 AND package_position=?2 ORDER BY position").map_err(err)?;
            let rows = q
                .query_map(params![id, i], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)? as u64,
                        r.get::<_, i64>(3)? as u64,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                })
                .map_err(err)?;
            for row in rows {
                let (path, hash, bytes, stored, stamp, relative) = row.map_err(err)?;
                let object = value!({"sha256":hash,"bytes":bytes,"stored_bytes":stored});
                objects.insert(relative, object.clone());
                members.push(value!({"path":path,"stamp":json::<Value>(stamp)?,"object":object}));
            }
            p["map"]["members"] = Value::Object(objects);
            p["members"] = Value::Array(members);
            packages.push(p);
        }
        out["packages"] = Value::Array(packages);
        Ok(Some(out))
    }
    pub fn status(&self, id: &str, status: &str) -> io::Result<()> {
        self.conn.execute("UPDATE migration_plans SET status=?2,completed_at=CASE WHEN ?2 IN ('Completed','Failed') THEN ?3 ELSE NULL END WHERE id=?1",params![id,status,chrono::Utc::now().to_rfc3339()]).map_err(err)?;
        Ok(())
    }
}
pub fn insert(db: &Connection, v: &Value) -> io::Result<()> {
    let id = v["id"].as_str().ok_or_else(|| err("plan id missing"))?;
    uuid::Uuid::parse_str(id).map_err(err)?;
    if v["schema_version"] != 1 {
        return Err(err("unsupported migration plan version"));
    }
    let packages = v["packages"]
        .as_array()
        .ok_or_else(|| err("plan packages missing"))?;
    let mut header = v.clone();
    header
        .as_object_mut()
        .ok_or_else(|| err("invalid plan"))?
        .remove("packages");
    db.execute("INSERT INTO migration_plans(id,schema_version,created_at,status,estimated_bytes,details_json) VALUES(?1,?2,?3,'Planned',?4,?5) ON CONFLICT(id) DO UPDATE SET details_json=excluded.details_json,schema_version=excluded.schema_version,created_at=excluded.created_at,estimated_bytes=excluded.estimated_bytes",params![id,v["schema_version"].as_i64().ok_or_else(||err("plan version missing"))?,v["created_at"].as_str().ok_or_else(||err("plan timestamp missing"))?,v["object_storage_bytes"].as_u64().and_then(|v|i64::try_from(v).ok()).ok_or_else(||err("plan bytes missing/overflow"))?,header.to_string()]).map_err(err)?;
    db.execute("DELETE FROM migration_packages WHERE plan_id=?1", [id])
        .map_err(err)?;
    for (i, p) in packages.iter().enumerate() {
        let mut head = p.clone();
        head.as_object_mut()
            .ok_or_else(|| err("invalid package"))?
            .remove("members");
        head["map"]
            .as_object_mut()
            .ok_or_else(|| err("invalid map"))?
            .remove("members");
        db.execute(
            "INSERT INTO migration_packages VALUES(?1,?2,?3)",
            params![id, i as i64, head.to_string()],
        )
        .map_err(err)?;
        let map: crate::evidence_store::PackageMap =
            serde_json::from_value(p["map"].clone()).map_err(err)?;
        if map.schema_version != 1
            || map.id != crate::evidence_store::package_id(&map.root)?
            || !crate::evidence_store::safe(&map.manifest)
            || !crate::evidence_store::safe(&map.identity_root)
        {
            return Err(err("invalid migration package identity"));
        }
        let members = p["members"]
            .as_array()
            .ok_or_else(|| err("invalid members"))?;
        if members.len() != map.members.len() {
            return Err(err("plan/map membership mismatch"));
        }
        for (j, m) in members.iter().enumerate() {
            let member = m["path"]
                .as_str()
                .ok_or_else(|| err("invalid candidate path"))?;
            let relative = Path::new(member).strip_prefix(&map.root).map_err(err)?;
            if !crate::evidence_store::safe(relative)
                || !crate::evidence_store::safe(Path::new(member))
            {
                return Err(err("unsafe migration candidate"));
            }
            let _: crate::filesystem::FileStamp =
                serde_json::from_value(m["stamp"].clone()).map_err(err)?;
            let object: crate::evidence_store::ObjectRef =
                serde_json::from_value(m["object"].clone()).map_err(err)?;
            if map.members.get(relative) != Some(&object) {
                return Err(err("candidate/map mismatch"));
            }
            db.execute(
                "INSERT INTO migration_candidates VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    id,
                    i as i64,
                    j as i64,
                    member,
                    object.sha256,
                    i64::try_from(object.bytes).map_err(err)?,
                    i64::try_from(object.stored_bytes).map_err(err)?,
                    m["stamp"].to_string(),
                    crate::database::path(relative)?
                ],
            )
            .map_err(err)?;
        }
    }
    Ok(())
}
