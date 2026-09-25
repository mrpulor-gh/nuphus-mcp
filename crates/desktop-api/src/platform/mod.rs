//! Platform abstraction layer - Windows implementation

use crate::core::*;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

/// The current foreground window handle (Windows), `None` on other platforms or
/// when no window holds the foreground. Used by callers that mean "type into
/// whatever currently has focus" instead of passing an invalid 0 handle.
pub fn foreground_hwnd() -> Option<isize> {
    #[cfg(windows)]
    {
        use ::windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.0 == 0 {
            None
        } else {
            Some(hwnd.0 as isize)
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Window manager
pub struct WindowManager {
    cache: lru::LruCache<String, isize>, // title -> hwnd
}

impl Default for WindowManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowManager {
    pub fn new() -> Self {
        Self {
            cache: lru::LruCache::new(std::num::NonZeroUsize::new(32).unwrap()),
        }
    }

    /// Find a window - cache first
    pub fn find(&mut self, title: &str) -> Result<Target> {
        #[cfg(windows)]
        {
            if let Some(&hwnd) = self.cache.get(title) {
                if self.is_valid(hwnd) {
                    let gfx = self.detect_gfx_backend(hwnd);
                    return Ok(Target::Window {
                        hwnd,
                        title: title.to_string(),
                        verified: false,
                        gfx_backend: gfx,
                    });
                }
            }

            // Re-search
            let hwnd = self.search(title)?;
            self.cache.put(title.to_string(), hwnd);
            let gfx = self.detect_gfx_backend(hwnd);

            Ok(Target::Window {
                hwnd,
                title: title.to_string(),
                verified: false,
                gfx_backend: gfx,
            })
        }
        #[cfg(not(windows))]
        {
            // Target::Window only exists on Windows; the window manager is a
            // Win32 concept. Non-Windows callers fall through to Browser/Tui.
            let _ = title;
            Err(DesktopError::PlatformNotSupported)
        }
    }

    /// List all windows
    pub fn list_all(&self) -> Result<Vec<WindowInfo>> {
        #[cfg(windows)]
        {
            use ::windows::Win32::Foundation::LPARAM;
            use ::windows::Win32::UI::WindowsAndMessaging::EnumWindows;

            let mut windows = Vec::new();
            let userdata = &mut windows as *mut Vec<WindowInfo>;

            unsafe {
                let _ = EnumWindows(Some(enum_callback), LPARAM(userdata as isize));
            }

            Ok(windows)
        }

        #[cfg(not(windows))]
        {
            Err(DesktopError::PlatformNotSupported)
        }
    }

    /// Verify whether the handle is valid
    fn is_valid(&self, hwnd: isize) -> bool {
        #[cfg(windows)]
        {
            use ::windows::Win32::Foundation::HWND;
            use ::windows::Win32::UI::WindowsAndMessaging::IsWindow;
            unsafe { IsWindow(HWND(hwnd)) }.as_bool()
        }
        #[cfg(not(windows))]
        {
            let _ = hwnd;
            false
        }
    }

    fn detect_gfx_backend(&self, hwnd: isize) -> GfxBackend {
        #[cfg(windows)]
        {
            windows::detect_gfx_backend(hwnd)
        }
        #[cfg(not(windows))]
        {
            let _ = hwnd;
            GfxBackend::Unknown
        }
    }

    /// Search for a window
    fn search(&self, title: &str) -> Result<isize> {
        #[cfg(windows)]
        {
            use ::windows::Win32::Foundation::LPARAM;
            use ::windows::Win32::UI::WindowsAndMessaging::EnumWindows;

            let mut ctx = SearchCtx {
                query: title.to_lowercase(),
                result: None,
            };

            let userdata = &mut ctx as *mut SearchCtx;

            unsafe {
                let _ = EnumWindows(Some(search_callback), LPARAM(userdata as isize));
            }

            ctx.result
                .ok_or_else(|| DesktopError::TargetNotFound(format!("window not found: {}", title)))
        }
        #[cfg(not(windows))]
        {
            let _ = title;
            Err(DesktopError::PlatformNotSupported)
        }
    }

    /// Move the window to the given coordinates (Windows: SetWindowPos).
    pub fn window_move(&self, hwnd: isize, x: i32, y: i32) -> Result<()> {
        #[cfg(windows)]
        {
            use ::windows::Win32::Foundation::HWND;
            use ::windows::Win32::UI::WindowsAndMessaging::{
                SetWindowPos, HWND_TOP, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
            };

            if !self.is_valid(hwnd) {
                return Err(DesktopError::TargetNotFound(format!(
                    "invalid window handle: {hwnd}"
                )));
            }
            let result = unsafe {
                SetWindowPos(
                    HWND(hwnd),
                    HWND_TOP,
                    x,
                    y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_SHOWWINDOW,
                )
            };
            if result.is_ok() {
                Ok(())
            } else {
                Err(DesktopError::ActivationFailed(format!(
                    "SetWindowPos move failed (hwnd={hwnd}): {result:?}"
                )))
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (hwnd, x, y);
            Err(DesktopError::PlatformNotSupported)
        }
    }

    /// Resize the window (Windows: SetWindowPos, keeping the current position and Z-order).
    pub fn window_resize(&self, hwnd: isize, width: i32, height: i32) -> Result<()> {
        #[cfg(windows)]
        {
            use ::windows::Win32::Foundation::HWND;
            use ::windows::Win32::UI::WindowsAndMessaging::{
                SetWindowPos, HWND_TOP, SWP_NOMOVE, SWP_NOZORDER, SWP_SHOWWINDOW,
            };

            if !self.is_valid(hwnd) {
                return Err(DesktopError::TargetNotFound(format!(
                    "invalid window handle: {hwnd}"
                )));
            }
            if width <= 0 || height <= 0 {
                return Err(DesktopError::ActivationFailed(format!(
                    "invalid window size: {width}x{height}"
                )));
            }
            let result = unsafe {
                SetWindowPos(
                    HWND(hwnd),
                    HWND_TOP,
                    0,
                    0,
                    width,
                    height,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_SHOWWINDOW,
                )
            };
            if result.is_ok() {
                Ok(())
            } else {
                Err(DesktopError::ActivationFailed(format!(
                    "SetWindowPos resize failed (hwnd={hwnd}): {result:?}"
                )))
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (hwnd, width, height);
            Err(DesktopError::PlatformNotSupported)
        }
    }

    /// Query detailed window information (title/visibility/minimized/maximized/window and client rect/process/class name).
    pub fn window_info(&self, hwnd: isize) -> Result<WindowDetail> {
        #[cfg(windows)]
        {
            use ::windows::Win32::Foundation::{HWND, POINT, RECT};
            use ::windows::Win32::Graphics::Gdi::ClientToScreen;
            use ::windows::Win32::UI::WindowsAndMessaging::{
                GetClassNameW, GetClientRect, GetWindowRect, GetWindowTextW,
                GetWindowThreadProcessId, IsIconic, IsWindowVisible, IsZoomed,
            };

            if !self.is_valid(hwnd) {
                return Err(DesktopError::TargetNotFound(format!(
                    "invalid window handle: {hwnd}"
                )));
            }
            let hwnd_ptr = HWND(hwnd);

            // Title
            let mut buf = [0u16; 512];
            let len = unsafe { GetWindowTextW(hwnd_ptr, &mut buf) };
            let title = String::from_utf16_lossy(&buf[..len as usize]);

            // Visibility & state
            let visible = unsafe { IsWindowVisible(hwnd_ptr).as_bool() };
            let minimized = unsafe { IsIconic(hwnd_ptr).as_bool() };
            let maximized = unsafe { IsZoomed(hwnd_ptr).as_bool() };

            // Window rect
            let mut rect = RECT::default();
            unsafe {
                _ = GetWindowRect(hwnd_ptr, &mut rect);
            }
            // Client rect + screen coordinate origin
            let mut client = RECT::default();
            unsafe {
                _ = GetClientRect(hwnd_ptr, &mut client);
            }
            let mut client_origin = POINT { x: 0, y: 0 };
            unsafe {
                _ = ClientToScreen(hwnd_ptr, &mut client_origin);
            }

            // Process ID + process name
            let mut pid: u32 = 0;
            unsafe {
                _ = GetWindowThreadProcessId(hwnd_ptr, Some(&mut pid));
            }
            let process_name = Self::process_name_from_pid(pid);

            // Class name
            let mut class_buf = [0u16; 256];
            let class_len = unsafe { GetClassNameW(hwnd_ptr, &mut class_buf) };
            let class_name = if class_len > 0 {
                String::from_utf16_lossy(&class_buf[..class_len as usize])
            } else {
                String::new()
            };

            Ok(WindowDetail {
                hwnd,
                title,
                visible,
                minimized,
                maximized,
                window: Rect {
                    x: rect.left,
                    y: rect.top,
                    w: (rect.right - rect.left).max(0) as u32,
                    h: (rect.bottom - rect.top).max(0) as u32,
                },
                client: Rect {
                    x: client_origin.x,
                    y: client_origin.y,
                    w: (client.right - client.left).max(0) as u32,
                    h: (client.bottom - client.top).max(0) as u32,
                },
                process_id: pid,
                process_name,
                class_name,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = hwnd;
            Err(DesktopError::PlatformNotSupported)
        }
    }

    /// Get the process executable file name from a PID (cross-platform fallback implementation).
    fn process_name_from_pid(pid: u32) -> String {
        #[cfg(windows)]
        {
            use ::windows::Win32::Foundation::CloseHandle;
            use ::windows::Win32::System::ProcessStatus::GetModuleBaseNameW;
            use ::windows::Win32::System::Threading::{
                OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
            };

            unsafe {
                match OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) {
                    Ok(handle) => {
                        let mut buf = [0u16; 260];
                        let result = GetModuleBaseNameW(handle, None, &mut buf);
                        _ = CloseHandle(handle);
                        if result > 0 {
                            String::from_utf16_lossy(&buf[..result as usize])
                        } else {
                            String::new()
                        }
                    }
                    Err(_) => String::new(),
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            if let Ok(output) = std::process::Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "comm="])
                .output()
            {
                if output.status.success() {
                    return String::from_utf8_lossy(&output.stdout).trim().to_string();
                }
            }
            String::new()
        }
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        }
        #[cfg(all(not(windows), not(target_os = "macos"), not(target_os = "linux")))]
        {
            String::new()
        }
    }
}

#[cfg(windows)]
use ::windows::Win32::Foundation::{BOOL, HWND, LPARAM};

#[cfg(windows)]
struct SearchCtx {
    query: String,
    result: Option<isize>,
}

#[cfg(windows)]
unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    use ::windows::Win32::Foundation::BOOL;
    use ::windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    };

    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }

    let mut buf = [0u16; 512];
    let len = GetWindowTextW(hwnd, &mut buf);
    if len == 0 {
        return BOOL(1);
    }

    let title = String::from_utf16_lossy(&buf[..len as usize]);
    let windows = &mut *(lparam.0 as *mut Vec<WindowInfo>);

    let mut rect = Default::default();
    let _ = GetWindowRect(hwnd, &mut rect);

    // Get the process ID
    let mut pid: u32 = 0;
    let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));

    windows.push(WindowInfo {
        hwnd: hwnd.0,
        title,
        x: rect.left,
        y: rect.top,
        width: (rect.right - rect.left) as u32,
        height: (rect.bottom - rect.top) as u32,
        visible: true,
        process_id: pid,
    });

    BOOL(1)
}

#[cfg(windows)]
unsafe extern "system" fn search_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    use ::windows::Win32::Foundation::BOOL;
    use ::windows::Win32::UI::WindowsAndMessaging::{GetWindowTextW, IsWindowVisible};

    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }

    let mut buf = [0u16; 512];
    let len = GetWindowTextW(hwnd, &mut buf);
    if len == 0 {
        return BOOL(1);
    }

    let title = String::from_utf16_lossy(&buf[..len as usize]).to_lowercase();
    let ctx = &mut *(lparam.0 as *mut SearchCtx);

    if title.contains(&ctx.query) {
        ctx.result = Some(hwnd.0);
        return BOOL(0);
    }

    BOOL(1)
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub visible: bool,
    /// Process ID (via GetWindowThreadProcessId)
    pub process_id: u32,
}

/// Detailed window information (returned by `WindowManager::window_info`).
#[derive(Debug, Clone)]
pub struct WindowDetail {
    pub hwnd: isize,
    pub title: String,
    pub visible: bool,
    pub minimized: bool,
    pub maximized: bool,
    /// Window rect (screen coordinates)
    pub window: Rect,
    /// Client rect (screen coordinate origin + width/height)
    pub client: Rect,
    pub process_id: u32,
    pub process_name: String,
    pub class_name: String,
}
