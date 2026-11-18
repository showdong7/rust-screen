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
    pub delay_ms: u64,
    pub annotation_color: [u8; 4],
    pub annotation_width: f32,
    pub annotation_font_size: f32,
    pub annotation_filled: bool,
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
            delay_ms: 300,
            annotation_color: [235, 55, 55, 255],
            annotation_width: 3.0,
            annotation_font_size: 32.0,
            annotation_filled: false,
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
            ..Default::default()
        };
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert!(restored.annotation_filled);
    }
}
