//! Brief on-screen frame around whatever an MCP client just captured, so the user
//! notices screenshots even after "always allow" in the client.
//!
//! The frame is a click-through, non-activating, topmost window shaped to a thin
//! border. It is excluded from screen capture (`WDA_EXCLUDEFROMCAPTURE`), so it
//! never appears in a later or concurrent screenshot. Set
//! `RUST_SCREEN_MCP_INDICATOR=0` to turn it off.
use std::{
    mem::zeroed,
    ptr::{null, null_mut},
    sync::OnceLock,
    thread,
};

use rust_screen::{geometry::DesktopRect, platform};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{
        BeginPaint, CombineRgn, CreateRectRgn, CreateSolidBrush, DeleteObject, EndPaint, FillRect,
        PAINTSTRUCT, RGN_DIFF, SetWindowRgn,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
        GetMessageW, HTTRANSPARENT, IDC_ARROW, LWA_ALPHA, LoadCursorW, MA_NOACTIVATE, MSG,
        PostQuitMessage, SW_SHOWNOACTIVATE, SetLayeredWindowAttributes, SetTimer,
        SetWindowDisplayAffinity, ShowWindow, TranslateMessage, WDA_EXCLUDEFROMCAPTURE, WM_DESTROY,
        WM_ERASEBKGND, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_PAINT, WM_TIMER, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    },
};

/// How long the frame stays on screen.
const DURATION_MS: u32 = 700;
/// Border width in physical pixels.
const THICKNESS: i32 = 6;
/// Rust Screen orange, as COLORREF (0x00BBGGRR).
const COLOR: u32 = 0x0024_7AFF;
const OPACITY: u8 = 230;

pub fn enabled() -> bool {
    !matches!(
        std::env::var("RUST_SCREEN_MCP_INDICATOR")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "off" | "false" | "no"
    )
}

/// Shows the frame around `bounds` on its own thread and returns immediately.
pub fn flash(bounds: DesktopRect) {
    if !enabled() {
        return;
    }
    thread::spawn(move || {
        if let Err(error) = show(bounds) {
            eprintln!("capture indicator not shown: {error}");
        }
    });
}

/// Frame-shaped region in window coordinates; small areas are filled entirely.
fn frame_region(width: i32, height: i32) -> Option<(RECT, Option<RECT>)> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let outer = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    let inner = (width > THICKNESS * 3 && height > THICKNESS * 3).then_some(RECT {
        left: THICKNESS,
        top: THICKNESS,
        right: width - THICKNESS,
        bottom: height - THICKNESS,
    });
    Some((outer, inner))
}

fn show(bounds: DesktopRect) -> Result<(), String> {
    static CLASS: OnceLock<Result<u16, String>> = OnceLock::new();
    platform::enable_dpi_awareness();
    let width = i32::try_from(bounds.width).map_err(|_| "area too wide".to_owned())?;
    let height = i32::try_from(bounds.height).map_err(|_| "area too tall".to_owned())?;
    let (outer, inner) = frame_region(width, height).ok_or("empty area")?;
    // SAFETY: the class lives for the process; the window and its message loop stay on
    // this thread, and the region handed to SetWindowRgn is owned by the system after.
    unsafe {
        let atom = CLASS
            .get_or_init(|| {
                platform::register_class(
                    "RustScreenMcpCaptureIndicator",
                    Some(window_proc),
                    LoadCursorW(null_mut(), IDC_ARROW),
                )
                .map_err(|error| error.to_string())
            })
            .as_ref()
            .map_err(Clone::clone)?;
        let title = platform::wide("Rust Screen MCP capture");
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            *atom as usize as *const u16,
            title.as_ptr(),
            WS_POPUP,
            bounds.x,
            bounds.y,
            width,
            height,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        if hwnd.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // Exclude before the window is ever visible, so no capture can include it.
        if SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) == 0 {
            let error = std::io::Error::last_os_error();
            DestroyWindow(hwnd);
            return Err(format!("cannot exclude it from capture: {error}"));
        }
        SetLayeredWindowAttributes(hwnd, 0, OPACITY, LWA_ALPHA);
        let region = CreateRectRgn(outer.left, outer.top, outer.right, outer.bottom);
        if let Some(inner) = inner {
            let hole = CreateRectRgn(inner.left, inner.top, inner.right, inner.bottom);
            CombineRgn(region, region, hole, RGN_DIFF);
            DeleteObject(hole);
        }
        if SetWindowRgn(hwnd, region, 0) == 0 {
            DeleteObject(region);
        }
        SetTimer(hwnd, 1, DURATION_MS, None);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let mut message: MSG = zeroed();
        while GetMessageW(&raw mut message, null_mut(), 0, 0) > 0 {
            TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: called by Windows on this window's own thread with valid arguments.
    unsafe {
        match message {
            WM_PAINT => {
                let mut paint: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(hwnd, &raw mut paint);
                let mut rect: RECT = zeroed();
                GetClientRect(hwnd, &raw mut rect);
                let brush = CreateSolidBrush(COLOR);
                FillRect(dc, &raw const rect, brush);
                DeleteObject(brush);
                EndPaint(hwnd, &raw const paint);
                0
            }
            WM_ERASEBKGND => 1,
            WM_NCHITTEST => HTTRANSPARENT as LRESULT,
            WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
            WM_TIMER => {
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_has_a_hole_unless_the_area_is_tiny() {
        let (outer, inner) = frame_region(200, 100).unwrap();
        assert_eq!((outer.right, outer.bottom), (200, 100));
        let inner = inner.unwrap();
        assert_eq!((inner.left, inner.top), (THICKNESS, THICKNESS));
        assert_eq!(
            (inner.right, inner.bottom),
            (200 - THICKNESS, 100 - THICKNESS)
        );
        assert!(frame_region(12, 400).unwrap().1.is_none());
        assert!(frame_region(0, 10).is_none());
    }
}
