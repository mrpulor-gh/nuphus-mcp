# nuphus-mcp

[![npm version](https://img.shields.io/npm/v/@nuphus/nuphus-mcp.svg)](https://www.npmjs.com/package/@nuphus/nuphus-mcp) [![npm downloads](https://img.shields.io/npm/dm/@nuphus/nuphus-mcp.svg)](https://www.npmjs.com/package/@nuphus/nuphus-mcp) [![GitHub stars](https://img.shields.io/github/stars/mrpulor-gh/nuphus-mcp.svg)](https://github.com/mrpulor-gh/nuphus-mcp) [![License: MIT](https://img.shields.io/github/license/mrpulor-gh/nuphus-mcp.svg)](LICENSE) [![CI](https://github.com/mrpulor-gh/nuphus-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/mrpulor-gh/nuphus-mcp/actions/workflows/ci.yml)

**Desktop automation MCP server — computer use for any AI agent. See the screen, control windows/mouse/keyboard, and drive Chrome over the Model Context Protocol (stdio). Desktop & browser automation need no API key; OCR runs locally; vision plugs into your own vision LLM (OpenAI-compatible or Anthropic native, BYOK).**

`nuphus-mcp` is a lightweight, cross-platform **desktop automation MCP server**
that exposes desktop + browser automation as standard MCP tools. It speaks
JSON-RPC 2.0 over stdio — no daemon, no network service, one binary. Claude
Desktop, Cursor, VS Code, Copilot, or any MCP client can connect and
immediately control the screen, windows, keyboard/mouse, and Chrome —
**computer use for any AI agent** — desktop & browser automation need no API
key; local OCR is built in; vision works with your own vision LLM
(OpenAI-compatible, BYOK).

> **🇨🇳 Mainland China mirror**: this repo is mirrored on
> [Gitee](https://gitee.com/nuphus/nuphus-mcp) for fast in-China access
> (Chinese docs served by default there). [中文文档](README.zh-CN.md)
>
> **Part of the Nuphus ecosystem**: [Nuphus](https://github.com/mrpulor-gh/nuphus) — a local-first AI agent with real desktop execution and dual-device (phone-as-second-screen) sync. nuphus-mcp exposes the same desktop + browser automation to any MCP client.
>
> **Using DeepSeek Harness (DSH)?** nuphus-mcp itself is a plain stdio MCP server — **not** a DSH/Cordis plugin. Install the dedicated [dsh-nuphus-mcp](https://github.com/mrpulor-gh/dsh-nuphus-mcp) plugin instead (see [DeepSeek Harness (DSH)](#deepseek-harness-dsh)).

```
┌──────────────────┐   stdio JSON-RPC   ┌──────────────────────┐
│  Any MCP Client  │  ───────────────►  │      nuphus-mcp      │
│  (Claude/Cursor/ │  ◄───────────────  │  desktop-api crate   │──► screen/window/mouse/keyboard
│   Nuphus itself) │  single-line JSON  │  nuphus-browser crate│──► Chrome (CDP)
└──────────────────┘                    └──────────────────────┘
```

## Features

- **38 MCP tools** (15 desktop + 23 browser) — screenshots, window control,
  mouse/keyboard, Chrome CDP automation, and more — see [TOOLS.md](TOOLS.md) /
  [TOOLS.zh-CN.md](TOOLS.zh-CN.md) for the full reference.
- **Desktop automation**: screen size, screenshot (PNG/base64), window list,
  window activate/screenshot/move/resize/info, mouse click/drag/scroll/position,
  keyboard input/hotkey, clipboard write/clean — implemented on the
  `desktop-api` crate (xcap + Win32, no Tauri dependency).
- **Computer vision pair**: `desktop_vision` (BYOK — send a screenshot to your
  own vision model via an OpenAI-compatible or Anthropic native API) +
  `desktop_perceive` (local OCR with PaddleOCR, models auto-downloaded on first
  run; optional YOLO icon detection). Used together they give AI agents **both
  semantic understanding
  and pixel-precise coordinates** — the battle-tested vision→perceive flow from
  the Nuphus desktop app. See [TOOLS.md](TOOLS.md) for BYOK env vars, model
  setup, and the recommended flow.
- **Browser automation**: navigate, snapshot (accessibility tree with `@N`
  refs), click, type, exec, scroll, extract, screenshot, evaluate,
  back/forward, wait_for, cookies get/set/import, upload, tabs, downloads —
  implemented on `nuphus-browser` (chromiumoxide CDP).
- **Zero-cost stdio**: no HTTP server, no daemon. The process reads
  single-line JSON from stdin and writes responses to stdout.
- **Safety-first**: destructive tools are annotated per the MCP spec; optional
  strict-confirm mode; path validation for screenshots, uploads, and file drags.

## Repository Layout

```
nuphus-mcp/
├── Cargo.toml                  # workspace root
├── TOOLS.md / TOOLS.zh-CN.md   # 38-tool reference
├── crates/
│   ├── nuphus-mcp/             # MCP Server (this repo's product)
│   ├── nuphus-browser/         # Browser automation core (CDP)
│   └── desktop-api/            # Desktop control core (vendored)
└── ...
```

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

### External browser (anti-detect / fingerprint browsers)

By default `browser_*` tools launch and manage their own Chrome instance. To
drive an **external browser** instead — e.g. an anti-detect / fingerprint
browser — start it with a debugging port and point the server at it:

| Environment variable | Required | Default | Description |
|----------------------|----------|---------|-------------|
| `NUPHUS_MCP_BROWSER_CDP_URL` | — | — | External CDP endpoint, e.g. `http://127.0.0.1:9222` |

```sh
# Example: start your fingerprint browser with a debugging port
chrome --remote-debugging-port=9222 --user-data-dir=...
```

```jsonc
// MCP client config
"env": { "NUPHUS_MCP_BROWSER_CDP_URL": "http://127.0.0.1:9222" }
```

When set, `browser_*` tools attach to that endpoint and never launch a managed
Chrome; attach failures are hard errors (no silent fallback into the wrong
browser). The external browser belongs to you — the server never kills it on
exit.

#### Self-healing on window reopen (recommended)

Fingerprint browsers typically get a **new random debug port every time a
window is reopened**, which would leave a fixed `..._CDP_URL` pointing at a
dead port. Provide the browser identity and the server re-resolves the live
port automatically on the next tool call:

| Environment variable | Required | Default | Description |
|----------------------|----------|---------|-------------|
| `NUPHUS_BROWSER_EXE_PATH` | for self-healing | — | Absolute path to the browser exe — the identity key used to locate the running window process |
| `NUPHUS_BROWSER_NAME` | — | `指纹浏览器` | Display name used in error guidance |
| `NUPHUS_BROWSER_USER_DATA_DIR` | — | — | Profile dir, used to read `DevToolsActivePort` when the window was started with a random port (`--remote-debugging-port=0`) and the process cmdline does not reveal it |

```jsonc
// MCP client config — attach + self-heal
"env": {
  "NUPHUS_MCP_BROWSER_CDP_URL": "http://127.0.0.1:9222",
  "NUPHUS_BROWSER_EXE_PATH": "C:\\path\\to\\fingerprint-browser.exe",
  "NUPHUS_BROWSER_NAME": "AdsPower",
  "NUPHUS_BROWSER_USER_DATA_DIR": "C:\\path\\to\\profile"
}
```

With the identity set, if the configured endpoint stops answering the server
locates the window process by exe path, re-resolves its actual port (literal
cmdline port, or `DevToolsActivePort` in the profile dir for random-port
launches), verifies the candidate with a proxy-bypassing CDP probe and retries
once — close and reopen the window and the next tool call just works. Without
an identity, attach failures stay hard errors asking you to update the
configured URL. Either way there is **no fallback to a managed Chrome**.

### Keeping logins: drive a browser that has your extensions & sessions

Want `browser_*` tools to drive a browser carrying your own extensions /
bookmarks / login state? First, a **hard Chrome 136+ restriction**: after
Chrome's official security change, `--remote-debugging-port` and
`--remote-debugging-pipe` are **ignored against the default user data
directory** — they must be paired with `--user-data-dir` pointing at a
non-default directory. This is a deliberate design to stop infostealer malware
from reading real cookies through the local debugging port — **not a
nuphus-mcp defect**, and no flag / registry policy can bypass it
(`RemoteDebuggingAllowed` only allows/disallows the switches; it cannot
re-enable them on the default directory).

So "real default profile + CDP-controllable" are mutually exclusive since
136+. Pick one of the following, in priority order:

**Option A (recommended, simplest): log in once in the nuphus-managed profile**

nuphus-mcp manages its own Chrome instance by default (a dedicated
`--user-data-dir`, always debuggable). Open it, log into the sites you want to
stay signed in once — the state persists to that profile and every subsequent
`browser_*` call carries those sessions. Zero config, zero copying.

**Option B: copy your real profile to a debuggable directory (keeps
extensions / bookmarks / logins)**

When you need the original browser's extensions and login state, copy the real
profile and launch from the copy:

1. **Quit** your running Chrome / Edge entirely first (profile-lock conflict)
2. Copy the profile to a non-default dir:
   - Windows: `copy "%LOCALAPPDATA%\Google\Chrome\User Data\Default" <dest>\Default`
   - macOS: `cp -R ~/Library/"Application Support"/Google/Chrome/Default <dest>/Default`
   - Linux: `cp -R ~/.config/google-chrome/Default <dest>/Default`
3. Launch the copy with a debug port: `chrome --remote-debugging-port=9222 --user-data-dir=<dest>`
4. Attach as an external browser: `NUPHUS_MCP_BROWSER_CDP_URL=http://127.0.0.1:9222`
   (add `NUPHUS_BROWSER_EXE_PATH` / `NUPHUS_BROWSER_USER_DATA_DIR` for port self-healing)

Notes: on Windows, cookies / passwords are DPAPI user-level encrypted and
decrypt fine from the copy under the same user — login state mostly survives;
on macOS some credentials live in Keychain and a few sites may ask to log in
again. The copy can be hundreds of MB to GBs, and it cannot run at the same
time as the real browser.

**Option C: desktop visual automation — operate the actually-running browser
window**

To operate the user's **currently running** real browser (DOM-level is not
possible), use the desktop OCR + mouse/keyboard chain
(`desktop_perceive` / `desktop_*` tools) — visually click, type and read the
screen without CDP, at the cost of no DOM access and no login-state injection.

### Perceive models (local, auto-downloaded)

`desktop_perceive` runs PaddleOCR and YOLO icon detection locally with ONNX
Runtime. The first call downloads the OCR models and `icon_detect.onnx` together
automatically into `%APPDATA%\Nuphus\models` (or `NUPHUS_MODELS_DIR`). Download
failures return a clear error with manual instructions. YOLO is optional at
runtime: if its download fails, perceive still returns OCR elements and reports
`yolo_available: false` (use `NUPHUS_MCP_YOLO_MODEL_URL` for a custom source).
See [TOOLS.md → Vision & Local Models](TOOLS.md#vision--local-models).

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
cargo build --release -p nuphus-mcp
# binary at target/release/nuphus-mcp(.exe)
```

The server reads newline-delimited JSON from stdin and writes JSON-RPC
responses to stdout. Logs go to stderr.

```sh
# quick smoke test
echo '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}' | nuphus-mcp
```

### 🔒 Recommended: enable strict confirmation

This server can physically control the machine it runs on. By default write
tools run **without** confirmation; we strongly recommend enabling strict
confirmation so destructive operations require an explicit `"confirm": true`
argument from the client (otherwise the tool is rejected with `isError`).

**Any one of the following:**

```sh
# CLI flag
nuphus-mcp --confirm-write

# Environment variable (recommended — survives across clients, keeps config simple)
export NUPHUS_MCP_CONFIRM_WRITE=1      # macOS / Linux
setx NUPHUS_MCP_CONFIRM_WRITE 1        # Windows (persistent for new shells)

# MCP client args
"args": ["--confirm-write"]
```

**Claude Desktop** — recommended `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "nuphus-mcp": {
      "command": "nuphus-mcp",
      "args": ["--confirm-write"]
    }
  }
}
```

> Prefer the environment variable: one setting applies to every MCP client on
> the machine. See [SECURITY.md](SECURITY.md) and the [Safety Annotations
> section of TOOLS.md](TOOLS.md#safety-annotations) for the full threat model.

## MCP Client Configuration

Point any MCP client at `nuphus-mcp`. After `npm install -g
@nuphus/nuphus-mcp` the command is on your PATH; otherwise use the absolute
path to the binary (`nuphus-mcp` / `nuphus-mcp.exe`).

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

Supported MCP methods: `initialize`, `notifications/initialized`, `ping`,
`tools/list`, `tools/call`.

## DeepSeek Harness (DSH)

**Recommended — install the official
[dsh-nuphus-mcp](https://github.com/mrpulor-gh/dsh-nuphus-mcp) plugin** (Gitee
mirror: [gitee.com/nuphus/dsh-nuphus-mcp](https://gitee.com/nuphus/dsh-nuphus-mcp)).
It mounts nuphus-mcp into DSH as native tools with zero config —
`--confirm-write` on by default, no code change needed:

```sh
npx -p @deepseek-ai/dsh dsh plugin --profile web add github:mrpulor-gh/dsh-nuphus-mcp
```

**Manual alternative** — plug into
[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) as a plain
stdio MCP server via its built-in MCP client (`@deepseek-ai/dsh-mcp-client`).
Mount in DSH's `cordis.yml` / patch:

```yaml
- id: nuphus-mcp
  name: '@deepseek-ai/dsh-mcp-client'
  config:
    serverName: nuphus-mcp
    transport: stdio
    command: nuphus-mcp
    args: ["--confirm-write"]
    toolCallTimeoutMs: 120000   # DSH default 60000 is too low for screenshots/OCR
```

Tools register as `mcp__nuphus-mcp__*` (e.g. `mcp__nuphus-mcp__desktop_click`).
Run DSH in the desktop session of the machine you want controlled.

## Demo

A self-contained stdio client that walks through
`initialize → tools/list → tools/call`:

```sh
cargo build -p nuphus-mcp
cargo run -p nuphus-mcp --example demo
```

## Tests

```sh
cargo check --workspace
cargo test -p nuphus-mcp          # protocol + security + vision + models tests (60)
```

## Safety

This server can physically control the machine it runs on. Read
[SECURITY.md](SECURITY.md) and the [Safety Annotations section of
TOOLS.md](TOOLS.md#safety-annotations) before deploying. Recommended: run with
`--confirm-write` (or `NUPHUS_MCP_CONFIRM_WRITE=1`) so write tools require an
explicit `"confirm": true` argument.

## License

MIT