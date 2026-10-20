//! A compact, throttled snapshot. No file list is serialized by the heartbeat.
use super::*;
use analytics_storage::filesystem::WalkProgress;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
struct State {
    snapshot: ScanSnapshot,
    error: Option<String>,
}
pub(crate) struct Reporter {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
    started: Instant,
}
impl Reporter {
    pub fn new(w: &Workspace, context: &TaskContext, level: ScanLevel) -> Result<Self> {
        let path = owned_path(w, Path::new(".jfquant/storage/snapshot.json"))?;
        let snapshot = ScanSnapshot {
            schema_version: 1,
            workspace: w.root().to_owned(),
            scan_id: context.id,
            level,
            phase: "准备扫描".into(),
            started_at: crate::now(),
            updated_at: crate::now(),
            elapsed_ms: 0,
            finished: false,
            complete: false,
            diagnostics: vec![],
            progress: WalkProgress::default(),
        };
        atomic_json(&path, &snapshot)?;
        let state = Arc::new(Mutex::new(State {
            snapshot,
            error: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let state = state.clone();
            let stop = stop.clone();
            let context = context.clone();
            let path = path.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                let mut last = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                    if last.elapsed() < Duration::from_secs(1) {
                        continue;
                    }
                    let mut guard = state.lock().unwrap();
                    guard.snapshot.updated_at = crate::now();
                    guard.snapshot.elapsed_ms = started.elapsed().as_millis() as u64;
                    let message = format!(
                        "{} · {} 个文件 · {} 个目录已完成 · {:.1} 秒",
                        guard.snapshot.phase,
                        guard.snapshot.progress.total.files,
                        guard.snapshot.progress.completed_directories,
                        started.elapsed().as_secs_f64()
                    );
                    drop(guard);
                    let report = context.report(message);
                    let mut state = state.lock().unwrap();
                    let result = report.map_err(|e| e.to_string()).and_then(|_| {
                        atomic_json(&path, &state.snapshot).map_err(|e| e.to_string())
                    });
                    if let Err(e) = result {
                        state.error = Some(e);
                        break;
                    }
                    last = Instant::now();
                }
            })
        };
        Ok(Self {
            state,
            stop,
            worker: Some(worker),
            path,
            started: Instant::now(),
        })
    }
    pub fn phase(&self, phase: &str) {
        self.state.lock().unwrap().snapshot.phase = phase.into();
    }
    pub fn update(&self, progress: &WalkProgress, diagnostics: &[String]) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if let Some(e) = &state.error {
            return Err(std::io::Error::other(e.clone()));
        }
        state.snapshot.progress = progress.clone();
        state.snapshot.diagnostics = diagnostics.to_vec();
        Ok(())
    }
    fn join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
    pub fn finish(mut self, inventory: &StorageInventory) -> Result<()> {
        self.join();
        let mut state = self.state.lock().unwrap();
        if let Some(e) = &state.error {
            return Err(e.clone().into());
        }
        state.snapshot.finished = true;
        state.snapshot.elapsed_ms = self.started.elapsed().as_millis() as u64;
        state.snapshot.complete = inventory.complete;
        state.snapshot.phase = "扫描完成".into();
        state.snapshot.updated_at = crate::now();
        state.snapshot.diagnostics = inventory.diagnostics.clone();
        atomic_json(&self.path, &state.snapshot)
    }
}
impl Drop for Reporter {
    fn drop(&mut self) {
        self.join();
        let mut state = self.state.lock().unwrap();
        if !state.snapshot.finished {
            state.snapshot.finished = true;
            state.snapshot.complete = false;
            state.snapshot.phase = "扫描中止，结果不完整".into();
            state.snapshot.updated_at = crate::now();
            if let Some(error) = state.error.clone() {
                state.snapshot.diagnostics.push(error);
            }
            let _ = atomic_json(&self.path, &state.snapshot);
        }
    }
}

/// Task-only heartbeat for long evidence verification, object I/O and export phases.
/// It reports elapsed time without inventing scan counts or completion percentages.
pub(crate) struct Activity {
    phase: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Activity {
    pub fn new(context: &TaskContext, phase: &str) -> Self {
        let phase = Arc::new(Mutex::new(phase.to_owned()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let phase = phase.clone();
            let stop = stop.clone();
            let context = context.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                let mut last = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                    if last.elapsed() < Duration::from_secs(1) {
                        continue;
                    }
                    if context
                        .report(format!(
                            "{} · {:.1} 秒",
                            phase.lock().unwrap(),
                            started.elapsed().as_secs_f64()
                        ))
                        .is_err()
                    {
                        break;
                    }
                    last = Instant::now();
                }
            })
        };
        Self {
            phase,
            stop,
            worker: Some(worker),
        }
    }
    pub fn phase(&self, phase: &str) {
        *self.phase.lock().unwrap() = phase.to_owned();
    }
}
impl Drop for Activity {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
