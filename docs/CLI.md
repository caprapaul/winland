# CLI

The CLI binary is named `winland`. During local development from the workspace root, use:

```powershell
cargo run -p winland-cli -- <command>
```

Most desktop commands require Windows because they call `winland-win32`.

## Commands

| Command | IPC | Purpose |
| --- | --- | --- |
| `state [--json]` | Yes | Query the running daemon. |
| `reload-config [--json]` | Yes | Ask the running daemon to reload config from disk. |
| `command <COMMAND...>` | Yes | Execute a daemon command through IPC. |
| `windows [--manageable-only]` | No | Enumerate top-level windows and explain the conservative filter. |
| `monitors` | No | List monitor IDs, rectangles, and work areas. |
| `diagnose-window [--hwnd <HWND>]` | No | Explain manageability, fullscreen detection, and game mode for one window. |
| `tile-once` | No | Tile manageable windows on the primary monitor once. |
| `config validate [--path <FILE>]` | No | Parse and validate config. |
| `shell ...` | No IPC | Experimental VM-only shell replacement helpers. |
| `widget run ...` | Optional | Run a built-in or user-provided Slint widget. The built-in taskbar subscribes to daemon state IPC. |
| `widget stop ...` / `widget restart ...` | No | Stop or restart running Winland widget processes, including daemon-started widgets. |

If the daemon is not running, IPC commands print:

```text
Winland daemon is not running. Start winland-daemon before using IPC commands.
```

and exit with code `2`.

## State

```powershell
cargo run -p winland-cli -- state
cargo run -p winland-cli -- state --json
```

Human output includes config version/path, total/manageable/floating counts, active workspace, foreground HWND, lightweight performance counters, monitor workspace state, and window participation.

JSON output matches the current IPC snapshot shape:

```json
{
  "config_path": "F:\\Projects\\winland\\winland.toml",
  "config_version": 1,
  "config_loaded_at_unix_ms": 1234567890,
  "total_windows": 3,
  "manageable_windows": 2,
  "floating_windows": 1,
  "temporary_floating_windows": 0,
  "active_workspace": 1,
  "foreground_window": 51966,
  "performance": {
    "relayout_count": 12,
    "skipped_relayout_count": 7,
    "last_relayout_duration_ms": 2,
    "last_relayout_move_count": 3,
    "managed_window_count": 2,
    "border_window_count": 2,
    "game_mode_active": false,
    "config_reload_count": 1
  },
  "monitors": [
    { "monitor_id": 1, "workspace_id": 1, "focused": true }
  ],
  "windows": [
    {
      "handle": 51966,
      "title": "Editor",
      "monitor_id": 1,
      "workspace_id": 1,
      "focused": true,
      "is_minimized": false,
      "participation": "tiled",
      "constrained": false,
      "visible_on_active_workspace": true
    }
  ]
}
```

`participation` is normally `tiled`, `floating`, or `temporary-floating`. The IPC schema still accepts `overflow-floating` for compatibility, but overflow promotion now reports as normal `floating`.

## Command IPC

```powershell
cargo run -p winland-cli -- command switch-workspace 2
cargo run -p winland-cli -- command focus-window 0x123456
```

`command` sends a named daemon command through IPC and prints a short success line when the daemon accepts it. It reuses the same command names as hotkey bindings where possible. `focus-window <HWND>` focuses a tracked app window; if the tracked window is minimized, the daemon restores it first with documented Win32 restore APIs. Invalid, unsafe, or untracked handles are rejected by the daemon.

The built-in taskbar uses this command surface for workspace pills. Widgets still launch ordinary command lines; they do not talk named-pipe IPC directly unless they choose to.

## Reload Config

```powershell
cargo run -p winland-cli -- reload-config
cargo run -p winland-cli -- reload-config --json
```

Reload reads the same discovery paths as daemon startup. It validates the new config, rebuilds runtime config, replaces hotkeys and modifier-drag registration, reapplies rules, updates workspace visibility, retails if allowed, and syncs borders.

JSON output includes:

```json
{
  "config_path": "F:\\Projects\\winland\\winland.toml",
  "config_version": 2,
  "reloaded_at_unix_ms": 1234567899,
  "changed_sections": ["hotkeys", "layout"],
  "state": {}
}
```

`state` is the same shape as `state --json`.

## Windows

```powershell
cargo run -p winland-cli -- windows
cargo run -p winland-cli -- windows --manageable-only
```

The table shows HWND, manage/skip status, skip reason, title, class, PID, executable path, rect, styles, visibility, minimized/cloaked state, owner state, and tool-window state.

Common skip reasons include `not visible`, `minimized`, `DWM cloaked`, `empty title`, `owned window`, `tool window`, `empty rectangle`, and `shell window class`.

## Monitors

```powershell
cargo run -p winland-cli -- monitors
```

Use the printed monitor IDs in config overrides and monitor hotkey commands:

```toml
[layout.per_monitor]
"0x10001" = { layout = "vertical-stack", gap = 6 }
```

## Diagnose Window

```powershell
cargo run -p winland-cli -- diagnose-window
cargo run -p winland-cli -- diagnose-window --hwnd 0x123456
```

Without `--hwnd`, Winland diagnoses the foreground window. Output includes:

- manageability and reason
- monitor ownership
- fullscreen detection and area
- game-mode active/inactive state
- game-mode reason
- matched executable/rule
- layout pause scope
- border, animation, and hook pause status

## Tile Once

```powershell
cargo run -p winland-cli -- tile-once
```

This command loads config, enumerates windows and monitors, selects manageable windows whose centers are on the primary monitor, computes the configured layout for workspace `1`, and calls `SetWindowPos` for each assignment. It does not start the daemon or maintain state after the command exits.

## Config Validate

```powershell
cargo run -p winland-cli -- config validate --path winland.toml
cargo run -p winland-cli -- config validate
```

With no path, validation uses discovery paths or built-in defaults. Success output summarizes the effective base layout and game-mode counts.

## Widgets

For a widget authoring guide, including Slint properties and external plugin JSON streams, see [WIDGETS.md](WIDGETS.md).

```powershell
cargo run -p winland-cli -- widget run taskbar
cargo run -p winland-cli -- widget run taskbar --no-topmost
cargo run -p winland-cli -- widget run --file .\widgets\bar.slint --component MainWindow
cargo run -p winland-cli -- widget run taskbar --plugin-once "my-status.exe"
cargo run -p winland-cli -- widget run taskbar --plugin-stream "my-events.exe"
cargo run -p winland-cli -- widget stop taskbar
cargo run -p winland-cli -- widget restart taskbar
```

`widget run taskbar` starts the built-in Slint taskbar widget from `winland-cli/widgets/taskbar.slint`. The built-in widget is loaded from disk at process startup, not embedded into the binary, so Slint-only edits require restarting the widget process rather than rebuilding Rust. By default it creates a 40px bottom widget on every monitor. The built-in taskbar declares Slint `no-frame: true` and `always-on-top` so it is created without the normal Windows titlebar/frame and stays above normal app windows. Pass `--no-topmost` to keep the widget in the normal z-order band.

The built-in taskbar subscribes to daemon state events through IPC. It currently displays a left-side placeholder power button, centered workspace pills, and a local clock on the right. It intentionally does not render open-window buttons or plugin badges; those data sources remain available for future or custom widgets. Custom Slint widgets can use the same properties:

| Property | Type | Meaning |
| --- | --- | --- |
| `workspaces` | `[WorkspaceRow]` | Workspace id/name, active flag, window count, and command string. |
| `windows` | `[WindowRow]` | HWND, title, workspace id, focused/visible/minimized flags, participation, and command string. |
| `plugin-blocks` | `[PluginBlock]` | Status blocks from external programs. |
| `time-text` | `string` | Local `HH:MM` clock text. |
| `label` | `string` | Compatibility summary for simple widgets. |

Widgets may declare `callback run-command(string);`. The CLI runner registers it and launches the supplied command line as a normal child process. The callback is generic; custom widgets can run any CLI or program. The built-in taskbar uses it for `winland command switch-workspace ...` from workspace pills. The built-in power menu is a local placeholder and does not run shutdown, restart, lock, shell, or daemon commands.

Widget command attempts, exit status, stdout, and stderr are logged to `%TEMP%\winland-widget-commands.log`. This is useful when a widget was launched by daemon startup commands and has no visible console.

`widget stop` stops running Winland widget processes owned by the current `winland-cli.exe`, regardless of whether they were started manually, by the daemon, or by shell startup. With no argument it stops all detected Winland widgets. `widget stop taskbar` targets the built-in taskbar by its `Winland Taskbar` window title. For custom widgets, use an exact title match:

```powershell
cargo run -p winland-cli -- widget stop --title "My Winland Bar"
```

`widget restart taskbar` stops the existing taskbar process and launches a fresh `widget run taskbar` process, then returns immediately. This is the fastest way to pick up edits to `winland-cli/widgets/taskbar.slint` and its referenced assets; rebuild only when Rust code changes.

External widget plugins are ordinary programs. `--plugin-once` runs a command once and reads one JSON object from stdout. `--plugin-stream` runs a command and reads newline-delimited JSON objects from stdout. Objects may contain `label` or `name`, and `text`, `value`, or `status`, for example:

```json
{"label":"CPU","text":"14%"}
```

Custom widgets can be authored as `.slint` files and loaded at runtime with `--file`. Put `no-frame: true` on the exported root `Window` when the widget should be frameless from creation. The widget process is separate from tiling; reserve space for it explicitly with `[layout].offset`, and ignore it through normal `window_rules` when needed, for example:

```toml
[[window_rules]]
name = "ignore winland taskbar"
[window_rules.match]
title = "Winland Taskbar"
[window_rules.action]
manage = false

[layout]
offset = { bottom = 40 }
```

## Experimental Shell Commands

These commands exist, but they are not normal Winland startup. Use them only in a VM with a checkpoint. Details are in [experimental-shell-replacement.md](experimental-shell-replacement.md).

| Command | Purpose |
| --- | --- |
| `shell status` | Inspect per-user shell registry state. |
| `shell test [--elevated-daemon]` | Launch one shell session without registry changes. |
| `shell install --experimental [--elevated-daemon]` | Persist Winland as the per-user shell. |
| `shell uninstall --experimental` | Restore previous per-user shell value. |
| `shell recover --experimental` | Recovery alias for uninstall; also removes elevated daemon task if present. |
| `shell explorer` | Launch `explorer.exe`. |
| `shell install-elevated-task --experimental` | Create/update the elevated daemon scheduled task. |
| `shell uninstall-elevated-task --experimental` | Delete the elevated daemon scheduled task. |
| `shell elevated-task-status` | Report whether the elevated task exists. |
