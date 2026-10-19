//! macOS-first egui shell for local-first quant analytics.
//!
//! The egui thread only renders [`AppState`] and sends
//! [`worker::WorkerCommand`]s;
//! every DuckDB/LightGBM handle lives on the single worker thread spawned in
//! [`worker::spawn_in_workspace`]. Layout: left navigation rail, top task bar, central
//! panel, and a bottom status bar with a collapsible message log.
#![warn(missing_docs)]

mod custom_factors;
mod factor_checks;
mod factor_groups;
mod panels;
mod platform;
mod quality_ui;
mod schedule_ui;
mod state;
mod theme;
mod worker;

#[cfg(test)]
#[allow(dead_code)]
#[path = "../build_support.rs"]
mod build_support;
#[cfg(test)]
mod build_tests;
#[cfg(test)]
mod interaction_tests;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../native_paths.rs"]
mod native_paths;

use analytics_factor::FactorRegistry;
use analytics_workflow::research::pipeline::Cancellation;
use eframe::egui;
use state::{AppState, Panel, apply_event};
use std::sync::mpsc::Receiver;
use worker::{WorkerEvent, WorkerHandle};

struct DesktopApp {
    state: AppState,
    worker: WorkerHandle,
    events: Receiver<WorkerEvent>,
    cancel: Option<Cancellation>,
    registry: FactorRegistry,
}

impl DesktopApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install(&cc.egui_ctx);
        let mut state = AppState::default();
        platform::load_preferences(&mut state);
        let root = state
            .platform
            .workspace
            .as_ref()
            .map(|w| w.root().to_owned());
        let (worker, events) = worker::spawn_in_workspace(cc.egui_ctx.clone(), root);
        Self {
            state,
            worker,
            events,
            cancel: None,
            registry: FactorRegistry::default(),
        }
    }

    fn drain_events(&mut self) {
        loop {
            let event = match self.events.try_recv() {
                Ok(event) => event,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.cancel = None;
                    apply_event(
                        &mut self.state,
                        &WorkerEvent::Unavailable {
                            message: "后台连接已断开，请重启后台后重试。".into(),
                        },
                    );
                    break;
                }
            };
            if matches!(&event,
                WorkerEvent::Finished { task } | WorkerEvent::Failed { task, .. }
                if self.state.active_task.as_ref().is_some_and(|active| active.id == *task)
            ) || matches!(&event, WorkerEvent::Unavailable { .. })
            {
                self.cancel = None;
            }
            apply_event(&mut self.state, &event);
        }
    }

    fn restart_worker(&mut self, ctx: &egui::Context) {
        (self.worker, self.events) = worker::spawn_in_workspace(
            ctx.clone(),
            self.state
                .platform
                .workspace
                .as_ref()
                .map(|w| w.root().to_owned()),
        );
        self.cancel = None;
        self.state.worker_restarted();
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.add_space(4.0);
            let changed = platform::workspace_bar(ui, &mut self.state);
            if changed {
                self.restart_worker(ui.ctx());
            }
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("JFQuant Analytics").strong().size(16.0));
                ui.label(
                    egui::RichText::new("本地 Parquet · DuckDB · GTJA191 / Alpha158 · LightGBM")
                        .small()
                        .weak(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.state.worker_error.is_some() {
                        if ui.button("重启后台").clicked() {
                            self.restart_worker(ui.ctx());
                        }
                        ui.colored_label(theme::semantic(ui.ctx()).error, "后台不可用");
                    } else if let Some(task) = &self.state.active_task {
                        if ui
                            .add_enabled(
                                self.cancel.is_some() || self.state.platform.workspace.is_some(),
                                egui::Button::new(
                                    egui::RichText::new("取消")
                                        .color(theme::semantic(ui.ctx()).error),
                                ),
                            )
                            .on_disabled_hover_text("该任务暂不支持取消")
                            .clicked()
                        {
                            if let Some(flag) = &self.cancel {
                                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                            if self.state.platform.workspace.is_some() {
                                let _ = self.worker.cancel(task.id);
                            }
                        }
                        ui.label(&task.label);
                        ui.spinner();
                    } else {
                        ui.label(
                            egui::RichText::new("空闲").color(theme::semantic(ui.ctx()).weak_text),
                        );
                    }
                });
            });
            ui.add_space(4.0);
        });
    }

    fn nav_rail(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("nav_rail")
            .resizable(false)
            .default_size(132.0)
            .show(ui, |ui| {
                ui.add_space(12.0);
                for (panel, label) in [
                    (Panel::Dataset, "数据管理"),
                    (Panel::Query, "查询工作台"),
                    (Panel::Factor, "因子工作台"),
                    (Panel::Model, "建模"),
                    (Panel::Research, "研究流程"),
                    (Panel::Runs, "运行记录"),
                    (Panel::Storage, "存储管理"),
                ] {
                    let selected = self.state.panel == panel;
                    let text = if selected {
                        egui::RichText::new(label)
                            .strong()
                            .color(theme::semantic(ui.ctx()).accent)
                    } else {
                        egui::RichText::new(label)
                    };
                    if ui
                        .add_sized([116.0, 28.0], egui::Button::selectable(selected, text))
                        .clicked()
                    {
                        self.state.panel = panel;
                    }
                    ui.add_space(2.0);
                }
            });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status_bar").show(ui, |ui| {
            if self.state.log_open {
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for message in &self.state.messages {
                            ui.monospace(message);
                        }
                    });
                ui.separator();
            }
            ui.horizontal(|ui| {
                let colors = theme::semantic(ui.ctx());
                theme::status_dot(ui, colors.success, "DuckDB");
                theme::status_dot(ui, colors.success, "LightGBM");
                ui.separator();
                if self.state.worker_error.is_some() {
                    ui.colored_label(colors.error, "worker：不可用");
                } else if self.state.busy() {
                    ui.label("worker：运行中");
                } else {
                    ui.label(egui::RichText::new("worker：空闲").weak());
                }
                ui.separator();
                if let Some(last) = self.state.messages.last() {
                    ui.label(
                        egui::RichText::new(last)
                            .small()
                            .color(theme::semantic(ui.ctx()).weak_text),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let toggle = if self.state.log_open {
                        "收起日志"
                    } else {
                        "消息日志"
                    };
                    if ui.button(toggle).clicked() {
                        self.state.log_open = !self.state.log_open;
                    }
                });
            });
        });
    }
}

impl eframe::App for DesktopApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.top_bar(ui);
        self.nav_rail(ui);
        self.status_bar(ui);
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(8.0);
                panels::render(
                    ui,
                    &mut self.state,
                    &self.worker,
                    &mut self.cancel,
                    &self.registry,
                );
                ui.add_space(16.0);
            });
        });
    }
}

fn main() -> eframe::Result {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--self-check") {
        if args.len() != 2 {
            eprintln!("usage: analytics-desktop --self-check OUTPUT_DIRECTORY");
            std::process::exit(2);
        }
        let root = std::path::PathBuf::from(&args[1]);
        let result = std::thread::spawn(move || {
            analytics_workflow::self_check(&root).map_err(|e| e.to_string())
        })
        .join();
        match result {
            Ok(Ok(value)) => {
                println!("{value}");
                return Ok(());
            }
            other => {
                eprintln!("native self-check failed: {other:?}");
                std::process::exit(1);
            }
        }
    }

    let mut options = eframe::NativeOptions::default();
    options.viewport = options
        .viewport
        .with_inner_size([1120.0, 760.0])
        .with_min_inner_size([880.0, 600.0]);
    eframe::run_native(
        "JFQuant Analytics",
        options,
        Box::new(|cc| Ok(Box::new(DesktopApp::new(cc)))),
    )
}
