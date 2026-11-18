//! Notification icon and its Win32 message loop stay alive when the GUI is hidden.
use std::{
    mem::{size_of, zeroed},
    ptr::null_mut,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{Shell::*, WindowsAndMessaging::*},
};

use crate::platform::{register_class, wide};

const ICON_EVENT: u32 = WM_APP + 7;
const OPEN: usize = 1;
const EXIT: usize = 2;

#[derive(Debug, PartialEq, Eq)]
pub enum TrayEvent {
    Open,
    Exit,
    Error(String),
}

pub struct TrayService {
    window: usize,
    pub events: Receiver<TrayEvent>,
    worker: Option<JoinHandle<()>>,
}

impl TrayService {
    pub fn new(
        ctx: eframe::egui::Context,
        main_window: usize,
        hidden: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let (sender, events) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            if let Err(error) = run(ctx, main_window, hidden, sender, &ready_tx) {
                let _ = ready_tx.send(Err(error));
            }
        });
        match ready_rx.recv() {
            Ok(Ok(window)) => Ok(Self {
                window,
                events,
                worker: Some(worker),
            }),
            result => {
                let _ = worker.join();
                Err(match result {
                    Ok(Err(error)) => error,
                    _ => "系統匣服務啟動失敗".into(),
                })
            }
        }
    }
}

impl Drop for TrayService {
    fn drop(&mut self) {
        // SAFETY: this borrowed HWND belongs to our worker. The worker removes its icon
        // and destroys the window before join returns.
        unsafe { PostMessageW(self.window as HWND, WM_CLOSE, 0, 0) };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct State {
    ctx: eframe::egui::Context,
    main_window: usize,
    hidden: Arc<AtomicBool>,
    events: Sender<TrayEvent>,
    data: NOTIFYICONDATAW,
    taskbar_created: u32,
    stop: bool,
}

impl State {
    fn emit(&self, event: TrayEvent) {
        self.hidden.store(false, Ordering::Release);
        let _ = self.events.send(event);
        // A hidden winit window cannot process egui repaint requests on its own.
        crate::platform::show_window_without_activation(self.main_window);
        self.ctx.request_repaint();
    }

    fn add_icon(&mut self) -> Result<(), String> {
        // SAFETY: NOTIFYICONDATAW and HICON remain owned by State for the icon's lifetime.
        unsafe {
            if Shell_NotifyIconW(NIM_ADD, &raw const self.data) == 0 {
                return Err("無法建立系統匣圖示，請確認 Windows 工作列正在執行".into());
            }
            self.data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            if Shell_NotifyIconW(NIM_SETVERSION, &raw const self.data) == 0 {
                Shell_NotifyIconW(NIM_DELETE, &raw const self.data);
                return Err("無法啟用系統匣互動功能".into());
            }
        }
        Ok(())
    }
}

fn menu(hwnd: HWND) {
    // SAFETY: menu strings are copied by AppendMenuW. The menu is destroyed after
    // TrackPopupMenu returns, and no mutable State reference spans its nested message loop.
    unsafe {
        let handle = CreatePopupMenu();
        if handle.is_null() {
            return;
        }
        let open = wide("開啟主視窗");
        let exit = wide("退出程式");
        AppendMenuW(handle, MF_STRING, OPEN, open.as_ptr());
        AppendMenuW(handle, MF_SEPARATOR, 0, null_mut());
        AppendMenuW(handle, MF_STRING, EXIT, exit.as_ptr());
        SetMenuDefaultItem(handle, OPEN as u32, 0);
        let mut point: POINT = zeroed();
        GetCursorPos(&raw mut point);
        SetForegroundWindow(hwnd);
        let choice = TrackPopupMenu(
            handle,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            hwnd,
            null_mut(),
        );
        DestroyMenu(handle);
        PostMessageW(hwnd, WM_NULL, 0, 0);
        if choice != 0 {
            PostMessageW(hwnd, WM_COMMAND, choice as usize, 0);
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: userdata is a raw Box allocation owned by run(), destroyed only after HWND.
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
        // TrackPopupMenu runs a nested message loop, so obtain State only afterwards.
        if message == ICON_EVENT && matches!(lparam as u32 & 0xffff, WM_CONTEXTMENU | WM_RBUTTONUP)
        {
            menu(hwnd);
            return 0;
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        if state.is_null() {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        if message == (*state).taskbar_created {
            if let Err(error) = (*state).add_icon() {
                (*state).emit(TrayEvent::Error(error));
            }
            return 0;
        }
        match message {
            ICON_EVENT => match lparam as u32 & 0xffff {
                NIN_SELECT | WM_LBUTTONDBLCLK | WM_LBUTTONUP | 0x401 => {
                    (*state).emit(TrayEvent::Open)
                }
                _ => {}
            },
            WM_COMMAND => match wparam & 0xffff {
                OPEN => (*state).emit(TrayEvent::Open),
                EXIT => (*state).emit(TrayEvent::Exit),
                _ => {}
            },
            WM_CLOSE => {
                (*state).stop = true;
                EndMenu();
            }
            WM_DESTROY => (*state).stop = true,
            _ => return DefWindowProcW(hwnd, message, wparam, lparam),
        }
        0
    }
}

struct OwnedState(*mut State);
impl Drop for OwnedState {
    fn drop(&mut self) {
        // SAFETY: OwnedWindow already removed the notification and destroyed the window.
        // Release the owned icon; State's channels and egui context are then dropped as well.
        unsafe {
            let state = Box::from_raw(self.0);
            DestroyIcon(state.data.hIcon);
        }
    }
}

struct OwnedWindow(HWND);
impl Drop for OwnedWindow {
    fn drop(&mut self) {
        // SAFETY: the worker owns this window and its State still exists.
        unsafe {
            let state = GetWindowLongPtrW(self.0, GWLP_USERDATA) as *mut State;
            if !state.is_null() {
                Shell_NotifyIconW(NIM_DELETE, &raw const (*state).data);
            }
            DestroyWindow(self.0);
        }
    }
}

fn run(
    ctx: eframe::egui::Context,
    main_window: usize,
    hidden: Arc<AtomicBool>,
    events: Sender<TrayEvent>,
    ready: &Sender<Result<usize, String>>,
) -> Result<(), String> {
    static CLASS: OnceLock<Result<u16, String>> = OnceLock::new();
    crate::platform::enable_dpi_awareness();
    // SAFETY: resources stay on this worker thread. Window is dropped before its State.
    unsafe {
        let instance = GetModuleHandleW(null_mut());
        let atom = CLASS
            .get_or_init(|| {
                register_class("RustScreenTrayService", Some(window_proc), null_mut())
                    .map_err(|error| format!("無法初始化系統匣：{error}"))
            })
            .as_ref()
            .map_err(Clone::clone)?;
        let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        if taskbar_created == 0 {
            return Err("無法註冊工作列重新啟動通知".into());
        }
        // Load the same multi-resolution resource shown by Explorer. Without
        // LR_SHARED this handle is owned by State and released by OwnedState.
        let icon = LoadImageW(
            instance,
            std::ptr::without_provenance(1), // MAKEINTRESOURCEW(1): integer resource ID.
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        );
        if icon.is_null() {
            return Err("無法建立系統匣圖示影像".into());
        }
        let mut data: NOTIFYICONDATAW = zeroed();
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.uID = 1;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = ICON_EVENT;
        data.hIcon = icon;
        let tip = wide("Rust Screen｜點擊開啟，右鍵選單退出");
        data.szTip[..tip.len()].copy_from_slice(&tip);
        let state = Box::into_raw(Box::new(State {
            ctx,
            main_window,
            hidden,
            events,
            data,
            taskbar_created,
            stop: false,
        }));
        let _owner = OwnedState(state);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            *atom as usize as *const u16,
            wide("Rust Screen Tray").as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            state.cast(),
        );
        if hwnd.is_null() {
            return Err("無法建立系統匣訊息視窗".into());
        }
        let _window = OwnedWindow(hwnd);
        (*state).data.hWnd = hwnd;
        (*state).add_icon()?;
        let _ = ready.send(Ok(hwnd as usize));
        let mut message: MSG = zeroed();
        loop {
            if (*state).stop {
                break;
            }
            let result = GetMessageW(&raw mut message, null_mut(), 0, 0);
            if result <= 0 {
                (*state).emit(TrayEvent::Error(
                    "系統匣服務已停止，關閉視窗將直接退出程式".into(),
                ));
                break;
            }
            TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    #[ignore = "requires a running Windows notification area"]
    fn tray_service_dispatches_open_exit_and_shuts_down() {
        let hidden = Arc::new(AtomicBool::new(true));
        let tray = TrayService::new(eframe::egui::Context::default(), 0, hidden.clone()).unwrap();
        // SAFETY: exercise commands on our own test service, without injecting user input.
        unsafe { PostMessageW(tray.window as HWND, WM_COMMAND, OPEN, 0) };
        assert_eq!(
            tray.events.recv_timeout(Duration::from_secs(2)).unwrap(),
            TrayEvent::Open
        );
        assert!(!hidden.load(Ordering::Acquire));
        unsafe { PostMessageW(tray.window as HWND, WM_COMMAND, EXIT, 0) };
        assert_eq!(
            tray.events.recv_timeout(Duration::from_secs(2)).unwrap(),
            TrayEvent::Exit
        );
        drop(tray); // joins the worker after removing the icon
    }
}
