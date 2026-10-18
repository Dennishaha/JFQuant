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
                "worker did not report its result: active={:?} worker_error={:?} platform_error={:?} details={}",
                self.app.state.active_task,
                self.app.state.worker_error,
                self.app.state.platform.error,
                self.app.state.platform.details
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
    h.app.cancel = Some(analytics_workflow::research::pipeline::cancellation());
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
        h.app.cancel = Some(analytics_workflow::research::pipeline::cancellation());
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
            TaskKind::Workflow => &h.app.state.platform.error,
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
    let flag = analytics_workflow::research::pipeline::cancellation();
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
    h.app.cancel = Some(analytics_workflow::research::pipeline::cancellation());
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
        for panel in [
            Panel::Dataset,
            Panel::Query,
            Panel::Factor,
            Panel::Model,
            Panel::Research,
            Panel::Runs,
        ] {
            let (mut h, _tmp) = platform_harness(panel);
            h.ctx.set_theme(preference);
            let output = h.frame(vec![]);
            assert!(!output.shapes.is_empty());
            assert!(h.commands.try_recv().is_err());
        }
    }
}

#[test]
fn quarantined_factor_is_visible_disabled_and_excluded_from_bulk_selection() {
    let mut h = Harness::new(Panel::Factor);
    h.click("清空");
    h.app.state.factor.filter = "alpha_054".into();
    h.click("全选筛选结果");
    assert!(h.app.state.factor.selected.is_empty());
    h.app.state.factor.filter.clear();
    h.click("全选筛选结果");
    assert_eq!(h.app.state.factor.selected.len(), 178);
    assert!(!h.app.state.factor.selected.contains("alpha_030"));
    for id in analytics_factor::audit::quarantined_ids() {
        assert!(!h.app.state.factor.selected.contains(&id));
    }
}

fn platform_harness(panel: Panel) -> (Harness, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = analytics_workflow::Workspace::open(tmp.path()).unwrap();
    let mut h = Harness::new(panel);
    h.app.state.platform.root_input = workspace.root().display().to_string();
    h.app.state.platform.lease = Some(workspace.lease().unwrap());
    h.app.state.platform.workspace = Some(workspace);
    (h, tmp)
}
#[test]
fn platform_data_update_disables_double_submit_and_supports_cancel() {
    let (mut h, _tmp) = platform_harness(Panel::Dataset);
    h.click("检查行情更新");
    let command = h.commands.try_recv().unwrap();
    let (task, cancel) = match command {
        WorkerCommand::ExecuteWorkflow {
            task,
            request,
            cancel,
        } => {
            assert!(matches!(
                *request,
                analytics_workflow::WorkflowRequest::CheckUpdates {}
            ));
            (task, cancel)
        }
        _ => panic!("expected workflow"),
    };
    h.click("检查行情更新");
    assert!(h.commands.try_recv().is_err());
    h.click("取消");
    assert!(cancel.load(Ordering::Relaxed));
    h.events
        .send(WorkerEvent::Failed {
            task,
            kind: TaskKind::Workflow,
            message: "网络连接失败".into(),
        })
        .unwrap();
    let out = h.frame(vec![]);
    assert!(rendered_text(&out).contains("网络连接失败"));
    assert!(!h.app.state.busy());
}
#[test]
fn research_template_saves_and_real_worker_finishes_synthetic_annual_account() {
    let (mut h, _tmp) = platform_harness(Panel::Research);
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let (m, a, template) = analytics_workflow::testing::synthetic(&w);
    h.app.state.platform.template = template;
    h.app.state.platform.datasets = w.datasets().unwrap();
    h.app.state.platform.market = m.id;
    h.app.state.platform.actions = a.id;
    h.click("保存模板");
    assert_eq!(
        std::fs::read_dir(w.resolve(".jfquant/templates"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|s| s == "json"))
            .count(),
        1
    );
    let (worker, events) = worker::spawn_in_workspace(h.ctx.clone(), Some(w.root().to_owned()));
    h.app.worker = worker;
    h.app.events = events;
    h.click("执行年度研究");
    h.wait_for(|app| !app.state.busy() && !app.state.platform.runs.is_empty());
    let record = &h.app.state.platform.runs[0];
    assert_eq!(record.status, analytics_workflow::RunStatus::Succeeded);
    assert!(
        h.app.state.platform.details["aggregate"]["total_return"]
            .as_f64()
            .unwrap()
            .abs()
            < 1e-6
    );
    assert!(
        w.resolve(&record.output)
            .join("portfolio_snapshots.json")
            .exists()
    );
    h.app.state.panel = Panel::Runs;
    h.wait_for(|app| app.state.platform.result_loaded());
    let out = h.frame(vec![]);
    assert!(rendered_text(&out).contains("年化"));
}

#[test]
fn plain_language_quality_choices_submit_the_shared_policy_and_render_reason_filters() {
    let (mut h, _temp) = platform_harness(Panel::Research);
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let (m, a, t) = analytics_workflow::testing::synthetic(&w);
    h.app.state.platform.template = t;
    h.app.state.platform.datasets = w.datasets().unwrap();
    h.app.state.platform.market = m.id;
    h.app.state.platform.actions = a.id;
    h.click("1. 严格检查");
    h.click("2. 允许部分数据缺失");
    assert_eq!(
        h.app.state.platform.template.quality_policy.strictness,
        analytics_core::research_quality::ResearchStrictness::AllowMissing
    );
    h.click("2. 允许部分数据缺失");
    h.click("3. 只用数据合格的股票");
    h.click("检查研究输入");
    let command = h.commands.recv().unwrap();
    assert!(
        matches!(command,WorkerCommand::ExecuteWorkflow{request,..} if matches!(&*request,analytics_workflow::WorkflowRequest::Research{template,audit_only:true,..} if template.quality_policy.strictness==analytics_core::research_quality::ResearchStrictness::QualifiedOnly))
    );
    h.finish_recorded_task();
    h.app.state.platform.details = serde_json::json!({"ready":false,"admission":{"policy":h.app.state.platform.template.quality_policy,"checks":[{"security":"SEC-fixture","category":"market","coverage":{"start":"2020-01-02","end":"2020-01-03"},"status":"failed","code":"input_file_missing","message":"成交额输入缺失","kind":"missing","disposition":"exclude_security","evidence":[],"affected_factors":["alpha_001"]}],"decisions":[]}});
    h.ctx
        .style_mut_of(egui::Theme::Light, |style| style.animation_time = 0.);
    h.ctx
        .style_mut_of(egui::Theme::Dark, |style| style.animation_time = 0.);
    h.click("数据检查原因列表（1项）");
    let output = h.frame(vec![]);
    let text = rendered_text(&output);
    assert!(text.contains("成交额输入缺失"), "{text}");
    assert!(text.contains("排除股票"));
    assert!(text.contains("导出完整检查JSON"));
}
#[test]
fn research_worker_preserves_shares_cash_and_paid_fees_across_annual_models() {
    let (mut h, _tmp) = platform_harness(Panel::Research);
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let (m, a, mut template) = analytics_workflow::testing::synthetic(&w);
    template.initial_cash_cny = 1050.0;
    template.market_rules.commission.minimum_cny = 5.0;
    h.app.state.platform.template = template;
    h.app.state.platform.datasets = w.datasets().unwrap();
    h.app.state.platform.market = m.id;
    h.app.state.platform.actions = a.id;
    let (worker, events) = worker::spawn_in_workspace(h.ctx.clone(), Some(w.root().to_owned()));
    h.app.worker = worker;
    h.app.events = events;
    h.click("执行年度研究");
    h.wait_for(|app| !app.state.busy() && !app.state.platform.runs.is_empty());
    let record = &h.app.state.platform.runs[0];
    assert_eq!(record.status, analytics_workflow::RunStatus::Succeeded);
    let result = &h.app.state.platform.details;
    assert_eq!(result["annual_reports"].as_array().unwrap().len(), 2);
    let total_return = result["aggregate"]["total_return"].as_f64().unwrap();
    assert!((total_return + 5.0 / 1050.0).abs() < 1e-6);
    let total_fee = result["aggregate"]["execution_fee_cny"].as_f64().unwrap();
    assert!((total_fee - 5.0).abs() < 1e-8);
    let snapshots: serde_json::Value = serde_json::from_slice(
        &std::fs::read(w.resolve(&record.output).join("portfolio_snapshots.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        snapshots["2009-01-12"]["positions"],
        snapshots["2010-01-12"]["positions"]
    );
}
#[test]
fn incomplete_research_is_persisted_as_blocked_and_shown_without_old_results() {
    let (mut h, _tmp) = platform_harness(Panel::Research);
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let (m, a, template) = analytics_workflow::testing::synthetic(&w);
    let p = w.resolve(&a.path);
    let mut book = analytics_storage::corporate_actions::ActionBundle::read(&p).unwrap();
    book.actions[0].cash_per_share = "99.0".into();
    std::fs::write(&p, serde_json::to_vec(&book).unwrap()).unwrap();
    let a = w.register(&p).unwrap();
    h.app.state.platform.template = template;
    h.app.state.platform.datasets = w.datasets().unwrap();
    h.app.state.platform.market = m.id;
    h.app.state.platform.actions = a.id;
    let (worker, events) = worker::spawn_in_workspace(h.ctx.clone(), Some(w.root().to_owned()));
    h.app.worker = worker;
    h.app.events = events;
    h.click("执行年度研究");
    let out = h.wait_for(|app| !app.state.busy() && !app.state.platform.runs.is_empty());
    assert!(rendered_text(&out).contains("数据未就绪"));
    assert_eq!(
        h.app.state.platform.runs[0].status,
        analytics_workflow::RunStatus::Blocked
    );
    assert!(
        !w.resolve(&h.app.state.platform.runs[0].output)
            .join("factor-cache")
            .exists()
    );
    assert_eq!(
        w.runs().unwrap()[0].status,
        analytics_workflow::RunStatus::Blocked
    );
}

#[test]
fn interrupted_acquisition_can_prepare_exact_owned_resume_without_starting_research() {
    let (mut h, _tmp) = platform_harness(Panel::Runs);
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let task = Uuid::new_v4();
    let request = analytics_workflow::WorkflowRequest::DownloadActions {
        pool_version: "builtin:csi500".into(),
        market: "market-id".into(),
        resume: None,
    };
    let mut run = w
        .begin(task, "公司行动", serde_json::to_value(request).unwrap())
        .unwrap();
    w.finish(&mut run, analytics_workflow::RunStatus::Interrupted, None)
        .unwrap();
    h.app.state.platform.runs = w.runs().unwrap();
    h.app.state.platform.selected_run = Some(task);
    h.click("准备恢复公司行动");
    assert_eq!(h.app.state.panel, Panel::Dataset);
    assert_eq!(h.app.state.platform.market, "market-id");
    assert_eq!(
        h.app.state.platform.details["resume_actions"],
        task.to_string()
    );
    assert!(h.commands.try_recv().is_err());
}

#[test]
fn demo_button_configures_full_research_without_download_or_developer_tools() {
    let (mut h, _tmp) = platform_harness(Panel::Dataset);
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let (worker, events) = worker::spawn_in_workspace(h.ctx.clone(), Some(w.root().to_owned()));
    h.app.worker = worker;
    h.app.events = events;
    h.click("创建合成演示数据");
    h.wait_for(|app| !app.state.busy() && !app.state.platform.datasets.is_empty());
    assert_eq!(
        h.app.state.platform.template.config.factor_ids,
        ["alpha_002"]
    );
    assert!(
        serde_json::to_string(&h.app.state.platform.template.schedule)
            .unwrap()
            .contains("2009-01-01")
    );
    assert!(!h.app.state.platform.actions.is_empty());
    h.app.state.panel = Panel::Research;
    h.click("执行年度研究");
    let out = h
        .wait_for(|app| !app.state.busy() && app.state.platform.details.get("aggregate").is_some());
    assert!(rendered_text(&out).contains("合成演示数据"));
    assert_eq!(
        h.app.state.platform.details["input_kind"],
        "synthetic_fixture"
    );
}

#[test]
fn service_client_tasks_appear_in_egui_can_be_cancelled_and_share_research_results() {
    use analytics_workflow::service::{Client, Rpc, TaskRequest};
    let (mut h, _tmp) = platform_harness(Panel::Runs);
    h.app.state.platform.lease.take();
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let root = w.root().to_owned();
    let server = std::thread::spawn(move || analytics_workflow::service::serve(&root).unwrap());
    let client = Client::connect(w.root()).unwrap();
    for _ in 0..200 {
        if client.call(Rpc::Status).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let ctx = h.ctx.clone();
    let (worker, events) = analytics_workflow::service::bridge(
        analytics_workflow::backend::Wake::new(move || ctx.request_repaint()),
        w.root().to_owned(),
    );
    h.app.worker = worker;
    h.app.events = events;
    let task = Uuid::new_v4();
    client
        .call(Rpc::Submit {
            id: task,
            request: Box::new(TaskRequest::TestingPause { milliseconds: 5000 }),
        })
        .unwrap();
    h.wait_for(|app| app.state.active_task.as_ref().is_some_and(|t| t.id == task));
    assert!(h.app.cancel.is_none());
    h.click("取消");
    h.wait_for(|app| !app.state.busy() && app.state.platform.runs.iter().any(|r| r.id == task));
    assert_eq!(
        h.app
            .state
            .platform
            .runs
            .iter()
            .find(|r| r.id == task)
            .unwrap()
            .status,
        analytics_workflow::RunStatus::Cancelled
    );
    // Another client's SQL must not replace the App's page or cursor.
    h.app.state.panel = Panel::Query;
    h.app.state.query.sql = "SELECT range AS id FROM range(600)".into();
    h.click("执行查询");
    h.wait_for(|app| !app.state.busy() && app.state.query.rows.len() == 500);
    let old_rows = h.app.state.query.rows.clone();
    let old_cursor = h.app.state.query.next_cursor.clone();
    let remote_query = Uuid::new_v4();
    client
        .call(Rpc::Submit {
            id: remote_query,
            request: Box::new(TaskRequest::Query {
                query_id: None,
                request: analytics_core::PageRequest {
                    sql: "SELECT 999 AS foreign_value".into(),
                    limit: 10,
                    sort: vec![],
                    after: None,
                },
            }),
        })
        .unwrap();
    client.wait(remote_query, Some(10)).unwrap();
    h.wait_for(|app| {
        !app.state.busy() && app.state.platform.runs.iter().any(|r| r.id == remote_query)
    });
    assert_eq!(h.app.state.query.rows, old_rows);
    assert_eq!(h.app.state.query.next_cursor, old_cursor);
    h.click("下一页");
    h.wait_for(|app| !app.state.busy() && app.state.query.rows.len() == 600);
    assert_eq!(
        h.app.state.query.rows[599][0],
        analytics_core::ScalarValue::Int64(599)
    );
    h.app.state.panel = Panel::Runs;
    let task = Uuid::new_v4();
    client
        .call(Rpc::Submit {
            id: task,
            request: Box::new(TaskRequest::Workflow {
                request: Box::new(analytics_workflow::WorkflowRequest::CreateDemo {}),
            }),
        })
        .unwrap();
    h.wait_for(|app| !app.state.busy() && app.state.platform.runs.iter().any(|r| r.id == task));
    assert_eq!(h.app.state.platform.template.schema_version, 12);
    h.app.state.panel = Panel::Research;
    h.click("执行年度研究");
    h.wait_for(|app| !app.state.busy() && app.state.platform.details.get("aggregate").is_some());
    let record = h
        .app
        .state
        .platform
        .runs
        .iter()
        .find(|r| r.operation.contains("年度滚动研究"))
        .unwrap()
        .clone();
    assert_eq!(
        client.call(Rpc::Results { id: record.id }).unwrap(),
        h.app.state.platform.details
    );
    assert_eq!(
        h.app.state.platform.details["annual_reports"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let expected = h.app.state.platform.details.clone();
    let cli_id = Uuid::new_v4();
    client
        .call(Rpc::Submit {
            id: cli_id,
            request: Box::new(TaskRequest::Workflow {
                request: Box::new(analytics_workflow::WorkflowRequest::Research {
                    market: h.app.state.platform.market.clone(),
                    actions: Some(h.app.state.platform.actions.clone()),
                    template: Box::new(h.app.state.platform.template.clone()),
                    audit_only: false,
                }),
            }),
        })
        .unwrap();
    h.wait_for(|app| !app.state.busy() && app.state.platform.runs.iter().any(|r| r.id == cli_id));
    assert_eq!(h.app.state.platform.details, expected);
    assert_eq!(client.call(Rpc::Results { id: cli_id }).unwrap(), expected);
    let read = |path: std::path::PathBuf| {
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap()).unwrap()
    };
    assert_eq!(
        read(w.resolve(&record.output).join("portfolio_snapshots.json")),
        read(w.resolve(format!(".jfquant/runs/{cli_id}/portfolio_snapshots.json")))
    );
    client.call(Rpc::Stop).unwrap();
    server.join().unwrap();
}

#[test]
fn unsupported_history_shows_error_without_nav_or_resumption_and_other_records_remain_readable() {
    let (mut h, _tmp) = platform_harness(Panel::Runs);
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let mut old = w.begin(Uuid::new_v4(), "旧格式研究", serde_json::json!({"request":{"operation":"compatibility","request":{"operation":"research"}}})).unwrap();
    let summary = serde_json::json!({"schema_version":3,"execution":{"mode":"legacy_adjusted_fractional"},"aggregate":{"total_return":2.0}}).to_string();
    std::fs::write(w.resolve(&old.output).join("summary.json"), &summary).unwrap();
    std::fs::write(
        w.resolve(&old.output).join("nav.csv"),
        "date,nav_after_fees,csi500_price_index_nav\n2020-01-02,3,1\n2020-01-03,4,1\n",
    )
    .unwrap();
    w.finish(&mut old, analytics_workflow::RunStatus::Interrupted, None)
        .unwrap();
    h.app.state.platform.runs = w.runs().unwrap();
    h.app.state.platform.selected_run = Some(old.id);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let text = loop {
        let text = rendered_text(&h.frame(vec![]));
        if text.contains("unsupported_result_format") {
            break text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "history read timed out: {text}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    assert!(text.contains("unsupported_result_format"));
    assert!(!text.contains("净值 ·"));
    assert!(!text.contains("年化"));
    assert!(h.commands.try_recv().is_err());
    h.app.state.platform.import_run_path = old.output.display().to_string();
    h.click("导入只读记录");
    h.wait_for(|app| app.state.platform.error.is_some());
    assert!(
        h.app
            .state
            .platform
            .error
            .as_ref()
            .unwrap()
            .contains("unsupported_result_format")
    );
    assert_eq!(w.runs().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(w.resolve(&old.output).join("summary.json")).unwrap(),
        summary
    );
    let (market, actions, template) = analytics_workflow::testing::synthetic(&w);
    let request = analytics_workflow::WorkflowRequest::Research {
        template: Box::new(template),
        market: market.id,
        actions: Some(actions.id),
        audit_only: false,
    };
    let ctx = analytics_core::task::TaskContext::new(
        Uuid::new_v4(),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        |_| {},
    );
    let mut current = w
        .begin(
            ctx.id,
            "当前研究",
            analytics_workflow::snapshot(&w, &request).unwrap(),
        )
        .unwrap();
    analytics_workflow::execute(&w, &request, &current, &ctx).unwrap();
    w.finish(&mut current, analytics_workflow::RunStatus::Succeeded, None)
        .unwrap();
    h.app.state.platform.runs = w.runs().unwrap();
    h.app.state.platform.selected_run = Some(current.id);
    h.app.state.platform.error = None;
    h.frame(vec![]);
    h.wait_for(|app| app.state.platform.result_loaded());
    let text = rendered_text(&h.frame(vec![]));
    assert!(text.contains("年化"));
    assert!(!text.contains("unsupported_result_format"));
}

#[test]
fn pool_benchmark_exit_policy_and_save_controls_use_shared_requests() {
    let (mut h, _tmp) = platform_harness(Panel::Research);
    h.ctx.all_styles_mut(|s| s.animation_time = 0.0);
    h.click("股票池、基准与日期调整");
    h.click("沪深300默认模板");
    assert_eq!(h.app.state.platform.template.benchmark, "SH000300");
    assert_eq!(h.app.state.platform.template.pool_version, "builtin:csi300");
    h.click("下次正常调仓退出");
    assert_eq!(
        h.app.state.platform.template.pool_exit_policy,
        analytics_core::pool::PoolExitPolicy::NextRebalance
    );
    h.click("自选池示例");
    h.click("校验并保存股票池新版本");
    let command = h
        .commands
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(
        matches!(command,WorkerCommand::ExecuteWorkflow{request,..} if matches!(*request,analytics_workflow::WorkflowRequest::SavePool{..}))
    );
    assert!(h.app.state.busy());
}
#[test]
fn data_collection_controls_preserve_provider_and_prevent_duplicate_submissions() {
    let (mut h, _tmp) = platform_harness(Panel::Dataset);
    h.ctx.all_styles_mut(|s| s.animation_time = 0.0);
    h.click("HiThink / 通达信采集");
    h.click("HiThink（主源）");
    h.click("采集日线并发布新版本");
    let command = h
        .commands
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(
        matches!(command,WorkerCommand::ExecuteWorkflow{request,..} if matches!(&*request,analytics_workflow::WorkflowRequest::CollectMarket{request,..} if request.provider==analytics_storage::collector::Provider::Hithink && request.adjustment_source==analytics_storage::adjustment::AdjustmentSource::HithinkForward))
    );
    h.click("采集日线并发布新版本");
    assert!(h.commands.try_recv().is_err());
}

#[test]
fn packaging_load_command_rejection_fixtures() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging");
    let output = std::process::Command::new(analytics_storage::collector::runtime().unwrap())
        .args(["-I", "-B", "-m", "unittest", "discover", "-s"])
        .arg(directory)
        .args(["-p", "test_packaging.py"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn date_preview_and_manual_fold_editor_submit_shared_requests_without_training() {
    let (mut h, _tmp) = platform_harness(Panel::Research);
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let (m, a, mut t) = analytics_workflow::demo::create_canonical(&w).unwrap();
    let first = match t.schedule {
        analytics_core::schedule::Schedule::Rolling { first, .. } => first,
        _ => unreachable!(),
    };
    t.schedule = analytics_core::schedule::Schedule::Single { fold: first };
    h.app.state.platform.template = t.clone();
    h.app.state.platform.datasets = w.datasets().unwrap();
    h.app.state.platform.market = m.id;
    h.app.state.platform.actions = a.id;
    h.click("预览研究日程");
    let command = h.commands.try_recv().unwrap();
    assert!(
        matches!(command,WorkerCommand::ExecuteWorkflow{request,..} if matches!(&*request,analytics_workflow::WorkflowRequest::PreviewResearch{template,..} if serde_json::to_value(&template.schedule).unwrap()==serde_json::to_value(&t.schedule).unwrap()))
    );
    h.click("预览研究日程");
    assert!(h.commands.try_recv().is_err());
}

#[test]
fn structured_action_inspection_uses_existing_workflow_without_announcement_manifest() {
    let (mut h, _tmp) = platform_harness(Panel::Research);
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let (m, a, t) = analytics_workflow::testing::synthetic(&w);
    h.app.state.platform.template = t;
    h.app.state.platform.datasets = w.datasets().unwrap();
    h.app.state.platform.market = m.id;
    h.app.state.platform.actions = a.id.clone();
    h.app.state.platform.action_review_instrument = "SH600030".into();
    h.ctx.all_styles_mut(|s| s.animation_time = 0.0);
    h.click("公司行动：查看证据与审计修订");
    h.click("加载事件、结构化字段与公告证据");
    let command = h.commands.recv().unwrap();
    assert!(
        matches!(command, WorkerCommand::ExecuteWorkflow {request,..} if matches!(&*request, analytics_workflow::WorkflowRequest::InspectActions {actions,instrument,evidence_manifest:None} if actions==&a.id && instrument=="SH600030"))
    );
}

#[test]
fn prediction_replay_exposes_rights_defaults_and_shared_task() {
    let (mut h, _tmp) = platform_harness(Panel::Research);
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let (m, a, t) = analytics_workflow::testing::synthetic(&w);
    h.app.state.platform.template = t;
    h.app.state.platform.market = m.id;
    h.app.state.platform.actions = a.id.clone();
    let source = uuid::Uuid::new_v4();
    h.app.state.platform.prediction_job = source.to_string();
    h.ctx.all_styles_mut(|s| s.animation_time = 0.0);
    h.click("配股与预测复用");
    h.click("复用已发布预测回测");
    let command = h.commands.try_recv().unwrap();
    assert!(
        matches!(command,WorkerCommand::ExecuteWorkflow{request,..} if matches!(&*request,analytics_workflow::WorkflowRequest::BacktestPredictions{source:id,actions,rights_policy,..} if *id==source && actions==&a.id && rights_policy==&analytics_core::rights::Policy::default()))
    );
}

#[test]
fn research_stage_buttons_submit_shared_tasks_without_identity_controls() {
    let (mut h, _tmp) = platform_harness(Panel::Research);
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let (m, a, t) = analytics_workflow::testing::synthetic(&w);
    h.app.state.platform.template = t;
    h.app.state.platform.datasets = w.datasets().unwrap();
    h.app.state.platform.market = m.id;
    h.app.state.platform.actions = a.id;
    h.click("仅训练模型");
    let command = h.commands.recv().unwrap();
    assert!(
        matches!(command,WorkerCommand::ExecuteWorkflow{request,..} if matches!(&*request,analytics_workflow::WorkflowRequest::TrainResearch{..}))
    );
    h.finish_recorded_task();
    let id = uuid::Uuid::new_v4();
    h.app.state.platform.training_job = id.to_string();
    h.click("使用已有模型回测");
    let command = h.commands.recv().unwrap();
    assert!(
        matches!(command,WorkerCommand::ExecuteWorkflow{request,..} if matches!(&*request,analytics_workflow::WorkflowRequest::BacktestResearch{training,..} if *training==id))
    );
}

#[test]
fn research_stage_results_distinguish_preserved_models_and_no_valid_trades() {
    let (mut h, _tmp) = platform_harness(Panel::Research);
    h.app.state.platform.details = serde_json::json!({"ready":false,"training_complete":true,"training_artifact":{"status":"trained"}});
    let frame = h.frame(vec![]);
    let text = rendered_text(&frame);
    assert!(text.contains("已保存训练模型"), "{text}");
    assert!(!text.contains("尚未进行训练"));
    h.app.state.platform.details = serde_json::json!({"backtest_status":"no_valid_trades"});
    assert!(rendered_text(&h.frame(vec![])).contains("无有效交易"));
}

#[test]
fn alpha158_provider_training_without_actions_uses_shared_backend_and_split_profiles() {
    use analytics_core::research_quality::{FactorSet, PriceBasis};
    let (mut h, _tmp) = platform_harness(Panel::Research);
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let (m, _, mut t) = analytics_workflow::testing::synthetic(&w);
    t.select_factor_set(FactorSet::Alpha158);
    t.config.factor_ids = vec!["alpha158_001_KMID".into(), "alpha158_014_ROC5".into()];
    t.price_basis = PriceBasis::Provider;
    let (worker, events) = worker::spawn_in_workspace(h.ctx.clone(), Some(w.root().to_owned()));
    h.app.worker = worker;
    h.app.events = events;
    h.app.state.platform.template = t;
    h.app.state.platform.datasets = w.datasets().unwrap();
    h.app.state.platform.market = m.id;
    h.app.state.platform.actions.clear();
    h.click("保存模板");
    assert!(w.resolve(".jfquant/templates/factors").is_dir());
    assert!(w.resolve(".jfquant/templates/targets").is_dir());
    h.click("仅训练模型");
    h.wait_for(|app| !app.state.busy() && app.state.platform.details["status"] == "trained");
    assert_eq!(h.app.state.platform.details["factor_set"], "alpha158");
    assert_eq!(h.app.state.platform.details["price_basis"], "provider");
    assert!(
        !h.app.state.platform.details["source_warnings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        h.app.state.platform.details["feature_order"],
        serde_json::json!(h.app.state.platform.template.config.factor_ids)
    );
}
#[test]
fn credentials_are_never_in_platform_debug_output() {
    let state = crate::platform::PlatformState {
        hithink_key: "private-fixture-token".into(),
        tdx_key: "private-tdx-fixture-key".into(),
        ..Default::default()
    };
    let value = format!("{state:?}");
    assert!(!value.contains("private-fixture-token"));
    assert!(!value.contains("private-tdx-fixture-key"));
    assert!(value.contains("[redacted]"));
}

#[test]
fn acquisition_defaults_match_hithink_main_and_explicit_execution_gap() {
    let state = crate::platform::PlatformState::default();
    assert_eq!(
        state.collection_provider,
        analytics_storage::collector::Provider::Hithink
    );
    assert_eq!(
        state.adjustment_source,
        analytics_storage::adjustment::AdjustmentSource::HithinkForward
    );
    let policy: analytics_workflow::refresh::SourcePolicy =
        serde_json::from_str(&state.source_policy_editor).unwrap();
    for (role, sources) in &policy.priorities {
        if *role == analytics_workflow::refresh::DataRole::Execution {
            assert!(sources.is_empty());
        } else {
            assert_eq!(sources[0], state.collection_provider);
        }
    }
}

#[test]
fn factor_group_builtin_is_readonly_and_copy_can_be_saved() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(Panel::Factor);
    h.app.state.platform.workspace = Some(analytics_workflow::Workspace::open(dir.path()).unwrap());
    h.frame(vec![]);
    let group = analytics_workflow::factor_groups::FactorGroup::builtin(
        analytics_core::research_quality::FactorSet::Alpha158,
    );
    h.app.state.factor.factor_set = group.profile.factor_set;
    h.app.state.factor.selected = group.profile.factor_ids.iter().cloned().collect();
    h.app.state.factor.group = Some(group);
    h.click("清空");
    assert_eq!(h.app.state.factor.selected.len(), 158);
    h.click("复制为自定义组");
    assert!(!h.app.state.factor.group.as_ref().unwrap().is_builtin());
    h.click("清空");
    assert!(h.app.state.factor.selected.is_empty());
    h.app
        .state
        .factor
        .selected
        .insert("alpha158_001_KMID".into());
    h.click("保存因子组");
    let groups = analytics_workflow::factor_groups::list(dir.path()).unwrap();
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[2].profile.factor_ids, vec!["alpha158_001_KMID"]);
    h.click("应用到研究配置");
    assert_eq!(
        h.app.state.platform.template.config.factor_ids,
        vec!["alpha158_001_KMID"]
    );
}

#[test]
fn factor_input_check_submits_unsaved_selection_policy_and_cancellation() {
    let mut h = Harness::new(Panel::Factor);
    h.app.state.factor.check_policy.strictness =
        analytics_core::research_quality::ResearchStrictness::AllowMissing;
    h.app.state.factor.horizon = 3;
    h.click("检查组合数据");
    match h.commands.try_recv().unwrap() {
        WorkerCommand::CheckFactors {
            request, cancel, ..
        } => {
            assert_eq!(request.horizon, 3);
            assert_eq!(request.input.factor_ids, ["alpha_001", "alpha_002"]);
            assert!(request.policy.allows_missing());
            assert!(request.expected_input_hash.is_none());
            h.app
                .cancel
                .as_ref()
                .unwrap()
                .store(true, Ordering::Relaxed);
            assert!(cancel.load(Ordering::Relaxed));
        }
        other => panic!("unexpected command {other:?}"),
    }
}

#[test]
fn factor_input_report_expires_after_context_or_source_changes() {
    use analytics_workflow::input_check::{InputCheckPlan, InputCheckReport, PhaseRange};
    let mut h = Harness::new(Panel::Factor);
    let input: analytics_workflow::factor_evaluation::EvaluationInput =
        panels::run_pipeline_request(&h.app.state).unwrap().into();
    let registry =
        FactorRegistry::resolve(input.factor_set, &input.factor_refs, &input.factor_ids).unwrap();
    let context = analytics_workflow::input_check::factor_context(
        &input,
        h.app.state.factor.horizon,
        &h.app.state.factor.check_policy,
    );
    let plan = InputCheckPlan::new(
        input.factor_set,
        &input.factor_refs,
        &input.factor_ids,
        &registry,
        "factor_page",
        context,
        vec![PhaseRange {
            phase: "factor_page".into(),
            requested: input.date_range.clone(),
            effective: input.date_range,
        }],
    )
    .unwrap();
    h.app.state.factor.input_check = Some(InputCheckReport {
        ready: false,
        schema_version: 1,
        plan,
        context_hash: "test".into(),
        stages: vec![],
        admission: serde_json::from_value(serde_json::json!({
            "schema_version":5,"price_basis":"provider",
            "policy":h.app.state.factor.check_policy,"decisions":[],"checks":[]
        }))
        .unwrap(),
        factors: vec![],
        samples: serde_json::json!({"source_stamp":"original"}),
        dependency_ranges: serde_json::json!({}),
        missing_counts: Default::default(),
        missing_scope_counts: Default::default(),
        warnings: vec![],
        artifact: None,
    });
    // Control the freshness probe so this test needs no market files or native handle.
    h.app.state.factor.check_probe_at = Some(std::time::Instant::now());
    h.app.state.factor.check_source_stamp = Some("original".into());
    h.frame(vec![]);
    assert!(rendered_text(&h.frame(vec![])).contains("组合数据检查未通过"));
    h.app.state.factor.horizon += 1;
    assert!(rendered_text(&h.frame(vec![])).contains("组合数据报告已过期"));
    h.app.state.factor.horizon -= 1;
    h.app.state.factor.check_source_stamp = Some("changed".into());
    assert!(rendered_text(&h.frame(vec![])).contains("组合数据报告已过期"));
}

#[test]
fn factor_evaluation_button_submits_independent_cancellable_request() {
    let mut h = Harness::new(Panel::Factor);
    h.click("因子有效性测试");
    let command = h.commands.try_recv().unwrap();
    match command {
        WorkerCommand::EvaluateFactors {
            request, cancel, ..
        } => {
            assert_eq!(request.horizon, 5);
            assert_eq!(request.quantiles, 5);
            assert_eq!(request.min_samples, 30);
            assert_eq!(request.input.factor_ids, vec!["alpha_001", "alpha_002"]);
            h.app
                .cancel
                .as_ref()
                .unwrap()
                .store(true, Ordering::Relaxed);
            assert!(cancel.load(Ordering::Relaxed));
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn formula_editor_saves_revisions_applies_mixed_selection_and_submits_preview() {
    let dir = tempfile::tempdir().unwrap();
    let w = analytics_workflow::Workspace::open(dir.path()).unwrap();
    let mut h = Harness::new(Panel::Factor);
    h.app.state.platform.workspace = Some(w.clone());
    h.frame(vec![]);
    h.click("新建 / 编辑自定义因子");
    for _ in 0..16 {
        h.frame(vec![]);
    }
    h.click("校验公式");
    h.frame(vec![]);
    assert_eq!(h.app.state.factor.compiled.as_ref().unwrap().lookback, 21);
    h.click("保存新修订");
    h.frame(vec![]);
    assert_eq!(h.app.state.factor.draft.expected_revision, Some(1));
    assert_eq!(
        analytics_workflow::custom_factors::list(&w, false)
            .unwrap()
            .len(),
        1
    );
    let d = analytics_workflow::custom_factors::list(&w, false).unwrap()[0]
        .definition
        .clone();
    h.app.state.factor.factor_refs = vec![
        analytics_factor::selection::FactorRef::Custom {
            definition: Box::new(d),
        },
        analytics_factor::selection::FactorRef::builtin(
            analytics_core::research_quality::FactorSet::Alpha158,
            "alpha158_001_KMID",
        ),
        analytics_factor::selection::FactorRef::builtin(Default::default(), "alpha_002"),
    ];
    h.app.state.factor.selected = h
        .app
        .state
        .factor
        .factor_refs
        .iter()
        .map(|r| r.id().to_owned())
        .collect();
    h.click("应用到研究配置");
    assert_eq!(h.app.state.platform.template.config.factor_refs.len(), 3);
    h.app.state.platform.template.validate().unwrap();
    h.click("合成样本预览");
    assert!(matches!(
        h.commands.try_recv().unwrap(),
        WorkerCommand::PreviewFactor { .. }
    ));
}

#[test]
fn independent_training_and_backtest_editors_dispatch_separate_plans() {
    use analytics_workflow::research::backtest::independent::{Job, TrainingPlan};
    let (mut h, _tmp) = platform_harness(Panel::Research);
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let (market, actions, t) = analytics_workflow::testing::synthetic(&w);
    h.app.state.platform.template = t;
    h.app.state.platform.market = market.id;
    h.app.state.platform.actions = actions.id;
    h.ctx.all_styles_mut(|s| s.animation_time = 0.);
    h.click("独立训练与跨池回测");
    h.click("从当前配置生成训练计划");
    let plan: TrainingPlan =
        serde_json::from_str(&h.app.state.platform.independent_training_editor).unwrap();
    let raw: serde_json::Value = serde_json::to_value(&plan).unwrap();
    assert!(!raw["schedule"].to_string().contains("test"));
    h.click("提交独立训练");
    let command = h.commands.try_recv().unwrap();
    assert!(
        matches!(command,WorkerCommand::ExecuteWorkflow {request,..} if matches!(&*request,analytics_workflow::WorkflowRequest::Independent {request} if matches!(&**request,Job::Train {..})))
    );
    h.finish_recorded_task();
    h.app.state.platform.training_job = uuid::Uuid::new_v4().to_string();
    h.app.state.platform.start = "2011-01-01".into();
    h.app.state.platform.end = "2012-01-01".into();
    h.click("生成独立回测计划");
    let backtest: serde_json::Value =
        serde_json::from_str(&h.app.state.platform.independent_backtest_editor).unwrap();
    assert_eq!(backtest["range"]["start"], "2011-01-01");
    assert_eq!(backtest["models"]["mode"], "rolling");
}

#[test]
fn independent_app_trains_a_queries_models_previews_and_executes_b() {
    use analytics_workflow::research::backtest::independent::BacktestPlan;
    let (mut h, _tmp) = platform_harness(Panel::Research);
    let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
    let (m, a, mut t) = analytics_workflow::testing::synthetic(&w);
    let pool = |name: &str, id: &str| {
        w.save_pool(analytics_core::pool::PoolDefinition {
            schema_version: 3,
            name: name.into(),
            coverage: analytics_core::DateRange {
                start: "2007-01-02".parse().unwrap(),
                end: "2010-01-12".parse().unwrap(),
            },
            expression: analytics_core::pool::PoolExpression::Symbols {
                instruments: [id.to_owned()].into(),
            },
            adjustments: vec![],
        })
        .unwrap()
        .id
    };
    t.pool_version = pool("App train A", "SH600000");
    let b = pool("App execute B", "SH600001");
    let (worker, events) = worker::spawn_in_workspace(h.ctx.clone(), Some(w.root().to_owned()));
    h.app.worker = worker;
    h.app.events = events;
    h.app.state.platform.template = t;
    h.app.state.platform.datasets = w.datasets().unwrap();
    h.app.state.platform.market = m.id;
    h.app.state.platform.actions = a.id;
    h.app.state.platform.start = "2010-01-02".into();
    h.app.state.platform.end = "2010-01-12".into();
    h.ctx.all_styles_mut(|s| s.animation_time = 0.);
    h.click("独立训练与跨池回测");
    h.click("从当前配置生成训练计划");
    h.click("提交独立训练");
    h.wait_for(|app| !app.state.busy() && app.state.platform.details["schema_version"] == 5);
    let training = h.app.state.platform.training_job.clone();
    assert!(!training.is_empty());
    h.click("查询已训练模型");
    h.wait_for(|app| !app.state.busy() && app.state.platform.details["training"] == training);
    h.click("生成独立回测计划");
    let mut p: BacktestPlan =
        serde_json::from_str(&h.app.state.platform.independent_backtest_editor).unwrap();
    p.pool_version = b;
    h.app.state.platform.independent_backtest_editor = serde_json::to_string_pretty(&p).unwrap();
    h.frame(vec![
        Event::PointerMoved(Pos2::new(800., 600.)),
        Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(0., -700.),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: Default::default(),
        },
    ]);
    for _ in 0..16 {
        h.frame(vec![]);
    }
    h.click("预检B与模型切换");
    h.wait_for(|app| {
        !app.state.busy() && app.state.platform.details["model_selection"]["segments"].is_array()
    });
    h.click("提交独立模型回测");
    h.wait_for(|app| {
        !app.state.busy() && app.state.platform.details["stages"]["backtest"] == "completed"
    });
    assert_eq!(
        h.app.state.platform.details["cross_pool"]["label"],
        "完全未见标的回测"
    );
}

#[test]
fn evidence_maintenance_submits_shared_status_and_scaffold_requests() {
    for scaffold in [false, true] {
        let (mut h, _tmp) = platform_harness(Panel::Research);
        let w = h.app.state.platform.workspace.as_ref().unwrap().clone();
        let (m, a, t) = analytics_workflow::testing::synthetic(&w);
        h.app.state.platform.template = t;
        h.app.state.platform.datasets = w.datasets().unwrap();
        h.app.state.platform.market = m.id;
        h.app.state.platform.actions = a.id.clone();
        h.app.state.platform.action_review_instrument = "SH600030".into();
        h.app.state.platform.action_evidence_manifest = "data/announcements.json".into();
        h.ctx.all_styles_mut(|s| s.animation_time = 0.0);
        h.click("证据维护：公告留存、批次恢复与修订草稿");
        h.click(if scaffold {
            "生成本证券修订草稿"
        } else {
            "查看本证券缺口"
        });
        let command = h.commands.try_recv().unwrap();
        match command {
            WorkerCommand::ExecuteWorkflow { request, .. } => match *request {
                analytics_workflow::WorkflowRequest::Evidence {
                    request:
                        analytics_workflow::action_evidence::EvidenceRequest::Status {
                            actions,
                            instrument,
                            task: _,
                        },
                } => {
                    assert!(!scaffold);
                    assert_eq!(actions, a.id);
                    assert_eq!(instrument.as_deref(), Some("SH600030"));
                }
                analytics_workflow::WorkflowRequest::Evidence {
                    request:
                        analytics_workflow::action_evidence::EvidenceRequest::Draft {
                            actions,
                            request: None,
                            instrument,
                            ..
                        },
                } => {
                    assert!(scaffold);
                    assert_eq!(actions, a.id);
                    assert_eq!(instrument.as_deref(), Some("SH600030"));
                }
                other => panic!("unexpected workflow {other:?}"),
            },
            _ => panic!("unexpected command"),
        }
    }
}

#[test]
fn index_catalogue_selection_preserves_factors_and_does_not_start_tasks() {
    let (mut h, _tmp) = platform_harness(Panel::Dataset);
    h.ctx.all_styles_mut(|s| s.animation_time = 0.0);
    let factors = h.app.state.platform.template.config.factor_ids.clone();
    h.click("常见指数目录");
    let text = rendered_text(&h.frame(vec![]));
    assert!(text.contains("MSCI中国A50互联互通人民币指数"));
    assert!(text.contains("待接数据源"));
    h.click("中证1000");
    h.click("加入行情采集列表");
    assert!(h.app.state.platform.collection_symbols.contains("SH000852"));
    assert!(
        h.app
            .state
            .platform
            .collection_benchmarks
            .contains("SH000852")
    );
    h.click("设为研究股票池与基准");
    assert_eq!(
        h.app.state.platform.template.pool_version,
        "builtin:csi1000"
    );
    assert_eq!(h.app.state.platform.template.benchmark, "SH000852");
    assert_eq!(h.app.state.platform.template.config.factor_ids, factors);
    assert!(h.commands.try_recv().is_err());
}

#[test]
fn runtime_index_editor_submits_shared_save_without_touching_models() {
    let (mut h, _tmp) = platform_harness(Panel::Dataset);
    h.ctx.all_styles_mut(|s| s.animation_time = 0.0);
    let before = h.app.state.platform.template.config.factor_ids.clone();
    let mut config = analytics_core::indices::builtin_config("csi1000").unwrap();
    config.definition.id = "app_index".into();
    config.definition.symbol = None;
    config.definition.official_code = "APP001".into();
    config.data_key = "IDX_app_index".into();
    config.mappings = std::collections::BTreeMap::from([("hithink".into(), "APP001.CSI".into())]);
    h.app.state.platform.index_editor = serde_json::to_string_pretty(&config).unwrap();
    h.click("维护指数配置");
    h.click("校验并保存新版本");
    match h.commands.try_recv().unwrap() {
        WorkerCommand::ExecuteWorkflow { request, .. } => match *request {
            analytics_workflow::WorkflowRequest::Index {
                request: analytics_workflow::indices::IndexOperation::Save { config: c },
            } => assert_eq!(*c, config),
            other => panic!("unexpected workflow {other:?}"),
        },
        other => panic!("unexpected command {other:?}"),
    }
    assert_eq!(h.app.state.platform.template.config.factor_ids, before);
}

#[test]
fn runtime_index_msci_collection_uses_selected_calendar_and_shared_task() {
    let (mut h, _tmp) = platform_harness(Panel::Dataset);
    h.ctx.all_styles_mut(|s| s.animation_time = 0.0);
    let mut config = analytics_core::indices::builtin_config("csi1000").unwrap();
    config.definition.id = "app_msci".into();
    config.definition.symbol = None;
    config.definition.official_code = "746059".into();
    config.data_key = "IDX_app_msci".into();
    config.mappings = std::collections::BTreeMap::from([("msci".into(), "746059".into())]);
    h.app.state.platform.index_version = "index-fixture".into();
    h.app.state.platform.index_versions = vec![analytics_workflow::indices::IndexVersion {
        id: "index-fixture".into(),
        signature: String::new(),
        calculation_signature: String::new(),
        builtin: false,
        config,
    }];
    h.app.state.platform.market = "market-fixture".into();
    h.app.state.platform.start = "2026-03-30".into();
    h.app.state.platform.end = "2026-03-31".into();
    h.click("维护指数配置");
    h.click("从 MSCI 官方采集收盘行情");
    match h.commands.try_recv().unwrap() {
        WorkerCommand::ExecuteWorkflow { request, .. } => match *request {
            analytics_workflow::WorkflowRequest::Index {
                request:
                    analytics_workflow::indices::IndexOperation::CollectMsci {
                        version,
                        calendar_market,
                        start,
                        end,
                    },
            } => {
                assert_eq!(version, "index-fixture");
                assert_eq!(calendar_market, "market-fixture");
                assert_eq!(start.to_string(), "2026-03-30");
                assert_eq!(end.to_string(), "2026-03-31");
            }
            other => panic!("unexpected workflow {other:?}"),
        },
        other => panic!("unexpected command {other:?}"),
    }
}

#[test]
fn history_editor_submits_shared_draft_without_network_or_model_changes() {
    let (mut h, _tmp) = platform_harness(Panel::Dataset);
    h.ctx.all_styles_mut(|s| s.animation_time = 0.0);
    h.app.state.platform.index_version = "builtin:csi300".into();
    h.app.state.platform.market = "market-fixture".into();
    h.app.state.platform.start = "2021-09-17".into();
    h.app.state.platform.end = "2026-10-12".into();
    let original = serde_json::to_value(&h.app.state.platform.template).unwrap();
    h.click("历史成分重建");
    h.click("生成历史成员草稿");
    match h.commands.try_recv().unwrap() {
        WorkerCommand::ExecuteWorkflow { request, .. } => match *request {
            analytics_workflow::WorkflowRequest::Index {
                request: analytics_workflow::indices::IndexOperation::History { request },
            } => match *request {
                analytics_workflow::membership_history::Operation::Draft {
                    index,
                    market,
                    range,
                } => {
                    assert_eq!(index, "builtin:csi300");
                    assert_eq!(market, "market-fixture");
                    assert_eq!(range.start.to_string(), "2021-09-17");
                }
                other => panic!("unexpected history operation {other:?}"),
            },
            other => panic!("unexpected workflow {other:?}"),
        },
        other => panic!("unexpected worker {other:?}"),
    }
    assert_eq!(
        serde_json::to_value(&h.app.state.platform.template).unwrap(),
        original
    );
}

#[test]
fn allocation_controls_do_not_silently_clamp_existing_valid_advanced_parameters() {
    let (mut h, _tmp) = platform_harness(Panel::Research);
    let c = &mut h.app.state.platform.template.config.allocation;
    c.method = analytics_workflow::allocation::AllocationMethod::MeanVariance;
    c.max_stock_weight = 0.005;
    c.max_strategy_weight = 0.02;
    c.risk_aversion = Some(1e9);
    c.calibration_min_pairs = 15000;
    c.validate().unwrap();
    let before = c.clone();
    h.click("优化配置与多策略统一账户");
    for _ in 0..3 {
        h.frame(vec![]);
    }
    assert_eq!(h.app.state.platform.template.config.allocation, before);
}
