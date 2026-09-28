//! Process-level shared BrowserClient (singleton).
//!
//! All browser consumers (browser_tools / web CDP rendering / cookies CDP data source)
//! reuse the same instance: the embedded browser uses a fixed profile_dir, and multiple
//! instances concurrently launching the same profile would conflict with each other;
//! a singleton + mutex eliminates the problem at the root.
//!
//! Runtime note: chromiumoxide's CDP handler is `tokio::spawn`ed onto the ambient runtime at
//! launch, and its lifetime must match BrowserClient. Any browser operation initiated from a
//! synchronous context runs on [`runtime()`], the process-level resident runtime; temporary
//! runtimes are forbidden (dropping one kills the handler, leaving a zombie browser connection).

use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::client::BrowserClient;

/// Browser-specific process-level resident tokio runtime (see module docs).
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("failed to create browser runtime"))
}

/// Process-level shared BrowserClient handle (lazily initialized; not launched initially).
pub fn shared_client() -> Arc<Mutex<Option<BrowserClient>>> {
    static INSTANCE: OnceLock<Arc<Mutex<Option<BrowserClient>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Arc::new(Mutex::new(None))).clone()
}

/// How long a browser tool waits for the process-wide client lock before giving
/// up with a readable "busy" error. Every browser tool takes this lock, so an
/// unbounded wait here is indistinguishable from the whole tool family hanging.
const SHARED_CLIENT_LOCK_TIMEOUT_SECS: u64 = 30;

/// get-or-launch: hold the lock to ensure a client exists and launch it per the requested mode, returning an owned guard.
///
/// While the caller holds the guard, the browser is exclusively owned, guaranteeing that
/// navigate/evaluate/cookies etc. CDP operation sequences do not interleave with other consumers;
/// dropping the guard releases it.
pub async fn get_or_launch(
    headless: bool,
) -> Result<OwnedMutexGuard<Option<BrowserClient>>, String> {
    // Bounded acquisition: this mutex is process-wide and every browser tool
    // takes it, so a wedged previous operation must not turn every later call
    // into an unbounded wait (that failure mode reads as "the whole tool family
    // hangs, only killing the process helps", with no error anywhere).
    let mut guard = match tokio::time::timeout(
        std::time::Duration::from_secs(SHARED_CLIENT_LOCK_TIMEOUT_SECS),
        shared_client().lock_owned(),
    )
    .await
    {
        Ok(guard) => guard,
        Err(_) => {
            return Err(format!(
                "Browser automation is busy: another operation in this process has held the \
                 shared browser client for over {SHARED_CLIENT_LOCK_TIMEOUT_SECS}s; retry later."
            ));
        }
    };
    if guard.is_none() {
        let client =
            BrowserClient::new().map_err(|e| format!("Browser automation unavailable: {}", e))?;
        *guard = Some(client);
    }
    let client = guard.as_mut().expect("client just initialized");
    client
        .launch(headless)
        .await
        .map_err(|e| format!("Failed to launch browser: {}", e))?;
    Ok(guard)
}

/// Explicitly shut down the shared browser. Call before process exit.
///
/// The shared BrowserClient lives in a process-level `static` (`OnceLock`), and Rust statics are
/// never dropped on process exit — so `kill_on_drop` on the Chrome child can never fire on its own,
/// and a gracefully-exiting server would otherwise leave an orphaned Chrome holding the profile lock.
/// This is the reliable cleanup path: close the CDP connection and kill a Chrome instance this
/// process launched. An attached external/user browser is only disconnected, never killed.
/// Bounded to 5s so a wedged CDP connection cannot block process shutdown.
pub async fn shutdown_browser() {
    let shared = shared_client();
    let mut guard = shared.lock().await;
    if let Some(client) = guard.as_mut() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), client.close()).await;
    }
    *guard = None;
}
