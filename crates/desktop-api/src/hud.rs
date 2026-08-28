//! hud — 工具调用执行提示（非侵入悬浮层）
//!
//! 设计约束（大王定调，2026-08-28）：
//! - 桌面操作的目标大多是**其它应用的窗口**，且��个 agent 并行共用一个桌面——
//!   绝不允许用「激活窗口」做执行提示（焦点是用户与其它 agent 的领地）
//! - HUD 是唯一的实时可见性通道：屏幕右下角小浮条，显示「正在执行什么」
//! - 非侵入四件套：`WS_EX_NOACTIVATE`（永不抢焦点）+ `WS_EX_TRANSPARENT`
//!   （鼠标穿透）+ `WS_EX_TOOLWINDOW`（不进任务栏/Alt-Tab）+ `SW_SHOWNOACTIVATE`
//! - Windows 实现；其它平台 no-op（与 desktop-api 平台策略一致）

/// 工具执行中浮条驻留时长（防工具 hang 留残影的上限；正常会被完成态覆盖）
pub const HOLD_EXEC_MS: u32 = 30_000;
/// 完成态浮条驻留时长（用户瞥一眼的时间��
pub const HOLD_DONE_MS: u32 = 2_500;

/// 单行摘要：`▶ desktop_mouse click left (512,384)` 风格。
/// 参数提炼：优先取 x/y/text/url/button/name 等高信息字段，JSON 全文截断兜底。
/// 平台无关（纯函数），供 HUD 与测试共用。
pub fn tool_summary(name: &str, args: &serde_json::Value) -> String {
    const MAX_TAIL: usize = 48;
    let short = |s: &str| -> String {
        let s = s.replace(['\n', '\r'], " ");
        let s = s.trim();
        if s.chars().count() > MAX_TAIL {
            let cut: String = s.chars().take(MAX_TAIL).collect();
            format!("{cut}…")
        } else {
            s.to_string()
        }
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(obj) = args.as_object() {
        // 高信息密度字段优先、按固定顺序，避免 JSON 序列化顺序抖动
        for key in [
            "action", "button", "x", "y", "direction", "amount", "url", "selector",
            "name", "key", "keys", "path", "title",
        ] {
            if let Some(v) = obj.get(key) {
                match v {
                    serde_json::Value::String(s) => parts.push(format!("{key}={}", short(s))),
                    other => parts.push(format!("{key}={other}")),
                }
            }
        }
        // text 单独处理：输入类工具的核心载荷
        if let Some(serde_json::Value::String(s)) = obj.get("text") {
            parts.push(format!("text=\"{}\"", short(s)));
        }
    }
    if parts.is_empty() {
        name.to_string()
    } else {
        format!("{name} {}", parts.join(" "))
    }
}

/// 显示 HUD（非阻塞，线程安全）。`hold_ms` 后自动隐藏。
/// 非 Windows 平台为 no-op。
pub fn show(text: impl AsRef<str>, hold_ms: u32) {
    #[cfg(windows)]
    imp::show(text.as_ref(), hold_ms);
    #[cfg(not(windows))]
    let _ = (text, hold_ms);
}

// ───────────────────────── Windows 实现 ─────────────────────────

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::{Mutex, OnceLock};

    use ::windows::core::w;
    use ::windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use ::windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
        GetDC, InvalidateRect, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
        CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CALCRECT,
        DT_CENTER, DT_END_ELLIPSIS, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_SEMIBOLD, HFONT,
        OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
    };
    use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use ::windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics,
        PostMessageW, RegisterClassW, SetLayeredWindowAttributes, SetTimer, SetWindowPos,
        ShowWindow, CW_USEDEFAULT, HMENU, HWND_TOPMOST, LWA_ALPHA, SM_CXSCREEN, SM_CYSCREEN,
        SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        TranslateMessage, WM_APP, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    const CLASS_NAME: &str = "NuphusMcpHud";
    /// 自定义通知消息：共享槽内容已更新，请重绘并重置隐藏计时
    const WM_APP_REFRESH: u32 = WM_APP + 1;
    const HIDE_TIMER_ID: usize = 1;

    /// 共享槽：调用线程写，HUD 窗口线程读（wndproc 收到 WM_APP_REFRESH 后绘制）
    struct Slot {
        text: String,
        hold_ms: u32,
    }
    static SLOT: OnceLock<Mutex<Slot>> = OnceLock::new();
    /// HUD 窗口句柄（窗口线程创建后写入；-1 = 未就绪）
    static HWND_SLOT: AtomicIsize = AtomicIsize::new(-1);

    fn slot() -> &'static Mutex<Slot> {
        SLOT.get_or_init(|| Mutex::new(Slot { text: String::new(), hold_ms: 0 }))
    }

    pub fn show(text: &str, hold_ms: u32) {
        ensure_thread();
        // 等窗口线程建好窗（冷启动 <100ms；超时放弃本次提示，绝不阻塞工具执行）
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while HWND_SLOT.load(Ordering::Acquire) == -1 {
            if std::time::Instant::now() > deadline {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        {
            let mut s = slot().lock().unwrap();
            s.text = text.to_string();
            s.hold_ms = hold_ms;
        }
        let hwnd = HWND_SLOT.load(Ordering::Acquire);
        unsafe {
            let _ = PostMessageW(HWND(hwnd), WM_APP_REFRESH, WPARAM(0), LPARAM(0));
        }
    }

    fn ensure_thread() {
        if HWND_SLOT.load(Ordering::Acquire) != -1 {
            return;
        }
        std::thread::Builder::new()
            .name("nuphus-mcp-hud".into())
            .spawn(hud_thread)
            .ok();
    }

    fn hud_thread() {
        unsafe {
            let hinstance = hinstance_from(GetModuleHandleW(None).unwrap());
            // w! 宏只接受字面量；类名是 const 变量 → 运行时 UTF-16 编码
            let class_name: Vec<u16> =
                CLASS_NAME.encode_utf16().chain(std::iter::once(0)).collect();
            let class_pcwstr = ::windows::core::PCWSTR(class_name.as_ptr());
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance,
                lpszClassName: class_pcwstr,
                ..Default::default()
            };
            // 类已存在（重复初始化）时忽略错误
            let _ = RegisterClassW(&wc);

            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_TRANSPARENT
                    | WS_EX_LAYERED,
                class_pcwstr,
                w!(""),
                WS_POPUP,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                0,
                0,
                None,
                HMENU::default(),
                hinstance,
                None,
            );

            // 整窗 alpha 混合：黑底 84% 不透明度
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 215, LWA_ALPHA);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );

            HWND_SLOT.store(hwnd.0, Ordering::Release);

            let mut msg = Default::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_APP_REFRESH => {
                let (text, hold_ms) = {
                    let s = slot().lock().unwrap();
                    (s.text.clone(), s.hold_ms)
                };
                unsafe {
                    layout_and_repaint(hwnd, &text);
                    SetTimer(hwnd, HIDE_TIMER_ID, hold_ms, None);
                }
                LRESULT(0)
            }
            WM_PAINT => {
                unsafe { hud_paint(hwnd) }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 as usize == HIDE_TIMER_ID => {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    /// 右下角锚定 + 按文本测宽，然后触发重绘
    unsafe fn layout_and_repaint(hwnd: HWND, text: &str) {
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let margin = 16;
        let max_w = 520.min(screen_w - margin * 2);

        let font = hud_font();
        let hdc = GetDC(hwnd);
        let old = SelectObject(hdc, font);
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut rect = RECT { left: 0, top: 0, right: max_w, bottom: 40 };
        unsafe {
            DrawTextW(hdc, &mut wide, &mut rect, DT_CALCRECT | DT_END_ELLIPSIS);
        }
        SelectObject(hdc, old);
        let _ = DeleteObject(font);
        ReleaseDC(hwnd, hdc);

        let text_w = (rect.right - rect.left).clamp(160, max_w);
        let h = (rect.bottom - rect.top).max(30) + 16;
        let x = screen_w - text_w - margin * 2;
        let y = screen_h - h - margin * 2;
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                x,
                y,
                text_w + 24,
                h,
                SWP_NOACTIVATE,
            );
            let _ = InvalidateRect(hwnd, None, true);
        }
    }

    unsafe fn hud_paint(hwnd: HWND) {
        let mut ps: PAINTSTRUCT = Default::default();
        unsafe {
            let hdc = BeginPaint(hwnd, &mut ps);
            let text = slot().lock().unwrap().text.clone();
            let rc = ps.rcPaint;

            // 黑底
            let bg = CreateSolidBrush(COLORREF(0x00000000));
            FillRect(hdc, &rc, bg);
            let _ = DeleteObject(bg);

            // 白字居中
            SetTextColor(hdc, COLORREF(0x00FFFFFF));
            SetBkMode(hdc, TRANSPARENT);
            let font = hud_font();
            let old = SelectObject(hdc, font);
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            let mut draw_rc = rc;
            draw_rc.left += 12;
            draw_rc.right -= 12;
            DrawTextW(
                hdc,
                &mut wide,
                &mut draw_rc,
                DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_END_ELLIPSIS,
            );
            SelectObject(hdc, old);
            let _ = DeleteObject(font);

            let _ = EndPaint(hwnd, &ps);
        }
    }

    unsafe fn hud_font() -> HFONT {
        unsafe {
            CreateFontW(
                -16,
                0,
                0,
                0,
                FW_SEMIBOLD.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                w!("Segoe UI"),
            )
        }
    }

    #[inline]
    fn hinstance_from(
        hmodule: ::windows::Win32::Foundation::HMODULE,
    ) -> ::windows::Win32::Foundation::HINSTANCE {
        ::windows::Win32::Foundation::HINSTANCE(hmodule.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summary_prefers_key_fields() {
        let s = tool_summary("desktop_mouse", &json!({"action":"click","button":"left","x":512,"y":384}));
        assert_eq!(s, "desktop_mouse action=click button=left x=512 y=384");
    }

    #[test]
    fn summary_truncates_long_text() {
        let long = "a".repeat(200);
        let s = tool_summary("desktop_input", &json!({"text": long}));
        assert!(s.starts_with("desktop_input text=\""));
        assert!(s.ends_with("…\""));
        assert!(s.chars().count() < 80);
    }

    #[test]
    fn summary_empty_args() {
        assert_eq!(tool_summary("desktop_screen_size", &json!({})), "desktop_screen_size");
    }
}