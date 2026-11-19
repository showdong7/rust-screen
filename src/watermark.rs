//! Tiled, rotated text watermark, shared by export and the preview so both match.
use std::borrow::Cow;

use ab_glyph::FontArc;
use eframe::egui::{Color32, Pos2};
use image::RgbaImage;
use tiny_skia::{FilterQuality, Paint, Pattern, Pixmap, SpreadMode, Transform};

use crate::{
    annotation::{Annotation, Style, Tool},
    annotation_render,
    config::Watermark,
    geometry::CropRect,
};

/// Space between repeats, as a multiple of the font size.
const GAP: f32 = 2.0;

/// One repeat of the pattern in output-image pixels. The second row is shifted by half
/// a width (a brick layout), so rotated rows do not line up into stripes.
fn tile(watermark: &Watermark, font: &FontArc) -> Result<Option<Pixmap>, String> {
    if watermark.text.is_empty() {
        return Ok(None);
    }
    let [r, g, b] = watermark.color;
    let alpha = (watermark.opacity * 255.0).round() as u8;
    let mut item = Annotation::new(
        Tool::Text,
        Pos2::ZERO,
        Style {
            color: Color32::from_rgba_unmultiplied(r, g, b, alpha),
            width: 1.0,
            font_size: watermark.font_size,
            filled: false,
            strength: 0.0,
        },
    );
    item.text = watermark.text.clone();
    let size = annotation_render::bounds(&item, font).size();
    let gap = watermark.font_size * GAP;
    let width = (size.x + gap).ceil();
    let row = (size.y + gap).ceil();
    let margin = gap / 2.0;
    // The third copy is the part of the shifted second row that wraps around.
    let items = [
        (margin, margin),
        (margin + width / 2.0, row + margin),
        (margin - width / 2.0, row + margin),
    ]
    .map(|(x, y)| {
        let mut copy = item.clone();
        copy.start = Pos2::new(x, y);
        copy.end = copy.start;
        copy
    });
    let (width, height) = (width as u32, (row * 2.0) as u32);
    annotation_render::overlay_premultiplied(
        &items,
        font,
        &RgbaImage::new(1, 1),
        CropRect {
            x: 0,
            y: 0,
            width,
            height,
        },
        width,
        height,
    )
    .map(Some)
}

/// The watermark for a `width` × `height` target. `scale` maps output-image pixels to
/// target pixels, so a downscaled preview shows exactly what the export will contain.
/// The pattern rotates about the image centre. Returns `None` when there is no text.
pub fn layer(
    watermark: &Watermark,
    font: &FontArc,
    width: u32,
    height: u32,
    scale: f32,
) -> Result<Option<Pixmap>, String> {
    let watermark = watermark.sanitized();
    let Some(tile) = tile(&watermark, font)? else {
        return Ok(None);
    };
    let mut pixmap = Pixmap::new(width, height).ok_or("無法配置浮水印繪圖記憶體")?;
    let transform = Transform::from_scale(scale, scale).post_concat(Transform::from_rotate_at(
        watermark.angle,
        width as f32 / 2.0,
        height as f32 / 2.0,
    ));
    let paint = Paint {
        shader: Pattern::new(
            tile.as_ref(),
            SpreadMode::Repeat,
            FilterQuality::Bilinear,
            1.0,
            transform,
        ),
        anti_alias: false,
        ..Paint::default()
    };
    let rect = tiny_skia::Rect::from_xywh(0.0, 0.0, width as f32, height as f32)
        .ok_or("浮水印尺寸錯誤")?;
    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    Ok(Some(pixmap))
}

/// Stamps the watermark on a composed image when it is enabled; otherwise passes the
/// image through untouched (still borrowed when nothing else changed it).
pub fn stamp<'a>(
    image: Cow<'a, RgbaImage>,
    watermark: &Watermark,
    font: &FontArc,
) -> Result<Cow<'a, RgbaImage>, String> {
    if !watermark.active() {
        return Ok(image);
    }
    let mut image = image.into_owned();
    if let Some(layer) = layer(watermark, font, image.width(), image.height(), 1.0)? {
        annotation_render::blend(&mut image, &layer);
    }
    Ok(Cow::Owned(image))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> Watermark {
        Watermark {
            enabled: true,
            text: "浮水印 Test".into(),
            opacity: 0.5,
            angle: -30.0,
            font_size: 24.0,
            color: [0, 0, 0],
        }
    }

    fn white(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_pixel(width, height, image::Rgba([255; 4]))
    }

    /// Share of pixels the watermark darkened, per quadrant.
    fn coverage(image: &RgbaImage) -> [f32; 4] {
        let (w, h) = (image.width(), image.height());
        let mut marked = [0_u32; 4];
        for (x, y, pixel) in image.enumerate_pixels() {
            if pixel[0] < 250 {
                marked[usize::from(x >= w / 2) + 2 * usize::from(y >= h / 2)] += 1;
            }
        }
        marked.map(|count| count as f32 / (w * h / 4) as f32)
    }

    #[test]
    fn disabled_or_blank_watermark_leaves_the_image_borrowed() {
        let font = annotation_render::load_font();
        let image = white(64, 48);
        let off = Watermark {
            enabled: false,
            ..enabled()
        };
        assert!(matches!(
            stamp(Cow::Borrowed(&image), &off, &font).unwrap(),
            Cow::Borrowed(_)
        ));
        let blank = Watermark {
            text: " ".into(),
            ..enabled()
        };
        assert!(matches!(
            stamp(Cow::Borrowed(&image), &blank, &font).unwrap(),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn watermark_repeats_across_the_whole_image_and_keeps_it_opaque() {
        let font = annotation_render::load_font();
        let image = white(640, 480);
        let output = stamp(Cow::Borrowed(&image), &enabled(), &font).unwrap();
        assert_eq!(output.dimensions(), (640, 480));
        for share in coverage(&output) {
            assert!(share > 0.01 && share < 0.5, "uneven coverage {share}");
        }
        assert!(output.pixels().all(|pixel| pixel[3] == 255));
        // Half opacity of black on white is never darker than mid-grey.
        assert!(output.pixels().all(|pixel| pixel[0] >= 120));
        assert!(output.pixels().any(|pixel| pixel[0] < 160));
    }

    #[test]
    fn opacity_and_angle_change_the_result() {
        let font = annotation_render::load_font();
        let image = white(320, 240);
        let darkest = |watermark: &Watermark| {
            stamp(Cow::Borrowed(&image), watermark, &font)
                .unwrap()
                .pixels()
                .map(|pixel| pixel[0])
                .min()
                .unwrap()
        };
        let faint = Watermark {
            opacity: 0.1,
            ..enabled()
        };
        let strong = Watermark {
            opacity: 0.9,
            ..enabled()
        };
        assert!(darkest(&faint) > 220, "{}", darkest(&faint));
        assert!(darkest(&strong) < 60, "{}", darkest(&strong));
        let level = Watermark {
            angle: 0.0,
            ..enabled()
        };
        let tilted = Watermark {
            angle: 45.0,
            ..enabled()
        };
        assert_ne!(
            stamp(Cow::Borrowed(&image), &level, &font).unwrap(),
            stamp(Cow::Borrowed(&image), &tilted, &font).unwrap()
        );
    }

    #[test]
    fn half_size_preview_covers_the_same_share_as_the_export() {
        let font = annotation_render::load_font();
        let watermark = enabled();
        let share = |layer: Pixmap| {
            let pixels = layer.data().as_chunks::<4>().0;
            pixels.iter().filter(|pixel| pixel[3] > 8).count() as f32 / pixels.len() as f32
        };
        let full = share(layer(&watermark, &font, 800, 600, 1.0).unwrap().unwrap());
        let half = share(layer(&watermark, &font, 400, 300, 0.5).unwrap().unwrap());
        assert!(
            (full - half).abs() < full * 0.35,
            "export {full} vs preview {half}"
        );
    }
}
