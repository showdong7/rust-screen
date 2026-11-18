#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod annotation;
mod annotation_render;
mod annotation_resize;
mod annotation_ui;
mod app;
mod canvas;
mod fonts;
mod hotkey;
mod single_instance;
mod theme;
mod tray;

// The capture core lives in the library so `rust-screen-mcp` can share it. Importing
// the modules here keeps the GUI's existing `crate::platform::…` paths working.
use rust_screen::{config, export, geometry, platform, selector};

fn main() -> eframe::Result {
    let instance = match single_instance::SingleInstance::acquire() {
        Ok(Some(instance)) => instance,
        Ok(None) => return Ok(()),
        Err(error) => {
            let message = platform::wide(&format!("無法啟動 Rust Screen：{error}"));
            let title = platform::wide("Rust Screen");
            // SAFETY: both strings are NUL-terminated and remain alive during the dialog.
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                    std::ptr::null_mut(),
                    message.as_ptr(),
                    title.as_ptr(),
                    windows_sys::Win32::UI::WindowsAndMessaging::MB_OK
                        | windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
                );
            }
            return Err(eframe::Error::AppCreation(Box::new(error)));
        }
    };
    platform::enable_dpi_awareness();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 780.0])
            .with_min_inner_size([760.0, 600.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/rust-screen.png"))
                    .expect("the bundled application icon must be a valid PNG"),
            )
            .with_resizable(true),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "Rust Screen · 螢幕截圖",
        options,
        Box::new(|cc| {
            Ok(Box::new(app::ScreenApp::new(
                cc,
                instance.activation.clone(),
            )))
        }),
    )
}
