//! Storage rendering and stale-safe asynchronous metadata reads; no filesystem walk on UI thread.
use super::*;
use analytics_workflow::{
    service::{Client, Rpc},
    storage_management::{
        CleanupPlan, CleanupResult, ResourceCategory, StorageInventory, StorageRequest,
    },
};
use std::{
    collections::BTreeSet,
    sync::mpsc::{Receiver, TryRecvError, channel},
};
type SnapshotReply = Receiver<Result<(StorageInventory, Vec<CleanupResult>), String>>;
#[derive(Default)]
pub(crate) struct StorageTools {
    root: Option<PathBuf>,
    inventory: Option<StorageInventory>,
    history: Vec<CleanupResult>,
    read: Option<(PathBuf, SnapshotReply)>,
    selected: BTreeSet<String>,
    plan: Option<CleanupPlan>,
    confirmed: bool,
    search: String,
    task: String,
    before: String,
    category: Option<ResourceCategory>,
    status: usize,
    newest: bool,
    stale: bool,
    refresh: bool,
    error: Option<String>,
}
impl StorageTools {
    pub(crate) fn accept(&mut self, root: &Path, value: &Value) {
        if self.root.as_deref().is_some_and(|p| p != root) {
            return;
        }
        self.read = None;
        if value.get("scanned_at").is_some() {
            if let Ok(snapshot) = serde_json::from_value::<StorageInventory>(value.clone())
                && snapshot.workspace == root
            {
                self.root = Some(root.to_owned());
                self.inventory = Some(snapshot);
                self.stale = false;
                self.plan = None;
                self.confirmed = false;
            }
        } else if value.get("fingerprint").is_some() {
            if let Ok(plan) = serde_json::from_value::<CleanupPlan>(value.clone())
                && plan.workspace == root
                && plan
                    .resources
                    .iter()
                    .map(|r| r.id.clone())
                    .collect::<BTreeSet<_>>()
                    == self.selected
            {
                self.root = Some(root.to_owned());
                self.plan = Some(plan);
                self.confirmed = false;
            }
        } else if value.get("committed").is_some() {
            self.plan = None;
            self.confirmed = false;
            self.selected.clear();
            self.stale = true;
            self.refresh = true;
        } else {
            self.stale = true;
            self.plan = None;
            self.confirmed = false;
        }
    }
    fn request_read(&mut self, root: PathBuf) {
        let (tx, rx) = channel();
        let path = root.clone();
        std::thread::spawn(move || {
            let result = (|| {
                let client = Client::connect(&path).map_err(|e| e.to_string())?;
                client.ensure(None).map_err(|e| e.to_string())?;
                let snapshot = serde_json::from_value(
                    client
                        .call(Rpc::StorageInventory)
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
                let history = serde_json::from_value(
                    client
                        .call(Rpc::StorageHistory)
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
                Ok((snapshot, history))
            })();
            let _ = tx.send(result);
        });
        self.read = Some((root, rx));
    }
    fn poll(&mut self, root: &Path, ctx: &egui::Context) {
        let Some((key, rx)) = &self.read else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                if key == root {
                    match result {
                        Ok((snapshot, history)) if snapshot.workspace == root => {
                            self.inventory = Some(snapshot);
                            self.history = history;
                            self.error = None;
                        }
                        Ok(_) => {}
                        Err(e) => self.error = Some(e),
                    }
                }
                self.read = None;
            }
            Err(TryRecvError::Disconnected) => {
                self.read = None;
                self.error = Some("存储快照读取未完成".into());
            }
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100))
            }
        }
    }
}
fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut n = n as f64;
    let mut i = 0;
    while n >= 1024.0 && i < 4 {
        n /= 1024.0;
        i += 1;
    }
    format!("{n:.1} {}", UNITS[i])
}
pub fn storage_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
) {
    ui.heading("存储管理");
    let Some(root) = state
        .platform
        .workspace
        .as_ref()
        .map(|w| w.root().to_owned())
    else {
        ui.label("先选择工作区。");
        return;
    };
    let enabled = !state.busy() && state.worker_error.is_none();
    let tools = &mut state.platform.storage;
    if tools.root.as_ref() != Some(&root) {
        *tools = StorageTools {
            root: Some(root.clone()),
            ..Default::default()
        };
    }
    tools.poll(&root, ui.ctx());
    if tools.refresh && enabled && tools.read.is_none() {
        tools.refresh = false;
        tools.request_read(root.clone());
    }
    let mut request = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(enabled, egui::Button::new("扫描工作区空间"))
            .clicked()
        {
            request = Some(StorageRequest::Scan);
            tools.error = None;
        }
        if ui
            .add_enabled(
                enabled && tools.read.is_none(),
                egui::Button::new("读取上次扫描及清理记录"),
            )
            .clicked()
        {
            tools.request_read(root.clone());
        }
        if tools.read.is_some() {
            ui.spinner();
        }
    });
    ui.label(
        "仅管理当前工作区。磁盘占用和可释放空间为估计，清理完成后显示实际删除容量及磁盘空间变化。",
    );
    ui.label("资源明细可能包含共享材料，不能直接相加；分类合计与清理预览会去重。");
    if let Some(error) = &tools.error {
        ui.colored_label(crate::theme::semantic(ui.ctx()).error, error);
    }
    if let Some(inventory) = &tools.inventory {
        overview(ui, inventory, tools.stale);
        filters(ui, tools);
        let changed = resource_rows(ui, tools, enabled);
        let inventory = tools.inventory.as_ref().unwrap();
        if changed {
            tools.plan = None;
            tools.confirmed = false;
        }
        if ui
            .add_enabled(
                enabled && !tools.selected.is_empty() && inventory.complete,
                egui::Button::new("预览所选资源清理"),
            )
            .clicked()
        {
            request = Some(StorageRequest::Plan {
                resources: tools.selected.iter().cloned().collect(),
            });
        }
        ui.collapsing("工作区外引用（不计入占用、不删除）", |ui| {
            for path in &inventory.external_references {
                ui.label(path.display().to_string());
            }
        });
    } else {
        ui.label("首次使用请扫描工作区，或读取上次完成的扫描。");
    }
    if let Some(next) = cleanup_preview(ui, tools, enabled) {
        request = Some(next);
    }
    if let Some(next) = cleanup_receipts(ui, tools, enabled) {
        request = Some(next);
    }
    if let Some(request) = request {
        submit(
            state,
            worker,
            cancel_slot,
            WorkflowRequest::Storage { request },
        );
    }
}
fn resource_rows(ui: &mut egui::Ui, tools: &mut StorageTools, enabled: bool) -> bool {
    let inventory = tools.inventory.as_ref().unwrap();
    let mut changed = false;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(enabled, egui::Button::new("选择可重建因子缓存"))
            .clicked()
        {
            tools.selected = inventory
                .resources
                .iter()
                .filter(|r| {
                    r.category == ResourceCategory::FactorCache && r.deletable && !r.released
                })
                .map(|r| r.id.clone())
                .collect();
            changed = true;
        }
        if ui
            .add_enabled(enabled, egui::Button::new("清空选择"))
            .clicked()
        {
            tools.selected.clear();
            changed = true;
        }
        ui.label(format!("已选 {} 项", tools.selected.len()));
    });
    let mut rows: Vec<_> = inventory
        .resources
        .iter()
        .filter(|r| {
            tools.category.is_none_or(|c| r.category == c)
                && format!("{} {}", r.name, r.path.display()).contains(&tools.search)
                && (tools.task.is_empty()
                    || r.task
                        .is_some_and(|id| id.to_string().contains(&tools.task)))
                && (tools.before.is_empty()
                    || r.modified_at
                        .get(..10)
                        .is_some_and(|d| d < tools.before.as_str()))
                && match tools.status {
                    1 => r.archived,
                    2 => r.temporary,
                    3 => r.released,
                    4 => r.deletable,
                    5 => !r.deletable && !r.released,
                    _ => true,
                }
        })
        .collect();
    if tools.newest {
        rows.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    } else {
        rows.sort_by_key(|r| std::cmp::Reverse(r.usage.allocated_bytes));
    }
    egui::ScrollArea::vertical()
        .id_salt("storage_resources")
        .max_height(360.0)
        .show(ui, |ui| {
            for r in rows {
                ui.horizontal(|ui| {
                    let mut selected = tools.selected.contains(&r.id);
                    if ui
                        .add_enabled(
                            enabled && r.deletable && !r.released,
                            egui::Checkbox::without_text(&mut selected),
                        )
                        .changed()
                    {
                        if selected {
                            tools.selected.insert(r.id.clone());
                        } else {
                            tools.selected.remove(&r.id);
                        }
                        changed = true;
                    }
                    ui.label(format!(
                        "{} · {} · {}{}",
                        r.name,
                        r.category.label(),
                        bytes(r.usage.allocated_bytes),
                        if r.released {
                            " · 已释放"
                        } else if r.temporary {
                            " · 临时文件"
                        } else if r.archived {
                            " · 历史归档"
                        } else {
                            ""
                        }
                    ));
                });
                ui.collapsing(format!("{} · 详情", r.path.display()), |ui| {
                    ui.label(format!(
                        "修改时间：{} · 覆盖：{} — {}",
                        r.modified_at,
                        r.start.as_deref().unwrap_or("未知"),
                        r.end.as_deref().unwrap_or("未知")
                    ));
                    for reason in &r.reasons {
                        ui.label(reason);
                    }
                    for edge in inventory
                        .references
                        .iter()
                        .filter(|e| e.target == r.id || e.source == r.id)
                    {
                        ui.label(format!(
                            "{:?}：{} → {} · {}",
                            edge.kind, edge.source, edge.target, edge.reason
                        ));
                    }
                });
            }
        });
    changed
}
fn overview(ui: &mut egui::Ui, inventory: &StorageInventory, stale: bool) {
    ui.horizontal_wrapped(|ui| {
        ui.strong(format!(
            "工作区占用 {}",
            bytes(inventory.total.allocated_bytes)
        ));
        ui.label(format!("逻辑大小 {}", bytes(inventory.total.logical_bytes)));
        ui.label(format!(
            "磁盘可用 {}",
            inventory
                .available_bytes
                .map(bytes)
                .unwrap_or_else(|| "未知".into())
        ));
        ui.label(format!(
            "可清理估计 {}",
            bytes(inventory.reclaimable_allocated_bytes_estimate)
        ));
    });
    ui.label(format!(
        "最近扫描 {}{}",
        inventory.scanned_at,
        if stale {
            " · 工作区已有变化，请重新扫描"
        } else {
            ""
        }
    ));
    if !inventory.complete {
        ui.colored_label(
            crate::theme::semantic(ui.ctx()).warning,
            "扫描不完整，当前禁止清理",
        );
        for e in &inventory.diagnostics {
            ui.label(e);
        }
    }
    egui::Grid::new("storage_categories")
        .striped(true)
        .show(ui, |ui| {
            for (category, usage) in &inventory.categories {
                ui.label(category.label());
                ui.label(bytes(usage.allocated_bytes));
                ui.add(
                    egui::ProgressBar::new(
                        (usage.allocated_bytes as f64
                            / inventory.total.allocated_bytes.max(1) as f64)
                            as f32,
                    )
                    .desired_width(160.0),
                );
                ui.end_row();
            }
        });
    ui.separator();
}
fn cleanup_preview(
    ui: &mut egui::Ui,
    tools: &mut StorageTools,
    enabled: bool,
) -> Option<StorageRequest> {
    let mut request = None;
    if let Some(plan) = &tools.plan {
        ui.separator();
        ui.heading("清理预览");
        ui.label(format!(
            "{} 项资源，{} 个文件；预计释放 {}",
            plan.resources.len(),
            plan.files.len(),
            bytes(plan.estimated_release.allocated_bytes)
        ));
        for blocker in &plan.blockers {
            ui.colored_label(crate::theme::semantic(ui.ctx()).error, blocker);
        }
        for impact in &plan.impacts {
            ui.label(impact);
        }
        ui.collapsing("将永久删除的文件", |ui| {
            egui::ScrollArea::vertical().max_height(200.0).show_rows(
                ui,
                18.0,
                plan.files.len(),
                |ui, range| {
                    for file in &plan.files[range] {
                        ui.label(file.path.display().to_string());
                    }
                },
            );
        });
        ui.checkbox(
            &mut tools.confirmed,
            "我确认永久删除上述内容，无法恢复；保留的任务摘要与曲线不受影响。",
        );
        ui.label("提交前可以取消；提交后会完成已确认的永久删除。");
        if ui
            .add_enabled(
                enabled && tools.confirmed && plan.blockers.is_empty(),
                egui::Button::new("永久释放所选资源"),
            )
            .clicked()
        {
            request = Some(StorageRequest::Apply { plan: plan.id });
            tools.confirmed = false;
        }
    }
    request
}
fn cleanup_receipts(
    ui: &mut egui::Ui,
    tools: &StorageTools,
    enabled: bool,
) -> Option<StorageRequest> {
    let mut request = None;
    ui.separator();
    ui.heading("清理记录");
    for item in &tools.history {
        ui.group(|ui| {
            ui.label(format!(
                "{} · {} · 已删除 {}",
                item.started_at,
                item.status,
                bytes(item.deleted.logical_bytes)
            ));
            if let (Some(before), Some(after)) = (item.available_before, item.available_after) {
                ui.label(format!("磁盘可用：{} → {}", bytes(before), bytes(after)));
            }
            for (path, error) in &item.errors {
                ui.label(format!("{}：{error}", path.display()));
            }
            if item.committed
                && item.completed_at.is_none()
                && ui
                    .add_enabled(enabled, egui::Button::new("重试剩余删除"))
                    .clicked()
            {
                request = Some(StorageRequest::Apply { plan: item.plan });
            }
        });
    }
    request
}
fn filters(ui: &mut egui::Ui, tools: &mut StorageTools) {
    ui.horizontal_wrapped(|ui| {
        ui.label("搜索");
        ui.text_edit_singleline(&mut tools.search);
        egui::ComboBox::from_id_salt("storage_category")
            .selected_text(
                tools
                    .category
                    .map(ResourceCategory::label)
                    .unwrap_or("全部类型"),
            )
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut tools.category, None, "全部类型");
                for c in [
                    ResourceCategory::Market,
                    ResourceCategory::Evidence,
                    ResourceCategory::FactorCache,
                    ResourceCategory::Models,
                    ResourceCategory::Predictions,
                    ResourceCategory::Backtest,
                    ResourceCategory::Metadata,
                    ResourceCategory::Other,
                ] {
                    ui.selectable_value(&mut tools.category, Some(c), c.label());
                }
            });
        let labels = [
            "全部状态",
            "历史归档",
            "临时文件",
            "已释放",
            "可清理",
            "受保护",
        ];
        egui::ComboBox::from_id_salt("storage_status")
            .selected_text(labels[tools.status])
            .show_ui(ui, |ui| {
                for (i, label) in labels.iter().enumerate() {
                    ui.selectable_value(&mut tools.status, i, *label);
                }
            });
        ui.checkbox(&mut tools.newest, "按修改时间排序");
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("关联任务");
        ui.text_edit_singleline(&mut tools.task);
        ui.label("修改日期早于 YYYY-MM-DD");
        ui.text_edit_singleline(&mut tools.before);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn storage_preview_from_previous_selection_is_discarded() {
        let temp = tempfile::tempdir().unwrap();
        let w = analytics_workflow::Workspace::open(temp.path()).unwrap();
        let cache = w.resolve(".jfquant/runs/.factor-shards-v1/selection/2020");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("features.parquet"), b"fixture").unwrap();
        std::fs::write(
            cache.join("manifest.json"),
            serde_json::json!({"schema_version":1,"features":["fixture"],"signature":"selection","sha256":"fixture"}).to_string(),
        ).unwrap();
        let inventory =
            analytics_workflow::storage_management::scan(&w, &Default::default()).unwrap();
        let id = inventory
            .resources
            .iter()
            .find(|r| r.category == ResourceCategory::FactorCache)
            .unwrap()
            .id
            .clone();
        let plan = analytics_workflow::storage_management::plan(
            &w,
            std::slice::from_ref(&id),
            &Default::default(),
        )
        .unwrap();
        let mut tools = StorageTools {
            root: Some(w.root().to_owned()),
            selected: BTreeSet::from([id]),
            ..Default::default()
        };
        let value = serde_json::to_value(plan).unwrap();
        tools.accept(w.root(), &value);
        assert!(tools.plan.is_some());
        tools.selected.clear();
        tools.plan = None;
        tools.accept(w.root(), &value);
        assert!(tools.plan.is_none());
        assert!(!tools.confirmed);
    }
    #[test]
    fn storage_snapshots_from_old_workspace_do_not_replace_new_selection() {
        let mut tools = StorageTools {
            root: Some(PathBuf::from("/new")),
            confirmed: true,
            ..Default::default()
        };
        tools.accept(Path::new("/old"), &serde_json::json!({"committed":true}));
        assert!(tools.confirmed);
        assert!(!tools.refresh);
    }
}
