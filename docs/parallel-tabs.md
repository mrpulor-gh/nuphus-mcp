# Parallel browser tasks (coordination protocol v2)

Each MCP stdio process is one task session. Separate processes can operate different
Chrome targets concurrently; a single process still executes tools sequentially.

## Resource rules

- Background `browser_*` tools share a machine gate. `desktop_*` tools and browser
  calls with `activate=true` hold that gate exclusively for one tool call.
- The selected task tab is claimed until the session switches successfully,
  disconnects (`browser_close`), or exits. Idle time between tools does not release
  the claim. Another session receives `busy` when selecting that tab.
- `browser_list_tabs` includes `target_id`, `selected` and `available`. Prefer
  `browser_switch_tab({"target_id":"..."})`; legacy `index` remains supported.
  Exactly one selector is required. A rejected switch preserves the current tab.
- Normal page operations share a browser gate. Cookie writes/imports and download
  configuration acquire it exclusively. Managed profile startup is serialized.
- Native OS file locks release immediately on process death. Lock files stay on
  disk intentionally; do not unlink them while sessions are running.
- Browser identity comes from the resolved DevTools WebSocket browser UUID, so
  hostname aliases and different connecting processes cannot bypass a tab claim.

## Shared Chrome lifecycle

In isolated MCP mode, `browser_close` means **disconnect this session and release
its claim**. It leaves Chrome and all tabs open. EOF/process exit has the same
behavior. The process which launched Chrome must not terminate other sessions'
work. Close idle Chrome manually when it is no longer needed. A managed headless
browser will not be killed automatically to upgrade to headed mode.

Standalone `BrowserClient` users retain the previous behavior unless they call
`enable_tab_isolation` before connecting. All automation clients that share these
resources must participate in the same protocol; this does not block human input
or third-party browser automation.

## Upgrade from v1

Do not mix v1 and v2 MCP servers. v1 uses a JSON `automation.lock` lease and does
not recognize v2's OS locks. Finish/pause browser interaction, stop **all** old
Nuphus MCP instances, then configure clients to use the new binary and reconnect.
Do not stop remote jobs just to release local browser control. Closing a legacy
launcher may close its managed Chrome, so first ensure remote jobs are detached
from interactive terminal lifetime. Do not delete locks to force an upgrade.

## Two concurrent browser tasks

1. Give each chat its own browser tab and bind it by `target_id`.
2. In each tab, create/select a distinct remote terminal session. Two browser tabs
   may reconnect to the same backend terminal; tab ownership cannot isolate that.
3. Check the terminal name, current directory and task-specific command output
   before starting work. Keep upstream/downstream output directories distinct.
4. Use background CDP operations normally. Explicit foreground/desktop operations
   are intentionally exclusive. Tab isolation does not isolate cookies, same-origin
   storage, shared backend files, or application-level job state.

This addresses cross-session tab contention. Upload failures and UI action verification
timeouts remain separate issues; do not blindly replay an uncertain command.

## Validation

Requires Rust 1.89+ for native file locking (dependencies may require a newer toolchain).

```sh
cargo test -p nuphus-browser -p nuphus-mcp
cargo build -p nuphus-mcp
python3 scripts/test_parallel_mcp.py --binary target/debug/nuphus-mcp --chrome /path/to/chrome
cargo test -p nuphus-browser isolated_tasks_keep_shared_chrome_alive -- --ignored --test-threads=1
```

The Python test starts an isolated headless Chrome profile and two MCP processes.
Its HTTP rendezvous requires both tool calls to be live at once; it also checks
persistent tab claims, input separation, desktop/activation exclusion, shared
cookie-write exclusion and session exit. The Rust cross-process lock tests kill
holders and verify immediate recovery. The ignored Chrome test checks the managed
launcher's disconnect path and preservation of the other client's target on reconnect.
