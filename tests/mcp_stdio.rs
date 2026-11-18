//! Drives the real `rust-screen-mcp` binary over stdio, the way an MCP client does.
//!
//! The protocol test needs no desktop. Tests that capture the screen are ignored by
//! default; run them on an unlocked Windows desktop with
//! `cargo test --test mcp_stdio -- --ignored --test-threads=1`.
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use base64::Engine as _;
use serde_json::{Value, json};

const REPLY_TIMEOUT: Duration = Duration::from_secs(20);

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Server {
    fn start(indicator: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rust-screen-mcp"));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if indicator {
            command.env_remove("RUST_SCREEN_MCP_INDICATOR");
        } else {
            command.env("RUST_SCREEN_MCP_INDICATOR", "0");
        }
        let mut child = command.spawn().expect("start rust-screen-mcp");
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut server = Self {
            stdin: child.stdin.take(),
            child,
            lines,
            next_id: 1,
        };
        let reply = server.request(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": { "name": "integration-test", "version": "0" } }),
        );
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(reply["result"]["serverInfo"]["name"], "rust-screen");
        server.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        server
    }

    fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{line}").expect("write request");
        stdin.flush().expect("flush request");
    }

    fn receive(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(REPLY_TIMEOUT)
            .expect("the server replies in time");
        serde_json::from_str(&line).unwrap_or_else(|error| {
            panic!("stdout must carry only JSON-RPC, got {line:?}: {error}")
        })
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send_raw(
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
        );
        let reply = self.receive();
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], id);
        reply
    }

    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        reply["result"].clone()
    }

    /// Closes stdin like a disconnecting client and returns the exit code.
    fn close(mut self) -> Option<i32> {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("query server status") {
                return status.code();
            }
            thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text(result: &Value, index: usize) -> &str {
    result["content"][index]["text"]
        .as_str()
        .expect("text content")
}

/// The JSON details block that ends every capture result.
fn details(result: &Value) -> Value {
    let content = result["content"].as_array().expect("content array");
    serde_json::from_str(content.last().unwrap()["text"].as_str().unwrap()).unwrap()
}

fn image(result: &Value) -> image::RgbaImage {
    assert_eq!(result["content"][0]["type"], "image");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(result["content"][0]["data"].as_str().unwrap())
        .unwrap();
    image::load_from_memory(&bytes).unwrap().to_rgba8()
}

#[test]
fn protocol_round_trip_and_errors_over_stdio() {
    let mut server = Server::start(false);

    assert_eq!(server.request("ping", json!({}))["result"], json!({}));

    let tools = server.request("tools/list", json!({}));
    let tools = tools["result"]["tools"].as_array().unwrap();
    let names: Vec<_> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "list_monitors",
            "list_windows",
            "capture_screen",
            "capture_window",
            "capture_region",
            "select_region"
        ]
    );
    let window = &tools[3]["inputSchema"]["properties"];
    assert_eq!(
        window["method"]["enum"],
        json!(["auto", "screen", "print_window"])
    );

    server.send_raw("{not json");
    let reply = server.receive();
    assert_eq!(reply["error"]["code"], -32700);
    assert_eq!(reply["id"], Value::Null);

    let reply = server.request("resources/list", json!({}));
    assert_eq!(reply["error"]["code"], -32601);
    let reply = server.request("tools/call", json!({ "name": "nope", "arguments": {} }));
    assert_eq!(reply["error"]["code"], -32602);

    // Bad arguments are tool errors the model can read, not protocol errors.
    let result = server.call("capture_region", json!({ "x": 0 }));
    assert_eq!(result["isError"], true);
    assert!(text(&result, 0).contains("missing field"));
    let result = server.call("capture_window", json!({ "method": "magic" }));
    assert_eq!(result["isError"], true);

    assert_eq!(
        server.close(),
        Some(0),
        "the server exits when stdin closes"
    );
}

#[test]
#[ignore = "requires an unlocked Windows desktop"]
fn captures_screen_region_and_windows_over_stdio() {
    let mut server = Server::start(false);
    let monitors = server.call("list_monitors", json!({}));
    let monitors: Value = serde_json::from_str(text(&monitors, 0)).unwrap();
    let primary = monitors["monitors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|monitor| monitor["primary"] == true)
        .expect("a primary monitor");

    let region = json!({ "x": primary["x"], "y": primary["y"], "width": 64, "height": 48,
                         "max_dimension": 0 });
    let result = server.call("capture_region", region);
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(result["content"][0]["mimeType"], "image/png");
    assert_eq!(image(&result).dimensions(), (64, 48));
    assert_eq!(details(&result)["scale"], 1.0);

    let result = server.call(
        "capture_screen",
        json!({ "format": "jpeg", "max_dimension": 320 }),
    );
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(result["content"][0]["mimeType"], "image/jpeg");
    let returned = details(&result)["returned_size"].clone();
    let decoded = image(&result);
    assert_eq!(json!([decoded.width(), decoded.height()]), returned);
    assert!(decoded.width().max(decoded.height()) <= 320);

    let result = server.call("capture_window", json!({ "max_dimension": 200 }));
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(
        details(&result)["method"],
        "screen",
        "foreground uses the screen"
    );

    let windows = server.call("list_windows", json!({}));
    let windows: Value = serde_json::from_str(text(&windows, 0)).unwrap();
    if let Some(background) = windows["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|window| window["minimized"] == false && window["foreground"] == false)
    {
        let result = server.call(
            "capture_window",
            json!({ "handle": background["handle"], "max_dimension": 200 }),
        );
        assert_eq!(result["isError"], false, "{result}");
        let details = details(&result);
        assert!(
            details["method"] == "print_window" || details.get("fallback").is_some(),
            "background windows render themselves or explain the fallback: {details}"
        );
    }
    assert_eq!(server.close(), Some(0));
}

/// Win32 view of the capture indicator, as another process sees it.
mod indicator {
    use std::ptr::null;

    use windows_sys::Win32::{
        Foundation::{HWND, RECT},
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
            WindowsAndMessaging::{
                FindWindowW, GetForegroundWindow, GetWindowDisplayAffinity, GetWindowRect,
                IsWindowVisible, WDA_EXCLUDEFROMCAPTURE,
            },
        },
    };

    pub struct Seen {
        pub visible: bool,
        pub excluded_from_capture: bool,
        pub foreground: bool,
        pub rect: [i32; 4],
    }

    fn find() -> HWND {
        let class: Vec<u16> = "RustScreenMcpCaptureIndicator"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // SAFETY: the class name is NUL-terminated and outlives the call.
        unsafe { FindWindowW(class.as_ptr(), null()) }
    }

    pub fn inspect() -> Option<Seen> {
        // SAFETY: every call validates the borrowed HWND; outputs are locals.
        unsafe {
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let hwnd = find();
            if hwnd.is_null() {
                return None;
            }
            let mut affinity = 0;
            GetWindowDisplayAffinity(hwnd, &mut affinity);
            let mut rect: RECT = std::mem::zeroed();
            GetWindowRect(hwnd, &mut rect);
            Some(Seen {
                visible: IsWindowVisible(hwnd) != 0,
                excluded_from_capture: affinity == WDA_EXCLUDEFROMCAPTURE,
                foreground: GetForegroundWindow() == hwnd,
                rect: [rect.left, rect.top, rect.right, rect.bottom],
            })
        }
    }

    pub fn exists() -> bool {
        !find().is_null()
    }
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
#[ignore = "requires an unlocked Windows desktop; briefly flashes a frame on screen"]
fn capture_indicator_is_visible_but_never_captured() {
    assert!(
        wait_until(Duration::from_secs(3), || !indicator::exists()),
        "no indicator left over from another process"
    );
    let mut server = Server::start(true);
    let monitors = server.call("list_monitors", json!({}));
    let monitors: Value = serde_json::from_str(text(&monitors, 0)).unwrap();
    let primary = monitors["monitors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|monitor| monitor["primary"] == true)
        .unwrap()
        .clone();
    let (x, y) = (
        primary["x"].as_i64().unwrap() as i32 + 100,
        primary["y"].as_i64().unwrap() as i32 + 100,
    );
    let region = json!({ "x": x, "y": y, "width": 320, "height": 240, "max_dimension": 0 });

    let first = server.call("capture_region", region.clone());
    assert_eq!(first["isError"], false, "{first}");
    let mut seen = None;
    assert!(
        wait_until(Duration::from_secs(2), || {
            seen = indicator::inspect();
            seen.as_ref().is_some_and(|seen| seen.visible)
        }),
        "the indicator appears after a capture"
    );
    let seen = seen.unwrap();
    assert!(seen.excluded_from_capture, "WDA_EXCLUDEFROMCAPTURE is set");
    assert!(!seen.foreground, "the indicator never takes focus");
    assert_eq!(seen.rect, [x, y, x + 320, y + 240]);

    // Capture the same area while the frame is on screen: its colour must not show up.
    let second = server.call("capture_region", region);
    assert_eq!(second["isError"], false, "{second}");
    let pixels = image(&second);
    let border: Vec<_> = pixels
        .enumerate_pixels()
        .filter(|(px, py, _)| *px < 6 || *py < 6 || *px >= 314 || *py >= 234)
        .collect();
    let orange = border
        .iter()
        .filter(|(_, _, pixel)| pixel.0 == [255, 122, 36, 255])
        .count();
    assert!(
        orange * 10 < border.len(),
        "{orange} of {} border pixels have the indicator colour",
        border.len()
    );
    assert!(
        wait_until(Duration::from_secs(3), || !indicator::exists()),
        "the indicator disappears on its own"
    );
    assert_eq!(server.close(), Some(0));

    // RUST_SCREEN_MCP_INDICATOR=0 turns it off.
    let mut server = Server::start(false);
    let result = server.call(
        "capture_region",
        json!({ "x": x, "y": y, "width": 32, "height": 32 }),
    );
    assert_eq!(result["isError"], false);
    assert!(
        !wait_until(Duration::from_millis(500), indicator::exists),
        "no indicator when disabled"
    );
    assert_eq!(server.close(), Some(0));
}
