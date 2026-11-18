//! PNG files and the clipboard. Both run on background threads, never the UI.
use std::{
    borrow::Cow,
    fs::{self, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    thread,
};

use image::{
    ImageEncoder, RgbaImage,
    codecs::png::{CompressionType, FilterType, PngEncoder},
};

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
/// On Windows arboard encodes its own PNG for the clipboard, so running both
/// concurrently roughly halves the time spent after a capture.
/// Returns the saved path and an optional clipboard warning.
pub fn save(
    image: &RgbaImage,
    directory: &Path,
    copy: bool,
) -> Result<(PathBuf, Option<String>), String> {
    thread::scope(|scope| {
        let clipboard = copy.then(|| scope.spawn(|| copy_image(image)));
        let path = write_png(image, directory);
        let warning = clipboard.and_then(|worker| match worker.join() {
            Ok(result) => result.err(),
            Err(_) => Some("剪貼簿作業意外停止".into()),
        });
        path.map(|path| (path, warning))
    })
}

pub fn write_png(image: &RgbaImage, directory: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(directory).map_err(|error| format!("無法建立儲存資料夾：{error}"))?;
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    for suffix in 0..1000 {
        let filename = if suffix == 0 {
            format!("capture_{stamp}.png")
        } else {
            format!("capture_{stamp}_{suffix}.png")
        };
        let path = directory.join(filename);
        let file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("無法儲存 PNG：{error}")),
        };
        let mut writer = BufWriter::new(file);
        // Fast (fdeflate) compression is several times faster than the default on
        // large screenshots, for files only slightly larger. Still lossless.
        let result =
            PngEncoder::new_with_quality(&mut writer, CompressionType::Fast, FilterType::Adaptive)
                .write_image(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .and_then(|()| writer.flush().map_err(image::ImageError::IoError));
        drop(writer);
        if let Err(error) = result {
            let _ = fs::remove_file(&path);
            return Err(format!("PNG 編碼失敗：{error}"));
        }
        return Ok(path);
    }
    Err("同一時間的截圖檔名過多，請稍後再試".into())
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
}
