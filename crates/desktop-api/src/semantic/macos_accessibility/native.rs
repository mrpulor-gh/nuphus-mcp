//! Small, ownership-checked bridge to Apple's public Accessibility APIs.
//! CF objects are deliberately !Send/!Sync and used only on the AX worker.

use super::semantic::*;
use super::*;
use std::ffi::{c_char, c_void, CString};
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::Instant;

type Ref = *const c_void;
type AxError = i32;
const UTF8: u32 = 0x08000100;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementCreateSystemWide() -> Ref;
    fn AXUIElementCreateApplication(pid: i32) -> Ref;
    fn AXUIElementGetTypeID() -> usize;
    fn AXUIElementGetPid(element: Ref, pid: *mut i32) -> AxError;
    fn AXUIElementSetMessagingTimeout(element: Ref, seconds: f32) -> AxError;
    fn AXUIElementCopyAttributeValue(element: Ref, attribute: Ref, value: *mut Ref) -> AxError;
    fn AXUIElementGetAttributeValueCount(
        element: Ref,
        attribute: Ref,
        count: *mut isize,
    ) -> AxError;
    fn AXUIElementCopyAttributeValues(
        element: Ref,
        attribute: Ref,
        index: isize,
        count: isize,
        values: *mut Ref,
    ) -> AxError;
    fn AXUIElementIsAttributeSettable(element: Ref, attribute: Ref, settable: *mut u8) -> AxError;
    fn AXUIElementCopyActionNames(element: Ref, value: *mut Ref) -> AxError;
    fn AXUIElementSetAttributeValue(element: Ref, attribute: Ref, value: Ref) -> AxError;
    fn AXUIElementPerformAction(element: Ref, action: Ref) -> AxError;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(value: Ref);
    fn CFRetain(value: Ref) -> Ref;
    fn CFGetTypeID(value: Ref) -> usize;
    fn CFEqual(first: Ref, second: Ref) -> u8;
    fn CFHash(value: Ref) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFStringCreateWithBytes(
        allocator: Ref,
        bytes: *const u8,
        length: isize,
        encoding: u32,
        external: u8,
    ) -> Ref;
    fn CFStringGetLength(value: Ref) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(value: Ref, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFArrayGetTypeID() -> usize;
    fn CFArrayGetCount(value: Ref) -> isize;
    fn CFArrayGetValueAtIndex(value: Ref, index: isize) -> Ref;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(value: Ref) -> u8;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(value: Ref, number_type: isize, result: *mut c_void) -> u8;
    fn CFNumberCreate(allocator: Ref, number_type: isize, value: *const c_void) -> Ref;
    static kCFBooleanTrue: Ref;
    static kCFBooleanFalse: Ref;
}

#[link(name = "AppKit", kind = "framework")]
extern "C" {}
#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

struct Pool(*mut c_void);
impl Pool {
    fn new() -> Self {
        Self(unsafe { objc_autoreleasePoolPush() })
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        unsafe {
            objc_autoreleasePoolPop(self.0);
        }
    }
}

struct Owned(Ref, PhantomData<Rc<()>>);
impl Owned {
    unsafe fn take(raw: Ref) -> Option<Self> {
        (!raw.is_null()).then_some(Self(raw, PhantomData))
    }
    fn string(value: &str) -> Self {
        // CF allocation failure is exceptional; null is safely rejected by
        // every attribute operation rather than passed into CoreFoundation.
        Self(
            unsafe {
                CFStringCreateWithBytes(
                    std::ptr::null(),
                    value.as_ptr(),
                    value.len() as isize,
                    UTF8,
                    0,
                )
            },
            PhantomData,
        )
    }
    fn text(&self) -> Option<String> {
        unsafe {
            if self.0.is_null() || CFGetTypeID(self.0) != CFStringGetTypeID() {
                return None;
            }
            let length = CFStringGetLength(self.0);
            // Accessibility values can be entire documents. Do not allocate
            // unbounded buffers, and never expose their text in observations.
            if length > 1_048_576 {
                return None;
            }
            let size = CFStringGetMaximumSizeForEncoding(length, UTF8).checked_add(1)?;
            if size <= 0 {
                return None;
            }
            let mut bytes = vec![0; size as usize];
            if CFStringGetCString(self.0, bytes.as_mut_ptr().cast(), size, UTF8) == 0 {
                return None;
            }
            let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
            String::from_utf8(bytes[..end].to_vec()).ok()
        }
    }
    fn boolean(&self) -> Option<bool> {
        unsafe {
            if CFGetTypeID(self.0) == CFBooleanGetTypeID() {
                return Some(CFBooleanGetValue(self.0) != 0);
            }
            if CFGetTypeID(self.0) == CFNumberGetTypeID() {
                let mut number = 0_i32;
                if CFNumberGetValue(self.0, 3, (&mut number as *mut i32).cast()) != 0 {
                    // AXValue == 2 is the mixed checkbox state, not "true".
                    return match number {
                        0 => Some(false),
                        1 => Some(true),
                        _ => None,
                    };
                }
            }
            None
        }
    }
    fn number(&self) -> Option<f64> {
        unsafe {
            if CFGetTypeID(self.0) != CFNumberGetTypeID() {
                return None;
            }
            let mut number = 0.0_f64;
            (CFNumberGetValue(self.0, 6, (&mut number as *mut f64).cast()) != 0
                && number.is_finite())
            .then_some(number)
        }
    }
    fn array(&self, limit: usize) -> Vec<Self> {
        unsafe {
            if CFGetTypeID(self.0) != CFArrayGetTypeID() {
                return vec![];
            }
            (0..CFArrayGetCount(self.0).min(limit as isize))
                .filter_map(|index| {
                    let value = CFArrayGetValueAtIndex(self.0, index);
                    if value.is_null() {
                        None
                    } else {
                        Self::take(CFRetain(value))
                    }
                })
                .collect()
        }
    }

    fn array_len(&self) -> Option<usize> {
        unsafe {
            (CFGetTypeID(self.0) == CFArrayGetTypeID()).then(|| CFArrayGetCount(self.0) as usize)
        }
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CFRelease(self.0);
            }
        }
    }
}

/// Public within the crate for the legacy window service. Keep instances on
/// the owning thread; re-acquire by application/window identity for each call.
pub(crate) struct Element(Owned);
impl Element {
    fn retained(&self) -> Self {
        Self(Owned(unsafe { CFRetain(self.0 .0) }, PhantomData))
    }
    fn from_owned(value: Owned) -> Option<Self> {
        if unsafe { CFGetTypeID(value.0) } != unsafe { AXUIElementGetTypeID() } {
            return None;
        }
        unsafe {
            AXUIElementSetMessagingTimeout(value.0, 0.15);
        }
        Some(Self(value))
    }
    pub(crate) fn application(pid: i32) -> Option<Self> {
        Self::from_owned(unsafe { Owned::take(AXUIElementCreateApplication(pid)) }?)
    }
    pub(crate) fn system() -> Option<Self> {
        Self::from_owned(unsafe { Owned::take(AXUIElementCreateSystemWide()) }?)
    }
    fn attribute(&self, name: &str, deadline: Instant) -> Option<Owned> {
        if Instant::now() >= deadline {
            return None;
        }
        let key = Owned::string(name);
        if key.0.is_null() {
            return None;
        }
        let mut result = std::ptr::null();
        let status = unsafe { AXUIElementCopyAttributeValue(self.0 .0, key.0, &mut result) };
        let owned = unsafe { Owned::take(result) };
        if status == 0 {
            owned
        } else {
            None
        }
    }
    pub(crate) fn text(&self, name: &str, deadline: Instant) -> Option<String> {
        self.attribute(name, deadline)?
            .text()
            .filter(|s| !s.is_empty())
    }
    pub(crate) fn boolean(&self, name: &str, deadline: Instant) -> Option<bool> {
        self.attribute(name, deadline)?.boolean()
    }
    pub(crate) fn child(&self, name: &str, deadline: Instant) -> Option<Self> {
        Self::from_owned(self.attribute(name, deadline)?)
    }
    fn children_page(
        &self,
        name: &str,
        limit: usize,
        deadline: Instant,
    ) -> Result<(Vec<Self>, bool), AutomationError> {
        self.children_page_at(name, 0, limit, deadline)
    }
    fn children_page_at(
        &self,
        name: &str,
        offset: usize,
        limit: usize,
        deadline: Instant,
    ) -> Result<(Vec<Self>, bool), AutomationError> {
        check_deadline(deadline)?;
        let key = Owned::string(name);
        if key.0.is_null() {
            return Err(AutomationError::Observation(
                "AX attribute allocation failed".into(),
            ));
        }
        let mut count = 0_isize;
        let status = unsafe { AXUIElementGetAttributeValueCount(self.0 .0, key.0, &mut count) };
        check_deadline(deadline)?;
        // Unsupported/absent children are normal on leaf controls. Messaging
        // failures must not masquerade as an empty, complete subtree.
        if matches!(status, -25205 | -25208 | -25212) {
            return Ok((vec![], false));
        }
        if status != 0 {
            return Err(AutomationError::Observation(format!(
                "AX tree enumeration failed (AXError {status}); partial tree discarded"
            )));
        }
        let count = usize::try_from(count).map_err(|_| {
            AutomationError::Observation("AX returned an invalid child count".into())
        })?;
        let (requested, omitted) = native_child_page(count, offset, limit);
        if requested == 0 {
            return Ok((vec![], omitted));
        }
        let mut result = std::ptr::null();
        let status = unsafe {
            AXUIElementCopyAttributeValues(
                self.0 .0,
                key.0,
                offset as isize,
                requested as isize,
                &mut result,
            )
        };
        let owned = unsafe { Owned::take(result) };
        check_deadline(deadline)?;
        if status != 0 {
            return Err(AutomationError::Observation(format!("AX child page changed or could not be read (AXError {status}); refresh the observation")));
        }
        let array = owned
            .ok_or_else(|| AutomationError::Observation("AX child page is unavailable".into()))?;
        if array.array_len() != Some(requested) {
            return Err(AutomationError::Observation(
                "AX child page changed during traversal; refresh the observation".into(),
            ));
        }
        let children: Vec<_> = array
            .array(requested)
            .into_iter()
            .filter_map(Self::from_owned)
            .collect();
        if children.len() != requested {
            return Err(AutomationError::Observation(
                "AX child page contains an invalid element".into(),
            ));
        }
        Ok((children, omitted))
    }
    pub(crate) fn pid(&self) -> Option<i32> {
        let mut pid = 0;
        (unsafe { AXUIElementGetPid(self.0 .0, &mut pid) } == 0).then_some(pid)
    }
    pub(crate) fn same(&self, other: &Self) -> bool {
        unsafe { CFEqual(self.0 .0, other.0 .0) != 0 }
    }
    fn settable(&self, name: &str, deadline: Instant) -> bool {
        if Instant::now() >= deadline {
            return false;
        }
        let key = Owned::string(name);
        let mut result = 0;
        !key.0.is_null()
            && unsafe { AXUIElementIsAttributeSettable(self.0 .0, key.0, &mut result) } == 0
            && result != 0
    }
    fn action_names(&self, deadline: Instant) -> Vec<String> {
        if Instant::now() >= deadline {
            return vec![];
        }
        let mut result = std::ptr::null();
        let status = unsafe { AXUIElementCopyActionNames(self.0 .0, &mut result) };
        let owned = unsafe { Owned::take(result) };
        if status != 0 {
            return vec![];
        }
        owned
            .map(|v| v.array(32).iter().filter_map(Owned::text).collect())
            .unwrap_or_default()
    }
    pub(crate) fn perform(&self, name: &str) -> Result<(), AutomationError> {
        let name = Owned::string(name);
        if name.0.is_null() {
            return Err(AutomationError::Execution(
                "AX action allocation failed".into(),
            ));
        }
        check(unsafe { AXUIElementPerformAction(self.0 .0, name.0) })
    }
    pub(crate) fn set_bool(&self, name: &str, value: bool) -> Result<(), AutomationError> {
        check(self.set_bool_status(name, value))
    }
    fn set_bool_status(&self, name: &str, value: bool) -> AxError {
        let key = Owned::string(name);
        if key.0.is_null() {
            return -25200;
        }
        unsafe {
            AXUIElementSetAttributeValue(
                self.0 .0,
                key.0,
                if value {
                    kCFBooleanTrue
                } else {
                    kCFBooleanFalse
                },
            )
        }
    }
    fn set_text(&self, name: &str, value: &str) -> Result<(), AutomationError> {
        let key = Owned::string(name);
        let value = Owned::string(value);
        if key.0.is_null() || value.0.is_null() {
            return Err(AutomationError::Execution(
                "AX value allocation failed".into(),
            ));
        }
        check(unsafe { AXUIElementSetAttributeValue(self.0 .0, key.0, value.0) })
    }
    fn set_number(&self, name: &str, value: f64) -> Result<(), AutomationError> {
        let key = Owned::string(name);
        let number = unsafe {
            Owned::take(CFNumberCreate(
                std::ptr::null(),
                6,
                (&value as *const f64).cast(),
            ))
        }
        .ok_or_else(|| AutomationError::Execution("AX numeric value allocation failed".into()))?;
        if key.0.is_null() {
            return Err(AutomationError::Execution(
                "AX attribute allocation failed".into(),
            ));
        }
        check(unsafe { AXUIElementSetAttributeValue(self.0 .0, key.0, number.0) })
    }
}

fn check(status: AxError) -> Result<(), AutomationError> {
    if status == 0 {
        Ok(())
    } else {
        Err(AutomationError::Execution(format!("Accessibility operation failed (AXError {status}); refresh the interface before retrying")))
    }
}

pub(crate) fn trusted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}

/// NSRunningApplication is safe to query off the main thread. NSString values
/// are copied before draining this thread's autorelease pool.
pub(crate) fn application_identity(pid: i32) -> Option<AppIdentity> {
    let _pool = Pool::new();
    unsafe {
        let class_name = CString::new("NSRunningApplication").ok()?;
        let class = objc_getClass(class_name.as_ptr());
        if class.is_null() {
            return None;
        }
        let selector = CString::new("runningApplicationWithProcessIdentifier:").ok()?;
        let send_pid: unsafe extern "C" fn(*mut c_void, *mut c_void, i32) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let app = send_pid(class, sel_registerName(selector.as_ptr()), pid);
        if app.is_null() {
            return None;
        }
        let send: unsafe extern "C" fn(*mut c_void, *mut c_void) -> Ref =
            std::mem::transmute(objc_msgSend as *const ());
        let string = |selector: &str| -> Option<String> {
            let selector = CString::new(selector).ok()?;
            let raw = send(app, sel_registerName(selector.as_ptr()));
            if raw.is_null() {
                return None;
            }
            Owned::take(CFRetain(raw))?.text()
        };
        let id = string("bundleIdentifier")?;
        Some(AppIdentity {
            display_name: redact(&string("localizedName").unwrap_or_else(|| id.clone())),
            id,
        })
    }
}

/// Public AppKit launchDate supplies a process-lifetime key without PID-only
/// caching or depending on a private WindowServer interface.
fn process_lifetime(pid: i32) -> Option<u64> {
    let _pool = Pool::new();
    unsafe {
        let class = objc_getClass(c"NSRunningApplication".as_ptr());
        if class.is_null() {
            return None;
        }
        let send_pid: unsafe extern "C" fn(*mut c_void, *mut c_void, i32) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let app = send_pid(
            class,
            sel_registerName(c"runningApplicationWithProcessIdentifier:".as_ptr()),
            pid,
        );
        if app.is_null() {
            return None;
        }
        let send_object: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let date = send_object(app, sel_registerName(c"launchDate".as_ptr()));
        if date.is_null() {
            return None;
        }
        let send_number: unsafe extern "C" fn(*mut c_void, *mut c_void) -> f64 =
            std::mem::transmute(objc_msgSend as *const ());
        let started = send_number(date, sel_registerName(c"timeIntervalSince1970".as_ptr()));
        started.is_finite().then_some(started.to_bits())
    }
}

fn enable_application_accessibility(
    app: &Element,
    pid: i32,
    deadline: Instant,
) -> Result<(), AutomationError> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static ATTEMPTED: OnceLock<Mutex<HashMap<i32, u64>>> = OnceLock::new();
    let cache = ATTEMPTED.get_or_init(|| Mutex::new(HashMap::new()));
    let lifetime = process_lifetime(pid);
    if cache
        .lock()
        .ok()
        .is_some_and(|entries| same_process_lifetime(entries.get(&pid).copied(), lifetime))
    {
        return Ok(());
    }
    check_deadline(deadline)?;
    let modern = app.set_bool_status("AXManualAccessibility", true);
    let status = if legacy_ax_enablement_allowed(modern) {
        app.set_bool_status("AXEnhancedUserInterface", true)
    } else {
        modern
    };
    if status == 0 {
        // Chromium builds its tree asynchronously after accepting the attribute.
        std::thread::sleep(
            std::time::Duration::from_millis(300)
                .min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    if status == 0 || legacy_ax_enablement_allowed(status) {
        if let (Some(stamp), Ok(mut entries)) = (lifetime, cache.lock()) {
            if entries.len() >= 256 {
                entries.clear();
            }
            entries.insert(pid, stamp);
        }
    }
    check_deadline(deadline)
}

fn target_elements(
    scope: &ObservationScope,
    deadline: Instant,
) -> Result<(Element, Element, i32), AutomationError> {
    if let Some(handle) = scope.window_handle {
        let reference = crate::platform::macos::accessibility_target(handle)
            .map_err(|error| AutomationError::Observation(error.to_string()))?;
        let (pid, app, window) = reference.into_raw();
        // These are independent retained ownerships sent by the native window
        // service. Adopt both before validating so every error releases both.
        let app = unsafe { Owned::take(app as Ref) }.and_then(Element::from_owned);
        let window = unsafe { Owned::take(window as Ref) }.and_then(Element::from_owned);
        let (Some(app), Some(window)) = (app, window) else {
            return Err(AutomationError::Observation(
                "bound AX target is no longer available".into(),
            ));
        };
        if app.pid() != Some(pid)
            || window.pid() != Some(pid)
            || window.text("AXRole", deadline).is_none()
        {
            return Err(AutomationError::Observation(
                "bound AX target expired".into(),
            ));
        }
        return Ok((app, window, pid));
    }
    let app = Element::system()
        .and_then(|system| system.child("AXFocusedApplication", deadline))
        .ok_or_else(|| {
            AutomationError::Observation("no accessible foreground application".into())
        })?;
    let pid = app.pid().ok_or_else(|| {
        AutomationError::Observation("foreground application has no process identity".into())
    })?;
    let app = Element::application(pid).ok_or_else(|| {
        AutomationError::Observation("foreground application is no longer available".into())
    })?;
    let window = app
        .child("AXFocusedWindow", deadline)
        .or_else(|| app.child("AXMainWindow", deadline))
        .ok_or_else(|| {
            AutomationError::Observation("foreground application has no accessible window".into())
        })?;
    Ok((app, window, pid))
}

pub(super) struct Snapshot {
    pub app: AppIdentity,
    pub window: WindowIdentity,
    pub metadata: Vec<Metadata>,
    pub fingerprint: String,
    pub truncated: bool,
    pub window_token: u64,
    pub window_unique: bool,
}
struct NativeSnapshot {
    snapshot: Snapshot,
    elements: Vec<Element>,
    app: Element,
    window: Element,
}

struct Region {
    element: Element,
    window: Element,
    ancestors: Vec<SemanticContext>,
    origin: String,
    web_content: bool,
}

struct WalkNode {
    element: Element,
    ancestors: Vec<SemanticContext>,
    secure_parent: bool,
    web_parent: bool,
    depth: usize,
}

enum WalkWork {
    Node(WalkNode),
    Children { parent: WalkNode, offset: usize },
}

/// Ephemeral references stay on the worker. Tokens are never serialized into
/// observations or saved locators. Evicted bindings fail instead of rebinding.
#[derive(Default)]
pub(super) struct Session {
    windows: std::collections::VecDeque<(u64, Element)>,
    next_token: u64,
    regions: std::collections::HashMap<String, Region>,
}

impl Session {
    pub(super) fn capture(
        &mut self,
        limit: usize,
        scope: &ObservationScope,
        deadline: Instant,
    ) -> Result<Snapshot, AutomationError> {
        let region = self.region_for(scope)?;
        let mut native = capture_native(limit, scope, region, deadline)?;
        let window = &native.window;
        let token = match self
            .windows
            .iter()
            .find(|(_, retained)| retained.same(window))
        {
            Some((token, _)) => *token,
            None => {
                self.next_token = self.next_token.checked_add(1).ok_or_else(|| {
                    AutomationError::Observation("AX window token space exhausted".into())
                })?;
                self.windows.push_back((self.next_token, window.retained()));
                if self.windows.len() > 16 {
                    self.windows.pop_front();
                }
                self.next_token
            }
        };
        native.snapshot.window_token = token;
        // Bound this local cache without invalidating the currently requested
        // region during capture. Native references never enter model output.
        if self.regions.len() + native.elements.len() > 1024 {
            self.regions.clear();
        }
        let mut counts = std::collections::HashMap::new();
        for meta in &native.snapshot.metadata {
            *counts.entry(meta.node.opaque_id.as_str()).or_insert(0) += 1;
        }
        for (meta, element) in native.snapshot.metadata.iter().zip(&native.elements) {
            if counts.get(meta.node.opaque_id.as_str()) != Some(&1) {
                self.regions.remove(&meta.node.opaque_id);
                continue;
            }
            self.regions.insert(
                meta.node.opaque_id.clone(),
                Region {
                    element: element.retained(),
                    window: window.retained(),
                    ancestors: meta.ancestors.clone(),
                    origin: meta.origin.clone(),
                    web_content: meta.web_content,
                },
            );
        }
        Ok(native.snapshot)
    }

    fn region_for(&self, scope: &ObservationScope) -> Result<Option<&Region>, AutomationError> {
        match scope.subtree_id.as_deref() {
            None | Some("@window" | "@menu") => Ok(None),
            Some(id) => self.regions.get(id).map(Some).ok_or_else(|| AutomationError::Observation("AX region expired or is ambiguous; observe the window/menu and select a fresh region".into())),
        }
    }

    pub(super) fn execute(
        &self,
        limit: usize,
        token: u64,
        require_unique: bool,
        scope: &ObservationScope,
        locator: &SemanticLocator,
        action: &NativeAction,
        input: &ExecutionInput,
        deadline: Instant,
        cancelled: impl Fn() -> bool,
    ) -> Result<bool, AutomationError> {
        let (_, window) = self
            .windows
            .iter()
            .find(|(retained, _)| *retained == token)
            .ok_or_else(|| {
                AutomationError::Execution(
                    "AX window binding expired; refresh the candidate".into(),
                )
            })?;
        execute(
            limit,
            window,
            require_unique,
            scope,
            self.region_for(scope)?,
            locator,
            action,
            input,
            deadline,
            cancelled,
        )
    }
}

fn capture_native(
    limit: usize,
    scope: &ObservationScope,
    region: Option<&Region>,
    deadline: Instant,
) -> Result<NativeSnapshot, AutomationError> {
    check_deadline(deadline)?;
    if !trusted() {
        return Err(AutomationError::Observation("macOS Accessibility permission is required; enable Nuphus in System Settings > Privacy & Security > Accessibility".into()));
    }
    let (app, window, pid) = target_elements(scope, deadline)?;
    enable_application_accessibility(&app, pid, deadline)?;
    let identity = application_identity(pid).ok_or_else(|| {
        AutomationError::Observation(
            "foreground application has no stable bundle identifier".into(),
        )
    })?;
    let title = window.text("AXTitle", deadline).unwrap_or_default();
    let identifier = window.text("AXIdentifier", deadline);
    // Inspect sibling windows before deriving the persisted identity. Some
    // apps reuse one AXIdentifier for every document window; add the title in
    // that case, while genuinely unique identifiers remain title-independent.
    let (siblings, windows_truncated) = app.children_page("AXWindows", 128, deadline)?;
    let mut sibling_keys = Vec::new();
    let mut focused_enumerated = false;
    for sibling in &siblings {
        sibling_keys.push((
            sibling.text("AXIdentifier", deadline),
            sibling.text("AXTitle", deadline).unwrap_or_default(),
        ));
        focused_enumerated |= sibling.same(&window);
    }
    let repeated_identifier = identifier.is_some()
        && sibling_keys
            .iter()
            .filter(|(id, _)| id == &identifier)
            .count()
            > 1;
    let key = |id: Option<&str>, title: &str| {
        if repeated_identifier {
            format!("{}|{title}", id.unwrap_or_default())
        } else {
            id.unwrap_or(title).to_owned()
        }
    };
    let target_key = key(identifier.as_deref(), &title);
    let matching_windows = sibling_keys
        .iter()
        .filter(|(id, title)| key(id.as_deref(), title) == target_key)
        .count();
    let window_unique = window_is_unique(!windows_truncated, focused_enumerated, matching_windows);
    check_deadline(deadline)?;
    // Never persist a pid, CGWindowID or AX object address.
    let window_id = format!("axw:{}", hash(&format!("{}|{}", identity.id, target_key)));
    if scope.app_id.as_ref().is_some_and(|id| id != &identity.id)
        || scope.window_id.as_ref().is_some_and(|id| id != &window_id)
    {
        return Err(AutomationError::Observation(
            "foreground application/window is outside the requested scope".into(),
        ));
    }
    let window_identity = WindowIdentity {
        id: window_id,
        title: redact(&title),
    };
    let (root, root_ancestors, origin, explicit_region) =
        match (scope.subtree_id.as_deref(), region) {
            (None | Some("@window"), _) => (window.retained(), vec![], "window".to_string(), false),
            (Some("@menu"), _) => {
                let menu = app.child("AXMenuBar", deadline).ok_or_else(|| {
                    AutomationError::Observation(
                        "application exposes no Accessibility menu bar".into(),
                    )
                })?;
                (
                    menu,
                    vec![SemanticContext {
                        role: Some(UiRole::Menu),
                        automation_id: Some(MENU_SCOPE_ID.into()),
                        accessible_name: Some("menu bar".into()),
                    }],
                    "menu".to_string(),
                    false,
                )
            }
            (Some(_), Some(region)) => {
                if !region.window.same(&window) {
                    return Err(AutomationError::Observation(
                        "AX region belongs to a different window; refresh the target region".into(),
                    ));
                }
                // Invalid native elements return no role. Never silently fall back
                // from a vanished local region to a different window/element.
                if region.element.text("AXRole", deadline).is_none() {
                    return Err(AutomationError::Observation(
                        "AX region disappeared; refresh the target region".into(),
                    ));
                }
                (
                    region.element.retained(),
                    region.ancestors.clone(),
                    region.origin.clone(),
                    true,
                )
            }
            _ => {
                return Err(AutomationError::Observation(
                    "AX region is unknown; select a region returned by observation".into(),
                ))
            }
        };
    if scope.tree_offset > 10_000 {
        return Err(AutomationError::Observation(
            "AX traversal offset exceeds the bounded window scan; choose a narrower region".into(),
        ));
    }
    let mut pending = vec![WalkWork::Node(WalkNode {
        element: root,
        ancestors: root_ancestors,
        secure_parent: false,
        web_parent: region.is_some_and(|region| region.web_content),
        depth: 0,
    })];
    let mut metadata = Vec::new();
    let mut elements: Vec<Element> = Vec::new();
    let mut truncated = scope.tree_offset > 0;
    let mut visited = 0usize;
    let mut seen: std::collections::HashMap<usize, Vec<Element>> = std::collections::HashMap::new();
    while let Some(work) = pending.pop() {
        check_deadline(deadline)?;
        if metadata.len() >= limit {
            truncated = true;
            break;
        }
        let node = match work {
            WalkWork::Node(node) => node,
            WalkWork::Children { parent, offset } => {
                let (children, more) =
                    parent
                        .element
                        .children_page_at("AXChildren", offset, 32, deadline)?;
                let count = children.len();
                for child in children.into_iter().rev() {
                    // Insert child work below the continuation, preserving DFS
                    // ordering independently of the native array page size.
                    let node = WalkNode {
                        element: child,
                        ancestors: parent.ancestors.clone(),
                        secure_parent: parent.secure_parent,
                        web_parent: parent.web_parent,
                        depth: parent.depth,
                    };
                    // The continuation is pushed first below the child batch.
                    pending.push(WalkWork::Node(node));
                }
                if more {
                    let position = pending.len().saturating_sub(count);
                    pending.insert(
                        position,
                        WalkWork::Children {
                            parent,
                            offset: offset + count,
                        },
                    );
                }
                continue;
            }
        };
        let WalkNode {
            element,
            ancestors,
            secure_parent,
            web_parent,
            depth,
        } = node;
        let bucket = seen.entry(unsafe { CFHash(element.0 .0) }).or_default();
        if bucket.iter().any(|old| old.same(&element)) {
            continue;
        }
        bucket.push(element.retained());
        if visited >= 10_000 {
            truncated = true;
            break;
        }
        let include = visited >= scope.tree_offset;
        visited += 1;
        let raw_role = element.text("AXRole", deadline).unwrap_or_default();
        let web_content = web_parent || raw_role == "AXWebArea";
        let role = role(&raw_role);
        let secure = secure_parent
            || element.text("AXSubrole", deadline).as_deref() == Some("AXSecureTextField")
            || element
                .boolean("AXProtectedContent", deadline)
                .unwrap_or(false);
        let identifier = element.text("AXIdentifier", deadline);
        let mut raw_name = if secure {
            None
        } else {
            element
                .text("AXTitle", deadline)
                .or_else(|| element.text("AXDescription", deadline))
                .or_else(|| element.text("AXHelp", deadline))
                .map(|s| s.chars().take(256).collect::<String>())
        };
        let value = if secure {
            None
        } else {
            element.attribute("AXValue", deadline)
        };
        if raw_name.is_none() && raw_role == "AXStaticText" && !secure {
            raw_name = value
                .as_ref()
                .and_then(Owned::text)
                .map(|text| text.chars().take(256).collect());
        }
        let expanded = element.boolean("AXExpanded", deadline);
        let toggled = if role == UiRole::CheckBox {
            value.as_ref().and_then(Owned::boolean)
        } else {
            None
        };
        let selected = element.boolean("AXSelected", deadline).or_else(|| {
            (role == UiRole::RadioButton)
                .then(|| value.as_ref().and_then(Owned::boolean))
                .flatten()
        });
        let names = element.action_names(deadline);
        let value_settable = element.settable("AXValue", deadline);
        let mut supported_actions = actions(
            &role,
            secure,
            &names,
            element.settable("AXFocused", deadline),
            value_settable,
            element.settable("AXSelected", deadline),
            element.settable("AXExpanded", deadline),
            expanded,
        );
        if role == UiRole::CheckBox && toggled.is_none() && !value_settable {
            // AXPress may cycle a mixed checkbox in provider-specific order.
            // Keep explicit legacy Toggle, but do not advertise a desired-state
            // action that cannot be implemented without guessing that order.
            supported_actions.retain(|action| *action != NativeAction::SetChecked);
        }
        if !secure
            && value_settable
            && matches!(
                raw_role.as_str(),
                "AXSlider" | "AXScrollBar" | "AXIncrementor"
            )
            && value.as_ref().and_then(Owned::number).is_some()
            && element
                .attribute("AXMinValue", deadline)
                .and_then(|value| value.number())
                .is_some()
            && element
                .attribute("AXMaxValue", deadline)
                .and_then(|value| value.number())
                .is_some()
        {
            supported_actions.push(NativeAction::SetRangeValue);
        }
        let key = hash(&format!(
            "{}|{}|{:?}|{:?}|{:?}|{:?}",
            identity.id, window_identity.id, role, identifier, raw_name, ancestors
        ));
        let node = UiNode {
            opaque_id: format!("axe:{key}"),
            semantic_key: Some(format!("ax:{key}")),
            role: role.clone(),
            name: raw_name.as_deref().map(redact),
            short_value: None,
            enabled: element.boolean("AXEnabled", deadline).unwrap_or(true),
            visible: !element.boolean("AXHidden", deadline).unwrap_or(false),
            focused: element.boolean("AXFocused", deadline).unwrap_or(false),
            secure,
            toggled,
            selected,
            expanded,
            value_fingerprint: value
                .as_ref()
                .and_then(|value| {
                    value
                        .text()
                        .or_else(|| value.number().map(|number| number.to_string()))
                })
                .map(|s| format!("value:{}", hash(&s))),
            supported_actions,
        };
        let mut child_ancestors = ancestors.clone();
        // Root window identity already lives in the locator. Keeping window
        // titles out of ancestry allows providers' stable AXIdentifier to work.
        if !matches!(raw_role.as_str(), "AXWindow" | "AXMenuBar")
            && (identifier.is_some() || raw_name.is_some())
        {
            child_ancestors.push(SemanticContext {
                role: Some(role),
                automation_id: identifier.clone(),
                accessible_name: raw_name.clone(),
            });
            if child_ancestors.len() > 8 {
                // Keep the scope marker in saved locators even for deeply
                // nested menus; it is needed to reopen the same observation
                // scope during replay instead of searching window contents.
                let oldest_context = usize::from(
                    child_ancestors
                        .first()
                        .and_then(|ancestor| ancestor.automation_id.as_deref())
                        == Some(MENU_SCOPE_ID),
                );
                child_ancestors.remove(oldest_context);
            }
        }
        if !secure
            && (origin != "menu"
                || descend_menu(
                    &raw_role,
                    explicit_region && depth == 0,
                    expanded,
                    selected,
                    node.focused,
                ))
        {
            if depth < 24 {
                pending.push(WalkWork::Children {
                    parent: WalkNode {
                        element: element.retained(),
                        ancestors: child_ancestors,
                        secure_parent: secure,
                        web_parent: web_content,
                        depth: depth + 1,
                    },
                    offset: 0,
                });
            } else {
                truncated |= element.children_page_at("AXChildren", 0, 0, deadline)?.1;
            }
        }
        check_deadline(deadline)?;
        if include {
            metadata.push(Metadata {
                node,
                identifier,
                raw_name,
                ancestors,
                origin: origin.clone(),
                web_content,
                scroll_directions: scroll_directions(&names),
            });
            elements.push(element);
        }
    }
    check_deadline(deadline)?;
    if metadata.is_empty() && scope.tree_offset == 0 {
        return Err(AutomationError::Observation(
            "Accessibility tree was empty or timed out".into(),
        ));
    }
    let fingerprint = hash(&format!(
        "{}|{}|{:?}|{truncated}",
        identity.id, window_identity.id, metadata
    ));
    Ok(NativeSnapshot {
        snapshot: Snapshot {
            app: identity,
            window: window_identity,
            metadata,
            fingerprint,
            truncated,
            window_token: 0,
            window_unique,
        },
        elements,
        app,
        window,
    })
}

pub(super) fn validate_locator_window(
    locator: &SemanticLocator,
    observation: &Observation,
) -> Result<(), AutomationError> {
    if locator.app_id != observation.app.id
        || locator
            .window_id
            .as_ref()
            .map(|id| id != &observation.window.id)
            .unwrap_or_else(|| {
                locator
                    .window_title
                    .as_ref()
                    .is_some_and(|title| title != &observation.window.title)
            })
    {
        return Err(AutomationError::Candidates(
            "foreground application/window does not match saved AX locator".into(),
        ));
    }
    Ok(())
}

fn execute(
    limit: usize,
    expected_window: &Element,
    require_unique_window: bool,
    scope: &ObservationScope,
    region: Option<&Region>,
    locator: &SemanticLocator,
    action: &NativeAction,
    input: &ExecutionInput,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<bool, AutomationError> {
    let snapshot = capture_native(
        limit,
        &ObservationScope {
            app_id: Some(locator.app_id.clone()),
            window_id: locator.window_id.clone(),
            subtree_id: scope.subtree_id.clone(),
            window_handle: scope.window_handle,
            delivery: scope.delivery,
            tree_offset: scope.tree_offset,
        },
        region,
        deadline,
    )?;
    if !snapshot.window.same(expected_window) {
        return Err(AutomationError::Execution(
            "AX window changed since the candidate was created".into(),
        ));
    }
    if require_unique_window && !snapshot.snapshot.window_unique {
        return Err(AutomationError::Execution(
            "saved AX window identity became ambiguous before dispatch".into(),
        ));
    }
    let found: Vec<_> = snapshot
        .snapshot
        .metadata
        .iter()
        .enumerate()
        .filter(|(_, m)| matches(m, locator, action))
        .collect();
    let [(index, _)] = found.as_slice() else {
        return Err(AutomationError::Execution(
            "AX target is missing or ambiguous after refresh".into(),
        ));
    };
    if Instant::now() >= deadline || cancelled() {
        return Err(AutomationError::Execution(
            "AX request expired before dispatch".into(),
        ));
    }
    let element = &snapshot.elements[*index];
    let is_foreground = || {
        Element::system()
            .and_then(|system| system.child("AXFocusedApplication", deadline))
            .is_some_and(|app| {
                app.same(&snapshot.app)
                    && app
                        .child("AXFocusedWindow", deadline)
                        .or_else(|| app.child("AXMainWindow", deadline))
                        .is_some_and(|window| window.same(&snapshot.window))
            })
    };
    let needs_foreground =
        requires_foreground(scope.delivery, scope.window_handle.is_some(), action);
    if needs_foreground && !is_foreground() {
        if scope.delivery == DeliveryMode::Background {
            return Err(AutomationError::Execution("background_unavailable: this AX focus action requires the bound window in foreground".into()));
        }
        let handle = scope.window_handle.ok_or_else(|| {
            AutomationError::Execution(
                "foreground application/window changed before AX dispatch".into(),
            )
        })?;
        crate::platform::macos::window_activate(handle)
            .map_err(|error| AutomationError::Execution(error.to_string()))?;
        if !is_foreground() {
            return Err(AutomationError::Execution(
                "the exact AX target did not become foreground".into(),
            ));
        }
    }
    // Target-addressed actions do not depend on whichever app is frontmost.
    // With an explicit handle, re-check its retained public AX identity rather
    // than silently changing the target after a user focus switch.
    if scope.window_handle.is_some() {
        let (app, window, _) = target_elements(scope, deadline)?;
        if !app.same(&snapshot.app) || !window.same(&snapshot.window) {
            return Err(AutomationError::Execution(
                "bound AX target changed before dispatch".into(),
            ));
        }
    }
    if Instant::now() >= deadline || cancelled() {
        return Err(AutomationError::Execution(
            "AX request expired before dispatch".into(),
        ));
    }
    match action {
        NativeAction::Invoke | NativeAction::Toggle => element.perform("AXPress"),
        NativeAction::SetChecked => {
            let desired = input.checked.ok_or_else(|| {
                AutomationError::Execution("set_checked requires an explicit desired state".into())
            })?;
            let current = element
                .attribute("AXValue", deadline)
                .and_then(|value| value.boolean());
            if current == Some(desired) {
                return Ok(false);
            }
            if element.settable("AXValue", deadline) {
                element.set_number("AXValue", f64::from(u8::from(desired)))
            } else if checked_needs_press(current, desired)? {
                element.perform("AXPress")
            } else {
                Ok(())
            }
        }
        NativeAction::Scroll => {
            if input.amount == Some(ScrollAmount::Small) {
                return Err(AutomationError::Execution(
                    "AX supports page scrolling for this control, not a small increment".into(),
                ));
            }
            let direction = input.direction.ok_or_else(|| {
                AutomationError::Execution("AX scroll requires a direction".into())
            })?;
            let action = scroll_action(direction);
            if !element
                .action_names(deadline)
                .iter()
                .any(|name| name == action)
            {
                return Err(AutomationError::Execution(
                    "AX scroll direction is no longer supported".into(),
                ));
            }
            element.perform(action)
        }
        NativeAction::ScrollIntoView => element.perform("AXScrollToVisible"),
        NativeAction::SetRangeValue => {
            let minimum = element
                .attribute("AXMinValue", deadline)
                .and_then(|value| value.number());
            let maximum = element
                .attribute("AXMaxValue", deadline)
                .and_then(|value| value.number());
            let (Some(minimum), Some(maximum), Some(value)) =
                (minimum, maximum, input.value.as_deref())
            else {
                return Err(AutomationError::Execution(
                    "AX range value or its live bounds are unavailable".into(),
                ));
            };
            let value = range_value(value, minimum, maximum)?;
            element.set_number("AXValue", value)
        }
        NativeAction::Select => {
            if element.settable("AXSelected", deadline) {
                element.set_bool("AXSelected", true)
            } else {
                element.perform("AXPress")
            }
        }
        NativeAction::Expand => element.set_bool("AXExpanded", true),
        NativeAction::Collapse => element.set_bool("AXExpanded", false),
        NativeAction::Focus => element.set_bool("AXFocused", true),
        NativeAction::SetValue => {
            let value = input.value.as_deref().ok_or_else(|| {
                AutomationError::Execution("AX SetValue requires a local value slot".into())
            })?;
            element.set_text("AXValue", value)
        }
        _ => Err(AutomationError::Execution("unsupported AX action".into())),
    }
    .map(|()| true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_foundation_string_roundtrips_unicode_and_preserves_owned_lifetime() {
        let value = Owned::string("中文 RPA 🖥️");
        let retained = unsafe { Owned::take(CFRetain(value.0)) }.unwrap();
        drop(value);
        assert_eq!(retained.text().as_deref(), Some("中文 RPA 🖥️"));
        assert!(retained.boolean().is_none());
        assert!(retained.array(10).is_empty());
    }

    #[test]
    fn native_checkbox_mixed_value_is_not_a_boolean() {
        for (number, expected) in [(0.0_f64, Some(false)), (1.0, Some(true)), (2.0, None)] {
            let value = unsafe {
                Owned::take(CFNumberCreate(
                    std::ptr::null(),
                    6,
                    (&number as *const f64).cast(),
                ))
            }
            .unwrap();
            assert_eq!(value.boolean(), expected);
            assert_eq!(value.number(), Some(number));
        }
    }

    #[test]
    fn actual_ax_permission_absence_is_a_recoverable_observation_error() {
        if trusted() {
            return;
        }
        let result = Session::default().capture(
            10,
            &ObservationScope::default(),
            Instant::now() + std::time::Duration::from_secs(1),
        );
        assert!(
            matches!(result, Err(AutomationError::Observation(message)) if message.contains("permission"))
        );
    }

    #[test]
    #[ignore = "requires a logged-in macOS desktop, Accessibility permission, and a foreground test application"]
    fn live_accessibility_observation_reports_semantics_without_document_values() {
        let snapshot = Session::default()
            .capture(
                200,
                &ObservationScope::default(),
                Instant::now() + std::time::Duration::from_secs(5),
            )
            .unwrap();
        assert!(!snapshot.app.id.is_empty());
        assert!(!snapshot.metadata.is_empty());
        assert!(snapshot
            .metadata
            .iter()
            .all(|meta| meta.node.short_value.is_none()));
        assert!(snapshot
            .metadata
            .iter()
            .filter(|meta| meta.node.secure)
            .all(|meta| meta.node.name.is_none() && meta.node.value_fingerprint.is_none()));
    }
}
