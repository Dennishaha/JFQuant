//! Original-byte views over immutable, content-addressed evidence.
#![allow(missing_docs)]
use crate::filesystem::FileStamp;
use analytics_core::task::TaskContext;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

pub const HEADER: &[u8; 8] = b"JFQOBJ01";
pub const REGISTRY: &str = ".jfquant/storage/packages/registry.json";
fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("evidence_store: {e}"))
}
pub fn digest(bytes: &[u8]) -> String {
    format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(bytes))
    )
}
pub fn safe(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|p| matches!(p, Component::Normal(_)))
}
pub fn package_id(root: &Path) -> io::Result<String> {
    Ok(digest(
        root.to_str()
            .ok_or_else(|| invalid("non-UTF8 package root"))?
            .as_bytes(),
    )[7..]
        .to_owned())
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObjectRef {
    pub sha256: String,
    pub bytes: u64,
    pub stored_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageMap {
    pub schema_version: u32,
    pub id: String,
    pub root: PathBuf,
    pub identity_root: PathBuf,
    pub manifest: PathBuf,
    pub manifest_sha256: String,
    pub resources: Vec<String>,
    pub members: BTreeMap<PathBuf, ObjectRef>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageLocator {
    pub root: PathBuf,
    pub map_sha256: String,
    pub released: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub schema_version: u32,
    pub packages: BTreeMap<String, PackageLocator>,
}
impl Default for Registry {
    fn default() -> Self {
        Self {
            schema_version: 1,
            packages: BTreeMap::new(),
        }
    }
}
#[derive(Clone)]
struct Cached {
    stamp: Option<FileStamp>,
    registry: Registry,
    maps: BTreeMap<String, (FileStamp, PackageMap)>,
}
thread_local! { static CACHE:RefCell<BTreeMap<PathBuf,Cached>>=const { RefCell::new(BTreeMap::new()) }; }
pub fn invalidate(root: &Path) {
    CACHE.with(|c| {
        c.borrow_mut().remove(root);
    });
}

pub fn owned(root: &Path, relative: &Path) -> io::Result<PathBuf> {
    if !safe(relative) {
        return Err(invalid("unsafe relative path"));
    }
    let mut p = root.to_owned();
    for c in relative.components() {
        p.push(c);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => return Err(invalid("unowned symbolic link")),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(p)
}
pub fn registry(root: &Path) -> io::Result<Registry> {
    let path = owned(root, Path::new(REGISTRY))?;
    if !path.try_exists()? {
        return Ok(Registry::default());
    }
    let r: Registry = serde_json::from_slice(&fs::read(path)?).map_err(invalid)?;
    if r.schema_version != 1 {
        return Err(invalid("unsupported registry format"));
    }
    for (id, l) in &r.packages {
        if id.len() != 64
            || !id.bytes().all(|b| b.is_ascii_hexdigit())
            || !safe(&l.root)
            || !l.map_sha256.starts_with("sha256:")
        {
            return Err(invalid("invalid package locator"));
        }
    }
    Ok(r)
}
pub fn map_path(root: &Path, id: &str) -> io::Result<PathBuf> {
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("invalid package ID"));
    }
    owned(
        root,
        &PathBuf::from(format!(".jfquant/storage/packages/{id}.json")),
    )
}
pub fn load_map(root: &Path, id: &str, locator: &PackageLocator) -> io::Result<PackageMap> {
    let bytes = fs::read(map_path(root, id)?)?;
    if digest(&bytes) != locator.map_sha256 {
        return Err(invalid("package map changed"));
    }
    let map: PackageMap = serde_json::from_slice(&bytes).map_err(invalid)?;
    if map.schema_version != 1
        || map.id != id
        || map.root != locator.root
        || !safe(&map.root)
        || !safe(&map.identity_root)
        || !safe(&map.manifest)
        || map.members.is_empty()
    {
        return Err(invalid("invalid package map"));
    }
    for (p, o) in &map.members {
        if !safe(p)
            || p == &map.manifest
            || o.sha256.len() != 71
            || !o.sha256.starts_with("sha256:")
            || !o.sha256[7..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(invalid("invalid object member"));
        }
    }
    Ok(map)
}
pub fn maps(root: &Path) -> io::Result<Vec<PackageMap>> {
    let r = registry(root)?;
    r.packages
        .iter()
        .filter(|(_, l)| !l.released)
        .map(|(id, l)| load_map(root, id, l))
        .collect()
}
fn workspace(path: &Path) -> io::Result<Option<(PathBuf, PathBuf)>> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    // Most evidence reads share one canonical workspace. Cache only roots, not every member
    // path; still check the marker, registry/map stamps and maintenance aliases on each read.
    let cached = CACHE.with(|cache| {
        cache
            .borrow()
            .keys()
            .filter(|root| absolute.starts_with(root))
            .max_by_key(|root| root.components().count())
            .cloned()
    });
    let candidate = cached
        .filter(|root| root.join(".jfquant/workspace.json").is_file())
        .or_else(|| {
            absolute
                .ancestors()
                .skip(1)
                .find(|p| p.join(".jfquant/workspace.json").is_file())
                .map(Path::to_owned)
        });
    if let Some(candidate) = candidate {
        let root = if CACHE.with(|cache| cache.borrow().contains_key(&candidate)) {
            candidate.clone()
        } else {
            fs::canonicalize(&candidate)?
        };
        let relative = absolute
            .strip_prefix(&candidate)
            .map_err(invalid)?
            .to_owned();
        // Only the maintenance command's explicit run aliases are recognized.
        let parts: Vec<_> = relative.components().collect();
        if parts.len() >= 3 && parts[0].as_os_str() == ".jfquant" && parts[1].as_os_str() == "runs"
        {
            let run = root.join(".jfquant/runs").join(parts[2]);
            if fs::symlink_metadata(&run).is_ok_and(|m| m.file_type().is_symlink()) {
                let actual = fs::canonicalize(&run)?;
                if !actual.starts_with(root.join(".jfquant/archive")) {
                    return Err(invalid("unknown run alias"));
                }
                let mut rel = actual.strip_prefix(&root).map_err(invalid)?.to_owned();
                for part in parts.iter().skip(3) {
                    rel.push(part);
                }
                return Ok(Some((root, rel)));
            }
        }
        return Ok(Some((root, relative)));
    }
    Ok(None)
}
fn locate(path: &Path) -> io::Result<Option<(PathBuf, ObjectRef)>> {
    let Some((root, relative)) = workspace(path)? else {
        return Ok(None);
    };
    let registry_path = owned(&root, Path::new(REGISTRY))?;
    let stamp = match FileStamp::read(&registry_path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.get(&root).is_none_or(|v| v.stamp != stamp) {
            cache.insert(
                root.clone(),
                Cached {
                    stamp,
                    registry: registry(&root)?,
                    maps: BTreeMap::new(),
                },
            );
        }
        let cached = cache.get_mut(&root).unwrap();
        let entry = cached
            .registry
            .packages
            .iter()
            .filter(|(_, l)| relative.starts_with(&l.root))
            .max_by_key(|(_, l)| l.root.components().count())
            .map(|(id, l)| (id.clone(), l.clone()));
        let Some((id, locator)) = entry else {
            return Ok(None);
        };
        if locator.released {
            return Err(invalid("相关产物已释放"));
        }
        let map_stamp = FileStamp::read(&map_path(&root, &id)?)
            .map_err(|e| invalid(format!("package map unavailable: {e}")))?;
        if cached.maps.get(&id).is_none_or(|(s, _)| s != &map_stamp) {
            cached
                .maps
                .insert(id.clone(), (map_stamp, load_map(&root, &id, &locator)?));
        }
        let map = &cached.maps[&id].1;
        let member = relative.strip_prefix(&map.root).map_err(invalid)?;
        Ok(map.members.get(member).cloned().map(|o| (root, o)))
    })
}
pub fn object_path(root: &Path, hash: &str) -> io::Result<PathBuf> {
    if hash.len() != 71
        || !hash.starts_with("sha256:")
        || !hash[7..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid("invalid content hash"));
    }
    owned(
        root,
        &PathBuf::from(format!(
            ".jfquant/objects/sha256/{}/{}.obj",
            &hash[7..9],
            &hash[7..]
        )),
    )
}
struct Verified {
    inner: Box<dyn Read>,
    hash: Sha256,
    expected: String,
    bytes: u64,
    read: u64,
    finished: bool,
}
impl Read for Verified {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        TaskContext::current().check()?;
        if buf.is_empty() {
            return Ok(0);
        }
        let n = self.inner.read(buf)?;
        self.read = self
            .read
            .checked_add(n as u64)
            .ok_or_else(|| invalid("object length overflow"))?;
        if self.read > self.bytes {
            return Err(invalid("object expanded beyond original size"));
        }
        self.hash.update(&buf[..n]);
        if n == 0 && !self.finished {
            self.finished = true;
            if self.read != self.bytes
                || format!(
                    "sha256:{:x}",
                    analytics_core::hash::Hex(self.hash.clone().finalize())
                ) != self.expected
            {
                return Err(invalid("object length or SHA-256 mismatch"));
            }
        }
        Ok(n)
    }
}
pub fn open_object(root: &Path, object: &ObjectRef) -> io::Result<Box<dyn Read>> {
    let mut f = fs::File::open(object_path(root, &object.sha256)?)?;
    if f.metadata()?.len() != object.stored_bytes {
        return Err(invalid("object storage length changed"));
    }
    let mut header = [0; 17];
    f.read_exact(&mut header)?;
    if &header[..8] != HEADER
        || u64::from_le_bytes(header[9..17].try_into().unwrap()) != object.bytes
    {
        return Err(invalid("unsupported object header"));
    }
    let inner: Box<dyn Read> = match header[8] {
        0 => Box::new(f),
        1 => Box::new(zstd::stream::read::Decoder::new(f)?),
        _ => return Err(invalid("unsupported object codec")),
    };
    Ok(Box::new(Verified {
        inner,
        hash: Sha256::new(),
        expected: object.sha256.clone(),
        bytes: object.bytes,
        read: 0,
        finished: false,
    }))
}
pub fn open(path: &Path) -> io::Result<Box<dyn Read>> {
    match locate(path)? {
        Some((root, o)) => open_object(&root, &o),
        None => Ok(Box::new(fs::File::open(path)?)),
    }
}
pub fn reference(path: &Path) -> io::Result<Option<(PathBuf, ObjectRef)>> {
    locate(path)
}
pub fn is_file(path: &Path) -> io::Result<bool> {
    Ok(locate(path)?.is_some() || path.is_file())
}
pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open(path.as_ref())?.read_to_end(&mut bytes)?;
    Ok(bytes)
}
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    String::from_utf8(read(path)?).map_err(invalid)
}
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    io::copy(&mut open(from.as_ref())?, &mut fs::File::create(to)?)
}
pub fn logical_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    fn visit(p: &Path, out: &mut BTreeSet<PathBuf>) -> io::Result<()> {
        for e in fs::read_dir(p)? {
            TaskContext::current().check()?;
            let e = e?;
            let ty = e.file_type()?;
            if ty.is_dir() {
                visit(&e.path(), out)?;
            } else if ty.is_file() {
                out.insert(e.path());
            } else {
                return Err(invalid("evidence tree contains a link"));
            }
        }
        Ok(())
    }
    let mut out = BTreeSet::new();
    visit(root, &mut out)?;
    if let Some((workspace, relative)) = workspace(&root.join(".member"))? {
        let relative = relative
            .parent()
            .ok_or_else(|| invalid("missing tree root"))?;
        for (id, locator) in registry(&workspace)?.packages {
            if locator.released
                || !(locator.root.starts_with(relative) || relative.starts_with(&locator.root))
            {
                continue;
            }
            let map = load_map(&workspace, &id, &locator)?;
            for p in map.members.keys() {
                let path = map.root.join(p);
                if path.starts_with(relative) {
                    out.insert(root.join(path.strip_prefix(relative).map_err(invalid)?));
                }
            }
        }
    }
    Ok(out.into_iter().collect())
}
pub fn copy_tree(source: &Path, output: &Path) -> io::Result<()> {
    fs::create_dir(output)?;
    for path in logical_files(source)? {
        TaskContext::current().check()?;
        if path
            .strip_prefix(source)
            .map_err(invalid)?
            .components()
            .any(|c| c.as_os_str() == ".tdx-runtime")
        {
            continue;
        }
        let dest = output.join(path.strip_prefix(source).map_err(invalid)?);
        fs::create_dir_all(
            dest.parent()
                .ok_or_else(|| invalid("missing output parent"))?,
        )?;
        copy(&path, &dest)?;
    }
    // Collectors expect this directory even for empty successful responses.
    if source.join("raw").is_dir() {
        fs::create_dir_all(output.join("raw"))?;
    }
    Ok(())
}
pub fn tree_hash(root: &Path) -> io::Result<String> {
    let mut h = Sha256::new();
    let mut buf = vec![0; 1024 * 1024];
    for p in logical_files(root)? {
        TaskContext::current().check()?;
        let r = p
            .strip_prefix(root)
            .map_err(invalid)?
            .to_str()
            .ok_or_else(|| invalid("non-UTF8 evidence path"))?;
        h.update((r.len() as u64).to_le_bytes());
        h.update(r.as_bytes());
        let (mut f, length): (Box<dyn Read>, u64) = match locate(&p)? {
            Some((workspace, object)) => (open_object(&workspace, &object)?, object.bytes),
            None => {
                let file = fs::File::open(&p)?;
                let length = file.metadata()?.len();
                (Box::new(file), length)
            }
        };
        h.update(length.to_le_bytes());
        loop {
            TaskContext::current().check()?;
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
    }
    Ok(format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(h.finalize())
    ))
}
struct Count(u64);
impl Write for Count {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0 += b.len() as u64;
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn encode<W: Write>(path: &Path, writer: W) -> io::Result<(String, u64, W)> {
    let mut input = open(path)?;
    let mut encoder = zstd::stream::write::Encoder::new(writer, 1)?;
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buf = vec![0; 1024 * 1024];
    loop {
        TaskContext::current().check()?;
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        bytes += n as u64;
        encoder.write_all(&buf[..n])?;
    }
    Ok((
        format!("sha256:{:x}", analytics_core::hash::Hex(hash.finalize())),
        bytes,
        encoder.finish()?,
    ))
}
pub fn estimate(path: &Path) -> io::Result<ObjectRef> {
    let (sha256, bytes, count) = encode(path, Count(0))?;
    Ok(ObjectRef {
        sha256,
        bytes,
        stored_bytes: 17 + count.0.min(bytes),
    })
}
pub fn create_object(
    root: &Path,
    source: &Path,
    expected: &ObjectRef,
    temp: &Path,
) -> io::Result<bool> {
    create(root, source, expected, temp, true)
}
/// Prepare an object, retaining `temp` as an inode ownership receipt when newly created.
/// The transaction syncs affected directories and removes the receipt after commit/rollback.
pub fn prepare_object(
    root: &Path,
    source: &Path,
    expected: &ObjectRef,
    temp: &Path,
) -> io::Result<bool> {
    create(root, source, expected, temp, false)
}
fn create(
    root: &Path,
    source: &Path,
    expected: &ObjectRef,
    temp: &Path,
    durable_parent: bool,
) -> io::Result<bool> {
    let final_path = object_path(root, &expected.sha256)?;
    if final_path.try_exists()? {
        io::copy(&mut open_object(root, expected)?, &mut io::sink())?;
        return Ok(false);
    }
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(temp)?;
    file.write_all(HEADER)?;
    file.write_all(&[1])?;
    file.write_all(&expected.bytes.to_le_bytes())?;
    let (hash, bytes, mut file) = encode(source, file)?;
    if hash != expected.sha256 || bytes != expected.bytes {
        return Err(invalid("migration source changed"));
    }
    if file.metadata()?.len() >= 17 + bytes {
        file.set_len(0)?;
        use std::io::Seek;
        file.rewind()?;
        file.write_all(HEADER)?;
        file.write_all(&[0])?;
        file.write_all(&bytes.to_le_bytes())?;
        let mut second = open(source)?;
        let mut hash = Sha256::new();
        let mut buf = vec![0; 1024 * 1024];
        loop {
            TaskContext::current().check()?;
            let n = second.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
            file.write_all(&buf[..n])?;
        }
        if format!("sha256:{:x}", analytics_core::hash::Hex(hash.finalize())) != expected.sha256 {
            return Err(invalid("migration source changed"));
        }
    }
    if file.metadata()?.len() != expected.stored_bytes {
        return Err(invalid("object estimate changed"));
    }
    file.sync_all()?;
    drop(file);
    fs::create_dir_all(final_path.parent().unwrap())?;
    match fs::hard_link(temp, &final_path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            io::copy(&mut open_object(root, expected)?, &mut io::sink())?;
            fs::remove_file(temp)?;
            return Ok(false);
        }
        Err(e) => return Err(e),
    }
    if durable_parent {
        fs::remove_file(temp)?;
        for dir in final_path.ancestors().skip(1).take(4) {
            fs::File::open(dir)?.sync_all()?;
        }
    }
    io::copy(&mut open_object(root, expected)?, &mut io::sink())?;
    Ok(true)
}
/// Filesystem facade for existing evidence consumers; ordinary writes remain ordinary writes.
pub mod io_fs {
    pub use super::{copy, read, read_to_string};
    pub use std::fs::*;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn storage_objects_preserve_both_encodings_and_reject_corruption_and_unknown_versions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for (i, bytes) in [
            vec![b'x'; 32768],
            Sha256::digest(b"noncompressible small document").to_vec(),
        ]
        .into_iter()
        .enumerate()
        {
            let source = root.join(format!("source-{i}"));
            fs::write(&source, &bytes).unwrap();
            let object = estimate(&source).unwrap();
            let pending = root.join("object.tmp");
            assert!(create_object(root, &source, &object, &pending).unwrap());
            assert!(!create_object(root, &source, &object, &pending).unwrap());
            let mut original = vec![];
            open_object(root, &object)
                .unwrap()
                .read_to_end(&mut original)
                .unwrap();
            assert_eq!(original, bytes);
            let path = object_path(root, &object.sha256).unwrap();
            let mut encoded = fs::read(&path).unwrap();
            assert_eq!(encoded[8], if i == 0 { 1 } else { 0 });
            encoded[8] = 9;
            fs::write(&path, &encoded).unwrap();
            assert!(open_object(root, &object).is_err());
            encoded[8] = if i == 0 { 1 } else { 0 };
            *encoded.last_mut().unwrap() ^= 1;
            fs::write(&path, &encoded).unwrap();
            assert!(io::copy(&mut open_object(root, &object).unwrap(), &mut io::sink()).is_err());
        }
        assert!(object_path(root, "../../outside").is_err());
    }
}
