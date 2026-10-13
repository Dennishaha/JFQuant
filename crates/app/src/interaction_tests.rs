//! Headless tests drive real egui widgets with pointer events and inspect commands.
use super::*;
use egui::{Event, PointerButton, Pos2, RawInput, Rect, Vec2, epaint::Shape};
use std::sync::{
    atomic::Ordering,
    mpsc::{Receiver, channel},
};
use uuid::Uuid;
use worker::{TaskKind, WorkerCommand};

struct Harness {
    app: DesktopApp,
    ctx: egui::Context,
    commands: Receiver<WorkerCommand>,
    events: std::sync::mpsc::Sender<WorkerEvent>,
}

impl Harness {
    fn new(panel: Panel) -> Self {
        let (worker, commands) = WorkerHandle::recording();
        let (events, rx) = channel();
        let ctx = egui::Context::default();
        theme::install(&ctx);
        Self {
            app: DesktopApp {
                state: AppState {
                    panel,
                    ..AppState::default()
                },
                worker,
                events: rx,
                cancel: None,
                registry: FactorRegistry::default(),
            },
            ctx,
            commands,
            events,
        }
    }

    fn frame(&mut self, events: Vec<Event>) -> egui::FullOutput {
        self.app.drain_events();
        let mut output = self.ctx.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 1200.0))),
                events,
                ..RawInput::default()
            },
            |ui| {
                self.app.top_bar(ui);
                self.app.nav_rail(ui);
                self.app.status_bar(ui);
                egui::CentralPanel::default().show(ui, |ui| {
                    panels::render(
                        ui,
                        &mut self.app.state,
                        &self.app.worker,
                        &mut self.app.cancel,
                        &self.app.registry,
                    );
                });
            },
        );
        // No GPU renderer in the harness; explicitly discard texture uploads.
        output.textures_delta.clear();
        output
    }

    fn click(&mut self, label: &str) {
        self.frame(vec![]);
        let output = self.frame(vec![]);
        fn locate(shape: &Shape, label: &str) -> Option<Pos2> {
            match shape {
                Shape::Text(text) if text.galley.text() == label => {
                    Some(text.pos + text.galley.rect.center().to_vec2())
                }
                Shape::Vec(shapes) => shapes.iter().find_map(|shape| locate(shape, label)),
                _ => None,
            }
        }
        let mut pos = output
            .shapes
            .iter()
            .rev()
            .find_map(|shape| locate(&shape.shape, label))
            .unwrap_or_else(|| panic!("widget text not rendered: {label}"));
        if label.starts_with("alpha_") {
            pos.x -= 15.0;
        }
        self.frame(vec![
            Event::PointerMoved(pos),
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
        ]);
        self.frame(vec![Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        }]);
    }

    fn busy(&mut self) {
        apply_event(
            &mut self.app.state,
            &WorkerEvent::Started {
                task: Uuid::new_v4(),
                kind: TaskKind::Training,
                label: "测试任务".into(),
            },
        );
    }

    fn wait_for(&mut self, ready: impl Fn(&DesktopApp) -> bool) -> egui::FullOutput {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let output = self.frame(vec![]);
            if ready(&self.app) {
                return output;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not report its result"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn finish_recorded_task(&mut self) {
        let task = self.app.state.active_task.as_ref().unwrap().id;
        self.events.send(WorkerEvent::Finished { task }).unwrap();
        self.frame(vec![]);
    }
}

#[test]
fn worker_panic_shows_error_and_restart_button_restores_real_queries() {
    let mut h = Harness::new(Panel::Query);
    (h.app.worker, h.app.events) = worker::spawn(h.ctx.clone());
    h.app.cancel = Some(analytics_ml::pipeline::cancellation());
    h.app
        .worker
        .send(WorkerCommand::PanicTask {
            task: Uuid::new_v4(),
            kind: TaskKind::Query,
        })
        .unwrap();
    let output = h.wait_for(|app| app.state.worker_error.is_some());
    let text = rendered_text(&output);
    assert!(text.contains("后台不可用"), "{text}");
    assert!(text.contains("模拟后台内部错误"), "{text}");
    assert!(!h.app.state.busy());
    assert!(h.app.cancel.is_none());
    h.click("重启后台");
    assert!(h.app.state.worker_error.is_none());
    assert!(h.app.state.query.next_cursor.is_none());
    assert!(h.app.state.query.submitted.is_none());
    h.app.state.query.sql = "SELECT 7 AS recovered".into();
    h.click("执行查询");
    h.wait_for(|app| !app.state.busy() && !app.state.query.rows.is_empty());
    assert!(h.app.state.query.error.is_none());
    assert_eq!(
        h.app.state.query.rows[0][0],
        analytics_core::ScalarValue::Int64(7)
    );
}

#[test]
fn event_channel_disconnect_clears_active_task_and_cancel_without_repeated_logs() {
    for kind in [
        TaskKind::Scan,
        TaskKind::Describe,
        TaskKind::Query,
        TaskKind::Export,
        TaskKind::Pipeline,
        TaskKind::Training,
    ] {
        let mut h = Harness::new(Panel::Query);
        h.events
            .send(WorkerEvent::Started {
                task: Uuid::new_v4(),
                kind,
                label: "正在运行".into(),
            })
            .unwrap();
        h.app.cancel = Some(analytics_ml::pipeline::cancellation());
        drop(std::mem::replace(&mut h.events, channel().0));
        let text = rendered_text(&h.frame(vec![]));
        assert!(text.contains("后台连接已断开"));
        assert!(!h.app.state.busy());
        assert!(h.app.cancel.is_none());
        let error = match kind {
            TaskKind::Scan => &h.app.state.dataset.error,
            TaskKind::Describe | TaskKind::Query | TaskKind::Export => &h.app.state.query.error,
            TaskKind::Pipeline => &h.app.state.factor.error,
            TaskKind::Training => &h.app.state.model.error,
        };
        assert!(error.as_ref().unwrap().contains("后台连接已断开"));
        let log_count = h.app.state.messages.len();
        h.frame(vec![]);
        h.frame(vec![]);
        assert_eq!(h.app.state.messages.len(), log_count);
    }
}

#[test]
fn closed_command_channel_reports_error_for_all_panels_and_disables_submission() {
    for (panel, button) in [
        (Panel::Dataset, "扫描数据集"),
        (Panel::Query, "执行查询"),
        (Panel::Factor, "运行因子流水线"),
        (Panel::Model, "开始训练"),
    ] {
        let mut h = Harness::new(panel);
        h.app.state.model.cache_path = "factors.parquet".into();
        drop(std::mem::replace(&mut h.commands, channel().1));
        h.click(button);
        assert!(
            h.app
                .state
                .worker_error
                .as_ref()
                .unwrap()
                .contains("后台线程已退出")
        );
        assert!(!h.app.state.busy());
        assert!(h.app.cancel.is_none());
        assert!(rendered_text(&h.frame(vec![])).contains("后台不可用"));
        let log_count = h.app.state.messages.len();
        h.click(button);
        assert_eq!(h.app.state.messages.len(), log_count);
    }
}

#[test]
fn unavailable_worker_disables_all_query_actions_and_old_events_after_restart_are_ignored() {
    let mut h = Harness::new(Panel::Query);
    h.app.state.query.columns = vec![analytics_core::ColumnSchema {
        name: "id".into(),
        data_type: analytics_core::DataType::Int64,
        nullable: false,
    }];
    h.app.state.query.sort_column = Some("id".into());
    h.app.state.query.next_cursor = Some(analytics_core::KeysetCursor(vec![
        analytics_core::ScalarValue::Int64(1),
    ]));
    h.app.state.query.submitted = Some(h.app.state.query.first_page_request().unwrap());
    h.events
        .send(WorkerEvent::Unavailable {
            message: "测试连接中断".into(),
        })
        .unwrap();
    h.events
        .send(WorkerEvent::Started {
            task: Uuid::new_v4(),
            kind: TaskKind::Query,
            label: "迟到的启动事件".into(),
        })
        .unwrap();
    for label in ["执行查询", "Describe", "导出", "下一页"] {
        h.click(label);
        assert!(h.commands.try_recv().is_err());
        assert!(!h.app.state.busy());
    }
    h.click("重启后台");
    assert!(h.app.state.worker_error.is_none());
    assert!(
        h.events
            .send(WorkerEvent::Unavailable {
                message: "旧后台事件".into()
            })
            .is_err()
    );
    h.frame(vec![]);
    assert!(h.app.state.worker_error.is_none());
}

#[test]
fn navigation_switches_all_four_workbenches() {
    let mut h = Harness::new(Panel::Dataset);
    for (label, panel) in [
        ("查询工作台", Panel::Query),
        ("因子工作台", Panel::Factor),
        ("建模", Panel::Model),
        ("数据管理", Panel::Dataset),
    ] {
        h.click(label);
        assert_eq!(h.app.state.panel, panel);
    }
    assert!(h.commands.try_recv().is_err());
}

#[test]
fn log_drawer_opens_and_closes() {
    let mut h = Harness::new(Panel::Dataset);
    h.click("消息日志");
    assert!(h.app.state.log_open);
    h.click("收起日志");
    assert!(!h.app.state.log_open);
}

#[test]
fn dataset_scan_sends_trimmed_path_and_rejects_blank() {
    let mut h = Harness::new(Panel::Dataset);
    h.app.state.dataset.root = "  fixtures/market  ".into();
    h.click("扫描数据集");
    match h.commands.try_recv().unwrap() {
        WorkerCommand::ScanDataset { root, .. } => {
            assert_eq!(root.to_str(), Some("fixtures/market"))
        }
        command => panic!("unexpected command: {command:?}"),
    }
    h.app.state.dataset.root = "  ".into();
    h.finish_recorded_task();
    h.click("扫描数据集");
    assert_eq!(h.app.state.dataset.error.as_deref(), Some("路径不能为空"));
    assert!(h.commands.try_recv().is_err());
}

#[test]
fn query_execute_resets_page_and_describe_normalizes_sql() {
    let mut h = Harness::new(Panel::Query);
    h.app.state.query.sql = " SELECT 42 AS answer; ".into();
    h.app.state.query.page = 3;
    h.click("执行查询");
    assert_eq!(h.app.state.query.page, 0);
    match h.commands.try_recv().unwrap() {
        WorkerCommand::RunQuery { request, .. } => assert_eq!(request.sql, "SELECT 42 AS answer"),
        command => panic!("unexpected: {command:?}"),
    }
    h.finish_recorded_task();
    h.click("Describe");
    assert!(
        matches!(h.commands.try_recv().unwrap(), WorkerCommand::DescribeQuery { sql, .. }
        if sql == "SELECT 42 AS answer")
    );
}

#[test]
fn query_buttons_reject_blank_sql() {
    let mut h = Harness::new(Panel::Query);
    h.app.state.query.sql.clear();
    for label in ["执行查询", "Describe", "导出"] {
        h.click(label);
        assert_eq!(h.app.state.query.error.as_deref(), Some("SQL 不能为空"));
        assert!(h.commands.try_recv().is_err());
    }
}

#[test]
fn default_query_pages_over_five_hundred_identical_rows_without_exposing_ordinal() {
    let mut h = Harness::new(Panel::Query);
    (h.app.worker, h.app.events) = worker::spawn(h.ctx.clone());
    h.app.state.query.sql = "SELECT 1 AS value FROM range(600)".into();
    h.click("执行查询");
    h.wait_for(|app| !app.state.busy() && app.state.query.rows.len() == 500);
    assert_eq!(h.app.state.query.columns.len(), 1);
    assert_eq!(h.app.state.query.columns[0].name, "value");
    assert!(h.app.state.query.sort_column.is_none());
    assert!(h.app.state.query.next_page_request().is_some());
    h.click("下一页");
    h.wait_for(|app| !app.state.busy() && app.state.query.rows.len() == 600);
    assert_eq!(h.app.state.query.page, 2);
    assert!(h.app.state.query.next_cursor.is_none());
    assert!(
        h.app
            .state
            .query
            .rows
            .iter()
            .all(|row| row == &vec![analytics_core::ScalarValue::Int64(1)])
    );
}

#[test]
fn changed_query_options_disable_old_cursor_and_failed_new_query_clears_old_results() {
    let mut h = Harness::new(Panel::Query);
    (h.app.worker, h.app.events) = worker::spawn(h.ctx.clone());
    h.app.state.query.sql = "SELECT range AS id FROM range(600)".into();
    h.click("执行查询");
    h.wait_for(|app| !app.state.busy() && app.state.query.rows.len() == 500);
    h.click("降序");
    assert!(h.app.state.query.next_page_request().is_some());
    h.click("降序");
    // Descending is irrelevant until an explicit sort column is selected.
    h.click("（未选择）");
    h.click("id");
    assert!(h.app.state.query.next_page_request().is_none());
    h.app.state.query.sort_column = None;
    assert!(h.app.state.query.next_page_request().is_some());
    h.app.state.query.limit = 100;
    assert!(h.app.state.query.next_page_request().is_none());
    h.app.state.query.limit = 500;
    h.app.state.query.sql = "SELECT FROM".into();
    assert!(h.app.state.query.next_page_request().is_none());
    h.click("执行查询");
    h.wait_for(|app| !app.state.busy() && app.state.query.error.is_some());
    assert!(h.app.state.query.rows.is_empty());
    assert!(h.app.state.query.next_cursor.is_none());
}

#[test]
fn submission_disables_repeat_clicks_before_worker_start_event() {
    let mut h = Harness::new(Panel::Query);
    h.click("执行查询");
    let command = h.commands.try_recv().unwrap();
    let (task, kind) = command.task_info();
    assert!(h.app.state.busy());
    h.click("执行查询");
    h.click("Describe");
    h.click("导出");
    assert!(h.commands.try_recv().is_err());
    h.events
        .send(WorkerEvent::Started {
            task,
            kind,
            label: "执行查询".into(),
        })
        .unwrap();
    h.events
        .send(WorkerEvent::Progress {
            task,
            message: "正在执行".into(),
        })
        .unwrap();
    h.events
        .send(WorkerEvent::Started {
            task,
            kind,
            label: "执行查询".into(),
        })
        .unwrap();
    h.frame(vec![]);
    assert_eq!(
        h.app.state.active_task.as_ref().unwrap().log,
        vec!["正在执行"]
    );
    h.finish_recorded_task();
    assert!(!h.app.state.busy());
}

fn rendered_text(output: &egui::FullOutput) -> String {
    fn collect(shape: &Shape, text: &mut String) {
        match shape {
            Shape::Text(label) => {
                text.push_str(label.galley.text());
                text.push('\n');
            }
            Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, text);
                }
            }
            _ => {}
        }
    }
    let mut text = String::new();
    for shape in &output.shapes {
        collect(&shape.shape, &mut text);
    }
    text
}

#[test]
fn real_query_failures_render_in_panel_and_corrected_sql_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let glob = dir.path().join("missing/**/*.parquet");
    let mut h = Harness::new(Panel::Query);
    (h.app.worker, h.app.events) = worker::spawn(h.ctx.clone());
    let wait_for_result = |h: &mut Harness, failed: bool| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let output = h.frame(vec![]);
            if !h.app.state.busy()
                && if failed {
                    h.app.state.query.error.is_some()
                } else {
                    !h.app.state.query.rows.is_empty()
                }
            {
                return rendered_text(&output);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "query did not complete"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    for (sql, detail, missing_files) in [
        (
            format!(
                "SELECT * FROM read_parquet('{}')",
                glob.display().to_string().replace('\'', "''")
            ),
            "No files found",
            true,
        ),
        ("SELECT FROM".into(), "Parser Error", false),
        ("SELECT * FROM missing_table".into(), "Catalog Error", false),
    ] {
        h.app.state.query.sql = sql;
        h.click("执行查询");
        let text = wait_for_result(&mut h, true);
        assert!(text.contains("查询操作失败"), "{text}");
        assert!(text.contains(detail), "{text}");
        assert_eq!(text.contains("未找到匹配的 Parquet 文件"), missing_files);
    }
    h.app.state.query.sql = "SELECT 1 AS recovered".into();
    h.click("执行查询");
    let text = wait_for_result(&mut h, false);
    assert!(h.app.state.query.error.is_none());
    assert!(!text.contains("查询操作失败"));
    assert_eq!(
        h.app.state.query.rows[0][0],
        analytics_core::ScalarValue::Int64(1)
    );
}

#[test]
fn describe_and_export_failures_render_and_retry_clears_old_error() {
    for kind in [TaskKind::Describe, TaskKind::Export] {
        let mut h = Harness::new(Panel::Query);
        let task = Uuid::new_v4();
        h.events
            .send(WorkerEvent::Started {
                task,
                kind,
                label: "查询操作".into(),
            })
            .unwrap();
        h.events
            .send(WorkerEvent::Failed {
                task,
                kind,
                message: "Permission denied".into(),
            })
            .unwrap();
        let text = rendered_text(&h.frame(vec![]));
        assert!(text.contains("查询操作失败"));
        assert!(text.contains("Permission denied"));
        assert!(!h.app.state.busy());
        h.click(if kind == TaskKind::Describe {
            "Describe"
        } else {
            "导出"
        });
        let task = match h.commands.try_recv().unwrap() {
            WorkerCommand::DescribeQuery { task, .. } | WorkerCommand::ExportQuery { task, .. } => {
                task
            }
            command => panic!("unexpected: {command:?}"),
        };
        h.events
            .send(WorkerEvent::Started {
                task,
                kind,
                label: "重试".into(),
            })
            .unwrap();
        let text = rendered_text(&h.frame(vec![]));
        assert!(h.app.state.query.error.is_none());
        assert!(!text.contains("查询操作失败"));
    }
}

#[test]
fn export_buttons_send_both_formats_and_reject_blank_destination() {
    let mut h = Harness::new(Panel::Query);
    h.app.state.query.sql = "SELECT 1".into();
    for csv in [false, true] {
        h.app.state.query.export_csv = csv;
        h.app.state.query.export_path = " /tmp/query-result ".into();
        h.click("导出");
        match h.commands.try_recv().unwrap() {
            WorkerCommand::ExportQuery { request, .. } => {
                assert_eq!(request.destination.to_str(), Some("/tmp/query-result"));
                assert!(matches!(
                    (csv, request.format),
                    (true, analytics_core::ExportFormat::Csv)
                        | (false, analytics_core::ExportFormat::Parquet)
                ));
            }
            command => panic!("unexpected: {command:?}"),
        }
        h.finish_recorded_task();
    }
    h.app.state.query.export_path = " ".into();
    h.click("导出");
    assert_eq!(h.app.state.query.error.as_deref(), Some("导出路径不能为空"));
    assert!(h.commands.try_recv().is_err());
}

#[test]
fn next_page_disabled_without_cursor_and_uses_selected_sort() {
    let mut h = Harness::new(Panel::Query);
    h.click("下一页");
    assert!(h.commands.try_recv().is_err());
    h.app.state.query.sql = "SELECT 1 AS id".into();
    h.app.state.query.sort_column = Some("id".into());
    h.app.state.query.next_cursor = Some(analytics_core::KeysetCursor(vec![
        analytics_core::ScalarValue::Int64(1),
    ]));
    h.click("降序");
    h.click("下一页");
    assert!(h.commands.try_recv().is_err());
    // Simulate completing a first-page query with the new ordering.
    h.app.state.query.submitted = Some(h.app.state.query.first_page_request().unwrap());
    h.click("下一页");
    match h.commands.try_recv().unwrap() {
        WorkerCommand::RunQuery { request, .. } => {
            assert!(request.after.is_some());
            assert!(request.sort[0].descending);
        }
        command => panic!("unexpected: {command:?}"),
    }
}

#[test]
fn factor_selection_clear_filter_select_all_and_toggle() {
    let mut h = Harness::new(Panel::Factor);
    h.click("清空");
    assert!(h.app.state.factor.selected.is_empty());
    h.app.state.factor.filter = "alpha_001".into();
    h.click("全选筛选结果");
    assert_eq!(h.app.state.factor.selected_ids(), vec!["alpha_001"]);
    h.click("alpha_001");
    assert!(h.app.state.factor.selected.is_empty());
    h.click("alpha_001");
    assert_eq!(h.app.state.factor.selected_ids(), vec!["alpha_001"]);
}

#[test]
fn factor_run_validates_input_and_shares_cancellation_flag() {
    let mut h = Harness::new(Panel::Factor);
    h.app.state.factor.selected.clear();
    h.click("运行因子流水线");
    assert_eq!(
        h.app.state.factor.error.as_deref(),
        Some("请至少选择一个因子")
    );
    assert!(h.commands.try_recv().is_err());
    h.app.state.factor.selected.insert("alpha_002".into());
    h.click("运行因子流水线");
    match h.commands.try_recv().unwrap() {
        WorkerCommand::RunPipeline {
            request, cancel, ..
        } => {
            assert_eq!(request.factor_ids, vec!["alpha_002"]);
            assert!(std::sync::Arc::ptr_eq(
                &cancel,
                h.app.cancel.as_ref().unwrap()
            ));
        }
        command => panic!("unexpected: {command:?}"),
    }
}

#[test]
fn training_validates_paths_and_parameters_and_preserves_universe_override() {
    let mut h = Harness::new(Panel::Model);
    h.app.state.model.cache_path.clear();
    h.click("开始训练");
    assert_eq!(
        h.app.state.model.error.as_deref(),
        Some("因子缓存路径不能为空")
    );
    assert!(h.commands.try_recv().is_err());
    h.app.state.model.cache_path = " /tmp/factors.parquet ".into();
    h.app.state.model.num_iterations = "bad".into();
    h.click("开始训练");
    assert!(
        h.app
            .state
            .model
            .error
            .as_ref()
            .unwrap()
            .contains("不是数值")
    );
    assert!(h.commands.try_recv().is_err());
    h.app.state.model.num_iterations = "20".into();
    for universe in ["", " csi500 "] {
        h.app.state.model.universe_id = universe.into();
        h.click("开始训练");
        match h.commands.try_recv().unwrap() {
            WorkerCommand::TrainFromCache {
                cache_path,
                universe_id,
                config,
                cancel,
                ..
            } => {
                assert_eq!(cache_path.to_str(), Some("/tmp/factors.parquet"));
                assert_eq!(
                    universe_id.as_deref(),
                    if universe.is_empty() {
                        None
                    } else {
                        Some("csi500")
                    }
                );
                assert_eq!(config.parameters["num_iterations"], "20");
                assert!(std::sync::Arc::ptr_eq(
                    &cancel,
                    h.app.cancel.as_ref().unwrap()
                ));
            }
            command => panic!("unexpected: {command:?}"),
        }
        h.finish_recorded_task();
    }
}

#[test]
fn all_task_buttons_are_disabled_while_worker_is_busy() {
    for (panel, labels) in [
        (Panel::Dataset, vec!["扫描数据集"]),
        (Panel::Query, vec!["执行查询", "Describe", "导出", "下一页"]),
        (Panel::Factor, vec!["运行因子流水线"]),
        (Panel::Model, vec!["开始训练"]),
    ] {
        let mut h = Harness::new(panel);
        h.busy();
        for label in labels {
            h.click(label);
        }
        assert!(h.commands.try_recv().is_err());
    }
}

#[test]
fn cancel_button_sets_flag_and_matching_completion_clears_it() {
    let mut h = Harness::new(Panel::Model);
    h.busy();
    let task = h.app.state.active_task.as_ref().unwrap().id;
    let flag = analytics_ml::pipeline::cancellation();
    h.app.cancel = Some(flag.clone());
    h.click("取消");
    assert!(flag.load(Ordering::Relaxed));
    h.events.send(WorkerEvent::Finished { task }).unwrap();
    h.frame(vec![]);
    assert!(!h.app.state.busy());
    assert!(h.app.cancel.is_none());
}

#[test]
fn page_size_sort_column_and_export_format_selectors_change_requests() {
    let mut h = Harness::new(Panel::Query);
    h.app.state.query.columns = vec![analytics_core::ColumnSchema {
        name: "id".into(),
        data_type: analytics_core::DataType::Int64,
        nullable: false,
    }];
    h.click("500");
    h.click("100");
    assert_eq!(h.app.state.query.limit, 100);
    h.click("（未选择）");
    h.click("id");
    assert_eq!(h.app.state.query.sort_column.as_deref(), Some("id"));
    h.click("id");
    h.click("（未选择）");
    assert!(h.app.state.query.sort_column.is_none());
    h.click("Parquet");
    h.click("CSV");
    assert!(h.app.state.query.export_csv);
}

#[test]
fn sql_editor_accepts_keyboard_input_before_execution() {
    let mut h = Harness::new(Panel::Query);
    h.app.state.query.sql = "SELECT 1".into();
    h.click("SELECT 1");
    let modifiers = egui::Modifiers {
        command: true,
        mac_cmd: true,
        ..Default::default()
    };
    h.frame(vec![
        Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        },
        Event::Text("SELECT 9 AS edited".into()),
    ]);
    h.click("执行查询");
    assert!(
        matches!(h.commands.try_recv().unwrap(), WorkerCommand::RunQuery { request, .. }
        if request.sql == "SELECT 9 AS edited")
    );
}

#[test]
fn dataset_file_selection_renders_schema_and_market_contract_warning() {
    let mut h = Harness::new(Panel::Dataset);
    h.app.state.dataset.files = vec![analytics_core::DatasetFile {
        path: "/tmp/example.parquet".into(),
        rows: Some(3),
        schema: vec![analytics_core::ColumnSchema {
            name: "id".into(),
            data_type: analytics_core::DataType::Int64,
            nullable: false,
        }],
    }];
    h.click("/tmp/example.parquet");
    assert_eq!(h.app.state.dataset.selected, Some(0));
    let output = h.frame(vec![]);
    assert!(
        output
            .shapes
            .iter()
            .any(|shape| matches!(&shape.shape, Shape::Text(text)
        if text.galley.text().contains("非标准行情表")))
    );
    assert!(h.commands.try_recv().is_err());
}

#[test]
fn factor_run_rejects_each_invalid_path_and_date_field() {
    for field in ["market", "universe", "output", "start", "end", "order"] {
        let mut h = Harness::new(Panel::Factor);
        match field {
            "market" => h.app.state.factor.market_path = " ".into(),
            "universe" => h.app.state.factor.universe_path.clear(),
            "output" => h.app.state.factor.output_dir.clear(),
            "start" => h.app.state.factor.date_start = "bad".into(),
            "end" => h.app.state.factor.date_end = "2024-02-30".into(),
            _ => h.app.state.factor.date_start = "2026-01-01".into(),
        }
        h.click("运行因子流水线");
        assert!(h.app.state.factor.error.is_some(), "field: {field}");
        assert!(h.commands.try_recv().is_err());
        assert!(h.app.cancel.is_none());
    }
}

#[test]
fn stale_terminal_events_preserve_active_task_and_cancellation() {
    let mut h = Harness::new(Panel::Model);
    h.busy();
    h.app.cancel = Some(analytics_ml::pipeline::cancellation());
    h.events
        .send(WorkerEvent::Failed {
            task: Uuid::new_v4(),
            kind: TaskKind::Training,
            message: "obsolete failure".into(),
        })
        .unwrap();
    h.events
        .send(WorkerEvent::Finished {
            task: Uuid::new_v4(),
        })
        .unwrap();
    h.frame(vec![]);
    assert!(h.app.state.busy());
    assert!(h.app.cancel.is_some());
    assert!(h.app.state.model.error.is_none());
}

#[test]
fn training_rejects_nonfinite_negative_fractional_and_empty_parameters() {
    for (field, value) in [
        ("iterations", "0"),
        ("iterations", "1.5"),
        ("leaves", "1"),
        ("leaf_data", "-1"),
        ("rate", "NaN"),
        ("rate", "inf"),
        ("rate", "-0.1"),
        ("rate", ""),
        ("early", "-2"),
        ("output", " "),
    ] {
        let mut h = Harness::new(Panel::Model);
        match field {
            "iterations" => h.app.state.model.num_iterations = value.into(),
            "leaves" => h.app.state.model.num_leaves = value.into(),
            "leaf_data" => h.app.state.model.min_data_in_leaf = value.into(),
            "rate" => h.app.state.model.learning_rate = value.into(),
            "early" => h.app.state.model.early_stopping = value.into(),
            _ => h.app.state.model.output_dir = value.into(),
        }
        h.click("开始训练");
        assert!(h.app.state.model.error.is_some(), "{field}={value}");
        assert!(h.commands.try_recv().is_err());
    }
}

#[test]
fn all_panels_render_under_light_and_dark_themes() {
    for preference in [egui::ThemePreference::Light, egui::ThemePreference::Dark] {
        for panel in [Panel::Dataset, Panel::Query, Panel::Factor, Panel::Model] {
            let mut h = Harness::new(panel);
            h.ctx.set_theme(preference);
            let output = h.frame(vec![]);
            assert!(!output.shapes.is_empty());
            assert!(h.commands.try_recv().is_err());
        }
    }
}
