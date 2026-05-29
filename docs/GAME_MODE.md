# Game Mode

Game mode is Winland's safety path for games, fullscreen apps, and borderless fullscreen windows. It uses documented Win32 window metadata only.

Winland does not:

- inject into game processes
- hook DirectX, Vulkan, OpenGL, or other graphics APIs
- inspect game memory
- install kernel drivers
- patch DWM
- replace the compositor

## Activation

Game mode activates for the focused window when `[game_mode].enabled = true` and any of these are true:

| Trigger | Details |
| --- | --- |
| Configured executable | The process file name matches `game_exes` or `ignored_exes`, case-insensitively. |
| Window rule mode | A matching rule sets `mode = "ignore"`, `mode = "game"`, or `mode = "fullscreen"`. |
| Fullscreen geometry | `pause_on_fullscreen = true` and the focused rect matches monitor bounds or work area within `fullscreen_tolerance_px`. |

Example:

```toml
[game_mode]
enabled = true
pause_on_fullscreen = true
fullscreen_tolerance_px = 4
game_exes = ["cs2.exe", "eldenring.exe"]
ignored_exes = ["game-launcher.exe"]
```

Executable entries must be file names, not paths.

## What Gets Disabled Or Paused

When active, depending on config:

- Layout commands can be ignored.
- Startup/dynamic retile can produce no tile assignments.
- Layout can pause globally or only on the focused game's monitor.
- Border overlays can be cleared.
- Low-level keyboard and mouse hook handling can be bypassed.
- Animation behavior is treated as disabled, although Winland does not currently implement animations.

The game window itself is not tiled, moved, resized, or focused by layout commands.

## Pause Scope

Safest global pause:

```toml
[game_mode]
pause_all_layouts_when_game_focused = true
pause_focused_monitor_only = false
```

Monitor-only pause:

```toml
[game_mode]
pause_all_layouts_when_game_focused = true
pause_focused_monitor_only = true
```

If monitor-only pause cannot identify the monitor, Winland falls back to global pause.

## Hotkey And Hook Safety

Advanced hotkey interception and modifier drag use documented low-level hooks. The callbacks classify input quickly and dispatch command work outside the hook.

With the defaults below, fullscreen/game contexts pass through:

```toml
[hotkeys]
bypass = { fullscreen = true, class = [], executable_path = [], process_name = [] }

[game_mode]
disable_keyboard_hooks = true
```

When `disable_keyboard_hooks = true`, configured `game_exes` and `ignored_exes` are added to the hotkey bypass rules. Fullscreen bypass also becomes active when `pause_on_fullscreen = true`.

## Borders And Overlays

Winland border feedback is an optional set of transparent Win32 overlay windows. It is not graphics API hooking and does not draw inside the game process.

For game safety:

```toml
[borders]
disable_when_fullscreen = true

[game_mode]
disable_borders = true
```

These are the cautious defaults except that borders themselves are disabled by default.

## Diagnostics

Diagnose the foreground window:

```powershell
cargo run -p winland-cli -- diagnose-window
```

Diagnose a specific HWND:

```powershell
cargo run -p winland-cli -- diagnose-window --hwnd 0x123456
```

The output reports manageability, fullscreen detection, game-mode reason, matched executable/rule, layout pause scope, border pause, animation pause, and hook bypass.

The daemon also logs game-mode activation/deactivation with focused HWND, title, executable path, monitor, reason, and selected actions.

## Limitations

- Detection is window-metadata based, so false positives are possible.
- Fullscreen detection is rectangle-based and controlled by `fullscreen_tolerance_px`.
- Game mode is focused-window driven.
- Anti-cheat behavior varies by product; the safest setup is to leave advanced interception and borders disabled for games you do not trust with overlays.
- No dedicated game database is bundled.

