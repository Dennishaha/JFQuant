//! Supervised, task-scoped Python worker. No interpreter or model handle enters Rust.
use super::{MlError, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
const VERSION: u32 = 2;
thread_local! {static SESSION:RefCell<Option<Session>>=const {RefCell::new(None)};}
/// Fingerprint of worker code and the exact bundled dependency lock.
pub fn runtime_signature() -> String {
    let mut h = Sha256::new();
    for (name, bytes) in [
        (
            "__init__.py",
            include_bytes!("../../research/jfquant_research/__init__.py").as_slice(),
        ),
        (
            "__main__.py",
            include_bytes!("../../research/jfquant_research/__main__.py").as_slice(),
        ),
        (
            "training.py",
            include_bytes!("../../research/jfquant_research/training.py").as_slice(),
        ),
        (
            "backtest.py",
            include_bytes!("../../research/jfquant_research/backtest.py").as_slice(),
        ),
    ] {
        h.update(name);
        h.update([0]);
        h.update(bytes);
        h.update([0]);
    }
    format!("sha256:{:x}", analytics_core::hash::Hex(h.finalize()))
}
/// Dependency receipt for cache and model isolation.
pub fn dependency_signature() -> String {
    let mut digest = Sha256::new();
    digest.update(include_bytes!(
        "../../../storage/collectors/requirements.lock"
    ));
    digest.update(include_bytes!(
        "../../../app/packaging/akquant-source.lock.json"
    ));
    format!("sha256:{:x}", analytics_core::hash::Hex(digest.finalize()))
}
/// Reuse one worker for all folds and final backtest; always retire it at scope exit.
pub fn scoped<T>(body: impl FnOnce() -> T) -> T {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            SESSION.with(|s| {
                s.borrow_mut().take();
            });
        }
    }
    let _guard = Guard;
    body()
}
/// Invoke a fixed built-in operation, retaining a worker until the task scope exits.
pub fn call(mut request: Value, cancel: Arc<AtomicBool>) -> Result<Value> {
    request["version"] = json!(VERSION);
    request["signature"] = json!(runtime_signature());
    SESSION.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Session::start(&cancel)?);
        }
        let s = slot.as_mut().unwrap();
        let result = s.call(&request, &cancel);
        if result.is_err() {
            slot.take();
        }
        result
    })
}
/// Task lifetime guard; tears down the worker after success, failure or unwind.
pub struct TaskSession;
impl Drop for TaskSession {
    fn drop(&mut self) {
        SESSION.with(|s| {
            s.borrow_mut().take();
        });
    }
}
struct Session {
    child: Child,
    input: ChildStdin,
    events: mpsc::Receiver<std::io::Result<Value>>,
    temp: PathBuf,
}
impl Session {
    fn start(cancel: &AtomicBool) -> Result<Self> {
        if cancel.load(Ordering::Relaxed) {
            return Err(MlError::Native("research cancelled".into()));
        }
        use std::os::unix::process::CommandExt;
        let python = analytics_storage::collector::runtime()?;
        let temp = std::env::temp_dir().join(format!("jfquant-research-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&temp)?;
        let stderr = fs::File::create(temp.join("stderr.log"))?;
        let mut child = Command::new(python)
            .args(["-I", "-B", "-m", "jfquant_research"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .process_group(0)
            .spawn()?;
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let value =
                    line.and_then(|l| serde_json::from_str(&l).map_err(std::io::Error::other));
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        let s = Self {
            child,
            input,
            events,
            temp,
        };
        let started = Instant::now();
        let event = loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(MlError::Native("research cancelled".into()));
            }
            match s.events.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => break event?,
                Err(mpsc::RecvTimeoutError::Timeout)
                    if started.elapsed() < Duration::from_secs(60) =>
                {
                    continue;
                }
                Err(e) => return Err(s.failure(&e.to_string())),
            }
        };
        if event["version"] != VERSION
            || event["event"] != "ready"
            || event["versions"]["akquant"] != "0.3.66"
            || event["versions"]["lightgbm"] != "4.7.0"
            || event["versions"]["worker_signature"] != runtime_signature()
            || event["versions"]["dependencies_sha256"] != dependency_signature()
            || event["versions"]["python"] != "3.12.15"
        {
            return Err(s.failure("runtime version/signature mismatch; rebuild bundled Python"));
        }
        Ok(s)
    }
    fn failure(&self, message: &str) -> MlError {
        MlError::Native(format!(
            "research_worker: {message}; {}",
            fs::read_to_string(self.temp.join("stderr.log"))
                .unwrap_or_default()
                .chars()
                .rev()
                .take(6000)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        ))
    }
    fn call(&mut self, request: &Value, cancel: &AtomicBool) -> Result<Value> {
        writeln!(self.input, "{request}")?;
        self.input.flush()?;
        let mut cancelling = None;
        loop {
            if cancel.load(Ordering::Relaxed) {
                if cancelling.is_none() {
                    writeln!(self.input, "{{\"operation\":\"cancel\"}}")?;
                    self.input.flush()?;
                    cancelling = Some(Instant::now());
                }
                if cancelling.is_some_and(|t| t.elapsed() > Duration::from_secs(2)) {
                    return Err(MlError::Native("research cancelled".into()));
                }
            }
            match self.events.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(e)) => {
                    if e["version"] != VERSION {
                        return Err(self.failure("invalid worker protocol"));
                    }
                    match e["event"].as_str() {
                        Some("progress") => {
                            analytics_core::task::TaskContext::current()
                                .report(e["message"].as_str().unwrap_or("research"))?;
                        }
                        Some("complete") => {
                            if cancelling.is_some() {
                                return Err(MlError::Native("research cancelled".into()));
                            }
                            return Ok(e["result"].clone());
                        }
                        Some("error") => {
                            return Err(
                                self.failure(e["message"].as_str().unwrap_or("worker error"))
                            );
                        }
                        _ => return Err(self.failure("unexpected worker event")),
                    }
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.failure("worker exited without a complete response"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = writeln!(
            self.input,
            "{{\"version\":1,\"operation\":\"shutdown\",\"signature\":\"{}\"}}",
            runtime_signature()
        );
        let _ = self.input.flush();
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                let _ = fs::remove_dir_all(&self.temp);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.temp);
    }
}
/// Write one cross-process feature matrix, retaining nulls as Arrow nulls.
pub(super) fn matrix(
    path: &Path,
    values: &[f64],
    labels: &[f32],
    rows: usize,
    columns: &[String],
) -> Result<()> {
    use arrow_array::{ArrayRef, Float64Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    if values.len() != rows * columns.len() || labels.len() != rows {
        return Err(MlError::InvalidData("matrix shape mismatch".into()));
    }
    let mut fields = Vec::new();
    let mut arrays = Vec::<ArrayRef>::new();
    for (i, name) in columns.iter().enumerate() {
        fields.push(Field::new(name, DataType::Float64, true));
        arrays.push(Arc::new(Float64Array::from(
            (0..rows)
                .map(|r| {
                    let v = values[r * columns.len() + i];
                    v.is_finite().then_some(v)
                })
                .collect::<Vec<_>>(),
        )));
    }
    fields.push(Field::new("__label", DataType::Float64, true));
    arrays.push(Arc::new(Float64Array::from(
        labels
            .iter()
            .map(|v| v.is_finite().then_some(*v as f64))
            .collect::<Vec<_>>(),
    )));
    let schema = Arc::new(Schema::new(fields));
    let batch =
        RecordBatch::try_new(schema.clone(), arrays).map_err(|e| MlError::Native(e.to_string()))?;
    let mut writer = parquet::arrow::ArrowWriter::try_new(fs::File::create(path)?, schema, None)
        .map_err(|e| MlError::Native(e.to_string()))?;
    writer
        .write(&batch)
        .map_err(|e| MlError::Native(e.to_string()))?;
    writer.close().map_err(|e| MlError::Native(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn running_python_training_cancels_and_a_fresh_task_can_train() {
        scoped(|| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("train.parquet");
            let values = (0..100).map(|v| v as f64).collect::<Vec<_>>();
            matrix(
                &path,
                &values,
                &values.iter().map(|v| *v as f32).collect::<Vec<_>>(),
                100,
                &["feature".into()],
            )
            .unwrap();
            let request = |iterations| json!({"operation":"train","train":path,"output":dir.path(),"request":{"factor_ids":["feature"],"objective":"Regression","parameters":{"num_iterations":iterations,"num_threads":1,"min_data_in_leaf":1},"early_stopping_rounds":null}});
            // Warm startup separately so the cancellation exercises native computation.
            let flag = Arc::new(AtomicBool::new(false));
            let session = Session::start(&flag).unwrap();
            let old_pid = session.child.id();
            SESSION.with(|s| s.replace(Some(session)));
            let cancel = flag.clone();
            let trigger = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                cancel.store(true, Ordering::Relaxed);
            });
            let start = Instant::now();
            assert!(call(request(100_000), flag).is_err());
            trigger.join().unwrap();
            assert!(start.elapsed() < Duration::from_secs(5));
            assert!(SESSION.with(|s| s.borrow().is_none()));
            call(request(2), Arc::new(AtomicBool::new(false))).unwrap();
            assert_ne!(
                SESSION.with(|s| s.borrow().as_ref().unwrap().child.id()),
                old_pid
            );
        });
    }
}
