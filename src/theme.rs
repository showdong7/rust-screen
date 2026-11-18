use eframe::egui::{self, Color32, Rect, Sense, Stroke, Vec2};

use crate::config::ThemeMode;

pub const PICKER_WIDTH: f32 = 102.0;

#[derive(Clone, Copy)]
pub struct Palette {
    pub accent: Color32,
    pub muted: Color32,
    pub canvas: Color32,
    pub empty: Color32,
    pub heading: Color32,
    pub error: Color32,
    pub button_text: Color32,
}

impl Palette {
    pub fn for_ui(ui: &egui::Ui) -> Self {
        if ui.visuals().dark_mode {
            Self {
                accent: Color32::from_rgb(78, 215, 185),
                muted: Color32::from_rgb(154, 170, 190),
                canvas: Color32::from_rgb(13, 19, 29),
                empty: Color32::from_rgb(19, 27, 39),
                heading: Color32::from_rgb(215, 229, 240),
                error: Color32::LIGHT_RED,
                button_text: Color32::from_rgb(12, 35, 32),
            }
        } else {
            Self {
                accent: Color32::from_rgb(0, 112, 94),
                muted: Color32::from_rgb(84, 101, 121),
                canvas: Color32::from_rgb(231, 237, 243),
                empty: Color32::from_rgb(240, 244, 248),
                heading: Color32::from_rgb(31, 49, 70),
                error: Color32::from_rgb(166, 34, 45),
                button_text: Color32::WHITE,
            }
        }
    }
}

pub fn configure(ctx: &egui::Context) {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(theme, |style| {
            style
                .text_styles
                .insert(egui::TextStyle::Small, egui::FontId::proportional(11.0));
            style.spacing.item_spacing = Vec2::new(9.0, 8.0);
            style.spacing.button_padding = Vec2::new(12.0, 8.0);
            style.visuals = match theme {
                egui::Theme::Dark => {
                    let mut visuals = egui::Visuals::dark();
                    visuals.panel_fill = Color32::from_rgb(24, 33, 46);
                    visuals.window_fill = visuals.panel_fill;
                    visuals.selection.bg_fill = Color32::from_rgb(38, 98, 91);
                    visuals
                }
                egui::Theme::Light => {
                    let mut visuals = egui::Visuals::light();
                    visuals.panel_fill = Color32::from_rgb(246, 248, 251);
                    visuals.window_fill = Color32::WHITE;
                    visuals.selection.bg_fill = Color32::from_rgb(210, 236, 230);
                    visuals
                }
            };
        });
    }
}

pub fn apply(ctx: &egui::Context, mode: ThemeMode) {
    ctx.set_theme(match mode {
        ThemeMode::System => egui::ThemePreference::System,
        ThemeMode::Dark => egui::ThemePreference::Dark,
        ThemeMode::Light => egui::ThemePreference::Light,
    });
    ctx.request_repaint();
}

/// Compact, font-independent monitor / moon / sun buttons, ordered System / Dark / Light.
pub fn picker(ui: &mut egui::Ui, mode: &mut ThemeMode) -> bool {
    let mut changed = false;
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.horizontal(|ui| {
            for (choice, label) in [
                (ThemeMode::System, "System｜跟隨系統主題"),
                (ThemeMode::Dark, "Dark｜深色主題"),
                (ThemeMode::Light, "Light｜淺色主題"),
            ] {
                let selected = *mode == choice;
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(30.0, 26.0), Sense::click());
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::SelectableLabel,
                        ui.is_enabled(),
                        selected,
                        label,
                    )
                });
                let visuals = ui.style().interact_selectable(&response, selected);
                let fill = if selected || response.hovered() || response.has_focus() {
                    visuals.bg_fill
                } else {
                    ui.visuals().panel_fill
                };
                let stroke = if selected || response.has_focus() {
                    Stroke::new(1.0_f32, Palette::for_ui(ui).accent)
                } else {
                    visuals.bg_stroke
                };
                ui.painter().rect(
                    rect.shrink(0.5),
                    5.0,
                    fill,
                    stroke,
                    egui::StrokeKind::Inside,
                );
                let center = rect.center();
                let color = visuals.fg_stroke.color;
                let line = Stroke::new(1.5_f32, color);
                match choice {
                    ThemeMode::System => {
                        let screen = Rect::from_center_size(
                            center - Vec2::new(0.0, 2.0),
                            Vec2::new(15.0, 10.0),
                        );
                        ui.painter()
                            .rect_stroke(screen, 1.5, line, egui::StrokeKind::Inside);
                        ui.painter().line_segment(
                            [center + Vec2::new(0.0, 3.0), center + Vec2::new(0.0, 7.0)],
                            line,
                        );
                        ui.painter().line_segment(
                            [center + Vec2::new(-4.0, 7.0), center + Vec2::new(4.0, 7.0)],
                            line,
                        );
                    }
                    ThemeMode::Dark => {
                        ui.painter().circle_filled(center, 7.0, color);
                        ui.painter()
                            .circle_filled(center + Vec2::new(3.5, -2.5), 6.0, fill);
                    }
                    ThemeMode::Light => {
                        ui.painter().circle_stroke(center, 3.5, line);
                        for ray in 0..8 {
                            let angle = ray as f32 * std::f32::consts::TAU / 8.0;
                            let direction = Vec2::new(angle.cos(), angle.sin());
                            ui.painter().line_segment(
                                [center + direction * 5.5, center + direction * 8.0],
                                line,
                            );
                        }
                    }
                }
                if response.clicked() && !selected {
                    *mode = choice;
                    changed = true;
                }
                response
                    .on_hover_text(label)
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
            }
        });
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_frame(ctx: &egui::Context, theme: egui::Theme) {
        let _ = ctx.run(
            egui::RawInput {
                system_theme: Some(theme),
                ..Default::default()
            },
            |_| {},
        );
    }

    #[test]
    fn system_theme_tracks_changes_while_explicit_choices_override_them() {
        let ctx = egui::Context::default();
        configure(&ctx);
        apply(&ctx, ThemeMode::System);
        system_frame(&ctx, egui::Theme::Light);
        assert_eq!(ctx.theme(), egui::Theme::Light);
        system_frame(&ctx, egui::Theme::Dark);
        assert_eq!(ctx.theme(), egui::Theme::Dark);
        apply(&ctx, ThemeMode::Light);
        system_frame(&ctx, egui::Theme::Dark);
        assert_eq!(ctx.theme(), egui::Theme::Light);
        assert_eq!(ctx.style().text_styles[&egui::TextStyle::Small].size, 11.0);
        apply(&ctx, ThemeMode::Dark);
        system_frame(&ctx, egui::Theme::Light);
        assert_eq!(ctx.theme(), egui::Theme::Dark);
        assert_eq!(ctx.style().text_styles[&egui::TextStyle::Small].size, 11.0);
    }
}
