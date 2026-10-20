//! Durable task execution and terminal event publication.
use super::*;
impl Worker {
    pub(super) fn run_task(
        &mut self,
        task: Uuid,
        kind: TaskKind,
        label: String,
        body: impl FnOnce(&mut Self) -> Result<(), String>,
    ) {
        if let Some(root) = &self.root
            && let Ok(bytes) = analytics_storage::evidence_store::read(
                root.join(format!(".jfquant/runs/{task}/run.json")),
            )
        {
            self.active_record = serde_json::from_slice(&bytes).ok();
            self.record_request = None;
        }
        if let (Some(root), Some(request)) = (&self.root, self.record_request.take()) {
            match crate::Workspace::open(root).and_then(|w| w.begin(task, &label, request)) {
                Ok(record) => self.active_record = Some(record),
                Err(e) => {
                    self.emit(WorkerEvent::Failed {
                        task,
                        kind,
                        message: e.to_string(),
                    });
                    self.active_cancel = None;
                    return;
                }
            }
        }
        self.emit(WorkerEvent::Started {
            task,
            kind,
            label: label.clone(),
        });
        let result = match catch_unwind(AssertUnwindSafe(|| body(self))) {
            Ok(result) => result,
            Err(payload) => {
                // Stop after unwinding: mutable/native state may be inconsistent.
                // Its handles are dropped here on their owning worker thread.
                let message = format!(
                    "后台任务发生内部错误：{}。请重启后台后重试。",
                    panic_message(payload.as_ref())
                );
                self.stop_reason = Some(message.clone());
                Err(message)
            }
        };
        if let Some(mut record) = self.active_record.take() {
            use crate::{RunStatus, WorkflowOutcome, Workspace};
            let persisted = (|| -> crate::Result<()> {
                let w = Workspace::open(self.root.as_ref().ok_or("workspace missing")?)?;
                let readiness = w.resolve(&record.output).join("execution_readiness.json");
                let blocked = analytics_storage::evidence_store::read(readiness)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                    .is_some_and(|v| v["ready"] == false);
                let status = if self
                    .active_cancel
                    .as_ref()
                    .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                {
                    RunStatus::Cancelled
                } else if blocked {
                    RunStatus::Blocked
                } else if result.is_err() {
                    RunStatus::Failed
                } else {
                    RunStatus::Succeeded
                };
                let outcome = match self.outcome.take() {
                    Some(outcome) => outcome,
                    None => WorkflowOutcome {
                        data: None,
                        details: if matches!(status, RunStatus::Succeeded | RunStatus::Blocked) {
                            w.results(&record)?
                        } else {
                            serde_json::Value::Null
                        },
                    },
                };
                let datasets = w.datasets()?;
                w.finish(
                    &mut record,
                    status,
                    result
                        .as_ref()
                        .err()
                        .cloned()
                        .or_else(|| blocked.then(|| "数据未就绪，请查看缺失项".into())),
                )?;
                let runs = w.runs()?;
                self.emit(WorkerEvent::WorkflowFinished {
                    task,
                    outcome,
                    record,
                    datasets,
                    runs,
                });
                Ok(())
            })();
            if let Err(e) = persisted {
                if let Some(root) = &self.root {
                    let _ = Workspace::open(root).and_then(|w| {
                        if let Some(mut current) = w.runs()?.into_iter().find(|r| r.id == task)
                            && current.status == RunStatus::Running
                        {
                            w.finish(&mut current, RunStatus::Failed, Some(e.to_string()))?;
                        }
                        Ok(())
                    });
                }
                self.emit(WorkerEvent::Failed {
                    task,
                    kind,
                    message: format!("运行记录写入失败：{e}"),
                });
                self.active_cancel = None;
                return;
            }
        }
        self.active_cancel = None;
        match result {
            Ok(()) => self.emit(WorkerEvent::Finished { task }),
            Err(message) => {
                self.emit(WorkerEvent::Failed {
                    task,
                    kind,
                    message,
                });
            }
        }
    }
}
