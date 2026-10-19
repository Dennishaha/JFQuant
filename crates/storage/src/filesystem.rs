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
        Ok(Self {
            device: m.dev(),
            inode: m.ino(),
            bytes: m.len(),
            allocated: m.blocks().saturating_mul(512),
            links: m.nlink(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
        })
    }
}
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
    let device = fs::metadata(root)?.dev();
    let mut out = Inventory::default();
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        check()?;
        let entries = match fs::read_dir(&dir) {
            Ok(v) => v,
            Err(e) => {
                out.errors.push(format!("{}: {e}", dir.display()));
                continue;
            }
        };
        for entry in entries {
            check()?;
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    out.errors.push(e.to_string());
                    continue;
                }
            };
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(io::Error::other)?
                .to_owned();
            let m = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) => {
                    out.errors.push(format!("{}: {e}", relative.display()));
                    continue;
                }
            };
            if m.file_type().is_symlink() {
                out.symlinks.push(relative);
            } else if m.dev() != device {
                out.errors.push(format!(
                    "{}: nested filesystem is not managed",
                    relative.display()
                ));
            } else if m.is_dir() {
                pending.push(path);
            } else if m.is_file() {
                match FileStamp::read(&path) {
                    Ok(stamp) => out.files.push(FileEntry {
                        path: relative,
                        stamp,
                    }),
                    Err(e) => out.errors.push(format!("{}: {e}", relative.display())),
                }
            }
        }
    }
    out.files.sort_by(|a, b| a.path.cmp(&b.path));
    out.symlinks.sort();
    Ok(out)
}
/// Available bytes for an unprivileged caller on the containing filesystem.
pub fn available_bytes(root: &Path) -> io::Result<u64> {
    let s = rustix::fs::statvfs(root)?;
    Ok(s.f_bavail.saturating_mul(s.f_frsize))
}
