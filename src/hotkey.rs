use std::{
    ptr::null_mut,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
};

use windows_sys::Win32::{
    System::Threading::GetCurrentThreadId,
    UI::{
        Input::KeyboardAndMouse::{MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey},
        WindowsAndMessaging::{
            GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, WM_APP, WM_HOTKEY,
            WM_QUIT,
        },
    },
};

/// Posted to the worker thread whenever a command is queued on the channel.
const WAKE: u32 = WM_APP + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shortcut {
    modifiers: u32,
    key: u32,
}

impl Shortcut {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut modifiers = 0;
        let mut key = None;
        for part in text.split('+').map(|s| s.trim().to_ascii_uppercase()) {
            let modifier = match part.as_str() {
                "CTRL" | "CONTROL" => Some(2),
                "SHIFT" => Some(4),
                "ALT" => Some(1),
                "WIN" | "SUPER" => Some(8),
                _ => None,
            };
            if let Some(modifier) = modifier {
                if modifiers & modifier != 0 {
                    return Err("修飾鍵不可重複".into());
                }
                modifiers |= modifier;
                continue;
            }
            let value = if part.len() == 1 && part.as_bytes()[0].is_ascii_alphanumeric() {
                u32::from(part.as_bytes()[0])
            } else if part == "PRINTSCREEN" || part == "PRTSC" {
                0x2C
            } else if let Some(number) = part.strip_prefix('F') {
                let number: u32 = number.parse().map_err(|_| "功能鍵需為 F1～F24")?;
                if !(1..=24).contains(&number) {
                    return Err("功能鍵需為 F1～F24".into());
                }
                0x70 + number - 1
            } else {
                return Err("格式範例：Ctrl+Shift+S、Alt+F9、Ctrl+PrintScreen".into());
            };
            if key.replace(value).is_some() {
                return Err("請指定一個主鍵（A～Z、0～9、F1～F24 或 PrintScreen）".into());
            }
        }
        if modifiers == 0 {
            return Err("請加入 Ctrl、Shift、Alt 或 Win 至少一個修飾鍵".into());
        }
        Ok(Self {
            modifiers,
            key: key.ok_or("快捷鍵缺少主鍵")?,
        })
    }
}

struct Command {
    shortcut: Shortcut,
    reply: Sender<Result<(), String>>,
}

pub struct HotkeyService {
    commands: Sender<Command>,
    thread_id: u32,
    pub events: Receiver<usize>,
    worker: Option<JoinHandle<()>>,
}

impl HotkeyService {
    pub fn new(ctx: eframe::egui::Context, window: usize, hidden_in_tray: Arc<AtomicBool>) -> Self {
        let (commands, receiver) = mpsc::channel::<Command>();
        let (events_tx, events) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            // SAFETY: MSG is a plain output struct. PeekMessageW forces this thread's
            // message queue to exist before its id is published for PostThreadMessageW.
            unsafe {
                let mut message: MSG = std::mem::zeroed();
                PeekMessageW(&raw mut message, null_mut(), WM_APP, WM_APP, PM_NOREMOVE);
                let _ = ready_tx.send(GetCurrentThreadId());
            }
            let mut active: Option<(i32, Shortcut)> = None;
            // RegisterHotKey and its Win32 message queue must live on the same thread.
            // GetMessageW blocks, so the worker never wakes while the app is idle.
            loop {
                // SAFETY: MSG is a plain Win32 output struct; only this thread's queue is read.
                let message = unsafe {
                    let mut message: MSG = std::mem::zeroed();
                    if GetMessageW(&raw mut message, null_mut(), 0, 0) <= 0 {
                        break; // WM_QUIT from Drop, or an invalid queue.
                    }
                    message
                };
                match message.message {
                    WAKE => {
                        while let Ok(Command { shortcut, reply }) = receiver.try_recv() {
                            let _ = reply.send(register(&mut active, shortcut));
                        }
                    }
                    WM_HOTKEY if active.is_some_and(|(id, _)| message.wParam == id as usize) => {
                        // Snapshot the target before waking our own minimized UI.
                        let target = crate::platform::foreground_window();
                        let _ = events_tx.send(target);
                        if hidden_in_tray.swap(false, Ordering::AcqRel) {
                            crate::platform::show_window_without_activation(window);
                        } else {
                            crate::platform::wake_minimized_window(window);
                        }
                        ctx.request_repaint();
                    }
                    _ => {}
                }
            }
            if let Some((id, _)) = active {
                // SAFETY: release our remaining thread registration on shutdown.
                unsafe { UnregisterHotKey(null_mut(), id) };
            }
        });
        Self {
            commands,
            thread_id: ready_rx.recv().unwrap_or(0),
            events,
            worker: Some(worker),
        }
    }

    pub fn register(&self, text: &str) -> Result<(), String> {
        let shortcut = Shortcut::parse(text)?;
        let (reply, result) = mpsc::channel();
        self.commands
            .send(Command { shortcut, reply })
            .map_err(|_| "快捷鍵服務已停止".to_string())?;
        // SAFETY: posts a payload-free message to our own worker thread.
        if unsafe { PostThreadMessageW(self.thread_id, WAKE, 0, 0) } == 0 {
            return Err("快捷鍵服務已停止".into());
        }
        result
            .recv()
            .map_err(|_| "快捷鍵服務沒有回應".to_string())?
    }
}

/// Registers `shortcut` on the calling thread, keeping the previous one on failure.
fn register(active: &mut Option<(i32, Shortcut)>, shortcut: Shortcut) -> Result<(), String> {
    if active.is_some_and(|(_, current)| current == shortcut) {
        return Ok(());
    }
    let id = if active.is_some_and(|(id, _)| id == 1) {
        2
    } else {
        1
    };
    // SAFETY: NULL registers a thread hotkey; key and flags were validated.
    let registered = unsafe {
        RegisterHotKey(
            null_mut(),
            id,
            shortcut.modifiers | MOD_NOREPEAT,
            shortcut.key,
        )
    };
    if registered == 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!(
            "無法註冊快捷鍵，可能已被其他程式使用：{error}。原快捷鍵仍保留。"
        ));
    }
    if let Some((old_id, _)) = active.replace((id, shortcut)) {
        // SAFETY: the old registration belongs to this thread.
        unsafe { UnregisterHotKey(null_mut(), old_id) };
    }
    Ok(())
}

impl Drop for HotkeyService {
    fn drop(&mut self) {
        // SAFETY: WM_QUIT ends the worker's GetMessageW loop. If posting fails the
        // worker has already exited (or never started), so joining cannot block.
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_user_shortcut_and_aliases() {
        assert_eq!(
            Shortcut::parse("Ctrl + Shift + S").unwrap(),
            Shortcut {
                modifiers: 6,
                key: 0x53
            }
        );
        assert_eq!(
            Shortcut::parse(" alt + f24 ").unwrap(),
            Shortcut {
                modifiers: 1,
                key: 0x87
            }
        );
        assert_eq!(
            Shortcut::parse("Control+PrtSc"),
            Shortcut::parse("Ctrl+PrintScreen")
        );
    }

    #[test]
    fn rejects_ambiguous_or_unmodified_shortcuts() {
        for text in [
            "S",
            "Ctrl",
            "Ctrl++S",
            "Ctrl+Ctrl+S",
            "Ctrl+S+A",
            "Alt+F0",
            "Alt+F25",
        ] {
            assert!(Shortcut::parse(text).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn blocking_worker_publishes_its_queue_and_stops_on_drop() {
        // Registration itself is covered by the ignored desktop test below.
        let service = HotkeyService::new(eframe::egui::Context::default(), 0, Arc::default());
        assert_ne!(service.thread_id, 0);
        drop(service); // must post WM_QUIT and join without hanging
    }

    #[test]
    #[ignore = "temporarily registers Ctrl+Alt+Shift+F23/F24 on the Windows desktop"]
    fn registration_conflict_keeps_old_shortcut_and_releases_on_drop() {
        let first = HotkeyService::new(eframe::egui::Context::default(), 0, Arc::default());
        let second = HotkeyService::new(eframe::egui::Context::default(), 0, Arc::default());
        let third = HotkeyService::new(eframe::egui::Context::default(), 0, Arc::default());
        first.register("Ctrl+Alt+Shift+F23").unwrap();
        second.register("Ctrl+Alt+Shift+F24").unwrap();
        assert!(second.register("Ctrl+Alt+Shift+F23").is_err());
        assert!(third.register("Ctrl+Alt+Shift+F24").is_err());
        drop(first);
        second.register("Ctrl+Alt+Shift+F23").unwrap();
        third.register("Ctrl+Alt+Shift+F24").unwrap();
    }
}
