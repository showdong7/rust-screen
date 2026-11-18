use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use eframe::egui::{self, RichText, Sense, Vec2};
use image::RgbaImage;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::{
    annotation_render, annotation_ui,
    canvas::{self, Canvas},
    config::{self, CaptureMode, Settings},
    export, fonts,
    hotkey::HotkeyService,
    platform, selector,
    single_instance::Activation,
    theme::{self, Palette},
    tray::{TrayEvent, TrayService},
};

enum JobResult {
    /// The original pixels, plus the preview prepared off the UI thread.
    Captured(RgbaImage, egui::ColorImage, CaptureMode),
    Cancelled,
    Saved(PathBuf, Option<String>),
    Copied,
}

#[derive(Clone, Copy)]
enum PendingAction {
    Capture(Option<usize>),
    Exit,
}

pub struct ScreenApp {
    window: usize,
    activation: Arc<Activation>,
    settings: Settings,
    shortcut_input: String,
    active_shortcut: Option<String>,
    output_input: String,
    hotkeys: HotkeyService,
    tray: Option<TrayService>,
    hidden_in_tray: Arc<AtomicBool>,
    quitting: bool,
    canvas: Option<Canvas>,
    job: Option<Receiver<Result<JobResult, String>>>,
    status: String,
    is_error: bool,
    settings_changed: Option<Instant>,
    annotation_font: ab_glyph::FontArc,
    pending_action: Option<PendingAction>,
    after_save: Option<PendingAction>,
    logo: egui::TextureHandle,
    show_about: bool,
}

/// Shown in the About dialog.
const APP_VERSION: &str = "V0.9 beta";
const WEBSITE_URL: &str = "https://rust-screen.fujin365.cc/";
const GITHUB_URL: &str = "https://github.com/showdong7/rust-screen";

/// Header logo height in points, next to the 23 pt "RUST SCREEN" title.
const LOGO_SIZE: f32 = 32.0;
/// The 256 px icon is pre-shrunk to this size: egui has no mipmaps, so drawing the
/// original at 32 pt would alias. 64 px stays sharp up to 200 % display scaling.
const LOGO_TEXTURE_PX: u32 = 64;

fn load_logo(ctx: &egui::Context) -> egui::TextureHandle {
    let icon = image::load_from_memory(include_bytes!("../assets/rust-screen.png"))
        .expect("the bundled application icon must be a valid PNG")
        .to_rgba8();
    let icon = image::imageops::resize(
        &icon,
        LOGO_TEXTURE_PX,
        LOGO_TEXTURE_PX,
        image::imageops::FilterType::Lanczos3,
    );
    let size = [icon.width() as usize, icon.height() as usize];
    ctx.load_texture(
        "rust-screen-logo",
        egui::ColorImage::from_rgba_unmultiplied(size, icon.as_raw()),
        egui::TextureOptions::LINEAR,
    )
}

impl ScreenApp {
    pub fn new(cc: &eframe::CreationContext<'_>, activation: Arc<Activation>) -> Self {
        configure_style(&cc.egui_ctx);
        let (settings, error) = match config::load() {
            Ok(settings) => (settings, None),
            Err(error) => (Settings::default(), Some(error)),
        };
        theme::apply(&cc.egui_ctx, settings.theme);
        let window = match cc.window_handle().expect("native window handle").as_raw() {
            RawWindowHandle::Win32(handle) => handle.hwnd.get() as usize,
            _ => unreachable!("Windows build requires a Win32 window"),
        };
        let hidden_in_tray = Arc::new(AtomicBool::new(false));
        let hotkeys = HotkeyService::new(cc.egui_ctx.clone(), window, hidden_in_tray.clone());
        let registration = hotkeys.register(&settings.shortcut);
        let active_shortcut = registration
            .as_ref()
            .ok()
            .map(|_| settings.shortcut.clone());
        let error = error.or_else(|| registration.err());
        let (tray, tray_error) =
            match TrayService::new(cc.egui_ctx.clone(), window, hidden_in_tray.clone()) {
                Ok(tray) => (Some(tray), None),
                Err(error) => (None, Some(format!("{error}；關閉視窗將直接退出程式"))),
            };
        let error = match (error, tray_error) {
            (Some(first), Some(second)) => Some(format!("{first}；{second}")),
            (first, second) => first.or(second),
        };
        activation.bind(cc.egui_ctx.clone(), window, hidden_in_tray.clone());
        Self {
            window,
            activation,
            shortcut_input: settings.shortcut.clone(),
            output_input: settings.output_dir.to_string_lossy().into_owned(),
            settings,
            active_shortcut,
            hotkeys,
            tray,
            hidden_in_tray,
            quitting: false,
            canvas: None,
            job: None,
            status: error
                .clone()
                .unwrap_or_else(|| "準備就緒，按快捷鍵開始截圖".into()),
            is_error: error.is_some(),
            settings_changed: None,
            annotation_font: annotation_render::load_font(),
            pending_action: None,
            after_save: None,
            logo: load_logo(&cc.egui_ctx),
            show_about: false,
        }
    }

    fn message(&mut self, text: impl Into<String>, error: bool) {
        self.status = text.into();
        self.is_error = error;
    }

    fn editing(&self) -> bool {
        self.canvas.as_ref().is_some_and(|canvas| canvas.editing)
    }

    fn hide_to_tray(&mut self) {
        if self.tray.is_none() || self.quitting {
            return;
        }
        self.persist();
        self.hidden_in_tray.store(true, Ordering::Release);
        platform::hide_window(self.window);
    }

    fn request_exit(&mut self, ctx: &egui::Context) {
        if let Some(canvas) = &mut self.canvas {
            canvas.annotations.finish_gesture();
        }
        if self.unsaved_annotations() {
            self.pending_action = Some(PendingAction::Exit);
            platform::restore_window(self.window, true);
            return;
        }
        self.quitting = true;
        self.hidden_in_tray.store(false, Ordering::Release);
        if self.job.is_some() {
            self.message("正在完成目前作業，完成後退出程式…", false);
        }
        ctx.request_repaint();
    }

    fn persist(&mut self) {
        self.settings_changed = None;
        if let Err(error) = config::save(&self.settings) {
            self.message(error, true);
        }
    }

    fn unsaved_annotations(&self) -> bool {
        self.canvas.as_ref().is_some_and(|canvas| {
            canvas.annotations.dirty()
                || (canvas.editing && canvas.crop != canvas.annotations.document.view)
        })
    }

    fn perform_action(&mut self, ctx: &egui::Context, action: PendingAction) {
        match action {
            PendingAction::Capture(target) => self.capture(ctx, target),
            PendingAction::Exit => self.request_exit(ctx),
        }
    }

    fn about_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        let mut close = false;
        let response = egui::Modal::new(egui::Id::new("about")).show(ctx, |ui| {
            let colors = Palette::for_ui(ui);
            ui.set_width(340.0);
            ui.horizontal(|ui| {
                ui.add(egui::Image::new((self.logo.id(), Vec2::splat(LOGO_SIZE))));
                ui.label(
                    RichText::new("RUST SCREEN")
                        .size(20.0)
                        .strong()
                        .color(colors.accent),
                );
            });
            ui.label(RichText::new("螢幕截圖工具").color(colors.muted));
            ui.add_space(10.0);
            egui::Grid::new("about_info")
                .num_columns(2)
                .spacing([14.0, 8.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("版本").strong());
                    ui.label(APP_VERSION);
                    ui.end_row();
                    ui.label(RichText::new("官網").strong());
                    ui.hyperlink_to(WEBSITE_URL, WEBSITE_URL);
                    ui.end_row();
                    ui.label(RichText::new("GitHub").strong());
                    ui.hyperlink_to(GITHUB_URL, GITHUB_URL);
                    ui.end_row();
                });
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui.button("關閉").clicked();
            });
        });
        if close || response.should_close() {
            self.show_about = false;
        }
    }

    fn unsaved_dialog(&mut self, ctx: &egui::Context) {
        let Some(action) = self.pending_action else {
            return;
        };
        let mut choice = 0;
        let response = egui::Modal::new(egui::Id::new("unsaved_annotations")).show(ctx, |ui| {
            ui.heading("尚有未儲存的標註或裁切");
            ui.label("要先另存目前圖片，再繼續操作嗎？");
            if self.is_error {
                ui.colored_label(ui.visuals().error_fg_color, &self.status);
            }
            ui.add_enabled_ui(self.job.is_none(), |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .button(match action {
                            PendingAction::Exit => "儲存並退出",
                            PendingAction::Capture(_) => "儲存後繼續",
                        })
                        .clicked()
                    {
                        choice = 1;
                    }
                    if ui
                        .button(match action {
                            PendingAction::Exit => "捨棄並退出",
                            PendingAction::Capture(_) => "捨棄並繼續",
                        })
                        .clicked()
                    {
                        choice = 2;
                    }
                    if ui.button("返回編輯").clicked() {
                        choice = 3;
                    }
                })
            });
        });
        if choice == 0 && response.should_close() {
            choice = 3;
        }
        match choice {
            1 => {
                self.pending_action = None;
                self.save_capture(ctx);
                if self.job.is_some() {
                    self.after_save = Some(action);
                } else {
                    self.pending_action = Some(action);
                }
            }
            2 => {
                self.pending_action = None;
                self.canvas = None;
                self.perform_action(ctx, action);
            }
            3 => {
                self.pending_action = None;
            }
            _ => {}
        }
    }

    fn copy_capture(&mut self, ctx: &egui::Context) {
        if self.job.is_some() {
            return;
        }
        let Some(canvas) = &mut self.canvas else {
            return;
        };
        canvas.annotations.finish_text();
        let image = Arc::clone(&canvas.image);
        let document = canvas.annotations.document.clone();
        let font = self.annotation_font.clone();
        let (sender, receiver) = mpsc::channel();
        self.job = Some(receiver);
        self.message("正在合成並複製圖片…", false);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result = annotation_render::compose(&image, &document.items, &font, document.view)
                .and_then(|image| export::copy_image(&image))
                .map(|()| JobResult::Copied);
            let _ = sender.send(result);
            ctx.request_repaint();
        });
    }

    fn capture(&mut self, ctx: &egui::Context, target: Option<usize>) {
        if self.job.is_some() || self.quitting || self.pending_action.is_some() {
            return;
        }
        if let Some(canvas) = &mut self.canvas {
            canvas.annotations.finish_gesture();
        }
        if self.unsaved_annotations() {
            self.pending_action = Some(PendingAction::Capture(target));
            platform::restore_window(self.window, true);
            return;
        }
        if self.editing() {
            self.message("請先儲存或取消目前的選取範圍", false);
            return;
        }
        let mode = self.settings.mode;
        let fixed = self
            .settings
            .fixed_size
            .then_some([self.settings.crop_width, self.settings.crop_height]);
        let delay = self.settings.delay_ms;
        let target = target.filter(|target| *target != self.window);
        let (sender, receiver) = mpsc::channel();
        self.job = Some(receiver);
        self.message("正在擷取畫面…", false);
        // Native restoration works even when a hidden winit window no longer receives paints.
        platform::hide_window(self.window);
        let window = self.window;
        let ctx = ctx.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(delay));
            platform::enable_dpi_awareness();
            let result = (|| {
                let bounds = if mode == CaptureMode::Window {
                    platform::window_bounds(target.unwrap_or_else(platform::foreground_window))?
                } else {
                    platform::desktop_bounds()?
                };
                let image = if mode == CaptureMode::Region {
                    // The selector draws straight from the native capture bitmap and
                    // converts only the selected pixels.
                    let Some(image) =
                        selector::select(platform::capture_bitmap(bounds)?, bounds, fixed)?
                    else {
                        return Ok(JobResult::Cancelled);
                    };
                    image
                } else {
                    platform::capture(bounds)?
                };
                let preview = canvas::preview_image(&image);
                Ok(JobResult::Captured(image, preview, mode))
            })();
            let _ = sender.send(result);
            platform::restore_window(window, true);
            ctx.request_repaint();
        });
    }

    fn save_capture(&mut self, ctx: &egui::Context) {
        if self.job.is_some() {
            return;
        }
        let Some(canvas) = &mut self.canvas else {
            return;
        };
        canvas.annotations.finish_text();
        if canvas.editing {
            let before = canvas.annotations.document.clone();
            canvas.annotations.document.view = canvas.crop;
            canvas.annotations.commit(before);
            canvas.editing = false;
            canvas.drag = None;
        }
        let image = Arc::clone(&canvas.image);
        let document = canvas.annotations.document.clone();
        let font = self.annotation_font.clone();
        let output_dir = PathBuf::from(self.output_input.trim());
        if self.output_input.trim().is_empty() {
            self.message("請先輸入儲存資料夾", true);
            return;
        }
        let auto_copy = self.settings.auto_copy;
        let (sender, receiver) = mpsc::channel();
        self.job = Some(receiver);
        self.message("正在儲存 PNG…", false);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result = (|| {
                let image =
                    annotation_render::compose(&image, &document.items, &font, document.view)?;
                let (path, warning) = export::save(&image, &output_dir, auto_copy)?;
                Ok(JobResult::Saved(path, warning))
            })();
            let _ = sender.send(result);
            ctx.request_repaint();
        });
    }

    fn check_job(&mut self, ctx: &egui::Context) {
        let result = match self.job.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => result,
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                Err("背景作業意外停止，請再試一次".into())
            }
            _ => return,
        };
        self.job = None;
        match result {
            Ok(JobResult::Captured(image, preview, mode)) => {
                if mode == CaptureMode::Region && !self.settings.fixed_size {
                    self.settings.crop_width = image.width();
                    self.settings.crop_height = image.height();
                    self.settings_changed = Some(Instant::now());
                }
                self.canvas = Some(Canvas::new(ctx, image, preview, &self.settings));
                self.save_capture(ctx);
            }
            Ok(JobResult::Cancelled) => self.message("已取消螢幕選取", false),
            Ok(JobResult::Saved(path, warning)) => {
                if let Some(canvas) = &mut self.canvas {
                    canvas.annotations.saved();
                }
                if let Some(warning) = warning {
                    self.message(
                        format!("已儲存 {}；剪貼簿失敗：{warning}", path.display()),
                        true,
                    );
                } else {
                    self.message(
                        format!(
                            "已儲存 {}{}",
                            path.display(),
                            if self.settings.auto_copy {
                                "，並複製到剪貼簿"
                            } else {
                                ""
                            }
                        ),
                        false,
                    );
                }
                if let Some(action) = self.after_save.take() {
                    self.perform_action(ctx, action);
                }
            }
            Ok(JobResult::Copied) => self.message("圖片（含標註）已複製到剪貼簿", false),
            Err(error) => {
                self.pending_action = self.after_save.take();
                self.message(error, true);
            }
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let colors = Palette::for_ui(ui);
        let busy = self.job.is_some();
        let editing = self.editing();
        ui.add_space(10.0);
        section(ui, "01", "截圖模式");
        ui.add_space(8.0);
        for mode in [
            CaptureMode::Region,
            CaptureMode::Desktop,
            CaptureMode::Window,
        ] {
            ui.add_enabled_ui(!busy && !editing, |ui| {
                if ui
                    .selectable_value(&mut self.settings.mode, mode, mode.label())
                    .changed()
                {
                    self.settings_changed = Some(Instant::now());
                }
            });
        }
        ui.add_space(10.0);
        let hint = match self.settings.mode {
            CaptureMode::Region => "直接在螢幕拖曳選取，放開即儲存；Esc 或右鍵取消。",
            CaptureMode::Desktop => "擷取所有螢幕，保留原始像素。",
            CaptureMode::Window => "快捷鍵擷取目前前景視窗；按鈕則擷取本程式隱藏後的前景視窗。",
        };
        ui.label(RichText::new(hint).small().color(colors.muted));
        if self.settings.mode == CaptureMode::Region {
            ui.add_enabled_ui(!busy && !editing, |ui| {
                let mut changed = ui
                    .checkbox(&mut self.settings.fixed_size, "固定截圖尺寸")
                    .changed();
                ui.add_enabled_ui(self.settings.fixed_size, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("寬");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut self.settings.crop_width)
                                    .range(1..=32768)
                                    .suffix(" px"),
                            )
                            .changed();
                        ui.label("高");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut self.settings.crop_height)
                                    .range(1..=32768)
                                    .suffix(" px"),
                            )
                            .changed();
                    });
                });
                if changed {
                    self.settings_changed = Some(Instant::now());
                }
            });
        }
        ui.add_space(12.0);
        if ui
            .add_enabled(
                !busy && !editing,
                egui::Button::new(RichText::new("開始截圖").strong().color(colors.button_text))
                    .fill(colors.accent)
                    .min_size(Vec2::new(ui.available_width(), 42.0)),
            )
            .clicked()
        {
            self.capture(ctx, None);
        }
        ui.add_space(22.0);
        section(ui, "02", "全域快捷鍵");
        ui.add_space(8.0);
        ui.label("可在其他程式或最小化時使用");
        ui.add(
            egui::TextEdit::singleline(&mut self.shortcut_input)
                .hint_text("Ctrl+Shift+S")
                .desired_width(f32::INFINITY),
        );
        if ui.button("套用快捷鍵").clicked() {
            match self.hotkeys.register(&self.shortcut_input) {
                Ok(()) => {
                    self.settings.shortcut = self.shortcut_input.trim().to_string();
                    self.active_shortcut = Some(self.settings.shortcut.clone());
                    self.message(format!("快捷鍵已啟用：{}", self.settings.shortcut), false);
                    self.persist();
                }
                Err(error) => self.message(error, true),
            }
        }
        ui.label(
            RichText::new(format!(
                "目前：{}",
                self.active_shortcut.as_deref().unwrap_or("未啟用")
            ))
            .small()
            .color(if self.active_shortcut.is_some() {
                colors.accent
            } else {
                colors.error
            }),
        );
        ui.label(
            RichText::new("支援 Ctrl / Shift / Alt / Win + 字母、數字、F1～F24、PrintScreen")
                .small()
                .color(colors.muted),
        );

        ui.add_space(22.0);
        section(ui, "03", "儲存與選項");
        ui.add_space(8.0);
        ui.label("PNG 儲存資料夾");
        if ui
            .add(egui::TextEdit::singleline(&mut self.output_input).desired_width(f32::INFINITY))
            .changed()
            && !self.output_input.trim().is_empty()
        {
            self.settings.output_dir = PathBuf::from(self.output_input.trim());
            self.settings_changed = Some(Instant::now());
        }
        ui.horizontal(|ui| {
            if ui.button("開啟資料夾").clicked() {
                self.open_output();
            }
            if ui.button("儲存設定").clicked() {
                self.message("設定已儲存", false);
                self.persist();
            }
        });
        if ui
            .checkbox(&mut self.settings.auto_copy, "儲存時複製到剪貼簿")
            .changed()
        {
            self.settings_changed = Some(Instant::now());
        }
        ui.horizontal(|ui| {
            ui.label("擷取延遲");
            if ui
                .add(
                    egui::DragValue::new(&mut self.settings.delay_ms)
                        .range(200..=10000)
                        .speed(50)
                        .suffix(" ms"),
                )
                .changed()
            {
                self.settings_changed = Some(Instant::now());
            }
        });
        ui.label(
            RichText::new("擷取前會暫時隱藏本程式，避免拍到操作介面。")
                .small()
                .color(colors.muted),
        );
        ui.add_space(16.0);
        ui.separator();
        ui.label(
            RichText::new("WINDOWS 10 / 11   ·   PNG")
                .small()
                .color(colors.muted),
        );
    }

    fn open_output(&mut self) {
        let directory = PathBuf::from(self.output_input.trim());
        if self.output_input.trim().is_empty() {
            self.message("請先輸入儲存資料夾", true);
            return;
        }
        let result = fs::create_dir_all(&directory).and_then(|_| {
            // Explorer accepts paths directly; there is no command-shell interpolation.
            std::process::Command::new("explorer.exe")
                .arg(directory)
                .spawn()
        });
        if let Err(error) = result {
            self.message(format!("無法開啟資料夾：{error}"), true);
        }
    }

    fn preview(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let colors = Palette::for_ui(ui);
        let busy = self.job.is_some()
            || self.pending_action.is_some()
            || self.after_save.is_some()
            || self.quitting;
        let editing = self.editing();
        ui.horizontal(|ui| {
            ui.heading(if editing {
                "選取截圖範圍"
            } else {
                "截圖預覽"
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(canvas) = &self.canvas {
                    ui.label(
                        RichText::new(format!(
                            "{} × {} px",
                            canvas.annotations.document.view.width,
                            canvas.annotations.document.view.height
                        ))
                        .color(colors.muted),
                    );
                }
            });
        });
        ui.add_space(8.0);
        if !editing && let Some(canvas) = &mut self.canvas {
            ui.add_enabled_ui(!busy, |ui| {
                annotation_ui::toolbar(ui, &mut canvas.annotations)
            });
            let style = canvas.annotations.style;
            let color = style.color.to_srgba_unmultiplied();
            if self.settings.annotation_color != color
                || self.settings.annotation_width != style.width
                || self.settings.annotation_font_size != style.font_size
                || self.settings.annotation_filled != style.filled
            {
                self.settings.annotation_color = color;
                self.settings.annotation_width = style.width;
                self.settings.annotation_font_size = style.font_size;
                self.settings.annotation_filled = style.filled;
                self.settings_changed = Some(Instant::now());
            }
            ui.add_space(6.0);
        }
        if editing {
            let canvas = self.canvas.as_mut().unwrap();
            let mut changed = false;
            ui.horizontal_wrapped(|ui| {
                changed |= ui
                    .checkbox(&mut self.settings.fixed_size, "固定尺寸")
                    .changed();
                ui.label("寬");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut canvas.crop.width)
                            .range(1..=canvas.image.width())
                            .suffix(" px"),
                    )
                    .changed();
                ui.label("高");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut canvas.crop.height)
                            .range(1..=canvas.image.height())
                            .suffix(" px"),
                    )
                    .changed();
                if ui.small_button("800 × 600").clicked() {
                    canvas.crop.width = 800;
                    canvas.crop.height = 600;
                    changed = true;
                }
                if ui.small_button("1280 × 720").clicked() {
                    canvas.crop.width = 1280;
                    canvas.crop.height = 720;
                    changed = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("X");
                ui.add(
                    egui::DragValue::new(&mut canvas.crop.x)
                        .range(0..=canvas.image.width().saturating_sub(canvas.crop.width)),
                );
                ui.label("Y");
                ui.add(
                    egui::DragValue::new(&mut canvas.crop.y)
                        .range(0..=canvas.image.height().saturating_sub(canvas.crop.height)),
                );
                ui.label(RichText::new("原始像素座標").small().color(colors.muted));
            });
            canvas.crop.fit(canvas.image.width(), canvas.image.height());
            if changed {
                self.settings.crop_width = canvas.crop.width;
                self.settings.crop_height = canvas.crop.height;
                self.settings_changed = Some(Instant::now());
            }
            ui.label(
                RichText::new(if self.settings.fixed_size {
                    "拖曳移動固定大小的範圍；在其他位置點選可重新定位。"
                } else {
                    "空白處拖曳建立範圍；框內拖曳移動；右下角拖曳調整大小。"
                })
                .small()
                .color(colors.muted),
            );
            ui.add_space(6.0);
        }

        let mut save = false;
        let mut cancel = false;
        let mut copy = false;
        let mut refine = false;
        if self.canvas.is_some() {
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if editing {
                        save = ui
                            .button(RichText::new("儲存選取範圍  [Enter]").color(colors.accent))
                            .clicked();
                        cancel = ui.button("取消  [Esc]").clicked();
                    } else {
                        copy = ui.button("複製圖片").clicked();
                        if ui.button("開啟儲存位置").clicked() {
                            self.open_output();
                        }
                        save = ui
                            .button(
                                if self
                                    .canvas
                                    .as_ref()
                                    .is_some_and(|c| !c.annotations.document.items.is_empty())
                                {
                                    "另存標註 PNG"
                                } else {
                                    "另存一份 PNG"
                                },
                            )
                            .clicked();
                        refine = ui.button("調整範圍").clicked();
                    }
                });
            });
        }
        ui.add_space(10.0);
        if let Some(canvas) = &mut self.canvas {
            let available = ui.available_size().max(Vec2::new(1.0, 1.0));
            ui.add_enabled_ui(!busy, |ui| {
                canvas::draw_canvas(
                    ui,
                    canvas,
                    self.settings.fixed_size,
                    available,
                    &self.annotation_font,
                )
            });
            if canvas.editing
                && (self.settings.crop_width != canvas.crop.width
                    || self.settings.crop_height != canvas.crop.height)
            {
                self.settings.crop_width = canvas.crop.width;
                self.settings.crop_height = canvas.crop.height;
                self.settings_changed = Some(Instant::now());
            }
            if canvas.editing && !busy && !ctx.wants_keyboard_input() {
                save |= ctx.input(|input| input.key_pressed(egui::Key::Enter));
                cancel |= ctx.input(|input| input.key_pressed(egui::Key::Escape));
            }
        } else {
            let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
            ui.painter().rect_filled(rect, 12.0, colors.empty);
            ui.painter().text(
                rect.center() - Vec2::new(0.0, 28.0),
                egui::Align2::CENTER_CENTER,
                "準備擷取你的畫面",
                egui::FontId::proportional(26.0),
                colors.heading,
            );
            ui.painter().text(
                rect.center() + Vec2::new(0.0, 18.0),
                egui::Align2::CENTER_CENTER,
                self.active_shortcut
                    .as_deref()
                    .unwrap_or("按左側「開始截圖」"),
                egui::FontId::proportional(20.0),
                colors.accent,
            );
            ui.painter().text(
                rect.center() + Vec2::new(0.0, 56.0),
                egui::Align2::CENTER_CENTER,
                "可調整視窗大小 · 精確像素範圍 · 全域快捷鍵",
                egui::FontId::proportional(14.0),
                colors.muted,
            );
        }
        if save {
            self.save_capture(ctx);
        }
        if refine && let Some(canvas) = &mut self.canvas {
            canvas.annotations.finish_text();
            canvas.annotations.cancel_gesture();
            canvas.editing = true;
            canvas.crop = canvas.annotations.document.view;
            self.message(
                "可調整寬高或拖曳微調範圍，Enter 儲存新圖片，Esc 取消編輯",
                false,
            );
        }
        if cancel {
            if let Some(canvas) = &mut self.canvas {
                canvas.editing = false;
                canvas.drag = None;
            }
            self.message("已取消裁切，保留原圖與標註", false);
        }
        if copy {
            self.copy_capture(ctx);
        }
        if self.job.is_none()
            && !busy
            && let Some(canvas) = &mut self.canvas
        {
            annotation_ui::text_window(ctx, &mut canvas.annotations);
        }
    }
}

impl eframe::App for ScreenApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.activation.take_requested() {
            self.hidden_in_tray.store(false, Ordering::Release);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        self.check_job(ctx);
        while let Some(event) = self
            .tray
            .as_ref()
            .and_then(|tray| tray.events.try_recv().ok())
        {
            match event {
                TrayEvent::Open => platform::restore_window(self.window, true),
                TrayEvent::Exit => self.request_exit(ctx),
                TrayEvent::Error(error) => {
                    self.tray = None;
                    self.message(error, true);
                }
            }
        }
        if ctx.input(|input| input.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.tray.is_some() {
                self.hide_to_tray();
            } else {
                self.request_exit(ctx);
            }
        } else if ctx.input(|input| input.viewport().close_requested()) && self.job.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.quitting && self.job.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let mut target = None;
        while let Ok(window) = self.hotkeys.events.try_recv() {
            target = Some(window);
        }
        if let Some(window) = target {
            self.capture(ctx, Some(window));
        }
        if self
            .settings_changed
            .is_some_and(|changed| changed.elapsed() >= Duration::from_millis(800))
        {
            self.persist();
        }
        if self.job.is_some() || self.settings_changed.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            let colors = Palette::for_ui(ui);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                // The theme's 9 pt item spacing separates the logo from the title.
                ui.add(egui::Image::new((self.logo.id(), Vec2::splat(LOGO_SIZE))));
                ui.label(
                    RichText::new("RUST SCREEN")
                        .size(23.0)
                        .strong()
                        .color(colors.accent),
                );
                ui.label(RichText::new(" / 螢幕截圖工具").color(colors.muted));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("退出程式").clicked() {
                        self.request_exit(ctx);
                    }
                    if self.tray.is_some() && ui.button("縮至系統匣").clicked() {
                        self.hide_to_tray();
                    }
                    if ui.button("關於").clicked() {
                        self.show_about = true;
                    }
                    ui.label(
                        RichText::new(if self.active_shortcut.is_some() {
                            "全域快捷鍵已啟用"
                        } else {
                            "全域快捷鍵未啟用"
                        })
                        .small()
                        .color(if self.active_shortcut.is_some() {
                            colors.accent
                        } else {
                            colors.error
                        }),
                    );
                });
            });
            ui.add_space(10.0);
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            let colors = Palette::for_ui(ui);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let status_width =
                    (ui.available_width() - theme::PICKER_WIDTH - ui.spacing().item_spacing.x)
                        .max(0.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(status_width, 26.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.set_min_width(status_width);
                        if self.job.is_some() {
                            ui.spinner();
                        }
                        ui.add(
                            egui::Label::new(RichText::new(&self.status).color(if self.is_error {
                                colors.error
                            } else {
                                colors.muted
                            }))
                            .wrap(),
                        );
                    },
                );
                if theme::picker(ui, &mut self.settings.theme) {
                    theme::apply(ctx, self.settings.theme);
                    self.settings_changed = Some(Instant::now());
                }
            });
            ui.add_space(6.0);
        });
        egui::SidePanel::left("settings")
            .exact_width(278.0)
            .resizable(false)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.sidebar(ui, ctx));
            });
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(10.0);
            self.preview(ui, ctx);
        });
        self.unsaved_dialog(ctx);
        self.about_dialog(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Err(error) = config::save(&self.settings) {
            eprintln!("{error}");
        }
    }
}

fn section(ui: &mut egui::Ui, number: &str, title: &str) {
    let colors = Palette::for_ui(ui);
    ui.horizontal(|ui| {
        // Just under the 16 pt title; monospace keeps 01/02/03 equally wide so titles align.
        ui.label(
            RichText::new(number)
                .size(15.0)
                .monospace()
                .strong()
                .color(colors.accent),
        );
        ui.label(RichText::new(title).strong().size(16.0));
    });
}

fn configure_style(ctx: &egui::Context) {
    let mut definitions = egui::FontDefinitions::default();
    // Shared with the annotation renderer; the file is read only once.
    if let Some(bytes) = fonts::cjk() {
        definitions
            .font_data
            .insert("cjk".into(), egui::FontData::from_static(bytes).into());
        definitions
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "cjk".into());
        definitions
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("cjk".into());
    }
    ctx.set_fonts(definitions);
    theme::configure(ctx);
}
