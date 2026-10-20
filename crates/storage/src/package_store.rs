//! Normalized evidence package membership and registry transactions.
#![allow(missing_docs)]
use crate::{
    database::{err, path},
    evidence_store::{ObjectRef, PackageLocator, PackageMap, Registry},
};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};
pub struct PackageStore {
    conn: Connection,
}
impl PackageStore {
    pub fn open(p: &Path) -> io::Result<Self> {
        Ok(Self {
            conn: crate::database::open(p)?,
        })
    }
    pub fn version(&self) -> io::Result<i64> {
        self.conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))
            .map_err(err)
    }
    pub fn registry(&self) -> io::Result<Registry> {
        let mut q = self
            .conn
            .prepare("SELECT id,root,map_sha256,released FROM package_maps ORDER BY id")
            .map_err(err)?;
        Ok(Registry {
            schema_version: 1,
            packages: q
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        PackageLocator {
                            root: PathBuf::from(r.get::<_, String>(1)?),
                            map_sha256: r.get(2)?,
                            released: r.get(3)?,
                        },
                    ))
                })
                .map_err(err)?
                .map(|r| r.map_err(err))
                .collect::<io::Result<_>>()?,
        })
    }
    pub fn get(&self, id: &str) -> io::Result<Option<PackageMap>> {
        let tx = self.conn.unchecked_transaction().map_err(err)?;
        let result = self.get_inner(id)?;
        tx.commit().map_err(err)?;
        Ok(result)
    }
    fn get_inner(&self, id: &str) -> io::Result<Option<PackageMap>> {
        let head=self.conn.query_row("SELECT root,identity_root,schema_version,manifest,manifest_sha256,integrity_sha256 FROM package_maps WHERE id=?1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,u32>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?))).optional().map_err(err)?;
        let Some((
            root,
            identity_root,
            schema_version,
            manifest,
            manifest_sha256,
            integrity_sha256,
        )) = head
        else {
            return Ok(None);
        };
        let mut q = self
            .conn
            .prepare(
                "SELECT resource_id FROM package_resources WHERE package_id=?1 ORDER BY position",
            )
            .map_err(err)?;
        let resources = q
            .query_map([id], |r| r.get(0))
            .map_err(err)?
            .map(|r| r.map_err(err))
            .collect::<io::Result<_>>()?;
        let mut q=self.conn.prepare("SELECT relative_path,sha256,original_bytes,stored_bytes FROM package_members WHERE package_id=?1 ORDER BY relative_path").map_err(err)?;
        let members = q
            .query_map([id], |r| {
                Ok((
                    PathBuf::from(r.get::<_, String>(0)?),
                    ObjectRef {
                        sha256: r.get(1)?,
                        bytes: r.get::<_, i64>(2)? as u64,
                        stored_bytes: r.get::<_, i64>(3)? as u64,
                    },
                ))
            })
            .map_err(err)?
            .map(|r| r.map_err(err))
            .collect::<io::Result<BTreeMap<_, _>>>()?;
        let map = PackageMap {
            schema_version,
            id: id.into(),
            root: root.into(),
            identity_root: identity_root.into(),
            manifest: manifest.into(),
            manifest_sha256,
            resources,
            members,
        };
        if map_hash(&map)? != integrity_sha256 {
            return Err(err("package map changed"));
        }
        Ok(Some(map))
    }
    pub fn save(&mut self, map: &PackageMap, released: bool) -> io::Result<()> {
        let tx = self.conn.transaction().map_err(err)?;
        insert(&tx, map, released)?;
        tx.commit().map_err(err)
    }
    pub fn publish(&mut self, maps: &[PackageMap], registry: &Registry) -> io::Result<()> {
        let tx = self.conn.transaction().map_err(err)?;
        for map in maps {
            let locator = registry
                .packages
                .get(&map.id)
                .ok_or_else(|| err("map missing registry entry"))?;
            insert(&tx, map, locator.released)?;
        }
        // A registry publication replaces the set atomically, including archive/removal.
        let ids: Vec<String> = {
            let mut q = tx.prepare("SELECT id FROM package_maps").map_err(err)?;
            q.query_map([], |r| r.get(0))
                .map_err(err)?
                .map(|r| r.map_err(err))
                .collect::<io::Result<_>>()?
        };
        for id in ids {
            if let Some(l) = registry.packages.get(&id) {
                let changed=tx.execute("UPDATE package_maps SET released=?2 WHERE id=?1 AND root=?3 AND map_sha256=?4",params![id,l.released,path(&l.root)?,l.map_sha256]).map_err(err)?;
                if changed != 1 {
                    return Err(err("registry/map mismatch"));
                }
            } else {
                tx.execute("DELETE FROM package_maps WHERE id=?1", [id])
                    .map_err(err)?;
            }
        }
        tx.commit().map_err(err)
    }
    pub fn member(&self, id: &str, relative: &Path) -> io::Result<Option<ObjectRef>> {
        self.conn.query_row("SELECT sha256,original_bytes,stored_bytes FROM package_members WHERE package_id=?1 AND relative_path=?2",params![id,path(relative)?],|r|Ok(ObjectRef{sha256:r.get(0)?,bytes:r.get::<_,i64>(1)? as u64,stored_bytes:r.get::<_,i64>(2)? as u64})).optional().map_err(err)
    }
}
pub fn map_hash(map: &PackageMap) -> io::Result<String> {
    Ok(crate::evidence_store::digest(
        &serde_json::to_vec_pretty(map).map_err(err)?,
    ))
}
pub fn insert(db: &Connection, m: &PackageMap, released: bool) -> io::Result<()> {
    if m.schema_version != 1
        || m.id != crate::evidence_store::package_id(&m.root)?
        || !crate::evidence_store::safe(&m.identity_root)
        || !crate::evidence_store::safe(&m.manifest)
        || m.members.is_empty()
    {
        return Err(err("invalid package map"));
    }
    for (relative, o) in &m.members {
        if !crate::evidence_store::safe(relative) || relative == &m.manifest {
            return Err(err("unsafe member"));
        }
        crate::evidence_store::object_path(Path::new("."), &o.sha256)?;
    }
    db.execute("INSERT INTO package_maps(id,root,identity_root,schema_version,created_at,manifest,manifest_sha256,map_sha256,integrity_sha256,released) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8,?9) ON CONFLICT(id) DO UPDATE SET root=excluded.root,identity_root=excluded.identity_root,schema_version=excluded.schema_version,manifest=excluded.manifest,manifest_sha256=excluded.manifest_sha256,map_sha256=excluded.map_sha256,integrity_sha256=excluded.integrity_sha256,released=excluded.released",params![m.id,path(&m.root)?,path(&m.identity_root)?,m.schema_version,chrono::Utc::now().to_rfc3339(),path(&m.manifest)?,m.manifest_sha256,map_hash(m)?,released]).map_err(err)?;
    for table in ["package_resources", "package_members"] {
        db.execute(&format!("DELETE FROM {table} WHERE package_id=?1"), [&m.id])
            .map_err(err)?;
    }
    for (position, r) in m.resources.iter().enumerate() {
        db.execute(
            "INSERT INTO package_resources VALUES(?1,?2,?3)",
            params![m.id, r, position as i64],
        )
        .map_err(err)?;
    }
    for (relative, o) in &m.members {
        db.execute(
            "INSERT INTO package_members VALUES(?1,?2,?3,?4,?5)",
            params![
                m.id,
                path(relative)?,
                o.sha256,
                i64::try_from(o.bytes).map_err(err)?,
                i64::try_from(o.stored_bytes).map_err(err)?
            ],
        )
        .map_err(err)?;
    }
    Ok(())
}
