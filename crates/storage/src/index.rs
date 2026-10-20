//! Rebuildable, bounded SQLite records. Connections never leave their owner thread.
#![allow(missing_docs)]
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    io,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub trait CacheRecord: Serialize + DeserializeOwned {
    fn cache_bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(err)
    }
    fn from_cache(_path: PathBuf, bytes: &[u8]) -> io::Result<Self> {
        serde_json::from_slice(bytes).map_err(err)
    }
    fn owned(&self) -> bool {
        false
    }
}
impl CacheRecord for String {}
fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("storage_index: {e}"))
}
struct State {
    db: Connection,
    error: RefCell<Option<String>>,
    writes: Cell<usize>,
    generations: RefCell<BTreeMap<String, i64>>,
    cancel: Arc<AtomicBool>,
}
impl State {
    fn remember<T>(&self, result: io::Result<T>) -> Option<T> {
        match result {
            Ok(v) => Some(v),
            Err(e) => {
                *self.error.borrow_mut() = Some(e.to_string());
                None
            }
        }
    }
    fn batch(&self) -> io::Result<()> {
        let n = self.writes.get() + 1;
        self.writes.set(n);
        if n.is_multiple_of(4096) {
            self.db
                .execute_batch("COMMIT; BEGIN IMMEDIATE")
                .map_err(err)?;
        }
        Ok(())
    }
}
impl Drop for State {
    fn drop(&mut self) {
        // Cancellation must not interrupt rollback of the current cache batch.
        let _ = self.db.progress_handler(0, None::<fn() -> bool>);
        let _ = self
            .db
            .execute_batch(if self.cancel.load(Ordering::Relaxed) {
                "ROLLBACK"
            } else {
                "COMMIT"
            });
    }
}
#[derive(Clone)]
pub struct Index {
    state: Rc<State>,
}
pub struct ResourceKey<'a> {
    pub id: &'a str,
    pub category: &'a str,
    pub task: Option<&'a str>,
    pub modified: &'a str,
    pub size: u64,
}
impl Index {
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let db = Connection::open(path).map_err(err)?;
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(err)?;
        if !matches!(version, 0 | 1) {
            return Err(err("unsupported cache schema; rebuild required"));
        }
        db.busy_timeout(std::time::Duration::from_secs(2))
            .map_err(err)?;
        db.execute_batch("PRAGMA auto_vacuum=INCREMENTAL; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-32768; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS records(scan INTEGER NOT NULL, kind TEXT NOT NULL, path BLOB NOT NULL, payload BLOB NOT NULL, device BLOB, inode BLOB, content_hash TEXT,owned INTEGER NOT NULL DEFAULT 0,filename BLOB, PRIMARY KEY(scan,kind,path)) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS file_identity ON records(scan,device,inode) WHERE device IS NOT NULL;
            CREATE INDEX IF NOT EXISTS content_identity ON records(scan,content_hash) WHERE content_hash IS NOT NULL;
            CREATE TABLE IF NOT EXISTS resources(scan INTEGER NOT NULL, id TEXT NOT NULL, category TEXT NOT NULL, task TEXT, modified TEXT NOT NULL, size INTEGER NOT NULL, payload BLOB NOT NULL, PRIMARY KEY(scan,id)) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS resource_filter ON resources(scan,category,task,size);
            CREATE TABLE IF NOT EXISTS owners(scan INTEGER NOT NULL,path BLOB NOT NULL,owner TEXT NOT NULL,PRIMARY KEY(scan,path,owner)) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS owner_resource ON owners(scan,owner,path);
            CREATE TABLE IF NOT EXISTS generations(id INTEGER PRIMARY KEY,scan TEXT NOT NULL UNIQUE,complete INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS owned_paths ON records(scan,kind,path) WHERE owned=1;
            CREATE INDEX IF NOT EXISTS json_paths ON records(scan,kind,path) WHERE filename IS NOT NULL;
            CREATE INDEX IF NOT EXISTS named_paths ON records(scan,kind,filename,path) WHERE filename IS NOT NULL;
            CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            PRAGMA user_version=1; BEGIN IMMEDIATE;").map_err(err)?;
        let cancel = analytics_core::task::TaskContext::current().cancel;
        let sql_cancel = cancel.clone();
        db.progress_handler(1024, Some(move || sql_cancel.load(Ordering::Relaxed)))
            .map_err(err)?;
        Ok(Self {
            state: Rc::new(State {
                db,
                error: RefCell::new(None),
                writes: Cell::new(0),
                generations: RefCell::new(BTreeMap::new()),
                cancel,
            }),
        })
    }
    pub fn open_read(path: &Path) -> io::Result<Self> {
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(err)?;
        db.busy_timeout(std::time::Duration::from_millis(500))
            .map_err(err)?;
        Ok(Self {
            state: Rc::new(State {
                db,
                error: RefCell::new(None),
                writes: Cell::new(0),
                generations: RefCell::new(BTreeMap::new()),
                cancel: analytics_core::task::TaskContext::current().cancel,
            }),
        })
    }
    fn generation(&self, scan: &str) -> io::Result<i64> {
        if let Some(id) = self.state.generations.borrow().get(scan) {
            return Ok(*id);
        }
        let id = self
            .state
            .db
            .query_row("SELECT id FROM generations WHERE scan=?1", [scan], |r| {
                r.get(0)
            })
            .map_err(err)?;
        self.state.generations.borrow_mut().insert(scan.into(), id);
        Ok(id)
    }
    pub fn begin(&self, scan: &str) -> io::Result<()> {
        let completed = self
            .state
            .db
            .query_row(
                "SELECT complete FROM generations WHERE scan=?1",
                [scan],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(err)?;
        if completed == Some(1) {
            return Err(err("completed scan generation cannot be overwritten"));
        }
        for table in ["records", "owners", "resources"] {
            self.state.db.execute(&format!("DELETE FROM {table} WHERE scan != COALESCE((SELECT id FROM generations WHERE scan=(SELECT value FROM metadata WHERE key='latest')),0)"),[]).map_err(err)?;
        }
        self.state.db.execute("DELETE FROM generations WHERE scan != COALESCE((SELECT value FROM metadata WHERE key='latest'),'')",[]).map_err(err)?;
        self.state.db.execute("INSERT INTO generations(scan,complete) VALUES(?1,0) ON CONFLICT(scan) DO UPDATE SET complete=0",[scan]).map_err(err)?;
        self.state.generations.borrow_mut().clear();
        Ok(())
    }
    pub fn table<T: CacheRecord>(&self, scan: &str, kind: &str) -> DiskMap<T> {
        DiskMap {
            index: self.clone(),
            scan: self.state.remember(self.generation(scan)).unwrap_or(0),
            kind: kind.into(),
            marker: PhantomData,
        }
    }
    pub fn check(&self) -> io::Result<()> {
        match &*self.state.error.borrow() {
            Some(e) => Err(err(e)),
            None => Ok(()),
        }
    }
    pub fn flush(&self) -> io::Result<()> {
        self.check()?;
        self.state
            .db
            .execute_batch("COMMIT; BEGIN IMMEDIATE")
            .map_err(err)
    }
    pub fn complete(&self, scan: &str) -> io::Result<()> {
        self.check()?;
        self.state
            .db
            .execute("UPDATE generations SET complete=1 WHERE scan=?1", [scan])
            .map_err(err)?;
        self.state
            .db
            .execute(
                "INSERT OR REPLACE INTO metadata(key,value) VALUES('latest',?1)",
                [scan],
            )
            .map_err(err)?;
        self.state.db.execute_batch("COMMIT; PRAGMA wal_checkpoint(TRUNCATE); PRAGMA incremental_vacuum(4096); BEGIN IMMEDIATE").map_err(err)
    }
    pub fn prune(&self, scan: &str) -> io::Result<()> {
        let id = self.generation(scan)?;
        for table in ["records", "owners", "resources"] {
            self.state
                .db
                .execute(&format!("DELETE FROM {table} WHERE scan!=?1"), [id])
                .map_err(err)?;
        }
        self.state
            .db
            .execute("DELETE FROM generations WHERE id!=?1", [id])
            .map_err(err)?;
        self.state.db.execute_batch("COMMIT; PRAGMA wal_checkpoint(TRUNCATE); PRAGMA incremental_vacuum; BEGIN IMMEDIATE").map_err(err)
    }
    pub fn generation_exists(&self, scan: &str) -> io::Result<bool> {
        self.state
            .db
            .query_row(
                "SELECT complete FROM generations WHERE scan=?1",
                [scan],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map(|v| v.is_some())
            .map_err(err)
    }
    pub fn resource(
        &self,
        scan: &str,
        key: ResourceKey<'_>,
        value: &impl Serialize,
    ) -> io::Result<()> {
        let ResourceKey {
            id,
            category,
            task,
            modified,
            size,
        } = key;
        let size = i64::try_from(size).map_err(err)?;
        self.state
            .db
            .execute(
                "INSERT OR REPLACE INTO resources VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    self.generation(scan)?,
                    id,
                    category,
                    task,
                    modified,
                    size,
                    serde_json::to_vec(value).map_err(err)?
                ],
            )
            .map_err(err)?;
        self.state.batch()
    }
    pub fn resources<T: DeserializeOwned>(
        &self,
        scan: &str,
        category: Option<&str>,
        task: Option<&str>,
        before: Option<&str>,
        newest: bool,
    ) -> io::Result<Vec<T>> {
        let order = if newest {
            "modified DESC,id"
        } else {
            "size DESC,id"
        };
        let mut q = self.state.db.prepare(&format!("SELECT payload FROM resources WHERE scan=?1 AND (?2 IS NULL OR category=?2) AND (?3 IS NULL OR task=?3) AND (?4 IS NULL OR substr(modified,1,10)<?4) ORDER BY {order}")).map_err(err)?;
        q.query_map(
            params![self.generation(scan)?, category, task, before],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .map_err(err)?
        .map(|r| serde_json::from_slice(&r.map_err(err)?).map_err(err))
        .collect()
    }
    pub fn owners(&self, scan: &str, path: &Path, owners: &[String]) -> io::Result<()> {
        self.state
            .db
            .execute(
                "DELETE FROM owners WHERE scan=?1 AND path=?2",
                params![self.generation(scan)?, path.as_os_str().as_bytes()],
            )
            .map_err(err)?;
        let mut stmt = self
            .state
            .db
            .prepare_cached("INSERT OR IGNORE INTO owners VALUES(?1,?2,?3)")
            .map_err(err)?;
        for owner in owners {
            stmt.execute(params![
                self.generation(scan)?,
                path.as_os_str().as_bytes(),
                owner
            ])
            .map_err(err)?;
        }
        self.state.batch()
    }
    pub fn selected<T: CacheRecord>(&self, scan: &str, ids: &[String]) -> io::Result<Vec<T>> {
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for id in ids {
            let mut q = self.state.db.prepare("SELECT r.path,r.payload FROM records r JOIN owners o ON r.scan=o.scan AND r.path=o.path WHERE r.kind='files' AND r.scan=?1 AND o.owner=?2 ORDER BY r.path").map_err(err)?;
            for row in q
                .query_map(params![self.generation(scan)?, id], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .map_err(err)?
            {
                let (path, bytes) = row.map_err(err)?;
                if seen.insert(path.clone()) {
                    out.push(T::from_cache(
                        PathBuf::from(std::ffi::OsString::from_vec(path)),
                        &bytes,
                    )?);
                }
            }
        }
        Ok(out)
    }

    /// Import resource metadata from a manifest inventory without filesystem scanning.
    /// This enables fast index updates when producers record metadata at publication time.
    pub fn import_from_inventory(
        &self,
        scan: &str,
        resource_id: &str,
        category: &str,
        inventory: &crate::manifest_metadata::ManifestInventory,
    ) -> io::Result<()> {
        inventory.validate()?;

        // Import each file into the records table
        let files_map = self.table::<crate::filesystem::FileStamp>(scan, "files");
        for (path, metadata) in &inventory.files {
            let stamp = crate::filesystem::FileStamp {
                device: 0, // Placeholder, will be filled during full verification
                inode: 0,  // Placeholder, will be filled during full verification
                bytes: metadata.bytes,
                allocated: metadata.allocated,
                links: 1,
                modified: metadata.modified,
                changed: metadata.modified, // Use same as modified
            };
            files_map.insert(path, &stamp)?;

            // Record ownership
            self.owners(scan, path, &[resource_id.to_string()])?;
        }

        // Register the resource
        self.resource(
            scan,
            ResourceKey {
                id: resource_id,
                category,
                task: None,
                modified: &inventory.created_at,
                size: inventory.total_bytes,
            },
            &serde_json::json!({
                "from_inventory": true,
                "files_count": inventory.files.len(),
                "total_bytes": inventory.total_bytes,
                "total_allocated": inventory.total_allocated,
            }),
        )?;

        Ok(())
    }
}

pub struct DiskMap<T> {
    index: Index,
    scan: i64,
    kind: String,
    marker: PhantomData<T>,
}
impl<T> Clone for DiskMap<T> {
    fn clone(&self) -> Self {
        Self {
            index: self.index.clone(),
            scan: self.scan,
            kind: self.kind.clone(),
            marker: PhantomData,
        }
    }
}
impl<T: CacheRecord> DiskMap<T> {
    pub fn insert(&self, path: &Path, value: &T) -> io::Result<()> {
        self.index
            .state
            .db
            .prepare_cached("INSERT INTO records(scan,kind,path,payload,owned) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(scan,kind,path) DO UPDATE SET payload=excluded.payload,owned=excluded.owned")
            .map_err(err)?
            .execute(params![
                self.scan,
                self.kind,
                path.as_os_str().as_bytes(),
                value.cache_bytes()?,value.owned()
            ])
            .map_err(err)?;
        self.index.state.batch()
    }
    pub fn insert_file(
        &self,
        path: &Path,
        value: &T,
        stamp: &crate::filesystem::FileStamp,
    ) -> io::Result<()> {
        let filename = path
            .extension()
            .filter(|e| *e == "json")
            .and_then(|_| path.file_name())
            .map(|s| s.as_bytes());
        self.index.state.db.prepare_cached("INSERT OR REPLACE INTO records(scan,kind,path,payload,device,inode,owned,filename) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)").map_err(err)?.execute(params![self.scan,self.kind,path.as_os_str().as_bytes(),value.cache_bytes()?,stamp.device.to_le_bytes().as_slice(),stamp.inode.to_le_bytes().as_slice(),value.owned(),filename]).map_err(err)?;
        self.index.state.batch()
    }
    pub fn content_hash(&self, path: &Path, hash: &str) -> io::Result<()> {
        self.index
            .state
            .db
            .prepare_cached(
                "UPDATE records SET content_hash=?4 WHERE scan=?1 AND kind=?2 AND path=?3",
            )
            .map_err(err)?
            .execute(params![
                self.scan,
                self.kind,
                path.as_os_str().as_bytes(),
                hash
            ])
            .map_err(err)?;
        self.index.state.batch()
    }
    pub fn get(&self, path: &Path) -> Option<T> {
        self.index
            .state
            .remember((|| {
                let bytes: Option<Vec<u8>> = self
                    .index
                    .state
                    .db
                    .prepare_cached(
                        "SELECT payload FROM records WHERE scan=?1 AND kind=?2 AND path=?3",
                    )
                    .map_err(err)?
                    .query_row(
                        params![self.scan, self.kind, path.as_os_str().as_bytes()],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(err)?;
                bytes
                    .map(|b| T::from_cache(path.to_owned(), &b))
                    .transpose()
            })())
            .flatten()
    }
    pub fn contains_key(&self, path: &Path) -> bool {
        self.index
            .state
            .remember(
                self.index
                    .state
                    .db
                    .query_row(
                        "SELECT 1 FROM records WHERE scan=?1 AND kind=?2 AND path=?3",
                        params![self.scan, self.kind, path.as_os_str().as_bytes()],
                        |_| Ok(true),
                    )
                    .optional()
                    .map(|v| v.unwrap_or(false))
                    .map_err(err),
            )
            .unwrap_or(false)
    }
    pub fn get_mut(&self, path: &Path) -> Option<RecordGuard<T>> {
        self.get(path).map(|value| RecordGuard {
            table: self.clone(),
            path: path.into(),
            value,
        })
    }
    pub fn entries(&self) -> Records<T> {
        Records {
            table: self.clone(),
            last: None,
            pending: VecDeque::new(),
            done: false,
            filter: "",
        }
    }
    pub fn owned_values(&self) -> impl Iterator<Item = T> {
        let mut rows = self.entries();
        rows.filter = "AND owned=1";
        rows.map(|(_, v)| v)
    }
    pub fn json_values(&self) -> impl Iterator<Item = T> {
        let mut rows = self.entries();
        rows.filter = "AND filename IS NOT NULL";
        rows.map(|(_, v)| v)
    }
    pub fn named(&self, name: &str) -> Vec<PathBuf> {
        self.index.state.remember((|| {
            let mut q=self.index.state.db.prepare("SELECT path FROM records INDEXED BY named_paths WHERE scan=?1 AND kind=?2 AND filename=?3 ORDER BY path").map_err(err)?;
            q.query_map(params![self.scan,self.kind,name.as_bytes()],|r|r.get::<_,Vec<u8>>(0)).map_err(err)?.map(|r|Ok(PathBuf::from(std::ffi::OsString::from_vec(r.map_err(err)?)))).collect::<io::Result<Vec<_>>>()
        })()).unwrap_or_default()
    }
    pub fn keys(&self) -> Keys<T> {
        Keys {
            table: self.clone(),
            last: None,
            pending: VecDeque::new(),
            done: false,
        }
    }
    pub fn values(&self) -> impl Iterator<Item = T> {
        self.entries().map(|(_, v)| v)
    }
    pub fn values_mut(&self) -> impl Iterator<Item = RecordGuard<T>> {
        let table = self.clone();
        self.entries().map(move |(path, value)| RecordGuard {
            table: table.clone(),
            path,
            value,
        })
    }
    pub fn under(&self, root: &Path) -> Vec<PathBuf> {
        let prefix = root.as_os_str().as_bytes();
        let mut lower = prefix.to_vec();
        lower.push(b'/');
        let mut upper = lower.clone();
        *upper.last_mut().unwrap() += 1;
        self.index.state.remember((|| {
            let mut q=self.index.state.db.prepare("SELECT path FROM records WHERE scan=?1 AND kind=?2 AND path=?3 UNION ALL SELECT path FROM records WHERE scan=?1 AND kind=?2 AND path>=?4 AND path<?5 ORDER BY path").map_err(err)?;
            q.query_map(params![self.scan,self.kind,prefix,lower,upper], |r|r.get::<_,Vec<u8>>(0)).map_err(err)?.map(|v|Ok(PathBuf::from(std::ffi::OsString::from_vec(v.map_err(err)?)))).collect::<io::Result<Vec<_>>>()
        })()).unwrap_or_default()
    }
}
pub struct RecordGuard<T: CacheRecord> {
    table: DiskMap<T>,
    path: PathBuf,
    value: T,
}
impl<T: CacheRecord> Deref for RecordGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}
impl<T: CacheRecord> DerefMut for RecordGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}
impl<T: CacheRecord> Drop for RecordGuard<T> {
    fn drop(&mut self) {
        self.table
            .index
            .state
            .remember(self.table.insert(&self.path, &self.value));
    }
}
pub struct Records<T> {
    filter: &'static str,
    table: DiskMap<T>,
    last: Option<Vec<u8>>,
    pending: VecDeque<(PathBuf, T)>,
    done: bool,
}
pub struct Keys<T> {
    table: DiskMap<T>,
    last: Option<Vec<u8>>,
    pending: VecDeque<Vec<u8>>,
    done: bool,
}
impl<T: CacheRecord> Iterator for Keys<T> {
    type Item = PathBuf;
    fn next(&mut self) -> Option<PathBuf> {
        if self.pending.is_empty() && !self.done {
            let result = (|| {
                analytics_core::task::TaskContext::current().check()?;
                let mut q=self.table.index.state.db.prepare("SELECT path FROM records WHERE scan=?1 AND kind=?2 AND path>?3 ORDER BY path LIMIT 512").map_err(err)?;
                q.query_map(
                    params![
                        self.table.scan,
                        self.table.kind,
                        self.last.as_deref().unwrap_or(&[])
                    ],
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .map_err(err)?
                .map(|r| r.map_err(err))
                .collect::<io::Result<VecDeque<_>>>()
            })();
            self.pending = self.table.index.state.remember(result).unwrap_or_default();
            self.done = self.pending.len() < 512;
        }
        let p = self.pending.pop_front()?;
        self.last = Some(p.clone());
        Some(PathBuf::from(std::ffi::OsString::from_vec(p)))
    }
}
impl<T: CacheRecord> Iterator for Records<T> {
    type Item = (PathBuf, T);
    fn next(&mut self) -> Option<Self::Item> {
        if self.pending.is_empty() && !self.done {
            let result = (|| {
                analytics_core::task::TaskContext::current().check()?;
                // Without statistics SQLite prefers the primary key even for empty partial indexes,
                // making selective owner/JSON queries walk the entire generation.
                let hint = match self.filter {
                    "AND owned=1" => "INDEXED BY owned_paths",
                    "AND filename IS NOT NULL" => "INDEXED BY json_paths",
                    _ => "",
                };
                let mut q=self.table.index.state.db.prepare(&format!("SELECT path,payload FROM records {hint} WHERE scan=?1 AND kind=?2 AND path>?3 {} ORDER BY path LIMIT 256",self.filter)).map_err(err)?;
                q.query_map(
                    params![
                        self.table.scan,
                        self.table.kind,
                        self.last.as_deref().unwrap_or(&[])
                    ],
                    |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
                )
                .map_err(err)?
                .map(|row| {
                    let (p, b) = row.map_err(err)?;
                    let path = PathBuf::from(std::ffi::OsString::from_vec(p));
                    let value = T::from_cache(path.clone(), &b)?;
                    Ok((path, value))
                })
                .collect::<io::Result<VecDeque<_>>>()
            })();
            self.pending = self.table.index.state.remember(result).unwrap_or_default();
            self.done = self.pending.len() < 256;
        }
        let v = self.pending.pop_front()?;
        self.last = Some(v.0.as_os_str().as_bytes().to_vec());
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn storage_sqlite_cancel_interrupts_generation_cleanup_and_keeps_completed_scan() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.sqlite");
        let cancel = Arc::new(AtomicBool::new(false));
        let context =
            analytics_core::task::TaskContext::new(uuid::Uuid::new_v4(), cancel.clone(), |_| {});
        context.execute(|| {
            let index = Index::open(&path).unwrap();
            index.begin("complete").unwrap();
            index
                .table::<String>("complete", "files")
                .insert(Path::new("kept"), &"kept".into())
                .unwrap();
            index.complete("complete").unwrap();
            index.begin("abandoned").unwrap();
            let files = index.table::<String>("abandoned", "files");
            for i in 0..10000 {
                files
                    .insert(Path::new(&format!("{i:05}")), &"discardable".into())
                    .unwrap();
            }
            index.flush().unwrap();
            cancel.store(true, Ordering::Relaxed);
            assert!(
                index
                    .begin("replacement")
                    .unwrap_err()
                    .to_string()
                    .contains("interrupted")
            );
        });
        let index = Index::open_read(&path).unwrap();
        assert_eq!(
            index
                .table::<String>("complete", "files")
                .get(Path::new("kept")),
            Some("kept".into())
        );
        assert!(!index.generation_exists("replacement").unwrap());
    }
    #[test]
    fn storage_sqlite_batches_order_records_and_keep_reader_generation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.sqlite");
        let index = Index::open(&path).unwrap();
        index.begin("one").unwrap();
        let rows = index.table::<String>("one", "files");
        for i in (0..1500).rev() {
            rows.insert(Path::new(&format!("目录/{i:04}")), &format!("value {i}"))
                .unwrap();
        }
        assert_eq!(rows.keys().count(), 1500);
        assert_eq!(rows.entries().count(), 1500);
        rows.get_mut(Path::new("目录/0000"))
            .unwrap()
            .push_str(" updated");
        assert_eq!(rows.get(Path::new("目录/0000")).unwrap(), "value 0 updated");
        assert_eq!(rows.under(Path::new("目录")).len(), 1500);
        index
            .resource(
                "one",
                ResourceKey {
                    id: "r",
                    category: "evidence",
                    task: None,
                    modified: "2026-10-20",
                    size: 12,
                },
                &"resource",
            )
            .unwrap();
        index.complete("one").unwrap();
        index.begin("two").unwrap();
        index
            .table::<String>("two", "files")
            .insert(Path::new("new"), &"new".into())
            .unwrap();
        index.flush().unwrap();
        let reader = Index::open_read(&path).unwrap();
        assert_eq!(
            reader
                .resources::<String>("one", Some("evidence"), None, None, false)
                .unwrap(),
            vec!["resource"]
        );
        assert_eq!(reader.table::<String>("one", "files").keys().count(), 1500);
        index.check().unwrap();
        drop(reader);
        index.complete("two").unwrap();
        index.prune("two").unwrap();
        assert!(!index.generation_exists("one").unwrap());
    }
    #[test]
    fn storage_sqlite_errors_cannot_turn_into_a_complete_empty_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(&temp.path().join("index.sqlite")).unwrap();
        index.begin("s").unwrap();
        let files = index.table::<String>("s", "files");
        index.state.db.execute_batch("DROP TABLE records").unwrap();
        assert!(files.get(Path::new("missing")).is_none());
        assert!(index.check().is_err());
        assert!(index.complete("s").is_err());
    }

    #[test]
    fn storage_sqlite_imports_from_manifest_inventory() {
        use crate::manifest_metadata::{FileMetadata, ManifestInventory};
        use std::collections::BTreeMap;

        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(&temp.path().join("index.sqlite")).unwrap();

        // Create a sample inventory
        let mut files = BTreeMap::new();
        files.insert(
            PathBuf::from("data/predictions/fold_1.parquet"),
            FileMetadata {
                bytes: 1024,
                allocated: 4096,
                modified: (1700000000, 0),
                sha256: Some("sha256:abc123".into()),
            },
        );
        files.insert(
            PathBuf::from("data/predictions/fold_2.parquet"),
            FileMetadata {
                bytes: 2048,
                allocated: 4096,
                modified: (1700000001, 0),
                sha256: Some("sha256:def456".into()),
            },
        );

        let inventory = ManifestInventory::new("predictions", files.clone());

        // Import into index
        index.begin("scan1").unwrap();
        index
            .import_from_inventory("scan1", "resource-123", "predictions", &inventory)
            .unwrap();
        index.complete("scan1").unwrap();

        // Verify resource was registered
        let resources: Vec<serde_json::Value> = index
            .resources("scan1", Some("predictions"), None, None, false)
            .unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0]["from_inventory"], true);
        assert_eq!(resources[0]["files_count"], 2);
        assert_eq!(resources[0]["total_bytes"], 3072);

        // Verify files were imported
        let files_map = index.table::<crate::filesystem::FileStamp>("scan1", "files");
        assert_eq!(files_map.keys().count(), 2);
        let stamp = files_map
            .get(Path::new("data/predictions/fold_1.parquet"))
            .unwrap();
        assert_eq!(stamp.bytes, 1024);
        assert_eq!(stamp.allocated, 4096);

        // Verify ownership
        let selected: Vec<crate::filesystem::FileStamp> =
            index.selected("scan1", &["resource-123".into()]).unwrap();
        assert_eq!(selected.len(), 2);
    }
}
