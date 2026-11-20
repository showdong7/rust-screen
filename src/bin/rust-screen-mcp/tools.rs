//! MCP tool definitions and their execution on top of the Rust Screen capture core.
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use base64::Engine as _;
use image::{
    ExtendedColorType, ImageEncoder, RgbImage, RgbaImage,
    buffer::ConvertBuffer,
    codecs::{
        jpeg::JpegEncoder,
        png::{CompressionType, FilterType, PngEncoder},
    },
};
use rust_screen::{
    config, enumerate, export,
    geometry::{self, DesktopRect},
    platform, selector,
};

use crate::indicator;
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// Claude and most vision models downscale beyond roughly this long edge anyway.
const DEFAULT_MAX_DIMENSION: u32 = 1568;
const MAX_DELAY_MS: u64 = 10_000;
const MAX_LISTED_WINDOWS: usize = 200;
/// Time for a restored window to repaint before it is captured.
const BRING_TO_FRONT_SETTLE_MS: u64 = 400;
/// Without an explicit format, larger PNGs are returned as JPEG. Base64 makes this
/// about 4.7 MB, under the common 5 MB per-image upload limit of model APIs.
const AUTO_PNG_LIMIT_BYTES: usize = 3584 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    ListMonitors,
    ListWindows,
    CaptureScreen,
    CaptureWindow,
    CaptureRegion,
    SelectRegion,
}

const TOOLS: [Tool; 6] = [
    Tool::ListMonitors,
    Tool::ListWindows,
    Tool::CaptureScreen,
    Tool::CaptureWindow,
    Tool::CaptureRegion,
    Tool::SelectRegion,
];

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Self::ListMonitors => "list_monitors",
            Self::ListWindows => "list_windows",
            Self::CaptureScreen => "capture_screen",
            Self::CaptureWindow => "capture_window",
            Self::CaptureRegion => "capture_region",
            Self::SelectRegion => "select_region",
        }
    }

    fn definition(self) -> Value {
        let (title, description, mut properties, required, read_only) = match self {
            Self::ListMonitors => (
                "List monitors",
                "List the connected monitors with their desktop rectangles (physical pixels), \
                 work areas, DPI scaling and which one is primary. Indexes are ordered left to \
                 right and are what capture_screen's `monitor` expects.",
                Map::new(),
                vec![],
                true,
            ),
            Self::ListWindows => (
                "List windows",
                "List visible top-level windows in Z order (topmost first) with handle, title, \
                 process name, desktop rectangle and whether each is minimized or in the \
                 foreground. Pass a handle to capture_window.",
                properties([(
                    "title_contains",
                    json!({ "type": "string",
                            "description": "Only windows whose title contains this text (case-insensitive)." }),
                )]),
                vec![],
                true,
            ),
            Self::CaptureScreen => (
                "Capture screen",
                "Screenshot one monitor, or the whole virtual desktop spanning every monitor \
                 when `monitor` is omitted.",
                properties([(
                    "monitor",
                    json!({ "type": "integer", "minimum": 0,
                            "description": "Monitor index from list_monitors. Omit for all monitors." }),
                )]),
                vec![],
                false,
            ),
            Self::CaptureWindow => (
                "Capture window",
                "Screenshot a window's visible frame. Choose it by `handle` (from list_windows) \
                 or by `title`; with neither, captures the current foreground window. By \
                 default a background window is asked to render itself (PrintWindow), so \
                 windows covering it are not included and focus is not taken; the result's \
                 `method` says how it was captured. Minimized windows need bring_to_front.",
                properties([
                    (
                        "handle",
                        json!({ "type": "integer", "minimum": 1,
                                "description": "Window handle from list_windows." }),
                    ),
                    (
                        "title",
                        json!({ "type": "string",
                                "description": "Case-insensitive part of the window title; the topmost match is used." }),
                    ),
                    (
                        "bring_to_front",
                        json!({ "type": "boolean", "default": false,
                                "description": "Restore and activate the window before capturing. Windows may refuse to move focus; the result reports whether the window ended up in front." }),
                    ),
                    (
                        "method",
                        json!({ "type": "string", "enum": ["auto", "screen", "print_window"], "default": "auto",
                                "description": "screen copies the pixels on screen (exactly what the user sees, including menus and anything covering the window). print_window asks the window to render itself (works when covered or partly off-screen; some GPU or protected content cannot be captured this way). auto uses screen for the foreground window and print_window otherwise, falling back to screen if print_window fails." }),
                    ),
                ]),
                vec![],
                false,
            ),
            Self::CaptureRegion => (
                "Capture region",
                "Screenshot a rectangle given in physical desktop pixels (the coordinates used \
                 by list_monitors and list_windows). Parts outside every monitor are clipped. \
                 Use it to zoom in on detail that is too small in a downscaled full capture.",
                properties([
                    (
                        "x",
                        json!({ "type": "integer", "description": "Left edge in desktop pixels; may be negative." }),
                    ),
                    (
                        "y",
                        json!({ "type": "integer", "description": "Top edge in desktop pixels; may be negative." }),
                    ),
                    ("width", json!({ "type": "integer", "minimum": 1 })),
                    ("height", json!({ "type": "integer", "minimum": 1 })),
                ]),
                vec!["x", "y", "width", "height"],
                false,
            ),
            Self::SelectRegion => (
                "Ask user to select region",
                "Ask the user to choose what to capture: the desktop freezes and dims, the user \
                 drags a rectangle with the mouse and releases to confirm, or presses Esc / \
                 right-clicks to cancel. Blocks until they finish. Tell the user before calling \
                 this so they know to drag. With width and height, the user only positions a \
                 rectangle of that fixed size.",
                properties([
                    (
                        "width",
                        json!({ "type": "integer", "minimum": 1, "maximum": 32768,
                                      "description": "Fixed selection width; requires height." }),
                    ),
                    (
                        "height",
                        json!({ "type": "integer", "minimum": 1, "maximum": 32768,
                                       "description": "Fixed selection height; requires width." }),
                    ),
                ]),
                vec![],
                false,
            ),
        };
        if !matches!(self, Self::ListMonitors | Self::ListWindows) {
            properties.extend(output_properties());
        }
        json!({
            "name": self.name(),
            "title": title,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            },
            "annotations": {
                "title": title,
                "readOnlyHint": read_only,
                "destructiveHint": false,
                "idempotentHint": read_only,
                "openWorldHint": false,
            },
        })
    }
}

fn properties<const N: usize>(entries: [(&str, Value); N]) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(name, schema)| (name.to_owned(), schema))
        .collect()
}

fn output_properties() -> Map<String, Value> {
    properties([
        (
            "max_dimension",
            json!({ "type": "integer", "minimum": 0, "maximum": 16384, "default": DEFAULT_MAX_DIMENSION,
                    "description": "Downscale the returned image so its longer edge is at most this many pixels. 0 returns full resolution. Saved and copied images are always full resolution." }),
        ),
        (
            "format",
            json!({ "type": "string", "enum": ["png", "jpeg"],
                    "description": "Encoding of the returned image. When omitted, png is used (sharpest text) unless it would exceed 3.5 MiB, in which case jpeg is returned instead so the image stays within model upload limits. jpeg is roughly half the size; token cost depends on pixel size, not encoding." }),
        ),
        (
            "jpeg_quality",
            json!({ "type": "integer", "minimum": 1, "maximum": 100, "default": 85 }),
        ),
        (
            "include_image",
            json!({ "type": "boolean", "default": true,
                    "description": "Return the image itself. Set false with save to only write the file and get its path." }),
        ),
        (
            "save",
            json!({ "type": "boolean", "default": false,
                    "description": "Also save a full-resolution PNG (to save_dir, or Rust Screen's configured output folder) and return its path. Never overwrites existing files." }),
        ),
        (
            "save_dir",
            json!({ "type": "string", "description": "Folder for save; created if missing." }),
        ),
        (
            "copy_to_clipboard",
            json!({ "type": "boolean", "default": false,
                    "description": "Also copy the full-resolution image to the Windows clipboard." }),
        ),
        (
            "delay_ms",
            json!({ "type": "integer", "minimum": 0, "maximum": MAX_DELAY_MS, "default": 0,
                    "description": "Wait this long before capturing, e.g. for a menu or animation to settle." }),
        ),
    ])
}

pub fn definitions() -> Vec<Value> {
    TOOLS.iter().map(|tool| tool.definition()).collect()
}

pub fn find(name: &str) -> Option<Tool> {
    TOOLS.into_iter().find(|tool| tool.name() == name)
}

/// Runs a tool and returns an MCP `CallToolResult`. Bad arguments and capture
/// failures are reported as tool errors (`isError`) so the model can correct itself.
pub fn call(tool: Tool, arguments: Value) -> Value {
    let started = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(|| run(tool, arguments)))
        .unwrap_or_else(|_| Err("Internal error: the capture panicked".into()));
    let elapsed = started.elapsed().as_millis();
    match result {
        Ok(content) => {
            eprintln!("[{}] ok in {elapsed} ms", tool.name());
            json!({ "content": content, "isError": false })
        }
        Err(message) => {
            eprintln!("[{}] failed in {elapsed} ms: {message}", tool.name());
            json!({ "content": [text(&message)], "isError": true })
        }
    }
}

fn text(value: &str) -> Value {
    json!({ "type": "text", "text": value })
}

fn json_text(value: &Value) -> Value {
    text(&serde_json::to_string_pretty(value).expect("JSON values always serialize"))
}

fn parse<T: for<'de> Deserialize<'de>>(tool: Tool, arguments: Value) -> Result<T, String> {
    let arguments = match arguments {
        Value::Null => Value::Object(Map::new()),
        Value::Object(map) => Value::Object(map),
        _ => return Err("Tool arguments must be a JSON object".into()),
    };
    let definition = tool.definition();
    let allowed = definition["inputSchema"]["properties"]
        .as_object()
        .expect("every schema lists its properties");
    if let Some(unknown) = arguments
        .as_object()
        .and_then(|map| map.keys().find(|key| !allowed.contains_key(*key)))
    {
        let mut names: Vec<_> = allowed.keys().map(String::as_str).collect();
        names.sort_unstable();
        return Err(format!(
            "Unknown argument `{unknown}` for {}. Allowed: {}",
            tool.name(),
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            }
        ));
    }
    serde_json::from_value(arguments)
        .map_err(|error| format!("Invalid arguments for {}: {error}", tool.name()))
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Format {
    Png,
    Jpeg,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Method {
    #[default]
    Auto,
    Screen,
    PrintWindow,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Output {
    max_dimension: u32,
    /// `None` means PNG, switching to JPEG when the PNG is too large.
    format: Option<Format>,
    jpeg_quality: u8,
    include_image: bool,
    save: bool,
    save_dir: Option<PathBuf>,
    copy_to_clipboard: bool,
    delay_ms: u64,
}

impl Default for Output {
    fn default() -> Self {
        Self {
            max_dimension: DEFAULT_MAX_DIMENSION,
            format: None,
            jpeg_quality: 85,
            include_image: true,
            save: false,
            save_dir: None,
            copy_to_clipboard: false,
            delay_ms: 0,
        }
    }
}

impl Output {
    fn validate(&self) -> Result<(), String> {
        if self.max_dimension > 16384 {
            return Err("max_dimension must be between 0 and 16384".into());
        }
        if !(1..=100).contains(&self.jpeg_quality) {
            return Err("jpeg_quality must be between 1 and 100".into());
        }
        if self.delay_ms > MAX_DELAY_MS {
            return Err(format!("delay_ms must be at most {MAX_DELAY_MS}"));
        }
        if !self.include_image && !self.save && !self.copy_to_clipboard {
            return Err(
                "include_image is false and neither save nor copy_to_clipboard is set, so \
                 there would be nothing to return"
                    .into(),
            );
        }
        Ok(())
    }

    fn wait(&self) {
        if self.delay_ms > 0 {
            thread::sleep(Duration::from_millis(self.delay_ms));
        }
    }
}

#[derive(Deserialize)]
struct ListWindows {
    title_contains: Option<String>,
}

#[derive(Deserialize)]
struct CaptureScreen {
    monitor: Option<usize>,
    #[serde(flatten)]
    output: Output,
}

#[derive(Deserialize)]
struct CaptureWindow {
    handle: Option<u64>,
    title: Option<String>,
    #[serde(default)]
    bring_to_front: bool,
    #[serde(default)]
    method: Method,
    #[serde(flatten)]
    output: Output,
}

#[derive(Deserialize)]
struct CaptureRegion {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    #[serde(flatten)]
    output: Output,
}

#[derive(Deserialize)]
struct SelectRegion {
    width: Option<u32>,
    height: Option<u32>,
    #[serde(flatten)]
    output: Output,
}

fn rect_json(rect: DesktopRect) -> Value {
    json!({ "x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height })
}

fn run(tool: Tool, arguments: Value) -> Result<Vec<Value>, String> {
    match tool {
        Tool::ListMonitors => {
            parse::<Map<String, Value>>(tool, arguments)?;
            list_monitors()
        }
        Tool::ListWindows => list_windows(parse(tool, arguments)?),
        Tool::CaptureScreen => capture_screen(parse(tool, arguments)?),
        Tool::CaptureWindow => capture_window(parse(tool, arguments)?),
        Tool::CaptureRegion => capture_region(parse(tool, arguments)?),
        Tool::SelectRegion => select_region(parse(tool, arguments)?),
    }
}

fn list_monitors() -> Result<Vec<Value>, String> {
    let desktop = platform::desktop_bounds()?;
    let monitors: Vec<Value> = enumerate::monitors()?
        .into_iter()
        .enumerate()
        .map(|(index, monitor)| {
            json!({
                "index": index,
                "name": monitor.name,
                "primary": monitor.primary,
                "x": monitor.bounds.x,
                "y": monitor.bounds.y,
                "width": monitor.bounds.width,
                "height": monitor.bounds.height,
                "work_area": rect_json(monitor.work_area),
                "dpi": monitor.dpi,
                "scale_percent": (monitor.dpi * 100 + 48) / 96,
            })
        })
        .collect();
    Ok(vec![json_text(&json!({
        "virtual_desktop": rect_json(desktop),
        "monitors": monitors,
    }))])
}

fn list_windows(arguments: ListWindows) -> Result<Vec<Value>, String> {
    let needle = arguments
        .title_contains
        .map(|text| text.to_lowercase())
        .filter(|text| !text.is_empty());
    let matching: Vec<_> = enumerate::windows()
        .into_iter()
        .filter(|window| {
            needle
                .as_ref()
                .is_none_or(|needle| window.title.to_lowercase().contains(needle))
        })
        .collect();
    let total = matching.len();
    let windows: Vec<Value> = matching
        .into_iter()
        .take(MAX_LISTED_WINDOWS)
        .map(|window| {
            json!({
                "handle": window.handle,
                "title": window.title,
                "process": window.process,
                "process_id": window.process_id,
                "bounds": window.bounds.map(rect_json),
                "minimized": window.minimized,
                "foreground": window.foreground,
            })
        })
        .collect();
    Ok(vec![json_text(&json!({
        "count": total,
        "truncated": total > MAX_LISTED_WINDOWS,
        "windows": windows,
    }))])
}

fn capture_screen(arguments: CaptureScreen) -> Result<Vec<Value>, String> {
    arguments.output.validate()?;
    let (bounds, mut details) = match arguments.monitor {
        None => (
            platform::desktop_bounds()?,
            json!({ "source": "virtual_desktop" }),
        ),
        Some(index) => {
            let monitors = enumerate::monitors()?;
            let count = monitors.len();
            let monitor = monitors.into_iter().nth(index).ok_or_else(|| {
                format!(
                    "Monitor {index} does not exist; there are {count} monitor(s), indexes 0 to {}",
                    count - 1
                )
            })?;
            (
                monitor.bounds,
                json!({ "source": "monitor", "monitor": index, "monitor_name": monitor.name }),
            )
        }
    };
    arguments.output.wait();
    let image = platform::capture(bounds)?;
    indicator::flash(bounds);
    finish(
        image,
        bounds,
        &arguments.output,
        details.as_object_mut().unwrap(),
    )
}

/// Copies the visible part of `frame` from the screen; returns the captured bounds.
fn capture_from_screen(frame: DesktopRect) -> Result<(RgbaImage, DesktopRect), String> {
    let bounds = platform::desktop_bounds()?
        .intersection(frame)
        .ok_or_else(|| "The window is entirely outside the visible monitors".to_owned())?;
    Ok((platform::capture(bounds)?, bounds))
}

fn capture_window(arguments: CaptureWindow) -> Result<Vec<Value>, String> {
    arguments.output.validate()?;
    let mut details = Map::new();
    details.insert("source".into(), "window".into());
    let handle = if let Some(handle) = arguments.handle {
        let handle = usize::try_from(handle).map_err(|_| "handle is out of range".to_owned())?;
        if !enumerate::is_window(handle) {
            return Err(format!(
                "No window has handle {handle}; it may have closed. Call list_windows again."
            ));
        }
        handle
    } else if let Some(title) = arguments.title.as_deref().filter(|title| !title.is_empty()) {
        let needle = title.to_lowercase();
        let mut matches = enumerate::windows()
            .into_iter()
            .filter(|window| window.title.to_lowercase().contains(&needle));
        let first = matches.next().ok_or_else(|| {
            format!(
                "No visible window title contains \"{title}\". Call list_windows to see titles."
            )
        })?;
        let others = matches.count();
        if others > 0 {
            details.insert("other_title_matches".into(), others.into());
        }
        first.handle
    } else {
        match platform::foreground_window() {
            0 => return Err("There is no foreground window to capture".into()),
            handle => handle,
        }
    };
    if arguments.bring_to_front {
        platform::restore_window(handle, true);
        thread::sleep(Duration::from_millis(
            arguments.output.delay_ms.max(BRING_TO_FRONT_SETTLE_MS),
        ));
    } else {
        arguments.output.wait();
    }
    if enumerate::is_minimized(handle) {
        return Err(
            "The window is minimized, so it has nothing on screen. Pass bring_to_front: true \
             to restore it first."
                .into(),
        );
    }
    let frame = enumerate::frame_bounds(handle)
        .ok_or_else(|| "Could not read the window's position; it may have closed".to_owned())?;
    let foreground = platform::foreground_window() == handle;
    let print = match arguments.method {
        Method::Screen => false,
        Method::PrintWindow => true,
        Method::Auto => !foreground,
    };
    let (image, bounds, method) = if print {
        match platform::print_window(handle, frame) {
            Ok(image) => (image, frame, "print_window"),
            Err(error) if arguments.method == Method::Auto => {
                details.insert(
                    "fallback".into(),
                    format!("print_window failed ({error}); captured from the screen instead")
                        .into(),
                );
                let (image, bounds) = capture_from_screen(frame)?;
                (image, bounds, "screen")
            }
            Err(error) => {
                return Err(format!(
                    "print_window failed: {error}. Try method \"screen\", with bring_to_front \
                     if other windows cover it."
                ));
            }
        }
    } else {
        let (image, bounds) = capture_from_screen(frame)?;
        (image, bounds, "screen")
    };
    indicator::flash(bounds);
    details.insert("method".into(), method.into());
    let info = enumerate::window_info(handle);
    details.insert(
        "window".into(),
        json!({
            "handle": handle,
            "title": info.as_ref().map(|window| window.title.as_str()),
            "process": info.as_ref().and_then(|window| window.process.as_deref()),
            "frame": rect_json(frame),
            "foreground": foreground,
        }),
    );
    if bounds != frame {
        details.insert("clipped_to_desktop".into(), true.into());
    }
    if method == "screen" && !foreground {
        details.insert(
            "note".into(),
            "Captured from the screen while the window was not in the foreground, so other \
             windows covering it appear in the image."
                .into(),
        );
    }
    finish(image, bounds, &arguments.output, &mut details)
}

fn capture_region(arguments: CaptureRegion) -> Result<Vec<Value>, String> {
    arguments.output.validate()?;
    if arguments.width == 0 || arguments.height == 0 {
        return Err("width and height must be at least 1".into());
    }
    let requested = DesktopRect {
        x: arguments.x,
        y: arguments.y,
        width: arguments.width,
        height: arguments.height,
    };
    let desktop = platform::desktop_bounds()?;
    let bounds = desktop.intersection(requested).ok_or_else(|| {
        format!(
            "The region is outside every monitor. The virtual desktop spans x {}..{}, y {}..{}.",
            desktop.x,
            i64::from(desktop.x) + i64::from(desktop.width),
            desktop.y,
            i64::from(desktop.y) + i64::from(desktop.height)
        )
    })?;
    arguments.output.wait();
    let image = platform::capture(bounds)?;
    indicator::flash(bounds);
    let mut details = Map::new();
    details.insert("source".into(), "region".into());
    if bounds != requested {
        details.insert("requested_rect".into(), rect_json(requested));
        details.insert("clipped_to_desktop".into(), true.into());
    }
    finish(image, bounds, &arguments.output, &mut details)
}

/// Only one selection overlay can be on screen at a time.
static SELECTING: AtomicBool = AtomicBool::new(false);

struct SelectingGuard;
impl Drop for SelectingGuard {
    fn drop(&mut self) {
        SELECTING.store(false, Ordering::Release);
    }
}

fn select_region(arguments: SelectRegion) -> Result<Vec<Value>, String> {
    arguments.output.validate()?;
    let fixed = match (arguments.width, arguments.height) {
        (None, None) => None,
        (Some(width), Some(height))
            if (1..=32768).contains(&width) && (1..=32768).contains(&height) =>
        {
            Some([width, height])
        }
        (Some(_), Some(_)) => return Err("width and height must be between 1 and 32768".into()),
        _ => {
            return Err("Give both width and height for a fixed-size selection, or neither".into());
        }
    };
    if SELECTING.swap(true, Ordering::AcqRel) {
        return Err("Another select_region is already waiting for the user".into());
    }
    let _guard = SelectingGuard;
    arguments.output.wait();
    let desktop = platform::desktop_bounds()?;
    let Some((bounds, image)) =
        selector::select_area(platform::capture_bitmap(desktop)?, desktop, fixed)?
    else {
        return Ok(vec![text(
            "The user cancelled the selection (Esc, right-click or switching away), so \
             nothing was captured.",
        )]);
    };
    let mut details = Map::new();
    details.insert("source".into(), "user_selection".into());
    finish(image, bounds, &arguments.output, &mut details)
}

fn default_save_dir() -> PathBuf {
    config::load().unwrap_or_default().output_dir
}

/// Saves/copies the full-resolution capture as requested, then returns the
/// (downscaled) image and a JSON text block describing how it maps to the desktop.
fn finish(
    image: RgbaImage,
    bounds: DesktopRect,
    output: &Output,
    details: &mut Map<String, Value>,
) -> Result<Vec<Value>, String> {
    details.insert("desktop_rect".into(), rect_json(bounds));
    details.insert(
        "captured_size".into(),
        json!([image.width(), image.height()]),
    );
    if output.save {
        let directory = output.save_dir.clone().unwrap_or_else(default_save_dir);
        let (path, warning) = export::save(&image, &directory, output.copy_to_clipboard)?;
        details.insert("saved_path".into(), path.display().to_string().into());
        if output.copy_to_clipboard {
            details.insert(
                "clipboard".into(),
                warning.unwrap_or_else(|| "copied".into()).into(),
            );
        }
    } else if output.copy_to_clipboard {
        let status = match export::copy_image(&image) {
            Ok(()) => "copied".into(),
            Err(error) => error,
        };
        details.insert("clipboard".into(), status.into());
    }
    let mut content = Vec::with_capacity(2);
    if output.include_image {
        let (width, height) =
            geometry::fit_longest_edge(image.width(), image.height(), output.max_dimension);
        let scaled = if (width, height) == image.dimensions() {
            image
        } else {
            image::imageops::resize(&image, width, height, image::imageops::FilterType::Triangle)
        };
        let (bytes, mime, note) = encode_for_return(
            &scaled,
            output.format,
            output.jpeg_quality,
            AUTO_PNG_LIMIT_BYTES,
        )?;
        if let Some(note) = note {
            details.insert("format_note".into(), note.into());
        }
        details.insert("returned_size".into(), json!([width, height]));
        details.insert(
            "scale".into(),
            json!((f64::from(width) / f64::from(bounds.width) * 10_000.0).round() / 10_000.0),
        );
        details.insert("bytes".into(), bytes.len().into());
        content.push(json!({
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "mimeType": mime,
        }));
    }
    content.push(json_text(&Value::Object(std::mem::take(details))));
    Ok(content)
}

/// With no explicit format, returns PNG unless it exceeds `png_limit`, then JPEG.
fn encode_for_return(
    image: &RgbaImage,
    format: Option<Format>,
    quality: u8,
    png_limit: usize,
) -> Result<(Vec<u8>, &'static str, Option<String>), String> {
    if let Some(format) = format {
        let (bytes, mime) = encode(image, format, quality)?;
        return Ok((bytes, mime, None));
    }
    let (png, mime) = encode(image, Format::Png, quality)?;
    if png.len() <= png_limit {
        return Ok((png, mime, None));
    }
    let (jpeg, mime) = encode(image, Format::Jpeg, quality)?;
    let mib = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
    Ok((
        jpeg,
        mime,
        Some(format!(
            "The PNG would be {:.1} MiB, over the {:.1} MiB limit for automatic format, so JPEG \
             (quality {quality}) was returned. Pass format \"png\" to force PNG, or capture a \
             smaller area or lower max_dimension.",
            mib(png.len()),
            mib(png_limit)
        )),
    ))
}

/// Screenshots are opaque, so both formats drop the alpha channel.
fn encode(
    image: &RgbaImage,
    format: Format,
    quality: u8,
) -> Result<(Vec<u8>, &'static str), String> {
    let rgb: RgbImage = image.convert();
    let mut bytes = Vec::new();
    let result = match format {
        Format::Png => {
            PngEncoder::new_with_quality(&mut bytes, CompressionType::Default, FilterType::Adaptive)
                .write_image(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    ExtendedColorType::Rgb8,
                )
        }
        Format::Jpeg => JpegEncoder::new_with_quality(&mut bytes, quality).write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        ),
    };
    result.map_err(|error| format!("Image encoding failed: {error}"))?;
    Ok((
        bytes,
        match format {
            Format::Png => "image/png",
            Format::Jpeg => "image/jpeg",
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_closed_object_schema() {
        for tool in TOOLS {
            let definition = tool.definition();
            assert_eq!(definition["name"], tool.name());
            assert_eq!(find(tool.name()), Some(tool));
            let schema = &definition["inputSchema"];
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], false);
            for required in schema["required"].as_array().unwrap() {
                assert!(
                    schema["properties"]
                        .get(required.as_str().unwrap())
                        .is_some()
                );
            }
        }
        assert_eq!(find("nope"), None);
    }

    #[test]
    fn arguments_are_validated_before_anything_is_captured() {
        let error = |tool, arguments| {
            let result = call(tool, arguments);
            assert_eq!(result["isError"], true, "{result}");
            result["content"][0]["text"].as_str().unwrap().to_owned()
        };
        assert!(
            error(Tool::CaptureScreen, json!({ "monitr": 1 }))
                .contains("Unknown argument `monitr`")
        );
        assert!(error(Tool::ListMonitors, json!({ "x": 1 })).contains("Allowed: none"));
        assert!(error(Tool::CaptureRegion, json!({ "x": 0, "y": 0 })).contains("missing field"));
        assert!(
            error(Tool::CaptureScreen, json!({ "format": "gif" })).contains("Invalid arguments")
        );
        assert!(error(Tool::CaptureScreen, json!({ "jpeg_quality": 0 })).contains("jpeg_quality"));
        assert!(error(Tool::CaptureScreen, json!({ "delay_ms": 60_000 })).contains("delay_ms"));
        assert!(
            error(Tool::CaptureScreen, json!({ "include_image": false }))
                .contains("nothing to return")
        );
        assert!(
            error(Tool::SelectRegion, json!({ "width": 10 })).contains("both width and height")
        );
        assert!(
            error(
                Tool::CaptureRegion,
                json!({ "x": 0, "y": 0, "width": 0, "height": 5 })
            )
            .contains("at least 1")
        );
        assert!(error(Tool::CaptureWindow, json!([1])).contains("JSON object"));
    }

    #[test]
    fn encoded_images_decode_to_the_same_size() {
        let image = RgbaImage::from_fn(40, 24, |x, y| {
            image::Rgba([x as u8 * 6, y as u8 * 10, 90, 255])
        });
        let (png, mime) = encode(&image, Format::Png, 85).unwrap();
        assert_eq!(mime, "image/png");
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(decoded, image);
        let (jpeg, mime) = encode(&image, Format::Jpeg, 85).unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        assert_eq!(image::load_from_memory(&jpeg).unwrap().width(), 40);
    }

    #[test]
    fn automatic_format_switches_to_jpeg_only_when_png_is_too_large() {
        // Noise compresses poorly, so the PNG is several KiB.
        let mut seed = 0x2545_F491_u32;
        let image = RgbaImage::from_fn(64, 64, |_, _| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let [r, g, b, _] = seed.to_le_bytes();
            image::Rgba([r, g, b, 255])
        });
        let (_, mime, note) = encode_for_return(&image, None, 85, usize::MAX).unwrap();
        assert_eq!((mime, note), ("image/png", None));
        let (bytes, mime, note) = encode_for_return(&image, None, 85, 1024).unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
        assert!(note.unwrap().contains("Pass format \"png\""));
        // An explicit format is always honoured.
        let (_, mime, note) = encode_for_return(&image, Some(Format::Png), 85, 1024).unwrap();
        assert_eq!((mime, note), ("image/png", None));
    }

    #[test]
    fn window_capture_method_is_validated() {
        let result = call(Tool::CaptureWindow, json!({ "method": "magic" }));
        assert_eq!(result["isError"], true);
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Invalid arguments")
        );
    }

    #[test]
    fn finish_reports_mapping_and_saves_full_resolution() {
        let directory = std::env::temp_dir().join(format!(
            "rust-screen-mcp-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let image = RgbaImage::from_pixel(400, 200, image::Rgba([10, 20, 30, 255]));
        let bounds = DesktopRect {
            x: -400,
            y: 100,
            width: 400,
            height: 200,
        };
        let output = Output {
            max_dimension: 100,
            save: true,
            save_dir: Some(directory.clone()),
            ..Output::default()
        };
        let mut details = Map::new();
        let content = finish(image, bounds, &output, &mut details).unwrap();
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["mimeType"], "image/png");
        let png = base64::engine::general_purpose::STANDARD
            .decode(content[0]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(image::load_from_memory(&png).unwrap().width(), 100);
        let meta: Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            meta["desktop_rect"],
            json!({ "x": -400, "y": 100, "width": 400, "height": 200 })
        );
        assert_eq!(meta["returned_size"], json!([100, 50]));
        assert_eq!(meta["scale"], 0.25);
        let saved = PathBuf::from(meta["saved_path"].as_str().unwrap());
        assert_eq!(image::open(&saved).unwrap().width(), 400);
        std::fs::remove_file(saved).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    #[ignore = "requires an unlocked Windows desktop"]
    fn captures_a_downscaled_jpeg_of_the_primary_monitor() {
        platform::enable_dpi_awareness();
        let result = call(
            Tool::CaptureScreen,
            json!({ "monitor": 0, "format": "jpeg", "max_dimension": 640 }),
        );
        assert_eq!(result["isError"], false, "{result}");
        assert_eq!(result["content"][0]["mimeType"], "image/jpeg");
        let meta: Value =
            serde_json::from_str(result["content"][1]["text"].as_str().unwrap()).unwrap();
        assert!(meta["returned_size"][0].as_u64().unwrap() <= 640);
    }
}
