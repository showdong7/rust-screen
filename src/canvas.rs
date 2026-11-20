//! The captured image, its GPU preview, crop editing and the annotation overlay.
use std::sync::Arc;

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, TextureHandle, Vec2};
use image::RgbaImage;

use crate::{
    annotation::{Editor, Style},
    annotation_render, annotation_ui,
    config::{ExportOptions, Settings, Watermark},
    geometry::CropRect,
    theme::Palette,
};

const ACCENT: Color32 = Color32::from_rgb(78, 215, 185);
/// Limit only the GPU preview. Export always uses the original physical pixels.
const PREVIEW_LIMIT: u32 = 4096;

#[derive(Clone, Copy)]
pub enum Drag {
    Draw([f32; 2]),
    Move([f32; 2], CropRect),
    Resize,
}

pub struct Canvas {
    pub image: Arc<RgbaImage>,
    texture: TextureHandle,
    pub crop: CropRect,
    pub editing: bool,
    pub drag: Option<Drag>,
    pub annotations: Editor,
    /// No copy of this capture has been written yet (edit-before-save mode).
    pub never_saved: bool,
    /// Desktop position of the image's top-left pixel, so a pin can open in place.
    pub origin: [i32; 2],
    /// Effective DPI of the monitor the capture came from, for `OutputSize::ScreenScale`.
    pub dpi: u32,
    overlay: Option<TextureHandle>,
    overlay_key: Option<(u64, u32, u32, CropRect)>,
    watermark: Option<TextureHandle>,
    watermark_key: Option<(Watermark, u32, u32, u32)>,
}

/// Builds the (possibly downscaled) preview. Expensive for large desktops, so it
/// runs on the capture thread; only the texture upload happens on the UI thread.
pub fn preview_image(image: &RgbaImage) -> egui::ColorImage {
    let longest = image.width().max(image.height());
    if longest > PREVIEW_LIMIT {
        let scale = PREVIEW_LIMIT as f32 / longest as f32;
        let preview = image::imageops::resize(
            image,
            (image.width() as f32 * scale).round().max(1.0) as u32,
            (image.height() as f32 * scale).round().max(1.0) as u32,
            image::imageops::FilterType::Triangle,
        );
        egui::ColorImage::from_rgba_unmultiplied(
            [preview.width() as usize, preview.height() as usize],
            preview.as_raw(),
        )
    } else {
        egui::ColorImage::from_rgba_unmultiplied(
            [image.width() as usize, image.height() as usize],
            image.as_raw(),
        )
    }
}

impl Canvas {
    pub fn new(
        ctx: &egui::Context,
        image: RgbaImage,
        preview: egui::ColorImage,
        settings: &Settings,
    ) -> Self {
        let texture = ctx.load_texture("capture", preview, egui::TextureOptions::LINEAR);
        let mut crop = CropRect {
            x: image.width().saturating_sub(settings.crop_width) / 2,
            y: image.height().saturating_sub(settings.crop_height) / 2,
            width: settings.crop_width,
            height: settings.crop_height,
        };
        crop.fit(image.width(), image.height());
        Self {
            annotations: Editor::new(
                image.width(),
                image.height(),
                Style {
                    color: Color32::from_rgba_unmultiplied(
                        settings.annotation_color[0],
                        settings.annotation_color[1],
                        settings.annotation_color[2],
                        settings.annotation_color[3],
                    ),
                    width: settings.annotation_width,
                    font_size: settings.annotation_font_size,
                    filled: settings.annotation_filled,
                    strength: settings.annotation_strength,
                },
            ),
            image: Arc::new(image),
            texture,
            crop,
            editing: false,
            drag: None,
            never_saved: false,
            origin: [0, 0],
            dpi: crate::export::BASE_DPI,
            overlay: None,
            overlay_key: None,
            watermark: None,
            watermark_key: None,
        }
    }
}

/// Draws the enabled watermark over the preview exactly as save and copy will stamp it.
/// The layer is rebuilt only when the settings or the preview size change.
fn paint_watermark(
    ui: &egui::Ui,
    canvas: &mut Canvas,
    rect: Rect,
    output_width: u32,
    watermark: &Watermark,
    font: &ab_glyph::FontArc,
) {
    if !watermark.active() {
        return;
    }
    let pixels_per_point = ui.ctx().pixels_per_point();
    let width = (rect.width() * pixels_per_point).ceil().clamp(1.0, 4096.0) as u32;
    let height = (rect.height() * pixels_per_point).ceil().clamp(1.0, 4096.0) as u32;
    let key = (watermark.sanitized(), width, height, output_width);
    if canvas.watermark_key.as_ref() != Some(&key) {
        let scale = width as f32 / output_width.max(1) as f32;
        match crate::watermark::layer(watermark, font, width, height, scale) {
            Ok(Some(pixmap)) => {
                let image = egui::ColorImage::from_rgba_premultiplied(
                    [width as usize, height as usize],
                    pixmap.data(),
                );
                if let Some(texture) = &mut canvas.watermark {
                    texture.set(image, egui::TextureOptions::LINEAR);
                } else {
                    canvas.watermark = Some(ui.ctx().load_texture(
                        "watermark",
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
            }
            Ok(None) => canvas.watermark = None,
            Err(error) => {
                canvas.watermark = None;
                ui.painter().text(
                    rect.left_bottom(),
                    egui::Align2::LEFT_BOTTOM,
                    error,
                    egui::FontId::proportional(14.0),
                    Color32::RED,
                );
                return;
            }
        }
        canvas.watermark_key = Some(key);
    }
    if let Some(texture) = &canvas.watermark {
        ui.painter().with_clip_rect(rect).image(
            texture.id(),
            rect,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

pub fn draw_canvas(
    ui: &mut egui::Ui,
    canvas: &mut Canvas,
    fixed_size: bool,
    available: Vec2,
    font: &ab_glyph::FontArc,
    watermark: &Watermark,
    export: &ExportOptions,
) {
    let colors = Palette::for_ui(ui);
    let view = if canvas.editing {
        CropRect {
            x: 0,
            y: 0,
            width: canvas.image.width(),
            height: canvas.image.height(),
        }
    } else {
        canvas.annotations.document.view
    };
    let image_size = Vec2::new(view.width as f32, view.height as f32);
    let scale = (available.x / image_size.x)
        .min(available.y / image_size.y)
        .min(1.0);
    let size = image_size * scale;
    let (outer, _) = ui.allocate_exact_size(available, Sense::hover());
    ui.painter().rect_filled(outer, 10.0, colors.canvas);
    let image_rect = Rect::from_center_size(outer.center(), size);
    ui.painter().image(
        canvas.texture.id(),
        image_rect,
        Rect::from_min_max(
            Pos2::new(
                view.x as f32 / canvas.image.width() as f32,
                view.y as f32 / canvas.image.height() as f32,
            ),
            Pos2::new(
                (view.x + view.width) as f32 / canvas.image.width() as f32,
                (view.y + view.height) as f32 / canvas.image.height() as f32,
            ),
        ),
        Color32::WHITE,
    );
    if !canvas.editing {
        annotation_ui::interact(ui, &mut canvas.annotations, image_rect, view, font);
    }
    paint_annotations(ui, canvas, image_rect, view, font);
    if !canvas.editing {
        // The crop editor shows the whole capture; the watermark follows the final crop.
        // Export stamps the watermark after scaling, so its size is in output pixels.
        let (output_width, _) =
            crate::export::output_size(export, view.width, view.height, canvas.dpi);
        paint_watermark(ui, canvas, image_rect, output_width, watermark, font);
        annotation_ui::selection(ui, &canvas.annotations, image_rect, view, font);
        return;
    }
    let response = ui.interact(
        image_rect,
        ui.id().with("selection"),
        Sense::click_and_drag(),
    );
    let to_pixel = |pos: Pos2| -> [f32; 2] {
        [
            ((pos.x - image_rect.left()) / scale).clamp(0.0, image_size.x),
            ((pos.y - image_rect.top()) / scale).clamp(0.0, image_size.y),
        ]
    };
    let crop_rect = |crop: CropRect| -> Rect {
        Rect::from_min_size(
            image_rect.min + Vec2::new(crop.x as f32, crop.y as f32) * scale,
            Vec2::new(crop.width as f32, crop.height as f32) * scale,
        )
    };
    if response.drag_started() {
        let pos = ui
            .input(|input| input.pointer.press_origin())
            .unwrap_or(image_rect.min);
        let point = to_pixel(pos);
        let selection = crop_rect(canvas.crop);
        canvas.drag = Some(
            if !fixed_size && pos.distance(selection.right_bottom()) < 16.0 {
                Drag::Resize
            } else if selection.contains(pos) {
                Drag::Move(point, canvas.crop)
            } else if fixed_size {
                canvas.crop.x = (point[0] - canvas.crop.width as f32 / 2.0).max(0.0) as u32;
                canvas.crop.y = (point[1] - canvas.crop.height as f32 / 2.0).max(0.0) as u32;
                canvas.crop.fit(canvas.image.width(), canvas.image.height());
                Drag::Move(point, canvas.crop)
            } else {
                Drag::Draw(point)
            },
        );
    }
    if response.dragged()
        && let Some(pos) = response.interact_pointer_pos()
    {
        let point = to_pixel(pos);
        match canvas.drag {
            Some(Drag::Draw(origin)) => {
                canvas.crop = CropRect::from_points(
                    origin,
                    point,
                    canvas.image.width(),
                    canvas.image.height(),
                );
            }
            Some(Drag::Move(origin, start)) => {
                canvas.crop.x = (start.x as f32 + point[0] - origin[0]).round().max(0.0) as u32;
                canvas.crop.y = (start.y as f32 + point[1] - origin[1]).round().max(0.0) as u32;
            }
            Some(Drag::Resize) => {
                canvas.crop.width = (point[0] - canvas.crop.x as f32).round().max(1.0) as u32;
                canvas.crop.height = (point[1] - canvas.crop.y as f32).round().max(1.0) as u32;
                canvas.crop.width = canvas.crop.width.min(canvas.image.width() - canvas.crop.x);
                canvas.crop.height = canvas
                    .crop
                    .height
                    .min(canvas.image.height() - canvas.crop.y);
            }
            None => {}
        }
        canvas.crop.fit(canvas.image.width(), canvas.image.height());
    }
    if response.clicked()
        && fixed_size
        && let Some(pos) = response.interact_pointer_pos()
    {
        let point = to_pixel(pos);
        canvas.crop.x = (point[0] - canvas.crop.width as f32 / 2.0).max(0.0) as u32;
        canvas.crop.y = (point[1] - canvas.crop.height as f32 / 2.0).max(0.0) as u32;
        canvas.crop.fit(canvas.image.width(), canvas.image.height());
    }
    if response.drag_stopped() {
        canvas.drag = None;
    }
    let selection = crop_rect(canvas.crop);
    let shade = Color32::from_black_alpha(160);
    for rect in [
        Rect::from_min_max(
            image_rect.min,
            Pos2::new(image_rect.right(), selection.top()),
        ),
        Rect::from_min_max(
            Pos2::new(image_rect.left(), selection.bottom()),
            image_rect.max,
        ),
        Rect::from_min_max(
            Pos2::new(image_rect.left(), selection.top()),
            selection.left_bottom(),
        ),
        Rect::from_min_max(
            selection.right_top(),
            Pos2::new(image_rect.right(), selection.bottom()),
        ),
    ] {
        ui.painter().rect_filled(rect, 0.0, shade);
    }
    ui.painter().rect_stroke(
        selection,
        0.0,
        Stroke::new(2.0_f32, ACCENT),
        egui::StrokeKind::Inside,
    );
    if !fixed_size {
        ui.painter().rect_filled(
            Rect::from_center_size(selection.right_bottom(), Vec2::splat(10.0)),
            2.0,
            ACCENT,
        );
    }
    let text_pos = Pos2::new(
        selection.left() + 6.0,
        (selection.top() - 22.0).max(image_rect.top() + 4.0),
    );
    let text = format!("{} × {} px", canvas.crop.width, canvas.crop.height);
    ui.painter().text(
        text_pos,
        egui::Align2::LEFT_TOP,
        text,
        egui::FontId::monospace(14.0),
        ACCENT,
    );
    if response.hovered() {
        let pos = response.hover_pos().unwrap_or(image_rect.min);
        ui.ctx().set_cursor_icon(
            if !fixed_size && pos.distance(selection.right_bottom()) < 16.0 {
                egui::CursorIcon::ResizeNwSe
            } else if selection.contains(pos) {
                egui::CursorIcon::Grab
            } else {
                egui::CursorIcon::Crosshair
            },
        );
    }
}

fn paint_annotations(
    ui: &egui::Ui,
    canvas: &mut Canvas,
    rect: Rect,
    view: CropRect,
    font: &ab_glyph::FontArc,
) {
    let pixels_per_point = ui.ctx().pixels_per_point();
    let width = (rect.width() * pixels_per_point).ceil().clamp(1.0, 4096.0) as u32;
    let height = (rect.height() * pixels_per_point).ceil().clamp(1.0, 4096.0) as u32;
    let key = (canvas.annotations.version, width, height, view);
    if canvas.overlay_key != Some(key) {
        let items = canvas.annotations.visible_items();
        match annotation_render::overlay_premultiplied(
            items,
            font,
            &canvas.image,
            view,
            width,
            height,
        ) {
            Ok(pixmap) => {
                // tiny-skia and egui both premultiply in gamma space: upload as-is.
                let color_image = egui::ColorImage::from_rgba_premultiplied(
                    [width as usize, height as usize],
                    pixmap.data(),
                );
                if let Some(texture) = &mut canvas.overlay {
                    texture.set(color_image, egui::TextureOptions::LINEAR);
                } else {
                    canvas.overlay = Some(ui.ctx().load_texture(
                        "annotations",
                        color_image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
                canvas.overlay_key = Some(key);
            }
            Err(error) => {
                ui.painter().text(
                    rect.min,
                    egui::Align2::LEFT_TOP,
                    error,
                    egui::FontId::proportional(14.0),
                    Color32::RED,
                );
                return;
            }
        }
    }
    if let Some(texture) = &canvas.overlay {
        ui.painter().with_clip_rect(rect).image(
            texture.id(),
            rect,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}
