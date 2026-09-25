use super::{hash, Application};
use std::path::{Path, PathBuf};
use windows::core::{Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegGetValueW, RegOpenKeyExW, HKEY,
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY,
    REG_SAM_FLAGS, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{
    AssocQueryStringW, IShellLinkW, ShellExecuteW, ShellLink, ASSOCF, ASSOCF_INIT_BYEXENAME,
    ASSOCF_NONE, ASSOCSTR, ASSOCSTR_EXECUTABLE, ASSOCSTR_FRIENDLYAPPNAME,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowTextLengthW, GetWindowTextW, SW_SHOWNORMAL,
};

pub(super) fn window_title(hwnd: i32) -> Option<String> {
    let hwnd = HWND(hwnd as isize);
    let length = unsafe { GetWindowTextLengthW(hwnd) }.max(0) as usize;
    let mut buffer = vec![0u16; length + 1];
    let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) }.max(0) as usize;
    Some(String::from_utf16_lossy(&buffer[..copied]))
}

fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().chain(Some(0)).collect()
}
fn string(value: &[u16]) -> String {
    String::from_utf16_lossy(&value[..value.iter().position(|c| *c == 0).unwrap_or(value.len())])
}
fn app(path: PathBuf, name: String, launch_path: Option<PathBuf>) -> Option<Application> {
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        return None;
    }
    let id = format!(
        "windows-app:{}",
        hash(&format!(
            "process|{}",
            path.to_string_lossy().to_lowercase()
        ))
    );
    Some(Application {
        app_id: id,
        name,
        launch_path,
    })
}

pub(super) fn running(pid: u32) -> Option<Application> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = vec![0_u16; 32768];
        let mut length = buffer.len() as u32;
        let read = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(process);
        read.ok()?;
        let path = PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]));
        let name = path.file_stem()?.to_string_lossy().into_owned();
        app(path, name, None)
    }
}

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn shortcut(path: &Path) -> Option<Application> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let persist: IPersistFile = link.cast().ok()?;
        let path_wide = wide(path.as_os_str());
        persist.Load(PCWSTR(path_wide.as_ptr()), STGM_READ).ok()?;
        let mut target = vec![0_u16; 32768];
        link.GetPath(&mut target, std::ptr::null_mut(), 0).ok()?;
        let target = PathBuf::from(string(&target));
        if !target.is_file() {
            return None;
        }
        app(
            target,
            path.file_stem()?.to_string_lossy().into_owned(),
            Some(path.to_owned()),
        )
    }
}

fn registry_key(root: HKEY, path: &str, view: REG_SAM_FLAGS) -> Option<Key> {
    let path = wide(std::ffi::OsStr::new(path));
    let mut key = HKEY::default();
    if unsafe { RegOpenKeyExW(root, PCWSTR(path.as_ptr()), 0, KEY_READ | view, &mut key) }
        == ERROR_SUCCESS
    {
        Some(Key(key))
    } else {
        None
    }
}

fn registry_string(key: HKEY, name: &str) -> Option<String> {
    let name = wide(std::ffi::OsStr::new(name));
    let mut value = vec![0u16; 32768];
    let mut bytes = (value.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            key,
            PCWSTR::null(),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
            None,
            Some(value.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    (status == ERROR_SUCCESS)
        .then(|| string(&value))
        .filter(|value| !value.trim().is_empty())
}

fn registry_value_names(key: HKEY) -> Vec<String> {
    let mut names = Vec::new();
    for index in 0..4096 {
        let mut value = vec![0u16; 32768];
        let mut length = value.len() as u32;
        let status = unsafe {
            RegEnumValueW(
                key,
                index,
                PWSTR(value.as_mut_ptr()),
                &mut length,
                None,
                None,
                None,
                None,
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            break;
        }
        if status == ERROR_SUCCESS {
            names.push(string(&value));
        }
    }
    names
}

fn association_value(name: &str, flags: ASSOCF, property: ASSOCSTR) -> Option<String> {
    let name = wide(std::ffi::OsStr::new(name));
    let mut value = vec![0u16; 32768];
    let mut length = value.len() as u32;
    let status = unsafe {
        AssocQueryStringW(
            flags,
            property,
            PCWSTR(name.as_ptr()),
            PCWSTR::null(),
            PWSTR(value.as_mut_ptr()),
            &mut length,
        )
    };
    status.ok().ok()?;
    let value = string(&value);
    (!value.trim().is_empty()).then_some(value)
}

fn registered_app(path: PathBuf, name: String) -> Option<Application> {
    // Query Windows for the associated executable, never infer launch commands
    // from icons, uninstall strings or model-supplied scripts.
    if !path.is_file() {
        return None;
    }
    registered_app_metadata(path, name)
}

fn registered_app_metadata(path: PathBuf, name: String) -> Option<Application> {
    if !path.is_absolute() {
        return None;
    }
    if path.file_name().is_some_and(|name| {
        [
            "applicationframehost.exe",
            "runtimebroker.exe",
            "rundll32.exe",
            "dllhost.exe",
        ]
        .iter()
        .any(|host| name.eq_ignore_ascii_case(host))
    }) {
        // These are activation hosts, not the registered application's own
        // entrypoint. Launching them without the real AppID would be misleading.
        return None;
    }
    let name = if name.trim().is_empty() || name.starts_with('@') {
        path.file_stem()?.to_string_lossy().into_owned()
    } else {
        name
    };
    app(path.clone(), name, Some(path))
}

fn association_app(
    identifier: &str,
    flags: ASSOCF,
    registered_name: Option<&str>,
) -> Option<Application> {
    let executable = association_value(identifier, flags, ASSOCSTR_EXECUTABLE)?;
    let name = registered_name
        .map(str::to_owned)
        .or_else(|| association_value(identifier, flags, ASSOCSTR_FRIENDLYAPPNAME))
        .unwrap_or_default();
    registered_app(PathBuf::from(executable), name)
}

fn registered_applications(root: HKEY, view: REG_SAM_FLAGS) -> Vec<Application> {
    let mut result = Vec::new();
    // Applications registered for Open With need not have a Start Menu shortcut
    // or an App Paths entry. Resolve their launch executable through Shell.
    if let Some(key) = registry_key(root, "Software\\Classes\\Applications", view) {
        for index in 0..4096 {
            let mut name = vec![0u16; 512];
            let mut length = name.len() as u32;
            let status = unsafe {
                RegEnumKeyExW(
                    key.0,
                    index,
                    PWSTR(name.as_mut_ptr()),
                    &mut length,
                    None,
                    PWSTR::null(),
                    None,
                    None,
                )
            };
            if status == ERROR_NO_MORE_ITEMS {
                break;
            }
            if status != ERROR_SUCCESS {
                continue;
            }
            let name = string(&name);
            if let Some(application) = association_app(&name, ASSOCF_INIT_BYEXENAME, None) {
                result.push(application);
            }
        }
    }
    // Windows Default Apps registration points to capabilities, whose file and
    // URL associations provide explicit ProgIDs. Do not execute registry text.
    if let Some(key) = registry_key(root, "Software\\RegisteredApplications", view) {
        for name in registry_value_names(key.0) {
            let Some(capabilities) = registry_string(key.0, &name) else {
                continue;
            };
            'associations: for category in ["FileAssociations", "URLAssociations"] {
                let Some(associations) =
                    registry_key(root, &format!("{capabilities}\\{category}"), view)
                else {
                    continue;
                };
                let mut seen = std::collections::HashSet::new();
                for extension in registry_value_names(associations.0) {
                    let Some(prog_id) = registry_string(associations.0, &extension) else {
                        continue;
                    };
                    if seen.insert(prog_id.clone()) {
                        if let Some(application) =
                            association_app(&prog_id, ASSOCF_NONE, Some(&name))
                        {
                            result.push(application);
                            // One application may register hundreds of file
                            // extensions. One native executable is sufficient.
                            break 'associations;
                        }
                    }
                }
            }
        }
    }
    result
}

pub(super) fn installed() -> Vec<Application> {
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let _apartment = initialized.then_some(Apartment);
    let mut result = Vec::new();
    if initialized {
        let roots = [std::env::var_os("APPDATA"), std::env::var_os("PROGRAMDATA")];
        let mut pending: Vec<_> = roots
            .into_iter()
            .flatten()
            .map(|p| {
                (
                    PathBuf::from(p).join("Microsoft/Windows/Start Menu/Programs"),
                    0,
                )
            })
            .collect();
        let mut visited = 0;
        while let Some((directory, depth)) = pending.pop() {
            if visited >= 4096 {
                break;
            }
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.flatten() {
                visited += 1;
                if visited > 4096 {
                    break;
                }
                let path = entry.path();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() && depth < 6 {
                    pending.push((path, depth + 1));
                } else if kind.is_file()
                    && path
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
                {
                    if let Some(app) = shortcut(&path) {
                        result.push(app);
                    }
                }
            }
        }
    }
    let key_path = wide(std::ffi::OsStr::new(
        "Software\\Microsoft\\Windows\\CurrentVersion\\App Paths",
    ));
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        for view in [KEY_WOW64_64KEY, KEY_WOW64_32KEY] {
            result.extend(registered_applications(root, view));
            let mut raw = HKEY::default();
            if unsafe {
                RegOpenKeyExW(
                    root,
                    PCWSTR(key_path.as_ptr()),
                    0,
                    KEY_READ | view,
                    &mut raw,
                )
            } != ERROR_SUCCESS
            {
                continue;
            }
            let key = Key(raw);
            for index in 0..2048 {
                let mut name = vec![0u16; 512];
                let mut length = name.len() as u32;
                if unsafe {
                    RegEnumKeyExW(
                        key.0,
                        index,
                        PWSTR(name.as_mut_ptr()),
                        &mut length,
                        None,
                        PWSTR::null(),
                        None,
                        None,
                    )
                } != ERROR_SUCCESS
                {
                    break;
                }
                let name = string(&name);
                let child = wide(std::ffi::OsStr::new(&name));
                let mut path = vec![0u16; 32768];
                let mut bytes = (path.len() * 2) as u32;
                if unsafe {
                    RegGetValueW(
                        key.0,
                        PCWSTR(child.as_ptr()),
                        PCWSTR::null(),
                        RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
                        None,
                        Some(path.as_mut_ptr().cast()),
                        Some(&mut bytes),
                    )
                } != ERROR_SUCCESS
                {
                    continue;
                }
                let path = PathBuf::from(string(&path).trim_matches('"'));
                if path.is_file() {
                    if let Some(app) = app(
                        path.clone(),
                        name.trim_end_matches(".exe").into(),
                        Some(path),
                    ) {
                        result.push(app);
                    }
                }
            }
        }
    }
    // Preserve the earlier native shortcut entry (including its registered
    // arguments) when another registry source describes the same executable.
    deduplicate_applications(&mut result);
    result
}

pub(super) fn selected(path: &Path) -> Option<Application> {
    if !path.is_absolute() || !path.is_file() {
        return None;
    }
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        return app(
            path.to_owned(),
            path.file_stem()?.to_string_lossy().into_owned(),
            Some(path.to_owned()),
        );
    }
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"))
    {
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let _apartment = initialized.then_some(Apartment);
        return shortcut(path);
    }
    None
}

fn deduplicate_applications(applications: &mut Vec<Application>) {
    let mut seen = std::collections::HashSet::new();
    applications.retain(|application| seen.insert(application.app_id.clone()));
}

pub(super) fn launch(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Err("Catalog launch entry no longer exists".into());
    }
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(|error| format!("Could not initialize native application launcher: {error}"))?;
    let _apartment = Apartment;
    let path = wide(path.as_os_str());
    let verb = wide(std::ffi::OsStr::new("open"));
    let result = unsafe {
        ShellExecuteW(
            HWND(0),
            PCWSTR(verb.as_ptr()),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 <= 32 {
        Err(format!("Native application launch failed ({})", result.0))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn application_id_uses_same_process_seed_as_uia() {
        let a = app(PathBuf::from("C:\\Apps\\Test.EXE"), "Test".into(), None).unwrap();
        assert_eq!(
            a.app_id,
            format!("windows-app:{}", hash("process|c:\\apps\\test.exe"))
        );
        assert!(app(PathBuf::from("C:\\Apps\\task.ps1"), "script".into(), None).is_none());
    }

    #[test]
    fn registered_app_uses_native_executable_identity_and_localized_name() {
        let path = PathBuf::from(r"C:\应用程序\Editor.EXE");
        let item = registered_app_metadata(path.clone(), "本地编辑器".into()).unwrap();
        assert_eq!(
            item.app_id,
            app(path.clone(), String::new(), None).unwrap().app_id
        );
        assert_eq!(item.name, "本地编辑器");
        assert_eq!(item.launch_path, Some(path));
        let indirect = registered_app_metadata(
            PathBuf::from(r"C:\Apps\Editor.exe"),
            "@resources.dll,-1".into(),
        )
        .unwrap();
        assert_eq!(indirect.name, "Editor");
    }

    #[test]
    fn registered_app_does_not_invent_executables_from_commands_or_activation_hosts() {
        for candidate in [
            r"Editor.exe",
            r"C:\Apps\task.ps1",
            r"C:\Apps\Editor.exe --open",
            r"C:\Windows\System32\rundll32.exe",
            r"C:\Windows\System32\ApplicationFrameHost.exe",
        ] {
            assert!(
                registered_app_metadata(PathBuf::from(candidate), "Registered app".into())
                    .is_none(),
                "{candidate}"
            );
        }
    }

    #[test]
    fn system_registry_duplicates_keep_original_native_shortcut_launch() {
        let executable = PathBuf::from(r"C:\Apps\Editor.exe");
        let shortcut = PathBuf::from(r"C:\Start Menu\Editor.lnk");
        let mut applications = vec![
            app(
                executable.clone(),
                "Editor shortcut".into(),
                Some(shortcut.clone()),
            )
            .unwrap(),
            registered_app_metadata(executable, "Registered Editor".into()).unwrap(),
            registered_app_metadata(PathBuf::from(r"C:\Apps\Other.exe"), "Other".into()).unwrap(),
        ];
        deduplicate_applications(&mut applications);
        assert_eq!(applications.len(), 2);
        assert_eq!(applications[0].launch_path, Some(shortcut));
        assert_eq!(applications[0].name, "Editor shortcut");
    }
}
