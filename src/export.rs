//! Output scaling, PNG/JPEG files and the clipboard. All of it runs on background
//! threads, never the UI.
use std::{
    borrow::Cow,
    fs::{self, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    thread,
};

use image::{
    ImageEncoder, RgbImage, RgbaImage,
    buffer::ConvertBuffer,
    codecs::{
        jpeg::JpegEncoder,
        png::{CompressionType, FilterType, PngEncoder},
    },
};

use crate::{
    config::{ExportOptions, ImageFormat, OutputSize},
    geometry,
};

/// DPI of a monitor at 100 % display scaling.
pub const BASE_DPI: u32 = 96;

/// The size a `width` × `height` image is saved and copied at. `dpi` is the effective
/// DPI of the monitor it was captured on (`OutputSize::ScreenScale`). Never upscales.
pub fn output_size(options: &ExportOptions, width: u32, height: u32, dpi: u32) -> (u32, u32) {
    let options = options.sanitized();
    let scale = |factor: f64| {
        let fit = |value: u32| ((f64::from(value) * factor).round() as u32).clamp(1, value.max(1));
        (fit(width), fit(height))
    };
    match options.size {
        OutputSize::Original => (width, height),
        OutputSize::ScreenScale if dpi > BASE_DPI => scale(f64::from(BASE_DPI) / f64::from(dpi)),
        OutputSize::ScreenScale => (width, height),
        OutputSize::Percent => scale(f64::from(options.percent) / 100.0),
        OutputSize::LongEdge => geometry::fit_longest_edge(width, height, options.long_edge),
    }
}

/// Shrinks a composed image to its output size; passes it through (still borrowed)
/// when the size does not change. Lanczos keeps text edges sharper than bilinear.
pub fn resize<'a>(
    image: Cow<'a, RgbaImage>,
    options: &ExportOptions,
    dpi: u32,
) -> Cow<'a, RgbaImage> {
    let (width, height) = output_size(options, image.width(), image.height(), dpi);
    if (width, height) == image.dimensions() {
        return image;
    }
    Cow::Owned(image::imageops::resize(
        image.as_ref(),
        width,
        height,
        image::imageops::FilterType::Lanczos3,
    ))
}

pub fn copy_image(image: &RgbaImage) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| {
            clipboard.set_image(arboard::ImageData {
                width: image.width() as usize,
                height: image.height() as usize,
                bytes: Cow::Borrowed(image.as_raw()),
            })
        })
        .map_err(|error| format!("無法複製到剪貼簿：{error}"))
}

/// Saves a PNG and, when requested, copies to the clipboard at the same time.
/// Returns the saved path and an optional clipboard warning.
pub fn save(
    image: &RgbaImage,
    directory: &Path,
    copy: bool,
) -> Result<(PathBuf, Option<String>), String> {
    save_as(image, directory, copy, ImageFormat::Png, 0)
}

/// Saves in `format` and, when requested, copies to the clipboard at the same time.
/// On Windows arboard encodes its own PNG for the clipboard, so running both
/// concurrently roughly halves the time spent after a capture.
/// `quality` (1–100) applies to JPEG only.
/// Returns the saved path and an optional clipboard warning.
pub fn save_as(
    image: &RgbaImage,
    directory: &Path,
    copy: bool,
    format: ImageFormat,
    quality: u8,
) -> Result<(PathBuf, Option<String>), String> {
    thread::scope(|scope| {
        let clipboard = copy.then(|| scope.spawn(|| copy_image(image)));
        let path = write_image(image, directory, format, quality);
        let warning = clipboard.and_then(|worker| match worker.join() {
            Ok(result) => result.err(),
            Err(_) => Some("剪貼簿作業意外停止".into()),
        });
        path.map(|path| (path, warning))
    })
}

pub fn write_png(image: &RgbaImage, directory: &Path) -> Result<PathBuf, String> {
    write_image(image, directory, ImageFormat::Png, 0)
}

/// Writes `capture_<timestamp>.png` / `.jpg`, never overwriting an existing file.
pub fn write_image(
    image: &RgbaImage,
    directory: &Path,
    format: ImageFormat,
    quality: u8,
) -> Result<PathBuf, String> {
    fs::create_dir_all(directory).map_err(|error| format!("無法建立儲存資料夾：{error}"))?;
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let (label, extension) = (format.label(), format.extension());
    for suffix in 0..1000 {
        let filename = if suffix == 0 {
            format!("capture_{stamp}.{extension}")
        } else {
            format!("capture_{stamp}_{suffix}.{extension}")
        };
        let path = directory.join(filename);
        let file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("無法儲存 {label}：{error}")),
        };
        let mut writer = BufWriter::new(file);
        let result = encode(image, &mut writer, format, quality)
            .and_then(|()| writer.flush().map_err(image::ImageError::IoError));
        drop(writer);
        if let Err(error) = result {
            let _ = fs::remove_file(&path);
            return Err(format!("{label} 編碼失敗：{error}"));
        }
        return Ok(path);
    }
    Err("同一時間的截圖檔名過多，請稍後再試".into())
}

fn encode(
    image: &RgbaImage,
    writer: &mut impl Write,
    format: ImageFormat,
    quality: u8,
) -> image::ImageResult<()> {
    match format {
        // Fast (fdeflate) compression is several times faster than the default on
        // large screenshots, for files only slightly larger. Still lossless.
        ImageFormat::Png => {
            PngEncoder::new_with_quality(writer, CompressionType::Fast, FilterType::Adaptive)
                .write_image(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    image::ExtendedColorType::Rgba8,
                )
        }
        // JPEG has no alpha channel; composed screenshots are opaque anyway.
        ImageFormat::Jpeg => {
            let rgb: RgbImage = image.convert();
            JpegEncoder::new_with_quality(writer, quality.clamp(1, 100)).write_image(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_export_round_trips_pixels_and_never_overwrites() {
        let directory = std::env::temp_dir().join(format!(
            "rust-screen-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let image = RgbaImage::from_fn(13, 7, |x, y| image::Rgba([x as u8, y as u8, 42, 255]));
        let first = write_png(&image, &directory).unwrap();
        let (second, warning) = save(&image, &directory, false).unwrap();
        assert!(warning.is_none());
        assert_ne!(first, second);
        assert_eq!(image::open(&first).unwrap().to_rgba8(), image);
        assert_eq!(image::open(&second).unwrap().to_rgba8(), image);
        fs::remove_file(first).unwrap();
        fs::remove_file(second).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn jpeg_export_uses_jpg_extension_and_decodes_to_the_same_size() {
        let directory = std::env::temp_dir().join(format!(
            "rust-screen-jpeg-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let image = RgbaImage::from_fn(40, 30, |x, y| {
            image::Rgba([(x * 6) as u8, (y * 8) as u8, 120, 255])
        });
        let (path, warning) = save_as(&image, &directory, false, ImageFormat::Jpeg, 85).unwrap();
        assert!(warning.is_none());
        assert_eq!(path.extension().unwrap(), "jpg");
        let decoded = image::open(&path).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (40, 30));
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    fn options(size: OutputSize) -> ExportOptions {
        ExportOptions {
            size,
            percent: 50,
            long_edge: 1280,
            ..Default::default()
        }
    }

    #[test]
    fn output_size_follows_the_chosen_mode_and_never_upscales() {
        let original = options(OutputSize::Original);
        assert_eq!(output_size(&original, 1920, 1080, 144), (1920, 1080));
        // 150 % display scaling: back to what the screen showed.
        let screen = options(OutputSize::ScreenScale);
        assert_eq!(output_size(&screen, 2880, 1620, 144), (1920, 1080));
        assert_eq!(output_size(&screen, 2500, 1250, 240), (1000, 500));
        // 100 % (or an unknown, lower DPI) keeps the original pixels.
        assert_eq!(output_size(&screen, 1920, 1080, 96), (1920, 1080));
        assert_eq!(output_size(&screen, 1920, 1080, 0), (1920, 1080));
        let percent = options(OutputSize::Percent);
        assert_eq!(output_size(&percent, 1234, 941, 96), (617, 471));
        assert_eq!(output_size(&percent, 1, 1, 96), (1, 1));
        let long_edge = options(OutputSize::LongEdge);
        assert_eq!(output_size(&long_edge, 3840, 2160, 96), (1280, 720));
        assert_eq!(output_size(&long_edge, 800, 600, 96), (800, 600));
        // Out-of-range values from a hand-edited file are clamped, never upscale.
        let wild = ExportOptions {
            percent: 400,
            ..options(OutputSize::Percent)
        };
        assert_eq!(output_size(&wild, 800, 600, 96), (800, 600));
    }

    #[test]
    fn resize_borrows_when_unchanged_and_shrinks_otherwise() {
        let image = RgbaImage::from_pixel(100, 60, image::Rgba([10, 20, 30, 255]));
        let same = resize(Cow::Borrowed(&image), &options(OutputSize::Original), 96);
        assert!(matches!(same, Cow::Borrowed(_)));
        let half = resize(Cow::Borrowed(&image), &options(OutputSize::Percent), 96);
        assert_eq!(half.dimensions(), (50, 30));
        // A flat colour stays flat through the filter.
        assert_eq!(half.get_pixel(25, 15), &image::Rgba([10, 20, 30, 255]));
    }
}
