//! hud — 工具调用执行提示（非侵入可见性层，跨平台）
//!
//! 设计约束（大王定调，2026-08-28）：
//! - 桌面操作的目标大多是**其它应用的窗口**，且多个 agent 并行共用一个桌面——
//!   绝不允许用「激活窗口」做执行提示（焦点是用户与其它 agent 的领地）
//! - 各平台的非侵入实现：
//!   - Windows：右下角紧凑状态胶囊（执行中 / 完成 / 失败 三态），Win32 原生窗口
//!   - macOS：系统通知中心（`osascript display notification`），仅完成态
//!   - Linux：libnotify（`notify-send`），仅完成态；无桌面环境时静默降级
//! - 非侵入共性：通知/浮条都**不夺焦点**；激活窗口被明令禁止作为可见性手段
//! - 开关：环境变量 `NUPHUS_MCP_HUD=off` 一键禁用（默认开启）
//!
//! 视觉约束（大王定调，2026-09-17）：HUD 不得呈「警告条 / 全宽横幅」形态。
//! 样式**对齐主项目 HUD**（`frontend/src/hud/App.tsx` 的 Void & Spark token）：
//! 直角卡片 + 左侧 3px 相位色条 + 14px 相位图标 + 12.5px 主文 / 10px mono 次行 +
//! 执行中底部 2px 渐变扫光 + 相位色辉光描边。绝不用整块饱和填充或分段进度条。

use serde_json::Value;

/// 工具执行中浮条驻留时长（防工具 hang 留残影的上限；正常会被完成态覆盖）
pub const HOLD_EXEC_MS: u32 = 30_000;
/// 完成态浮条驻留时长（用户瞥一眼的时间）
pub const HOLD_DONE_MS: u32 = 2_500;

/// 提示种类。Windows 胶囊三态全程实时显示；macOS/Linux 系统通知**只发
/// 非开始态**——通知是一次性事件，开始态也发会高频轰炸通知中心。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HudKind {
    /// 工具开始执行（仅 Windows 胶囊显示）
    Start,
    /// 工具执行完成（所有平台的完成态通道）
    Done,
    /// 工具执行失败（Windows 胶囊转失败态；macOS/Linux 走同一条完成态通知）
    Fail,
}

fn disabled_by_env() -> bool {
    std::env::var("NUPHUS_MCP_HUD")
        .map(|v| v.eq_ignore_ascii_case("off") || v == "0" || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// 显示执行提示（非阻塞，线程安全）。`hold_ms` 仅 Windows 浮条使用（自动隐藏）。
pub fn show(kind: HudKind, text: impl AsRef<str>, hold_ms: u32) {
    if disabled_by_env() {
        return;
    }
    let text = text.as_ref();
    #[cfg(windows)]
    imp::show(kind, text, hold_ms);
    #[cfg(target_os = "macos")]
    imp::show(kind, text);
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "windows")))]
    imp::show(kind, text);
    #[cfg(not(any(windows, target_os = "macos", all(unix, not(target_os = "windows")))))]
    let _ = (kind, text, hold_ms);
}

/// 单行摘要：`desktop_mouse action=click button=left x=512 y=384` 风格。
/// 参数提炼：优先取 x/y/text/url/button/name 等高信息字段，JSON 全文截断兜底。
/// 平台无关（纯函数），供 HUD 与测试共用。
pub fn tool_summary(name: &str, args: &Value) -> String {
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
            "action",
            "button",
            "x",
            "y",
            "direction",
            "amount",
            "url",
            "selector",
            "name",
            "key",
            "keys",
            "path",
            "title",
        ] {
            if let Some(v) = obj.get(key) {
                match v {
                    Value::String(s) => parts.push(format!("{key}={}", short(s))),
                    other => parts.push(format!("{key}={other}")),
                }
            }
        }
        // text 单独处理：输入类工具的核心载荷
        if let Some(Value::String(s)) = obj.get("text") {
            parts.push(format!("text=\"{}\"", short(s)));
        }
    }
    if parts.is_empty() {
        name.to_string()
    } else {
        format!("{name} {}", parts.join(" "))
    }
}

// ──────────────────────── Windows 实现（右下角状态胶囊） ────────────────────────

/// 文本切分：首个空格前是工具名（head），其后是参数 / 结果摘要（tail）。
/// HUD 用两种字重与颜色分层渲染：工具名恒完整可见，摘要按剩余宽度省略。
pub fn split_head_tail(text: &str) -> (&str, &str) {
    let t = text.trim();
    match t.split_once(' ') {
        Some((head, tail)) => (head, tail.trim()),
        None => (t, ""),
    }
}

#[cfg(windows)]
mod imp {
    use super::{split_head_tail, HudKind};
    use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
    use std::sync::{Mutex, OnceLock};

    use ::windows::core::w;
    use ::windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use ::windows::Win32::Graphics::Gdi::{
        Arc, BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW,
        Ellipse, EndPaint, FillRect, GetDC, InvalidateRect, LineTo, MoveToEx, Polyline, ReleaseDC,
        SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
        DEFAULT_CHARSET, DEFAULT_PITCH, DT_BOTTOM, DT_CALCRECT, DT_END_ELLIPSIS, DT_LEFT,
        DT_SINGLELINE, DT_TOP, FF_DONTCARE, FW_MEDIUM, FW_NORMAL, HDC, HFONT, OUT_DEFAULT_PRECIS,
        PAINTSTRUCT, PS_SOLID, TRANSPARENT,
    };
    use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use ::windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics,
        KillTimer, PostMessageW, RegisterClassW, SetLayeredWindowAttributes, SetTimer,
        SetWindowPos, ShowWindow, SystemParametersInfoW, TranslateMessage, CW_USEDEFAULT, HMENU,
        HWND_TOPMOST, LWA_ALPHA, SM_CXSCREEN, SM_CYSCREEN, SPI_GETWORKAREA, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
        WM_APP, WM_DESTROY, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    const CLASS_NAME: &str = "NuphusMcpHud";
    /// 自定义通知消息：共享槽内容已更新，请重绘并重置隐藏计时
    const WM_APP_REFRESH: u32 = WM_APP + 1;
    const HIDE_TIMER_ID: usize = 1;
    /// 动画计时器：入场淡入 / 执行中转圈 / 底部扫光（只改 alpha 或局部重绘）
    const ANIM_TIMER_ID: usize = 2;
    const ANIM_STEP_MS: u32 = 60;

    // ── 卡片整体（对齐主项目 .hud-root：直角、rgba(15,15,20,.85)、1px 描边 + 相位辉光） ──
    const CARD_H: i32 = 52;
    const CARD_MIN_W: i32 = 200;
    const CARD_MAX_W: i32 = 420;
    const MARGIN: i32 = 20;
    /// 0.85 不透明度（与主项目 --hud-bg 一致）
    const ALPHA: u8 = 217;
    /// 入场淡入步数（60ms × 5 ≈ 主项目 hudEnter 0.2s 的观感）
    const FADE_IN_STEPS: u32 = 5;

    // ── 几何（对齐主项目：3px 色条 / 20px 图标盒 / 12px 右内边距） ──
    const BAR_W: i32 = 3;
    const BAR_H: i32 = 26;
    const ICON_BOX: i32 = 20;
    const ICON_SIZE: i32 = 14;
    const PAD_RIGHT: i32 = 12;
    const GAP_ICON_TEXT: i32 = 8;
    const PROGRESS_H: i32 = 2;

    // ── 配色（COLORREF = 0x00BBGGRR；取自主项目 HUD dark token） ──
    /// `rgba(15,15,20,0.85)` 的底色
    const C_BG: u32 = 0x0014_0F0F;
    /// `rgba(255,255,255,0.06)` 压在底色上的结果
    const C_BORDER: u32 = 0x0022_1D1D;
    const C_TEXT: u32 = 0x00FA_F5F5;
    /// `rgba(255,255,255,0.35)` 压在底色上的结果（meta 行）
    const C_META: u32 = 0x0066_6363;
    /// 相位色（主项目 dark）：running `#60a5fa` / done `#34d399` / error `#f87171`
    const A_RUNNING: u32 = 0x00FA_A560;
    const A_DONE: u32 = 0x0099_D334;
    const A_ERROR: u32 = 0x0071_71F8;
    /// 扫光渐变末端：`rgba(255,255,255,0.12)` 压在底色上的结果
    const C_SWEEP_TAIL: u32 = 0x0030_2F32;

    /// 共享槽：调用线程写，HUD 窗口线程读（wndproc 收到 WM_APP_REFRESH 后绘制）
    struct Slot {
        kind: HudKind,
        text: String,
        hold_ms: u32,
    }
    static SLOT: OnceLock<Mutex<Slot>> = OnceLock::new();
    /// HUD 窗口句柄（窗口线程创建后写入；-1 = 未就绪）
    static HWND_SLOT: AtomicIsize = AtomicIsize::new(-1);
    /// 动画步进计数（窗口线程独占写读）：驱动入场淡入 / 转圈 / 扫光
    static ANIM_TICK: AtomicU32 = AtomicU32::new(0);

    fn slot() -> &'static Mutex<Slot> {
        SLOT.get_or_init(|| {
            Mutex::new(Slot {
                kind: HudKind::Start,
                text: String::new(),
                hold_ms: 0,
            })
        })
    }

    pub fn show(kind: HudKind, text: &str, hold_ms: u32) {
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
            s.kind = kind;
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
            let class_name: Vec<u16> = CLASS_NAME
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
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

            // 整窗 alpha 混合（初值；每次 show 会重设：入场淡入 → 目标 0.85）
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), ALPHA, LWA_ALPHA);
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
                    // 上轮 HOLD_DONE 到点后窗口已被 SW_HIDE——重新显示是必须步骤，
                    // 否则隐藏窗口收不到 WM_PAINT，HUD 永久消失
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    layout_and_repaint(hwnd, &text);
                    // 入场淡入（对齐主项目 hudEnter）：alpha 由 0 逐帧升到目标值；
                    // 执行中随后由动画计时器驱动转圈 + 底部扫光
                    ANIM_TICK.store(0, Ordering::Release);
                    apply_alpha(hwnd, 0);
                    SetTimer(hwnd, ANIM_TIMER_ID, ANIM_STEP_MS, None);
                    SetTimer(hwnd, HIDE_TIMER_ID, hold_ms, None);
                }
                LRESULT(0)
            }
            WM_PAINT => {
                unsafe { hud_paint(hwnd) }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == HIDE_TIMER_ID => {
                unsafe {
                    let _ = KillTimer(hwnd, ANIM_TIMER_ID);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == ANIM_TIMER_ID => {
                unsafe {
                    let tick = ANIM_TICK.fetch_add(1, Ordering::AcqRel) + 1;
                    if tick <= FADE_IN_STEPS {
                        // 入场淡入：只抬整体 alpha，不重绘
                        apply_alpha(hwnd, (ALPHA as u32 * tick / FADE_IN_STEPS) as u8);
                    } else if slot().lock().unwrap().kind == HudKind::Start {
                        // 执行中：转圈 + 底部扫光（局部重绘；不擦背景，避免闪烁）
                        apply_alpha(hwnd, ALPHA);
                        let _ = InvalidateRect(hwnd, None, false);
                    } else {
                        // 完成 / 失败：静止（不持续动，避免告警感）
                        apply_alpha(hwnd, ALPHA);
                        let _ = KillTimer(hwnd, ANIM_TIMER_ID);
                    }
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                // 句柄槽归位：允许后续 show 重建 HUD 线程（防御性自愈）
                HWND_SLOT.store(-1, Ordering::Release);
                unsafe { ::windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) };
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    /// 主显示器工作区（已排除任务栏 / 托盘区），用于右下角锚定
    unsafe fn work_area() -> RECT {
        let mut rc = RECT::default();
        unsafe {
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut rc as *mut RECT as *mut std::ffi::c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
        }
        if rc.right - rc.left <= 0 || rc.bottom - rc.top <= 0 {
            // 兜底：取不到工作区时退回整屏（至少保证可见）
            rc = RECT {
                left: 0,
                top: 0,
                right: GetSystemMetrics(SM_CXSCREEN),
                bottom: GetSystemMetrics(SM_CYSCREEN),
            };
        }
        rc
    }

    /// 右下角锚定：两行文字测宽（主文 + meta）→ 定卡片尺寸 → 触发重绘
    /// 直角卡片（与主项目一致：Windows 无边框窗口不做椭圆角）；
    /// 锚点是**工作区**右下角（排除任务栏/托盘），避免与托盘区重叠
    unsafe fn layout_and_repaint(hwnd: HWND, text: &str) {
        let work = work_area();
        let work_w = work.right - work.left;
        let (head, tail) = split_head_tail(text);
        let (font_text, font_meta) = hud_fonts();

        let avail =
            (CARD_MAX_W - (BAR_W + GAP_ICON_TEXT + ICON_BOX + GAP_ICON_TEXT + PAD_RIGHT)).max(60);
        let hdc = GetDC(hwnd);
        let head_w = measure(hdc, font_text, head, avail).min(avail);
        let tail_w = if tail.is_empty() {
            0
        } else {
            measure(hdc, font_meta, tail, avail).min(avail)
        };
        ReleaseDC(hwnd, hdc);
        let _ = DeleteObject(font_text);
        let _ = DeleteObject(font_meta);

        let content = head_w.max(tail_w);
        let width = (BAR_W + GAP_ICON_TEXT + ICON_BOX + GAP_ICON_TEXT + content + PAD_RIGHT)
            .clamp(CARD_MIN_W, CARD_MAX_W.min(work_w - MARGIN * 2));
        let x = work.right - width - MARGIN;
        let y = work.bottom - CARD_H - MARGIN;
        unsafe {
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, width, CARD_H, SWP_NOACTIVATE);
            let _ = InvalidateRect(hwnd, None, true);
        }
    }

    /// 用指定字体测量单行文本宽度（DT_CALCRECT，不绘制）
    unsafe fn measure(hdc: HDC, font: HFONT, text: &str, max_w: i32) -> i32 {
        if text.is_empty() || max_w <= 0 {
            return 0;
        }
        unsafe {
            let old = SelectObject(hdc, font);
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: max_w,
                bottom: 0,
            };
            DrawTextW(
                hdc,
                &mut wide,
                &mut rect,
                DT_CALCRECT | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
            SelectObject(hdc, old);
            rect.right - rect.left
        }
    }

    /// 仅改分层窗口整体不透明度（不重绘内容，避免任何闪烁）
    fn apply_alpha(hwnd: HWND, alpha: u8) {
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
        }
    }

    unsafe fn hud_paint(hwnd: HWND) {
        let mut ps: PAINTSTRUCT = Default::default();
        unsafe {
            let hdc = BeginPaint(hwnd, &mut ps);
            let rc = ps.rcPaint;
            let (kind, text) = {
                let s = slot().lock().unwrap();
                (s.kind, s.text.clone())
            };
            let accent = match kind {
                HudKind::Start => A_RUNNING,
                HudKind::Done => A_DONE,
                HudKind::Fail => A_ERROR,
            };

            // 底：直角卡片 rgba(15,15,20,.85)（整窗 alpha 亦为 0.85，与主项目 --hud-bg 一致）
            let bg = CreateSolidBrush(COLORREF(C_BG));
            FillRect(hdc, &rc, bg);
            let _ = DeleteObject(bg);

            // 相位色辉光外圈 1px（对齐主项目 box-shadow: 0 0 0 1px var(--hud-glow)）
            let glow = CreateSolidBrush(COLORREF(blend(accent, C_BG, 0.25)));
            for ring in [
                RECT {
                    left: rc.left,
                    top: rc.top,
                    right: rc.right,
                    bottom: rc.top + 1,
                },
                RECT {
                    left: rc.left,
                    top: rc.bottom - 1,
                    right: rc.right,
                    bottom: rc.bottom,
                },
                RECT {
                    left: rc.left,
                    top: rc.top,
                    right: rc.left + 1,
                    bottom: rc.bottom,
                },
                RECT {
                    left: rc.right - 1,
                    top: rc.top,
                    right: rc.right,
                    bottom: rc.bottom,
                },
            ] {
                FillRect(hdc, &ring, glow);
            }
            let _ = DeleteObject(glow);

            // 内描边 1px rgba(255,255,255,.06)
            let border = CreateSolidBrush(COLORREF(C_BORDER));
            for line in [
                RECT {
                    left: rc.left + 1,
                    top: rc.top + 1,
                    right: rc.right - 1,
                    bottom: rc.top + 2,
                },
                RECT {
                    left: rc.left + 1,
                    top: rc.bottom - 2,
                    right: rc.right - 1,
                    bottom: rc.bottom - 1,
                },
                RECT {
                    left: rc.left + 1,
                    top: rc.top + 1,
                    right: rc.left + 2,
                    bottom: rc.bottom - 1,
                },
                RECT {
                    left: rc.right - 2,
                    top: rc.top + 1,
                    right: rc.right - 1,
                    bottom: rc.bottom - 1,
                },
            ] {
                FillRect(hdc, &line, border);
            }
            let _ = DeleteObject(border);

            // 左侧 3px 相位色条（26px 高、垂直居中，对齐主项目 .hud-accent-bar）
            let bar = CreateSolidBrush(COLORREF(accent));
            let bar_y = rc.top + (CARD_H - BAR_H) / 2;
            let bar_rc = RECT {
                left: rc.left,
                top: bar_y,
                right: rc.left + BAR_W,
                bottom: bar_y + BAR_H,
            };
            FillRect(hdc, &bar_rc, bar);
            let _ = DeleteObject(bar);

            // 相位图标（20px 盒内 14px，对齐主项目 .hud-icon）
            let icon_x = rc.left + BAR_W + GAP_ICON_TEXT;
            let icon_y = rc.top + (CARD_H - ICON_BOX) / 2;
            draw_phase_icon(hdc, kind, icon_x, icon_y, accent);

            // 两行文字：主文 12.5px / meta 10px（对齐 .hud-text + .hud-meta）
            let (head, tail) = split_head_tail(&text);
            let (font_text, font_meta) = hud_fonts();
            SetBkMode(hdc, TRANSPARENT);
            let text_left = icon_x + ICON_BOX + GAP_ICON_TEXT;
            let text_right = rc.right - PAD_RIGHT;

            let old = SelectObject(hdc, font_text);
            SetTextColor(hdc, COLORREF(C_TEXT));
            let mut head_w: Vec<u16> = head.encode_utf16().collect();
            let mut head_rc = RECT {
                left: text_left,
                top: rc.top + 9,
                right: text_right,
                bottom: rc.top + 28,
            };
            DrawTextW(
                hdc,
                &mut head_w,
                &mut head_rc,
                DT_SINGLELINE | DT_BOTTOM | DT_LEFT | DT_END_ELLIPSIS,
            );
            SelectObject(hdc, old);

            if !tail.is_empty() {
                let old_meta = SelectObject(hdc, font_meta);
                SetTextColor(hdc, COLORREF(C_META));
                let mut tail_w: Vec<u16> = tail.encode_utf16().collect();
                let mut tail_rc = RECT {
                    left: text_left,
                    top: rc.top + 29,
                    right: text_right,
                    bottom: rc.top + 43,
                };
                DrawTextW(
                    hdc,
                    &mut tail_w,
                    &mut tail_rc,
                    DT_SINGLELINE | DT_TOP | DT_LEFT | DT_END_ELLIPSIS,
                );
                SelectObject(hdc, old_meta);
            }
            let _ = DeleteObject(font_text);
            let _ = DeleteObject(font_meta);

            // 执行中：底部 2px 渐变扫光（对齐主项目 .hud-progress + hudProgressSweep）
            if kind == HudKind::Start {
                draw_progress_sweep(hdc, rc, accent);
            }

            let _ = EndPaint(hwnd, &ps);
        }
    }

    /// 相位图标：形状对齐主项目 HUD 的 SVG（转圈 / 对勾 / 感叹号），14px 画在 20px 盒中央
    unsafe fn draw_phase_icon(hdc: HDC, kind: HudKind, box_x: i32, box_y: i32, accent: u32) {
        unsafe {
            let x = box_x + (ICON_BOX - ICON_SIZE) / 2;
            let y = box_y + (ICON_BOX - ICON_SIZE) / 2;
            let ring = blend(accent, C_BG, 0.25);

            if kind == HudKind::Start {
                // 转圈：只画 270° 圆弧，随动画步进旋转
                let pen = CreatePen(PS_SOLID, 2, COLORREF(accent));
                let old = SelectObject(hdc, pen);
                let cx = (x + ICON_SIZE / 2) as f64;
                let cy = (y + ICON_SIZE / 2) as f64;
                let r = 5.0;
                let start = ((ANIM_TICK.load(Ordering::Acquire) as f64) * 30.0) % 360.0;
                let pt = |deg: f64| -> (i32, i32) {
                    let a = deg.to_radians();
                    (
                        (cx + r * a.cos()).round() as i32,
                        (cy - r * a.sin()).round() as i32,
                    )
                };
                let (sx, sy) = pt(start);
                let (ex, ey) = pt(start + 270.0);
                let arc_rc = RECT {
                    left: x + 1,
                    top: y + 1,
                    right: x + ICON_SIZE - 1,
                    bottom: y + ICON_SIZE - 1,
                };
                let _ = Arc(
                    hdc,
                    arc_rc.left,
                    arc_rc.top,
                    arc_rc.right,
                    arc_rc.bottom,
                    sx,
                    sy,
                    ex,
                    ey,
                );
                SelectObject(hdc, old);
                let _ = DeleteObject(pen);
                return;
            }

            // done / fail 共用底盘圆环（相位色 25% 描边，内部填底色）
            let ring_pen = CreatePen(PS_SOLID, 1, COLORREF(ring));
            let fill = CreateSolidBrush(COLORREF(C_BG));
            let old_pen = SelectObject(hdc, ring_pen);
            let old_brush = SelectObject(hdc, fill);
            let _ = Ellipse(hdc, x + 1, y + 1, x + ICON_SIZE - 1, y + ICON_SIZE - 1);
            SelectObject(hdc, old_brush);
            SelectObject(hdc, old_pen);
            let _ = DeleteObject(fill);
            let _ = DeleteObject(ring_pen);

            let mark_pen = CreatePen(PS_SOLID, 2, COLORREF(accent));
            let old = SelectObject(hdc, mark_pen);
            if kind == HudKind::Done {
                // 对勾
                let pts = [
                    POINT { x: x + 4, y: y + 7 },
                    POINT { x: x + 6, y: y + 9 },
                    POINT {
                        x: x + 10,
                        y: y + 5,
                    },
                ];
                let _ = Polyline(hdc, &pts);
            } else {
                // 感叹号：竖线 + 点
                let _ = MoveToEx(hdc, x + 7, y + 4, None);
                let _ = LineTo(hdc, x + 7, y + 8);
                let _ = MoveToEx(hdc, x + 7, y + 10, None);
                let _ = LineTo(hdc, x + 7, y + 10);
            }
            SelectObject(hdc, old);
            let _ = DeleteObject(mark_pen);
        }
    }

    /// 执行中底部扫光：2px 细线（弱底 + 柔和相位色光带从左向右循环）
    unsafe fn draw_progress_sweep(hdc: HDC, rc: RECT, accent: u32) {
        unsafe {
            let y = rc.bottom - PROGRESS_H;
            let x0 = rc.left + BAR_W;
            let x1 = rc.right;
            let w = x1 - x0;
            if w <= 8 {
                return;
            }
            let base = CreateSolidBrush(COLORREF(C_SWEEP_TAIL));
            let base_rc = RECT {
                left: x0,
                top: y,
                right: x1,
                bottom: rc.bottom,
            };
            FillRect(hdc, &base_rc, base);
            let _ = DeleteObject(base);

            let band = (w * 35 / 100).max(24);
            let travel = w + band;
            let offset = ((ANIM_TICK.load(Ordering::Acquire) as i32) * 14) % travel;
            let start = x0 + offset - band;
            let steps = 12;
            for i in 0..steps {
                let seg_w = (band / steps).max(1);
                let seg_x = start + i * seg_w;
                if seg_x + seg_w < x0 || seg_x > x1 {
                    continue;
                }
                let t = (i as f64 + 0.5) / steps as f64;
                let weight = 1.0 - (2.0 * t - 1.0).abs();
                let brush = CreateSolidBrush(COLORREF(blend(accent, C_SWEEP_TAIL, weight)));
                let seg_rc = RECT {
                    left: seg_x.max(x0),
                    top: y,
                    right: (seg_x + seg_w).min(x1),
                    bottom: rc.bottom,
                };
                FillRect(hdc, &seg_rc, brush);
                let _ = DeleteObject(brush);
            }
        }
    }

    /// 把 fg 按 t 混合到 bg 上（t=0 → bg，t=1 → fg），输入输出均为 COLORREF
    fn blend(fg: u32, bg: u32, t: f64) -> u32 {
        let ch = |shift: u32| -> u32 {
            let f = ((fg >> shift) & 0xFF) as f64;
            let b = ((bg >> shift) & 0xFF) as f64;
            (b + (f - b) * t).round().clamp(0.0, 255.0) as u32
        };
        ch(0) | (ch(8) << 8) | (ch(16) << 16)
    }

    /// 两行文字字体：主文 12.5px/500（Segoe UI）、meta 10px/400（等宽，对齐 --font-mono）
    unsafe fn hud_fonts() -> (HFONT, HFONT) {
        unsafe {
            let text = CreateFontW(
                -13,
                0,
                0,
                0,
                FW_MEDIUM.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                w!("Segoe UI"),
            );
            let meta = CreateFontW(
                -10,
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                w!("Consolas"),
            );
            (text, meta)
        }
    }

    #[inline]
    fn hinstance_from(
        hmodule: ::windows::Win32::Foundation::HMODULE,
    ) -> ::windows::Win32::Foundation::HINSTANCE {
        ::windows::Win32::Foundation::HINSTANCE(hmodule.0)
    }
}

// ─────────────────── macOS 实现（osascript 系统通知，仅完成态） ───────────────────

#[cfg(target_os = "macos")]
mod imp {
    use super::HudKind;
    use std::process::Command;

    pub fn show(kind: HudKind, text: &str) {
        // 开始态不发：通知是一次性事件，高频工具会轰炸通知中心
        if kind == HudKind::Start {
            return;
        }
        // AppleScript 字符串转义（\ 与 "）
        let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
        // 通知不夺焦点（通知中心横幅/横幅数秒自隐），符合可见性哲学
        let _ = Command::new("osascript")
            .args([
                "-e",
                &format!(
                    "display notification \"{}\" with title \"nuphus-mcp\"",
                    escaped
                ),
            ])
            .output();
        // 失败静默：通知是辅助通道，绝不影响工具执行
    }
}

// ──────────────── Linux 实现（libnotify/notify-send，仅完成态） ────────────────

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::HudKind;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// notify-send 首次探测失败（无桌面环境/未安装）后置 false，后续调用零开销跳过
    static AVAILABLE: AtomicBool = AtomicBool::new(true);

    pub fn show(kind: HudKind, text: &str) {
        if kind == HudKind::Start {
            return; // 与 macOS 同策略：仅完成态，防轰炸
        }
        if !AVAILABLE.load(Ordering::Relaxed) {
            return;
        }
        let ok = Command::new("notify-send")
            .args(["-a", "nuphus-mcp", "nuphus-mcp", text])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            // 无桌面环境/未装 libnotify → 静默降级，绝不影响工具执行
            AVAILABLE.store(false, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summary_prefers_key_fields() {
        let s = tool_summary(
            "desktop_mouse",
            &json!({"action":"click","button":"left","x":512,"y":384}),
        );
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
        assert_eq!(
            tool_summary("desktop_screen_size", &json!({})),
            "desktop_screen_size"
        );
    }

    #[test]
    fn split_head_tail_splits_at_first_space() {
        let (head, tail) = split_head_tail("browser_navigate url=https://example.com");
        assert_eq!(head, "browser_navigate");
        assert_eq!(tail, "url=https://example.com");
    }

    #[test]
    fn split_head_tail_handles_name_only_and_blank() {
        assert_eq!(
            split_head_tail("desktop_screen_size").0,
            "desktop_screen_size"
        );
        assert_eq!(split_head_tail("desktop_screen_size").1, "");
        assert_eq!(split_head_tail("   "), ("", ""));
        // 结果态文本（工具名 · 摘要）同样按首个空格切分：工具名恒完整可见
        assert_eq!(
            split_head_tail("browser_click · 842ms"),
            ("browser_click", "· 842ms")
        );
    }
}
