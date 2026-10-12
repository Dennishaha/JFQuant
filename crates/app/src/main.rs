use analytics_factor::FactorRegistry;
use eframe::egui;
use std::sync::mpsc::{self, Receiver, Sender};

struct DesktopApp {
    query: String,
    market_path: String,
    universe_path: String,
    date_start: String,
    date_end: String,
    output_path: String,
    factor_ids: String,
    selected_tab: usize,
    factor_count: usize,
    messages: Vec<String>,
    _events: (Sender<String>, Receiver<String>),
}

impl Default for DesktopApp {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            query: "SELECT * FROM read_parquet('data/*.parquet') LIMIT 100".into(),
            market_path: "./market".into(),
            universe_path: "./universe".into(),
            date_start: "2020-01-01".into(),
            date_end: "2025-12-31".into(),
            output_path: "./runs/alpha191".into(),
            factor_ids: "alpha_001,alpha_002".into(),
            selected_tab: 0,
            factor_count: FactorRegistry::default().len(),
            messages: Vec::new(),
            _events: (tx, rx),
        }
    }
}

impl eframe::App for DesktopApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(message) = self._events.1.try_recv() {
            self.messages.push(message);
        }
        ui.horizontal(|ui| {
            ui.heading("analytics-desktop");
            ui.label("Parquet · DuckDB · Alpha191 · LightGBM");
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.selected_tab, 0, "数据管理");
            ui.selectable_value(&mut self.selected_tab, 1, "查询工作台");
            ui.selectable_value(&mut self.selected_tab, 2, "因子工作台");
            ui.selectable_value(&mut self.selected_tab, 3, "建模");
        });
        ui.separator();
        match self.selected_tab {
            0 => self.dataset_panel(ui),
            1 => self.query_panel(ui),
            2 => self.factor_panel(ui),
            _ => self.model_panel(ui),
        }
        if !self.messages.is_empty() {
            ui.separator();
            ui.label("任务消息");
            for message in &self.messages {
                ui.monospace(message);
            }
        }
    }
}

impl DesktopApp {
    fn dataset_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("本地 Parquet");
        ui.label("选择本地 Parquet 文件或目录后，将在后台扫描 schema 与行数。");
        if ui.button("扫描数据集（后端待接入）").clicked() {
            self.messages.push("dataset scan requested".into());
        }
    }

    fn query_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("查询工作台");
        ui.add(
            egui::TextEdit::multiline(&mut self.query)
                .desired_rows(8)
                .code_editor(),
        );
        if ui.button("执行查询（后端待接入）").clicked() {
            self.messages.push("query requested".into());
        }
        ui.separator();
        ui.label("当前 UI 使用分页结果模型，正式后端将使用 keyset cursor。");
    }

    fn factor_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Alpha191 因子工作台");
        ui.label(format!("已注册 {0} 个因子", self.factor_count));
        ui.horizontal(|ui| {
            ui.label("行情");
            ui.text_edit_singleline(&mut self.market_path);
        });
        ui.horizontal(|ui| {
            ui.label("股票池");
            ui.text_edit_singleline(&mut self.universe_path);
        });
        ui.horizontal(|ui| {
            ui.label("日期");
            ui.text_edit_singleline(&mut self.date_start);
            ui.label("至");
            ui.text_edit_singleline(&mut self.date_end);
        });
        ui.horizontal(|ui| {
            ui.label("因子");
            ui.text_edit_singleline(&mut self.factor_ids);
        });
        ui.horizontal(|ui| {
            ui.label("输出");
            ui.text_edit_singleline(&mut self.output_path);
        });
        if ui.button("生成训练命令").clicked() {
            self.messages.push(format!(
                "market={} universe={} dates={}..{} factors={} output={}",
                self.market_path,
                self.universe_path,
                self.date_start,
                self.date_end,
                self.factor_ids,
                self.output_path
            ));
        }
        ui.label("后台执行入口使用 analytics-ml factor_pipeline，结果包括因子缓存、模型、指标和预测 Parquet。");
    }

    fn model_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("LightGBM 建模");
        ui.label("训练接口已通过 Rust FFI 抽象；启用 lightgbm-native 后连接官方库。");
        if ui.button("训练模型（后端待接入）").clicked() {
            self.messages.push("training requested".into());
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "analytics-desktop",
        options,
        Box::new(|cc| {
            install_cjk_font(&cc.egui_ctx);
            Ok(Box::new(DesktopApp::default()))
        }),
    )
}

fn install_cjk_font(ctx: &egui::Context) {
    let candidates = [
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
    ];
    let Some(path) = candidates
        .iter()
        .find(|path| std::path::Path::new(path).exists())
    else {
        return;
    };
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".into(), egui::FontData::from_owned(bytes).into());
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "cjk".into());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "cjk".into());
    ctx.set_fonts(fonts);
}
