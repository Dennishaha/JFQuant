//! Bounded directory workers; a full queue is drained inline instead of blocking producers.
use super::*;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    time::{Duration, Instant},
};
/// Disjoint directory bucket, with hard links assigned to their smallest bucket.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectoryUsage {
    /// Logical bytes, deduplicated by inode.
    pub logical_bytes: u64,
    /// Allocated bytes; shared extents remain an estimate.
    pub allocated_bytes: u64,
    /// Number of file paths, including hard link aliases.
    pub files: u64,
}
/// Live traversal counters. Counts do not imply a complete snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WalkProgress {
    /// Entries in directories that have finished enumeration.
    pub entries: u64,
    /// Number of fully enumerated directories.
    pub completed_directories: u64,
    /// Space discovered so far.
    pub total: DirectoryUsage,
    /// Disjoint directory buckets discovered so far.
    pub directories: BTreeMap<PathBuf, DirectoryUsage>,
}
/// Full detail or compact overview of one traversal.
pub struct WalkResult {
    /// File detail, omitted in overview mode.
    pub inventory: Inventory,
    /// Final counters and directory aggregates.
    pub progress: WalkProgress,
}
enum Event {
    Files(Vec<FileEntry>),
    Link(PathBuf),
    Error(String),
    Directory(u64),
}
struct Queue {
    pending: VecDeque<PathBuf>,
    outstanding: usize,
}
struct Walker<'a> {
    root: &'a Path,
    device: u64,
    queue: (Mutex<Queue>, Condvar),
    stop: AtomicBool,
    tx: SyncSender<Event>,
}
impl Walker<'_> {
    fn send(&self, event: Event) -> io::Result<()> {
        self.tx
            .send(event)
            .map_err(|_| io::Error::new(io::ErrorKind::Interrupted, "scan stopped"))
    }
    fn directory(&self, dir: PathBuf) -> io::Result<()> {
        let before = fs::symlink_metadata(&dir)?;
        if !before.is_dir() || before.file_type().is_symlink() {
            return self.send(Event::Error(format!(
                "{}: directory changed during scan",
                dir.display()
            )));
        }
        if before.dev() != self.device {
            return self.send(Event::Error(format!(
                "{}: nested filesystem is not managed",
                dir.display()
            )));
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                self.send(Event::Error(format!("{}: {e}", dir.display())))?;
                return Ok(());
            }
        };
        let mut batch = Vec::with_capacity(256);
        let mut count = 1;
        for entry in entries {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            count += 1;
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    self.send(Event::Error(e.to_string()))?;
                    continue;
                }
            };
            let path = entry.path();
            let relative = path
                .strip_prefix(self.root)
                .map_err(io::Error::other)?
                .to_owned();
            let m = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) => {
                    self.send(Event::Error(format!("{}: {e}", relative.display())))?;
                    continue;
                }
            };
            if m.file_type().is_symlink() {
                self.send(Event::Link(relative))?;
            } else if m.dev() != self.device {
                self.send(Event::Error(format!(
                    "{}: nested filesystem is not managed",
                    relative.display()
                )))?;
            } else if m.is_dir() {
                let mut queue = self.queue.0.lock().unwrap();
                if queue.pending.len() < 128 {
                    queue.pending.push_back(path);
                    queue.outstanding += 1;
                    self.queue.1.notify_one();
                } else {
                    drop(queue);
                    self.directory(path)?;
                }
            } else if m.is_file() {
                batch.push(FileEntry {
                    path: relative,
                    stamp: FileStamp::from_metadata(&m),
                });
                if batch.len() == 256 {
                    self.send(Event::Files(std::mem::replace(
                        &mut batch,
                        Vec::with_capacity(256),
                    )))?;
                }
            }
        }
        if !batch.is_empty() {
            self.send(Event::Files(batch))?;
        }
        match fs::symlink_metadata(&dir) {
            Ok(after) if before.ino() == after.ino() => {}
            _ => self.send(Event::Error(format!(
                "{}: directory changed during scan",
                dir.display()
            )))?,
        }
        self.send(Event::Directory(count))
    }
    fn worker(&self) {
        loop {
            let mut queue = self.queue.0.lock().unwrap();
            while queue.pending.is_empty()
                && queue.outstanding > 0
                && !self.stop.load(Ordering::Relaxed)
            {
                queue = self.queue.1.wait(queue).unwrap();
            }
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            let Some(dir) = queue.pending.pop_front() else {
                return;
            };
            drop(queue);
            if let Err(e) = self.directory(dir) {
                let _ = self.send(Event::Error(e.to_string()));
            }
            let mut queue = self.queue.0.lock().unwrap();
            queue.outstanding -= 1;
            self.queue.1.notify_all();
        }
    }
}
/// Traverse without following links. The coordinator invokes `update` at least once a second,
/// including while waiting for workers; returning an error cancels and joins all workers.
/// `overview` retains inode identities and bucket IDs, never per-file paths.
pub fn walk(
    root: &Path,
    threads: usize,
    overview: bool,
    update: impl FnMut(&WalkProgress, &[String]) -> io::Result<()>,
) -> io::Result<WalkResult> {
    walk_impl(root, threads, overview, !overview, update, |_| Ok(()))
}
/// Stream bounded file batches to the coordinator's sink without retaining file objects.
pub fn walk_stream(
    root: &Path,
    threads: usize,
    update: impl FnMut(&WalkProgress, &[String]) -> io::Result<()>,
    sink: impl FnMut(&[FileEntry]) -> io::Result<()>,
) -> io::Result<WalkResult> {
    walk_impl(root, threads, false, false, update, sink)
}
fn walk_impl(
    root: &Path,
    threads: usize,
    overview: bool,
    retain_files: bool,
    mut update: impl FnMut(&WalkProgress, &[String]) -> io::Result<()>,
    mut sink: impl FnMut(&[FileEntry]) -> io::Result<()>,
) -> io::Result<WalkResult> {
    if !(1..=8).contains(&threads) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "threads must be 1..8",
        ));
    }
    update(&WalkProgress::default(), &[])?;
    let (tx, rx) = sync_channel(threads * 2);
    let walker = Arc::new(Walker {
        root,
        device: fs::metadata(root)?.dev(),
        queue: (
            Mutex::new(Queue {
                pending: VecDeque::from([root.to_owned()]),
                outstanding: 1,
            }),
            Condvar::new(),
        ),
        stop: AtomicBool::new(false),
        tx,
    });
    let mut inventory = Inventory::default();
    let mut progress = WalkProgress::default();
    // Bucket strings are interned. Each inode only retains stamp and a compact bucket index.
    let mut buckets = Vec::<PathBuf>::new();
    let mut bucket_ids = BTreeMap::<PathBuf, usize>::new();
    let mut inodes = std::collections::HashMap::<(u64, u64), (usize, u64, u64)>::new();
    let mut last = Instant::now();
    let mut failure = None;
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let walker = walker.clone();
            scope.spawn(move || walker.worker());
        }
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Event::Files(files)) => {
                    if let Err(e) = sink(&files) {
                        failure = Some(e);
                        walker.stop.store(true, Ordering::Relaxed);
                        walker.queue.1.notify_all();
                        break;
                    }
                    for file in files {
                        let bucket: PathBuf = file
                            .path
                            .parent()
                            .unwrap_or(Path::new(""))
                            .components()
                            .take(2)
                            .collect();
                        let id = *bucket_ids.entry(bucket.clone()).or_insert_with(|| {
                            buckets.push(bucket.clone());
                            buckets.len() - 1
                        });
                        progress
                            .directories
                            .entry(bucket.clone())
                            .or_default()
                            .files += 1;
                        progress.total.files += 1;
                        let key = (file.stamp.device, file.stamp.inode);
                        let mut add = true;
                        if file.stamp.links <= 1 {
                            progress.total.logical_bytes += file.stamp.bytes;
                            progress.total.allocated_bytes += file.stamp.allocated;
                        } else if let Some((old_id, logical, allocated)) = inodes.get_mut(&key) {
                            if *logical != file.stamp.bytes || *allocated != file.stamp.allocated {
                                inventory.errors.push(format!(
                                    "{}: hard link changed during scan",
                                    file.path.display()
                                ));
                            }
                            if bucket < buckets[*old_id] {
                                let old = progress.directories.get_mut(&buckets[*old_id]).unwrap();
                                old.logical_bytes -= *logical;
                                old.allocated_bytes -= *allocated;
                                *old_id = id;
                            } else {
                                add = false;
                            }
                        } else {
                            inodes.insert(key, (id, file.stamp.bytes, file.stamp.allocated));
                            progress.total.logical_bytes += file.stamp.bytes;
                            progress.total.allocated_bytes += file.stamp.allocated;
                        }
                        if add {
                            let usage = progress.directories.get_mut(&bucket).unwrap();
                            usage.logical_bytes += file.stamp.bytes;
                            usage.allocated_bytes += file.stamp.allocated;
                        }
                        if retain_files {
                            inventory.files.push(file);
                        }
                    }
                }
                Ok(Event::Link(path)) => {
                    if !overview {
                        inventory.symlinks.push(path);
                    }
                }
                Ok(Event::Error(error)) => inventory.errors.push(error),
                Ok(Event::Directory(entries)) => {
                    progress.entries += entries;
                    progress.completed_directories += 1;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if walker.queue.0.lock().unwrap().outstanding == 0 {
                        break;
                    }
                }
                Err(_) => break,
            }
            if last.elapsed() >= Duration::from_millis(100) {
                if let Err(e) = update(&progress, &inventory.errors) {
                    failure = Some(e);
                    walker.stop.store(true, Ordering::Relaxed);
                    walker.queue.1.notify_all();
                    break;
                }
                last = Instant::now();
            }
        }
        // Closing the channel unblocks senders before scoped workers are joined.
        drop(rx);
    });
    if let Some(e) = failure {
        return Err(e);
    }
    update(&progress, &inventory.errors)?;
    inventory.files.sort_by(|a, b| a.path.cmp(&b.path));
    inventory.symlinks.sort();
    inventory.errors.sort();
    inventory.errors.dedup();
    Ok(WalkResult {
        inventory,
        progress,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for i in 0..300 {
            let dir = root.path().join(format!("bucket-{i:03}/nested"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("invalid.json"),
                b"not JSON; overview never parses me",
            )
            .unwrap();
        }
        fs::hard_link(
            root.path().join("bucket-299/nested/invalid.json"),
            root.path().join("bucket-000/nested/link"),
        )
        .unwrap();
        std::os::unix::fs::symlink(root.path(), root.path().join("cycle")).unwrap();
        root
    }
    #[test]
    fn storage_parallel_overview_is_deterministic_and_retains_no_file_details() {
        let root = fixture();
        let one = walk(root.path(), 1, true, |_, _| Ok(())).unwrap();
        let four = walk(root.path(), 4, true, |_, _| Ok(())).unwrap();
        let details = walk(root.path(), 4, false, |_, _| Ok(())).unwrap();
        assert!(
            one.inventory.errors.is_empty(),
            "{:?}",
            one.inventory.errors
        );
        assert!(four.inventory.errors.is_empty());
        assert!(one.inventory.files.is_empty());
        assert!(one.inventory.symlinks.is_empty());
        assert_eq!(one.progress.directories, four.progress.directories);
        assert_eq!(one.progress.total, four.progress.total);
        assert_eq!(one.progress.total.files, 301);
        assert_eq!(one.progress.total.logical_bytes, 300 * 34);
        assert_eq!(details.inventory.files.len(), 301);
        assert_eq!(details.inventory.symlinks.len(), 1);
    }
    #[test]
    fn storage_parallel_cancellation_joins_blocked_workers() {
        let root = fixture();
        let start = Instant::now();
        let error = walk(root.path(), 4, false, |p, _| {
            if p.total.files > 0 {
                Err(io::Error::new(io::ErrorKind::Interrupted, "cancel"))
            } else {
                Ok(())
            }
        })
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(walk(root.path(), 0, true, |_, _| Ok(())).is_err());
        assert!(walk(root.path(), 9, true, |_, _| Ok(())).is_err());
    }
}
