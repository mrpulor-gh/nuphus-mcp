//! Tool definitions (source of inputSchema for MCP tools/list).
//!
//! Parameter definitions mirror the main crate's `src/tools/desktop_schemas.rs` (translated to JSON Schema),
//! only including tools this server actually executes (desktop via desktop-api, browser via
//! nuphus-browser).

use serde_json::{json, Map, Value};

/// A single MCP tool definition
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

/// Helper macro to build a JSON object from key=value pairs.
macro_rules! obj {
    ($($k:literal = $v:expr),* $(,)?) => {{
        let mut m = Map::new();
        $(m.insert($k.to_string(), json!($v));)*
        Value::Object(m)
    }};
}

/// Helper macro to build the properties object for tool parameters.
macro_rules! json_props {
    ($($k:literal => $v:expr),* $(,)?) => {{
        let mut m = Map::new();
        $(m.insert($k.to_string(), $v);)*
        Value::Object(m)
    }};
}

fn tool_def(
    name: &'static str,
    description: &'static str,
    properties: Value,
    required: &[&str],
) -> ToolDef {
    let Value::Object(mut props) = properties else {
        panic!("tool '{name}': properties must be a JSON object");
    };
    // Strict-confirm mode gates write tools on an explicit `"confirm": true` argument
    // (security::check_write_confirmation). Declare `confirm` in the schema for every
    // tool the runtime *may* classify as a write — otherwise spec-compliant clients
    // strip unknown arguments before the runtime ever sees them. The set is derived
    // from the same source of truth as the runtime check (`is_write_tool_schema`),
    // so the two cannot drift apart again.
    if crate::security::is_write_tool_schema(name) {
        props.insert(
            "confirm".to_string(),
            json!({
                "type": "boolean",
                "description": "Set to true to explicitly confirm this write operation (required in strict-confirm mode)"
            }),
        );
    }
    let required: Vec<String> = required.iter().map(|s| s.to_string()).collect();
    ToolDef {
        name,
        description,
        input_schema: json!({
            "type": "object",
            "properties": props,
            "required": required,
        }),
    }
}

/// Build a tool schema that accepts either `selector` or its `ref` alias.
/// `anyOf` is intentional: callers may provide both, while at least one must
/// be present. Other required fields remain in the top-level `required` list.
fn tool_def_with_selector_or_ref(
    name: &'static str,
    description: &'static str,
    properties: Value,
    required: &[&str],
) -> ToolDef {
    let mut tool = tool_def(name, description, properties, required);
    tool.input_schema["anyOf"] = json!([
        { "required": ["selector"] },
        { "required": ["ref"] }
    ]);
    tool
}

/// All tools (desktop + browser)
pub fn all_tools() -> Vec<ToolDef> {
    let mut tools = desktop_tools();
    tools.extend(browser_tools());
    tools
}

/// Desktop tools (via desktop-api)
fn desktop_tools() -> Vec<ToolDef> {
    vec![
        tool_def(
            "desktop_screen_size",
            "Get the current screen resolution (width x height).",
            json!({}),
            &[],
        ),
        tool_def(
            "desktop_screenshot",
            "Capture the full screen (or a region). Saves as PNG when a path is provided, otherwise returns base64-encoded PNG data.",
            json_props! {
                "path" => obj!("type"="string","description"="Save path (auto-appends .png); omit to return base64"),
                "region" => obj!("type"="object","description"="Crop region {x,y,width,height}; omit for full screen")
            },
            &[],
        ),
        tool_def(
            "desktop_windows_list",
            "List all visible OS windows (hwnd/title/position).",
            json!({}),
            &[],
        ),
        tool_def(
            "desktop_window_activate",
            "Bring a window to the foreground by hwnd. Activate before window ops or actions may hit the wrong window.",
            json_props! {
                "hwnd" => obj!("type"="integer","description"="Window handle from desktop_windows_list")
            },
            &["hwnd"],
        ),
        tool_def(
            "desktop_window_screenshot",
            "Capture a specified window and save as PNG (locate by hwnd or title; provide at least one).",
            json_props! {
                "title" => obj!("type"="string","description"="Window title substring to find"),
                "hwnd" => obj!("type"="integer","description"="Window handle from desktop_windows_list"),
                "path" => obj!("type"="string","description"="Save path (auto-appends .png); omit to return base64")
            },
            &[],
        ),
        tool_def(
            "desktop_window_move",
            "Move a window to screen coordinates (x,y) by hwnd.",
            json_props! {
                "hwnd" => obj!("type"="integer","description"="Window handle from desktop_windows_list"),
                "x" => obj!("type"="integer","description"="Target X screen coordinate"),
                "y" => obj!("type"="integer","description"="Target Y screen coordinate")
            },
            &["hwnd", "x", "y"],
        ),
        tool_def(
            "desktop_window_resize",
            "Resize a window to (width,height) by hwnd.",
            json_props! {
                "hwnd" => obj!("type"="integer","description"="Window handle from desktop_windows_list"),
                "width" => obj!("type"="integer","description"="New window width in pixels"),
                "height" => obj!("type"="integer","description"="New window height in pixels")
            },
            &["hwnd", "width", "height"],
        ),
        tool_def(
            "desktop_window_info",
            "Query detailed window info (title/visibility/state/rects/process/class) by hwnd.",
            json_props! {
                "hwnd" => obj!("type"="integer","description"="Window handle from desktop_windows_list")
            },
            &["hwnd"],
        ),
        tool_def(
            "desktop_vision",
            "Understand a screenshot with a vision model (BYOK, OpenAI-compatible/Anthropic). Pass a focused prompt or omit for full text. Requires NUPHUS_MCP_VISION_API_KEY/MODEL; provider from BASE_URL or forced via PROVIDER. Omit path = capture full screen first. ⚠️ Coords imprecise — never click with them; use desktop_perceive for exact coords.",
            json_props! {
                "path" => obj!("type"="string","description"="Image file path (PNG); omit to capture the full screen first"),
                "prompt" => obj!("type"="string","description"="Optional instruction for the vision model; defaults to a generic describe-and-read-text prompt")
            },
            &[],
        ),
        tool_def(
            "desktop_perceive",
            "Locate UI elements via local OCR (PaddleOCR) + optional YOLO; auto-downloads OCR models on first run. Returns rect{x,y,w,h} + center{x,y}; ALWAYS click center, never rect.x/y. Omit path to capture full screen first. OCR text may be inaccurate — trust desktop_vision.",
            json_props! {
                "path" => obj!("type"="string","description"="Image file path (PNG); omit to capture the full screen first"),
            },
            &[],
        ),
        tool_def(
            "desktop_mouse",
            "Mouse operations: click/double_click/hover/scroll/move require (x,y). position is read-only and returns the current cursor (x,y) without moving it.",
            json_props! {
                "action" => obj!("type"="string","enum"=["click","double_click","hover","scroll","position","move"],"description"="What to do"),
                "x" => obj!("type"="integer","description"="X coordinate"),
                "y" => obj!("type"="integer","description"="Y coordinate"),
                "button" => obj!("type"="string","enum"=["left","right","middle"],"description"="Mouse button (click)"),
                "clicks" => obj!("type"="integer","default"=1,"description"="Number of clicks (click)"),
                "direction" => obj!("type"="string","enum"=["up","down"],"description"="Scroll direction (scroll)"),
                "amount" => obj!("type"="integer","default"=3,"description"="Scroll ticks (scroll)")
            },
            &["action"],
        ),
        tool_def(
            "desktop_mouse_drag",
            "Drag the mouse from start to end coordinates (e.g. CAPTCHA sliders).",
            json_props! {
                "start_x" => obj!("type"="integer","description"="Start X coordinate"),
                "start_y" => obj!("type"="integer","description"="Start Y coordinate"),
                "end_x" => obj!("type"="integer","description"="End X coordinate"),
                "end_y" => obj!("type"="integer","description"="End Y coordinate")
            },
            &["start_x", "start_y", "end_x", "end_y"],
        ),
        tool_def(
            "desktop_input",
            "Type text into a window (UTF-8), optionally with a follow-up key. Use clipboard for >500 chars. Activate the target window first.",
            json_props! {
                "mode" => obj!("type"="string","enum"=["type","hotkey"],"description"="type: input text; hotkey: press keys only"),
                "hwnd" => obj!("type"="integer","description"="Target window handle. Get from desktop_windows_list."),
                "text" => obj!("type"="string","description"="Text to type (mode=type required)"),
                "send" => obj!("type"="string","description"="Key to send after typing: \"enter\" (default), \"ctrl+enter\", \"tab\", or \"none\" to skip."),
                "keys" => obj!("type"="array","items"=obj!("type"="string"),"description"="Key combo to press (mode=hotkey required)")
            },
            &["mode", "hwnd"],
        ),
        tool_def(
            "desktop_clipboard_clean",
            "Clear the clipboard. Call after pasting sensitive content to prevent residue leaks. Clearing only — do not use to read.",
            json!({}),
            &[],
        ),
        tool_def(
            "desktop_clipboard_write",
            "Write long text (>500 chars) to clipboard for pasting. Normal text → desktop_input. Clean after pasting. Never for passwords/sensitive data.",
            json_props! {
                "text" => obj!("type"="string","description"="Text to write")
            },
            &["text"],
        ),
    ]
}

/// Browser tools (via nuphus-browser)
fn browser_tools() -> Vec<ToolDef> {
    vec![
        tool_def(
            "browser_navigate",
            "Open URL in browser",
            json_props! {
                "url" => obj!("type"="string","description"="URL to navigate to")
            },
            &["url"],
        ),
        tool_def(
            "browser_snapshot",
            "Text snapshot of visible interactive elements via AX tree: @N [role] \"name\". Use @N refs for click/type. Falls back to DOM traversal if AX unavailable.",
            json_props! {
                "full" => obj!("type"="boolean","default"=false,"description"="Include hidden elements too"),
                "selector" => obj!("type"="string","description"="Scope snapshot to this subtree")
            },
            &[],
        ),
        tool_def(
            "browser_exec",
            "Run multi-step batch script in ONE CDP round trip (form filling, multi-click). Helpers: h.click('@N'|'selector'), h.fill(sel, text), h.scroll(px), h.wait(ms), h.extract(sel), h.snapshot(). Returns [{op, ref, success, detail}] per step.",
            json_props! {
                "script" => obj!("type"="string","description"="JS using window.__nuphus helpers (alias 'h')")
            },
            &["script"],
        ),
        tool_def_with_selector_or_ref(
            "browser_click",
            "Click element by CSS selector or @N ref. Auto-waits for visibility (5s). Left clicks default to JS click (no user activation); trusted=true sends real CDP events. Right/middle clicks always trusted.",
            json_props! {
                "selector" => obj!("type"="string","minLength"=1,"description"="CSS selector or ref ID (e.g. @1, @e0, 'button')"),
                "ref" => obj!("type"="string","minLength"=1,"description"="Ref ID from snapshot (e.g. @1, @e0); alias of selector — provide either one"),
                "trusted" => obj!("type"="boolean","description"="Dispatch real trusted CDP mouse events at the element's center (produces user activation). Default false for left clicks; right and middle clicks are always trusted."),
                "button" => obj!("type"="string","enum"=["left","right","middle"],"default"="left","description"="Mouse button. Right and middle clicks use trusted CDP events automatically."),
                "snapshot" => obj!("type"="boolean","description"="Include a post-click page snapshot. Defaults to true for left clicks and false for right/middle clicks so transient context menus remain open for the next operation.")
            },
            &[],
        ),
        tool_def_with_selector_or_ref(
            "browser_type",
            "Type text into input by CSS selector or @N ref. Auto-waits for visibility (5s).",
            json_props! {
                "selector" => obj!("type"="string","minLength"=1,"description"="CSS selector or ref ID of input field (e.g. @1, @e0)"),
                "ref" => obj!("type"="string","minLength"=1,"description"="Ref ID from snapshot (e.g. @1, @e0); alias of selector — provide either one"),
                "text" => obj!("type"="string","description"="Text to type")
            },
            &["text"],
        ),
        tool_def(
            "browser_press",
            "Press physical key or chord on focused element (click/type first to focus). Supports named keys, single chars, chords (Control+c, Shift+Tab, Meta+ArrowLeft). Does not verify DOM change (terminal/canvas may update outside DOM).",
            json_props! {
                "key" => obj!("type"="string","minLength"=1,"description"="Key or chord to press, e.g. Enter, ArrowUp, Control+c, Shift+Tab, Meta+ArrowLeft"),
                "snapshot" => obj!("type"="boolean","default"=false,"description"="Include a post-key page snapshot. Defaults to false so terminal/canvas state and transient UI are not disturbed.")
            },
            &["key"],
        ),
        tool_def(
            "browser_scroll",
            "Scroll page up/down by N pixels.",
            json_props! {
                "direction" => obj!("type"="string","enum"=["up","down"],"description"="Scroll direction"),
                "amount" => obj!("type"="integer","default"=500,"description"="Pixels to scroll")
            },
            &["direction"],
        ),
        tool_def(
            "browser_extract",
            "Extract readable text from current page (strips nav/ads).",
            json_props! {
                "max_chars" => obj!("type"="integer","default"=8000,"description"="Max characters to extract")
            },
            &[],
        ),
        tool_def(
            "browser_screenshot",
            "Screenshot the current browser page and save it as a PNG file.",
            json_props! {
                "path" => obj!("type"="string","minLength"=1,"description"="Save path for the PNG file; the parent directory must exist")
            },
            &["path"],
        ),
        tool_def(
            "browser_close",
            "Close browser and free resources.",
            json!({}),
            &[],
        ),
        tool_def(
            "browser_evaluate",
            "Execute arbitrary JavaScript in the current page.",
            json_props! {
                "script" => obj!("type"="string","description"="JavaScript code")
            },
            &["script"],
        ),
        tool_def(
            "browser_back",
            "Navigate back in browser history.",
            json!({}),
            &[],
        ),
        tool_def(
            "browser_forward",
            "Navigate forward in browser history.",
            json!({}),
            &[],
        ),
        tool_def(
            "browser_wait_for",
            "Wait for CSS selector to reach a state (up to timeout). Note: click/type already auto-wait 5s; use for custom states or longer delays.",
            json_props! {
                "selector" => obj!("type"="string","description"="CSS selector to wait for"),
                "timeout_ms" => obj!("type"="integer","default"=5000,"description"="Max wait time in ms"),
                "state" => obj!("type"="string","enum"=["attached","visible","hidden"],"default"="attached","description"="Target state")
            },
            &["selector"],
        ),
        tool_def(
            "browser_cookies_get",
            "Get all cookies for the current page.",
            json!({}),
            &[],
        ),
        tool_def(
            "browser_cookies_set",
            "Set a cookie for the current domain.",
            json_props! {
                "name" => obj!("type"="string","description"="Cookie name"),
                "value" => obj!("type"="string","description"="Cookie value"),
                "domain" => obj!("type"="string","description"="Cookie domain (defaults to current page domain)"),
                "path" => obj!("type"="string","description"="Cookie path")
            },
            &["name", "value"],
        ),
        tool_def(
            "browser_import_cookies",
            "Import cookies from user's Chrome profile into current browser session.",
            json_props! {
                "domain" => obj!("type"="string","description"="Optional domain filter")
            },
            &[],
        ),
        tool_def(
            "browser_upload",
            "Upload a file to <input type=file>. Use @N ref or CSS selector.",
            json_props! {
                "selector" => obj!("type"="string","description"="CSS selector or @N ref of file input"),
                "file_path" => obj!("type"="string","description"="Absolute path to the file to upload")
            },
            &["selector", "file_path"],
        ),
        tool_def_with_selector_or_ref(
            "browser_drag_files",
            "Drag local files/dirs onto a browser element (native CDP drag). Unlike browser_upload, no input[type=file] needed.",
            json_props! {
                "selector" => obj!("type"="string","minLength"=1,"description"="CSS selector or ref ID of the drop target (e.g. @1, @e0, '.explorer-viewlet')"),
                "ref" => obj!("type"="string","minLength"=1,"description"="Ref ID from snapshot; alias of selector — provide either one"),
                "file_paths" => obj!("type"="array","items"=obj!("type"="string"),"minItems"=1,"description"="Absolute paths of existing local files or directories to drag")
            },
            &["file_paths"],
        ),
        tool_def(
            "browser_list_downloads",
            "List files in the browser download directory.",
            json!({}),
            &[],
        ),
        tool_def(
            "browser_new_tab",
            "Open new browser tab",
            json_props! {
                "url" => obj!("type"="string","description"="URL to open in new tab")
            },
            &[],
        ),
        tool_def(
            "browser_list_tabs",
            "List all open tabs with indices, URLs, and titles.",
            json!({}),
            &[],
        ),
        tool_def(
            "browser_switch_tab",
            "Switch focus to tab by index.",
            json_props! {
                "index" => obj!("type"="integer","description"="Tab index from list_tabs")
            },
            &["index"],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{is_write_tool, is_write_tool_schema, SecurityPolicy};

    /// Argument samples exercising each runtime classification branch. For
    /// `desktop_mouse` we read the action enum straight from the schema so this stays
    /// in sync if the enum changes.
    fn representative_args(tool: &ToolDef) -> Vec<Value> {
        let mut args = vec![json!({})];
        if tool.name == "desktop_mouse" {
            if let Some(enum_vals) = tool.input_schema["properties"]["action"]
                .get("enum")
                .and_then(Value::as_array)
            {
                for val in enum_vals {
                    if let Some(action) = val.as_str() {
                        args.push(json!({ "action": action }));
                    }
                }
            }
        }
        args
    }

    /// Simulate a spec-compliant MCP client: drop any argument not declared in
    /// `inputSchema.properties` before it reaches the server (this is exactly what
    /// made strict-confirm mode deadlock before the fix — `confirm` was stripped).
    fn strip_to_declared(schema: &Value, args: &Value) -> Value {
        let props = schema.get("properties").and_then(Value::as_object);
        let mut filtered = Map::new();
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                if props.map_or(true, |p| p.contains_key(k)) {
                    filtered.insert(k.clone(), v.clone());
                }
            }
        }
        Value::Object(filtered)
    }

    #[test]
    fn write_tools_declare_confirm() {
        let tools = all_tools();
        assert_eq!(tools.len(), 38, "published docs expect 38 total tools");
        assert_eq!(
            tools
                .iter()
                .filter(|tool| tool.name.starts_with("browser_"))
                .count(),
            23,
            "published docs expect 23 browser tools"
        );
        let write_count = tools
            .iter()
            .filter(|t| is_write_tool_schema(t.name))
            .count();
        // Regression anchor from issue #1 plus browser_drag_files/browser_press: exactly 27 write tools.
        assert_eq!(write_count, 27, "expected 27 write tools");

        for tool in &tools {
            let props = tool.input_schema["properties"]
                .as_object()
                .expect("schema must have properties object");
            let required = tool.input_schema["required"]
                .as_array()
                .expect("schema must have required array");
            let has_confirm = props.contains_key("confirm");

            if is_write_tool_schema(tool.name) {
                assert!(
                    has_confirm,
                    "write tool '{}' must declare `confirm` in inputSchema",
                    tool.name
                );
                assert_eq!(props["confirm"]["type"], "boolean");
                assert!(
                    !required.iter().any(|r| r == "confirm"),
                    "`confirm` must NOT be required for '{}' (absence is what triggers strict-mode rejection)",
                    tool.name
                );
            } else {
                assert!(
                    !has_confirm,
                    "read tool '{}' must not declare `confirm`",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn selector_ref_aliases_match_the_runtime_contract() {
        for (name, other_required) in [
            ("browser_click", &[][..]),
            ("browser_type", &["text"][..]),
            ("browser_drag_files", &["file_paths"][..]),
        ] {
            let tool = all_tools()
                .into_iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("missing schema for {name}"));
            let required = tool.input_schema["required"]
                .as_array()
                .expect("required must be an array");

            assert_eq!(
                required,
                &other_required
                    .iter()
                    .map(|field| json!(field))
                    .collect::<Vec<_>>(),
                "{name}: selector/ref must be alternatives, not top-level requirements"
            );
            assert_eq!(
                tool.input_schema["anyOf"],
                json!([
                    { "required": ["selector"] },
                    { "required": ["ref"] }
                ]),
                "{name}: schema must require selector or ref"
            );
            assert_eq!(tool.input_schema["properties"]["selector"]["minLength"], 1);
            assert_eq!(tool.input_schema["properties"]["ref"]["minLength"], 1);
        }
    }

    #[test]
    fn browser_screenshot_schema_matches_save_only_runtime() {
        let tool = all_tools()
            .into_iter()
            .find(|tool| tool.name == "browser_screenshot")
            .expect("missing browser_screenshot schema");

        assert_eq!(tool.input_schema["required"], json!(["path"]));
        assert_eq!(tool.input_schema["properties"]["path"]["minLength"], 1);
        assert!(tool.description.contains("save"));
    }

    #[test]
    fn browser_press_schema_exposes_key_contract_and_safe_snapshot_default() {
        let tool = all_tools()
            .into_iter()
            .find(|tool| tool.name == "browser_press")
            .expect("missing browser_press schema");

        assert_eq!(tool.input_schema["required"], json!(["key"]));
        assert_eq!(tool.input_schema["properties"]["key"]["minLength"], 1);
        assert_eq!(
            tool.input_schema["properties"]["snapshot"]["default"],
            false
        );
        assert_eq!(
            tool.input_schema["properties"]["confirm"]["type"],
            "boolean"
        );
    }

    /// Anti-drift guard: the schema declaration and the runtime write classification
    /// must agree. A tool declares `confirm` iff the runtime *can* require it for some
    /// argument set — so a future write tool that misses one of the two lists is caught.
    #[test]
    fn confirm_declaration_matches_runtime_predicate() {
        for tool in all_tools() {
            let runtime_write = representative_args(&tool)
                .iter()
                .any(|a| is_write_tool(tool.name, a));
            assert_eq!(
                is_write_tool_schema(tool.name),
                runtime_write,
                "tool '{}': schema `confirm` declaration must agree with runtime write classification",
                tool.name
            );
        }
    }

    /// End-to-end reproduction of issue #1: with a spec-compliant client stripping
    /// undeclared arguments, a write tool must still accept `"confirm": true`, and
    /// reject a write without it — under strict-confirm mode.
    #[test]
    fn strict_confirm_survives_spec_client_schema_filtering() {
        let policy = SecurityPolicy {
            strict_confirm: true,
        };
        let mut write_tools_checked = 0;

        for tool in all_tools() {
            let schema = &tool.input_schema;
            let base = representative_args(&tool)
                .into_iter()
                .find(|a| is_write_tool(tool.name, a))
                .unwrap_or_else(|| json!({}));

            if !is_write_tool_schema(tool.name) {
                continue;
            }

            let mut with_confirm = base.clone();
            if let Value::Object(m) = &mut with_confirm {
                m.insert("confirm".to_string(), Value::Bool(true));
            }
            let stripped = strip_to_declared(schema, &with_confirm);
            assert_eq!(
                stripped.get("confirm").and_then(Value::as_bool),
                Some(true),
                "`confirm` must survive schema stripping for '{}'",
                tool.name
            );
            assert!(
                policy
                    .check_write_confirmation(tool.name, &stripped)
                    .is_ok(),
                "write tool '{}' must pass strict confirmation with confirm:true",
                tool.name
            );

            let stripped_without = strip_to_declared(schema, &base);
            assert!(
                policy
                    .check_write_confirmation(tool.name, &stripped_without)
                    .is_err(),
                "write tool '{}' must be rejected without confirm:true",
                tool.name
            );
            write_tools_checked += 1;
        }

        assert_eq!(
            write_tools_checked, 27,
            "expected all 27 write tools to be exercised"
        );
    }
}