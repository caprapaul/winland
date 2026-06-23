# Configuration

Winland uses TOML. If no config file is found, the daemon and CLI diagnostics use built-in defaults from `winland-config`.

## Discovery Order

Without an explicit CLI `--path`, Winland checks the first existing file in this order:

| Order | Location |
| --- | --- |
| 1 | `%WINLAND_CONFIG%` |
| 2 | `%APPDATA%\winland\winland.toml` |
| 3 | `%USERPROFILE%\.config\winland\winland.toml` |
| 4 | `winland.toml` next to the running executable |
| 5 | `winland.toml` in the current working directory |

Validate a file before running the daemon:

```powershell
cargo run -p winland-cli -- config validate --path winland.toml
```

Reload is explicit:

```powershell
cargo run -p winland-cli -- reload-config
```

There is no automatic file watcher yet.

## Full Example

```toml
window_rules = []

[general]
log_level = "info"

[hotkeys]
mode = "advanced-interception"
panic_hotkey = "Ctrl+Alt+Shift+P"
override_latency_budget_micros = 250
bypass = { fullscreen = true, class = [], executable_path = [], process_name = [] }
modifier_drag = { enabled = true, modifiers = "Win" }
bindings = [
  { keys = "Win+T", launch = "wt.exe", override_app = true },
  { keys = "Win+Left", command = "focus-left", override_app = true },
  { keys = "Win+Right", command = "focus-right", override_app = true },
  { keys = "Win+Shift+Left", command = "swap-left", override_app = true },
  { keys = "Win+Shift+Right", command = "swap-right", override_app = true },
  { keys = "Win+R", command = "retile", override_app = true },
  { keys = "Win+F", command = "toggle-float", override_app = true },
  { keys = "Win+C", command = "reload", override_app = true },
  { keys = "Win+Q", command = "quit", override_app = true },
  { keys = "Win+1", command = "switch-workspace-1", override_app = true },
  { keys = "Win+Shift+1", command = "move-to-workspace-1", override_app = true },
]

[layout]
default = "dwindle"
gap = 8
border = 1
master_ratio_percent = 50
smart_split = true
preserve_split = true

[layout.per_monitor]
primary = { layout = "dwindle", gap = 8, border = 1, smart_split = true }

[layout.per_workspace]
"1" = { layout = "master-stack", gap = 6, master_ratio_percent = 55 }

[workspaces]
count = 9
names = ["main", "web", "chat", "code", "docs", "media", "vm", "scratch", "misc"]
initial_monitor = { "1" = "primary" }
startup = "keep-current"

[behavior]
startup_retile = true
dynamic_retile = true
drag_to_float = true
retile_on_drag_end = true
overflow_focus_policy = "tile-focused"
overflow_float_persistence = "permanent"
focus_follows_mouse = false
restore_previous_placement = true
manage_minimized_windows = false
avoid_fullscreen_windows = true

[borders]
enabled = false
width = 3
active_color = "#5AA9FF"
inactive_color = "#3A3A3A"
floating_color = "#FFB454"
show_inactive = true
disable_when_fullscreen = true

[game_mode]
enabled = true
pause_on_fullscreen = true
pause_all_layouts_when_game_focused = true
pause_focused_monitor_only = false
disable_borders = true
disable_animations = true
disable_keyboard_hooks = true
fullscreen_tolerance_px = 4
ignored_exes = []
game_exes = ["cs2.exe"]

[[window_rules]]
name = "float settings"
[window_rules.match]
title = { contains = "Settings" }
process_name = "SystemSettings.exe"
[window_rules.action]
float = true
workspace = 2
always_on_workspace = false
```

The root [winland.toml](../winland.toml) has a larger annotated sample.

## Built-In Defaults

| Key | Default |
| --- | --- |
| `general.log_level` | `"info"` |
| `hotkeys.mode` | `"advanced-interception"` |
| `hotkeys.panic_hotkey` | `"Ctrl+Alt+Shift+P"` |
| `hotkeys.override_latency_budget_micros` | `250` |
| `hotkeys.bypass.fullscreen` | `true` |
| `hotkeys.modifier_drag.enabled` | `true` |
| `hotkeys.modifier_drag.modifiers` | `"Win"` |
| `layout.default` | `"master-stack"` |
| `layout.gap` | `0` |
| `layout.border` | `0` |
| `layout.master_ratio_percent` | `50` |
| `layout.smart_split` | `false` |
| `layout.preserve_split` | `false` |
| `workspaces.count` | `9` |
| `workspaces.startup` | `"keep-current"` |
| `behavior.startup_retile` | `true` |
| `behavior.dynamic_retile` | `true` |
| `behavior.drag_to_float` | `true` |
| `behavior.retile_on_drag_end` | `true` |
| `behavior.overflow_focus_policy` | `"tile-focused"` |
| `behavior.overflow_float_persistence` | `"permanent"` |
| `borders.enabled` | `false` |
| `borders.width` | `3` |
| `game_mode.enabled` | `true` |
| `game_mode.pause_on_fullscreen` | `true` |
| `game_mode.pause_all_layouts_when_game_focused` | `true` |
| `game_mode.pause_focused_monitor_only` | `false` |
| `game_mode.disable_borders` | `true` |
| `game_mode.disable_keyboard_hooks` | `true` |
| `game_mode.fullscreen_tolerance_px` | `4` |

## Hotkeys

`hotkeys.mode` controls the backend:

| Mode | Behavior |
| --- | --- |
| `normal` | Uses documented `RegisterHotKey`. Windows or other apps may already own some shortcuts. |
| `advanced-interception` | Uses a documented low-level keyboard hook. Only bindings with `override_app = true` are suppressed before the app sees them. |

Supported modifiers: `Alt`, `Ctrl`/`Control`, `Shift`, `Win`/`Super`/`Windows`.

Supported keys: single ASCII letters/digits, `Left`, `Down`, `Up`, `Right`, `Esc`/`Escape`, and `Space`.

`Win+L` and `Ctrl+Alt+Escape` are protected from suppressing override bindings. The panic hotkey always passes through and is not dispatched as a Winland command.

Supported command strings:

| Command | Effect |
| --- | --- |
| `focus-left`, `focus-down`, `focus-up`, `focus-right` | Focus a nearby manageable window. |
| `focus-monitor next`, `focus-monitor prev`, `focus-monitor 1`, `focus-monitor 0x...` | Focus a window on another monitor. |
| `swap-left`, `swap-down`, `swap-up`, `swap-right` | Swap the focused window with a neighbor in tile order and retile. |
| `retile` | Reapply tiling. |
| `toggle-float` | Toggle the focused window between tiled and floating. |
| `reload` | Reload config in the daemon. |
| `quit` | Stop the daemon message loop. |
| `switch-workspace-N`, `switch-workspace N`, `switch-workspace next`, `switch-workspace prev` | Switch workspace. |
| `move-to-workspace-N`, `move-window-to-workspace N` | Move the focused window without following. |
| `move-window-to-workspace-and-follow N` | Move the focused window and switch to that workspace. |
| `move-window-to-monitor next`, `move-window-to-monitor prev`, `move-window-to-monitor 1`, `move-window-to-monitor 0x...` | Move the focused window to a monitor. |
| `send-workspace-to-monitor WORKSPACE MONITOR` | Assign a workspace to a monitor and move its safe windows there. |

`launch = "..."` starts an application command line through Win32 process creation.

## Layouts

Supported layout names:

| Layout | Behavior |
| --- | --- |
| `master-stack` | First tiled window is master; remaining windows stack vertically. |
| `dwindle` | Binary split tree. New windows split existing space. |
| `vertical-stack` | Windows are split into rows. |
| `horizontal-stack` | Windows are split into columns. |

`gap` and `border` are geometry reservations used by layout math. They are not the same as the optional visual border overlay. Both must be `<= 256`.

`master_ratio_percent` applies to `master-stack` and must be `10..=90`.

`smart_split` and `preserve_split` only apply to `dwindle`. `smart_split = true` implies preserve behavior at runtime.

Per-monitor overrides are keyed by `primary` or the monitor ID shown by `winland monitors`, such as `"0x10001"`. Per-workspace overrides are keyed by workspace number strings.

## Workspaces

`workspaces.count` is active and controls the fake workspace range. Values must be `1..=32`.

The daemon maintains active workspace state per monitor. Workspace switching hides and shows normal managed windows through documented `ShowWindow` calls and restores remembered placement when available.

Currently parsed but not fully wired into daemon behavior: `workspaces.names`, `workspaces.initial_monitor`, and `workspaces.startup`.

## Behavior

Implemented:

| Key | Behavior |
| --- | --- |
| `startup_retile` | Retile manageable windows after daemon startup. |
| `dynamic_retile` | Retile after window lifecycle/metadata events and monitor moves. |
| `drag_to_float` | Temporarily float a tiled window while the user moves/resizes it. |
| `retile_on_drag_end` | Return temporary floats to the layout after the drag ends. |
| `overflow_focus_policy` | `tile-focused` floats other overflow windows first; `float-focused` floats the focused window first. |
| `overflow_float_persistence` | `permanent` keeps overflow-floated windows floating until `toggle-float`; `retile-on-drag-end` lets a user drag/drop the window back into tiling when the resulting layout fits. |

Parsed but currently reserved or only partially represented in state: `focus_follows_mouse`, `restore_previous_placement`, `manage_minimized_windows`, and `avoid_fullscreen_windows`. Minimized and fullscreen-like windows are still handled conservatively by the current filtering and game-mode paths.

## Borders

`[borders]` controls optional Win32 overlay border windows. It is disabled by default.

Colors must use `#RRGGBB`. Width must be `1..=64`.

Borders are cleared when disabled, when game mode says to hide them, and when `disable_when_fullscreen = true` and the focused window is fullscreen or not manageable.

Border overlays are layered behind their own target windows. Floating target windows are raised before border sync, so floating borders sit above tiled windows but below their associated floating windows.

## Game Mode

See [GAME_MODE.md](GAME_MODE.md). Executable entries are process file names only, not paths, and are case-insensitive. Both `game_exes` and `ignored_exes` make matching windows unmanageable and can activate game mode when focused.

## Window Rules

See [RULES.md](RULES.md). Rules are evaluated in order; later matching rules override earlier action fields.
