use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureMode {
    #[default]
    Region,
    Desktop,
    Window,
}

impl CaptureMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Region => "自訂範圍",
            Self::Desktop => "全螢幕",
            Self::Window => "作用中視窗",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeMode,
    pub shortcut: String,
    pub mode: CaptureMode,
    pub crop_width: u32,
    pub crop_height: u32,
    pub fixed_size: bool,
    pub output_dir: PathBuf,
    pub auto_copy: bool,
    /// Open each capture for editing first. Nothing is written to disk or the
    /// clipboard until the user saves, so sensitive areas can be masked first.
    pub edit_before_save: bool,
    pub delay_ms: u64,
    pub annotation_color: [u8; 4],
    pub annotation_width: f32,
    pub annotation_font_size: f32,
    pub annotation_filled: bool,
    /// Mosaic cell size and blur radius, in original image pixels.
    pub annotation_strength: f32,
    pub watermark: Watermark,
}

/// A repeated, rotated text watermark stamped on every saved or copied image.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Watermark {
    pub enabled: bool,
    pub text: String,
    /// 0.05 (faint) to 1.0 (solid).
    pub opacity: f32,
    /// Degrees; negative values rise from left to right.
    pub angle: f32,
    /// Text height in output image pixels.
    pub font_size: f32,
    pub color: [u8; 3],
}

impl Watermark {
    pub const OPACITY_RANGE: std::ops::RangeInclusive<f32> = 0.05..=1.0;
    pub const ANGLE_RANGE: std::ops::RangeInclusive<f32> = -90.0..=90.0;
    pub const FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 12.0..=200.0;
    pub const TEXT_LIMIT: usize = 60;

    /// The settings with every value inside its supported range, as rendered.
    /// A hand-edited settings file can contain anything.
    pub fn sanitized(&self) -> Self {
        let clamp = |value: f32, range: &std::ops::RangeInclusive<f32>, default: f32| {
            if value.is_finite() {
                value.clamp(*range.start(), *range.end())
            } else {
                default
            }
        };
        let defaults = Self::default();
        Self {
            enabled: self.enabled,
            text: self
                .text
                .chars()
                .filter(|ch| !ch.is_control())
                .take(Self::TEXT_LIMIT)
                .collect::<String>()
                .trim()
                .to_string(),
            opacity: clamp(self.opacity, &Self::OPACITY_RANGE, defaults.opacity),
            angle: clamp(self.angle, &Self::ANGLE_RANGE, defaults.angle),
            font_size: clamp(self.font_size, &Self::FONT_SIZE_RANGE, defaults.font_size),
            color: self.color,
        }
    }

    /// Whether saving or copying should stamp the watermark.
    pub fn active(&self) -> bool {
        self.enabled && !self.sanitized().text.is_empty()
    }
}

impl Default for Watermark {
    fn default() -> Self {
        Self {
            enabled: false,
            text: "Rust Screen".into(),
            opacity: 0.25,
            angle: -30.0,
            font_size: 32.0,
            color: [128, 128, 128],
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        let output_dir = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Pictures")
            .join("RustScreen");
        Self {
            theme: ThemeMode::System,
            shortcut: "Ctrl+Shift+S".into(),
            mode: CaptureMode::Region,
            crop_width: 800,
            crop_height: 600,
            fixed_size: false,
            output_dir,
            auto_copy: true,
            edit_before_save: false,
            delay_ms: 300,
            annotation_color: [235, 55, 55, 255],
            annotation_width: 3.0,
            annotation_font_size: 32.0,
            annotation_filled: false,
            annotation_strength: 12.0,
            watermark: Watermark::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    System,
    Dark,
    Light,
}

pub fn settings_path() -> PathBuf {
    if let Some(directory) = std::env::var_os("RUST_SCREEN_CONFIG_DIR") {
        return PathBuf::from(directory).join("settings.json");
    }
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("RustScreen")
        .join("settings.json")
}

pub fn load() -> Result<Settings, String> {
    let path = settings_path();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default());
        }
        Err(error) => return Err(format!("讀取設定失敗：{error}")),
    };
    let mut settings: Settings = serde_json::from_str(&text)
        .map_err(|error| format!("設定格式錯誤（{}）：{error}", path.display()))?;
    settings.crop_width = settings.crop_width.clamp(1, 32768);
    settings.crop_height = settings.crop_height.clamp(1, 32768);
    settings.delay_ms = settings.delay_ms.clamp(200, 10000);
    settings.annotation_width = if settings.annotation_width.is_finite() {
        settings.annotation_width.clamp(1.0, 40.0)
    } else {
        3.0
    };
    settings.annotation_font_size = if settings.annotation_font_size.is_finite() {
        settings.annotation_font_size.clamp(8.0, 200.0)
    } else {
        32.0
    };
    Ok(settings)
}

pub fn save(settings: &Settings) -> Result<(), String> {
    let path = settings_path();
    fs::create_dir_all(path.parent().expect("settings directory"))
        .map_err(|error| format!("建立設定目錄失敗：{error}"))?;
    let bytes = serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temp, bytes).map_err(|error| format!("寫入設定失敗：{error}"))?;
    fs::rename(&temp, &path).map_err(|error| format!("儲存設定失敗：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_settings_keep_preferences_and_default_to_system_theme() {
        let settings: Settings =
            serde_json::from_str(r#"{"shortcut":"Alt+F9","crop_width":1234}"#).unwrap();
        assert_eq!(settings.theme, ThemeMode::System);
        assert_eq!(settings.shortcut, "Alt+F9");
        assert_eq!(settings.crop_width, 1234);
        assert!(!settings.annotation_filled);
        // Older settings files keep the established save-on-release behaviour.
        assert!(!settings.edit_before_save);
        assert_eq!(settings.annotation_strength, 12.0);
        assert_eq!(settings.watermark, Watermark::default());
        assert!(!settings.watermark.active());
    }

    #[test]
    fn watermark_survives_round_trip_and_out_of_range_values_are_clamped() {
        let settings = Settings {
            watermark: Watermark {
                enabled: true,
                text: "機密 Confidential".into(),
                opacity: 0.6,
                angle: 45.0,
                font_size: 48.0,
                color: [200, 30, 30],
            },
            ..Default::default()
        };
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored.watermark, settings.watermark);
        assert!(restored.watermark.active());
        // A partial, hand-edited watermark keeps the remaining defaults.
        let partial: Settings =
            serde_json::from_str(r#"{"watermark":{"enabled":true,"opacity":7,"angle":-400,"font_size":1e9,"text":" a\nb "}}"#)
                .unwrap();
        let clean = partial.watermark.sanitized();
        assert_eq!(clean.opacity, 1.0);
        assert_eq!(clean.angle, -90.0);
        assert_eq!(clean.font_size, 200.0);
        assert_eq!(clean.text, "ab");
        assert_eq!(clean.color, Watermark::default().color);
        let blank = Watermark {
            enabled: true,
            text: "   ".into(),
            ..Default::default()
        };
        assert!(!blank.active());
        let nan = Watermark {
            opacity: f32::NAN,
            ..Default::default()
        };
        assert_eq!(nan.sanitized().opacity, Watermark::default().opacity);
    }

    #[test]
    fn theme_choices_survive_settings_round_trip() {
        for theme in [ThemeMode::System, ThemeMode::Dark, ThemeMode::Light] {
            let settings = Settings {
                theme,
                ..Default::default()
            };
            let restored: Settings =
                serde_json::from_slice(&serde_json::to_vec(&settings).unwrap()).unwrap();
            assert_eq!(restored.theme, theme);
        }
    }

    #[test]
    fn solid_fill_preference_survives_settings_round_trip() {
        let settings = Settings {
            annotation_filled: true,
            edit_before_save: true,
            annotation_strength: 20.0,
            ..Default::default()
        };
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert!(restored.annotation_filled);
        assert!(restored.edit_before_save);
        assert_eq!(restored.annotation_strength, 20.0);
    }
}
