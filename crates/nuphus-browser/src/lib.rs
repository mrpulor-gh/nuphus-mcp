//! nuphus-browser — browser automation core (CDP, chromiumoxide).
//!
//! Extracted from the `src/browser` module of the Nuphus main crate into a standalone crate,
//! shared by nuphus-mcp (MCP Server) and the main crate (nuphus) as a single source of truth.
//!
//! Dependency note: browser operations must use [`shared::runtime`], the process-level resident
//! tokio runtime (temporary runtimes are forbidden — dropping one kills the CDP handler and leaves
//! a zombie browser connection). Synchronous contexts enter uniformly via `runtime().block_on(...)`.

mod chrome_finder;
mod client;
pub mod coordination;
mod shared;

pub mod cookie_source;

pub use chrome_finder::{find_chrome, ChromeError};
pub use client::{BrowserClient, BrowserError, ExternalIdentity};
pub use shared::{get_or_launch, get_or_launch_isolated, runtime, shared_client, shutdown_browser};

/// Round a byte index down to the nearest valid UTF-8 character boundary.
///
/// Migrated verbatim from the main crate's `crate::utils::floor_char_boundary` (used for snapshot title truncation).
#[inline]
pub fn floor_char_boundary(s: &str, mut index: usize) -> usize {
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}
