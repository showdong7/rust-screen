//! A shared CPU renderer keeps preview, PNG and clipboard geometry/text consistent.
use std::{borrow::Cow, ops::Range, sync::OnceLock};

use ab_glyph::{Font, FontArc, Glyph, PxScale, ScaleFont, point};
use eframe::egui::{self, Pos2, Rect, Vec2};
use image::RgbaImage;
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::{
    annotation::{Annotation, STRENGTH_RANGE, Tool},
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
        if item.tool.is_effect()
            || (item.style.filled && matches!(item.tool, Tool::Rectangle | Tool::Ellipse))
        {
            rect
        } else {
            rect.expand(item.style.width * 0.5)
        }
    }
}

/// The annotation layer alone, in straight-alpha RGBA, for tests to inspect.
/// `source` is the original capture, which mosaic and blur regions are drawn from.
#[cfg(test)]
pub fn overlay<'a>(
    items: impl IntoIterator<Item = &'a Annotation>,
    font: &FontArc,
    source: &RgbaImage,
    view: CropRect,
    width: u32,
    height: u32,
) -> Result<RgbaImage, String> {
    unpremultiply(overlay_premultiplied(
        items, font, source, view, width, height,
    )?)
}

/// Source-over blend of a premultiplied layer onto straight-alpha pixels of the same
/// size. Integer maths keeps opaque screenshots exactly opaque, which
/// `image::imageops::overlay` does not guarantee for partly transparent sources.
pub fn blend(image: &mut RgbaImage, layer: &Pixmap) {
    for (pixel, source) in image.pixels_mut().zip(layer.data().as_chunks::<4>().0) {
        let source_alpha = u32::from(source[3]);
        if source_alpha == 0 {
            continue;
        }
        let keep = 255 - source_alpha;
        let alpha = u32::from(pixel[3]);
        let out_alpha = source_alpha + (alpha * keep + 127) / 255;
        for channel in 0..3 {
            let below = (u32::from(pixel[channel]) * alpha + 127) / 255;
            let premultiplied = u32::from(source[channel]) + (below * keep + 127) / 255;
            pixel[channel] = ((premultiplied * 255 + out_alpha / 2) / out_alpha).min(255) as u8;
        }
        pixel[3] = out_alpha.min(255) as u8;
    }
}

/// Converts a tiny-skia layer to the straight alpha `image` expects.
#[cfg(test)]
fn unpremultiply(pixmap: Pixmap) -> Result<RgbaImage, String> {
    let (width, height) = (pixmap.width(), pixmap.height());
    let mut pixels = pixmap.take();
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
/// Mosaic and blur regions are opaque and painted first, beneath every other annotation.
pub fn overlay_premultiplied<'a>(
    items: impl IntoIterator<Item = &'a Annotation>,
    font: &FontArc,
    source: &RgbaImage,
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
    let items: Vec<&Annotation> = items.into_iter().collect();
    for item in items.iter().filter(|item| item.tool.is_effect()) {
        if Rect::from_two_pos(item.start, item.end).intersects(visible) {
            paint_effect(&mut pixmap, item, source, view, sx, sy);
        }
    }
    for item in items.into_iter().filter(|item| !item.tool.is_effect()) {
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

/// Target pixels along one axis whose centres fall inside `[min, max)` of the original.
fn covered(min: f32, max: f32, origin: u32, scale: f32, limit: u32) -> Range<i64> {
    let edge = |value: f32| {
        (((value - origin as f32) * scale - 0.5).ceil() as i64).clamp(0, i64::from(limit))
    };
    edge(min)..edge(max)
}

/// Original pixels under a target pixel along one axis, clamped to the image. Downscaled
/// previews average this footprint, so fine detail such as text does not alias.
fn footprint(target: i64, origin: u32, scale: f32, size: u32) -> Range<u32> {
    let edge = |value: i64| {
        ((origin as f32 + value as f32 / scale).floor() as i64).clamp(0, i64::from(size) - 1)
    };
    let start = edge(target);
    start as u32..edge(target + 1).max(start + 1) as u32
}

fn put(pixmap: &mut Pixmap, x: i64, y: i64, rgb: [u8; 3]) {
    let offset = (y as usize * pixmap.width() as usize + x as usize) * 4;
    pixmap.data_mut()[offset..offset + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
}

/// Mosaic and blur replace their rectangle with opaque pixels derived only from the
/// original capture, so the result is identical in the preview and the export and
/// no original pixel inside the rectangle survives.
fn paint_effect(
    pixmap: &mut Pixmap,
    item: &Annotation,
    source: &RgbaImage,
    view: CropRect,
    sx: f32,
    sy: f32,
) {
    if source.width() == 0 || source.height() == 0 {
        return;
    }
    let rect = Rect::from_two_pos(item.start, item.end).intersect(Rect::from_min_size(
        Pos2::ZERO,
        Vec2::new(source.width() as f32, source.height() as f32),
    ));
    let xs = covered(rect.left(), rect.right(), view.x, sx, pixmap.width());
    let ys = covered(rect.top(), rect.bottom(), view.y, sy, pixmap.height());
    if xs.is_empty() || ys.is_empty() {
        return;
    }
    let strength = item
        .style
        .strength
        .clamp(*STRENGTH_RANGE.start(), *STRENGTH_RANGE.end());
    match item.tool {
        Tool::Mosaic => mosaic(pixmap, rect, strength, source, view, [sx, sy], xs, ys),
        Tool::Blur => blur(pixmap, strength, source, view, [sx, sy], xs, ys),
        _ => {}
    }
}

/// Original pixels covered by cell `index` of a grid starting at `min`, clipped to `max`.
fn cell_span(min: f32, max: f32, index: usize, cell: f32, size: u32) -> Range<u32> {
    let start = min + index as f32 * cell;
    let end = (start + cell).min(max);
    let edge = |value: f32| ((value - 0.5).ceil() as i64).clamp(0, i64::from(size)) as u32;
    if edge(end) > edge(start) {
        edge(start)..edge(end)
    } else {
        let centre = (((start + end) * 0.5).floor() as i64).clamp(0, i64::from(size) - 1) as u32;
        centre..centre + 1
    }
}

/// Mean colour of a cell. At most 8 × 8 evenly spaced samples keep large cells cheap;
/// the preview and the export sample identically.
fn average(source: &RgbaImage, xs: Range<u32>, ys: Range<u32>) -> [u8; 3] {
    let step_x = (xs.end - xs.start).div_ceil(8).max(1) as usize;
    let step_y = (ys.end - ys.start).div_ceil(8).max(1) as usize;
    let mut sum = [0_u32; 3];
    let mut count = 0_u32;
    for y in ys.step_by(step_y) {
        for x in xs.clone().step_by(step_x) {
            let pixel = source.get_pixel(x, y).0;
            for channel in 0..3 {
                sum[channel] += u32::from(pixel[channel]);
            }
            count += 1;
        }
    }
    sum.map(|value| ((value + count / 2) / count.max(1)) as u8)
}

#[allow(clippy::too_many_arguments)]
fn mosaic(
    pixmap: &mut Pixmap,
    rect: Rect,
    cell: f32,
    source: &RgbaImage,
    view: CropRect,
    [sx, sy]: [f32; 2],
    xs: Range<i64>,
    ys: Range<i64>,
) {
    // Cells are anchored to the rectangle, so moving it moves the grid with it.
    let column = |x: i64| {
        ((view.x as f32 + (x as f32 + 0.5) / sx - rect.left()) / cell)
            .floor()
            .max(0.0) as usize
    };
    let first = column(xs.start);
    let last = column(xs.end - 1).max(first);
    // Colours of the current row of cells, computed only for cells actually shown.
    let mut row = None;
    let mut colours: Vec<Option<[u8; 3]>> = vec![None; last - first + 1];
    for y in ys {
        let index = ((view.y as f32 + (y as f32 + 0.5) / sy - rect.top()) / cell)
            .floor()
            .max(0.0) as usize;
        if row != Some(index) {
            row = Some(index);
            colours.fill(None);
        }
        let span_y = cell_span(rect.top(), rect.bottom(), index, cell, source.height());
        for x in xs.clone() {
            let column = column(x).clamp(first, last);
            let colour = *colours[column - first].get_or_insert_with(|| {
                let span_x = cell_span(rect.left(), rect.right(), column, cell, source.width());
                average(source, span_x, span_y.clone())
            });
            put(pixmap, x, y, colour);
        }
    }
}

/// One box-blur pass along every line of a buffer; `index` maps (line, position).
fn box_pass(
    buffer: &mut [[u8; 3]],
    lines: usize,
    length: usize,
    index: impl Fn(usize, usize) -> usize,
    radius: usize,
    scratch: &mut Vec<[u8; 3]>,
) {
    let window = (2 * radius + 1) as u32;
    let last = length as isize - 1;
    for line in 0..lines {
        scratch.clear();
        scratch.extend((0..length).map(|i| buffer[index(line, i)]));
        let at = |i: isize| scratch[i.clamp(0, last) as usize];
        let mut sum = [0_u32; 3];
        for i in -(radius as isize)..=radius as isize {
            let pixel = at(i);
            for channel in 0..3 {
                sum[channel] += u32::from(pixel[channel]);
            }
        }
        for i in 0..length {
            buffer[index(line, i)] = sum.map(|value| ((value + window / 2) / window) as u8);
            let add = at(i as isize + radius as isize + 1);
            let remove = at(i as isize - radius as isize);
            for channel in 0..3 {
                sum[channel] = sum[channel] + u32::from(add[channel]) - u32::from(remove[channel]);
            }
        }
    }
}

/// Three box passes per axis approximate a Gaussian. The radius is in original pixels
/// and scales with the target, so a downscaled preview looks like the export.
fn blur(
    pixmap: &mut Pixmap,
    strength: f32,
    source: &RgbaImage,
    view: CropRect,
    [sx, sy]: [f32; 2],
    xs: Range<i64>,
    ys: Range<i64>,
) {
    let rx = (strength * sx * 0.5).round().max(1.0) as i64;
    let ry = (strength * sy * 0.5).round().max(1.0) as i64;
    // Include the surrounding pixels the passes read, so edges blend naturally.
    let bx = xs.start - rx * 3..xs.end + rx * 3;
    let by = ys.start - ry * 3..ys.end + ry * 3;
    let width = (bx.end - bx.start) as usize;
    let height = (by.end - by.start) as usize;
    let columns: Vec<Range<u32>> = bx
        .clone()
        .map(|x| footprint(x, view.x, sx, source.width()))
        .collect();
    let mut buffer = Vec::with_capacity(width * height);
    for y in by.clone() {
        let rows = footprint(y, view.y, sy, source.height());
        buffer.extend(
            columns
                .iter()
                .map(|columns| average(source, columns.clone(), rows.clone())),
        );
    }
    let mut scratch = Vec::new();
    for _ in 0..3 {
        box_pass(
            &mut buffer,
            height,
            width,
            |line, i| line * width + i,
            rx as usize,
            &mut scratch,
        );
    }
    for _ in 0..3 {
        box_pass(
            &mut buffer,
            width,
            height,
            |line, i| i * width + line,
            ry as usize,
            &mut scratch,
        );
    }
    for y in ys {
        let row = (y - by.start) as usize * width;
        for x in xs.clone() {
            put(pixmap, x, y, buffer[row + (x - bx.start) as usize]);
        }
    }
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
        let annotations = overlay_premultiplied(items, font, image, view, view.width, view.height)?;
        blend(&mut output, &annotations);
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
                    strength: 12.0,
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
                strength: 12.0,
            },
        );
        item.end = Pos2::new(60.0, 40.0);
        let straight = overlay([&item], &font, &image, full, 64, 48).unwrap();
        let premultiplied = overlay_premultiplied([&item], &font, &image, full, 64, 48).unwrap();
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
        let image = RgbaImage::from_pixel(200, 120, image::Rgba([255; 4]));
        let mut item = Annotation::new(
            Tool::Text,
            Pos2::new(10.0, 8.0),
            Style {
                color: Color32::from_rgb(20, 120, 240),
                width: 2.0,
                font_size: 32.0,
                filled: false,
                strength: 12.0,
            },
        );
        item.text = "標註\n測試".into();
        let full = overlay(
            &[item.clone()],
            &font,
            &image,
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
            &image,
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
        let image = RgbaImage::from_pixel(200, 160, image::Rgba([255; 4]));
        for tool in [Tool::Rectangle, Tool::Ellipse] {
            let mut item = Annotation::new(
                tool,
                Pos2::new(20.0, 20.0),
                Style {
                    color: Color32::from_rgb(30, 120, 230),
                    width: 8.0,
                    font_size: 32.0,
                    filled: false,
                    strength: 12.0,
                },
            );
            item.end = Pos2::new(180.0, 140.0);
            let outline = overlay(&[item.clone()], &font, &image, view, 200, 160).unwrap();
            assert_eq!(outline.get_pixel(100, 80)[3], 0);
            assert!(!item.hit(Pos2::new(100.0, 80.0), 0.0, Vec2::ZERO));
            item.style.filled = true;
            let solid = overlay(&[item.clone()], &font, &image, view, 200, 160).unwrap();
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

    #[test]
    fn antialiased_annotations_keep_an_opaque_capture_fully_opaque() {
        let font = load_font();
        let image = RgbaImage::from_pixel(240, 120, image::Rgba([255; 4]));
        let mut text = Annotation::new(
            Tool::Text,
            Pos2::new(10.0, 10.0),
            Style {
                color: Color32::from_rgba_unmultiplied(235, 55, 55, 200),
                width: 3.0,
                font_size: 40.0,
                filled: false,
                strength: 12.0,
            },
        );
        text.text = "標註 Alpha".into();
        let mut line = text.clone();
        line.tool = Tool::Line;
        line.start = Pos2::new(5.0, 100.0);
        line.end = Pos2::new(230.0, 80.0);
        let view = CropRect {
            x: 0,
            y: 0,
            width: 240,
            height: 120,
        };
        let output = compose(&image, &[text, line], &font, view).unwrap();
        assert!(output.pixels().any(|pixel| pixel[1] < 200));
        assert!(output.pixels().all(|pixel| pixel[3] == 255));
    }

    /// One-pixel black/white checkerboard: any surviving original pixel is pure
    /// black or white, while a mosaic or blur of it is mid-grey.
    fn checkerboard(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, y| {
            let value = if (x + y) % 2 == 0 { 0 } else { 255 };
            image::Rgba([value, value, value, 255])
        })
    }

    fn effect(tool: Tool, start: Pos2, end: Pos2, strength: f32) -> Annotation {
        let mut item = Annotation::new(
            tool,
            start,
            Style {
                color: Color32::RED,
                width: 3.0,
                font_size: 32.0,
                filled: false,
                strength,
            },
        );
        item.end = end;
        item
    }

    #[test]
    fn effects_replace_every_original_pixel_inside_and_leave_outside_untouched() {
        let font = load_font();
        let image = checkerboard(120, 90);
        let full = CropRect {
            x: 0,
            y: 0,
            width: 120,
            height: 90,
        };
        for tool in [Tool::Mosaic, Tool::Blur] {
            let item = effect(tool, Pos2::new(20.0, 10.0), Pos2::new(84.0, 58.0), 8.0);
            let output = compose(&image, std::slice::from_ref(&item), &font, full).unwrap();
            for (x, y, pixel) in output.enumerate_pixels() {
                let inside = (20..84).contains(&x) && (10..58).contains(&y);
                if inside {
                    assert!(
                        pixel[0].abs_diff(128) <= 24 && pixel[3] == 255,
                        "{tool:?} kept original detail at ({x}, {y}): {pixel:?}"
                    );
                } else {
                    assert_eq!(pixel, image.get_pixel(x, y), "{tool:?} at ({x}, {y})");
                }
            }
            // The original capture is never modified.
            assert_eq!(image.get_pixel(30, 20).0, [0, 0, 0, 255]);
        }
    }

    #[test]
    fn mosaic_cells_are_uniform_anchored_to_the_rectangle_and_crop_exactly() {
        let font = load_font();
        let image = RgbaImage::from_fn(160, 120, |x, y| {
            image::Rgba([(x * 3) as u8, (y * 2) as u8, ((x + y) % 256) as u8, 255])
        });
        let item = effect(
            Tool::Mosaic,
            Pos2::new(13.0, 7.0),
            Pos2::new(93.0, 71.0),
            16.0,
        );
        let full = CropRect {
            x: 0,
            y: 0,
            width: 160,
            height: 120,
        };
        let output = compose(&image, std::slice::from_ref(&item), &font, full).unwrap();
        // Each 16 px cell, measured from the rectangle's corner, is one colour.
        for (cx, cy) in [(13, 7), (29, 23), (77, 55)] {
            let colour = output.get_pixel(cx, cy);
            for y in cy..cy + 16 {
                for x in cx..cx + 16 {
                    assert_eq!(output.get_pixel(x, y), colour, "cell at ({cx}, {cy})");
                }
            }
        }
        assert_ne!(output.get_pixel(13, 7), output.get_pixel(29, 7));
        // A cropped export is exactly the crop of the full export.
        for tool in [Tool::Mosaic, Tool::Blur] {
            let item = effect(tool, item.start, item.end, 16.0);
            let full_output = compose(&image, std::slice::from_ref(&item), &font, full).unwrap();
            let view = CropRect {
                x: 30,
                y: 20,
                width: 90,
                height: 70,
            };
            let cropped = compose(&image, std::slice::from_ref(&item), &font, view).unwrap();
            let expected = image::imageops::crop_imm(&*full_output, 30, 20, 90, 70).to_image();
            assert_eq!(*cropped, expected, "{tool:?}");
        }
    }

    #[test]
    fn effects_paint_beneath_annotations_drawn_before_them_and_preview_matches_scale() {
        let font = load_font();
        let image = checkerboard(200, 100);
        let mut line = Annotation::new(
            Tool::Line,
            Pos2::new(10.0, 50.0),
            Style {
                color: Color32::from_rgb(30, 120, 230),
                width: 8.0,
                font_size: 32.0,
                filled: false,
                strength: 12.0,
            },
        );
        line.end = Pos2::new(190.0, 50.0);
        // The line comes first in the list but must stay visible over the blur.
        let items = [
            line,
            effect(
                Tool::Blur,
                Pos2::new(40.0, 20.0),
                Pos2::new(160.0, 80.0),
                10.0,
            ),
        ];
        let view = CropRect {
            x: 0,
            y: 0,
            width: 200,
            height: 100,
        };
        let output = compose(&image, &items, &font, view).unwrap();
        assert_eq!(output.get_pixel(100, 50).0, [30, 120, 230, 255]);
        assert!(output.get_pixel(100, 30)[0].abs_diff(128) <= 24);
        // A half-size preview is opaque grey inside, transparent outside.
        let preview = overlay_premultiplied(&items[1..], &font, &image, view, 100, 50).unwrap();
        let pixel = |x: u32, y: u32| {
            let offset = ((y * 100 + x) * 4) as usize;
            &preview.data()[offset..offset + 4]
        };
        assert_eq!(pixel(50, 15)[3], 255);
        assert!(pixel(50, 15)[0].abs_diff(128) <= 24);
        assert_eq!(pixel(5, 5)[3], 0);
    }
}
