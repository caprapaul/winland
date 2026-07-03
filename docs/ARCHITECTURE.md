# Architecture

Winland is a Rust workspace with narrow crate boundaries. The important rule is that platform-independent behavior stays out of Win32 code, and unsafe Win32 calls stay inside `winland-win32`.

## Crates

| Crate | Role |
| --- | --- |
| `winland-core` | Pure layout, geometry, workspace, rule, fullscreen, and game-mode policy logic. |
| `winland-config` | TOML schema, defaults, discovery, validation, and conversion to core/runtime types. |
| `winland-win32` | Window/monitor enumeration, WinEvent hooks, hotkeys, low-level hooks, movement, focus, borders, IPC transport, and experimental shell helpers. |
| `winland-ipc` | Versioned JSON-line request/response structs and encode/decode helpers. |
| `winland-daemon` | Long-running event processor that connects config, core state, Win32 events, hotkeys, IPC, and border updates. |
| `winland-cli` | Human-facing diagnostics, daemon IPC client, and separate widget runner commands. |
| `winland-shell` | Experimental VM-only user shell entrypoint that starts the daemon. |

## Core vs Win32 Boundary

`winland-core` contains no HWNDs, RECTs, callbacks, handles, or unsafe code. It works with plain Rust types such as `WindowHandle`, `Rect`, `MonitorInfo`, `WindowInfo`, `WorkspaceId`, `LayoutConfig`, and rule structs.

`winland-win32` owns direct Windows API calls through the `windows` crate. It converts Win32 data into core types before handing it to the daemon or CLI.

## Daemon Event Model

At startup, the daemon:

1. Loads config through `winland-config`.
2. Initializes tracing and opens `winland-daemon.log`.
3. Subscribes to documented WinEvent hooks.
4. Starts the named-pipe IPC server.
5. Installs either `RegisterHotKey` bindings or the advanced low-level keyboard hook.
6. Optionally installs modifier-drag low-level mouse hook.
7. Builds the initial window and monitor snapshot.
8. Starts the border overlay manager.
9. Applies startup retile if enabled.
10. Enters the Win32 message loop.

Event bridge threads forward Win32/window, hotkey, mouse-drag, and IPC events into one daemon channel. The processor debounces bursty lifecycle events briefly, while foreground, move, and move/size events are handled with lower latency.

## Layout And State Pipeline

The daemon stores:

- discovered windows
- foreground window
- tile order
- participation state: tiled, floating, temporarily floating
- learned size constraints
- per-workspace/per-monitor state
- monitor ownership overrides
- overflow-promoted floating state
- game-mode activation

For a retile:

1. Enumerate monitors.
2. Sync workspace state.
3. Skip all layout if global game-mode pause is active.
4. For each monitor, pick the active workspace.
5. Apply configured layout offsets to the monitor work area.
6. Select tiled windows owned by that monitor and visible on that monitor's workspace.
7. Resolve overflow by promoting windows to floating until assignments fit the offset work area.
8. Compute core layout assignments.
9. Move windows through `SetWindowPos`.
10. Read accepted rectangles and learn constraints if Windows refused requested sizes.
11. Repeat feedback passes up to three times.
12. Sync borders.

## Widget Runner

Widgets are separate user processes started from CLI commands such as `winland widget run taskbar`. They do not participate in layout state or workspace state, and widget pointer input is handled by the widget process like any other app window. Tiling cooperates with widgets only through explicit user configuration: `[layout].offset` reserves screen-edge space, and normal `[[window_rules]]` entries can ignore widget windows.

The first widget backend uses Slint for declarative UI. The built-in taskbar is authored as `winland-cli/widgets/taskbar.slint` and loaded from disk at widget process startup through the same path-based compiler used for custom `.slint` widgets. Frameless widgets should use Slint's `no-frame: true` root `Window` property so the titlebar is absent from creation. Topmost behavior is requested through Slint's `always-on-top` property, while Win32-specific panel shaping such as tool-window styling stays in `winland-win32`.

Widget data is source-driven. The CLI can subscribe to daemon state events over IPC, update a local clock source, and run external executable sources. External sources either print one JSON object and exit or print newline-delimited JSON objects as an event stream. The CLI maps those sources into Slint properties such as `workspaces`, `windows`, `plugin-blocks`, and `time-text`; Slint files own layout and presentation.

Widget interactivity is generic. A root Slint component can declare `callback run-command(string);`; the CLI runner registers it and launches the supplied command line as an ordinary child process. The built-in taskbar uses this generic callback to invoke `winland command ...` for workspace switching, but custom widgets do not need to know daemon IPC or Winland action types.

Widget lifecycle management is CLI-owned. `winland widget stop` and `winland widget restart` enumerate top-level windows, match widget processes owned by the current `winland-cli.exe`, and terminate matching widget processes through `winland-win32`. Restart then launches a fresh `widget run ...` process through the existing Win32 process creation helper. This works whether the original widget was started manually, by the daemon, or by shell startup.

The user-facing widget authoring API is documented in [WIDGETS.md](WIDGETS.md).

## Config And Rule Pipeline

Config is parsed with `serde` and `toml` using `deny_unknown_fields`. Validation collects multiple errors before returning.

At runtime, config is converted into:

- `RuntimeConfig`
- core `LayoutConfig`
- per-monitor and per-workspace layout maps
- core `WindowRule` values
- `GameModePolicy`
- hotkey bindings and command maps

Rules are evaluated in order. Later rules override only the fields they set. Rule decisions participate in manageability, workspace assignment, floating state, sticky visibility, and game-mode detection.

Reload is explicit through IPC or hotkey command. Reload validates new config, replaces hotkey/modifier-drag registrations, reapplies rules, updates visibility, recalculates game mode, retails when allowed, and reports changed sections.

## IPC And CLI Pipeline

`winland-ipc` defines protocol version `1`. Requests and responses are JSON plus trailing newline. `subscribe-state` keeps the IPC pipe open and streams state snapshot responses when daemon state changes.

Current commands:

- `state`
- `reload-config`

`winland-win32` hosts a local named pipe at `\\.\pipe\winland-ipc`. `winland-cli` encodes requests, sends them to the pipe, decodes responses, and prints human or JSON output.

## Border Manager

The border manager runs a separate Win32 message-loop thread. It creates transparent, no-activate overlay windows around managed windows. The daemon sends sync/clear commands to that worker and posts a thread message.

Border overlays are restacked on every visible sync. The daemon raises floating target windows before syncing border geometry, then the worker inserts each border immediately behind its own target window. The intended z-order is tiled border, tiled window, inactive floating border, inactive floating window, focused floating border, focused floating window.

Border candidates are filtered by:

- `[borders].enabled`
- game-mode border hide
- focused fullscreen or unmanageable window
- active workspace visibility
- monitor pause scope
- `show_inactive`

## Game Mode

Game mode is computed from the focused window, monitors, window rules, and `GameModePolicy`. Activation records reason, monitor, fullscreen detection, and selected actions.

The daemon updates game mode on startup, foreground changes, window events, move/size events, and reload. It then updates low-level hook pause state and clears borders when configured.

## Unsafe Code Boundary

Unsafe code should stay in `winland-win32`. Current unsafe usage wraps documented APIs such as:

- `EnumWindows`
- `EnumDisplayMonitors`
- `SetWindowPos`
- `SetForegroundWindow`
- `ShowWindow`
- `SetWinEventHook`
- `RegisterHotKey`
- `SetWindowsHookExW`
- named pipes
- border overlay window creation
- registry and scheduled task shell helpers

Every unsafe block should state the preconditions being relied on.

## Testing Strategy

- Pure layout, workspace, rule, fullscreen, game-mode, and config tests live in `winland-core`, `winland-config`, `winland-ipc`, and daemon unit tests.
- Desktop-mutating behavior is not exercised by default tests.
- `winland-win32` exposes stubs on non-Windows so pure tests can still compile where possible.
- The standard regression command is `cargo test --workspace`; run `cargo fmt` and `cargo clippy --workspace --all-targets` with it before behavior changes land.
- CI runs those checks on a Windows runner so Win32-facing crates stay compiled without requiring tests to rearrange the desktop.
