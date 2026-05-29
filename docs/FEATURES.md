# Features

This document describes implemented behavior only.

## Window Discovery And Filtering

Winland enumerates top-level windows through documented Win32 APIs. It records title, class, PID, executable path, visibility, minimized state, DWM cloaking, owner/tool-window state, styles, size constraints, and rectangle.

A window is skipped by the base filter when it is invisible, minimized, DWM cloaked, title-less, class-less, owned, a tool window, empty-sized, or a known shell/IME class. Window rules and game-mode executable lists can make otherwise manageable windows unmanaged.

Use:

```powershell
cargo run -p winland-cli -- windows
cargo run -p winland-cli -- diagnose-window
```

## Monitor Discovery

Winland enumerates monitors with full rect and work area. Layouts use work areas, so taskbars and reserved desktop areas are respected.

Monitor ownership is based on overlap first, then nearest monitor fallback. This helps windows that straddle monitors or sit partly offscreen.

## Tiling Layouts

Implemented layouts:

- `master-stack`
- `dwindle`
- `vertical-stack`
- `horizontal-stack`

Gaps and layout border reservation are applied in layout math. `dwindle` keeps a split tree per workspace/monitor and can use cursor-aware smart splits.

## Constraint-Aware Tiling

Winland reads minimum/maximum sizing information through `WM_GETMINMAXINFO` when plausible. It also learns minimum sizes from windows that refuse a requested tile size. The daemon retries layout feedback passes and marks windows as constrained in `state --json`.

When assignments cannot fit inside a monitor work area without overlap or empty rectangles, Winland automatically treats one or more windows as `overflow-floating`. By default it keeps the focused window tiled and floats other windows first. Set:

```toml
[behavior]
overflow_focus_policy = "float-focused"
```

to float the focused window first.

## Floating Windows

The focused window can be toggled floating with the `toggle-float` command. Floating windows are excluded from tiling assignments and raised above tiled windows without activation.

Windows can also become temporarily floating during an interactive move/resize or modifier-drag. Temporary floats are reabsorbed when the drag ends if `retile_on_drag_end = true`.

## Workspaces

Winland implements fake workspaces by hiding and showing managed windows. It tracks a workspace per managed window and active workspace per monitor.

Supported commands include switching workspaces, cycling next/previous, moving the focused window to a workspace, moving and following, and sending a workspace to a monitor.

Limitations:

- This does not use Windows Virtual Desktops.
- Hidden workspace windows are managed by Winland's process state.
- Workspace names and initial monitor assignment are parsed but not yet surfaced in CLI/status behavior.

## Multi-Monitor Behavior

Each monitor has an active workspace. Layouts run independently per monitor. Per-monitor and per-workspace layout overrides are supported.

Implemented monitor commands:

- focus another monitor
- move focused window to monitor
- send workspace to monitor

Floating windows moved between monitors keep a translated/clamped rectangle when possible.

## Focus And Swap

Directional focus chooses the nearest focusable window in that direction based on window centers, with wrapping fallback. Swap changes tile order with the chosen directional target and retails.

## Hotkeys

The daemon supports two hotkey modes:

- `normal`: `RegisterHotKey`.
- `advanced-interception`: low-level keyboard hook with optional app suppression.

Command execution is dispatched outside the low-level hook callback. The hook classifies quickly and bypasses fullscreen/game windows according to config.

## Modifier Drag

If enabled, holding configured modifiers and left-dragging a normal top-level window moves it. The default is:

```toml
[hotkeys]
modifier_drag = { enabled = true, modifiers = "Win" }
```

Modifier drag uses a documented low-level mouse hook. Game-mode and bypass rules can prevent handling around fullscreen/game windows.

## Window Rules

Rules can match class, title, executable path, and process name. Actions can manage/ignore, float/tile, assign workspace, pin visible on all workspaces, and mark a window as ignore/game/fullscreen for game-mode behavior.

See [RULES.md](RULES.md).

## Borders

Optional border feedback uses lightweight Win32 overlay windows. Borders can show active, inactive, and floating colors. They are disabled by default and cleared for fullscreen or game mode when configured.

No blur, shadows, animation, custom compositing, DirectComposition effects, or DWM patching are implemented.

## Game Mode

Game mode detects configured executable names, rule modes, and fullscreen/borderless fullscreen geometry. When active it can pause layout globally or on the focused monitor, hide borders, and pause low-level input hook handling.

See [GAME_MODE.md](GAME_MODE.md).

## IPC And State

The daemon starts a local named-pipe IPC server at:

```text
\\.\pipe\winland-ipc
```

Implemented protocol commands:

- `state`
- `reload-config`

The protocol is JSON-line based and versioned. CLI output can be human-readable or JSON for these commands.

## Logging

The daemon writes tracing output to stdout and to `winland-daemon.log` next to the daemon executable. Override the file with:

```powershell
$env:WINLAND_LOG_FILE = "C:\Temp\winland-daemon.log"
```

`general.log_level` controls the default tracing filter unless `RUST_LOG` is set.

## Current Limitations

- Automatic config file watching is not implemented.
- IPC mutation commands beyond reload are not implemented.
- Workspace names and startup assignment fields are parsed but not fully user-visible.
- Visual feedback is limited to optional borders.
- Elevated windows may reject movement unless the daemon is elevated.
- Some applications ignore or adjust Win32 move/resize requests.
- Non-Windows platforms expose stubs for most Win32 operations.

