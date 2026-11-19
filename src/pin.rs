//! Pinned screenshots: borderless, always-on-top windows that show an image until the
//! user closes them. Each pin is a native window with its own message loop on its own
//! thread, so pins keep working while the main window is hidden in the tray.
//!
//! Drag to move, wheel to zoom about the cursor, Ctrl+wheel for opacity, double-click
//! or Esc to close, Ctrl+C to copy, right-click for a menu.
use std::{
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::{Mutex, OnceLock, PoisonError, mpsc},
    thread,
};

use image::RgbaImage;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{
        BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW,
        CreateSolidBrush, DT_CALCRECT, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DeleteDC,
        DeleteObject, DrawTextW, EndPaint, FW_SEMIBOLD, FillRect, GetMonitorInfoW, HALFTONE, HDC,
        InvalidateRect, LOGFONTW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
        PAINTSTRUCT, SRCCOPY, SelectObject, SetBkMode, SetBrushOrgEx, SetStretchBltMode,
        SetTextColor, StretchBlt, TRANSPARENT, UpdateWindow,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::GetDpiForWindow,
        Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_ESCAPE},
        WindowsAndMessaging::*,
    },
};

use crate::{
    export,
    platform::{self, Bitmap, wide},
};

/// Frame around the image, in physical pixels. The frame sits outside the image, so a
/// pin opened in place lines up exactly with the pixels it was captured from.
const BORDER: i32 = 2;
/// The app accent, rgb(78, 215, 185), as COLORREF 0x00BBGGRR.
const BORDER_COLOR: u32 = 0x00B9_D74E;
/// The image never shrinks below this many pixels on its short side.
const MIN_SIDE: f32 = 24.0;
const MAX_ZOOM: f32 = 10.0;
/// Zoom factor per wheel notch.
const ZOOM_STEP: f32 = 1.1;
const MIN_OPACITY: f32 = 0.2;
/// How long the zoom / opacity label stays visible.
const LABEL_MS: u32 = 1200;
const LABEL_TIMER: usize = 1;

/// Ctrl held during a mouse message (MK_CONTROL in WinUser.h).
const MK_CONTROL: usize = 0x0008;

const COPY: usize = 1;
const ACTUAL_SIZE: usize = 2;
const CLOSE: usize = 3;
const CLOSE_ALL: usize = 4;

/// Open pin windows, as HWND values.
static PINS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Number of pins currently open.
pub fn count() -> usize {
    PINS.lock().unwrap_or_else(PoisonError::into_inner).len()
}

/// Asks every pin to close. Each one closes on its own thread.
pub fn close_all() {
    let pins = PINS.lock().unwrap_or_else(PoisonError::into_inner).clone();
    for hwnd in pins {
        // SAFETY: posting to a window that has just closed fails harmlessly.
        unsafe { PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0) };
    }
}

/// Opens `image` in a new pin and returns its window handle once the window exists.
///
/// `at` is the desktop position the image was captured from: the pin opens exactly
/// there when the area fits comfortably on its monitor, otherwise scaled down and
/// centred. `closed` runs on the pin's thread after it closes.
pub fn open(
    image: RgbaImage,
    at: Option<[i32; 2]>,
    closed: impl Fn() + Send + 'static,
) -> Result<usize, String> {
    let (ready, created) = mpsc::channel();
    thread::Builder::new()
        .name("rust-screen-pin".into())
        .spawn(move || run(image, at, Box::new(closed), &ready))
        .map_err(|error| format!("無法建立釘選視窗：{error}"))?;
    created
        .recv()
        .unwrap_or_else(|_| Err("釘選視窗意外結束".into()))
}

struct State {
    bitmap: Bitmap,
    zoom: f32,
    opacity: f32,
    label: Option<String>,
    closed: Box<dyn Fn() + Send>,
}

impl State {
    fn size(&self) -> [u32; 2] {
        [self.bitmap.width(), self.bitmap.height()]
    }
}

/// Owns the window state for the whole message loop. The window is always destroyed
/// before this drops, and the window procedure only ever borrows the state.
struct OwnedState(*mut State);
impl Drop for OwnedState {
    fn drop(&mut self) {
        // SAFETY: allocated by Box::into_raw in run(); no window refers to it any more.
        unsafe { drop(Box::from_raw(self.0)) };
    }
}

fn run(
    image: RgbaImage,
    at: Option<[i32; 2]>,
    closed: Box<dyn Fn() + Send>,
    ready: &mpsc::Sender<Result<usize, String>>,
) {
    static CLASS: OnceLock<Result<u16, String>> = OnceLock::new();
    platform::enable_dpi_awareness();
    let bitmap = match Bitmap::from_rgba(&image) {
        Ok(bitmap) => bitmap,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    drop(image);
    // SAFETY: the class lives for the process. The window, its state and its message
    // loop all stay on this thread, and the state outlives the window (see OwnedState).
    unsafe {
        let atom = match CLASS
            .get_or_init(|| {
                platform::register_class(
                    "RustScreenPin",
                    Some(window_proc),
                    LoadCursorW(null_mut(), IDC_SIZEALL),
                )
                .map_err(|error| format!("無法建立釘選視窗：{error}"))
            })
            .as_ref()
        {
            Ok(atom) => *atom,
            Err(error) => {
                let _ = ready.send(Err(error.clone()));
                return;
            }
        };
        let size = [bitmap.width(), bitmap.height()];
        let point = at.map_or_else(cursor, |[x, y]| POINT { x, y });
        let mut monitor: MONITORINFO = zeroed();
        monitor.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(
            MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST),
            &raw mut monitor,
        ) == 0
        {
            // Fall back to the whole desktop rather than a zero-sized work area.
            let desktop = platform::desktop_bounds().unwrap_or(crate::geometry::DesktopRect {
                x: 0,
                y: 0,
                width: 1280,
                height: 720,
            });
            monitor.rcMonitor = RECT {
                left: desktop.x,
                top: desktop.y,
                right: desktop.x + desktop.width as i32,
                bottom: desktop.y + desktop.height as i32,
            };
            monitor.rcWork = monitor.rcMonitor;
        }
        let (rect, zoom) = placement(size, at, monitor.rcMonitor, monitor.rcWork);
        let state = Box::into_raw(Box::new(State {
            bitmap,
            zoom,
            opacity: 1.0,
            label: None,
            closed,
        }));
        let _owner = OwnedState(state);
        let title = wide("Rust Screen · 釘選");
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
            atom as usize as *const u16,
            title.as_ptr(),
            WS_POPUP,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            state.cast(),
        );
        if hwnd.is_null() {
            let _ = ready.send(Err(format!(
                "無法建立釘選視窗：{}",
                std::io::Error::last_os_error()
            )));
            return;
        }
        PINS.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(hwnd as usize);
        SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA);
        ShowWindow(hwnd, SW_SHOWNORMAL);
        // Take focus so Esc and Ctrl+C work straight away.
        SetForegroundWindow(hwnd);
        UpdateWindow(hwnd);
        let _ = ready.send(Ok(hwnd as usize));
        let mut message: MSG = zeroed();
        while GetMessageW(&raw mut message, null_mut(), 0, 0) > 0 {
            TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
}

fn cursor() -> POINT {
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: GetCursorPos writes one POINT.
    unsafe { GetCursorPos(&raw mut point) };
    point
}

/// Smallest and largest zoom for an image of this size.
fn zoom_range([width, height]: [u32; 2]) -> (f32, f32) {
    let short = width.min(height).max(1) as f32;
    ((MIN_SIDE / short).min(1.0), MAX_ZOOM)
}

/// Window rectangle for an image area of `width` × `height` with its top-left at `x, y`.
fn frame(x: i32, y: i32, width: i32, height: i32) -> RECT {
    RECT {
        left: x - BORDER,
        top: y - BORDER,
        right: x + width + BORDER,
        bottom: y + height + BORDER,
    }
}

/// Where a new pin opens, and at what zoom. In place over the captured area when it
/// lies on one monitor and leaves room around it (a full-screen pin in place would look
/// like a frozen screen); otherwise scaled to fit two thirds of the work area, centred.
fn placement(size: [u32; 2], at: Option<[i32; 2]>, monitor: RECT, work: RECT) -> (RECT, f32) {
    let (width, height) = (i64::from(size[0]), i64::from(size[1]));
    let work_width = i64::from(work.right - work.left).max(1);
    let work_height = i64::from(work.bottom - work.top).max(1);
    if let Some([x, y]) = at {
        let (x, y) = (i64::from(x), i64::from(y));
        let inside = x >= i64::from(monitor.left)
            && y >= i64::from(monitor.top)
            && x + width <= i64::from(monitor.right)
            && y + height <= i64::from(monitor.bottom);
        let roomy = width * 10 <= work_width * 9 && height * 10 <= work_height * 9;
        if inside && roomy {
            return (frame(x as i32, y as i32, width as i32, height as i32), 1.0);
        }
    }
    let (min_zoom, _) = zoom_range(size);
    let zoom = (work_width as f32 * 2.0 / 3.0 / width as f32)
        .min(work_height as f32 * 2.0 / 3.0 / height as f32)
        .min(1.0)
        .max(min_zoom);
    let shown_width = (width as f32 * zoom).round() as i64;
    let shown_height = (height as f32 * zoom).round() as i64;
    (
        frame(
            (i64::from(work.left) + (work_width - shown_width) / 2) as i32,
            (i64::from(work.top) + (work_height - shown_height) / 2) as i32,
            shown_width as i32,
            shown_height as i32,
        ),
        zoom,
    )
}

/// The window rectangle after zooming to `zoom`, keeping the image point under
/// `anchor` (a desktop position) where it is.
fn zoomed(window: RECT, size: [u32; 2], zoom: f32, anchor: POINT) -> RECT {
    let old_width = (window.right - window.left - 2 * BORDER).max(1) as f32;
    let old_height = (window.bottom - window.top - 2 * BORDER).max(1) as f32;
    let fx = ((anchor.x - window.left - BORDER) as f32 / old_width).clamp(0.0, 1.0);
    let fy = ((anchor.y - window.top - BORDER) as f32 / old_height).clamp(0.0, 1.0);
    let width = (size[0] as f32 * zoom).round().max(1.0);
    let height = (size[1] as f32 * zoom).round().max(1.0);
    frame(
        anchor.x - (fx * width).round() as i32,
        anchor.y - (fy * height).round() as i32,
        width as i32,
        height as i32,
    )
}

/// Runs `f` with the window's state. `f` must not call anything that sends a message to
/// this window, so the mutable borrow can never alias a reentrant one.
unsafe fn with_state<R>(hwnd: HWND, f: impl FnOnce(&mut State) -> R) -> Option<R> {
    // SAFETY: GWLP_USERDATA holds the live state from WM_NCCREATE until WM_NCDESTROY.
    unsafe {
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        (!pointer.is_null()).then(|| f(&mut *pointer))
    }
}

/// Shows `text` briefly in the corner of the pin.
unsafe fn flash_label(hwnd: HWND, text: String) {
    // SAFETY: plain calls on our own window; the borrow ends before SetTimer.
    unsafe {
        with_state(hwnd, |state| state.label = Some(text));
        SetTimer(hwnd, LABEL_TIMER, LABEL_MS, None);
        InvalidateRect(hwnd, null(), 0);
    }
}

fn centre(hwnd: HWND) -> POINT {
    // SAFETY: GetWindowRect writes one RECT for our own window.
    let window = unsafe {
        let mut window: RECT = zeroed();
        GetWindowRect(hwnd, &raw mut window);
        window
    };
    POINT {
        x: (window.left + window.right) / 2,
        y: (window.top + window.bottom) / 2,
    }
}

/// Sets the zoom (clamped to the image's limits) and resizes about `anchor`.
unsafe fn zoom_to(hwnd: HWND, zoom: impl FnOnce(f32) -> f32, anchor: POINT) {
    // SAFETY: the state borrow ends before SetWindowPos, which sends messages back.
    unsafe {
        let Some((size, zoom)) = with_state(hwnd, |state| {
            let (min, max) = zoom_range(state.size());
            state.zoom = zoom(state.zoom).clamp(min, max);
            (state.size(), state.zoom)
        }) else {
            return;
        };
        let mut window: RECT = zeroed();
        GetWindowRect(hwnd, &raw mut window);
        let rect = zoomed(window, size, zoom, anchor);
        SetWindowPos(
            hwnd,
            null_mut(),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        flash_label(hwnd, format!("{:.0}%", zoom * 100.0));
    }
}

unsafe fn set_opacity(hwnd: HWND, change: f32) {
    // SAFETY: the state borrow ends before the window is updated.
    unsafe {
        let Some(opacity) = with_state(hwnd, |state| {
            state.opacity = (state.opacity + change).clamp(MIN_OPACITY, 1.0);
            state.opacity
        }) else {
            return;
        };
        SetLayeredWindowAttributes(hwnd, 0, (opacity * 255.0).round() as u8, LWA_ALPHA);
        flash_label(hwnd, format!("不透明度 {:.0}%", opacity * 100.0));
    }
}

unsafe fn copy(hwnd: HWND) {
    // SAFETY: reading the DIB does not touch the window.
    unsafe {
        let Some(image) = with_state(hwnd, |state| state.bitmap.to_rgba(state.bitmap.full()))
        else {
            return;
        };
        let text = match export::copy_image(&image) {
            Ok(()) => "已複製".to_string(),
            Err(error) => format!("複製失敗：{error}"),
        };
        flash_label(hwnd, text);
    }
}

unsafe fn menu(hwnd: HWND, at: POINT) {
    // SAFETY: the strings outlive AppendMenuW, which copies them. The menu is destroyed
    // after TrackPopupMenu, and no state borrow spans its nested message loop.
    unsafe {
        let handle = CreatePopupMenu();
        if handle.is_null() {
            return;
        }
        let items = [
            (COPY, "複製圖片\tCtrl+C"),
            (ACTUAL_SIZE, "原始大小 100%"),
            (0, ""),
            (CLOSE, "關閉\tEsc／雙擊"),
            (CLOSE_ALL, "關閉全部釘選"),
        ];
        for (id, label) in items {
            if id == 0 {
                AppendMenuW(handle, MF_SEPARATOR, 0, null());
            } else {
                let label = wide(label);
                AppendMenuW(handle, MF_STRING, id, label.as_ptr());
            }
        }
        SetForegroundWindow(hwnd);
        let choice = TrackPopupMenu(
            handle,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            at.x,
            at.y,
            0,
            hwnd,
            null(),
        );
        DestroyMenu(handle);
        PostMessageW(hwnd, WM_NULL, 0, 0);
        if choice != 0 {
            PostMessageW(hwnd, WM_COMMAND, choice as usize, 0);
        }
    }
}

unsafe fn paint(hwnd: HWND) {
    // SAFETY: every GDI object created here is selected out and deleted before return;
    // drawing into memory DCs sends no messages, so the state borrow is not reentered.
    unsafe {
        let mut paint: PAINTSTRUCT = zeroed();
        let dc = BeginPaint(hwnd, &raw mut paint);
        let mut client: RECT = zeroed();
        GetClientRect(hwnd, &raw mut client);
        let (width, height) = (client.right, client.bottom);
        let memory = CreateCompatibleDC(dc);
        let canvas = CreateCompatibleBitmap(dc, width.max(1), height.max(1));
        let previous = SelectObject(memory, canvas);
        let border = CreateSolidBrush(BORDER_COLOR);
        FillRect(memory, &raw const client, border);
        DeleteObject(border);
        with_state(hwnd, |state| {
            let (inner_width, inner_height) = (width - 2 * BORDER, height - 2 * BORDER);
            let (source_width, source_height) =
                (state.bitmap.width() as i32, state.bitmap.height() as i32);
            if inner_width > 0 && inner_height > 0 {
                if (inner_width, inner_height) == (source_width, source_height) {
                    BitBlt(
                        memory,
                        BORDER,
                        BORDER,
                        inner_width,
                        inner_height,
                        state.bitmap.dc,
                        0,
                        0,
                        SRCCOPY,
                    );
                } else {
                    SetStretchBltMode(memory, HALFTONE);
                    SetBrushOrgEx(memory, 0, 0, null_mut());
                    StretchBlt(
                        memory,
                        BORDER,
                        BORDER,
                        inner_width,
                        inner_height,
                        state.bitmap.dc,
                        0,
                        0,
                        source_width,
                        source_height,
                        SRCCOPY,
                    );
                }
            }
            if let Some(label) = &state.label {
                draw_label(memory, GetDpiForWindow(hwnd), label);
            }
        });
        BitBlt(dc, 0, 0, width, height, memory, 0, 0, SRCCOPY);
        SelectObject(memory, previous);
        DeleteObject(canvas);
        DeleteDC(memory);
        EndPaint(hwnd, &raw const paint);
    }
}

unsafe fn draw_label(dc: HDC, dpi: u32, text: &str) {
    // SAFETY: the font is selected out and deleted before return.
    unsafe {
        let dpi = dpi.max(96) as i32;
        let mut logical: LOGFONTW = zeroed();
        logical.lfHeight = -(10 * dpi / 72);
        logical.lfWeight = FW_SEMIBOLD as i32;
        for (slot, unit) in logical
            .lfFaceName
            .iter_mut()
            .zip("Microsoft JhengHei UI".encode_utf16())
        {
            *slot = unit;
        }
        let font = CreateFontIndirectW(&raw const logical);
        let previous = SelectObject(dc, font);
        let mut text: Vec<u16> = text.encode_utf16().collect();
        let length = text.len() as i32;
        let mut measure = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        DrawTextW(
            dc,
            text.as_mut_ptr(),
            length,
            &raw mut measure,
            DT_CALCRECT | DT_SINGLELINE,
        );
        let pad = 6 * dpi / 96;
        let left = BORDER + pad;
        let top = BORDER + pad;
        let mut box_rect = RECT {
            left,
            top,
            right: left + measure.right + pad * 2,
            bottom: top + measure.bottom + pad,
        };
        let background = CreateSolidBrush(0x0020_2020);
        FillRect(dc, &raw const box_rect, background);
        DeleteObject(background);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, 0x00FF_FFFF);
        DrawTextW(
            dc,
            text.as_mut_ptr(),
            length,
            &raw mut box_rect,
            DT_SINGLELINE | DT_CENTER | DT_VCENTER,
        );
        SelectObject(dc, previous);
        DeleteObject(font);
    }
}

/// Signed screen coordinates packed in a mouse message's LPARAM.
fn lparam_point(lparam: LPARAM) -> POINT {
    POINT {
        x: lparam as u16 as i16 as i32,
        y: (lparam >> 16) as u16 as i16 as i32,
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: called by Windows on this window's thread. State is reached only through
    // with_state, whose borrows never span a call that re-enters this procedure.
    unsafe {
        match message {
            WM_NCCREATE => {
                let create = &*(lparam as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
                DefWindowProcW(hwnd, message, wparam, lparam)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DefWindowProcW(hwnd, message, wparam, lparam)
            }
            // The whole pin is a drag handle; Windows runs the move loop.
            WM_NCHITTEST => HTCAPTION as LRESULT,
            WM_NCLBUTTONDBLCLK => {
                DestroyWindow(hwnd);
                0
            }
            // Replace the caption's system menu with ours.
            WM_NCRBUTTONDOWN => 0,
            WM_NCRBUTTONUP => {
                menu(hwnd, lparam_point(lparam));
                0
            }
            WM_CONTEXTMENU => {
                let mut at = lparam_point(lparam);
                if lparam == -1 {
                    // Keyboard menu key: open at the top-left corner.
                    let mut window: RECT = zeroed();
                    GetWindowRect(hwnd, &raw mut window);
                    at = POINT {
                        x: window.left + BORDER,
                        y: window.top + BORDER,
                    };
                }
                menu(hwnd, at);
                0
            }
            WM_MOUSEWHEEL => {
                let notches = f32::from((wparam >> 16) as u16 as i16) / WHEEL_DELTA as f32;
                if wparam & MK_CONTROL != 0 {
                    set_opacity(hwnd, notches * 0.1);
                } else {
                    zoom_to(
                        hwnd,
                        |zoom| zoom * ZOOM_STEP.powf(notches),
                        lparam_point(lparam),
                    );
                }
                0
            }
            WM_KEYDOWN => {
                let control = GetKeyState(i32::from(VK_CONTROL)) < 0;
                if wparam == usize::from(VK_ESCAPE) {
                    DestroyWindow(hwnd);
                } else if control && wparam == usize::from(b'C') {
                    copy(hwnd);
                } else if control && wparam == usize::from(b'0') {
                    zoom_to(hwnd, |_| 1.0, centre(hwnd));
                }
                0
            }
            WM_COMMAND => {
                match wparam & 0xffff {
                    COPY => copy(hwnd),
                    ACTUAL_SIZE => zoom_to(hwnd, |_| 1.0, centre(hwnd)),
                    CLOSE => {
                        DestroyWindow(hwnd);
                    }
                    CLOSE_ALL => close_all(),
                    _ => {}
                }
                0
            }
            WM_TIMER if wparam == LABEL_TIMER => {
                KillTimer(hwnd, LABEL_TIMER);
                with_state(hwnd, |state| state.label = None);
                InvalidateRect(hwnd, null(), 0);
                0
            }
            // Keep the physical size when dragged to a monitor with another scale.
            WM_DPICHANGED => 0,
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                paint(hwnd);
                0
            }
            WM_DESTROY => {
                PINS.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .retain(|pin| *pin != hwnd as usize);
                with_state(hwnd, |state| (state.closed)());
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

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn small_capture_opens_in_place_with_the_frame_outside_the_image() {
        let monitor = rect(-1920, 0, 0, 1080);
        let work = rect(-1920, 0, 0, 1040);
        let (window, zoom) = placement([400, 300], Some([-1500, 200]), monitor, work);
        assert_eq!(zoom, 1.0);
        assert_eq!(
            (window.left, window.top, window.right, window.bottom),
            (-1500 - BORDER, 200 - BORDER, -1100 + BORDER, 500 + BORDER)
        );
    }

    #[test]
    fn full_screen_or_cross_monitor_capture_is_scaled_and_centred() {
        let monitor = rect(0, 0, 1920, 1080);
        let work = rect(0, 0, 1920, 1040);
        for (size, at) in [
            ([1920, 1080], Some([0, 0])),
            ([800, 600], Some([1500, 100])), // runs off the right edge
            ([800, 600], None),
        ] {
            let (window, zoom) = placement(size, at, monitor, work);
            let shown = [
                window.right - window.left - 2 * BORDER,
                window.bottom - window.top - 2 * BORDER,
            ];
            assert!(zoom <= 1.0, "{size:?} {at:?}");
            assert!(shown[0] <= 1920 * 2 / 3 + 1 && shown[1] <= 1040 * 2 / 3 + 1);
            let centre = [
                (window.left + window.right) / 2,
                (window.top + window.bottom) / 2,
            ];
            assert!((centre[0] - 960).abs() <= 1 && (centre[1] - 520).abs() <= 1);
        }
    }

    #[test]
    fn zoom_keeps_the_point_under_the_cursor_and_respects_limits() {
        let window = frame(100, 100, 400, 200);
        let anchor = POINT { x: 200, y: 150 }; // a quarter across, a quarter down
        let bigger = zoomed(window, [400, 200], 2.0, anchor);
        assert_eq!(bigger.right - bigger.left - 2 * BORDER, 800);
        assert_eq!(bigger.bottom - bigger.top - 2 * BORDER, 400);
        assert_eq!(bigger.left + BORDER + 200, 200);
        assert_eq!(bigger.top + BORDER + 100, 150);
        assert_eq!(zoom_range([400, 200]), (0.12, MAX_ZOOM));
        // Tiny images are never shrunk below their own size.
        assert_eq!(zoom_range([10, 30]).0, 1.0);
    }

    /// Opens a real pin, checks it is on top and drawn correctly, zooms it with the
    /// wheel and closes it. Needs an unlocked desktop and briefly shows a small window.
    #[test]
    #[ignore = "needs an interactive desktop"]
    fn pin_window_draws_zooms_and_closes() {
        use std::{
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
            time::{Duration, Instant},
        };

        // Read window rectangles in physical pixels, as the app's own threads do.
        platform::enable_dpi_awareness();
        let colour = image::Rgba([200, 40, 120, 255]);
        let image = RgbaImage::from_pixel(160, 120, colour);
        let closed = Arc::new(AtomicBool::new(false));
        let notified = closed.clone();
        let before = count();
        let hwnd = open(image, Some([200, 200]), move || {
            notified.store(true, Ordering::SeqCst);
        })
        .unwrap() as HWND;
        assert_eq!(count(), before + 1);
        thread::sleep(Duration::from_millis(400));
        // SAFETY: the window belongs to this process and stays open until WM_CLOSE.
        unsafe {
            let style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            assert!(style & WS_EX_TOPMOST != 0);
            let mut window: RECT = zeroed();
            GetWindowRect(hwnd, &raw mut window);
            assert_eq!(
                (window.left, window.top, window.right, window.bottom),
                (198, 198, 362, 322)
            );
            let shot = platform::capture(crate::geometry::DesktopRect {
                x: window.left,
                y: window.top,
                width: 164,
                height: 124,
            })
            .unwrap();
            assert_eq!(shot.get_pixel(0, 0).0, [78, 215, 185, 255], "frame");
            assert_eq!(shot.get_pixel(82, 62).0, colour.0, "image");
            // One wheel notch up over the centre zooms in by 10 % about the cursor.
            SendMessageW(
                hwnd,
                WM_MOUSEWHEEL,
                (WHEEL_DELTA as usize) << 16,
                ((280_isize) << 16) | 280,
            );
            GetWindowRect(hwnd, &raw mut window);
            assert_eq!(window.right - window.left - 2 * BORDER, 176);
            assert_eq!(window.bottom - window.top - 2 * BORDER, 132);
        }
        close_all();
        let deadline = Instant::now() + Duration::from_secs(3);
        while count() > before && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(count(), before);
        assert!(closed.load(Ordering::SeqCst));
    }
}
