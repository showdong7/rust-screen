//! Session-local instance guard and an IPC window independent of the notification area.
use std::{
    io,
    mem::zeroed,
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
    },
    System::{LibraryLoader::GetModuleHandleW, Threading::CreateMutexW},
    UI::WindowsAndMessaging::*,
};

use crate::platform::{register_class, wide};

const INSTANCE_NAME: &str = "Local\\RustScreen.SingleInstance.v1";
const WINDOW_CLASS: &str = "RustScreenSingleInstanceService.v1";
const ACTIVATE: u32 = WM_APP + 1;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct Target {
    ctx: eframe::egui::Context,
    window: usize,
    hidden: Arc<AtomicBool>,
}

impl Target {
    fn wake(&self) {
        self.hidden.store(false, Ordering::Release);
        // Hidden winit windows need a native show before they can process repaint events.
        crate::platform::show_window_without_activation(self.window);
        self.ctx.request_repaint();
    }
}

#[derive(Default)]
pub struct Activation {
    requested: AtomicBool,
    target: Mutex<Option<Target>>,
}

impl Activation {
    pub fn bind(&self, ctx: eframe::egui::Context, window: usize, hidden: Arc<AtomicBool>) {
        let target = Target {
            ctx,
            window,
            hidden,
        };
        *self.target.lock().unwrap() = Some(target.clone());
        // A second launch may arrive before eframe has created the main window.
        if self.requested.load(Ordering::Acquire) {
            target.wake();
        }
    }

    pub fn take_requested(&self) -> bool {
        self.requested.swap(false, Ordering::AcqRel)
    }

    fn request(&self) {
        self.requested.store(true, Ordering::Release);
        let target = self.target.lock().unwrap().clone();
        if let Some(target) = target {
            target.wake();
        }
    }
}

struct NamedMutex(HANDLE);

impl NamedMutex {
    fn create(name: &[u16]) -> io::Result<(Self, bool)> {
        // SAFETY: name is NUL-terminated. No ownership is taken: existence of the
        // kernel object, held until the UI and IPC worker exit, is the instance lock.
        unsafe {
            let handle = CreateMutexW(null(), 0, name.as_ptr());
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let existing = GetLastError() == ERROR_ALREADY_EXISTS;
            Ok((Self(handle), existing))
        }
    }
}

impl Drop for NamedMutex {
    fn drop(&mut self) {
        // SAFETY: this handle is owned here and is never inherited by a child process.
        unsafe { CloseHandle(self.0) };
    }
}

pub struct SingleInstance {
    window: usize,
    worker: Option<JoinHandle<()>>,
    pub activation: Arc<Activation>,
    // Dropped after the worker has destroyed its IPC window.
    _mutex: NamedMutex,
}

impl SingleInstance {
    /// None means the existing instance acknowledged our activation request.
    pub fn acquire() -> io::Result<Option<Self>> {
        Self::acquire_named(INSTANCE_NAME)
    }

    fn acquire_named(name: &str) -> io::Result<Option<Self>> {
        let name = wide(name);
        let started = Instant::now();
        loop {
            let (mutex, existing) = NamedMutex::create(&name)?;
            if !existing {
                let activation = Arc::new(Activation::default());
                let state = activation.clone();
                let (ready_tx, ready_rx) = mpsc::channel();
                let worker = thread::Builder::new()
                    .name("single-instance".into())
                    .spawn(move || {
                        if let Err(error) = run(name, state, &ready_tx) {
                            let _ = ready_tx.send(Err(error));
                        }
                    })?;
                return match ready_rx.recv() {
                    Ok(Ok(window)) => Ok(Some(Self {
                        window,
                        worker: Some(worker),
                        activation,
                        _mutex: mutex,
                    })),
                    result => {
                        let _ = worker.join();
                        Err(match result {
                            Ok(Err(error)) => error,
                            _ => io::Error::other("無法啟動單一執行個體服務"),
                        })
                    }
                };
            }
            // Do not keep the old mutex alive while retrying: the first process may
            // exit during startup, in which case a retry can become the new primary.
            drop(mutex);
            if notify_existing(&name)? {
                return Ok(None);
            }
            if started.elapsed() >= STARTUP_TIMEOUT {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "程式已在執行，但尚未能連線喚回主視窗，請稍後再試",
                ));
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: the worker owns this window and processes WM_CLOSE without the UI.
        unsafe { PostMessageW(self.window as HWND, WM_CLOSE, 0, 0) };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn notify_existing(name: &[u16]) -> io::Result<bool> {
    // SAFETY: discovery uses a fixed class and exact instance name, never a UI title.
    // The custom message carries no pointers across process boundaries.
    unsafe {
        let window = FindWindowExW(
            HWND_MESSAGE,
            null_mut(),
            wide(WINDOW_CLASS).as_ptr(),
            name.as_ptr(),
        );
        if window.is_null() {
            return Ok(false);
        }
        let mut process = 0;
        if GetWindowThreadProcessId(window, &raw mut process) == 0 {
            return Ok(false);
        }
        // A launch from Explorer can transfer its foreground permission to the primary.
        // Windows may deny permission for background launches; visibility still works.
        AllowSetForegroundWindow(process);
        let mut acknowledged = 0;
        if SendMessageTimeoutW(
            window,
            ACTIVATE,
            0,
            0,
            SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
            2000,
            &raw mut acknowledged,
        ) == 0
            || acknowledged != 1
        {
            return Err(io::Error::other(
                "程式已在執行，但未回應喚回通知，請稍後再試",
            ));
        }
        Ok(true)
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: run() retains the Arc until after the window is destroyed. The shared
    // state uses atomics/a mutex; no mutable reference spans a native API call.
    unsafe {
        match message {
            WM_NCCREATE => {
                let create = &*(lparam as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            ACTIVATE => {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Activation;
                if !state.is_null() {
                    (*state).request();
                    return 1;
                }
            }
            WM_CLOSE => {
                PostQuitMessage(0);
                return 0;
            }
            _ => {}
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

struct OwnedWindow(HWND);

impl Drop for OwnedWindow {
    fn drop(&mut self) {
        // SAFETY: destruction runs on the owning thread while Activation is alive.
        unsafe { DestroyWindow(self.0) };
    }
}

fn run(
    name: Vec<u16>,
    activation: Arc<Activation>,
    ready: &Sender<io::Result<usize>>,
) -> io::Result<()> {
    static CLASS: OnceLock<Result<u16, i32>> = OnceLock::new();
    // SAFETY: the message-only window and message loop live entirely on this worker.
    unsafe {
        let instance = GetModuleHandleW(null());
        let atom = CLASS
            .get_or_init(|| {
                register_class(WINDOW_CLASS, Some(window_proc), null_mut())
                    .map_err(|error| error.raw_os_error().unwrap_or_default())
            })
            .as_ref()
            .map_err(|code| io::Error::from_raw_os_error(*code))?;
        let window = CreateWindowExW(
            0,
            *atom as usize as *const u16,
            name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance,
            Arc::as_ptr(&activation).cast(),
        );
        if window.is_null() {
            return Err(io::Error::last_os_error());
        }
        let _window = OwnedWindow(window);
        let _ = ready.send(Ok(window as usize));
        let mut message: MSG = zeroed();
        loop {
            let result = GetMessageW(&raw mut message, null_mut(), 0, 0);
            if result == -1 {
                return Err(io::Error::last_os_error());
            }
            if result == 0 {
                return Ok(());
            }
            TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::Command, sync::atomic::AtomicU64};

    fn unique_name() -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        format!(
            "Local\\RustScreen.Test.{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn secondary_process() {
        let Ok(name) = std::env::var("RUST_SCREEN_TEST_INSTANCE") else {
            return;
        };
        assert!(SingleInstance::acquire_named(&name).unwrap().is_none());
    }

    #[test]
    fn second_process_activates_before_ui_binding_and_exit_releases_instance() {
        let name = unique_name();
        let first = SingleInstance::acquire_named(&name).unwrap().unwrap();
        assert!(!first.activation.take_requested());
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "single_instance::tests::secondary_process",
                "--nocapture",
            ])
            .env("RUST_SCREEN_TEST_INSTANCE", &name)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(first.activation.take_requested());
        assert!(!first.activation.take_requested());
        drop(first);
        assert!(SingleInstance::acquire_named(&name).unwrap().is_some());
    }

    #[test]
    fn simultaneous_launches_elect_one_primary() {
        let name = unique_name();
        let barrier = Arc::new(std::sync::Barrier::new(6));
        let (sender, receiver) = mpsc::channel();
        let workers: Vec<_> = (0..6)
            .map(|_| {
                let name = name.clone();
                let barrier = barrier.clone();
                let sender = sender.clone();
                thread::spawn(move || {
                    barrier.wait();
                    let guard = SingleInstance::acquire_named(&name).unwrap();
                    sender.send(guard.is_some()).unwrap();
                    // Retain the primary until every competitor has finished acquiring.
                    barrier.wait();
                    if let Some(guard) = guard {
                        assert!(guard.activation.take_requested());
                    }
                })
            })
            .collect();
        assert_eq!(
            (0..6)
                .filter(|_| receiver.recv_timeout(Duration::from_secs(10)).unwrap())
                .count(),
            1
        );
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
