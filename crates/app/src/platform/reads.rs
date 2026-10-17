//! Asynchronous read snapshots. Stale workspace/selection responses are discarded.
use super::*;
use analytics_workflow::views::{RunView, WorkspaceView};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
type Reply<T> = Receiver<Result<T, String>>;
#[derive(Default)]
pub(crate) struct ReadState {
    overview: Option<(PathBuf, Reply<WorkspaceView>)>,
    run: Option<(RunKey, Reply<RunView>)>,
    pub(super) current: Option<(RunKey, Result<RunView, String>)>,
}
#[derive(Clone, PartialEq, Eq)]
pub(super) struct RunKey(PathBuf, Uuid, String, Option<String>);
impl ReadState {
    pub(super) fn import(&mut self, root: PathBuf, path: PathBuf) {
        let (tx, rx) = channel();
        let worker_root = root.clone();
        std::thread::spawn(move || {
            let result = (|| {
                // Validate before connecting; malformed imports never start a service.
                let summary: Value = serde_json::from_slice(
                    &fs::read(path.join("summary.json")).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
                analytics_workflow::research::backtest::validate_result(&summary)
                    .map_err(|e| e.to_string())?;
                let client = analytics_workflow::service::Client::connect(&worker_root)
                    .map_err(|e| e.to_string())?;
                client.ensure(None).map_err(|e| e.to_string())?;
                client
                    .call(analytics_workflow::service::Rpc::ImportRun { path })
                    .map_err(|e| e.to_string())?;
                analytics_workflow::views::overview(&worker_root).map_err(|e| e.to_string())
            })();
            let _ = tx.send(result);
        });
        self.overview = Some((root, rx));
    }
    pub(super) fn overview(&mut self, root: PathBuf) {
        let (tx, rx) = channel();
        let path = root.clone();
        std::thread::spawn(move || {
            let _ = tx.send(analytics_workflow::views::overview(&path).map_err(|e| e.to_string()));
        });
        self.overview = Some((root, rx));
    }
    pub(super) fn select(&mut self, root: PathBuf, record: &RunRecord) {
        let key = RunKey(
            root.clone(),
            record.id,
            record.stage.clone(),
            record.finished_at.clone(),
        );
        if self.run.as_ref().is_some_and(|(k, _)| k == &key)
            || self.current.as_ref().is_some_and(|(k, _)| k == &key)
        {
            return;
        }
        let (tx, rx) = channel();
        let id = record.id;
        std::thread::spawn(move || {
            let _ = tx.send(analytics_workflow::views::run(&root, id).map_err(|e| e.to_string()));
        });
        self.current = None;
        self.run = Some((key, rx));
    }
}
pub(super) fn poll(state: &mut PlatformState, ctx: &egui::Context) {
    if let Some((root, rx)) = &state.reads.overview {
        match rx.try_recv() {
            Ok(result) => {
                if state.workspace.as_ref().is_some_and(|w| w.root() == root) {
                    match result {
                        Ok(view) => {
                            state.index_versions = view.index_versions;
                            state.datasets = view.datasets;
                            state.runs = view.runs;
                            if state.market.is_empty() {
                                state.market = state
                                    .datasets
                                    .iter()
                                    .find(|d| d.kind == "market")
                                    .map(|d| d.id.clone())
                                    .unwrap_or_default();
                            }
                            if state.actions.is_empty() {
                                state.actions = state
                                    .datasets
                                    .iter()
                                    .find(|d| d.kind == "actions")
                                    .map(|d| d.id.clone())
                                    .unwrap_or_default();
                            }
                        }
                        Err(e) => state.error = Some(e),
                    }
                }
                state.reads.overview = None;
            }
            Err(TryRecvError::Disconnected) => {
                state.reads.overview = None;
                state.error = Some("工作区读取线程已退出".into());
            }
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(50))
            }
        }
    }
    if let Some((key, rx)) = &state.reads.run {
        match rx.try_recv() {
            Ok(result) => {
                if state.workspace.as_ref().is_some_and(|w| w.root() == key.0)
                    && state.selected_run == Some(key.1)
                {
                    state.reads.current = Some((key.clone(), result));
                }
                state.reads.run = None;
            }
            Err(TryRecvError::Disconnected) => {
                state.reads.current = Some((key.clone(), Err("结果读取线程已退出".into())));
                state.reads.run = None;
            }
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(50))
            }
        }
    }
}
