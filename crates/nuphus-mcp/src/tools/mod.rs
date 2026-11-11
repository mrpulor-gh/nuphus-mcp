//! MCP tool registry: schema exposure + name dispatch execution.

pub mod browser;
mod browser_sequence;
pub mod desktop;
mod schemas;
pub mod semantic;

pub use schemas::{all_tools, ToolDef};

/// How long a tool waits for the process-level automation lock (the one that
/// serializes automation inside this process) before failing with "busy".
/// It is acquired *outside* every per-tool budget, so it needs its own bound.
const PROCESS_LOCK_TIMEOUT_SECS: u64 = 30;

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
/// Different MCP processes may operate separate task tabs concurrently. Desktop
/// and explicit foreground operations acquire the machine gate exclusively.
/// Within one session, preserve sequential command/ref/target state.
pub async fn execute(name: &str, args: &serde_json::Value) -> Result<ToolOutput, String> {
    let known = has_tool(name);
    #[cfg(test)]
    let known = known || name == "test_panic_tool";
    if !known {
        return Err(format!("Unknown tool: {name}"));
    }
    // Process-level mutual exclusion: serialize automation operations within this
    // process. Held across the await points of the actual tool call.
    // Bounded: this lock sits *outside* every per-tool budget, so an unbounded
    // wait here would wedge the whole server (one stuck tool = every later call
    // unanswered). Give up with a readable busy error instead.
    static PROCESS_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let process_guard = match tokio::time::timeout(
        std::time::Duration::from_secs(PROCESS_LOCK_TIMEOUT_SECS),
        PROCESS_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock(),
    )
    .await
    {
        Ok(guard) => guard,
        Err(_) => {
            return Ok(ToolOutput::failure(format!(
                "Automation is busy: another operation in this process has held the automation \
                 lock for over {PROCESS_LOCK_TIMEOUT_SECS}s; retry later."
            )));
        }
    };

    // Shared machine access for background browser operations, exclusive access
    // for desktop / foreground operations. Tab ownership lives in BrowserClient.
    let lock = crate::automation_lock::AutomationLock::new();
    let file_guard = match lock.acquire(name, args) {
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
        Ok(text) => Ok(output_from_text(name, text)),
        Err(e) => Ok(ToolOutput::failure(e)),
    }
}

fn output_from_text(name: &str, text: String) -> ToolOutput {
    // Only the batch executor owns this envelope. A user's evaluate result may
    // legitimately contain success:false and is not a tool-level error.
    let batch_failed = name == "browser_exec"
        && serde_json::from_str::<serde_json::Value>(&text).is_ok_and(|value| {
            let failed = |row: &serde_json::Value| {
                row.get("success").and_then(serde_json::Value::as_bool) == Some(false)
                    || row.get("success").and_then(serde_json::Value::as_str) == Some("unknown")
            };
            value
                .as_array()
                .map_or_else(|| failed(&value), |rows| rows.iter().any(failed))
        });
    if batch_failed {
        ToolOutput::failure(text)
    } else {
        ToolOutput::success(text)
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

    #[test]
    fn batch_failures_propagate_to_mcp_without_reinterpreting_page_data() {
        assert!(
            output_from_text(
                "browser_exec",
                r#"{"success":false,"status":"unknown","steps":[]}"#.into()
            )
            .is_error
        );
        assert!(!output_from_text("browser_exec", r#"{"success":true}"#.into()).is_error);
        assert!(!output_from_text("browser_evaluate", r#"{"success":false}"#.into()).is_error);
        assert!(!output_from_text("browser_exec", "legacy text".into()).is_error);
        for text in [
            r#"[{"op":"batch_error","success":false}]"#,
            r#"[{"op":"batch_truncated","success":"unknown"}]"#,
            r#"[{"op":"click","success":true},{"op":"fill","success":false}]"#,
        ] {
            assert!(output_from_text("browser_exec", text.into()).is_error);
        }
        assert!(
            !output_from_text("browser_exec", r#"[{"op":"click","success":true}]"#.into()).is_error
        );
    }

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
