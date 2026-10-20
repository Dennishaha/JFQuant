//! Metadata-only, non-following filesystem inventory for workspace accounting.
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// File identity and allocation; timestamps detect changes without reading data bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileStamp {
    /// Filesystem device.
    pub device: u64,
    /// Inode within the device.
    pub inode: u64,
    /// Logical length.
    pub bytes: u64,
    /// Allocated blocks, in bytes; shared extents remain an estimate.
    pub allocated: u64,
    /// Number of hard links, including links outside this inventory.
    pub links: u64,
    /// Modification time, seconds and nanoseconds.
    pub modified: (i64, i64),
    /// Metadata change time, seconds and nanoseconds.
    pub changed: (i64, i64),
}
impl FileStamp {
    /// Capture metadata without following a symbolic link.
    pub fn read(path: &Path) -> io::Result<Self> {
        let m = fs::symlink_metadata(path)?;
        if !m.is_file() {
            return Err(io::Error::other("expected regular file"));
        }
        Ok(Self::from_metadata(&m))
    }
    /// Construct a stamp from metadata already obtained during traversal.
    pub fn from_metadata(m: &fs::Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
            bytes: m.len(),
            allocated: m.blocks().saturating_mul(512),
            links: m.nlink(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
        }
    }
}

impl crate::index::CacheRecord for FileStamp {}
/// A physical directory entry, relative to the inventory root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Relative path; symbolic links are reported separately.
    pub path: PathBuf,
    /// Identity and size.
    pub stamp: FileStamp,
}
/// Partial inventories retain diagnostics instead of counting errors as zero bytes.
#[derive(Debug, Default)]
pub struct Inventory {
    /// Regular files, deterministically ordered.
    pub files: Vec<FileEntry>,
    /// Symbolic links; targets are never traversed.
    pub symlinks: Vec<PathBuf>,
    /// Unreadable entries and nested mount points.
    pub errors: Vec<String>,
}
/// Walk one filesystem. The callback supports cancellation and incremental progress.
pub fn inventory(root: &Path, mut check: impl FnMut() -> io::Result<()>) -> io::Result<Inventory> {
    parallel::walk(root, 1, false, |_, _| check()).map(|v| v.inventory)
}
mod parallel;
pub use parallel::{DirectoryUsage, WalkProgress, WalkResult, walk, walk_stream};

/// Available bytes for an unprivileged caller on the containing filesystem.
pub fn available_bytes(root: &Path) -> io::Result<u64> {
    let s = rustix::fs::statvfs(root)?;
    Ok(s.f_bavail.saturating_mul(s.f_frsize))
}
