//! Model Context Protocol server for Rust Screen over stdio.
//!
//! Messages are newline-delimited JSON-RPC 2.0 on stdin/stdout; stdout carries
//! nothing else, and logs go to stderr. Tool calls run on worker threads so a
//! long interactive selection never blocks `ping` or other requests.
//!
//! This binary is independent of the GUI: it never takes the single-instance lock,
//! so it works whether or not the Rust Screen window is running.

mod indicator;
mod tools;

use std::{
    io::{self, BufRead, Write},
    sync::{Arc, Mutex},
    thread,
};

use serde_json::{Value, json};

/// Newest first. An unknown client version falls back to the newest.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const INSTRUCTIONS: &str = "Screenshots of the local Windows desktop. All coordinates are physical \
desktop pixels; secondary monitors can have negative origins. Call list_monitors or list_windows \
first to find a target, then capture_screen, capture_window or capture_region. Images are \
downscaled to max_dimension by default; each result's text block gives the captured desktop_rect \
and scale, so an image point maps back to desktop_rect.x + image_x / scale. To read small text, \
capture_region a smaller area instead of raising max_dimension. select_region asks the user to \
drag an area on screen and waits until they finish or cancel.";

/// What to send back for one incoming message.
enum Reply {
    None,
    Now(Value),
    /// Runs on a worker thread; produces the full response message.
    Later(Box<dyn FnOnce() -> Value + Send>),
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .and_then(|requested| PROTOCOL_VERSIONS.iter().find(|&&v| v == requested))
        .unwrap_or(&PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "rust-screen",
            "title": "Rust Screen",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

fn handle(message: Value) -> Reply {
    let Some(object) = message.as_object() else {
        return Reply::Now(failure(
            Value::Null,
            INVALID_REQUEST,
            "Expected a single JSON-RPC object",
        ));
    };
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        // A response to a server request; this server never sends any.
        return Reply::None;
    };
    let Some(id) = object.get("id").cloned() else {
        // Notifications (initialized, cancelled, …) need no reply.
        return Reply::None;
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => Reply::Now(success(id, initialize(&params))),
        "ping" => Reply::Now(success(id, json!({}))),
        "tools/list" => Reply::Now(success(id, json!({ "tools": tools::definitions() }))),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Reply::Now(failure(
                    id,
                    INVALID_PARAMS,
                    "tools/call requires a tool name",
                ));
            };
            let Some(tool) = tools::find(name) else {
                return Reply::Now(failure(id, INVALID_PARAMS, format!("Unknown tool: {name}")));
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            Reply::Later(Box::new(move || success(id, tools::call(tool, arguments))))
        }
        _ => Reply::Now(failure(
            id,
            METHOD_NOT_FOUND,
            format!("Method not found: {method}"),
        )),
    }
}

fn send(output: &Mutex<io::Stdout>, message: &Value) {
    let mut line = serde_json::to_vec(message).expect("JSON values always serialize");
    line.push(b'\n');
    let mut output = output.lock().unwrap_or_else(|poison| poison.into_inner());
    // A closed stdout means the client is gone; the stdin loop will end next.
    let _ = output.write_all(&line).and_then(|()| output.flush());
}

fn main() {
    rust_screen::platform::enable_dpi_awareness();
    eprintln!(
        "rust-screen-mcp {} ready on stdio",
        env!("CARGO_PKG_VERSION")
    );
    let output = Arc::new(Mutex::new(io::stdout()));
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(message),
            Err(error) => Reply::Now(failure(
                Value::Null,
                PARSE_ERROR,
                format!("Parse error: {error}"),
            )),
        };
        match reply {
            Reply::None => {}
            Reply::Now(response) => send(&output, &response),
            Reply::Later(work) => {
                let output = Arc::clone(&output);
                thread::spawn(move || {
                    rust_screen::platform::enable_dpi_awareness();
                    let response = work();
                    send(&output, &response);
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now(message: Value) -> Value {
        match handle(message) {
            Reply::Now(value) => value,
            Reply::None => panic!("expected an immediate reply, got none"),
            Reply::Later(_) => panic!("expected an immediate reply, got deferred work"),
        }
    }

    #[test]
    fn initialize_negotiates_known_versions_and_falls_back_to_newest() {
        let reply = now(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-03-26", "capabilities": {},
                        "clientInfo": { "name": "test", "version": "0" } }
        }));
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(reply["result"]["serverInfo"]["name"], "rust-screen");
        assert!(reply["result"]["capabilities"]["tools"].is_object());

        let reply = now(json!({
            "jsonrpc": "2.0", "id": "a", "method": "initialize",
            "params": { "protocolVersion": "1999-01-01" }
        }));
        assert_eq!(reply["id"], "a");
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn notifications_and_responses_get_no_reply() {
        assert!(matches!(
            handle(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })),
            Reply::None
        ));
        assert!(matches!(
            handle(json!({ "jsonrpc": "2.0", "id": 9, "result": {} })),
            Reply::None
        ));
    }

    #[test]
    fn protocol_errors_use_json_rpc_codes() {
        assert_eq!(now(json!([1, 2]))["error"]["code"], INVALID_REQUEST);
        let reply = now(json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list" }));
        assert_eq!(reply["error"]["code"], METHOD_NOT_FOUND);
        let reply = now(json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "format_disk", "arguments": {} }
        }));
        assert_eq!(reply["error"]["code"], INVALID_PARAMS);
        assert_eq!(reply["id"], 3);
    }

    #[test]
    fn ping_and_tool_listing() {
        assert_eq!(
            now(json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" }))["result"],
            json!({})
        );
        let reply = now(json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" }));
        let names: Vec<_> = reply["result"]["tools"]
            .as_array()
            .unwrap()
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
    }

    #[test]
    fn tool_calls_are_deferred_to_a_worker() {
        assert!(matches!(
            handle(json!({
                "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                "params": { "name": "list_monitors" }
            })),
            Reply::Later(_)
        ));
    }
}
