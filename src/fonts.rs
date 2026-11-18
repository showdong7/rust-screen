//! System fonts are read from disk once and shared, for the life of the process, by
//! the egui UI and the annotation renderer. CJK font collections exceed 20 MB.
use std::{path::PathBuf, sync::OnceLock};

/// Traditional Chinese first, then Simplified Chinese and Japanese fallbacks.
const CJK: [&str; 3] = ["msjh.ttc", "msyh.ttc", "YuGothM.ttc"];

fn directory() -> PathBuf {
    std::env::var_os("WINDIR")
        .map_or_else(|| PathBuf::from("C:\\Windows"), PathBuf::from)
        .join("Fonts")
}

/// Reads a font file and keeps its bytes alive until the process exits.
pub fn read(name: &str) -> Option<&'static [u8]> {
    let bytes = std::fs::read(directory().join(name)).ok()?;
    Some(Box::leak(bytes.into_boxed_slice()))
}

/// The first installed CJK font, loaded at most once.
pub fn cjk() -> Option<&'static [u8]> {
    static FONT: OnceLock<Option<&'static [u8]>> = OnceLock::new();
    *FONT.get_or_init(|| CJK.into_iter().find_map(read))
}
