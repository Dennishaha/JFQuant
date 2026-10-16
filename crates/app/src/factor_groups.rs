//! Factor library controls and standalone diagnostic results.
use crate::state::AppState;
use analytics_factor::selection::FactorRef;
use analytics_workflow::factor_groups::{self, FactorGroup};
use analytics_workflow::service::Rpc;

pub(crate) fn controls(ui: &mut egui::Ui, state: &mut AppState) {
    crate::custom_factors::poll(ui, state);
    let root = state
        .platform
        .workspace
        .as_ref()
        .map(|w| w.root().to_owned())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    if state.factor.group_root.as_ref() != Some(&root) {
        state.factor.group_root = Some(root.clone());
        state.factor.group = None;
        state.factor.factor_refs.clear();
        state.factor.custom_factors.clear();
        // Builtin source is local; the service fills the user library asynchronously.
        state.factor.groups = factor_groups::list(&root).unwrap_or_else(|_| {
            vec![
                FactorGroup::builtin(Default::default()),
                FactorGroup::builtin(analytics_core::research_quality::FactorSet::Alpha158),
            ]
        });
        if state.platform.workspace.is_some() {
            crate::custom_factors::request(state, Rpc::FactorLibrary);
            crate::custom_factors::poll(ui, state);
        }
    }
    let mut chosen = None;
    ui.horizontal(|ui| {
        ui.label("因子组");
        egui::ComboBox::from_id_salt("factor_group_library")
            .selected_text(
                state
                    .factor
                    .group
                    .as_ref()
                    .map(|g| g.name.as_str())
                    .unwrap_or("未保存的选择"),
            )
            .show_ui(ui, |ui| {
                for g in &state.factor.groups {
                    if ui
                        .selectable_label(
                            state.factor.group.as_ref().is_some_and(|v| v.id == g.id),
                            format!(
                                "{}{} · {}项",
                                g.name,
                                if g.is_builtin() { "（只读）" } else { "" },
                                g.profile.factor_ids.len()
                            ),
                        )
                        .clicked()
                    {
                        chosen = Some(g.clone());
                    }
                }
            });
        if ui.button("刷新分组").clicked() {
            crate::custom_factors::request(state, Rpc::FactorLibrary);
        }
    });
    if let Some(g) = chosen {
        let f = &mut state.factor;
        f.factor_set = g.profile.factor_set;
        f.selected = g.profile.factor_ids.iter().cloned().collect();
        f.factor_refs = analytics_factor::selection::frozen_refs(
            g.profile.factor_set,
            &g.profile.factor_refs,
            &g.profile.factor_ids,
        );
        f.group = Some(g);
    }
    let readonly = state
        .factor
        .group
        .as_ref()
        .is_some_and(FactorGroup::is_builtin);
    let pending = state.factor.library_reply.is_some();
    let (mut copy, mut save, mut archive, mut apply) = (false, false, false, false);
    ui.horizontal(|ui| {
        copy = ui
            .add_enabled(!pending, egui::Button::new("复制为自定义组"))
            .clicked();
        save = ui
            .add_enabled(!readonly && !pending, egui::Button::new("保存因子组"))
            .clicked();
        archive = ui
            .add_enabled(
                state.factor.group.as_ref().is_some_and(|g| !g.is_builtin()) && !pending,
                egui::Button::new("删除自定义组"),
            )
            .clicked();
        apply = ui.button("应用到研究配置").clicked();
    });
    if copy || (save && state.factor.group.is_none()) {
        let base = state
            .factor
            .group
            .clone()
            .unwrap_or_else(|| FactorGroup::builtin(state.factor.factor_set));
        state.factor.group = Some(base.duplicate());
    }
    if let Some(g) = &mut state.factor.group {
        ui.add_enabled_ui(!g.is_builtin() && !pending, |ui| {
            ui.horizontal(|ui| {
                ui.label("名称");
                ui.text_edit_singleline(&mut g.name);
            });
            ui.horizontal(|ui| {
                ui.label("说明");
                ui.text_edit_singleline(&mut g.description);
            });
        });
    }
    let ids = state.factor.selected_ids();
    let refs = state.factor.selected_refs();
    if let Some(g) = &mut state.factor.group
        && !g.is_builtin()
    {
        g.profile.schema_version = 2;
        g.profile.factor_set = state.factor.factor_set;
        g.profile.factor_ids = ids.clone();
        if let Ok(refs) = &refs {
            g.profile.factor_refs = refs.clone();
        }
        if let Ok(registry) = analytics_factor::FactorRegistry::resolve(
            g.profile.factor_set,
            &g.profile.factor_refs,
            &ids,
        ) && let Ok(version) = registry.selection_version(&ids)
        {
            g.profile.formula_version = version;
        }
    }
    if save && let Some(g) = state.factor.group.clone() {
        match &refs {
            Ok(_) => {
                crate::custom_factors::request(state, Rpc::SaveFactorGroup { group: Box::new(g) })
            }
            Err(e) => state.factor.error = Some(e.clone()),
        }
    }
    if archive && let Some(g) = &state.factor.group {
        crate::custom_factors::request(state, Rpc::ArchiveFactorGroup { id: g.id.clone() });
    }
    if apply {
        match refs {
            Ok(refs) => {
                let mut profile = analytics_workflow::profiles::FactorProfile::from_template(
                    &state.platform.template,
                );
                profile.schema_version = 2;
                profile.factor_set = state.factor.factor_set;
                profile.factor_ids = ids;
                profile.factor_refs = refs;
                let result = analytics_factor::FactorRegistry::resolve(
                    profile.factor_set,
                    &profile.factor_refs,
                    &profile.factor_ids,
                )
                .and_then(|r| r.selection_version(&profile.factor_ids));
                match result {
                    Ok(version) => {
                        profile.formula_version = version;
                        state.factor.error = profile
                            .apply(&mut state.platform.template)
                            .err()
                            .map(|e| e.to_string());
                    }
                    Err(e) => state.factor.error = Some(e.to_string()),
                }
            }
            Err(e) => state.factor.error = Some(e),
        }
    }
    ui.label("内置定义与分组只读；复制后可混合158、191和自定义因子。已有选择固定修订，更新需显式操作；删除分组会归档。");
    let mut edit = None;
    let mut include = None;
    let mut archive_id = None;
    let mut update = None;
    ui.collapsing("自定义因子库", |ui| {
        for entry in &state.factor.custom_factors {
            let pinned = state.factor.factor_refs.iter().find_map(|r| match r {
                FactorRef::Custom { definition }
                    if definition.id == entry.definition.id
                        && state.factor.selected.contains(&definition.id) =>
                {
                    Some(definition.revision)
                }
                _ => None,
            });
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{} · r{}{}",
                    entry.definition.name,
                    entry.definition.revision,
                    if entry.archived {
                        "（已归档）"
                    } else {
                        ""
                    }
                ));
                if let Some(revision) = pinned {
                    ui.small(format!("当前组固定r{revision}"));
                }
                ui.label("公式").on_hover_text(&entry.definition.formula);
                if ui
                    .add_enabled(!entry.archived, egui::Button::new("编辑公式"))
                    .clicked()
                {
                    edit = Some(entry.definition.clone());
                }
                if ui
                    .add_enabled(
                        !readonly && !entry.archived,
                        egui::Button::new("加入当前组"),
                    )
                    .clicked()
                {
                    include = Some(entry.definition.clone());
                }
                if ui
                    .add_enabled(
                        !readonly
                            && !entry.archived
                            && pinned.is_some_and(|revision| revision < entry.definition.revision),
                        egui::Button::new("更新选中修订"),
                    )
                    .clicked()
                {
                    update = Some(entry.definition.clone());
                }
                if ui
                    .add_enabled(!entry.archived && !pending, egui::Button::new("归档因子"))
                    .clicked()
                {
                    archive_id = Some(entry.definition.id.clone());
                }
            });
        }
    });
    if let Some(d) = edit {
        state.factor.draft = analytics_workflow::custom_factors::FactorDraft {
            id: Some(d.id),
            expected_revision: Some(d.revision),
            name: d.name,
            description: d.description,
            formula: d.formula,
        };
    }
    if let Some(d) = include {
        let f = &mut state.factor;
        if !f.factor_refs.iter().any(|r| r.id() == d.id) {
            f.factor_refs.push(FactorRef::Custom {
                definition: Box::new(d.clone()),
            });
        }
        f.selected.insert(d.id);
    }
    if let Some(d) = update {
        for r in &mut state.factor.factor_refs {
            if r.id() == d.id {
                *r = FactorRef::Custom {
                    definition: Box::new(d.clone()),
                };
            }
        }
    }
    if let Some(id) = archive_id {
        crate::custom_factors::request(state, Rpc::ArchiveFactor { id });
    }
    let selected = state.factor.selected_ids();
    if !selected.is_empty() {
        ui.collapsing("特征顺序", |ui| {
            let mut movement = None;
            for (i, id) in selected.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("{} · {id}", i + 1));
                    if ui
                        .add_enabled(!readonly && i > 0, egui::Button::new("上移"))
                        .clicked()
                    {
                        movement = Some((i, i - 1));
                    }
                    if ui
                        .add_enabled(
                            !readonly && i + 1 < selected.len(),
                            egui::Button::new("下移"),
                        )
                        .clicked()
                    {
                        movement = Some((i, i + 1));
                    }
                });
            }
            if let Some((a, b)) = movement
                && let Ok(mut refs) = state.factor.selected_refs()
            {
                refs.swap(a, b);
                state.factor.factor_refs = refs;
            }
        });
    }
}

fn number(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.4}")).unwrap_or_else(|| "—".into())
}
pub(crate) fn report(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(report) = &state.factor.evaluation else {
        return;
    };
    ui.separator();
    ui.heading("因子有效性报告");
    ui.label(format!(
        "{} · {} 至 {} · 未来{}日 · {}分层",
        if report.request.input.factor_refs.is_empty() {
            report.request.input.factor_set.label()
        } else {
            "固定因子组合"
        },
        report.request.input.date_range.start,
        report.request.input.date_range.end,
        report.request.horizon,
        report.request.quantiles
    ));
    for warning in &report.warnings {
        ui.small(warning);
    }
    if ui.button("导出完整报告 JSON").clicked()
        && let Some(path) = rfd::FileDialog::new()
            .set_file_name("factor-evaluation.json")
            .save_file()
    {
        match serde_json::to_vec_pretty(report)
            .map_err(|e| e.to_string())
            .and_then(|b| std::fs::write(path, b).map_err(|e| e.to_string()))
        {
            Ok(()) => state.factor.error = None,
            Err(e) => state.factor.error = Some(e),
        }
    }
    ui.small("按绝对RankIC从高到低排列；负值表示反向关系，空值置后。");
    let mut ranked: Vec<_> = report.factors.iter().collect();
    ranked.sort_by(|a, b| {
        b.mean_rank_ic
            .map(f64::abs)
            .partial_cmp(&a.mean_rank_ic.map(f64::abs))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut selected = None;
    egui::ScrollArea::both()
        .id_salt("factor_diagnostics")
        .max_height(400.)
        .show(ui, |ui| {
            egui::Grid::new("factor_diagnostic_table")
                .striped(true)
                .show(ui, |ui| {
                    for h in [
                        "因子",
                        "覆盖率",
                        "标签配对率",
                        "有效天数",
                        "IC",
                        "RankIC",
                        "RankIC IR",
                        "正RankIC比例",
                        "高减低收益",
                        "筛选",
                    ] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for d in &ranked {
                        ui.label(&d.factor_id);
                        for v in [d.coverage, d.labeled_coverage] {
                            ui.label(number(v));
                        }
                        ui.label(d.valid_days.to_string());
                        for v in [
                            d.mean_ic,
                            d.mean_rank_ic,
                            d.rank_ic_ir,
                            d.positive_rank_ic_ratio,
                            d.mean_spread,
                        ] {
                            ui.label(number(v));
                        }
                        if ui.button("加入自定义选择").clicked() {
                            selected = Some(d.factor_id.clone());
                        }
                        ui.end_row();
                    }
                });
            for d in &ranked {
                ui.collapsing(format!("{} · 分层收益与每日IC", d.factor_id), |ui| {
                    ui.label(format!(
                        "低 → 高：{}",
                        d.quantile_returns
                            .iter()
                            .map(|v| number(*v))
                            .collect::<Vec<_>>()
                            .join(" / ")
                    ));
                    egui::ScrollArea::vertical()
                        .id_salt((&d.factor_id, "daily"))
                        .max_height(180.)
                        .show(ui, |ui| {
                            for day in &d.daily {
                                ui.monospace(format!(
                                    "{}  样本{}/{}  IC {}  RankIC {}  高减低 {}",
                                    day.date,
                                    day.paired,
                                    day.eligible,
                                    number(day.ic),
                                    number(day.rank_ic),
                                    number(day.spread)
                                ));
                            }
                        });
                });
            }
        });
    if let Some(id) = selected {
        let f = &mut state.factor;
        let family = f.evaluation.as_ref().unwrap().request.input.factor_set;
        if f.factor_set != family || f.group.as_ref().is_none_or(FactorGroup::is_builtin) {
            f.factor_set = family;
            f.selected.clear();
            f.factor_refs.clear();
            f.group = Some(FactorGroup::builtin(family).duplicate());
            f.group.as_mut().unwrap().name = "有效性筛选".into();
        }
        let input = &f.evaluation.as_ref().unwrap().request.input;
        let refs = analytics_factor::selection::frozen_refs(
            input.factor_set,
            &input.factor_refs,
            &input.factor_ids,
        );
        if let Some(r) = refs.into_iter().find(|r| r.id() == id)
            && !f.factor_refs.iter().any(|v| v.id() == id)
        {
            f.factor_refs.push(r);
        }
        f.selected.insert(id);
    }
}
