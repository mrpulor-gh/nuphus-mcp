//! Input control module - SendInput Unicode + mouse + keyboard

use crate::core::*;

pub mod keyboard;
pub mod mouse;
#[cfg(windows)]
pub mod sendinput;

pub use keyboard::*;
pub use mouse::*;
#[cfg(windows)]
pub use sendinput::*;

/// Input engine
pub struct InputEngine;

impl Default for InputEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl InputEngine {
    pub fn new() -> Self {
        Self
    }

    /// Send text - automatically picks the optimal strategy
    pub async fn send_text(&self, text: &str, target: &mut Target) -> Result<()> {
        // Ensure the target is active
        self.ensure_active(target).await?;

        // text is only used by the Windows SendInput path; keep the non-Windows
        // build warning-free without renaming the shared parameter.
        #[cfg(not(windows))]
        let _ = text;

        match target {
            #[cfg(windows)]
            Target::Window { .. } | Target::Tui { .. } => {
                // Windows: SendInput Unicode. Pass the hwnd through and enable the
                // session's own foreground verification (belt-and-suspenders after
                // ensure_active): if focus slipped away between activation and
                // injection, refuse to type into the wrong window.
                let hwnd = match target {
                    Target::Window { hwnd, .. } | Target::Tui { hwnd, .. } => *hwnd,
                    _ => unreachable!("match arm guarantees a window target"),
                };
                let session = sendinput::InputSession {
                    target_hwnd: Some(hwnd),
                    verify_foreground: true,
                    ..Default::default()
                };
                // SendInput + inter-character sleeps are blocking — keep them off
                // the async runtime's worker threads.
                let text = text.to_string();
                tokio::task::spawn_blocking(move || sendinput::nuphus_input(&text, &session))
                    .await
                    .map_err(|e| {
                        DesktopError::InputFailed(format!("input task join failed: {e}"))
                    })??;
            }
            #[cfg(not(windows))]
            Target::Tui { .. } => {
                // Non-Windows: no TUI text-input path yet (Tui is effectively a
                // Windows concept); keep the match exhaustive.
            }
            Target::Browser { .. } => {
                // Browser: Playwright input
                // TODO: call the browser module
            }
        }

        Ok(())
    }

    /// Click - auto-activate + click
    pub async fn click(&self, target: &mut Target, point: Point) -> Result<()> {
        self.ensure_active(target).await?;
        mouse::click(point.x, point.y).await
    }

    /// Activate the target window to the foreground (idempotent: skipped if already verified).
    ///
    /// Standalone window-activation entrypoint (originally an internal ensure_active capability), for
    /// consumers that need "activate only, no operation" (e.g. nuphus-mcp's desktop_window_activate).
    ///
    /// Activation is *verified*: an error is returned when the window is not in
    /// the foreground after both SetForegroundWindow and the AttachThreadInput
    /// fallback — callers must never type/click into the wrong window believing
    /// activation succeeded.
    pub async fn activate(&self, target: &mut Target) -> Result<()> {
        self.ensure_active(target).await
    }

    /// Drag
    pub async fn drag(&self, target: &mut Target, start: Point, end: Point) -> Result<()> {
        self.ensure_active(target).await?;
        mouse::drag(start, end).await
    }

    /// Key press
    pub async fn press(&self, target: &mut Target, key: &str) -> Result<()> {
        self.ensure_active(target).await?;
        keyboard::press(key).await
    }

    /// Key combination
    pub async fn hotkey(&self, target: &mut Target, keys: &[&str]) -> Result<()> {
        self.ensure_active(target).await?;
        keyboard::hotkey(keys).await
    }

    /// Ensure the target is active (built-in)
    async fn ensure_active(&self, target: &mut Target) -> Result<()> {
        if target.is_verified() {
            return Ok(());
        }

        #[cfg(windows)]
        {
            use ::windows::Win32::Foundation::HWND;
            use ::windows::Win32::UI::WindowsAndMessaging::{
                IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
            };
            use std::time::Duration;
            use tokio::time::sleep;

            let hwnd = match target {
                Target::Window { hwnd, .. } => *hwnd,
                Target::Tui { hwnd, .. } => *hwnd,
                _ => return Ok(()),
            };

            let handle = HWND(hwnd);
            unsafe {
                // 最小化窗口必须先还原再置前——SetForegroundWindow 对 iconic
                // 窗口不会还原它，后续 is_foreground 判定必然失败（对齐主项目
                // client.rs window_activate 的实现：还原 + 等动画 + 置前）
                if IsIconic(handle).as_bool() {
                    tracing::info!("[activate] hwnd {} is iconic -> SW_RESTORE", hwnd);
                    let _ = ShowWindow(handle, SW_RESTORE);
                    // 等待还原动画完成（Windows 通常需要 300-400ms）
                    sleep(Duration::from_millis(400)).await;
                    tracing::info!(
                        "[activate] after restore: IsIconic={}",
                        IsIconic(handle).as_bool()
                    );
                }
                let _ = SetForegroundWindow(handle);
            }
            sleep(Duration::from_millis(100)).await;

            // Verify whether it is in the foreground
            if !self.is_foreground(hwnd) {
                // Force foreground: AttachThreadInput
                self.force_foreground(hwnd).await?;
            }

            // Post-activation verification: Windows foreground stealing is
            // best-effort and can be refused. Never mark the target verified on
            // wishful thinking — a wrong-window click/type is far worse than an
            // honest error.
            if !self.is_foreground(hwnd) {
                return Err(DesktopError::InputFailed(format!(
                    "window activation failed: hwnd {hwnd} is not in the foreground \
                     after SetForegroundWindow + AttachThreadInput"
                )));
            }
        }

        target.verify();
        Ok(())
    }

    #[cfg(windows)]
    fn is_foreground(&self, hwnd: isize) -> bool {
        use ::windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        unsafe {
            let fg = GetForegroundWindow();
            fg.0 as isize == hwnd
        }
    }

    #[cfg(windows)]
    async fn force_foreground(&self, hwnd: isize) -> Result<()> {
        use ::windows::Win32::Foundation::HWND;
        use ::windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
        use ::windows::Win32::UI::WindowsAndMessaging::{
            GetWindowThreadProcessId, SetForegroundWindow, ShowWindow, SW_RESTORE,
        };
        use std::time::Duration;
        use tokio::time::sleep;

        let handle = HWND(hwnd);
        let target_tid = unsafe { GetWindowThreadProcessId(handle, None) };
        let current_tid = unsafe { GetCurrentThreadId() };

        unsafe {
            let _ = AttachThreadInput(current_tid, target_tid, true);
            let _ = ShowWindow(handle, SW_RESTORE);
            // 还原动画完成后再置前，否则 is_foreground 判定必然失败
            sleep(Duration::from_millis(400)).await;
            let _ = SetForegroundWindow(handle);
            let _ = AttachThreadInput(current_tid, target_tid, false);
        }

        sleep(Duration::from_millis(200)).await;
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// An invalid hwnd (0) can never become the foreground window — activation
    /// must fail honestly instead of marking the target verified and typing into
    /// whatever happens to have focus (P1-2a post-activation verification).
    #[tokio::test]
    async fn ensure_active_rejects_unactivatable_window() {
        // Headless session with no foreground window: is_foreground(0) would
        // spuriously pass — the failure path cannot be exercised there.
        if crate::platform::foreground_hwnd().is_none() {
            return;
        }
        let engine = InputEngine::new();
        let mut target = Target::Window {
            hwnd: 0,
            title: String::new(),
            verified: false,
            gfx_backend: crate::core::GfxBackend::Unknown,
        };
        let err = engine
            .activate(&mut target)
            .await
            .expect_err("hwnd=0 cannot be activated");
        assert!(
            err.to_string().contains("activation failed"),
            "error must name the activation failure: {err}"
        );
        assert!(
            !target.is_verified(),
            "failed activation must not mark the target verified"
        );
    }
}
