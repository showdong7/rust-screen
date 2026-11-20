//! Monitor and top-level window discovery. All rectangles are physical desktop
//! pixels, so call `platform::enable_dpi_awareness` on the calling thread first.
use std::{
    mem::{size_of, zeroed},
    ptr::null_mut,
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, HWND, LPARAM, RECT},
    Graphics::{
        Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW},
    },
    System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    },
    UI::{
        HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
        WindowsAndMessaging::{
            EnumWindows, GWL_EXSTYLE, GetForegroundWindow, GetWindowLongW, GetWindowRect,
            GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
            IsWindowVisible, WS_EX_TOOLWINDOW,
        },
    },
};

use crate::geometry::DesktopRect;

const MONITORINFOF_PRIMARY: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitor {
    /// GDI device name such as `\\.\DISPLAY1`.
    pub name: String,
    pub bounds: DesktopRect,
    /// Desktop area excluding the taskbar and app bars.
    pub work_area: DesktopRect,
    pub primary: bool,
    /// Effective DPI; 96 means 100% scaling.
    pub dpi: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    /// The HWND as an opaque integer token; it may close at any time.
    pub handle: usize,
    pub title: String,
    pub process_id: u32,
    /// Executable file name, when the process allows querying it.
    pub process: Option<String>,
    /// Visible frame (without the invisible resize border); `None` when minimized.
    pub bounds: Option<DesktopRect>,
    pub minimized: bool,
    pub foreground: bool,
}

fn rect(value: RECT) -> Option<DesktopRect> {
    (value.right > value.left && value.bottom > value.top).then(|| DesktopRect {
        x: value.left,
        y: value.top,
        width: (i64::from(value.right) - i64::from(value.left)) as u32,
        height: (i64::from(value.bottom) - i64::from(value.top)) as u32,
    })
}

/// Monitors ordered left to right, then top to bottom.
pub fn monitors() -> Result<Vec<Monitor>, String> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> i32 {
        // SAFETY: `data` is the Vec passed to EnumDisplayMonitors below, alive for the call.
        let handles = unsafe { &mut *(data as *mut Vec<HMONITOR>) };
        handles.push(monitor);
        1
    }

    let mut handles: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only runs during this call and receives a pointer to `handles`.
    let ok = unsafe {
        EnumDisplayMonitors(
            null_mut(),
            null_mut(),
            Some(collect),
            (&raw mut handles) as LPARAM,
        )
    };
    if ok == 0 {
        return Err(format!("無法列舉螢幕：{}", std::io::Error::last_os_error()));
    }
    let mut monitors: Vec<Monitor> = handles
        .into_iter()
        .filter_map(|handle| {
            // SAFETY: MONITORINFOEXW starts with MONITORINFO and cbSize declares the larger size.
            unsafe {
                let mut info: MONITORINFOEXW = zeroed();
                info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
                if GetMonitorInfoW(handle, (&raw mut info).cast::<MONITORINFO>()) == 0 {
                    return None;
                }
                let (mut dpi_x, mut dpi_y) = (0, 0);
                let dpi =
                    if GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) >= 0 {
                        dpi_x
                    } else {
                        96
                    };
                let length = info.szDevice.iter().position(|&c| c == 0).unwrap_or(32);
                Some(Monitor {
                    name: String::from_utf16_lossy(&info.szDevice[..length]),
                    bounds: rect(info.monitorInfo.rcMonitor)?,
                    work_area: rect(info.monitorInfo.rcWork)?,
                    primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                    dpi,
                })
            }
        })
        .collect();
    if monitors.is_empty() {
        return Err("找不到可擷取的螢幕".into());
    }
    monitors.sort_by_key(|monitor| (monitor.bounds.x, monitor.bounds.y));
    Ok(monitors)
}

/// Effective DPI of the monitor that shows most of `area` (96 when none does). A
/// capture spanning monitors with different scaling follows the one it mostly covers.
pub fn dpi_for(monitors: &[Monitor], area: DesktopRect) -> u32 {
    monitors
        .iter()
        .filter_map(|monitor| {
            let shared = monitor.bounds.intersection(area)?;
            Some((
                u64::from(shared.width) * u64::from(shared.height),
                monitor.dpi,
            ))
        })
        .max_by_key(|(covered, _)| *covered)
        .map_or(96, |(_, dpi)| dpi)
}

/// The visible frame of a window, falling back to GetWindowRect when DWM has none.
pub fn frame_bounds(handle: usize) -> Option<DesktopRect> {
    let hwnd = handle as HWND;
    // SAFETY: Win32 validates this borrowed HWND; the output RECT is a local.
    unsafe {
        let mut value: RECT = zeroed();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&raw mut value).cast(),
            size_of::<RECT>() as u32,
        ) < 0
            && GetWindowRect(hwnd, &raw mut value) == 0
        {
            return None;
        }
        rect(value)
    }
}

pub fn is_window(handle: usize) -> bool {
    // SAFETY: IsWindow accepts any value and only reports whether it names a window.
    handle != 0 && unsafe { IsWindow(handle as HWND) } != 0
}

pub fn is_minimized(handle: usize) -> bool {
    // SAFETY: IsIconic validates the borrowed HWND.
    unsafe { IsIconic(handle as HWND) != 0 }
}

/// Title and process details for one window, if it is still listed by [`windows`].
pub fn window_info(handle: usize) -> Option<Window> {
    windows().into_iter().find(|window| window.handle == handle)
}

fn window_title(hwnd: HWND) -> String {
    // SAFETY: the buffer holds length + 1 UTF-16 units including the terminator.
    unsafe {
        let length = GetWindowTextLengthW(hwnd);
        if length <= 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let copied = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        String::from_utf16_lossy(&buffer[..copied.max(0) as usize])
    }
}

fn process_name(process_id: u32) -> Option<String> {
    // SAFETY: the process handle is closed on every path; the buffer length is passed in.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if process.is_null() {
            return None;
        }
        let mut buffer = [0u16; 1024];
        let mut length = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            buffer.as_mut_ptr(),
            &mut length,
        );
        CloseHandle(process);
        if ok == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buffer[..length as usize]);
        path.rsplit(['\\', '/']).next().map(str::to_owned)
    }
}

/// Visible, titled top-level windows in Z order (topmost first), as a user would see
/// them in Alt+Tab: tool windows and cloaked windows (other virtual desktops,
/// suspended UWP apps) are skipped.
pub fn windows() -> Vec<Window> {
    unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> i32 {
        // SAFETY: `data` is the Vec passed to EnumWindows below, alive for the call.
        let handles = unsafe { &mut *(data as *mut Vec<HWND>) };
        handles.push(hwnd);
        1
    }

    let mut handles: Vec<HWND> = Vec::new();
    // SAFETY: the callback only runs during this call and receives a pointer to `handles`.
    unsafe { EnumWindows(Some(collect), (&raw mut handles) as LPARAM) };
    // SAFETY: returns a borrowed handle or null.
    let foreground = unsafe { GetForegroundWindow() };
    handles
        .into_iter()
        .filter_map(|hwnd| {
            // SAFETY: every call validates the borrowed HWND, which may close concurrently.
            unsafe {
                if IsWindowVisible(hwnd) == 0
                    || GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW != 0
                {
                    return None;
                }
                let mut cloaked = 0u32;
                if DwmGetWindowAttribute(
                    hwnd,
                    DWMWA_CLOAKED as u32,
                    (&raw mut cloaked).cast(),
                    size_of::<u32>() as u32,
                ) >= 0
                    && cloaked != 0
                {
                    return None;
                }
                let title = window_title(hwnd);
                if title.trim().is_empty() {
                    return None;
                }
                let mut process_id = 0;
                GetWindowThreadProcessId(hwnd, &mut process_id);
                let minimized = IsIconic(hwnd) != 0;
                Some(Window {
                    handle: hwnd as usize,
                    title,
                    process_id,
                    process: process_name(process_id),
                    bounds: if minimized {
                        None
                    } else {
                        frame_bounds(hwnd as usize)
                    },
                    minimized,
                    foreground: hwnd == foreground,
                })
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_rejects_empty_and_keeps_negative_origins() {
        assert_eq!(
            rect(RECT {
                left: -1920,
                top: -200,
                right: 0,
                bottom: 880
            }),
            Some(DesktopRect {
                x: -1920,
                y: -200,
                width: 1920,
                height: 1080
            })
        );
        assert_eq!(
            rect(RECT {
                left: 10,
                top: 10,
                right: 10,
                bottom: 20
            }),
            None
        );
    }

    #[test]
    fn capture_dpi_follows_the_monitor_it_mostly_covers() {
        let monitor = |x: i32, dpi: u32| Monitor {
            name: String::new(),
            bounds: DesktopRect {
                x,
                y: 0,
                width: 1920,
                height: 1080,
            },
            work_area: DesktopRect {
                x,
                y: 0,
                width: 1920,
                height: 1040,
            },
            primary: x == 0,
            dpi,
        };
        let monitors = [monitor(-1920, 96), monitor(0, 144)];
        let area = |x: i32, width: u32| DesktopRect {
            x,
            y: 100,
            width,
            height: 400,
        };
        assert_eq!(dpi_for(&monitors, area(100, 500)), 144);
        assert_eq!(dpi_for(&monitors, area(-1000, 500)), 96);
        // Mostly on the left (100 %) monitor.
        assert_eq!(dpi_for(&monitors, area(-600, 700)), 96);
        // Off every monitor.
        assert_eq!(dpi_for(&monitors, area(5000, 100)), 96);
    }

    #[test]
    #[ignore = "requires an unlocked Windows desktop"]
    fn monitors_cover_the_virtual_desktop() {
        crate::platform::enable_dpi_awareness();
        let desktop = crate::platform::desktop_bounds().unwrap();
        let monitors = monitors().unwrap();
        assert_eq!(monitors.iter().filter(|monitor| monitor.primary).count(), 1);
        for monitor in &monitors {
            assert_eq!(desktop.intersection(monitor.bounds), Some(monitor.bounds));
        }
        assert!(windows().iter().all(|window| !window.title.is_empty()));
    }
}
