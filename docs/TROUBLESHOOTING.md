# Troubleshooting

Start with these commands:

```powershell
cargo run -p winland-cli -- config validate --path winland.toml
cargo run -p winland-cli -- windows
cargo run -p winland-cli -- monitors
cargo run -p winland-cli -- diagnose-window
cargo run -p winland-cli -- state
```

Check logs in `winland-daemon.log` next to `winland-daemon.exe`, or set:

```powershell
$env:WINLAND_LOG_FILE = "C:\Temp\winland-daemon.log"
```

## A Window Is Not Tiled

Likely causes:

- It fails the base manageability filter.
- A window rule sets `manage = false` or `mode = "ignore"`, `mode = "game"`, or `mode = "fullscreen"`.
- Its process name is in `game_mode.game_exes` or `game_mode.ignored_exes`.
- It is fullscreen, minimized, cloaked, owned, a tool window, or a shell/internal window.
- It belongs to an inactive fake workspace.
- Game mode is pausing layout.

Diagnose:

```powershell
cargo run -p winland-cli -- windows
cargo run -p winland-cli -- diagnose-window --hwnd 0x123456
cargo run -p winland-cli -- state --json
```

Config fixes:

- Remove or narrow an ignore/game rule.
- Remove the executable from `game_exes` or `ignored_exes`.
- Set a more specific rule if a broad rule is catching too much.

## A Window Stays Floating

Likely causes:

- You toggled it with `toggle-float`.
- A rule sets `float = true`.
- It is temporarily floating during drag/resize.
- It became `overflow-floating` because its constraints made the layout not fit.

Diagnose:

```powershell
cargo run -p winland-cli -- state --json
```

Look for `participation`.

Config fixes:

```toml
[[window_rules]]
name = "force app tiled"
[window_rules.match]
process_name = "app.exe"
[window_rules.action]
float = false
```

If it is overflow-floating, reduce `gap`/`border`, reduce window count on that monitor/workspace, or set:

```toml
[behavior]
overflow_focus_policy = "float-focused"
```

## A Window Is Ignored

Likely causes:

- Base filter skip reason.
- Matching window rule.
- Matching game executable.

Run:

```powershell
cargo run -p winland-cli -- windows
cargo run -p winland-cli -- diagnose-window
```

The CLI's `Reason` and `Matched game rule` fields are the best first clue.

## Borders Do Not Appear

Likely causes:

- `borders.enabled = false`.
- Game mode is active and `game_mode.disable_borders = true`.
- Focused window is fullscreen and `borders.disable_when_fullscreen = true`.
- The window is not manageable or is not visible on the active workspace.
- Border overlay worker failed; check daemon logs.

Config:

```toml
[borders]
enabled = true
width = 3
active_color = "#5AA9FF"
inactive_color = "#3A3A3A"
floating_color = "#FFB454"
show_inactive = true
disable_when_fullscreen = true
```

## Hotkeys Do Not Work

Likely causes:

- The daemon is not running.
- Config validation failed.
- In `normal` mode, another app or Windows owns the hotkey.
- In `advanced-interception`, the binding is not marked `override_app = true` and the app receives it too.
- The focused window matches bypass rules.
- Game mode is active and hook bypass is enabled.

Diagnose:

```powershell
cargo run -p winland-cli -- config validate --path winland.toml
cargo run -p winland-cli -- diagnose-window
cargo run -p winland-cli -- state
```

Check logs for hotkey registration failures or low-level bypass messages.

## Config Reload Fails

Likely causes:

- Invalid TOML or unknown fields.
- Validation errors.
- A reloaded hotkey cannot register in strict reload handling.
- Modifier-drag hook reload failed.
- The daemon is not running.

Diagnose:

```powershell
cargo run -p winland-cli -- config validate --path winland.toml
cargo run -p winland-cli -- reload-config
cargo run -p winland-cli -- reload-config --json
```

If IPC says the daemon is not running, start `winland-daemon` first.

## Game Mode Activates Unexpectedly

Likely causes:

- The focused window matches monitor bounds or work area within `fullscreen_tolerance_px`.
- Its process name is in `game_exes` or `ignored_exes`.
- A rule sets mode to `ignore`, `game`, or `fullscreen`.

Diagnose:

```powershell
cargo run -p winland-cli -- diagnose-window
```

Config fixes:

- Lower `fullscreen_tolerance_px`.
- Remove the executable from game lists.
- Narrow or remove the matching rule.
- Set `pause_on_fullscreen = false` only if you understand the risk.

## Multi-Monitor Or Workspace Behavior Seems Wrong

Likely causes:

- The window overlaps multiple monitors and ownership chose the largest overlap.
- The window is offscreen and ownership fell back to nearest monitor.
- Each monitor has its own active workspace.
- A dragged or moved window left a monitor override in daemon state.
- Workspace fields such as `names` and `initial_monitor` are parsed but not fully wired yet.

Diagnose:

```powershell
cargo run -p winland-cli -- monitors
cargo run -p winland-cli -- state --json
```

Try `retile`, reload config, or restart the daemon to clear transient state.

## Elevated Windows Do Not Move

Windows can block lower-integrity processes from moving elevated windows. Run the daemon elevated only when you need to tile elevated windows and understand the security tradeoff. The experimental shell docs include an elevated-daemon VM path, but normal Winland usage does not require shell replacement.

