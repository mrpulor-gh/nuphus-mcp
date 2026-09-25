//! Registry-local desktop targets. Models select catalog references, never launch paths.
use super::{DeliveryMode, ObservationScope, SemanticLocator};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

#[cfg(target_os = "macos")]
mod macos;
mod registered;
#[cfg(target_os = "windows")]
mod windows;

/// Host-only entry point. It is deliberately absent from model tool schemas.
pub fn register_user_application(path: &std::path::Path) -> Result<Value, String> {
    registered::register(path)
}

#[derive(Clone, Debug)]
struct Application {
    app_id: String,
    name: String,
    launch_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct Window {
    hwnd: i32,
    pid: u32,
    app_id: String,
    title: String,
}

/// Persistent information is separate from the live token and native window handle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DesktopTargetDescriptor {
    pub app_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_ref: Option<String>,
}

#[derive(Clone)]
struct Binding {
    owner: String,
    window: Window,
    descriptor: DesktopTargetDescriptor,
}

const PAGE_BUDGET: usize = 6500;
const PAGE_ROWS: usize = 20;

#[derive(Clone)]
struct CatalogRow {
    reference: String,
    app: Application,
    window: Option<(String, Window)>,
    window_count: usize,
    is_self: bool,
}

struct CatalogSnapshot {
    rows: Vec<CatalogRow>,
    application_count: usize,
}

#[derive(Default)]
struct State {
    apps: BTreeMap<String, Application>,
    windows: HashMap<String, Window>,
    bindings: HashMap<String, Binding>,
    saved_windows: HashMap<(String, String, Option<String>, Option<String>), Window>,
    catalogs: HashMap<(String, String), CatalogSnapshot>,
}

/// Native window enumeration for the target catalog.
///
/// The main application reads this through its `DesktopClient`; in nuphus-mcp
/// every Win32 call belongs to desktop-api, so the catalog enumerates windows
/// through this crate's own platform layer instead.
struct DesktopWindowSource;

impl DesktopWindowSource {
    fn new() -> Self {
        Self
    }

    /// Window rows in the shape the catalog consumes: `hwnd`, `process_id` and —
    /// on Windows — the DWM cloaked flag (UWP shell windows stay visible while
    /// being cloaked). macOS rows come from the AX window service and carry a
    /// title instead of a cloaked flag.
    async fn windows_list(&self) -> Result<Value, String> {
        #[cfg(windows)]
        {
            let windows = crate::platform::WindowManager::new()
                .list_all()
                .map_err(|error| error.to_string())?;
            let rows: Vec<Value> = windows
                .into_iter()
                .map(|window| {
                    json!({
                        "hwnd": window.hwnd as i64,
                        "title": window.title,
                        "process_id": window.process_id,
                        "cloaked": is_window_cloaked(window.hwnd),
                    })
                })
                .collect();
            Ok(json!({"success": true, "result": rows}))
        }
        #[cfg(target_os = "macos")]
        {
            // The AX window service owns one blocking worker thread; keep the
            // catalog off the async runtime while it enumerates window servers.
            tokio::task::spawn_blocking(crate::platform::macos::windows_list)
                .await
                .map_err(|error| error.to_string())?
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            Err("Native desktop application catalog is unavailable on this platform".into())
        }
    }

    /// Bring a bound target window to the foreground, answering the same
    /// `{success, result:{foreground}}` envelope the catalog validates. The main
    /// application gets this from its DesktopClient; here it goes through this
    /// crate's own input engine and foreground probe on Windows, and through the
    /// AX window service on macOS.
    async fn window_activate(&self, hwnd: i32) -> Result<Value, String> {
        #[cfg(windows)]
        {
            let handle = hwnd as isize;
            let mut target = crate::Target::Window {
                hwnd: handle,
                title: String::new(),
                verified: false,
                gfx_backend: crate::GfxBackend::Unknown,
            };
            crate::input::InputEngine::new()
                .activate(&mut target)
                .await
                .map_err(|error| error.to_string())?;
            Ok(json!({
                "success": true,
                "result": {"foreground": crate::platform::foreground_hwnd() == Some(handle)},
            }))
        }
        #[cfg(target_os = "macos")]
        {
            // The AX service raises the window and only answers success once it
            // observes that handle focused, so its envelope already carries the
            // `result.foreground` flag the catalog validates.
            tokio::task::spawn_blocking(move || crate::platform::macos::window_activate(hwnd))
                .await
                .map_err(|error| error.to_string())?
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = hwnd;
            Err("Native desktop application catalog is unavailable on this platform".into())
        }
    }
}

/// UWP shell windows are visible to `IsWindowVisible` but cloaked by DWM; they
/// must never appear as selectable targets. Only the Windows window source
/// produces a `cloaked` column, so this helper is Windows-only.
#[cfg(windows)]
fn is_window_cloaked(hwnd: isize) -> bool {
    use ::windows::Win32::Foundation::HWND;
    use ::windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};

    let mut cloaked: u32 = 0;
    unsafe {
        let result = DwmGetWindowAttribute(
            HWND(hwnd),
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        );
        result.is_ok() && cloaked != 0
    }
}

/// One service belongs to one tool registry; it never shares tokens globally.
pub struct DesktopTargetService {
    desktop: DesktopWindowSource,
    state: Mutex<State>,
    interactive_owner: String,
}

impl Default for DesktopTargetService {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopTargetService {
    pub fn new() -> Self {
        Self {
            desktop: DesktopWindowSource::new(),
            state: Mutex::new(State::default()),
            interactive_owner: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// Execution owner of this catalog. The main application asks its automation
    /// gate for the owning task; nuphus-mcp already serializes every automation
    /// call through a process-wide lock plus a cross-process file lock, so one
    /// service instance is a single logical execution owner.
    fn owner(&self) -> String {
        self.interactive_owner.clone()
    }

    #[cfg(test)]
    fn set_owner(&mut self, owner: &str) {
        self.interactive_owner = owner.to_string();
    }

    async fn running(&self) -> Result<Vec<(Application, Window)>, String> {
        let response = self
            .desktop
            .windows_list()
            .await
            .map_err(|e| e.to_string())?;
        if response.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Window enumeration failed")
                .into());
        }
        let rows = response
            .get("result")
            .and_then(Value::as_array)
            .ok_or("Window enumeration returned an invalid response")?
            .clone();
        tokio::task::spawn_blocking(move || {
            rows.iter()
                .filter(|row| row.get("cloaked").and_then(Value::as_bool) != Some(true))
                .filter_map(running_window)
                .collect()
        })
        .await
        .map_err(|e| e.to_string())
    }

    pub async fn list(&self, query: Option<&str>, cursor: usize) -> Result<Value, String> {
        let query = query.unwrap_or_default().trim().to_lowercase();
        let key = (self.owner(), query.clone());
        if cursor != 0 {
            let state = self
                .state
                .lock()
                .map_err(|_| "Desktop target state unavailable")?;
            let snapshot = state.catalogs.get(&key).ok_or(
                "invalid_cursor: 目录快照已过期或 query/执行会话改变，请从 cursor=0 重新查询",
            )?;
            return catalog_page(snapshot, cursor);
        }
        let running = self.running().await?;
        let installed = tokio::task::spawn_blocking(installed_applications)
            .await
            .map_err(|e| e.to_string())?;
        let mut apps = BTreeMap::new();
        for app in installed {
            apps.entry(app.app_id.clone()).or_insert(app);
        }
        for (app, _) in &running {
            apps.entry(app.app_id.clone())
                .or_insert_with(|| app.clone());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Desktop target state unavailable")?;
        state.apps = apps
            .into_iter()
            .map(|(id, app)| (app_reference(&id), app))
            .collect();
        state.windows = running
            .into_iter()
            .map(|(_, w)| (window_reference(&w), w))
            .collect();
        let snapshot = catalog_snapshot(&state.apps, &state.windows, &query);
        let page = catalog_page(&snapshot, 0)?;
        if state.catalogs.len() >= 32 && !state.catalogs.contains_key(&key) {
            state.catalogs.clear();
        }
        state.catalogs.insert(key, snapshot);
        Ok(page)
    }

    /// Launch is possible only for a reference previously returned by the local catalog.
    /// Multiple windows are returned to the caller for explicit selection.
    pub async fn bind(&self, app_ref: &str, window_ref: Option<&str>) -> Result<Value, String> {
        self.bind_with_delivery(app_ref, window_ref, DeliveryMode::Foreground)
            .await
    }

    pub async fn bind_with_delivery(
        &self,
        app_ref: &str,
        window_ref: Option<&str>,
        delivery: DeliveryMode,
    ) -> Result<Value, String> {
        let app = self
            .state
            .lock()
            .map_err(|_| "Desktop target state unavailable")?
            .apps
            .get(app_ref)
            .cloned()
            .ok_or("Unknown app_ref; list desktop targets first")?;
        let selected = if let Some(reference) = window_ref {
            let state = self
                .state
                .lock()
                .map_err(|_| "Desktop target state unavailable")?;
            let window = state
                .windows
                .get(reference)
                .ok_or("Unknown window_ref; list desktop targets again")?;
            if window.app_id != app.app_id {
                return Err("window_ref does not belong to app_ref".into());
            }
            Some(window.clone())
        } else {
            None
        };
        let mut available: Vec<_> = self
            .running()
            .await?
            .into_iter()
            .map(|(_, w)| w)
            .filter(|w| w.app_id == app.app_id)
            .collect();
        if available.is_empty() && selected.is_none() {
            let path = app
                .launch_path
                .clone()
                .ok_or("The application is no longer running and has no catalog launch entry")?;
            tokio::task::spawn_blocking(move || launch(&path))
                .await
                .map_err(|e| e.to_string())??;
            for _ in 0..30 {
                tokio::time::sleep(Duration::from_millis(250)).await;
                available = self
                    .running()
                    .await?
                    .into_iter()
                    .map(|(_, w)| w)
                    .filter(|w| w.app_id == app.app_id)
                    .collect();
                if !available.is_empty() {
                    break;
                }
            }
        }
        let window = match choose_window(&available, selected.as_ref(), None)? {
            Some(window) => window.clone(),
            None => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| "Desktop target state unavailable")?;
                let windows: Vec<_> = available
                    .into_iter()
                    .map(|w| {
                        let reference = window_reference(&w);
                        let item = (reference.clone(), w.title.clone());
                        state.windows.insert(reference, w);
                        item
                    })
                    .collect();
                return window_selection_page(app_ref, windows);
            }
        };
        if delivery == DeliveryMode::Foreground {
            self.activate(&window).await?;
        }
        let token = format!("target:{}", uuid::Uuid::new_v4().simple());
        let descriptor = DesktopTargetDescriptor {
            app_id: app.app_id,
            window_title: Some(window.title.clone()),
            launch_ref: app.launch_path.as_ref().map(|_| app_ref.to_owned()),
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Desktop target state unavailable")?;
        // Bound registry memory; old tokens explicitly expire instead of aliasing a new target.
        if state.bindings.len() >= 256 {
            state.bindings.clear();
        }
        state.bindings.insert(
            token.clone(),
            Binding {
                owner: self.owner(),
                window,
                descriptor: descriptor.clone(),
            },
        );
        Ok(json!({"status":"bound", "target_token":token, "target": descriptor}))
    }

    async fn activate(&self, window: &Window) -> Result<(), String> {
        let matches = self
            .running()
            .await?
            .iter()
            .any(|(_, actual)| same_window(actual, window));
        if !matches {
            return Err("Bound target window is no longer available; bind again".into());
        }
        let result = self
            .desktop
            .window_activate(window.hwnd)
            .await
            .map_err(|e| e.to_string())?;
        if result.get("success").and_then(Value::as_bool) != Some(true)
            || result
                .pointer("/result/foreground")
                .and_then(Value::as_bool)
                != Some(true)
        {
            return Err("Could not bring the bound target window to the foreground".into());
        }
        Ok(())
    }

    pub async fn ensure(&self, token: &str) -> Result<DesktopTargetDescriptor, String> {
        let binding = self
            .state
            .lock()
            .map_err(|_| "Desktop target state unavailable")?
            .bindings
            .get(token)
            .cloned()
            .ok_or("Unknown or expired target_token; bind a desktop target first")?;
        if binding.owner != self.owner() {
            return Err("target_token belongs to another execution owner".into());
        }
        self.activate(&binding.window).await?;
        Ok(binding.descriptor)
    }

    pub async fn ensure_saved(
        &self,
        locator: &SemanticLocator,
        launch_ref: Option<&str>,
    ) -> Result<(), String> {
        self.ensure_saved_scope(locator, launch_ref, DeliveryMode::Foreground)
            .await
            .map(|_| ())
    }

    pub async fn ensure_saved_scope(
        &self,
        locator: &SemanticLocator,
        launch_ref: Option<&str>,
        delivery: DeliveryMode,
    ) -> Result<ObservationScope, String> {
        let key = (
            self.owner(),
            locator.app_id.clone(),
            locator.window_id.clone(),
            locator.window_title.clone(),
        );
        let mut windows: Vec<_> = self
            .running()
            .await?
            .into_iter()
            .map(|(_, w)| w)
            .filter(|w| w.app_id == locator.app_id)
            .collect();
        let bound = self
            .state
            .lock()
            .map_err(|_| "Desktop target state unavailable")?
            .saved_windows
            .get(&key)
            .cloned();
        if let Some(bound) = bound {
            if let Some(current) = windows.iter().find(|w| same_window(w, &bound)) {
                // A title may change after editing or saving. Within this
                // execution, retain the verified live window rather than
                // asking a model to invent clean/dirty title variants.
                // Native semantic validation still runs before any input.
                if delivery == DeliveryMode::Foreground {
                    self.activate(current).await?;
                }
                return Ok(window_scope(current, delivery));
            }
            self.state
                .lock()
                .map_err(|_| "Desktop target state unavailable")?
                .saved_windows
                .remove(&key);
        }
        if windows.is_empty() {
            let reference = launch_ref.ok_or(
                "Saved target application is not running; a catalog launch_ref is required",
            )?;
            // Re-discover the catalog after restart; never accept a path as a launch reference.
            let installed = tokio::task::spawn_blocking(installed_applications)
                .await
                .map_err(|e| e.to_string())?;
            let app = installed
                .into_iter()
                .find(|app| app_reference(&app.app_id) == reference && app.app_id == locator.app_id)
                .ok_or("Saved launch_ref is no longer present in the local application catalog")?;
            let path = app
                .launch_path
                .ok_or("Application has no native launch entry")?;
            tokio::task::spawn_blocking(move || launch(&path))
                .await
                .map_err(|e| e.to_string())??;
            for _ in 0..30 {
                tokio::time::sleep(Duration::from_millis(250)).await;
                windows = self
                    .running()
                    .await?
                    .into_iter()
                    .map(|(_, w)| w)
                    .filter(|w| w.app_id == locator.app_id)
                    .collect();
                if !windows.is_empty() {
                    break;
                }
            }
        }
        let window = choose_saved_window(&windows, locator)?
            .ok_or("Saved target matches multiple windows; select a window before replay")?;
        if delivery == DeliveryMode::Foreground {
            self.activate(window).await?;
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Desktop target state unavailable")?;
        if state.saved_windows.len() >= 256 {
            state.saved_windows.clear();
        }
        state.saved_windows.insert(key, window.clone());
        Ok(window_scope(window, delivery))
    }

    pub async fn bound_scope(
        &self,
        token: &str,
        delivery: DeliveryMode,
    ) -> Result<(ObservationScope, Option<String>), String> {
        let binding = self
            .state
            .lock()
            .map_err(|_| "Desktop target state unavailable")?
            .bindings
            .get(token)
            .cloned()
            .ok_or("Unknown target_token")?;
        if binding.owner != self.owner() {
            return Err("target_token belongs to another execution owner".into());
        }
        if !self
            .running()
            .await?
            .iter()
            .any(|(_, actual)| same_window(actual, &binding.window))
        {
            return Err("Bound window no longer exists; bind again".into());
        }
        if delivery == DeliveryMode::Foreground {
            self.activate(&binding.window).await?;
        }
        Ok((
            window_scope(&binding.window, delivery),
            binding.descriptor.launch_ref,
        ))
    }

    /// Read-only verification resolution. Never starts or activates an app.
    pub async fn verification_scope(
        &self,
        locator: &SemanticLocator,
    ) -> Result<Option<ObservationScope>, String> {
        let windows: Vec<_> = self
            .running()
            .await?
            .into_iter()
            .map(|(_, window)| window)
            .filter(|w| w.app_id == locator.app_id)
            .collect();
        if windows.is_empty() {
            return Ok(None);
        }
        // Titles may be redacted or change after saving. A missing title match
        // does not prove a stable window ID disappeared; the observer checks it.
        if locator.window_id.is_some() && windows.len() == 1 {
            return Ok(Some(window_scope(&windows[0], DeliveryMode::Auto)));
        }
        let selected = if let Some(title) = &locator.window_title {
            let matches: Vec<_> = windows.iter().filter(|w| &w.title == title).collect();
            match matches.as_slice() {
                [] if locator.window_id.is_some() => {
                    return Err("stable verification window could not be resolved by title".into())
                }
                [] => return Ok(None),
                [one] => *one,
                _ => return Err("verification window is ambiguous".into()),
            }
        } else {
            match windows.as_slice() {
                [one] => one,
                _ => return Err("verification requires an exact window".into()),
            }
        };
        Ok(Some(window_scope(selected, DeliveryMode::Auto)))
    }
}

fn window_scope(window: &Window, delivery: DeliveryMode) -> ObservationScope {
    ObservationScope {
        app_id: Some(window.app_id.clone()),
        window_handle: Some(window.hwnd),
        delivery,
        ..Default::default()
    }
}

fn choose_saved_window<'a>(
    windows: &'a [Window],
    locator: &SemanticLocator,
) -> Result<Option<&'a Window>, String> {
    // Public AX titles are redacted and may be shortened. With exactly one
    // app window and a stable identity, let the native adapter check that
    // identity before any action instead of comparing a redacted display label.
    // Never use this fallback to choose among multiple documents.
    if locator
        .window_id
        .as_deref()
        .is_some_and(|id| id.starts_with("axw:"))
        && windows.len() == 1
    {
        return Ok(windows.first());
    }
    choose_window(windows, None, locator.window_title.as_deref())
}

fn display_text(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn window_selection_page(
    app_ref: &str,
    mut windows: Vec<(String, String)>,
) -> Result<Value, String> {
    windows.sort_by(|a, b| {
        a.1.to_lowercase()
            .cmp(&b.1.to_lowercase())
            .then(a.1.cmp(&b.1))
            .then(a.0.cmp(&b.0))
    });
    let count = windows.len();
    let response = |page: &Vec<Value>| {
        json!({"status":"needs_window_selection","app_ref":app_ref,"windows":page,
        "window_count":count,"has_more_windows":count>page.len(),"list_query":app_ref,
        "hint":"使用 desktop_targets_list 的 query=list_query、cursor=0 查询全部窗口；后续沿 next_cursor 翻页"})
    };
    let mut page = Vec::new();
    for (reference, title) in windows.into_iter().take(PAGE_ROWS) {
        page.push(json!({"window_ref":reference,"title":display_text(&title,120)}));
        if serde_json::to_vec(&response(&page))
            .map_err(|e| e.to_string())?
            .len()
            > PAGE_BUDGET
        {
            page.pop();
            break;
        }
    }
    Ok(response(&page))
}

fn catalog_snapshot(
    apps: &BTreeMap<String, Application>,
    windows: &HashMap<String, Window>,
    query: &str,
) -> CatalogSnapshot {
    let mut rows = Vec::new();
    let mut application_count = 0;
    for (reference, app) in apps {
        let mut windows: Vec<_> = windows
            .iter()
            .filter(|(_, w)| w.app_id == app.app_id)
            .collect();
        windows.sort_by(|(a_ref, a), (b_ref, b)| {
            a.title
                .to_lowercase()
                .cmp(&b.title.to_lowercase())
                .then(a.title.cmp(&b.title))
                .then(a_ref.cmp(b_ref))
        });
        let window_count = windows.len();
        let is_self = windows.iter().any(|(_, w)| w.pid == std::process::id());
        let app_matches = query.is_empty()
            || reference == query
            || app.name.to_lowercase().contains(query)
            || app.app_id.to_lowercase().contains(query);
        // Match full local titles before shortening presentation text.
        let matching: Vec<_> = windows
            .into_iter()
            .filter(|(_, w)| app_matches || w.title.to_lowercase().contains(query))
            .collect();
        if matching.is_empty() && !(app_matches && window_count == 0) {
            continue;
        }
        application_count += 1;
        let base = CatalogRow {
            reference: reference.clone(),
            app: app.clone(),
            window: None,
            window_count,
            is_self,
        };
        if matching.is_empty() {
            rows.push(base);
        } else {
            rows.extend(matching.into_iter().map(|(reference, window)| CatalogRow {
                window: Some((reference.clone(), window.clone())),
                ..base.clone()
            }));
        }
    }
    CatalogSnapshot {
        rows,
        application_count,
    }
}

fn catalog_page(snapshot: &CatalogSnapshot, cursor: usize) -> Result<Value, String> {
    if cursor > snapshot.rows.len() {
        return Err("invalid_cursor: 目标分页位置无效".into());
    }
    let response = |applications: &Vec<Value>, count: usize| {
        json!({"applications":applications,"application_count":snapshot.application_count,
        "target_count":snapshot.rows.len(),"cursor":cursor,"next_cursor":(cursor+count<snapshot.rows.len()).then_some(cursor+count)})
    };
    let mut applications: Vec<Value> = Vec::new();
    let mut count = 0;
    for row in snapshot.rows.iter().skip(cursor).take(PAGE_ROWS) {
        let mut proposed = applications.clone();
        if proposed.last().and_then(|app| app["app_ref"].as_str()) != Some(row.reference.as_str()) {
            proposed.push(json!({"app_ref":row.reference,
                "app_id":(row.app.app_id.chars().count()<=256).then_some(&row.app.app_id),
                "name":display_text(&row.app.name,96),"is_self":row.is_self,"launch_ref":row.app.launch_path.as_ref().map(|_|&row.reference),
                "running":row.window_count>0,"window_count":row.window_count,"windows":[]}));
        }
        if let Some((reference, window)) = &row.window {
            proposed.last_mut().expect("page app exists")["windows"]
                .as_array_mut()
                .expect("windows array")
                .push(json!({"window_ref":reference,"title":display_text(&window.title,120)}));
        }
        let candidate = response(&proposed, count + 1);
        if serde_json::to_vec(&candidate)
            .map_err(|e| e.to_string())?
            .len()
            > PAGE_BUDGET
        {
            if count == 0 {
                return Err("Target catalog entry exceeds the presentation budget".into());
            }
            break;
        }
        applications = proposed;
        count += 1;
    }
    Ok(response(&applications, count))
}

fn choose_window<'a>(
    windows: &'a [Window],
    selected: Option<&Window>,
    title: Option<&str>,
) -> Result<Option<&'a Window>, String> {
    if let Some(selected) = selected {
        return windows
            .iter()
            .find(|window| same_window(window, selected))
            .map(Some)
            .ok_or_else(|| "Selected window disappeared; list targets again".into());
    }
    let candidates: Vec<_> = windows
        .iter()
        .filter(|w| title.is_none_or(|title| w.title == title))
        .collect();
    match candidates.as_slice() {
        [window] => Ok(Some(*window)),
        [] => Err("No window matches the selected application and saved title".into()),
        _ => Ok(None),
    }
}

fn same_window(a: &Window, b: &Window) -> bool {
    a.hwnd == b.hwnd && a.pid == b.pid && a.app_id == b.app_id
}
fn hash(value: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut h);
    format!("{:016x}", h.finish())
}
fn app_reference(id: &str) -> String {
    format!("app:{}", hash(id))
}
fn window_reference(w: &Window) -> String {
    format!(
        "window:{}",
        hash(&format!("{}|{}|{}", w.app_id, w.pid, w.hwnd))
    )
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn running_window(row: &Value) -> Option<(Application, Window)> {
    let hwnd = i32::try_from(row.get("hwnd")?.as_i64()?).ok()?;
    let pid = u32::try_from(row.get("process_id")?.as_u64()?).ok()?;
    #[cfg(target_os = "windows")]
    let title = windows::window_title(hwnd)?;
    #[cfg(target_os = "macos")]
    let title = row.get("title")?.as_str()?.to_owned();
    #[cfg(target_os = "windows")]
    let app = windows::running(pid)?;
    #[cfg(target_os = "macos")]
    let app = macos::running(pid)?;
    Some((
        app.clone(),
        Window {
            hwnd,
            pid,
            app_id: app.app_id,
            title,
        },
    ))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn running_window(_: &Value) -> Option<(Application, Window)> {
    None
}

fn installed_applications() -> Vec<Application> {
    let result = registered::applications();
    #[cfg(target_os = "windows")]
    let result = result.into_iter().chain(windows::installed()).collect();
    #[cfg(target_os = "macos")]
    let result = result.into_iter().chain(macos::installed()).collect();
    result
}

fn launch(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        windows::launch(path)
    }
    #[cfg(target_os = "macos")]
    {
        macos::launch(path)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = path;
        Err("Native desktop application catalog is unavailable on this platform".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn window(hwnd: i32, title: &str) -> Window {
        Window {
            hwnd,
            pid: 42,
            app_id: "app.test".into(),
            title: title.into(),
        }
    }
    #[test]
    fn repeated_windows_require_explicit_selection() {
        let windows = [window(1, "Report"), window(2, "Report")];
        assert!(choose_window(&windows, None, Some("Report"))
            .unwrap()
            .is_none());
        assert_eq!(
            choose_window(&windows, Some(&windows[1]), None)
                .unwrap()
                .unwrap()
                .hwnd,
            2
        );
        assert!(choose_window(&windows, Some(&window(3, "Report")), None).is_err());
    }
    #[test]
    fn saved_title_is_not_a_hint_to_choose_an_unrelated_window() {
        let windows = [window(1, "Unrelated")];
        assert!(choose_window(&windows, None, Some("Report")).is_err());
        assert_eq!(
            choose_window(&windows, None, None).unwrap().unwrap().hwnd,
            1
        );
    }
    #[test]
    fn live_window_binding_survives_title_changes_but_not_window_replacement() {
        let original = window(10, "Document");
        let dirty = window(10, "*Document");
        assert!(same_window(&original, &dirty));
        assert!(!same_window(&original, &window(11, "*Document")));
        let mut replaced = dirty;
        replaced.pid += 1;
        assert!(!same_window(&original, &replaced));
        let mut state = State::default();
        let key = (
            "task-a".to_string(),
            "app.test".to_string(),
            None,
            Some("Document".to_string()),
        );
        state.saved_windows.insert(key.clone(), original);
        let mut other_task = key;
        other_task.0 = "task-b".into();
        assert!(!state.saved_windows.contains_key(&other_task));
    }
    #[test]
    fn stable_identity_allows_native_validation_of_a_redacted_single_window() {
        let mut locator: SemanticLocator = serde_json::from_value(json!({
            "app_id": "app.test", "window_id": "axw:stable", "window_title": "redacted control"
        }))
        .unwrap();
        let windows = [window(1, "Private long title: user@example.invalid")];
        assert_eq!(
            choose_saved_window(&windows, &locator)
                .unwrap()
                .unwrap()
                .hwnd,
            1
        );
        let multiple = [windows[0].clone(), window(2, "Another document")];
        assert!(choose_saved_window(&multiple, &locator).is_err());
        locator.window_id = Some("windows-window:shared-class".into());
        assert!(choose_saved_window(&windows, &locator).is_err());
        locator.window_id = None;
        assert!(choose_saved_window(&windows, &locator).is_err());
    }
    #[test]
    fn catalog_references_are_not_launch_paths_and_window_references_are_distinct() {
        assert_eq!(app_reference("app.test"), app_reference("app.test"));
        assert_ne!(
            window_reference(&window(1, "Report")),
            window_reference(&window(2, "Report"))
        );
        let descriptor = DesktopTargetDescriptor {
            app_id: "app.test".into(),
            window_title: Some("Report".into()),
            launch_ref: Some(app_reference("app.test")),
        };
        let json = serde_json::to_string(&descriptor).unwrap();
        assert!(!json.contains("hwnd") && !json.contains("pid") && !json.contains("target_token"));
    }

    #[tokio::test]
    async fn tokens_cannot_cross_execution_owners_or_be_invented() {
        let mut service = DesktopTargetService::new();
        service.state.lock().unwrap().bindings.insert(
            "target:test".into(),
            Binding {
                owner: "owner:first".into(),
                window: window(1, "Report"),
                descriptor: DesktopTargetDescriptor {
                    app_id: "app.test".into(),
                    window_title: Some("Report".into()),
                    launch_ref: None,
                },
            },
        );
        service.set_owner("owner:second");
        let error = service.ensure("target:test").await.unwrap_err();
        assert!(error.contains("another execution owner"));
        assert!(service
            .ensure("target:invented")
            .await
            .unwrap_err()
            .contains("Unknown"));
        assert!(service
            .bind("C:\\arbitrary.exe", None)
            .await
            .unwrap_err()
            .contains("Unknown app_ref"));
    }

    fn catalog_fixture(count: usize) -> (BTreeMap<String, Application>, HashMap<String, Window>) {
        let app = Application {
            app_id: "app.test".into(),
            name: "Editor".into(),
            launch_path: None,
        };
        let apps = [(app_reference(&app.app_id), app)].into();
        let windows = (0..count)
            .map(|n| {
                // Include escaped controls and multi-byte text to exercise serialized-byte budgets.
                let window = window(
                    n as i32 + 1,
                    &format!("Document {n:04} {}", "\u{0001}中🖥".repeat(100)),
                );
                (window_reference(&window), window)
            })
            .collect();
        (apps, windows)
    }

    #[test]
    fn single_application_with_many_windows_is_bounded_and_fully_pageable() {
        let (apps, windows) = catalog_fixture(500);
        let snapshot = catalog_snapshot(&apps, &windows, "");
        let mut cursor = 0;
        let mut references = std::collections::HashSet::new();
        loop {
            let page = catalog_page(&snapshot, cursor).unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() <= PAGE_BUDGET);
            assert_eq!(page["application_count"], 1);
            assert_eq!(page["target_count"], 500);
            for app in page["applications"].as_array().unwrap() {
                for window in app["windows"].as_array().unwrap() {
                    assert!(references.insert(window["window_ref"].as_str().unwrap().to_owned()));
                }
            }
            let Some(next) = page["next_cursor"].as_u64() else {
                break;
            };
            assert!(next as usize > cursor);
            cursor = next as usize;
        }
        assert_eq!(references.len(), 500);
        assert!(catalog_page(&snapshot, 501).is_err());
    }

    #[test]
    fn title_search_uses_unshortened_text_and_sort_does_not_follow_hashmap_order() {
        let (apps, mut windows) = catalog_fixture(30);
        let long = window(999, &format!("{}needle at end", "long ".repeat(100)));
        let wanted = window_reference(&long);
        windows.insert(wanted.clone(), long);
        let filtered = catalog_snapshot(&apps, &windows, "needle at end");
        assert_eq!(filtered.rows.len(), 1);
        assert_eq!(filtered.rows[0].window.as_ref().unwrap().0, wanted);
        let mut reversed = HashMap::new();
        let mut entries: Vec<_> = windows.iter().collect();
        entries.sort_by(|a, b| b.0.cmp(a.0));
        for (reference, window) in entries {
            reversed.insert(reference.clone(), window.clone());
        }
        assert_eq!(
            catalog_page(&catalog_snapshot(&apps, &windows, ""), 0).unwrap(),
            catalog_page(&catalog_snapshot(&apps, &reversed, ""), 0).unwrap()
        );
        assert_eq!(
            catalog_snapshot(&apps, &windows, &app_reference("app.test"))
                .rows
                .len(),
            31
        );
    }

    #[test]
    fn multiwindow_bind_choices_fit_budget_and_link_to_full_catalog() {
        let choices = (0..500)
            .map(|n| (format!("window:{n:016x}"), "\u{0001}".repeat(1000)))
            .collect();
        let page = window_selection_page("app:123", choices).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= PAGE_BUDGET);
        assert_eq!(page["window_count"], 500);
        assert_eq!(page["has_more_windows"], true);
        assert_eq!(page["list_query"], "app:123");
        assert!(!page["windows"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn subsequent_pages_use_owner_query_snapshot_without_native_enumeration() {
        let mut service = DesktopTargetService::new();
        let (apps, windows) = catalog_fixture(30);
        service.state.lock().unwrap().catalogs.insert(
            (service.owner(), "editor".into()),
            catalog_snapshot(&apps, &windows, "editor"),
        );
        assert_eq!(
            service.list(Some("Editor"), 20).await.unwrap()["cursor"],
            20
        );
        assert!(service
            .list(Some("Changed query"), 20)
            .await
            .unwrap_err()
            .contains("invalid_cursor"));
        service.set_owner("other owner");
        let result = service.list(Some("Editor"), 20).await;
        assert!(result.unwrap_err().contains("invalid_cursor"));
    }
}
