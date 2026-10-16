//! Date controls edit the shared schedule; resolution remains in workflow.
use analytics_core::{
    Date, DateRange,
    schedule::{Fold, Schedule, Step, StepUnit},
};
#[derive(Clone)]
struct DateEdit {
    value: Date,
    text: String,
}
fn date(ui: &mut egui::Ui, label: &str, value: &mut Date) {
    ui.label(label);
    let id = ui.make_persistent_id(label);
    let mut edit = ui
        .data_mut(|d| d.get_temp::<DateEdit>(id))
        .unwrap_or_else(|| DateEdit {
            value: *value,
            text: value.to_string(),
        });
    if edit.value != *value {
        edit = DateEdit {
            value: *value,
            text: value.to_string(),
        };
    }
    ui.add(egui::TextEdit::singleline(&mut edit.text).desired_width(100.));
    match edit.text.parse::<Date>() {
        Ok(day) => {
            *value = day;
            edit.value = day;
        }
        Err(_) => {
            ui.data_mut(|d| d.insert_temp(egui::Id::unique("research-invalid-date"), true));
            ui.colored_label(egui::Color32::RED, "YYYY-MM-DD");
        }
    }
    ui.data_mut(|d| d.insert_temp(id, edit));
}
fn range(ui: &mut egui::Ui, label: &str, r: &mut DateRange) {
    ui.push_id(label, |ui| {
        ui.horizontal(|ui| {
            ui.label(label);
            date(ui, "从", &mut r.start);
            date(ui, "至", &mut r.end);
        });
    });
}
fn fold(ui: &mut egui::Ui, f: &mut Fold) {
    range(ui, "训练", &mut f.train);
    let mut validation = f.validation.is_some();
    if ui.checkbox(&mut validation, "使用验证集 / 早停").changed() {
        f.validation = validation.then(|| DateRange {
            start: f.train.end.succ_opt().unwrap_or(f.train.end),
            end: f.test.start.pred_opt().unwrap_or(f.test.start),
        });
    }
    if let Some(v) = &mut f.validation {
        range(ui, "验证", v);
    }
    range(ui, "回测", &mut f.test);
}
pub(crate) fn draw(ui: &mut egui::Ui, schedule: &mut Schedule) -> bool {
    ui.data_mut(|d| d.insert_temp(egui::Id::unique("research-invalid-date"), false));
    let mut mode = match schedule {
        Schedule::Single { .. } => 0,
        Schedule::Rolling { .. } => 1,
        Schedule::Manual { .. } => 2,
    };
    let old = mode;
    egui::ComboBox::from_id_salt("date-schedule-mode")
        .selected_text(["单次指定区间", "可配置滚动", "手动多折"][mode])
        .show_ui(ui, |ui| {
            for (i, label) in ["单次指定区间", "可配置滚动", "手动多折"]
                .iter()
                .enumerate()
            {
                ui.selectable_value(&mut mode, i, *label);
            }
        });
    if mode != old {
        let first = match schedule {
            Schedule::Single { fold } => fold.clone(),
            Schedule::Rolling { first, .. } => first.clone(),
            Schedule::Manual { folds } => folds[0].clone(),
        };
        *schedule = match mode {
            0 => Schedule::Single { fold: first },
            1 => Schedule::Rolling {
                first,
                step: Step {
                    count: 1,
                    unit: StepUnit::Years,
                },
                expanding: false,
                until: None,
            },
            _ => Schedule::Manual { folds: vec![first] },
        };
    }
    match schedule {
        Schedule::Single { fold: f } => fold(ui, f),
        Schedule::Rolling {
            first,
            step,
            expanding,
            until,
        } => {
            fold(ui, first);
            ui.horizontal(|ui| {
                ui.label("每折移动");
                ui.add(egui::DragValue::new(&mut step.count).range(1..=10000));
                egui::ComboBox::from_id_salt("rolling-unit")
                    .selected_text(format!("{:?}", step.unit))
                    .show_ui(ui, |ui| {
                        for (unit, label) in [
                            (StepUnit::Days, "日历日"),
                            (StepUnit::Months, "月"),
                            (StepUnit::Years, "年"),
                            (StepUnit::TradingDays, "交易日"),
                        ] {
                            ui.selectable_value(&mut step.unit, unit, label);
                        }
                    });
                ui.checkbox(expanding, "扩展训练窗口");
            });
            let mut auto = until.is_none();
            if ui.checkbox(&mut auto, "截至数据最后完整交易日").changed() {
                *until = if auto { None } else { Some(first.test.end) };
            }
            if let Some(end) = until {
                ui.horizontal(|ui| date(ui, "滚动结束", end));
            }
        }
        Schedule::Manual { folds } => {
            let mut remove = None;
            for (i, f) in folds.iter_mut().enumerate() {
                ui.push_id(i, |ui| {
                    ui.collapsing(format!("第{}折", i + 1), |ui| {
                        fold(ui, f);
                        if ui.button("删除此折").clicked() {
                            remove = Some(i);
                        }
                    });
                });
            }
            if let Some(i) = remove
                && folds.len() > 1
            {
                folds.remove(i);
            }
            if ui.button("添加一折（随后编辑日期）").clicked() && folds.len() < 10000 {
                folds.push(folds.last().unwrap().clone());
            }
        }
    }
    if let Err(e) = schedule.validate() {
        ui.colored_label(egui::Color32::RED, e);
    }
    !ui.data_mut(|d| d.get_temp::<bool>(egui::Id::unique("research-invalid-date")))
        .unwrap_or(false)
        && schedule.validate().is_ok()
}

/// Forget unfinished date text when explicitly loading or applying another template.
pub(crate) fn reset(ui: &mut egui::Ui) {
    ui.data_mut(|d| d.remove_by_type::<DateEdit>());
}
