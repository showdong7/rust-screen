#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod annotation;
mod annotation_render;
mod annotation_resize;
mod annotation_ui;
mod app;
mod canvas;
mod fonts;
mod hotkey;
mod pin;
mod single_instance;
mod theme;
mod tray;
mod watermark;

// The capture core lives in the library so `rust-screen-mcp` can share it. Importing
// the modules here keeps the GUI's existing `crate::platform::…` paths working.
use rust_screen::{config, export, geometry, platform, selector};

fn main() -> eframe::Result {
    let instance = match single_instance::SingleInstance::acquire() {
        Ok(Some(instance)) => instance,
        Ok(None) => return Ok(()),
        Err(error) => {
            show_startup_error(&format!("無法啟動 Rust Screen：{error}"));
            return Err(eframe::Error::AppCreation(Box::new(error)));
        }
    };
    platform::enable_dpi_awareness();
    let run = |renderer| {
        let activation = instance.activation.clone();
        eframe::run_native(
            "Rust Screen · 螢幕截圖",
            native_options(renderer),
            Box::new(move |cc| Ok(Box::new(app::ScreenApp::new(cc, activation)))),
        )
    };
    // OpenGL (glow) is the primary renderer. Machines without OpenGL 2.0 (server
    // graphics such as ASPEED BMC chips, virtual display adapters, some remote
    // sessions) fail before the app is created, so retry with wgpu on Direct3D 12,
    // which falls back to Windows' built-in WARP software rasteriser. eframe keeps
    // its event loop between runs, so a second `run_native` is allowed.
    let result = match run(eframe::Renderer::Glow) {
        Err(
            glow_error @ (eframe::Error::OpenGL(_)
            | eframe::Error::Glutin(_)
            | eframe::Error::NoGlutinConfigs(..)),
        ) => run(eframe::Renderer::Wgpu).map_err(|wgpu_error| {
            eframe::Error::AppCreation(
                format!("OpenGL：{glow_error}；Direct3D 12 (wgpu)：{wgpu_error}").into(),
            )
        }),
        other => other,
    };
    if let Err(error) = &result {
        show_startup_error(&format!("無法啟動 Rust Screen 的繪圖介面。\n\n{error}"));
    }
    result
}

fn native_options(renderer: eframe::Renderer) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 780.0])
            .with_min_inner_size([760.0, 600.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/rust-screen.png"))
                    .expect("the bundled application icon must be a valid PNG"),
            )
            .with_resizable(true),
        centered: true,
        renderer,
        ..Default::default()
    }
}

/// The release build has no console, so startup failures must be shown in a dialog.
fn show_startup_error(message: &str) {
    let message = platform::wide(message);
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
}
