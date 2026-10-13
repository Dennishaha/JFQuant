//! Visual identity for the desktop shell.
//!
//! The app follows the system light/dark preference and layers a small set of
//! semantic colors (accent/success/warning/error/weak text) plus an 8pt
//! spacing grid on top of egui's stock visuals. Widgets read the active
//! palette through [`semantic`] so both themes stay consistent.

use egui::{Color32, Context, CornerRadius, Stroke, Style, ThemePreference};

/// Semantic colors resolved against the active light or dark theme.
#[derive(Debug, Clone, Copy)]
pub struct Semantic {
    /// Primary action color (buttons, active navigation).
    pub accent: Color32,
    /// Successful state (finished tasks, valid data contracts).
    pub success: Color32,
    /// Warning state (cancellable or degraded conditions).
    pub warning: Color32,
    /// Error state (failed tasks, invalid input).
    pub error: Color32,
    /// Secondary/weak foreground text.
    pub weak_text: Color32,
    /// Subtle fill used for badges and inline result headers.
    pub badge_fill: Color32,
}

/// Returns the semantic palette for the currently active theme.
pub fn semantic(ctx: &Context) -> Semantic {
    if ctx.theme() == egui::Theme::Dark {
        Semantic {
            accent: Color32::from_rgb(0x4C, 0x9A, 0xFF),
            success: Color32::from_rgb(0x36, 0xB3, 0x7E),
            warning: Color32::from_rgb(0xFF, 0xAB, 0x00),
            error: Color32::from_rgb(0xFF, 0x56, 0x30),
            weak_text: Color32::from_rgb(0x8A, 0x94, 0xA6),
            badge_fill: Color32::from_rgb(0x24, 0x2A, 0x35),
        }
    } else {
        Semantic {
            accent: Color32::from_rgb(0x00, 0x65, 0xFF),
            success: Color32::from_rgb(0x1F, 0x84, 0x5A),
            warning: Color32::from_rgb(0x94, 0x6F, 0x00),
            error: Color32::from_rgb(0xAE, 0x2E, 0x24),
            weak_text: Color32::from_rgb(0x5E, 0x6C, 0x84),
            badge_fill: Color32::from_rgb(0xEC, 0xEF, 0xF4),
        }
    }
}

/// Installs the system-following theme, tuned spacing, and the CJK font
/// stack. Called once during app creation.
pub fn install(ctx: &Context) {
    ctx.set_theme(ThemePreference::System);
    ctx.all_styles_mut(|style| {
        tune_style(style);
    });
    install_cjk_font(ctx);
}

fn tune_style(style: &mut Style) {
    let dark = style.visuals.dark_mode;
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(12.0, 5.0);
    style.spacing.indent = 16.0;
    style.spacing.combo_width = 140.0;
    let radius = CornerRadius::same(6);
    style.visuals.widgets.noninteractive.corner_radius = radius;
    style.visuals.widgets.inactive.corner_radius = radius;
    style.visuals.widgets.hovered.corner_radius = radius;
    style.visuals.widgets.active.corner_radius = radius;
    style.visuals.widgets.open.corner_radius = radius;
    style.visuals.window_corner_radius = CornerRadius::same(8);
    if dark {
        style.visuals.panel_fill = Color32::from_rgb(0x16, 0x1A, 0x21);
        style.visuals.window_fill = Color32::from_rgb(0x1B, 0x20, 0x27);
        style.visuals.extreme_bg_color = Color32::from_rgb(0x10, 0x13, 0x18);
        style.visuals.faint_bg_color = Color32::from_rgb(0x20, 0x25, 0x2E);
        style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x24, 0x2A, 0x35);
        style.visuals.widgets.inactive.bg_stroke =
            Stroke::new(1.0, Color32::from_rgb(0x33, 0x3B, 0x48));
        style.visuals.selection.bg_fill = Color32::from_rgb(0x2A, 0x4A, 0x75);
    } else {
        style.visuals.panel_fill = Color32::from_rgb(0xF6, 0xF7, 0xF9);
        style.visuals.window_fill = Color32::WHITE;
        style.visuals.extreme_bg_color = Color32::from_rgb(0xEC, 0xEF, 0xF4);
        style.visuals.faint_bg_color = Color32::from_rgb(0xF0, 0xF2, 0xF6);
        style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(0xEC, 0xEF, 0xF4);
        style.visuals.widgets.inactive.bg_stroke =
            Stroke::new(1.0, Color32::from_rgb(0xD2, 0xD9, 0xE2));
        style.visuals.selection.bg_fill = Color32::from_rgb(0xC8, 0xDD, 0xFF);
    }
}

/// Draws a colored status dot followed by `text`.
pub fn status_dot(ui: &mut egui::Ui, color: Color32, text: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.colored_label(color, "\u{25CF}");
        ui.label(text);
    });
}

/// Draws a consistent section heading with a weak caption underneath.
pub fn section_heading(ui: &mut egui::Ui, title: &str, caption: &str) {
    ui.strong(title);
    ui.add_space(2.0);
    ui.label(egui::RichText::new(caption).small().weak());
}

fn install_cjk_font(ctx: &Context) {
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
