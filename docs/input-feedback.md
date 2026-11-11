# Input delivery and UI outcomes

`browser_type` now observes trusted input on the resolved target, in that target's
own frame. It checks editability and focus first. This works for terminal
textareas that immediately consume/clear their value, without waiting for an
unrelated top-level DOM mutation. Receipt proves delivery, not application success
or shell execution. Unknown delivery must be inspected before any retry.

`clear` is now a real schema/implementation option: default `true` preserves the
historical replacement behavior; `false` preserves current content and selection.
Empty text with `clear:false` is a no-op. Clearing a textarea is not a way to clear
the remote shell's command line.

Clicks and key presses return dispatch receipts, not a promise that the requested
application action completed. Check the expected UI/output condition. Clicks no
longer wait 15 seconds for arbitrary outer-page changes; optional
`observe_change:true` retains that observation and reports lack of change as a
note. `NUPHUS_MCP_EFFECT_TIMEOUT_MS` applies to that optional click observation.
Keys no longer wait 2 seconds or suggest foreground activation merely because
the outer DOM stayed unchanged. Transport/action errors still fail; writes are
never automatically replayed after an uncertain dispatch.

Trusted `@N` clicks use CDP content quads in main-viewport coordinates, fixing
iframe-relative offsets. This does not remove Chrome compositor requirements:
in the isolated headless test, trusted cross-frame pointer events in a background
tab can still time out. Ordinary JS clicks and text/key input work in that test's
background tab. Prefer them when user activation is not needed. Do not silently
activate the user's window or downgrade a requested trusted click.

Regression tests use only an isolated headless Chrome and local fixture servers:

```sh
cargo test -p nuphus-browser -p nuphus-mcp
cargo build -p nuphus-mcp
python3 scripts/test_type_receipt.py --binary target/debug/nuphus-mcp --chrome /path/to/chrome
python3 scripts/test_parallel_mcp.py --binary target/debug/nuphus-mcp --chrome /path/to/chrome
```

The input test covers cross-origin terminal receipt with no Enter, real Enter
delivery, ordinary background iframe clicks, trusted iframe coordinates on an
activated **headless fixture** tab, append/replace/empty input, contenteditable,
CSS/AX/legacy refs, readonly/disabled controls, canceled input and diverted focus.
Its `--baseline` and `--click-baseline` modes reproduce the old timeout and missed
iframe click using the previous binary. No remote job or browser profile is involved.
