//! Browser tool executor — reuses the `nuphus-browser` crate (CDP, chromiumoxide).
//!
//! Resident runtime discipline: the browser CDP handler is `tokio::spawn`ed onto the ambient runtime.
//! nuphus-mcp's stdio loop runs on a process-level tokio runtime (lifetime = process),
//! so here we `.await` `get_or_launch` directly (no nested `runtime().block_on`).

use nuphus_browser::{BrowserClient, BrowserError};
use serde_json::Value;
use std::time::Duration;

/// Every browser_* tool that has a dispatch branch in `run_op_with_reconnect`.
///
/// Keep in sync with the match arms below. A tool registered in `schemas::all_tools`
/// without a branch here is a schema/dispatch mismatch that only surfaces at
/// tools/call time as "Unknown browser tool" (regression: 1978fc7 dropped
/// `browser_close` + 3 cookie tools). The `dispatch_matches_schema` test enforces this.
pub const EXECUTABLE_BROWSER_TOOLS: &[&str] = &[
    "browser_navigate",
    "browser_back",
    "browser_forward",
    "browser_snapshot",
    "browser_exec",
    "browser_click",
    "browser_type",
    "browser_press",
    "browser_scroll",
    "browser_screenshot",
    "browser_extract",
    "browser_close",
    "browser_cookies_get",
    "browser_cookies_set",
    "browser_import_cookies",
    "browser_evaluate",
    "browser_wait_for",
    "browser_upload",
    "browser_drag_files",
    "browser_list_downloads",
    "browser_new_tab",
    "browser_list_tabs",
    "browser_switch_tab",
];

/// Allowed navigation URL schemes. Rejects `file://`, `javascript:`, `data:` etc.,
/// which could otherwise read local files or bypass page-context sandboxing.
/// SSRF guard: private/loopback/link-local hosts are refused by default
/// (`NUPHUS_MCP_ALLOW_PRIVATE_NAV=1` is the explicit escape hatch for intranet use).
fn validate_nav_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("url parameter must not be empty".to_string());
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        if private_nav_allowed() {
            return Ok(());
        }
        if let Some(host) = nav_host(url) {
            if host_is_private(&host) {
                return Err(format!(
                    "navigation to private/loopback/link-local host '{host}' is refused by default \
                     (SSRF guard); set NUPHUS_MCP_ALLOW_PRIVATE_NAV=1 to allow intranet targets"
                ));
            }
        }
        Ok(())
    } else {
        Err(format!(
            "unsupported URL scheme (only http/https allowed): {url}"
        ))
    }
}

fn private_nav_allowed() -> bool {
    std::env::var("NUPHUS_MCP_ALLOW_PRIVATE_NAV")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Extract the host portion of an http(s) URL without pulling in a URL crate:
/// strip scheme → strip userinfo → take up to the first `/` → strip port.
fn nav_host(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest)?;
    let authority = after_scheme.split('/').next()?;
    let host_port = authority.rsplit('@').next()?;
    let host = if let Some(stripped) = host_port.strip_prefix('[') {
        // [IPv6]:port
        stripped.split(']').next()?.to_string()
    } else {
        host_port.split(':').next()?.to_string()
    };
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Whether the host is loopback / private / link-local / unspecified, as a
/// literal IP or a localhost-style name. DNS-rebinding via public hostnames is
/// out of scope (that requires resolution-time checks).
fn host_is_private(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    if let Ok(ip) = h.parse::<std::net::Ipv4Addr>() {
        // 127/8, 10/8 + 172.16/12 + 192.168/16, 169.254/16, 0.0.0.0
        return ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified();
    }
    if let Ok(ip6) = h.parse::<std::net::Ipv6Addr>() {
        let seg0 = ip6.segments()[0];
        let link_local = (seg0 & 0xffc0) == 0xfe80; // fe80::/10
        let ula = (seg0 & 0xfe00) == 0xfc00; // fc00::/7
        return ip6.is_loopback() || ip6.is_unspecified() || link_local || ula;
    }
    false
}

/// Optional page-reaction observation window for clicks. This is not proof of
/// target delivery or application success, particularly in iframes/canvas UIs.
fn effect_timeout_ms() -> u64 {
    std::env::var("NUPHUS_MCP_EFFECT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(15_000)
}

/// Resolve the shared `selector` / `ref` tool contract and reject missing or
/// blank values before any page-side effect is attempted.
fn selector_or_ref<'a>(tool: &str, args: &'a Value) -> Result<&'a str, BrowserError> {
    args.get("selector")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            args.get("ref")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
        })
        .ok_or_else(|| {
            BrowserError::Execution(format!("{tool}: selector or ref parameter is required"))
        })
}

/// Execute a browser_* tool, returning a text result.
pub async fn execute(name: &str, args: &Value) -> Result<String, String> {
    if name == "browser_exec" {
        if let super::browser_sequence::Plan::Steps(steps) =
            super::browser_sequence::plan(args).map_err(|e| e.to_string())?
        {
            // Connection has its own bound. Once dispatch starts, the sequence
            // owns the deadline and returns its completed-step journal on expiry.
            let connection_deadline = tokio::time::Instant::now() + Duration::from_secs(25);
            let mut guard =
                tokio::time::timeout_at(connection_deadline, nuphus_browser::get_or_launch_isolated(false))
                    .await
                    .map_err(|_| {
                        "Browser connection timed out before sequence dispatch".to_string()
                    })?
                    .map_err(|e| format!("browser launch failed before sequence dispatch: {e}"))?;
            let client = guard
                .as_mut()
                .ok_or_else(|| "browser client unavailable before sequence dispatch".to_string())?;
            return super::browser_sequence::execute(client, steps)
                .await
                .map_err(|e| e.to_string());
        }
    }
    // Per-operation budget (same policy as the main crate's browser_tools.rs).
    let timeout_secs: u64 = match name {
        "browser_navigate" | "browser_back" | "browser_forward" => 30,
        "browser_exec" => 15,
        "browser_click"
            if args
                .get("observe_change")
                .and_then(Value::as_bool)
                .unwrap_or(false) =>
        {
            15 + effect_timeout_ms() / 1000
        }
        "browser_wait_for" => {
            args.get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(5000)
                / 1000
                + 5
        }
        _ => 15,
    };
    // Outer total budget: one operation + reconnect probe (3s) + Chrome relaunch (~10s) + one retry.
    let total_budget = timeout_secs + 25;
    tokio::time::timeout(
        Duration::from_secs(total_budget),
        run_tool(name, args, timeout_secs),
    )
    .await
    .map_err(|_| format!("Browser '{}' timed out after {}s", name, total_budget))?
}

/// Hold the lock while executing a specific tool (exclusive browser access, avoid interleaving with other consumers).
/// Every operation runs through connection-level self-healing: a dead CDP connection (killed / crashed Chrome)
/// is automatically reset, relaunched and retried once, so a mid-workflow browser death does not turn
/// into a string of user-visible errors.
async fn run_tool(name: &str, args: &Value, timeout_secs: u64) -> Result<String, String> {
    // MCP scenarios use a visible browser window (same as the main crate's browser_tools)
    let mut guard = nuphus_browser::get_or_launch_isolated(false)
        .await
        .map_err(|e| format!("browser launch failed: {e}"))?;
    let client = guard
        .as_mut()
        .ok_or_else(|| "browser client unavailable".to_string())?;

    run_op_with_reconnect(client, name, args, timeout_secs).await
}

/// Tools safe to auto-retry after a reconnect: pure reads with no page-state side
/// effects. Writes (click/type/exec/cookies_set/upload/...) are NOT auto-retried —
/// the command may have reached the browser before the response was lost, and
/// re-issuing it would double-execute. browser_navigate is deliberately excluded:
/// re-navigation can resubmit forms (POST) and destroy in-progress page state.
const READ_ONLY_BROWSER_TOOLS: &[&str] = &[
    "browser_snapshot",
    "browser_extract",
    "browser_list_tabs",
    "browser_cookies_get",
    "browser_screenshot",
    "browser_list_downloads",
];

fn is_read_only_tool(name: &str) -> bool {
    READ_ONLY_BROWSER_TOOLS.contains(&name)
}

/// Run one tool operation with self-healing:
/// - fast failure with a connection-class error → reconnect + retry once (reads only);
/// - hang past the budget → probe liveness; connection dead → reconnect + retry once;
///   connection alive (slow page / heavy events) → return the timeout error unchanged.
/// Non-connection errors are returned unchanged (retrying would mask the real problem).
async fn run_op_with_reconnect(
    client: &mut BrowserClient,
    name: &str,
    args: &Value,
    timeout_secs: u64,
) -> Result<String, String> {
    let timeout = Duration::from_secs(timeout_secs);

    match tokio::time::timeout(timeout, run_op(client, name, args)).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) if BrowserClient::is_connection_error(&e) => {
            if client.is_connection_alive().await {
                return Err(format!(
                    "Browser '{name}' failed with a connection error but the CDP connection is \
                     still responsive ({e}); the page is likely busy or mid-navigation — retry \
                     later"
                ));
            }
            tracing::warn!(
                "[browser] CDP connection failed ({e}); reattaching to the task browser"
            );
            client
                .reconnect()
                .await
                .map_err(|e| format!("browser reconnect failed: {e}"))?;
            if !is_read_only_tool(name) {
                // The write may have executed before the connection dropped; the
                // browser is healthy again, but re-issuing could double-execute.
                return Err(format!(
                    "Browser '{name}' failed with a dead CDP connection ({e}); the browser was \
                     reconnected, but the operation may have already executed before the connection \
                     dropped — verify the page state, then retry manually (writes are not auto-retried)."
                ));
            }
            tokio::time::timeout(timeout, run_op(client, name, args))
                .await
                .map_err(|_| format!("Browser '{name}' retry timed out after {timeout_secs}s"))?
                .map_err(|e| e.to_string())
        }
        Ok(Err(e)) => Err(e.to_string()),
        Err(_elapsed) => {
            // Operation hung past the budget. Distinguish a dead connection from
            // slow-but-healthy work: only reconnect when the probe also fails.
            if client.is_connection_alive().await {
                return Err(format!("Browser '{name}' timed out after {timeout_secs}s"));
            }
            tracing::warn!("[browser] operation timed out after {timeout_secs}s and probe failed; reattaching without killing a live browser");
            client
                .reconnect()
                .await
                .map_err(|e| format!("browser reconnect failed: {e}"))?;
            if !is_read_only_tool(name) {
                return Err(format!(
                    "Browser '{name}' timed out after {timeout_secs}s; the browser was reconnected, \
                     but the operation may have already executed — verify the page state, then \
                     retry manually (writes are not auto-retried)."
                ));
            }
            tokio::time::timeout(timeout, run_op(client, name, args))
                .await
                .map_err(|_| format!("Browser '{name}' retry timed out after {timeout_secs}s"))?
                .map_err(|e| e.to_string())
        }
    }
}

/// Execute a single tool operation against the browser. Returns a connection-class error
/// when the CDP link is dead; `run_op_with_reconnect` turns that into a reconnect + retry.
#[cfg(test)]
mod ui_sequence_tests {
    use super::validate_ui_steps;
    use serde_json::json;

    #[test]
    fn validates_whole_sequence_before_dispatch() {
        assert!(validate_ui_steps(&[]).is_err());
        assert!(validate_ui_steps(&[
            json!({"op":"type","ref":"@1","text":"pwd"}),
            json!({"op":"invalid"})
        ])
        .is_err());
        assert!(validate_ui_steps(&[json!({"op":"press"})]).is_err());
        assert!(validate_ui_steps(&[
            json!({"op":"type","ref":"@1","text":"pwd","mode":"unknown"})
        ])
        .is_err());
        assert!(validate_ui_steps(&[json!({"op":"read","ref":"@1","timeout_ms":8001})]).is_err());
    }

    #[test]
    fn rejects_invalid_tail_before_any_write() {
        let invalid_tails = [
            json!({"op":"read","ref":"@1","timeout_ms":-1}),
            json!({"op":"read","ref":"@1","timeout_ms":"100"}),
            json!({"op":"read","ref":"@1","max_chars":0}),
            json!({"op":"read","ref":"@1","max_chars":20001}),
            json!({"op":"type","ref":"@1","text":"x","clear":"false"}),
            json!({"op":"type","ref":"@1","text":"x","mode":false}),
            json!({"op":"click","ref":"@1","trusted":"true"}),
            json!({"op":"click","ref":"@0"}),
            json!({"op":"click","ref":"@-1"}),
            json!({"op":"read","ref":"#terminal"}),
            json!({"op":"read","ref":"@1","selector":42}),
            json!({"op":"press","key":"Enter","ref":"@1"}),
            json!({"op":"snapshot","unexpected":true}),
        ];
        for tail in invalid_tails {
            assert!(
                validate_ui_steps(&[
                    json!({"op":"type","ref":"@1","text":"must not be dispatched"}),
                    tail.clone(),
                ])
                .is_err(),
                "accepted invalid tail: {tail}"
            );
        }
    }

    #[test]
    fn snapshot_can_only_finish_a_sequence() {
        assert!(
            validate_ui_steps(&[json!({"op":"snapshot"}), json!({"op":"click","ref":"@1"}),])
                .is_err()
        );
        assert!(
            validate_ui_steps(&[json!({"op":"click","ref":"@1"}), json!({"op":"snapshot"}),])
                .is_ok()
        );
    }

    #[test]
    fn accepts_bounded_terminal_sequence() {
        assert!(validate_ui_steps(&[
            json!({"op":"type","ref":"@1","text":"pwd","mode":"paste","clear":false}),
            json!({"op":"press","key":"Enter"}),
            json!({"op":"read","ref":"@1","until_text":"/work","timeout_ms":2000})
        ])
        .is_ok());
    }
}

#[cfg(test)]
fn validate_ui_steps(steps: &[Value]) -> Result<(), BrowserError> {
    super::browser_sequence::validate_steps(steps)
}

async fn run_op(
    client: &mut BrowserClient,
    name: &str,
    args: &Value,
) -> Result<String, BrowserError> {
    client.prepare_shared_settings().await?;
    let identity = client.browser_identity().ok_or(BrowserError::NotStarted)?;
    let exclusive = matches!(name, "browser_cookies_set" | "browser_import_cookies");
    let _browser_guard = crate::automation_lock::coordination()
        .browser(identity, exclusive)
        .map_err(BrowserError::Execution)?;
    let output: String = match name {
        "browser_navigate" => {
            let url = args.get("url").and_then(Value::as_str).unwrap_or("");
            validate_nav_url(url).map_err(BrowserError::Execution)?;
            let result = client.navigate(url).await?;
            // Auto-snapshot after navigation
            match client.snapshot(false, None).await {
                Ok(snap) => format!("{}\n\n── Page state ──\n{}", result, snap),
                Err(e) => format!(
                    "{}\n\n── Note: post-action snapshot failed; page state unavailable for the next step: {} ──",
                    result, e
                ),
            }
        }
        "browser_snapshot" => {
            let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
            let selector = args.get("selector").and_then(Value::as_str);
            client.snapshot(full, selector).await?
        }
        "browser_exec" => match super::browser_sequence::plan(args)? {
            super::browser_sequence::Plan::Steps(steps) => {
                super::browser_sequence::execute(client, steps).await?
            }
            super::browser_sequence::Plan::Script(script) => client.batch_exec(script).await?,
        },
        "browser_click" => {
            let selector = selector_or_ref("browser_click", args)?;
            let trusted = args
                .get("trusted")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let button = args.get("button").and_then(Value::as_str).unwrap_or("left");
            if !matches!(button, "left" | "right" | "middle") {
                return Err(BrowserError::Execution(format!(
                    "browser_click: unsupported button '{button}' (expected left, right, or middle)"
                )));
            }
            let effect_timeout = effect_timeout_ms();
            let verify_effect = args
                .get("observe_change")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && button == "left"
                && effect_timeout > 0;
            if verify_effect {
                client.arm_change_detector().await?;
            }
            let result = if trusted || button != "left" {
                client.click_trusted(selector, button).await?
            } else {
                client.click(selector).await?
            };
            let mut result = format!("{}\nClick dispatched; verify the intended UI state before proceeding. Dispatch alone does not prove the requested application action completed.", result);
            if verify_effect && !client.wait_for_change(effect_timeout).await? {
                result.push_str(&format!("\nNo top-level page change observed within {}ms. This does not determine whether a nested frame, canvas or new tab reacted. Inspect the actual target before retrying.", effect_timeout));
            }
            let include_snapshot = args
                .get("snapshot")
                .and_then(Value::as_bool)
                .unwrap_or(button == "left");
            if !include_snapshot {
                return Ok(result);
            }
            match client.snapshot(false, None).await {
                Ok(snap) => format!("{}\n\n── Page state ──\n{}", result, snap),
                Err(e) => format!(
                    "{}\n\n── Note: post-action snapshot failed; page state unavailable for the next step: {} ──",
                    result, e
                ),
            }
        }
        "browser_type" => {
            let selector = selector_or_ref("browser_type", args)?;
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            let clear = args.get("clear").and_then(Value::as_bool).unwrap_or(true);
            let result = match args.get("mode").and_then(Value::as_str).unwrap_or("insert") {
                "insert" => client.type_text_with_options(selector, text, clear).await?,
                "paste" => client.paste_text_at_ref(selector, text).await?,
                _ => {
                    return Err(BrowserError::Execution(
                        "type mode must be insert or paste".into(),
                    ))
                }
            };
            if !args
                .get("snapshot")
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                return Ok(result);
            }
            match client.snapshot(false, None).await {
                Ok(snap) => format!("{}\n\n── Page state ──\n{}", result, snap),
                Err(e) => format!(
                    "{}\n\n── Note: post-action snapshot failed; page state unavailable for the next step: {} ──",
                    result, e
                ),
            }
        }
        "browser_press" => {
            let key = args
                .get("key")
                .and_then(Value::as_str)
                .filter(|key| !key.trim().is_empty())
                .ok_or_else(|| {
                    BrowserError::Execution("browser_press: key parameter is required".to_string())
                })?;
            let result = client.press_key(key).await?;
            let include_snapshot = args
                .get("snapshot")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !include_snapshot {
                return Ok(result);
            }
            match client.snapshot(false, None).await {
                Ok(snap) => format!("{}\n\n── Page state ──\n{}", result, snap),
                Err(e) => format!(
                    "{}\n\n── Note: post-action snapshot failed; page state unavailable for the next step: {} ──",
                    result, e
                ),
            }
        }
        "browser_scroll" => {
            let direction = args.get("direction").and_then(Value::as_str).unwrap_or("");
            // Clamp instead of `as i32` truncation: a huge u64 would wrap into a
            // negative delta and scroll in the opposite direction.
            let amount = args
                .get("amount")
                .and_then(Value::as_u64)
                .map(|v| v.min(i32::MAX as u64) as i32)
                .unwrap_or(500);
            client.scroll(direction, amount).await?
        }
        "browser_screenshot" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            // Empty path → return a base64 data URI (same contract as the main
            // app, whose client returns inline base64 when no path is given) so
            // the calling Agent can read the image inline; a non-empty path is
            // validated and saved to disk.
            if path.is_empty() {
                client.screenshot(None).await?
            } else {
                let validated = crate::security::validate_screenshot_path(path)
                    .map_err(BrowserError::Execution)?;
                client.screenshot(Some(&validated)).await?
            }
        }
        "browser_extract" => {
            let max_chars = args
                .get("max_chars")
                .and_then(Value::as_u64)
                .unwrap_or(8000) as usize;
            client.extract(max_chars).await?
        }
        "browser_close" => {
            client.close().await?;
            "Browser session disconnected; tab claim released. Shared Chrome and its tabs remain open.".to_string()
        }
        "browser_cookies_get" => {
            let cookies = client.cookies_get().await?;
            serde_json::to_string_pretty(&cookies).unwrap_or_default()
        }
        "browser_cookies_set" => {
            let name = args.get("name").and_then(Value::as_str).unwrap_or("");
            let value = args.get("value").and_then(Value::as_str).unwrap_or("");
            let domain = args.get("domain").and_then(Value::as_str);
            let path = args.get("path").and_then(Value::as_str);
            client.cookies_set(name, value, domain, path).await?
        }
        "browser_import_cookies" => {
            let domain = args.get("domain").and_then(Value::as_str);
            client.import_cookies(domain).await?
        }
        "browser_evaluate" => {
            let script = args.get("script").and_then(Value::as_str).unwrap_or("");
            if let Some(reference) = args.get("ref").and_then(Value::as_str) {
                client.evaluate_at_ref(script, reference).await?.to_string()
            } else {
                client.evaluate(script).await?.to_string()
            }
        }
        "browser_back" => client.back().await?,
        "browser_forward" => client.forward().await?,
        "browser_wait_for" => {
            let selector = args.get("selector").and_then(Value::as_str).unwrap_or("");
            let state = args
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("attached");
            let timeout_ms = args
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(5000);
            client.wait_for(selector, timeout_ms, state).await?
        }
        "browser_upload" => {
            let selector = args.get("selector").and_then(Value::as_str).unwrap_or("");
            let file_path = args.get("file_path").and_then(Value::as_str).unwrap_or("");
            if file_path.is_empty() {
                return Err(BrowserError::Execution(
                    "browser_upload: file_path parameter is required".to_string(),
                ));
            }
            // Security boundary: the file to upload must really exist
            crate::security::validate_upload_file(file_path).map_err(BrowserError::Execution)?;
            client.upload_file(selector, file_path).await?
        }
        "browser_drag_files" => {
            let selector = selector_or_ref("browser_drag_files", args)?;
            let file_paths = args
                .get("file_paths")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    BrowserError::Execution(
                        "browser_drag_files: file_paths must be a non-empty array".to_string(),
                    )
                })?;
            if file_paths.is_empty() {
                return Err(BrowserError::Execution(
                    "browser_drag_files: file_paths must be a non-empty array".to_string(),
                ));
            }
            let mut canonical_paths = Vec::with_capacity(file_paths.len());
            for value in file_paths {
                let path = value.as_str().ok_or_else(|| {
                    BrowserError::Execution(
                        "browser_drag_files: every file_paths entry must be a string".to_string(),
                    )
                })?;
                canonical_paths.push(
                    crate::security::validate_drag_path(path).map_err(BrowserError::Execution)?,
                );
            }
            client.drag_files(selector, &canonical_paths).await?
        }
        "browser_list_downloads" => client.list_downloads()?,
        "browser_new_tab" => {
            let url = match args.get("url").and_then(Value::as_str) {
                Some(u) => {
                    validate_nav_url(u).map_err(BrowserError::Execution)?;
                    Some(u)
                }
                None => None,
            };
            let activate = args
                .get("activate")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            client.new_tab_with_options(url, activate).await?
        }
        "browser_list_tabs" => {
            let tabs = client.list_tabs().await?;
            serde_json::to_string_pretty(&tabs).unwrap_or_default()
        }
        "browser_switch_tab" => {
            let activate = args
                .get("activate")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            match (args.get("target_id"), args.get("index")) {
                (Some(Value::String(id)), None) if !id.is_empty() => {
                    client.switch_tab_by_id(id, activate).await?
                }
                (None, Some(value)) if value.as_u64().is_some() => {
                    let index = usize::try_from(value.as_u64().unwrap())
                        .map_err(|_| BrowserError::Execution("Tab index too large".into()))?;
                    client.switch_tab_with_options(index, activate).await?
                }
                _ => {
                    return Err(BrowserError::Execution(
                        "Provide exactly one of target_id (preferred) or a non-negative index"
                            .into(),
                    ))
                }
            }
        }
        _ => {
            return Err(BrowserError::Execution(format!(
                "Unknown browser tool: {name}"
            )))
        }
    };

    Ok(output)
}

/// Browser availability probe for tests/docs (does not launch; only checks whether Chrome exists)
pub fn chrome_available() -> bool {
    nuphus_browser::find_chrome().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::all_tools;
    use std::collections::HashSet;

    /// Every browser_* tool registered in schemas must have a dispatch branch.
    /// Regression guard for 1978fc7, which silently dropped browser_close and
    /// the three cookie tools from the match while leaving them in tools/list.
    #[test]
    fn dispatch_matches_schema() {
        let registered: HashSet<&str> = all_tools()
            .iter()
            .map(|t| t.name)
            .filter(|n| n.starts_with("browser_"))
            .collect();
        let executable: HashSet<&str> = EXECUTABLE_BROWSER_TOOLS.iter().copied().collect();
        assert_eq!(
            registered, executable,
            "schemas::all_tools browser_* tools must exactly match browser.rs dispatch branches"
        );
    }

    #[test]
    fn retry_whitelist_covers_only_reads() {
        for read in [
            "browser_snapshot",
            "browser_extract",
            "browser_list_tabs",
            "browser_cookies_get",
            "browser_screenshot",
            "browser_list_downloads",
        ] {
            assert!(is_read_only_tool(read), "{read} must be retryable");
        }
        // Writes and navigation are NOT auto-retried (double-execution hazard).
        for write in EXECUTABLE_BROWSER_TOOLS {
            if !READ_ONLY_BROWSER_TOOLS.contains(write) {
                assert!(
                    !is_read_only_tool(write),
                    "{write} must not be auto-retried"
                );
            }
        }
        // Deliberately excluded despite being idempotent-looking (form resubmit).
        assert!(!is_read_only_tool("browser_navigate"));
    }

    #[test]
    fn selector_or_ref_rejects_missing_and_blank_values() {
        let missing = selector_or_ref("browser_click", &serde_json::json!({}))
            .expect_err("missing selector/ref must fail");
        assert_eq!(
            missing.to_string(),
            "Execution error: browser_click: selector or ref parameter is required"
        );

        assert!(selector_or_ref(
            "browser_click",
            &serde_json::json!({"selector": "  ", "ref": ""})
        )
        .is_err());
    }

    #[test]
    fn selector_or_ref_accepts_either_alias_and_prefers_selector() {
        let selector_args = serde_json::json!({"selector": "#submit", "ref": "@2"});
        assert_eq!(
            selector_or_ref("browser_click", &selector_args).unwrap(),
            "#submit"
        );

        let ref_args = serde_json::json!({"ref": "@2"});
        assert_eq!(selector_or_ref("browser_click", &ref_args).unwrap(), "@2");
    }

    #[test]
    fn nav_url_ssrf_guard() {
        let _lock = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var("NUPHUS_MCP_ALLOW_PRIVATE_NAV").ok();
        std::env::remove_var("NUPHUS_MCP_ALLOW_PRIVATE_NAV");

        // Public targets pass.
        assert!(validate_nav_url("https://example.com").is_ok());
        assert!(validate_nav_url("http://8.8.8.8:8080/path").is_ok());
        assert!(validate_nav_url("https://user:pw@example.com/x").is_ok());
        // Scheme guard still applies.
        assert!(validate_nav_url("file:///C:/Windows/win.ini").is_err());
        assert!(validate_nav_url("javascript:alert(1)").is_err());
        // Private / loopback / link-local / metadata hosts are refused.
        for url in [
            "http://169.254.169.254/latest/meta-data",
            "http://10.0.0.1/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://127.0.0.1:9222/json",
            "http://localhost:3000/",
            "http://[::1]/",
            "http://0.0.0.0/",
        ] {
            assert!(
                validate_nav_url(url).is_err(),
                "private host must be refused: {url}"
            );
        }

        // Escape hatch restores intranet access.
        std::env::set_var("NUPHUS_MCP_ALLOW_PRIVATE_NAV", "1");
        assert!(validate_nav_url("http://192.168.1.1/").is_ok());

        match saved {
            Some(v) => std::env::set_var("NUPHUS_MCP_ALLOW_PRIVATE_NAV", v),
            None => std::env::remove_var("NUPHUS_MCP_ALLOW_PRIVATE_NAV"),
        }
    }

    #[test]
    fn nav_host_parsing() {
        assert_eq!(
            nav_host("https://example.com/path"),
            Some("example.com".into())
        );
        assert_eq!(
            nav_host("http://user:pw@127.0.0.1:8080/x"),
            Some("127.0.0.1".into())
        );
        assert_eq!(nav_host("http://[::1]:3000/"), Some("::1".into()));
        assert!(host_is_private("fe80::1"));
        assert!(host_is_private("fd00::1"));
        assert!(!host_is_private("203.0.113.9"));
        assert!(!host_is_private("example.com"));
    }

    /// Effect-verification window defaults to 15s, is overridable via env, and
    /// degrades to the default on garbage input. `0` disables the check.
    #[test]
    fn effect_timeout_default_and_override() {
        let _lock = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var("NUPHUS_MCP_EFFECT_TIMEOUT_MS").ok();

        // Default 15s.
        std::env::remove_var("NUPHUS_MCP_EFFECT_TIMEOUT_MS");
        assert_eq!(effect_timeout_ms(), 15_000);

        // Explicit override.
        std::env::set_var("NUPHUS_MCP_EFFECT_TIMEOUT_MS", "5000");
        assert_eq!(effect_timeout_ms(), 5_000);

        // 0 disables the check.
        std::env::set_var("NUPHUS_MCP_EFFECT_TIMEOUT_MS", "0");
        assert_eq!(effect_timeout_ms(), 0);

        // Garbage / negative / empty fall back to the default.
        for bad in ["abc", "-1", ""] {
            std::env::set_var("NUPHUS_MCP_EFFECT_TIMEOUT_MS", bad);
            assert_eq!(
                effect_timeout_ms(),
                15_000,
                "garbage input {bad:?} must fall back"
            );
        }

        match saved {
            Some(v) => std::env::set_var("NUPHUS_MCP_EFFECT_TIMEOUT_MS", v),
            None => std::env::remove_var("NUPHUS_MCP_EFFECT_TIMEOUT_MS"),
        }
    }
}
