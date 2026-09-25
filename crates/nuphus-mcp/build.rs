//! Nuphus MCP build script — ensures the ONNX Runtime native library sits next to the exe.
//!
//! `desktop-api` uses ort with `load-dynamic`: at runtime it loads `onnxruntime` by name from
//! the exe directory → `PATH`/`system32` (Windows) or `LD_LIBRARY_PATH`/`DYLD_LIBRARY_PATH`.
//! If the correct library is missing, Windows silently picks up a stale `onnxruntime.dll`
//! (e.g. a 2019 v1.0 in `C:\Windows\System32`) and PaddleOCR session creation hangs forever.
//!
//! To keep the repo free of large binaries, this script downloads Microsoft's official
//! all-platform nupkg from NuGet once (first build only) and extracts the native library for
//! the current target next to the exe. Failures only print a warning — the build still succeeds
//! (the user can place the DLL manually, like the main repo's build.rs).

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// NuGet package + exact version to fetch (must match the ORT API version loaded at runtime).
const NUGET_URL: &str = "https://www.nuget.org/api/v2/package/Microsoft.ML.OnnxRuntime/1.27.0";
/// SHA-256 of the nupkg above (verified 2026-08-01). Used to detect a truncated download.
const NUPKG_SHA256: &str = "c725e8cefdfe4de08befb9f79b3f218c162439cdcdb97cbbb5192888d58ea669";
/// Skip download entirely (e.g. offline CI that vendored the DLL itself).
const SKIP_ENV: &str = "NUPHUS_MCP_NO_ORT_DOWNLOAD";
/// A valid onnxruntime native library is at least this many bytes; smaller ones are stale.
const MIN_VALID_SIZE: u64 = 4 * 1024 * 1024;

/// (NuGet RID subdir, native library filenames — [main lib, optional provider-shared lib])
struct NativeLib {
    rid: &'static str,
    files: &'static [&'static str],
}

/// Minimum valid size per file. The main ORT lib is tens of MB (stale shims are ~2 MB),
/// but `onnxruntime_providers_shared.dll` is genuinely tiny (~21 KB) — only require non-zero.
fn min_valid_size(file: &str) -> u64 {
    if file.ends_with("providers_shared.dll")
        || file.ends_with("providers_shared.so")
        || file.ends_with("providers_shared.dylib")
    {
        1
    } else {
        MIN_VALID_SIZE
    }
}

fn main() {
    if env::var_os(SKIP_ENV).is_some() {
        return;
    }

    let target = match env::var("TARGET") {
        Ok(t) => t,
        Err(_) => return,
    };

    // Map the cargo target triple → (NuGet RID, native libs). osx-x64 / win-32 are not
    // published by ORT ≥1.19, so they fall through to the "unsupported" warning below.
    let lib = match target.as_str() {
        t if t.starts_with("x86_64-pc-windows") => Some(NativeLib {
            rid: "win-x64",
            files: &["onnxruntime.dll", "onnxruntime_providers_shared.dll"],
        }),
        t if t.starts_with("aarch64-pc-windows") => Some(NativeLib {
            rid: "win-arm64",
            files: &["onnxruntime.dll", "onnxruntime_providers_shared.dll"],
        }),
        t if t.starts_with("x86_64-unknown-linux") => Some(NativeLib {
            rid: "linux-x64",
            files: &["libonnxruntime.so", "libonnxruntime_providers_shared.so"],
        }),
        t if t.starts_with("aarch64-unknown-linux") => Some(NativeLib {
            rid: "linux-arm64",
            files: &["libonnxruntime.so", "libonnxruntime_providers_shared.so"],
        }),
        t if t.starts_with("aarch64-apple-darwin") => Some(NativeLib {
            rid: "osx-arm64",
            files: &["libonnxruntime.dylib"], // osx nupkg ships no provider-shared lib
        }),
        _ => None,
    };
    let Some(lib) = lib else {
        println!(
            "cargo:warning=ONNX Runtime: target `{target}` has no prebuilt ORT library in the NuGet package (win-x64/win-arm64/linux-x64/linux-arm64/osx-arm64 only)."
        );
        println!("cargo:warning=  Place onnxruntime manually next to the exe, or set {SKIP_ENV} to silence this.");
        return;
    };

    // OUT_DIR = target/<profile>/build/<crate>-<hash>/out → walk up 3 levels to <profile>/
    let profile_dir = match env::var("OUT_DIR") {
        Ok(o) => {
            let p = PathBuf::from(&o);
            let build_dir = p
                .parent() // build/<hash>
                .and_then(|d| d.parent()) // build/
                .and_then(|d| d.parent()); // <profile>/
            build_dir.map(Path::to_path_buf)
        }
        Err(_) => None,
    };
    let Some(profile_dir) = profile_dir else {
        return;
    };

    // Skip when every requested lib is already present (user-placed or previous build).
    let all_present = lib
        .files
        .iter()
        .all(|f| valid_lib_exists(&profile_dir.join(f), min_valid_size(f)));
    if all_present {
        return;
    }

    let nupkg_cache = profile_dir.join(".nuphus-onnxruntime-1.27.0.nupkg");
    let nupkg = match ensure_nupkg(&nupkg_cache) {
        Some(n) => n,
        None => {
            println!("cargo:warning=ONNX Runtime: could not download the all-platform package; PaddleOCR/YOLO will be unavailable at runtime.");
            println!(
                "cargo:warning=  Place `{}` manually next to the exe, or re-run the build with network access.",
                lib.files[0]
            );
            return;
        }
    };

    // Extract runtimes/<rid>/native/<file> for each file into <profile>/ (next to the exe).
    for file in lib.files {
        let dest = profile_dir.join(file);
        match extract_native(&nupkg, lib.rid, file, &dest) {
            Ok(()) => println!(
                "cargo:warning=ONNX Runtime: {file} ({rid}) → {dest}",
                rid = lib.rid,
                dest = dest.display()
            ),
            Err(e) => {
                println!("cargo:warning=ONNX Runtime: extraction failed for `{file}`: {e}; PaddleOCR/YOLO may be unavailable at runtime.");
                println!("cargo:warning=  Place `{}` manually next to the exe.", file);
            }
        }
    }
}

/// True when `dest` exists and is at least `min_size` bytes (not a stale shim).
fn valid_lib_exists(dest: &Path, min_size: u64) -> bool {
    dest.metadata()
        .map(|m| m.len() >= min_size)
        .unwrap_or(false)
}

/// Download the all-platform nupkg once, verifying its SHA-256. Returns the cached path.
fn ensure_nupkg(cache: &Path) -> Option<PathBuf> {
    if cache.exists() && file_size(cache).unwrap_or(0) > 100_000_000 {
        return Some(cache.to_path_buf());
    }
    let _ = std::fs::remove_file(cache);
    if !download(NUGET_URL, cache) {
        return None;
    }
    // Verify hash (catches truncated downloads / captive-portal HTML pages).
    match sha256(cache) {
        Some(h) if h != NUPKG_SHA256 => {
            println!(
                "cargo:warning=ONNX Runtime: downloaded nupkg hash mismatch (got {h}); discarding."
            );
            let _ = std::fs::remove_file(cache);
            None
        }
        _ => Some(cache.to_path_buf()),
    }
}

/// Extract a single file from the nupkg into `dest`.
fn extract_native(nupkg: &Path, rid: &str, file: &str, dest: &Path) -> Result<(), String> {
    let entry = format!("runtimes/{rid}/native/{file}");
    let out_dir = dest
        .parent()
        .ok_or_else(|| "no parent dir for dest".to_string())?;
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;

    let ok = if cfg!(windows) {
        // Windows ships bsdtar as System32\tar.exe (supports zip). Git Bash's GNU tar doesn't.
        run_tar(&entry, nupkg, out_dir).is_ok()
    } else {
        // macOS ships bsdtar; Linux ships GNU tar (zip-unsupported) → fall back to `unzip`.
        run_tar(&entry, nupkg, out_dir)
            .or_else(|_| run_unzip(&entry, nupkg, out_dir))
            .is_ok()
    };
    if !ok {
        return Err(format!(
            "could not extract `{entry}` from {}",
            nupkg.display()
        ));
    }

    let extracted = out_dir.join(&entry);
    if !extracted.exists() {
        return Err(format!("`{entry}` not present in {}", nupkg.display()));
    }
    std::fs::copy(&extracted, dest).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(windows)]
fn tar_cmd() -> Command {
    // Prefer System32 bsdtar (zip-capable); fall back to plain `tar` in PATH.
    let sys32 = env::var("SystemRoot")
        .map(|r| PathBuf::from(r).join("System32").join("tar.exe"))
        .unwrap_or_default();
    if sys32.exists() {
        Command::new(sys32)
    } else {
        Command::new("tar")
    }
}
#[cfg(not(windows))]
fn tar_cmd() -> Command {
    Command::new("tar")
}

fn run_tar(entry: &str, nupkg: &Path, out_dir: &Path) -> Result<(), ()> {
    tar_cmd()
        .arg("-xf")
        .arg(nupkg)
        .arg("-C")
        .arg(out_dir)
        .arg(entry)
        .output()
        .map(|o| if o.status.success() { Ok(()) } else { Err(()) })
        .unwrap_or(Err(()))
}

fn run_unzip(entry: &str, nupkg: &Path, out_dir: &Path) -> Result<(), ()> {
    Command::new("unzip")
        .arg("-o")
        .arg(nupkg)
        .arg(entry)
        .arg("-d")
        .arg(out_dir)
        .output()
        .map(|o| if o.status.success() { Ok(()) } else { Err(()) })
        .unwrap_or(Err(()))
}

fn download(url: &str, dest: &Path) -> bool {
    // Prefer curl (present on Windows 10 1803+); fall back to PowerShell.
    let ok = Command::new("curl")
        .args(["-fSL", "--max-time", "600", "-o"])
        .arg(dest)
        .arg(url)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        return true;
    }
    let ps = format!(
        "[Net.ServicePointManager]::SecurityProtocol='Tls12'; Invoke-WebRequest -Uri '{url}' -OutFile '{}' -UseBasicParsing",
        dest.display()
    );
    Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn file_size(p: &Path) -> Option<u64> {
    p.metadata().ok().map(|m| m.len())
}

fn sha256(p: &Path) -> Option<String> {
    let out = Command::new("certutil")
        .args(["-hashfile"])
        .arg(p)
        .arg("SHA256")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // certutil prints locale-dependent header/footer (GBK on zh-CN systems breaks UTF-8
    // lossy decoding). Extract only the first run of exactly 64 hex chars instead.
    let raw = String::from_utf8_lossy(&out.stdout);
    let mut run = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_hexdigit() {
            run.push(ch);
            if run.len() == 64 {
                return Some(run.to_ascii_lowercase());
            }
        } else {
            run.clear();
        }
    }
    None
}
