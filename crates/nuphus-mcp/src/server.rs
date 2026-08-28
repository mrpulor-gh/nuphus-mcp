//! MCP Server core: JSON-RPC method dispatch (mirrors the protocol shape of `src/mcp/client.rs`).
//!
//! Supported MCP methods:
//! - `initialize` / `notifications/initialized`
//! - `shutdown` / `exit` (MCP lifecycle)
//! - `tools/list`
//! - `tools/call`
//! - `ping`

use serde_json::{json, Value};

use crate::protocol::{codes, Request, Response, RpcError};
use crate::tools;

/// MCP Server instance (no shared mutable state, reusable across requests).
#[derive(Debug, Default)]
pub struct McpServer {
    /// Whether the initialize handshake has completed
    initialized: bool,
    /// Security policy (strict confirmation mode, etc.)
    policy: crate::security::SecurityPolicy,
    /// MCP lifecycle: a `shutdown` request has been answered (phase 1 of graceful teardown)
    shutdown_received: bool,
    /// MCP lifecycle: an `exit` notification arrived — the stdio loop should terminate
    exit_received: bool,
}

/// Result of a single dispatch: either result or error (never both).
#[derive(Debug)]
enum Dispatched {
    Ok(Value),
    Err(RpcError),
}

impl McpServer {
    /// Default security policy (reads the `NUPHUS_MCP_CONFIRM_WRITE` environment variable).
    pub fn new() -> Self {
        Self {
            initialized: false,
            policy: crate::security::SecurityPolicy::from_env(),
            shutdown_received: false,
            exit_received: false,
        }
    }

    /// Explicitly specify a security policy (test/CLI injection).
    pub fn with_policy(policy: crate::security::SecurityPolicy) -> Self {
        Self {
            initialized: false,
            policy,
            shutdown_received: false,
            exit_received: false,
        }
    }

    /// Whether an `exit` notification has arrived — the stdio loop should terminate.
    pub fn exit_received(&self) -> bool {
        self.exit_received
    }

    /// Process exit code per the MCP lifecycle: 0 when `exit` follows a `shutdown`
    /// request, 1 when the client exits without a graceful shutdown.
    pub fn exit_code(&self) -> u8 {
        if self.shutdown_received {
            0
        } else {
            1
        }
    }

    /// Handle one line of inbound JSON (request or notification), returning the response line to write to stdout.
    /// Notifications (no id member) produce no response; an explicit `"id": null` does.
    pub async fn handle_line(&mut self, line: &str) -> Option<String> {
        let request = match Request::parse(line) {
            Ok(r) => r,
            Err(e) => {
                // Parse/structure failure: the request id cannot be trusted → null
                return Some(Response::err(Value::Null, e).to_line());
            }
        };

        // Notification: id member absent → process but do not respond.
        // An explicit "id": null IS a request (Some(Value::Null)) and gets a response.
        let is_notification = request.id.is_none();
        let id = request.id.unwrap_or(Value::Null);

        let dispatched = self
            .dispatch(&request.method, request.params.unwrap_or(Value::Null))
            .await;

        if is_notification {
            if let Dispatched::Err(err) = &dispatched {
                tracing::warn!(
                    "[mcp] notification '{}' handled with error: {} (code {})",
                    request.method,
                    err.message,
                    err.code
                );
            }
            None
        } else {
            let response = match dispatched {
                Dispatched::Ok(result) => Response::ok(id, result),
                Dispatched::Err(error) => Response::err(id, error),
            };
            Some(response.to_line())
        }
    }

    /// Dispatch an MCP method (async: tools/call may execute desktop/browser operations).
    async fn dispatch(&mut self, method: &str, params: Value) -> Dispatched {
        match method {
            "initialize" => self.initialize(params),
            "notifications/initialized" => Dispatched::Ok(json!({})),
            "ping" => Dispatched::Ok(json!({})),
            // MCP lifecycle: client signals graceful teardown — answer null and
            // wait for the `exit` notification (handled by the main loop).
            "shutdown" => {
                self.shutdown_received = true;
                Dispatched::Ok(Value::Null)
            }
            // MCP lifecycle notification: terminate the process. The main loop
            // reads `exit_received()` after each line and exits with `exit_code()`.
            "exit" => {
                self.exit_received = true;
                Dispatched::Ok(Value::Null)
            }
            "tools/list" => {
                if !self.initialized {
                    return Dispatched::Err(RpcError::new(
                        codes::SERVER_NOT_INITIALIZED,
                        "Server not initialized",
                    ));
                }
                self.tools_list()
            }
            "tools/call" => {
                if !self.initialized {
                    return Dispatched::Err(RpcError::new(
                        codes::SERVER_NOT_INITIALIZED,
                        "Server not initialized",
                    ));
                }
                self.tools_call(params).await
            }
            _ => Dispatched::Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("Method not found: {}", method),
            )),
        }
    }

    // ─────────────────────────── methods ───────────────────────────

    fn initialize(&mut self, _params: Value) -> Dispatched {
        // Always answer with the version this server actually implements.
        // Echoing the client's protocolVersion would falsely claim support for
        // versions we never implemented.
        self.initialized = true;
        Dispatched::Ok(json!({
            "protocolVersion": crate::protocol::PROTOCOL_VERSION,
            "capabilities": {
                "tools": { "listChanged": false }
            },
            "serverInfo": {
                "name": "nuphus-mcp",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": "Nuphus MCP Server — AI computer operation capability (desktop + browser). \
                             desktop_* tools operate the local screen/windows/keyboard/mouse; browser_* tools \
                             operate Chrome (CDP). Activate the target window with desktop_window_activate before window operations.",
        }))
    }

    fn tools_list(&self) -> Dispatched {
        let tools: Vec<Value> = tools::all_tools()
            .into_iter()
            .map(|t| {
                let mut tool = json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                });
                // Security annotations (MCP spec annotations): destructiveHint for write tools / readOnlyHint for read tools
                if let Some(annotations) = crate::security::annotations_for(t.name, &t.input_schema)
                {
                    tool["annotations"] = annotations;
                }
                tool
            })
            .collect();
        Dispatched::Ok(json!({ "tools": tools }))
    }

    async fn tools_call(&self, params: Value) -> Dispatched {
        let name = match params.get("name").and_then(Value::as_str) {
            Some(n) if !n.is_empty() => n,
            _ => {
                return Dispatched::Err(RpcError::new(
                    codes::INVALID_PARAMS,
                    "tools/call: 'name' is required",
                ));
            }
        };
        let args = match params.get("arguments") {
            // Absent (or explicit null) arguments default to an empty object —
            // MCP clients commonly omit `arguments` for parameterless tools.
            None | Some(Value::Null) => json!({}),
            Some(v) if v.is_object() => v.clone(),
            Some(_) => {
                return Dispatched::Err(RpcError::new(
                    codes::INVALID_PARAMS,
                    "tools/call: 'arguments' must be an object",
                ));
            }
        };

        // Security boundary: in strict confirmation mode, write operations must carry confirm:true
        if let Err(msg) = self.policy.check_write_confirmation(name, &args) {
            let mut result = json!({
                "content": [ { "type": "text", "text": msg } ],
            });
            result["isError"] = json!(true);
            return Dispatched::Ok(result);
        }

        match execute_tool_isolated(name.to_owned(), args).await {
            Ok(output) => {
                let mut result = json!({
                    "content": [ { "type": "text", "text": output.text } ],
                });
                if output.is_error {
                    result["isError"] = json!(true);
                }
                Dispatched::Ok(result)
            }
            Err(e) => Dispatched::Err(RpcError::new(codes::INVALID_PARAMS, e)),
        }
    }
}

/// Execute a tool with a panic guard so a panicking tool cannot take down the
/// whole server process (P1 guard). A tool panic is caught by `catch_unwind`
/// and converted into a semantic tool failure (`isError: true`
/// with a sanitized "internal error" message) instead of unwinding through
/// `#[tokio::main]` and killing every connected Agent's server.
///
/// Lock safety: the process-wide tokio Mutex guard and the cross-process file
/// lock guard both live *inside* the guarded future. Unwinding drops that
/// future, so both guards are released by RAII — a panicking tool never wedges
/// the automation locks (covered by `panicking_tool_returns_is_error_and_server_survives`).
///
/// Why `catch_unwind` instead of `tokio::spawn`: spawning would require the
/// tool future to be `Send`, but the desktop input chain holds `!Send` FFI
/// handles across await points on some platforms (e.g. macOS enigo's
/// `NonNull<CGEventSource>`). Catching unwinds on the current task gives the
/// same panic isolation without a `Send` bound.
async fn execute_tool_isolated(name: String, args: Value) -> Result<tools::ToolOutput, String> {
    use futures_util::FutureExt;
    let log_name = name.clone();
    let start = std::time::Instant::now();
    // HUD 可见性协议：执行前显示「▶ 工具+关键参数」，完成后覆盖为结果态。
    // 桌面操作目标多为其它应用的窗口，激活窗口做提示是灾难——HUD 浮条是唯一实时通道。
    desktop_api::hud::show(
        format!("▶ {}", desktop_api::hud::tool_summary(&name, &args)),
        desktop_api::hud::HOLD_EXEC_MS,
    );
    let fut = std::panic::AssertUnwindSafe(tools::execute(&name, &args));
    let result = match fut.catch_unwind().await {
        Ok(res) => res,
        Err(payload) => {
            let detail = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic payload".to_string());
            tracing::error!("[mcp] tool '{}' panicked: {}", log_name, detail);
            Ok(tools::ToolOutput::failure(
                "internal error: tool execution failed unexpectedly; the server is still alive",
            ))
        }
    };
    match &result {
        Ok(out) if !out.is_error => desktop_api::hud::show(
            format!("✓ {} ({}ms)", name, start.elapsed().as_millis()),
            desktop_api::hud::HOLD_DONE_MS,
        ),
        Ok(_) => desktop_api::hud::show(
            format!("⚠ {} failed", name),
            desktop_api::hud::HOLD_DONE_MS,
        ),
        Err(_) => desktop_api::hud::show(
            format!("✗ {} error", name),
            desktop_api::hud::HOLD_DONE_MS,
        ),
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn parse(resp: &str) -> Value {
        serde_json::from_str(resp).expect("response must be valid JSON")
    }

    fn error_of(resp: &str) -> (i32, String) {
        let v = parse(resp);
        let err = v.get("error").expect("response must have error");
        (
            err.get("code").and_then(Value::as_i64).unwrap_or(0) as i32,
            err.get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        )
    }

    #[tokio::test]
    async fn initialize_handshake_returns_protocol_and_capabilities() {
        let mut server = McpServer::new();
        let resp = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"nuphus","version":"0.1.0"}}}"#,
            )
            .await
            .expect("initialize must produce a response");

        let v = parse(&resp);
        assert_eq!(v["id"], 0);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
        assert!(v["result"]["capabilities"]["tools"].is_object());
        assert_eq!(v["result"]["serverInfo"]["name"], "nuphus-mcp");
        assert!(v.get("error").is_none());
    }

    #[tokio::test]
    async fn initialized_notification_produces_no_response() {
        let mut server = McpServer::new();
        let resp = server
            .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .await;
        assert!(resp.is_none(), "notification must not produce a response");
    }

    #[tokio::test]
    async fn ping_returns_empty_result() {
        let mut server = McpServer::new();
        // ping also works before initialize (protocol allows health checks)
        let resp = server
            .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#)
            .await
            .expect("ping must produce a response");
        let v = parse(&resp);
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"], serde_json::json!({}));
        assert!(v.get("error").is_none());
    }

    #[tokio::test]
    async fn tools_list_requires_initialize_first() {
        let mut server = McpServer::new();
        let resp = server
            .handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#)
            .await
            .expect("response expected");
        let (code, _) = error_of(&resp);
        assert_eq!(code, codes::SERVER_NOT_INITIALIZED);
    }

    #[tokio::test]
    async fn tools_list_returns_valid_schemas_after_initialize() {
        let mut server = McpServer::new();
        server
            .handle_line(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#)
            .await;

        let resp = server
            .handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}"#)
            .await
            .expect("response expected");
        let v = parse(&resp);
        assert!(
            v.get("error").is_none(),
            "tools/list must succeed: {}",
            resp
        );

        let tools = v["result"]["tools"]
            .as_array()
            .expect("tools must be array");
        assert!(
            tools.len() >= 10,
            "expected >=10 tools, got {}",
            tools.len()
        );

        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();

        // Required desktop tools
        for required in [
            "desktop_screen_size",
            "desktop_screenshot",
            "desktop_windows_list",
            "desktop_input",
            "desktop_mouse",
        ] {
            assert!(
                names.contains(&required),
                "missing required desktop tool: {}",
                required
            );
        }
        // Required browser tools
        for required in [
            "browser_navigate",
            "browser_snapshot",
            "browser_click",
            "browser_type",
            "browser_press",
            "browser_exec",
        ] {
            assert!(
                names.contains(&required),
                "missing required browser tool: {}",
                required
            );
        }

        // Every tool inputSchema is valid JSON Schema (type=object + properties)
        for t in tools {
            let schema = t.get("inputSchema").expect("inputSchema required");
            assert_eq!(
                schema.get("type").and_then(Value::as_str),
                Some("object"),
                "inputSchema must be type=object for {}",
                t.get("name").and_then(Value::as_str).unwrap_or("?")
            );
            assert!(
                schema
                    .get("properties")
                    .map(Value::is_object)
                    .unwrap_or(false),
                "inputSchema must have properties for {}",
                t.get("name").and_then(Value::as_str).unwrap_or("?")
            );
            let desc = t.get("description").and_then(Value::as_str);
            assert!(
                desc.map(|d| !d.is_empty()).unwrap_or(false),
                "description must be non-empty for {}",
                t.get("name").and_then(Value::as_str).unwrap_or("?")
            );
        }
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_returns_invalid_params_error() {
        let mut server = McpServer::new();
        server
            .handle_line(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#)
            .await;

        let resp = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#,
            )
            .await
            .expect("response expected");
        let (code, msg) = error_of(&resp);
        assert_eq!(code, codes::INVALID_PARAMS);
        assert!(
            msg.contains("no_such_tool"),
            "msg should name the tool: {}",
            msg
        );
    }

    #[tokio::test]
    async fn tools_call_missing_name_returns_error() {
        let mut server = McpServer::new();
        server
            .handle_line(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#)
            .await;

        let resp = server
            .handle_line(r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{}}"#)
            .await
            .expect("response expected");
        let (code, _) = error_of(&resp);
        assert_eq!(code, codes::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn tools_call_desktop_screen_size_returns_result() {
        let mut server = McpServer::new();
        server
            .handle_line(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#)
            .await;

        let resp = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"desktop_screen_size","arguments":{}}}"#,
            )
            .await
            .expect("response expected");
        let v = parse(&resp);
        assert!(
            v.get("error").is_none(),
            "screen_size must succeed: {}",
            resp
        );
        let content = v["result"]["content"][0].clone();
        assert_eq!(content["type"], "text");
        let text = content["text"].as_str().expect("text must be string");
        // Text should be {"width":N,"height":M}
        let parsed: Value = serde_json::from_str(text).expect("result must be JSON");
        assert!(parsed["width"].as_u64().unwrap_or(0) > 0);
        assert!(parsed["height"].as_u64().unwrap_or(0) > 0);
    }

    #[tokio::test]
    async fn malformed_json_returns_parse_error() {
        let mut server = McpServer::new();
        let resp = server
            .handle_line("this is not json")
            .await
            .expect("parse error must produce response");
        let (code, _) = error_of(&resp);
        assert_eq!(code, codes::PARSE_ERROR);
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let mut server = McpServer::new();
        let resp = server
            .handle_line(r#"{"jsonrpc":"2.0","id":7,"method":"resources/list","params":{}}"#)
            .await
            .expect("response expected");
        let (code, _) = error_of(&resp);
        assert_eq!(code, codes::METHOD_NOT_FOUND);
    }

    // ── Security boundary tests ──

    #[tokio::test]
    async fn tools_list_includes_security_annotations() {
        let mut server = McpServer::new();
        server
            .handle_line(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#)
            .await;

        let resp = server
            .handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}"#)
            .await
            .expect("response expected");
        let v = parse(&resp);
        let tools = v["result"]["tools"].as_array().expect("tools array");

        // Write tools marked destructiveHint
        let input = tools
            .iter()
            .find(|t| t["name"] == "desktop_input")
            .expect("desktop_input exists");
        assert_eq!(input["annotations"]["destructiveHint"], true);

        // Read tools marked readOnlyHint
        let size = tools
            .iter()
            .find(|t| t["name"] == "desktop_screen_size")
            .expect("desktop_screen_size exists");
        assert_eq!(size["annotations"]["readOnlyHint"], true);

        // desktop_mouse conservatively marked destructive
        let mouse = tools
            .iter()
            .find(|t| t["name"] == "desktop_mouse")
            .expect("desktop_mouse exists");
        assert_eq!(mouse["annotations"]["destructiveHint"], true);
    }

    #[tokio::test]
    async fn strict_confirm_mode_rejects_write_without_confirm() {
        use crate::security::SecurityPolicy;
        let mut server = McpServer::with_policy(SecurityPolicy {
            strict_confirm: true,
        });
        server
            .handle_line(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#)
            .await;

        // Write operation without confirm → rejected via isError (not actually executed)
        let resp = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"desktop_clipboard_write","arguments":{"text":"x"}}}"#,
            )
            .await
            .expect("response expected");
        let v = parse(&resp);
        assert!(
            v.get("error").is_none(),
            "rejection is not a JSON-RPC error"
        );
        assert_eq!(v["result"]["isError"], true, "must be isError: {}", resp);
        let msg = v["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            msg.contains("confirm"),
            "message must mention confirm: {}",
            msg
        );

        // With confirm → allowed to execute (isError defaults to false, per MCP spec)
        let resp = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"desktop_clipboard_write","arguments":{"text":"ok","confirm":true}}}"#,
            )
            .await
            .expect("response expected");
        let v = parse(&resp);
        let is_err = v["result"]
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        assert!(!is_err, "confirm=true must execute: {}", resp);
        assert!(v["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("written_chars"));

        // Read operations are not subject to confirm
        let resp = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":22,"method":"tools/call","params":{"name":"desktop_screen_size","arguments":{}}}"#,
            )
            .await
            .expect("response expected");
        let v = parse(&resp);
        let is_err = v["result"]
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        assert!(!is_err);
    }

    #[tokio::test]
    async fn default_mode_allows_write_without_confirm() {
        let mut server = McpServer::new(); // default is lenient
        server
            .handle_line(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#)
            .await;

        let resp = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"desktop_clipboard_write","arguments":{"text":"ok"}}}"#,
            )
            .await
            .expect("response expected");
        let v = parse(&resp);
        let is_err = v["result"]
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        assert!(!is_err, "lenient mode allows write: {}", resp);
        assert!(v["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("written_chars"));
    }

    #[tokio::test]
    async fn screenshot_path_traversal_is_rejected() {
        let mut server = McpServer::new();
        server
            .handle_line(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#)
            .await;

        let resp = server
            .handle_line(
                // Forward-slash traversal works on every platform: both Windows and
                // POSIX Path parsers treat '/' as a separator and see the '..' parent
                // component, so it is rejected (a backslash-only path would be a plain
                // filename on POSIX, where '\' is not a separator).
                r#"{"jsonrpc":"2.0","id":40,"method":"tools/call","params":{"name":"desktop_screenshot","arguments":{"path":"C:/Users/../evil.bmp"}}}"#,
            )
            .await
            .expect("response expected");
        let v = parse(&resp);
        assert_eq!(
            v["result"]["isError"],
            json!(true),
            "path traversal must be rejected"
        );
        let msg = v["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            msg.contains("..") || msg.contains("path"),
            "message should mention path: {}",
            msg
        );
    }

    #[tokio::test]
    async fn tools_list_includes_new_vision_and_window_tools() {
        let mut server = McpServer::new();
        server
            .handle_line(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#)
            .await;

        let resp = server
            .handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}"#)
            .await
            .expect("response expected");
        let v = parse(&resp);
        let tools = v["result"]["tools"]
            .as_array()
            .expect("tools must be array");

        let by_name: std::collections::HashMap<&str, &Value> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str).map(|n| (n, t)))
            .collect();

        for required in [
            "desktop_vision",
            "desktop_perceive",
            "desktop_window_move",
            "desktop_window_resize",
            "desktop_window_info",
        ] {
            let t = by_name
                .get(required)
                .unwrap_or_else(|| panic!("missing tool: {}", required));
            // Description must be English (non-empty + no Chinese comment chars; here we only assert non-empty)
            let desc = t.get("description").and_then(Value::as_str).unwrap_or("");
            assert!(
                !desc.is_empty(),
                "description must be non-empty for {}",
                required
            );
            assert!(
                t.get("inputSchema").is_some(),
                "inputSchema required for {}",
                required
            );
        }
    }

    #[tokio::test]
    async fn desktop_vision_missing_key_returns_is_error() {
        let _lock = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Ensure the three vision env vars do not exist (restored by guard after the test)
        let saved: Vec<(String, Option<String>)> = [
            "NUPHUS_MCP_VISION_API_KEY",
            "NUPHUS_MCP_VISION_BASE_URL",
            "NUPHUS_MCP_VISION_MODEL",
        ]
        .iter()
        .map(|k| (k.to_string(), std::env::var(k).ok()))
        .collect();
        for k in [
            "NUPHUS_MCP_VISION_API_KEY",
            "NUPHUS_MCP_VISION_BASE_URL",
            "NUPHUS_MCP_VISION_MODEL",
        ] {
            std::env::remove_var(k);
        }

        let mut server = McpServer::new();
        server
            .handle_line(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#)
            .await;

        let resp = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":50,"method":"tools/call","params":{"name":"desktop_vision","arguments":{"path":"C:/nonexistent.bmp"}}}"#,
            )
            .await
            .expect("response expected");
        let v = parse(&resp);
        assert_eq!(
            v["result"]["isError"],
            json!(true),
            "missing key must be an error: {}",
            resp
        );
        let msg = v["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            msg.contains("NUPHUS_MCP_VISION_API_KEY"),
            "message must name the required env var: {}",
            msg
        );

        for (k, v) in &saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    #[tokio::test]
    async fn desktop_perceive_missing_models_returns_is_error() {
        let _lock = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Point to an empty dir + skip download → fast-fail with a clear error
        let tmp = std::env::temp_dir().join("nuphus_mcp_perceive_missing_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp models dir");
        let saved = std::env::var("NUPHUS_MODELS_DIR").ok();
        let saved_skip = std::env::var("NUPHUS_MCP_NO_MODEL_DOWNLOAD").ok();
        std::env::set_var("NUPHUS_MODELS_DIR", &tmp);
        std::env::set_var("NUPHUS_MCP_NO_MODEL_DOWNLOAD", "1");

        let mut server = McpServer::new();
        server
            .handle_line(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#)
            .await;

        let resp = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":51,"method":"tools/call","params":{"name":"desktop_perceive","arguments":{"path":"C:/nonexistent.bmp"}}}"#,
            )
            .await
            .expect("response expected");
        let v = parse(&resp);
        assert_eq!(
            v["result"]["isError"],
            json!(true),
            "missing models must be an error: {}",
            resp
        );
        let msg = v["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            msg.contains("ch_PP-OCRv4_det.onnx") || msg.contains("model"),
            "message must mention missing models: {}",
            msg
        );

        match saved {
            Some(v) => std::env::set_var("NUPHUS_MODELS_DIR", v),
            None => std::env::remove_var("NUPHUS_MODELS_DIR"),
        }
        match saved_skip {
            Some(v) => std::env::set_var("NUPHUS_MCP_NO_MODEL_DOWNLOAD", v),
            None => std::env::remove_var("NUPHUS_MCP_NO_MODEL_DOWNLOAD"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── P1 panic guard ──

    #[tokio::test]
    async fn panicking_tool_returns_is_error_and_server_survives() {
        let mut server = McpServer::new();
        server
            .handle_line(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#)
            .await;

        // test_panic_tool panics inside tools::execute while holding both the
        // process-level mutex and the cross-process file lock.
        let resp = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":60,"method":"tools/call","params":{"name":"test_panic_tool","arguments":{}}}"#,
            )
            .await
            .expect("panicking tool must still produce a response");
        let v = parse(&resp);
        assert!(
            v.get("error").is_none(),
            "tool panic is a semantic failure, not a JSON-RPC error: {resp}"
        );
        assert_eq!(v["result"]["isError"], json!(true));
        let text = v["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("internal error"),
            "panic detail must be sanitized: {text}"
        );
        assert!(
            !text.contains("intentional test panic"),
            "raw panic message must not leak into tool output: {text}"
        );

        // The server keeps serving, and both automation locks were released by
        // RAII during unwinding — otherwise this call would hang on the process
        // mutex or fail with a bogus "busy" from the leaked file lock.
        let resp = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":61,"method":"tools/call","params":{"name":"desktop_screen_size","arguments":{}}}"#,
            )
            .await
            .expect("server must keep responding after a tool panic");
        let v = parse(&resp);
        assert!(v.get("error").is_none(), "follow-up call works: {resp}");
        assert_ne!(
            v["result"]["isError"],
            json!(true),
            "locks released after panic: {resp}"
        );
    }

    // ── P2 JSON-RPC validation ──

    #[tokio::test]
    async fn wrong_jsonrpc_version_returns_invalid_request() {
        let mut server = McpServer::new();
        let resp = server
            .handle_line(r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#)
            .await
            .expect("response expected");
        let (code, _) = error_of(&resp);
        assert_eq!(code, codes::INVALID_REQUEST);
    }

    #[tokio::test]
    async fn structural_errors_return_invalid_request_not_parse_error() {
        let mut server = McpServer::new();
        // Valid JSON, not a request object.
        let resp = server
            .handle_line(r#"[1,2,3]"#)
            .await
            .expect("response expected");
        assert_eq!(error_of(&resp).0, codes::INVALID_REQUEST);
        // Request object missing "method".
        let resp = server
            .handle_line(r#"{"jsonrpc":"2.0","id":2}"#)
            .await
            .expect("response expected");
        assert_eq!(error_of(&resp).0, codes::INVALID_REQUEST);
        // Missing "jsonrpc" member.
        let resp = server
            .handle_line(r#"{"id":3,"method":"ping"}"#)
            .await
            .expect("response expected");
        assert_eq!(error_of(&resp).0, codes::INVALID_REQUEST);
    }

    #[tokio::test]
    async fn explicit_null_id_is_a_request_and_gets_a_response() {
        let mut server = McpServer::new();
        let resp = server
            .handle_line(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#)
            .await
            .expect("explicit null id must NOT be swallowed as a notification");
        let v = parse(&resp);
        assert_eq!(v["id"], Value::Null);
        assert!(
            v.get("result").is_some(),
            "null-id request answered: {resp}"
        );
    }

    #[tokio::test]
    async fn non_object_arguments_returns_invalid_params() {
        let mut server = McpServer::new();
        server
            .handle_line(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#)
            .await;
        let resp = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":70,"method":"tools/call","params":{"name":"desktop_screen_size","arguments":"not-an-object"}}"#,
            )
            .await
            .expect("response expected");
        let (code, msg) = error_of(&resp);
        assert_eq!(code, codes::INVALID_PARAMS);
        assert!(msg.contains("arguments"), "message names the field: {msg}");
    }

    // ── P2 initialize version ──

    #[tokio::test]
    async fn initialize_answers_with_server_version_not_client_echo() {
        let mut server = McpServer::new();
        let resp = server
            .handle_line(
                r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2099-01-01","capabilities":{},"clientInfo":{"name":"test"}}}"#,
            )
            .await
            .expect("initialize must produce a response");
        let v = parse(&resp);
        assert_eq!(
            v["result"]["protocolVersion"],
            crate::protocol::PROTOCOL_VERSION,
            "server must answer with the version it implements, not echo the client"
        );
    }

    // ── P3 shutdown / exit lifecycle ──

    #[tokio::test]
    async fn shutdown_returns_null_result_and_exit_sets_flag() {
        let mut server = McpServer::new();
        let resp = server
            .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"shutdown"}"#)
            .await
            .expect("shutdown must produce a response");
        let v = parse(&resp);
        assert!(v.get("error").is_none(), "shutdown succeeds: {resp}");
        assert_eq!(v["result"], Value::Null);
        assert!(!server.exit_received());

        let resp = server
            .handle_line(r#"{"jsonrpc":"2.0","method":"exit"}"#)
            .await;
        assert!(resp.is_none(), "exit is a notification: no response");
        assert!(server.exit_received());
        assert_eq!(server.exit_code(), 0, "exit after shutdown is a clean exit");
    }

    #[tokio::test]
    async fn exit_without_shutdown_yields_exit_code_1() {
        let mut server = McpServer::new();
        let resp = server
            .handle_line(r#"{"jsonrpc":"2.0","method":"exit"}"#)
            .await;
        assert!(resp.is_none());
        assert!(server.exit_received());
        assert_eq!(
            server.exit_code(),
            1,
            "exit without a prior shutdown exits with code 1 (per spec)"
        );
    }
}