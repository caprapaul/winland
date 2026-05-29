# Game Mode

Game mode is Winland's conservative safety path for games, fullscreen apps, and borderless fullscreen windows. When game mode is active, Winland backs away from the focused game window so it does not add input latency, steal focus, draw overlays above the game, or resize a window that may be protected by anti-cheat or strict presentation logic.

Game mode uses documented Win32 window metadata only. It does not use DirectX, Vulkan, OpenGL hooks, process injection, game memory inspection, kernel drivers, DWM patches, or private Windows APIs.

## Configuration

Game mode is configured in `winland.toml`:

```toml
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
game_exes = []
```

The default is intentionally cautious. A false positive is better than touching a game window.

## Activation

Game mode activates for the focused window when any of these are true:

- The executable name matches `game_exes` or `ignored_exes`.
- A matching window rule sets `mode = "ignore"`, `mode = "game"`, or `mode = "fullscreen"`.
- `pause_on_fullscreen = true` and the focused window covers a monitor.
- The focused window covers either the monitor bounds or monitor work area within `fullscreen_tolerance_px`.

Executable matching uses the process file name only, not the full path, and is case-insensitive:

```toml
[game_mode]
game_exes = ["eldenring.exe", "cs2.exe"]
ignored_exes = ["game-launcher.exe"]
```

Window rules are useful for launchers, unusual game wrappers, or apps that should always be skipped:

```toml
[[window_rules]]
name = "ignore steam game wrapper"
[window_rules.match]
process_name = "game.exe"
[window_rules.action]
manage = false
mode = "game"
```

## What Pauses

When game mode is active:

- Winland does not tile, move, or resize the game window.
- Winland does not steal focus for layout commands.
- Layout commands are ignored while global game-mode pause is active.
- Border overlays are cleared if `disable_borders = true`.
- Existing low-level keyboard and mouse hook callbacks pass input straight through if `disable_keyboard_hooks = true`.
- Animations are treated as disabled if `disable_animations = true`. Winland does not currently add animation behavior here.

When game mode ends, Winland restores normal layout behavior and performs one relayout of normal managed windows.

## Global vs Monitor-Only Pause

By default, game mode pauses layout globally:

```toml
[game_mode]
pause_all_layouts_when_game_focused = true
pause_focused_monitor_only = false
```

This is the safest mode. No monitor receives new tile assignments while a focused game is active.

To pause only the monitor that contains the game:

```toml
[game_mode]
pause_all_layouts_when_game_focused = true
pause_focused_monitor_only = true
```

In this mode, Winland skips tile assignments for the detected game monitor, but other monitors continue tiling normally. If Winland cannot identify the game monitor, it falls back to a global pause.

## Fullscreen Tolerance

Some apps report a rect that is a few pixels larger or smaller than the monitor. `fullscreen_tolerance_px` controls how much mismatch is accepted:

```toml
[game_mode]
fullscreen_tolerance_px = 4
```

The intended range is small, usually 2 to 8 pixels. Higher values can cause more false positives.

## Diagnostics

Use `diagnose-window` to see whether Winland thinks a window should activate game mode:

```powershell
cargo run -p winland-cli -- diagnose-window
```

That diagnoses the foreground window. To diagnose a specific HWND:

```powershell
cargo run -p winland-cli -- diagnose-window --hwnd 0x123456
```

The output reports:

- fullscreen detection result
- matched executable
- matched rule
- game mode active or inactive
- layout pause scope
- whether borders, animations, and input hooks are paused

The daemon also logs game-mode activation and deactivation with focused HWND, title, executable path, monitor, detection reason, and actions taken.

## Current Boundaries

Game mode is deliberately narrow:

- No visual effects are added.
- No animation system is added.
- No new keyboard or mouse hooks are added.
- No graphics API hooks are used.
- No game process memory is inspected.
- No shell replacement behavior changes.

The goal is to make Winland boring and predictable around games.
