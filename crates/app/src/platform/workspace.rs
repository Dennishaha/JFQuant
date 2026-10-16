//! Workspace panel and interaction.
use super::*;
fn preferences() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|p| PathBuf::from(p).join("Library/Application Support/JFQuant/preferences.json"))
}
fn select_workspace(state: &mut AppState, path: &Path) -> analytics_workflow::Result<()> {
    if state.busy() {
        return Err("任务执行时不能切换工作区".into());
    }
    if state
        .platform
        .workspace
        .as_ref()
        .is_some_and(|w| w.root() == path)
    {
        return Ok(());
    }
    let w = Workspace::open(path)?;
    if state
        .platform
        .workspace
        .as_ref()
        .is_some_and(|old| old.root() == w.root())
    {
        return Ok(());
    }
    #[cfg(test)]
    let lease = Some(w.lease()?);
    #[cfg(not(test))]
    let lease = None;
    #[cfg(test)]
    w.recover()?;
    state.platform.reads = Default::default();
    state.platform.reads.overview(w.root().to_owned());
    let datasets = Vec::new();
    let runs = Vec::new();
    state.platform.root_input = w.root().display().to_string();
    state.platform.workspace = Some(w);
    state.platform.lease = lease;
    state.platform.datasets = datasets;
    state.platform.runs = runs;
    state.platform.market = state
        .platform
        .datasets
        .iter()
        .find(|d| d.kind == "market")
        .map(|d| d.id.clone())
        .unwrap_or_default();
    state.platform.actions = state
        .platform
        .datasets
        .iter()
        .find(|d| d.kind == "actions")
        .map(|d| d.id.clone())
        .unwrap_or_default();
    state.platform.selected_run = None;
    state.platform.details = Value::Null;
    state.platform.error = None;
    Ok(())
}
pub fn load_preferences(state: &mut AppState) {
    state.platform.require_workspace = true;
    if let Some(p) = preferences()
        && let Ok(bytes) = fs::read(p)
    {
        match serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v["workspace"].as_str().map(PathBuf::from))
        {
            Some(path) => {
                if !path.is_dir() {
                    state.platform.root_input = path.display().to_string();
                    state.platform.error =
                        Some("上次工作区无法访问，请连接磁盘或重新选择。".into());
                    return;
                }
                if let Err(e) = select_workspace(state, &path) {
                    state.platform.error = Some(e.to_string());
                }
            }
            None => state.platform.error = Some("工作区设置无效，请重新选择".into()),
        }
    }
}
fn save_preferences(state: &AppState) -> analytics_workflow::Result<()> {
    if let (Some(p), Some(w)) = (preferences(), &state.platform.workspace) {
        fs::create_dir_all(p.parent().unwrap())?;
        let part = p.with_extension("tmp");
        fs::write(
            &part,
            serde_json::to_vec_pretty(
                &serde_json::json!({"schema_version":1,"workspace":w.root()}),
            )?,
        )?;
        fs::rename(part, p)?;
    }
    Ok(())
}
pub fn workspace_bar(ui: &mut egui::Ui, state: &mut AppState) -> bool {
    reads::poll(&mut state.platform, ui.ctx());
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label("工作区");
        ui.add_enabled(
            !state.busy(),
            egui::TextEdit::singleline(&mut state.platform.root_input).desired_width(330.0),
        );
        if ui
            .add_enabled(!state.busy(), egui::Button::new("选择目录"))
            .clicked()
            && let Some(p) = rfd::FileDialog::new().pick_folder()
        {
            state.platform.root_input = p.display().to_string();
        }
        if ui
            .add_enabled(!state.busy(), egui::Button::new("创建/打开工作区"))
            .clicked()
        {
            let p = PathBuf::from(&state.platform.root_input);
            match select_workspace(state, &p) {
                Ok(()) => {
                    changed = true;
                    if let Err(e) = save_preferences(state) {
                        state.platform.error = Some(e.to_string());
                    }
                }
                Err(e) => state.platform.error = Some(e.to_string()),
            }
        }
    });
    if state.platform.workspace.is_none() {
        ui.label("选择可写目录保存数据、模板和本地运行记录。运行不依赖源码目录。");
    }
    if let Some(e) = &state.platform.error {
        ui.colored_label(crate::theme::semantic(ui.ctx()).error, e);
    }
    changed
}
