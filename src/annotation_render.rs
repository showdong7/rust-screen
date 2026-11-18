//! A shared CPU renderer keeps preview, PNG and clipboard geometry/text consistent.
use std::{borrow::Cow, sync::OnceLock};

use ab_glyph::{Font, FontArc, Glyph, PxScale, ScaleFont, point};
use eframe::egui::{self, Pos2, Rect, Vec2};
use image::RgbaImage;
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::{
    annotation::{Annotation, Tool},
    fonts,
    geometry::CropRect,
};

/// The annotation font shares its bytes with the egui UI font and is parsed once.
pub fn load_font() -> FontArc {
    static FONT: OnceLock<FontArc> = OnceLock::new();
    FONT.get_or_init(|| {
        fonts::cjk()
            .into_iter()
            .chain(fonts::read("segoeui.ttf"))
            .find_map(|bytes| FontArc::try_from_slice(bytes).ok())
            .or_else(|| {
                egui::FontDefinitions::default()
                    .font_data
                    .values()
                    .find_map(|data| FontArc::try_from_vec(data.font.to_vec()).ok())
            })
            .expect("egui includes a valid default font")
    })
    .clone()
}

/// Layout happens in image pixels. Rasterization alone uses the target scale.
fn layout(item: &Annotation, font: &FontArc) -> (Vec<Glyph>, Vec2) {
    let scaled = font.as_scaled(item.style.font_size);
    let line_height = scaled.height() * 1.25;
    let mut x = 0.0_f32;
    let mut baseline = scaled.ascent();
    let mut width = 0.0_f32;
    let mut previous = None;
    let mut glyphs = Vec::new();
    for ch in item.text.chars() {
        if ch == '\r' {
            continue;
        }
        if ch == '\n' {
            width = width.max(x);
            x = 0.0;
            baseline += line_height;
            previous = None;
            continue;
        }
        let ch = if ch == '\t' { ' ' } else { ch };
        let id = scaled.glyph_id(ch);
        if let Some(prev) = previous {
            x += scaled.kern(prev, id);
        }
        glyphs.push(id.with_scale_and_position(item.style.font_size, point(x, baseline)));
        x += scaled.h_advance(id);
        width = width.max(x);
        previous = Some(id);
    }
    (
        glyphs,
        Vec2::new(width.max(1.0), baseline - scaled.descent()),
    )
}

pub fn bounds(item: &Annotation, font: &FontArc) -> Rect {
    if item.tool == Tool::Text {
        Rect::from_min_size(item.start, layout(item, font).1)
    } else {
        let mut rect = Rect::from_two_pos(item.start, item.end);
        for path in item.paths() {
            for point in path {
                rect.extend_with(point);
            }
        }
        if item.style.filled && matches!(item.tool, Tool::Rectangle | Tool::Ellipse) {
            rect
        } else {
            rect.expand(item.style.width * 0.5)
        }
    }
}

/// Straight-alpha RGBA, as used by `image` for PNG and clipboard export.
pub fn overlay<'a>(
    items: impl IntoIterator<Item = &'a Annotation>,
    font: &FontArc,
    view: CropRect,
    width: u32,
    height: u32,
) -> Result<RgbaImage, String> {
    let mut pixels = overlay_premultiplied(items, font, view, width, height)?.take();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = (u32::from(*channel) * 255 + alpha / 2)
                .checked_div(alpha)
                .unwrap_or(0)
                .min(255) as u8;
        }
    }
    RgbaImage::from_raw(width, height, pixels).ok_or_else(|| "標註圖片尺寸錯誤".into())
}

/// Premultiplied (gamma-space) RGBA, which egui textures accept without conversion.
pub fn overlay_premultiplied<'a>(
    items: impl IntoIterator<Item = &'a Annotation>,
    font: &FontArc,
    view: CropRect,
    width: u32,
    height: u32,
) -> Result<Pixmap, String> {
    let mut pixmap = Pixmap::new(width, height).ok_or("無法配置標註繪圖記憶體")?;
    let sx = width as f32 / view.width as f32;
    let sy = height as f32 / view.height as f32;
    let transform = Transform::from_row(
        sx,
        0.0,
        0.0,
        sy,
        -(view.x as f32) * sx,
        -(view.y as f32) * sy,
    );
    let visible = Rect::from_min_size(
        Pos2::new(view.x as f32, view.y as f32),
        Vec2::new(view.width as f32, view.height as f32),
    );
    for item in items {
        if !bounds(item, font)
            .expand(item.style.font_size * 0.2)
            .intersects(visible)
        {
            continue;
        }
        let rgba = item.style.color.to_srgba_unmultiplied();
        if item.tool == Tool::Text {
            let (glyphs, _) = layout(item, font);
            for mut glyph in glyphs {
                glyph.position.x = (item.start.x + glyph.position.x - view.x as f32) * sx;
                glyph.position.y = (item.start.y + glyph.position.y - view.y as f32) * sy;
                glyph.scale = PxScale {
                    x: item.style.font_size * sx,
                    y: item.style.font_size * sy,
                };
                if let Some(outline) = font.outline_glyph(glyph) {
                    let b = outline.px_bounds();
                    // Avoid rasterizing enormous off-screen glyphs.
                    if b.max.x < 0.0
                        || b.max.y < 0.0
                        || b.min.x >= width as f32
                        || b.min.y >= height as f32
                    {
                        continue;
                    }
                    outline.draw(|x, y, coverage| {
                        let x = b.min.x as i64 + i64::from(x);
                        let y = b.min.y as i64 + i64::from(y);
                        if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) {
                            return;
                        }
                        let offset = (y as usize * width as usize + x as usize) * 4;
                        let pixel = &mut pixmap.data_mut()[offset..offset + 4];
                        let alpha = (coverage * f32::from(rgba[3])).round() as u32;
                        for channel in 0..3 {
                            pixel[channel] = ((u32::from(rgba[channel]) * alpha
                                + u32::from(pixel[channel]) * (255 - alpha)
                                + 127)
                                / 255) as u8;
                        }
                        pixel[3] =
                            (alpha + (u32::from(pixel[3]) * (255 - alpha) + 127) / 255) as u8;
                    });
                }
            }
        } else {
            let mut paint = Paint::default();
            paint.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
            paint.anti_alias = true;
            let stroke = Stroke {
                width: item.style.width,
                line_cap: LineCap::Round,
                line_join: LineJoin::Round,
                ..Stroke::default()
            };
            for points in item.paths() {
                let mut path = PathBuilder::new();
                if let Some(first) = points.first() {
                    path.move_to(first.x, first.y);
                }
                for point in points.iter().skip(1) {
                    path.line_to(point.x, point.y);
                }
                let filled =
                    item.style.filled && matches!(item.tool, Tool::Rectangle | Tool::Ellipse);
                if filled {
                    path.close();
                }
                if let Some(path) = path.finish() {
                    if filled {
                        pixmap.fill_path(&path, &paint, FillRule::Winding, transform, None);
                    } else {
                        pixmap.stroke_path(&path, &paint, &stroke, transform, None);
                    }
                }
            }
        }
    }
    Ok(pixmap)
}

/// Borrows the original when there is nothing to crop or draw, avoiding a full copy.
pub fn compose<'a>(
    image: &'a RgbaImage,
    items: &[Annotation],
    font: &FontArc,
    view: CropRect,
) -> Result<Cow<'a, RgbaImage>, String> {
    let mut view = view;
    view.fit(image.width(), image.height());
    let full =
        view.x == 0 && view.y == 0 && view.width == image.width() && view.height == image.height();
    if full && items.is_empty() {
        return Ok(Cow::Borrowed(image));
    }
    let mut output =
        image::imageops::crop_imm(image, view.x, view.y, view.width, view.height).to_image();
    if !items.is_empty() {
        let annotations = overlay(items, font, view, view.width, view.height)?;
        image::imageops::overlay(&mut output, &annotations, 0, 0);
    }
    Ok(Cow::Owned(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::Style;
    use egui::Color32;

    #[test]
    fn crop_is_equivalent_to_cropping_composed_original() {
        let font = load_font();
        let image = RgbaImage::from_pixel(240, 160, image::Rgba([255, 255, 255, 255]));
        let mut items = Vec::new();
        for (i, tool) in [
            Tool::Line,
            Tool::Rectangle,
            Tool::Ellipse,
            Tool::Arrow,
            Tool::Text,
        ]
        .into_iter()
        .enumerate()
        {
            let mut item = Annotation::new(
                tool,
                Pos2::new(20.0 + i as f32 * 8.0, 30.0),
                Style {
                    color: Color32::RED,
                    width: 3.0,
                    font_size: 24.0,
                    filled: false,
                },
            );
            item.end = Pos2::new(170.0, 120.0);
            item.text = "中文 Test\n第二行".into();
            items.push(item);
        }
        let full = compose(
            &image,
            &items,
            &font,
            CropRect {
                x: 0,
                y: 0,
                width: 240,
                height: 160,
            },
        )
        .unwrap();
        let cropped = compose(
            &image,
            &items,
            &font,
            CropRect {
                x: 30,
                y: 20,
                width: 160,
                height: 100,
            },
        )
        .unwrap();
        let expected = image::imageops::crop_imm(&*full, 30, 20, 160, 100).to_image();
        // tiny-skia's scan conversion can differ by one of its 16 coverage
        // steps under an integer translation. Check both error magnitude and
        // affected area so actual coordinate displacement cannot pass.
        let difference = cropped
            .as_raw()
            .iter()
            .zip(expected.as_raw())
            .map(|(a, b)| a.abs_diff(*b) as u64)
            .sum::<u64>();
        let max_difference = cropped
            .as_raw()
            .iter()
            .zip(expected.as_raw())
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(
            difference < 2000 && max_difference <= 16,
            "unexpected crop displacement: total={difference}, max={max_difference}"
        );
        assert_eq!(image.get_pixel(50, 50).0, [255; 4]);
        assert!(full.pixels().any(|p| p.0 != [255; 4]));
    }

    #[test]
    fn unannotated_full_view_borrows_and_premultiplied_preview_matches_export() {
        let font = load_font();
        let image = RgbaImage::from_pixel(64, 48, image::Rgba([1, 2, 3, 255]));
        let full = CropRect {
            x: 0,
            y: 0,
            width: 64,
            height: 48,
        };
        assert!(matches!(
            compose(&image, &[], &font, full).unwrap(),
            Cow::Borrowed(_)
        ));
        let mut item = Annotation::new(
            Tool::Line,
            Pos2::new(4.0, 4.0),
            Style {
                color: Color32::from_rgba_unmultiplied(200, 100, 50, 128),
                width: 6.0,
                font_size: 24.0,
                filled: false,
            },
        );
        item.end = Pos2::new(60.0, 40.0);
        let straight = overlay([&item], &font, full, 64, 48).unwrap();
        let premultiplied = overlay_premultiplied([&item], &font, full, 64, 48).unwrap();
        // egui premultiplies straight RGBA in gamma space exactly like tiny-skia does.
        for (s, p) in straight
            .pixels()
            .zip(premultiplied.data().as_chunks::<4>().0)
        {
            let expected = Color32::from_rgba_unmultiplied(s[0], s[1], s[2], s[3]).to_array();
            for channel in 0..4 {
                assert!(
                    expected[channel].abs_diff(p[channel]) <= 1,
                    "{expected:?} vs {p:?}"
                );
            }
        }
    }

    #[test]
    fn chinese_text_rasterizes_and_scaled_preview_keeps_geometry() {
        let font = load_font();
        assert_ne!(font.glyph_id('標').0, 0);
        let mut item = Annotation::new(
            Tool::Text,
            Pos2::new(10.0, 8.0),
            Style {
                color: Color32::from_rgb(20, 120, 240),
                width: 2.0,
                font_size: 32.0,
                filled: false,
            },
        );
        item.text = "標註\n測試".into();
        let full = overlay(
            &[item.clone()],
            &font,
            CropRect {
                x: 0,
                y: 0,
                width: 200,
                height: 120,
            },
            200,
            120,
        )
        .unwrap();
        let half = overlay(
            &[item],
            &font,
            CropRect {
                x: 0,
                y: 0,
                width: 200,
                height: 120,
            },
            100,
            60,
        )
        .unwrap();
        assert!(full.pixels().filter(|p| p[3] > 0).count() > 100);
        assert!(half.pixels().filter(|p| p[3] > 0).count() > 25);
        assert_eq!(half.get_pixel(0, 0)[3], 0);
    }

    #[test]
    fn solid_shapes_fill_interiors_and_ellipse_corners_stay_empty() {
        let font = load_font();
        let view = CropRect {
            x: 0,
            y: 0,
            width: 200,
            height: 160,
        };
        for tool in [Tool::Rectangle, Tool::Ellipse] {
            let mut item = Annotation::new(
                tool,
                Pos2::new(20.0, 20.0),
                Style {
                    color: Color32::from_rgb(30, 120, 230),
                    width: 8.0,
                    font_size: 32.0,
                    filled: false,
                },
            );
            item.end = Pos2::new(180.0, 140.0);
            let outline = overlay(&[item.clone()], &font, view, 200, 160).unwrap();
            assert_eq!(outline.get_pixel(100, 80)[3], 0);
            assert!(!item.hit(Pos2::new(100.0, 80.0), 0.0, Vec2::ZERO));
            item.style.filled = true;
            let solid = overlay(&[item.clone()], &font, view, 200, 160).unwrap();
            assert_eq!(solid.get_pixel(100, 80).0, [30, 120, 230, 255]);
            assert!(item.hit(Pos2::new(100.0, 80.0), 0.0, Vec2::ZERO));
            if tool == Tool::Ellipse {
                assert_eq!(solid.get_pixel(22, 22)[3], 0);
                assert!(!item.hit(Pos2::new(22.0, 22.0), 0.0, Vec2::ZERO));
            }
            // Cropped export uses the same fill geometry, with no control points.
            let image = RgbaImage::from_pixel(200, 160, image::Rgba([255; 4]));
            let cropped = compose(
                &image,
                &[item],
                &font,
                CropRect {
                    x: 50,
                    y: 40,
                    width: 100,
                    height: 80,
                },
            )
            .unwrap();
            assert_eq!(cropped.get_pixel(50, 40).0, [30, 120, 230, 255]);
        }
    }
}
