// desktop-api v2.0 - Nuphus core infrastructure
// Perceive · Locate · Operate · Communicate

#[cfg(feature = "http-server")]
pub mod api;
pub mod clipboard;
pub mod core;
pub mod hud;
pub mod input;
pub mod platform;
pub mod utils;
pub mod vision;

pub use core::*;
pub use input::*;
pub use platform::*;
pub use vision::*;

#[cfg(feature = "http-server")]
pub use http_server_entry::DesktopApi;

#[cfg(feature = "http-server")]
mod http_server_entry {
    use crate::core::{Result, SessionHandle, SessionManager, TargetSpec};
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use uuid::Uuid;

    /// Desktop API main entrypoint
    pub struct DesktopApi {
        sessions: Arc<RwLock<SessionManager>>,
    }

    impl DesktopApi {
        pub fn new() -> Self {
            Self {
                sessions: Arc::new(RwLock::new(SessionManager::new())),
            }
        }

        /// Create a session - auto-detects the target type
        pub async fn create_session(&self, target: TargetSpec) -> Result<SessionHandle> {
            let mut manager = self.sessions.write().await;
            manager.create(target).await
        }

        /// Get a session
        pub async fn get_session(&self, id: Uuid) -> Option<SessionHandle> {
            let manager = self.sessions.read().await;
            manager.get(id)
        }

        /// Close a session - automatically releases resources
        pub async fn close_session(&self, id: Uuid) -> Result<()> {
            let mut manager = self.sessions.write().await;
            manager.close(id).await
        }
    }

    impl Default for DesktopApi {
        fn default() -> Self {
            Self::new()
        }
    }
}
