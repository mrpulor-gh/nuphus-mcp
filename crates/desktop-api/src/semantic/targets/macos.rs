use super::Application;
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};

type Ref = *const c_void;
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(value: Ref);
    fn CFGetTypeID(value: Ref) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFStringCreateWithBytes(
        allocator: Ref,
        bytes: *const u8,
        length: isize,
        encoding: u32,
        external: u8,
    ) -> Ref;
    fn CFStringGetCString(value: Ref, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFURLCreateFromFileSystemRepresentation(
        allocator: Ref,
        bytes: *const u8,
        length: isize,
        directory: u8,
    ) -> Ref;
    fn CFBundleCreate(allocator: Ref, url: Ref) -> Ref;
    fn CFBundleGetIdentifier(bundle: Ref) -> Ref;
    fn CFBundleGetValueForInfoDictionaryKey(bundle: Ref, key: Ref) -> Ref;
}

struct Owned(Ref);
impl Owned {
    fn new(raw: Ref) -> Option<Self> {
        (!raw.is_null()).then(|| Self(raw))
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn null_cf_object_is_not_released() {
        assert!(super::Owned::new(std::ptr::null()).is_none());
    }
}

fn text(raw: Ref) -> Option<String> {
    if raw.is_null() || unsafe { CFGetTypeID(raw) != CFStringGetTypeID() } {
        return None;
    }
    let mut bytes = vec![0_u8; 16384];
    if unsafe {
        CFStringGetCString(
            raw,
            bytes.as_mut_ptr().cast(),
            bytes.len() as isize,
            0x08000100,
        )
    } == 0
    {
        return None;
    }
    let end = bytes.iter().position(|v| *v == 0)?;
    let value = String::from_utf8(bytes[..end].to_vec()).ok()?;
    (!value.is_empty()).then_some(value)
}

fn bundle(path: &Path) -> Option<Application> {
    let bytes = path.as_os_str().as_encoded_bytes();
    let url = Owned::new(unsafe {
        CFURLCreateFromFileSystemRepresentation(
            std::ptr::null(),
            bytes.as_ptr(),
            bytes.len() as isize,
            1,
        )
    })?;
    let bundle = Owned::new(unsafe { CFBundleCreate(std::ptr::null(), url.0) })?;
    let app_id = text(unsafe { CFBundleGetIdentifier(bundle.0) })?;
    let property = |name: &str| -> Option<String> {
        let key = Owned::new(unsafe {
            CFStringCreateWithBytes(
                std::ptr::null(),
                name.as_ptr(),
                name.len() as isize,
                0x08000100,
                0,
            )
        })?;
        text(unsafe { CFBundleGetValueForInfoDictionaryKey(bundle.0, key.0) })
    };
    let name = property("CFBundleDisplayName")
        .or_else(|| property("CFBundleName"))
        .unwrap_or_else(|| {
            path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    Some(Application {
        app_id,
        name,
        launch_path: Some(path.to_owned()),
    })
}

pub(super) fn selected(path: &Path) -> Option<Application> {
    if path.is_dir()
        && path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
    {
        bundle(path)
    } else {
        None
    }
}

pub(super) fn running(pid: u32) -> Option<Application> {
    let identity = crate::semantic::macos_accessibility::native::application_identity(
        i32::try_from(pid).ok()?,
    )?;
    Some(Application {
        app_id: identity.id,
        name: identity.display_name,
        launch_path: None,
    })
}

pub(super) fn installed() -> Vec<Application> {
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
    ];
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Applications"));
    }
    let mut pending: Vec<_> = roots.into_iter().map(|p| (p, 0)).collect();
    let mut result = Vec::new();
    let mut visited = 0;
    while let Some((directory, depth)) = pending.pop() {
        if visited >= 2048 {
            break;
        }
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > 2048 {
                break;
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if !kind.is_dir() {
                continue;
            }
            if path.extension().is_some_and(|e| e == "app") {
                if let Some(app) = bundle(&path) {
                    result.push(app);
                }
            } else if depth < 3 {
                pending.push((path, depth + 1));
            }
        }
    }
    result
}

pub(super) fn launch(path: &Path) -> Result<(), String> {
    if !path.is_dir() || !path.extension().is_some_and(|e| e == "app") {
        return Err("Catalog application bundle is no longer available".into());
    }
    let status = std::process::Command::new("/usr/bin/open")
        .arg("-a")
        .arg(path)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("macOS failed to open the catalog application".into())
    }
}
