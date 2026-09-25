//! MCP tool registry: schema exposure + name dispatch execution.

pub mod browser;
pub mod desktop;
mod schemas;
pub mod semantic;

pub use schemas::{all_tools, ToolDef};

/// Tool execution result
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Text content (tools/call content[].text)
    pub text: String,
    /// Whether it is a semantic error (true → tools/call returns isError: true)
    pub is_error: bool,
}

impl ToolOutput {
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    pub fn failure(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// Execute a tool by name. Unknown tool names return Err (→ JSON-RPC -32602).
///
/// Automation safety: desktop and browser automation operate on exclusive machine
/// resources (mouse/keyboard/screen/browser). Only one Agent may run an automation
/// operation at any moment:
/// - **Process-level**: a process-wide tokio Mutex serializes all automation calls
///   within this server instance (a stdio loop is sequential anyway; this also makes
///   parallel test execution deterministic).
/// - **Cross-process**: a file lock coordinates multiple MCP server instances (one
///   per Agent via stdio) through `{data_dir}/Nuphus/nuphus-mcp/automation.lock`.
/// The lock is held only for the duration of this call (short hold), released on return.
pub async fn execute(name: &str, args: &serde_json::Value) -> Result<ToolOutput, String> {
    // Process-level mutual exclusion: serialize automation operations within this
    // process. Held across the await points of the actual tool call.
    static PROCESS_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let process_guard = PROCESS_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;

    // Cross-process lock: full coverage for every desktop_*/browser_* tool.
    // Read-only operations (e.g. desktop_screen_size) also acquire it — they still
    // touch the shared machine state and must not interleave with a writer's sequence.
    let lock = crate::automation_lock::AutomationLock::new();
    let file_guard = match lock.acquire(name) {
        Ok(guard) => guard,
        Err(e) => return Ok(ToolOutput::failure(e)),
    };

    // Test-only panic hook: exercised by the server panic-guard test to prove a
    // panicking tool cannot kill the process and that both locks above are
    // released by RAII while the panic unwinds. Compiled out in production.
    #[cfg(test)]
    if name == "test_panic_tool" {
        panic!("intentional test panic (automation lock guard must unwind safely)");
    }

    let result = if name.starts_with("desktop_") {
        desktop::execute(name, args).await
    } else if name.starts_with("browser_") {
        browser::execute(name, args).await
    } else {
        return Err(format!("Unknown tool: {}", name));
    };

    drop(file_guard);
    drop(process_guard);
    match result {
        Ok(text) => Ok(ToolOutput::success(text)),
        Err(e) => Ok(ToolOutput::failure(e)),
    }
}

/// Whether the tool name exists (the set exposed by tools/list).
pub fn has_tool(name: &str) -> bool {
    all_tools().iter().any(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Every desktop_* tool registered in schemas must have a dispatch branch in
    /// `tools/desktop.rs` — the desktop counterpart of browser.rs's
    /// `dispatch_matches_schema` guard (which uses EXECUTABLE_BROWSER_TOOLS).
    ///
    /// desktop.rs is owned by another workstream and exposes no executable-tool
    /// constant, so this guard statically verifies that every schema name appears
    /// as a quoted match arm in the dispatch source. A schema/dispatch drift
    /// otherwise only surfaces at tools/call time as "Unknown desktop tool".
    #[test]
    fn desktop_dispatch_matches_schema() {
        let dispatch_src = include_str!("desktop.rs");
        let registered: HashSet<&str> = all_tools()
            .iter()
            .map(|t| t.name)
            .filter(|n| n.starts_with("desktop_"))
            .collect();
        assert!(
            !registered.is_empty(),
            "schemas must register at least one desktop_* tool"
        );
        for name in &registered {
            let match_arm = format!("\"{name}\" =>");
            assert!(
                dispatch_src.contains(&match_arm),
                "schemas::all_tools registers '{name}' but desktop.rs has no dispatch arm for it"
            );
        }
    }
}
