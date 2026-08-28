# nuphus-mcp

**让任何兼容 MCP 的 AI 客户端获得"操作电脑"的能力 —— 桌面自动化 + 浏览器自动化，经 Model Context Protocol（stdio）接入。**

`nuphus-mcp` 是一个轻量、依赖克制的 MCP Server，把 Nuphus 的桌面与浏览器自动化能力封装为标准 MCP 工具。它通过 stdio 走 JSON-RPC 2.0 —— 无服务器、无网络、无安装服务。Claude Desktop、Cursor、任意 MCP 客户端、乃至 Nuphus 自身（dogfooding）都能即连即用，控制屏幕、窗口、键鼠与 Chrome。

```
┌──────────────────┐   stdio JSON-RPC   ┌──────────────────────┐
│  任意 MCP 客户端  │  ───────────────►  │      nuphus-mcp      │
│  (Claude/Cursor/ │  ◄───────────────  │  desktop-api crate   │──► 屏幕/窗口/键鼠
│   Nuphus 自身)    │    单行 JSON       │  nuphus-browser crate│──► Chrome (CDP)
└──────────────────┘                    └──────────────────────┘
```

## 特性

- **桌面自动化（15 个工具）**：屏幕分辨率、截图（PNG/base64）、窗口列表、窗口激活、窗口截图、**窗口移动/缩放/信息查询**、鼠标点击/拖拽/滚轮/定位、键盘输入/快捷键、剪贴板写入/清空 —— 基于 `desktop-api` crate（xcap + Win32），不依赖 Tauri。
- **计算机视觉（2 个工具）**：`desktop_vision`（BYOK —— 截图发送到你自己的视觉模型，OpenAI 兼容或 Anthropic 原生 API）与 `desktop_perceive`（本地 OCR + YOLO 元素定位，PaddleOCR，首次运行自动下载模型）。
- **浏览器自动化（23 个工具）**：导航、快照（无障碍树 `@N` 引用）、点击、输入、可信键盘按键/组合键、批量脚本、滚动、正文提取、截图、JS 执行、前进/后退、等待、Cookie 读写/导入、文件上传/拖放、标签页、下载目录 —— 基于 `nuphus-browser`（chromiumoxide CDP，与 Nuphus 主程序共用）。
- **零成本 stdio**：无 HTTP 服务、无常驻进程。进程从 stdin 读单行 JSON，向 stdout 写响应。
- **安全优先**：破坏性工具按 MCP 规范标注；可选严格确认模式；截图、上传和文件拖放路径校验。
- **Dogfooding**：Nuphus 主程序自身通过 MCP client 调用本 server（双通道），MCP 层被真实使用持续验证。

## 环境依赖

- **Rust 工具链（stable）** —— 通过 Cargo 从源码构建。
- **Chrome 或 Edge** —— browser 工具必需。server 自动查找本机已安装的浏览器；
  找不到时 `browser_*` 工具返回明确错误。
- **Windows 优先推荐** —— 桌面控制完整支持，见下方平台支持。

## 平台支持

| 平台 | 浏览器工具 | 桌面工具 |
|------|-----------|---------|
| Windows | 全量 | 全量（Win32 API） |
| macOS | 全量 | 桌面输入需在「系统设置 → 隐私与安全性 → 辅助功能」中授权 |
| Linux | 可用 | 部分支持——窗口/输入能力受限 |

> **执行 HUD**：全平台非侵入执行反馈——Windows 屏幕浮条（开始 ▶ / 完成 ✓ 实时显示），macOS/Linux 完成后发**系统通知**（`NUPHUS_MCP_HUD=off` 关闭）。绝不以激活窗口作为可见性手段。

## API Key 与本地模型

### 视觉理解（可选，BYOK）

`desktop_vision` 使用**你自己的**视觉模型，支持两种协议：
- **OpenAI 兼容** Chat Completions（默认）——适用于 OpenAI、MiniMax、通义、Ollama、vLLM 等；
- **Anthropic 原生** Messages API——把 `NUPHUS_MCP_VISION_BASE_URL` 指向
  `https://api.anthropic.com/v1` 即可，协议从 host 自动识别；也可用
  `NUPHUS_MCP_VISION_PROVIDER=anthropic` 显式指定。

不调用该工具就不需要任何配置；未配置时工具返回明确错误，不会静默失败。

| 环境变量 | 必填 | 默认值 | 说明 |
|----------|------|--------|------|
| `NUPHUS_MCP_VISION_API_KEY` | ✅ | — | 视觉模型 API Key |
| `NUPHUS_MCP_VISION_BASE_URL` | — | `https://api.openai.com/v1` | base URL（Claude 用 `https://api.anthropic.com/v1`） |
| `NUPHUS_MCP_VISION_MODEL` | ✅ | — | 模型 ID，如 `gpt-4o-mini`、`qwen-vl-max`、`claude-sonnet-4-5` |
| `NUPHUS_MCP_VISION_PROVIDER` | — | `auto` | `auto` \| `openai` \| `anthropic`；`auto` 按 base URL host 自动识别 |
| `NUPHUS_MCP_VISION_MAX_TOKENS` | — | `1024` | 最大输出 token 数（智谱 GLM-4V-Flash 上限 1024；文本多时可调大） |

```sh
# OpenAI 兼容（默认）
set NUPHUS_MCP_VISION_API_KEY=sk-...
set NUPHUS_MCP_VISION_MODEL=qwen-vl-max
# 可选：set NUPHUS_MCP_VISION_BASE_URL=https://your-gateway/v1

# Anthropic / Claude —— 从 base URL 自动识别协议
set NUPHUS_MCP_VISION_API_KEY=sk-ant-...
set NUPHUS_MCP_VISION_BASE_URL=https://api.anthropic.com/v1
set NUPHUS_MCP_VISION_MODEL=claude-sonnet-4-5
```

### perceive 模型（本地，自动下载）

`desktop_perceive` 用 ONNX Runtime 本地运行 PaddleOCR（`ch_PP-OCRv4_det.onnx`、
`ch_PP-OCRv4_rec.onnx`、`ch_PP-OCR_keys_v1.txt`）和 YOLO 图标检测
（`icon_detect.onnx`）。首次调用把**全部模型一起**自动下载到
`%APPDATA%\Nuphus\models`（或 `NUPHUS_MODELS_DIR`）。下载失败时工具返回明确
错误并附手动下载指引，绝不 panic。

- YOLO 图标检测（`icon_detect.onnx`）随 PaddleOCR 一起自动下载，来源为
  `onnx-community/OmniParser-icon_detect_640x640`（优先 hf-mirror.com，回退
  huggingface.co）。它在运行时是**可选**的：下载失败时 `desktop_perceive` 仍返回
  OCR 结果并报告 `yolo_available: false`。如需指定其它来源（如完整 ~80MB 的
  OmniParser 导出或私有镜像），设置 `NUPHUS_MCP_YOLO_MODEL_URL` 为 `.onnx` 直链。
- `NUPHUS_MCP_NO_MODEL_DOWNLOAD=1` 跳过自动下载（受限网络/CI 快速失败）。
- 运行需要 `onnxruntime.dll` 可加载（Nuphus 主程序旁已内置；独立运行时请把它
  复制到 `nuphus-mcp.exe` 同目录）。

其余工具无需任何 API key。

## 安装与运行

**npm 安装（推荐 —— 全平台，预编译二进制）：**

```sh
npm install -g @nuphus/nuphus-mcp
```

`nuphus-mcp` meta 包会自动安装当前平台对应的预编译二进制（Windows
x64/arm64、macOS arm64、Linux x64/arm64），并把 `nuphus-mcp` 命令加入 PATH。
无需 Rust 工具链：

```sh
nuphus-mcp   # stdio MCP server
```

**源码构建**（需要 Rust 工具链）：

```sh
# 在 workspace 根目录
cargo build --release -p nuphus-mcp
# 二进制在 target/release/nuphus-mcp(.exe)
```

server 从 stdin 读换行分隔的 JSON，向 stdout 写 JSON-RPC 响应；日志一律走 stderr。

```sh
# 快速冒烟
echo '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}' | nuphus-mcp
```

## MCP 客户端配置

把任意 MCP 客户端指向 `nuphus-mcp` 即可。`npm install -g @nuphus/nuphus-mcp`
之后命令已在 PATH 上；否则使用二进制的绝对路径（`nuphus-mcp` /
`nuphus-mcp.exe`）。

**Claude Desktop** — `claude_desktop_config.json`：

```json
{
  "mcpServers": {
    "nuphus-mcp": {
      "command": "nuphus-mcp",
      "args": []
    }
  }
}
```

**Cursor** — `.cursor/mcp.json`：

```json
{
  "mcpServers": {
    "nuphus-mcp": {
      "command": "nuphus-mcp",
      "args": []
    }
  }
}
```

**任意 MCP 客户端**（通用 mcpServers JSON）：

```json
{
  "mcpServers": {
    "nuphus-mcp": {
      "command": "nuphus-mcp",
      "args": [],
      "env": {}
    }
  }
}
```

支持的 MCP 方法：`initialize`、`notifications/initialized`、`ping`、`tools/list`、`tools/call`。

## Demo

自带一个自包含 stdio client，走完 `initialize → tools/list → tools/call`：

```sh
cargo build -p nuphus-mcp
cargo run -p nuphus-mcp --example demo
```

```
[1] initialize OK → server=nuphus-mcp, protocol=2024-11-05
[2] tools/list OK → 38 tools (desktop 15 + browser 23), 27 marked destructive
[3] desktop_screen_size → {"height":1080,"width":1920}
[4] browser_navigate → Navigated to: data:text/html,...  | Title: Untitled
[5] browser_evaluate → "nuphus-mcp demo"
[6] browser_close → Browser closed
```

## 工具清单

### 桌面（15 个）

| 工具 | 说明 |
|------|------|
| `desktop_screen_size` | 屏幕分辨率（宽 × 高） |
| `desktop_screenshot` | 全屏/区域截图，保存 PNG 或返回 base64 |
| `desktop_windows_list` | 列出可见窗口（hwnd/标题/位置） |
| `desktop_window_activate` | 按 hwnd 激活窗口到前台 |
| `desktop_window_screenshot` | 截取指定窗口为 PNG |
| `desktop_window_move` | 移动窗口到屏幕坐标（SetWindowPos） |
| `desktop_window_resize` | 缩放窗口（SetWindowPos，保持位置） |
| `desktop_window_info` | 查询窗口详情（标题/状态/矩形/进程/类名） |
| `desktop_vision` | 用你自己的视觉模型理解截图（BYOK） |
| `desktop_perceive` | 本地 OCR + YOLO 元素定位（PaddleOCR，自动下载） |
| `desktop_mouse` | click / double_click / hover / move / scroll / position |
| `desktop_mouse_drag` | 从 (x1,y1) 拖拽到 (x2,y2) |
| `desktop_input` | 输入文本或按快捷键（SendInput Unicode） |
| `desktop_clipboard_write` | 写入长文本（>500 字符）到剪贴板 |
| `desktop_clipboard_clean` | 清空系统剪贴板 |

### 浏览器（23 个）

| 工具 | 说明 |
|------|------|
| `browser_navigate` | 打开 URL（导航后自动快照） |
| `browser_snapshot` | 无障碍树快照，带 `@N` 引用 |
| `browser_click` / `browser_type` | 左/右/中键点击 / 输入（CSS 选择器或 `@N`） |
| `browser_press` | 向当前聚焦元素发送可信按键或组合键（`Enter`、`Control+c`、`Shift+Tab`） |
| `browser_exec` | 单次 CDP 往返执行多步脚本 |
| `browser_scroll` / `browser_extract` | 滚动页面 / 提取可读文本 |
| `browser_screenshot` | 当前页面截图 |
| `browser_evaluate` | 执行任意 JavaScript |
| `browser_back` / `browser_forward` | 历史导航 |
| `browser_wait_for` | 等待选择器状态（attached/visible/hidden） |
| `browser_cookies_get` / `browser_cookies_set` / `browser_import_cookies` | Cookie 管理 |
| `browser_upload` | 上传文件到 `<input type=file>` |
| `browser_drag_files` | 原生拖放本地文件/目录到任意浏览器元素 |
| `browser_list_tabs` / `browser_switch_tab` / `browser_new_tab` | 标签页管理 |
| `browser_list_downloads` | 列出下载目录 |
| `browser_close` | 关闭浏览器 |

## 安全边界

`nuphus-mcp` 暴露的是高风险桌面/浏览器控制能力，内置多层防护：

1. **工具标注（MCP 规范）** — `tools/list` 为破坏性工具标注 `annotations.destructiveHint`、为只读工具标注 `annotations.readOnlyHint`，客户端可据此呈现确认 UI。
2. **严格确认模式** — 以 `--confirm-write` 启动（或设置环境变量 `NUPHUS_MCP_CONFIRM_WRITE=1`）；写工具必须显式携带 `"confirm": true` 参数，否则拒绝执行：
   ```sh
   nuphus-mcp --confirm-write
   # tools/call {"name":"desktop_input","arguments":{"mode":"type","hwnd":123}} → isError "requires confirmation"
   # tools/call {"name":"desktop_input","arguments":{"mode":"type","hwnd":123,"confirm":true}} → 执行
   ```
3. **路径校验** — 桌面/浏览器截图保存路径拒绝 `..` 穿越、Windows 设备路径
   （`\\?\`、`\\.\`）与系统保护目录；上传文件必须真实存在，拖放路径必须为
   规范化后的绝对路径。
4. **stdio / 仅本机** — 传输为本地管道，无网络面；只有能本机拉起该二进制的进程才能触达。
5. **只读工具不受影响** — `desktop_screen_size`、`desktop_windows_list`、`browser_snapshot` 等永不要求确认。

## Nuphus 双通道（dogfooding）

Nuphus 主程序通过自己的 MCP client 调用本 server。每次 `desktop_*` / `browser_*` 工具调用**优先走 MCP 通道**，server 不可用时回退直连执行器：

- server 已配置 / 自动发现（二进制与 Nuphus 同在 `target/<profile>/`）→ **MCP 优先**
- 未配置 / 传输错误 / 超时 → 回退直连
- MCP 语义失败（`isError`）→ 读工具回退直连；**写工具不回退**（防双重执行）

```sh
# 整体关闭双通道（总是直连）：
set NUPHUS_MCP_DUAL=off
```

## 测试

```sh
cargo test -p nuphus-mcp          # 协议 + 安全 + 视觉 + 模型测试（28）
cargo test -p nuphus-browser      # 浏览器单元测试
cargo test -p nuphus --lib workflow  # 主程序 workflow 回归（52）

# 真实端到端 dogfooding 测试（需先构建二进制）：
cargo build -p nuphus-mcp
cargo test -p nuphus --lib mcp::dual::tests::e2e_dogfood_screen_size -- --ignored --nocapture
```

## License

MIT