//! Security boundary: dangerous operation annotations + strict confirmation mode + path validation.
//!
//! nuphus-mcp exposes high-risk desktop/browser control capabilities. Security model:
//! 1. **annotations**: tools/list marks write tools with `destructiveHint` (MCP spec),
//!    so clients can show danger prompts/confirmation UI accordingly;
//! 2. **strict confirmation mode** (`--confirm-write` / env var `NUPHUS_MCP_CONFIRM_WRITE=1`):
//!    write tools require an explicit `"confirm": true` argument, otherwise they are rejected via isError —
//!    preventing a runaway Agent from accidentally triggering side-effect operations like mouse clicks/input;
//! 3. **path validation**: screenshot save paths must not allow path traversal or system-protected directories; upload files must really exist.

use serde_json::Value;

/// Write-classification for tools whose write-ness does NOT depend on arguments.
///
/// `desktop_mouse` is the sole exception (its `action` decides read vs write) and is
/// handled explicitly in `is_write_tool` / `is_write_tool_schema`. This is the single
/// source of truth shared by the runtime check and the schema declaration.
fn is_write_tool_name_only(name: &str) -> bool {
    matches!(
        name,
        // desktop writes
        "desktop_mouse_drag" | "desktop_input" | "desktop_window_activate"
            | "desktop_window_screenshot" | "desktop_window_move" | "desktop_window_resize"
            | "desktop_screenshot" | "desktop_clipboard_write"
            | "desktop_clipboard_clean"
            // semantic desktop writes: binding may launch/activate an application, and
            // both execution tools dispatch a native UIA action
            | "desktop_target_bind" | "desktop_semantic_execute" | "desktop_semantic_action"
            // browser writes
            | "browser_navigate" | "browser_click" | "browser_type" | "browser_press" | "browser_exec"
            | "browser_scroll" | "browser_screenshot" | "browser_close" | "browser_evaluate"
            | "browser_back" | "browser_forward" | "browser_cookies_set"
            | "browser_import_cookies" | "browser_upload" | "browser_drag_files" | "browser_new_tab"
            | "browser_switch_tab"
    )
}

/// Whether the tool is a "write operation" (changes system/page state) for a GIVEN
/// argument set. `desktop_mouse` is differentiated by action: position is read,
/// everything else is write.
pub fn is_write_tool(name: &str, args: &Value) -> bool {
    if name == "desktop_mouse" {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("position");
        return action != "position";
    }
    is_write_tool_name_only(name)
}

/// Schema-side counterpart of `is_write_tool`: whether a tool's `inputSchema` must
/// declare the `confirm` property.
///
/// Strict-confirm mode requires write tools to carry `"confirm": true` at runtime, and
/// spec-compliant MCP clients strip arguments that are not declared in `inputSchema`.
/// A tool needs `confirm` declared iff the runtime *may* require confirmation for some
/// argument set. For `desktop_mouse` that means always (action can be click/scroll/…),
/// mirroring the conservative treatment in `annotations_for`.
pub fn is_write_tool_schema(name: &str) -> bool {
    if name == "desktop_mouse" {
        return true;
    }
    is_write_tool_name_only(name)
}

/// MCP Tool `annotations` (spec field, output of tools/list).
///
/// Note: `desktop_mouse` is marked destructive at the schema level (action can be
/// click/scroll and other write operations); the runtime confirmation check (`check_write_confirmation`)
/// distinguishes by actual action (position is read).
pub fn annotations_for(name: &str, args: &Value) -> Option<Value> {
    if name == "desktop_mouse" {
        return Some(serde_json::json!({ "destructiveHint": true }));
    }
    if is_write_tool(name, args) {
        return Some(serde_json::json!({ "destructiveHint": true }));
    }
    if is_read_only_tool(name, args) {
        return Some(serde_json::json!({ "readOnlyHint": true }));
    }
    None
}

fn is_read_only_tool(name: &str, args: &Value) -> bool {
    match name {
        "desktop_mouse" => {
            args.get("action")
                .and_then(Value::as_str)
                .unwrap_or("position")
                == "position"
        }
        "desktop_screen_size"
        | "desktop_windows_list"
        | "desktop_window_info"
        | "desktop_vision"
        | "desktop_perceive"
        // semantic desktop reads: observation, candidate lookup, target listing and
        // postcondition verification never change system state
        | "desktop_targets_list"
        | "desktop_semantic_observe"
        | "desktop_semantic_candidate"
        | "desktop_verify_state" => true,
        "browser_snapshot"
        | "browser_extract"
        | "browser_cookies_get"
        | "browser_list_tabs"
        | "browser_list_downloads"
        | "browser_wait_for" => true,
        _ => false,
    }
}

/// Security policy (determined at server startup, injectable for tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SecurityPolicy {
    /// Strict confirmation mode: write tools require explicit `"confirm": true`
    pub strict_confirm: bool,
}

impl SecurityPolicy {
    /// Read from environment variable (`NUPHUS_MCP_CONFIRM_WRITE=1`).
    pub fn from_env() -> Self {
        Self {
            strict_confirm: std::env::var("NUPHUS_MCP_CONFIRM_WRITE")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        }
    }

    /// Write confirmation check: in strict mode write tools must carry `"confirm": true`.
    /// Returns Err(msg) when execution should be rejected.
    pub fn check_write_confirmation(&self, name: &str, args: &Value) -> Result<(), String> {
        if !self.strict_confirm {
            return Ok(());
        }
        if !is_write_tool(name, args) {
            return Ok(());
        }
        let confirmed = args
            .get("confirm")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if confirmed {
            return Ok(());
        }
        Err(format!(
            "write operation '{}' requires explicit confirmation: set \"confirm\": true in arguments (strict confirm mode)",
            name
        ))
    }
}

/// Screenshot save path validation: forbid path traversal, Windows device paths, and system-protected directories.
///
/// Relative paths are allowed and resolved against the server working directory.
/// Returns `Ok(normalized absolute path)` ready for writing.
pub fn validate_screenshot_path(path: &str) -> Result<String, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("screenshot path must not be empty".to_string());
    }
    // Windows device paths / UNC device paths
    if trimmed.starts_with("\\\\?\\") || trimmed.starts_with("\\\\.\\") {
        return Err("screenshot path must not use device path prefix".to_string());
    }
    let p = std::path::Path::new(trimmed);
    // Path traversal
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("screenshot path must not contain '..'".to_string());
    }
    // Resolve to an absolute path (relative → against the server working directory),
    // so the protected-dir and parent-exists checks below evaluate the real location
    // and the returned value is directly usable for writing.
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("cannot resolve current directory: {e}"))?
            .join(p)
    };
    // System-protected directories
    if in_system_protected_dir(&abs) {
        return Err("screenshot path must not point into a system-protected directory".to_string());
    }
    // Parent directory must exist (avoid writing into an arbitrary missing directory and failing later)
    if let Some(parent) = abs.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            return Err(format!(
                "screenshot parent directory does not exist: {}",
                parent.display()
            ));
        }
    }
    Ok(abs.to_string_lossy().to_string())
}

/// Upload file validation: must be an existing regular file (readable).
pub fn validate_upload_file(path: &str) -> Result<(), String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("upload file path must not be empty".to_string());
    }
    let p = std::path::Path::new(trimmed);
    if !p.is_file() {
        return Err(format!(
            "upload file does not exist or is not a regular file: {}",
            p.display()
        ));
    }
    Ok(())
}

/// Native drag path validation: each entry must be an existing file or directory.
/// Returns a canonical absolute path for Chrome's `Input.dispatchDragEvent`.
pub fn validate_drag_path(path: &str) -> Result<String, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("drag path must not be empty".to_string());
    }
    let p = std::path::Path::new(trimmed);
    if !p.is_absolute() {
        return Err(format!("drag path must be absolute: {}", p.display()));
    }
    if !p.exists() {
        return Err(format!("drag path does not exist: {}", p.display()));
    }
    p.canonicalize()
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|e| format!("cannot resolve drag path '{}': {e}", p.display()))
}

fn in_system_protected_dir(p: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        // Windows: system drive root + Program Files + Windows directories
        let normalized = p.to_string_lossy().to_lowercase();
        const PROTECTED: &[&str] = &[
            "\\windows\\",
            "\\program files\\",
            "\\program files (x86)\\",
            "\\windows.old\\",
        ];
        if PROTECTED.iter().any(|frag| normalized.contains(frag)) {
            return true;
        }
        // System drive root (e.g. C:\foo.bmp)
        if let Some(root) = p.components().next() {
            if let std::path::Component::Prefix(prefix) = root {
                if let std::path::Prefix::Disk(drive) = prefix.kind() {
                    let drive_root = format!("{}:\\", ({ drive } as char).to_ascii_uppercase());
                    if p.starts_with(&drive_root) && p.parent().is_none() {
                        return true;
                    }
                }
            }
        }
        false
    }
    #[cfg(not(windows))]
    {
        let normalized = p.to_string_lossy().to_lowercase();
        const PROTECTED: &[&str] = &[
            "/etc/", "/usr/", "/bin/", "/sbin/", "/boot/", "/proc/", "/sys/", "/dev/",
        ];
        PROTECTED.iter().any(|frag| normalized.starts_with(frag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn write_classifier() {
        assert!(is_write_tool("desktop_input", &json!({"mode":"type"})));
        assert!(is_write_tool("desktop_mouse", &json!({"action":"click"})));
        assert!(!is_write_tool(
            "desktop_mouse",
            &json!({"action":"position"})
        ));
        assert!(is_write_tool("browser_click", &json!({"selector":"#a"})));
        assert!(is_write_tool("browser_press", &json!({"key":"Enter"})));
        assert!(is_write_tool(
            "browser_drag_files",
            &json!({"selector":"#drop","file_paths":["/tmp/a"]})
        ));
        assert!(!is_write_tool("browser_snapshot", &json!({})));
        assert!(!is_write_tool("desktop_screen_size", &json!({})));
    }

    #[test]
    fn annotations_mark_destructive_and_readonly() {
        let d = annotations_for("desktop_input", &json!({"mode":"type"}))
            .expect("write tool has annotations");
        assert_eq!(d["destructiveHint"], true);

        let r =
            annotations_for("desktop_screen_size", &json!({})).expect("read tool has annotations");
        assert_eq!(r["readOnlyHint"], true);

        // desktop_mouse conservatively marked destructive at the schema level (it can click)
        let m = annotations_for("desktop_mouse", &json!({"action":"position"}))
            .expect("mouse has annotations");
        assert_eq!(m["destructiveHint"], true);
        assert!(m.get("readOnlyHint").is_none());
    }

    #[test]
    fn strict_confirm_rejects_write_without_confirm() {
        let policy = SecurityPolicy {
            strict_confirm: true,
        };
        assert!(policy
            .check_write_confirmation("desktop_input", &json!({"mode":"type"}))
            .is_err());
        assert!(policy
            .check_write_confirmation("desktop_input", &json!({"mode":"type","confirm":true}))
            .is_ok());
        // Read tools are unaffected
        assert!(policy
            .check_write_confirmation("desktop_screen_size", &json!({}))
            .is_ok());
        // Non-strict mode allows writes
        let lenient = SecurityPolicy {
            strict_confirm: false,
        };
        assert!(lenient
            .check_write_confirmation("desktop_input", &json!({"mode":"type"}))
            .is_ok());
    }

    #[test]
    fn screenshot_path_validation() {
        assert!(validate_screenshot_path("").is_err());
        // Device-path prefix check is string-based, platform-independent.
        assert!(validate_screenshot_path("\\\\?\\C:\\evil.bmp").is_err());
        #[cfg(windows)]
        {
            // Windows: backslash is the path separator.
            assert!(validate_screenshot_path("C:\\x\\..\\evil.bmp").is_err());
            assert!(validate_screenshot_path("C:\\Windows\\evil.bmp").is_err());
            assert!(validate_screenshot_path("C:\\Program Files\\evil.bmp").is_err());
        }
        #[cfg(not(windows))]
        {
            // POSIX: forward slash is the path separator.
            assert!(validate_screenshot_path("/x/../evil.bmp").is_err());
            assert!(validate_screenshot_path("/etc/evil.bmp").is_err());
            assert!(validate_screenshot_path("/usr/bin/evil.bmp").is_err());
        }
        // Existing user-writable directory passes (temp directory really exists)
        let tmp = std::env::temp_dir().join("nuphus_mcp_shot_test.bmp");
        let ok = validate_screenshot_path(&tmp.to_string_lossy());
        assert!(ok.is_ok(), "temp dir path should pass: {:?}", ok.err());
    }

    #[test]
    fn upload_file_validation_requires_existing_file() {
        assert!(validate_upload_file("").is_err());
        assert!(validate_upload_file("Z:\\definitely\\missing\\file.bin").is_err());
    }

    #[test]
    fn drag_path_validation_accepts_files_and_directories() {
        let dir = std::env::temp_dir();
        assert!(validate_drag_path(&dir.to_string_lossy()).is_ok());
        let executable = std::env::current_exe().expect("test executable path");
        assert!(validate_drag_path(&executable.to_string_lossy()).is_ok());
        assert!(validate_drag_path("").is_err());
        assert!(validate_drag_path(".").is_err());
        assert!(validate_drag_path("Z:\\definitely\\missing\\path").is_err());
    }

    /// Anti-drift guard: every tool declared in tools/list must be classified as either
    /// write or read, otherwise strict-confirm mode (which only gates write tools) and
    /// annotations silently misbehave for new tools added later.
    #[test]
    fn every_tool_is_classified_write_or_read() {
        use crate::tools::all_tools;
        for tool in all_tools() {
            let args = json!({});
            let is_w = is_write_tool(tool.name, &args);
            let is_r = is_read_only_tool(tool.name, &args);
            assert!(
                is_w || is_r,
                "tool '{}' is neither write nor read — add it to is_write_tool/is_read_only_tool",
                tool.name
            );
        }
    }

    #[test]
    fn relative_screenshot_path_resolves_to_absolute() {
        let p = validate_screenshot_path("nuphus_shot.bmp")
            .expect("cwd-relative path passes when cwd exists");
        assert!(
            std::path::Path::new(&p).is_absolute(),
            "must be absolute: {p}"
        );
    }
}
