//! Native, borderless desktop selector. All coordinates are physical screenshot pixels.
use std::{
    mem::{size_of, zeroed},
    ptr::null_mut,
    sync::OnceLock,
};

use image::RgbaImage;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus},
        WindowsAndMessaging::*,
    },
};

use crate::{
    geometry::{CropRect, DesktopRect},
    platform::{self, Bitmap, wide},
};

/// Black at 60% opacity leaves 40% of the original, matching the former 2/5 dimming.
const DIM_ALPHA: u8 = 153;

#[derive(Debug)]
struct Selection {
    width: u32,
    height: u32,
    fixed: Option<[u32; 2]>,
    origin: Option<[f32; 2]>,
    pointer: [f32; 2],
    crop: Option<CropRect>,
}

impl Selection {
    fn new(width: u32, height: u32, fixed: Option<[u32; 2]>) -> Self {
        Self {
            width,
            height,
            fixed,
            origin: None,
            pointer: [0.0, 0.0],
            crop: None,
        }
    }

    fn update(&mut self, point: [f32; 2]) {
        self.pointer = [
            point[0].clamp(0.0, self.width as f32),
            point[1].clamp(0.0, self.height as f32),
        ];
        if let Some([width, height]) = self.fixed {
            let width = width.clamp(1, self.width);
            let height = height.clamp(1, self.height);
            let mut crop = CropRect {
                x: (self.pointer[0] - width as f32 / 2.0).max(0.0).round() as u32,
                y: (self.pointer[1] - height as f32 / 2.0).max(0.0).round() as u32,
                width,
                height,
            };
            crop.fit(self.width, self.height);
            self.crop = Some(crop);
        } else if let Some(origin) = self.origin {
            self.crop = Some(CropRect::from_points(
                origin,
                self.pointer,
                self.width,
                self.height,
            ));
        }
    }

    fn begin(&mut self, point: [f32; 2]) {
        self.origin = None;
        self.crop = None;
        self.update(point);
        self.origin = Some(self.pointer);
    }

    fn finish(&mut self, point: [f32; 2]) -> Option<CropRect> {
        // A release without a matching press (e.g. the capture button's release) is ignored.
        self.origin?;
        self.update(point);
        self.origin = None;
        self.crop
            .filter(|crop| self.fixed.is_some() || (crop.width >= 2 && crop.height >= 2))
    }
}

struct Brush(HBRUSH);
impl Drop for Brush {
    fn drop(&mut self) {
        // SAFETY: brushes are created specifically for this overlay.
        unsafe { DeleteObject(self.0) };
    }
}

struct State {
    selection: Selection,
    /// The native capture itself; it is never modified.
    original: Bitmap,
    /// Offscreen composition target. Dimming is applied per damaged rectangle, so no
    /// second full-size dimmed copy of the desktop is kept.
    frame: Bitmap,
    /// A single black pixel, stretched by AlphaBlend to dim the frame.
    black: Bitmap,
    accent: Brush,
    background: Brush,
    done: bool,
    armed: bool,
    result: Option<CropRect>,
}

impl State {
    fn new(original: Bitmap, fixed: Option<[u32; 2]>) -> Result<Self, String> {
        // SAFETY: these brushes are owned by State and released with the DIB surfaces.
        let accent = Brush(unsafe { CreateSolidBrush(0x00B9_D74E) });
        let background = Brush(unsafe { CreateSolidBrush(0x0027_1B13) });
        if accent.0.is_null() || background.0.is_null() {
            return Err("無法建立選框畫筆".into());
        }
        let (width, height) = (original.width(), original.height());
        Ok(Self {
            selection: Selection::new(width, height, fixed),
            frame: Bitmap::blank(width, height)?,
            black: Bitmap::blank(1, 1)?, // new DIB sections are zero-filled
            original,
            accent,
            background,
            done: false,
            armed: false,
            result: None,
        })
    }

    fn pointer(&self, hwnd: HWND) -> [f32; 2] {
        // SAFETY: POINT is an output struct. ScreenToClient accounts for negative monitor origins.
        unsafe {
            let mut point: POINT = zeroed();
            GetCursorPos(&raw mut point);
            ScreenToClient(hwnd, &raw mut point);
            [point.x as f32, point.y as f32]
        }
    }

    fn event_point(&self, hwnd: HWND, lparam: LPARAM) -> [f32; 2] {
        message_point(lparam, self.pointer(hwnd))
    }

    fn paint(&self, hwnd: HWND) {
        // SAFETY: BeginPaint/EndPaint are paired. Only this final BitBlt touches the
        // window DC; all intermediate dimming, selection and text drawing stays offscreen.
        unsafe {
            let mut paint: PAINTSTRUCT = zeroed();
            let dc = BeginPaint(hwnd, &raw mut paint);
            if !dc.is_null() {
                let dirty = paint.rcPaint;
                if dirty.right > dirty.left && dirty.bottom > dirty.top {
                    self.compose_frame(self.hint_rect(hwnd), dirty);
                    BitBlt(
                        dc,
                        dirty.left,
                        dirty.top,
                        dirty.right - dirty.left,
                        dirty.bottom - dirty.top,
                        self.frame.dc,
                        dirty.left,
                        dirty.top,
                        SRCCOPY,
                    );
                }
            }
            EndPaint(hwnd, &raw const paint);
        }
    }

    fn compose_frame(&self, label: RECT, dirty: RECT) {
        // SAFETY: the persistent memory DC has the same physical dimensions as the
        // source DIB. Restore the clip and all DC attributes before the next frame.
        unsafe {
            let dc = self.frame.dc;
            let saved = SaveDC(dc);
            if saved != 0 {
                IntersectClipRect(dc, dirty.left, dirty.top, dirty.right, dirty.bottom);
            }
            let left = dirty.left.max(0);
            let top = dirty.top.max(0);
            let width = dirty.right.min(self.selection.width as i32) - left;
            let height = dirty.bottom.min(self.selection.height as i32) - top;
            if width > 0 && height > 0 {
                BitBlt(
                    dc,
                    left,
                    top,
                    width,
                    height,
                    self.original.dc,
                    left,
                    top,
                    SRCCOPY,
                );
                AlphaBlend(
                    dc,
                    left,
                    top,
                    width,
                    height,
                    self.black.dc,
                    0,
                    0,
                    1,
                    1,
                    BLENDFUNCTION {
                        BlendOp: AC_SRC_OVER as u8,
                        BlendFlags: 0,
                        SourceConstantAlpha: DIM_ALPHA,
                        AlphaFormat: 0,
                    },
                );
            }
            if let Some(crop) = self.selection.crop {
                let x = crop.x as i32;
                let y = crop.y as i32;
                let width = crop.width as i32;
                let height = crop.height as i32;
                BitBlt(dc, x, y, width, height, self.original.dc, x, y, SRCCOPY);
                let rect = RECT {
                    left: x,
                    top: y,
                    right: x + width,
                    bottom: y + height,
                };
                FrameRect(dc, &raw const rect, self.accent.0);
                let inner = RECT {
                    left: x + 1,
                    top: y + 1,
                    right: x + width - 1,
                    bottom: y + height - 1,
                };
                if width > 3 && height > 3 {
                    FrameRect(dc, &raw const inner, self.accent.0);
                }
            }

            let x = label.left;
            let y = label.top;
            FillRect(dc, &raw const label, self.background.0);
            let dimensions = self
                .selection
                .crop
                .map(|crop| format!("{} × {} px", crop.width, crop.height))
                .unwrap_or_else(|| "直接在螢幕拖曳選取範圍".into());
            let hint = if self.selection.fixed.is_some() {
                "固定尺寸：移動或拖曳定位，放開完成｜Esc / 右鍵取消"
            } else {
                "按住左鍵拖曳，放開完成｜Esc / 右鍵取消"
            };
            let previous = SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, 0x00B9D74E);
            let title = wide(&dimensions);
            let mut title_rect = RECT {
                left: x + 12,
                top: y + 9,
                right: label.right - 8,
                bottom: y + 30,
            };
            DrawTextW(
                dc,
                title.as_ptr(),
                -1,
                &raw mut title_rect,
                DT_LEFT | DT_SINGLELINE | DT_NOPREFIX,
            );
            SetTextColor(dc, 0x00F0E5D7);
            let hint = wide(hint);
            let mut hint_rect = RECT {
                left: x + 12,
                top: y + 35,
                right: label.right - 8,
                bottom: label.bottom - 4,
            };
            DrawTextW(
                dc,
                hint.as_ptr(),
                -1,
                &raw mut hint_rect,
                DT_LEFT | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(dc, previous);
            if saved != 0 {
                RestoreDC(dc, saved);
            }
        }
    }

    fn hint_rect(&self, hwnd: HWND) -> RECT {
        // SAFETY: monitor and coordinate APIs only fill their output structs. Anchor
        // the label to the same queued-event pointer used to draw the selection.
        unsafe {
            let mut point = POINT {
                x: self.selection.pointer[0] as i32,
                y: self.selection.pointer[1] as i32,
            };
            ClientToScreen(hwnd, &raw mut point);
            let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
            let mut info: MONITORINFO = zeroed();
            info.cbSize = size_of::<MONITORINFO>() as u32;
            let mut area = RECT {
                left: 0,
                top: 0,
                right: self.selection.width as i32,
                bottom: self.selection.height as i32,
            };
            if GetMonitorInfoW(monitor, &raw mut info) != 0 {
                let mut origin = POINT {
                    x: info.rcMonitor.left,
                    y: info.rcMonitor.top,
                };
                ScreenToClient(hwnd, &raw mut origin);
                area = RECT {
                    left: origin.x,
                    top: origin.y,
                    right: origin.x + info.rcMonitor.right - info.rcMonitor.left,
                    bottom: origin.y + info.rcMonitor.bottom - info.rcMonitor.top,
                };
            }
            let width = 470.min(area.right - area.left);
            let x = (self.selection.pointer[0] as i32 + 22)
                .clamp(area.left, (area.right - width).max(area.left));
            let y = (self.selection.pointer[1] as i32 + 28)
                .clamp(area.top, (area.bottom - 68).max(area.top));
            RECT {
                left: x,
                top: y,
                right: x + width,
                bottom: y + 68,
            }
        }
    }

    fn invalidate_changes(&self, hwnd: HWND, old_crop: Option<CropRect>, old_label: RECT) {
        let label = self.hint_rect(hwnd);
        // SAFETY: each damaged rectangle is in client pixels. Background erase stays
        // disabled; unchanged parts of the virtual desktop are no longer invalidated.
        unsafe {
            if old_crop != self.selection.crop {
                for crop in [old_crop, self.selection.crop].into_iter().flatten() {
                    let rect = RECT {
                        left: crop.x as i32,
                        top: crop.y as i32,
                        right: (crop.x + crop.width) as i32,
                        bottom: (crop.y + crop.height) as i32,
                    };
                    InvalidateRect(hwnd, &raw const rect, 0);
                }
            }
            if old_crop != self.selection.crop
                || (
                    old_label.left,
                    old_label.top,
                    old_label.right,
                    old_label.bottom,
                ) != (label.left, label.top, label.right, label.bottom)
            {
                InvalidateRect(hwnd, &raw const old_label, 0);
                InvalidateRect(hwnd, &raw const label, 0);
            }
        }
    }
}

fn message_point(lparam: LPARAM, current: [f32; 2]) -> [f32; 2] {
    // Use the position belonging to this queued mouse event, rather than a later cursor
    // position. LPARAM packs signed 16-bit coordinates; unwrap unusually wide desktops
    // using the current pointer only as a hint for the 65536-pixel coordinate band.
    let packed = lparam as u32;
    let x = packed as u16 as i16 as f32;
    let y = (packed >> 16) as u16 as i16 as f32;
    [
        x + ((current[0] - x) / 65536.0).round() * 65536.0,
        y + ((current[1] - y) / 65536.0).round() * 65536.0,
    ]
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: userdata points to the Box<State> kept alive by select() until after
    // DestroyWindow. No state is freed by callbacks; destruction occurs in the outer loop.
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lparam as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        if message == WM_NCDESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        if message == WM_ERASEBKGND {
            return 1;
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        if pointer.is_null() {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let state = &mut *pointer;
        match message {
            WM_PAINT => state.paint(hwnd),
            WM_LBUTTONDOWN => {
                let previous = (state.selection.crop, state.hint_rect(hwnd));
                state.selection.begin(state.event_point(hwnd, lparam));
                state.invalidate_changes(hwnd, previous.0, previous.1);
                SetCapture(hwnd);
            }
            WM_MOUSEMOVE => {
                let previous = (state.selection.crop, state.hint_rect(hwnd));
                state.selection.update(state.event_point(hwnd, lparam));
                state.invalidate_changes(hwnd, previous.0, previous.1);
            }
            WM_LBUTTONUP => {
                let previous = (state.selection.crop, state.hint_rect(hwnd));
                if let Some(crop) = state.selection.finish(state.event_point(hwnd, lparam)) {
                    state.result = Some(crop);
                    state.done = true;
                }
                state.invalidate_changes(hwnd, previous.0, previous.1);
                ReleaseCapture();
            }
            WM_KEYDOWN if wparam == 0x1B => state.done = true,
            WM_KEYDOWN if wparam == 0x0D && state.selection.fixed.is_some() => {
                state.result = state.selection.crop;
                state.done = true;
            }
            WM_RBUTTONDOWN | WM_CLOSE | WM_DESTROY | WM_CANCELMODE | WM_DISPLAYCHANGE => {
                state.done = true
            }
            WM_KILLFOCUS if state.armed => state.done = true,
            _ => return DefWindowProcW(hwnd, message, wparam, lparam),
        }
        0
    }
}

struct OverlayWindow(HWND);
impl Drop for OverlayWindow {
    fn drop(&mut self) {
        // SAFETY: this window belongs to the current worker thread. Callbacks still have
        // access to State because OverlayWindow is dropped first.
        unsafe { DestroyWindow(self.0) };
    }
}

struct OwnedState(*mut State);
impl Drop for OwnedState {
    fn drop(&mut self) {
        // SAFETY: the state was allocated by Box::into_raw and the window is already destroyed.
        unsafe { drop(Box::from_raw(self.0)) };
    }
}

/// Freeze the captured desktop and let the user select directly on its physical coordinates.
/// Mouse release confirms; Esc, right-click, focus loss or display changes cancel.
/// Takes ownership of the native capture and converts only the selected pixels to RGBA.
pub fn select(
    capture: Bitmap,
    bounds: DesktopRect,
    fixed: Option<[u32; 2]>,
) -> Result<Option<RgbaImage>, String> {
    Ok(select_area(capture, bounds, fixed)?.map(|(_, image)| image))
}

/// Like [`select`], but also returns the chosen area in desktop coordinates.
pub fn select_area(
    capture: Bitmap,
    bounds: DesktopRect,
    fixed: Option<[u32; 2]>,
) -> Result<Option<(DesktopRect, RgbaImage)>, String> {
    static CLASS: OnceLock<Result<u16, String>> = OnceLock::new();
    // SAFETY: class has process lifetime, the window and message loop share one thread,
    // and the boxed state outlives every call into window_proc.
    unsafe {
        let instance = GetModuleHandleW(null_mut());
        let atom = CLASS
            .get_or_init(|| {
                platform::register_class(
                    "RustScreenSelectionOverlay",
                    Some(window_proc),
                    LoadCursorW(null_mut(), IDC_CROSS),
                )
                .map_err(|error| format!("無法建立螢幕選取視窗：{error}"))
            })
            .as_ref()
            .map_err(Clone::clone)?;
        let state = Box::into_raw(Box::new(State::new(capture, fixed)?));
        // Keep only a raw owning pointer while Win32 callbacks can mutate the state.
        // This avoids holding a Box's unique reference across reentrant window callbacks.
        let _state_owner = OwnedState(state);
        let title = wide("Rust Screen · 直接選取螢幕");
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_APPWINDOW,
            *atom as usize as *const u16,
            title.as_ptr(),
            WS_POPUP,
            bounds.x,
            bounds.y,
            bounds.width as i32,
            bounds.height as i32,
            null_mut(),
            null_mut(),
            instance,
            state.cast(),
        );
        if hwnd.is_null() {
            return Err(format!(
                "開啟選取視窗失敗：{}",
                std::io::Error::last_os_error()
            ));
        }
        let window = OverlayWindow(hwnd);
        (*state).selection.update((*state).pointer(hwnd));
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            bounds.x,
            bounds.y,
            bounds.width as i32,
            bounds.height as i32,
            SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        );
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);
        (*state).armed = true;
        UpdateWindow(hwnd);
        let mut message: MSG = zeroed();
        loop {
            // window_proc mutates done through GWLP_USERDATA during DispatchMessageW.
            if (*state).done {
                break;
            }
            let result = GetMessageW(&raw mut message, hwnd, 0, 0);
            if result == -1 {
                return Err(format!(
                    "螢幕選取作業失敗：{}",
                    std::io::Error::last_os_error()
                ));
            }
            if result == 0 {
                break;
            }
            TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
        ReleaseCapture();
        drop(window);
        Ok((*state).result.map(|mut crop| {
            crop.fit((*state).original.width(), (*state).original.height());
            let area = DesktopRect {
                x: bounds.x + crop.x as i32,
                y: bounds.y + crop.y as i32,
                width: crop.width,
                height: crop.height,
            };
            (area, (*state).original.to_rgba(crop))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendering_fixture() -> (State, RECT, RECT) {
        let image = RgbaImage::from_pixel(128, 96, image::Rgba([100, 150, 200, 255]));
        let state = State::new(Bitmap::from_rgba(&image).unwrap(), None).unwrap();
        let full = RECT {
            left: 0,
            top: 0,
            right: 128,
            bottom: 96,
        };
        let label = RECT {
            left: 256,
            top: 256,
            right: 726,
            bottom: 324,
        };
        (state, full, label)
    }

    fn pixel(dc: HDC, x: i32, y: i32) -> u32 {
        // SAFETY: tests sample only in-bounds pixels of their own memory DCs.
        unsafe {
            GdiFlush();
            GetPixel(dc, x, y)
        }
    }

    #[test]
    fn frame_composes_bright_selection_and_dim_background_without_altering_capture() {
        let (mut state, full, label) = rendering_fixture();
        state.selection.crop = Some(CropRect {
            x: 10,
            y: 10,
            width: 50,
            height: 50,
        });
        state.compose_frame(label, full);
        assert_eq!(pixel(state.frame.dc, 25, 25), 0x00C89664); // source RGB 100,150,200
        assert_eq!(pixel(state.frame.dc, 100, 80), 0x00503C28); // dimmed RGB 40,60,80
        assert_eq!(pixel(state.frame.dc, 10, 10), 0x00B9D74E); // selection border
        assert_eq!(pixel(state.original.dc, 100, 80), 0x00C89664);
        assert_eq!(pixel(state.original.dc, 10, 10), 0x00C89664);
    }

    #[test]
    fn moving_selection_clears_old_pixels_and_preserves_unchanged_frame_regions() {
        let (mut state, full, label) = rendering_fixture();
        state.selection.crop = Some(CropRect {
            x: 10,
            y: 10,
            width: 50,
            height: 50,
        });
        state.compose_frame(label, full);
        // SAFETY: place a sentinel in our own bitmap outside the upcoming damaged region.
        unsafe { SetPixelV(state.frame.dc, 110, 80, 0x00010203) };
        state.selection.crop = Some(CropRect {
            x: 60,
            y: 20,
            width: 30,
            height: 30,
        });
        state.compose_frame(
            label,
            RECT {
                left: 10,
                top: 10,
                right: 90,
                bottom: 60,
            },
        );
        assert_eq!(pixel(state.frame.dc, 25, 25), 0x00503C28); // previous selection cleared
        assert_eq!(pixel(state.frame.dc, 70, 30), 0x00C89664); // current selection bright
        assert_eq!(pixel(state.frame.dc, 110, 80), 0x00010203); // outside damage untouched
        state.compose_frame(
            label,
            RECT {
                left: 100,
                top: 70,
                right: 128,
                bottom: 96,
            },
        );
        assert_eq!(pixel(state.frame.dc, 110, 80), 0x00503C28); // clip restored between paints
    }

    #[test]
    fn confirmed_selection_converts_only_the_selected_capture_pixels() {
        let image = RgbaImage::from_fn(40, 30, |x, y| image::Rgba([x as u8, y as u8, 7, 255]));
        let state = State::new(Bitmap::from_rgba(&image).unwrap(), None).unwrap();
        let crop = CropRect {
            x: 5,
            y: 6,
            width: 20,
            height: 10,
        };
        assert_eq!(
            state.original.to_rgba(crop),
            image::imageops::crop_imm(&image, 5, 6, 20, 10).to_image()
        );
    }

    #[test]
    fn mouse_event_uses_queued_position_even_when_cursor_has_moved() {
        let packed = (200u32 << 16 | 300) as LPARAM;
        assert_eq!(message_point(packed, [900.0, 700.0]), [300.0, 200.0]);
        let wide = (200u32 << 16 | 40000) as LPARAM;
        assert_eq!(message_point(wide, [41000.0, 300.0]), [40000.0, 200.0]);
    }

    #[test]
    fn direct_drag_handles_reverse_direction_and_ignores_accidental_click() {
        let mut selection = Selection::new(1920, 1080, None);
        assert_eq!(selection.finish([200.0, 100.0]), None);
        selection.begin([900.0, 700.0]);
        selection.update([100.0, 200.0]);
        assert_eq!(
            selection.finish([100.0, 200.0]),
            Some(CropRect {
                x: 100,
                y: 200,
                width: 800,
                height: 500
            })
        );
        selection.begin([100.0, 200.0]);
        assert_eq!(selection.finish([100.0, 200.0]), None);
    }

    #[test]
    fn fixed_selection_preserves_size_at_desktop_edges() {
        let mut selection = Selection::new(1920, 1080, Some([800, 600]));
        selection.begin([1800.0, 1000.0]);
        assert_eq!(
            selection.finish([2200.0, 1200.0]),
            Some(CropRect {
                x: 1120,
                y: 480,
                width: 800,
                height: 600
            })
        );
        selection.fixed = Some([4000, 3000]);
        selection.update([0.0, 0.0]);
        assert_eq!(
            selection.crop,
            Some(CropRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            })
        );
    }
}
