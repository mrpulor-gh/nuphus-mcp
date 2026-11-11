//! Machine gate for cooperating MCP sessions (protocol v2).
//! Desktop/foreground calls are exclusive; background browser calls share it.
//! Stop all legacy v1 servers before upgrading: their JSON lease protocol does
//! not participate in OS locks. Never mix versions on the same desktop.

use nuphus_browser::coordination::{Coordination, ResourceGuard};
use serde_json::Value;

pub struct AutomationLock {
    coordination: Coordination,
}

impl Default for AutomationLock {
    fn default() -> Self {
        Self::new()
    }
}
/// Coordination root for this build.
///
/// Production sessions all share the user data dir. Unit tests run as parallel
/// threads of one process, and OS file locks are per open file, so a shared root
/// would make independent tests fail with `Automation is busy`. Each test-side
/// call therefore gets its own root: cross-process exclusion is covered by
/// `crates/nuphus-browser/tests/coordination.rs`, which uses real child processes.
pub(crate) fn coordination() -> Coordination {
    #[cfg(test)]
    {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        return Coordination::at(std::env::temp_dir().join(format!(
            "nuphus-machine-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )));
    }
    #[cfg(not(test))]
    Coordination::default()
}

impl AutomationLock {
    pub fn new() -> Self {
        Self {
            coordination: coordination(),
        }
    }
    pub fn acquire(&self, tool: &str, args: &Value) -> Result<ResourceGuard, String> {
        self.coordination
            .machine(needs_exclusive_machine(tool, args))
    }
}

fn needs_exclusive_machine(tool: &str, args: &Value) -> bool {
    !tool.starts_with("browser_") || args.get("activate").and_then(Value::as_bool) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn background_calls_share_but_desktop_and_activation_exclude() {
        let lock = AutomationLock {
            coordination: Coordination::at(
                std::env::temp_dir().join(format!("nuphus-gate-policy-{}", std::process::id())),
            ),
        };
        let a = lock.acquire("browser_wait_for", &json!({})).unwrap();
        let b = lock.acquire("browser_snapshot", &json!({})).unwrap();
        assert!(lock.acquire("desktop_mouse", &json!({})).is_err());
        assert!(lock
            .acquire("browser_switch_tab", &json!({"activate":true}))
            .is_err());
        drop((a, b));
        let desktop = lock.acquire("desktop_mouse", &json!({})).unwrap();
        assert!(lock.acquire("desktop_screenshot", &json!({})).is_err());
        assert!(lock.acquire("browser_snapshot", &json!({})).is_err());
        drop(desktop);
        assert!(lock.acquire("browser_snapshot", &json!({})).is_ok());
    }
}
