//! Native AX window service — the macOS counterpart of the Win32 window layer.
//! Numeric handles are process-local aliases to retained AX windows,
//! never AppleScript window indexes. All AX references live on one worker thread.
//!
//! Errors are plain messages (`Result<_, String>`), matching this crate's other
//! native services that the semantic layer consumes, so callers map them into
//! their own `AutomationError`/catalog errors without an extra error type.

use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashMap},
    ffi::{c_char, c_void},
    sync::{mpsc, OnceLock},
    time::{Duration, Instant},
};

type Ref = *const c_void;
type NativeResult<T> = std::result::Result<T, String>;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Pair {
    x: f64,
    y: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementCreateApplication(pid: i32) -> Ref;
    fn AXUIElementCreateSystemWide() -> Ref;
    fn AXUIElementCopyAttributeValue(element: Ref, name: Ref, value: *mut Ref) -> i32;
    fn AXUIElementSetAttributeValue(element: Ref, name: Ref, value: Ref) -> i32;
    fn AXUIElementPerformAction(element: Ref, action: Ref) -> i32;
    fn AXUIElementSetMessagingTimeout(element: Ref, timeout: f32) -> i32;
    fn AXUIElementGetPid(element: Ref, pid: *mut i32) -> i32;
    fn AXValueGetValue(value: Ref, kind: u32, result: *mut c_void) -> u8;
    fn AXValueGetTypeID() -> usize;
    fn AXValueCreate(kind: u32, value: *const c_void) -> Ref;
    fn CGWindowListCopyWindowInfo(options: u32, relative: u32) -> Ref;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: Ref);
    fn CFRetain(value: Ref) -> Ref;
    fn CFEqual(left: Ref, right: Ref) -> u8;
    fn CFGetTypeID(value: Ref) -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFArrayGetCount(value: Ref) -> isize;
    fn CFArrayGetValueAtIndex(value: Ref, index: isize) -> Ref;
    fn CFDictionaryGetValue(value: Ref, key: Ref) -> Ref;
    fn CFNumberGetValue(value: Ref, kind: isize, result: *mut c_void) -> u8;
    fn CFStringCreateWithBytes(
        allocator: Ref,
        bytes: *const u8,
        count: isize,
        encoding: u32,
        external: u8,
    ) -> Ref;
    fn CFStringGetLength(value: Ref) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(value: Ref, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    static kCFBooleanTrue: Ref;
    static kCFBooleanFalse: Ref;
}

struct Owned(Ref);
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}
impl Owned {
    fn new(raw: Ref) -> NativeResult<Self> {
        if raw.is_null() {
            Err("macOS native object unavailable".into())
        } else {
            Ok(Self(raw))
        }
    }
    fn string(text: &str) -> NativeResult<Self> {
        Self::new(unsafe {
            CFStringCreateWithBytes(
                std::ptr::null(),
                text.as_ptr(),
                text.len() as isize,
                0x08000100,
                0,
            )
        })
    }
    fn attr(&self, name: &str) -> NativeResult<Self> {
        let name_ref = Self::string(name)?;
        let mut value = std::ptr::null();
        check(
            unsafe { AXUIElementCopyAttributeValue(self.0, name_ref.0, &mut value) },
            name,
        )?;
        Self::new(value)
    }
    fn text(&self) -> String {
        unsafe {
            if CFGetTypeID(self.0) != CFStringGetTypeID() {
                return String::new();
            }
            let length =
                CFStringGetMaximumSizeForEncoding(CFStringGetLength(self.0), 0x08000100) + 1;
            if !(1..=1_048_576).contains(&length) {
                return String::new();
            }
            let mut bytes = vec![0u8; length as usize];
            if CFStringGetCString(self.0, bytes.as_mut_ptr().cast(), length, 0x08000100) == 0 {
                return String::new();
            }
            let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
            String::from_utf8_lossy(&bytes[..end]).into_owned()
        }
    }
    fn pair(&self, attribute: &str, kind: u32) -> NativeResult<Pair> {
        let value = self.attr(attribute)?;
        if unsafe { CFGetTypeID(value.0) != AXValueGetTypeID() } {
            return Err(format!("invalid {attribute} value type"));
        }
        let mut pair = Pair::default();
        if unsafe { AXValueGetValue(value.0, kind, (&mut pair as *mut Pair).cast()) } == 0 {
            return Err(format!("invalid {attribute}"));
        }
        Ok(pair)
    }
    fn set(&self, attribute: &str, value: Ref) -> NativeResult<()> {
        check(
            unsafe { AXUIElementSetAttributeValue(self.0, Self::string(attribute)?.0, value) },
            attribute,
        )
    }
}
fn check(code: i32, operation: &str) -> NativeResult<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(format!("macOS {operation} failed (AXError {code}); check window state and Accessibility permission"))
    }
}
fn permission() -> NativeResult<()> {
    if unsafe { AXIsProcessTrusted() } == 0 {
        Err("请在系统设置→隐私与安全性→辅助功能中授权 Nuphus 后重试".into())
    } else {
        Ok(())
    }
}

struct Window {
    app: Owned,
    element: Owned,
    pid: i32,
}
#[derive(Default)]
struct State {
    next: i32,
    windows: HashMap<i32, Window>,
}
impl State {
    fn register(&mut self, pid: i32, app: Ref, element: Ref) -> NativeResult<i32> {
        unsafe {
            AXUIElementSetMessagingTimeout(app, 0.25);
            AXUIElementSetMessagingTimeout(element, 0.25);
        }
        if let Some((&handle, _)) = self
            .windows
            .iter()
            .find(|(_, w)| w.pid == pid && unsafe { CFEqual(w.element.0, element) } != 0)
        {
            return Ok(handle);
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or("window handle space exhausted")?;
        self.windows.insert(
            self.next,
            Window {
                app: Owned::new(unsafe { CFRetain(app) })?,
                element: Owned::new(unsafe { CFRetain(element) })?,
                pid,
            },
        );
        Ok(self.next)
    }
    fn refresh(&mut self) -> NativeResult<Value> {
        permission()?;
        let deadline = Instant::now() + Duration::from_secs(8);
        // Process IDs remain available without Screen Recording access; titles come from AX.
        let cg = Owned::new(unsafe { CGWindowListCopyWindowInfo(16, 0) })?;
        let key = Owned::string("kCGWindowOwnerPID")?;
        let mut pids = BTreeSet::new();
        for index in 0..unsafe { CFArrayGetCount(cg.0) } {
            let dictionary = unsafe { CFArrayGetValueAtIndex(cg.0, index) };
            let number = unsafe { CFDictionaryGetValue(dictionary, key.0) };
            let mut pid = 0i32;
            if !number.is_null()
                && unsafe { CFNumberGetValue(number, 3, (&mut pid as *mut i32).cast()) } != 0
                && pid > 0
            {
                pids.insert(pid);
            }
        }
        let mut result = Vec::new();
        let mut live = BTreeSet::new();
        for pid in pids {
            if Instant::now() >= deadline {
                return Err(
                    "window enumeration timed out; an application may be unresponsive".into(),
                );
            }
            let app = Owned::new(unsafe { AXUIElementCreateApplication(pid) })?;
            unsafe {
                AXUIElementSetMessagingTimeout(app.0, 0.25);
            }
            let Ok(windows) = app.attr("AXWindows") else {
                continue;
            };
            if unsafe { CFGetTypeID(windows.0) != CFArrayGetTypeID() } {
                continue;
            }
            for index in 0..unsafe { CFArrayGetCount(windows.0) }.min(256) {
                if Instant::now() >= deadline {
                    return Err(
                        "window enumeration timed out; an application may be unresponsive".into(),
                    );
                }
                let element = unsafe { CFArrayGetValueAtIndex(windows.0, index) };
                let handle = self.register(pid, app.0, element)?;
                if let Ok(info) = self.info(handle) {
                    live.insert(handle);
                    let bounds = &info["window"];
                    result.push(json!({"hwnd": handle, "title": info["title"], "x": bounds["x"], "y": bounds["y"], "width": bounds["width"], "height": bounds["height"], "process_id": pid, "process_name": info["process_name"]}));
                }
            }
        }
        self.windows.retain(|handle, _| live.contains(handle));
        Ok(Value::Array(result))
    }
    fn get(&self, handle: i32) -> NativeResult<&Window> {
        self.windows
            .get(&handle)
            .ok_or_else(|| format!("window handle {handle} expired; refresh window list"))
    }
    fn info(&self, handle: i32) -> NativeResult<Value> {
        let window = self.get(handle)?;
        let position = window.element.pair("AXPosition", 1)?;
        let size = window.element.pair("AXSize", 2)?;
        let title = window
            .element
            .attr("AXTitle")
            .map(|v| v.text())
            .unwrap_or_default();
        let app = window
            .app
            .attr("AXTitle")
            .map(|v| v.text())
            .unwrap_or_default();
        let minimized = window
            .element
            .attr("AXMinimized")
            .map(|v| unsafe { CFEqual(v.0, kCFBooleanTrue) != 0 })
            .unwrap_or(false);
        let bounds = json!({"x": position.x.round() as i32, "y": position.y.round() as i32, "width": size.x.round() as i32, "height": size.y.round() as i32});
        Ok(
            json!({"hwnd": handle, "title": title, "process_id": window.pid, "process_name": app, "visible": !minimized, "minimized": minimized, "window": bounds, "client": bounds}),
        )
    }
    fn foreground(&mut self) -> NativeResult<i32> {
        permission()?;
        let system = Owned::new(unsafe { AXUIElementCreateSystemWide() })?;
        unsafe {
            AXUIElementSetMessagingTimeout(system.0, 0.5);
        }
        let app = system.attr("AXFocusedApplication")?;
        let window = app.attr("AXFocusedWindow")?;
        let mut pid = 0;
        check(
            unsafe { AXUIElementGetPid(app.0, &mut pid) },
            "focused process",
        )?;
        self.register(pid, app.0, window.0)
    }
}

type Job = Box<dyn FnOnce(&mut State) + Send>;
fn call(
    operation: impl FnOnce(&mut State) -> NativeResult<Value> + Send + 'static,
) -> Result<Value, String> {
    static WORKER: OnceLock<NativeResult<mpsc::Sender<Job>>> = OnceLock::new();
    let sender = WORKER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("macos-window".into())
                .spawn(move || {
                    let mut state = State::default();
                    while let Ok(job) = receiver.recv() {
                        job(&mut state);
                    }
                })
                .map_err(|error| error.to_string())?;
            Ok(sender)
        })
        .as_ref()
        .map_err(|e| e.clone())?;
    let (send, receive) = mpsc::sync_channel(1);
    let deadline = Instant::now() + Duration::from_secs(15);
    sender
        .send(Box::new(move |state| {
            // A timed-out queued action must never execute later.
            if Instant::now() >= deadline {
                return;
            }
            let _ = send.send(operation(state));
        }))
        .map_err(|_| "macOS window worker stopped".to_string())?;
    // Both the timeout and the worker's own error carry plain messages.
    let value = receive
        .recv_timeout(Duration::from_secs(15))
        .map_err(|_| "macOS window operation timed out".to_string())??;
    Ok(json!({"success": true, "result": value}))
}

pub fn windows_list() -> Result<Value, String> {
    call(State::refresh)
}
pub fn window_info(handle: i32) -> Result<Value, String> {
    call(move |state| {
        permission()?;
        state.info(handle)
    })
}

/// An ownership-only ferry for public AX proxy references. The source worker
/// retains each object before sending it; the receiving AX worker creates its
/// own non-Send owner. No native call is made through these pointers in transit.
/// Unlike a title/geometry match, CFEqual still identifies the exact window.
pub(crate) struct AxWindowReference {
    pid: i32,
    app: usize,
    window: usize,
}

impl AxWindowReference {
    pub(crate) fn into_raw(mut self) -> (i32, usize, usize) {
        let app = std::mem::take(&mut self.app);
        let window = std::mem::take(&mut self.window);
        (self.pid, app, window)
    }
}

impl Drop for AxWindowReference {
    fn drop(&mut self) {
        for value in [self.app, self.window] {
            if value != 0 {
                unsafe { CFRelease(value as Ref) };
            }
        }
    }
}

pub(crate) fn accessibility_target(handle: i32) -> Result<AxWindowReference, String> {
    let (sender, receiver) = mpsc::sync_channel(1);
    call(move |state| {
        permission()?;
        state.info(handle)?;
        let target = state.get(handle)?;
        let reference = AxWindowReference {
            pid: target.pid,
            app: unsafe { CFRetain(target.app.0) } as usize,
            window: unsafe { CFRetain(target.element.0) } as usize,
        };
        sender
            .send(reference)
            .map_err(|_| "AX target receiver stopped")?;
        Ok(Value::Null)
    })?;
    receiver
        .recv()
        .map_err(|_| "AX target identity was unavailable".to_string())
}
pub fn foreground_hwnd() -> Result<Value, String> {
    call(|state| Ok(json!({"hwnd": state.foreground()?})))
}
pub fn window_is_foreground(handle: i32) -> Result<Value, String> {
    call(move |state| {
        state.info(handle)?;
        Ok(json!({"hwnd": handle, "foreground": state.foreground()? == handle}))
    })
}
pub fn window_activate(handle: i32) -> Result<Value, String> {
    call(move |state| {
        permission()?;
        let window = state.get(handle)?;
        if window
            .element
            .attr("AXMinimized")
            .map(|v| unsafe { CFEqual(v.0, kCFBooleanTrue) != 0 })
            .unwrap_or(false)
        {
            window
                .element
                .set("AXMinimized", unsafe { kCFBooleanFalse })?;
        }
        window.app.set("AXFrontmost", unsafe { kCFBooleanTrue })?;
        check(
            unsafe { AXUIElementPerformAction(window.element.0, Owned::string("AXRaise")?.0) },
            "AXRaise",
        )?;
        // Some apps make AXMain read-only. Raising is sufficient if focus verification succeeds.
        let _ = window.element.set("AXMain", unsafe { kCFBooleanTrue });
        for _ in 0..10 {
            if state.foreground()? == handle {
                return Ok(json!({"hwnd": handle, "foreground": true}));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(format!("window {handle} did not become the focused window"))
    })
}
fn set_pair(
    handle: i32,
    attribute: &'static str,
    kind: u32,
    first: i32,
    second: i32,
) -> Result<Value, String> {
    call(move |state| {
        permission()?;
        let window = state.get(handle)?;
        let pair = Pair {
            x: first as f64,
            y: second as f64,
        };
        let value = Owned::new(unsafe { AXValueCreate(kind, (&pair as *const Pair).cast()) })?;
        window.element.set(attribute, value.0)?;
        let actual = window.element.pair(attribute, kind)?;
        if (actual.x - pair.x).abs() > 2.0 || (actual.y - pair.y).abs() > 2.0 {
            return Err(format!(
                "window rejected requested {attribute}; actual {}, {}",
                actual.x, actual.y
            ));
        }
        let mut info = state.info(handle)?;
        if attribute == "AXPosition" {
            info["x"] = json!(actual.x.round() as i32);
            info["y"] = json!(actual.y.round() as i32);
        } else {
            info["width"] = json!(actual.x.round() as i32);
            info["height"] = json!(actual.y.round() as i32);
        }
        Ok(info)
    })
}
pub fn window_move(handle: i32, x: i32, y: i32) -> Result<Value, String> {
    set_pair(handle, "AXPosition", 1, x, y)
}
pub fn window_resize(handle: i32, width: i32, height: i32) -> Result<Value, String> {
    if width <= 0 || height <= 0 {
        return Err("window dimensions must be positive".to_string());
    }
    set_pair(handle, "AXSize", 2, width, height)
}

/// Resolve AX's retained identity to one unambiguous CG window only when capture is requested.
pub fn capture_window_id(handle: i32) -> Result<u32, String> {
    let info = window_info(handle)?;
    let info = &info["result"];
    let bounds = &info["window"];
    let windows = xcap::Window::all().map_err(|e| e.to_string())?;
    let matches: Vec<_> = windows
        .into_iter()
        .filter(|window| {
            window.pid().ok().map(i64::from) == info["process_id"].as_i64()
                && window.x().ok().map(i64::from) == bounds["x"].as_i64()
                && window.y().ok().map(i64::from) == bounds["y"].as_i64()
                && window.width().ok().map(i64::from) == bounds["width"].as_i64()
                && window.height().ok().map(i64::from) == bounds["height"].as_i64()
                && window.title().ok().as_deref() == info["title"].as_str()
        })
        .collect();
    if matches.len() != 1 {
        return Err("窗口截图无法唯一定位；请确认录屏权限并刷新窗口列表".to_string());
    }
    matches[0].id().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_cf_strings_preserve_unicode_and_delimiters() {
        let title = "编辑文稿 ||| 中文\nsecond line";
        assert_eq!(Owned::string(title).unwrap().text(), title);
    }

    #[test]
    fn stale_handles_are_errors_without_desktop_permissions() {
        let state = State::default();
        assert!(state.info(123).unwrap_err().contains("expired"));
    }

    #[test]
    fn retained_identity_ferry_keeps_both_ownerships_alive() {
        let original = Owned::string("identity ferry").unwrap();
        let reference = AxWindowReference {
            pid: 7,
            app: unsafe { CFRetain(original.0) } as usize,
            window: unsafe { CFRetain(original.0) } as usize,
        };
        drop(original);
        let (pid, app, window) = reference.into_raw();
        let app = Owned::new(app as Ref).unwrap();
        let window = Owned::new(window as Ref).unwrap();
        assert_eq!(pid, 7);
        assert_ne!(unsafe { CFEqual(app.0, window.0) }, 0);
        drop(app);
        assert_eq!(window.text(), "identity ferry");
    }

    #[test]
    fn native_ax_values_round_trip_logical_negative_coordinates() {
        let position = Pair {
            x: -1920.0,
            y: 25.0,
        };
        let owned =
            Owned::new(unsafe { AXValueCreate(1, (&position as *const Pair).cast()) }).unwrap();
        let mut actual = Pair::default();
        assert_ne!(
            unsafe { AXValueGetValue(owned.0, 1, (&mut actual as *mut Pair).cast()) },
            0
        );
        assert_eq!(actual.x, position.x);
        assert_eq!(actual.y, position.y);
    }
}
