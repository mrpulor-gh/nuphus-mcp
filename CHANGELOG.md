# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.2] - 2026-08-30

### Changed

- **Compacted 17 MCP tool descriptions to short phrases** — key parameters,
  hard constraints, and behavior contracts preserved, reducing prompt-injection
  overhead for AI clients.

## [0.2.1] - 2026-08-29

### Added

- **Cross-platform system notifications for the execution HUD** — macOS
  `osascript` / Linux `notify-send`, so tool-call progress stays visible even
  when the terminal is not focused.

## [0.2.0] - 2026-08-29

### Added

- **Execution HUD**: non-intrusive floating status bar showing tool-call
  execution in real time (fades out automatically).

### Fixed

- **HUD disappearing permanently after one show** — the floating bar now stays
  until the next tool call; minimized-window activation now restores and
  focuses the window.
- **Browser**: upgrade a resident headless Chrome instance to headed mode when
  a headed request arrives.
- **CI**: EOF newline on `mouse.rs`, bump `h2` for RUSTSEC-2026-0258.

## [0.1.13] - 2026-08-21

### Fixed

- **Mouse movement silently "succeeding" without moving the cursor** — `move_to`
  on Windows ignored the `SetCursorPos` return value (`let _ = ...`) and always
  returned `Ok`, so when the OS rejected the move (session / privilege limits)
  callers got a fake success while the cursor never moved. `move_to` now checks
  the API result and verifies the actual cursor position against the target
  (2px tolerance for DPI rounding) before returning; any mismatch fails loudly
  with a descriptive error instead of a silent success. `drag` propagates the
  same errors instead of swallowing them mid-path.

## [0.1.11] - 2026-08-09

### Fixed

- **npm install on macOS** — the `@nuphus/nuphus-mcp-osx-arm64` platform
  package declared `"os": ["osx"]`, which never matches npm's
  `process.platform` value on macOS (`"darwin"`), so the optional dependency
  was skipped with a `notsup` warning and the postinstall check failed. The
  generated `os` field now uses npm's platform name `"darwin"` (the package
  name stays `osx-arm64`); the win32/linux packages were already correct.

## [0.1.10] - 2026-08-07

### Added

- **Anthropic native vision backend** — `desktop_vision` can now talk to Claude
  (and any Anthropic Messages API endpoint) natively. Set `NUPHUS_MCP_VISION_PROVIDER`
  to `auto` (default), `openai`, or `anthropic`. In `auto` mode the provider is
  inferred from the base URL: an endpoint whose host contains `anthropic` is sent
  the native `/v1/messages` protocol with `x-api-key` + `anthropic-version:
  2023-06-01` headers and a real `image`/`source` base64 block; everything else
  keeps the OpenAI-compatible `image_url` path. (Anthropic's OpenAI-compat layer
  does not convert `image_url` blocks, so Claude requires the native protocol —
  merely pointing a base URL at `api.anthropic.com` used to fail or silently drop
  the image.)
- **Configurable vision `max_tokens`** — new `NUPHUS_MCP_VISION_MAX_TOKENS` env var
  (default **1024**, validated `1..=32768`). Previously hardcoded, the value is the
  request parameter that Chinese OpenAI-compatible vision models (Zhipu GLM-4V-Flash
  et al.) cap at 1024 — exceeding it returns HTTP 400. Defaulting to 1024 makes
  BYOK vision work against all those providers out of the box.

### Fixed

- **Unified CDP timeouts** — every CDP call now runs under a single 5-second budget
  through one `cdp()` wrapper. The old per-command hardcoded 30s could make a
  half-dead page hang `snapshot`/`click` for 15s+ before erroring; those now
  fast-fail in ~5s with actionable errors. A connection error on `snapshot` no
  longer silently falls back to a JS eval that reports a false "empty page", and a
  reconnect race can no longer kill a live browser while trying to verify one.

## [0.1.9] - 2026-08-06

### Added

- **YOLO model auto-download** — `desktop_perceive` now downloads
  `icon_detect.onnx` together with the PaddleOCR models on first run, instead
  of requiring a manual placement. Default source is onnx-community's
  OmniParser icon_detect 640x640 ONNX export (hf-mirror.com first,
  huggingface.co fallback; same `[1,3,640,640]` → `[1,5,8400]` I/O contract as
  the exported model). Set `NUPHUS_MCP_YOLO_MODEL_URL` to override with any
  direct `.onnx` URL (e.g. the full ~80 MB OmniParser export or a private
  mirror). A failed YOLO download degrades gracefully to OCR-only and reports
  `yolo_available: false` — it never blocks perceive. `NUPHUS_MCP_NO_MODEL_DOWNLOAD=1`
  skips both OCR and YOLO downloads. `icon_detect.onnx` passes the same size
  floor and ONNX trial-load integrity checks as the OCR models.

### Fixed

- **`NUPHUS_MCP_YOLO_MODEL_URL` was documented but unimplemented** — the env
  var is now actually honored by the downloader.

## [0.1.8] - 2026-08-06

### Added

- **External browser self-healing** — attach to a fingerprint browser now
  survives window reopens: set `NUPHUS_BROWSER_EXE_PATH` (plus optional
  `NUPHUS_BROWSER_NAME` / `NUPHUS_BROWSER_USER_DATA_DIR`) alongside
  `NUPHUS_MCP_BROWSER_CDP_URL`, and when the configured endpoint stops
  answering the server locates the window process by exe path, re-resolves
  its live debug port (literal cmdline port, or `DevToolsActivePort` for
  random-port launches), verifies it with a `no_proxy` CDP probe and retries
  once. Attach failures remain hard errors with actionable, user-facing
  guidance — still no silent fallback into a managed Chrome.
- **Trusted click mode** — `browser_click` gains an optional `trusted`
  parameter (default `false`). When `true`, the click is dispatched as real
  CDP `Input.dispatchMouseEvent` events (`isTrusted=true`, produces user
  activation) instead of a JS-synthesized `el.click()` — required to unlock
  autoplay-gated audio/video playback and other gesture-gated features.
  Default JS click behavior is unchanged.
- **External browser support** — set `NUPHUS_MCP_BROWSER_CDP_URL` (e.g.
  `http://127.0.0.1:9222`) to attach all `browser_*` tools to a user-started
  external browser (anti-detect / fingerprint browsers) instead of launching a
  managed Chrome. Attach failures are hard errors (no silent fallback); the
  external instance is never killed on exit. Endpoint discovery bypasses the
  system proxy (`no_proxy`).

## [0.1.7] - 2026-08-04

### Fixed

- **Internal mechanism audit remediation** — full audit of server core, CDP
  client, desktop automation and release pipeline; all P1/P2 findings fixed:
- **Server**: tool execution is now isolated via `tokio::spawn` — a panicking
  tool returns `isError` instead of killing the whole server process;
  initialize no longer echoes the client's protocol version; JSON-RPC
  validation (-32600/-32700/-32602, explicit `id:null`); `shutdown`/`exit`
  implemented; `desktop_*` dispatch/schema drift guard.
- **Automation lock**: token-based ownership, atomic publish (tmp+hard_link),
  heartbeat renewal (long-running tools no longer outlive the 90s TTL and
  break cross-process mutual exclusion), rename-before-delete (TOCTOU fix).
- **Browser**: structured `BrowserError::Connection` classification — page JS
  exceptions can no longer trigger a reconnect that kills a healthy browser;
  timeout path now checks the child process is actually dead before
  reconnect-kill; retries limited to read-only tools (write tools report
  "may have been executed" instead of silently re-executing); download
  directory re-configured after reconnect; JS interpolation uniformly
  escaped; `batch_exec` reports honest `unknown` status; snapshot refs
  invalidated on page/target changes; navigation URL host filter blocks
  link-local/private ranges (`NUPHUS_MCP_ALLOW_PRIVATE_NAV=1` escape hatch).
- **Desktop**: clipboard read no longer dereferences an unlocked HGLOBAL
  with an unbounded scan (UB fix); foreground activation is verified before
  input injection; ONNX sessions are process-level singletons (no more
  80MB model reload per perceive call); temp capture files are cleaned up;
  model downloads use per-PID partial files and an ORT load check;
  middle-click support; ort dylib path guard against stale System32 DLL.
- **Release pipeline**: Cargo.toml version aligned with npm (0.1.0 → real
  version — published binaries reported the wrong `serverInfo.version`);
  `verify-release-versions.js` preflight gate (tag ↔ npm ↔ Cargo.toml must
  match); E409 publish handling now verifies via `npm view` instead of
  blanket success; CI covers all three crates plus real-Chrome integration
  tests and advisory `cargo audit`; npm CLI validates platform-package
  version; local publish guard against stale binaries.

## [0.1.6] - 2026-08-04

### Fixed

- **Browser dispatch regression (P1, release-blocking)**: the connection
  self-healing refactor (0.1.5) dropped five dispatch arms from
  `browser.rs` while leaving the tools in `tools/list` — `browser_close`,
  `browser_cookies_get`, `browser_cookies_set`, `browser_import_cookies`,
  and `browser_upload` (registered as `browser_upload_file`, so `browser_upload`
  fell through to "Unknown browser tool"). Affected every 0.1.5 npm binary.
  All five arms restored and `browser_upload` renamed to match the schema.
- **Regression guard**: new `dispatch_matches_schema` test asserts that every
  `browser_*` tool registered in `schemas::all_tools` has a dispatch branch
  (enforced by `EXECUTABLE_BROWSER_TOOLS`), so a future schema/dispatch drift
  fails CI instead of surfacing at runtime.
- **npm platform package scope (release-blocking)**: `gen-platform-packages.js`
  generated unscoped names (`nuphus-mcp-win32-x64`) while the registry packages
  are `@nuphus/nuphus-mcp-*`. Publishing the unscoped name made npm treat each
  platform package as brand-new and return 403 spam detection — the 0.1.5 CI
  publish failed for this reason. Script now emits the scoped name (directory
  layout unchanged), and 0.1.6 was published from the corrected packages.

## [0.1.5] - 2026-08-04

### Added

- **Connection-level self-healing (browser)**: a dead CDP connection (killed /
  crashed Chrome) is automatically detected at the operation level, the browser
  is relaunched, and the operation retried once — a mid-workflow browser death
  no longer turns into a string of user-visible errors. `launch()`'s liveness
  probe deliberately does NOT diagnose death (a probe timeout is not proof of
  death); death is proven only by a connection-class error from a real
  operation.

## [0.1.4] - 2026-08-04

### Fixed

- **Anti-detection fingerprints triggering CAPTCHA walls**: Chrome no longer
  exposes the CDP automation state — `--disable-blink-features=AutomationControlled`
  plus an injected `navigator.webdriver` hider on every page, removing the
  single most flaggable signature of automation for user-authorized workflows.

## [0.1.3] - 2026-08-03

### Fixed

- **Strict confirm mode deadlocked with spec-compliant clients**: `confirm` was
  not declared in write-tool input schemas, so spec-compliant MCP clients
  stripped it before the server ever saw it and strict-confirm mode could never
  be satisfied. `confirm` is now declared on every tool the runtime may
  classify as a write, derived from the same source of truth as the runtime
  check (guarded by anti-drift tests).

### Docs

- Added Gitee mirror links to the READMEs.

## [0.1.2] - 2026-08-02

### Fixed

- **CDP liveness probe killing open pages**: the probe used `Target.getTargets`,
  whose response handler re-creates every target and drops their PageHandles —
  any operation after the first failed with "receiver is gone" while the probe
  still reported "alive". The probe is now the side-effect-free `version()`.
- **Navigate hanging 30s on slow pages**: `goto` waits for the `load` lifecycle
  event, which a page with hanging subresources never fires. Navigation is now
  bounded and degrades to polling `document.readyState` (DOM usable at
  "interactive") instead of hanging the tool on the hard CDP timeout.

## [0.1.1] - 2026-08-02

### Added

- **Cross-process automation lock**: desktop and browser automation operate on
  exclusive machine resources; multiple nuphus-mcp instances (one per Agent)
  coordinate through a shared lock file with busy rejection and TTL-based crash
  self-healing.

### Fixed

- **npm launcher**: resolve the nested `optionalDependencies` layout — npm >= 10
  nests platform packages under the dependent package in global installs
  instead of hoisting them as siblings; the launcher now walks up the directory
  tree like `require` does.
- **CI**: platform-aware path tests and workspace-wide `cargo fmt`.

## [0.1.0] - 2026-07-31

### Added

- Initial public release of `nuphus-mcp`, an MCP Server exposing desktop and
  browser automation over stdio (JSON-RPC 2.0).
- **Desktop automation (15 tools)**: `desktop_screen_size`, `desktop_screenshot`,
  `desktop_windows_list`, `desktop_window_activate`, `desktop_window_screenshot`,
  `desktop_window_move`, `desktop_window_resize`, `desktop_window_info`,
  `desktop_vision`, `desktop_perceive`, `desktop_mouse`, `desktop_mouse_drag`,
  `desktop_input`, `desktop_clipboard_clean`, `desktop_clipboard_write`.
- **Browser automation (21 tools)**: `browser_navigate`, `browser_snapshot`,
  `browser_exec`, `browser_click`, `browser_type`, `browser_scroll`,
  `browser_extract`, `browser_screenshot`, `browser_close`, `browser_evaluate`,
  `browser_back`, `browser_forward`, `browser_wait_for`, `browser_cookies_get`,
  `browser_cookies_set`, `browser_import_cookies`, `browser_upload`,
  `browser_list_downloads`, `browser_new_tab`, `browser_list_tabs`,
  `browser_switch_tab`.
- **Protocol**: JSON-RPC 2.0 over stdio; `initialize` / `notifications/initialized`
  / `ping` / `tools/list` / `tools/call`; protocol version `2024-11-05`.
- **Safety**:
  - MCP `annotations`: 25 write tools marked `destructiveHint`, 11 read tools
    marked `readOnlyHint`.
  - Strict confirm mode (`--confirm-write` / `NUPHUS_MCP_CONFIRM_WRITE=1`)
    requires explicit `"confirm": true` on write tools.
  - Screenshot path validation (path traversal / protected directories) and
    upload file existence check.
- **Workspace**: three crates — `nuphus-mcp` (server), `nuphus-browser`
  (CDP browser core, chromiumoxide), `desktop-api` (desktop control core,
  vendored, xcap + Win32).
- **Docs**: `TOOLS.md` / `TOOLS.zh-CN.md` (36-tool reference), demo example
  (`examples/demo.rs`).

[0.1.9]: https://github.com/mrpulor-gh/nuphus-mcp/releases/tag/v0.1.9
[0.1.8]: https://github.com/mrpulor-gh/nuphus-mcp/releases/tag/v0.1.8
[0.1.7]: https://github.com/mrpulor-gh/nuphus-mcp/releases/tag/v0.1.7
[0.1.6]: https://github.com/mrpulor-gh/nuphus-mcp/releases/tag/v0.1.6
[0.1.5]: https://github.com/mrpulor-gh/nuphus-mcp/releases/tag/v0.1.5
[0.1.4]: https://github.com/mrpulor-gh/nuphus-mcp/releases/tag/v0.1.4
[0.1.3]: https://github.com/mrpulor-gh/nuphus-mcp/releases/tag/v0.1.3
[0.1.2]: https://github.com/mrpulor-gh/nuphus-mcp/releases/tag/v0.1.2
[0.1.1]: https://github.com/mrpulor-gh/nuphus-mcp/releases/tag/v0.1.1
[0.1.0]: https://github.com/mrpulor-gh/nuphus-mcp/releases/tag/v0.1.0