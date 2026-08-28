# nuphus-mcp

**Give any MCP-compatible AI client the power to operate your computer — desktop automation + browser automation over the Model Context Protocol (stdio).**

`nuphus-mcp` is a lightweight, dependency-light MCP Server that exposes Nuphus's desktop and browser automation capabilities as standard MCP tools. It speaks JSON-RPC 2.0 over stdio — no server, no network, no installation service. Claude Desktop, Cursor, any MCP client, or Nuphus itself (dogfooding) can connect and immediately control screen, windows, keyboard/mouse, and Chrome.

```
┌──────────────────┐   stdio JSON-RPC   ┌──────────────────────┐
│  Any MCP Client  │  ───────────────►  │      nuphus-mcp      │
│  (Claude/Cursor/ │  ◄───────────────  │  desktop-api crate   │──► screen/window/mouse/keyboard
│   Nuphus itself) │  single-line JSON  │  nuphus-browser crate│──► Chrome (CDP)
└──────────────────┘                    └──────────────────────┘
```

## Features

- **Desktop automation** (15 tools): screen size, screenshot (PNG/base64), window list/activate/screenshot/**move/resize/info**, mouse click/drag/scroll/position, keyboard input/hotkey, clipboard write/clean — implemented on the `desktop-api` crate (xcap + Win32, no Tauri dependency).
- **Computer vision** (2 tools): `desktop_vision` (BYOK — send a screenshot to your own vision model via an OpenAI-compatible or Anthropic native API) and `desktop_perceive` (local OCR + YOLO element location with PaddleOCR, models auto-downloaded on first run).
- **Browser automation** (23 tools): navigate, snapshot (accessibility tree with `@N` refs), click, type, trusted key/chord press, exec, scroll, extract, screenshot, evaluate, back/forward, wait_for, cookies get/set/import, upload/drag, tabs, downloads — implemented on `nuphus-browser` (chromiumoxide CDP, shared with the Nuphus main app).
- **Zero-cost stdio**: no HTTP server, no daemon. The process reads single-line JSON from stdin and writes responses to stdout.
- **Safety-first**: destructive tools are annotated per the MCP spec; optional strict-confirm mode; path validation for screenshots, uploads, and file drags.
- **Dogfooded**: the Nuphus main app itself calls this server through its MCP client (dual-channel) so the MCP layer is validated by real use.

## Prerequisites

- **Rust toolchain** (stable) — build from source with Cargo.
- **Chrome or Edge** — required for browser tools. The server auto-detects an
  installed browser; if none is found, `browser_*` tools return a clear error.
- **Windows recommended** for full desktop control — see Platform Support below.

## Platform Support

| Platform | Browser tools | Desktop tools |
|----------|---------------|---------------|
| Windows  | Full          | Full (Win32 API) |
| macOS    | Full          | Desktop input requires Accessibility permission (System Settings → Privacy & Security → Accessibility) |
| Linux    | Available     | Partial — window/input capabilities are limited |

> **Execution HUD**: non-intrusive execution feedback on every platform — Windows shows an on-screen OSD bar (start + result, real-time), macOS/Linux post a **system notification** on completion (`NUPHUS_MCP_HUD=off` to disable). Window activation is never used as a visibility fallback.

## API Keys & Local Models

### Vision — BYOK, OpenAI-compatible or Anthropic native

`desktop_vision` uses **your own** vision model. It speaks two protocols:
- **OpenAI-compatible** Chat Completions (default) — works with OpenAI, MiniMax, Qwen, Ollama, vLLM, …
- **Anthropic native** Messages API — point `NUPHUS_MCP_VISION_BASE_URL` at
  `https://api.anthropic.com/v1` and the protocol is auto-detected from the host;
  or force it with `NUPHUS_MCP_VISION_PROVIDER=anthropic`.

Nothing is required unless you call this tool — and when it is not configured
the tool returns a clear error instead of silently failing.

| Environment variable | Required | Default | Description |
|----------------------|----------|---------|-------------|
| `NUPHUS_MCP_VISION_API_KEY` | ✅ | — | API key for your vision model |
| `NUPHUS_MCP_VISION_BASE_URL` | — | `https://api.openai.com/v1` | Base URL (`https://api.anthropic.com/v1` for Claude) |
| `NUPHUS_MCP_VISION_MODEL` | ✅ | — | Model id, e.g. `gpt-4o-mini`, `qwen-vl-max`, `claude-sonnet-4-5` |
| `NUPHUS_MCP_VISION_PROVIDER` | — | `auto` | `auto` \| `openai` \| `anthropic`; `auto` infers from the base URL host |
| `NUPHUS_MCP_VISION_MAX_TOKENS` | — | `1024` | Max output tokens (Zhipu GLM-4V-Flash caps at 1024; raise for text-heavy screenshots) |

```sh
# OpenAI-compatible provider (default)
set NUPHUS_MCP_VISION_API_KEY=sk-...
set NUPHUS_MCP_VISION_MODEL=qwen-vl-max
# optional: set NUPHUS_MCP_VISION_BASE_URL=https://your-gateway/v1

# Anthropic / Claude — provider is auto-detected from the base URL
set NUPHUS_MCP_VISION_API_KEY=sk-ant-...
set NUPHUS_MCP_VISION_BASE_URL=https://api.anthropic.com/v1
set NUPHUS_MCP_VISION_MODEL=claude-sonnet-4-5
```

### Perceive models (local, auto-downloaded)

`desktop_perceive` runs PaddleOCR (`ch_PP-OCRv4_det.onnx`, `ch_PP-OCRv4_rec.onnx`,
`ch_PP-OCR_keys_v1.txt`) and the YOLO icon detector (`icon_detect.onnx`) locally
with ONNX Runtime. The first call downloads **all models together** automatically
into `%APPDATA%\Nuphus\models` (or `NUPHUS_MODELS_DIR` if set). If a download
fails the tool returns a clear error with manual download instructions — it never
panics.

- YOLO icon detection (`icon_detect.onnx`) is auto-downloaded alongside PaddleOCR
  from `onnx-community/OmniParser-icon_detect_640x640` (hf-mirror.com first,
  huggingface.co fallback). It is **optional** at runtime: if its download fails,
  `desktop_perceive` still returns OCR elements and reports `yolo_available: false`.
  To use a different source (e.g. the full ~80 MB OmniParser export or a private
  mirror), set `NUPHUS_MCP_YOLO_MODEL_URL` to the direct `.onnx` URL.
- `NUPHUS_MCP_NO_MODEL_DOWNLOAD=1` skips the automatic download (fast-fail on
  restricted networks / CI).
- Requires `onnxruntime.dll` on the library search path (it is bundled next to
  the Nuphus app; copy it next to `nuphus-mcp.exe` for standalone use).

All other tools need no API key.

## Install & Run

**Install via npm (recommended — all platforms, prebuilt binaries):**

```sh
npm install -g @nuphus/nuphus-mcp
```

The `nuphus-mcp` meta package installs the prebuilt binary for your platform
automatically (Windows x64/arm64, macOS arm64, Linux x64/arm64) and puts the
`nuphus-mcp` command on your PATH. No Rust toolchain needed:

```sh
nuphus-mcp   # stdio MCP server
```

**Build from source** (requires the Rust toolchain):

```sh
# from the workspace root
cargo build --release -p nuphus-mcp
# binary at target/release/nuphus-mcp(.exe)
```

The server reads newline-delimited JSON from stdin and writes JSON-RPC responses to stdout. Logs go to stderr.

```sh
# quick smoke test
echo '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}' | nuphus-mcp
```

## MCP Client Configuration

Point any MCP client at `nuphus-mcp`. After `npm install -g @nuphus/nuphus-mcp`
the command is on your PATH; otherwise use the absolute path to the binary
(`nuphus-mcp` / `nuphus-mcp.exe`).

**Claude Desktop** — `claude_desktop_config.json`:

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

**Cursor** — `.cursor/mcp.json`:

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

**Any MCP client** (generic mcpServers JSON):

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

Supported MCP methods: `initialize`, `notifications/initialized`, `ping`, `tools/list`, `tools/call`.

## Demo

A self-contained stdio client that walks through `initialize → tools/list → tools/call`:

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

## Tools

### Desktop (15)

| Tool | Description |
|------|-------------|
| `desktop_screen_size` | Screen resolution (width × height) |
| `desktop_screenshot` | Fullscreen / region screenshot, save PNG or return base64 |
| `desktop_windows_list` | List visible OS windows (hwnd/title/position) |
| `desktop_window_activate` | Bring a window to the foreground by hwnd |
| `desktop_window_screenshot` | Capture a specific window as PNG |
| `desktop_window_move` | Move a window to screen coordinates (SetWindowPos) |
| `desktop_window_resize` | Resize a window (SetWindowPos, keeps position) |
| `desktop_window_info` | Query window details (title/state/rects/process/class) |
| `desktop_vision` | Understand a screenshot with your own vision model (BYOK) |
| `desktop_perceive` | Local OCR + YOLO element location (PaddleOCR, auto-download) |
| `desktop_mouse` | click / double_click / hover / move / scroll / position |
| `desktop_mouse_drag` | Drag from (x1,y1) to (x2,y2) |
| `desktop_input` | Type text or press hotkeys (SendInput Unicode) |
| `desktop_clipboard_write` | Write long text (>500 chars) to clipboard |
| `desktop_clipboard_clean` | Clear the system clipboard |

### Browser (23)

| Tool | Description |
|------|-------------|
| `browser_navigate` | Open URL (auto-snapshots after) |
| `browser_snapshot` | Accessibility-tree snapshot with `@N` refs |
| `browser_click` / `browser_type` | Left/right/middle click / type into element (CSS selector or `@N`) |
| `browser_press` | Press a trusted key or chord on the focused element (`Enter`, `Control+c`, `Shift+Tab`) |
| `browser_exec` | Multi-step batch script in one CDP round trip |
| `browser_scroll` / `browser_extract` | Scroll page / extract readable text |
| `browser_screenshot` | Screenshot current page |
| `browser_evaluate` | Run arbitrary JavaScript |
| `browser_back` / `browser_forward` | History navigation |
| `browser_wait_for` | Wait for selector state (attached/visible/hidden) |
| `browser_cookies_get` / `browser_cookies_set` / `browser_import_cookies` | Cookie management |
| `browser_upload` | Upload file to `<input type=file>` |
| `browser_drag_files` | Native file/directory drag onto any browser element |
| `browser_list_tabs` / `browser_switch_tab` / `browser_new_tab` | Tab management |
| `browser_list_downloads` | List download directory |
| `browser_close` | Close browser |

## Security

`nuphus-mcp` exposes high-risk desktop/browser control. It ships with layered protections:

1. **Tool annotations (MCP spec)** — `tools/list` marks destructive tools with `annotations.destructiveHint` and read-only tools with `annotations.readOnlyHint`, so clients can surface confirmation UI.
2. **Strict confirm mode** — start with `--confirm-write` (or set `NUPHUS_MCP_CONFIRM_WRITE=1`); write tools then require an explicit `"confirm": true` argument and are rejected otherwise:
   ```sh
   nuphus-mcp --confirm-write
   # tools/call {"name":"desktop_input","arguments":{"mode":"type","hwnd":123}} → isError "requires confirmation"
   # tools/call {"name":"desktop_input","arguments":{"mode":"type","hwnd":123,"confirm":true}} → executes
   ```
3. **Path validation** — desktop/browser screenshot save paths reject `..` traversal,
   Windows device paths (`\\?\`, `\\.\`), and system-protected directories; upload
   files must exist, and drag paths must be canonical absolute paths.
4. **stdio / localhost-only** — the transport is a local pipe; no network surface. Only processes that can spawn the binary locally can reach it.
5. **Read-only tools unaffected** — `desktop_screen_size`, `desktop_windows_list`, `browser_snapshot`, etc. never require confirmation.

## Nuphus Dogfooding (dual-channel)

The Nuphus main app calls its own `nuphus-mcp` through its MCP client. Every `desktop_*` / `browser_*` tool call tries the MCP channel first, and falls back to the direct executor when the server is unavailable:

- Server configured / auto-discovered (binary next to Nuphus in `target/<profile>/`) → **MCP first**
- Not configured / transport error / timeout → fall back to direct
- MCP semantic error (`isError`) → read tools fall back to direct; **write tools do not** (avoid double execution)

```sh
# disable the dual channel entirely (always direct):
set NUPHUS_MCP_DUAL=off
```

## Tests

```sh
cargo test -p nuphus-mcp          # protocol + security + vision + models tests (60)
cargo test -p nuphus-browser      # browser unit tests
cargo test -p nuphus --lib workflow  # main app workflow regression (52)

# real end-to-end dogfooding test (requires built binary):
cargo build -p nuphus-mcp
cargo test -p nuphus --lib mcp::dual::tests::e2e_dogfood_screen_size -- --ignored --nocapture
```

## License

MIT