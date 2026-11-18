use std::{
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
};

use image::RgbaImage;
use windows_sys::Win32::{
    Foundation::{HWND, RECT},
    Graphics::{
        Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC,
            CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HBITMAP,
            HDC, HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
        },
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
        WindowsAndMessaging::{
            GetForegroundWindow, GetSystemMetrics, GetWindowRect, HCURSOR, IsIconic,
            RegisterClassW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWNA, SW_SHOWNOACTIVATE,
            SetForegroundWindow, ShowWindow, ShowWindowAsync, WNDCLASSW, WNDPROC,
        },
    },
};

use crate::geometry::{CropRect, DesktopRect};

/// Upper bound for one 32-bit capture buffer.
const MAX_BYTES: u64 = 512 * 1024 * 1024;

/// A NUL-terminated UTF-16 string for Win32 APIs.
pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

/// Registers a window class for this module. Callers cache the atom in a `OnceLock`.
pub fn register_class(name: &str, procedure: WNDPROC, cursor: HCURSOR) -> std::io::Result<u16> {
    let name = wide(name);
    // SAFETY: RegisterClassW copies the class name; every pointer is valid for the call.
    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: procedure,
            hInstance: GetModuleHandleW(null()),
            hCursor: cursor,
            lpszClassName: name.as_ptr(),
            ..zeroed()
        };
        match RegisterClassW(&raw const class) {
            0 => Err(std::io::Error::last_os_error()),
            atom => Ok(atom),
        }
    }
}

pub fn enable_dpi_awareness() {
    // SAFETY: called before creating UI and on the capture thread so coordinates use physical pixels.
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

pub fn desktop_bounds() -> Result<DesktopRect, String> {
    // SAFETY: these system metrics do not require input pointers.
    let rect = unsafe {
        DesktopRect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(0) as u32,
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(0) as u32,
        }
    };
    if rect.width == 0 || rect.height == 0 {
        return Err("找不到可擷取的螢幕".into());
    }
    Ok(rect)
}

pub fn foreground_window() -> usize {
    // SAFETY: returning an opaque, borrowed handle as a token; it is revalidated at capture time.
    unsafe { GetForegroundWindow() as usize }
}

pub fn hide_window(token: usize) {
    // SAFETY: this borrowed handle belongs to the main application window on this thread.
    unsafe { ShowWindow(token as HWND, SW_HIDE) };
}

pub fn restore_window(token: usize, focus: bool) {
    if token == 0 {
        return;
    }
    // SAFETY: ShowWindowAsync queues work on the owning UI thread, even without repaint events.
    unsafe {
        let hwnd = token as HWND;
        let command = if IsIconic(hwnd) != 0 {
            SW_RESTORE
        } else {
            SW_SHOW
        };
        ShowWindowAsync(hwnd, command);
        if focus {
            SetForegroundWindow(hwnd);
        }
    }
}

pub fn wake_minimized_window(token: usize) {
    if token == 0 {
        return;
    }
    // SAFETY: this only wakes our own UI; foreground target was captured before this call.
    unsafe {
        let hwnd = token as HWND;
        if IsIconic(hwnd) != 0 {
            ShowWindowAsync(hwnd, SW_SHOWNOACTIVATE);
        }
    }
}

pub fn show_window_without_activation(token: usize) {
    if token == 0 {
        return;
    }
    // SAFETY: the borrowed handle is our main window. Queue a show/restore without
    // stealing focus from the foreground window being captured by a global shortcut.
    unsafe {
        let hwnd = token as HWND;
        let command = if IsIconic(hwnd) != 0 {
            SW_SHOWNOACTIVATE
        } else {
            SW_SHOWNA
        };
        ShowWindowAsync(hwnd, command);
    }
}

pub fn window_bounds(token: usize) -> Result<DesktopRect, String> {
    let hwnd = token as HWND;
    // SAFETY: Win32 validates this borrowed HWND, which may have closed since the request.
    unsafe {
        if hwnd.is_null() || IsIconic(hwnd) != 0 {
            return Err("作用中視窗不存在或已最小化，請重新觸發快捷鍵".into());
        }
        let mut rect: RECT = zeroed();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&raw mut rect).cast(),
            size_of::<RECT>() as u32,
        ) < 0
            && GetWindowRect(hwnd, &raw mut rect) == 0
        {
            return Err(format!(
                "無法取得視窗範圍：{}",
                std::io::Error::last_os_error()
            ));
        }
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return Err("視窗沒有可擷取的範圍".into());
        }
        desktop_bounds()?
            .intersection(DesktopRect {
                x: rect.left,
                y: rect.top,
                width: (rect.right - rect.left) as u32,
                height: (rect.bottom - rect.top) as u32,
            })
            .ok_or_else(|| "視窗位於可見螢幕範圍之外".into())
    }
}

/// A top-down 32-bit BGRX DIB section, selected into its own memory DC. GDI draws
/// into it directly and the CPU reads its pixels without an extra GetDIBits copy.
pub struct Bitmap {
    pub dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    pixels: *mut u8,
    width: u32,
    height: u32,
}

impl Bitmap {
    pub fn blank(width: u32, height: u32) -> Result<Self, String> {
        if width == 0
            || height == 0
            || width > i32::MAX as u32
            || height > i32::MAX as u32
            || u64::from(width) * u64::from(height) * 4 > MAX_BYTES
        {
            return Err("截圖範圍為空或超過 512 MiB，請縮小範圍".into());
        }
        // SAFETY: dimensions fit a checked 32-bit DIB. All GDI resources and its pixel
        // allocation are owned by Bitmap and remain valid until Drop.
        unsafe {
            let mut bitmap = Self {
                dc: CreateCompatibleDC(null_mut()),
                bitmap: null_mut(),
                previous: null_mut(),
                pixels: null_mut(),
                width,
                height,
            };
            if bitmap.dc.is_null() {
                return Err("無法建立截圖緩衝區".into());
            }
            let mut info: BITMAPINFO = zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32), // top-down DIB
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..zeroed()
            };
            let mut pixels = null_mut();
            bitmap.bitmap = CreateDIBSection(
                bitmap.dc,
                &raw const info,
                DIB_RGB_COLORS,
                &raw mut pixels,
                null_mut(),
                0,
            );
            if bitmap.bitmap.is_null() || pixels.is_null() {
                return Err(format!(
                    "截圖緩衝區記憶體不足：{}",
                    std::io::Error::last_os_error()
                ));
            }
            bitmap.pixels = pixels.cast();
            bitmap.previous = SelectObject(bitmap.dc, bitmap.bitmap);
            if bitmap.previous.is_null() || bitmap.previous as isize == -1 {
                bitmap.previous = null_mut();
                return Err("無法初始化截圖緩衝區".into());
            }
            Ok(bitmap)
        }
    }

    #[cfg(test)]
    pub fn from_rgba(image: &RgbaImage) -> Result<Self, String> {
        let bitmap = Self::blank(image.width(), image.height())?;
        // SAFETY: no GDI drawing has used this new DIB yet; lengths match exactly.
        let target = unsafe { std::slice::from_raw_parts_mut(bitmap.pixels, image.as_raw().len()) };
        for (source, target) in image
            .as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .zip(target.as_chunks_mut::<4>().0)
        {
            *target = [source[2], source[1], source[0], 255];
        }
        Ok(bitmap)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn full(&self) -> CropRect {
        CropRect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        }
    }

    /// Converts `crop` to RGBA in one pass. GDI's alpha channel is undefined, so the
    /// result is opaque.
    pub fn to_rgba(&self, mut crop: CropRect) -> RgbaImage {
        crop.fit(self.width, self.height);
        let stride = self.width as usize * 4;
        let row_bytes = crop.width as usize * 4;
        // SAFETY: GdiFlush completes pending GDI drawing before the CPU reads the DIB,
        // whose length is exactly stride * height bytes.
        let source = unsafe {
            GdiFlush();
            std::slice::from_raw_parts(self.pixels, stride * self.height as usize)
        };
        let mut output = vec![0u8; row_bytes * crop.height as usize];
        for (row, target) in output.chunks_exact_mut(row_bytes).enumerate() {
            let start = (crop.y as usize + row) * stride + crop.x as usize * 4;
            for (source, target) in source[start..start + row_bytes]
                .as_chunks::<4>()
                .0
                .iter()
                .zip(target.as_chunks_mut::<4>().0)
            {
                *target = [source[2], source[1], source[0], 255];
            }
        }
        RgbaImage::from_raw(crop.width, crop.height, output)
            .expect("buffer length matches the crop dimensions")
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        // SAFETY: restore the original selection before destroying our owned bitmap and DC.
        unsafe {
            if !self.previous.is_null() {
                SelectObject(self.dc, self.previous);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            if !self.dc.is_null() {
                DeleteDC(self.dc);
            }
        }
    }
}

/// Copies the desktop straight into a DIB section, avoiding a device-dependent bitmap
/// and the GetDIBits copy.
pub fn capture_bitmap(bounds: DesktopRect) -> Result<Bitmap, String> {
    enable_dpi_awareness();
    let bitmap = Bitmap::blank(bounds.width, bounds.height)?;
    // SAFETY: the screen DC is released on every path; both DCs are valid for the copy.
    unsafe {
        let screen = GetDC(null_mut());
        if screen.is_null() {
            return Err("無法取得桌面裝置內容".into());
        }
        let copied = BitBlt(
            bitmap.dc,
            0,
            0,
            bounds.width as i32,
            bounds.height as i32,
            screen,
            bounds.x,
            bounds.y,
            SRCCOPY | CAPTUREBLT,
        );
        let copy_error = std::io::Error::last_os_error();
        ReleaseDC(null_mut(), screen);
        if copied == 0 {
            return Err(format!("擷取畫面失敗：{copy_error}"));
        }
    }
    Ok(bitmap)
}

pub fn capture(bounds: DesktopRect) -> Result<RgbaImage, String> {
    let bitmap = capture_bitmap(bounds)?;
    Ok(bitmap.to_rgba(bitmap.full()))
}

/// Asks the window to render itself with `PrintWindow(PW_RENDERFULLCONTENT)`, so the
/// result does not include windows covering it and is not clipped by the monitors.
/// `frame` is the visible frame (DWM extended bounds) to cut out of the full window
/// rectangle, which also contains the invisible resize borders.
///
/// Fails instead of returning a wrong image when the window is not responding, is
/// DPI-virtualized (it would render at its unscaled size), or renders all black
/// (some GPU-accelerated or protected content).
pub fn print_window(token: usize, frame: DesktopRect) -> Result<RgbaImage, String> {
    use windows_sys::Win32::{
        Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow},
        Storage::Xps::PrintWindow,
        UI::{
            HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI},
            WindowsAndMessaging::{IsHungAppWindow, PW_RENDERFULLCONTENT},
        },
    };

    enable_dpi_awareness();
    let hwnd = token as HWND;
    // SAFETY: Win32 validates the borrowed HWND, which may close at any time; all
    // output pointers are locals and the bitmap's DC outlives the PrintWindow call.
    unsafe {
        if IsHungAppWindow(hwnd) != 0 {
            return Err("視窗沒有回應，無法要求它繪製畫面".into());
        }
        let window_dpi = GetDpiForWindow(hwnd);
        let (mut monitor_dpi, mut unused) = (0, 0);
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if window_dpi == 0
            || GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut monitor_dpi, &mut unused) < 0
        {
            return Err("無法取得視窗的 DPI".into());
        }
        if window_dpi != monitor_dpi {
            return Err(format!(
                "視窗使用 DPI 虛擬化（視窗 {window_dpi} DPI、螢幕 {monitor_dpi} DPI），PrintWindow 會以未縮放的尺寸繪製"
            ));
        }
        let mut rect: RECT = zeroed();
        if GetWindowRect(hwnd, &raw mut rect) == 0 {
            return Err(format!(
                "無法取得視窗範圍：{}",
                std::io::Error::last_os_error()
            ));
        }
        let window = DesktopRect {
            x: rect.left,
            y: rect.top,
            width: (i64::from(rect.right) - i64::from(rect.left)).max(0) as u32,
            height: (i64::from(rect.bottom) - i64::from(rect.top)).max(0) as u32,
        };
        let frame = window.intersection(frame).unwrap_or(window);
        let bitmap = Bitmap::blank(window.width, window.height)?;
        if PrintWindow(hwnd, bitmap.dc, PW_RENDERFULLCONTENT) == 0 {
            return Err("PrintWindow 失敗".into());
        }
        let image = bitmap.to_rgba(CropRect {
            x: (frame.x - window.x) as u32,
            y: (frame.y - window.y) as u32,
            width: frame.width,
            height: frame.height,
        });
        if image
            .pixels()
            .all(|pixel| pixel[0] == 0 && pixel[1] == 0 && pixel[2] == 0)
        {
            return Err("PrintWindow 只取得全黑畫面（可能是 GPU 加速或受保護的內容）".into());
        }
        Ok(image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_round_trips_rgba_and_crops_rows() {
        let image = RgbaImage::from_fn(7, 5, |x, y| image::Rgba([x as u8, y as u8, 99, 17]));
        let bitmap = Bitmap::from_rgba(&image).unwrap();
        let opaque = RgbaImage::from_fn(7, 5, |x, y| image::Rgba([x as u8, y as u8, 99, 255]));
        assert_eq!(bitmap.to_rgba(bitmap.full()), opaque);
        let crop = CropRect {
            x: 2,
            y: 1,
            width: 4,
            height: 3,
        };
        assert_eq!(
            bitmap.to_rgba(crop),
            image::imageops::crop_imm(&opaque, 2, 1, 4, 3).to_image()
        );
    }

    #[test]
    fn bitmap_allocation_rejects_empty_or_oversized_images() {
        assert!(Bitmap::blank(0, 96).is_err());
        assert!(Bitmap::blank(128, 0).is_err());
        assert!(Bitmap::blank(32768, 32768).is_err());
        assert!(Bitmap::blank(u32::MAX, u32::MAX).is_err());
    }

    #[test]
    #[ignore = "requires an unlocked Windows desktop"]
    fn gdi_capture_returns_physical_pixels_with_opaque_alpha() {
        enable_dpi_awareness();
        let bounds = desktop_bounds().unwrap();
        let image = capture(bounds).unwrap();
        assert_eq!(image.dimensions(), (bounds.width, bounds.height));
        assert!(image.pixels().all(|pixel| pixel[3] == 255));
        let window = foreground_window();
        if window != 0 {
            let window = window_bounds(window).unwrap();
            assert_eq!(bounds.intersection(window), Some(window));
        }
    }
}
