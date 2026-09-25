//! BrowserClient - Rust native CDP browser control
//!
//! Based on chromiumoxide, supports:
//! - Navigate, click, type, scroll
//! - Screenshot, extract content
//! - Login state persistence (--user-data-dir)

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::js_protocol::runtime::RemoteObjectId;
use chromiumoxide::handler::viewport::Viewport;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::{Command, Method, Page};
use futures_util::StreamExt;
use serde::Serialize;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::chrome_finder::{ensure_profile_dir, find_chrome};
use super::ChromeError;

// ═══════════════════════════════════════════════════
// Chrome launch-failure diagnostics
// ═══════════════════════════════════════════════════

/// Bounded stderr tail kept for launch-failure diagnostics.
const CHROME_STDERR_MAX_LINES: usize = 16;
const CHROME_STDERR_MAX_LINE_CHARS: usize = 512;

/// Chrome prints these when the sandbox cannot be initialized (containers,
/// `root` without `--no-sandbox`, missing setuid helper, restricted namespaces).
const CHROME_SANDBOX_ERROR_MARKERS: &[&str] = &[
    "running as root without --no-sandbox",
    "no usable sandbox",
    "suid sandbox helper binary was found",
    "failed to move to new namespace",
    "failed to unshare",
    "sandbox initialization failed",
    "failed to initialize sandbox",
];

/// Chrome prints these when remote debugging is turned off by policy.
const CHROME_POLICY_ERROR_MARKERS: &[&str] = &[
    "remote debugging is disabled by policy",
    "remote debugging has been disabled by the system administrator",
    "devtools remote debugging is disallowed",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChromeLaunchFailureKind {
    SandboxUnavailable,
    RemoteDebuggingBlocked,
    Other,
}

/// Append one stderr line to the bounded tail: control characters are stripped
/// and the line length is capped, so a pathological browser cannot inflate the
/// error message or smuggle terminal escapes into it.
fn push_chrome_stderr(tail: &mut VecDeque<String>, line: &str) {
    let clean: String = line
        .trim()
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .take(CHROME_STDERR_MAX_LINE_CHARS)
        .collect();
    if clean.is_empty() {
        return;
    }
    if tail.len() == CHROME_STDERR_MAX_LINES {
        tail.pop_front();
    }
    tail.push_back(clean);
}

fn classify_chrome_launch_failure(tail: &VecDeque<String>) -> ChromeLaunchFailureKind {
    let stderr = tail
        .iter()
        .map(|line| line.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("\n");

    if CHROME_SANDBOX_ERROR_MARKERS
        .iter()
        .any(|marker| stderr.contains(marker))
        || (stderr.contains("sandbox") && stderr.contains("operation not permitted"))
    {
        return ChromeLaunchFailureKind::SandboxUnavailable;
    }

    if CHROME_POLICY_ERROR_MARKERS
        .iter()
        .any(|marker| stderr.contains(marker))
        || (stderr.contains("remote-debugging") && stderr.contains("policy"))
    {
        return ChromeLaunchFailureKind::RemoteDebuggingBlocked;
    }

    ChromeLaunchFailureKind::Other
}

fn relevant_chrome_stderr(tail: &VecDeque<String>, kind: ChromeLaunchFailureKind) -> Option<&str> {
    tail.iter()
        .rev()
        .find(|line| {
            let line = line.to_ascii_lowercase();
            match kind {
                ChromeLaunchFailureKind::SandboxUnavailable => {
                    CHROME_SANDBOX_ERROR_MARKERS
                        .iter()
                        .any(|marker| line.contains(marker))
                        || line.contains("sandbox")
                        || line.contains("operation not permitted")
                }
                ChromeLaunchFailureKind::RemoteDebuggingBlocked => {
                    CHROME_POLICY_ERROR_MARKERS
                        .iter()
                        .any(|marker| line.contains(marker))
                        || (line.contains("remote-debugging") && line.contains("policy"))
                }
                ChromeLaunchFailureKind::Other => true,
            }
        })
        .map(String::as_str)
        .or_else(|| tail.back().map(String::as_str))
}

/// Build the launch error, turning an opaque "no DevTools endpoint" into an
/// actionable cause when the stderr tail identifies one.
fn chrome_launch_error(reason: &str, tail: &VecDeque<String>) -> BrowserError {
    if !tail.is_empty() {
        tracing::warn!(
            reason,
            stderr_tail = %tail.iter().cloned().collect::<Vec<_>>().join(" | "),
            "Chrome launch failed before exposing a DevTools endpoint"
        );
    }

    let kind = classify_chrome_launch_failure(tail);
    let detail = relevant_chrome_stderr(tail, kind)
        .map(|line| format!(" Chrome stderr: {line}"))
        .unwrap_or_default();
    let message = match kind {
        ChromeLaunchFailureKind::SandboxUnavailable => format!(
            "{reason}. Chrome sandbox initialization failed. Headed browser sessions keep the \
             sandbox enabled and will not retry with --no-sandbox. Run this server in a supported \
             desktop session, or fix the container / enterprise sandbox policy.{detail}"
        ),
        ChromeLaunchFailureKind::RemoteDebuggingBlocked => format!(
            "{reason}. Chrome remote debugging was blocked by a system or enterprise policy. \
             Allow remote debugging for the selected browser, then retry.{detail}"
        ),
        ChromeLaunchFailureKind::Other if !detail.is_empty() => format!("{reason}.{detail}"),
        ChromeLaunchFailureKind::Other => reason.to_string(),
    };
    BrowserError::Launch(message)
}

// ═══════════════════════════════════════════════════
// Custom CDP Command types for domains not covered by chromiumoxide_cdp
// ═══════════════════════════════════════════════════

/// CDP `Accessibility.getFullAXTree` — get the full accessibility tree for the page.
#[derive(Debug, Clone, Serialize)]
struct GetFullAXTree {
    #[serde(skip_serializing_if = "Option::is_none")]
    depth: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "frameId")]
    frame_id: Option<String>,
}

impl Method for GetFullAXTree {
    fn identifier(&self) -> Cow<'static, str> {
        "Accessibility.getFullAXTree".into()
    }
}

impl Command for GetFullAXTree {
    type Response = serde_json::Value;
}

/// CDP `DOM.resolveNode` — resolve a `backendNodeId` to a `RemoteObjectId`.
#[derive(Debug, Clone, Serialize)]
struct DOMResolveNode {
    #[serde(rename = "backendNodeId")]
    backend_node_id: u32,
    #[serde(skip_serializing_if = "Option::is_none", rename = "objectGroup")]
    object_group: Option<String>,
}

impl Method for DOMResolveNode {
    fn identifier(&self) -> Cow<'static, str> {
        "DOM.resolveNode".into()
    }
}

impl Command for DOMResolveNode {
    type Response = serde_json::Value;
}

/// CDP `DOM.querySelector` — find a node by CSS selector within a given node.
#[derive(Debug, Clone, Serialize)]
struct DOMQuerySelector {
    #[serde(rename = "nodeId")]
    node_id: u32,
    selector: String,
}

impl Method for DOMQuerySelector {
    fn identifier(&self) -> Cow<'static, str> {
        "DOM.querySelector".into()
    }
}

impl Command for DOMQuerySelector {
    type Response = serde_json::Value;
}

/// CDP `DOM.describeNode` — get node details by nodeId.
#[derive(Debug, Clone, Serialize)]
struct DOMDescribeNode {
    #[serde(rename = "nodeId")]
    node_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    depth: Option<i32>,
}

impl Method for DOMDescribeNode {
    fn identifier(&self) -> Cow<'static, str> {
        "DOM.describeNode".into()
    }
}

impl Command for DOMDescribeNode {
    type Response = serde_json::Value;
}

/// CDP `Input.insertText` — dispatches real text input event to the focused element.
/// Triggers full event chain (keydown/keypress/beforeinput/input/keyup) that
/// React/Vue controlled components listen to. Unlike setting `this.value` via JS,
/// this is indistinguishable from real user typing.
#[derive(Debug, Clone, Serialize)]
struct InputInsertText {
    text: String,
}

impl Method for InputInsertText {
    fn identifier(&self) -> Cow<'static, str> {
        "Input.insertText".into()
    }
}

impl Command for InputInsertText {
    type Response = serde_json::Value;
}

/// CDP `DOM.enable` — enables DOM agent for querySelector/resolveNode/describeNode.
#[derive(Debug, Clone, Serialize, Default)]
struct DOMEnable {}

impl Method for DOMEnable {
    fn identifier(&self) -> Cow<'static, str> {
        "DOM.enable".into()
    }
}

impl Command for DOMEnable {
    type Response = serde_json::Value;
}

/// CDP `Runtime.callFunctionOn` — call a function with a remote object as `this`.
///
/// This is a custom command that deliberately OMITS `executionContextId`,
/// because CDP requires mutual exclusion between `objectId` and `executionContextId`.
/// chromiumoxide's `evaluate_function` always injects `executionContextId`, so we
/// bypass it and use `page.execute()` directly.
#[derive(Debug, Clone, Serialize)]
struct RuntimeCallFunctionOn {
    #[serde(rename = "functionDeclaration")]
    function_declaration: String,
    #[serde(skip_serializing_if = "Option::is_none", rename = "objectId")]
    object_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "returnByValue")]
    return_by_value: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "awaitPromise")]
    await_promise: Option<bool>,
}

impl Method for RuntimeCallFunctionOn {
    fn identifier(&self) -> Cow<'static, str> {
        "Runtime.callFunctionOn".into()
    }
}

impl Command for RuntimeCallFunctionOn {
    type Response = serde_json::Value;
}

/// CDP `Network.setCookie` — set a cookie with full attributes.
#[derive(Debug, Clone, Serialize)]
struct NetworkSetCookie {
    pub name: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "httpOnly")]
    pub http_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "sameSite")]
    pub same_site: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
}

impl Method for NetworkSetCookie {
    fn identifier(&self) -> Cow<'static, str> {
        "Network.setCookie".into()
    }
}

impl Command for NetworkSetCookie {
    type Response = serde_json::Value;
}

/// CDP `Browser.setDownloadBehavior` — control download behavior and target directory.
#[derive(Debug, Clone, Serialize)]
struct BrowserSetDownloadBehavior {
    pub behavior: String, // "deny", "allow", "allowAndName", "default"
    #[serde(skip_serializing_if = "Option::is_none", rename = "downloadPath")]
    pub download_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "eventsEnabled")]
    pub events_enabled: Option<bool>,
}

impl Method for BrowserSetDownloadBehavior {
    fn identifier(&self) -> Cow<'static, str> {
        "Browser.setDownloadBehavior".into()
    }
}

impl Command for BrowserSetDownloadBehavior {
    type Response = serde_json::Value;
}

/// Minimum viable viewport for reliable automation (screenshot, click, image match).
/// Screens below this threshold force an explicit viewport constrained to screen
/// size; screens at or above it leave the window to the system/native management.
const MIN_AUTOMATION_WIDTH: u32 = 1280;
const MIN_AUTOMATION_HEIGHT: u32 = 720;

/// Detect primary monitor physical size via xcap.
///
/// Falls back to the minimum automation viewport when display enumeration fails
/// (headless/CI/RDP-disconnected environments) instead of panicking.
fn detect_screen_size() -> (u32, u32) {
    let fallback = || {
        tracing::warn!(
            "[Browser] failed to detect screen size (headless environment?), using default {}x{}",
            MIN_AUTOMATION_WIDTH,
            MIN_AUTOMATION_HEIGHT
        );
        (MIN_AUTOMATION_WIDTH, MIN_AUTOMATION_HEIGHT)
    };
    match xcap::Monitor::all() {
        Ok(monitors) => match monitors.into_iter().next() {
            // xcap 0.9 returns Result for width/height — bail to fallback on error.
            Some(primary) => match (primary.width(), primary.height()) {
                (Ok(w), Ok(h)) => (w, h),
                _ => fallback(),
            },
            None => fallback(),
        },
        Err(_) => fallback(),
    }
}

/// Identity of the user-picked external (fingerprint) browser, mirrored from
/// the persisted preference via env (`NUPHUS_BROWSER_NAME` /
/// `NUPHUS_BROWSER_EXE_PATH` / `NUPHUS_BROWSER_USER_DATA_DIR`).
///
/// Fingerprint browsers typically launch with `--remote-debugging-port=0`, so
/// a reopened window listens on a NEW random port and the persisted CDP URL
/// goes stale. With this identity, `attach_external` can locate the running
/// process by exe path, re-resolve its actual debug port, and heal the
/// connection without any user action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIdentity {
    /// Human-readable platform name (e.g. "AdsPower") for error display.
    pub name: String,
    /// Browser executable path — locates the running window process.
    pub exe_path: String,
    /// `--user-data-dir` fallback for DevToolsActivePort resolution when the
    /// process cmdline can't be read.
    pub user_data_dir: Option<String>,
}

/// Browser client
pub struct BrowserClient {
    /// Chrome executable path
    chrome_path: PathBuf,
    /// Profile directory (login state persistence)
    profile_dir: PathBuf,
    /// Browser instance (set after launch)
    browser: Option<Arc<tokio::sync::Mutex<Browser>>>,
    /// Current page
    page: Option<Arc<tokio::sync::Mutex<Page>>>,
    /// Cached backendNodeIds from last AX tree snapshot (index → backendNodeId).
    /// @1 → index 0, @2 → index 1, etc.
    snapshot_backend_ids: Vec<u32>,
    /// Whether the __nuphus helpers have been injected into this page.
    helpers_injected: bool,
    /// Download directory path.
    download_dir: PathBuf,
    /// Whether download behavior has been configured for this session.
    download_configured: bool,
    /// Warning recorded when download-dir configuration failed (bubbled into the
    /// `browser_list_downloads` output — downloads land in Chrome's default dir).
    download_config_warning: Option<String>,
    /// Chromium child process (managed manually to bypass chromiumoxide's
    /// stderr parsing on Windows).
    child_process: Option<chromiumoxide::async_process::Child>,
    /// Mode of the currently running browser instance (`None` = not launched).
    /// Headed mode is a functional superset: a headed instance also serves
    /// headless requests; a headless instance is upgraded on headed request.
    launched_headless: Option<bool>,
    /// External CDP endpoint (e.g. an anti-detect/fingerprint browser started by
    /// the user with `--remote-debugging-port`). Set via the
    /// `NUPHUS_MCP_BROWSER_CDP_URL` environment variable. When set, `launch()`
    /// attaches to this endpoint instead of launching/attaching our own Chrome.
    external_cdp_url: Option<String>,
    /// Identity of the external browser (see [`ExternalIdentity`]); enables
    /// `attach_external` self-healing when the endpoint's port changed.
    external_identity: Option<ExternalIdentity>,
}

/// Interactive ARIA roles to include in the snapshot output.
const INTERACTIVE_ROLES: &[&str] = &[
    "button",
    "link",
    "textbox",
    "combobox",
    "checkbox",
    "radio",
    "switch",
    "menuitem",
    "tab",
    "option",
    "listbox",
    "slider",
    "searchbox",
    "spinbutton",
    "togglebutton",
    "heading",
    "cell",
    "gridcell",
    "row",
    "treeitem",
    "listitem",
    "menu",
    "menubar",
    "toolbar",
    "navigation",
];

/// Default timeout for Playwright-style actionability waits (presence + visible)
/// applied before CSS-path click/type operations.
const ACTIONABILITY_TIMEOUT_MS: u64 = 5000;
/// Poll step of the in-page actionability wait loop (single evaluate round trip,
/// no Rust-side CDP polling).
const ACTIONABILITY_POLL_MS: u64 = 100;
/// CDP budget for the actionability evaluate: the in-page wait loop runs up to
/// `ACTIONABILITY_TIMEOUT_MS` and then throws a descriptive timeout error (with
/// the `browser_snapshot` hint), so the CDP timeout must sit *above* the JS side
/// — otherwise the tokio budget fires first and strips the hint. Only a genuinely
/// wedged CDP call (frozen renderer, mid-navigation) hits this budget.
const ACTIONABILITY_CDP_BUDGET: std::time::Duration =
    std::time::Duration::from_millis(ACTIONABILITY_TIMEOUT_MS + 2000);
/// Retry budget for @N refs whose backend node went stale between snapshot and
/// use (page re-rendered): retries × interval, then the original error surfaces.
const STALE_NODE_RETRIES: u32 = 3;
const STALE_NODE_RETRY_MS: u64 = 200;
/// Bounded wait for the main frame's `load` event inside `Page::goto`.
/// chromiumoxide's navigation-aware `goto` holds the `Page.navigate` response
/// until the frame's `load` lifecycle event (hard `REQUEST_TIMEOUT` = 30s), so a
/// page with a hanging/blocked subresource would hang the whole tool on the
/// outer 30s guard. Kept short on purpose: on a SPA (or any page whose `load`
/// is delayed by blocked subresources) `goto` would otherwise burn the whole
/// budget waiting on an event that never fires. Time out fast, then degrade to
/// polling `document.readyState` — the DOM is usable at "interactive", which is
/// what an automation actually needs; the full `load` adds nothing for the agent.
const NAVIGATE_LOAD_WAIT_SECS: u64 = 5;
/// Fallback deadline (after the load-event wait) for the DOM to become
/// interactive. Combined with NAVIGATE_LOAD_WAIT_SECS it stays well inside the
/// tool-level 30s guard (5s + 12s = 17s).
const NAVIGATE_DOM_READY_SECS: u64 = 12;
/// Upper bound for any single CDP evaluate/execute call. chromiumoxide's
/// `CommandFuture` hard-codes `handler::REQUEST_TIMEOUT` = 30s and ignores
/// `BrowserConfig::request_timeout` for evaluate/execute, so without this a page
/// whose CDP connection is unresponsive (mid-navigation, half-dead renderer)
/// stalls the whole tool for 30s. Wrapping every raw CDP call in [`cdp`] caps
/// each at this budget and fails fast with a clear error instead.
const CDP_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Recursively walk the AX tree node array, collecting interactive nodes.
///
/// Each AXNode has an optional `children` array of nested AXNodes.
/// We traverse the full tree and emit `@N [role] "name"` for interactive nodes.
fn collect_interactive_nodes(
    nodes: &[serde_json::Value],
    backend_ids: &mut Vec<u32>,
    lines: &mut Vec<String>,
) {
    for node in nodes {
        // Check if node is ignored (non-interactive wrapper)
        if node
            .get("ignored")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            // Still traverse children of ignored nodes (they may contain interactive children)
            if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
                collect_interactive_nodes(children, backend_ids, lines);
            }
            continue;
        }

        // Get role
        let role = node
            .get("role")
            .and_then(|v| v.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Get name
        let name = node
            .get("name")
            .and_then(|v| v.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        // Get backendNodeId
        let backend_id = node
            .get("backendDOMNodeId")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);

        // Include if role is interactive and has a backendNodeId
        let interactive_role = !role.is_empty() && INTERACTIVE_ROLES.contains(&role);
        if let Some(backend_id) = backend_id.filter(|_| interactive_role) {
            let idx = backend_ids.len() + 1; // 1-based display index
            backend_ids.push(backend_id);

            let name_display = if name.len() > 60 {
                let boundary = crate::floor_char_boundary(&name, 60);
                format!("{}…", &name[..boundary])
            } else {
                name
            };

            lines.push(format!(
                "@{} [{}] \"{}\"",
                idx,
                role,
                name_display.replace('"', "\\\"")
            ));
        }

        // Recurse into children
        if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
            collect_interactive_nodes(children, backend_ids, lines);
        }
    }
}

/// Resolve a CSS selector to a `backendNodeId` via CDP DOM.querySelector + DOM.describeNode.
async fn resolve_selector_backend_id(page: &Page, selector: &str) -> Result<u32, String> {
    // Step 1: querySelector on the document (nodeId=0)
    let query_cmd = DOMQuerySelector {
        node_id: 0,
        selector: selector.to_string(),
    };
    let query_resp = cdp(page.execute(query_cmd))
        .await
        .map_err(|e| format!("DOM.querySelector failed: {}", e))?;

    let node_id = query_resp
        .result
        .get("nodeId")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| format!("Selector '{}' not found", selector))? as u32;

    if node_id == 0 {
        return Err(format!("Selector '{}' not found (nodeId=0)", selector));
    }

    // Step 2: describeNode to get backendNodeId
    let desc_cmd = DOMDescribeNode {
        node_id,
        depth: Some(1),
    };
    let desc_resp = cdp(page.execute(desc_cmd))
        .await
        .map_err(|e| format!("DOM.describeNode failed: {}", e))?;

    let backend_id = desc_resp
        .result
        .get("node")
        .and_then(|v| v.get("backendNodeId"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "describeNode missing backendNodeId".to_string())?
        as u32;

    Ok(backend_id)
}

/// Search AX tree for node with `target_id` (backendDOMNodeId), return its children as owned Vec.
fn extract_subtree_children(
    nodes: &[serde_json::Value],
    target_id: u32,
) -> Option<Vec<serde_json::Value>> {
    for node in nodes {
        if node
            .get("backendDOMNodeId")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32 == target_id)
            .unwrap_or(false)
        {
            // Found the scope node — return its children (cloned)
            return Some(
                node.get("children")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.to_vec())
                    .unwrap_or_default(),
            );
        }
        // Recurse into children
        if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
            if let Some(found) = extract_subtree_children(&children.to_vec(), target_id) {
                return Some(found);
            }
        }
    }
    None
}

/// Result of re-resolving the external browser's CDP endpoint from its
/// persisted identity (self-healing after the window was reopened on a new port).
#[derive(Debug)]
enum ExternalHeal {
    /// A live CDP endpoint was found and verified (base URL `http://127.0.0.1:{port}`).
    Resolved(String),
    /// No running process matches the identity's exe path — the window is closed.
    ProcessNotFound,
    /// Process found, but no candidate exposes a responsive CDP endpoint.
    PortUnresponsive,
}

/// Extract the debug port and profile dir from a process command line.
/// Handles both `--flag=value` and `--flag value` forms. Port may be 0
/// (= random port chosen by the browser — the actual port is written to
/// DevToolsActivePort in the profile dir; resolve via [`resolve_debug_port`]).
///
/// Mirrors the detect-side parser in src-tauri's `commands/config/preferences.rs`
/// (`parse_cmdline`) — keep semantics in sync (cross-crate; sharing would pull
/// process-scanning into the detect path's iteration model).
fn parse_debug_cmdline(cmd: &[std::ffi::OsString]) -> (Option<u16>, Option<PathBuf>) {
    let args: Vec<String> = cmd
        .iter()
        .map(|a| a.to_string_lossy().trim_matches('"').to_string())
        .collect();
    let mut port = None;
    let mut profile = None;
    for (i, arg) in args.iter().enumerate() {
        if let Some(v) = arg.strip_prefix("--remote-debugging-port=") {
            port = v.parse::<u16>().ok();
        } else if arg == "--remote-debugging-port" {
            port = args.get(i + 1).and_then(|v| v.parse::<u16>().ok());
        } else if let Some(v) = arg.strip_prefix("--user-data-dir=") {
            profile = Some(PathBuf::from(v));
        }
    }
    (port, profile)
}

/// Resolve the effective debug port. A literal port is returned as-is; port 0
/// means the browser picked a random port and wrote it to
/// `<user-data-dir>/DevToolsActivePort` (first line) — how AdsPower launches
/// its SunBrowser. Mirrors src-tauri's `resolve_debug_port`; keep in sync.
fn resolve_debug_port(port: u16, profile: Option<&std::path::Path>) -> Option<u16> {
    if port > 0 {
        return Some(port);
    }
    let content = std::fs::read_to_string(profile?.join("DevToolsActivePort")).ok()?;
    content
        .lines()
        .next()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
}

/// Fallback DevTools endpoint: read `<profile>/DevToolsActivePort` — the file
/// the browser itself writes at the profile root (first line port, second line
/// ws path). Returns the `ws://127.0.0.1:<port><path>` URL only when the file
/// exists AND the port is actually listening. A stale file (browser already
/// exited) must never be treated as healthy — that is what the TCP liveness
/// probe filters out.
async fn dev_tools_active_port_url(profile: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(profile.join("DevToolsActivePort")).ok()?;
    let mut lines = content.lines();
    let port = lines
        .next()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 0)?;
    let ws_path = lines.next().map(str::trim).filter(|s| !s.is_empty())?;
    let url = format!("ws://127.0.0.1:{port}{ws_path}");
    let probe = tokio::time::timeout(
        std::time::Duration::from_millis(1000),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .ok()?;
    if probe.is_err() {
        return None;
    }
    Some(url)
}

/// Whether two executable paths refer to the same file. Case-insensitive on
/// Windows (the persisted path and the running process path may differ in case).
fn same_exe(a: &std::path::Path, b: &std::path::Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

/// Running processes whose exe matches `exe_path`, as (debug-port flag from
/// cmdline, --user-data-dir) pairs.
fn find_identity_processes(exe_path: &str) -> Vec<(Option<u16>, Option<PathBuf>)> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    let target = std::path::Path::new(exe_path);
    sys.processes()
        .values()
        .filter(|p| p.exe().is_some_and(|e| same_exe(e, target)))
        .map(|p| parse_debug_cmdline(p.cmd()))
        .collect()
}

/// Kill the Chrome main process(es) using the given profile dir.
///
/// Used to upgrade a resident headless instance to a headed relaunch: an
/// attached instance is not owned by this process (`child_process` is `None`),
/// so `close()` only drops the CDP connection — the process itself must be
/// terminated explicitly. Only the main process is targeted; child processes
/// (renderer/gpu/utility, which carry `--type=`) die with the parent. A short
/// wait lets the process release its profile locks before a hard relaunch.
fn kill_chrome_for_profile(profile_dir: &std::path::Path) -> Result<(), BrowserError> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

    let profile_key = profile_dir.to_string_lossy().to_lowercase();
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );

    let mut killed_any = false;
    for process in sys.processes().values() {
        let name = process.name().to_string_lossy().to_lowercase();
        if name != "chrome.exe" && name != "msedge.exe" && name != "chromium.exe" {
            continue;
        }
        let cmd: Vec<String> = process
            .cmd()
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // Skip child processes — only the main process owns the profile.
        if cmd.iter().any(|a| a.starts_with("--type=")) {
            continue;
        }
        if cmd.join(" ").to_lowercase().contains(&profile_key) {
            process.kill();
            killed_any = true;
            tracing::info!(
                "[Browser] killed process {:?} for profile upgrade",
                process.pid()
            );
        }
    }

    if !killed_any {
        return Err(BrowserError::Launch(format!(
            "no running Chrome for profile {} found to upgrade",
            profile_dir.display()
        )));
    }

    // Give the killed process a moment to release SingletonLock / SingletonSocket.
    std::thread::sleep(std::time::Duration::from_millis(800));
    Ok(())
}

/// GET `{base}/json/version` (no_proxy, short timeout); returns the browser
/// version string only when the endpoint actually speaks CDP.
async fn probe_cdp_version(base: &str) -> Option<String> {
    let http = reqwest::Client::builder().no_proxy().build().ok()?;
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        http.get(format!("{base}/json/version")).send(),
    )
    .await
    .ok()?
    .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("webSocketDebuggerUrl").and_then(|v| v.as_str())?;
    Some(
        body.get("Browser")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
    )
}

/// Self-healing core: given the persisted identity, locate the running window
/// process, re-resolve its actual debug port (literal cmdline port, or port 0
/// → DevToolsActivePort in the process's --user-data-dir, falling back to the
/// stored user_data_dir), and verify candidates with a no_proxy CDP probe.
async fn heal_external_endpoint(identity: &ExternalIdentity) -> ExternalHeal {
    let candidates = find_identity_processes(&identity.exe_path);
    if candidates.is_empty() {
        return ExternalHeal::ProcessNotFound;
    }

    let mut ports: Vec<u16> = Vec::new();
    for (port_flag, profile) in &candidates {
        let fallback = identity.user_data_dir.as_deref().map(std::path::Path::new);
        let profile = profile.as_deref().or(fallback);
        if let Some(p) = port_flag.and_then(|p| resolve_debug_port(p, profile)) {
            if !ports.contains(&p) {
                ports.push(p);
            }
        }
    }

    for port in ports {
        let base = format!("http://127.0.0.1:{port}");
        if probe_cdp_version(&base).await.is_some() {
            return ExternalHeal::Resolved(base);
        }
    }
    ExternalHeal::PortUnresponsive
}

/// Compose the agent-facing message for a failed external attach. Actionable
/// and anti-misoperation: names the browser, tells the user exactly what to do
/// (reopen the window — the connection then auto-heals), and explicitly forbids
/// switching browsers / changing config / blind retries. Developer detail
/// (`--remote-debugging-port`, raw transport errors) stays in tracing logs.
fn attach_failure_message(
    base: &str,
    identity: Option<&ExternalIdentity>,
    heal: Option<&ExternalHeal>,
) -> String {
    match (identity, heal) {
        (Some(id), Some(ExternalHeal::ProcessNotFound)) => format!(
            "指纹浏览器「{}」当前没有运行中的窗口。请让用户在指纹浏览器平台中打开该窗口——\
             窗口打开后连接会自动恢复。不要切换浏览器、不要修改配置、不要盲目重试。",
            id.name
        ),
        (Some(id), _) => format!(
            "指纹浏览器「{}」的窗口进程正在运行，但调试端口无响应。请让用户在指纹浏览器平台中\
             关闭并重新打开该窗口（确认调试端口已开启）——窗口重开后连接会自动恢复。\
             不要切换浏览器、不要修改配置。",
            id.name
        ),
        (None, _) => format!(
            "无法连接已配置的外部浏览器（{base}）。该配置缺少浏览器身份信息，无法自动恢复。\
             请让用户到 Nuphus 设置页的「浏览器执行环境」中重新检测并选择目标浏览器窗口——\
             重新选择后连接即可恢复。不要切换到内置浏览器、不要盲目重试。"
        ),
    }
}

impl BrowserClient {
    /// Read the external CDP endpoint from the environment (shared by both constructors).
    fn external_cdp_url_from_env() -> Option<String> {
        std::env::var("NUPHUS_MCP_BROWSER_CDP_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
    }

    /// Read the external browser identity from the environment (mirrors the
    /// persisted preference; exe path is the identity key — without it there
    /// is no way to locate the window process, so no identity).
    fn external_identity_from_env() -> Option<ExternalIdentity> {
        fn non_empty(key: &str) -> Option<String> {
            std::env::var(key)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        }
        let exe_path = non_empty("NUPHUS_BROWSER_EXE_PATH")?;
        Some(ExternalIdentity {
            name: non_empty("NUPHUS_BROWSER_NAME").unwrap_or_else(|| "指纹浏览器".to_string()),
            exe_path,
            user_data_dir: non_empty("NUPHUS_BROWSER_USER_DATA_DIR"),
        })
    }

    /// Create new BrowserClient (does not launch browser)
    pub fn new() -> Result<Self, ChromeError> {
        let chrome_path = find_chrome()?;
        let profile_dir = ensure_profile_dir().map_err(ChromeError::Io)?;
        let download_dir = profile_dir.join("downloads");

        Ok(Self {
            chrome_path,
            profile_dir,
            browser: None,
            page: None,
            snapshot_backend_ids: Vec::new(),
            helpers_injected: false,
            download_dir,
            download_configured: false,
            download_config_warning: None,
            child_process: None,
            launched_headless: None,
            external_cdp_url: Self::external_cdp_url_from_env(),
            external_identity: Self::external_identity_from_env(),
        })
    }

    /// Create with specified Chrome path
    pub fn with_chrome(chrome_path: PathBuf) -> Result<Self, ChromeError> {
        let profile_dir = ensure_profile_dir().map_err(ChromeError::Io)?;
        let download_dir = profile_dir.join("downloads");

        Ok(Self {
            chrome_path,
            profile_dir,
            browser: None,
            page: None,
            snapshot_backend_ids: Vec::new(),
            helpers_injected: false,
            download_dir,
            download_configured: false,
            download_config_warning: None,
            child_process: None,
            launched_headless: None,
            external_cdp_url: Self::external_cdp_url_from_env(),
            external_identity: Self::external_identity_from_env(),
        })
    }

    /// Launch browser.
    ///
    /// Idempotent: returns immediately if already launched (connection confirmed alive) and the
    /// current mode satisfies the request. Headed is a functional superset —
    /// a headed instance also serves headless requests; a headless instance receives a headed
    /// request and is closed and relaunched as an upgrade (browser tools require a user-visible window).
    ///
    /// A dead connection (shared Chrome killed by an external process) is NOT diagnosed here:
    /// the liveness probe only answers "definitely alive?". A probe timeout/failure is not proof
    /// of death — CDP event floods (complex or slow-loading pages) or a busy browser can delay
    /// the probe response. Resetting on a false negative would destroy the current page state,
    /// and falling through could even kill a live shared Chrome via lock cleanup + hard relaunch.
    /// Real death is proven only by a failed operation with a connection-class error;
    /// the self-healing caller (nuphus-mcp's `run_op_with_reconnect`, built on
    /// [`Self::reconnect`] + [`Self::is_connection_error`]) then resets, relaunches
    /// and retries the operation once.
    pub async fn launch(&mut self, headless: bool) -> Result<(), BrowserError> {
        // External CDP endpoint configured (e.g. a user-started anti-detect /
        // fingerprint browser): attach to it and NEVER launch our own Chrome.
        // Falling back silently would run automations in the wrong browser while
        // the user believes the fingerprint browser is being driven — so any
        // attach failure here is a hard, explicit error.
        if self.external_cdp_url.is_some() {
            return self.attach_external().await;
        }

        // Idempotency + liveness probe. self.browser may be Some while the underlying CDP
        // handler is gone (shared Chrome killed by an external process); without any check we
        // would return Ok(()) and every later call would fail with "receiver is gone". The
        // probe confirms the happy path cheaply; a non-confirmation leaves the existing
        // connection in place and lets the operation itself surface a connection error.
        if self.browser.is_some() {
            if self.is_connection_alive().await {
                let upgrade = self.launched_headless == Some(true) && !headless;
                if !upgrade {
                    return Ok(()); // Already launched, current mode satisfies the request
                }
                self.close().await?;
            } else {
                tracing::debug!(
                    "[Browser] liveness probe did not confirm alive; trusting existing connection"
                );
                return Ok(());
            }
        }

        // Attach first: if a Chrome with a debugging port is already running for the same profile (an in-app
        // existing instance, leftovers from a previous crash, or another Nuphus process), connect and reuse it —
        // only one Chrome instance per profile is allowed at a time, so a hard launch would inevitably fail.
        if self.try_attach().await.is_ok() {
            // A headed request must not ride a headless instance: web_extract / cookies CDP may
            // have left a headless Chrome resident in this profile (attached instances are not
            // owned here — close() only drops the connection, the process survives). Probe the
            // user agent and upgrade: kill the process, then fall through to the headed launch.
            if !headless && self.instance_is_headless().await.unwrap_or(false) {
                tracing::info!(
                    "[Browser] attached to headless instance; upgrading to headed (profile={})",
                    self.profile_dir.display()
                );
                self.close().await?;
                kill_chrome_for_profile(&self.profile_dir)?;
                // close() cleared the local connection; DevToolsActivePort may still point at
                // the (now dead) instance — attach_target_alive() below detects the dead port
                // and proceeds to a hard headed launch.
            } else {
                return Ok(());
            }
        }

        // Before deleting locks, confirm the attach target is actually dead: if DevToolsActivePort
        // still points to a live instance, removing SingletonLock/SingletonSocket would break its
        // singleton state, and a hard launch on the same profile would exit with code 21. Retry
        // attach once against the live instance; only delete locks when the instance is dead.
        if self.attach_target_alive().await {
            if self.try_attach().await.is_ok() {
                return Ok(());
            }
            return Err(BrowserError::Launch(
                "DevToolsActivePort points to a live Chrome instance but CDP attach \
                 keeps failing; refusing to delete its profile locks"
                    .into(),
            ));
        }

        // Clean up stale lock files that can cause Chrome exit code 21
        for lock_name in &["lockfile", "SingletonLock", "SingletonSocket"] {
            let lock_path = self.profile_dir.join(lock_name);
            if lock_path.exists() {
                let _ = std::fs::remove_file(&lock_path);
            }
        }

        // Viewport strategy:
        //   Screen ≥ 1280×720 → leave to system/browser (no override)
        //   Screen < 1280×720 → force viewport = screen resolution (constrained fit)
        let (w, h) = detect_screen_size();
        let viewport = if w < MIN_AUTOMATION_WIDTH || h < MIN_AUTOMATION_HEIGHT {
            Some(Viewport {
                width: w,
                height: h,
                device_scale_factor: None,
                emulating_mobile: false,
                is_landscape: w >= h,
                has_touch: false,
            })
        } else {
            None
        };
        let mut config_builder = BrowserConfig::builder()
            .chrome_executable(self.chrome_path.clone())
            .user_data_dir(self.profile_dir.clone())
            // no_sandbox: required for Chrome headless mode in certain environments
            // (e.g. containerized/CI runners or restrictive kernel configs).
            // Risk mitigation: Nuphus enforces CSP restrictions and only navigates
            // to user-specified URLs; arbitrary web browsing is not exposed.
            .no_sandbox()
            .viewport(viewport);

        if headless {
            config_builder = config_builder.new_headless_mode();
        } else {
            config_builder = config_builder.with_head();
        }

        // ── Launch arguments ──
        // Common flags, present in both modes. `--disable-blink-features=AutomationControlled`
        // stops Chrome from exposing the CDP automation state (`navigator.webdriver`), which a
        // real user's browser never sets — the single most flaggable signature of automation.
        config_builder = config_builder
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-blink-features=AutomationControlled")
            .arg("--disable-popup-blocking") // keep `window.open` flows from being lost mid-workflow
            .arg("--metrics-recording-only")
            .arg("--safebrowsing-disable-auto-update");

        // Headed mode is a real, user-visible Chrome: keep its fingerprint indistinguishable from
        // a normal install (real GPU/WebGL, extensions present, background features on). The
        // flags below are headless-only stability/perf optimizations that leak automation
        // signals when carried into headed mode.
        if headless {
            config_builder = config_builder
                .arg("--disable-default-apps")
                .arg("--disable-translate")
                .arg("--disable-extensions")
                .arg("--disable-gpu")
                .arg("--disable-background-networking")
                .arg("--disable-sync")
                .arg("--disable-background-timer-throttling")
                .arg("--disable-backgrounding-occluded-windows")
                .arg("--disable-renderer-backgrounding")
                .arg("--disable-features=TranslateUI");
        }

        let config = config_builder
            .build()
            .map_err(|e| BrowserError::Config(e.to_string()))?;

        // ── Manual process launch + stderr parsing ──
        // chromiumoxide 0.9.1's ws_url_from_output uses futures::io::BufReader
        // which has compatibility issues with tokio pipes on Windows + Chrome 150.
        // We bypass Browser::launch entirely: spawn Chrome ourselves, read stderr
        // with pure tokio, then connect via Browser::connect.
        use tokio::io::AsyncBufReadExt;

        let mut child = config
            .launch()
            .map_err(|e| BrowserError::Launch(format!("Chrome spawn failed: {e}")))?;

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| BrowserError::Launch("no stderr pipe".into()))?;
        let inner_stderr = stderr.into_inner(); // tokio::process::ChildStderr
        let mut reader = tokio::io::BufReader::new(inner_stderr);

        // Resolve the DevTools endpoint from stderr, with a DevToolsActivePort
        // fallback. A premature stderr EOF/timeout must NOT be treated as launch
        // failure: on Windows, Edge/Chrome can close (re-target) its stderr pipe
        // during startup while the browser keeps running and has already written
        // DevToolsActivePort (issue#2: healthy Edge, healthy port, nuphus saw EOF).
        let mut stderr_tail: VecDeque<String> = VecDeque::with_capacity(CHROME_STDERR_MAX_LINES);
        let mut line = String::new();
        let mut parse_detail = String::new();
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(20));
        tokio::pin!(timeout);

        let ws_url_from_stderr: Option<String> = loop {
            tokio::select! {
                _ = &mut timeout => {
                    parse_detail = "timeout waiting for DevTools URL".to_string();
                    break None;
                }
                result = reader.read_line(&mut line) => {
                    match result {
                        Ok(0) => {
                            parse_detail =
                                "stderr closed before the DevTools URL line appeared".to_string();
                            break None;
                        }
                        Ok(_) => {
                            push_chrome_stderr(&mut stderr_tail, &line);
                            if let Some(url) = line.trim().strip_prefix("DevTools listening on ") {
                                break Some(url.to_string());
                            }
                            line.clear();
                        }
                        Err(e) => {
                            parse_detail = format!("stderr read error: {e}");
                            break None;
                        }
                    }
                }
            }
        };

        let ws_url = match ws_url_from_stderr {
            Some(url) => url,
            None => {
                // Fallback: the browser writes DevToolsActivePort itself even when
                // the stderr line never reaches our pipe. If the file already exists
                // but the port is dead, fail fast (browser gone); otherwise poll
                // briefly for it to appear (browser still starting up).
                let fallback = async {
                    if self.profile_dir.join("DevToolsActivePort").exists() {
                        return dev_tools_active_port_url(&self.profile_dir).await;
                    }
                    for _ in 0..20 {
                        if let Some(url) = dev_tools_active_port_url(&self.profile_dir).await {
                            return Some(url);
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                    None
                }
                .await;

                match fallback {
                    Some(url) => url,
                    None => {
                        // No fallback: clean up the child explicitly (do not rely on
                        // kill_on_drop alone) and report a *classified* failure — the
                        // stderr tail separates sandbox / enterprise-policy causes from
                        // the generic case instead of dumping a raw tail.
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        return Err(chrome_launch_error(
                            &format!(
                                "Chrome did not expose a DevTools endpoint: {parse_detail}; \
                                 DevToolsActivePort fallback failed too"
                            ),
                            &stderr_tail,
                        ));
                    }
                }
            }
        };

        // Connect to Chrome via the resolved WebSocket URL (stderr line or
        // DevToolsActivePort fallback)
        let (browser, mut handler) = Browser::connect(&ws_url)
            .await
            .map_err(|e| BrowserError::Launch(format!("CDP connect failed: {e}")))?;

        // Start handler running in background
        tokio::spawn(async move { while handler.next().await.is_some() {} });

        self.child_process = Some(child);
        self.browser = Some(Arc::new(Mutex::new(browser)));
        self.launched_headless = Some(headless);
        Ok(())
    }

    /// Probe whether the connected Chrome instance runs headless.
    ///
    /// CDP `Browser.getVersion` reports a user agent of `HeadlessChrome/…` for
    /// headless launches vs `Chrome/…` for headed ones. Used to upgrade a
    /// resident headless instance (left behind by web_extract / cookies CDP)
    /// when a headed request needs a user-visible window. `None`/failure on the
    /// probe is treated as headed — never block a headed session on a probe.
    async fn instance_is_headless(&self) -> Result<bool, BrowserError> {
        let Some(browser_arc) = self.browser.as_ref() else {
            return Ok(false);
        };
        let browser = browser_arc.lock().await;
        let version = browser
            .version()
            .await
            .map_err(|e| BrowserError::Launch(format!("headless probe failed: {e}")))?;
        Ok(version.user_agent.contains("HeadlessChrome"))
    }

    /// Try to attach to a Chrome instance already running for the same profile.
    ///
    /// Chrome started with `--remote-debugging-port` writes `DevToolsActivePort`
    /// at the profile root (first line port, second line ws path). A successful attach avoids
    /// a second launch's profile-lock conflict (tests reusing the app instance, the app reusing
    /// a crashed leftover instance after restart, etc.). Any failure (file missing / port stale /
    /// connection timeout) silently falls back to the launch path.
    async fn try_attach(&mut self) -> Result<(), BrowserError> {
        let port_file = self.profile_dir.join("DevToolsActivePort");
        let content = std::fs::read_to_string(&port_file)
            .map_err(|e| BrowserError::Launch(format!("no attachable instance: {e}")))?;
        let mut lines = content.lines();
        let port = lines
            .next()
            .and_then(|l| l.trim().parse::<u16>().ok())
            .ok_or_else(|| BrowserError::Launch("DevToolsActivePort: bad port".into()))?;
        let ws_path = lines
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| BrowserError::Launch("DevToolsActivePort: missing ws path".into()))?;
        let ws_url = format!("ws://127.0.0.1:{port}{ws_path}");

        let (browser, mut handler) =
            tokio::time::timeout(std::time::Duration::from_secs(3), Browser::connect(&ws_url))
                .await
                .map_err(|_| BrowserError::Launch("attach timed out".into()))?
                .map_err(|e| BrowserError::Launch(format!("attach connect failed: {e}")))?;

        // Start handler running in background (same lifetime as the launch path)
        tokio::spawn(async move { while handler.next().await.is_some() {} });

        tracing::info!(
            "[Browser] attached to running Chrome instance (port={})",
            port
        );
        let browser_arc = Arc::new(Mutex::new(browser));
        self.browser = Some(browser_arc.clone());
        self.child_process = None; // attached instance does not belong to this process; close must not kill it
        self.launched_headless = None; // mode unknown; do not trigger a headless→headed upgrade restart

        // A2: actively pull existing targets after attaching. chromiumoxide only tracks
        // pages created after the connection is established; without fetch_targets,
        // list_tabs/switch_tab would return an empty list for pre-existing tabs. Failure
        // is warn-only — the connection is already established; missing targets only
        // mean an empty tab list and should not fail the whole attach.
        {
            let mut browser_guard = browser_arc.lock().await;
            match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                browser_guard.fetch_targets(),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(
                    "[Browser] fetch_targets after attach failed (existing tabs may be missing): {e}"
                ),
                Err(_) => tracing::warn!(
                    "[Browser] fetch_targets after attach timed out (existing tabs may be missing)"
                ),
            }
        }

        // A3: adopt the first existing page as the current tab (refreshing a leftover /
        // orphan instance's page) so this session reuses the old tab instead of letting
        // navigate's new_page fallback accumulate fresh tabs on every reattach.
        self.adopt_existing_page(&browser_arc, true).await;

        Ok(())
    }

    /// Attach to the external CDP endpoint configured via `NUPHUS_MCP_BROWSER_CDP_URL`
    /// (e.g. `http://127.0.0.1:9222` of a user-started anti-detect/fingerprint browser).
    ///
    /// Discovers the browser-level WebSocket URL via the endpoint's `GET /json/version`,
    /// then connects like `try_attach` does. The attached instance belongs to the user:
    /// `child_process` stays `None` so `close` never kills it, and `launched_headless`
    /// stays `None` so no headless→headed upgrade restart is attempted. Failures are
    /// hard errors (no fallback to a managed Chrome — see `launch`).
    async fn attach_external(&mut self) -> Result<(), BrowserError> {
        let base = self
            .external_cdp_url
            .clone()
            .expect("attach_external called without external_cdp_url");

        // Idempotency: a live connection serves all later calls.
        if self.browser.is_some() && self.is_connection_alive().await {
            return Ok(());
        }

        // First try the configured endpoint. Developer-facing detail goes to
        // logs; the error surfaced to the agent is composed at the end
        // (actionable guidance, anti-misoperation).
        if let Err(first_err) = self.try_attach_external(&base).await {
            tracing::warn!("[Browser] external attach to {base} failed: {first_err}");

            let identity = match &self.external_identity {
                Some(id) => id.clone(),
                // Legacy config (URL only, no identity): no self-heal possible —
                // guide the user to re-pick the window in the settings page.
                None => {
                    return Err(BrowserError::Launch(attach_failure_message(
                        &base, None, None,
                    )));
                }
            };

            // Self-heal: the window may have been reopened on a new (random)
            // debug port — locate the process by exe path, re-resolve the port
            // and retry once. NEVER falls back to a managed Chrome.
            match heal_external_endpoint(&identity).await {
                ExternalHeal::Resolved(new_base) => {
                    if new_base != base {
                        tracing::info!(
                            "[Browser] external endpoint self-healed: {base} -> {new_base}"
                        );
                        self.external_cdp_url = Some(new_base.clone());
                    }
                    match self.try_attach_external(&new_base).await {
                        Ok(()) => Ok(()),
                        Err(e) => {
                            tracing::warn!(
                                "[Browser] attach to healed endpoint {new_base} failed: {e}"
                            );
                            Err(BrowserError::Launch(attach_failure_message(
                                &base,
                                Some(&identity),
                                Some(&ExternalHeal::PortUnresponsive),
                            )))
                        }
                    }
                }
                outcome => Err(BrowserError::Launch(attach_failure_message(
                    &base,
                    Some(&identity),
                    Some(&outcome),
                ))),
            }
        } else {
            Ok(())
        }
    }

    /// One attach attempt against `base`: discover the browser-level ws URL via
    /// `GET {base}/json/version`, then connect like `try_attach` does.
    async fn try_attach_external(&mut self, base: &str) -> Result<(), BrowserError> {
        // Discover the browser-level ws URL via the CDP HTTP endpoint.
        // no_proxy: a CDP endpoint is an infrastructure address (loopback or an
        // explicit host) and must never be routed through the system proxy — a
        // proxy would hijack the request and break discovery with opaque errors.
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .map_err(|e| BrowserError::Launch(format!("http client build failed: {e}")))?;
        let version_url = format!("{base}/json/version");
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            http.get(&version_url).send(),
        )
        .await
        .map_err(|_| {
            BrowserError::Launch(format!(
                "external browser at {base} did not answer /json/version within 5s — \
                 is it running with --remote-debugging-port?"
            ))
        })?
        .map_err(|e| {
            BrowserError::Launch(format!(
                "external browser at {base} unreachable: {e} — start it with \
                 --remote-debugging-port and check NUPHUS_MCP_BROWSER_CDP_URL"
            ))
        })?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BrowserError::Launch(format!("{base}/json/version: invalid JSON: {e}")))?;
        let ws_url = body
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                BrowserError::Launch(format!("{base}/json/version: missing webSocketDebuggerUrl"))
            })?
            .to_string();

        let (browser, mut handler) =
            tokio::time::timeout(std::time::Duration::from_secs(5), Browser::connect(&ws_url))
                .await
                .map_err(|_| {
                    BrowserError::Launch(format!("external browser ws connect timed out: {ws_url}"))
                })?
                .map_err(|e| {
                    BrowserError::Launch(format!("external browser ws connect failed: {e}"))
                })?;

        // Start handler running in background (same lifetime as the launch path)
        tokio::spawn(async move { while handler.next().await.is_some() {} });

        tracing::info!("[Browser] attached to external browser ({})", base);
        let browser_arc = Arc::new(Mutex::new(browser));
        self.browser = Some(browser_arc.clone());
        self.child_process = None; // external instance belongs to the user; close must not kill it
        self.launched_headless = None; // mode unknown; no headless→headed upgrade restart

        // A2: actively pull existing targets after attaching (same rationale as try_attach;
        // failure is warn-only — the connection itself is established).
        {
            let mut browser_guard = browser_arc.lock().await;
            match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                browser_guard.fetch_targets(),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(
                    "[Browser] fetch_targets after external attach failed (existing tabs may be missing): {e}"
                ),
                Err(_) => tracing::warn!(
                    "[Browser] fetch_targets after external attach timed out (existing tabs may be missing)"
                ),
            }
        }

        // A3: adopt the first existing page as the current tab. External/user browser:
        // adopt without reloading so the user's current page is never disturbed.
        self.adopt_existing_page(&browser_arc, false).await;

        Ok(())
    }

    /// Probe whether the existing CDP connection is still alive.
    ///
    /// Lightweight probe: issue one browser-level read-only command (`version`,
    /// `Browser.getVersion`) to the background handler. When the connection is dead
    /// the handler loop has already exited (receiver gone), so the send or rx.await
    /// necessarily fails; if the handler is still running but the ws is broken, its
    /// internal submit_command panics and drops tx, and rx.await returns an error the
    /// same way (the panic does not cross the handler task boundary). A timeout guards
    /// against a half-dead connection blocking indefinitely. Any failure or timeout is
    /// treated as an unusable connection.
    ///
    /// ⚠ Do NOT use `fetch_targets` here: chromiumoxide's `Target.getTargets` response
    /// handler re-creates every existing target (`on_target_created` → `targets.insert`),
    /// which overwrites the live `Target` entries and drops their `PageHandle`s — the
    /// command channel every existing `Page` sends on. A probe on an established
    /// connection would therefore kill every open page while still returning "alive".
    pub async fn is_connection_alive(&self) -> bool {
        let browser_arc = match self.browser.as_ref() {
            Some(arc) => arc,
            None => return false,
        };
        let browser_guard = browser_arc.lock().await;
        match tokio::time::timeout(std::time::Duration::from_secs(3), browser_guard.version()).await
        {
            Ok(Ok(_)) => true,
            Ok(Err(e)) => {
                tracing::debug!("[Browser] liveness probe failed: {e}");
                false
            }
            Err(_) => {
                tracing::debug!("[Browser] liveness probe timed out");
                false
            }
        }
    }

    /// Probe whether the DevToolsActivePort port is still listened on by a live instance.
    ///
    /// try_attach has already failed at this point; this is a light TCP probe: port
    /// connectable → instance alive (usually a transient attach failure, locks must not
    /// be deleted); file missing / parse failure / port unreachable → instance dead
    /// (crash leftover), locks may be safely deleted for a hard launch.
    async fn attach_target_alive(&self) -> bool {
        let port_file = self.profile_dir.join("DevToolsActivePort");
        let content = match std::fs::read_to_string(&port_file) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let port = match content
            .lines()
            .next()
            .and_then(|l| l.trim().parse::<u16>().ok())
        {
            Some(p) => p,
            None => return false,
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .map(|res| res.is_ok())
        .unwrap_or(false)
    }

    /// Navigate to URL
    pub async fn navigate(&mut self, url: &str) -> Result<String, BrowserError> {
        // Helpers are lost on any navigation (including failed ones that may
        // have partially loaded a new page). Reset early so batch_exec re-injects.
        self.helpers_injected = false;
        // @N refs point at the pre-navigation page's backendNodeIds — clear them
        // so a stale ref can never click the wrong element on the new page.
        self.snapshot_backend_ids.clear();

        let page = self.get_or_create_page().await?;
        let page_guard = page.lock().await;

        let before_url = cdp(page_guard.url())
            .await
            .unwrap_or_default()
            .unwrap_or_else(|| "about:blank".to_string());

        // chromiumoxide's `Page::goto` is navigation-aware: the `Page.navigate`
        // command response is held until the main frame's `load` lifecycle event
        // (handler `on_target_message` → `NavigationInProgress`), with a hard
        // `REQUEST_TIMEOUT` (30s) deadline. A page with a hanging/blocked
        // subresource never fires `load`, so goto would otherwise hang the whole
        // tool on the 30s guard with a confusing generic timeout. Bound it
        // ourselves; if the load event is blocked, degrade to polling
        // `document.readyState` — the DOM is usable at "interactive" even while
        // subresources are still pending.
        let mut page_still_loading = false;
        match tokio::time::timeout(
            std::time::Duration::from_secs(NAVIGATE_LOAD_WAIT_SECS),
            page_guard.goto(url),
        )
        .await
        {
            // goto() only resolves once the frame's `load` lifecycle completed,
            // so a successful goto needs no further load wait.
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                // Navigation rejected outright (invalid URL / net::ERR_*): surface it.
                return Err(BrowserError::Navigation(e.to_string()));
            }
            Err(_elapsed) => {
                if !Self::wait_for_dom_usable(&page_guard, &before_url).await? {
                    return Err(BrowserError::Navigation(format!(
                        "navigation timed out after {}s — page did not finish loading \
                         (unreachable host, blocked subresources, or very slow)",
                        NAVIGATE_LOAD_WAIT_SECS + NAVIGATE_DOM_READY_SECS
                    )));
                }
                // The DOM is usable; distinguish "fully loaded" (readyState complete —
                // goto just didn't get to confirm the load event in time) from
                // "interactive with subresources still pending" so the agent gets an
                // accurate status instead of a blanket "still loading".
                let ready = cdp(page_guard.evaluate("document.readyState"))
                    .await
                    .ok()
                    .and_then(|r| r.into_value::<serde_json::Value>().ok())
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                page_still_loading = ready != "complete";
            }
        }

        let title = page_guard
            .get_title()
            .await
            .unwrap_or_default()
            .unwrap_or_else(|| "Untitled".to_string());

        if page_still_loading {
            Ok(format!(
                "Navigated to: {} | Title: {} (page still loading subresources)",
                url, title
            ))
        } else {
            Ok(format!("Navigated to: {} | Title: {}", url, title))
        }
    }

    /// Get page snapshot — tries Accessibility.getFullAXTree first, falls back to JS DOM traversal.
    ///
    /// Each interactive element is serialized with a `@N` reference ID
    /// that can be used directly with browser_click / browser_type.
    /// Set `full=true` to include hidden elements too.
    /// Optional `selector` scopes the snapshot to a subtree (stable @N refs, ignores outside noise).
    pub async fn snapshot(
        &mut self,
        full: bool,
        selector: Option<&str>,
    ) -> Result<String, BrowserError> {
        // Phase 1: Try Accessibility.getFullAXTree (penetrates Shadow DOM, semantic roles)
        match self.snapshot_ax_tree(selector).await {
            Ok(result) if !result.is_empty() => return Ok(result),
            Ok(_empty) => {
                tracing::warn!(
                    "[Browser] AX tree snapshot returned empty, falling back to JS DOM traversal"
                );
            }
            Err(e) if Self::is_connection_error(&e) => {
                // CDP link unresponsive (page mid-navigation or half-dead): the JS DOM
                // fallback hits the same evaluate and fails identically, doubling the
                // wait for nothing. Surface the fast error instead.
                return Err(e);
            }
            Err(e) => {
                tracing::warn!(
                    "[Browser] AX tree snapshot failed: {}, falling back to JS DOM traversal",
                    e
                );
            }
        }

        // Fallback: JS DOM traversal (existing behavior)
        self.snapshot_js(full, selector).await
    }

    /// AX tree snapshot via CDP `Accessibility.getFullAXTree`.
    ///
    /// Returns formatted text like `@1 [button] "Submit"` and caches backendNodeIds
    /// internally for click/type resolution.
    /// Optional `selector` scopes to a subtree — only elements within that DOM node are collected.
    async fn snapshot_ax_tree(&mut self, selector: Option<&str>) -> Result<String, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        // If scoped, resolve the selector to a backendNodeId first.
        // If resolution fails, return error so caller falls back to JS DOM traversal (which supports scoping natively).
        let scope_id: Option<u32> = if let Some(sel) = selector {
            match resolve_selector_backend_id(&page_guard, sel).await {
                Ok(id) => {
                    tracing::info!(
                        "[Browser] AX snapshot: selector '{}' resolved to backendNodeId={}",
                        sel,
                        id
                    );
                    Some(id)
                }
                Err(e) => {
                    tracing::warn!("[Browser] AX snapshot: selector '{}' resolve failed: {}, falling back to JS snapshot", sel, e);
                    return Err(BrowserError::Execution(format!(
                        "AX selector '{}' resolve failed, fallback to JS: {}",
                        sel, e
                    )));
                }
            }
        } else {
            None
        };

        let cmd = GetFullAXTree {
            depth: None,
            frame_id: None,
        };

        let resp = cdp(page_guard.execute(cmd)).await?;

        let nodes = resp
            .result
            .get("nodes")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                BrowserError::Execution("AXTree response missing 'nodes' array".to_string())
            })?;

        if nodes.is_empty() {
            self.snapshot_backend_ids.clear();
            return Ok(String::new());
        }

        let mut lines: Vec<String> = Vec::new();
        let mut backend_ids: Vec<u32> = Vec::new();

        if let Some(sid) = scope_id {
            // Scoped: find the scope node in the AX tree, then only collect its subtree.
            // If not found, return error so caller falls back to JS DOM traversal.
            let ax_nodes_count = nodes.len();
            match extract_subtree_children(nodes, sid) {
                Some(found_children) => {
                    tracing::info!("[Browser] AX snapshot scoped: found {} children for backendNodeId={} (AX tree has {} nodes)", found_children.len(), sid, ax_nodes_count);
                    collect_interactive_nodes(&found_children, &mut backend_ids, &mut lines);
                }
                None => {
                    tracing::warn!("[Browser] AX snapshot scoped: backendNodeId={} not found in AX tree ({} nodes), falling back to JS snapshot", sid, ax_nodes_count);
                    return Err(BrowserError::Execution(format!(
                        "AX scope node backendNodeId={} not found in AX tree, fallback to JS",
                        sid
                    )));
                }
            }
        } else {
            // Full tree (existing behavior)
            collect_interactive_nodes(nodes, &mut backend_ids, &mut lines);
        }

        self.snapshot_backend_ids = backend_ids;

        if lines.is_empty() {
            return Ok(String::new());
        }

        Ok(lines.join("\n"))
    }

    /// JS DOM traversal snapshot (existing behavior, now fallback-only).
    /// Optional `selector` scopes to a subtree.
    async fn snapshot_js(
        &self,
        full: bool,
        selector: Option<&str>,
    ) -> Result<String, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        // Build scoping prefix: if selector given, scope querySelectorAll to that element
        let scope_prefix = if let Some(sel) = selector {
            let escaped = sel.replace('\\', "\\\\").replace('\'', "\\'");
            format!("const root = document.querySelector('{}'); if (!root) return '';\n                ", escaped)
        } else {
            "const root = document;\n                ".to_string()
        };

        let js = if full {
            format!(
                r#"
            (function() {{
                {scope_prefix}
                const elements = root.querySelectorAll('a, button, input, textarea, select, [onclick]');
                const results = [];
                elements.forEach((el, i) => {{
                    const tag = el.tagName.toLowerCase();
                    const text = el.textContent?.trim().substring(0, 60) || '';
                    const type = el.type || '';
                    const placeholder = el.placeholder || '';
                    const id = el.id ? '#' + el.id : '';
                    const cls = Array.from(el.classList).filter(c => !c.startsWith('_')).join('.');
                    const cl = cls ? '.' + cls : '';
                    let extra = '';
                    if (tag === 'input' && type) extra += ` type="${{type}}"`;
                    if (placeholder) extra += ` placeholder="${{placeholder}}"`;
                    const display = text.substring(0, 40).replace(/"/g, '\\"');
                    const val = el.value ? ` value="${{el.value.substring(0, 30)}}"` : '';
                    results.push(`[@e${{i}}] <${{tag}}${{id}}${{cl}}${{extra}}${{val}}> "${{display}}"`);
                }});
                return results.join('\n');
            }})()
            "#
            )
        } else {
            format!(
                r#"
            (function() {{
                {scope_prefix}
                const elements = root.querySelectorAll('a, button, input, textarea, select, [onclick]');
                const results = [];
                elements.forEach((el, i) => {{
                    // Skip hidden / non-visible elements
                    const rect = el.getBoundingClientRect();
                    const style = window.getComputedStyle(el);
                    if (rect.width === 0 || rect.height === 0) return;
                    if (style.display === 'none' || style.visibility === 'hidden') return;
                    const tag = el.tagName.toLowerCase();
                    const text = el.textContent?.trim().substring(0, 60) || '';
                    const type = el.type || '';
                    const placeholder = el.placeholder || '';
                    const id = el.id ? '#' + el.id : '';
                    const cls = Array.from(el.classList).filter(c => !c.startsWith('_')).join('.');
                    const cl = cls ? '.' + cls : '';
                    let extra = '';
                    if (tag === 'input' && type) extra += ` type="${{type}}"`;
                    if (placeholder) extra += ` placeholder="${{placeholder}}"`;
                    const display = text.substring(0, 40).replace(/"/g, '\\"');
                    const val = el.value ? ` value="${{el.value.substring(0, 30)}}"` : '';
                    results.push(`[@e${{i}}] <${{tag}}${{id}}${{cl}}${{extra}}${{val}}> "${{display}}"`);
                }});
                return results.join('\n');
            }})()
            "#
            )
        };

        let result = cdp(page_guard.evaluate(js)).await?;

        let value: String = result
            .into_value()
            .unwrap_or_else(|_| "No interactive elements found".to_string());

        Ok(value)
    }

    /// Click element (via @N ref from AX snapshot, @eN legacy ref, or CSS selector)
    pub async fn click(&self, selector: &str) -> Result<String, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        // New AX tree ref: @N (1-based index into snapshot_backend_ids)
        if let Some(idx_str) = selector.strip_prefix('@') {
            if let Ok(idx) = idx_str.parse::<usize>() {
                if idx < 1 || idx > self.snapshot_backend_ids.len() {
                    return Err(BrowserError::ElementNotFound(
                        selector.to_string(),
                        format!(
                            "@{} out of range (max @{})",
                            idx,
                            self.snapshot_backend_ids.len()
                        ),
                    ));
                }
                let backend_id = self.snapshot_backend_ids[idx - 1];
                return self
                    .retry_on_stale(|| {
                        self.click_via_backend_node_id(&page_guard, backend_id, selector)
                    })
                    .await;
            }
        }

        // Legacy @eN ref (JS DOM traversal fallback)
        if let Some(idx_str) = selector.strip_prefix("@e") {
            // Numeric validation before JS interpolation (the @N branch above
            // already parses; keep the two ref paths symmetric).
            let idx: usize = idx_str.parse().map_err(|_| {
                BrowserError::ElementNotFound(
                    selector.to_string(),
                    "@e ref index must be a non-negative integer".to_string(),
                )
            })?;
            let js = format!(
                r#"(function() {{
                    const els = document.querySelectorAll('a, button, input, textarea, select, [onclick]');
                    const i = {idx};
                    if (!els[i]) throw new Error('Element @e{idx} not found on page');
                    els[i].click();
                    return 'Clicked @e{idx}';
                }})()"#,
                idx = idx
            );
            cdp(page_guard.evaluate(js)).await?;
            return Ok(format!("Clicked @e{}", idx));
        }

        // CSS selector — Playwright-style auto-wait (presence + visible, single
        // in-page async poll loop), then JS click to bypass chromiumoxide's
        // mouse-event path (which can hang on complex pages or when CDP timing is off)
        let js = Self::actionability_script(selector, "el.click(); return 'clicked';");
        cdp_ctx_with_timeout(
            &format!("Click on '{}' failed", selector),
            ACTIONABILITY_CDP_BUDGET,
            page_guard.evaluate(js),
        )
        .await?;
        Ok(format!("Clicked element: {}", selector))
    }

    /// Trusted click: dispatches real CDP mouse events (mouseMoved → mousePressed →
    /// mouseReleased) at the element's center coordinates. Unlike `el.click()`
    /// (JS-synthesized, untrusted), these events are trusted (isTrusted=true) and
    /// produce user activation — required to unlock autoplay-gated audio/video
    /// playback and other gesture-gated browser features.
    ///
    /// Trade-off: coordinate-based dispatch hits whatever is topmost at the point,
    /// so an element covered by an overlay will NOT receive the click. The default
    /// `click` (JS path) ignores overlays; use trusted only when activation is needed.
    pub async fn click_trusted(
        &self,
        selector: &str,
        button: &str,
    ) -> Result<String, BrowserError> {
        use chromiumoxide::cdp::browser_protocol::input::{
            DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
        };

        let (mouse_button, button_mask) = match button {
            "left" => (MouseButton::Left, 1),
            "right" => (MouseButton::Right, 2),
            "middle" => (MouseButton::Middle, 4),
            other => {
                return Err(BrowserError::Execution(format!(
                    "unsupported mouse button '{other}'"
                )))
            }
        };

        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        let (x, y) = self.element_center(&page_guard, selector).await?;

        cdp_ctx(
            "trusted click: mouseMoved failed",
            page_guard.execute(DispatchMouseEventParams::new(
                DispatchMouseEventType::MouseMoved,
                x,
                y,
            )),
        )
        .await?;

        let cmd = DispatchMouseEventParams::builder()
            .x(x)
            .y(y)
            .button(mouse_button)
            .click_count(1);
        let pressed = cmd
            .clone()
            .r#type(DispatchMouseEventType::MousePressed)
            .buttons(button_mask)
            .build()
            .map_err(|e| BrowserError::Execution(format!("mousePressed build: {e}")))?;
        cdp_ctx(
            "trusted click: mousePressed failed",
            page_guard.execute(pressed),
        )
        .await?;
        let released = cmd
            .r#type(DispatchMouseEventType::MouseReleased)
            .buttons(0)
            .build()
            .map_err(|e| BrowserError::Execution(format!("mouseReleased build: {e}")))?;
        cdp_ctx(
            "trusted click: mouseReleased failed",
            page_guard.execute(released),
        )
        .await?;

        Ok(format!("Clicked (trusted, {}): {}", button, selector))
    }

    /// Resolve an element's center point in viewport CSS pixels, scrolling it into
    /// view first. Supports the same three selector forms as `click` (@N / @eN / CSS).
    async fn element_center(
        &self,
        page: &Page,
        selector: &str,
    ) -> Result<(f64, f64), BrowserError> {
        // Shared JS fragment: after scrolling, return "centerX,centerY" of the box.
        const CENTER_EXPR: &str =
            "const r = el.getBoundingClientRect(); return Math.round(r.left + r.width/2) + ',' + Math.round(r.top + r.height/2);";

        let coords: String = if let Some(rest) = selector.strip_prefix('@') {
            if let Ok(idx) = rest.parse::<usize>() {
                // @N AX ref path — resolve backendNodeId, then callFunctionOn for the rect
                if idx < 1 || idx > self.snapshot_backend_ids.len() {
                    return Err(BrowserError::ElementNotFound(
                        selector.to_string(),
                        format!(
                            "@{} out of range (max @{})",
                            idx,
                            self.snapshot_backend_ids.len()
                        ),
                    ));
                }
                let backend_id = self.snapshot_backend_ids[idx - 1];
                let object_id = self.resolve_backend_node(page, backend_id).await?;
                let cmd = RuntimeCallFunctionOn {
                    function_declaration: format!(
                        "function(){{ const el = this; el.scrollIntoViewIfNeeded(); {} }}",
                        CENTER_EXPR
                    ),
                    object_id: Some(object_id.inner().clone()),
                    return_by_value: Some(true),
                    await_promise: Some(true),
                };
                let resp =
                    cdp_ctx("element_center: callFunctionOn failed", page.execute(cmd)).await?;
                if let Some(details) = resp.result.get("exceptionDetails") {
                    let desc = details
                        .get("exception")
                        .and_then(|e| e.get("description"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown exception");
                    return Err(BrowserError::Execution(format!(
                        "element_center JS exception on {}: {}",
                        selector, desc
                    )));
                }
                resp.result
                    .pointer("/result/value")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        BrowserError::Execution(format!(
                            "element_center: no coordinates returned for {}",
                            selector
                        ))
                    })?
                    .to_string()
            } else if let Some(idx_str) = rest.strip_prefix('e') {
                // Legacy @eN ref path (rest = "e5" after stripping '@')
                let idx: usize = idx_str.parse().map_err(|_| {
                    BrowserError::ElementNotFound(
                        selector.to_string(),
                        "@e ref index must be a non-negative integer".to_string(),
                    )
                })?;
                let js = format!(
                    r#"(function() {{
                        const els = document.querySelectorAll('a, button, input, textarea, select, [onclick]');
                        const el = els[{idx}];
                        if (!el) throw new Error('Element @e{idx} not found on page');
                        el.scrollIntoViewIfNeeded();
                        {CENTER_EXPR}
                    }})()"#,
                    idx = idx,
                    CENTER_EXPR = CENTER_EXPR
                );
                let result = cdp_ctx("element_center: evaluate failed", page.evaluate(js)).await?;
                result.into_value().map_err(|_| {
                    BrowserError::Execution(format!(
                        "element_center: unexpected return type for {}",
                        selector
                    ))
                })?
            } else {
                return Err(BrowserError::ElementNotFound(
                    selector.to_string(),
                    "unrecognized ref format (expected @N or @eN)".to_string(),
                ));
            }
        } else {
            // CSS selector path — reuse the auto-wait loop (presence + visible),
            // then return the center instead of clicking.
            let js = Self::actionability_script(selector, CENTER_EXPR);
            let result = cdp_ctx_with_timeout(
                "element_center: evaluate failed",
                ACTIONABILITY_CDP_BUDGET,
                page.evaluate(js),
            )
            .await?;
            result.into_value().map_err(|_| {
                BrowserError::Execution(format!(
                    "element_center: unexpected return type for {}",
                    selector
                ))
            })?
        };

        let (xs, ys) = coords.split_once(',').ok_or_else(|| {
            BrowserError::Execution(format!(
                "element_center: malformed coordinates '{}' for {}",
                coords, selector
            ))
        })?;
        let x = xs.trim().parse::<f64>().map_err(|_| {
            BrowserError::Execution(format!("element_center: bad x in '{}'", coords))
        })?;
        let y = ys.trim().parse::<f64>().map_err(|_| {
            BrowserError::Execution(format!("element_center: bad y in '{}'", coords))
        })?;
        Ok((x, y))
    }

    /// Type text into element (via @N ref from AX snapshot, @eN legacy ref, or CSS selector)
    pub async fn type_text(&self, selector: &str, text: &str) -> Result<String, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        // New AX tree ref: @N
        if let Some(idx_str) = selector.strip_prefix('@') {
            if let Ok(idx) = idx_str.parse::<usize>() {
                if idx < 1 || idx > self.snapshot_backend_ids.len() {
                    return Err(BrowserError::ElementNotFound(
                        selector.to_string(),
                        format!(
                            "@{} out of range (max @{})",
                            idx,
                            self.snapshot_backend_ids.len()
                        ),
                    ));
                }
                let backend_id = self.snapshot_backend_ids[idx - 1];
                return self
                    .retry_on_stale(|| {
                        self.type_via_backend_node_id(&page_guard, backend_id, selector, text)
                    })
                    .await;
            }
        }

        // Legacy @eN ref — focus+clear via JS, then Input.insertText for real input
        if let Some(idx_str) = selector.strip_prefix("@e") {
            if let Ok(idx) = idx_str.parse::<usize>() {
                let js = format!(
                    r#"(function() {{
                        const els = document.querySelectorAll('a, button, input, textarea, select, [onclick]');
                        const i = parseInt({idx});
                        if (!els[i]) throw new Error('Element @e{idx} not found');
                        const el = els[i];
                        el.scrollIntoViewIfNeeded();
                        el.focus();
                        el.value = '';
                        return true;
                    }})()"#,
                    idx = idx,
                );
                cdp(page_guard.evaluate(js)).await?;

                let input_cmd = InputInsertText {
                    text: text.to_string(),
                };
                cdp_ctx(
                    &format!("Input.insertText on @e{}", idx),
                    page_guard.execute(input_cmd),
                )
                .await?;
                return Ok(format!("Typed '{}' into @e{}", text, idx));
            }
        }

        // CSS selector — auto-wait (presence + visible), then focus+clear via JS,
        // then Input.insertText for real input
        let js = Self::actionability_script(selector, "el.focus(); el.value=''; return true;");
        cdp_ctx_with_timeout(
            &format!("Type focus on '{}' failed", selector),
            ACTIONABILITY_CDP_BUDGET,
            page_guard.evaluate(js),
        )
        .await?;

        let input_cmd = InputInsertText {
            text: text.to_string(),
        };
        cdp_ctx(
            &format!("Input.insertText on '{}'", selector),
            page_guard.execute(input_cmd),
        )
        .await?;

        Ok(format!("Typed '{}' into {}", text, selector))
    }

    /// Press a physical keyboard key or chord on the currently focused element.
    ///
    /// Uses CDP `Input.dispatchKeyEvent`, so page listeners receive trusted
    /// `keydown` / `keyup` events. Chords use Playwright-style names such as
    /// `Control+c`, `Shift+Tab`, or `Meta+ArrowLeft`.
    pub async fn press_key(&self, chord: &str) -> Result<String, BrowserError> {
        use chromiumoxide::cdp::browser_protocol::input::{
            DispatchKeyEventParams, DispatchKeyEventType,
        };

        let (key, modifiers) = parse_key_chord(chord)?;
        let definition = chromiumoxide::keys::get_key_definition(&key).ok_or_else(|| {
            BrowserError::Execution(format!(
                "unsupported key '{key}' in chord '{chord}' (use a named key such as Enter, Tab, ArrowUp, F1, Space, or a single US-keyboard character)"
            ))
        })?;

        let mut command = DispatchKeyEventParams::builder()
            .key(definition.key)
            .code(definition.code)
            .windows_virtual_key_code(definition.key_code)
            .modifiers(modifiers);

        // Ctrl/Alt/Meta shortcuts must not insert the printable target. Enter and
        // ordinary characters use `keyDown` with text, matching chromiumoxide's
        // own `Page::press_key` behavior; named non-printable keys use rawKeyDown.
        let text = if modifiers & (1 | 2 | 4) == 0 {
            definition
                .text
                .or_else(|| (definition.key.chars().count() == 1).then_some(definition.key))
        } else {
            None
        };
        let down_type = if let Some(text) = text {
            command = command.text(text);
            DispatchKeyEventType::KeyDown
        } else {
            DispatchKeyEventType::RawKeyDown
        };

        let key_down = command
            .clone()
            .r#type(down_type)
            .build()
            .map_err(|e| BrowserError::Execution(format!("keyDown build: {e}")))?;
        let key_up = command
            .r#type(DispatchKeyEventType::KeyUp)
            .build()
            .map_err(|e| BrowserError::Execution(format!("keyUp build: {e}")))?;

        let page = self.get_page().await?;
        let page_guard = page.lock().await;
        cdp_ctx("keyboard keyDown failed", page_guard.execute(key_down)).await?;
        cdp_ctx("keyboard keyUp failed", page_guard.execute(key_up)).await?;

        Ok(format!("Pressed {chord}"))
    }

    /// Arm the in-page DOM-change detector. Call *before* a write action, then
    /// `wait_for_change` after it to confirm the action took effect.
    ///
    /// Baseline captured: (1) a MutationObserver on the document subtree — any
    /// childList/attribute/characterData mutation clears the flag; (2) the current
    /// URL (a navigation is a change); (3) the focused element's value/text
    /// (typing into a field is a change even on vanilla inputs that never mutate
    /// the DOM tree). Any of the three differing from its baseline within the wait
    /// window means the page reacted to the action.
    pub async fn arm_change_detector(&self) -> Result<(), BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;
        const JS: &str = r#"(function() {
            try { if (window.__nuphus_mut_obs) window.__nuphus_mut_obs.disconnect(); } catch (e) {}
            window.__nuphus_mut_seen = false;
            window.__nuphus_mut_url = location.href;
            var ae = document.activeElement;
            window.__nuphus_mut_val = ae ? (ae.value !== undefined ? ae.value : ae.textContent) : null;
            window.__nuphus_mut_obs = new MutationObserver(function() { window.__nuphus_mut_seen = true; });
            window.__nuphus_mut_obs.observe(document.documentElement, {
                childList: true, subtree: true, attributes: true, characterData: true
            });
            return true;
        })()"#;
        cdp(page_guard.evaluate(JS)).await?;
        Ok(())
    }

    /// Wait up to `timeout_ms` for the armed detector to observe a page change
    /// (DOM mutation, URL change, or focused-element value change). Returns
    /// `true` if a change was seen within the window, `false` if the page stayed
    /// quiet the whole window.
    ///
    /// Post-action effect verification: a change within the window means the
    /// action took effect; a quiet page strongly suggests it did not (dead or
    /// covered element, wrong selector). A full navigation destroys the page
    /// context mid-wait — that itself is a change, so it reports `true` rather
    /// than a false "no effect".
    pub async fn wait_for_change(&self, timeout_ms: u64) -> Result<bool, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;
        let js = r#"(async function() {
            var deadline = Date.now() + __TIMEOUT_MS__;
            for (;;) {
                if (window.__nuphus_mut_seen) return true;
                if (location.href !== window.__nuphus_mut_url) return true;
                var ae = document.activeElement;
                var v = ae ? (ae.value !== undefined ? ae.value : ae.textContent) : null;
                if (v !== window.__nuphus_mut_val) return true;
                if (Date.now() >= deadline) return false;
                await new Promise(function(r) { setTimeout(r, 100); });
            }
        })()"#
            .replace("__TIMEOUT_MS__", &timeout_ms.to_string());
        // Budget sits above the JS-side poll window: on a healthy page the JS
        // resolves (true/false) inside its own deadline, and only a wedged CDP
        // call (frozen renderer) hits the outer budget.
        let budget = std::time::Duration::from_millis(timeout_ms + 2000);
        match cdp_with_timeout(budget, page_guard.evaluate(js)).await {
            Ok(res) => Ok(res.into_value::<bool>().unwrap_or(false)),
            Err(e) => {
                // Execution context destroyed mid-wait (full navigation), or the CDP
                // connection went unresponsive: the page changed (or is gone), so treat
                // the action as effective rather than erroring.
                tracing::warn!(
                    "[Browser] effect-change wait evaluate failed, treating as page changed: {}",
                    e
                );
                Ok(true)
            }
        }
    }

    /// Build the in-page actionability script shared by the CSS path of
    /// click/type_text: poll (~100ms inside a single async evaluate round trip,
    /// no Rust-side CDP polling) until the element is present AND visible
    /// (non-zero bounding rect, not display:none / visibility:hidden), then run
    /// `action`. The action snippet must `return` a value or throw. Timeout
    /// errors carry the selector, the timeout and a browser_snapshot hint.
    fn actionability_script(selector: &str, action: &str) -> String {
        let escaped = selector.replace('\\', "\\\\").replace('\'', "\\'");
        format!(
            r#"(async (s, timeoutMs, pollMs) => {{
    const isVisible = (el) => {{
        const r = el.getBoundingClientRect();
        if (r.width === 0 || r.height === 0) return false;
        const st = window.getComputedStyle(el);
        return st.display !== 'none' && st.visibility !== 'hidden';
    }};
    const deadline = Date.now() + timeoutMs;
    for (;;) {{
        const el = document.querySelector(s);
        if (el && isVisible(el)) {{
            el.scrollIntoViewIfNeeded();
            {action}
        }}
        if (Date.now() >= deadline) throw new Error('Timeout ' + timeoutMs + 'ms waiting for element to be present and visible: ' + s + ' (hint: run browser_snapshot to confirm page state)');
        await new Promise((r) => setTimeout(r, pollMs));
    }}
}})('{escaped}', {timeout}, {poll})"#,
            escaped = escaped,
            action = action,
            timeout = ACTIONABILITY_TIMEOUT_MS,
            poll = ACTIONABILITY_POLL_MS,
        )
    }

    /// Retry an @N-path operation only while it fails with a stale-node error:
    /// the page may re-render between snapshot and action, transiently
    /// invalidating backendNodeIds. Budget: {STALE_NODE_RETRIES} retries ×
    /// {STALE_NODE_RETRY_MS}ms. The success path and non-stale errors return
    /// immediately (zero overhead).
    async fn retry_on_stale<F, Fut>(&self, mut op: F) -> Result<String, BrowserError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<String, BrowserError>>,
    {
        let mut last_err = match op().await {
            Ok(ok) => return Ok(ok),
            Err(e) => e,
        };
        for _ in 0..STALE_NODE_RETRIES {
            if !Self::is_stale_node_error(&last_err) {
                return Err(last_err);
            }
            tokio::time::sleep(std::time::Duration::from_millis(STALE_NODE_RETRY_MS)).await;
            match op().await {
                Ok(ok) => return Ok(ok),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// Classify failures caused by a stale backend node (node destroyed or
    /// replaced between snapshot and action) — the only @N errors worth
    /// retrying.
    fn is_stale_node_error(err: &BrowserError) -> bool {
        let msg = err.to_string().to_ascii_lowercase();
        msg.contains("node with given id")
            || msg.contains("resolvenode")
            || msg.contains("node is detached")
            || msg.contains("not attached")
    }

    /// Click an element by its backendNodeId via CDP DOM.resolveNode + Runtime.callFunctionOn.
    /// Uses a custom CDP command to avoid chromiumoxide's auto-injection of `executionContextId`,
    /// which conflicts with `objectId` in the CDP protocol.
    async fn click_via_backend_node_id(
        &self,
        page: &Page,
        backend_node_id: u32,
        selector: &str,
    ) -> Result<String, BrowserError> {
        let object_id = self.resolve_backend_node(page, backend_node_id).await?;

        let cmd = RuntimeCallFunctionOn {
            function_declaration: "function(){ this.scrollIntoViewIfNeeded(); this.click(); }"
                .to_string(),
            object_id: Some(object_id.inner().clone()),
            return_by_value: Some(true),
            await_promise: Some(true),
        };

        let resp = cdp_ctx("Runtime.callFunctionOn failed", page.execute(cmd)).await?;

        // Check for JS exception in response
        if let Some(details) = resp.result.get("exceptionDetails") {
            let desc = details
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown exception");
            return Err(BrowserError::Execution(format!(
                "Click JS exception on {}: {}",
                selector, desc
            )));
        }

        Ok(format!("Clicked {}", selector))
    }

    /// Type text into an element by its backendNodeId via CDP DOM.resolveNode + Runtime.callFunctionOn.
    /// Uses a custom CDP command to avoid chromiumoxide's auto-injection of `executionContextId`.
    async fn type_via_backend_node_id(
        &self,
        page: &Page,
        backend_node_id: u32,
        selector: &str,
        text: &str,
    ) -> Result<String, BrowserError> {
        let object_id = self.resolve_backend_node(page, backend_node_id).await?;

        // Step 1: Focus the target element (scroll into view + clear + focus)
        let focus_func =
            "function(){ this.scrollIntoViewIfNeeded(); this.focus(); this.value=''; }";
        let focus_cmd = RuntimeCallFunctionOn {
            function_declaration: focus_func.to_string(),
            object_id: Some(object_id.inner().clone()),
            return_by_value: Some(true),
            await_promise: Some(true),
        };

        let focus_resp = cdp_ctx("Type focus failed", page.execute(focus_cmd)).await?;

        if let Some(details) = focus_resp.result.get("exceptionDetails") {
            let desc = details
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown exception");
            return Err(BrowserError::Execution(format!(
                "Type JS exception on {}: {}",
                selector, desc
            )));
        }

        // Step 2: Use CDP Input.insertText to dispatch real text input.
        // This triggers the full browser event chain (keydown/keypress/beforeinput/input/keyup),
        // which React/Vue controlled components require to update their internal state.
        let input_cmd = InputInsertText {
            text: text.to_string(),
        };
        cdp_ctx("Input.insertText failed", page.execute(input_cmd)).await?;

        Ok(format!("Typed '{}' into {}", text, selector))
    }

    /// Resolve a backendNodeId to a RemoteObjectId via CDP DOM.resolveNode.
    async fn resolve_backend_node(
        &self,
        page: &Page,
        backend_node_id: u32,
    ) -> Result<RemoteObjectId, BrowserError> {
        let cmd = DOMResolveNode {
            backend_node_id,
            object_group: None,
        };

        let resp = cdp_ctx("DOM.resolveNode failed", page.execute(cmd)).await?;

        let object_id_str = resp
            .result
            .get("object")
            .and_then(|o| o.get("objectId"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                BrowserError::Execution(format!(
                    "DOM.resolveNode for backendNodeId={} did not return objectId",
                    backend_node_id
                ))
            })?;

        Ok(RemoteObjectId::from(object_id_str.to_string()))
    }

    /// Inject `window.__nuphus` helpers into the page context for batch_exec.
    ///
    /// Helpers provide click, fill, scroll, wait, extract, snapshot operations
    /// that can be called from batch scripts in a single CDP round trip.
    async fn inject_helpers(&mut self) -> Result<(), BrowserError> {
        if self.helpers_injected {
            return Ok(());
        }

        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        let helpers_js = include_str!("helpers.js");

        cdp_ctx("Helper injection failed", page_guard.evaluate(helpers_js)).await?;

        self.helpers_injected = true;
        Ok(())
    }

    /// Execute a multi-step batch script in a single CDP round trip.
    ///
    /// The script can use the pre-injected `window.__nuphus` helpers:
    /// - `h.click(ref)` — click element by @N ref or CSS selector
    /// - `h.fill(ref, text)` — type text into input by @N ref or CSS selector
    /// - `h.scroll(px)` — scroll window vertically
    /// - `h.wait(ms)` — wait for ms
    /// - `h.extract(selector)` — get text content (CSS selector only)
    /// - `h.snapshot()` — lightweight DOM snapshot
    ///
    /// Each helper auto-collects its result. The script runs as an async IIFE.
    /// Use `const h = window.__nuphus;` at the start for convenience.
    /// Returns JSON array: `[{op, ref, success, detail}]`.
    pub async fn batch_exec(&mut self, script: &str) -> Result<String, BrowserError> {
        // Ensure helpers are injected
        self.inject_helpers().await?;

        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        // Verify helpers actually exist in page context (defense-in-depth:
        // navigation errors or page crashes can clear JS context without
        // resetting helpers_injected)
        let check_js =
            "typeof window.__nuphus !== 'undefined' && window.__nuphus._results !== undefined";
        let helpers_present: bool = cdp(page_guard.evaluate(check_js))
            .await
            .map(|r| r.into_value().unwrap_or(false))
            .unwrap_or(false);

        // Must drop the first guard first: page is an Arc<tokio::sync::Mutex<Page>>,
        // and locking it again while the lock is held would deadlock forever (tokio Mutex is not reentrant).
        drop(page_guard);
        if !helpers_present {
            self.helpers_injected = false;
            self.inject_helpers().await?;
        }

        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        // Wrap script: initialize results array, run user script, return results
        let wrapped = format!(
            r#"(async () => {{
    window.__nuphus._results = [];
    const h = window.__nuphus;
    try {{
        {}
    }} catch(e) {{
        h._results.push({{ op: 'batch_error', success: false, detail: e.message }});
    }}
    return JSON.stringify(h._results);
}})()"#,
            script
        );

        // ── Evaluate with internal timeout (10s) ──
        // page.evaluate() with awaitPromise can hang indefinitely if the page
        // navigates during execution (e.g. form submit click). We wrap it with
        // a 10s timeout and gracefully degrade on timeout / context-destroyed errors.
        let eval_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            page_guard.evaluate(wrapped),
        )
        .await;

        match eval_result {
            Ok(Ok(result)) => {
                let value: String = result.into_value().unwrap_or_else(|_| "[]".to_string());
                Ok(value)
            }
            Ok(Err(e)) => {
                let err_str = e.to_string();
                // If the page navigated away (context destroyed), the completion
                // state is UNKNOWN — the script may or may not have finished, and
                // its _results died with the old context. Report that honestly
                // instead of claiming success.
                if err_str.contains("context was destroyed")
                    || err_str.contains("detached from frame")
                    || err_str.contains("Cannot find context")
                    || err_str.contains("execution context")
                {
                    tracing::warn!(
                        "[batch_exec] Execution context lost (page navigated): {}",
                        err_str
                    );
                    Ok(r#"[{"op":"batch_truncated","success":"unknown","detail":"Page navigated during execution; completion state unknown (results lost with the old page context)"}]"#.to_string())
                } else {
                    Err(BrowserError::Execution(err_str))
                }
            }
            Err(_elapsed) => {
                // Timeout: evaluate() hung, almost certainly because the page
                // navigated mid-execution and the JS promise never resolved. If the
                // page is still alive, salvage the steps that completed before the
                // hang (window.__nuphus._results) and report success:"unknown".
                tracing::warn!("[batch_exec] Timed out after 10s — page likely navigated");
                let salvaged = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    page_guard.evaluate(
                        "JSON.stringify((window.__nuphus && window.__nuphus._results) || [])",
                    ),
                )
                .await;
                let done: String = match salvaged {
                    Ok(Ok(r)) => r.into_value().unwrap_or_else(|_| "[]".to_string()),
                    _ => "[]".to_string(),
                };
                let items = done
                    .trim()
                    .strip_prefix('[')
                    .and_then(|s| s.strip_suffix(']'))
                    .unwrap_or("")
                    .trim();
                let truncated = r#"{"op":"batch_truncated","success":"unknown","detail":"Execution timed out (page likely navigated); completion state unknown — any completed steps are appended after this entry"}"#;
                if items.is_empty() {
                    Ok(format!("[{truncated}]"))
                } else {
                    Ok(format!("[{truncated},{items}]"))
                }
            }
        }
    }

    /// Configure Chrome download behavior via CDP `Browser.setDownloadBehavior`.
    async fn configure_download_dir(&mut self) -> Result<(), BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        // Ensure download directory exists
        let _ = std::fs::create_dir_all(&self.download_dir);

        let download_path = self.download_dir.to_string_lossy().to_string();
        let cmd = BrowserSetDownloadBehavior {
            behavior: "allow".to_string(),
            download_path: Some(download_path.clone()),
            events_enabled: Some(true),
        };

        match cdp(page_guard.execute(cmd)).await {
            Ok(_) => {
                tracing::info!("[Browser] Download dir set to: {}", download_path);
                self.download_configured = true;
                Ok(())
            }
            Err(e) => {
                tracing::warn!("[Browser] Failed to set download dir via CDP: {}. Downloads will use Chrome default.", e);
                // Don't fail — downloads still work, just in the default dir. Record
                // the warning so list_downloads can bubble it into the tool output
                // instead of silently listing an empty profile dir.
                self.download_config_warning = Some(format!(
                    "download directory could not be configured via CDP ({e}); \
                     downloads land in Chrome's default download folder, not {}",
                    self.download_dir.display()
                ));
                self.download_configured = true;
                Ok(())
            }
        }
    }

    /// List files in the download directory.
    pub fn list_downloads(&self) -> Result<String, BrowserError> {
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.download_dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() {
                        files.push(serde_json::json!({
                            "name": entry.file_name().to_string_lossy(),
                            "path": entry.path().to_string_lossy(),
                            "size": meta.len(),
                            "modified": meta.modified().ok().map(|t| {
                                chrono::DateTime::<chrono::Utc>::from(t)
                                    .format("%Y-%m-%d %H:%M:%S").to_string()
                            }),
                        }));
                    }
                }
            }
        }
        if let Some(warning) = &self.download_config_warning {
            // Bubble the configuration failure into the tool output: the listing
            // below covers the profile downloads dir while real downloads may be
            // landing in Chrome's default folder.
            return Ok(serde_json::to_string_pretty(&serde_json::json!({
                "warning": warning,
                "files": files,
            }))
            .unwrap_or_else(|_| "[]".to_string()));
        }
        Ok(serde_json::to_string_pretty(&files).unwrap_or_else(|_| "[]".to_string()))
    }

    /// Import cookies from the user's Chrome profile into the current page.
    ///
    /// Reads via the host-registered cookie source (`crate::cookie_source`),
    /// forcing a fresh read from the source (login state may have just
    /// changed), and injects them via CDP `Network.setCookie`.
    pub async fn import_cookies(
        &self,
        domain_filter: Option<&str>,
    ) -> Result<String, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        // Get current page URL for domain context
        let current_url = cdp(page_guard.url())
            .await
            .unwrap_or_default()
            .unwrap_or_else(|| "about:blank".to_string());

        let cookies = match crate::cookie_source::load(domain_filter) {
            Ok(c) => c,
            Err(e) => {
                return Ok(format!("Failed to read Chrome cookies: {}", e));
            }
        };

        if cookies.is_empty() {
            return Ok("No cookies found to import.".to_string());
        }

        let mut imported = 0;
        let mut failed = 0;

        for cookie in &cookies {
            let cmd = NetworkSetCookie {
                name: cookie.name.clone(),
                value: cookie.value.clone(),
                url: Some(current_url.clone()),
                domain: Some(cookie.domain.clone()),
                path: Some(cookie.path.clone()),
                secure: Some(cookie.secure),
                http_only: Some(cookie.http_only),
                same_site: cookie.same_site.clone(),
                expires: cookie.expires,
            };

            match cdp(page_guard.execute(cmd)).await {
                Ok(_) => imported += 1,
                Err(_) => failed += 1,
            }
        }

        Ok(format!(
            "Cookie import complete: {} imported, {} failed (total {} cookies found)",
            imported,
            failed,
            cookies.len()
        ))
    }

    /// Upload a file to a file input element using the DataTransfer trick.
    ///
    /// Reads the file from disk, base64-encodes it, creates a File object in JS,
    /// and sets it on the target `<input type="file">` element.
    pub async fn upload_file(
        &self,
        selector: &str,
        file_path: &str,
    ) -> Result<String, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        // Read file from disk
        let path = std::path::Path::new(file_path);
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        let data = std::fs::read(path).map_err(BrowserError::Io)?;

        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);

        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();

        // JS string literals via serde_json — safe against quotes/backslashes in
        // the selector or file name (ad-hoc ' escaping was both incomplete and
        // inconsistent with the rest of the codebase).
        let selector_lit = js_string_literal(selector);
        let name_lit = js_string_literal(&file_name);
        let mime_lit = js_string_literal(&mime);

        let js = format!(
            r#"(function() {{
    // Find the file input element
    let el;
    const sel = {selector_lit};
    if (sel.startsWith('@')) {{
        const idx = parseInt(sel.slice(1)) - 1;
        const els = document.querySelectorAll('input[type="file"]');
        if (!els[idx]) throw new Error('File input ' + sel + ' not found');
        el = els[idx];
    }} else {{
        el = document.querySelector(sel);
        if (!el) throw new Error('File input ' + sel + ' not found');
    }}

    // Decode base64 to Uint8Array
    const b64 = '{b64}';
    const byteChars = atob(b64);
    const byteArr = new Uint8Array(byteChars.length);
    for (let i = 0; i < byteChars.length; i++) byteArr[i] = byteChars.charCodeAt(i);

    // Create File object
    const file = new File([byteArr], {name_lit}, {{ type: {mime_lit} }});

    // Set via DataTransfer
    const dt = new DataTransfer();
    dt.items.add(file);
    el.files = dt.files;
    el.dispatchEvent(new Event('change', {{ bubbles: true }}));

    return 'Uploaded ' + {name_lit} + ' (' + byteArr.length + ' bytes) to ' + sel;
}})()"#,
            selector_lit = selector_lit,
            b64 = b64,
            name_lit = name_lit,
            mime_lit = mime_lit,
        );

        let result = cdp(page_guard.evaluate(js)).await?;

        let value: String = result
            .into_value()
            .unwrap_or_else(|_| "Upload completed".to_string());

        Ok(value)
    }

    /// Drag existing local files or directories onto an element using Chrome's
    /// native DevTools drag-event path. Chrome reads the paths directly, so file
    /// contents are not copied through JavaScript or base64-encoded.
    pub async fn drag_files(
        &self,
        selector: &str,
        file_paths: &[String],
    ) -> Result<String, BrowserError> {
        use chromiumoxide::cdp::browser_protocol::input::{
            DispatchDragEventParams, DispatchDragEventType, DragData, DragDataItem,
        };

        if file_paths.is_empty() {
            return Err(BrowserError::Execution(
                "drag_files requires at least one path".to_string(),
            ));
        }

        let page = self.get_page().await?;
        let page_guard = page.lock().await;
        let (x, y) = self.element_center(&page_guard, selector).await?;

        let drag_data = DragData::builder()
            // Chromium requires the `items` field even for file-only drags; the
            // generated CDP type omits an empty Vec during serialization.
            .item(DragDataItem::new("text/plain", ""))
            .files(file_paths.iter().cloned())
            .drag_operations_mask(1)
            .build()
            .map_err(|e| BrowserError::Execution(format!("drag data build: {e}")))?;

        for event_type in [
            DispatchDragEventType::DragEnter,
            DispatchDragEventType::DragOver,
            DispatchDragEventType::Drop,
        ] {
            let event = DispatchDragEventParams::new(event_type, x, y, drag_data.clone());
            cdp_ctx("file drag event failed", page_guard.execute(event)).await?;
        }

        Ok(format!(
            "Dragged {} path(s) onto {}",
            file_paths.len(),
            selector
        ))
    }

    /// Scroll page
    pub async fn scroll(&self, direction: &str, amount: i32) -> Result<String, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        let js = match direction {
            "up" => format!("window.scrollBy(0, -{})", amount),
            "down" => format!("window.scrollBy(0, {})", amount),
            "left" => format!("window.scrollBy(-{}, 0)", amount),
            "right" => format!("window.scrollBy({}, 0)", amount),
            _ => {
                return Err(BrowserError::Config(format!(
                    "unknown scroll direction: {direction} (expected up/down/left/right)"
                )));
            }
        };

        cdp(page_guard.evaluate(js)).await?;

        Ok(format!("Scrolled {} by {}", direction, amount))
    }

    /// Extract page content
    pub async fn extract(&self, max_chars: usize) -> Result<String, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        let js = format!(
            r#"
            (function() {{
                // Try to get main content
                const article = document.querySelector('article');
                const main = document.querySelector('main');
                const content = document.querySelector('[class*="content"]');
                const body = document.body;
                
                let text = '';
                if (article) text = article.innerText;
                else if (main) text = main.innerText;
                else if (content) text = content.innerText;
                else text = body.innerText;
                
                return text.substring(0, {}).replace(/\s+/g, ' ').trim();
            }})()
            "#,
            max_chars
        );

        let result = cdp(page_guard.evaluate(js)).await?;

        let value: String = result
            .into_value()
            .unwrap_or_else(|_| "No content found".to_string());

        Ok(value)
    }

    /// Screenshot
    pub async fn screenshot(&self, path: Option<&str>) -> Result<String, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        let data = cdp(page_guard.screenshot(ScreenshotParams {
            full_page: Some(true),
            omit_background: None,
            cdp_params: Default::default(),
        }))
        .await?;

        if let Some(path) = path {
            std::fs::write(path, &data).map_err(BrowserError::Io)?;
            Ok(format!(
                "Screenshot saved to: {} ({} bytes)",
                path,
                data.len()
            ))
        } else {
            // Return Base64
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
            Ok(format!("data:image/png;base64,{}", b64))
        }
    }

    /// Execute JavaScript (supports async/await via IIFE wrapping).
    pub async fn evaluate(&self, script: &str) -> Result<serde_json::Value, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        // Only wrap in async IIFE when script contains `await`.
        // Otherwise sync expressions (e.g. "document.title") return undefined
        // because there's no `return` inside the async wrapper.
        // Known limitation: this is a substring heuristic, not a parse — `await`
        // inside a comment or string literal (e.g. `const s = "await me"`) also
        // triggers the wrapper (the script then needs its own `return`, which is
        // the documented contract for async scripts), and genuinely async code
        // using .then() without the `await` keyword is not wrapped.
        let wrapped = if script.contains("await") {
            format!("(async () => {{\n{}\n}})()", script)
        } else {
            script.to_string()
        };

        let result = cdp(page_guard.evaluate(wrapped)).await?;

        let value = result.into_value().unwrap_or(serde_json::Value::Null);

        Ok(value)
    }

    /// Browser back
    pub async fn back(&self) -> Result<String, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        let before = cdp(page_guard.url())
            .await
            .unwrap_or_default()
            .unwrap_or_else(|| "about:blank".to_string());

        cdp(page_guard.evaluate("history.back()")).await?;

        Self::wait_for_url_change(&page_guard, &before).await?;

        let url = cdp(page_guard.url())
            .await
            .unwrap_or_default()
            .unwrap_or_else(|| "about:blank".to_string());

        Ok(format!("Navigated back to: {}", url))
    }

    /// Browser forward
    pub async fn forward(&self) -> Result<String, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        let before = cdp(page_guard.url())
            .await
            .unwrap_or_default()
            .unwrap_or_else(|| "about:blank".to_string());

        cdp(page_guard.evaluate("history.forward()")).await?;

        Self::wait_for_url_change(&page_guard, &before).await?;

        let url = cdp(page_guard.url())
            .await
            .unwrap_or_default()
            .unwrap_or_else(|| "about:blank".to_string());

        Ok(format!("Navigated forward to: {}", url))
    }

    /// Poll the page until the navigation commits and the new document becomes
    /// usable (`document.readyState` at least `"interactive"`), or the deadline
    /// passes. Returns `true` if the DOM became usable, `false` on timeout.
    ///
    /// Used as the degradation path when `goto()` can't complete: `goto()` waits
    /// for the `load` event, which never fires while a subresource hangs, so the
    /// DOM may already be parsed and usable even though the page "isn't loaded".
    /// `"interactive"` only requires the parser to finish.
    ///
    /// `before_url` distinguishes the new document from a stale previous page
    /// (whose readyState is also `"complete"`): a committed navigation changes
    /// the URL. `saw_loading` additionally covers same-URL navigations (reload),
    /// where readyState cycles through `"loading"` but the URL is unchanged.
    /// Transient evaluate failures (execution context destroyed mid-navigation)
    /// mean "not committed yet" — never treated as the `"loading"` signal.
    async fn wait_for_dom_usable(page: &Page, before_url: &str) -> Result<bool, BrowserError> {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(NAVIGATE_DOM_READY_SECS);
        let mut saw_loading = false;
        loop {
            // Each probe is individually bounded: this is only reached after `goto` timed
            // out, and a mid-navigation page can leave CDP unresponsive — an unbounded
            // evaluate would hang past `deadline` (the deadline check happens after the
            // await), turning a 12s fallback into the full 30s tool guard.
            let ready = match tokio::time::timeout(
                std::time::Duration::from_secs(1),
                page.evaluate("document.readyState"),
            )
            .await
            {
                Ok(Ok(res)) => res
                    .into_value::<serde_json::Value>()
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
                _ => String::new(),
            };
            if ready == "loading" {
                saw_loading = true;
            }
            let url =
                match tokio::time::timeout(std::time::Duration::from_secs(1), page.url()).await {
                    Ok(Ok(u)) => u.unwrap_or_default(),
                    _ => String::new(),
                };
            // "complete" is unambiguous — the document finished loading, no need to
            // confirm a URL change (a navigate to the same URL reloads without the URL
            // ever differing). "interactive" needs the confirmation that navigation
            // actually started (URL changed or we saw the loading transition), otherwise
            // the pre-navigation page's readyState could be mistaken for the new one.
            if ready == "complete" {
                return Ok(true);
            }
            if ready == "interactive" && (url != before_url || saw_loading) {
                return Ok(true);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    /// Wait until the page URL differs from `before`.
    ///
    /// `history.back()` / `history.forward()` are async JS: the `evaluate` returns before the
    /// navigation completes, and BFCache / same-document history navigations fire no
    /// `Page.loadEventFired` — so `wait_for_navigation()` can hang until the tool-level 30s
    /// timeout even though the navigation already succeeded. Polling the URL is the only
    /// reliable completion signal. Deadline is 20s (inside the 30s tool guard).
    async fn wait_for_url_change(page: &Page, before: &str) -> Result<(), BrowserError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            // Bounded probe, same rationale as wait_for_dom_usable: a mid-navigation
            // page can leave CDP unresponsive, and an unbounded url() would hang past
            // the deadline, turning a 20s fallback into the full 30s tool guard.
            let now =
                match tokio::time::timeout(std::time::Duration::from_secs(1), page.url()).await {
                    Ok(Ok(u)) => u.unwrap_or_default(),
                    _ => String::new(),
                };
            if now != before {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(BrowserError::Navigation(format!(
                    "history navigation timed out (url unchanged: {before})"
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    /// Wait for element to reach the given state.
    ///
    /// `state`:
    /// - `attached` (default): element present in the DOM. Keeps the original
    ///   Rust-side 100ms `find_element` poll loop.
    /// - `visible`: present AND visible (non-zero bounding rect, not
    ///   display:none / visibility:hidden). Single in-page async evaluate
    ///   poll loop (no Rust-side CDP polling).
    /// - `hidden`: element absent from the DOM OR not visible. Same loop.
    pub async fn wait_for(
        &self,
        selector: &str,
        timeout_ms: u64,
        state: &str,
    ) -> Result<String, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        match state {
            "attached" => {
                let start = std::time::Instant::now();
                let timeout = std::time::Duration::from_millis(timeout_ms);

                while start.elapsed() < timeout {
                    match cdp(page_guard.find_element(selector)).await {
                        Ok(_) => return Ok(format!("Element '{}' found", selector)),
                        Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                    }
                }

                Err(BrowserError::ElementNotFound(
                    selector.to_string(),
                    format!("Timeout after {}ms waiting for state 'attached'. Hint: run browser_snapshot to confirm page state", timeout_ms),
                ))
            }
            "visible" | "hidden" => {
                let escaped = selector.replace('\\', "\\\\").replace('\'', "\\'");
                let want_visible = state == "visible";
                let js = format!(
                    r#"(async (s, timeoutMs, pollMs, wantVisible) => {{
    const isVisible = (el) => {{
        const r = el.getBoundingClientRect();
        if (r.width === 0 || r.height === 0) return false;
        const st = window.getComputedStyle(el);
        return st.display !== 'none' && st.visibility !== 'hidden';
    }};
    const deadline = Date.now() + timeoutMs;
    for (;;) {{
        const el = document.querySelector(s);
        const ok = wantVisible ? (el !== null && isVisible(el)) : (el === null || !isVisible(el));
        if (ok) return true;
        if (Date.now() >= deadline) throw new Error('Timeout ' + timeoutMs + 'ms waiting for element state: ' + s + ' (hint: run browser_snapshot to confirm page state)');
        await new Promise((r) => setTimeout(r, pollMs));
    }}
}})('{escaped}', {timeout_ms}, {poll}, {want_visible})"#,
                    escaped = escaped,
                    timeout_ms = timeout_ms,
                    poll = ACTIONABILITY_POLL_MS,
                    want_visible = want_visible,
                );
                cdp(page_guard.evaluate(js)).await.map_err(|e| {
                    BrowserError::ElementNotFound(
                        selector.to_string(),
                        format!("Timeout after {}ms waiting for state '{}'. Hint: run browser_snapshot to confirm page state ({})", timeout_ms, state, e),
                    )
                })?;
                Ok(format!("Element '{}' reached state '{}'", selector, state))
            }
            other => Err(BrowserError::Config(format!(
                "wait_for: invalid state '{}' (expected attached|visible|hidden)",
                other
            ))),
        }
    }

    /// Get cookies
    pub async fn cookies_get(&self) -> Result<Vec<serde_json::Value>, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        let cookies = cdp(page_guard.get_cookies()).await?;

        let values: Vec<serde_json::Value> = cookies
            .into_iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "value": c.value,
                    "domain": c.domain,
                    "path": c.path,
                    "expires": c.expires,
                    "http_only": c.http_only,
                    "secure": c.secure,
                    "same_site": c.same_site,
                })
            })
            .collect();

        Ok(values)
    }

    /// Set cookies
    pub async fn cookies_set(
        &self,
        name: &str,
        value: &str,
        domain: Option<&str>,
        path: Option<&str>,
    ) -> Result<String, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        let url = cdp(page_guard.url())
            .await
            .unwrap_or_default()
            .unwrap_or_else(|| "about:blank".to_string());

        // Set cookie using JS — the full cookie string goes through serde_json so
        // quotes/semicolons/backslashes in name/value/domain/path can neither break
        // out of the JS string nor inject extra cookie attributes.
        let mut cookie = format!("{name}={value}");
        if let Some(domain) = domain {
            cookie.push_str(&format!("; domain={domain}"));
        }
        if let Some(path) = path {
            cookie.push_str(&format!("; path={path}"));
        }
        let cookie_str = format!("document.cookie = {}", js_string_literal(&cookie));

        cdp(page_guard.evaluate(cookie_str)).await?;

        Ok(format!("Set cookie: {}={} for {}", name, value, url))
    }

    /// Close browser
    pub async fn close(&mut self) -> Result<(), BrowserError> {
        // Only send Browser.close to an instance launched by this process (it terminates the Chrome process);
        // an attached instance belongs to another process — dropping the local connection is enough, we must not close someone else's browser.
        if let Some(browser_arc) = self.browser.take() {
            if self.child_process.is_some() {
                let mut browser = browser_arc.lock().await;
                let _ = browser.close().await;
                drop(browser);
            }
            drop(browser_arc);
        }
        self.page = None;
        self.launched_headless = None;
        // Session state must not leak into the next launch: @N refs die with the
        // page, and download behavior has to be re-configured on the new browser.
        self.snapshot_backend_ids.clear();
        self.helpers_injected = false;
        self.download_configured = false;
        self.download_config_warning = None;

        // Kill the child process (managed manually, not via Browser::launch)
        if let Some(mut child) = self.child_process.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }

        Ok(())
    }

    /// Update the external CDP endpoint at runtime (UI-driven configuration).
    /// Closes any existing connection so the next `launch()` re-attaches with the
    /// new configuration; `None` (or empty) returns to managed-Chrome behavior.
    /// `close` semantics apply: an external attach is only disconnected (never
    /// kills the user's browser), a managed Chrome is shut down.
    pub async fn set_external_cdp_url(
        &mut self,
        url: Option<String>,
        identity: Option<ExternalIdentity>,
    ) -> Result<(), BrowserError> {
        let normalized = url
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty());
        if self.external_cdp_url == normalized && self.external_identity == identity {
            return Ok(());
        }
        if self.browser.is_some() {
            self.close().await?;
        }
        self.external_cdp_url = normalized;
        self.external_identity = identity;
        Ok(())
    }

    /// The current external CDP endpoint (`None` = managed-Chrome mode). May
    /// differ from the configured value after `attach_external` self-healing —
    /// callers should reconcile env/preferences with it after a successful op.
    pub fn external_cdp_url(&self) -> Option<&str> {
        self.external_cdp_url.as_deref()
    }

    /// Whether the Chromium child process launched by this client is still running:
    /// `Some(true)` alive / `Some(false)` exited / `None` no child (attached instance
    /// or handle already consumed). Used to gate reconnect-kill: a live process with
    /// an unresponsive CDP connection is a *busy* browser, not a dead one.
    pub fn child_process_alive(&mut self) -> Option<bool> {
        self.child_process.as_mut().map(|child| {
            match child.try_wait() {
                Ok(Some(_status)) => false, // exited
                Ok(None) => true,           // still running
                Err(_) => false,            // status unknown → treat as gone
            }
        })
    }

    /// Reconnect after a confirmed dead CDP connection: reset local state, relaunch a fresh
    /// browser (same window mode), and restore a usable `about:blank` page so the next
    /// operation does not fail with `NoPage`. Callers (tool executors) should probe liveness
    /// first and only invoke this when the connection is genuinely dead — a slow page with a
    /// healthy connection must NOT be torn down.
    pub async fn reconnect(&mut self) -> Result<(), BrowserError> {
        let headless_mode = self.launched_headless.unwrap_or(false);
        self.browser = None;
        self.page = None;
        self.launched_headless = None;
        self.snapshot_backend_ids.clear();
        self.helpers_injected = false;
        self.download_configured = false;
        self.download_config_warning = None;
        if let Some(mut child) = self.child_process.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.launch(headless_mode).await?;
        // Restore a usable page so page-dependent operations don't hit NoPage after the reset
        // (the caller's retried operation then targets a blank page; a navigate re-targets it).
        self.get_or_create_page().await?;
        Ok(())
    }

    /// Classify failures caused by a dead CDP connection — the only errors worth a
    /// reconnect-and-retry. Structured: transport failures are wrapped as
    /// [`BrowserError::Connection`] at the source (see [`cdp_err`]), so a page throwing
    /// `Error("WebSocket disconnected")` can NEVER spoof this classification — page JS
    /// exceptions always land in `BrowserError::Execution`.
    pub fn is_connection_error(err: &BrowserError) -> bool {
        match err {
            BrowserError::Connection(_) => true,
            // Fallback for errors stringified by outer layers before the structured
            // variant existed. Narrowed to handler-specific phrasing that chromiumoxide
            // produces only when the background handler task is gone; page-controlled
            // text ("websocket"/"disconnected"/"connection reset" in a JS exception
            // message) must never match.
            BrowserError::Execution(msg) => {
                let msg = msg.to_ascii_lowercase();
                msg.contains("receiver is gone") || msg.contains("channel closed")
            }
            _ => false,
        }
    }

    /// New tab
    pub async fn new_tab(&mut self, url: Option<&str>) -> Result<String, BrowserError> {
        let browser = self.browser.as_ref().ok_or(BrowserError::NotStarted)?;

        let browser_guard = browser.lock().await;
        let page = browser_guard
            .new_page(url.unwrap_or("about:blank"))
            .await
            .map_err(|e| BrowserError::Launch(e.to_string()))?;

        let page_arc = Arc::new(Mutex::new(page));
        self.page = Some(page_arc);
        // New page, new backendNodeId space — stale @N refs must not carry over.
        self.snapshot_backend_ids.clear();

        // Enable DOM domain for the new tab and register the anti-detection script.
        {
            let page_guard = self.page.as_ref().unwrap().lock().await;
            let _ = cdp(page_guard.execute(DOMEnable::default())).await;
            let _ = Self::inject_anti_detection(&page_guard).await;
        }

        let url_str = url.unwrap_or("about:blank");
        Ok(format!("New tab opened: {}", url_str))
    }

    /// Get all tabs info
    pub async fn list_tabs(&self) -> Result<Vec<serde_json::Value>, BrowserError> {
        let browser = self.browser.as_ref().ok_or(BrowserError::NotStarted)?;

        let browser_guard = browser.lock().await;
        let pages = cdp(browser_guard.pages()).await?;

        let mut tabs = Vec::new();
        for (i, page) in pages.iter().enumerate() {
            let url = cdp(page.url())
                .await
                .unwrap_or_default()
                .unwrap_or_else(|| "about:blank".to_string());
            let title = cdp(page.get_title())
                .await
                .unwrap_or_default()
                .unwrap_or_else(|| "Untitled".to_string());

            tabs.push(serde_json::json!({
                "index": i,
                "url": url,
                "title": title,
            }));
        }

        Ok(tabs)
    }

    /// Switch to tab (by index)
    pub async fn switch_tab(&mut self, index: usize) -> Result<String, BrowserError> {
        let browser = self.browser.as_ref().ok_or(BrowserError::NotStarted)?;

        let browser_guard = browser.lock().await;
        let pages = cdp(browser_guard.pages()).await?;

        if index >= pages.len() {
            return Err(BrowserError::Execution(format!(
                "Tab index {} out of range ({} tabs)",
                index,
                pages.len()
            )));
        }

        let page = pages
            .get(index)
            .ok_or_else(|| BrowserError::Execution("Invalid tab index".to_string()))?;

        cdp_ctx("bring selected tab to front failed", page.bring_to_front()).await?;

        let page_arc = Arc::new(Mutex::new(page.clone()));
        self.page = Some(page_arc);
        // Different tab, different backendNodeId space — stale @N refs must not carry over.
        self.snapshot_backend_ids.clear();

        let url = cdp(page.url())
            .await
            .unwrap_or_default()
            .unwrap_or_else(|| "about:blank".to_string());

        Ok(format!("Switched to tab {}: {}", index, url))
    }

    /// Get current page URL
    pub async fn current_url(&self) -> Result<String, BrowserError> {
        let page = self.get_page().await?;
        let page_guard = page.lock().await;

        let url = cdp(page_guard.url())
            .await
            .unwrap_or_default()
            .unwrap_or_else(|| "about:blank".to_string());

        Ok(url)
    }

    // ── Internal helper methods ──

    /// Register the anti-detection script on a page so CDP-driven automation is not
    /// flagged as a bot — the failure mode that turns into CAPTCHA walls mid-workflow
    /// (Nuphus workflows are explicit, user-authorized operations, never scraping).
    ///
    /// CDP-launched Chrome exposes `navigator.webdriver = true`; a real user's browser
    /// never sets it. `Page.addScriptToEvaluateOnNewDocument` overrides it before any
    /// frame script runs (on every subsequent navigation), and an immediate `Runtime.evaluate`
    /// covers the document that is already loaded at attach time.
    async fn inject_anti_detection(page: &Page) -> Result<(), BrowserError> {
        use chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams;

        const STEALTH_SOURCE: &str = r#"
            Object.defineProperty(navigator, 'webdriver', {
                get: () => undefined,
                configurable: true,
            });
        "#;

        // Apply to every future document, before its scripts run.
        cdp(page.execute(AddScriptToEvaluateOnNewDocumentParams::new(STEALTH_SOURCE))).await?;

        // Also neutralize the document that is already loaded right now.
        cdp(page.evaluate(STEALTH_SOURCE)).await?;

        Ok(())
    }

    /// After attaching to a running Chrome, wait for its existing targets to become
    /// page-attachable and adopt the first existing page as the current page.
    ///
    /// chromiumoxide's attach is asynchronous: `fetch_targets()` returns before the
    /// `AttachToTarget` responses land, and `pages()` only yields pages whose target
    /// `session_id` has been set — so right after attach `pages()` can be empty and
    /// `self.page` stays `None`. `navigate` then falls back to `new_page("about:blank")`,
    /// accumulating a fresh tab every time a later session reuses the same Chrome
    /// (the tab leak observed on orphan reattach).
    ///
    /// Polling until the first page is attachable and adopting it fixes both:
    ///  - `self.page` is initialized from the existing tab, so snapshot/click/type work
    ///    immediately and `navigate` reuses the same tab instead of opening a new one;
    ///  - `list_tabs`/`switch_tab` see the pre-existing tabs.
    ///
    /// `reload` refreshes the adopted page — used for a leftover/orphan instance (this
    /// process's own profile). The refresh is a raw `Page.reload` command plus a bounded
    /// wait for `readyState` to leave "loading": chromiumoxide's `Page::reload()` additionally
    /// waits for the `load` event, which a page with blocked subresources would hang on.
    /// An external/user browser is adopted without reloading, so the user's current page
    /// is never disturbed. Best-effort: any failure here degrades to the previous behavior
    /// (new tab on navigate) and must not fail the attach itself.
    async fn adopt_existing_page(&mut self, browser_arc: &Arc<Mutex<Browser>>, reload: bool) {
        // Attach sessions land asynchronously; poll until a page is attachable.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let first_page = loop {
            let pages = cdp(browser_arc.lock().await.pages())
                .await
                .unwrap_or_default();
            if !pages.is_empty() {
                // Prefer a real content page: Chrome's startup tab (`about:blank` /
                // `chrome://new-tab-page`) lists first and would snapshot empty, so skip
                // internal pages and adopt the first actual document. Fall back to the
                // first page if every tab is an internal page.
                let mut content_page = None;
                for p in &pages {
                    let u = cdp(p.url()).await.ok().flatten().unwrap_or_default();
                    if !is_internal_chrome_url(&u) {
                        content_page = Some(p.clone());
                        break;
                    }
                }
                break Some(content_page.unwrap_or_else(|| pages[0].clone()));
            }
            if tokio::time::Instant::now() >= deadline {
                break None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        let Some(page) = first_page else {
            tracing::debug!("[Browser] no existing page to adopt after attach");
            return;
        };

        let page_arc = Arc::new(Mutex::new(page));
        self.page = Some(page_arc.clone()); // configure_download_dir below needs it set

        if reload {
            use chromiumoxide::cdp::browser_protocol::page::ReloadParams;
            let guard = page_arc.lock().await;
            if let Err(e) = cdp(guard.execute(ReloadParams::default())).await {
                tracing::warn!("[Browser] reload of adopted page failed (leaving as-is): {e}");
            }
            drop(guard);
            // Bounded wait for the reload to leave "loading" (the DOM is usable at
            // "interactive"; a blocked-subresource page may never reach "complete").
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let ready = cdp(page_arc.lock().await.evaluate("document.readyState"))
                    .await
                    .ok()
                    .and_then(|res| res.into_value::<serde_json::Value>().ok())
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                if !ready.is_empty() && ready != "loading" {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    tracing::warn!(
                        "[Browser] adopted page still loading after reload; continuing anyway"
                    );
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }

        // Same setup as a freshly created page (see get_or_create_page): DOM domain +
        // anti-detection script (covers snapshot DOM ops and bot-detection).
        {
            let guard = page_arc.lock().await;
            let _ = cdp(guard.execute(DOMEnable::default())).await;
            let _ = Self::inject_anti_detection(&guard).await;
        }

        // Download dir config on first page (same as get_or_create_page).
        if !self.download_configured {
            if let Err(e) = self.configure_download_dir().await {
                tracing::warn!("[Browser] download dir config failed on adopted page: {e}");
            }
        }

        // Different page, different backendNodeId space — stale @N refs must not carry over.
        self.snapshot_backend_ids.clear();
        tracing::info!("[Browser] adopted existing page as current tab");
    }

    async fn get_or_create_page(&mut self) -> Result<Arc<Mutex<Page>>, BrowserError> {
        if let Some(page) = &self.page {
            return Ok(page.clone());
        }

        let browser = self.browser.as_ref().ok_or(BrowserError::NotStarted)?;

        let browser_guard = browser.lock().await;
        let page = browser_guard
            .new_page("about:blank")
            .await
            .map_err(|e| BrowserError::Launch(e.to_string()))?;

        let page_arc = Arc::new(Mutex::new(page));
        self.page = Some(page_arc.clone());

        // Enable DOM domain (required for DOM.querySelector / resolveNode / describeNode)
        // and register the anti-detection script (covers every page this instance creates).
        {
            let page_guard = page_arc.lock().await;
            let _ = cdp(page_guard.execute(DOMEnable::default())).await;
            let _ = Self::inject_anti_detection(&page_guard).await;
        }

        // Configure download behavior on first page
        if !self.download_configured {
            drop(browser_guard);
            self.configure_download_dir().await?;
        }

        Ok(page_arc)
    }

    async fn get_page(&self) -> Result<Arc<Mutex<Page>>, BrowserError> {
        self.page.clone().ok_or(BrowserError::NoPage)
    }
}

/// Browser error
#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("Browser not started. Call launch() first.")]
    NotStarted,

    #[error("No page open. Call navigate() first.")]
    NoPage,

    #[error("Browser config error: {0}")]
    Config(String),

    #[error("Browser launch error: {0}")]
    Launch(String),

    #[error("Navigation error: {0}")]
    Navigation(String),

    #[error("Element not found: {0} ({1})")]
    ElementNotFound(String, String),

    #[error("Execution error: {0}")]
    Execution(String),

    /// CDP transport-level failure (background handler task gone / websocket dropped /
    /// channel closed / no response) — the only error class worth a reconnect-and-retry.
    /// Structured variant so page-controlled text (JS exception messages) cannot spoof it.
    #[error("Connection error: {0}")]
    Connection(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Chrome not found: {0}")]
    Chrome(#[from] ChromeError),
}

/// Whether a chromiumoxide error is a CDP **transport** failure (the background
/// handler task is gone / the websocket died) as opposed to a business error
/// (page JS exception, protocol rejection, request timeout on a slow page).
fn is_transport_cdp_error(e: &chromiumoxide::error::CdpError) -> bool {
    use chromiumoxide::error::CdpError;
    matches!(
        e,
        CdpError::ChannelSendError(_) | CdpError::Ws(_) | CdpError::Io(_) | CdpError::NoResponse
    )
}

/// Wrap a chromiumoxide error preserving the transport-vs-business distinction
/// (P1: string-flattening used to let page JS exception text like "WebSocket
/// disconnected" spoof the connection-error classifier).
fn cdp_err(e: chromiumoxide::error::CdpError) -> BrowserError {
    if is_transport_cdp_error(&e) {
        BrowserError::Connection(e.to_string())
    } else {
        BrowserError::Execution(e.to_string())
    }
}

/// Whether a page URL is a browser-internal page (startup tab, tooling surface)
/// rather than a real document — used by `adopt_existing_page` to skip the empty
/// `about:blank` / `chrome://new-tab-page` tabs Chrome opens at startup.
fn is_internal_chrome_url(url: &str) -> bool {
    url.is_empty()
        || url.starts_with("about:")
        || url.starts_with("chrome://")
        || url.starts_with("devtools://")
        || url.starts_with("edge://")
}

/// Run a single CDP evaluate/execute with a hard timeout budget.
///
/// The tool-level guard is 30s; without this wrapper a wedged CDP call (page
/// mid-navigation, crashed renderer) hangs the whole tool for that long. On
/// timeout the future is simply dropped — chromiumoxide tolerates an unanswered
/// command — and the caller gets a fast `Connection` error.
async fn cdp_with_timeout<T>(
    budget: std::time::Duration,
    fut: impl std::future::Future<Output = Result<T, chromiumoxide::error::CdpError>>,
) -> Result<T, BrowserError> {
    tokio::time::timeout(budget, fut)
        .await
        .map_err(|_| {
            BrowserError::Connection(format!(
                "CDP did not respond within {}s (page may be navigating or unresponsive)",
                budget.as_secs()
            ))
        })?
        .map_err(cdp_err)
}

/// Run a single CDP evaluate/execute with the standard [`CDP_CALL_TIMEOUT`]
/// budget. Call sites that wrap a self-bounded JS wait (the actionability loop,
/// the change detector) use [`cdp_with_timeout`] / [`cdp_ctx_with_timeout`]
/// with a budget that exceeds the JS side, so the JS produces its own
/// descriptive timeout error instead of being preempted by the tokio one.
async fn cdp<T>(
    fut: impl std::future::Future<Output = Result<T, chromiumoxide::error::CdpError>>,
) -> Result<T, BrowserError> {
    cdp_with_timeout(CDP_CALL_TIMEOUT, fut).await
}

/// Same classification with operation context prefixed (shared by [`cdp_ctx`]).
fn cdp_err_ctx(ctx: &str, e: chromiumoxide::error::CdpError) -> BrowserError {
    if is_transport_cdp_error(&e) {
        BrowserError::Connection(format!("{ctx}: {e}"))
    } else {
        BrowserError::Execution(format!("{ctx}: {e}"))
    }
}

/// [`cdp_with_timeout`] with an operation context prefixed onto the error. The
/// context is applied to both the CDP error and the timeout error, so a failing
/// call reads like `Click on '#x' failed: CDP did not respond within 5s (...)`,
/// and the transport-vs-business classification is preserved.
async fn cdp_ctx_with_timeout<T>(
    ctx: &str,
    budget: std::time::Duration,
    fut: impl std::future::Future<Output = Result<T, chromiumoxide::error::CdpError>>,
) -> Result<T, BrowserError> {
    tokio::time::timeout(budget, fut)
        .await
        .map_err(|_| {
            BrowserError::Connection(format!(
                "{ctx}: CDP did not respond within {}s (page may be navigating or unresponsive)",
                budget.as_secs()
            ))
        })?
        .map_err(|e| cdp_err_ctx(ctx, e))
}

/// [`cdp_ctx_with_timeout`] with the standard [`CDP_CALL_TIMEOUT`] budget.
async fn cdp_ctx<T>(
    ctx: &str,
    fut: impl std::future::Future<Output = Result<T, chromiumoxide::error::CdpError>>,
) -> Result<T, BrowserError> {
    cdp_ctx_with_timeout(ctx, CDP_CALL_TIMEOUT, fut).await
}

/// Serialize a Rust string as a JavaScript string literal (double-quoted, fully
/// escaped) for safe interpolation into generated JS. serde_json string
/// serialization is infallible for `&str`; the fallback is defensive only.
fn js_string_literal(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Parse a Playwright-style key chord into a chromiumoxide key name and the
/// CDP modifier bit field (Alt=1, Control=2, Meta=4, Shift=8).
fn parse_key_chord(chord: &str) -> Result<(String, i64), BrowserError> {
    let chord = chord.trim();
    if chord.is_empty() {
        return Err(BrowserError::Execution(
            "key chord must not be empty".to_string(),
        ));
    }

    // `+` is itself a valid key. `Plus` also avoids ambiguity in modifier chords.
    if chord == "+" {
        return Ok(("+".to_string(), 0));
    }

    let mut parts: Vec<&str> = chord.split('+').map(str::trim).collect();
    let target = parts.pop().unwrap_or_default();
    if target.is_empty() {
        return Err(BrowserError::Execution(format!(
            "invalid key chord '{chord}': missing target key (use Plus for the '+' key in a chord)"
        )));
    }

    let mut modifiers = 0;
    for modifier in parts {
        let bit = match modifier.to_ascii_lowercase().as_str() {
            "alt" | "option" => 1,
            "control" | "ctrl" => 2,
            "meta" | "command" | "cmd" => 4,
            "shift" => 8,
            _ => {
                return Err(BrowserError::Execution(format!(
                    "unsupported modifier '{modifier}' in chord '{chord}' (expected Alt, Control, Meta, or Shift)"
                )))
            }
        };
        modifiers |= bit;
    }

    let key = match target.to_ascii_lowercase().as_str() {
        "space" => " ",
        "plus" => "+",
        "return" => "Enter",
        "esc" => "Escape",
        "del" => "Delete",
        _ => target,
    };
    Ok((key.to_string(), modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_launch_diagnostics_classify_sandbox_failures() {
        for stderr in [
            "Running as root without --no-sandbox is not supported.",
            "No usable sandbox! Update your kernel.",
            "The SUID sandbox helper binary was found, but is not configured correctly.",
            "Failed to move to new namespace: Operation not permitted",
        ] {
            let mut tail = VecDeque::new();
            push_chrome_stderr(&mut tail, stderr);
            push_chrome_stderr(&mut tail, "Chrome shutdown completed");
            assert_eq!(
                classify_chrome_launch_failure(&tail),
                ChromeLaunchFailureKind::SandboxUnavailable
            );
            let message = chrome_launch_error("Chrome exited", &tail).to_string();
            assert!(message.contains("sandbox initialization failed"));
            assert!(message.contains("will not retry with --no-sandbox"));
            assert!(message.contains(stderr));
        }
    }

    #[test]
    fn chrome_launch_diagnostics_classify_enterprise_policy() {
        let mut tail = VecDeque::new();
        push_chrome_stderr(&mut tail, "Remote debugging is disabled by policy");
        assert_eq!(
            classify_chrome_launch_failure(&tail),
            ChromeLaunchFailureKind::RemoteDebuggingBlocked
        );
        let message = chrome_launch_error("Chrome exited", &tail).to_string();
        assert!(message.contains("system or enterprise policy"));
        assert!(message.contains("Allow remote debugging"));
    }

    #[test]
    fn chrome_launch_diagnostics_keep_other_failures_generic_and_bounded() {
        let mut tail = VecDeque::new();
        for index in 0..(CHROME_STDERR_MAX_LINES + 3) {
            push_chrome_stderr(
                &mut tail,
                &format!("ordinary failure {index} {}", "x".repeat(800)),
            );
        }
        assert_eq!(tail.len(), CHROME_STDERR_MAX_LINES);
        assert!(tail.front().is_some_and(|line| line.contains("failure 3")));
        assert!(tail
            .iter()
            .all(|line| line.chars().count() <= CHROME_STDERR_MAX_LINE_CHARS));
        assert_eq!(
            classify_chrome_launch_failure(&tail),
            ChromeLaunchFailureKind::Other
        );
        let message = chrome_launch_error("Chrome exited", &tail).to_string();
        assert!(message.contains("ordinary failure"));
        assert!(!message.contains("sandbox initialization failed"));
    }

    // ── Unit tests (no browser) ──

    #[test]
    fn key_chord_parser_supports_terminal_keys_and_aliases() {
        assert_eq!(parse_key_chord("Enter").unwrap(), ("Enter".into(), 0));
        assert_eq!(parse_key_chord("Ctrl+c").unwrap(), ("c".into(), 2));
        assert_eq!(
            parse_key_chord("Control+Shift+P").unwrap(),
            ("P".into(), 10)
        );
        assert_eq!(parse_key_chord("Cmd+ArrowLeft").unwrap().1, 4);
        assert_eq!(parse_key_chord("Space").unwrap(), (" ".into(), 0));
        assert_eq!(parse_key_chord("Shift+Plus").unwrap(), ("+".into(), 8));
        assert!(parse_key_chord("").is_err());
        assert!(parse_key_chord("Control+").is_err());
        assert!(parse_key_chord("Hyper+x").is_err());
    }

    #[test]
    fn actionability_script_escapes_selector_and_embeds_constants() {
        let js = BrowserClient::actionability_script("a.b'c\\d", "el.click(); return 'clicked';");
        // Selector escaping: backslash doubled, single quote escaped
        assert!(
            js.contains(r#"('a.b\'c\\d', "#),
            "escaped selector missing: {}",
            js
        );
        // Default timeout / poll constants embedded
        assert!(js.contains(&ACTIONABILITY_TIMEOUT_MS.to_string()));
        assert!(js.contains(&ACTIONABILITY_POLL_MS.to_string()));
        // Visibility predicate + diagnostic hint present
        assert!(js.contains("getBoundingClientRect"));
        assert!(js.contains("visibility !== 'hidden'"));
        assert!(js.contains("browser_snapshot"));
        // Action snippet inlined
        assert!(js.contains("el.click(); return 'clicked';"));
    }

    #[test]
    fn stale_node_error_classification() {
        // resolveNode / detached-node failures → retryable
        assert!(BrowserClient::is_stale_node_error(
            &BrowserError::Execution(
                "DOM.resolveNode failed: No node with given id found".to_string()
            )
        ));
        assert!(BrowserClient::is_stale_node_error(
            &BrowserError::Execution("Click JS exception on @3: node is detached".to_string())
        ));
        assert!(BrowserClient::is_stale_node_error(
            &BrowserError::Execution(
                "Runtime.callFunctionOn failed: Node is not attached to the page".to_string()
            )
        ));
        // Out-of-range / generic failures → NOT retryable
        assert!(!BrowserClient::is_stale_node_error(
            &BrowserError::ElementNotFound(
                "@9".to_string(),
                "@9 out of range (max @4)".to_string()
            )
        ));
        assert!(!BrowserClient::is_stale_node_error(
            &BrowserError::Execution("Click on '#x' failed: some other error".to_string())
        ));
    }

    #[test]
    fn connection_error_classification() {
        // Structured transport variant → reconnectable
        assert!(BrowserClient::is_connection_error(
            &BrowserError::Connection("send failed because receiver is gone".to_string())
        ));
        // String fallback: only handler-specific phrasing classifies
        assert!(BrowserClient::is_connection_error(
            &BrowserError::Execution("send failed because receiver is gone".to_string())
        ));
        assert!(BrowserClient::is_connection_error(&BrowserError::Execution(
            "Browser 'browser_extract' failed: Execution error: send failed because receiver is gone"
                .to_string()
        )));
        assert!(BrowserClient::is_connection_error(
            &BrowserError::Execution("channel closed while waiting for response".to_string())
        ));
        // Page-spoofable phrasing must NOT classify, even in the fallback — a page
        // throwing Error("WebSocket disconnected") used to kill a healthy browser.
        for spoof in [
            "WebSocket disconnected",
            "Error: websocket connection reset by peer",
            "connection closed unexpectedly",
            "CDP connect failed: ws error",
            "Exception: read ECONNRESET",
        ] {
            assert!(
                !BrowserClient::is_connection_error(&BrowserError::Execution(spoof.to_string())),
                "page-spoofable text must not classify as connection error: {spoof}"
            );
        }
        // Business errors → NOT reconnectable (must not mask the real problem)
        assert!(!BrowserClient::is_connection_error(
            &BrowserError::ElementNotFound(
                "@9".to_string(),
                "@9 out of range (max @4)".to_string()
            )
        ));
        assert!(!BrowserClient::is_connection_error(
            &BrowserError::Execution("Click on '#x' failed: selector not found".to_string())
        ));
        assert!(!BrowserClient::is_connection_error(&BrowserError::Navigation(
            "navigation timed out after 22s — page did not finish loading (unreachable host, blocked subresources, or very slow)".to_string()
        )));
        assert!(!BrowserClient::is_connection_error(&BrowserError::NoPage));
    }

    #[test]
    fn cdp_err_maps_transport_vs_business() {
        use chromiumoxide::error::CdpError;
        // Transport: handler gone (NoResponse) → Connection variant → reconnectable
        let e = cdp_err(CdpError::NoResponse);
        assert!(matches!(e, BrowserError::Connection(_)));
        assert!(BrowserClient::is_connection_error(&e));
        // Business: request timeout (slow page, NOT death) → Execution
        let e = cdp_err(CdpError::Timeout);
        assert!(matches!(e, BrowserError::Execution(_)));
        assert!(!BrowserClient::is_connection_error(&e));
        // Page-controlled text via CDP error messages stays business — the exact
        // spoof that used to kill a healthy browser via substring matching.
        let e = cdp_err(CdpError::ChromeMessage(
            "WebSocket disconnected".to_string(),
        ));
        assert!(matches!(e, BrowserError::Execution(_)));
        assert!(!BrowserClient::is_connection_error(&e));
        // Context-prefixed wrapper preserves the class on both sides
        let e = cdp_err_ctx("Click on '#x' failed", CdpError::NoResponse);
        assert!(matches!(e, BrowserError::Connection(_)));
        let e = cdp_err_ctx("Click on '#x' failed", CdpError::Timeout);
        assert!(matches!(e, BrowserError::Execution(_)));
    }

    #[test]
    fn js_string_literal_escapes_safely() {
        // Quotes / backslashes / newlines are escaped; roundtrip via JSON parse.
        for s in [
            "input[name='q']",
            "a'b\\c\"d",
            "line1\nline2",
            "'; alert(1); //",
        ] {
            let lit = js_string_literal(s);
            let back: String = serde_json::from_str(&lit).expect("valid JSON string literal");
            assert_eq!(back, s, "literal must roundtrip: {s}");
        }
        // A selector containing quotes must not be able to break out of the JS string.
        let lit = js_string_literal("';alert(1);//");
        assert!(!lit.contains("''"), "no raw quote breakout: {lit}");
    }

    // ── External (fingerprint) browser self-healing ──

    fn cmdline(args: &[&str]) -> Vec<std::ffi::OsString> {
        args.iter().map(std::ffi::OsString::from).collect()
    }

    fn test_identity() -> ExternalIdentity {
        ExternalIdentity {
            name: "AdsPower".to_string(),
            exe_path: r"C:\Users\x\AppData\Roaming\adspower_global\sunbrowser.exe".to_string(),
            user_data_dir: None,
        }
    }

    #[test]
    fn parse_cmdline_literal_port_and_profile() {
        let (port, profile) = parse_debug_cmdline(&cmdline(&[
            "sunbrowser.exe",
            "--remote-debugging-port=9222",
            "--user-data-dir=C:\\tmp\\prof",
        ]));
        assert_eq!(port, Some(9222));
        assert_eq!(profile, Some(PathBuf::from("C:\\tmp\\prof")));
    }

    #[test]
    fn parse_cmdline_space_separated_and_quoted() {
        let (port, _) = parse_debug_cmdline(&cmdline(&[
            "sunbrowser.exe",
            "--remote-debugging-port",
            "9333",
        ]));
        assert_eq!(port, Some(9333));
        let (_, profile) = parse_debug_cmdline(&cmdline(&[
            "sunbrowser.exe",
            "\"--user-data-dir=C:\\.ADSPOWER_GLOBAL\\cache\\k1ffh0or\"",
        ]));
        assert_eq!(
            profile,
            Some(PathBuf::from("C:\\.ADSPOWER_GLOBAL\\cache\\k1ffh0or"))
        );
    }

    #[test]
    fn resolve_random_port_via_devtools_active_port() {
        // AdsPower SunBrowser launches with --remote-debugging-port=0; the real
        // port lands in <user-data-dir>/DevToolsActivePort (first line).
        let dir = std::env::temp_dir().join(format!("nuphus-heal-dap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("DevToolsActivePort"),
            "54738\n/devtools/browser/abc\n",
        )
        .unwrap();
        assert_eq!(resolve_debug_port(0, Some(&dir)), Some(54738));
        // Literal port is returned as-is; random port without profile is unresolvable.
        assert_eq!(resolve_debug_port(9222, None), Some(9222));
        assert_eq!(resolve_debug_port(0, None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failure_message_process_not_found_is_actionable() {
        // Window closed: name the browser, tell the user to reopen the window,
        // and explicitly forbid switching browsers / changing config / blind retries.
        let msg = attach_failure_message(
            "http://127.0.0.1:9222",
            Some(&test_identity()),
            Some(&ExternalHeal::ProcessNotFound),
        );
        assert!(msg.contains("AdsPower"), "names the browser: {msg}");
        assert!(
            msg.contains("没有运行中的窗口"),
            "states the window is closed: {msg}"
        );
        assert!(
            msg.contains("连接会自动恢复"),
            "promises auto-recovery: {msg}"
        );
        assert!(msg.contains("不要切换浏览器"), "forbids switching: {msg}");
        assert!(
            msg.contains("不要修改配置"),
            "forbids config changes: {msg}"
        );
        assert!(msg.contains("不要盲目重试"), "forbids blind retries: {msg}");
        assert!(
            !msg.contains("--remote-debugging-port"),
            "developer detail stays out of the message: {msg}"
        );
    }

    #[test]
    fn failure_message_port_unresponsive_guides_reopen() {
        let msg = attach_failure_message(
            "http://127.0.0.1:9222",
            Some(&test_identity()),
            Some(&ExternalHeal::PortUnresponsive),
        );
        assert!(msg.contains("AdsPower"), "names the browser: {msg}");
        assert!(msg.contains("调试端口无响应"), "states the cause: {msg}");
        assert!(
            msg.contains("重新打开"),
            "guides reopening the window: {msg}"
        );
        assert!(msg.contains("不要切换浏览器"), "forbids switching: {msg}");
    }

    #[test]
    fn failure_message_without_identity_guides_to_settings() {
        // Legacy config (URL only): no self-heal possible — guide the user to
        // re-pick the window in the settings page; never suggest managed Chrome.
        let msg = attach_failure_message("http://127.0.0.1:9222", None, None);
        assert!(
            msg.contains("http://127.0.0.1:9222"),
            "shows the endpoint: {msg}"
        );
        assert!(msg.contains("设置页"), "guides to the settings page: {msg}");
        assert!(
            msg.contains("重新检测并选择"),
            "guides re-picking the window: {msg}"
        );
        assert!(
            msg.contains("不要切换到内置浏览器"),
            "forbids managed fallback: {msg}"
        );
        assert!(
            !msg.contains("--remote-debugging-port"),
            "developer detail stays out of the message: {msg}"
        );
    }

    #[tokio::test]
    async fn heal_without_running_process_reports_not_found() {
        // An exe path no process can match (also covers find_identity_processes
        // not panicking on a live process scan).
        let mut id = test_identity();
        id.exe_path = r"Z:\definitely\not\existing\nuphus-no-such-browser.exe".to_string();
        assert!(matches!(
            heal_external_endpoint(&id).await,
            ExternalHeal::ProcessNotFound
        ));
    }

    // ── Integration tests (real Chrome, #[ignore]) ──
    // Run: cargo test --lib browser::client::tests:: -- --ignored --test-threads=1
    // (single-threaded: multiple tests share the same Chrome profile; parallel runs collide on SingletonLock)

    /// Anti-detection: after `inject_anti_detection`, `navigator.webdriver` must not be
    /// exposed as the CDP-forced `true` — the failure mode that turns into CAPTCHA walls
    /// mid-workflow for user-authorized automation.
    #[tokio::test]
    #[ignore = "launches real Chrome; requires Chrome installed locally"]
    async fn anti_detection_webdriver_is_hidden() {
        let mut client = isolated_client("webdriver");
        client.launch(true).await.expect("launch headless");
        client
            .navigate(&fixture_url(
                "webdriver",
                "<!doctype html><html><body>probe</body></html>",
            ))
            .await
            .expect("navigate");

        // Plain CDP-driven Chrome would report `true` here; after injection the getter
        // returns undefined, so the script reports `hidden`.
        let value = client
            .evaluate(
                "typeof navigator.webdriver === 'undefined' ? 'hidden' : String(navigator.webdriver)",
            )
            .await
            .expect("evaluate navigator.webdriver");

        assert_eq!(
            value.as_str().map(str::to_string),
            Some("hidden".to_string()),
            "navigator.webdriver should be hidden after anti-detection injection, got: {value}"
        );
    }

    /// Connection-level self-healing: after the Chrome child process is killed (an externally
    /// killed / crashed browser), a direct operation hangs instead of failing fast (Windows
    /// half-open websocket), the liveness probe reports the dead connection, and `reconnect()`
    /// resets + relaunches + restores a usable page so the retried operation succeeds — the
    /// caller observes recovery instead of a dead-connection error. This is the exact failure
    /// mode of "receiver is gone" that users hit mid-workflow.
    #[tokio::test]
    #[ignore = "launches real Chrome; requires Chrome installed locally"]
    async fn reconnect_recovers_dead_connection() {
        let mut client = isolated_client("reconnect");
        client.launch(true).await.expect("launch headless");
        client
            .navigate(&fixture_url(
                "reconnect",
                "<!doctype html><html><body>alive</body></html>",
            ))
            .await
            .expect("navigate");

        // Prove the connection works before killing it.
        assert!(client.snapshot(false, None).await.is_ok());

        // Kill the Chrome child process to simulate an externally-killed browser.
        let mut child = client.child_process.take().expect("child process exists");
        child.kill().await.expect("kill chrome");
        child.wait().await.expect("chrome exited");

        // On Windows a killed Chrome does NOT surface as a fast error: the handler may block
        // on the half-open websocket, so a direct operation hangs (verified below) instead of
        // failing with "receiver is gone". This is exactly why the self-healing path must
        // combine timeout + liveness probe + reconnect.
        let hung = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client.snapshot(false, None),
        )
        .await;
        assert!(
            hung.is_err(),
            "direct snapshot on dead connection should hang past the probe window"
        );
        // The liveness probe confirms the connection is gone...
        assert!(
            !client.is_connection_alive().await,
            "liveness probe must report the dead connection"
        );

        // ...and reconnect() resets, relaunches, restores a blank page; the retried
        // operation succeeds instead of failing with NoPage / dead connection.
        client
            .reconnect()
            .await
            .expect("reconnect after dead connection");
        let recovered = client.snapshot(false, None).await;
        assert!(
            recovered.is_ok(),
            "operation after reconnect should succeed: {:?}",
            recovered.err()
        );

        // The new instance is usable for real work.
        client
            .navigate(&fixture_url(
                "reconnect",
                "<!doctype html><html><body>again</body></html>",
            ))
            .await
            .expect("navigate after reconnect");

        let _ = client.close().await;
        cleanup_profile("reconnect");
    }

    /// Regression: after attaching to a running Chrome (a leftover/orphan instance, or a
    /// second process on the same profile), the existing page must be adopted as the
    /// current tab — `list_tabs` non-empty, `snapshot` works immediately (not "No page
    /// open"), and `navigate` reuses the same tab instead of accumulating a new one.
    ///
    /// Before the fix, chromiumoxide's attach is async (`fetch_targets` returns before the
    /// `AttachToTarget` responses land, and `pages()` only yields pages whose `session_id`
    /// is set), so `self.page` stayed `None` after attach and every `navigate` fell back to
    /// `new_page("about:blank")`, growing the tab list on every reattach.
    #[tokio::test]
    #[ignore]
    async fn attach_adopts_existing_page_real_chrome() {
        // Instance A: a live Chrome with one page. Snapshot only lists interactive
        // elements (buttons/links/inputs), so the fixture must contain one.
        let mut a = isolated_client("adopt");
        a.launch(true).await.expect("launch A");
        a.navigate(&fixture_url(
            "adopt",
            "<!doctype html><html><body><button>page-a</button></body></html>",
        ))
        .await
        .expect("navigate A");
        assert!(
            a.snapshot(false, None).await.unwrap().contains("page-a"),
            "A's own page should snapshot its button"
        );

        // Instance B: same profile → try_attach connects to A's Chrome and adopts the
        // existing page as the current tab.
        let mut b = isolated_client("adopt");
        b.launch(true).await.expect("launch B (attach)");

        // list_tabs enumerates the pre-existing tab (was [] before the fix).
        let tabs_before = b.list_tabs().await.expect("list_tabs after attach");
        assert!(
            !tabs_before.is_empty(),
            "attach must enumerate existing tabs, got {tabs_before:?}"
        );

        // snapshot works immediately on the adopted page (was "No page open").
        let snap = b
            .snapshot(false, None)
            .await
            .expect("snapshot on adopted page");
        assert!(
            snap.contains("page-a"),
            "adopted page should reflect the existing tab content, got: {snap}"
        );

        // navigate reuses the adopted tab — the tab count must not grow. Use a
        // different URL than A (a same-URL navigate is a degenerate reload that
        // exercises a different path; here we want the plain re-navigate case).
        b.navigate(&fixture_url(
            "adopt_b",
            "<!doctype html><html><body><button>page-b</button></body></html>",
        ))
        .await
        .expect("navigate on adopted page");
        let tabs_after = b.list_tabs().await.expect("list_tabs after navigate");
        assert_eq!(
            tabs_before.len(),
            tabs_after.len(),
            "navigate must reuse the adopted tab, not open a new one: {tabs_after:?}"
        );

        b.close().await.expect("close B (attach, disconnect only)");
        a.close().await.expect("close A (kills chrome)");
        cleanup_profile("adopt");
    }

    /// Test client with an isolated profile: avoids sharing the running Nuphus App's
    /// browser_profile_v2 (try_attach would connect to the App instance and hang navigate).
    /// Each test uses its own directory to avoid SingletonLock conflicts.
    fn isolated_client(name: &str) -> BrowserClient {
        let mut client = BrowserClient::new().expect("chrome required");
        client.profile_dir = std::env::temp_dir().join(format!("nuphus_autowait_profile_{}", name));
        client
    }

    /// Best-effort cleanup of the isolated profile directory after close (Chrome handle release may lag).
    fn cleanup_profile(name: &str) {
        let dir = std::env::temp_dir().join(format!("nuphus_autowait_profile_{}", name));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Write a temp HTML fixture and return its file:// URL.
    fn fixture_url(name: &str, html: &str) -> String {
        let path = std::env::temp_dir().join(format!("nuphus_autowait_{}.html", name));
        std::fs::write(&path, html).expect("write fixture");
        let p = path.to_string_lossy().replace('\\', "/");
        format!("file:///{}", p.trim_start_matches('/'))
    }

    const DELAYED_PAGE: &str = r#"<!doctype html><html><body>
<div id="root"></div>
<script>
setTimeout(function() {
    var b = document.createElement('button');
    b.id = 'delayed-btn';
    b.textContent = 'late-button';
    b.onclick = function() { b.textContent = 'was-clicked'; };
    document.getElementById('root').appendChild(b);
    var i = document.createElement('input');
    i.id = 'delayed-input';
    document.getElementById('root').appendChild(i);
}, 1200);
</script>
</body></html>"#;

    const STATE_PAGE: &str = r#"<!doctype html><html><body>
<div id="will-hide">ghost</div>
<div id="will-show" style="display:none">surprise</div>
<script>
setTimeout(function(){ document.getElementById('will-hide').remove(); }, 1000);
setTimeout(function(){ document.getElementById('will-show').style.display = 'block'; }, 1500);
</script>
</body></html>"#;

    const STATIC_PAGE: &str = r#"<!doctype html><html><body>
<div id="static-node">static</div>
</body></html>"#;

    const KEYBOARD_PAGE: &str = r#"<!doctype html><html><body>
<button id="previous">previous focus target</button>
<textarea id="target" autofocus></textarea>
<script>
window.events = [];
const target = document.getElementById('target');
target.addEventListener('keydown', function(event) {
    window.events.push({
        type: event.type,
        key: event.key,
        code: event.code,
        alt: event.altKey,
        ctrl: event.ctrlKey,
        meta: event.metaKey,
        shift: event.shiftKey,
        trusted: event.isTrusted
    });
});
</script>
</body></html>"#;

    /// Native keyboard dispatch: named keys and modifier chords reach the
    /// focused element as trusted DOM keyboard events.
    #[tokio::test]
    #[ignore = "launches real Chrome; requires Chrome installed locally"]
    async fn press_key_dispatches_trusted_events_real_chrome() {
        let mut client = isolated_client("keyboard");
        client.launch(true).await.expect("launch headless");
        client
            .navigate(&fixture_url("keyboard", KEYBOARD_PAGE))
            .await
            .expect("navigate");

        client.press_key("Enter").await.expect("press Enter");
        client.press_key("Ctrl+c").await.expect("press Ctrl+c");
        client
            .press_key("Shift+Tab")
            .await
            .expect("press Shift+Tab");

        let value = client
            .evaluate("JSON.stringify(window.events)")
            .await
            .expect("read events");
        let serialized = value.as_str().expect("events JSON string");
        let events: Vec<serde_json::Value> =
            serde_json::from_str(serialized).expect("parse events");
        assert_eq!(events.len(), 3, "events: {serialized}");
        assert_eq!(events[0]["key"], "Enter");
        assert_eq!(events[0]["code"], "Enter");
        assert_eq!(events[0]["trusted"], true);
        assert_eq!(events[1]["key"], "c");
        assert_eq!(events[1]["ctrl"], true);
        assert_eq!(events[1]["trusted"], true);
        assert_eq!(events[2]["key"], "Tab");
        assert_eq!(events[2]["shift"], true);

        client.close().await.expect("close");
        cleanup_profile("keyboard");
    }

    /// Regression: a second operation on an established connection must not kill the page.
    ///
    /// The CDP liveness probe used to call `fetch_targets`, whose `Target.getTargets`
    /// response handler re-creates every existing target and drops their PageHandles,
    /// so any operation after the first (probe runs again → page channel receiver gone)
    /// failed with "send failed because receiver is gone" while the probe still reported
    /// the connection alive. Fixed by probing with the side-effect-free `version()`.
    /// Two navigations on the same page must both succeed.
    #[tokio::test]
    #[ignore]
    async fn probe_must_not_kill_existing_page() {
        let mut client = isolated_client("probe_regression");
        client.launch(true).await.expect("launch");

        let url = fixture_url("probe_regression", "<h1>hello</h1>");
        client
            .navigate(&url)
            .await
            .expect("first navigation should succeed");
        // launch() runs the liveness probe on the second call — this used to drop the page.
        client
            .navigate(&url)
            .await
            .expect("second navigation must not fail with a dead page channel");
        // A page-level command must still reach the handler (get_title returns Ok).
        let page = client.page.as_ref().expect("page exists").clone();
        {
            let guard = page.lock().await;
            guard
                .get_title()
                .await
                .expect("page command must still work after second launch()");
        }

        client.close().await.ok();
        cleanup_profile("probe_regression");
    }

    /// Regression: navigate must not hang until the tool-level 30s timeout on a
    /// page whose `load` event is permanently blocked by a hanging subresource.
    ///
    /// chromiumoxide's `Page::goto` holds the `Page.navigate` response until the
    /// main frame's `load` lifecycle event (with a 30s deadline). An `<img>` that
    /// receives response headers but never completes its body keeps the frame
    /// loading forever, so `load` never fires. The fix bounds the goto wait and
    /// degrades to polling `document.readyState` — the DOM is usable at
    /// "interactive" even while subresources are still pending. Navigate must
    /// return success (with a "still loading" note) far below the old 30s hard
    /// timeout.
    #[tokio::test]
    #[ignore = "launches real Chrome; requires Chrome installed locally"]
    async fn navigate_slow_page_does_not_hang_30s() {
        // Mini HTTP server: `/` serves the test page over http (avoids Chrome's
        // mixed-content fast-fail on file:// + http img), and `/hang` sends
        // response headers declaring a huge Content-Length but never the body, so
        // the img request stays pending forever and the `load` event is blocked.
        // `Connection: close` on `/` forces a fresh connection for the img so the
        // hanging response can't be affected by the main page's connection.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind hang listener");
        let port = listener.local_addr().expect("addr").port();
        let hang_html = format!(
            "<!doctype html><html><body><h1 id='main'>slow</h1>\
             <img src='http://127.0.0.1:{port}/hang'></body></html>"
        );
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let hang_html = hang_html.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await; // request headers
                    if buf.starts_with(b"GET /hang") {
                        // Declare 1GB but never send the body → request stays pending.
                        let _ = sock
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000000\r\n\r\n")
                            .await;
                        std::future::pending::<()>().await; // hold the socket open
                    } else {
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\nContent-Type: text/html\r\n\r\n{}",
                            hang_html.len(),
                            hang_html
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    }
                });
            }
        });

        let mut client = isolated_client("navigate_slow_page");
        client.launch(true).await.expect("launch");

        let start = std::time::Instant::now();
        let result = client.navigate(&format!("http://127.0.0.1:{port}/")).await;
        let elapsed = start.elapsed();

        let msg = result.expect("slow page should still navigate successfully");
        assert!(
            elapsed.as_secs() < 25,
            "navigate took too long on a slow page: {elapsed:?} — goto bound or readyState fallback not working"
        );
        assert!(msg.starts_with("Navigated to"), "unexpected result: {msg}");
        // The load event is blocked, so the degradation path must have run.
        assert!(
            msg.contains("still loading"),
            "expected the 'still loading subresources' fallback note: {msg}"
        );

        // The DOM is usable even though the page is still loading: the heading is
        // parsed and reachable even though the `load` event never fired.
        let page = client.page.as_ref().expect("page exists").clone();
        {
            let guard = page.lock().await;
            let heading = cdp(
                guard.evaluate("document.querySelector('#main') ? document.querySelector('#main').textContent : ''"),
            )
            .await
            .ok()
            .and_then(|r| r.into_value::<serde_json::Value>().ok())
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
            assert_eq!(
                heading, "slow",
                "DOM should be parsed and reachable on slow page"
            );
        }

        client.close().await.ok();
        cleanup_profile("navigate_slow_page");
    }

    /// wait_for three states: attached hits immediately; visible waits for the element to
    /// transition from display:none to visible; hidden waits for the element to be removed;
    /// an absent element with visible times out → error contains the selector and troubleshooting hint;
    /// invalid state → Config error.
    #[tokio::test]
    #[ignore = "launches real Chrome; requires Chrome installed locally"]
    async fn wait_for_state_transitions_real_chrome() {
        let mut client = isolated_client("state");
        client.launch(true).await.expect("launch headless");
        client
            .navigate(&fixture_url("state", STATE_PAGE))
            .await
            .expect("navigate");

        // attached (default semantics): an already-present element hits immediately
        let r = client
            .wait_for("#will-hide", 3000, "attached")
            .await
            .expect("attached");
        assert!(r.contains("#will-hide"));

        // visible: element starts display:none, becomes visible after 1.5s → wait succeeds
        let r = client
            .wait_for("#will-show", 5000, "visible")
            .await
            .expect("visible");
        assert!(r.contains("visible"));

        // hidden: element removed after 1s → wait succeeds (hidden = absent or invisible)
        let r = client
            .wait_for("#will-hide", 5000, "hidden")
            .await
            .expect("hidden");
        assert!(r.contains("hidden"));

        // hidden holds immediately for an element that never existed
        client
            .wait_for("#never-existed", 2000, "hidden")
            .await
            .expect("hidden for absent");

        // Timeout: error must contain the selector, the timeout value, and the troubleshooting hint
        let err = client
            .wait_for("#never-exists", 800, "visible")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("#never-exists"), "selector missing: {}", msg);
        assert!(msg.contains("800"), "timeout missing: {}", msg);
        assert!(msg.contains("browser_snapshot"), "hint missing: {}", msg);

        // Invalid state
        let err = client
            .wait_for("#will-show", 500, "gone")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("attached|visible|hidden"));

        client.close().await.expect("close");
        cleanup_profile("state");
    }

    /// click/type_text CSS path auto-wait: the button and input are only inserted 1.2s after page load;
    /// an immediate call should wait rather than error; the click side effect and the typed value are both verifiable;
    /// a nonexistent element times out after 5s → error contains the selector and the troubleshooting hint.
    #[tokio::test]
    #[ignore = "launches real Chrome; requires Chrome installed locally"]
    async fn click_and_type_css_path_auto_wait_real_chrome() {
        let mut client = isolated_client("delayed");
        client.launch(true).await.expect("launch headless");
        client
            .navigate(&fixture_url("delayed", DELAYED_PAGE))
            .await
            .expect("navigate");

        // Element does not exist yet — click must auto-wait ~1.2s and then succeed
        let r = client
            .click("#delayed-btn")
            .await
            .expect("click should auto-wait");
        assert!(r.contains("#delayed-btn"));
        let page_text = client.extract(2000).await.expect("extract");
        assert!(
            page_text.contains("was-clicked"),
            "click side effect missing: {}",
            page_text
        );

        // type_text also auto-waits and writes the real input value
        client
            .type_text("#delayed-input", "hello-nuphus")
            .await
            .expect("type should auto-wait");
        let results = client
            .batch_exec("const v = document.querySelector('#delayed-input').value; h._results.push({ op: 'assert', success: v === 'hello-nuphus', detail: v });")
            .await
            .expect("batch assert");
        assert!(
            results.contains(r#""success":true"#),
            "typed value mismatch: {}",
            results
        );

        // Nonexistent element: times out after ~5s; error contains the selector and the troubleshooting hint
        let start = std::time::Instant::now();
        let err = client.click("#absent-forever").await.unwrap_err();
        let elapsed = start.elapsed();
        let msg = err.to_string();
        assert!(msg.contains("#absent-forever"), "selector missing: {}", msg);
        assert!(msg.contains("browser_snapshot"), "hint missing: {}", msg);
        assert!(
            elapsed >= std::time::Duration::from_millis(ACTIONABILITY_TIMEOUT_MS),
            "returned too early ({:?}) — wait loop not engaged",
            elapsed
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "blocked too long ({:?})",
            elapsed
        );

        client.close().await.expect("close");
        cleanup_profile("delayed");
    }

    /// helpers.js auto-wait: h.click/h.fill in batch_exec auto-wait for delayed elements;
    /// timeout items with a custom timeoutMs return success:false with the troubleshooting hint;
    /// h.wait/h.extract behavior is unchanged.
    #[tokio::test]
    #[ignore = "launches real Chrome; requires Chrome installed locally"]
    async fn batch_exec_helpers_auto_wait_real_chrome() {
        let mut client = isolated_client("batch");
        client.launch(true).await.expect("launch headless");
        client
            .navigate(&fixture_url("delayed", DELAYED_PAGE))
            .await
            .expect("navigate");

        // Page just loaded (elements appear only after 1.2s) — run batch operations immediately
        let results = client
            .batch_exec(
                "await h.click('#delayed-btn'); \
                 await h.fill('#delayed-input', 'batch-text'); \
                 await h.wait(50); \
                 const v = document.querySelector('#delayed-input').value; \
                 h._results.push({ op: 'assert', success: v === 'batch-text', detail: v }); \
                 h.extract('#delayed-btn');",
            )
            .await
            .expect("batch_exec");
        let steps: Vec<serde_json::Value> = serde_json::from_str(&results).expect("json results");
        let click = steps
            .iter()
            .find(|s| s["op"] == "click")
            .expect("click step");
        assert_eq!(click["success"], true, "click step failed: {}", results);
        let fill = steps.iter().find(|s| s["op"] == "fill").expect("fill step");
        assert_eq!(fill["success"], true, "fill step failed: {}", results);
        let assert_step = steps
            .iter()
            .find(|s| s["op"] == "assert")
            .expect("assert step");
        assert_eq!(
            assert_step["success"], true,
            "typed value mismatch: {}",
            results
        );
        // h.wait / h.extract behavior is unchanged
        let wait = steps.iter().find(|s| s["op"] == "wait").expect("wait step");
        assert_eq!(wait["success"], true);
        let extract = steps
            .iter()
            .find(|s| s["op"] == "extract")
            .expect("extract step");
        assert_eq!(extract["text"], "was-clicked", "extract step: {}", results);

        // Timeout path: custom 800ms wait for a nonexistent element → success:false with the troubleshooting hint
        let results = client
            .batch_exec("await h.click('#absent-forever', 800);")
            .await
            .expect("batch_exec timeout case");
        let steps: Vec<serde_json::Value> = serde_json::from_str(&results).expect("json results");
        assert_eq!(
            steps[0]["success"], false,
            "expected timeout failure: {}",
            results
        );
        let detail = steps[0]["detail"].as_str().unwrap_or("");
        assert!(detail.contains("800"), "timeout value missing: {}", detail);
        assert!(detail.contains("h.snapshot()"), "hint missing: {}", detail);

        client.close().await.expect("close");
        cleanup_profile("batch");
    }

    // ── Post-action effect verification (arm_change_detector / wait_for_change) ──

    /// A live DOM mutation within the wait window must be detected.
    #[tokio::test]
    #[ignore = "launches real Chrome; requires Chrome installed locally"]
    async fn change_detector_detects_mutation_real_chrome() {
        let mut client = isolated_client("changedet");
        client.launch(true).await.expect("launch headless");
        client
            .navigate(&fixture_url("state", STATE_PAGE))
            .await
            .expect("navigate");

        client.arm_change_detector().await.expect("arm");
        let changed = client.wait_for_change(4000).await.expect("wait");
        assert!(
            changed,
            "mutation at 1000/1500ms must be detected within 4s"
        );

        // STATE_PAGE has a second timer at 1500ms (display:block) — sleep past it so
        // the re-armed detector observes a fully settled DOM.
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        client.arm_change_detector().await.expect("re-arm");
        let unchanged = client.wait_for_change(1500).await.expect("wait 2");
        assert!(!unchanged, "no mutation after settle must report no change");

        client.close().await.expect("close");
        cleanup_profile("changedet");
    }

    /// No mutation, no URL change, no activeElement value change → false (negative path).
    #[tokio::test]
    #[ignore = "launches real Chrome; requires Chrome installed locally"]
    async fn change_detector_false_without_mutation_real_chrome() {
        let mut client = isolated_client("changeneg");
        client.launch(true).await.expect("launch headless");
        client
            .navigate(&fixture_url("static", STATIC_PAGE))
            .await
            .expect("navigate");

        client.arm_change_detector().await.expect("arm");
        let changed = client.wait_for_change(2000).await.expect("wait");
        assert!(!changed, "static page must not report a change");

        client.close().await.expect("close");
        cleanup_profile("changeneg");
    }

    /// The linkage that browser_click relies on: arm → click (side effect) → change detected.
    #[tokio::test]
    #[ignore = "launches real Chrome; requires Chrome installed locally"]
    async fn change_detector_links_to_click_side_effect_real_chrome() {
        let mut client = isolated_client("changeclick");
        client.launch(true).await.expect("launch headless");
        client
            .navigate(&fixture_url("delayed", DELAYED_PAGE))
            .await
            .expect("navigate");

        // Wait out the 1.2s delayed element injection so arming sees a settled DOM,
        // then the click is the ONLY mutation the detector can observe.
        tokio::time::sleep(std::time::Duration::from_millis(1400)).await;
        client.arm_change_detector().await.expect("arm");
        client
            .click("#delayed-btn")
            .await
            .expect("click (auto-waits for element)");
        let changed = client.wait_for_change(3000).await.expect("wait");
        assert!(changed, "click's textContent mutation must be detected");

        client.close().await.expect("close");
        cleanup_profile("changeclick");
    }

    // ── dev_tools_active_port_url fallback (issue#2 fix) ──

    #[tokio::test]
    async fn dev_tools_fallback_returns_url_for_live_port() {
        // Bind a real listener so the TCP liveness probe passes.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let dir = std::env::temp_dir().join("nuphus_devtools_fallback_live");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("DevToolsActivePort"),
            format!("{port}\n/devtools/browser/abc\n"),
        )
        .unwrap();

        let url = dev_tools_active_port_url(&dir).await;
        assert_eq!(
            url,
            Some(format!("ws://127.0.0.1:{port}/devtools/browser/abc"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dev_tools_fallback_rejects_dead_port() {
        // A port nobody listens on → file exists but stale → must be rejected.
        let dir = std::env::temp_dir().join("nuphus_devtools_fallback_dead");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("DevToolsActivePort"),
            "65530\n/devtools/browser/x\n",
        )
        .unwrap();

        assert_eq!(dev_tools_active_port_url(&dir).await, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dev_tools_fallback_rejects_garbage_and_missing() {
        let dir = std::env::temp_dir().join("nuphus_devtools_fallback_garbage");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("DevToolsActivePort"), "not-a-port\n").unwrap();
        assert_eq!(dev_tools_active_port_url(&dir).await, None);
        std::fs::write(dir.join("DevToolsActivePort"), "0\n/devtools/browser/x\n").unwrap();
        assert_eq!(dev_tools_active_port_url(&dir).await, None);
        let missing = std::env::temp_dir().join("nuphus_devtools_fallback_missing");
        let _ = std::fs::remove_dir_all(&missing);
        std::fs::create_dir_all(&missing).unwrap();
        assert_eq!(dev_tools_active_port_url(&missing).await, None);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&missing);
    }
}
