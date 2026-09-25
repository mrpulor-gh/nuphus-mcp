//! Desktop tool executor — reuses the low-level vision/input/platform
//! modules of the `desktop-api` crate (compiled with the default feature, does not depend on its http-server UnifiedApi).
//!
//! Constraint: no direct Win32 API calls bypassing desktop-api. The two capabilities desktop-api lacks
//! (window activation, scroll wheel) are added as minimal public methods (see desktop-api change notes).

use base64::Engine;
use desktop_api::input::{self, InputEngine};
use desktop_api::platform::WindowManager;
use desktop_api::{CaptureGeometry, GfxBackend, Scope, Target};
use serde_json::{json, Value};

use super::semantic;

/// Execute a desktop_* tool, returning a text result.
pub async fn execute(name: &str, args: &Value) -> Result<String, String> {
    match name {
        "desktop_screen_size" => screen_size().await,
        "desktop_screenshot" => screenshot(args).await,
        "desktop_windows_list" => windows_list().await,
        "desktop_window_activate" => window_activate(args).await,
        "desktop_window_screenshot" => window_screenshot(args).await,
        "desktop_window_move" => window_move(args).await,
        "desktop_window_resize" => window_resize(args).await,
        "desktop_window_info" => window_info(args).await,
        "desktop_vision" => vision(args).await,
        "desktop_perceive" => perceive(args).await,
        "desktop_mouse" => mouse(args).await,
        "desktop_mouse_drag" => mouse_drag(args).await,
        "desktop_input" => input(args).await,
        "desktop_clipboard_clean" => clipboard_clean().await,
        "desktop_clipboard_write" => clipboard_write(args).await,
        // Semantic (Accessibility/UIA-first) tools: see `tools/semantic.rs`.
        // The observation → candidate → execution → verification state lives in
        // one process-wide backend, so these share it rather than the vision path.
        "desktop_targets_list" => semantic::execute(name, args).await,
        "desktop_target_bind" => semantic::execute(name, args).await,
        "desktop_semantic_observe" => semantic::execute(name, args).await,
        "desktop_semantic_candidate" => semantic::execute(name, args).await,
        "desktop_semantic_execute" => semantic::execute(name, args).await,
        "desktop_semantic_action" => semantic::execute(name, args).await,
        "desktop_verify_state" => semantic::execute(name, args).await,
        _ => Err(format!("Unknown desktop tool: {}", name)),
    }
}

// ─────────────────────────────── helpers ───────────────────────────────

/// Build a window target (Window variant on Windows to support window screenshots; Tui fallback on other platforms).
#[cfg(windows)]
fn target_window(hwnd: isize) -> Target {
    Target::Window {
        hwnd,
        title: String::new(),
        verified: false,
        gfx_backend: GfxBackend::Unknown,
    }
}

#[cfg(not(windows))]
fn target_window(hwnd: isize) -> Target {
    Target::Tui {
        hwnd,
        title: String::new(),
    }
}

/// Placeholder target for full-screen screenshots (Scope::Fullscreen ignores target).
fn dummy_target() -> Target {
    Target::Tui {
        hwnd: 0,
        title: String::new(),
    }
}

/// Screenshot → PNG bytes (lossless; LLM APIs and template matching both accept PNG)
fn encode_png(frame: &desktop_api::Frame) -> Result<Vec<u8>, String> {
    let img = image::RgbaImage::from_raw(frame.width, frame.height, frame.pixels.clone())
        .ok_or_else(|| format!("invalid frame dimensions {}x{}", frame.width, frame.height))?;
    let mut cursor = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| format!("PNG encode failed: {}", e))?;
    Ok(cursor.into_inner())
}

/// Save or inline-return the PNG, unified output {path|data, width, height, size}
fn output_png(
    path: Option<&str>,
    frame: &desktop_api::Frame,
    png: &[u8],
) -> Result<String, String> {
    match path {
        Some(p) => {
            // Security boundary: path validation (path traversal / system-protected dirs / parent exists)
            let validated = crate::security::validate_screenshot_path(p)?;
            let final_path = if validated.to_lowercase().ends_with(".png") {
                validated
            } else {
                format!("{}.png", validated)
            };
            std::fs::write(&final_path, png).map_err(|e| format!("save failed: {}", e))?;
            Ok(json!({
                "path": final_path,
                "format": "png",
                "size": png.len(),
                "width": frame.width,
                "height": frame.height,
            })
            .to_string())
        }
        None => {
            let data = base64::engine::general_purpose::STANDARD.encode(png);
            Ok(json!({
                "format": "png",
                "data": data,
                "width": frame.width,
                "height": frame.height,
            })
            .to_string())
        }
    }
}

// ─────────────────────────────── tools ───────────────────────────────

async fn screen_size() -> Result<String, String> {
    let target = dummy_target();
    let frame = desktop_api::vision::capture::capture(&target, Scope::Fullscreen)
        .await
        .map_err(|e| format!("screen capture failed: {}", e))?;
    Ok(json!({ "width": frame.width, "height": frame.height }).to_string())
}

/// Parse the optional `region` argument shared by `desktop_screenshot` and
/// `desktop_perceive` (`{x, y, width, height}` → `Scope::Element`); `None` means
/// "no region given". Out-of-screen origins are clamped downstream by the capture
/// itself, which `CaptureGeometry` then reports.
fn region_scope(args: &Value) -> Option<Scope> {
    let region = args.get("region")?;
    if !region.is_object() {
        return None;
    }
    Some(Scope::Element {
        x: region.get("x").and_then(Value::as_i64).unwrap_or(0) as i32,
        y: region.get("y").and_then(Value::as_i64).unwrap_or(0) as i32,
        w: region.get("width").and_then(Value::as_i64).unwrap_or(0) as u32,
        h: region.get("height").and_then(Value::as_i64).unwrap_or(0) as u32,
    })
}

async fn screenshot(args: &Value) -> Result<String, String> {
    let target = dummy_target();
    let (scope, failure) = match region_scope(args) {
        Some(scope) => (scope, "region capture failed"),
        None => (Scope::Fullscreen, "screen capture failed"),
    };
    let frame = desktop_api::vision::capture::capture(&target, scope)
        .await
        .map_err(|e| format!("{}: {}", failure, e))?;
    let png = encode_png(&frame)?;
    let path = args.get("path").and_then(Value::as_str);
    output_png(path, &frame, &png)
}

async fn windows_list() -> Result<String, String> {
    let wm = WindowManager::new();
    let windows = wm
        .list_all()
        .map_err(|e| format!("windows list failed: {}", e))?;
    let arr: Vec<Value> = windows
        .into_iter()
        .map(|w| {
            json!({
                "hwnd": w.hwnd,
                "title": w.title,
                "x": w.x,
                "y": w.y,
                "width": w.width,
                "height": w.height,
                "visible": w.visible,
                "process_id": w.process_id,
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&arr).unwrap_or_default())
}

async fn window_activate(args: &Value) -> Result<String, String> {
    let hwnd = args
        .get("hwnd")
        .and_then(Value::as_i64)
        .ok_or_else(|| "hwnd is required".to_string())? as isize;
    let mut target = target_window(hwnd);
    let engine = InputEngine::new();
    engine
        .activate(&mut target)
        .await
        .map_err(|e| format!("window activate failed: {}", e))?;
    Ok(json!({ "hwnd": hwnd, "activated": true }).to_string())
}

async fn window_screenshot(args: &Value) -> Result<String, String> {
    let hwnd = args.get("hwnd").and_then(Value::as_i64).map(|v| v as isize);
    let title = args.get("title").and_then(Value::as_str);
    let target = match hwnd {
        Some(h) => target_window(h),
        None => match title {
            Some(t) => {
                let mut wm = WindowManager::new();
                wm.find(t)
                    .map_err(|e| format!("window find failed: {}", e))?
            }
            None => return Err("hwnd or title required".to_string()),
        },
    };
    let frame = desktop_api::vision::capture::capture(&target, Scope::Window)
        .await
        .map_err(|e| format!("window capture failed: {}", e))?;
    let png = encode_png(&frame)?;
    let path = args.get("path").and_then(Value::as_str);
    output_png(path, &frame, &png)
}

/// Move the window to the given coordinates (Windows: SetWindowPos).
async fn window_move(args: &Value) -> Result<String, String> {
    let hwnd = args
        .get("hwnd")
        .and_then(Value::as_i64)
        .ok_or_else(|| "hwnd is required".to_string())? as isize;
    let x = args.get("x").and_then(Value::as_i64).unwrap_or(0) as i32;
    let y = args.get("y").and_then(Value::as_i64).unwrap_or(0) as i32;
    let wm = WindowManager::new();
    wm.window_move(hwnd, x, y)
        .map_err(|e| format!("window move failed: {}", e))?;
    Ok(json!({ "hwnd": hwnd, "x": x, "y": y, "moved": true }).to_string())
}

/// Resize the window (Windows: SetWindowPos, keeping the current position).
async fn window_resize(args: &Value) -> Result<String, String> {
    let hwnd = args
        .get("hwnd")
        .and_then(Value::as_i64)
        .ok_or_else(|| "hwnd is required".to_string())? as isize;
    let width = args
        .get("width")
        .and_then(Value::as_i64)
        .ok_or_else(|| "width is required".to_string())? as i32;
    let height = args
        .get("height")
        .and_then(Value::as_i64)
        .ok_or_else(|| "height is required".to_string())? as i32;
    let wm = WindowManager::new();
    wm.window_resize(hwnd, width, height)
        .map_err(|e| format!("window resize failed: {}", e))?;
    Ok(json!({ "hwnd": hwnd, "width": width, "height": height, "resized": true }).to_string())
}

/// Query detailed window information.
async fn window_info(args: &Value) -> Result<String, String> {
    let hwnd = args
        .get("hwnd")
        .and_then(Value::as_i64)
        .ok_or_else(|| "hwnd is required".to_string())? as isize;
    let wm = WindowManager::new();
    let detail = wm
        .window_info(hwnd)
        .map_err(|e| format!("window info failed: {}", e))?;
    Ok(json!({
        "hwnd": detail.hwnd,
        "title": detail.title,
        "visible": detail.visible,
        "minimized": detail.minimized,
        "maximized": detail.maximized,
        "window": { "x": detail.window.x, "y": detail.window.y, "width": detail.window.w, "height": detail.window.h },
        "client": { "x": detail.client.x, "y": detail.client.y, "width": detail.client.w, "height": detail.client.h },
        "process_id": detail.process_id,
        "process_name": detail.process_name,
        "class_name": detail.class_name,
    })
    .to_string())
}

/// desktop_vision — BYOK cloud vision understanding.
///
/// Reads `NUPHUS_MCP_VISION_API_KEY` / `NUPHUS_MCP_VISION_BASE_URL` /
/// `NUPHUS_MCP_VISION_MODEL` / `NUPHUS_MCP_VISION_PROVIDER` (auto|openai|anthropic,
/// auto-detected from the base URL host); missing key → clear error.
/// Auto-captures a screenshot when `path` is omitted.
async fn vision(args: &Value) -> Result<String, String> {
    let prompt = args.get("prompt").and_then(Value::as_str);
    // `_temp` keeps the temp-PNG guard alive until the vision call completes.
    let (image_path, _temp) = match args.get("path").and_then(Value::as_str) {
        Some(p) => (p.to_string(), None),
        None => {
            let (temp, _geometry) = TempCapture::capture(Scope::Fullscreen).await?;
            (temp.path().to_string(), Some(temp))
        }
    };
    let text = crate::vision::vision_image(&image_path, prompt).await?;
    Ok(json!({ "description": text }).to_string())
}

/// desktop_perceive — local OCR + YOLO element location.
///
/// Auto-downloads PaddleOCR models on first run; missing models with failed downloads → clear error.
/// Auto-captures a screenshot when `path` is omitted (full screen, or `region` when given) and then
/// reports elements in *screen* coordinates, so `center` can be clicked as-is; a caller-supplied
/// `path` has no known screen origin, so its elements stay in image coordinates.
/// When the YOLO model is missing, returns OCR-only results and reports it honestly.
async fn perceive(args: &Value) -> Result<String, String> {
    // 1. Ensure models are ready (auto-download missing OCR models)
    let status = crate::models::ensure_models().await?;

    // `path` means "analyse this image", `region` means "capture this area first".
    if region_scope(args).is_some() && args.get("path").and_then(Value::as_str).is_some() {
        return Err(
            "path and region are mutually exclusive: pass path to analyse an existing PNG, or region to capture an area"
                .to_string(),
        );
    }

    // 2. Obtain the image to analyze (`_temp` keeps the temp-PNG guard alive
    // until inference completes; the file is deleted on every exit path).
    // `geometry` is the screen rectangle of an image we captured ourselves: it is
    // what turns the model's image-space pixels into clickable screen pixels.
    let (image_path, geometry, _temp) = match args.get("path").and_then(Value::as_str) {
        Some(p) => (p.to_string(), None, None),
        None => {
            let scope = region_scope(args).unwrap_or(Scope::Fullscreen);
            let (temp, geometry) = TempCapture::capture(scope).await?;
            (temp.path().to_string(), Some(geometry), Some(temp))
        }
    };

    // 3. OCR + YOLO inference (CPU-intensive → spawn_blocking, avoid blocking the IO loop)
    let output = tokio::task::spawn_blocking(move || {
        desktop_api::vision::perceive::perceive_image(&image_path)
    })
    .await
    .map_err(|e| format!("perceive worker panicked: {}", e))?
    .map_err(|e| format!("perceive failed: {}", e))?;

    let json_elements: Vec<Value> = output
        .elements
        .iter()
        .map(|el| {
            let rect = match geometry {
                Some(geometry) => geometry.rect_to_screen(el.rect),
                None => el.rect,
            };
            let center = match geometry {
                Some(geometry) => geometry.to_screen(el.rect.center()),
                None => el.rect.center(),
            };
            json!({
                "id": el.id,
                "kind": format!("{:?}", el.kind).to_lowercase(),
                "text": el.text,
                "rect": { "x": rect.x, "y": rect.y, "w": rect.w, "h": rect.h },
                "center": { "x": center.x, "y": center.y },
                "confidence": el.confidence,
                "source": format!("{:?}", el.source).to_lowercase(),
            })
        })
        .collect();

    // `screen` = rect/center are absolute and safe to click; `image` = they are
    // relative to the supplied PNG's own top-left pixel (origin unknown).
    let coordinate_space = if geometry.is_some() {
        "screen"
    } else {
        "image"
    };
    let mut result = json!({
        "elements": json_elements,
        "count": json_elements.len(),
        "ocr_count": output.ocr_count,
        "yolo_count": output.yolo_count,
        "yolo_available": output.yolo_available,
        "models_dir": status.dir.display().to_string(),
        "coordinate_space": coordinate_space,
    });
    if let Some(geometry) = geometry {
        result["geometry"] = json!({
            "x": geometry.x,
            "y": geometry.y,
            "width": geometry.width,
            "height": geometry.height,
        });
    }
    if !output.yolo_available {
        result["yolo_disabled_reason"] = json!(
            "icon_detect.onnx not available (auto-download failed or skipped). OCR-only result. Retry desktop_perceive to re-attempt the download, or set NUPHUS_MCP_YOLO_MODEL_URL for a custom source."
        );
    }
    Ok(result.to_string())
}

/// Screenshot to a temp file and return a guard whose Drop deletes it (P2:
/// `%TEMP%\nuphus_mcp_capture_*.png` used to accumulate ~8MB per call forever).
/// The guard covers every early-return path of the caller.
struct TempCapture {
    path: String,
}

impl TempCapture {
    /// Capture `scope` into a temp PNG and report the screen rectangle it covers.
    async fn capture(scope: Scope) -> Result<(Self, CaptureGeometry), String> {
        let target = dummy_target();
        let (frame, geometry) = desktop_api::vision::capture::capture_with_geometry(&target, scope)
            .await
            .map_err(|e| format!("screen capture failed: {}", e))?;
        let png = encode_png(&frame)?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("nuphus_mcp_capture_{}.png", nanos));
        std::fs::write(&path, &png).map_err(|e| format!("save temp capture failed: {}", e))?;
        Ok((
            Self {
                path: path.to_string_lossy().into_owned(),
            },
            geometry,
        ))
    }

    fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for TempCapture {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            tracing::warn!(
                "[desktop] temp capture cleanup failed ({}): {}",
                self.path,
                e
            );
        }
    }
}

async fn mouse(args: &Value) -> Result<String, String> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| "action is required".to_string())?;
    // Coordinate-carrying actions must fail loudly when x/y are missing —
    // defaulting to (0,0) clicks/moves at the top-left screen corner by mistake.
    let needs_xy = matches!(action, "move" | "hover" | "click" | "double_click");
    let x = args.get("x").and_then(Value::as_i64).map(|v| v as i32);
    let y = args.get("y").and_then(Value::as_i64).map(|v| v as i32);
    if needs_xy && (x.is_none() || y.is_none()) {
        return Err(format!(
            "x and y are required for action={action} (got x={x:?}, y={y:?})"
        ));
    }
    let (x, y) = (x.unwrap_or(0), y.unwrap_or(0));

    match action {
        "position" => {
            let p = input::mouse::position()
                .await
                .map_err(|e| format!("cursor position failed: {}", e))?;
            Ok(json!({ "x": p.x, "y": p.y }).to_string())
        }
        "move" | "hover" => {
            input::mouse::move_to(x, y)
                .await
                .map_err(|e| format!("mouse move failed: {}", e))?;
            Ok(json!({ "moved_to": { "x": x, "y": y } }).to_string())
        }
        "click" | "double_click" => {
            let button = args.get("button").and_then(Value::as_str).unwrap_or("left");
            let clicks = if action == "double_click" {
                2
            } else {
                args.get("clicks")
                    .and_then(Value::as_i64)
                    .unwrap_or(1)
                    .max(1) as u32
            };
            // click/right_click internally move_to(x, y) first
            for _ in 0..clicks {
                match button {
                    "left" => input::mouse::click(x, y)
                        .await
                        .map_err(|e| format!("mouse click failed: {}", e))?,
                    "right" => input::mouse::right_click(x, y)
                        .await
                        .map_err(|e| format!("mouse right click failed: {}", e))?,
                    "middle" => input::mouse::middle_click(x, y)
                        .await
                        .map_err(|e| format!("mouse middle click failed: {}", e))?,
                    other => return Err(format!("unknown mouse button: {}", other)),
                }
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            }
            Ok(
                json!({ "clicked": { "x": x, "y": y, "clicks": clicks, "button": button } })
                    .to_string(),
            )
        }
        "scroll" => {
            let direction = args
                .get("direction")
                .and_then(Value::as_str)
                .unwrap_or("down");
            let amount = args.get("amount").and_then(Value::as_i64).unwrap_or(3) as i32;
            input::mouse::scroll(direction, amount)
                .await
                .map_err(|e| format!("scroll failed: {}", e))?;
            Ok(json!({ "scrolled": { "direction": direction, "amount": amount } }).to_string())
        }
        _ => Err(format!("Unknown mouse action: {}", action)),
    }
}

async fn mouse_drag(args: &Value) -> Result<String, String> {
    let start_x = args
        .get("start_x")
        .and_then(Value::as_i64)
        .ok_or_else(|| "start_x required".to_string())? as i32;
    let start_y = args
        .get("start_y")
        .and_then(Value::as_i64)
        .ok_or_else(|| "start_y required".to_string())? as i32;
    let end_x = args
        .get("end_x")
        .and_then(Value::as_i64)
        .ok_or_else(|| "end_x required".to_string())? as i32;
    let end_y = args
        .get("end_y")
        .and_then(Value::as_i64)
        .ok_or_else(|| "end_y required".to_string())? as i32;
    input::mouse::drag(
        desktop_api::Point {
            x: start_x,
            y: start_y,
        },
        desktop_api::Point { x: end_x, y: end_y },
    )
    .await
    .map_err(|e| format!("mouse drag failed: {}", e))?;
    Ok(json!({ "dragged": { "from": { "x": start_x, "y": start_y }, "to": { "x": end_x, "y": end_y } } }).to_string())
}

async fn input(args: &Value) -> Result<String, String> {
    let mode = args
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| "mode is required".to_string())?;
    // hwnd omitted → explicit "current focus" semantics: resolve the actual
    // foreground window instead of passing 0 (SetForegroundWindow(HWND(0)) always
    // fails silently and text would go wherever focus happens to be, unverified).
    let hwnd = match args.get("hwnd").and_then(Value::as_i64) {
        Some(v) => v as isize,
        None => desktop_api::platform::foreground_hwnd()
            .ok_or_else(|| "hwnd is required (no foreground window to target)".to_string())?,
    };
    let mut target = target_window(hwnd);
    let engine = InputEngine::new();

    match mode {
        "type" => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() {
                return Err("text is required for mode=type".to_string());
            }
            if text.len() > 500 {
                // Long text goes through clipboard + paste. Snapshot the previous
                // clipboard text first; restoration runs on EVERY exit path
                // (including paste failure) so the user's clipboard is never
                // silently clobbered. When the previous content is not text
                // (image/files/empty), there is nothing to restore — keep the
                // pasted text in place and warn, never wipe the clipboard.
                let prev = desktop_api::clipboard::read_text().ok();
                let paste_result: Result<(), String> = async {
                    desktop_api::clipboard::write_text(text)
                        .map_err(|e| format!("clipboard write failed: {}", e))?;
                    engine
                        .hotkey(&mut target, &["ctrl", "v"])
                        .await
                        .map_err(|e| format!("paste failed: {}", e))
                }
                .await;
                let restore_warning = match &prev {
                    Some(saved) => desktop_api::clipboard::write_text(saved)
                        .err()
                        .map(|e| format!("paste done but clipboard restore failed: {e}")),
                    None => {
                        tracing::warn!(
                            "[desktop_input] previous clipboard content was not text \
                             (or unreadable); leaving the pasted text in the clipboard"
                        );
                        None
                    }
                };
                // Propagate a paste failure only AFTER the restore attempt.
                paste_result?;
                if let Some(w) = restore_warning {
                    return Err(w);
                }
            } else {
                engine
                    .send_text(text, &mut target)
                    .await
                    .map_err(|e| format!("input failed: {}", e))?;
            }
            let send = args.get("send").and_then(Value::as_str).unwrap_or("enter");
            send_after(&engine, &mut target, send).await?;
            Ok(json!({ "typed_chars": text.chars().count(), "send": send }).to_string())
        }
        "hotkey" => {
            let keys: Vec<String> = args
                .get("keys")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            if keys.is_empty() {
                return Err("keys is required for mode=hotkey".to_string());
            }
            let refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
            engine
                .hotkey(&mut target, &refs)
                .await
                .map_err(|e| format!("hotkey failed: {}", e))?;
            Ok(json!({ "hotkey": keys }).to_string())
        }
        _ => Err(format!("Unknown input mode: {}", mode)),
    }
}

/// Send an extra key after input: "none" skips; "enter"/"tab" single key; "ctrl+enter" combo.
async fn send_after(engine: &InputEngine, target: &mut Target, send: &str) -> Result<(), String> {
    if send == "none" {
        return Ok(());
    }
    let keys: Vec<&str> = send.split('+').collect();
    if keys.len() == 1 {
        engine
            .press(target, keys[0])
            .await
            .map_err(|e| format!("send key '{}' failed: {}", send, e))
    } else {
        engine
            .hotkey(target, &keys)
            .await
            .map_err(|e| format!("send hotkey '{}' failed: {}", send, e))
    }
}

async fn clipboard_clean() -> Result<String, String> {
    desktop_api::clipboard::write_text("").map_err(|e| format!("clipboard clear failed: {}", e))?;
    Ok(json!({ "cleared": true }).to_string())
}

async fn clipboard_write(args: &Value) -> Result<String, String> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| "text is required".to_string())?;
    desktop_api::clipboard::write_text(text)
        .map_err(|e| format!("clipboard write failed: {}", e))?;
    Ok(json!({ "written_chars": text.chars().count() }).to_string())
}
