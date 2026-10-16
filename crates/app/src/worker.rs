//! Egui adapter for the shared backend and local service.
#[cfg(test)]
use analytics_core::{ExportRequest, PageRequest};
#[cfg(test)]
use analytics_engine::{DuckDbEngine, QueryEngine};
pub use analytics_workflow::backend::{
    RunSummary, TaskKind, WorkerCommand, WorkerEvent, WorkerHandle,
};
#[cfg(test)]
use analytics_workflow::backend::{TestWorker as Worker, pipeline_error_message};
#[cfg(test)]
use analytics_workflow::research::TrainingConfig;

#[cfg(test)]
use analytics_workflow::research::pipeline::{PipelineError, PipelineRequest};

#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::sync::mpsc::{Receiver, channel};
#[cfg(test)]
use uuid::Uuid;

#[cfg(test)]
pub fn spawn(ctx: egui::Context) -> (WorkerHandle, std::sync::mpsc::Receiver<WorkerEvent>) {
    spawn_in_workspace(ctx, None)
}
pub fn spawn_in_workspace(
    ctx: egui::Context,
    root: Option<std::path::PathBuf>,
) -> (WorkerHandle, std::sync::mpsc::Receiver<WorkerEvent>) {
    let wake = analytics_workflow::backend::Wake::new(move || ctx.request_repaint());
    #[cfg(not(test))]
    if let Some(root) = root {
        return analytics_workflow::service::bridge(wake, root);
    }
    analytics_workflow::backend::spawn_in_workspace(wake, root)
}
#[cfg(test)]
mod tests {
    use super::*;
    use analytics_core::{ExportFormat, ScalarValue, SortKey};
    use analytics_workflow::research::pipeline::cancellation;
    use std::time::{Duration, Instant};

    fn collect(rx: &Receiver<WorkerEvent>, task: Uuid) -> Vec<WorkerEvent> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut events = Vec::new();
        loop {
            let event = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("worker must terminate task within 30 seconds");
            let terminal = matches!(&event, WorkerEvent::Finished { task: id }
                | WorkerEvent::Failed { task: id, .. } if *id == task);
            events.push(event);
            if terminal {
                return events;
            }
        }
    }

    fn successful(events: &[WorkerEvent], task: Uuid) {
        assert!(
            matches!(events.first(), Some(WorkerEvent::Started { task: id, .. }) if *id == task)
        );
        assert!(
            matches!(events.last(), Some(WorkerEvent::Finished { task: id }) if *id == task),
            "{events:?}"
        );
    }

    #[test]
    fn desktop_pages_preserve_duplicate_and_null_keys_and_quote_column_names() {
        let (tx, _) = channel();
        let mut worker = Worker::new(tx);
        for descending in [false, true] {
            let sql = "SELECT * FROM (VALUES (NULL, 5), (1, 2), (1, 2), (1, 3), (2, 4), (NULL, 6)) AS v(\"排序 列\", \"__JFQUANT_PAGE_ROW\")";
            let mut after = None;
            let mut rows = Vec::new();
            let mut pages = 0;
            loop {
                let page = worker
                    .query_page(PageRequest {
                        sql: sql.into(),
                        limit: 2,
                        sort: vec![SortKey {
                            column: "排序 列".into(),
                            descending,
                        }],
                        after,
                    })
                    .unwrap();
                assert_eq!(page.columns.len(), 2);
                assert_eq!(page.columns[1].name, "__JFQUANT_PAGE_ROW");
                rows.extend(page.rows);
                after = page.next_cursor;
                pages += 1;
                assert!(pages <= 3, "pagination did not terminate");
                if after.is_none() {
                    break;
                }
            }
            let pairs = if descending {
                vec![
                    (Some(2), 4),
                    (Some(1), 2),
                    (Some(1), 2),
                    (Some(1), 3),
                    (None, 5),
                    (None, 6),
                ]
            } else {
                vec![
                    (Some(1), 2),
                    (Some(1), 2),
                    (Some(1), 3),
                    (Some(2), 4),
                    (None, 5),
                    (None, 6),
                ]
            };
            let expected: Vec<_> = pairs
                .into_iter()
                .map(|(key, id)| {
                    vec![
                        key.map(ScalarValue::Int64).unwrap_or(ScalarValue::Null),
                        ScalarValue::Int64(id),
                    ]
                })
                .collect();
            assert_eq!(rows, expected);
        }
    }

    #[test]
    fn desktop_snapshot_survives_source_changes_and_rejects_mismatched_requests() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.parquet");
        let (tx, _) = channel();
        let mut worker = Worker::new(tx);
        worker
            .engine()
            .unwrap()
            .export(ExportRequest {
                sql: "SELECT range AS id FROM range(6)".into(),
                destination: path.clone(),
                format: ExportFormat::Parquet,
                compression: None,
            })
            .unwrap();
        let mut request = PageRequest {
            sql: format!(
                "SELECT * FROM read_parquet('{}')",
                path.display().to_string().replace('\'', "''")
            ),
            limit: 2,
            sort: vec![],
            after: None,
        };
        let first = worker.query_page(request.clone()).unwrap();
        request.after = first.next_cursor;
        std::fs::remove_file(&path).unwrap();
        let second = worker.query_page(request.clone()).unwrap();
        assert_eq!(
            second.rows,
            vec![vec![ScalarValue::Int64(2)], vec![ScalarValue::Int64(3)]]
        );
        let mut mismatch = request.clone();
        mismatch.limit = 3;
        assert!(
            worker
                .query_page(mismatch)
                .unwrap_err()
                .contains("查询条件已改变")
        );
        request.after = second.next_cursor;
        assert_eq!(
            worker.query_page(request.clone()).unwrap().rows,
            vec![vec![ScalarValue::Int64(4)], vec![ScalarValue::Int64(5)]]
        );
        let engine = worker.engine().unwrap();
        let snapshot = engine
            .replace_query_snapshot("SELECT random() AS x")
            .unwrap();
        let read = PageRequest {
            sql: snapshot,
            limit: 1,
            sort: vec![],
            after: None,
        };
        assert_eq!(
            engine.query_page(read.clone()).unwrap().rows,
            engine.query_page(read).unwrap().rows
        );
        assert!(engine.replace_query_snapshot("SELECT FROM").is_err());
        assert!(
            worker
                .query_page(PageRequest {
                    after: None,
                    ..request.clone()
                })
                .is_err()
        );
        assert!(worker.query_page(request).unwrap_err().contains("失效"));
    }

    #[test]
    fn engine_keyset_handles_null_composite_keys_and_nonfinite_numbers() {
        let mut engine = DuckDbEngine::open_in_memory().unwrap();
        for descending in [false, true] {
            let mut after = None;
            let mut ids = Vec::new();
            for _ in 0..6 {
                let page = engine.query_page(PageRequest {
                    sql: "SELECT * FROM (VALUES (1, 1), (2, 2), (NULL, 3), (NULL, 4)) AS v(k, id)".into(), limit: 1,
                    sort: vec![SortKey { column: "K".into(), descending }, SortKey { column: "id".into(), descending: false }], after,
                }).unwrap();
                ids.extend(page.rows.iter().map(|row| row[1].clone()));
                after = page.next_cursor;
                if after.is_none() {
                    break;
                }
            }
            assert_eq!(
                ids,
                if descending {
                    vec![2, 1, 3, 4]
                } else {
                    vec![1, 2, 3, 4]
                }
                .into_iter()
                .map(ScalarValue::Int64)
                .collect::<Vec<_>>()
            );
            let mut after = None;
            let mut values = Vec::new();
            for _ in 0..6 {
                let page = engine.query_page(PageRequest {
                    sql: "SELECT * FROM (VALUES (CAST('-Infinity' AS DOUBLE)), (0.0), (CAST('Infinity' AS DOUBLE)), (CAST('NaN' AS DOUBLE)), (NULL)) AS v(k)".into(), limit: 1,
                    sort: vec![SortKey { column: "k".into(), descending }], after,
                }).unwrap();
                values.extend(page.rows.into_iter().flatten());
                after = page.next_cursor;
                if after.is_none() {
                    break;
                }
            }
            assert_eq!(values.len(), 5, "{values:?}");
            assert!(matches!(values.last(), Some(ScalarValue::Null)));
            let floats: Vec<_> = values[..4]
                .iter()
                .map(|value| match value {
                    ScalarValue::Float64(value) => *value,
                    _ => panic!("{value:?}"),
                })
                .collect();
            if descending {
                assert!(floats[0].is_nan());
                assert_eq!(&floats[1..], &[f64::INFINITY, 0.0, f64::NEG_INFINITY]);
            } else {
                assert_eq!(&floats[..3], &[f64::NEG_INFINITY, 0.0, f64::INFINITY]);
                assert!(floats[3].is_nan());
            }
        }
    }

    #[test]
    fn engine_rejects_ambiguous_page_boundary_and_does_not_emit_unsorted_empty_cursor() {
        let mut engine = DuckDbEngine::open_in_memory().unwrap();
        let request = PageRequest {
            sql: "SELECT 1 AS id FROM range(5)".into(),
            limit: 2,
            sort: vec![],
            after: None,
        };
        assert!(
            engine
                .query_page(request.clone())
                .unwrap()
                .next_cursor
                .is_none()
        );
        let mut request = request;
        request.sort = vec![SortKey {
            column: "id".into(),
            descending: false,
        }];
        assert!(
            engine
                .query_page(request)
                .unwrap_err()
                .to_string()
                .contains("not unique")
        );
    }

    #[test]
    fn export_filename_in_current_directory_succeeds() {
        let mut engine = DuckDbEngine::open_in_memory().unwrap();
        let path = PathBuf::from(format!("jfquant-test-{}.csv", Uuid::new_v4()));
        // A basename has an empty parent; it must not be passed to create_dir_all.
        let outcome = engine.export(ExportRequest {
            sql: "SELECT 1 AS id".into(),
            destination: path.clone(),
            format: ExportFormat::Csv,
            compression: None,
        });
        let content = std::fs::read_to_string(&path);
        let _ = std::fs::remove_file(&path);
        outcome.unwrap();
        assert_eq!(content.unwrap(), "id\n1\n");
    }

    #[test]
    fn cancellation_words_in_sql_errors_are_not_misclassified_as_user_cancellation() {
        let (worker, rx) = spawn(egui::Context::default());
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::DescribeQuery {
                task,
                sql: "SELECT * FROM cancelled_table".into(),
            })
            .unwrap();
        let events = collect(&rx, task);
        assert!(
            matches!(events.last(), Some(WorkerEvent::Failed { message, .. }) if message.contains("cancelled_table") && message != "任务已取消")
        );
        assert_eq!(
            pipeline_error_message(PipelineError::Cancelled),
            "任务已取消"
        );
        assert_eq!(
            pipeline_error_message(PipelineError::Ml(
                analytics_workflow::research::MlError::Native("training cancelled".into())
            )),
            "任务已取消"
        );
        assert!(
            pipeline_error_message(PipelineError::InvalidInput("cancelled_table".into()))
                .contains("cancelled_table")
        );
    }

    #[test]
    fn task_panics_report_failure_for_every_kind_and_retire_worker() {
        for kind in [
            TaskKind::Scan,
            TaskKind::Describe,
            TaskKind::Query,
            TaskKind::Export,
            TaskKind::Pipeline,
            TaskKind::Training,
        ] {
            let (worker, rx) = spawn(egui::Context::default());
            let task = Uuid::new_v4();
            worker
                .send(WorkerCommand::PanicTask { task, kind })
                .unwrap();
            let events = collect(&rx, task);
            assert!(
                matches!(events.last(), Some(WorkerEvent::Failed { task: id, kind: failed_kind, message })
                if *id == task && *failed_kind == kind && message.contains("模拟后台内部错误")),
                "{events:?}"
            );
            assert!(
                matches!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), WorkerEvent::Unavailable { message }
                if message.contains("模拟后台内部错误"))
            );
            assert!(matches!(
                rx.recv_timeout(Duration::from_secs(10)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            ));
            assert!(
                worker
                    .send(WorkerCommand::DescribeQuery {
                        task: Uuid::new_v4(),
                        sql: "SELECT 1".into()
                    })
                    .is_err()
            );
        }
    }

    #[test]
    fn panic_outside_task_reports_worker_unavailable() {
        let (worker, rx) = spawn(egui::Context::default());
        worker.send(WorkerCommand::PanicOutsideTask).unwrap();
        assert!(
            matches!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), WorkerEvent::Unavailable { message }
            if message.contains("模拟任务外内部错误"))
        );
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(10)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn owned_and_non_string_panic_payloads_become_errors() {
        for owned_string in [true, false] {
            let (tx, rx) = channel();
            let mut worker = Worker::new(tx);
            let task = Uuid::new_v4();
            worker.run_task(task, TaskKind::Query, "测试 panic 内容".into(), || {
                if owned_string {
                    std::panic::panic_any(String::from("动态错误内容"));
                }
                std::panic::panic_any(42_u32);
            });
            let expected = if owned_string {
                "动态错误内容"
            } else {
                "未知 panic"
            };
            let events = collect(&rx, task);
            assert!(
                matches!(events.last(), Some(WorkerEvent::Failed { message, .. }) if message.contains(expected))
            );
            assert!(worker.stop_reason().unwrap().contains(expected));
        }
    }

    #[test]
    fn file_scan_export_pipeline_and_training_errors_keep_worker_usable() {
        let dir = tempfile::tempdir().unwrap();
        let invalid = dir.path().join("invalid.parquet");
        std::fs::write(&invalid, b"not a parquet file").unwrap();
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, b"a file cannot be an output directory").unwrap();
        let (worker, rx) = spawn(egui::Context::default());
        let scan = Uuid::new_v4();
        let export = Uuid::new_v4();
        let pipeline = Uuid::new_v4();
        let training = Uuid::new_v4();
        let cases = [
            (
                scan,
                TaskKind::Scan,
                WorkerCommand::ScanDataset {
                    task: scan,
                    root: invalid,
                },
            ),
            (
                export,
                TaskKind::Export,
                WorkerCommand::ExportQuery {
                    task: export,
                    request: ExportRequest {
                        sql: "SELECT 1 AS id".into(),
                        destination: blocked.join("query.csv"),
                        format: ExportFormat::Csv,
                        compression: None,
                    },
                },
            ),
            (
                pipeline,
                TaskKind::Pipeline,
                WorkerCommand::RunPipeline {
                    task: pipeline,
                    request: PipelineRequest {
                        factor_refs: Vec::new(),
                        factor_set: Default::default(),
                        market_path: dir.path().join("missing-market.parquet"),
                        universe_path: dir.path().join("missing-universe.parquet"),
                        universe_id: "test".into(),
                        date_range: crate::state::parse_date_range("2024-01-01", "2024-01-30")
                            .unwrap(),
                        factor_ids: vec!["alpha_001".into()],
                        output_dir: dir.path().join("pipeline"),
                        training: TrainingConfig::default(),
                    },
                    cancel: cancellation(),
                },
            ),
            (
                training,
                TaskKind::Training,
                WorkerCommand::TrainFromCache {
                    task: training,
                    cache_path: dir.path().join("missing-cache.parquet"),
                    universe_id: None,
                    output_dir: dir.path().join("training"),
                    config: TrainingConfig::default(),
                    cancel: cancellation(),
                },
            ),
        ];
        for (task, kind, command) in cases {
            worker.send(command).unwrap();
            let events = collect(&rx, task);
            assert!(
                matches!(events.last(), Some(WorkerEvent::Failed { task: id, kind: failed_kind, message })
                if *id == task && *failed_kind == kind && !message.is_empty()),
                "{events:?}"
            );
            let mut state = crate::state::AppState::default();
            for event in &events {
                crate::state::apply_event(&mut state, event);
            }
            assert!(!state.busy());
            assert!(state.worker_error.is_none());
        }
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::DescribeQuery {
                task,
                sql: "SELECT 1 AS recovered".into(),
            })
            .unwrap();
        successful(&collect(&rx, task), task);
    }

    fn export(
        worker: &WorkerHandle,
        rx: &Receiver<WorkerEvent>,
        sql: &str,
        path: PathBuf,
        format: ExportFormat,
    ) {
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::ExportQuery {
                task,
                request: ExportRequest {
                    sql: sql.into(),
                    destination: path.clone(),
                    format,
                    compression: None,
                },
            })
            .unwrap();
        let events = collect(rx, task);
        successful(&events, task);
        assert!(events.iter().any(|event| matches!(event, WorkerEvent::Exported { destination, .. } if *destination == path)));
        assert!(path.is_file());
    }

    #[test]
    fn worker_describe_query_and_keyset_pages_use_real_duckdb() {
        let (worker, rx) = spawn(egui::Context::default());
        let sql = "SELECT range AS id FROM range(5)";
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::DescribeQuery {
                task,
                sql: sql.into(),
            })
            .unwrap();
        let events = collect(&rx, task);
        successful(&events, task);
        assert!(events.iter().any(
            |event| matches!(event, WorkerEvent::QueryDescribed { columns, .. }
            if columns.len() == 1 && columns[0].name == "id")
        ));
        for descending in [false, true] {
            let mut after = None;
            let mut values = Vec::new();
            loop {
                let task = Uuid::new_v4();
                worker
                    .send(WorkerCommand::RunQuery {
                        task,
                        request: PageRequest {
                            sql: sql.into(),
                            limit: 2,
                            sort: vec![SortKey {
                                column: "id".into(),
                                descending,
                            }],
                            after,
                        },
                    })
                    .unwrap();
                let events = collect(&rx, task);
                successful(&events, task);
                let page = events
                    .iter()
                    .find_map(|event| match event {
                        WorkerEvent::QueryPaged { page, .. } => Some(page),
                        _ => None,
                    })
                    .unwrap();
                values.extend(page.rows.iter().map(|row| match row[0] {
                    ScalarValue::Int64(id) => id,
                    ref value => panic!("unexpected {value:?}"),
                }));
                after = page.next_cursor.clone();
                if after.is_none() {
                    break;
                }
            }
            assert_eq!(
                values,
                if descending {
                    vec![4, 3, 2, 1, 0]
                } else {
                    vec![0, 1, 2, 3, 4]
                }
            );
        }
    }

    #[test]
    fn worker_exports_csv_and_parquet_then_scans_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let (worker, rx) = spawn(egui::Context::default());
        let csv = dir.path().join("result.csv");
        let parquet = dir.path().join("result.parquet");
        let sql = "SELECT range AS id FROM range(3)";
        export(&worker, &rx, sql, csv.clone(), ExportFormat::Csv);
        let text = std::fs::read_to_string(csv).unwrap();
        assert_eq!(text.lines().collect::<Vec<_>>(), vec!["id", "0", "1", "2"]);
        export(&worker, &rx, sql, parquet.clone(), ExportFormat::Parquet);
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::ScanDataset {
                task,
                root: dir.path().into(),
            })
            .unwrap();
        let events = collect(&rx, task);
        successful(&events, task);
        let files = events
            .iter()
            .find_map(|event| match event {
                WorkerEvent::DatasetScanned { files, .. } => Some(files),
                _ => None,
            })
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].rows, Some(3));
        assert_eq!(files[0].schema[0].name, "id");
    }

    #[test]
    fn query_results_preserve_null_numeric_boolean_date_and_text_values() {
        let (worker, rx) = spawn(egui::Context::default());
        let task = Uuid::new_v4();
        worker.send(WorkerCommand::RunQuery { task, request: PageRequest {
            sql: "SELECT NULL AS missing, true AS flag, 7::BIGINT AS count, 1.25::DOUBLE AS price, DATE '2024-01-02' AS day, 'hello' AS name".into(),
            limit: 100, sort: vec![], after: None,
        }}).unwrap();
        let events = collect(&rx, task);
        successful(&events, task);
        let page = events
            .iter()
            .find_map(|event| match event {
                WorkerEvent::QueryPaged { page, .. } => Some(page),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            page.rows[0],
            vec![
                ScalarValue::Null,
                ScalarValue::Boolean(true),
                ScalarValue::Int64(7),
                ScalarValue::Float64(1.25),
                ScalarValue::Date(chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap()),
                ScalarValue::Utf8("hello".into())
            ]
        );
    }

    #[test]
    fn export_preserves_sql_literals_and_quotes_destination_paths() {
        let dir = tempfile::tempdir().unwrap();
        let (worker, rx) = spawn(egui::Context::default());
        let path = dir.path().join("it's-a-query.csv");
        export(
            &worker,
            &rx,
            "SELECT 'it''s valid' AS text",
            path.clone(),
            ExportFormat::Csv,
        );
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("it's valid")
        );
    }

    #[test]
    fn worker_reports_sql_failure_and_recovers_for_next_command() {
        let (worker, rx) = spawn(egui::Context::default());
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::DescribeQuery {
                task,
                sql: "SELECT * FROM missing_table".into(),
            })
            .unwrap();
        let events = collect(&rx, task);
        assert!(
            matches!(events.last(), Some(WorkerEvent::Failed { kind: TaskKind::Describe, message, .. }) if !message.is_empty())
        );
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::DescribeQuery {
                task,
                sql: "SELECT 1 AS recovered".into(),
            })
            .unwrap();
        successful(&collect(&rx, task), task);
    }

    #[test]
    fn missing_parquet_glob_returns_query_error_and_worker_remains_usable() {
        let dir = tempfile::tempdir().unwrap();
        let (worker, rx) = spawn(egui::Context::default());
        let task = Uuid::new_v4();
        let glob = dir
            .path()
            .join("missing/**/*.parquet")
            .display()
            .to_string()
            .replace('\'', "''");
        worker
            .send(WorkerCommand::RunQuery {
                task,
                request: PageRequest {
                    sql: format!("SELECT * FROM read_parquet('{glob}')"),
                    limit: 100,
                    sort: vec![],
                    after: None,
                },
            })
            .unwrap();
        let events = collect(&rx, task);
        assert!(
            matches!(events.last(), Some(WorkerEvent::Failed { kind: TaskKind::Query, message, .. })
            if message.contains("No files found")),
            "{events:?}"
        );
        let mut state = crate::state::AppState::default();
        for event in &events {
            crate::state::apply_event(&mut state, event);
        }
        assert!(!state.busy());
        assert!(
            state
                .query
                .error
                .as_ref()
                .unwrap()
                .contains("No files found")
        );
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::RunQuery {
                task,
                request: PageRequest {
                    sql: "SELECT 1 AS recovered".into(),
                    limit: 100,
                    sort: vec![],
                    after: None,
                },
            })
            .unwrap();
        successful(&collect(&rx, task), task);
    }

    #[test]
    fn cancelled_pipeline_reports_cancellation_and_never_emits_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let (worker, rx) = spawn(egui::Context::default());
        let cancel = analytics_workflow::research::pipeline::cancellation();
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::RunPipeline {
                task,
                cancel,
                request: PipelineRequest {
                    factor_refs: Vec::new(),
                    factor_set: Default::default(),
                    market_path: dir.path().join("missing"),
                    universe_path: dir.path().join("missing"),
                    universe_id: "test".into(),
                    date_range: crate::state::parse_date_range("2024-01-01", "2024-01-30").unwrap(),
                    factor_ids: vec!["alpha_001".into()],
                    output_dir: dir.path().join("out"),
                    training: TrainingConfig::default(),
                },
            })
            .unwrap();
        let events = collect(&rx, task);
        assert!(
            matches!(events.last(), Some(WorkerEvent::Failed { kind: TaskKind::Pipeline, message, .. }) if message == "任务已取消")
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, WorkerEvent::PipelineFinished { .. }))
        );
    }

    #[test]
    fn workspace_relative_query_export_and_scan_are_independent_of_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace'中文");
        let w = analytics_workflow::Workspace::open(&root).unwrap();
        let (worker, rx) = spawn_in_workspace(egui::Context::default(), Some(w.root().to_owned()));
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::ExportQuery {
                task,
                request: ExportRequest {
                    sql: "SELECT 42::BIGINT AS answer".into(),
                    destination: PathBuf::from("data/sample.parquet"),
                    format: ExportFormat::Parquet,
                    compression: None,
                },
            })
            .unwrap();
        successful(&collect(&rx, task), task);
        assert!(root.join("data/sample.parquet").exists());
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::RunQuery {
                task,
                request: PageRequest {
                    sql: "SELECT * FROM read_parquet('data/sample.parquet')".into(),
                    limit: 100,
                    sort: Vec::new(),
                    after: None,
                },
            })
            .unwrap();
        let events = collect(&rx, task);
        successful(&events, task);
        assert!(events.iter().any(|e|matches!(e,WorkerEvent::QueryPaged{page,..} if page.rows[0][0]==ScalarValue::Int64(42))));
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::ScanDataset {
                task,
                root: PathBuf::from("data"),
            })
            .unwrap();
        let events = collect(&rx, task);
        successful(&events, task);
        assert!(
            events
                .iter()
                .any(|e| matches!(e,WorkerEvent::DatasetScanned{files,..} if files.len()==1))
        );
    }

    #[test]
    fn worker_pipeline_and_cache_retraining_publish_real_artifacts_and_previews() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = analytics_workflow::Workspace::open(dir.path()).unwrap();
        let (worker, rx) =
            spawn_in_workspace(egui::Context::default(), Some(workspace.root().to_owned()));
        let market = dir.path().join("market.parquet");
        let universe = dir.path().join("universe.parquet");
        export(
            &worker,
            &rx,
            "SELECT 'stock' || i::VARCHAR AS instrument_id, DATE '2024-01-01' + d::INTEGER AS trade_date, \
            p AS open, p+1 AS high, p-1 AS low, p+0.2 AS close, \
            100.0::DOUBLE AS volume, 1000.0::DOUBLE AS amount, p AS adj_open, p+1 AS adj_high, \
            p-1 AS adj_low, p+0.2 AS adj_close, true AS is_trading, DATE '2023-01-01' AS listing_date \
            FROM (SELECT i, d, (10+i+d*0.05)::DOUBLE AS p FROM range(3) a(i), range(30) b(d))",
            market.clone(),
            ExportFormat::Parquet,
        );
        export(
            &worker,
            &rx,
            "SELECT 'stock' || i::VARCHAR AS instrument_id, 'test' AS universe_id, \
            DATE '2024-01-01' AS trade_date, true AS is_member FROM range(3) a(i)",
            universe.clone(),
            ExportFormat::Parquet,
        );
        let config = TrainingConfig {
            parameters: std::collections::BTreeMap::from([
                ("num_iterations".into(), "3".into()),
                ("num_leaves".into(), "7".into()),
                ("min_data_in_leaf".into(), "2".into()),
                ("num_threads".into(), "1".into()),
            ]),
            early_stopping_rounds: None,
        };
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::RunPipeline {
                task,
                cancel: analytics_workflow::research::pipeline::cancellation(),
                request: PipelineRequest {
                    factor_refs: Vec::new(),
                    factor_set: Default::default(),
                    market_path: market,
                    universe_path: universe,
                    universe_id: "test".into(),
                    date_range: crate::state::parse_date_range("2024-01-01", "2024-01-30").unwrap(),
                    factor_ids: vec!["alpha_001".into()],
                    output_dir: dir.path().join("pipeline"),
                    training: config.clone(),
                },
            })
            .unwrap();
        let events = collect(&rx, task);
        successful(&events, task);
        let summary = events
            .iter()
            .find_map(|event| match event {
                WorkerEvent::PipelineFinished { summary, .. } => Some(summary),
                _ => None,
            })
            .unwrap();
        assert!(summary.artifact.training.model_path.is_file());
        assert!(
            summary
                .metrics_json
                .as_ref()
                .unwrap()
                .contains("iterations")
        );
        assert_eq!(summary.importance.len(), 1);
        assert!(!summary.preview.as_ref().unwrap().rows.is_empty());
        let mut state = crate::state::AppState::default();
        for event in &events {
            crate::state::apply_event(&mut state, event);
        }
        assert!(!state.busy());
        assert!(state.factor.summary.is_some());
        let task = Uuid::new_v4();
        worker
            .send(WorkerCommand::TrainFromCache {
                task,
                cache_path: summary.artifact.factor_cache_path.clone().unwrap(),
                universe_id: None,
                output_dir: dir.path().join("retrain"),
                config,
                cancel: analytics_workflow::research::pipeline::cancellation(),
            })
            .unwrap();
        let events = collect(&rx, task);
        successful(&events, task);
        for event in &events {
            crate::state::apply_event(&mut state, event);
        }
        assert!(!state.busy());
        let summary = state.model.summary.unwrap();
        assert!(summary.artifact.predictions_path.is_file());
        assert!(!summary.preview.as_ref().unwrap().rows.is_empty());
        let records = workspace.runs().unwrap();
        assert_eq!(records.len(), 2);
        assert!(
            records
                .iter()
                .all(|r| r.status == analytics_workflow::RunStatus::Succeeded)
        );
        let saved = workspace.results(&records[0]).unwrap();
        assert!(saved["metrics"].is_object());
        assert!(
            saved["artifact"]["predictions_path"]
                .as_str()
                .unwrap()
                .starts_with(".jfquant/")
        );
    }
}
