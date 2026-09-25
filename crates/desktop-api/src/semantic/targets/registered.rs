//! User-selected local applications supplement OS discovery. No model-facing
//! registration tool exists; paths come from the desktop host's file picker.
use super::Application;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn catalog_path() -> PathBuf {
    nuphus_data_dir().join("desktop-applications.json")
}

/// Shared Nuphus data directory. The main application registers applications from
/// its own file picker, so nuphus-mcp reads the same catalog file instead of
/// inventing a second, unpopulatable one.
fn nuphus_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("NUPHUS_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::data_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
        .join(".nuphus")
}

fn read(path: &Path) -> Result<Vec<PathBuf>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("读取已登记应用失败: {e}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(error) => Err(error.to_string()),
    }
}

fn save(path: &Path, selected: PathBuf) -> Result<(), String> {
    let _guard = WRITE_LOCK.lock().map_err(|_| "应用登记锁不可用")?;
    let mut entries = read(path)?;
    if entries.contains(&selected) {
        return Ok(());
    }
    entries.push(selected);
    let parent = path.parent().ok_or("应用登记目录不可用")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temporary = parent.join(format!(
        ".desktop-applications-{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let result = std::fs::write(
        &temporary,
        serde_json::to_vec_pretty(&entries).map_err(|e| e.to_string())?,
    )
    .and_then(|_| std::fs::rename(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|e| e.to_string())
}

fn resolve(path: &Path) -> Option<Application> {
    #[cfg(target_os = "windows")]
    {
        super::windows::selected(path)
    }
    #[cfg(target_os = "macos")]
    {
        super::macos::selected(path)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = path;
        None
    }
}

pub(super) fn applications() -> Vec<Application> {
    match read(&catalog_path()) {
        Ok(paths) => paths.iter().filter_map(|path| resolve(path)).collect(),
        Err(error) => {
            tracing::warn!("{error}");
            vec![]
        }
    }
}

pub(super) fn register(path: &Path) -> Result<serde_json::Value, String> {
    if !path.is_absolute() {
        return Err("请选择本地应用的绝对路径".into());
    }
    let selected = path.canonicalize().map_err(|e| e.to_string())?;
    // QueryFullProcessImageName uses normal DOS paths, not extended prefixes.
    #[cfg(target_os = "windows")]
    let selected = PathBuf::from(
        selected
            .to_string_lossy()
            .strip_prefix(r"\\?\")
            .unwrap_or(&selected.to_string_lossy()),
    );
    let app = resolve(&selected)
        .ok_or("请选择有效的 .exe/.lnk 或 macOS .app 应用，不接受脚本或命令行")?;
    save(&catalog_path(), selected)?;
    Ok(serde_json::json!({"name":app.name,"app_ref":super::app_reference(&app.app_id)}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registration_is_idempotent_and_does_not_rewrite_corrupt_catalogs() {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/catalog-tests")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("apps.json");
        let entry = PathBuf::from("selected-app.exe");
        save(&path, entry.clone()).unwrap();
        save(&path, entry.clone()).unwrap();
        assert_eq!(read(&path).unwrap(), vec![entry]);
        std::fs::write(&path, "invalid data").unwrap();
        assert!(save(&path, PathBuf::from("another.exe")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "invalid data");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
